//! The end of the title's loading, for `VICTORY_LOAD_FPS=<rate>` (the
//! loader's, `victory_loader::frame_rate`): the title runs at that rate
//! while it loads, and once loading is over at its playing 30, our clock
//! with it. Loading is over when no draw of the title's skin and decoration
//! bake (`crate::draw::BAKE_PIXEL_SHADERS`) was seen for [`HOLD_MS`], or
//! [`MOST_MS`] after the first frame whatever happens.
//!
//! The title bakes its characters' skin into textures of its own a set
//! number of frames after it asks for the textures the bake reads; at 25
//! frames a second those had not landed yet, and every head was without
//! skin for the whole run (RX 7900 XTX: 20 runs of 20; at 5 a second while
//! loading, 2 of 2 with skin). (avatarmanz/VIctory-7900XTX PR #449)
//!
//! (bootfast) For a faster loading screen instead: `VICTORY_LOAD_END=world`
//! ends loading at the world's first frame (the first pass over the heap's
//! buffer views, `crate::draw`) rather than after the bake hold, so a rate
//! above 30 (`VICTORY_LOAD_FPS=999`) never reaches the world; and
//! `VICTORY_LOAD_UNPACED=1` lets no frame wait sleep on our clock until that
//! frame (`crate::frame::wait`, as `VICTORY_PACE=0` but for loading only;
//! our clock follows the title's interval, at most 60 Hz as the display we
//! report has no 120 Hz mode). In batch 3 the loading screen ran at a flat
//! 33.3 ms for 10 s with the disk at 86-150 MB/s and our presenting thread
//! waiting for work: the title pacing itself.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// No bake for this long ends loading.
const HOLD_MS: u64 = 10_000;
/// Loading ends this long after the first frame in any case.
const MOST_MS: u64 = 600_000;
/// The interval of the title's playing 30 frames a second.
const PLAYING: Duration = Duration::from_micros(33_333);

fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var("VICTORY_LOAD_FPS")
            .ok()
            .and_then(|v| v.trim().parse::<u32>().ok())
            .is_some_and(|r| (1..1000).contains(&r))
    })
}

/// Milliseconds since our first look at the time.
fn since_start_ms() -> u64 {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    EPOCH.get_or_init(Instant::now).elapsed().as_millis() as u64
}

/// (bootfast) `VICTORY_LOAD_END=world`: loading ends at the world's first frame.
fn end_at_world() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("VICTORY_LOAD_END").is_ok_and(|v| v.trim() == "world"))
}

/// (bootfast) `VICTORY_LOAD_UNPACED=1`: frame waits do not sleep until the
/// world's first frame.
fn unpaced_loading() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_LOAD_UNPACED").is_some_and(|v| v == "1"))
}

/// The world's first frame has been seen.
static WORLD: AtomicBool = AtomicBool::new(false);

/// The world's first frame (`crate::draw`, the first pass over the heap's
/// buffer views).
pub(crate) fn world_frame() {
    if !WORLD.swap(true, Ordering::Relaxed) && (end_at_world() || unpaced_loading()) {
        if unpaced_loading() {
            crate::frame::rebase();
        }
        tracing::info!(
            at_ms = since_start_ms(),
            frame = crate::frame::current(),
            "the world's first frame: frame waits paced again (VICTORY_LOAD_UNPACED), loading over at the next frame wait (VICTORY_LOAD_END=world)"
        );
    }
}

/// Whether a frame wait sleeps on our clock now (`crate::frame::wait`).
pub(crate) fn paced_now() -> bool {
    paced(unpaced_loading(), WORLD.load(Ordering::Relaxed))
}

/// With `VICTORY_LOAD_UNPACED` (`unpaced`): not before the world's first
/// frame (`world`).
fn paced(unpaced: bool, world: bool) -> bool {
    !unpaced || world
}

/// When a bake draw was last seen ([`since_start_ms`] plus one; 0: none).
static LAST_BAKE: AtomicU64 = AtomicU64::new(0);

/// A draw of the title's skin and decoration bake (`crate::draw`).
pub(crate) fn bake_seen() {
    if on() {
        LAST_BAKE.store(since_start_ms() + 1, Ordering::Relaxed);
    }
}

/// Whether loading is over at `now` with the last bake at `last_bake`
/// (both as [`LAST_BAKE`] keeps them); with `at_world`
/// (`VICTORY_LOAD_END=world`) once the world's first frame was seen (`world`)
/// instead of after the bake hold.
fn over(now: u64, last_bake: u64, at_world: bool, world: bool) -> bool {
    let ended = if at_world {
        world
    } else {
        last_bake != 0 && now > last_bake - 1 + HOLD_MS
    };
    ended || now > MOST_MS
}

/// At the start of each frame wait for the origin: ends loading when it is
/// over, our clock then at the playing interval.
pub(crate) fn gate(origin: bool) {
    static DONE: AtomicBool = AtomicBool::new(false);
    if !origin || DONE.load(Ordering::Relaxed) {
        return;
    }
    if !on() {
        DONE.store(true, Ordering::Relaxed);
        return;
    }
    let now = since_start_ms();
    let bake = LAST_BAKE.load(Ordering::Relaxed);
    if !over(now, bake, end_at_world(), WORLD.load(Ordering::Relaxed))
        || DONE.swap(true, Ordering::Relaxed)
    {
        return;
    }
    // The title asked for its loading rate's interval and will not ask
    // again: ours is the playing one from now.
    crate::frame::restart(PLAYING);
    victory_core::LOADING_OVER.store(true, Ordering::Relaxed);
    tracing::info!(
        at_ms = now,
        last_bake_ms = bake.saturating_sub(1),
        "loading over: the title's playing rate from now (VICTORY_LOAD_FPS)"
    );
}

/// The interval our clock keeps once loading is over, whatever the title
/// asks for (it may still ask for its loading rate's).
pub(crate) fn playing_interval() -> Option<Duration> {
    (on() && victory_core::LOADING_OVER.load(Ordering::Relaxed)).then_some(PLAYING)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loading_is_over_after_a_quiet_hold_or_at_the_latest() {
        assert!(!over(5_000, 0, false, false), "no bake yet");
        assert!(!over(15_000, 6_001, false, false), "a bake 9 s ago");
        assert!(over(16_002, 6_001, false, false), "none for 10 s");
        assert!(over(MOST_MS + 1, 0, false, false), "at the latest");
        assert!(!over(15_000, 6_001, false, true), "the world alone ends nothing without the switch");
    }

    /// `VICTORY_LOAD_END=world`: over at the world's first frame, not at the
    /// bake hold.
    #[test]
    fn loading_ends_at_the_world_with_the_switch() {
        assert!(!over(16_002, 6_001, true, false), "a quiet hold is not the world");
        assert!(over(5_000, 4_000, true, true), "the world, a bake 1 s ago");
        assert!(over(MOST_MS + 1, 0, true, false), "at the latest");
    }

    /// `VICTORY_LOAD_UNPACED=1`: frame waits sleep again from the world's
    /// first frame; without it, always.
    #[test]
    fn unpaced_only_while_loading() {
        assert!(paced(false, false));
        assert!(paced(false, true));
        assert!(!paced(true, false));
        assert!(paced(true, true));
        if std::env::var_os("VICTORY_LOAD_UNPACED").is_none() {
            assert!(paced_now());
        }
    }

    /// Without the switch nothing changes the interval.
    #[test]
    fn off_without_the_switch() {
        if std::env::var_os("VICTORY_LOAD_FPS").is_none() {
            assert_eq!(playing_interval(), None);
            bake_seen();
            assert_eq!(LAST_BAKE.load(Ordering::Relaxed), 0);
        }
    }
}
