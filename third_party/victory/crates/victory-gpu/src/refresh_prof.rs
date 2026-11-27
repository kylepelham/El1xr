//! Opt-in (`VICTORY_FLUSH_PROF=1`) breakdown of where `Batch::flush`,
//! `Shadow::update_free`, `Shadow::copy`, and `watch::writes` spend their time
//! across all worker threads. Zero overhead when the env var is unset.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_FLUSH_PROF").is_some_and(|v| v == "1"))
}

#[derive(Clone, Copy)]
pub(crate) enum P {
    Calls,
    Span,
    CleanCalls,
    CleanUs,
    AskCalls,
    AskUs,
    StandingCalls,
    StandingUs,
    PublishedCalls,
    PublishedUs,
    KernelCalls,
    KernelUs,
    GuardCalls,
    GuardUs,
    PrepareViewsCalls,
    PrepareViewsUs,
    PreparePrivCalls,
    PreparePrivUs,
    StalePlanUs,
    StaleCopyUs,
    StaleBytes,
    CopyTransientUs,
    CopyTransientBytes,
    CopyRebarUs,
    CopyRebarBytes,
    CopyDiffUs,
    CopyDiffBytes,
    CopyDiffFaults,
    CopyDirtyLockUs,
    CopyPlainUs,
    CopyPlainBytes,
    WholeCalls,
    WholeBytes,
    WholeUs,
    LazyUs,
    BusyCalls,
    BusyPieces,
}

const COUNT: usize = P::BusyPieces as usize + 1;

static STATS: [AtomicU64; COUNT] = [const { AtomicU64::new(0) }; COUNT];

#[inline]
pub(crate) fn add(what: P, n: u64) {
    if on() {
        STATS[what as usize].fetch_add(n, Ordering::Relaxed);
    }
}

#[inline]
pub(crate) fn start() -> Option<std::time::Instant> {
    on().then(std::time::Instant::now)
}

#[inline]
pub(crate) fn finish(what: P, started: Option<std::time::Instant>) {
    if let Some(t) = started {
        STATS[what as usize].fetch_add(t.elapsed().as_micros() as u64, Ordering::Relaxed);
    }
}

pub(crate) fn log(frames: f64) {
    if !on() || frames <= 0.0 {
        return;
    }
    let s: Vec<u64> = STATS.iter().map(|a| a.swap(0, Ordering::Relaxed)).collect();
    let ms = |p: P| s[p as usize] as f64 / (frames * 1000.0);
    let per = |p: P| s[p as usize] as f64 / frames;
    let mb = |p: P| s[p as usize] as f64 / (frames * (1 << 20) as f64);
    tracing::info!(
        "host copy refreshes per frame (all threads): {:.0} calls over {:.0} MB; \
         prepare_flush [views {:.0}/{:.2} ms, priv {:.0}/{:.2} ms]; \
         clean {:.0} in {:.2} ms; ask {:.0} in {:.2} ms \
         [standing {:.0}/{:.2} ms, published {:.0}/{:.2} ms, kernel {:.0}/{:.2} ms, guard {:.0}/{:.2} ms]; \
         stale plan {:.2} ms, copy {:.2} ms ({:.1} MB) \
         [transient {:.1} MB/{:.2} ms, rebar {:.1} MB/{:.2} ms, diff {:.1} MB/{:.2} ms (faults {:.0}, dirty_lock {:.2} ms), plain {:.1} MB/{:.2} ms]; \
         whole {:.0}/{:.1} MB/{:.2} ms; lazy {:.2} ms; busy {:.0}->{:.0} pieces",
        per(P::Calls),
        mb(P::Span),
        per(P::PrepareViewsCalls),
        ms(P::PrepareViewsUs),
        per(P::PreparePrivCalls),
        ms(P::PreparePrivUs),
        per(P::CleanCalls),
        ms(P::CleanUs),
        per(P::AskCalls),
        ms(P::AskUs),
        per(P::StandingCalls),
        ms(P::StandingUs),
        per(P::PublishedCalls),
        ms(P::PublishedUs),
        per(P::KernelCalls),
        ms(P::KernelUs),
        per(P::GuardCalls),
        ms(P::GuardUs),
        ms(P::StalePlanUs),
        ms(P::StaleCopyUs),
        mb(P::StaleBytes),
        mb(P::CopyTransientBytes),
        ms(P::CopyTransientUs),
        mb(P::CopyRebarBytes),
        ms(P::CopyRebarUs),
        mb(P::CopyDiffBytes),
        ms(P::CopyDiffUs),
        per(P::CopyDiffFaults),
        ms(P::CopyDirtyLockUs),
        mb(P::CopyPlainBytes),
        ms(P::CopyPlainUs),
        per(P::WholeCalls),
        mb(P::WholeBytes),
        ms(P::WholeUs),
        ms(P::LazyUs),
        per(P::BusyCalls),
        per(P::BusyPieces),
    );
}
