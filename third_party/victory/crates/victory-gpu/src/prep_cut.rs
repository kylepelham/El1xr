//! (entryhitch, CPU round, #16) Two cuts on the submitting thread, both
//! Live and off by default, and a finer split of `card_before`,
//! `stale_flights` and `exec_submit` with `VICTORY_CARD_PROF=1`.
//!
//! - `VICTORY_STALE_WIDE_MIN=<n>`: `stale_under_flights` sends its merges
//!   under writes to the kept threads only when at least `n` copies are to
//!   be merged (unset or below 2: 2, as today). In the base run a call merges
//!   about 4 copies of about one page each, 27 calls a frame, 2-4 ms a frame:
//!   each call wakes threads and waits for the last one. Below `n` the caller
//!   merges them itself, each copy with a budget of runs of its own and every
//!   copy merged, as the threads do: the same merges.
//! - `VICTORY_CARD_CHECK_ASIDE=1`: `card_before` records the compare's list
//!   on a kept thread while it records the sync list itself (the two lists
//!   are separate allocators and lists; neither reads what the other
//!   writes). The compare's ranges are placed before the sync list is
//!   recorded instead of after; nothing the sync list reads or writes is
//!   touched by that. Same commands in both lists.
//! - `VICTORY_SYNC_MASKS_LEAST=<n>` (with `VICTORY_UNDER_WRITES_MASKS=1`):
//!   the sync list sends a 4 KiB block's changed ranges up by one masked
//!   pass when `n` or more of them lie in it (1: every range under 8 KiB),
//!   instead of one `CopyBufferRegion` each; unset or 0: as
//!   `VICTORY_UNDER_WRITES_MASKS_LEAST` (4). The same bytes go up either way
//!   and no byte between them is written (`host_tests::mask_tests`). The
//!   sync record was 1.7-1.8 ms a frame for ~6,000 ranges.
//!
//! Not in the cache key: no shader or pipeline output changes.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use victory_core::settings::Live;

fn count(v: Option<&str>) -> u64 {
    v.and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(0)
}

static STALE_WIDE_MIN: Live = Live::new("VICTORY_STALE_WIDE_MIN", count);
static CHECK_ASIDE: Live = Live::flag("VICTORY_CARD_CHECK_ASIDE");
static SYNC_MASKS_LEAST: Live = Live::new("VICTORY_SYNC_MASKS_LEAST", count);

/// The fewest copies a stale merge sends to the kept threads.
pub(crate) fn stale_wide_min() -> usize {
    (STALE_WIDE_MIN.get() as usize).max(2)
}

pub(crate) fn check_aside() -> bool {
    CHECK_ASIDE.get_bool()
}

/// `VICTORY_SYNC_MASKS_LEAST`, if set above 0.
pub(crate) fn masks_least() -> Option<usize> {
    let n = SYNC_MASKS_LEAST.get() as usize;
    (n > 0).then_some(n)
}

/// The finer parts, summed since the last frame line (microseconds, or
/// counts).
#[derive(Clone, Copy)]
pub(crate) enum F {
    /// Sync list: allocator and list reset.
    SyncOpen,
    /// Sync list: close.
    SyncClose,
    /// Compare list: allocator and list reset.
    CheckOpen,
    /// Compare list: close.
    CheckClose,
    /// Merges under writes, each copy's own time, summed over the threads.
    StaleWork,
    /// Copies merged under writes.
    StaleCopies,
    /// Of them, merged on the caller because the call had fewer than
    /// `VICTORY_STALE_WIDE_MIN`.
    StaleSerial,
    /// Compare lists recorded beside a sync list.
    Aside,
    /// `ExecuteCommandLists` of a submission sent ahead.
    ExecLists,
    /// The rest of `exec_submit`: signals, the card clock, the writes noted.
    ExecRest,
    /// `CopyBufferRegion` calls recorded into sync lists.
    SyncCopies,
    /// Fills of upload memory cut by `VICTORY_WIDE_COPY`.
    WideFills,
    /// The pieces they became.
    WidePieces,
    /// Title copies cut by `VICTORY_WIDE_COPY`.
    WideSteps,
}

const N: usize = F::WideSteps as usize + 1;
static STATS: [AtomicU64; N] = [const { AtomicU64::new(0) }; N];

#[inline]
pub(crate) fn add(what: F, n: u64) {
    if crate::card_prof::on() {
        STATS[what as usize].fetch_add(n, Ordering::Relaxed);
    }
}

/// A start time, only while the split is on.
#[inline]
pub(crate) fn now() -> Option<Instant> {
    crate::card_prof::on().then(Instant::now)
}

/// The time since `from` goes to `what`; `from` moves to now.
#[inline]
pub(crate) fn lap(what: F, from: &mut Option<Instant>) {
    if let Some(t) = from.as_mut() {
        let at = Instant::now();
        STATS[what as usize].fetch_add((at - *t).as_micros() as u64, Ordering::Relaxed);
        *t = at;
    }
}

/// `from` moves to now, its time going nowhere.
#[inline]
pub(crate) fn restart(from: &mut Option<Instant>) {
    if let Some(t) = from.as_mut() {
        *t = Instant::now();
    }
}

pub(crate) fn log(frames: f64) {
    let s: Vec<u64> = STATS.iter().map(|a| a.swap(0, Ordering::Relaxed)).collect();
    if !crate::card_prof::on() || frames <= 0.0 {
        return;
    }
    let ms = |f: F| s[f as usize] as f64 / (frames * 1000.0);
    let per = |f: F| s[f as usize] as f64 / frames;
    tracing::info!(
        "card_before, stale_flights and exec_submit finer per frame (VICTORY_CARD_PROF, VICTORY_STALE_WIDE_MIN={}, \
         VICTORY_CARD_CHECK_ASIDE={}, VICTORY_SYNC_MASKS_LEAST={}, VICTORY_WIDE_COPY={}, VICTORY_RESULT_RING={}): sync list reset {:.2} ms, \
         close {:.2} ms, {:.0} CopyBufferRegion; compare list reset {:.2} ms, close {:.2} ms; \
         {:.1} compare lists recorded aside; merges under writes {:.2} ms of their own work summed over the threads \
         for {:.1} copies, {:.1} of them on the caller below the min; exec_submit ExecuteCommandLists {:.2} ms, rest {:.2} ms; \
         wide copies: {:.1} fills cut into {:.1} pieces, {:.2} title copies cut",
        stale_wide_min(),
        u8::from(check_aside()),
        masks_least().unwrap_or(0),
        u8::from(crate::wide_copy::on()),
        u8::from(crate::result_ring::on()),
        ms(F::SyncOpen),
        ms(F::SyncClose),
        per(F::SyncCopies),
        ms(F::CheckOpen),
        ms(F::CheckClose),
        per(F::Aside),
        ms(F::StaleWork),
        per(F::StaleCopies),
        per(F::StaleSerial),
        ms(F::ExecLists),
        ms(F::ExecRest),
        per(F::WideFills),
        per(F::WidePieces),
        per(F::WideSteps),
    );
}

struct Erased(*mut (dyn FnMut() + 'static));

// SAFETY: [`both`] hands the pointer to one kept thread and returns only
// after that thread ran it; the closures it is given call free-threaded
// D3D12 methods on objects no other thread uses meanwhile.
unsafe impl Send for Erased {}

/// Runs `a` and `b` side by side, one on a kept thread (`crate::shadow::wide_run`)
/// and one on this thread, or both here if no thread takes one; returns
/// when both have run.
pub(crate) fn both(a: &mut dyn FnMut(), b: &mut dyn FnMut()) {
    fn erase(f: &mut dyn FnMut()) -> Erased {
        let f: *mut (dyn FnMut() + '_) = f;
        // SAFETY: only the lifetime is erased; `wide_run` returns after
        // every job ran, before the borrow ends.
        Erased(unsafe {
            std::mem::transmute::<*mut (dyn FnMut() + '_), *mut (dyn FnMut() + 'static)>(f)
        })
    }
    let jobs: Vec<Box<dyn FnOnce() + Send>> = [erase(a), erase(b)]
        .into_iter()
        .map(|f| {
            Box::new(move || {
                let f = f;
                // SAFETY: see `erase`; each closure runs once, on one thread.
                unsafe { (*f.0)() }
            }) as Box<dyn FnOnce() + Send>
        })
        .collect();
    crate::shadow::wide_run(jobs);
}

/// [`both`], for two closures that give something back.
pub(crate) fn both_out<A, B>(a: impl FnOnce() -> A, b: impl FnOnce() -> B) -> (A, B) {
    let (mut a, mut b) = (Some(a), Some(b));
    let (mut ra, mut rb) = (None, None);
    both(
        &mut || ra = a.take().map(|f| f()),
        &mut || rb = b.take().map(|f| f()),
    );
    match (ra, rb) {
        (Some(ra), Some(rb)) => (ra, rb),
        _ => unreachable!("`both` returns after running both"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_min_is_two_unless_more_is_asked() {
        assert_eq!(count(None), 0);
        assert_eq!(count(Some(" 8 ")), 8);
        assert_eq!(count(Some("x")), 0);
        assert_eq!((count(Some("1")) as usize).max(2), 2);
    }

    #[test]
    fn both_run_once_each_and_their_borrows_hold_what_they_wrote() {
        for _ in 0..200 {
            let mut left: Vec<u32> = Vec::new();
            let mut right = 0u64;
            let mut a = || left.extend(0..1000);
            let mut b = || right = (0..1000u64).sum();
            both(&mut a, &mut b);
            assert_eq!(left.len(), 1000);
            assert_eq!(right, 499_500);
            let (x, y) = both_out(|| left.len() * 2, || right + 1);
            assert_eq!((x, y), (2000, 499_501));
        }
    }
}
