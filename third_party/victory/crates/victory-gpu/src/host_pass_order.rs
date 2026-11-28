//! (feedcard, #88) `VICTORY_FLUSH_PASS_ORDER=1` (Live, off by default): the tasks of each pass of a
//! wide flush are handed out longest first, by what the same task (the same copy from the same
//! address) took last time, so a long one does not start last and keep the pass waiting.
//!
//! Why: the final flush's refreshes are the submitting thread's longest wait (4.1-4.7 ms a frame
//! behind `victory-flush-beside`, critpath r9). They run in about 51 passes a frame, each ending at
//! a barrier when its slowest thread is done, and their wall time is about 2.5 times their busy
//! time over the 17 threads (part Q: 6.2 ms wall for 42.8 ms busy; the longest task 1-2 ms, the
//! longest pass 2.9 ms). The kept threads take tasks in list order, and the list is in the order
//! the copies were noted, so a pass whose last task is its longest waits for that task alone.
//! Handing out the longest first (the classic longest-processing-time rule) leaves the short ones
//! to fill in at the end.
//!
//! Same output: the tasks of one pass are independent (each copy's steps in order are one task,
//! a long refresh's pieces are disjoint), and the threads already run them in any order; only the
//! order they are taken in changes. A task's cost is kept in a small table by a hash of its copy and
//! address; a task not found there counts as long (it runs early and is timed). Not in the cache key.

use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering::Relaxed;

static ON: victory_core::settings::Live =
    victory_core::settings::Live::flag("VICTORY_FLUSH_PASS_ORDER");

pub(super) fn on() -> bool {
    ON.get_bool()
}

const SLOTS: usize = 1 << 13;
/// Per slot: the key's high 32 bits, then the task's last time in microseconds.
static COST: [AtomicU64; SLOTS] = [const { AtomicU64::new(0) }; SLOTS];
/// What a task never timed counts as, in microseconds: before every timed one.
pub(super) const UNKNOWN: u64 = u32::MAX as u64;

/// Since the last line: passes ordered, their tasks of a known cost, and not known.
pub(super) static STATS: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

/// A task's key: its copy and the address it starts at.
pub(super) fn key(copy: u64, from: usize) -> u64 {
    let mut x = copy.wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ (from as u64).rotate_left(29);
    x ^= x >> 31;
    x = x.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    (x ^ (x >> 29)) | 1
}

/// What the task of `key` took last time, or [`UNKNOWN`].
pub(super) fn cost(key: u64) -> u64 {
    let v = COST[key as usize % SLOTS].load(Relaxed);
    if v >> 32 == key >> 32 && v != 0 {
        v & 0xffff_ffff
    } else {
        UNKNOWN
    }
}

/// The task of `key` took `us`.
pub(super) fn learn(key: u64, us: u64) {
    COST[key as usize % SLOTS].store((key >> 32) << 32 | us.min(UNKNOWN - 1), Relaxed);
}

/// `tasks` with their keys, longest (by [`cost`]) first; ties keep their order.
pub(super) fn longest_first<T>(
    tasks: Vec<T>,
    keys: Vec<u64>,
    cost: impl Fn(u64) -> u64,
) -> (Vec<T>, Vec<u64>) {
    let mut both: Vec<(u64, u64, T)> = keys
        .into_iter()
        .zip(tasks)
        .map(|(k, t)| (cost(k), k, t))
        .collect();
    let known = both.iter().filter(|(c, _, _)| *c != UNKNOWN).count() as u64;
    STATS[0].fetch_add(1, Relaxed);
    STATS[1].fetch_add(known, Relaxed);
    STATS[2].fetch_add(both.len() as u64 - known, Relaxed);
    both.sort_by(|a, b| b.0.cmp(&a.0));
    both.into_iter().map(|(_, k, t)| (t, k)).unzip()
}

/// The line, per frame, with the batch flush lines.
pub(super) fn line(frames: f64) {
    let [passes, known, unknown] = [0, 1, 2].map(|i| STATS[i].swap(0, Relaxed) as f64 / frames);
    if on() {
        tracing::info!(
            "wide flush passes handed out longest first per frame (VICTORY_FLUSH_PASS_ORDER=1): {passes:.1} passes, \
             {known:.0} tasks timed before, {unknown:.0} not"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn longest_first_keeps_every_task_and_puts_the_unknown_and_long_first() {
        let costs = [5u64, UNKNOWN, 300, 5, 2000, 40];
        let tasks: Vec<usize> = (0..costs.len()).collect();
        let keys: Vec<u64> = (0..costs.len() as u64).collect();
        let (t, k) = longest_first(tasks, keys, |k| costs[k as usize]);
        assert_eq!(t, vec![1, 4, 2, 5, 0, 3]);
        assert_eq!(k, vec![1, 4, 2, 5, 0, 3], "keys follow their tasks");
    }

    #[test]
    fn a_cost_is_learned_by_its_key_and_another_key_is_unknown() {
        let (a, b) = (key(17, 0x1000_0000), key(17, 0x1010_0000));
        assert_ne!(a, b);
        learn(a, 123);
        assert_eq!(cost(a), 123);
        assert!(cost(b) == UNKNOWN || b as usize % SLOTS == a as usize % SLOTS);
        learn(a, u64::MAX);
        assert_eq!(cost(a), UNKNOWN - 1, "never taken for unknown");
    }

    #[test]
    fn the_switch_is_live() {
        if std::env::var_os("VICTORY_FLUSH_PASS_ORDER").is_some() {
            return;
        }
        assert!(!on(), "off by default");
        victory_core::settings::set("VICTORY_FLUSH_PASS_ORDER", "1").unwrap();
        let now = on();
        victory_core::settings::set("VICTORY_FLUSH_PASS_ORDER", "").unwrap();
        assert!(now);
        assert!(!on());
    }

    /// (#88) A pass of 256 tasks, mostly short, a few long ones noted last (as copies come):
    /// wall time on the kept threads in list order and longest first, alternated.
    /// `cargo test --release -p victory-gpu --lib bench_pass_order -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn bench_pass_order() {
        use std::sync::Arc;
        fn spin(us: u64) {
            let t = std::time::Instant::now();
            while t.elapsed().as_micros() < u128::from(us) {
                std::hint::spin_loop();
            }
        }
        let mut costs: Vec<u64> = (0..256).map(|i| 4 + (i % 7) as u64 * 3).collect();
        for (i, c) in [(120, 300), (200, 450), (230, 600), (250, 500), (255, 700)] {
            costs[i] = c;
        }
        for round in 0..4 {
            for ordered in [false, true] {
                let tasks: Vec<u64> = costs.clone();
                let keys: Vec<u64> = (0..tasks.len() as u64).collect();
                let (tasks, _) = if ordered {
                    longest_first(tasks, keys, |k| costs[k as usize])
                } else {
                    (tasks, keys)
                };
                let tasks = Arc::new(tasks);
                let n = tasks.len();
                let t = std::time::Instant::now();
                for _ in 0..20 {
                    let tasks = tasks.clone();
                    crate::shadow::wide_chunks(n, 1, move |s, e| {
                        for &c in &tasks[s..e] {
                            spin(c);
                        }
                    });
                }
                eprintln!(
                    "round {round} ordered={ordered}: {:.0} us a pass on {} threads (busy {} us a pass)",
                    t.elapsed().as_secs_f64() * 1e6 / 20.0,
                    crate::shadow::wide_threads() + 1,
                    costs.iter().sum::<u64>()
                );
            }
        }
    }
}
