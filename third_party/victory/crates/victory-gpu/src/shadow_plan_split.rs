//! (feedcard, #88) `VICTORY_PLAN_SPLIT=1` (Live; off by default): the sub-page
//! plan of a refresh's stale copy (`CopiedPages::plan_stale_subpages`, the path
//! `VICTORY_WATCH_4K` / `VICTORY_DIRTY_4K` take) is cut in two. Under the copy's
//! lock only what reads or writes the copy's own state is done: each page's last
//! copy is read and stamped, its gaps (partial spans) are found, and a page cut
//! by the request's ends is planned whole there, partial spans and all. Pages
//! taken whole by the request leave the lock as a step; their 4 KiB masks
//! (`watch::stale_mask`: two table lookups and sixteen atomic loads, plus the
//! shared counters the mask closure bumps) and the byte runs are worked out
//! after the lock is let go.
//!
//! The runs are the same as the one-piece plan gives for the same masks (unit
//! test), and so is the copy's state after it. The masks read the guards' and
//! the write watch's stamps, which no copy lock orders: reading them after the
//! lock is as if the thread had been held up between the stamp and the mask,
//! which the one-piece plan already allows. A batch 4 profile put the plan under
//! the lock at 2.6-5.0 ms a frame over all threads. Not in the cache key: no
//! shader or pipeline output.

use super::{CopiedPages, PAGE, extend_runs, insert_partial_span};

static ON: victory_core::settings::Live =
    victory_core::settings::Live::new("VICTORY_PLAN_SPLIT", |v| u64::from(v == Some("1")));

/// Live: read once per stale copy.
pub(crate) fn on() -> bool {
    ON.get_bool()
}

const KERNEL_PAGE: usize = 4096;

/// A page's gaps: the whole requested part `[lo, hi)`, or the spans left.
#[derive(Debug)]
pub(super) enum Gaps {
    Whole(u32, u32),
    Spans(Vec<(u32, u32)>),
}

/// What the plan under the lock leaves to do, in page order.
#[derive(Debug)]
pub(super) enum Step {
    /// End the run being built.
    Flush,
    /// A byte run of its own.
    Push(usize, usize),
    /// The page whole, joined to the run when it follows it.
    Extend(usize),
    /// A page taken whole by the request: its mask, then its runs.
    Page {
        page: usize,
        prev: u64,
        sub: Option<(u64, u16)>,
        gaps: Gaps,
    },
}

/// The plan taken under the lock; [`Claimed::finish`] makes the runs.
#[derive(Debug)]
pub(super) struct Claimed {
    steps: Vec<Step>,
    from: usize,
    to: usize,
}

impl CopiedPages {
    /// The part of [`CopiedPages::plan_stale_subpages`] that needs the copy's
    /// lock: the same stamps and partial spans, the masks of pages taken whole
    /// left to [`Claimed::finish`].
    #[allow(clippy::too_many_arguments)]
    pub(super) fn plan_stale_subpages_claim(
        &mut self,
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
    ) -> Claimed {
        let from = from.max(start);
        let to = to.min(end);
        let mut steps = Vec::new();
        if from >= to {
            return Claimed { steps, from, to };
        }
        let mut writes = written.iter().peekable();
        let mut subs = subs.iter().peekable();
        let last = (to - 1) / PAGE;
        for page in from / PAGE..=last {
            let write = match writes.peek() {
                Some(&&(p, w)) if p == page => {
                    writes.next();
                    w
                }
                _ => 0,
            };
            while subs.peek().is_some_and(|s| s.0 < page) {
                subs.next();
            }
            let sub = subs
                .peek()
                .filter(|s| s.0 == page && s.1 == write && write != 0)
                .map(|s| (s.2, s.3));
            let idx = page - base;
            if skip_idx(idx) {
                continue;
            }
            let prev = self.get(idx).unwrap_or(0);
            if !(prev == 0 || write > prev) {
                steps.push(Step::Flush);
                continue;
            }
            let page_base = page * PAGE;
            let page_lo = page_base.max(start);
            let page_hi = (page_base + PAGE).min(end);
            let req_lo = page_lo.max(from);
            let req_hi = page_hi.min(to);
            let r_lo = (req_lo - page_base) as u32;
            let r_hi = (req_hi - page_base) as u32;
            let full_page = req_lo == page_lo && req_hi == page_hi;
            let gaps = if self.partial.is_empty() {
                Gaps::Whole(r_lo, r_hi)
            } else {
                let g = self.stale_gaps(idx, write, r_lo, r_hi);
                if g.is_empty() {
                    steps.push(Step::Flush);
                    if full_page {
                        self.set(idx, now);
                    }
                    continue;
                }
                if g.len() == 1 && g[0] == (r_lo, r_hi) {
                    Gaps::Whole(r_lo, r_hi)
                } else {
                    Gaps::Spans(g)
                }
            };
            if full_page {
                self.set(idx, now);
                steps.push(Step::Page {
                    page,
                    prev,
                    sub,
                    gaps,
                });
                continue;
            }
            // Cut by the request's ends: planned here, as the one-piece plan does.
            let mask = page_mask(page, prev, sub);
            let spans = match gaps {
                Gaps::Whole(lo, hi) => vec![(lo, hi)],
                Gaps::Spans(g) => g,
            };
            if let Some(bits) = mask {
                steps.push(Step::Flush);
                for (sub_lo, sub_hi) in sub_runs(bits) {
                    for &(g_lo, g_hi) in &spans {
                        let lo = g_lo.max(sub_lo);
                        let hi = g_hi.min(sub_hi);
                        if lo < hi {
                            steps
                                .push(Step::Push(page_base + lo as usize, page_base + hi as usize));
                            insert_partial_span(self.partial.entry(idx).or_default(), lo, hi, now);
                        }
                    }
                }
            } else {
                if spans.len() == 1 && spans[0] == (r_lo, r_hi) {
                    steps.push(Step::Extend(page));
                } else {
                    steps.push(Step::Flush);
                    for &(g_lo, g_hi) in &spans {
                        steps.push(Step::Push(
                            page_base + g_lo as usize,
                            page_base + g_hi as usize,
                        ));
                    }
                }
                let partial = self.partial.entry(idx).or_default();
                for (g_lo, g_hi) in spans {
                    insert_partial_span(partial, g_lo, g_hi, now);
                }
            }
        }
        Claimed { steps, from, to }
    }
}

/// The runs of set bits of a 4 KiB mask, as byte offsets in the page.
fn sub_runs(bits: u16) -> impl Iterator<Item = (u32, u32)> {
    let mut k = 0;
    std::iter::from_fn(move || {
        while k < PAGE / KERNEL_PAGE && bits & (1 << k) == 0 {
            k += 1;
        }
        if k >= PAGE / KERNEL_PAGE {
            return None;
        }
        let a = k;
        while k < PAGE / KERNEL_PAGE && bits & (1 << k) != 0 {
            k += 1;
        }
        Some(((a * KERNEL_PAGE) as u32, (k * KERNEL_PAGE) as u32))
    })
}

impl Claimed {
    /// The byte runs, the masks of pages taken whole asked for now (no lock).
    pub(super) fn finish(
        self,
        page_mask: &mut impl FnMut(usize, u64, Option<(u64, u16)>) -> Option<u16>,
    ) -> Vec<(usize, usize)> {
        let (mut run, mut bytes_runs) = (None::<(usize, usize)>, Vec::new());
        for step in self.steps {
            match step {
                Step::Flush => bytes_runs.extend(run.take()),
                Step::Push(a, b) => bytes_runs.push((a, b)),
                Step::Extend(page) => extend_runs(&mut run, &mut bytes_runs, page, u16::MAX),
                Step::Page {
                    page,
                    prev,
                    sub,
                    gaps,
                } => {
                    let page_base = page * PAGE;
                    if let Some(bits) = page_mask(page, prev, sub) {
                        bytes_runs.extend(run.take());
                        let spans: &[(u32, u32)] = match &gaps {
                            Gaps::Whole(lo, hi) => &[(*lo, *hi)],
                            Gaps::Spans(g) => g,
                        };
                        for (sub_lo, sub_hi) in sub_runs(bits) {
                            for &(g_lo, g_hi) in spans {
                                let lo = g_lo.max(sub_lo);
                                let hi = g_hi.min(sub_hi);
                                if lo < hi {
                                    bytes_runs
                                        .push((page_base + lo as usize, page_base + hi as usize));
                                }
                            }
                        }
                    } else {
                        match gaps {
                            Gaps::Whole(..) => {
                                extend_runs(&mut run, &mut bytes_runs, page, u16::MAX)
                            }
                            Gaps::Spans(g) => {
                                bytes_runs.extend(run.take());
                                for (g_lo, g_hi) in g {
                                    bytes_runs.push((
                                        page_base + g_lo as usize,
                                        page_base + g_hi as usize,
                                    ));
                                }
                            }
                        }
                    }
                }
            }
        }
        bytes_runs.extend(run);
        let (from, to) = (self.from, self.to);
        bytes_runs
            .into_iter()
            .filter_map(|(a, b)| {
                let (a, b) = (a.max(from), b.min(to));
                (a < b).then_some((a, b))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A small deterministic generator (xorshift).
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

    /// A mask that depends on the page, the last copy and the sub-page
    /// answer only, as the real ones do on stamps that hold still.
    fn mask(page: usize, prev: u64, sub: Option<(u64, u16)>) -> Option<u16> {
        let h = (page as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ prev.rotate_left(17);
        match (h % 5, sub) {
            (0, _) => None,
            (1, Some((_, bits))) => (bits != 0).then_some(bits),
            (2, _) => Some(1 << (h % 16)),
            _ => Some((h >> 8) as u16).filter(|&m| m != 0),
        }
    }

    fn state(c: &CopiedPages) -> (Vec<u64>, Vec<(usize, Vec<(u32, u32, u64)>)>, usize) {
        let mut partial: Vec<_> = c.partial.iter().map(|(&k, v)| (k, v.clone())).collect();
        partial.sort_unstable();
        (c.at.clone(), partial, c.uncopied)
    }

    #[test]
    fn the_split_plan_gives_the_same_runs_and_state_as_the_one_piece_plan() {
        const PAGES: usize = 40;
        let base = 1000;
        for seed in 1..60u64 {
            let mut rng = Rng(seed.wrapping_mul(0x2545_f491_4f6c_dd1d));
            // A buffer that need not start or end on a page.
            let start = base * PAGE + rng.below(3) as usize * 4096 + rng.below(2) as usize * 100;
            let end =
                (base + PAGES) * PAGE - rng.below(3) as usize * 4096 - rng.below(2) as usize * 52;
            let (mut one, mut split) = (
                CopiedPages::with_base(PAGES, base),
                CopiedPages::with_base(PAGES, base),
            );
            for now in 1..40u64 {
                let a = start + rng.below((end - start) as u64) as usize;
                let b = (a + 1 + rng.below(9 * PAGE as u64) as usize).min(end);
                let (a, b) = if rng.below(3) == 0 {
                    (a / PAGE * PAGE, b.div_ceil(PAGE) * PAGE)
                } else {
                    (a, b)
                };
                let mut written = Vec::new();
                let mut subs = Vec::new();
                for page in base..base + PAGES {
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
                let skip = rng.next();
                let skip_idx = |idx: usize| skip >> (idx % 64) & 1 == 1 && skip % 7 == 0;
                let (mut asked_one, mut asked_split) = (Vec::new(), Vec::new());
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
                    skip_idx,
                );
                let mut m = |p, prev, s| {
                    asked_split.push((p, prev, s));
                    mask(p, prev, s)
                };
                let claimed = split.plan_stale_subpages_claim(
                    base, start, end, a, b, now, &written, &subs, &mut m, skip_idx,
                );
                assert_eq!(
                    state(&one),
                    state(&split),
                    "seed {seed} now {now}: state after the lock"
                );
                let got = claimed.finish(&mut m);
                assert_eq!(want, got, "seed {seed} now {now}: [{a:#x}, {b:#x})");
                asked_one.sort_unstable();
                asked_split.sort_unstable();
                assert_eq!(asked_one, asked_split, "the same masks asked for");
            }
        }
    }

    #[test]
    fn sub_runs_are_the_runs_of_set_bits() {
        assert_eq!(sub_runs(0).collect::<Vec<_>>(), vec![]);
        assert_eq!(sub_runs(0xffff).collect::<Vec<_>>(), vec![(0, 65536)]);
        assert_eq!(
            sub_runs(0b1000_0000_0110_0001).collect::<Vec<_>>(),
            vec![(0, 4096), (5 * 4096, 7 * 4096), (15 * 4096, 16 * 4096)]
        );
    }

    #[test]
    fn the_switch_is_live() {
        assert_eq!(
            on(),
            std::env::var_os("VICTORY_PLAN_SPLIT").is_some_and(|v| v == "1")
        );
        if std::env::var_os("VICTORY_PLAN_SPLIT").is_some() {
            return;
        }
        victory_core::settings::set("VICTORY_PLAN_SPLIT", "1").unwrap();
        let now = on();
        victory_core::settings::set("VICTORY_PLAN_SPLIT", "").unwrap();
        assert!(now, "a change takes effect while running");
        assert!(!on());
    }
    /// (#88) Sizes the switch: 16 threads plan 1 MiB pieces of one 256 MiB
    /// copy behind its lock, a quarter of the pages stale, with a mask that
    /// costs what `watch::stale_mask` and `note_4k` cost (sixteen loads from a
    /// shared table, two adds to shared counters). Ignored; `--ignored
    /// --nocapture`, release for real numbers.
    #[test]
    #[ignore]
    fn bench_plan_under_the_lock() {
        use std::sync::Mutex;
        use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
        const PAGES: usize = 4096;
        const THREADS: usize = 16;
        const ROUNDS: u64 = 40;
        let stamps: Vec<AtomicU64> = (0..PAGES * 16).map(|i| AtomicU64::new(i as u64 % 7)).collect();
        let counters = [AtomicU64::new(0), AtomicU64::new(0)];
        let real_mask = |page: usize, prev: u64, _sub: Option<(u64, u16)>| -> Option<u16> {
            let m = (0..16).fold(0u16, |m, k| {
                m | (u16::from(stamps[(page % PAGES) * 16 + k].load(Relaxed) > prev % 7) << k)
            });
            counters[0].fetch_add(u64::from(m.count_ones()), Relaxed);
            counters[1].fetch_add(1, Relaxed);
            (m != 0).then_some(m)
        };
        for split in [false, true, false, true] {
            let copied = Mutex::new(CopiedPages::with_base(PAGES, 0));
            let (start, end) = (0, PAGES * PAGE);
            let (in_lock, total) = (AtomicU64::new(0), AtomicU64::new(0));
            let wall = std::time::Instant::now();
            for now in 1..=ROUNDS {
                let written: Vec<(usize, u64)> =
                    (0..PAGES).filter(|p| (p * 7 + now as usize) % 4 == 0).map(|p| (p, now)).collect();
                let pieces: Vec<(usize, usize)> =
                    (0..PAGES / 16).map(|i| (i * 16 * PAGE, (i + 1) * 16 * PAGE)).collect();
                let next = std::sync::atomic::AtomicUsize::new(0);
                std::thread::scope(|sc| {
                    for _ in 0..THREADS {
                        sc.spawn(|| {
                            let mut m = real_mask;
                            loop {
                                let i = next.fetch_add(1, Relaxed);
                                let Some(&(a, b)) = pieces.get(i) else { break };
                                let t = std::time::Instant::now();
                                let runs = if split {
                                    let claimed = {
                                        let mut c = copied.lock().unwrap();
                                        c.plan_stale_subpages_claim(
                                            0, start, end, a, b, now, &written, &[], &mut m, |_| false,
                                        )
                                    };
                                    in_lock.fetch_add(t.elapsed().as_nanos() as u64, Relaxed);
                                    claimed.finish(&mut m)
                                } else {
                                    let runs = {
                                        let mut c = copied.lock().unwrap();
                                        c.plan_stale_subpages(
                                            0, start, end, a, b, now, &written, &[], &mut m, |_| false,
                                        )
                                    };
                                    in_lock.fetch_add(t.elapsed().as_nanos() as u64, Relaxed);
                                    runs
                                };
                                total.fetch_add(t.elapsed().as_nanos() as u64, Relaxed);
                                std::hint::black_box(runs);
                            }
                        });
                    }
                });
            }
            let plans = (PAGES / 16) as f64 * ROUNDS as f64;
            println!(
                "plan of 1 MiB pieces on {THREADS} threads, split={split}: up to the lock's release \
                 {:.2} us a plan, all of it {:.2} us a plan, wall {:.1} ms for {plans} plans",
                in_lock.into_inner() as f64 / 1e3 / plans,
                total.into_inner() as f64 / 1e3 / plans,
                wall.elapsed().as_secs_f64() * 1e3
            );
        }
    }
}
