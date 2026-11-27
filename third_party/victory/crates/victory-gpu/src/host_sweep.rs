//! (feedcard) The once-a-second sweep on a thread of its own (`VICTORY_SWEEP_ASIDE=1`; off by
//! default).
//!
//! With presents sent ahead there is never a moment with nothing in the air, so once every
//! 1.5 s the submitting thread made one: it settled every flight (waited for the card, landed
//! each one on this thread), then swept ([`super::sweep`]: pages, staging buffers and host
//! copies of textures nothing names any more). The card stood idle behind it: the fence
//! timeline found 1-list submissions at 11.5 ms of preparing, all of it in that step.
//!
//! With the switch, the submitting thread only asks for a sweep and goes on. A thread of its
//! own sweeps holding the host's submission lock, so no submission is being prepared meanwhile
//! (one being prepared may use what it would let go, before naming it); flights in the air
//! stay there. That is safe for what the sweep lets go of: each is let go only when nothing
//! else holds it (lists hold what they name, recorded, waiting or flying), and is dropped
//! through `retire`, after every submission started so far has finished on the card.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::Instant;

/// `VICTORY_SWEEP_ASIDE=1`.
pub(super) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        let on = std::env::var_os("VICTORY_SWEEP_ASIDE").is_some_and(|v| v == "1");
        if on {
            tracing::info!("the once-a-second sweep runs on a thread of its own, with flights in the air (VICTORY_SWEEP_ASIDE=1)");
        }
        on
    })
}

/// Since the last line: microseconds and times of the settle made for the sweep, of the
/// sweep, and microseconds the sweep thread waited for the submission lock.
static COUNTS: [AtomicU64; 5] = [const { AtomicU64::new(0) }; 5];

/// The settle made for a sweep took `us` microseconds (on the submitting thread).
pub(super) fn settled(us: u64) {
    COUNTS[0].fetch_add(us, Ordering::Relaxed);
    COUNTS[1].fetch_add(1, Ordering::Relaxed);
}

/// A sweep took `us` microseconds.
pub(super) fn swept(us: u64) {
    COUNTS[2].fetch_add(us, Ordering::Relaxed);
    COUNTS[3].fetch_add(1, Ordering::Relaxed);
}

/// A sweep asked for and not started yet.
static ASKED: (Mutex<bool>, Condvar) = (Mutex::new(false), Condvar::new());

/// Asks the sweep thread for a sweep; one asked already and not started counts for both.
pub(super) fn ask() {
    static STARTED: std::sync::Once = std::sync::Once::new();
    STARTED.call_once(|| {
        let spawned = std::thread::Builder::new().name("victory-sweep".into()).spawn(|| {
            crate::topology::pin_background("victory-sweep");
            loop {
                wait_asked();
                let Some(host) = super::host() else { continue };
                let asked = Instant::now();
                let submitted = host.submitted.lock().unwrap_or_else(|e| e.into_inner());
                COUNTS[4].fetch_add(asked.elapsed().as_micros() as u64, Ordering::Relaxed);
                sweep_timed();
                drop(submitted);
            }
        });
        if let Err(e) = spawned {
            tracing::warn!(error = %e, "no thread for the sweep (VICTORY_SWEEP_ASIDE=1): sweeps are skipped");
        }
    });
    *ASKED.0.lock().unwrap_or_else(|e| e.into_inner()) = true;
    ASKED.1.notify_one();
}

/// Waits for a sweep asked for and takes it.
fn wait_asked() {
    let mut asked = ASKED.0.lock().unwrap_or_else(|e| e.into_inner());
    while !*asked {
        asked = ASKED.1.wait(asked).unwrap_or_else(|e| e.into_inner());
    }
    *asked = false;
}

/// [`super::sweep`], timed.
pub(super) fn sweep_timed() {
    let started = Instant::now();
    super::sweep();
    swept(started.elapsed().as_micros() as u64);
}

/// With the frame line (with the fence timeline or this switch on).
pub(super) fn line(frames: f64) {
    if !(on() || crate::timeline::on()) || frames <= 0.0 {
        return;
    }
    let [settle_us, settles, sweep_us, sweeps, waited_us] =
        [0, 1, 2, 3, 4].map(|i| COUNTS[i].swap(0, Ordering::Relaxed));
    if settles + sweeps == 0 {
        return;
    }
    let each = |us: u64, n: u64| us as f64 / 1000.0 / n.max(1) as f64;
    tracing::info!(
        "the sweep: {settles} settles made for it, {:.2} ms each on the submitting thread; {sweeps} sweeps, {:.2} ms each \
         ({}); the sweep thread waited {:.2} ms each for the submission lock ({:.2} frames)",
        each(settle_us, settles),
        each(sweep_us, sweeps),
        if on() {
            "on their own thread, VICTORY_SWEEP_ASIDE=1"
        } else {
            "on the submitting thread"
        },
        each(waited_us, sweeps),
        frames,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asks_before_a_sweep_starts_count_as_one() {
        *ASKED.0.lock().unwrap() = true;
        *ASKED.0.lock().unwrap() = true;
        wait_asked();
        assert!(!*ASKED.0.lock().unwrap());
    }
}
