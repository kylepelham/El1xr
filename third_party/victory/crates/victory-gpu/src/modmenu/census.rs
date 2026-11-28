//! (El1xr) `VICTORY_MOD_MENU_CENSUS=1` (default off): counts how often each script native runs,
//! to name natives by behaviour (in RAGE titles the hottest are the wait, the player's ped and
//! entity coordinates, and the counts follow what the player does).
//!
//! Each handler found by [`super::natives::scan`] starts with the same compare,
//! `cmp dword ptr [rcx+10h], 0`, followed either by a register-to-register `mov` and the `js`
//! (shape A) or by the `js` itself (shape B). Before the title runs, [`install`] replaces those
//! first instructions with a jump to a thunk of ours, near the image:
//!
//! ```text
//! lock inc qword ptr [counter]   ; this native's counter
//! cmp dword ptr [rcx+10h], 0     ; the handler's own compare
//! mov r, r                       ; shape A: its mov, the flags stay the compare's
//! js  error                      ; shape B: its js, moved here with a 32-bit offset
//! jmp back                       ; the instruction after the replaced ones
//! ```
//!
//! The thunk calls nothing and touches no register, so the handler runs as before; handlers of
//! another shape are left alone and counted as skipped. [`Census::report`] gives the busiest
//! natives since the last report.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use super::natives::Native;

const CMP: [u8; 4] = [0x83, 0x79, 0x10, 0x00];
/// The most bytes between a handler's start and its compare (as in the scan).
const MAX_CMP_AT: usize = 0x20;
const THUNK: usize = 32;
const JMP_LEN: usize = 5;

/// The instructions a hook replaces at a handler's compare.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Site {
    /// `cmp` + a 3-byte REX.W register-to-register `mov`: 7 bytes, moved as they are.
    CmpMov { cmp: usize },
    /// `cmp` + `js` to `target` (`len` bytes in all: 6 for a short `js`, 10 for a near one).
    CmpJs {
        cmp: usize,
        len: usize,
        target: usize,
    },
}

impl Site {
    fn cmp(self) -> usize {
        match self {
            Site::CmpMov { cmp } | Site::CmpJs { cmp, .. } => cmp,
        }
    }

    fn len(self) -> usize {
        match self {
            Site::CmpMov { .. } => 7,
            Site::CmpJs { len, .. } => len,
        }
    }
}

/// The hook site of the handler starting at `start` (`code[0]` is at address `at`).
pub fn site(code: &[u8], at: usize, start: usize) -> Option<Site> {
    let off = start.checked_sub(at)?;
    let head = code.get(off..)?;
    let head = &head[..head.len().min(MAX_CMP_AT + 14)];
    let k = head[..head.len().min(MAX_CMP_AT + 4)]
        .windows(4)
        .position(|w| w == CMP)?;
    let cmp = start + k;
    // The compare, a 3-byte mov and a near js at most: 10 bytes after the compare.
    let rest = head.get(k + 4..k + 14)?;
    let is_mov = matches!(rest[0], 0x48 | 0x49 | 0x4c | 0x4d)
        && matches!(rest[1], 0x89 | 0x8b)
        && rest[2] >= 0xc0;
    let js = if is_mov { &rest[3..] } else { rest };
    if js[0] != 0x78 && !(js[0] == 0x0f && js[1] == 0x88) {
        return None;
    }
    if is_mov {
        return Some(Site::CmpMov { cmp });
    }
    let (len, rel) = if js[0] == 0x78 {
        (2, js[1] as i8 as isize)
    } else {
        (6, i32::from_le_bytes(js[2..6].try_into().ok()?) as isize)
    };
    let end = cmp + 4 + len;
    Some(Site::CmpJs {
        cmp,
        len: 4 + len,
        target: end.checked_add_signed(rel)?,
    })
}

fn rel32(from_end: usize, to: usize) -> Option<[u8; 4]> {
    let d = i32::try_from(to as i64 - from_end as i64).ok()?;
    Some(d.to_le_bytes())
}

/// The thunk for `site` placed at `at`, counting into the u64 at `counter`; `original` holds the
/// site's bytes (at least the compare and what follows it).
pub fn thunk(site: Site, original: &[u8], at: usize, counter: usize) -> Option<[u8; THUNK]> {
    let mut t = Vec::with_capacity(THUNK);
    t.extend([0xf0, 0x48, 0xff, 0x05]);
    t.extend(rel32(at + 8, counter)?);
    match site {
        Site::CmpMov { .. } => t.extend_from_slice(original.get(..7)?),
        Site::CmpJs { target, .. } => {
            t.extend_from_slice(original.get(..4)?);
            t.extend([0x0f, 0x88]);
            t.extend(rel32(at + t.len() + 4, target)?);
        }
    }
    t.push(0xe9);
    t.extend(rel32(at + t.len() + 4, site.cmp() + site.len())?);
    t.resize(THUNK, 0xcc);
    t.try_into().ok()
}

/// What goes over the site: a jump to the thunk, the rest of the replaced bytes `int3`.
pub fn jump(site: Site, thunk_at: usize) -> Option<Vec<u8>> {
    let mut j = vec![0xe9];
    j.extend(rel32(site.cmp() + JMP_LEN, thunk_at)?);
    j.resize(site.len(), 0xcc);
    Some(j)
}

/// Counters for the hooked natives, in memory of ours next to the thunks.
pub struct Census {
    hashes: Vec<u64>,
    counters: &'static [AtomicU64],
    pub hooked: usize,
    pub skipped: usize,
    last: Mutex<Vec<u64>>,
}

impl Census {
    /// The busiest `top` natives since the last call: (total calls in the period, natives that
    /// ran, `(hash, calls)` busiest first).
    pub fn report(&self, top: usize) -> (u64, usize, Vec<(u64, u64)>) {
        let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
        let mut rows = Vec::new();
        let mut total = 0;
        for (i, c) in self.counters.iter().enumerate() {
            let now = c.load(Relaxed);
            let d = now - last[i];
            last[i] = now;
            if d > 0 {
                total += d;
                rows.push((self.hashes[i], d));
            }
        }
        let ran = rows.len();
        rows.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        rows.truncate(top);
        (total, ran, rows)
    }

    /// Calls counted so far for `hash`.
    pub fn count(&self, hash: u64) -> Option<u64> {
        let i = self.hashes.iter().position(|&h| h == hash)?;
        Some(self.counters[i].load(Relaxed))
    }
}

/// Memory for thunks within reach of a 32-bit jump from `lo..hi`, tried every 16 MiB above and
/// below.
#[cfg(windows)]
fn near_alloc(lo: usize, hi: usize, size: usize) -> Option<usize> {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_RESERVE, PAGE_EXECUTE_READWRITE, VirtualAlloc,
    };
    let up = (hi + 0xffff) & !0xffff;
    let down = lo & !0xffff;
    for step in 0..100usize {
        for at in [up + (step << 24), down.wrapping_sub((step + 1) << 24)] {
            // SAFETY: a reservation of our own where the address is free.
            let p = unsafe {
                VirtualAlloc(
                    at as *const _,
                    size,
                    MEM_RESERVE | MEM_COMMIT,
                    PAGE_EXECUTE_READWRITE,
                )
            } as usize;
            if p != 0 {
                return Some(p);
            }
        }
    }
    None
}

/// Hooks every native of `natives` (offsets into the image at `base`, `size` bytes) whose
/// handler has a known shape.
///
/// # Safety
/// `base..base + size` is the mapped title image and no title thread runs yet.
#[cfg(windows)]
pub unsafe fn install(base: usize, size: usize, natives: &[Native]) -> Result<Census, String> {
    use windows_sys::Win32::System::Diagnostics::Debug::FlushInstructionCache;
    use windows_sys::Win32::System::Memory::{PAGE_EXECUTE_READWRITE, VirtualProtect};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    // SAFETY: the caller's contract.
    let image = unsafe { std::slice::from_raw_parts(base as *const u8, size) };
    let sites: Vec<(u64, Site)> = natives
        .iter()
        .filter_map(|n| Some((n.hash, site(image, base, base + n.rva as usize)?)))
        .collect();
    let skipped = natives.len() - sites.len();
    let n = sites.len();
    let counters_len = (n * 8).next_multiple_of(64);
    let total = (counters_len + n * THUNK).next_multiple_of(0x10000);
    let region = near_alloc(base, base + size, total).ok_or("no memory near the image")?;
    // SAFETY: fresh zeroed memory of ours, aligned, `n` u64s long, never freed.
    let counters: &'static [AtomicU64] =
        unsafe { std::slice::from_raw_parts(region as *const AtomicU64, n) };
    let mut hooked = 0;
    for (i, &(_, s)) in sites.iter().enumerate() {
        let at = region + counters_len + i * THUNK;
        let original = &image[s.cmp() - base..s.cmp() - base + s.len()];
        let (Some(code), Some(jmp)) = (thunk(s, original, at, region + i * 8), jump(s, at)) else {
            continue;
        };
        let mut old = 0u32;
        // SAFETY: the thunk slot is ours; the site's bytes are the handler's, made writable
        // for the patch and restored, before any title thread runs.
        unsafe {
            std::ptr::copy_nonoverlapping(code.as_ptr(), at as *mut u8, THUNK);
            if VirtualProtect(
                s.cmp() as *const _,
                jmp.len(),
                PAGE_EXECUTE_READWRITE,
                &mut old,
            ) == 0
            {
                continue;
            }
            std::ptr::copy_nonoverlapping(jmp.as_ptr(), s.cmp() as *mut u8, jmp.len());
            VirtualProtect(s.cmp() as *const _, jmp.len(), old, &mut old);
        }
        hooked += 1;
    }
    // SAFETY: flushing our own process's code.
    unsafe { FlushInstructionCache(GetCurrentProcess(), std::ptr::null(), 0) };
    Ok(Census {
        hashes: sites.iter().map(|&(h, _)| h).collect(),
        counters,
        hooked,
        skipped: skipped + (n - hooked),
        last: Mutex::new(vec![0; n]),
    })
}

#[cfg(not(windows))]
pub unsafe fn install(_: usize, _: usize, _: &[Native]) -> Result<Census, String> {
    Err("the census needs Windows".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A handler of shape A at `start` returning 42, and one of shape B (no mov).
    fn handler_a() -> Vec<u8> {
        vec![
            0x56, 0x57, 0x48, 0x83, 0xec, 0x28, // push rsi; push rdi; sub rsp, 28h
            0x83, 0x79, 0x10, 0x00, // cmp dword [rcx+10h], 0
            0x48, 0x89, 0xce, // mov rsi, rcx
            0x78, 0x12, // js +0x12 (error)
            0x48, 0x8b, 0x46, 0x08, // mov rax, [rsi+8]
            0x48, 0xc7, 0x00, 0x2a, 0x00, 0x00, 0x00, // mov qword [rax], 42
            0x48, 0x83, 0xc4, 0x28, 0x5f, 0x5e, 0xc3, // add rsp, 28h; pop rdi; pop rsi; ret
            0x48, 0x83, 0xc4, 0x28, 0x5f, 0x5e, 0xc3, // error: the same epilogue
        ]
    }

    fn handler_b() -> Vec<u8> {
        vec![
            0x48, 0x83, 0xec, 0x28, // sub rsp, 28h
            0x83, 0x79, 0x10, 0x00, // cmp dword [rcx+10h], 0
            0x78, 0x10, // js +0x10 (error)
            0x48, 0x8b, 0x41, 0x08, // mov rax, [rcx+8]
            0x48, 0xc7, 0x00, 0x07, 0x00, 0x00, 0x00, // mov qword [rax], 7
            0x48, 0x83, 0xc4, 0x28, 0xc3, // add rsp, 28h; ret
            0x48, 0x83, 0xc4, 0x28, 0xc3, // error
        ]
    }

    #[test]
    fn sites_of_both_shapes_are_found_and_others_refused() {
        let a = handler_a();
        assert_eq!(site(&a, 0x1000, 0x1000), Some(Site::CmpMov { cmp: 0x1006 }));
        let b = handler_b();
        assert_eq!(
            site(&b, 0x1000, 0x1000),
            Some(Site::CmpJs {
                cmp: 0x1004,
                len: 6,
                target: 0x100a + 0x10
            })
        );
        let mut c = handler_a();
        c[13] = 0x90;
        assert_eq!(site(&c, 0x1000, 0x1000), None, "no js after the mov");
        assert_eq!(site(&a, 0x1000, 0x2000), None, "out of the code");
    }

    #[test]
    fn thunks_count_then_replay_the_replaced_instructions() {
        let a = handler_a();
        let s = site(&a, 0x1000, 0x1000).unwrap();
        let t = thunk(s, &a[6..], 0x9000, 0x8000).unwrap();
        assert_eq!(t[..4], [0xf0, 0x48, 0xff, 0x05], "lock inc qword [rip+..]");
        assert_eq!(
            i32::from_le_bytes(t[4..8].try_into().unwrap()),
            0x8000 - 0x9008
        );
        assert_eq!(t[8..15], a[6..13], "cmp and mov as they were");
        assert_eq!(t[15], 0xe9);
        let back = 0x9000 + 20 + i32::from_le_bytes(t[16..20].try_into().unwrap()) as i64;
        assert_eq!(back, 0x1006 + 7, "back to the js");
        assert_eq!(jump(s, 0x9000).unwrap()[..1], [0xe9]);
        assert_eq!(jump(s, 0x9000).unwrap().len(), 7);

        let b = handler_b();
        let s = site(&b, 0x1000, 0x1000).unwrap();
        let t = thunk(s, &b[4..], 0x9000, 0x8000).unwrap();
        assert_eq!(t[12..14], [0x0f, 0x88], "the js, near");
        let js = 0x9000 + 18 + i32::from_le_bytes(t[14..18].try_into().unwrap()) as i64;
        assert_eq!(js, 0x100a + 0x10);
        let back = 0x9000 + 23 + i32::from_le_bytes(t[19..23].try_into().unwrap()) as i64;
        assert_eq!(back, 0x100a);
        assert!(
            thunk(s, &b[4..], 0x9000, 0x9000 + (1usize << 33)).is_none(),
            "out of reach"
        );
    }

    #[cfg(windows)]
    #[test]
    fn hooked_handlers_still_answer_and_are_counted() {
        use super::super::natives::invoke;
        use windows_sys::Win32::System::Memory::{
            MEM_COMMIT, MEM_RESERVE, PAGE_EXECUTE_READWRITE, VirtualAlloc,
        };
        const SIZE: usize = 0x1000;
        // SAFETY: a fresh page of our own.
        let base = unsafe {
            VirtualAlloc(
                std::ptr::null(),
                SIZE,
                MEM_RESERVE | MEM_COMMIT,
                PAGE_EXECUTE_READWRITE,
            )
        } as usize;
        assert_ne!(base, 0);
        let (a, b) = (handler_a(), handler_b());
        // SAFETY: both fit in the page.
        unsafe {
            std::ptr::copy_nonoverlapping(a.as_ptr(), base as *mut u8, a.len());
            std::ptr::copy_nonoverlapping(b.as_ptr(), (base + 0x100) as *mut u8, b.len());
            std::ptr::write_bytes((base + 0x200) as *mut u8, 0x90, 0x40);
        }
        let natives = [
            Native { hash: 0xa, rva: 0 },
            Native {
                hash: 0xb,
                rva: 0x100,
            },
            Native {
                hash: 0xc,
                rva: 0x200,
            },
        ];
        // SAFETY: our page stands in for the image; nothing else runs it.
        let census = unsafe { install(base, SIZE, &natives) }.unwrap();
        assert_eq!((census.hooked, census.skipped), (2, 1));
        // SAFETY: the stand-in handlers follow the handler contract.
        unsafe {
            assert_eq!(invoke(base, &[]), Ok([42, 0, 0, 0]));
            assert_eq!(invoke(base, &[1]), Ok([42, 0, 0, 0]));
            assert_eq!(invoke(base + 0x100, &[]), Ok([7, 0, 0, 0]));
        }
        assert_eq!(census.count(0xa), Some(2));
        assert_eq!(census.count(0xb), Some(1));
        assert_eq!(census.count(0xc), None, "not hooked");
        assert_eq!(census.report(1), (3, 2, vec![(0xa, 2)]));
        assert_eq!(census.report(8), (0, 0, vec![]), "since the last report");
    }
}
