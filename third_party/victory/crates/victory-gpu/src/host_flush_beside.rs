//! (critpath, #88) `VICTORY_FLUSH_BESIDE=1`: the refreshes of the flush that
//! ends a submission's preparing run on a thread of ours (spread over the
//! kept threads, as before) while the submitting thread records the lists'
//! preludes; it waits for them after, before the snapshots, the card's plan
//! and ExecuteCommandLists. Without the switch the preludes are recorded
//! after the whole flush, one after the other on the submitting thread.
//!
//! What may run side by side: a prelude records texture states, depth-twin
//! copies, title-texture uploads from their staging and the clamps ahead; it
//! reads no host copy and writes no title memory. The staging is brought up
//! to date by the flush's texture checks, which still run before the
//! preludes (with the fills, before the refreshes rather than after: the
//! order of a flush before a step that writes title memory). Preludes were
//! already recorded with refreshes pending: in the middle of a submission,
//! before a list with constant buffers in a new epoch. The refreshes are the
//! same calls on the same copies, on another thread.

use super::flush::{self, F};
use super::{Batch, PREP_PARTS};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::Instant;

static BESIDE: victory_core::settings::Live =
    victory_core::settings::Live::flag("VICTORY_FLUSH_BESIDE");

/// Whether the flush that ends this submission's preparing runs its
/// refreshes beside the preludes: the switch, preludes to record, and
/// refreshes that would wake the kept threads.
pub(super) fn wanted(batch: &Batch, preludes: bool) -> bool {
    preludes && BESIDE.get_bool() && super::wide_refresh() && batch.ops.copies.len() >= 2
}

/// Since the last line: flushes, refresh us on the beside thread, prelude us
/// meanwhile, us waited for the refreshes after the preludes.
static STATS: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

/// The flush that ends a submission's preparing, with `preludes` recorded
/// while its refreshes run. Fills and texture checks first, on this thread.
pub(super) fn run(batch: &mut Batch, preludes: impl FnOnce()) {
    let began = Instant::now();
    flush::last(|| {
        let copies = {
            let _hitch = crate::hitch::span(crate::hitch::Phase::Copies);
            batch.flush_fills();
            batch.flush_textures();
            batch.ops.take()
        };
        flush::add(F::Flushes, 1);
        flush::add(F::Copies, copies.len() as u64);
        let ahead = copies.iter().filter(|c| c.refresh_span.is_some()).count() > 1
            && !crate::watch::ask_once_active();
        let refresh_us = Arc::new(AtomicU64::new(0));
        let refreshed = refresh_us.clone();
        let work: Job = Box::new(move || {
            let started = Instant::now();
            if ahead {
                crate::watch::ask_ahead(
                    copies
                        .iter()
                        .filter(|c| !c.shadow.unwatched())
                        .filter_map(|c| c.refresh_span),
                );
                let us = started.elapsed().as_micros() as u64;
                super::PREPARING[0].fetch_add(us, Ordering::Relaxed);
                flush::add(F::AskAheadUs, us);
            }
            flush::refresh(copies, super::wide_refresh());
            if ahead {
                crate::watch::ask_ahead_done();
            }
            refreshed.store(started.elapsed().as_micros() as u64, Ordering::Relaxed);
        });
        PREP_PARTS[6].fetch_add(began.elapsed().as_micros() as u64, Ordering::Relaxed);
        let (beside_us, wait_us) = overlap(work, preludes, || {
            crate::hitch::span(crate::hitch::Phase::Copies)
        });
        PREP_PARTS[5].fetch_add(beside_us, Ordering::Relaxed);
        PREP_PARTS[6].fetch_add(wait_us, Ordering::Relaxed);
        flush::add(F::TotalUs, began.elapsed().as_micros() as u64);
        STATS[0].fetch_add(1, Ordering::Relaxed);
        STATS[1].fetch_add(refresh_us.load(Ordering::Relaxed), Ordering::Relaxed);
        STATS[2].fetch_add(beside_us, Ordering::Relaxed);
        STATS[3].fetch_add(wait_us, Ordering::Relaxed);
    });
    crate::watch::ask_once_end();
}

type Job = Box<dyn FnOnce() + Send>;

/// One job's end: its result once the thread has run it.
#[derive(Default)]
struct Done {
    result: Mutex<Option<std::thread::Result<()>>>,
    cv: Condvar,
}

/// The thread that runs the refreshes, made on first use. `None` if it
/// could not be made: the work then runs on the caller's thread.
fn thread() -> Option<&'static Mutex<std::sync::mpsc::Sender<(Job, Arc<Done>)>>> {
    static TX: OnceLock<Option<Mutex<std::sync::mpsc::Sender<(Job, Arc<Done>)>>>> = OnceLock::new();
    TX.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel::<(Job, Arc<Done>)>();
        let made = std::thread::Builder::new()
            .name("victory-flush-beside".into())
            .spawn(move || {
                crate::cores::keep_on_fast_cores();
                while let Ok((job, done)) = rx.recv() {
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job));
                    *done.result.lock().unwrap_or_else(|e| e.into_inner()) = Some(result);
                    done.cv.notify_one();
                }
            });
        match made {
            Ok(_) => Some(Mutex::new(tx)),
            Err(e) => {
                tracing::warn!(error = %e, "no thread for the refreshes beside the preludes (VICTORY_FLUSH_BESIDE=1): they run before them");
                None
            }
        }
    })
    .as_ref()
}

/// Runs `work` on the beside thread while `beside` runs here, then waits for
/// `work` (under the guard `waiting` makes). Returns the us `beside` took and
/// the us waited after it. A panic in `work` goes on here.
fn overlap<G>(work: Job, beside: impl FnOnce(), waiting: impl FnOnce() -> G) -> (u64, u64) {
    let done = Arc::new(Done::default());
    let sent = match thread() {
        Some(tx) => match tx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .send((work, done.clone()))
        {
            Ok(()) => None,
            Err(std::sync::mpsc::SendError((work, _))) => Some(work),
        },
        None => Some(work),
    };
    if let Some(work) = sent {
        work();
        *done.result.lock().unwrap_or_else(|e| e.into_inner()) = Some(Ok(()));
    }
    let at = Instant::now();
    beside();
    let beside_us = at.elapsed().as_micros() as u64;
    let at = Instant::now();
    let result = {
        let _guard = waiting();
        let mut result = done.result.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(r) = result.take() {
                break r;
            }
            result = done.cv.wait(result).unwrap_or_else(|e| e.into_inner());
        }
    };
    let wait_us = at.elapsed().as_micros() as u64;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
    (beside_us, wait_us)
}

/// The line, per frame, when the switch is on.
pub(super) fn line(frames: f64) {
    let s: Vec<u64> = STATS.iter().map(|a| a.swap(0, Ordering::Relaxed)).collect();
    if !BESIDE.get_bool() {
        return;
    }
    let ms = |us: u64| us as f64 / (frames * 1000.0);
    tracing::info!(
        "flush beside the preludes per frame (VICTORY_FLUSH_BESIDE=1): {:.1} flushes, refreshes {:.2} ms on their thread, \
         preludes {:.2} ms recorded meanwhile, {:.2} ms waited for the refreshes after them",
        s[0] as f64 / frames,
        ms(s[1]),
        ms(s[2]),
        ms(s[3]),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;

    #[test]
    fn the_work_runs_beside_and_is_waited_for() {
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let ran = Arc::new(AtomicUsize::new(0));
        let r = ran.clone();
        let here = std::thread::current().id();
        let work: Job = Box::new(move || {
            assert_ne!(
                std::thread::current().id(),
                here,
                "the work ran on the caller's thread"
            );
            // Done only once the caller's side has run: serially this waits for good.
            rx.recv_timeout(Duration::from_secs(10))
                .expect("the work did not run beside");
            std::thread::sleep(Duration::from_millis(20));
            r.store(1, Ordering::SeqCst);
        });
        let (_, waited) = overlap(work, move || tx.send(()).unwrap(), || ());
        assert_eq!(
            ran.load(Ordering::SeqCst),
            1,
            "returned before the work ended"
        );
        assert!(
            waited >= 10_000,
            "the wait for the work was not counted ({waited} us)"
        );
    }

    #[test]
    fn a_panic_in_the_work_reaches_the_waiter() {
        let work: Job = Box::new(|| panic!("refresh failed"));
        let back =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| overlap(work, || (), || ())));
        assert!(back.is_err());
        // The thread outlives it.
        let ran = Arc::new(AtomicUsize::new(0));
        let r = ran.clone();
        overlap(Box::new(move || r.store(1, Ordering::SeqCst)), || (), || ());
        assert_eq!(ran.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn preludes_come_after_the_texture_checks_and_the_copies_are_taken() {
        let checked = Arc::new(AtomicUsize::new(0));
        let mut batch = Batch::default();
        for i in 0..5 {
            let checked = checked.clone();
            batch.early.start(move || {
                if i % 2 == 0 {
                    std::thread::sleep(Duration::from_millis(1));
                }
                checked.fetch_add(1, Ordering::SeqCst);
            });
        }
        let seen = AtomicUsize::new(usize::MAX);
        run(&mut batch, || {
            seen.store(checked.load(Ordering::SeqCst), Ordering::SeqCst)
        });
        assert_eq!(
            seen.load(Ordering::SeqCst),
            5,
            "a prelude ran before the texture checks ended"
        );
        assert!(!batch.early.started());
        assert!(batch.ops.copies.is_empty());
    }

    #[test]
    fn the_switch_is_live() {
        if std::env::var_os("VICTORY_FLUSH_BESIDE").is_some() {
            return;
        }
        assert!(!BESIDE.get_bool(), "off by default");
        victory_core::settings::set("VICTORY_FLUSH_BESIDE", "1").unwrap();
        let now = BESIDE.get_bool();
        victory_core::settings::set("VICTORY_FLUSH_BESIDE", "").unwrap();
        assert!(now, "a change takes effect while running");
        assert!(!BESIDE.get_bool());
    }
}
