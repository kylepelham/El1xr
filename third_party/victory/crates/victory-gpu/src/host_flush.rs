//! [`Batch::flush`](super::Batch::flush)'s refreshes of host copies on
//! several threads, and the frame line's account of the flush that ends a
//! submission's preparing (`batch_flush` on the "preparing breakdown" line).

use super::{PREPARING, ShadowBatch, ShadowStep};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

type HostCopy = ShadowBatch<Arc<crate::shadow::Shadow>>;

/// Refresh steps this long or longer are cut into pieces of [`SPLIT_CHUNK`]
/// so that one large copy (the 174 MB world heap) spreads over the threads.
const SPLIT_MIN: usize = 2 << 20;
const SPLIT_CHUNK: usize = 1 << 20;

/// One piece of work of a pass: a copy's steps in order, or a piece of one
/// long refresh.
pub(super) enum Task<C> {
    Copy(ShadowBatch<C>),
    Chunk(C, usize, usize),
}

/// One pass of a wide flush: each copy's steps up to its first long refresh
/// as one task, or, when that refresh comes first, its pieces as tasks of
/// their own; what follows is left for the next pass, in order.
pub(super) fn split_pass<C: Clone>(
    pending: Vec<ShadowBatch<C>>,
    page: usize,
) -> (Vec<Task<C>>, Vec<ShadowBatch<C>>) {
    let mut tasks: Vec<Task<C>> = Vec::with_capacity(pending.len());
    let mut next: Vec<ShadowBatch<C>> = Vec::new();
    for mut copy in pending {
        let big = copy.steps.iter().position(
            |s| matches!(*s, ShadowStep::Refresh(f, t) if t.saturating_sub(f) >= SPLIT_MIN),
        );
        match big {
            Some(0) => {
                if let ShadowStep::Refresh(from, to) = copy.steps.remove(0) {
                    for (a, b) in pieces(from, to, page) {
                        tasks.push(Task::Chunk(copy.shadow.clone(), a, b));
                    }
                }
                if !copy.steps.is_empty() {
                    next.push(copy);
                }
            }
            Some(pos) => {
                let rest = copy.steps.split_off(pos);
                tasks.push(Task::Copy(ShadowBatch {
                    shadow: copy.shadow.clone(),
                    refresh_span: None,
                    steps: copy.steps,
                }));
                next.push(ShadowBatch {
                    shadow: copy.shadow,
                    refresh_span: None,
                    steps: rest,
                });
            }
            None => tasks.push(Task::Copy(copy)),
        }
    }
    (tasks, next)
}

/// `[from, to)` in pieces of about [`SPLIT_CHUNK`] that end on `page`
/// boundaries (but the last).
pub(super) fn pieces(from: usize, to: usize, page: usize) -> Vec<(usize, usize)> {
    let mut out = Vec::with_capacity((to - from).div_ceil(SPLIT_CHUNK));
    let mut at = from;
    while at < to {
        let raw = at.saturating_add(SPLIT_CHUNK).min(to);
        let end = if raw < to {
            let aligned = raw & !(page - 1);
            if aligned > at {
                aligned
            } else {
                (at & !(page - 1)).saturating_add(page).min(to)
            }
        } else {
            to
        };
        out.push((at, end));
        at = end;
    }
    out
}

/// The refreshes of a flush: on several threads with `wide`, else one copy
/// after the other on this thread.
pub(super) fn refresh(copies: Vec<HostCopy>, wide: bool) {
    let started = Instant::now();
    if wide && copies.len() >= 2 {
        refresh_wide(copies);
    } else {
        for copy in copies {
            copy.run();
        }
    }
    add(F::RefreshUs, started.elapsed().as_micros() as u64);
}

fn refresh_wide(copies: Vec<HostCopy>) {
    let mut pending = copies;
    while !pending.is_empty() {
        if pending.len() == 1 {
            let alone = Instant::now();
            pending.pop().expect("one").run();
            add(F::AloneUs, alone.elapsed().as_micros() as u64);
            break;
        }
        let pass = Instant::now();
        let (tasks, next) = split_pass(pending, crate::watch::PAGE);
        add(F::Passes, 1);
        add(F::Tasks, tasks.len() as u64);
        add(
            F::Chunks,
            tasks
                .iter()
                .filter(|t| matches!(t, Task::Chunk(..)))
                .count() as u64,
        );
        let tasks = Arc::new(tasks);
        let total = tasks.len();
        crate::shadow::wide_chunks(total, 1, move |start, end| {
            let mut copied = 0usize;
            for task in &tasks[start..end] {
                let each = Instant::now();
                let (bytes, span) = match task {
                    Task::Copy(copy) => (
                        copy.run_wide(),
                        copy.steps
                            .iter()
                            .map(|s| match *s {
                                ShadowStep::Refresh(f, t) => t - f,
                                _ => 0,
                            })
                            .sum(),
                    ),
                    Task::Chunk(shadow, from, to) => {
                        let bytes = shadow.update(*from, to - from);
                        #[cfg(feature = "diagnostics")]
                        crate::copystat::range(
                            *from,
                            to - from,
                            each.elapsed().as_micros() as u64,
                            0,
                        );
                        (bytes, to - from)
                    }
                };
                copied += bytes;
                task_done(each.elapsed().as_micros() as u64, span);
            }
            if copied > 0 {
                PREPARING[1].fetch_add(copied as u64, Ordering::Relaxed);
                add(F::Bytes, copied as u64);
            }
        });
        let us = pass.elapsed().as_micros() as u64;
        PREPARING[0].fetch_add(us, Ordering::Relaxed);
        stats()[F::PassMaxUs as usize].fetch_max(us, Ordering::Relaxed);
        pending = next;
    }
}

/// The account's counters, summed since the last frame line (maxima kept).
#[derive(Clone, Copy)]
pub(super) enum F {
    Flushes,
    Copies,
    AskAheadUs,
    RefreshUs,
    AloneUs,
    Passes,
    Tasks,
    Chunks,
    BusyUs,
    TaskMaxUs,
    TaskMaxSpan,
    PassMaxUs,
    Bytes,
    FillsUs,
    Fills,
    FillBytes,
    TexturesUs,
    Textures,
    AfterUs,
    After,
    TotalUs,
}

const COUNT: usize = F::TotalUs as usize + 1;

/// Index 0: flushes in the middle of a submission (before a step that writes
/// title memory); 1: the flush that ends its preparing.
static STATS_BY: [[AtomicU64; COUNT]; 2] = [const { [const { AtomicU64::new(0) }; COUNT] }; 2];
/// Which of [`STATS_BY`] the flushes now running count into.
static LAST: AtomicBool = AtomicBool::new(false);

fn stats() -> &'static [AtomicU64; COUNT] {
    &STATS_BY[usize::from(LAST.load(Ordering::Relaxed))]
}

pub(super) fn add(what: F, n: u64) {
    stats()[what as usize].fetch_add(n, Ordering::Relaxed);
}

fn task_done(us: u64, span: usize) {
    add(F::BusyUs, us);
    if stats()[F::TaskMaxUs as usize].fetch_max(us, Ordering::Relaxed) < us {
        stats()[F::TaskMaxSpan as usize].store(span as u64, Ordering::Relaxed);
    }
}

pub(super) fn fills(us: u64, bytes: usize) {
    add(F::FillsUs, us);
    add(F::Fills, 1);
    add(F::FillBytes, bytes as u64);
}

/// `textures` refreshed in `refreshed_us`, then `after` callbacks, `us` in all.
pub(super) fn textures(textures: usize, refreshed_us: u64, after: usize, us: u64) {
    if textures == 0 && after == 0 {
        return;
    }
    add(F::Textures, textures as u64);
    add(F::TexturesUs, refreshed_us);
    add(F::After, after as u64);
    add(F::AfterUs, us - refreshed_us);
}

/// Runs the flush that ends a submission's preparing, counted apart from
/// the flushes in the middle of it.
pub(super) fn last<R>(flush: impl FnOnce() -> R) -> R {
    LAST.store(true, Ordering::Relaxed);
    let back = flush();
    LAST.store(false, Ordering::Relaxed);
    back
}

/// The account's line, per frame, for both kinds of flush.
pub(super) fn log(frames: f64) {
    crate::refresh_prof::log(frames);
    for (which, name) in [(1, "the last flush"), (0, "the rest of the submission")] {
        let s: Vec<u64> = STATS_BY[which]
            .iter()
            .map(|a| a.swap(0, Ordering::Relaxed))
            .collect();
        let ms = |f: F| s[f as usize] as f64 / (frames * 1000.0);
        let per = |f: F| s[f as usize] as f64 / frames;
        tracing::info!(
            "batch flush per frame, {name}: {:.1} flushes of {:.0} copies, {:.2} ms in all; asking ahead {:.2} ms; \
             refreshes {:.2} ms ({:.1} passes, {:.0} tasks of them {:.0} pieces of long refreshes, {:.2} ms busy \
             on all threads, the longest task {:.2} ms over {:.2} MB, the longest pass {:.2} ms, a copy alone \
             at the end {:.2} ms, {:.1} MB copied); fills {:.2} ms ({:.0}, {:.2} MB); textures {:.2} ms ({:.0}); \
             after textures {:.2} ms ({:.0})",
            per(F::Flushes),
            per(F::Copies),
            ms(F::TotalUs),
            ms(F::AskAheadUs),
            ms(F::RefreshUs),
            per(F::Passes),
            per(F::Tasks),
            per(F::Chunks),
            ms(F::BusyUs),
            s[F::TaskMaxUs as usize] as f64 / 1000.0,
            s[F::TaskMaxSpan as usize] as f64 / (1 << 20) as f64,
            s[F::PassMaxUs as usize] as f64 / 1000.0,
            ms(F::AloneUs),
            per(F::Bytes) / (1 << 20) as f64,
            ms(F::FillsUs),
            per(F::Fills),
            per(F::FillBytes) / (1 << 20) as f64,
            ms(F::TexturesUs),
            per(F::Textures),
            ms(F::AfterUs),
            per(F::After),
        );
    }
}
