//! (feedcard, #88) `VICTORY_STALE_RUN_LEAN=1` (Live, off by default): the per-run work of the stale
//! copy's sub-page path (`VICTORY_WATCH_4K` / `VICTORY_DIRTY_4K`, both in the launcher set).
//!
//! What a run cost (part Q: 7.62 ms of thread time a frame for 27.0 MB in 1045 runs; planning under
//! the copy's lock 1.83 ms, noting 0.26 ms, the rest in `Shadow::copy` run by run):
//! - The plan cuts every stale 64 KiB page's 4 KiB mask into runs of its own, so a stretch written
//!   across a page edge is two runs, or 16 for 16 whole masked pages. Each run is one more
//!   `Shadow::copy`: a compare that starts cold, and a note with its own locks.
//! - A run starts on memory last read a frame ago, both on the title's side and on the copy's: the
//!   core's prefetchers start again at each run and at each 4 KiB page. Measured from the code
//!   (`bench_lean_runs`, cold memory, runs of a few 4 KiB pages scattered over 1 GiB): about
//!   2.5 us a run of 26 KiB on one thread, a tenth to a quarter of which is waiting for its first lines.
//! - Every stale page counted itself into two shared counters (`watch::note_4k`) under the copy's
//!   lock, so 17 threads moved those lines between them while others waited for the lock.
//!
//! With the switch:
//! - Runs that touch are joined when the joint lies a whole number of 32-byte lanes from the joined
//!   run's start, and the run stays at most 16 MiB (the compare's limit in `Shadow::copy`). The
//!   same bytes are read and compared, the same lanes differ and go up. With `VICTORY_COPY_COALESCE`
//!   the ranges sent may also span the joint when their changes are near it. Those bytes are the
//!   same on both sides, as for any coalesced range.
//! - While a run is copied, the next run's first 2 KiB are asked for on both sides (a prefetch
//!   never faults, also on a page that is not there or is guarded).
//! - The plan's page counts are kept on the stack and added once a plan.
//!
//! Same copy, same plan state. Not in the cache key.

use super::*;

static ON: victory_core::settings::Live =
    victory_core::settings::Live::flag("VICTORY_STALE_RUN_LEAN");

#[cfg(test)]
thread_local! {
    pub(crate) static FORCED: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
    /// Overrides `VICTORY_COPY_CHANGED` for this test thread.
    pub(crate) static COPY_CHANGED_IN_TEST: std::cell::Cell<Option<bool>> =
        const { std::cell::Cell::new(None) };
}

pub(crate) fn on() -> bool {
    #[cfg(test)]
    if let Some(forced) = FORCED.with(std::cell::Cell::get) {
        return forced;
    }
    ON.get_bool()
}

/// The most bytes of a joined run: past this `Shadow::copy` copies without the compare.
const MOST: usize = 16 << 20;
/// The compare's lane: a joint this many bytes from the joined run's start keeps the lanes.
const LANE: usize = 32;
/// Bytes of the next run asked for ahead.
const AHEAD: usize = 2048;

/// Since the last line: plans, runs planned, runs after joining.
static STATS: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

/// `runs` (in address order, as the plan gives them) with those that touch joined, as the
/// module says.
pub(crate) fn join(mut runs: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
    let planned = runs.len();
    let mut out = 0;
    for i in 0..runs.len() {
        let (a, b) = runs[i];
        if out > 0 {
            let last = &mut runs[out - 1];
            if last.1 == a && (a - last.0) % LANE == 0 && b - last.0 <= MOST {
                last.1 = b;
                continue;
            }
        }
        runs[out] = (a, b);
        out += 1;
    }
    runs.truncate(out);
    STATS[0].fetch_add(1, Ordering::Relaxed);
    STATS[1].fetch_add(planned as u64, Ordering::Relaxed);
    STATS[2].fetch_add(out as u64, Ordering::Relaxed);
    runs
}

/// Asks for the first [`AHEAD`] bytes at `src` and, when given, at `dst`.
#[inline]
pub(crate) fn prefetch(src: usize, dst: Option<*const u8>) {
    #[cfg(target_arch = "x86_64")]
    {
        use core::arch::x86_64::{_MM_HINT_T0, _mm_prefetch};
        for i in (0..AHEAD).step_by(64) {
            // SAFETY: a prefetch is a hint: it never faults, whatever the address.
            unsafe {
                _mm_prefetch::<_MM_HINT_T0>(src.wrapping_add(i) as *const i8);
                if let Some(d) = dst {
                    _mm_prefetch::<_MM_HINT_T0>(d.wrapping_add(i).cast::<i8>());
                }
            }
        }
    }
}

/// [`crate::watch::note_4k`]'s counts of a plan, kept here and added once.
#[derive(Default)]
pub(crate) struct Tally([u64; 4]);

impl Tally {
    pub(crate) fn page(&mut self, mask: Option<u16>) {
        let (i, n) = match mask {
            Some(m) => (0, u64::from(m.count_ones())),
            None => (2, 16),
        };
        self.0[i] += n * 4096;
        self.0[i + 1] += 1;
    }

    pub(crate) fn add(self) {
        crate::watch::note_4k_add(self.0);
    }
}

impl Shadow {
    /// [`Shadow::copy_runs_here`] of joined runs, each with the next one's first bytes asked for.
    pub(super) fn copy_runs_lean(&self, runs: &[(usize, usize)]) -> usize {
        let batch = crate::stale_batch::begin(self as *const Self as usize);
        let start = self.start as usize;
        let ahead_dst = !self.data.is_null() && !self.rebar && !self.transient();
        let mut bytes = 0;
        for (i, &(a, b)) in runs.iter().enumerate() {
            if let Some(&(next, _)) = runs.get(i + 1) {
                let dst = (ahead_dst && next >= start)
                    .then(|| self.data.wrapping_add(next - start).cast_const());
                prefetch(next, dst);
            }
            bytes += self.copy(a, b);
        }
        crate::refresh_prof::add(crate::refresh_prof::P::StaleRuns, runs.len() as u64);
        if let Some(notes) = batch.end() {
            self.take_notes(notes);
        }
        bytes
    }
}

pub(crate) fn line(frames: f64) {
    let [plans, planned, joined] =
        [0, 1, 2].map(|i| STATS[i].swap(0, Ordering::Relaxed) as f64 / frames);
    if on() {
        tracing::info!(
            "stale copy runs per frame (VICTORY_STALE_RUN_LEAN=1): {plans:.0} plans, {planned:.0} runs planned, {joined:.0} copied after joining those that touch"
        );
    }
}

#[cfg(test)]
#[path = "shadow_run_lean_tests.rs"]
mod tests;
