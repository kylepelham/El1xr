//! Where `Host::card_before` and `stale_under_flights` spend their time
//! (`VICTORY_CARD_PROF=1`, or with `VICTORY_FLUSH_PROF=1`; off by default):
//! the two are `card_before` and `stale_flights` on the preparing breakdown
//! line, and that line gives no split. Summed since the last frame line
//! and said per frame on a line of its own. Off, it costs one cached load
//! per submission. `VICTORY_CARD_PROF` is live: a change takes effect at
//! the next submission (sums from before it are dropped at the next line).
//!
//! Also the joining of a submission's declared shader writes in one sort
//! per copy ([`decl_join`], `VICTORY_CARD_DECL_JOIN=1`, live: read once
//! per `card_before`).

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use victory_core::settings::Live;

fn is_one(v: Option<&str>) -> u64 {
    u64::from(v == Some("1"))
}

static PROF: Live = Live::new("VICTORY_CARD_PROF", is_one);
static DECL_JOIN: Live = Live::new("VICTORY_CARD_DECL_JOIN", is_one);

pub(crate) fn on() -> bool {
    static FLUSH_PROF: OnceLock<bool> = OnceLock::new();
    PROF.get_bool()
        || *FLUSH_PROF.get_or_init(|| std::env::var_os("VICTORY_FLUSH_PROF").is_some_and(|v| v == "1"))
}

/// The parts of `card_before`, in the order it runs them, then counts.
#[derive(Clone, Copy)]
pub(crate) enum P {
    /// Gathering the ranges the submission's shaders may write, per copy.
    Declared,
    /// Waiting for the order of the queue (`QUEUE_ORDER`, with the relay).
    Order,
    /// Taking what changed on the copies' CPU sides (`shadow::card_dirty`).
    Dirty,
    /// Waiting for the card work (`CARD_WORK`).
    Work,
    /// Bytes from before for copies declared the first time (`card_make_before`).
    Before,
    /// The masks of small changes (`under_writes_masks`).
    Masks,
    /// Recording the list that sends changes up and hashes them again.
    Sync,
    /// The declared ranges joined and placed in the compare's results.
    Spans,
    /// Recording the compare.
    Check,
    /// Calls that found something to do (past the first return).
    Plans,
    /// Declared ranges as gathered (after each list's own dedup).
    DeclIn,
    /// Declared ranges once joined (the compare's ranges).
    DeclOut,
    /// Copies with changes to send up.
    DirtyCopies,
    /// Their changed ranges.
    DirtyRanges,
    /// Blocks sent up by mask.
    MaskBlocks,
    /// `stale_under_flights`: the walk of the lists.
    StaleWalk,
    /// `stale_under_flights`: the merges under writes (`late_exact`).
    StaleMerge,
    /// Calls of `stale_under_flights`.
    StaleCalls,
    /// Copies merged under writes.
    StaleCopies,
}

const COUNT: usize = P::StaleCopies as usize + 1;
const PARTS: usize = P::Check as usize + 1;

static STATS: [AtomicU64; COUNT] = [const { AtomicU64::new(0) }; COUNT];

#[inline]
pub(crate) fn add(what: P, n: u64) {
    if on() {
        STATS[what as usize].fetch_add(n, Ordering::Relaxed);
    }
}

/// Time since the last mark goes to a part.
pub(crate) struct Lap(Option<Instant>);

impl Lap {
    #[inline]
    pub(crate) fn new() -> Self {
        Self(on().then(Instant::now))
    }

    #[inline]
    pub(crate) fn mark(&mut self, what: P) {
        if let Some(t) = self.0.as_mut() {
            let now = Instant::now();
            STATS[what as usize].fetch_add((now - *t).as_micros() as u64, Ordering::Relaxed);
            *t = now;
        }
    }
}

pub(crate) fn log(frames: f64) {
    if !on() {
        for a in &STATS {
            a.store(0, Ordering::Relaxed);
        }
        return;
    }
    if frames <= 0.0 {
        return;
    }
    let s: Vec<u64> = STATS.iter().map(|a| a.swap(0, Ordering::Relaxed)).collect();
    let ms = |p: P| s[p as usize] as f64 / (frames * 1000.0);
    let per = |p: P| s[p as usize] as f64 / frames;
    let parts: f64 = (0..PARTS).map(|i| s[i] as f64).sum::<f64>() / (frames * 1000.0);
    tracing::info!(
        "card_before split per frame (VICTORY_CARD_PROF, VICTORY_CARD_DECL_JOIN={}): {:.2} ms timed = declared {:.2}, \
         order wait {:.2}, card_dirty {:.2}, work wait {:.2}, make_before {:.2}, masks {:.2}, sync record {:.2}, \
         spans {:.2}, check record {:.2} ms; {:.1} plans, declared ranges {:.0} in / {:.0} joined, \
         {:.1} copies sent up with {:.0} ranges, {:.0} mask blocks",
        u8::from(decl_join()),
        parts,
        ms(P::Declared),
        ms(P::Order),
        ms(P::Dirty),
        ms(P::Work),
        ms(P::Before),
        ms(P::Masks),
        ms(P::Sync),
        ms(P::Spans),
        ms(P::Check),
        per(P::Plans),
        per(P::DeclIn),
        per(P::DeclOut),
        per(P::DirtyCopies),
        per(P::DirtyRanges),
        per(P::MaskBlocks),
    );
    tracing::info!(
        "stale_flights split per frame (VICTORY_CARD_PROF): walk {:.2} ms, merges under writes {:.2} ms; \
         {:.1} calls, {:.1} copies merged",
        ms(P::StaleWalk),
        ms(P::StaleMerge),
        per(P::StaleCalls),
        per(P::StaleCopies),
    );
}

/// `VICTORY_CARD_DECL_JOIN=1` (off by default): `card_before` gathers the
/// ranges a submission's shaders may write as they come, per copy, and
/// sorts and joins each copy's ranges once ([`DeclJoin`]). Today each range
/// of a list after the first is looked for among those gathered so far
/// (one pass over them each), then the whole list is copied, sorted and
/// joined again for the compare. The compare's ranges come out the same.
pub(crate) fn decl_join() -> bool {
    DECL_JOIN.get_bool()
}

/// Per copy (by key, in the order first seen), its ranges sorted and
/// joined where they overlap or touch.
pub(crate) struct DeclJoin<T> {
    index: std::collections::HashMap<usize, usize>,
    out: Vec<(T, Vec<(usize, usize)>)>,
}

impl<T> DeclJoin<T> {
    pub(crate) fn new() -> Self {
        Self {
            index: std::collections::HashMap::new(),
            out: Vec::new(),
        }
    }

    pub(crate) fn add(&mut self, key: usize, copy: impl FnOnce() -> T, ranges: &[(usize, usize)]) {
        match self.index.get(&key) {
            Some(&i) => self.out[i].1.extend_from_slice(ranges),
            None => {
                self.index.insert(key, self.out.len());
                self.out.push((copy(), ranges.to_vec()));
            }
        }
    }

    pub(crate) fn finish(mut self) -> Vec<(T, Vec<(usize, usize)>)> {
        for (_, ranges) in &mut self.out {
            *ranges = joined_spans(std::mem::take(ranges));
        }
        self.out
    }
}

/// `ranges` in order, those that overlap or touch as one: the spans
/// `card_before` gives the compare.
pub(crate) fn joined_spans(mut ranges: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
    ranges.sort_unstable();
    let mut spans: Vec<(usize, usize)> = Vec::with_capacity(ranges.len());
    for (from, to) in ranges {
        match spans.last_mut() {
            Some(last) if from <= last.1 => last.1 = last.1.max(to),
            _ => spans.push((from, to)),
        }
    }
    spans
}

#[cfg(test)]
#[path = "card_prof_tests.rs"]
mod tests;
