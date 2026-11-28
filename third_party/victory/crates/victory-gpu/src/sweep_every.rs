//! (nvaudit) `VICTORY_SWEEP_EVERY_MS=<ms>`: how long between the sweeps made
//! behind flights (`host::late_free_present`, the launcher's `VICTORY_RELAY=2`).
//! Each one settles everything in the air first; on an RTX 5070 Ti that frame
//! took 36-82 ms instead of ~31, one frame over 50 ms every 1.5 s, and none
//! without sweeps. Unset: every 1.5 s, as before. Set above 1,500: that long
//! while memory is not pressed, and back to 1.5 s while it is (the card past
//! the line textures are made up to, `resource::room_lately`, where the sweep
//! is what lets idle textures go; or the PC's memory load at 90% or more).
//! Looked at twice a second; a change is logged.
//!
//! Source: trustno1-coder/VIctory-NVIDIA PR #340 (bentsignal), adapted: their
//! pressure test (`residency::pressured`, `pool::texture_room`) is ours by
//! `resource::room_lately` and the system memory load.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};

/// The sweep period without the switch, and while memory is pressed.
pub(crate) const PRESSED_MS: u64 = 1500;
/// The PC's memory load, in percent, from which memory counts as pressed.
const RAM_LOAD_PRESSED: u32 = 90;
/// How often the pressure is looked at.
const LOOK_MS: u64 = 500;

fn asked() -> Option<u64> {
    static EVERY: OnceLock<Option<u64>> = OnceLock::new();
    *EVERY.get_or_init(|| {
        let every = parse(std::env::var("VICTORY_SWEEP_EVERY_MS").ok().as_deref());
        if let Some(ms) = every {
            tracing::info!(
                every_ms = ms,
                pressed_ms = PRESSED_MS,
                "sweeps behind flights: VICTORY_SWEEP_EVERY_MS"
            );
        }
        every
    })
}

/// The switch's value: `None` unset, unreadable or 0.
fn parse(v: Option<&str>) -> Option<u64> {
    v.and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&ms| ms > 0)
}

/// The period for `asked` and whether memory is pressed.
fn pick(asked: Option<u64>, pressed: bool) -> u64 {
    match asked {
        None => PRESSED_MS,
        Some(every) if every <= PRESSED_MS || pressed => every.min(PRESSED_MS),
        Some(every) => every,
    }
}

fn ram_pressed() -> bool {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    // SAFETY: plain data the call fills in.
    let mut m: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    m.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
    // SAFETY: a sized structure.
    let ok = unsafe { GlobalMemoryStatusEx(&mut m) } != 0;
    ok && m.dwMemoryLoad >= RAM_LOAD_PRESSED
}

/// Milliseconds between sweeps behind flights, now.
pub(crate) fn every_ms() -> u64 {
    let Some(every) = asked() else {
        return PRESSED_MS;
    };
    if every <= PRESSED_MS {
        return every;
    }
    static LOOKED: AtomicU64 = AtomicU64::new(0);
    static PRESSED: AtomicBool = AtomicBool::new(true);
    let now = crate::host::now_ms();
    if now >= LOOKED.load(Relaxed) + LOOK_MS {
        LOOKED.store(now, Relaxed);
        let pressed = !crate::resource::room_lately() || ram_pressed();
        if PRESSED.swap(pressed, Relaxed) != pressed {
            tracing::info!(
                pressed,
                every_ms = pick(Some(every), pressed),
                "sweeps behind flights"
            );
        }
    }
    pick(Some(every), PRESSED.load(Relaxed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unset_keeps_the_old_period() {
        assert_eq!(parse(None), None);
        assert_eq!(parse(Some("")), None);
        assert_eq!(parse(Some("0")), None);
        assert_eq!(parse(Some("x")), None);
        assert_eq!(pick(None, false), PRESSED_MS);
        assert_eq!(pick(None, true), PRESSED_MS);
    }

    #[test]
    fn a_long_period_holds_only_while_memory_is_not_pressed() {
        let every = parse(Some(" 15000 "));
        assert_eq!(every, Some(15_000));
        assert_eq!(pick(every, false), 15_000);
        assert_eq!(pick(every, true), PRESSED_MS);
    }

    #[test]
    fn a_short_period_is_taken_as_asked() {
        assert_eq!(pick(Some(500), false), 500);
        assert_eq!(pick(Some(500), true), 500);
    }
}
