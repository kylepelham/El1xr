//! `VICTORY_BAKE_SPREAD=1|N` (#35, live): in the world, a decoration bake draw's texture pass
//! makes only the bake's own textures (`VICTORY_BAKE_INPUTS`) on the recording thread; the rest
//! of the due queue goes to the prepare threads (`VICTORY_TEXTURE_PREPARE_THREADS`), and the
//! pass's time is checked before each slot.
//!
//! Part I (b5i, all eight logs): 1-17 frames a run of 1.0-1.2 s (worst 2.07 s), `bake watch:
//! ... used_ms=1000-1150 budget_ms=500`. A bake pass (`PASS_BUDGET` with `now`) runs twice its
//! budget: `VICTORY_BAKE_PARALLEL` threads make host copies (and with `VICTORY_BAKE_STAGE_AHEAD`
//! stage) for every due slot, up to 500 ms, and the pass then takes the whole 500 ms again on the
//! recording thread (`VICTORY_BAKE_ONE_BUDGET` would halve that but was dropped in b5 part A:
//! it cut how much of the general queue a bake pass drains). It also checks its time only after
//! an upload or at every 8th look, and never hands a slot to the prepare threads, so each host
//! copy it makes (1-3 ms) is made there and then.
//!
//! With the switch, only for a bake pass in the world whose own textures were read
//! (`bake_pass::PINNED`) and with prepare threads:
//! - the `VICTORY_BAKE_PARALLEL` threads make the bake's own textures only (from 2 of them);
//! - the other due slots are handed to the prepare threads as a frame's plain pass does
//!   (made ready off the thread; a later pass records their uploads), so the queue still drains
//!   at the bake's pace, without the recording thread making them;
//! - the pass stops before the next slot once the budget is spent (the threads' time comes out
//!   of it; `N` >= 2 caps it at N ms), whatever it uploaded; the bake's own slots are still
//!   made first, outside the budget.
//!
//! One `bake spread frame` line per frame with such a pass.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::Duration;

/// Live: read at each bake pass.
pub(crate) static SPREAD: victory_core::settings::Live =
    victory_core::settings::Live::new("VICTORY_BAKE_SPREAD", parse);

fn parse(v: Option<&str>) -> u64 {
    v.and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(0)
}

/// 0 off, 1 on with the pass's own budget, N >= 2 on with the budget capped at N ms.
pub(crate) fn mode() -> u64 {
    SPREAD.get()
}

/// Whether a texture pass spreads: switch `mode`, a bake pass (`now`) whose own textures are
/// known (`inputs_known`), in the world, with prepare threads to hand the rest to.
pub(crate) fn spreads(
    mode: u64,
    now: bool,
    inputs_known: bool,
    world: bool,
    prepare_threads: usize,
) -> bool {
    mode > 0 && now && inputs_known && world && prepare_threads > 0
}

/// The pass's time for its slots after the bake threads took `took` of `budget` (capped at
/// `mode` ms from 2).
pub(crate) fn budget_left(mode: u64, budget: Duration, took: Duration) -> Duration {
    let budget = if mode >= 2 {
        budget.min(Duration::from_millis(mode))
    } else {
        budget
    };
    budget.saturating_sub(took)
}

/// Whether the pass stops before its `i`th slot: never for the first `pinned` (the bake's own),
/// else once the frame's bytes are spent or `elapsed` reached `budget`, checked before each slot.
pub(crate) fn stops_before(
    i: usize,
    pinned: usize,
    bytes_spent: bool,
    budget: Duration,
    elapsed: impl FnOnce() -> Duration,
) -> bool {
    i >= pinned && (bytes_spent || elapsed() >= budget)
}

#[derive(Clone, Copy)]
#[repr(usize)]
pub(crate) enum C {
    /// Bake passes spread.
    Passes,
    /// Their own slots made first.
    Pinned,
    /// Other slots handed to the prepare threads.
    Handed,
    /// Passes stopped by the budget with slots left.
    Stopped,
    /// Microseconds of the bake threads (own slots) and of the passes in all.
    AheadUs,
    Us,
}
const N: usize = 6;
const NAMES: [&str; N] = ["passes", "pinned", "handed", "stopped", "ahead_us", "us"];
static COUNTS: [AtomicU64; N] = [const { AtomicU64::new(0) }; N];
static FRAME: AtomicU64 = AtomicU64::new(0);
static LINES: AtomicU64 = AtomicU64::new(0);
const MAX_LINES: u64 = 20_000;

pub(crate) fn add(c: C, n: u64) {
    roll();
    COUNTS[c as usize].fetch_add(n, Relaxed);
}

/// The line for a frame with a spread pass, at the first count of the next.
fn roll() {
    let now = crate::frame::current();
    let prev = FRAME.load(Relaxed);
    if prev == now || FRAME.compare_exchange(prev, now, Relaxed, Relaxed).is_err() {
        return;
    }
    let c: [u64; N] = std::array::from_fn(|i| COUNTS[i].swap(0, Relaxed));
    if c[C::Passes as usize] == 0 || LINES.fetch_add(1, Relaxed) >= MAX_LINES {
        return;
    }
    tracing::info!(frame = prev, counts = %line(&c), "bake spread frame");
}

fn line(c: &[u64; N]) -> String {
    NAMES
        .iter()
        .zip(c)
        .map(|(name, n)| format!("{name}={n}"))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only a bake pass in the world, with its own textures known and prepare threads.
    #[test]
    fn only_a_bake_pass_in_the_world_spreads() {
        assert!(spreads(1, true, true, true, 4));
        assert!(spreads(300, true, true, true, 4));
        assert!(!spreads(0, true, true, true, 4), "off");
        assert!(!spreads(1, false, true, true, 4), "a plain pass");
        assert!(
            !spreads(1, true, false, true, 4),
            "own textures not read: all must be there now"
        );
        assert!(!spreads(1, true, true, false, 4), "loading");
        assert!(!spreads(1, true, true, true, 0), "no prepare threads");
    }

    /// Part I: `used_ms=1000-1150 budget_ms=500`. The threads' 500 ms came on top of the pass's
    /// 500; spread, the pass gets what the threads left, and no more than N ms with `N`.
    #[test]
    fn the_threads_time_comes_out_of_one_budget() {
        let ms = Duration::from_millis;
        assert_eq!(budget_left(1, ms(500), ms(0)), ms(500));
        assert_eq!(budget_left(1, ms(500), ms(120)), ms(380));
        assert_eq!(budget_left(1, ms(500), ms(900)), ms(0));
        assert_eq!(budget_left(50, ms(500), ms(10)), ms(40));
        assert_eq!(budget_left(800, ms(500), ms(0)), ms(500));
    }

    /// The old check (`bake_pass::stops_at`) let a pass go on past its budget until an upload or
    /// an 8th look: a pass that looked at 7 slots after its budget kept going. Spread, it stops
    /// before the next slot; the bake's own slots are never cut.
    #[test]
    fn the_budget_is_checked_before_each_slot() {
        let none = Duration::ZERO;
        let late = || Duration::from_secs(1);
        assert!(stops_before(5, 2, false, none, late));
        for i in 0..2 {
            assert!(!stops_before(i, 2, true, none, late), "own slot {i}");
        }
        assert!(
            stops_before(2, 2, true, Duration::from_secs(9), late),
            "bytes spent"
        );
        assert!(
            !stops_before(2, 2, false, Duration::from_secs(9), late),
            "in time"
        );
    }

    #[test]
    fn the_switch_is_live_and_takes_a_cap() {
        assert_eq!(parse(None), 0);
        assert_eq!(parse(Some("x")), 0);
        assert_eq!(parse(Some(" 1")), 1);
        assert_eq!(parse(Some("80")), 80);
        victory_core::settings::check_live("VICTORY_BAKE_SPREAD", "1", mode, 0, 1);
    }

    #[test]
    fn the_line_names_every_count() {
        assert_eq!(
            line(&[2, 7, 300, 1, 900, 41000]),
            "passes=2 pinned=7 handed=300 stopped=1 ahead_us=900 us=41000"
        );
    }
}
