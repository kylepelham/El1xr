//! (feedcard, #88) How [`super::wide`] wakes its kept threads and waits for
//! them. Two switches, both Live and off by default:
//!
//! - `VICTORY_WIDE_WAKE_ONE=1`: wake one kept thread for each job beyond the
//!   one the calling thread takes first, all of them only when there are jobs
//!   for all. Today every call wakes every kept thread (16 with
//!   `VICTORY_WIDE_THREADS=16`), even for 2 jobs; those that find nothing go
//!   back to sleep, after a system call each on the calling thread and a turn
//!   on a core the others want. A thread busy meanwhile looks at the queue
//!   before it sleeps, and the calling thread takes every job nobody took, so
//!   no job waits for a thread. Same jobs, same threads. After
//!   readmet3xt-lab/foxhound `8c1fa5d` (there: "woken, not running yet"
//!   1.4-6.1 ms of the frame's chain, 2.4 % of the submitting thread's samples
//!   in the wake calls).
//! - `VICTORY_WAIT_SPIN=<microseconds>` (0: not at all; at most 2000): when the
//!   calling thread has run out of jobs, it waits awake that long at most for
//!   the helpers' last ones before it sleeps on the condvar. That thread is the
//!   one the frame waits for, and the last helper is more often than not
//!   microseconds from done; woken from a sleep, it is not running for 50 us
//!   and more. Only the one waiting thread spins, only while helpers still
//!   run, and for a bounded time. After trustno1-coder/VIctory-NVIDIA PR #125
//!   (1mosce).
//!
//! The frame line `kept threads woken per frame` prints in both settings so
//! the operator can compare the wakes ON vs OFF. Not in the cache key: no
//! shader or pipeline output.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

static WAKE_ONE: victory_core::settings::Live =
    victory_core::settings::Live::flag("VICTORY_WIDE_WAKE_ONE");

/// The most microseconds [`spin_for_last`] waits, whatever is asked.
const SPIN_MOST_US: u64 = 2000;

static WAIT_SPIN: victory_core::settings::Live =
    victory_core::settings::Live::new("VICTORY_WAIT_SPIN", spin_micros);

fn spin_micros(v: Option<&str>) -> u64 {
    v.and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(0)
        .min(SPIN_MOST_US)
}

/// Since the last line: calls, threads woken (all counted as `threads`),
/// spins, spins that saw the jobs done, microseconds spun.
static COUNTS: [AtomicU64; 5] = [const { AtomicU64::new(0) }; 5];

#[cfg(test)]
thread_local! {
    /// Tests: (wake one, spin microseconds) for calls made on this thread.
    static FORCED: std::cell::Cell<Option<(bool, u64)>> = const { std::cell::Cell::new(None) };
}

fn wake_one() -> bool {
    #[cfg(test)]
    if let Some((one, _)) = FORCED.with(std::cell::Cell::get) {
        return one;
    }
    WAKE_ONE.get_bool()
}

fn wait_spin() -> u64 {
    #[cfg(test)]
    if let Some((_, us)) = FORCED.with(std::cell::Cell::get) {
        return us;
    }
    WAIT_SPIN.get()
}

/// How many kept threads a call with `jobs` jobs wakes (`threads` means
/// all, by `notify_all`).
pub(super) fn woken(jobs: usize, threads: usize, one: bool) -> usize {
    if one {
        jobs.saturating_sub(1).min(threads)
    } else {
        threads
    }
}

/// Wakes the kept threads for a call of `jobs` jobs, the jobs queued.
pub(super) fn wake(more: &std::sync::Condvar, jobs: usize, threads: usize) {
    let n = woken(jobs, threads, wake_one());
    COUNTS[0].fetch_add(1, Ordering::Relaxed);
    COUNTS[1].fetch_add(n as u64, Ordering::Relaxed);
    if n >= threads {
        more.notify_all();
    } else {
        (0..n).for_each(|_| more.notify_one());
    }
}

/// With `VICTORY_WAIT_SPIN`: waits awake until `left` is 0, its microseconds
/// at most. The caller sleeps on its condvar after, as before, if it is not.
pub(super) fn spin_for_last(left: &AtomicUsize) {
    let most = wait_spin();
    if most == 0 || left.load(Ordering::Acquire) == 0 {
        return;
    }
    let began = std::time::Instant::now();
    let done = spin_for(most, || left.load(Ordering::Acquire) == 0);
    COUNTS[2].fetch_add(1, Ordering::Relaxed);
    COUNTS[3].fetch_add(u64::from(done), Ordering::Relaxed);
    COUNTS[4].fetch_add(began.elapsed().as_micros() as u64, Ordering::Relaxed);
}

/// Spins until `there`, `most` microseconds at most; whether it got there.
fn spin_for(most: u64, there: impl Fn() -> bool) -> bool {
    let began = std::time::Instant::now();
    loop {
        if there() {
            return true;
        }
        if began.elapsed().as_micros() as u64 >= most {
            return false;
        }
        for _ in 0..16 {
            std::hint::spin_loop();
        }
    }
}

/// With the batch flush's lines.
pub(crate) fn line(frames: f64) {
    let [calls, woke, spins, done, us] =
        [0, 1, 2, 3, 4].map(|i| COUNTS[i].swap(0, Ordering::Relaxed) as f64 / frames);
    if calls == 0.0 {
        return;
    }
    tracing::info!(
        "kept threads woken per frame (VICTORY_WIDE_WAKE_ONE={}, VICTORY_WAIT_SPIN={}): {woke:.0} for {calls:.0} spread calls; \
         the caller spun for the last helper {spins:.0} times, saw it done {done:.0}, {:.2} ms spinning",
        u8::from(WAKE_ONE.get_bool()),
        WAIT_SPIN.get(),
        us / 1000.0
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn off_wakes_every_kept_thread() {
        for jobs in [2, 3, 16, 40] {
            assert_eq!(woken(jobs, 16, false), 16);
        }
    }

    #[test]
    fn one_wakes_a_thread_per_job_past_the_callers_first() {
        assert_eq!(woken(2, 16, true), 1);
        assert_eq!(woken(5, 16, true), 4);
        assert_eq!(woken(17, 16, true), 16, "jobs for all: all");
        assert_eq!(woken(40, 16, true), 16);
    }

    #[test]
    fn the_spin_is_bounded_and_parsed_with_a_cap() {
        assert_eq!(spin_micros(None), 0);
        assert_eq!(spin_micros(Some("50")), 50);
        assert_eq!(spin_micros(Some(" 9999 ")), SPIN_MOST_US);
        assert_eq!(spin_micros(Some("x")), 0);
        let began = std::time::Instant::now();
        assert!(!spin_for(200, || false), "never there");
        assert!(began.elapsed() >= std::time::Duration::from_micros(200));
        assert!(spin_for(1_000_000, || true), "there at once");
    }

    #[test]
    fn a_spin_returns_when_the_last_helper_is_done() {
        let left = std::sync::Arc::new(AtomicUsize::new(1));
        let other = left.clone();
        let helper = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(1));
            other.store(0, Ordering::Release);
        });
        assert!(spin_for(2_000_000, || left.load(Ordering::Acquire) == 0));
        helper.join().unwrap();
    }

    /// The real `wide()` with few jobs and with many, each switch either way:
    /// every job runs once (the counts of wakes do not matter here).
    #[test]
    fn wide_runs_every_job_once_with_few_or_many_jobs() {
        for jobs in [2usize, 3, 7, 64] {
            let ran = std::sync::Arc::new(AtomicUsize::new(0));
            let list: Vec<super::super::Job> = (0..jobs)
                .map(|_| {
                    let ran = ran.clone();
                    Box::new(move || {
                        ran.fetch_add(1, Ordering::Relaxed);
                    }) as super::super::Job
                })
                .collect();
            super::super::wide_run(list);
            assert_eq!(ran.load(Ordering::Relaxed), jobs);
        }
    }

    #[test]
    fn wide_runs_every_job_once_with_each_switch_either_way() {
        for forced in [(false, 0), (true, 0), (false, 500), (true, 500), (true, 2000)] {
            FORCED.with(|f| f.set(Some(forced)));
            for jobs in [2usize, 3, 7, 64] {
                for _ in 0..20 {
                    let ran = std::sync::Arc::new(AtomicUsize::new(0));
                    let list: Vec<super::super::Job> = (0..jobs)
                        .map(|i| {
                            let ran = ran.clone();
                            Box::new(move || {
                                if i % 3 == 1 {
                                    std::thread::sleep(std::time::Duration::from_micros(100));
                                }
                                ran.fetch_add(1, Ordering::Relaxed);
                            }) as super::super::Job
                        })
                        .collect();
                    super::super::wide_run(list);
                    assert_eq!(ran.load(Ordering::Relaxed), jobs, "{forced:?}, {jobs} jobs");
                }
            }
        }
        FORCED.with(|f| f.set(None));
    }

    /// Microbenchmark (release, `VICTORY_WIDE_THREADS=16`, `--ignored --nocapture`):
    /// wall per `wide()` call of J jobs of about 10 us each, each setting.
    #[test]
    #[ignore]
    fn bench_wide_wakes() {
        fn work(us: u64) {
            let began = std::time::Instant::now();
            while began.elapsed() < std::time::Duration::from_micros(us) {
                std::hint::spin_loop();
            }
        }
        for jobs in [2usize, 4, 17] {
            for forced in [(false, 0), (true, 0), (false, 200), (true, 200)] {
                FORCED.with(|f| f.set(Some(forced)));
                let mut took: Vec<u128> = (0..3000)
                    .map(|_| {
                        let list: Vec<super::super::Job> =
                            (0..jobs).map(|_| Box::new(|| work(10)) as super::super::Job).collect();
                        let began = std::time::Instant::now();
                        super::super::wide_run(list);
                        // A gap between calls, as between a submission's refreshes.
                        let took = began.elapsed().as_nanos();
                        work(50);
                        took
                    })
                    .collect();
                took.sort_unstable();
                let mean = took.iter().sum::<u128>() as f64 / took.len() as f64 / 1000.0;
                println!(
                    "{jobs:2} jobs, wake one {}, spin {:3} us: mean {mean:6.1} us, p50 {:6.1}, p99 {:6.1}",
                    forced.0,
                    forced.1,
                    took[took.len() / 2] as f64 / 1000.0,
                    took[took.len() * 99 / 100] as f64 / 1000.0
                );
            }
        }
        FORCED.with(|f| f.set(None));
    }
}
