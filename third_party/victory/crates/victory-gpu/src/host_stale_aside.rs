//! (critpath, #88) `VICTORY_STALE_ASIDE=1` (Live, off by default; acts with
//! `VICTORY_ASK_ASIDE=1`): behind flights, the check of whether the title wrote memory a
//! flight's shaders write (`stale_under_flights`: the walk of the lists, then the merges of the
//! title's bytes under the pending writes) runs on `victory-ask-aside` right after the asking,
//! not on the submitting thread at the gate.
//!
//! Part S/T: stale_flights 1.0-2.5 ms a frame on the submitting thread (28 calls, 115-175 copies
//! merged, walk 0.05-0.12 ms, merges 1.0-1.9 ms), and it found nothing to settle in any frame:
//! the time is the merges' own work, which reads title memory and the journal the asking fills,
//! so it cannot start before the asking; it can run behind it, beside the walk. The walk's
//! inputs (per copy with writes pending, the span of what the lists refresh; the ReBAR
//! refreshes; the declared writes) are gathered on the submitting thread before the hand-off
//! ([`gather`]); the checks and merges run on the thread ([`Check::run`]). Nothing in the walk
//! before the gate reads title memory, the journal or a copy's bytes (`host_ask_aside.rs`), and
//! only the submitting thread declares new pending writes, after the walk: what is gathered
//! covers what the check would have seen at the gate.
//!
//! At the gate: if the check found something, the flights are settled and the asking made
//! again there, as before. A check that did not run (the asking panicked) is made at the gate.
//! The gate's wait is split: the part the check took on its thread, at most, is stale_flights;
//! the rest ask_once. Not in the cache key: no shader or pipeline output.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};

use super::HostList;
use crate::shadow::Shadow;

static ON: victory_core::settings::Live = victory_core::settings::Live::flag("VICTORY_STALE_ASIDE");

pub(super) fn on() -> bool {
    ON.get_bool()
}

/// What the check found, written by the thread that ran it.
#[derive(Default)]
pub(super) struct Out {
    done: AtomicBool,
    stale: AtomicBool,
    us: AtomicU64,
}

impl Out {
    /// After the join, with the microseconds waited for it: `Some((stale, us of the wait that
    /// were the check's))`, or `None` if the check did not run.
    pub(super) fn result(&self, waited_us: u64) -> Option<(bool, u64)> {
        if !self.done.load(Relaxed) {
            return None;
        }
        let us = self.us.load(Relaxed);
        COUNTS[THREAD_US].fetch_add(us, Relaxed);
        let on_check = waited_us.min(us);
        COUNTS[WAITED_US].fetch_add(on_check, Relaxed);
        Some((self.stale.load(Relaxed), on_check))
    }

    fn finish(&self, stale: bool, us: u64) {
        self.stale.store(stale, Relaxed);
        self.us.store(us, Relaxed);
        self.done.store(true, Relaxed);
    }
}

type Span = (Arc<Shadow>, usize, usize);

/// The inputs of `stale_under_flights` for a submission's lists, owned.
pub(super) struct Check {
    rebar: Vec<Span>,
    writes: Vec<(Arc<Shadow>, Vec<(usize, usize)>)>,
    spans: Vec<Span>,
    out: Arc<Out>,
}

/// The check's inputs for `runs`: `None` without the lists' step index (the old walk is used).
pub(super) fn gather(runs: &[&HostList]) -> Option<Check> {
    if !super::step_index() {
        return None;
    }
    let mut check = Check {
        rebar: Vec::new(),
        writes: Vec::new(),
        spans: Vec::new(),
        out: Arc::default(),
    };
    let mut index: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();
    for list in runs {
        let deferred = list.deferred();
        check.rebar.extend(deferred.rebar_refreshes.iter().cloned());
        for (shadow, ranges) in &deferred.gpu_writes {
            if shadow.writes_pending() {
                check.writes.push((shadow.clone(), ranges.clone()));
            }
        }
        for (shadow, from, to) in &deferred.refresh_spans {
            if shadow.writes_pending() {
                join_span(&mut check.spans, &mut index, shadow, *from, *to);
            }
        }
    }
    COUNTS[GATHERED].fetch_add(1, Relaxed);
    Some(check)
}

/// Per copy one span: `[from, to)` joined into the copy's.
fn join_span<T>(
    spans: &mut Vec<(Arc<T>, usize, usize)>,
    index: &mut std::collections::HashMap<usize, usize>,
    shadow: &Arc<T>,
    from: usize,
    to: usize,
) {
    let key = Arc::as_ptr(shadow) as *const () as usize;
    if let Some(&i) = index.get(&key) {
        let s = &mut spans[i];
        (s.1, s.2) = (s.1.min(from), s.2.max(to));
    } else {
        index.insert(key, spans.len());
        spans.push((shadow.clone(), from, to));
    }
}

impl Check {
    pub(super) fn out(&self) -> Arc<Out> {
        self.out.clone()
    }

    /// The check, as `stale_under_flights` makes it (on the thread of the asking, after it).
    pub(super) fn run(self) {
        use crate::card_prof::P;
        let started = std::time::Instant::now();
        let mut lap = crate::card_prof::Lap::new();
        crate::card_prof::add(P::StaleCalls, 1);
        let early = self
            .rebar
            .iter()
            .any(|(shadow, address, len)| shadow.rebar_stale(*address, *len))
            || self.writes.iter().any(|(shadow, ranges)| {
                shadow.writes_pending()
                    && ranges
                        .iter()
                        .any(|&(a, e)| shadow.has_lazy_resync_under_writes(a, e - a))
            });
        let stale = if early {
            true
        } else {
            let spans: Vec<Span> = self
                .spans
                .into_iter()
                .filter(|(shadow, ..)| shadow.writes_pending())
                .collect();
            lap.mark(P::StaleWalk);
            super::late::stale_spans(spans, &mut lap)
        };
        self.out.finish(stale, started.elapsed().as_micros() as u64);
    }
}

const GATHERED: usize = 0;
const THREAD_US: usize = 1;
const WAITED_US: usize = 2;
const AT_GATE: usize = 3;
static COUNTS: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

/// A check that did not run on the thread was made at the gate.
pub(super) fn at_gate() {
    COUNTS[AT_GATE].fetch_add(1, Relaxed);
}

/// For the preparing breakdown line, per frame over `frames`: (checks handed aside, ms on the
/// thread, ms of the gate's wait that were the check's, checks made at the gate instead).
pub(super) fn take(frames: f64) -> [f64; 4] {
    let [g, t, w, a] = COUNTS.each_ref().map(|c| c.swap(0, Relaxed) as f64);
    if frames <= 0.0 {
        return [0.0; 4];
    }
    [
        g / frames,
        t / 1000.0 / frames,
        w / 1000.0 / frames,
        a / frames,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_gate_wait_is_the_checks_up_to_its_time_on_the_thread() {
        let out = Out::default();
        assert_eq!(out.result(500), None, "not run: made at the gate");
        out.finish(false, 300);
        assert_eq!(
            out.result(500),
            Some((false, 300)),
            "waited longer than the check took"
        );
        assert_eq!(
            out.result(120),
            Some((false, 120)),
            "the check was mostly hidden"
        );
        let out = Out::default();
        out.finish(true, 40);
        assert_eq!(out.result(0), Some((true, 0)));
    }

    #[test]
    fn spans_join_per_copy() {
        let (a, b) = (Arc::new(1u8), Arc::new(2u8));
        let mut spans = Vec::new();
        let mut index = std::collections::HashMap::new();
        join_span(&mut spans, &mut index, &a, 0x10_1000, 0x10_2000);
        join_span(&mut spans, &mut index, &b, 0x20_0000, 0x20_1000);
        join_span(&mut spans, &mut index, &a, 0x10_0000, 0x10_1800);
        let got: Vec<(usize, usize)> = spans.iter().map(|s| (s.1, s.2)).collect();
        assert_eq!(got, [(0x10_0000, 0x10_2000), (0x20_0000, 0x20_1000)]);
        assert!(Arc::ptr_eq(&spans[0].0, &a) && Arc::ptr_eq(&spans[1].0, &b));
    }

    /// Lists with no copy under pending writes: nothing gathered, the check says no and is done.
    #[test]
    fn a_check_with_nothing_pending_says_no() {
        let Some(h) = crate::host::host() else { return };
        let list = h.new_list().unwrap();
        let Some(check) = gather(&[&list]) else {
            return;
        };
        assert!(check.spans.is_empty() && check.writes.is_empty());
        let out = check.out();
        std::thread::spawn(move || check.run()).join().unwrap();
        assert_eq!(out.result(0).map(|r| r.0), Some(false));
        list.close();
    }
}
