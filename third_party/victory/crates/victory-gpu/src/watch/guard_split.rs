//! (feedcard, #88) The guard path of a refresh split by part (`VICTORY_FLUSH_PROF=1`), and
//! `VICTORY_GUARD_NOTE_ONCE` and `VICTORY_GUARD_PUBLISH_TRY`.
//!
//! The flush profile put the guard path (`watch::writes` on view memory) at 24.7 ms a frame over
//! about 1,600-2,000 calls on all threads, of which the `VirtualProtect` closes are about 7-9 ms.
//! The rest had no number: waits for the guards' shared lock (the title's write faults wait for
//! it 1.4-15 ms a frame by renderer port 6's line, so something holds it alone), the lock-free
//! checks before the walk (with their `VirtualQuery`), the walk itself, the alias lookups (one
//! global mutex), and the noting after it (one exclusive lock of the 4 KiB page table per page
//! written, the block claims of `publish`). This line gives each its share, and the exclusive
//! holds of the guards' lock by who took them, which are what every shared waiter waits behind.
//! Off, every count costs one cached load.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::Instant;

#[derive(Clone, Copy)]
pub(super) enum S {
    /// Shared holds of the guards' lock taken on the guard path, and the time waited for them.
    LockCalls,
    LockWaitUs,
    /// The lock-free checks before the walk (clean, lazy, left open, absent with their queries).
    PreUs,
    /// Every `VirtualQuery` of the guards, `is_view`'s too (all threads).
    QueryCalls,
    QueryUs,
    /// The walk under the shared lock, all of it (its `VirtualProtect`s, aliases and claims too).
    WalkUs,
    /// Of the walk: alias lookups (the views' global mutex).
    AliasUs,
    /// After the walk: pages noted written and the epochs published (claims, 4 KiB table lock).
    NoteUs,
    /// Block claims waited for (`ClaimOne::wait`: faults and `publish`).
    ClaimWaits,
    ClaimWaitUs,
    /// Of the noting: `publish` (its claim waits too), and blocks it left with
    /// `VICTORY_GUARD_PUBLISH_TRY` for want of their claim.
    PublishUs,
    PublishSkipped,
    /// Write faults handled under the shared lock: count, time in the handler, waiting for the lock.
    FaultCalls,
    FaultUs,
    FaultLockUs,
    /// Of the checks before the walk: guard calls, those answered there without the walk, and the
    /// queries of uncommitted holes they made (each call asks again).
    PreCalls,
    PreSettled,
    PreHoleCalls,
    PreHoleNs,
}

const S_COUNT: usize = S::PreHoleNs as usize + 1;
static STATS: [AtomicU64; S_COUNT] = [const { AtomicU64::new(0) }; S_COUNT];

/// Who held the guards' lock alone.
#[derive(Clone, Copy)]
pub(super) enum Who {
    Remap,
    DsWrite,
    KernelWrite,
    ViewsMapped,
    BeforeFree,
    BeforeProtect,
    LazyNoAccess,
    Other,
}

const WHO: usize = Who::Other as usize + 1;
const WHO_NAMES: [&str; WHO] = [
    "remaps",
    "DirectStorage writes",
    "kernel writes",
    "views mapped",
    "before VirtualFree",
    "before VirtualProtect",
    "lazy no-access",
    "other",
];
/// Per [`Who`]: holds, time waited for the lock, time held.
static EXCL: [[AtomicU64; 3]; WHO] = [const { [const { AtomicU64::new(0) }; 3] }; WHO];

#[inline]
pub(super) fn on() -> bool {
    crate::refresh_prof::on()
}

#[inline]
pub(super) fn add(what: S, n: u64) {
    if on() {
        STATS[what as usize].fetch_add(n, Relaxed);
    }
}

#[inline]
pub(super) fn start() -> Option<Instant> {
    on().then(Instant::now)
}

/// [`finish`] in nanoseconds, for calls of a microsecond or less.
#[inline]
pub(super) fn finish_ns(what: S, started: Option<Instant>) {
    if let Some(t) = started {
        STATS[what as usize].fetch_add(t.elapsed().as_nanos() as u64, Relaxed);
    }
}

#[inline]
pub(super) fn finish(what: S, started: Option<Instant>) {
    if let Some(t) = started {
        STATS[what as usize].fetch_add(t.elapsed().as_micros() as u64, Relaxed);
    }
}

/// The guards' lock held alone, timed by who took it.
pub(super) struct Excl {
    g: std::sync::RwLockWriteGuard<'static, super::Guards>,
    held: Option<(Instant, Who)>,
}

impl std::ops::Deref for Excl {
    type Target = super::Guards;
    fn deref(&self) -> &super::Guards {
        &self.g
    }
}

impl Drop for Excl {
    fn drop(&mut self) {
        if let Some((t, who)) = self.held {
            EXCL[who as usize][2].fetch_add(t.elapsed().as_micros() as u64, Relaxed);
        }
    }
}

pub(super) fn exclusive(lock: &'static std::sync::RwLock<super::Guards>, who: Who) -> Excl {
    let asked = start();
    let g = lock.write().unwrap_or_else(|e| e.into_inner());
    let held = asked.map(|t| {
        let e = &EXCL[who as usize];
        e[0].fetch_add(1, Relaxed);
        e[1].fetch_add(t.elapsed().as_micros() as u64, Relaxed);
        (Instant::now(), who)
    });
    Excl { g, held }
}

/// The guards' lock shared, on the guard path: the wait counted.
pub(super) fn shared(
    lock: &'static std::sync::RwLock<super::Guards>,
) -> std::sync::RwLockReadGuard<'static, super::Guards> {
    let asked = start();
    let g = lock.read().unwrap_or_else(|e| e.into_inner());
    if asked.is_some() {
        add(S::LockCalls, 1);
        finish(S::LockWaitUs, asked);
    }
    g
}

pub(super) fn log(frames: f64) {
    if !on() || frames <= 0.0 {
        return;
    }
    let s: Vec<u64> = STATS.iter().map(|a| a.swap(0, Relaxed)).collect();
    let marks = super::hole_marks::STATS.each_ref().map(|a| a.swap(0, Relaxed) as f64 / frames);
    let ms = |p: S| s[p as usize] as f64 / (frames * 1000.0);
    let per = |p: S| s[p as usize] as f64 / frames;
    let mut excl = String::new();
    let (mut holds, mut waited, mut held) = (0.0, 0.0, 0.0);
    for (i, e) in EXCL.iter().enumerate() {
        let [n, w, h] = [0, 1, 2].map(|k| e[k].swap(0, Relaxed) as f64);
        if n == 0.0 {
            continue;
        }
        holds += n;
        waited += w;
        held += h;
        excl.push_str(&format!(
            "; {} {:.1} ({:.2} ms waiting, {:.2} ms held)",
            WHO_NAMES[i],
            n / frames,
            w / (frames * 1000.0),
            h / (frames * 1000.0)
        ));
    }
    tracing::info!(
        "guard path split per frame (VICTORY_FLUSH_PROF, VICTORY_GUARD_NOTE_ONCE={}): \
         shared lock {:.0} holds, {:.2} ms waiting; checks before the walk {:.2} ms ({:.0} calls, \
         {:.0} answered there; their hole queries {:.0} in {:.2} ms; holes kept {:.0}, \
         VICTORY_GUARD_HOLES_KEPT={}; hole marks {:.0} answered, {:.0} queries, {:.0} runs marked, \
         {:.0} free left unmarked, {:.0} lost to a commit told meanwhile, VICTORY_GUARD_HOLE_MARKS={}); \
         walk {:.2} ms (alias lookups {:.2} ms; its VirtualProtects are the split line's); \
         noting after it {:.2} ms (publish {:.2} ms, {:.1} blocks left unpublished, \
         VICTORY_GUARD_PUBLISH_TRY={}); VirtualQuery {:.0} in {:.2} ms (all threads); \
         claims waited for {:.0} in {:.2} ms (faults and publish); \
         faults {:.0} handled in {:.2} ms (waiting for the shared lock {:.2} ms); \
         held alone {:.1} times, {:.2} ms waiting, {:.2} ms held{}",
        u8::from(note_once()),
        per(S::LockCalls),
        ms(S::LockWaitUs),
        ms(S::PreUs),
        per(S::PreCalls),
        per(S::PreSettled),
        per(S::PreHoleCalls),
        ms(S::PreHoleNs) / 1000.0,
        super::holes::KEPT_HITS.swap(0, Relaxed) as f64 / frames,
        u8::from(super::holes::kept_on()),
        marks[0],
        marks[1],
        marks[2],
        marks[3],
        marks[4],
        super::hole_marks::mode(),
        ms(S::WalkUs),
        ms(S::AliasUs),
        ms(S::NoteUs),
        ms(S::PublishUs),
        per(S::PublishSkipped),
        u8::from(publish_try()),
        per(S::QueryCalls),
        ms(S::QueryUs),
        per(S::ClaimWaits),
        ms(S::ClaimWaitUs),
        per(S::FaultCalls),
        ms(S::FaultUs),
        ms(S::FaultLockUs),
        holds / frames,
        waited / (frames * 1000.0),
        held / (frames * 1000.0),
        excl,
    );
}

/// (feedcard, #88) `VICTORY_GUARD_NOTE_ONCE=1` (Live; off by default): a guard check notes the
/// pages it found written under one hold of the 4 KiB page table's lock (`SUB_PAGES`, with
/// `VICTORY_DIRTY_4K`), not one exclusive hold per page.
///
/// Why: with `VICTORY_DIRTY_4K=1` (tuned set) every page the guards report written takes that
/// global lock alone, about a thousand to three thousand times a frame from up to 16 threads,
/// while every refresh (5,000 a frame) reads it shared (`watch::subpages`) and every kernel
/// asking writes it. Each exclusive taking waits for the readers in it and stops new ones.
///
/// Same result: per page the same steps in the same order (the whole-page stamp, then under the
/// lock the page's stamp and its 4 KiB entry dropped); the lock is held across the pages of one
/// check instead of taken again for each, so a reader sees all of them noted or none, which the
/// per-page form also allows. Not in the cache key: no shader or pipeline output.
static NOTE_ONCE: victory_core::settings::Live =
    victory_core::settings::Live::new("VICTORY_GUARD_NOTE_ONCE", |v| u64::from(v == Some("1")));

pub(super) fn note_once() -> bool {
    #[cfg(test)]
    if let Some(forced) = NOTE_ONCE_FORCED.with(std::cell::Cell::get) {
        return forced;
    }
    NOTE_ONCE.get_bool()
}

#[cfg(test)]
thread_local! {
    pub(super) static NOTE_ONCE_FORCED: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

/// The guards' written pages noted at asking `now`: [`super::written_set`] for each, the 4 KiB
/// table's lock taken once for all of them with [`note_once`].
pub(super) fn note_written(pages: &[usize], now: u64) {
    if pages.len() < 2 || !note_once() || !super::dirty4k_kept() {
        for &page in pages {
            super::written_set(page, now);
        }
        return;
    }
    for &page in pages {
        super::written_whole(page, now);
    }
    #[cfg(test)]
    super::NOTE_LOCKS.set(super::NOTE_LOCKS.get() + 1);
    let mut sub = super::SUB_PAGES.write().unwrap_or_else(|e| e.into_inner());
    for &page in pages {
        super::stamp_set(page, now);
        sub.remove(&page);
    }
}

/// (feedcard, #88) `VICTORY_GUARD_PUBLISH_TRY=1` (Live; off by default): after a guard walk,
/// `publish` marks a block published only if it can take the block's claim at once, and leaves
/// it unpublished otherwise, instead of spinning and then yielding the core until it is free.
///
/// Why: renderer port 6's line counts about 50,000 claim spins a frame (b5j), and the only
/// waiters are the write faults and `publish`. The claim `publish` waits for is held by a fault
/// opening that block (or by another walk closing it): a fault makes the block not clean, so
/// `publish` would find nothing to publish once it got the claim, and another walk publishes the
/// block itself. Past 64 spins the wait yields the core, which on a loaded machine can cost a
/// time slice inside the flush.
///
/// Same output: a published block only lets a later refresh skip its guard walk; a block left
/// unpublished keeps the 0 every path that made it not clean stored, so a later refresh answers
/// it from the clean blocks' table (also lock-free) until it is next written and closed, when
/// its walk publishes it. Copies, stamps and guards are unchanged. Not in
/// the cache key: no shader or pipeline output.
static PUBLISH_TRY: victory_core::settings::Live =
    victory_core::settings::Live::new("VICTORY_GUARD_PUBLISH_TRY", |v| u64::from(v == Some("1")));

pub(super) fn publish_try() -> bool {
    #[cfg(test)]
    if let Some(forced) = PUBLISH_TRY_FORCED.with(std::cell::Cell::get) {
        return forced;
    }
    PUBLISH_TRY.get_bool()
}

#[cfg(test)]
thread_local! {
    pub(super) static PUBLISH_TRY_FORCED: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
#[path = "guard_split_tests.rs"]
mod tests;
