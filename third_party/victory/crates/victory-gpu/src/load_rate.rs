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

/// When a bake draw was last seen ([`since_start_ms`] plus one; 0: none).
static LAST_BAKE: AtomicU64 = AtomicU64::new(0);

/// A draw of the title's skin and decoration bake (`crate::draw`).
pub(crate) fn bake_seen() {
    if on() {
        LAST_BAKE.store(since_start_ms() + 1, Ordering::Relaxed);
    }
}

/// Whether loading is over at `now` with the last bake at `last_bake`
/// (both as [`LAST_BAKE`] keeps them).
fn over(now: u64, last_bake: u64) -> bool {
    (last_bake != 0 && now > last_bake - 1 + HOLD_MS) || now > MOST_MS
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
    if !over(now, bake) || DONE.swap(true, Ordering::Relaxed) {
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
        assert!(!over(5_000, 0), "no bake yet");
        assert!(!over(15_000, 6_001), "a bake 9 s ago");
        assert!(over(16_002, 6_001), "none for 10 s");
        assert!(over(MOST_MS + 1, 0), "at the latest");
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
