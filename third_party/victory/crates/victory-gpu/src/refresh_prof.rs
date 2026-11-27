//! Opt-in (`VICTORY_FLUSH_PROF=1`) account of where host copy refreshes
//! (`Shadow::update_free`, `watch::writes`, `watch::prepare_flush`) spend
//! their time, summed over every thread since the last frame line. Off, it
//! costs one cached load per call.

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
    PublishedCalls,
    KernelCalls,
    KernelUs,
    GuardCalls,
    GuardUs,
    PrepareViewsCalls,
    PrepareViewsUs,
    PreparePrivCalls,
    PreparePrivUs,
    StaleCopyUs,
    StaleBytes,
    // (feedcard, #88) the stale copy and the guards, split.
    StalePlanUs,
    StaleRuns,
    StaleNoteUs,
    GuardWalks,
    ProtectCalls,
    ProtectUs,
    OpenCalls,
    OpenUs,
    // (feedcard, #88) the 4 KiB guards' closing calls, the plan's wait for the copy's lock.
    Protect4kCalls,
    Protect4kUs,
    StalePlanWaitUs,
}

const COUNT: usize = P::StalePlanWaitUs as usize + 1;

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
         flush prepass [views {:.0}/{:.2} ms, private {:.0}/{:.2} ms]; \
         clean {:.0} in {:.2} ms; ask {:.0} in {:.2} ms \
         [published {:.0}, kernel {:.0}/{:.2} ms, guard {:.0}/{:.2} ms]; \
         stale copy {:.2} ms ({:.1} MB)",
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
        per(P::PublishedCalls),
        per(P::KernelCalls),
        ms(P::KernelUs),
        per(P::GuardCalls),
        ms(P::GuardUs),
        ms(P::StaleCopyUs),
        mb(P::StaleBytes),
    );
    tracing::info!(
        "stale copy and guards split per frame (VICTORY_FLUSH_PROF, VICTORY_STALE_BATCH={}): \
         stale copy: plan under the copy's lock {:.2} ms (of it waiting for the lock {:.2} ms, \
         VICTORY_PLAN_SPLIT={}), {:.0} runs copied one by one, \
         noting their changes {:.2} ms (the rest of the stale copy is the bytes); \
         guards: {:.0} walks under the guards' lock, VirtualProtect read-only {:.0} in {:.2} ms, \
         opened {:.0} in {:.2} ms (all threads, faults' openings too); \
         4 KiB guards closed read-only in {:.0} calls, {:.2} ms (VICTORY_GUARD_4K_JOIN={})",
        u8::from(crate::stale_batch::on()),
        ms(P::StalePlanUs),
        ms(P::StalePlanWaitUs),
        u8::from(crate::shadow::plan_split::on()),
        per(P::StaleRuns),
        ms(P::StaleNoteUs),
        per(P::GuardWalks),
        per(P::ProtectCalls),
        ms(P::ProtectUs),
        per(P::OpenCalls),
        ms(P::OpenUs),
        per(P::Protect4kCalls),
        ms(P::Protect4kUs),
        u8::from(crate::watch::guard_4k_join()),
    );
}
