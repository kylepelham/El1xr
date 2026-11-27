//! The title's frame rate while it loads (`VICTORY_LOAD_FPS=<rate>`, off
//! without it; avatarmanz/VIctory-7900XTX PR #449): the rate the game asks
//! for is a setting of its own, and three places in its code are changed at
//! load to read another, then changed back to its playing rate once
//! loading is over ([`change`]). The sites and their reading are from
//! readmet3xt-lab/foxhound `crates/victory-loader/src/frame_rate.rs`
//! (its `VICTORY_FRAME_INTERVAL_MS`, not ported).
//!
//! What the title does (game_scarlett_master.exe 1.0.2.1, image offsets):
//!
//! - Its graphics settings hold a frame rate (+0x210 of the settings object,
//!   30 on Series X). Two functions that apply the settings, +0x1e0fb30 and
//!   +0x22434a0, turn it into a frame time: `frame_us = 1e6 /
//!   clamp(rate)`, 0 and 1000 or more being 1000 (the code's "no limit"),
//!   stored at +0x98f84ac.
//! - +0x247ab10 sets the frame interval from it: the display's rate is 60,
//!   or 120 if its mode list (the output's `GetDisplayModeList`) has a
//!   120 Hz mode (+0x2481ca0); the divisor is `min(display / rate, 4)`, or
//!   1 for a rate above the display's; the interval is a table at
//!   +0x61ece60 (`[16667, 16667, 33333, 50000, 66667]` at 60 Hz, `[8333,
//!   8333, 16667, 25000, 33333]` at 120 Hz) and goes to
//!   `SetFrameIntervalX` (device +0x270), with `ScheduleFrameEventX`
//!   (+0x278) after it. At 30 and 60 Hz: 33,333 us, the flat 33.3 ms of the
//!   menus and videos.
//! - +0x120530, once a flag (+0x7453ec9, set by +0x21b77c0) is up, is a
//!   limiter of its own on `QueryPerformanceCounter`: it waits until
//!   `1000 / trunc(1e6 / frame_us)` ms have passed since the last frame.
//! - +0x3eaf5d0 hands `1e6 / frame_us` to scripts, and +0x234490 scales
//!   per-frame amounts by it (1.0 below 60 except 0.7 at 40, 0.5 from 60).
//!
//! - The simulation: +0x22264d0 hands each of its timers the real time
//!   since the last frame (`QueryPerformanceCounter`); +0x2226040 makes the
//!   step of it, at most 1/15 s and at least `1 / rate` (+0x6849844, made
//!   at +0x22435de and +0x2241190 from +0x2241fd0, which reads the same
//!   setting). So at 30 every frame shorter than 33.3 ms still moves game
//!   time 33.3 ms: a fixed 30 Hz step for fast frames, delta time for slow
//!   ones.
//!
//! So reporting a 120 Hz display alone changes nothing at 30 (divisor 4,
//! 33,333 us). Here the two loads of the setting (`mov eax, [reg+210h]`)
//! become `mov eax, rate ; nop`, so the title's frame time, interval and
//! own limiter follow the rate asked for, and +0x2241fd0 returns the rate
//! too (1000 for no limit), so the shortest step follows; the setting
//! itself is left as it is. Each site's bytes, from the load to the store of the frame time, are
//! compared before anything is written; a site whose bytes differ is left
//! alone and said so (another build of the title).

/// `VICTORY_LOAD_FPS=<rate>`: the title's rate while it loads, 1 to 999;
/// `None` unset or malformed.
pub fn asked_load_rate() -> Option<u32> {
    parse_load_rate(&std::env::var("VICTORY_LOAD_FPS").ok()?)
}

/// `5` → 5; 0, 1000 and more, or not a number → `None`.
pub fn parse_load_rate(spec: &str) -> Option<u32> {
    spec.trim().parse::<u32>().ok().filter(|r| (1..1000).contains(r))
}

/// The title's own rate on Series X, its playing rate once loading is over.
pub const PLAYING_RATE: u32 = 30;

/// The frame time the title derives from a rate, in microseconds, as its
/// code computes it (single-precision division, truncated).
pub fn title_frame_us(rate: u32) -> u32 {
    let clamped = if rate == 0 || rate >= 1000 { 1000 } else { rate };
    (1_000_000.0_f32 / clamped as f32) as u32
}

/// The interval the title then asks for (`SetFrameIntervalX`), in
/// microseconds, on a display of 60 Hz (or 120 Hz).
pub fn title_interval_us(rate: u32, display_120: bool) -> u32 {
    const TABLE: [[u32; 5]; 2] = [[16667, 16667, 33333, 50000, 66667], [8333, 8333, 16667, 25000, 33333]];
    let fps = (1_000_000.0_f32 / title_frame_us(rate) as f32) as u32 & 0xffff;
    let display = if display_120 { 120 } else { 60 };
    let index = if fps <= display { (display / fps.max(1)).min(4) } else { 1 };
    TABLE[usize::from(display_120)][index as usize]
}

/// A read of the setting to change: its offset in the image, what it is,
/// and the bytes there (compared first; the replacement goes at the start).
pub struct Site {
    pub at: usize,
    pub kind: Kind,
    pub bytes: &'static [u8],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// `mov eax, [reg+210h]` before the frame time is made of it: becomes
    /// `mov eax, rate ; nop` (6 bytes).
    Load,
    /// +0x2241fd0, the rate the timers' shortest step is made of (`1 /
    /// rate`, +0x6849844): `cmp dword [rax+210h], 0` becomes `mov eax, rate
    /// ; jmp` to its `vcvtsi2ss` (7 bytes). Left as it was, the timers step at
    /// least 1/30 s however short the frame: at 60 frames a second the game
    /// ran at twice its speed (run fps-1, menus). For no limit, 1000.
    SimRate,
}

/// The three reads (game_scarlett_master.exe 1.0.2.1, image offsets).
pub const SITES: [Site; 3] = [
    // +0x1e0fb30: mov eax, [rdi+210h] ... mov [+0x98f84ac], eax
    Site {
        at: 0x1e0fc62,
        kind: Kind::Load,
        bytes: &[
            0x8b, 0x87, 0x10, 0x02, 0x00, 0x00, 0xba, 0xe8, 0x03, 0x00, 0x00, 0xc5, 0xfa, 0x10, 0x0d, 0x9f, 0xa1, 0x34, 0x04, 0xb9, 0xe8,
            0x03, 0x00, 0x00, 0x3d, 0xe8, 0x03, 0x00, 0x00, 0x0f, 0x42, 0xd0, 0x85, 0xc0, 0x0f, 0x44, 0xd1, 0xc5, 0xea, 0x2a, 0xc2, 0xc5,
            0xf2, 0x5e, 0xc0, 0xc4, 0xe1, 0xfa, 0x2c, 0xc0, 0x89, 0x05, 0x12, 0x88, 0xae, 0x07,
        ],
    },
    // +0x22434a0: mov eax, [rsi+210h] ... mov [+0x98f84ac], eax
    Site {
        at: 0x224359a,
        kind: Kind::Load,
        bytes: &[
            0x8b, 0x86, 0x10, 0x02, 0x00, 0x00, 0xba, 0xe8, 0x03, 0x00, 0x00, 0xc5, 0xfa, 0x10, 0x0d, 0x67, 0x68, 0xf1, 0x03, 0xb9, 0xe8,
            0x03, 0x00, 0x00, 0x3d, 0xe8, 0x03, 0x00, 0x00, 0x0f, 0x42, 0xd0, 0x85, 0xc0, 0x0f, 0x44, 0xd1, 0xb1, 0xff, 0xc5, 0xea, 0x2a,
            0xc2, 0xc5, 0xf2, 0x5e, 0xc0, 0xc4, 0xe1, 0xfa, 0x2c, 0xc0, 0x89, 0x05, 0xd8, 0x4e, 0x6b, 0x07,
        ],
    },
    // +0x2241fd0: cmp dword [rax+210h], 0 ; je ; call ; mov eax, [rax+210h] ; vcvtsi2ss xmm0, xmm0, rax
    Site {
        at: 0x2241fdb,
        kind: Kind::SimRate,
        bytes: &[
            0x83, 0xb8, 0x10, 0x02, 0x00, 0x00, 0x00, 0x74, 0x12, 0xe8, 0xc7, 0xa1, 0xe0, 0x02, 0x8b, 0x80, 0x10, 0x02, 0x00, 0x00, 0xc4,
            0xe1, 0xfa, 0x2a, 0xc0,
        ],
    },
];

/// The title's frame time in microseconds (+0x98f84ac), made of the rate
/// when it applies its settings.
pub const FRAME_US: usize = 0x98f84ac;

/// The shortest step of the title's timers, `1 / rate` in seconds (an
/// `f32`), which the title makes of +0x2241fd0's rate when it applies its
/// settings (+0x22435de, +0x2241190).
pub const MIN_STEP: usize = 0x6849844;

/// What goes at a site of `kind` for `rate`.
pub fn replacement(kind: Kind, rate: u32) -> Vec<u8> {
    match kind {
        // mov eax, rate ; nop
        Kind::Load => {
            let r = rate.to_le_bytes();
            vec![0xb8, r[0], r[1], r[2], r[3], 0x90]
        }
        // mov eax, rate ; jmp +13 (to the vcvtsi2ss, 20 bytes on)
        Kind::SimRate => {
            let r = (if rate == 0 { 1000 } else { rate }).to_le_bytes();
            vec![0xb8, r[0], r[1], r[2], r[3], 0xeb, 0x0d]
        }
    }
}

/// What became of a site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Patched {
    Done,
    /// Its bytes are not the ones expected: left alone.
    Differs,
    Failed(&'static str),
}

/// Makes each site of `sites` in the image of `size` bytes at `base` load
/// `rate`. What became of each, said in the log too.
///
/// # Safety
/// The image stays mapped and no thread runs its code yet.
#[cfg(windows)]
pub unsafe fn apply(base: usize, size: usize, rate: u32, sites: &[Site]) -> Vec<Patched> {
    use windows_sys::Win32::System::Diagnostics::Debug::FlushInstructionCache;
    use windows_sys::Win32::System::Memory::{PAGE_EXECUTE_READWRITE, VirtualProtect};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    sites
        .iter()
        .map(|site| {
            let code = replacement(site.kind, rate);
            let len = code.len();
            let done = (|| {
                if site.bytes.len() < len || site.at.checked_add(site.bytes.len()).is_none_or(|end| end > size) {
                    return Patched::Failed("outside the image");
                }
                let at = base + site.at;
                // SAFETY: inside the mapped image (checked above).
                let there = unsafe { std::slice::from_raw_parts(at as *const u8, site.bytes.len()) };
                if there != site.bytes {
                    return Patched::Differs;
                }
                let mut old = 0;
                // SAFETY: the load's bytes, made writable for this and put
                // back; nothing runs them yet.
                unsafe {
                    if VirtualProtect(at as *const _, len, PAGE_EXECUTE_READWRITE, &mut old) == 0 {
                        return Patched::Failed("Windows would not make it writable");
                    }
                    std::ptr::copy_nonoverlapping(code.as_ptr(), at as *mut u8, len);
                    VirtualProtect(at as *const _, len, old, &mut old);
                    FlushInstructionCache(GetCurrentProcess(), at as *const _, len);
                }
                Patched::Done
            })();
            let offset = format_args!("{:#x}", site.at);
            match done {
                Patched::Done => tracing::info!(site = %offset, kind = ?site.kind, rate, "frame rate: the title's read of its frame rate setting patched (VICTORY_LOAD_FPS)"),
                Patched::Differs => tracing::warn!(site = %offset, "frame rate: not patched, the title's bytes there are not the ones expected (another build?)"),
                Patched::Failed(why) => tracing::warn!(site = %offset, why, "frame rate: not patched"),
            }
            done
        })
        .collect()
}

/// The rate a frame rate stands for in the title's code (0: its "no
/// limit", 1000).
fn effective(rate: u32) -> u32 {
    if rate == 0 || rate >= 1000 { 1000 } else { rate }
}

/// Gives the title `rate` in place of `from` while it runs
/// (`VICTORY_LOAD_FPS`: `from` while it loads, its playing rate after):
/// each site [`apply`] patched for `from` loads `rate`, and what the title
/// made of `from` when it applied its settings is made again of `rate`, as
/// it would have: the frame time ([`FRAME_US`]) and the timers' shortest
/// step ([`MIN_STEP`], only if it holds `1 / from`). The interval it asked
/// for is our clock's business (`victory-gpu`'s `load_rate`). Returns the
/// sites changed. (avatarmanz/VIctory-7900XTX PR #449)
///
/// # Safety
/// The image at `base` is the title's, `size` bytes, mapped.
#[cfg(windows)]
pub unsafe fn change(base: usize, size: usize, from: u32, rate: u32, sites: &[Site]) -> usize {
    use windows_sys::Win32::System::Diagnostics::Debug::FlushInstructionCache;
    use windows_sys::Win32::System::Memory::{PAGE_EXECUTE_READWRITE, VirtualProtect};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let mut changed = 0;
    for site in sites {
        let (old, new) = (replacement(site.kind, from), replacement(site.kind, rate));
        if site.at.checked_add(old.len()).is_none_or(|end| end > size) {
            continue;
        }
        let at = base + site.at;
        // SAFETY: inside the mapped image (checked above).
        if unsafe { std::slice::from_raw_parts(at as *const u8, old.len()) } != old.as_slice() {
            continue;
        }
        let mut was = 0;
        // SAFETY: the bytes `apply` wrote, made writable for this and put
        // back. (Only the immediates change; the title runs these when it
        // applies its settings.)
        unsafe {
            if VirtualProtect(at as *const _, new.len(), PAGE_EXECUTE_READWRITE, &mut was) == 0 {
                continue;
            }
            std::ptr::copy_nonoverlapping(new.as_ptr(), at as *mut u8, new.len());
            VirtualProtect(at as *const _, new.len(), was, &mut was);
            FlushInstructionCache(GetCurrentProcess(), at as *const _, new.len());
        }
        changed += 1;
    }
    if FRAME_US + 4 <= size {
        // SAFETY: title data inside the image, a u32 the title writes.
        unsafe { ((base + FRAME_US) as *mut u32).write_volatile(title_frame_us(rate)) };
    }
    if MIN_STEP + 4 <= size {
        let at = (base + MIN_STEP) as *mut f32;
        // SAFETY: as above, an f32.
        let step = unsafe { at.read_volatile() };
        let expected = 1.0 / effective(from) as f32;
        if (step - expected).abs() < 1e-5 {
            // SAFETY: as above.
            unsafe { at.write_volatile(1.0 / effective(rate) as f32) };
            tracing::info!(from, rate, "frame rate: the title's rate changed while it runs (sites, frame time, timers' shortest step)");
        } else {
            tracing::warn!(from, rate, step, expected, "frame rate: the timers' shortest step is not 1 / the loading rate: left as it is");
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_rates() {
        assert_eq!(parse_load_rate(" 5 "), Some(5));
        assert_eq!(parse_load_rate("0"), None);
        assert_eq!(parse_load_rate("1000"), None);
        assert_eq!(parse_load_rate("slow"), None);
    }

    /// The title's own arithmetic: its 30 is the 33,333 us the log shows,
    /// on a 60 Hz display and on a 120 Hz one.
    #[test]
    fn the_titles_intervals() {
        assert_eq!(title_frame_us(30), 33333);
        assert_eq!(title_frame_us(60), 16666);
        assert_eq!(title_frame_us(0), 1000);
        assert_eq!(title_frame_us(5000), 1000);
        assert_eq!(title_interval_us(30, false), 33333);
        assert_eq!(title_interval_us(30, true), 33333);
        assert_eq!(title_interval_us(40, true), 25000);
        assert_eq!(title_interval_us(60, false), 16667);
        assert_eq!(title_interval_us(60, true), 16667);
        assert_eq!(title_interval_us(120, true), 8333);
        assert_eq!(title_interval_us(120, false), 16667);
        assert_eq!(title_interval_us(0, false), 16667);
        assert_eq!(title_interval_us(0, true), 8333);
        assert_eq!(title_interval_us(20, false), 50000);
    }


    #[test]
    fn the_sites_are_reads_of_the_setting() {
        for site in SITES.iter().filter(|s| s.kind == Kind::Load) {
            // mov eax, [rdi/rsi+210h]
            assert_eq!(site.bytes[0], 0x8b);
            assert!(matches!(site.bytes[1], 0x86 | 0x87));
            assert_eq!(&site.bytes[2..6], &[0x10, 0x02, 0x00, 0x00]);
            // mov edx, 1000 follows; the window ends with the store of eax.
            assert_eq!(&site.bytes[6..11], &[0xba, 0xe8, 0x03, 0x00, 0x00]);
            assert_eq!(&site.bytes[site.bytes.len() - 6..site.bytes.len() - 4], &[0x89, 0x05]);
            // RIP-relative: the store is of +0x98f84ac.
            let disp = i32::from_le_bytes(site.bytes[site.bytes.len() - 4..].try_into().unwrap());
            let end = site.at + site.bytes.len();
            assert_eq!(end as i64 + i64::from(disp), FRAME_US as i64);
        }
        let sim = SITES.iter().find(|s| s.kind == Kind::SimRate).unwrap();
        // cmp dword [rax+210h], 0 ; ... ; mov eax, [rax+210h] at 14 ; vcvtsi2ss at 20
        assert_eq!(&sim.bytes[..7], &[0x83, 0xb8, 0x10, 0x02, 0x00, 0x00, 0x00]);
        assert_eq!(&sim.bytes[14..20], &[0x8b, 0x80, 0x10, 0x02, 0x00, 0x00]);
        assert_eq!(&sim.bytes[20..], &[0xc4, 0xe1, 0xfa, 0x2a, 0xc0]);
        let code = replacement(Kind::SimRate, 60);
        assert_eq!(code.len() + usize::from(code[6]), 20, "the jump lands on the vcvtsi2ss");
        assert_eq!(replacement(Kind::Load, 60), [0xb8, 60, 0, 0, 0, 0x90]);
        assert_eq!(replacement(Kind::SimRate, 0)[1..5], 1000u32.to_le_bytes());
    }

    #[cfg(windows)]
    #[test]
    fn sites_take_the_rate_and_others_are_left_alone() {
        use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_EXECUTE_READWRITE, VirtualAlloc, VirtualFree};
        // SAFETY: fresh memory of our own, written and run inside it.
        unsafe {
            let place = VirtualAlloc(std::ptr::null(), 0x10000, MEM_COMMIT | MEM_RESERVE, PAGE_EXECUTE_READWRITE) as *mut u8;
            assert!(!place.is_null());
            let bytes = SITES[0].bytes;
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), place.add(0x100), bytes.len());
            let mut other = SITES[1].bytes.to_vec();
            other[2] = 0x14; // [rsi+214h]
            std::ptr::copy_nonoverlapping(other.as_ptr(), place.add(0x200), other.len());
            // The simulation's rate as a function: rax = settings (+0x210 = 30),
            // the window, ret.
            let settings = [30u32; 0x100];
            let sim = SITES[2].bytes;
            let mut f = vec![0x48, 0xb8];
            f.extend((settings.as_ptr() as u64).to_le_bytes());
            let window_at = f.len();
            f.extend(sim);
            f.push(0xc3);
            std::ptr::copy_nonoverlapping(f.as_ptr(), place.add(0x300), f.len());
            let mine = [
                Site { at: 0x100, kind: Kind::Load, bytes },
                Site { at: 0x200, kind: Kind::Load, bytes: SITES[1].bytes },
                Site { at: 0x300 + window_at, kind: Kind::SimRate, bytes: sim },
                Site { at: 0xfff0, kind: Kind::Load, bytes },
            ];
            let done = apply(place as usize, 0x10000, 60, &mine);
            assert_eq!(done[..3], [Patched::Done, Patched::Differs, Patched::Done]);
            assert!(matches!(done[3], Patched::Failed(_)));
            let after = std::slice::from_raw_parts(place.add(0x100), bytes.len());
            assert_eq!(after[..6], replacement(Kind::Load, 60)[..]);
            assert_eq!(after[6..], bytes[6..]);
            assert_eq!(std::slice::from_raw_parts(place.add(0x200), other.len()), &other[..]);
            let rate: extern "system" fn() -> f32 = std::mem::transmute(place.add(0x300));
            assert_eq!(rate(), 60.0);
            VirtualFree(place.cast(), 0, MEM_RELEASE);
        }
    }

    /// After `apply` at the loading rate, `change` puts the playing rate in
    /// each patched site and leaves one it did not patch alone.
    #[cfg(windows)]
    #[test]
    fn the_loading_rate_is_changed_to_the_playing_one() {
        use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_EXECUTE_READWRITE, VirtualAlloc, VirtualFree};
        // SAFETY: fresh memory of our own.
        unsafe {
            let place = VirtualAlloc(std::ptr::null(), 0x10000, MEM_COMMIT | MEM_RESERVE, PAGE_EXECUTE_READWRITE) as *mut u8;
            assert!(!place.is_null());
            let bytes = SITES[0].bytes;
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), place.add(0x100), bytes.len());
            std::ptr::copy_nonoverlapping(SITES[2].bytes.as_ptr(), place.add(0x200), SITES[2].bytes.len());
            let mut other = SITES[1].bytes.to_vec();
            other[2] = 0x14;
            std::ptr::copy_nonoverlapping(other.as_ptr(), place.add(0x300), other.len());
            let mine = [
                Site { at: 0x100, kind: Kind::Load, bytes },
                Site { at: 0x200, kind: Kind::SimRate, bytes: SITES[2].bytes },
                Site { at: 0x300, kind: Kind::Load, bytes: SITES[1].bytes },
            ];
            let done = apply(place as usize, 0x10000, 5, &mine);
            assert_eq!(done, [Patched::Done, Patched::Done, Patched::Differs]);
            // The image is smaller than FRAME_US and MIN_STEP: only the sites.
            assert_eq!(change(place as usize, 0x10000, 5, 30, &mine), 2);
            assert_eq!(std::slice::from_raw_parts(place.add(0x100), 6), &replacement(Kind::Load, 30)[..]);
            assert_eq!(std::slice::from_raw_parts(place.add(0x200), 7), &replacement(Kind::SimRate, 30)[..]);
            assert_eq!(std::slice::from_raw_parts(place.add(0x300), other.len()), &other[..]);
            // Done once, the loading rate is no longer there to change.
            assert_eq!(change(place as usize, 0x10000, 5, 30, &mine), 0);
            VirtualFree(place.cast(), 0, MEM_RELEASE);
        }
        assert_eq!(effective(0), 1000);
        assert_eq!(effective(5), 5);
    }
}
