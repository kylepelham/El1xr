//! (trustno1 `2ad55e6`) `VICTORY_QUEUE_MARKS=1`: notes that would change
//! nothing leave the lock of the queue of textures picked by index alone.
//!
//! The queue marks, per title slot, the write number of the entry its
//! `slots` hold (0: none; [`QueueMarks`]), set by the queue itself under its
//! lock whether the switch is on or not. With the switch, three places read
//! the mark (or the slot's `SLOT_IN_TEXTURES` flag) first and skip the lock
//! when taking it would change nothing:
//! - the first table read of a slot in a frame, which takes the slot out of
//!   `slots`: skipped when the mark says it is not there;
//! - a slot's record made (`seen`, not written) as the queue holds it:
//!   skipped when the mark says it is there (the entry stays as it is);
//! - a record or a stale placed view that takes a slot out of the queue:
//!   skipped when the slot's flag says the queue holds nothing of it. With
//!   the switch, a slot leaving `slots` at its first table read of a frame
//!   or as done, with nothing else of the queue's left (a look again, a live
//!   mark), clears that flag, which otherwise stayed set until the slot was
//!   written again. (`VICTORY_TABLED_REREAD`'s later reads are as before.)
//!
//! A mark read while the queue changes it says what the queue held just
//! before or just after, as if the lock had been taken a moment earlier or
//! later, which any of these threads may always do. Writes always take the
//! lock. Upstream measured 33-44 s of other threads' waits for this lock in a
//! 150 s run. Unset: the lock is taken every time, as before.

use std::sync::atomic::{AtomicU64, Ordering};

use victory_core::settings::Live;

/// Whether the marks spare the queue's lock (`VICTORY_QUEUE_MARKS=1`).
pub(crate) fn on() -> bool {
    static ON: Live = Live::flag("VICTORY_QUEUE_MARKS");
    #[cfg(test)]
    if let Some(on) = FORCE.with(std::cell::Cell::get) {
        return on;
    }
    ON.get_bool()
}

#[cfg(test)]
thread_local! {
    /// For the heap's tests: the switch as this thread sees it, without
    /// touching the live setting other tests read.
    pub(super) static FORCE: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

/// Which title slots the queue's `slots` hold: for each the write number of
/// its entry (0: none). Changed only by the queue, under its lock.
#[derive(Default)]
pub(crate) struct QueueMarks(Box<[AtomicU64]>);

impl QueueMarks {
    pub(super) fn new(count: usize) -> Self {
        QueueMarks((0..count).map(|_| AtomicU64::new(0)).collect())
    }

    /// Under the queue's lock: slot `index` holds the entry of write `write`
    /// (0: none).
    pub(super) fn set(&self, index: u32, write: u64) {
        if let Some(mark) = self.0.get(index as usize) {
            mark.store(write, Ordering::Release);
        }
    }

    /// The write of slot `index`'s entry (0: none); `None`: past the marks,
    /// ask the queue.
    pub(super) fn get(&self, index: u32) -> Option<u64> {
        Some(self.0.get(index as usize)?.load(Ordering::Acquire))
    }
}

/// Where the lock was not taken: a slot's record noted, a table read, stale
/// views.
pub(super) const WRITE: usize = 0;
pub(super) const TABLE: usize = 1;
pub(super) const STALE: usize = 2;

/// Lock takes skipped since the last line, by place; counted by each thread
/// in batches (hundreds of thousands in five seconds, on many threads).
static NOT_TAKEN: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

pub(super) fn not_taken(site: usize) {
    const BATCH: u32 = 64;
    thread_local! {
        static N: std::cell::Cell<[u32; 3]> = const { std::cell::Cell::new([0; 3]) };
    }
    N.with(|n| {
        let mut counts = n.get();
        counts[site] += 1;
        if counts[site] >= BATCH {
            NOT_TAKEN[site].fetch_add(u64::from(counts[site]), Ordering::Relaxed);
            counts[site] = 0;
        }
        n.set(counts);
    });
}

/// Logs [`NOT_TAKEN`] on the frame-meter cadence while the switch is on, and
/// starts it over.
pub(crate) fn log(frames: f64) {
    let n: [u64; 3] = std::array::from_fn(|i| NOT_TAKEN[i].swap(0, Ordering::Relaxed));
    if !on() || frames <= 0.0 {
        return;
    }
    let per = |n: u64| n as f64 / frames;
    tracing::info!(
        "texture queue lock not taken, as nothing would have changed, per frame (VICTORY_QUEUE_MARKS=1): \
         {:.0} noting a slot's record, {:.0} by table reads, {:.0} for stale views",
        per(n[WRITE]),
        per(n[TABLE]),
        per(n[STALE]),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_switch_is_live() {
        victory_core::settings::check_live("VICTORY_QUEUE_MARKS", "1", on, false, true);
    }

    #[test]
    fn marks_hold_a_write_per_slot_and_know_their_reach() {
        let marks = QueueMarks::new(4);
        assert_eq!(marks.get(2), Some(0));
        marks.set(2, 7);
        assert_eq!(marks.get(2), Some(7));
        marks.set(2, 0);
        assert_eq!(marks.get(2), Some(0));
        marks.set(9, 1);
        assert_eq!(marks.get(9), None);
        assert_eq!(QueueMarks::default().get(0), None);
    }
}
