//! Where the card's idle time comes from, stretch by stretch (`VICTORY_FENCE_TIMELINE=1`).
//!
//! The card clock (`host.rs`, [`card_idle`]) notes each stretch the card had nothing of ours
//! queued: from the moment it reached the last thing sent (`I0`) to the next send (`I1`). That
//! send ends a submission whose preparing on our thread began at `P` ([`prep_begins`]). Between
//! them, this module keeps when the title made a submission ([`title_submit`]) and when a
//! landing told the title something: the stretch of the landing thread (or a settle) that ended
//! in bottom-of-pipe writes or fence signals ([`landed`]).
//!
//! Each stretch is cut into, in order of time:
//! - `wake`: the card idle until the landing that told the title began (the landing thread
//!   waking for the flight, or busy with an earlier one);
//! - `landing`: that landing (copy-back merged into title memory, then the writes and signals);
//! - `title`: from the title being told (or the stretch's start) to its next submission;
//! - `queued`: from that submission (or the told / start moment when none came) to our thread
//!   starting to prepare the one that ended the stretch (our thread busy with other work, held
//!   pieces waiting their turn);
//! - `prepare`: our thread preparing the submission that ended it.
//!
//! One line a second gives them per frame, beside the card clock's line.

use std::cell::Cell;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

/// `VICTORY_FENCE_TIMELINE=1`.
pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_FENCE_TIMELINE").is_some_and(|v| v == "1"))
}

/// How long title submissions and landings are kept for stretches still open.
const KEEP_US: u64 = 2_000_000;

struct Marks {
    /// Times the title made a submission.
    submits: VecDeque<u64>,
    /// Landings that told the title something: (start, end).
    landed: VecDeque<(u64, u64)>,
}

static MARKS: Mutex<Marks> = Mutex::new(Marks {
    submits: VecDeque::new(),
    landed: VecDeque::new(),
});

/// Since the last line: microseconds of `wake`, `landing`, `title`, `queued`, `prepare`;
/// stretches; stretches of 1 ms and more; microseconds of stretches ended by a thread that
/// prepares no submissions (the relay's copy-back fetch).
static SUMS: [AtomicU64; 8] = [const { AtomicU64::new(0) }; 8];

/// Since the last line: microseconds of each preparing part (`crate::host::PREP_PARTS`) spent
/// within `prepare`, the part's time in the submission scaled by the share of its preparing
/// that fell in the stretch.
static PREP_IDLE: [AtomicU64; PARTS] = [const { AtomicU64::new(0) }; PARTS];

const PARTS: usize = 10;

thread_local! {
    /// When this thread began preparing its current submission.
    static PREP: Cell<u64> = const { Cell::new(0) };
    /// The preparing parts' counters then.
    static PREP_AT: Cell<[u64; PARTS]> = const { Cell::new([0; PARTS]) };
}

fn prep_parts() -> [u64; PARTS] {
    std::array::from_fn(|i| crate::host::PREP_PARTS[i].load(Ordering::Relaxed))
}

fn marks() -> std::sync::MutexGuard<'static, Marks> {
    MARKS.lock().unwrap_or_else(|e| e.into_inner())
}

fn now() -> u64 {
    crate::host::now_us()
}

/// The title made a submission.
pub(crate) fn title_submit() {
    if !on() {
        return;
    }
    let t = now();
    let mut m = marks();
    m.submits.push_back(t);
    while m.submits.front().is_some_and(|&f| f + KEEP_US < t) {
        m.submits.pop_front();
    }
}

/// This thread begins preparing a submission.
pub(crate) fn prep_begins() {
    if on() {
        PREP.with(|p| p.set(now()));
        PREP_AT.with(|p| p.set(prep_parts()));
    }
}

/// A landing that began at `start` (a [`crate::host::now_us`] time) told the title `told`
/// things (bottom-of-pipe writes, fence signals); it ends now.
pub(crate) fn landed(start: u64, told: usize) {
    if !on() || told == 0 {
        return;
    }
    let t = now();
    let mut m = marks();
    m.landed.push_back((start, t));
    while m.landed.front().is_some_and(|&(_, e)| e + KEEP_US < t) {
        m.landed.pop_front();
    }
}

/// The card stood idle for `idle` until now, when this thread sends the submission it prepares.
pub(crate) fn card_idle(idle: std::time::Duration) {
    if !on() {
        return;
    }
    let i1 = now();
    let i0 = i1.saturating_sub(idle.as_micros() as u64);
    let p = PREP.with(Cell::get);
    SUMS[5].fetch_add(1, Ordering::Relaxed);
    if i1 - i0 >= 1000 {
        SUMS[6].fetch_add(1, Ordering::Relaxed);
    }
    if p == 0 {
        SUMS[7].fetch_add(i1 - i0, Ordering::Relaxed);
        return;
    }
    let parts = {
        let mut guard = marks();
        let m = &mut *guard;
        split(
            i0,
            i1,
            p,
            m.submits.make_contiguous(),
            m.landed.make_contiguous(),
        )
    };
    for (sum, part) in SUMS.iter().zip(parts) {
        sum.fetch_add(part, Ordering::Relaxed);
    }
    let whole = i1.saturating_sub(p);
    if whole > 0 {
        let at = PREP_AT.with(Cell::get);
        let share = parts[4] as f64 / whole as f64;
        for ((sum, now), at) in PREP_IDLE.iter().zip(prep_parts()).zip(at) {
            sum.fetch_add(
                (now.saturating_sub(at) as f64 * share) as u64,
                Ordering::Relaxed,
            );
        }
    }
}

/// The idle stretch `[i0, i1]`, ended by a submission prepared from `p`, cut into `wake`,
/// `landing`, `title`, `queued` and `prepare` (microseconds, adding up to `i1 - i0`), given the
/// title's submissions (`submits`) and the landings that told it something (`landed`), in order.
pub(crate) fn split(i0: u64, i1: u64, p: u64, submits: &[u64], landed: &[(u64, u64)]) -> [u64; 5] {
    let p = p.clamp(i0, i1);
    let prepare = i1 - p;
    if p == i0 {
        return [0, 0, 0, 0, prepare];
    }
    let submit = submits.iter().rev().find(|&&t| t > i0 && t <= p).copied();
    let told = landed
        .iter()
        .rev()
        .find(|&&(_, e)| e >= i0 && e <= submit.unwrap_or(p))
        .copied();
    let (wake, landing, base) = match told {
        Some((s, e)) => {
            let s = s.clamp(i0, e);
            (s - i0, e - s, e)
        }
        None => (0, 0, i0),
    };
    let (title, queued) = match submit {
        Some(t) => (t - base, p - t),
        None => (0, p - base),
    };
    [wake, landing, title, queued, prepare]
}

/// The line, for `frames` frames since the last one.
pub(crate) fn line(frames: f64) {
    if !on() || frames <= 0.0 {
        return;
    }
    let s: Vec<f64> = SUMS
        .iter()
        .map(|v| v.swap(0, Ordering::Relaxed) as f64)
        .collect();
    let ms = |us: f64| us / 1000.0 / frames;
    tracing::info!(
        "card idle stretches per frame: {:.1} ({:.1} of 1 ms and more), {:.2} ms idle = wake {:.2} ms + landing {:.2} ms \
         + title {:.2} ms + queued {:.2} ms + prepare {:.2} ms + ended by the relay's fetch {:.2} ms (VICTORY_FENCE_TIMELINE)",
        s[5] / frames,
        s[6] / frames,
        ms(s[0] + s[1] + s[2] + s[3] + s[4] + s[7]),
        ms(s[0]),
        ms(s[1]),
        ms(s[2]),
        ms(s[3]),
        ms(s[4]),
        ms(s[7]),
    );
    let p: Vec<f64> = PREP_IDLE
        .iter()
        .map(|v| v.swap(0, Ordering::Relaxed) as f64 / 1000.0 / frames)
        .collect();
    tracing::info!(
        "card idle prepare per frame by part: sync_heaps {:.2} ms, make_waiting {:.2} ms, ask_once {:.2} ms, \
         stale_flights {:.2} ms, run_flush_one {:.2} ms, run_other {:.2} ms, batch_flush {:.2} ms, snapshots {:.2} ms, \
         card_before {:.2} ms, exec_submit {:.2} ms (VICTORY_FENCE_TIMELINE)",
        p[0],
        p[1],
        p[2],
        p[3],
        p[4],
        p[5],
        p[6],
        p[7],
        p[8],
        p[9],
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepared_before_the_card_went_idle_is_all_prepare() {
        assert_eq!(split(100, 900, 50, &[60], &[(70, 80)]), [0, 0, 0, 0, 800]);
    }

    #[test]
    fn landing_then_title_then_queue_then_prepare() {
        // Idle at 100; landing 150-300 tells the title; it submits at 450; we start at 500.
        let parts = split(100, 1000, 500, &[50, 450], &[(10, 20), (150, 300)]);
        assert_eq!(parts, [50, 150, 150, 50, 500]);
        assert_eq!(parts.iter().sum::<u64>(), 900);
    }

    #[test]
    fn a_landing_after_the_title_submitted_does_not_count() {
        // The title submitted at 200 without waiting for the landing that ends at 400.
        assert_eq!(
            split(100, 1000, 500, &[200], &[(300, 400)]),
            [0, 0, 100, 300, 500]
        );
    }

    #[test]
    fn no_submission_in_the_stretch_is_queued_after_the_landing() {
        // A held piece released by the landing 120-250, prepared from 600.
        assert_eq!(
            split(100, 1000, 600, &[], &[(120, 250)]),
            [20, 130, 0, 350, 400]
        );
        // Nothing at all: our thread started late.
        assert_eq!(split(100, 1000, 600, &[], &[]), [0, 0, 0, 500, 400]);
    }

    #[test]
    fn a_landing_begun_before_the_stretch_is_cut_at_its_start() {
        assert_eq!(
            split(100, 1000, 500, &[400], &[(50, 300)]),
            [0, 200, 100, 100, 500]
        );
    }
}
