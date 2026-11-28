//! (feedcard, #88) `VICTORY_FLUSH_PASS_MERGE=1` (Live, off by default): a copy's long refresh runs
//! in the same pass of a wide flush as the copy's other steps around it (those before it, and those
//! after it up to its next long refresh) when none of those steps touches a page of the long one.
//!
//! Why: without it a copy whose steps are `pre, L, post` (`L` a refresh of 2 MiB or more, cut into
//! pieces over the threads) takes three passes, `pre`, then `L`'s pieces, then `post`, each ending
//! at a barrier that waits for the slowest thread; a flush takes as many passes as its slowest copy
//! needs (part Q: about 51 passes a frame, wall about 2.5 times busy). The steps of `pre` and `post`
//! are mostly a few small refreshes and the snapshots of what a dispatch writes, elsewhere in the
//! copy than the long refresh.
//!
//! Same output: steps of one copy on different 64 KiB pages do not see each other (a refresh reads
//! the title's bytes of its own pages into the copy and skips those a snapshot holds; a snapshot
//! keeps the copy's bytes of its own pages; guards, watches and stamps are per page), so `L` commutes
//! with each of them and `pre` then `post` stay in order in one task. Steps of different copies
//! already run beside each other in any pass. A copy with a step on a page of `L` is split as before.
//! Not in the cache key.

use super::ShadowStep;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

static ON: victory_core::settings::Live =
    victory_core::settings::Live::flag("VICTORY_FLUSH_PASS_MERGE");

#[cfg(test)]
thread_local! {
    pub(crate) static FORCED: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

pub(super) fn on() -> bool {
    #[cfg(test)]
    if let Some(forced) = FORCED.with(std::cell::Cell::get) {
        return forced;
    }
    ON.get_bool()
}

/// Since the last line: copies whose long refresh ran beside their other steps, and the passes of
/// a copy that saved (one for steps before it, one for steps after it).
static STATS: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

/// Whether every step of `steps` lies on other `page`s than `[from, to)`.
pub(super) fn apart(steps: &[ShadowStep], from: usize, to: usize, page: usize) -> bool {
    let (lo, hi) = (from & !(page - 1), to.div_ceil(page).saturating_mul(page));
    steps.iter().all(|s| {
        let (a, b) = match *s {
            ShadowStep::Refresh(f, t) => (f, t),
            ShadowStep::Write(at, len) => (at, at.saturating_add(len)),
            ShadowStep::Barrier => return true,
        };
        let (a, b) = (
            a & !(page - 1),
            b.max(a + 1).div_ceil(page).saturating_mul(page),
        );
        b <= lo || a >= hi
    })
}

/// The copy's steps around its long refresh `steps[at]` that may run beside it, joined in order
/// (those before it, then those after it up to `end`, the next long refresh or the end), or `None`
/// when one of them touches its pages.
pub(super) fn beside(
    steps: &[ShadowStep],
    at: usize,
    end: usize,
    page: usize,
) -> Option<Vec<ShadowStep>> {
    let ShadowStep::Refresh(from, to) = steps[at] else {
        return None;
    };
    let (before, after) = (&steps[..at], &steps[at + 1..end]);
    if !apart(before, from, to, page) || !apart(after, from, to, page) {
        return None;
    }
    let saved = u64::from(!before.is_empty()) + u64::from(!after.is_empty());
    if saved > 0 {
        STATS[0].fetch_add(1, Relaxed);
        STATS[1].fetch_add(saved, Relaxed);
    }
    Some(before.iter().chain(after).copied().collect())
}

pub(super) fn line(frames: f64) {
    let [merged, saved] = [0, 1].map(|i| STATS[i].swap(0, Relaxed) as f64 / frames);
    if on() {
        tracing::info!(
            "wide flush long refreshes run beside their copy's other steps per frame (VICTORY_FLUSH_PASS_MERGE=1): \
             {merged:.1} copies, {saved:.1} passes of a copy saved"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_on_other_pages_are_apart() {
        let p = 0x10000;
        let r = |a, b| ShadowStep::Refresh(a, b);
        assert!(apart(
            &[r(0, p), ShadowStep::Barrier, ShadowStep::Write(5 * p, 16)],
            p,
            5 * p,
            p
        ));
        assert!(
            !apart(&[r(0, p + 1)], p, 5 * p, p),
            "a byte on its first page"
        );
        assert!(
            !apart(&[ShadowStep::Write(5 * p - 1, 1)], p + 7, 5 * p - 9, p),
            "pages, not bytes"
        );
        assert!(
            !apart(&[ShadowStep::Write(3 * p, 0)], p, 5 * p, p),
            "an empty write counts its page"
        );
        assert!(
            apart(&[r(9 * p, 10 * p)], p, 5 * p + 1, p)
                && !apart(&[r(5 * p, 6 * p)], p, 5 * p + 1, p)
        );
    }

    #[test]
    fn the_switch_is_live() {
        if std::env::var_os("VICTORY_FLUSH_PASS_MERGE").is_some() {
            return;
        }
        assert!(!ON.get_bool(), "off by default");
        victory_core::settings::set("VICTORY_FLUSH_PASS_MERGE", "1").unwrap();
        let now = ON.get_bool();
        victory_core::settings::set("VICTORY_FLUSH_PASS_MERGE", "").unwrap();
        assert!(now, "a change takes effect while running");
    }
}
