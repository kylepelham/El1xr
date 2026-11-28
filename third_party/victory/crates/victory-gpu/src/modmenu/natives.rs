//! (El1xr) The title's script natives, found by their shape in the mapped image.
//!
//! game_scarlett_master.exe 1.0.2.1 keeps RAGE's native calling convention (as known from GTA V
//! and RDR2): each native is a handler that takes one call context, its arguments as 8-byte
//! slots, its result written through a return pointer. Here the context is `args` at +0x00,
//! `ret` at +0x08 and a signed count at +0x10. Every handler starts the same way:
//!
//! ```text
//! push rsi ; push rdi ; sub rsp, N
//! cmp dword ptr [rcx+10h], 0     ; 83 79 10 00
//! mov rsi, rcx
//! js  error                      ; 78 xx (or 0F 88 rel32)
//! ...                            ; a global recursive native lock, the body, the unlock
//! error:
//! mov rax, [rsi]                 ; 48 8B 06 (or [rcx], [rdi], [rbx])
//! mov rcx, <64-bit hash>         ; 48 B9 imm64
//! mov [rax], rcx                 ; 48 89 08
//! ```
//!
//! The 64-bit constant is unique to each handler (6,828 of them, none repeated), so it names the
//! native. The handlers are referenced from nowhere in the file (no pointer, relative offset or
//! `lea` names them: the game builds its table at run time), so instead of the game's table this
//! builds our own: hash to handler, from the handlers' own bytes. The function start comes from
//! the image's unwind table (`.pdata`), and must be at most 0x20 bytes before the `cmp`.

use std::fmt;

/// One native: its hash and its handler's offset in the image.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Native {
    pub hash: u64,
    pub rva: u32,
}

/// What a scan found.
#[derive(Debug, Default)]
pub struct Scan {
    /// Sorted by hash.
    pub natives: Vec<Native>,
    /// Hashes seen at more than one handler (all copies dropped).
    pub repeated: usize,
    /// Handler shapes whose start the unwind table does not give.
    pub unplaced: usize,
}

impl Scan {
    pub fn find(&self, hash: u64) -> Option<u32> {
        self.natives
            .binary_search_by_key(&hash, |n| n.hash)
            .ok()
            .map(|i| self.natives[i].rva)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum ScanError {
    NotPe,
    NoCode,
    NoUnwindTable,
    NoNatives,
}

impl fmt::Display for ScanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            ScanError::NotPe => "the image has no PE32+ header",
            ScanError::NoCode => "the image has no code section",
            ScanError::NoUnwindTable => "the image has no unwind table",
            ScanError::NoNatives => "no native handler shape in the code",
        })
    }
}

fn u16_at(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?))
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn u64_at(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(at..at + 8)?.try_into().ok()?))
}

/// The executable sections (offset, size) and the unwind table (offset, size) of a mapped image.
fn layout(image: &[u8]) -> Result<(Vec<(usize, usize)>, (usize, usize)), ScanError> {
    let pe = u32_at(image, 0x3c).ok_or(ScanError::NotPe)? as usize;
    if image.get(pe..pe + 4) != Some(b"PE\0\0".as_slice())
        || u16_at(image, pe + 0x18) != Some(0x20b)
    {
        return Err(ScanError::NotPe);
    }
    let sections = u16_at(image, pe + 6).ok_or(ScanError::NotPe)? as usize;
    let opt_size = u16_at(image, pe + 0x14).ok_or(ScanError::NotPe)? as usize;
    let opt = pe + 0x18;
    let exception = opt + 0x70 + 3 * 8;
    let pdata = (
        u32_at(image, exception).ok_or(ScanError::NotPe)? as usize,
        u32_at(image, exception + 4).ok_or(ScanError::NotPe)? as usize,
    );
    if pdata.1 < 12 || pdata.0 + pdata.1 > image.len() {
        return Err(ScanError::NoUnwindTable);
    }
    let mut code = Vec::new();
    for i in 0..sections {
        let s = opt + opt_size + i * 40;
        let size = u32_at(image, s + 8).ok_or(ScanError::NotPe)? as usize;
        let va = u32_at(image, s + 12).ok_or(ScanError::NotPe)? as usize;
        let flags = u32_at(image, s + 36).ok_or(ScanError::NotPe)?;
        if flags & 0x2000_0000 != 0 && va < image.len() {
            code.push((va, size.min(image.len() - va)));
        }
    }
    if code.is_empty() {
        return Err(ScanError::NoCode);
    }
    Ok((code, pdata))
}

/// The start of the function holding `rva`, from the unwind table (sorted by start).
fn function_start(image: &[u8], pdata: (usize, usize), rva: u32) -> Option<u32> {
    let count = pdata.1 / 12;
    let entry = |i: usize| {
        (
            u32_at(image, pdata.0 + i * 12),
            u32_at(image, pdata.0 + i * 12 + 4),
        )
    };
    let (mut lo, mut hi) = (0, count);
    while lo < hi {
        let mid = (lo + hi) / 2;
        match entry(mid) {
            (Some(begin), _) if begin <= rva => lo = mid + 1,
            _ => hi = mid,
        }
    }
    let (Some(begin), Some(end)) = entry(lo.checked_sub(1)?) else {
        return None;
    };
    (rva < end).then_some(begin)
}

/// The hash in a handler's error block at `at`: `mov rax, [r]` ; `mov rcx, imm64` ; `mov [rax], r`.
fn error_hash(code: &[u8], at: usize) -> Option<u64> {
    let b = code.get(at..at + 16)?;
    // `mov rax, [r]`: mod 00, reg rax, r neither rsp nor rbp (no SIB, no disp32).
    let load = b[0] == 0x48 && b[1] == 0x8b && b[2] & 0xf8 == 0 && !matches!(b[2], 4 | 5);
    let movabs = b[3] == 0x48 && (0xb8..=0xbf).contains(&b[4]);
    // `mov [rax], r`: mod 00, rm rax.
    let store = b[13] == 0x48 && b[14] == 0x89 && b[15] & 0xc7 == 0;
    (load && movabs && store).then(|| u64_at(b, 5)).flatten()
}

/// Finds every native handler in `image`, the title's image as mapped (offsets are RVAs).
pub fn scan(image: &[u8]) -> Result<Scan, ScanError> {
    let (code, pdata) = layout(image)?;
    let mut found: Vec<Native> = Vec::new();
    let mut unplaced = 0;
    for (va, size) in code {
        let text = &image[va..va + size];
        let mut i = 0;
        while let Some(off) = text[i..]
            .windows(4)
            .position(|w| w == [0x83, 0x79, 0x10, 0x00])
        {
            let cmp = i + off;
            i = cmp + 1;
            let Some(tail) = text.get(cmp + 4..cmp + 14) else {
                break;
            };
            let mut target = None;
            for k in 0..6 {
                if tail[k] == 0x78 {
                    target = Some((cmp + 4 + k + 2) as isize + tail[k + 1] as i8 as isize);
                    break;
                }
                if tail[k] == 0x0f && tail[k + 1] == 0x88 {
                    let rel = i32::from_le_bytes(tail[k + 2..k + 6].try_into().unwrap_or_default());
                    target = Some((cmp + 4 + k + 6) as isize + rel as isize);
                    break;
                }
            }
            let Some(hash) = target
                .and_then(|t| usize::try_from(t).ok())
                .and_then(|t| error_hash(text, t))
            else {
                continue;
            };
            let at = (va + cmp) as u32;
            match function_start(image, pdata, at).filter(|&s| at - s <= 0x20) {
                Some(rva) => found.push(Native { hash, rva }),
                None => unplaced += 1,
            }
        }
    }
    found.sort_by_key(|n| (n.hash, n.rva));
    let mut natives = Vec::with_capacity(found.len());
    let mut repeated = 0;
    let mut k = 0;
    while k < found.len() {
        let run = found[k..]
            .iter()
            .take_while(|n| n.hash == found[k].hash)
            .count();
        if run == 1 {
            natives.push(found[k]);
        } else {
            repeated += 1;
        }
        k += run;
    }
    if natives.is_empty() {
        return Err(ScanError::NoNatives);
    }
    Ok(Scan {
        natives,
        repeated,
        unplaced,
    })
}

/// A native call context as the handlers read it.
#[repr(C)]
pub struct CallContext {
    pub args: *mut u64,
    pub ret: *mut u64,
    /// The argument count; a negative one sends the handler down its error path.
    pub count: i32,
    pad: u32,
    /// Room for fields after the count that some handlers may use (vector fix-ups in GTA V).
    rest: [u64; 12],
}

/// The most argument slots a call passes.
pub const MAX_ARGS: usize = 32;

/// Calls the handler at `entry` with `args` and returns its first four result slots.
///
/// # Safety
/// `entry` must be a native handler from [`scan`] in the mapped title image, called on a title
/// thread, with the arguments that native takes.
#[cfg(target_arch = "x86_64")]
pub unsafe fn invoke(entry: usize, args: &[u64]) -> [u64; 4] {
    let mut slots = [0u64; MAX_ARGS];
    let n = args.len().min(MAX_ARGS);
    slots[..n].copy_from_slice(&args[..n]);
    let mut ret = [0u64; 8];
    let mut ctx = CallContext {
        args: slots.as_mut_ptr(),
        ret: ret.as_mut_ptr(),
        count: n as i32,
        pad: 0,
        rest: [0; 12],
    };
    // SAFETY: the caller's contract; the handler reads `ctx` and the slots and writes `ret`.
    unsafe {
        let handler: unsafe extern "C" fn(*mut CallContext) = std::mem::transmute(entry);
        handler(&mut ctx);
    }
    [ret[0], ret[1], ret[2], ret[3]]
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    const TEXT: usize = 0x1000;

    /// A synthetic mapped image: headers, a code section at 0x1000 holding `handlers` (each a
    /// hash, with the `js` near or far) and one decoy, and an unwind table naming each handler.
    pub(crate) fn image(handlers: &[(u64, bool)], unwound: bool) -> Vec<u8> {
        let mut img = vec![0u8; 0x8000];
        let pe = 0x80;
        img[0x3c..0x40].copy_from_slice(&(pe as u32).to_le_bytes());
        img[pe..pe + 4].copy_from_slice(b"PE\0\0");
        img[pe + 6..pe + 8].copy_from_slice(&2u16.to_le_bytes());
        img[pe + 0x14..pe + 0x16].copy_from_slice(&0xf0u16.to_le_bytes());
        img[pe + 0x18..pe + 0x1a].copy_from_slice(&0x20bu16.to_le_bytes());
        let opt = pe + 0x18;
        let sec = opt + 0xf0;
        let mut put_section = |i: usize, va: u32, size: u32, flags: u32| {
            let s = sec + i * 40;
            img[s + 8..s + 12].copy_from_slice(&size.to_le_bytes());
            img[s + 12..s + 16].copy_from_slice(&va.to_le_bytes());
            img[s + 36..s + 40].copy_from_slice(&flags.to_le_bytes());
        };
        put_section(0, TEXT as u32, 0x4000, 0x6000_0020);
        put_section(1, 0x6000, 0x1000, 0x4000_0040);
        let mut code = Vec::new();
        let mut funcs = Vec::new();
        for &(hash, far) in handlers {
            while code.len() % 16 != 0 {
                code.push(0xcc);
            }
            let start = code.len();
            code.extend([0x56, 0x57, 0x48, 0x83, 0xec, 0x28]);
            code.extend([0x83, 0x79, 0x10, 0x00, 0x48, 0x89, 0xce]);
            let body = [0x90u8; 20];
            if far {
                code.extend([0x0f, 0x88]);
                code.extend((body.len() as i32 + 1).to_le_bytes());
            } else {
                code.extend([0x78, body.len() as u8 + 1]);
            }
            code.extend(body);
            code.push(0xc3);
            code.extend([0x48, 0x8b, 0x06, 0x48, 0xb9]);
            code.extend(hash.to_le_bytes());
            code.extend([0x48, 0x89, 0x08, 0xeb, 0x00]);
            funcs.push((start, code.len()));
        }
        // A decoy: the same compare, no error block behind its branch.
        while code.len() % 16 != 0 {
            code.push(0xcc);
        }
        let decoy = code.len();
        code.extend([0x83, 0x79, 0x10, 0x00, 0x78, 0x02, 0x90, 0x90, 0xc3]);
        code.extend([0x90; 16]);
        funcs.push((decoy, code.len()));
        img[TEXT..TEXT + code.len()].copy_from_slice(&code);
        let pdata = 0x6000;
        if unwound {
            for (i, (s, e)) in funcs.iter().enumerate() {
                let at = pdata + i * 12;
                img[at..at + 4].copy_from_slice(&((TEXT + s) as u32).to_le_bytes());
                img[at + 4..at + 8].copy_from_slice(&((TEXT + e) as u32).to_le_bytes());
            }
        }
        let exception = opt + 0x70 + 3 * 8;
        img[exception..exception + 4].copy_from_slice(&(pdata as u32).to_le_bytes());
        img[exception + 4..exception + 8]
            .copy_from_slice(&((funcs.len() * 12) as u32).to_le_bytes());
        img
    }

    #[test]
    fn finds_each_handler_by_its_hash_and_start() {
        let hashes = [
            (0x0bb2_0f74_b5a2_5613, false),
            (0xf617_1953_0675_1ec8, true),
            (0x1234, false),
        ];
        let img = image(&hashes, true);
        let s = scan(&img).unwrap();
        assert_eq!(s.natives.len(), 3, "the decoy is not a native");
        assert_eq!((s.repeated, s.unplaced), (0, 0));
        assert_eq!(
            s.find(0x1234),
            Some(TEXT as u32 + 0x80),
            "sorted by hash, starts from .pdata"
        );
        assert_eq!(s.find(0x0bb2_0f74_b5a2_5613), Some(TEXT as u32));
        assert!(s.find(0xf617_1953_0675_1ec8).is_some(), "far js");
        assert_eq!(s.find(0x5678), None);
    }

    #[test]
    fn repeated_hashes_are_dropped_and_unplaced_handlers_counted() {
        let img = image(&[(7, false), (7, true), (9, false)], true);
        let s = scan(&img).unwrap();
        assert_eq!(
            s.natives,
            vec![Native {
                hash: 9,
                rva: TEXT as u32 + 0x80
            }]
        );
        assert_eq!(s.repeated, 1);
        let img = image(&[(9, false)], false);
        assert_eq!(scan(&img).unwrap_err(), ScanError::NoNatives);
    }

    #[test]
    fn non_images_are_refused() {
        assert_eq!(scan(&[0u8; 0x400]).unwrap_err(), ScanError::NotPe);
        let mut img = image(&[(1, false)], true);
        img[0x80] = b'X';
        assert_eq!(scan(&img).unwrap_err(), ScanError::NotPe);
    }

    /// The real title, mapped by hand (`MODMENU_MAPPED_IMAGE`: the image as loaded, e.g. pefile's
    /// `get_memory_mapped_image()` written to a file): how many natives and where the frame-rate
    /// one is. For re-finding the table after a title update.
    #[test]
    #[ignore]
    fn scans_a_mapped_title() {
        let path = std::env::var("MODMENU_MAPPED_IMAGE").expect("MODMENU_MAPPED_IMAGE");
        let img = std::fs::read(path).unwrap();
        let s = scan(&img).unwrap();
        let fps = s.find(super::super::model::NATIVE_TITLE_FPS);
        println!("natives {} repeated {} unplaced {} title fps {fps:x?}", s.natives.len(), s.repeated, s.unplaced);
        assert!(s.natives.len() > 6000);
    }

    #[test]
    fn the_context_has_the_handlers_layout() {
        assert_eq!(std::mem::offset_of!(CallContext, args), 0);
        assert_eq!(std::mem::offset_of!(CallContext, ret), 8);
        assert_eq!(std::mem::offset_of!(CallContext, count), 0x10);
    }

    /// A stand-in handler with the real calling convention: returns its two arguments' sum.
    unsafe extern "C" fn add(ctx: *mut CallContext) {
        // SAFETY: `invoke` passes a valid context with two slots.
        unsafe {
            let c = &*ctx;
            assert_eq!(c.count, 2);
            *c.ret = *c.args + *c.args.add(1);
        }
    }

    #[test]
    fn invoke_passes_slots_and_reads_the_result() {
        // SAFETY: `add` follows the handler contract.
        let r = unsafe { invoke(add as *const () as usize, &[40, 2]) };
        assert_eq!(r, [42, 0, 0, 0]);
    }
}
