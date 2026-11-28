//! (feedcard, #88) `VICTORY_HEAP_PASS_SPLIT=1|N` (Live, off by default): the
//! heap-buffers pass (`draw::State::heap_buffers`) looks at its slots on N
//! threads (1: 8) before it takes them in, one after the other, as before.
//!
//! The pass's safety net forgets everything every
//! [`crate::heap_pass::FULL_EVERY`] passes and looks again at every buffer
//! view of the heap: ~90,000 slots on the pass's one title thread, 100-155 ms
//! every 256 frames (the settled `heap buffers` hitches of batch 5 parts T/U
//! fall exactly 256 frames apart: f466, f722, f1234, f1490, f1746, f2002).
//! Scene cuts bring 6-15k slots at once the same way. Nearly every one of
//! those slots shows its view already, so its look makes nothing: it reads
//! the record, looks the view up, and finds the host copy the view reads
//! (`descriptor::host::buffer_copy`: two or three table lookups), ~1 us a
//! slot. Those lookups take only read sides and make a copy at most as
//! `buffer_ahead` does beside the pass, so they run here on helper threads
//! ([`pre_look`]). The pass then goes over its slots in order as before
//! ([`run`]): a slot looked at beside it that still shows the same view (its
//! record and `made_too` key unchanged) is taken in with what was found; any
//! other slot, and every slot of a pass under [`LEAST`], goes through the
//! old path on the pass's thread (`mirror_heap_buffer`, which makes views
//! and lets stale ones go, in order). So the kept slots, orphans and spans
//! are those the serial pass gives (tests below), looked at a moment earlier.
//!
//! In both settings the pass names its parts when it is long ([`line`]).

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

/// Passes over fewer slots are looked at on the pass's thread: the threads
/// cost more than they save.
pub(crate) const LEAST: usize = 1024;
/// Slots a helper takes at a time.
const CHUNK: usize = 128;
/// Threads at `=1`.
const DEFAULT_THREADS: usize = 8;
/// The most threads asked for that are used.
const MOST_THREADS: usize = 32;
/// A pass's buffer part at least this long is named in [`line`].
const LINE_US: u64 = 20_000;

static SPLIT: victory_core::settings::Live =
    victory_core::settings::Live::new("VICTORY_HEAP_PASS_SPLIT", |v| {
        v.and_then(|v| v.trim().parse().ok()).unwrap_or(0)
    });

static NET_IN_PLACE: victory_core::settings::Live =
    victory_core::settings::Live::flag("VICTORY_HEAP_NET_IN_PLACE");

/// Whether the incremental pass's looks at every slot keep what they know
/// (`VICTORY_HEAP_NET_IN_PLACE=1`, `heap_pass::Picked::set_net_in_place`):
/// the 90,000 slots found as they were are updated in place, not taken in
/// anew into emptied tables (the microbenchmark's 50-55 -> 10-12 ms).
pub(crate) fn net_in_place() -> bool {
    NET_IN_PLACE.get_bool()
}

/// Threads (the caller among them) a pass over `slots` slots is looked at
/// on; 1: on the pass's thread alone.
pub(crate) fn threads_for(slots: usize) -> usize {
    threads(SPLIT.get(), slots)
}

fn threads(setting: u64, slots: usize) -> usize {
    if slots < LEAST {
        return 1;
    }
    match setting {
        0 => 1,
        1 => DEFAULT_THREADS,
        n => (n as usize).min(MOST_THREADS),
    }
}

/// What the pass does with a slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Look<C> {
    /// No view of ours (or no handle, or an empty range): let go.
    Forget,
    /// A buffer view whose memory has no host copy yet.
    Orphan,
    /// A buffer view reading `[from, to)` through host copy `copy`; `made`:
    /// the look made the view and resolved the copy with it.
    Keep {
        copy: C,
        from: u64,
        to: u64,
        made: bool,
    },
}

/// A slot looked at beside the pass: the key it showed (`made_too`) and
/// what was found; `None`: left to the pass's thread.
pub(crate) type Pre<C> = Option<(u64, Look<C>)>;

/// Looks at `slots` on `threads` threads (this one among them) with
/// `look`, which only reads; the results in the order of `slots`.
pub(crate) fn pre_look<C: Send>(
    slots: &[u32],
    threads: usize,
    look: impl Fn(u32) -> Pre<C> + Sync,
) -> Vec<Pre<C>> {
    let n = slots.len();
    let next = AtomicUsize::new(0);
    let work = || {
        let mut mine: Vec<(usize, Vec<Pre<C>>)> = Vec::new();
        loop {
            let piece = next.fetch_add(1, Ordering::Relaxed);
            let from = piece * CHUNK;
            if from >= n {
                break;
            }
            let to = (from + CHUNK).min(n);
            mine.push((piece, slots[from..to].iter().map(|&s| look(s)).collect()));
        }
        mine
    };
    let mut pieces = std::thread::scope(|s| {
        let helpers: Vec<_> = (1..threads.min(n.div_ceil(CHUNK)))
            .filter_map(|_| {
                std::thread::Builder::new()
                    .name("victory-heap-look".into())
                    .spawn_scoped(s, &work)
                    .ok()
            })
            .collect();
        let mut pieces = work();
        for helper in helpers {
            match helper.join() {
                Ok(theirs) => pieces.extend(theirs),
                Err(panic) => std::panic::resume_unwind(panic),
            }
        }
        pieces
    });
    pieces.sort_unstable_by_key(|(piece, _)| *piece);
    pieces.into_iter().flat_map(|(_, looks)| looks).collect()
}

/// Goes over `slots` in order, as the serial pass does: a slot `pre` looked
/// at that `still(slot, key)` says shows the same view is taken in with
/// what was found, any other through `slow` (the old path), and each
/// handed to `apply(n, slot, look)`. `pre` empty: all through `slow`.
/// Returns how many were taken from `pre` and how many through `slow`.
pub(crate) fn run<C>(
    slots: &[u32],
    pre: Vec<Pre<C>>,
    still: impl Fn(u32, u64) -> bool,
    mut slow: impl FnMut(u32) -> Look<C>,
    mut apply: impl FnMut(usize, u32, Look<C>),
) -> [usize; 2] {
    let mut pre = pre.into_iter();
    let mut counts = [0; 2];
    for (n, &slot) in slots.iter().enumerate() {
        let look = match pre.next().flatten() {
            Some((key, look)) if still(slot, key) => {
                counts[0] += 1;
                look
            }
            _ => {
                counts[1] += 1;
                slow(slot)
            }
        };
        apply(n, slot, look);
    }
    counts
}

/// Microseconds of a pass's parts.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct Times {
    /// Which slots (`heap_pass::Picked::plan_in_ranges`).
    pub(crate) plan: u64,
    /// Host copies made ahead (`buffer_ahead`).
    pub(crate) ahead: u64,
    /// Slots looked at beside the pass ([`pre_look`]).
    pub(crate) look: u64,
    /// Slots taken in, in order ([`run`]).
    pub(crate) keep: u64,
    /// Each copy's span asked to be brought up to date.
    pub(crate) spans: u64,
}

/// One long pass's line: a pass whose buffer part took [`LINE_US`] or more,
/// or that looked at every slot; at most 4 a second.
#[allow(clippy::too_many_arguments)]
pub(crate) fn line(
    slots: usize,
    all: bool,
    threads: usize,
    counts: [usize; 2],
    copies: usize,
    t: Times,
) {
    let buffers = t.plan + t.ahead + t.look + t.keep + t.spans;
    if buffers < LINE_US && !all {
        return;
    }
    static LAST_MS: AtomicU64 = AtomicU64::new(0);
    static SKIPPED: AtomicU64 = AtomicU64::new(0);
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    // (From 1 s, so that the first long pass is named.)
    let now = START
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_millis() as u64
        + 1000;
    let last = LAST_MS.load(Ordering::Relaxed);
    if now < last + 250
        || LAST_MS
            .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
    {
        SKIPPED.fetch_add(1, Ordering::Relaxed);
        return;
    }
    let ms = |us: u64| us as f64 / 1000.0;
    tracing::info!(
        "heap buffers pass: {slots} slots{} in {:.1} ms: plan {:.1}, copies made ahead {:.1}, looked at beside {:.1} \
         on {threads} threads, taken in {:.1} ({} as looked at, {} one by one), spans {:.1} over {copies} copies \
         (VICTORY_HEAP_PASS_SPLIT; {} long passes not named)",
        match (all, net_in_place()) {
            (false, _) => "",
            (true, false) => " (all: the safety net)",
            (true, true) => " (all: the safety net, in place)",
        },
        ms(buffers),
        ms(t.plan),
        ms(t.ahead),
        ms(t.look),
        ms(t.keep),
        counts[0],
        counts[1],
        ms(t.spans),
        SKIPPED.swap(0, Ordering::Relaxed),
    );
}

#[cfg(test)]
#[path = "heap_pass_split_tests.rs"]
mod tests;
