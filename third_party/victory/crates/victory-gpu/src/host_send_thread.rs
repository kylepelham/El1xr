//! (critpath, #88) `VICTORY_SEND_THREAD=1` (Live; off by default): a
//! submission sent ahead (`Host::submit` with `late`) used to end, on the
//! submitting thread, with ExecuteCommandLists of its lists, the compare's
//! call of its own (`crate::compare_apart`), the Signal of its fence value and
//! `Host::card_sent`: `exec_submit` on the preparing breakdown line, 1.9-2.2
//! ms a frame. With the switch the thread hands that tail to one thread of
//! ours, `victory-send`, and goes on to the next submission. Nothing is
//! recorded or read there: the lists are closed and the bytes they read were
//! taken before.
//!
//! The order rules, which keep the card seeing what it saw before:
//!
//! 1. The fence value is reserved by the submitting thread (`*submitted` was
//!    raised under the submission lock) and noted as submitted
//!    (`submitted_fence`, the copies' write fences) before the job is handed
//!    over, as it was noted before the Signal inline. The flight goes into the
//!    line from the submitting thread, as before. The value is not completed
//!    until this thread signals it, and every wait and every landing goes by
//!    completion, so no "sent" flag is kept.
//! 2. Jobs run in the order they were handed over, by the one thread.
//! 3. Everything else that puts work on the host queue calls [`wait_idle`]
//!    first, so it comes after every job handed over so far: the card's fetch
//!    of pieces (submitting thread and relay thread, the latter under
//!    `QUEUE_ORDER`, so the send count it reads is on the queue), submissions
//!    not sent ahead, the swap chain's Present, tile mappings, the lazy
//!    copy-back's fallback on the host queue, defrag's counter reads. With the
//!    switch off nothing is outstanding and the wait returns at once.
//! 4. The thread always signals: a job that panicked before its Signal is
//!    signalled after, so nothing that waits for the flight waits for good;
//!    then the job is booked as done (waiters wake), and only then the run
//!    is failed, as an error inline failed it.
//!
//! The thread takes none of the host's locks (submission, `QUEUE_ORDER`,
//! line, `LAZY_GPU`): callers of [`wait_idle`] may hold any of them. It takes
//! the card clock's lock in `card_sent`, which nobody holds while waiting.
//!
//! With `VICTORY_CARD_RECORD_SEND=1` a job also carries its plan's two card
//! lists unrecorded (`super::card_rec`): the thread records them first, then
//! sends (rule 2 keeps every other user of the queue behind both). A
//! recording that fails sends nothing of the job and signals its fence (rule
//! 4).
//!
//! After Kemerd/VIctory-NVIDIA-RT `host/send_thread.rs` (step 5a of its
//! `-Fast` set, as applied in TheUnrealZaka/VIctoryUnified-DLSS `aeeb2f72`).

use std::cell::Cell;
use std::collections::VecDeque;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::Instant;

use windows::Win32::Graphics::Direct3D12::ID3D12CommandList;

use super::Host;
use super::card_rec::Pending;

static ON: victory_core::settings::Live = victory_core::settings::Live::flag("VICTORY_SEND_THREAD");

/// The switch, read once per submission.
pub(super) fn on() -> bool {
    ON.get_bool()
}

/// What the thread runs: one submission's tail.
pub(super) trait Work: Send + 'static {
    /// The fence value it signals (to check the order jobs come in).
    fn fence(&self) -> u64;
    /// Sends and signals; `Err` with the run's failure when the signal (or
    /// what comes before it) failed.
    fn run(&mut self) -> Result<(), String>;
    /// After a panic in `run`: signals what it reserved if `run` did not.
    fn rescue(&mut self);
    /// Ends the run (after the job was booked as done).
    fn failed(what: &str);
}

struct State<W> {
    /// Handed over, not taken by the thread yet, in order.
    jobs: VecDeque<W>,
    /// Jobs handed over so far.
    handed: u64,
    /// Jobs the thread is finished with (sent, or failed and signalled).
    done: u64,
}

/// Since the last line: jobs, ns the thread spent in them, waits that found
/// it busy, ns of those waits, jobs out of order, jobs that failed.
const JOBS: usize = 0;
const BUSY_NS: usize = 1;
const WAITS: usize = 2;
const WAIT_NS: usize = 3;
const DISORDER: usize = 4;
const FAILED: usize = 5;

/// A queue of jobs and the one thread that runs them.
pub(super) struct Sender<W> {
    state: Mutex<State<W>>,
    /// The thread waits here for a job.
    work: Condvar,
    /// [`Sender::wait_idle`] waits here for `done` to reach `handed`.
    idle: Condvar,
    /// Whether the thread was made (tried once).
    started: OnceLock<bool>,
    name: &'static str,
    stats: [AtomicU64; 6],
}

thread_local! {
    /// Set on a sending thread: its own waits return at once.
    static SENDING: Cell<bool> = const { Cell::new(false) };
}

impl<W: Work> Sender<W> {
    pub(super) const fn new(name: &'static str) -> Self {
        Sender {
            state: Mutex::new(State {
                jobs: VecDeque::new(),
                handed: 0,
                done: 0,
            }),
            work: Condvar::new(),
            idle: Condvar::new(),
            started: OnceLock::new(),
            name,
            stats: [const { AtomicU64::new(0) }; 6],
        }
    }

    /// Queues `job`, making the thread on the first call. `Err(job)` when the
    /// thread could not be made (said once, never tried again).
    pub(super) fn hand_over(&'static self, job: W) -> Result<(), W> {
        let started = *self.started.get_or_init(|| {
            match std::thread::Builder::new().name(self.name.into()).spawn(move || self.worker()) {
                Ok(_) => true,
                Err(e) => {
                    tracing::warn!(error = %e, "no thread to send submissions with: the submitting thread sends them (VICTORY_SEND_THREAD=1)");
                    false
                }
            }
        });
        if !started {
            return Err(job);
        }
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.jobs.push_back(job);
        state.handed += 1;
        drop(state);
        self.work.notify_one();
        Ok(())
    }

    /// Returns once every job handed over so far is done; at once when none
    /// is outstanding or on the sending thread itself.
    pub(super) fn wait_idle(&self) {
        if SENDING.with(Cell::get) {
            return;
        }
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.handed == state.done {
            return;
        }
        let began = Instant::now();
        while state.handed != state.done {
            state = self.idle.wait(state).unwrap_or_else(|e| e.into_inner());
        }
        drop(state);
        self.stats[WAITS].fetch_add(1, Ordering::Relaxed);
        self.stats[WAIT_NS].fetch_add(began.elapsed().as_nanos() as u64, Ordering::Relaxed);
    }

    fn worker(&'static self) {
        crate::cores::keep_on_fast_cores();
        SENDING.with(|s| s.set(true));
        let mut last = 0u64;
        loop {
            let mut job = {
                let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                loop {
                    if let Some(job) = state.jobs.pop_front() {
                        break job;
                    }
                    state = self.work.wait(state).unwrap_or_else(|e| e.into_inner());
                }
            };
            // Out of order is counted and said, then run anyway: holding it
            // back would leave its fence value unsignalled.
            if !in_order(&mut last, job.fence()) {
                let seen = self.stats[DISORDER].fetch_add(1, Ordering::Relaxed);
                if seen < 4 {
                    tracing::warn!(
                        fence = job.fence(),
                        "a submission reached the thread that sends out of order (VICTORY_SEND_THREAD=1)"
                    );
                }
            }
            let began = Instant::now();
            let failure = match std::panic::catch_unwind(AssertUnwindSafe(|| job.run())) {
                Ok(Ok(())) => None,
                Ok(Err(e)) => Some(e),
                Err(_) => {
                    let _ = std::panic::catch_unwind(AssertUnwindSafe(|| job.rescue()));
                    Some("the thread that sends submissions panicked".to_string())
                }
            };
            drop(job);
            self.stats[JOBS].fetch_add(1, Ordering::Relaxed);
            self.stats[BUSY_NS].fetch_add(began.elapsed().as_nanos() as u64, Ordering::Relaxed);
            self.state.lock().unwrap_or_else(|e| e.into_inner()).done += 1;
            self.idle.notify_all();
            if let Some(what) = failure {
                self.stats[FAILED].fetch_add(1, Ordering::Relaxed);
                let _ = std::panic::catch_unwind(|| W::failed(&what));
            }
        }
    }

    fn take_stats(&self) -> [u64; 6] {
        std::array::from_fn(|i| self.stats[i].swap(0, Ordering::Relaxed))
    }
}

/// Whether `fence` comes after the last job's; `last` moves on to it.
fn in_order(last: &mut u64, fence: u64) -> bool {
    let ok = fence > *last;
    *last = fence;
    ok
}

/// One submission's tail: its card lists to record (`VICTORY_CARD_RECORD_SEND`),
/// its closed lists, the compare sent in a call of its own, and the fence
/// value signalled behind them.
pub(super) struct Job {
    host: &'static Host,
    card: Option<Pending>,
    lists: Vec<Option<ID3D12CommandList>>,
    apart: Option<ID3D12CommandList>,
    fence: u64,
    signalled: bool,
}

/// A job's steps in order: `record` (the card's lists), then `send` (lists,
/// compare, signal). A recording that fails sends nothing; `signal` alone
/// completes the fence value and the recording's failure is given back.
fn steps(
    record: impl FnOnce() -> Result<(), String>,
    send: impl FnOnce() -> Result<(), String>,
    signal: impl FnOnce(),
) -> Result<(), String> {
    match record() {
        Ok(()) => send(),
        Err(e) => {
            signal();
            Err(e)
        }
    }
}

// SAFETY: closed lists of free-threaded D3D12 objects; nothing records into
// them again, and the host lives for the run.
unsafe impl Send for Job {}

impl Work for Job {
    fn fence(&self) -> u64 {
        self.fence
    }

    /// The calls `Host::submit` makes inline, in the same order.
    fn run(&mut self) -> Result<(), String> {
        let host = self.host;
        let (fence, card) = (self.fence, self.card.take());
        let signalled = Cell::new(false);
        let ran = steps(
            || card.as_ref().map_or(Ok(()), Pending::record),
            || {
                // SAFETY: closed lists (the card's recorded just now); the fence is ours.
                let sent = unsafe {
                    host.queue.ExecuteCommandLists(&self.lists);
                    crate::compare_apart::send(&host.queue, &self.apart);
                    host.queue.Signal(&host.fence, fence)
                };
                signalled.set(true);
                host.card_sent();
                sent.map_err(|e| format!("a submission sent ahead could not be followed by a signal ({e})"))
            },
            || {
                // SAFETY: our queue and fence; the value was reserved for this job.
                let _ = unsafe { host.queue.Signal(&host.fence, fence) };
                signalled.set(true);
            },
        );
        self.signalled |= signalled.get();
        ran
    }

    fn rescue(&mut self) {
        if !self.signalled {
            // SAFETY: our queue and fence; the value was reserved for this job.
            let _ = unsafe { self.host.queue.Signal(&self.host.fence, self.fence) };
        }
    }

    fn failed(what: &str) {
        super::fail(format_args!("{what} (VICTORY_SEND_THREAD=1)"));
    }
}

static SEND: Sender<Job> = Sender::new("victory-send");

/// With the switch, for the host every run has: `note` (what is noted as
/// submitted at `fence`), then `lists` and `apart` handed to the thread,
/// which sends them and signals `fence`; true. False with nothing done
/// otherwise: the caller sends them itself.
pub(super) fn send_ahead(
    host: &Host,
    lists: &mut Vec<Option<ID3D12CommandList>>,
    apart: &mut Option<ID3D12CommandList>,
    card: &mut Option<Pending>,
    fence: u64,
    note: impl FnOnce(),
) -> bool {
    if !on() {
        return false;
    }
    let Some(host) = super::host().filter(|h| std::ptr::eq(*h, host)) else {
        return false;
    };
    note();
    let job = Job {
        host,
        card: card.take(),
        lists: std::mem::take(lists),
        apart: apart.take(),
        fence,
        signalled: false,
    };
    let carded = job.card.is_some();
    match SEND.hand_over(job) {
        Ok(()) => {
            if carded {
                Pending::counted(false);
            }
        }
        Err(mut job) => {
            if carded {
                Pending::counted(true);
            }
            if let Err(what) = job.run() {
                Job::failed(&what);
            }
        }
    }
    true
}

/// A submission the thread did not take: its card lists, if left to record,
/// are recorded here, before its lists go to the queue.
pub(super) fn record_here(card: &mut Option<Pending>) {
    if let Some(card) = card.take() {
        Pending::counted(true);
        if let Err(what) = card.record() {
            super::fail(format_args!("{what}"));
        }
    }
}

/// Returns once the thread has sent everything handed over so far; at once
/// when nothing is outstanding (always, with the switch off). Called before
/// anything else of ours goes to the host queue.
pub(crate) fn wait_idle() {
    SEND.wait_idle();
}

/// The line, per frame, when the thread sent anything.
pub(super) fn line(frames: f64) {
    let s = SEND.take_stats();
    if s[JOBS] == 0 && s[WAITS] == 0 && !on() {
        return;
    }
    let frames = frames.max(1.0);
    tracing::info!(
        "submissions sent by a thread of their own per frame (VICTORY_SEND_THREAD=1): {:.1} jobs, {:.2} ms in the sends on it, \
         {:.2} ms waited for it in {:.1} waits before other work went to the queue, {} out of order, {} failed",
        s[JOBS] as f64 / frames,
        s[BUSY_NS] as f64 / 1e6 / frames,
        s[WAIT_NS] as f64 / 1e6 / frames,
        s[WAITS] as f64 / frames,
        s[DISORDER],
        s[FAILED],
    );
}

#[cfg(test)]
#[path = "host_send_thread_tests.rs"]
mod tests;
