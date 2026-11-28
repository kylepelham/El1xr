//! (entryhitch, CPU round, #16) `VICTORY_STALE_QUICK=1` (Live, off by
//! default): `stale_under_flights` asks each copy on the submitting thread
//! whether its merge under writes can do anything ([`Shadow::merge_due`])
//! and hands only those to the kept threads (none: no thread is woken; one:
//! the caller merges it, as `wide_run` does with one job).
//!
//! What `stale_flights` waits for: never the card (b5 part H, 0.0 settles for
//! "title memory changed under shader writes" a frame); it waits for the
//! kept threads to finish the merges. 28 calls a frame merged 131 copies,
//! 0.38-0.45 ms of the threads' own work, for 0.78 ms of the submitting
//! thread's wall time; 42 pages a frame were looked at in all, so most
//! copies have nothing new and their job only costs the wake and the join.
//!
//! [`Shadow::merge_due`] is the head of [`Shadow::merge_under_writes`] up to
//! its first read of title memory: it says no only where the merge would
//! return `false` having changed nothing but the refresh stamp, which it
//! clears as the merge does. Not in the cache key: no shader or pipeline
//! output changes.

use std::sync::atomic::{AtomicU64, Ordering};

use victory_core::settings::Live;

use super::{LAZY_RESYNC, NOT_WATCHED, PAGE, Shadow, joined};

static QUICK: Live = Live::flag("VICTORY_STALE_QUICK");

pub(crate) fn on() -> bool {
    QUICK.get_bool()
}

/// Since the last frame line: copies asked, and of them those left out.
pub(crate) static ASKED: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

/// Keeps the copies of `spans` whose merge is due; counts both.
pub(crate) fn keep_due<T: AsRef<Shadow>>(spans: &mut Vec<(T, usize, usize)>) {
    let before = spans.len();
    spans.retain(|(shadow, from, to)| shadow.as_ref().merge_due(*from, *to - *from));
    ASKED[0].fetch_add(before as u64, Ordering::Relaxed);
    ASKED[1].fetch_add((before - spans.len()) as u64, Ordering::Relaxed);
}

/// The counts since the last call: (asked, left out).
pub(crate) fn take() -> (u64, u64) {
    (
        ASKED[0].swap(0, Ordering::Relaxed),
        ASKED[1].swap(0, Ordering::Relaxed),
    )
}

impl Shadow {
    /// Whether [`Shadow::merge_under_writes`] of `[address, address + len)`
    /// may read title memory or ask for a settle. `false` only where the
    /// merge would return `false` and change nothing but the refresh stamp,
    /// which this clears too: no write pending under the range, or memory
    /// write-watched whose pages under pending writes hold nothing the
    /// journal has not seen the copy take.
    pub(crate) fn merge_due(&self, address: usize, len: usize) -> bool {
        if !self.has_pending.load(Ordering::Acquire) {
            return false;
        }
        self.stamp.clear();
        let (start, end) = (self.start as usize, (self.start + self.len) as usize);
        let (from, to) = (address.max(start), (address + len).min(end));
        if from >= to {
            return false;
        }
        let busy: Vec<(usize, usize)> = self
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .overlapping(from, to)
            .map(|(a, e)| (a.max(from), e.min(to)))
            .collect();
        if busy.is_empty() {
            return false;
        }
        let watched = self.watched.load(Ordering::Relaxed);
        if watched == NOT_WATCHED {
            return true;
        }
        let busy = if busy.len() > 1 { joined(busy) } else { busy };
        let base = start / PAGE;
        if self.resync_count.load(Ordering::Acquire) > 0 {
            for &(a, e) in &busy {
                for page in a / PAGE..e.div_ceil(PAGE) {
                    if page >= base
                        && (page - base) < self.lazy_state.len()
                        && self.lazy_state[page - base].load(Ordering::Acquire) == LAZY_RESYNC
                    {
                        return true;
                    }
                }
            }
        }
        if watched != 1 {
            return true;
        }
        let copied = self.copied.look();
        !busy
            .iter()
            .all(|&(a, e)| crate::watch::nothing_new(a, e, base, &copied.at))
    }
}
