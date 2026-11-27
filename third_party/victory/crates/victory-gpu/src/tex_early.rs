//! Texture checks started at their `Step::Texture` (`VICTORY_TEX_EARLY=1`,
//! off by default; needs `VICTORY_PARALLEL_REFRESH` on, its default).
//!
//! Without the switch a submission's due checks (`Staging::refresh`) wait in
//! `Batch::pending_textures` until the next point that needs them, and run
//! side by side there (`crate::tex_check`): 29 such joins a frame, 3.1 ms of
//! the submitting thread, each mostly waiting for one texture (its hash turn
//! comes every eighth frame).
//!
//! With the switch a due check is handed to a few threads of its own as soon
//! as its step is walked, and the submitting thread goes on with the steps
//! after it. It waits for the checks at exactly the points that waited for
//! them before (`Batch::flush_textures`): before a step that writes title
//! memory (a copy into it, `Step::Run`), before a host callback (it may use a
//! refresh result), before the preludes of the lists so far are made (they
//! read what the checks found), and at the end of every flush, the
//! submission's last one among them. Between the start and the wait the
//! submitting thread only queues refreshes and fills, and runs flushes of
//! host copies and fills, which do not read or write a staging buffer; the
//! order of the checks against those was not fixed before either (a title
//! write checks textures before the refreshes, the last flush after them).
//! At the wait it runs the checks no thread has taken yet itself.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::Instant;

type Job = Box<dyn FnOnce() + Send>;

/// Threads for the checks: `VICTORY_TEX_EARLY=1` four, `=N` (2-16) N, unset
/// or `0` none (off).
fn threads_from(value: Option<&str>) -> usize {
    match value.map(str::trim).and_then(|v| v.parse::<usize>().ok()) {
        Some(0) | None => 0,
        Some(1) => 4,
        Some(n) => n.min(16),
    }
}

fn threads() -> usize {
    static N: OnceLock<usize> = OnceLock::new();
    *N.get_or_init(|| threads_from(std::env::var("VICTORY_TEX_EARLY").ok().as_deref()))
}

pub(crate) fn on() -> bool {
    threads() > 0
}

/// One submission's checks not joined yet.
struct Group {
    state: Mutex<State>,
    done: Condvar,
    /// Microseconds the checks took, on whichever thread ran them.
    thread_us: AtomicU64,
}

struct State {
    /// Checks no thread has taken yet.
    queued: VecDeque<Job>,
    /// Checks queued or running.
    open: usize,
}

impl Group {
    fn run(&self, job: Job) {
        let started = Instant::now();
        job();
        self.thread_us
            .fetch_add(started.elapsed().as_micros() as u64, Ordering::Relaxed);
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.open -= 1;
        if state.open == 0 {
            self.done.notify_all();
        }
    }

    /// Takes one queued check, if any is left.
    fn take(&self) -> Option<Job> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .queued
            .pop_front()
    }
}

/// The checks' threads: each ticket is one check of its group to take (or
/// nothing, if the joining thread took it first).
struct Pool {
    tickets: Mutex<VecDeque<Arc<Group>>>,
    more: Condvar,
}

/// The game's pool, of [`threads`] threads.
fn pool() -> &'static Pool {
    static POOL: OnceLock<&'static Pool> = OnceLock::new();
    POOL.get_or_init(|| new_pool(threads().max(1)))
}

/// A pool of `threads` threads that live as long as the process.
fn new_pool(threads: usize) -> &'static Pool {
    {
        let pool: &'static Pool = Box::leak(Box::new(Pool {
            tickets: Mutex::new(VecDeque::new()),
            more: Condvar::new(),
        }));
        for i in 0..threads {
            std::thread::Builder::new()
                .name(format!("victory-tex-early-{i}"))
                .spawn(move || {
                    loop {
                        let group = {
                            let mut tickets =
                                pool.tickets.lock().unwrap_or_else(|e| e.into_inner());
                            loop {
                                if let Some(g) = tickets.pop_front() {
                                    break g;
                                }
                                tickets =
                                    pool.more.wait(tickets).unwrap_or_else(|e| e.into_inner());
                            }
                        };
                        if let Some(job) = group.take() {
                            group.run(job);
                        }
                    }
                })
                .expect("a texture check thread");
        }
        pool
    }
}

/// What a join found.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Joined {
    pub(crate) checks: usize,
    /// The submitting thread's time at the join, the checks it ran itself in it.
    pub(crate) wall_us: u64,
    pub(crate) inline_us: u64,
    /// The checks' time on every thread.
    pub(crate) thread_us: u64,
}

/// A submission's checks started and not joined yet (part of `Batch`).
#[derive(Default)]
pub(crate) struct Early {
    group: Option<Arc<Group>>,
    checks: usize,
}

impl Early {
    /// Starts `job` on the checks' threads.
    pub(crate) fn start(&mut self, job: impl FnOnce() + Send + 'static) {
        self.start_on(pool(), Box::new(job));
    }

    fn start_on(&mut self, pool: &'static Pool, job: Job) {
        let group = self.group.get_or_insert_with(|| {
            Arc::new(Group {
                state: Mutex::new(State {
                    queued: VecDeque::new(),
                    open: 0,
                }),
                done: Condvar::new(),
                thread_us: AtomicU64::new(0),
            })
        });
        {
            let mut state = group.state.lock().unwrap_or_else(|e| e.into_inner());
            state.queued.push_back(job);
            state.open += 1;
        }
        self.checks += 1;
        pool.tickets
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push_back(group.clone());
        pool.more.notify_one();
    }

    /// Whether checks were started since the last join.
    pub(crate) fn started(&self) -> bool {
        self.group.is_some()
    }

    /// Returns once every check started since the last join is done, running
    /// those no thread has taken yet on this one.
    pub(crate) fn join(&mut self) -> Option<Joined> {
        let group = self.group.take()?;
        let checks = std::mem::take(&mut self.checks);
        let started = Instant::now();
        let mut inline_us = 0;
        while let Some(job) = group.take() {
            let at = Instant::now();
            group.run(job);
            inline_us += at.elapsed().as_micros() as u64;
        }
        let mut state = group.state.lock().unwrap_or_else(|e| e.into_inner());
        while state.open > 0 {
            state = group.done.wait(state).unwrap_or_else(|e| e.into_inner());
        }
        drop(state);
        Some(Joined {
            checks,
            wall_us: started.elapsed().as_micros() as u64,
            inline_us,
            thread_us: group.thread_us.load(Ordering::Relaxed),
        })
    }
}

impl Drop for Early {
    fn drop(&mut self) {
        if let Some(joined) = self.join() {
            account(joined);
        }
    }
}

/// Since the last frame line: joins, checks, the submitting thread's
/// microseconds at them, of that running checks itself, the checks' time on
/// every thread (with the switch).
static STATS: [AtomicU64; 5] = [const { AtomicU64::new(0) }; 5];

/// A join of started checks: counted, and the frame line's "textures"
/// (`PREPARING[2]`, which each check adds its own time to) brought to the
/// submitting thread's time, as `tex_check` does for its side-by-side run.
pub(crate) fn account(joined: Joined) {
    use Ordering::Relaxed;
    for (i, v) in [
        1,
        joined.checks as u64,
        joined.wall_us,
        joined.inline_us,
        joined.thread_us,
    ]
    .into_iter()
    .enumerate()
    {
        STATS[i].fetch_add(v, Relaxed);
    }
    let excess = joined.thread_us.saturating_sub(joined.wall_us);
    if excess > 0 {
        let _ = crate::host::PREPARING[2]
            .fetch_update(Relaxed, Relaxed, |cur| Some(cur.saturating_sub(excess)));
    }
}

/// Without the switch: the side-by-side run of `checks` checks at a join
/// took the submitting thread `wall_us`.
pub(crate) fn waited_today(checks: usize, wall_us: u64) {
    use Ordering::Relaxed;
    STATS[0].fetch_add(1, Relaxed);
    STATS[1].fetch_add(checks as u64, Relaxed);
    STATS[2].fetch_add(wall_us, Relaxed);
}

/// The frame line's, in both settings: compare "at the joins" ON vs OFF.
pub(crate) fn log(frames: f64) {
    let [joins, checks, wall, inline, thread] =
        [0, 1, 2, 3, 4].map(|i| STATS[i].swap(0, Ordering::Relaxed) as f64 / frames);
    if joins == 0.0 {
        return;
    }
    if on() {
        tracing::info!(
            "texture check joins per frame (VICTORY_TEX_EARLY=1, {} threads): {joins:.1} joins, {checks:.0} textures; \
             the submitting thread at the joins {:.2} ms (running checks itself {:.2} ms), the checks {:.2} ms on all threads",
            threads(),
            wall / 1e3,
            inline / 1e3,
            thread / 1e3
        );
    } else {
        tracing::info!(
            "texture check joins per frame (VICTORY_TEX_EARLY=0): {joins:.1} joins, {checks:.0} textures; \
             the submitting thread at the joins {:.2} ms (the checks run side by side there)",
            wall / 1e3
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize};
    use std::time::Duration;

    fn test_pool() -> &'static Pool {
        static POOL: OnceLock<&'static Pool> = OnceLock::new();
        POOL.get_or_init(|| new_pool(4))
    }

    #[test]
    fn the_switch_reads_threads() {
        assert_eq!(threads_from(None), 0);
        assert_eq!(threads_from(Some("0")), 0);
        assert_eq!(threads_from(Some("1")), 4);
        assert_eq!(threads_from(Some(" 6 ")), 6);
        assert_eq!(threads_from(Some("99")), 16);
        assert_eq!(threads_from(Some("yes")), 0);
    }

    #[test]
    fn a_join_returns_after_every_check_started_before_it_is_done() {
        for _ in 0..50 {
            let mut early = Early::default();
            let done = Arc::new(AtomicUsize::new(0));
            for i in 0..9 {
                let done = done.clone();
                early.start_on(test_pool(),
                    Box::new(move || {
                        if i % 3 == 0 {
                            std::thread::sleep(Duration::from_micros(300));
                        }
                        done.fetch_add(1, Ordering::SeqCst);
                    }),
                );
            }
            let joined = early.join().expect("checks were started");
            assert_eq!(done.load(Ordering::SeqCst), 9);
            assert_eq!(joined.checks, 9);
            assert!(!early.started());
            assert_eq!(early.join(), None);
        }
    }

    #[test]
    fn checks_started_after_a_join_belong_to_the_next_join() {
        let mut early = Early::default();
        let first = Arc::new(AtomicBool::new(false));
        let f = first.clone();
        early.start_on(test_pool(), Box::new(move || f.store(true, Ordering::SeqCst)));
        assert_eq!(early.join().map(|j| j.checks), Some(1));
        assert!(first.load(Ordering::SeqCst));
        let second = Arc::new(AtomicBool::new(false));
        let s = second.clone();
        early.start_on(test_pool(),
            Box::new(move || {
                std::thread::sleep(Duration::from_millis(2));
                s.store(true, Ordering::SeqCst);
            }),
        );
        assert!(early.started());
        assert_eq!(early.join().map(|j| j.checks), Some(1));
        assert!(second.load(Ordering::SeqCst));
    }

    #[test]
    fn the_joining_thread_runs_the_checks_no_thread_took() {
        // Hold every pool thread in a check of another submission; the
        // join must not wait for them to come free.
        let pool = new_pool(2);
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let mut blockers = Early::default();
        let held = Arc::new(AtomicUsize::new(0));
        for _ in 0..16 {
            let (release, held) = (release.clone(), held.clone());
            blockers.start_on(pool,
                Box::new(move || {
                    held.fetch_add(1, Ordering::SeqCst);
                    let (lock, cv) = &*release;
                    let mut go = lock.lock().unwrap();
                    while !*go {
                        go = cv.wait(go).unwrap();
                    }
                }),
            );
        }
        while held.load(Ordering::SeqCst) < 2 {
            std::thread::yield_now();
        }
        let mut early = Early::default();
        let me = std::thread::current().id();
        let ran_here = Arc::new(AtomicUsize::new(0));
        for _ in 0..3 {
            let ran_here = ran_here.clone();
            early.start_on(pool,
                Box::new(move || {
                    if std::thread::current().id() == me {
                        ran_here.fetch_add(1, Ordering::SeqCst);
                    }
                }),
            );
        }
        let joined = early.join().unwrap();
        assert_eq!(ran_here.load(Ordering::SeqCst), 3);
        assert_eq!(joined.checks, 3);
        let (lock, cv) = &*release;
        *lock.lock().unwrap() = true;
        cv.notify_all();
        // The other submission's join runs what is left of it here.
        assert_eq!(blockers.join().map(|j| j.checks), Some(16));
        assert_eq!(held.load(Ordering::SeqCst), 16);
    }

    #[test]
    fn dropping_waits_for_the_started_checks() {
        let done = Arc::new(AtomicBool::new(false));
        {
            let mut early = Early::default();
            let d = done.clone();
            early.start_on(test_pool(),
                Box::new(move || {
                    std::thread::sleep(Duration::from_millis(3));
                    d.store(true, Ordering::SeqCst);
                }),
            );
        }
        assert!(done.load(Ordering::SeqCst));
    }

    #[test]
    fn a_check_runs_after_its_start_and_its_effects_are_seen_after_the_join() {
        // The order a walk needs: what a check writes is there for whatever
        // follows the join, and nothing the walk writes after the join is
        // seen by a check started before it.
        let cell = Arc::new(Mutex::new(Vec::new()));
        let mut early = Early::default();
        for round in 0..20u32 {
            for i in 0..5u32 {
                let cell = cell.clone();
                early.start_on(test_pool(), Box::new(move || cell.lock().unwrap().push((round, i))));
            }
            early.join().unwrap();
            let seen = std::mem::take(&mut *cell.lock().unwrap());
            let mut seen: Vec<_> = seen;
            seen.sort_unstable();
            assert_eq!(seen, (0..5).map(|i| (round, i)).collect::<Vec<_>>());
        }
    }
}
