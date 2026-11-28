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
pub(super) const SPLIT_MIN: usize = 2 << 20;
pub(super) const SPLIT_CHUNK: usize = 1 << 20;

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

/// Whether `pending` contains only cheap carded `Write`/`Barrier` steps (no
/// `Refresh` steps left), which run faster serially on the submitting thread
/// than waking up the worker pool for another pass.
fn only_carded_writes(pending: &[HostCopy]) -> bool {
    pending.iter().all(|copy| {
        copy.shadow.carded()
            && !copy
                .steps
                .iter()
                .any(|s| matches!(s, ShadowStep::Refresh(..)))
    })
}

/// `VICTORY_FLUSH_SMALL_ALONE=1`: a pass of a wide flush this small runs on
/// the submitting thread instead of waking the kept threads.
static SMALL_ALONE: victory_core::settings::Live =
    victory_core::settings::Live::flag("VICTORY_FLUSH_SMALL_ALONE");
/// The most copies, and bytes of refreshes over them all, of a small pass.
pub(super) const SMALL_COPIES: usize = 4;
pub(super) const SMALL_BYTES: usize = 256 << 10;

/// Whether `pending` is a pass small enough to run alone: at most
/// [`SMALL_COPIES`] copies whose refreshes span at most [`SMALL_BYTES`].
pub(super) fn small_pass<C>(pending: &[ShadowBatch<C>]) -> bool {
    pending.len() <= SMALL_COPIES
        && pending
            .iter()
            .flat_map(|c| &c.steps)
            .map(|s| match *s {
                ShadowStep::Refresh(f, t) => t.saturating_sub(f),
                _ => 0,
            })
            .sum::<usize>()
            <= SMALL_BYTES
}

/// Whether the next pass of a wide flush runs alone on this thread.
pub(super) fn alone_now<C>(
    pending: &[ShadowBatch<C>],
    small: bool,
    carded: impl Fn(&[ShadowBatch<C>]) -> bool,
) -> bool {
    pending.len() == 1 || carded(pending) || (small && small_pass(pending))
}

/// Small passes run alone since the last line: passes, refresh bytes, us.
static SMALL: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

/// The refreshes of a flush: on several threads with `wide`, else one copy
/// after the other on this thread.
pub(super) fn refresh(copies: Vec<HostCopy>, wide: bool) {
    let started = Instant::now();
    if wide && copies.len() >= 2 {
        let prep = Instant::now();
        let ranges: Vec<(usize, usize)> = copies
            .iter()
            .filter(|c| !c.shadow.unwatched())
            .filter_map(|c| c.refresh_span)
            .collect();
        if !ranges.is_empty() {
            crate::watch::prepare_flush(&ranges);
        }
        let prep_us = prep.elapsed().as_micros() as u64;
        PREPARING[0].fetch_add(prep_us, Ordering::Relaxed);
        add(F::AskAheadUs, prep_us);
        let wide = Instant::now();
        refresh_wide(copies);
        let one = &super::one_pass::STATS;
        one[super::one_pass::FLUSHES].fetch_add(1, Ordering::Relaxed);
        one[super::one_pass::WALL_US]
            .fetch_add(wide.elapsed().as_micros() as u64, Ordering::Relaxed);
    } else {
        for copy in copies {
            copy.run();
        }
    }
    add(F::RefreshUs, started.elapsed().as_micros() as u64);
}

fn refresh_wide(copies: Vec<HostCopy>) {
    let prof = crate::refresh_prof::on();
    let one_pass = super::one_pass::on();
    let small = SMALL_ALONE.get_bool();
    let mut pending = copies;
    while !pending.is_empty() {
        if alone_now(&pending, small, only_carded_writes) {
            let counted = small && pending.len() > 1 && !only_carded_writes(&pending);
            let bytes: usize = if counted {
                pending
                    .iter()
                    .flat_map(|c| &c.steps)
                    .map(|s| match *s {
                        ShadowStep::Refresh(f, t) => t - f,
                        _ => 0,
                    })
                    .sum()
            } else {
                0
            };
            let alone = Instant::now();
            for copy in pending {
                copy.run();
            }
            let us = alone.elapsed().as_micros() as u64;
            add(F::AloneUs, us);
            if counted {
                SMALL[0].fetch_add(1, Ordering::Relaxed);
                SMALL[1].fetch_add(bytes as u64, Ordering::Relaxed);
                SMALL[2].fetch_add(us, Ordering::Relaxed);
            }
            break;
        }
        if one_pass {
            refresh_one_pass(pending);
            return;
        }
        let pass = Instant::now();
        let (tasks, next) = split_pass(pending, crate::watch::PAGE);
        add(F::Passes, 1);
        let one = &super::one_pass::STATS;
        one[super::one_pass::RUNS].fetch_add(1, Ordering::Relaxed);
        one[super::one_pass::PIECES].fetch_add(
            tasks.iter().filter(|t| matches!(t, Task::Chunk(..))).count() as u64,
            Ordering::Relaxed,
        );
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
            let mut busy_us = 0u64;
            let mut max_us = 0u64;
            let mut max_span = 0usize;
            for task in &tasks[start..end] {
                let each = prof.then(Instant::now);
                let (bytes, span) = match task {
                    Task::Copy(copy) => {
                        let b = copy.run_wide();
                        let s = if prof {
                            copy.steps
                                .iter()
                                .map(|s| match *s {
                                    ShadowStep::Refresh(f, t) => t - f,
                                    _ => 0,
                                })
                                .sum()
                        } else {
                            0
                        };
                        (b, s)
                    }
                    Task::Chunk(shadow, from, to) => {
                        #[cfg(feature = "diagnostics")]
                        let diag_t = Instant::now();
                        let bytes = shadow.update(*from, to - from);
                        #[cfg(feature = "diagnostics")]
                        crate::copystat::range(
                            *from,
                            to - from,
                            diag_t.elapsed().as_micros() as u64,
                            0,
                        );
                        (bytes, to - from)
                    }
                };
                copied += bytes;
                if let Some(t) = each {
                    let us = t.elapsed().as_micros() as u64;
                    busy_us += us;
                    if us > max_us {
                        max_us = us;
                        max_span = span;
                    }
                }
            }
            if copied > 0 {
                PREPARING[1].fetch_add(copied as u64, Ordering::Relaxed);
                add(F::Bytes, copied as u64);
            }
            if busy_us > 0 {
                task_done(busy_us, max_us, max_span);
            }
        });
        let us = pass.elapsed().as_micros() as u64;
        PREPARING[0].fetch_add(us, Ordering::Relaxed);
        stats()[F::PassMaxUs as usize].fetch_max(us, Ordering::Relaxed);
        pending = next;
    }
}

/// [`refresh_wide`] in one run of the kept threads (`VICTORY_FLUSH_ONE_PASS=1`).
fn refresh_one_pass(copies: Vec<HostCopy>) {
    let started = Instant::now();
    add(F::Passes, 1);
    super::one_pass::STATS[super::one_pass::RUNS].fetch_add(1, Ordering::Relaxed);
    let run = Arc::new(super::one_pass::Run::new(
        copies,
        SPLIT_MIN,
        SPLIT_CHUNK,
        crate::watch::PAGE,
    ));
    let threads = crate::shadow::wide_threads() + 1;
    crate::shadow::wide_chunks(threads, 1, move |_, _| {
        let copied = AtomicU64::new(0);
        run.work(
            |copy| {
                add(F::Tasks, 1);
                copied.fetch_add(copy.run_wide() as u64, Ordering::Relaxed);
            },
            |shadow, from, to| {
                add(F::Tasks, 1);
                add(F::Chunks, 1);
                #[cfg(feature = "diagnostics")]
                let diag_t = Instant::now();
                let bytes = shadow.update(from, to - from);
                #[cfg(feature = "diagnostics")]
                crate::copystat::range(from, to - from, diag_t.elapsed().as_micros() as u64, 0);
                copied.fetch_add(bytes as u64, Ordering::Relaxed);
            },
        );
        let copied = copied.into_inner();
        if copied > 0 {
            PREPARING[1].fetch_add(copied, Ordering::Relaxed);
            add(F::Bytes, copied);
        }
    });
    let us = started.elapsed().as_micros() as u64;
    PREPARING[0].fetch_add(us, Ordering::Relaxed);
    stats()[F::PassMaxUs as usize].fetch_max(us, Ordering::Relaxed);
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
    match what {
        F::Copies => crate::timeline::flushed(0, n),
        F::Bytes | F::FillBytes => crate::timeline::flushed(1, n),
        F::Textures => crate::timeline::flushed(2, n),
        _ => {}
    }
}

fn task_done(busy_us: u64, max_us: u64, max_span: usize) {
    add(F::BusyUs, busy_us);
    if stats()[F::TaskMaxUs as usize].fetch_max(max_us, Ordering::Relaxed) < max_us {
        stats()[F::TaskMaxSpan as usize].store(max_span as u64, Ordering::Relaxed);
    }
}

pub(super) fn fills(us: u64, bytes: usize) {
    add(F::FillsUs, us);
    add(F::Fills, 1);
    add(F::FillBytes, bytes as u64);
}

/// `textures` refreshed in `us`.
pub(super) fn textures(textures: usize, us: u64) {
    add(F::Textures, textures as u64);
    add(F::TexturesUs, us);
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
    super::one_pass::line(frames);
    let small: Vec<u64> = SMALL.iter().map(|a| a.swap(0, Ordering::Relaxed)).collect();
    if SMALL_ALONE.get_bool() {
        tracing::info!(
            "small flush passes run on the submitting thread per frame (VICTORY_FLUSH_SMALL_ALONE=1, up to {} copies and {} KiB): {:.1} passes, {:.0} KiB of refreshes, {:.2} ms",
            SMALL_COPIES,
            SMALL_BYTES >> 10,
            small[0] as f64 / frames,
            small[1] as f64 / (frames * 1024.0),
            small[2] as f64 / (frames * 1000.0),
        );
    }
    crate::refresh_prof::log(frames);
    crate::shadow::refresh_stamp::line(frames);
    super::scoped_flush::line(frames);
    crate::compare_apart::line(frames);
    crate::shadow::wide_wake::line(frames);
    crate::scratch_landing::line(frames);
    crate::fresh_open::line(frames);
    if !crate::refresh_prof::on() {
        for arr in &STATS_BY {
            for a in arr {
                a.store(0, Ordering::Relaxed);
            }
        }
        return;
    }
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
             at the end {:.2} ms, {:.1} MB copied); fills {:.2} ms ({:.0}, {:.2} MB); textures {:.2} ms ({:.0})",
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
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn batch(id: usize, steps: Vec<ShadowStep>) -> ShadowBatch<usize> {
        ShadowBatch {
            shadow: id,
            refresh_span: None,
            steps,
        }
    }

    #[test]
    fn small_passes_are_few_copies_of_few_bytes() {
        let r = |n: usize| ShadowStep::Refresh(0x10000, 0x10000 + n);
        assert!(small_pass(&[batch(0, vec![r(SMALL_BYTES)])]));
        assert!(!small_pass(&[batch(0, vec![r(SMALL_BYTES + 1)])]));
        let four: Vec<_> = (0..SMALL_COPIES).map(|i| batch(i, vec![r(4096), ShadowStep::Write(1, 2)])).collect();
        assert!(small_pass(&four));
        let five: Vec<_> = (0..=SMALL_COPIES).map(|i| batch(i, vec![r(4096)])).collect();
        assert!(!small_pass(&five));
        let split = vec![batch(0, vec![r(SMALL_BYTES / 2)]), batch(1, vec![r(SMALL_BYTES / 2 + 1)])];
        assert!(!small_pass(&split), "the bytes count over all the copies");
        assert!(!alone_now(&split, true, |_| false));
        assert!(alone_now(&split[..1], false, |_| false), "one copy runs alone as before");
        assert!(alone_now(&four, true, |_| false));
        assert!(!alone_now(&four, false, |_| false), "off: as before");
    }

    /// Drives a wide flush the way `refresh_wide` does (same `split_pass`
    /// and `alone_now`), recording each copy's steps in the order they ran
    /// and how many passes woke the kept threads.
    fn drive(copies: Vec<ShadowBatch<usize>>, small: bool) -> (Vec<Vec<ShadowStep>>, usize) {
        let n = copies.len();
        let mut did: Vec<Vec<ShadowStep>> = vec![Vec::new(); n];
        let mut pending = copies;
        let mut wide = 0;
        while !pending.is_empty() {
            if alone_now(&pending, small, |_| false) {
                for c in pending {
                    did[c.shadow].extend(c.steps);
                }
                break;
            }
            wide += 1;
            let (tasks, next) = split_pass(pending, crate::watch::PAGE);
            // The kept threads run tasks in any order: last first here.
            for t in tasks.into_iter().rev() {
                match t {
                    Task::Copy(c) => did[c.shadow].extend(c.steps),
                    Task::Chunk(id, a, b) => did[id].push(ShadowStep::Refresh(a, b)),
                }
            }
            pending = next;
        }
        (did, wide)
    }

    /// Joins adjacent refreshes (pieces of a long refresh) for comparison.
    fn joined(steps: &[ShadowStep]) -> Vec<(u8, usize, usize)> {
        let mut out: Vec<(u8, usize, usize)> = Vec::new();
        for &s in steps {
            let k = match s {
                ShadowStep::Refresh(a, b) => (0, a, b),
                ShadowStep::Write(a, l) => (1, a, l),
                ShadowStep::Barrier => (2, 0, 0),
            };
            match out.last_mut() {
                Some(last) if last.0 == 0 && k.0 == 0 && last.2 == k.1 => last.2 = k.2,
                _ => out.push(k),
            }
        }
        out
    }

    #[test]
    fn small_passes_alone_do_the_same_steps_in_the_same_order() {
        let p = crate::watch::PAGE;
        let long = SPLIT_MIN + 3 * p;
        let shapes: Vec<Vec<ShadowBatch<usize>>> = vec![
            // A long refresh, then a small tail: the tail's pass runs alone.
            vec![
                batch(0, vec![ShadowStep::Refresh(0, long), ShadowStep::Write(8, 4), ShadowStep::Refresh(p, 2 * p)]),
                batch(1, vec![ShadowStep::Refresh(long, 2 * long), ShadowStep::Refresh(p, 2 * p)]),
                batch(2, vec![ShadowStep::Refresh(p, 2 * p)]),
            ],
            // Few small copies: the whole flush runs alone.
            vec![
                batch(0, vec![ShadowStep::Refresh(0, 4096)]),
                batch(1, vec![ShadowStep::Write(0, 16), ShadowStep::Barrier, ShadowStep::Refresh(0, 64)]),
            ],
            // Many copies and two long refreshes in one copy: wide as before.
            (0..12)
                .map(|i| {
                    batch(i, vec![
                        ShadowStep::Refresh(i * long, (i + 1) * long),
                        ShadowStep::Write(i, 1),
                        ShadowStep::Refresh(i * long, i * long + long),
                    ])
                })
                .collect(),
        ];
        let mut fewer = 0;
        for copies in shapes {
            let copy = |c: &Vec<ShadowBatch<usize>>| {
                c.iter().map(|b| batch(b.shadow, b.steps.clone())).collect::<Vec<_>>()
            };
            let (off, wide_off) = drive(copy(&copies), false);
            let (on, wide_on) = drive(copy(&copies), true);
            assert_eq!(off.len(), on.len());
            for (a, b) in off.iter().zip(&on) {
                assert_eq!(joined(a), joined(b), "each copy: the same steps in the same order");
            }
            assert!(wide_on <= wide_off);
            fewer += wide_off - wide_on;
        }
        assert!(fewer >= 2, "the small passes did not wake the kept threads");
    }

    #[test]
    fn the_switch_is_live() {
        if std::env::var_os("VICTORY_FLUSH_SMALL_ALONE").is_some() {
            return;
        }
        assert!(!SMALL_ALONE.get_bool(), "off by default");
        victory_core::settings::set("VICTORY_FLUSH_SMALL_ALONE", "1").unwrap();
        let now = SMALL_ALONE.get_bool();
        victory_core::settings::set("VICTORY_FLUSH_SMALL_ALONE", "").unwrap();
        assert!(now, "a change takes effect while running");
        assert!(!SMALL_ALONE.get_bool());
    }
}
