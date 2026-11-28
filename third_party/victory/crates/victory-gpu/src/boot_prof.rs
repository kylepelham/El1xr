//! (bootfast) `VICTORY_BOOT_PROF=1`: where the time from launch to a
//! settled world goes, one line a second from the title's first frame wait
//! until [`AFTER_WORLD_S`] after the first world frame (or [`MOST_S`]).
//!
//! The frame phases (`crate::hitch`) name our calls, but not the time the
//! title sleeps in its own frame waits on our clock (`crate::frame::wait`):
//! a loading screen at a flat 33.3 ms with our presenting thread "waiting
//! for work" is the title pacing itself, and nothing in the log said so.
//! Each line has the frames begun in the second, the milliseconds the
//! title slept in frame waits (origin and other), and every phase's
//! milliseconds on all threads in that second, and the milliseconds presents
//! were held by `VICTORY_PRESENT_SMOOTH` (`crate::pacing`); milestones (first frame,
//! first bake pass, first world frame) are logged once each.
//!
//! Off: nothing is counted and nothing is logged.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::time::Instant;

use crate::hitch::{NAMES, PHASES};

/// Lines go on this long after the first world frame.
const AFTER_WORLD_S: u64 = 60;
/// And stop this long after the first frame wait in any case.
const MOST_S: u64 = 400;

pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_BOOT_PROF").is_some_and(|v| v == "1"))
}

fn epoch() -> Instant {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}

/// Microseconds of every phase on all threads since the start.
static PHASE_US: [AtomicU64; PHASES] = [const { AtomicU64::new(0) }; PHASES];
/// Microseconds the title slept in frame waits: origin, other.
static PACED_US: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];
/// The second of the last line (since [`epoch`]), plus one; 0: none yet.
static LAST_LINE: AtomicU64 = AtomicU64::new(0);
/// The second of the first world frame, plus one; 0: not yet.
static WORLD_AT: AtomicU64 = AtomicU64::new(0);
static FIRST_BAKE: AtomicBool = AtomicBool::new(false);

/// A phase's span closed (`crate::hitch`).
pub(crate) fn phase(index: usize, micros: u64) {
    if on()
        && let Some(c) = PHASE_US.get(index)
    {
        c.fetch_add(micros, Relaxed);
    }
}

/// A frame wait of the title ended after `slept_us` (`crate::frame::wait`).
pub(crate) fn paced(origin: bool, slept_us: u64) {
    if on() {
        PACED_US[usize::from(!origin)].fetch_add(slept_us, Relaxed);
    }
}

/// A bake draw's texture pass ran (`crate::draw`).
pub(crate) fn bake_pass() {
    if on() && !FIRST_BAKE.swap(true, Relaxed) {
        tracing::info!(
            at_ms = epoch().elapsed().as_millis() as u64,
            frame = crate::frame::current(),
            "boot prof: first bake pass"
        );
    }
}

/// The first pass over the heap's buffer views ran: the world's first frame
/// (`crate::draw`).
pub(crate) fn world_frame() {
    if on() && WORLD_AT.load(Relaxed) == 0 {
        let s = epoch().elapsed().as_secs();
        if WORLD_AT
            .compare_exchange(0, s + 1, Relaxed, Relaxed)
            .is_ok()
        {
            tracing::info!(
                at_ms = epoch().elapsed().as_millis() as u64,
                frame = crate::frame::current(),
                "boot prof: first world frame"
            );
        }
    }
}

/// Whether a line is due at second `now` with the last at `last` and the
/// world's first frame at `world` (both plus one, 0: none).
fn due(now: u64, last: u64, world: u64) -> bool {
    let within = now < MOST_S && (world == 0 || now < world - 1 + AFTER_WORLD_S);
    within && (last == 0 || now > last - 1)
}

/// At each origin frame wait: the line for the second just past, when due.
pub(crate) fn tick() {
    if !on() {
        return;
    }
    let started = epoch();
    let now = started.elapsed().as_secs();
    let last = LAST_LINE.load(Relaxed);
    if last == 0
        && LAST_LINE
            .compare_exchange(0, now + 1, Relaxed, Relaxed)
            .is_ok()
    {
        tracing::info!(
            frame = crate::frame::current(),
            "boot prof: first frame wait (VICTORY_BOOT_PROF)"
        );
        return;
    }
    if !due(now, last, WORLD_AT.load(Relaxed))
        || LAST_LINE
            .compare_exchange(last, now + 1, Relaxed, Relaxed)
            .is_err()
    {
        return;
    }
    static FRAME: AtomicU64 = AtomicU64::new(0);
    let frame = crate::frame::current();
    let frames = frame.saturating_sub(FRAME.swap(frame, Relaxed));
    let ms = |c: &AtomicU64| c.swap(0, Relaxed) as f64 / 1000.0;
    let paced = [ms(&PACED_US[0]), ms(&PACED_US[1])];
    #[cfg(windows)]
    let held = crate::pacing::take_held_us() as f64 / 1000.0;
    #[cfg(not(windows))]
    let held = 0.0;
    let mut phases = String::new();
    for (name, c) in NAMES.iter().zip(&PHASE_US) {
        let v = ms(c);
        if v >= 1.0 {
            phases.push_str(&format!(", {name} {v:.0}"));
        }
    }
    tracing::info!(
        "boot prof: second {now} (frame {frame}, {frames} frames, world {}): the title slept in frame waits {:.0} ms origin, {:.0} ms other; presents held by smoothing {held:.0} ms; phases on all threads (ms){phases}",
        WORLD_AT.load(Relaxed) != 0,
        paced[0],
        paced[1],
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_once_a_second_until_after_the_world() {
        assert!(due(3, 0, 0), "the first");
        assert!(!due(3, 4, 0), "the same second");
        assert!(due(4, 4, 0), "the next second");
        assert!(due(70, 70, 20), "under a minute after the world");
        assert!(!due(80, 80, 20), "a minute after the world");
        assert!(!due(MOST_S, MOST_S, 0), "at the latest");
    }

    #[test]
    fn off_without_the_switch() {
        if std::env::var_os("VICTORY_BOOT_PROF").is_none() {
            phase(0, 1000);
            paced(true, 1000);
            assert_eq!(PHASE_US[0].load(Relaxed), 0);
            assert_eq!(PACED_US[0].load(Relaxed), 0);
        }
    }
}
