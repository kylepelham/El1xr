//! (bootfast) Pipelines queued for the workers when the title creates them,
//! not at their first draw (`VICTORY_PIPELINE_EAGER=N`; unset or 0: off, as
//! before).
//!
//! Ported from Belobug/VIctory-Belobug-AMD PR #622 (XboxCompatibility
//! T-0037, GPL-3.0-or-later). Without it a pipeline that is not on the early
//! list (`super::library::wanted_early`: an earlier run drew with it) is
//! queued only when a draw or dispatch first asks for it, and that draw
//! waits for it within the frame's budget or is dropped as "being made". A
//! loading screen that creates and uses many new pipelines a frame waited
//! for each in turn: in Belobug's synthetic loading guest (64 pipelines a
//! frame, WARP, 5 runs each) 24 of 24 loading frames took 70 ms or more,
//! and none did with the change (max ~140 ms down to ~45 ms).
//!
//! With the switch such a pipeline is queued at creation behind the early
//! list (`make::EAGER_RANK`), while fewer than N queued this way are not
//! yet taken by a worker. It uses the pipeline workers that are already
//! there (no thread of its own): requested pipelines are always taken
//! first, then the early list, then these. A pipeline is still made once
//! (`make::Pending`'s queue state and `OnceLock`), and a shader once (with
//! `VICTORY_TRANSLATE_AHEAD` a worker finds its translation made, or waits
//! for the one under way). The host pipeline is the same; only when it is
//! made changes, so it is not part of any cache key.
//!
//! The counters below are kept with or without the switch, so an arm with
//! it off reports what a first draw found too (`log`).

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// Up to how many pipelines queued at creation may wait for a worker
/// (`VICTORY_PIPELINE_EAGER`; 0, unset or not a number: off).
pub(super) fn limit() -> u64 {
    static LIMIT: OnceLock<u64> = OnceLock::new();
    *LIMIT.get_or_init(|| {
        let limit = parse_limit(std::env::var("VICTORY_PIPELINE_EAGER").ok().as_deref());
        if limit > 0 {
            tracing::info!(
                limit,
                "new pipelines are queued when the title creates them while fewer wait (VICTORY_PIPELINE_EAGER; Belobug #622)"
            );
        }
        limit
    })
}

fn parse_limit(value: Option<&str>) -> u64 {
    value.and_then(|v| v.trim().parse().ok()).unwrap_or(0)
}

/// How a pipeline came to be queued, as its first draw or dispatch found it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Origin {
    /// Queued at creation by this switch.
    Eager,
    /// Queued at creation from the early list (or as a scene input).
    Early,
    /// Not queued before its first draw.
    AtDraw,
}

/// Counters, by index: queued eagerly; first uses (eager made, eager still
/// being made, early made, early still being made, first queued at the
/// draw); draws that found their pipeline not made, and of those the ones
/// that still had none after waiting (dropped as "being made").
static COUNTS: [AtomicU64; 8] = [const { AtomicU64::new(0) }; 8];
const QUEUED: usize = 0;
const EAGER_READY: usize = 1;
const EAGER_LATE: usize = 2;
const EARLY_READY: usize = 3;
const EARLY_LATE: usize = 4;
const AT_DRAW: usize = 5;
const NOT_READY: usize = 6;
const DROPPED: usize = 7;

/// A pipeline was queued at creation by the switch.
pub(super) fn queued() {
    COUNTS[QUEUED].fetch_add(1, Relaxed);
}

/// A pipeline's first draw or dispatch: how it was queued, and whether it
/// was made by then.
pub(super) fn first_use(origin: Origin, made: bool) {
    let i = match (origin, made) {
        (Origin::Eager, true) => EAGER_READY,
        (Origin::Eager, false) => EAGER_LATE,
        (Origin::Early, true) => EARLY_READY,
        (Origin::Early, false) => EARLY_LATE,
        (Origin::AtDraw, _) => AT_DRAW,
    };
    COUNTS[i].fetch_add(1, Relaxed);
}

/// A draw or dispatch found its pipeline not made, waited within the
/// frame's budget, and then had it (`got`) or not.
pub(super) fn waited(got: bool) {
    COUNTS[NOT_READY].fetch_add(1, Relaxed);
    if !got {
        COUNTS[DROPPED].fetch_add(1, Relaxed);
    }
}

/// The line the operator greps: `pipelines eager vs at first draw`.
pub(super) fn log() {
    let c = |i: usize| COUNTS[i].load(Relaxed);
    tracing::info!(
        limit = limit(),
        eager_queued = c(QUEUED),
        eager_used_ready = c(EAGER_READY),
        eager_used_late = c(EAGER_LATE),
        early_used_ready = c(EARLY_READY),
        early_used_late = c(EARLY_LATE),
        at_first_draw = c(AT_DRAW),
        draws_not_ready = c(NOT_READY),
        draws_dropped = c(DROPPED),
        "pipelines eager vs at first draw (VICTORY_PIPELINE_EAGER)"
    );
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_switch_is_off_unless_set_to_a_number() {
        use super::parse_limit;
        assert_eq!(parse_limit(None), 0);
        assert_eq!(parse_limit(Some("0")), 0);
        assert_eq!(parse_limit(Some("on")), 0);
        assert_eq!(parse_limit(Some(" 256")), 256);
    }
}
