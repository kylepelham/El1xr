//! A wide flush's refreshes in one run of the kept threads
//! (`VICTORY_FLUSH_ONE_PASS=1`, off by default).
//!
//! Without the switch a wide flush runs in passes (`flush::split_pass`): each
//! copy's steps up to its first long refresh, or that refresh's 1 MiB pieces,
//! then a barrier, then the next part of every copy. The world submissions
//! end with two passes a flush (59 a frame over 29 flushes), and each pass
//! waits for its slowest piece (5-16 ms) while the other threads stand idle,
//! the next part of the world heap among the work they could do.
//!
//! With the switch there is one run: when the last piece of a copy's long
//! refresh is done, or the steps before it, the thread that did it queues
//! the copy's next part at the front of the queue; the other copies go on
//! meanwhile. Each copy's steps still run in their order and none of them
//! before the one ahead of it is done; copies have nothing to do with one
//! another, as in the passes.

use super::{ShadowBatch, ShadowStep};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};

/// Live: read once per wide flush.
pub(super) static ON: victory_core::settings::Live =
    victory_core::settings::Live::new("VICTORY_FLUSH_ONE_PASS", |v| u64::from(v == Some("1")));

pub(super) fn on() -> bool {
    ON.get_bool()
}

/// A long refresh's pieces not done yet, and what of the copy follows them.
pub(super) struct Join<C> {
    left: AtomicUsize,
    rest: Mutex<Option<ShadowBatch<C>>>,
}

pub(super) enum Work<C> {
    /// Steps with no long refresh among them, then what of the copy follows.
    Steps(ShadowBatch<C>, Option<ShadowBatch<C>>),
    /// A piece of a long refresh.
    Piece(C, usize, usize, std::sync::Arc<Join<C>>),
}

/// Since the last frame line: wide flushes, their wall microseconds, parts
/// queued after a part before them was done (with the switch), pieces of
/// long refreshes, and runs of the kept threads (a pass each without the
/// switch, one a flush with it).
pub(super) static STATS: [AtomicU64; 5] = [const { AtomicU64::new(0) }; 5];
pub(super) const FLUSHES: usize = 0;
pub(super) const WALL_US: usize = 1;
pub(super) const CONTINUED: usize = 2;
pub(super) const PIECES: usize = 3;
pub(super) const RUNS: usize = 4;

/// The copy's work up to the end of its first long refresh (`min` bytes or
/// more), as [`Work`]: the steps before it with the rest to follow, or its
/// pieces of about `chunk` bytes ending on `page` boundaries.
pub(super) fn plan<C: Clone>(
    mut copy: ShadowBatch<C>,
    min: usize,
    split: impl Fn(usize, usize) -> Vec<(usize, usize)>,
    out: &mut Vec<Work<C>>,
) {
    let big = copy
        .steps
        .iter()
        .position(|s| matches!(*s, ShadowStep::Refresh(f, t) if t.saturating_sub(f) >= min));
    match big {
        None => out.push(Work::Steps(copy, None)),
        Some(0) => {
            let ShadowStep::Refresh(from, to) = copy.steps.remove(0) else {
                unreachable!("found above")
            };
            let parts = split(from, to);
            let rest = (!copy.steps.is_empty()).then(|| ShadowBatch {
                shadow: copy.shadow.clone(),
                refresh_span: None,
                steps: copy.steps,
            });
            let join = std::sync::Arc::new(Join {
                left: AtomicUsize::new(parts.len()),
                rest: Mutex::new(rest),
            });
            for (a, b) in parts {
                out.push(Work::Piece(copy.shadow.clone(), a, b, join.clone()));
            }
        }
        Some(pos) => {
            let rest = copy.steps.split_off(pos);
            out.push(Work::Steps(
                ShadowBatch {
                    shadow: copy.shadow.clone(),
                    refresh_span: None,
                    steps: copy.steps,
                },
                Some(ShadowBatch {
                    shadow: copy.shadow,
                    refresh_span: None,
                    steps: rest,
                }),
            ));
        }
    }
}

struct Queue<C> {
    work: VecDeque<Work<C>>,
    /// Work queued or running.
    open: usize,
}

/// The run's work, shared by the threads that take part.
pub(super) struct Run<C> {
    queue: Mutex<Queue<C>>,
    more: Condvar,
    min: usize,
    chunk: usize,
    page: usize,
}

impl<C: Clone> Run<C> {
    pub(super) fn new(copies: Vec<ShadowBatch<C>>, min: usize, chunk: usize, page: usize) -> Self {
        let run = Run {
            queue: Mutex::new(Queue {
                work: VecDeque::new(),
                open: 0,
            }),
            more: Condvar::new(),
            min,
            chunk,
            page,
        };
        let mut work = Vec::with_capacity(copies.len());
        for copy in copies {
            run.plan(copy, &mut work);
        }
        let mut q = run.queue.lock().unwrap_or_else(|e| e.into_inner());
        q.open = work.len();
        q.work.extend(work);
        drop(q);
        run
    }

    fn plan(&self, copy: ShadowBatch<C>, out: &mut Vec<Work<C>>) {
        let (chunk, page) = (self.chunk, self.page);
        plan(copy, self.min, |a, b| pieces(a, b, chunk, page), out);
        let pieces = out.iter().filter(|w| matches!(w, Work::Piece(..))).count();
        STATS[PIECES].fetch_add(pieces as u64, Ordering::Relaxed);
    }

    /// Takes and does work until there is none queued or running.
    /// `steps` does a copy's steps, `piece` a piece of a long refresh.
    pub(super) fn work(&self, steps: impl Fn(&ShadowBatch<C>), piece: impl Fn(&C, usize, usize)) {
        let mut q = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(work) = q.work.pop_front() {
                drop(q);
                let next = match work {
                    Work::Steps(copy, rest) => {
                        steps(&copy);
                        rest
                    }
                    Work::Piece(shadow, from, to, join) => {
                        piece(&shadow, from, to);
                        if join.left.fetch_sub(1, Ordering::AcqRel) == 1 {
                            join.rest.lock().unwrap_or_else(|e| e.into_inner()).take()
                        } else {
                            None
                        }
                    }
                };
                let mut queued = Vec::new();
                if let Some(rest) = next {
                    STATS[CONTINUED].fetch_add(1, Ordering::Relaxed);
                    self.plan(rest, &mut queued);
                }
                q = self.queue.lock().unwrap_or_else(|e| e.into_inner());
                let woken = queued.len();
                q.open += woken;
                for w in queued.into_iter().rev() {
                    q.work.push_front(w);
                }
                q.open -= 1;
                if q.open == 0 {
                    self.more.notify_all();
                } else if woken > 1 {
                    self.more.notify_all();
                } else if woken == 1 {
                    self.more.notify_one();
                }
                continue;
            }
            if q.open == 0 {
                return;
            }
            q = self.more.wait(q).unwrap_or_else(|e| e.into_inner());
        }
    }
}

/// `[from, to)` in pieces of about `chunk` bytes that end on `page`
/// boundaries (but the last), as `flush::pieces` cuts them.
fn pieces(from: usize, to: usize, chunk: usize, page: usize) -> Vec<(usize, usize)> {
    let mut out = Vec::with_capacity((to - from).div_ceil(chunk));
    let mut at = from;
    while at < to {
        let raw = at.saturating_add(chunk).min(to);
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

/// The frame line's, every frame line (both settings, for the A/B).
pub(super) fn line(frames: f64) {
    let [flushes, wall, continued, pieces, runs] = [FLUSHES, WALL_US, CONTINUED, PIECES, RUNS]
        .map(|i| STATS[i].swap(0, Ordering::Relaxed) as f64 / frames);
    if flushes == 0.0 {
        return;
    }
    tracing::info!(
        "wide flush refreshes per frame: {flushes:.1} flushes in {:.2} ms on the submitting thread, \
         {runs:.1} runs of the kept threads, {continued:.1} parts queued behind the part before them, \
         {pieces:.0} pieces of long refreshes (VICTORY_FLUSH_ONE_PASS={})",
        wall / 1000.0,
        u8::from(on())
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn the_switch_is_live() {
        // Unchanged, the switch reads what the environment said, as before.
        assert_eq!(on(), std::env::var_os("VICTORY_FLUSH_ONE_PASS").is_some_and(|v| v == "1"));
        if std::env::var_os("VICTORY_FLUSH_ONE_PASS").is_some() {
            return;
        }
        victory_core::settings::set("VICTORY_FLUSH_ONE_PASS", "1").unwrap();
        let now = on();
        victory_core::settings::set("VICTORY_FLUSH_ONE_PASS", "").unwrap();
        assert!(now, "a change takes effect while running");
        assert!(!on(), "and clears back to the launch value");
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Did {
        Refresh(usize, usize),
        Write(usize, usize),
    }

    fn batch(id: usize, steps: Vec<ShadowStep>) -> ShadowBatch<usize> {
        ShadowBatch {
            shadow: id,
            refresh_span: None,
            steps,
        }
    }

    /// Runs `copies` on `threads` threads; returns, per copy, what was done
    /// in the order it was done.
    fn run(copies: Vec<ShadowBatch<usize>>, threads: usize) -> Vec<Vec<Did>> {
        let n = copies.len();
        let log: Arc<Vec<Mutex<Vec<Did>>>> =
            Arc::new((0..n).map(|_| Mutex::new(Vec::new())).collect());
        let r = Run::new(copies, 16, 4, 2);
        std::thread::scope(|s| {
            for t in 0..threads {
                let (r, log) = (&r, &log);
                s.spawn(move || {
                    r.work(
                        |copy| {
                            for &step in &copy.steps {
                                let did = match step {
                                    ShadowStep::Refresh(a, b) => Did::Refresh(a, b),
                                    ShadowStep::Write(a, l) => Did::Write(a, l),
                                    ShadowStep::Barrier => continue,
                                };
                                log[copy.shadow].lock().unwrap().push(did);
                            }
                            if t % 2 == 0 {
                                std::thread::yield_now();
                            }
                        },
                        |&id, a, b| {
                            if (a / 4) % 3 == t % 3 {
                                std::thread::sleep(std::time::Duration::from_micros(200));
                            }
                            log[id].lock().unwrap().push(Did::Refresh(a, b));
                        },
                    );
                });
            }
        });
        Arc::try_unwrap(log)
            .unwrap()
            .into_iter()
            .map(|m| m.into_inner().unwrap())
            .collect()
    }

    #[test]
    fn a_copys_parts_run_in_order_and_all_its_pieces_before_what_follows() {
        use ShadowStep::{Barrier, Refresh, Write};
        let copies = vec![
            // A long refresh first, then a write, then another long one, then short work.
            batch(
                0,
                vec![
                    Refresh(0, 40),
                    Write(8, 4),
                    Barrier,
                    Refresh(40, 80),
                    Write(1, 1),
                    Refresh(0, 2),
                ],
            ),
            // Short work, then a long refresh, then a write.
            batch(
                1,
                vec![Refresh(0, 4), Write(0, 2), Refresh(100, 130), Write(5, 5)],
            ),
            // Nothing long.
            batch(2, vec![Refresh(0, 8), Write(3, 1), Refresh(8, 12)]),
            // Long refresh alone.
            batch(3, vec![Refresh(7, 33)]),
        ];
        for threads in [1, 2, 7] {
            for _ in 0..20 {
                let did = run(copies.clone_like(), threads);
                // Copy 0: pieces of [0, 40) in any order, then the write, then
                // pieces of [40, 80), then the write and the short refresh.
                let mut it = did[0].iter();
                let mut covered = Vec::new();
                for d in it.by_ref().take(10) {
                    let Did::Refresh(a, b) = *d else {
                        panic!("{did:?}")
                    };
                    covered.push((a, b));
                }
                covered.sort_unstable();
                assert_eq!(
                    covered,
                    (0..10).map(|i| (i * 4, i * 4 + 4)).collect::<Vec<_>>()
                );
                assert_eq!(it.next(), Some(&Did::Write(8, 4)));
                let mut covered: Vec<_> = it
                    .by_ref()
                    .take(10)
                    .map(|d| match *d {
                        Did::Refresh(a, b) => (a, b),
                        _ => panic!("{did:?}"),
                    })
                    .collect();
                covered.sort_unstable();
                assert_eq!(
                    covered,
                    (10..20).map(|i| (i * 4, i * 4 + 4)).collect::<Vec<_>>()
                );
                assert_eq!(
                    it.cloned().collect::<Vec<_>>(),
                    vec![Did::Write(1, 1), Did::Refresh(0, 2)]
                );
                // Copy 1: the short steps, then the pieces, then the write.
                assert_eq!(&did[1][..2], &[Did::Refresh(0, 4), Did::Write(0, 2)]);
                let mut covered: Vec<_> = did[1][2..did[1].len() - 1]
                    .iter()
                    .map(|d| match *d {
                        Did::Refresh(a, b) => (a, b),
                        _ => panic!("{did:?}"),
                    })
                    .collect();
                covered.sort_unstable();
                assert_eq!(covered.first().unwrap().0, 100);
                assert_eq!(covered.last().unwrap().1, 130);
                assert!(covered.windows(2).all(|w| w[0].1 == w[1].0));
                assert_eq!(did[1].last(), Some(&Did::Write(5, 5)));
                assert_eq!(
                    did[2],
                    vec![Did::Refresh(0, 8), Did::Write(3, 1), Did::Refresh(8, 12)]
                );
                // Copy 3: pieces end on page (2) boundaries and cover [7, 33).
                let mut covered: Vec<_> = did[3]
                    .iter()
                    .map(|d| match *d {
                        Did::Refresh(a, b) => (a, b),
                        _ => panic!("{did:?}"),
                    })
                    .collect();
                covered.sort_unstable();
                assert_eq!(covered.first().unwrap().0, 7);
                assert_eq!(covered.last().unwrap().1, 33);
                assert!(
                    covered
                        .windows(2)
                        .all(|w| w[0].1 == w[1].0 && w[0].1 % 2 == 0)
                );
            }
        }
    }

    #[test]
    fn an_empty_run_returns_at_once() {
        let r: Run<usize> = Run::new(Vec::new(), 16, 4, 2);
        r.work(|_| panic!("no work"), |_, _, _| panic!("no work"));
    }

    #[test]
    fn pieces_match_the_passes_cut() {
        for (from, to) in [
            (0, 5 << 20),
            (4096, (3 << 20) + 17),
            (65536 * 3 + 5, 9 << 20),
        ] {
            assert_eq!(
                pieces(from, to, super::super::flush::SPLIT_CHUNK, 65536),
                super::super::flush::pieces(from, to, 65536)
            );
        }
    }

    trait CloneLike {
        fn clone_like(&self) -> Self;
    }

    impl CloneLike for Vec<ShadowBatch<usize>> {
        fn clone_like(&self) -> Self {
            self.iter()
                .map(|c| ShadowBatch {
                    shadow: c.shadow,
                    refresh_span: c.refresh_span,
                    steps: c.steps.clone(),
                })
                .collect()
        }
    }
}
