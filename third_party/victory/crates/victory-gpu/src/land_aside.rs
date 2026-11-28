//! Resets landed host command lists on a dedicated thread after submission
//! (`VICTORY_LAND_ASIDE=1`, off by default).
//!
//! Resetting the lists of landed flights is host and D3D12 driver work on
//! their command allocators, and nothing the following submission waits for:
//! it takes an already-idle list or creates one. `crate::host::land` is
//! already called from `victory-late-wait` as well, so running it beside the
//! submitting thread is safe.
//!
//! Ported from Peppacodes/playablegames-peppa (`cb412956cf`, originally
//! fxceless/victory-fork PR #1146 by beamdotdev), GPL-3.0.
//!
//! (critpath #88) `VICTORY_RESET_ASIDE=1` (Live, off by default) does the
//! same and, with it, resets each landed list's prelude ahead
//! (`crate::host::run_wait::reset_ahead`, as `VICTORY_PRELUDE_AHEAD`): both
//! resets leave the submitting thread. `VICTORY_PRELUDE_AHEAD` alone only
//! moved the prelude resets from `run_other` to this landing, which ran on the
//! submitting thread after each submission (b5g: run_other -1.1 ms, the frame
//! unmoved). Only settled flights land (their fence was reached), and a
//! landed list is reached by nothing else until it is back in the pool, so
//! the thread that lands it does not matter. The landing is timed either way
//! (the line `landed lists reset per frame`).

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Mutex, OnceLock, mpsc};
use std::time::Instant;

static RESET_ASIDE: victory_core::settings::Live =
    victory_core::settings::Live::flag("VICTORY_RESET_ASIDE");

#[cfg(test)]
thread_local! {
    /// Tests: `VICTORY_RESET_ASIDE` for calls made on this thread.
    pub(crate) static FORCED: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

/// `VICTORY_RESET_ASIDE`: landed lists and their preludes are reset on
/// `victory-land-aside`.
pub(crate) fn reset_aside_on() -> bool {
    #[cfg(test)]
    if let Some(on) = FORCED.with(std::cell::Cell::get) {
        return on;
    }
    RESET_ASIDE.get_bool()
}

/// Since the last line: landings, lists landed and nanoseconds, on the
/// submitting thread (inline) and on `victory-land-aside`.
const INLINE: usize = 0;
const ASIDE: usize = 3;
static COUNTS: [AtomicU64; 6] = [const { AtomicU64::new(0) }; 6];

/// One landing by `land` (it returns the lists landed), counted at `at`.
fn timed(at: usize, land: impl FnOnce() -> usize) {
    let started = Instant::now();
    let lists = land();
    if lists == 0 {
        return;
    }
    COUNTS[at].fetch_add(1, Relaxed);
    COUNTS[at + 1].fetch_add(lists as u64, Relaxed);
    COUNTS[at + 2].fetch_add(started.elapsed().as_nanos() as u64, Relaxed);
}

/// The frame line's, in both settings.
pub(crate) fn line(frames: f64) {
    let c: Vec<f64> = COUNTS.iter().map(|c| c.swap(0, Relaxed) as f64).collect();
    tracing::info!(
        "landed lists reset per frame: {:.1} lists in {:.1} landings on the submitting thread {:.2} ms, {:.1} lists in \
         {:.1} landings on victory-land-aside {:.2} ms (VICTORY_RESET_ASIDE={}, VICTORY_LAND_ASIDE={})",
        c[INLINE + 1] / frames,
        c[INLINE] / frames,
        c[INLINE + 2] / 1e6 / frames,
        c[ASIDE + 1] / frames,
        c[ASIDE] / frames,
        c[ASIDE + 2] / 1e6 / frames,
        u8::from(reset_aside_on()),
        u8::from(wanted()),
    );
}

/// Parses `VICTORY_LAND_ASIDE`: opt-in on `"1"`, off otherwise.
fn parse_switch(value: Option<&str>) -> bool {
    value.is_some_and(|v| v.trim() == "1")
}

fn wanted() -> bool {
    static WANTED: OnceLock<bool> = OnceLock::new();
    *WANTED.get_or_init(|| {
        let on = parse_switch(std::env::var("VICTORY_LAND_ASIDE").ok().as_deref());
        if on {
            tracing::info!(
                "landed host lists reset on a thread of their own (VICTORY_LAND_ASIDE=1)"
            );
        }
        on
    })
}

/// Coalescing background worker that invokes `run` on a dedicated thread.
struct AsideWorker {
    send: Mutex<mpsc::Sender<()>>,
}

impl AsideWorker {
    fn spawn<F>(name: &str, mut run: F) -> Option<Self>
    where
        F: FnMut() + Send + 'static,
    {
        let (send, asked) = mpsc::channel::<()>();
        std::thread::Builder::new()
            .name(name.to_owned())
            .spawn(move || {
                crate::topology::pin_background("victory-land-aside");
                while asked.recv().is_ok() {
                    // One landing for all the asks queued so far.
                    while asked.try_recv().is_ok() {}
                    run();
                }
            })
            .ok()
            .map(|_| Self {
                send: Mutex::new(send),
            })
    }

    fn trigger(&self) -> bool {
        self.send
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .send(())
            .is_ok()
    }
}

/// Calls [`crate::host::land`] at the end of a submission: offloaded to
/// `victory-land-aside` when `VICTORY_LAND_ASIDE=1` or
/// `VICTORY_RESET_ASIDE=1`, inline otherwise.
pub(crate) fn after_submission() {
    static WORKER: OnceLock<Option<AsideWorker>> = OnceLock::new();
    route(wanted() || reset_aside_on(), &WORKER, crate::host::land);
}

/// One landing by `land`: triggered on the worker in `worker` (spawned the
/// first time) when `aside`, inline otherwise or if there is no worker.
fn route(aside: bool, worker: &OnceLock<Option<AsideWorker>>, land: fn() -> usize) {
    if aside {
        let worker = worker.get_or_init(|| AsideWorker::spawn("victory-land-aside", move || timed(ASIDE, land)));
        if let Some(worker) = worker
            && worker.trigger()
        {
            return;
        }
    }
    timed(INLINE, land);
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread::ThreadId;
    use std::time::Duration;

    use super::*;

    #[test]
    fn parse_switch_is_opt_in_on_one_only() {
        assert!(!parse_switch(None));
        assert!(!parse_switch(Some("")));
        assert!(!parse_switch(Some("0")));
        assert!(!parse_switch(Some("true")));
        assert!(parse_switch(Some("1")));
        assert!(parse_switch(Some(" 1 ")));
    }

    #[test]
    fn worker_runs_off_caller_thread_and_coalesces_bursts() {
        let caller = std::thread::current().id();
        let calls = Arc::new(AtomicUsize::new(0));
        let worker_tid = Arc::new(Mutex::new(None::<ThreadId>));
        let (entered_tx, entered_rx) = mpsc::channel::<()>();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let release_rx = Arc::new(Mutex::new(Some(release_rx)));
        let (done_tx, done_rx) = mpsc::channel::<usize>();

        let calls_c = Arc::clone(&calls);
        let tid_c = Arc::clone(&worker_tid);
        let worker = AsideWorker::spawn("victory-land-aside-test", move || {
            *tid_c.lock().unwrap() = Some(std::thread::current().id());
            let n = calls_c.fetch_add(1, Ordering::SeqCst) + 1;
            if n == 1 {
                let _ = entered_tx.send(());
                if let Some(rx) = release_rx.lock().unwrap().take() {
                    let _ = rx.recv_timeout(Duration::from_secs(5));
                }
            }
            let _ = done_tx.send(n);
        })
        .expect("spawn aside test worker");

        // Trigger the first landing and wait until the worker is inside it.
        assert!(worker.trigger());
        entered_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("worker entered first call");

        // Queue a burst of 16 asks while the first call is blocked: they must
        // coalesce into a single follow-up call once released.
        for _ in 0..16 {
            assert!(worker.trigger());
        }
        release_tx.send(()).unwrap();

        assert_eq!(done_rx.recv_timeout(Duration::from_secs(5)).unwrap(), 1);
        assert_eq!(done_rx.recv_timeout(Duration::from_secs(5)).unwrap(), 2);
        // No 3rd call should follow for the coalesced burst.
        assert!(done_rx.recv_timeout(Duration::from_millis(50)).is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_ne!(
            *worker_tid.lock().unwrap(),
            Some(caller),
            "aside worker must run on a separate thread"
        );
    }

    static LANDED_ON: Mutex<Vec<ThreadId>> = Mutex::new(Vec::new());

    fn land_here() -> usize {
        LANDED_ON.lock().unwrap().push(std::thread::current().id());
        3
    }

    fn landings() -> Vec<ThreadId> {
        LANDED_ON.lock().unwrap().clone()
    }

    /// (critpath #88) `VICTORY_RESET_ASIDE`: off, the landing runs on the
    /// submitting thread before `after_submission` returns and no worker is
    /// made; on, on the worker; off again, inline again.
    #[test]
    fn route_lands_inline_when_off_and_on_the_worker_when_on() {
        let caller = std::thread::current().id();
        let worker = OnceLock::new();
        route(false, &worker, land_here);
        assert_eq!(landings(), [caller]);
        assert!(worker.get().is_none(), "no worker while off");
        route(true, &worker, land_here);
        let started = std::time::Instant::now();
        while landings().len() < 2 {
            assert!(started.elapsed() < Duration::from_secs(5), "the worker landed");
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_ne!(landings()[1], caller, "landed on the worker");
        route(false, &worker, land_here);
        assert_eq!(landings().len(), 3);
        assert_eq!(landings()[2], caller, "inline again once off");
    }
}
