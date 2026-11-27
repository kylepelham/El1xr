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

use std::sync::{Mutex, OnceLock, mpsc};

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
/// `victory-land-aside` when `VICTORY_LAND_ASIDE=1`, inline otherwise.
pub(crate) fn after_submission() {
    static ASIDE: OnceLock<Option<AsideWorker>> = OnceLock::new();
    if wanted() {
        let aside = ASIDE.get_or_init(|| AsideWorker::spawn("victory-land-aside", crate::host::land));
        if let Some(worker) = aside
            && worker.trigger()
        {
            return;
        }
    }
    crate::host::land();
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
}
