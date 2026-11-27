//! Changed ranges that come in order and apart already, as they mostly do
//! (`VICTORY_JOINED_APART=1`, off by default): `shadow::joined` returns them
//! as they are after one look, without the pass that joins.
//!
//! Source: TaynanGT/Vice-Tay PR #286 (PetrSeifert, "send one pass, joined
//! fast path"), GPL-3.0. Of that PR only the `joined` half applies here: our
//! `joined` already skips its sort when the ranges are in order and joins
//! in place (fxceless/victory-fork #1431), and our send (`host.rs`
//! `masked_blocks_slices`) lays its blocks out in one pass already.
//!
//! With `VICTORY_RUN_SPLIT=1` the joins are timed for that line.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_JOINED_APART").is_some_and(|v| v == "1"))
}

/// Whether `ranges` are in order and none overlaps or touches the next:
/// what `joined` gives back for them is themselves.
pub(crate) fn apart(ranges: &[(usize, usize)]) -> bool {
    ranges.windows(2).all(|w| w[0] <= w[1] && w[0].1 < w[1].0)
}

/// Since the last line: joins, the ranges in them, those returned as they
/// were, and nanoseconds (timed only with `VICTORY_RUN_SPLIT=1`).
static STATS: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

/// Times one join of `n` ranges while it is held, if asked to.
pub(crate) struct Lap(Option<std::time::Instant>, u64);

pub(crate) fn lap(n: usize) -> Lap {
    Lap(timed().then(std::time::Instant::now), n as u64)
}

/// `VICTORY_RUN_SPLIT=1` (`host::run_split`, which prints the line).
fn timed() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_RUN_SPLIT").is_some_and(|v| v == "1"))
}

impl Lap {
    /// The ranges were returned as they were.
    pub(crate) fn as_they_were(&self) {
        if self.0.is_some() {
            STATS[2].fetch_add(1, Relaxed);
        }
    }
}

impl Drop for Lap {
    fn drop(&mut self) {
        if let Some(at) = self.0 {
            STATS[0].fetch_add(1, Relaxed);
            STATS[1].fetch_add(self.1, Relaxed);
            STATS[3].fetch_add(at.elapsed().as_nanos() as u64, Relaxed);
        }
    }
}

/// The run_other split's addition: (joins, ranges, as they were, ms), per frame.
pub(crate) fn taken(frames: f64) -> [f64; 4] {
    let [joins, ranges, kept, ns] = STATS.each_ref().map(|a| a.swap(0, Relaxed) as f64 / frames);
    [joins, ranges, kept, ns / 1e6]
}

#[cfg(test)]
mod tests {
    use super::apart;

    #[test]
    fn apart_only_in_order_and_with_a_gap() {
        assert!(apart(&[]));
        assert!(apart(&[(0, 4)]));
        assert!(apart(&[(0, 10), (20, 30), (40, 50)]));
        // Touching, overlapping, nested, out of order.
        assert!(!apart(&[(0, 10), (10, 20)]));
        assert!(!apart(&[(0, 10), (5, 8)]));
        assert!(!apart(&[(20, 30), (0, 10)]));
        // Upside-down ranges in an order sorting would change.
        assert!(!apart(&[(5, 3), (5, 2)]));
        assert!(!apart(&[(10, 5), (6, 20)]));
    }
}
