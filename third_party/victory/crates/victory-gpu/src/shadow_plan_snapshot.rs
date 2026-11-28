//! (feedcard, #88) `VICTORY_PLAN_SNAPSHOT=1` (Live; off by default): the sub-page plan of a
//! refresh's stale copy (`CopiedPages::plan_stale_subpages`, the path `VICTORY_WATCH_4K` /
//! `VICTORY_DIRTY_4K` take) is made from a snapshot of the copy's page stamps, with no lock held,
//! and published under the copy's lock only if the stamps it read have not moved.
//!
//! The wide flush cuts a big copy into pieces and refreshes them on up to 16 threads at once.
//! Each piece's plan (stamps, partial spans, the 4 KiB masks of `watch::stale_mask`) ran under the
//! copy's one exclusive lock, so the pieces of one copy planned one after another: b5a's FLUSH_PROF
//! line put the plans at 5.0 ms a frame over all threads, 2.4 ms of it waiting for the lock (b5h:
//! 2.3 / 0.8 ms).
//!
//! Now, under the lock shared, the stamps and partial spans of the request's pages are copied out
//! (a slice copy). The plan runs on that copy alone (the same `plan_stale_subpages`, so the same
//! runs for the same stamps). Then, under the lock exclusive, the request's stamps and spans are
//! compared with the snapshot: equal, the plan's changes are written in (a few stores); moved
//! (another thread changed a page of the request between), the plan is made again under the
//! exclusive lock, as with the switch off, the masks already asked for reused. A plan that changes
//! nothing takes no exclusive lock at all. The masks read the guards' and the write watch's
//! stamps, which no copy lock orders (as `VICTORY_PLAN_SPLIT` argues): asking them outside the
//! lock is as if the thread had been held up. Unit tests: the same runs and stamps as the locked
//! plan over random requests, and after a change slipped in between. Not in the cache key: no
//! shader or pipeline output.

use super::copied_shared::CopiedLock;
use super::{CopiedPages, PAGE};
use crate::refresh_prof::{self, P};

static ON: victory_core::settings::Live =
    victory_core::settings::Live::new("VICTORY_PLAN_SNAPSHOT", |v| u64::from(v == Some("1")));

/// Live: read once per stale copy.
pub(crate) fn on() -> bool {
    ON.get_bool()
}

type Spans = Vec<(u32, u32, u64)>;

/// The partial spans of the pages `lo..=hi`, in page order.
fn partial_of(c: &CopiedPages, lo: usize, hi: usize) -> Vec<(usize, Spans)> {
    if c.partial.is_empty() {
        return Vec::new();
    }
    let mut v: Vec<(usize, Spans)> = if c.partial.len() <= hi - lo + 1 {
        c.partial
            .iter()
            .filter(|(k, _)| (lo..=hi).contains(*k))
            .map(|(&k, s)| (k, s.clone()))
            .collect()
    } else {
        (lo..=hi)
            .filter_map(|i| c.partial.get(&i).map(|s| (i, s.clone())))
            .collect()
    };
    v.sort_unstable_by_key(|e| e.0);
    v
}

/// What [`plan`] did, for the tests and the line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Done {
    /// Nothing to plan (an empty request).
    Empty,
    /// The plan changed no stamp: no exclusive lock.
    Unchanged,
    /// The snapshot's plan was written in.
    Published,
    /// A page moved between the snapshot and the publish: planned again under the lock.
    Redone,
}

/// The stale copy's sub-page plan from a snapshot (module doc). Same arguments and runs as
/// [`CopiedPages::plan_stale_subpages`] on the locked stamps.
#[allow(clippy::too_many_arguments)]
pub(super) fn plan(
    lock: &CopiedLock,
    base: usize,
    start: usize,
    end: usize,
    from: usize,
    to: usize,
    now: u64,
    written: &[(usize, u64)],
    subs: &[(usize, u64, u64, u16)],
    page_mask: &mut impl FnMut(usize, u64, Option<(u64, u16)>) -> Option<u16>,
    skip_idx: impl Fn(usize) -> bool,
    planning: Option<std::time::Instant>,
) -> Vec<(usize, usize)> {
    plan_with(
        lock,
        base,
        start,
        end,
        from,
        to,
        now,
        written,
        subs,
        page_mask,
        skip_idx,
        planning,
        |_| {},
    )
    .0
}

/// [`plan`], with `between` run after the plan and before the publish (tests slip changes in).
#[allow(clippy::too_many_arguments)]
pub(super) fn plan_with(
    lock: &CopiedLock,
    base: usize,
    start: usize,
    end: usize,
    from: usize,
    to: usize,
    now: u64,
    written: &[(usize, u64)],
    subs: &[(usize, u64, u64, u16)],
    page_mask: &mut impl FnMut(usize, u64, Option<(u64, u16)>) -> Option<u16>,
    skip_idx: impl Fn(usize) -> bool,
    planning: Option<std::time::Instant>,
    between: impl FnOnce(&CopiedLock),
) -> (Vec<(usize, usize)>, Done) {
    let (from, to) = (from.max(start), to.min(end));
    if from >= to {
        refresh_prof::finish(P::StalePlanUs, planning);
        return (Vec::new(), Done::Empty);
    }
    let (lo, hi) = (from / PAGE - base, (to - 1) / PAGE - base);
    let (at, partial) = {
        let c = lock.read();
        refresh_prof::finish(P::StalePlanWaitUs, planning);
        (c.at[lo..=hi].to_vec(), partial_of(&c, lo, hi))
    };
    let mut scratch = CopiedPages::with_base(hi - lo + 1, base + lo);
    for (i, &s) in at.iter().enumerate() {
        scratch.set(i, s);
    }
    scratch.partial = partial.iter().map(|(k, s)| (k - lo, s.clone())).collect();
    // The masks asked for, in page order, to answer the same questions again on a redo.
    let mut asked: Vec<(usize, u64, Option<(u64, u16)>, Option<u16>)> = Vec::new();
    let runs = scratch.plan_stale_subpages(
        base + lo,
        start,
        end,
        from,
        to,
        now,
        written,
        subs,
        |p, prev, sub| {
            let m = page_mask(p, prev, sub);
            asked.push((p, prev, sub, m));
            m
        },
        |i| skip_idx(i + lo),
    );
    between(lock);
    let after = partial_of(&scratch, 0, hi - lo);
    let moved_partial = after.len() != partial.len()
        || after
            .iter()
            .zip(&partial)
            .any(|((k, s), (k0, s0))| k + lo != *k0 || s != s0);
    if scratch.at == at && !moved_partial {
        refresh_prof::add(P::SnapUnchanged, 1);
        refresh_prof::finish(P::StalePlanUs, planning);
        return (runs, Done::Unchanged);
    }
    let waiting = refresh_prof::start();
    let mut live = lock.lock().unwrap_or_else(|e| e.into_inner());
    refresh_prof::finish(P::StalePlanWaitUs, waiting);
    let held = refresh_prof::start();
    let done = if live.at[lo..=hi] == at[..] && partial_of(&live, lo, hi) == partial {
        for (i, (&s, &s0)) in scratch.at.iter().zip(&at).enumerate() {
            if s != s0 {
                live.set(lo + i, s);
            }
        }
        for (k, _) in &partial {
            live.partial.remove(k);
        }
        for (k, s) in after {
            live.partial.insert(k + lo, s);
        }
        refresh_prof::add(P::SnapPublished, 1);
        (runs, Done::Published)
    } else {
        let mut ask = |p: usize, prev: u64, sub: Option<(u64, u16)>| match asked
            .binary_search_by_key(&p, |e| e.0)
        {
            Ok(i) if asked[i].1 == prev && asked[i].2 == sub => asked[i].3,
            _ => page_mask(p, prev, sub),
        };
        let runs = live.plan_stale_subpages(
            base, start, end, from, to, now, written, subs, &mut ask, skip_idx,
        );
        refresh_prof::add(P::SnapRedone, 1);
        (runs, Done::Redone)
    };
    drop(live);
    refresh_prof::finish(P::SnapHeldUs, held);
    refresh_prof::finish(P::StalePlanUs, planning);
    done
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    /// A mask that depends on the page, the last copy and the sub-page answer only.
    fn mask(page: usize, prev: u64, sub: Option<(u64, u16)>) -> Option<u16> {
        let h = (page as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ prev.rotate_left(17);
        match (h % 5, sub) {
            (0, _) => None,
            (1, Some((_, bits))) => (bits != 0).then_some(bits),
            (2, _) => Some(1 << (h % 16)),
            _ => Some((h >> 8) as u16).filter(|&m| m != 0),
        }
    }

    fn state(c: &CopiedPages) -> (Vec<u64>, Vec<u64>, Vec<(usize, Spans)>, usize) {
        let mut partial: Vec<_> = c.partial.iter().map(|(&k, v)| (k, v.clone())).collect();
        partial.sort_unstable();
        (c.at.clone(), c.uncopied_bits.clone(), partial, c.uncopied)
    }

    /// One random request against a buffer of `pages` pages from `base`.
    #[allow(clippy::type_complexity)]
    fn request(
        rng: &mut Rng,
        base: usize,
        pages: usize,
        start: usize,
        end: usize,
        now: u64,
    ) -> (
        usize,
        usize,
        Vec<(usize, u64)>,
        Vec<(usize, u64, u64, u16)>,
        u64,
    ) {
        let a = start + rng.below((end - start) as u64) as usize;
        let b = (a + 1 + rng.below(9 * PAGE as u64) as usize).min(end);
        let (a, b) = if rng.below(3) == 0 {
            (a / PAGE * PAGE, b.div_ceil(PAGE) * PAGE)
        } else {
            (a, b)
        };
        let (mut written, mut subs) = (Vec::new(), Vec::new());
        for page in base..base + pages {
            if rng.below(3) == 0 {
                let w = rng.below(now + 2);
                if w > 0 {
                    written.push((page, w));
                    if rng.below(2) == 0 {
                        subs.push((page, w, rng.below(now + 1), rng.next() as u16));
                    }
                }
            }
        }
        (a, b, written, subs, rng.next())
    }

    fn skipper(skip: u64) -> impl Fn(usize) -> bool {
        move |idx: usize| skip >> (idx % 64) & 1 == 1 && skip % 7 == 0
    }

    #[test]
    fn the_snapshot_plan_gives_the_same_runs_and_stamps_as_the_locked_plan() {
        const PAGES: usize = 40;
        let base = 1000;
        let mut seen = [0usize; 4];
        for seed in 1..80u64 {
            let mut rng = Rng(seed.wrapping_mul(0x2545_f491_4f6c_dd1d));
            let start = base * PAGE + rng.below(3) as usize * 4096 + rng.below(2) as usize * 100;
            let end =
                (base + PAGES) * PAGE - rng.below(3) as usize * 4096 - rng.below(2) as usize * 52;
            let mut one = CopiedPages::with_base(PAGES, base);
            let snap = CopiedLock::new(CopiedPages::with_base(PAGES, base));
            for now in 1..50u64 {
                let (a, b, written, subs, skip) = request(&mut rng, base, PAGES, start, end, now);
                let (mut asked_one, mut asked_snap) = (Vec::new(), Vec::new());
                let want = one.plan_stale_subpages(
                    base,
                    start,
                    end,
                    a,
                    b,
                    now,
                    &written,
                    &subs,
                    |p, prev, s| {
                        asked_one.push((p, prev, s));
                        mask(p, prev, s)
                    },
                    skipper(skip),
                );
                let mut m = |p, prev, s| {
                    asked_snap.push((p, prev, s));
                    mask(p, prev, s)
                };
                let (got, done) = plan_with(
                    &snap,
                    base,
                    start,
                    end,
                    a,
                    b,
                    now,
                    &written,
                    &subs,
                    &mut m,
                    skipper(skip),
                    None,
                    |_| {},
                );
                assert_ne!(done, Done::Redone, "nothing moved");
                seen[done as usize] += 1;
                assert_eq!(want, got, "seed {seed} now {now}: [{a:#x}, {b:#x})");
                assert_eq!(
                    state(&one),
                    state(&snap.lock().unwrap()),
                    "seed {seed} now {now}"
                );
                assert_eq!(asked_one, asked_snap, "the same masks asked for");
            }
        }
        assert!(
            seen[1] > 0 && seen[2] > 0,
            "both no-op and published plans seen: {seen:?}"
        );
    }

    #[test]
    fn a_change_slipped_in_between_is_redone_under_the_lock() {
        const PAGES: usize = 24;
        let base = 50;
        let mut seen = [0usize; 4];
        for seed in 1..80u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
            let (start, end) = (base * PAGE + 4096, (base + PAGES) * PAGE - 100);
            let snap = CopiedLock::new(CopiedPages::with_base(PAGES, base));
            for now in 1..30u64 {
                let (a, b, written, subs, skip) = request(&mut rng, base, PAGES, start, end, now);
                // The change another thread makes: a stamp in the request moved (or forgotten),
                // or a partial page's spans.
                let page = (a / PAGE).clamp(base, base + PAGES - 1) - base;
                let kind = rng.below(4);
                let stamp = if kind == 0 { 0 } else { now + rng.below(3) };
                let change = |c: &mut CopiedPages| match kind {
                    3 => {
                        super::super::insert_partial_span(
                            c.partial.entry(page).or_default(),
                            4096,
                            8192,
                            stamp,
                        );
                    }
                    _ => c.set(page, stamp),
                };
                let plan_locked = |c: &mut CopiedPages| {
                    c.plan_stale_subpages(
                        base,
                        start,
                        end,
                        a,
                        b,
                        now,
                        &written,
                        &subs,
                        mask,
                        skipper(skip),
                    )
                };
                // The change first, then the plan (what a redo must give) ...
                let mut first = copy_of(&snap.lock().unwrap());
                change(&mut first);
                let want_first = plan_locked(&mut first);
                // ... or the plan first (a plan that changed nothing linearises at its snapshot).
                let mut after = copy_of(&snap.lock().unwrap());
                let want_after = plan_locked(&mut after);
                change(&mut after);
                let (got, done) = plan_with(
                    &snap,
                    base,
                    start,
                    end,
                    a,
                    b,
                    now,
                    &written,
                    &subs,
                    &mut mask,
                    skipper(skip),
                    None,
                    |l| change(&mut *l.lock().unwrap()),
                );
                seen[done as usize] += 1;
                let (want, c) = if done == Done::Unchanged {
                    (want_after, after)
                } else {
                    (want_first, first)
                };
                assert_eq!(
                    want, got,
                    "seed {seed} now {now}: [{a:#x}, {b:#x}) {done:?}"
                );
                assert_eq!(state(&c), state(&snap.lock().unwrap()), "seed {seed} now {now}");
            }
        }
        assert!(seen[3] > 100, "redone {seen:?}");
    }

    fn copy_of(c: &CopiedPages) -> CopiedPages {
        let mut n = CopiedPages::with_base(c.at.len(), c.base);
        n.at.clone_from(&c.at);
        n.uncopied_bits.clone_from(&c.uncopied_bits);
        n.uncopied = c.uncopied;
        n.partial = c.partial.clone();
        n
    }

    /// Sixteen threads plan overlapping requests of one copy with the snapshot; the stamps stay
    /// consistent (the uncopied bits and count match the stamps).
    #[test]
    fn plans_on_sixteen_threads_keep_the_stamps_consistent() {
        use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
        const PAGES: usize = 256;
        let snap = CopiedLock::new(CopiedPages::with_base(PAGES, 0));
        let redone = AtomicUsize::new(0);
        std::thread::scope(|sc| {
            for t in 0..16u64 {
                let (snap, redone) = (&snap, &redone);
                sc.spawn(move || {
                    let mut rng = Rng((t + 1).wrapping_mul(0x2545_f491_4f6c_dd1d));
                    for now in 1..300u64 {
                        let a = rng.below(PAGES as u64 - 8) as usize * PAGE;
                        let b = a + (1 + rng.below(8) as usize) * PAGE;
                        let written: Vec<(usize, u64)> = (a / PAGE..b / PAGE)
                            .map(|p| (p, now + rng.below(2)))
                            .collect();
                        let (_, done) = plan_with(
                            snap,
                            0,
                            0,
                            PAGES * PAGE,
                            a,
                            b,
                            now * 16 + t,
                            &written,
                            &[],
                            &mut mask,
                            |_| false,
                            None,
                            |_| {},
                        );
                        redone.fetch_add(usize::from(done == Done::Redone), Relaxed);
                    }
                });
            }
        });
        let c = snap.lock().unwrap();
        let zeros = c.at.iter().filter(|&&s| s == 0).count();
        assert_eq!(c.uncopied, zeros);
        for (i, &s) in c.at.iter().enumerate() {
            assert_eq!(
                c.uncopied_bits[i / 64] >> (i % 64) & 1 == 1,
                s == 0,
                "page {i}"
            );
        }
        assert!(
            c.partial.is_empty(),
            "whole-page requests leave no partial spans"
        );
        println!("redone {}", redone.into_inner());
    }

    #[test]
    fn the_switch_is_live() {
        assert_eq!(
            on(),
            std::env::var_os("VICTORY_PLAN_SNAPSHOT").is_some_and(|v| v == "1")
        );
        if std::env::var_os("VICTORY_PLAN_SNAPSHOT").is_some() {
            return;
        }
        victory_core::settings::set("VICTORY_PLAN_SNAPSHOT", "1").unwrap();
        let now = on();
        victory_core::settings::set("VICTORY_PLAN_SNAPSHOT", "").unwrap();
        assert!(now, "a change takes effect while running");
        assert!(!on());
    }

    /// (#88) Sizes the switch: 16 threads plan 1 MiB pieces of one 256 MiB copy, a quarter of
    /// the pages stale, with a mask that costs what `watch::stale_mask` and `note_4k` cost
    /// (sixteen loads from a shared table, two adds to shared counters): under the exclusive lock
    /// (switch off) and from a snapshot (on). Ignored; `--ignored --nocapture`, release for real
    /// numbers.
    #[test]
    #[ignore]
    fn bench_plan_from_a_snapshot() {
        use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};
        const PAGES: usize = 4096;
        const THREADS: usize = 16;
        const ROUNDS: u64 = 60;
        let stamps: Vec<AtomicU64> = (0..PAGES * 16)
            .map(|i| AtomicU64::new(i as u64 % 7))
            .collect();
        let counters = [AtomicU64::new(0), AtomicU64::new(0)];
        let real_mask = |page: usize, prev: u64, _sub: Option<(u64, u16)>| -> Option<u16> {
            let m = (0..16).fold(0u16, |m, k| {
                m | (u16::from(stamps[(page % PAGES) * 16 + k].load(Relaxed) > prev % 7) << k)
            });
            counters[0].fetch_add(u64::from(m.count_ones()), Relaxed);
            counters[1].fetch_add(1, Relaxed);
            (m != 0).then_some(m)
        };
        for snapshot in [false, true, false, true] {
            let copied = CopiedLock::new(CopiedPages::with_base(PAGES, 0));
            let (start, end) = (0, PAGES * PAGE);
            let (total, worst) = (AtomicU64::new(0), AtomicU64::new(0));
            let redone = AtomicUsize::new(0);
            let wall = std::time::Instant::now();
            for now in 1..=ROUNDS {
                let written: Vec<(usize, u64)> = (0..PAGES)
                    .filter(|p| (p * 7 + now as usize) % 4 == 0)
                    .map(|p| (p, now))
                    .collect();
                let pieces: Vec<(usize, usize)> = (0..PAGES / 16)
                    .map(|i| (i * 16 * PAGE, (i + 1) * 16 * PAGE))
                    .collect();
                let next = AtomicUsize::new(0);
                std::thread::scope(|sc| {
                    for _ in 0..THREADS {
                        sc.spawn(|| {
                            let mut m = real_mask;
                            loop {
                                let i = next.fetch_add(1, Relaxed);
                                let Some(&(a, b)) = pieces.get(i) else { break };
                                let t = std::time::Instant::now();
                                let runs = if snapshot {
                                    let (runs, done) = plan_with(
                                        &copied,
                                        0,
                                        start,
                                        end,
                                        a,
                                        b,
                                        now,
                                        &written,
                                        &[],
                                        &mut m,
                                        |_| false,
                                        None,
                                        |_| {},
                                    );
                                    redone.fetch_add(usize::from(done == Done::Redone), Relaxed);
                                    runs
                                } else {
                                    let mut c = copied.lock().unwrap();
                                    c.plan_stale_subpages(
                                        0,
                                        start,
                                        end,
                                        a,
                                        b,
                                        now,
                                        &written,
                                        &[],
                                        &mut m,
                                        |_| false,
                                    )
                                };
                                let ns = t.elapsed().as_nanos() as u64;
                                total.fetch_add(ns, Relaxed);
                                worst.fetch_max(ns, Relaxed);
                                std::hint::black_box(runs);
                            }
                        });
                    }
                });
            }
            let plans = (PAGES / 16) as f64 * ROUNDS as f64;
            println!(
                "plan of 1 MiB pieces on {THREADS} threads, snapshot={snapshot}: {:.2} us a plan \
                 (lock wait included, all threads), worst {:.0} us, wall {:.1} ms for {plans} \
                 plans, redone {}",
                total.into_inner() as f64 / 1e3 / plans,
                worst.into_inner() as f64 / 1e3,
                wall.elapsed().as_secs_f64() * 1e3,
                redone.into_inner()
            );
        }
    }
}
