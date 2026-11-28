//! `VICTORY_TABLE_PROF=1` (restart; diag, off unless `1`): where the binds' descriptor tables
//! time goes (`State::tables`, 72-75 ms of ~100 ms bind CPU a frame summed over the title's
//! threads in batch 5). One line a second beside the binding line:
//! `descriptor tables split per frame (VICTORY_TABLE_PROF=1): ...` with
//! - tables found ready (nothing done), found made and made ready again, and made: count and ms;
//! - inside the last two: view lookups, view making (`make_at`/`make_listed`), readiness checks
//!   (`readable`), the title's own slot (`texture_slot_tabled` + `mirror`): ms;
//! - the shared descriptor lookup lock (`descriptor::VIEWS`, read side): acquisitions and the ms
//!   spent acquiring it, on every thread (what tables share with everything else that looks up a
//!   view).
//!
//! Timing costs two clock reads a part (~40 ns): the ms are inflated a little with the switch,
//! and the sum is compared with the binding line's `descriptor tables` of the same run.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::Instant;

/// The switch, read once.
pub(crate) fn on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_TABLE_PROF").is_some_and(|v| v == "1"))
}

#[derive(Clone, Copy)]
#[repr(usize)]
pub(crate) enum P {
    /// Tables found ready.
    Ready,
    /// Tables found made and made ready again.
    Again,
    /// Tables made.
    Made,
    /// View lookups of made / made-again tables.
    Lookup,
    /// Views made in dynamic slots.
    Make,
    /// `readable` of made / made-again tables.
    Readable,
    /// The title's own slot: tabled stamp and mirror.
    Title,
    /// Waiting to acquire the descriptor lookup lock's read side.
    LockWait,
}
const PARTS: usize = 8;
/// Nanoseconds per part, then counts of the three table kinds and of lock acquisitions.
static NS: [AtomicU64; PARTS] = [const { AtomicU64::new(0) }; PARTS];
static COUNTS: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

/// When a timed part began (`None` without the switch).
#[inline]
pub(crate) fn now() -> Option<Instant> {
    on().then(Instant::now)
}

/// A table of kind `p` (`Ready`, `Again`, `Made`) begun at `began` is done.
#[inline]
pub(crate) fn done(p: P, began: Option<Instant>) {
    if let Some(began) = began {
        NS[p as usize].fetch_add(began.elapsed().as_nanos() as u64, Relaxed);
        COUNTS[(p as usize).min(2)].fetch_add(1, Relaxed);
    }
}

/// Adds the time from `began` to part `p`.
#[inline]
pub(crate) fn part(p: P, began: Option<Instant>) {
    if let Some(began) = began {
        NS[p as usize].fetch_add(began.elapsed().as_nanos() as u64, Relaxed);
    }
}

/// `read` (taking the lookup lock's read side), timed with the switch.
#[inline]
pub(crate) fn timed_read<T>(read: impl FnOnce() -> T) -> T {
    if !on() {
        return read();
    }
    let began = Instant::now();
    let guard = read();
    NS[P::LockWait as usize].fetch_add(began.elapsed().as_nanos() as u64, Relaxed);
    COUNTS[3].fetch_add(1, Relaxed);
    guard
}

/// The figures since the last line, per frame over `frames`, taken.
fn take(frames: f64) -> ([f64; PARTS], [f64; 4]) {
    let ms: [f64; PARTS] = std::array::from_fn(|i| NS[i].swap(0, Relaxed) as f64 / 1e6 / frames);
    let n: [f64; 4] = std::array::from_fn(|i| COUNTS[i].swap(0, Relaxed) as f64 / frames);
    (ms, n)
}

fn text(ms: &[f64; PARTS], n: &[f64; 4]) -> String {
    format!(
        "{:.0} found ready {:.1} ms, {:.0} found made and made ready again {:.1} ms, {:.0} made {:.1} ms; \
         of the last two: view lookups {:.1} ms, view making {:.1} ms, readiness checks {:.1} ms, \
         the title's own slots {:.1} ms; descriptor lookup lock (all threads): {:.0} read acquisitions, \
         {:.2} ms acquiring",
        n[0], ms[0], n[1], ms[1], n[2], ms[2], ms[3], ms[4], ms[5], ms[6], n[3], ms[7]
    )
}

/// With the frame line (`draw::bind_split_line`).
pub(crate) fn line(frames: f64) {
    if !on() || frames <= 0.0 {
        return;
    }
    let (ms, n) = take(frames);
    if n[0] + n[1] + n[2] == 0.0 {
        return;
    }
    tracing::info!(
        "descriptor tables split per frame (VICTORY_TABLE_PROF=1): {}",
        text(&ms, &n)
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_line_names_each_part() {
        let ms = [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 0.25];
        let n = [10.0, 20.0, 30.0, 400.0];
        assert_eq!(
            text(&ms, &n),
            "10 found ready 1.0 ms, 20 found made and made ready again 2.0 ms, 30 made 3.0 ms; \
             of the last two: view lookups 4.0 ms, view making 5.0 ms, readiness checks 6.0 ms, \
             the title's own slots 7.0 ms; descriptor lookup lock (all threads): 400 read acquisitions, \
             0.25 ms acquiring"
        );
    }

    #[test]
    fn off_nothing_is_timed() {
        // The switch is unset in tests: no clock read, nothing counted.
        assert!(now().is_none());
        done(P::Made, None);
        assert_eq!(timed_read(|| 7), 7);
    }
}
