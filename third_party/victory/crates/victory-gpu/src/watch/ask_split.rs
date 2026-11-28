//! Where a submission's one write-watch asking (`ask_once` in the preparing
//! breakdown) spends its time: gathering the lists' spans, the filter before
//! the journal's lock, the wait for that lock, the reservation queries, the
//! kernel's write-watch askings and noting what they found. Counted always
//! (a few clock reads per submission and per kernel call), one line on the
//! frame-meter cadence beside the reservation line.

use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// Since the last line.
pub(crate) const SUBMISSIONS: usize = 0;
pub(crate) const SPANS: usize = 1;
pub(crate) const RANGES: usize = 2;
pub(crate) const GATHER_US: usize = 3;
pub(crate) const END_US: usize = 4;
pub(crate) const ASK_US: usize = 5;
pub(crate) const BUSY: usize = 6;
pub(crate) const FILTER_US: usize = 7;
pub(crate) const LOCK_US: usize = 8;
pub(crate) const REGIONS: usize = 9;
pub(crate) const REGION_US: usize = 10;
pub(crate) const CALLS: usize = 11;
pub(crate) const KERNEL_US: usize = 12;
pub(crate) const WALKED: usize = 13;
pub(crate) const FOUND: usize = 14;
pub(crate) const NOTE_US: usize = 15;
pub(crate) const PIECES: usize = 16;
const COUNT: usize = 17;

static STATS: [AtomicU64; COUNT] = [const { AtomicU64::new(0) }; COUNT];

thread_local! {
    /// Set while this thread runs the submission's asking, so the kernel
    /// askings of other paths (a refresh asking for itself) are not counted.
    static ASKING: Cell<bool> = const { Cell::new(false) };
}

pub(crate) fn add(which: usize, n: u64) {
    STATS[which].fetch_add(n, Relaxed);
}

pub(crate) fn add_us(which: usize, since: std::time::Instant) {
    add(which, since.elapsed().as_micros() as u64);
}

/// Whether this thread is in the submission's asking.
pub(crate) fn asking() -> bool {
    ASKING.with(Cell::get)
}

/// Marks this thread as in the submission's asking until the value drops.
pub(crate) struct Asking(bool);

impl Asking {
    pub(crate) fn begin() -> Self {
        Self(ASKING.with(|a| a.replace(true)))
    }
}

impl Drop for Asking {
    fn drop(&mut self) {
        ASKING.with(|a| a.set(self.0));
    }
}

/// Takes the counts since the last call, per frame.
pub(crate) fn take(frames: f64) -> [f64; COUNT] {
    STATS.each_ref().map(|n| n.swap(0, Relaxed) as f64 / frames)
}

pub(crate) fn line(frames: f64) {
    if frames <= 0.0 {
        return;
    }
    let s = take(frames);
    if s[SUBMISSIONS] == 0.0 {
        return;
    }
    let ms = |i: usize| s[i] / 1000.0;
    tracing::info!(
        "write-watch asking per frame (ask_once): {:.1} submissions, {:.0} spans gathered into {:.0} ranges in {:.2} ms; \
         ending the last asking {:.2} ms; asking {:.2} ms: {:.0} pieces, {:.1} skipped while the runtime remapped, \
         filter {:.2} ms, journal lock wait {:.2} ms, {:.0} reservation queries {:.2} ms, \
         {:.0} kernel askings {:.2} ms over {:.0} pages of 4 KiB ({:.0} found written), noting them {:.2} ms",
        s[SUBMISSIONS],
        s[SPANS],
        s[RANGES],
        ms(GATHER_US),
        ms(END_US),
        ms(ASK_US),
        s[PIECES],
        s[BUSY],
        ms(FILTER_US),
        ms(LOCK_US),
        s[REGIONS],
        ms(REGION_US),
        s[CALLS],
        ms(KERNEL_US),
        s[WALKED],
        s[FOUND],
        ms(NOTE_US),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kernel_askings_count_only_inside_the_asking() {
        assert!(!asking());
        {
            let _a = Asking::begin();
            assert!(asking());
            {
                let _b = Asking::begin();
                assert!(asking());
            }
            assert!(asking());
        }
        assert!(!asking());
    }
}
