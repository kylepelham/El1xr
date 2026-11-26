//! Where a host copy's refresh ([`crate::shadow::Shadow::update`]) spends its
//! time, summed over every thread since the last frame line: the account
//! behind the submission's `batch_flush`.

use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Clone, Copy)]
pub(crate) enum P {
    /// Calls of `update_free`, and the bytes they covered.
    Calls,
    Span,
    /// Calls that found nothing new without asking (`nothing_new`).
    CleanCalls,
    CleanUs,
    /// The write watch or the journal asked (`watch::writes`).
    AskUs,
    AskCalls,
    /// Copying what the asking found (`copy_stale`).
    StaleUs,
    StaleBytes,
    /// Memory not watched: copied whole.
    WholeUs,
    WholeBytes,
    WholeCalls,
    /// Lazy pages of this copy or another one.
    LazyUs,
    /// Calls cut around ranges shaders are writing, and the pieces.
    BusyCalls,
    BusyPieces,
}

const COUNT: usize = P::BusyPieces as usize + 1;

static STATS: [AtomicU64; COUNT] = [const { AtomicU64::new(0) }; COUNT];

pub(crate) fn add(what: P, n: u64) {
    STATS[what as usize].fetch_add(n, Ordering::Relaxed);
}

/// Microseconds since `started`, added to `what`.
pub(crate) fn time(what: P, started: std::time::Instant) {
    add(what, started.elapsed().as_micros() as u64);
}

/// The account's line, per frame.
pub(crate) fn log(frames: f64) {
    let s: Vec<u64> = STATS.iter().map(|a| a.swap(0, Ordering::Relaxed)).collect();
    let ms = |p: P| s[p as usize] as f64 / (frames * 1000.0);
    let per = |p: P| s[p as usize] as f64 / frames;
    let mb = |p: P| s[p as usize] as f64 / (frames * (1 << 20) as f64);
    tracing::info!(
        "host copy refreshes per frame, all threads: {:.0} calls over {:.0} MB; {:.0} found nothing new \
         without asking in {:.2} ms; {:.0} asked the write watch in {:.2} ms, then copied {:.1} MB in {:.2} ms; \
         {:.0} copied whole {:.1} MB in {:.2} ms; lazy pages {:.2} ms; {:.0} cut around shader writes into \
         {:.0} pieces",
        per(P::Calls),
        mb(P::Span),
        per(P::CleanCalls),
        ms(P::CleanUs),
        per(P::AskCalls),
        ms(P::AskUs),
        mb(P::StaleBytes),
        ms(P::StaleUs),
        per(P::WholeCalls),
        mb(P::WholeBytes),
        ms(P::WholeUs),
        ms(P::LazyUs),
        per(P::BusyCalls),
        per(P::BusyPieces),
    );
}
