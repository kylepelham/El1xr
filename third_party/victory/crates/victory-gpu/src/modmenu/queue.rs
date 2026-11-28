//! (El1xr) Native calls asked for by the menu and run later on a title thread: the menu's thread
//! queues them, the game-thread hook drains them, and each result is kept under its call's name
//! for the menu to show.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

/// Natives run back to back on the game thread, each fed what the earlier ones returned:
/// `call(hash, args)` runs one (`None`: not in the table). Gets the call's `args`.
pub type Script = fn(&mut dyn FnMut(u64, &[u64]) -> Option<[u64; 4]>, &[u64]) -> Option<[u64; 4]>;

/// One call: the native's hash and its argument slots, named for its result. With a `script`,
/// the script runs instead (and `hash` is its first native, for the logs).
#[derive(Clone, Debug)]
pub struct Call {
    pub name: &'static str,
    pub hash: u64,
    pub args: Vec<u64>,
    pub script: Option<Script>,
}

/// How a call went.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Done([u64; 4]),
    /// The hash is not in the table.
    Unknown,
}

/// The most calls waiting; more are refused (a stuck game thread must not grow it).
pub const CAP: usize = 64;

#[derive(Default)]
pub struct Queue {
    calls: Mutex<VecDeque<Call>>,
    waiting: AtomicUsize,
    results: Mutex<HashMap<&'static str, Outcome>>,
    ran: AtomicUsize,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Queue {
    /// Queues `call`; false when [`CAP`] calls already wait.
    pub fn push(&self, call: Call) -> bool {
        let mut calls = lock(&self.calls);
        if calls.len() >= CAP {
            return false;
        }
        calls.push_back(call);
        self.waiting.store(calls.len(), Relaxed);
        true
    }

    /// Whether nothing waits (one atomic load: the hook's cost with nothing to do).
    pub fn idle(&self) -> bool {
        self.waiting.load(Relaxed) == 0
    }

    /// Runs every waiting call in order through `run` (`None`: hash not in the table), outside the
    /// queue's lock, and keeps the results. Returns how many ran.
    pub fn drain(&self, mut run: impl FnMut(&Call) -> Option<[u64; 4]>) -> usize {
        let calls: Vec<Call> = {
            let mut q = lock(&self.calls);
            self.waiting.store(0, Relaxed);
            q.drain(..).collect()
        };
        for call in &calls {
            let outcome = run(call).map_or(Outcome::Unknown, Outcome::Done);
            lock(&self.results).insert(call.name, outcome);
        }
        self.ran.fetch_add(calls.len(), Relaxed);
        calls.len()
    }

    /// The last result of the call named `name`.
    pub fn result(&self, name: &str) -> Option<Outcome> {
        lock(&self.results).get(name).copied()
    }

    /// Calls run so far.
    pub fn ran(&self) -> usize {
        self.ran.load(Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(name: &'static str, hash: u64, args: &[u64]) -> Call {
        Call {
            name,
            hash,
            args: args.to_vec(),
            script: None,
        }
    }

    #[test]
    fn calls_run_in_order_on_the_draining_thread_and_results_are_kept() {
        let q = std::sync::Arc::new(Queue::default());
        assert!(q.idle());
        let menu = {
            let q = q.clone();
            std::thread::spawn(move || {
                assert!(q.push(call("a", 1, &[2, 3])));
                assert!(q.push(call("b", 9, &[])));
                assert!(q.push(call("a", 1, &[10, 20])));
            })
        };
        menu.join().unwrap();
        assert!(!q.idle());
        let me = std::thread::current().id();
        let mut order = Vec::new();
        let ran = q.drain(|c| {
            assert_eq!(std::thread::current().id(), me);
            order.push(c.args.clone());
            (c.hash == 1).then(|| [c.args[0] + c.args[1], 0, 0, 0])
        });
        assert_eq!(ran, 3);
        assert_eq!(order, vec![vec![2, 3], vec![], vec![10, 20]]);
        assert!(q.idle());
        assert_eq!(
            q.result("a"),
            Some(Outcome::Done([30, 0, 0, 0])),
            "the latest"
        );
        assert_eq!(q.result("b"), Some(Outcome::Unknown));
        assert_eq!(q.result("c"), None);
        assert_eq!(q.drain(|_| unreachable!()), 0);
        assert_eq!(q.ran(), 3);
    }

    #[test]
    fn a_full_queue_refuses_more() {
        let q = Queue::default();
        for _ in 0..CAP {
            assert!(q.push(call("x", 1, &[])));
        }
        assert!(!q.push(call("x", 1, &[])));
        q.drain(|_| None);
        assert!(q.push(call("x", 1, &[])));
    }
}
