//! (critpath, #88) `VICTORY_ASK_ASIDE=1` (Live, off by default): the submission's one
//! write-watch asking runs on a thread of its own while the submitting thread walks the lists'
//! steps (`run_deferred`).
//!
//! The asking (`crate::watch::ask_once`: the kernel's `GetWriteWatch` over every range the
//! submission refreshes) took 3.6-4.3 ms a frame on the submitting thread (part S/T: 29
//! submissions, ~300,000 pages walked, 210-255 kernel calls), with the card idle behind us 7.6-11
//! ms a frame. Most of what the walk does before its first flush only notes work for later
//! (refreshes, fills, shaders' writes, which textures are due): nothing that reads the answer. So
//! the walk goes on while the asking runs, and the asking is joined at a gate ([`Gate`]) before
//! the first step that needs it: a title buffer copy or a step of ours on title memory (each
//! flushes what came before it), a host step, the preludes made mid-walk, or at the walk's end at
//! the latest, before the flush. Due texture checks noted before the gate are held and started at
//! it (they ask about memory too).
//!
//! What moves: behind flights, the check of whether the title wrote memory a flight's shaders
//! write (`stale_under_flights`, which reads the answer) runs at the gate, not before the walk;
//! if it finds some, the flights are settled and the asking made again there, before any step
//! that reads title memory, as before. Nothing in the walk before the gate reads title memory or
//! the journal. Not in the cache key: no shader or pipeline output.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::Instant;

static ON: victory_core::settings::Live = victory_core::settings::Live::flag("VICTORY_ASK_ASIDE");

#[cfg(test)]
thread_local! {
    pub(crate) static FORCED: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

pub(super) fn on() -> bool {
    #[cfg(test)]
    if let Some(forced) = FORCED.with(std::cell::Cell::get) {
        return forced;
    }
    ON.get_bool()
}

type Job = Box<dyn FnOnce() + Send>;

struct State {
    job: Option<(u64, Job)>,
    /// Askings handed over, and done.
    started: u64,
    done: u64,
}

struct Worker {
    state: Mutex<State>,
    moved: Condvar,
}

impl Worker {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn spawn(name: &str) -> Option<&'static Self> {
        let worker: &'static Self = Box::leak(Box::new(Self {
            state: Mutex::new(State {
                job: None,
                started: 0,
                done: 0,
            }),
            moved: Condvar::new(),
        }));
        std::thread::Builder::new()
            .name(name.into())
            .spawn(move || {
                loop {
                    let (number, job) = {
                        let mut state = worker.lock();
                        loop {
                            if let Some(job) = state.job.take() {
                                break job;
                            }
                            state = worker.moved.wait(state).unwrap_or_else(|e| e.into_inner());
                        }
                    };
                    let started = Instant::now();
                    // A panic still marks the asking done: the submitting thread waits for it.
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job));
                    COUNTS[C::ThreadUs as usize]
                        .fetch_add(started.elapsed().as_micros() as u64, Relaxed);
                    worker.lock().done = number;
                    worker.moved.notify_all();
                }
            })
            .ok()?;
        Some(worker)
    }

    /// Hands `job` over (after the one before is done).
    fn start(&'static self, job: Job) -> Pending {
        let mut state = self.lock();
        while state.done < state.started {
            state = self.moved.wait(state).unwrap_or_else(|e| e.into_inner());
        }
        state.started += 1;
        let number = state.started;
        state.job = Some((number, job));
        drop(state);
        self.moved.notify_all();
        Pending {
            worker: self,
            number,
        }
    }
}

/// An asking handed to the thread, not joined yet.
pub(super) struct Pending {
    worker: &'static Worker,
    number: u64,
}

impl Pending {
    /// Waits until the asking is done; returns the microseconds waited.
    pub(super) fn join(self) -> u64 {
        let started = Instant::now();
        let mut state = self.worker.lock();
        while state.done < self.number {
            state = self
                .worker
                .moved
                .wait(state)
                .unwrap_or_else(|e| e.into_inner());
        }
        drop(state);
        let us = started.elapsed().as_micros() as u64;
        COUNTS[C::WaitedUs as usize].fetch_add(us, Relaxed);
        us
    }
}

/// Hands `job` (the asking) to `victory-ask-aside`; `None` if the thread could not be made.
pub(super) fn start(job: impl FnOnce() + Send + 'static) -> Option<Pending> {
    static WORKER: OnceLock<Option<&'static Worker>> = OnceLock::new();
    let worker = (*WORKER.get_or_init(|| Worker::spawn("victory-ask-aside")))?;
    COUNTS[C::Asks as usize].fetch_add(1, Relaxed);
    Some(worker.start(Box::new(job)))
}

/// Where the walk needs the asking: [`Gate::pass`] runs the join (and what goes with it) once.
pub(super) struct Gate<'a> {
    pass: Option<&'a mut dyn FnMut() -> bool>,
    /// The join found flights' memory written and settled them.
    pub(super) settled: bool,
}

impl<'a> Gate<'a> {
    /// No asking aside: nothing to wait for.
    pub(super) fn open() -> Self {
        Self {
            pass: None,
            settled: false,
        }
    }

    /// `pass` joins the asking and checks the flights; it returns whether it settled them.
    pub(super) fn new(pass: &'a mut dyn FnMut() -> bool) -> Self {
        Self {
            pass: Some(pass),
            settled: false,
        }
    }

    /// Whether the asking is still to be joined.
    pub(super) fn pending(&self) -> bool {
        self.pass.is_some()
    }

    /// Joins (once): whether it did now. `inside`: from within the walk.
    pub(super) fn pass(&mut self, inside: bool) -> bool {
        let Some(pass) = self.pass.take() else {
            return false;
        };
        COUNTS[if inside { C::Inside } else { C::AtEnd } as usize].fetch_add(1, Relaxed);
        self.settled |= pass();
        true
    }
}

#[derive(Clone, Copy)]
enum C {
    Asks,
    ThreadUs,
    WaitedUs,
    Inside,
    AtEnd,
    Held,
}
static COUNTS: [AtomicU64; 6] = [const { AtomicU64::new(0) }; 6];

/// A due texture check held until the gate.
pub(super) fn held() {
    COUNTS[C::Held as usize].fetch_add(1, Relaxed);
}

/// The line, per frame over `frames` frames (with the frame-rate line).
pub(super) fn line(frames: f64) {
    let [asks, thread_us, waited_us, inside, at_end, held] = [
        C::Asks,
        C::ThreadUs,
        C::WaitedUs,
        C::Inside,
        C::AtEnd,
        C::Held,
    ]
    .map(|c| COUNTS[c as usize].swap(0, Relaxed) as f64);
    if asks == 0.0 || frames <= 0.0 {
        return;
    }
    tracing::info!(
        "write-watch asking aside per frame (VICTORY_ASK_ASIDE=1): {:.1} askings, {:.2} ms on \
         their thread, {:.2} ms waited for at the gate ({:.2} ms hidden behind the walk); gates \
         passed inside the walk {:.1}, at its end {:.1}; {:.1} texture checks held until the gate",
        asks / frames,
        thread_us / 1000.0 / frames,
        waited_us / 1000.0 / frames,
        (thread_us - waited_us).max(0.0) / 1000.0 / frames,
        inside / frames,
        at_end / frames,
        held / frames,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    #[test]
    fn the_asking_runs_aside_and_is_joined() {
        let worker = Worker::spawn("victory-ask-aside-test").expect("spawn");
        let done = Arc::new(AtomicBool::new(false));
        let seen = done.clone();
        let pending = worker.start(Box::new(move || {
            std::thread::sleep(std::time::Duration::from_millis(20));
            seen.store(true, Relaxed);
        }));
        // The caller goes on meanwhile.
        assert!(!done.load(Relaxed));
        let waited = pending.join();
        assert!(done.load(Relaxed));
        assert!(waited > 0);
        // One after the other, each joined; a panicking asking is still marked done.
        for _ in 0..3 {
            worker.start(Box::new(|| panic!("asking failed"))).join();
        }
        let n = Arc::new(AtomicU64::new(0));
        let pendings: Vec<_> = (0..4)
            .map(|_| {
                let n = n.clone();
                worker.start(Box::new(move || {
                    n.fetch_add(1, Relaxed);
                }))
            })
            .collect();
        pendings.into_iter().for_each(|p| {
            p.join();
        });
        assert_eq!(n.load(Relaxed), 4);
    }

    #[test]
    fn the_gate_passes_once_and_keeps_its_settle() {
        let mut calls = 0;
        let mut pass = || {
            calls += 1;
            true
        };
        let mut gate = Gate::new(&mut pass);
        assert!(gate.pending());
        assert!(gate.pass(true));
        assert!(!gate.pending());
        assert!(!gate.pass(false));
        assert!(gate.settled);
        drop(gate);
        assert_eq!(calls, 1);
        let mut open = Gate::open();
        assert!(!open.pending() && !open.pass(false) && !open.settled);
    }

    #[test]
    fn the_switch_is_live() {
        let read = || u64::from(ON.get_bool());
        victory_core::settings::check_live("VICTORY_ASK_ASIDE", "1", read, 0, 1);
    }
}
