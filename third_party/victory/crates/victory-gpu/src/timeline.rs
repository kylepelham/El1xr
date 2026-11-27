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

/// The batch flushes' copies, bytes and textures so far (`host_flush`).
static FLUSHED: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

/// The stretches with the longest preparing since the last line, longest first.
static LONGEST: Mutex<Vec<Long>> = Mutex::new(Vec::new());
const LONGEST_KEPT: usize = 4;

/// A long idle stretch and the submission that ended it.
#[derive(Clone, Copy)]
struct Long {
    idle_us: u64,
    prepare_us: u64,
    /// Waiting for the title (`title` + `queued`), microseconds.
    title_us: u64,
    lists: usize,
    /// The preparing parts in the submission, microseconds (all of it, not only the idle share).
    parts: [u64; PARTS],
    /// Copies, bytes and textures its batch flushes refreshed.
    flushed: [u64; 3],
    /// In the stretch: waiting for the submission lock, for flights (settle or relay wait),
    /// and the sweep with what follows up to the parts, microseconds ([`mark`]).
    steps: [u64; 3],
    /// From its first and its last list's `Close` to the start of its preparing, microseconds:
    /// the room there is for work done at `Close` instead ([`closed`]).
    closed: (u64, u64),
}

/// When each host list (by address) was last closed, until a submission takes it.
static CLOSED: Mutex<Vec<(usize, u64)>> = Mutex::new(Vec::new());

/// The most closed lists kept: a list closed and never submitted is forgotten past it.
const CLOSED_KEPT: usize = 4096;

/// The title closed the host list at `list`.
pub(crate) fn closed(list: usize) {
    if !on() {
        return;
    }
    let t = now();
    let mut c = CLOSED.lock().unwrap_or_else(|e| e.into_inner());
    c.retain(|&(l, _)| l != list);
    if c.len() >= CLOSED_KEPT {
        c.remove(0);
    }
    c.push((list, t));
}

/// For lists at `lists` taken by a submission whose preparing starts at `t`: microseconds from
/// the first and from the last one's close; (0, 0) when none was seen closing.
fn since_closed(closed: &mut Vec<(usize, u64)>, lists: &[usize], t: u64) -> (u64, u64) {
    let mut at: Vec<u64> = Vec::new();
    closed.retain(|&(l, c)| {
        let taken = lists.contains(&l);
        if taken {
            at.push(c);
        }
        !taken
    });
    match (at.iter().min(), at.iter().max()) {
        (Some(&first), Some(&last)) => (t.saturating_sub(first), t.saturating_sub(last)),
        _ => (0, 0),
    }
}

thread_local! {
    /// [`since_closed`] for this thread's current preparing.
    static CLOSED_AT: Cell<(u64, u64)> = const { Cell::new((0, 0)) };
}

/// Since the last line: microseconds of the stretches' [`Long::steps`], all stretches.
static STEPS_IDLE: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

thread_local! {
    /// When this thread began preparing its current submission, and its lists.
    static PREP: Cell<(u64, usize)> = const { Cell::new((0, 0)) };
    /// The preparing parts' and flushes' counters then.
    static PREP_AT: Cell<([u64; PARTS], [u64; 3])> = const { Cell::new(([0; PARTS], [0; 3])) };
    /// When this thread got the submission lock, was past the wait for flights, and past the
    /// sweep, in its current preparing.
    static STEPS: Cell<[u64; 3]> = const { Cell::new([0; 3]) };
}

/// This thread's preparing reached step `k` ([`Long::steps`]).
pub(crate) fn mark(k: usize) {
    if on() {
        STEPS.with(|s| {
            let mut v = s.get();
            v[k] = now();
            s.set(v);
        });
    }
}

/// Microseconds of `[a, b]` within `[i0, i1]`.
fn within(a: u64, b: u64, i0: u64, i1: u64) -> u64 {
    b.min(i1).saturating_sub(a.max(i0))
}

/// A batch flush refreshed `n` more copies (`what` 0), bytes (1) or textures (2).
pub(crate) fn flushed(what: usize, n: u64) {
    if on() {
        FLUSHED[what].fetch_add(n, Ordering::Relaxed);
    }
}

fn flushed_now() -> [u64; 3] {
    std::array::from_fn(|i| FLUSHED[i].load(Ordering::Relaxed))
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

/// This thread begins preparing a submission of `runs`.
pub(crate) fn prep_begins(runs: &[&crate::host::HostList]) {
    if on() {
        let lists: Vec<usize> = runs.iter().map(|&l| std::ptr::from_ref(l) as usize).collect();
        let lists = &lists[..];
        let t = now();
        let gap = since_closed(&mut CLOSED.lock().unwrap_or_else(|e| e.into_inner()), lists, t);
        CLOSED_AT.with(|c| c.set(gap));
        PREP.with(|p| p.set((t, lists.len())));
        STEPS.with(|s| s.set([0; 3]));
        PREP_AT.with(|p| p.set((prep_parts(), flushed_now())));
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
    let (p, lists) = PREP.with(Cell::get);
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
    let (parts_at, flushed_at) = PREP_AT.with(Cell::get);
    let spent: [u64; PARTS] = {
        let now = prep_parts();
        std::array::from_fn(|i| now[i].saturating_sub(parts_at[i]))
    };
    let [m0, m1, m2] = STEPS.with(Cell::get);
    let steps = if m0 >= p && m1 >= m0 && m2 >= m1 {
        [within(p, m0, i0, i1), within(m0, m1, i0, i1), within(m1, m2, i0, i1)]
    } else {
        [0; 3]
    };
    for (sum, step) in STEPS_IDLE.iter().zip(steps) {
        sum.fetch_add(step, Ordering::Relaxed);
    }
    let whole = i1.saturating_sub(p);
    if whole > 0 {
        let share = parts[4] as f64 / whole as f64;
        for (sum, spent) in PREP_IDLE.iter().zip(spent) {
            sum.fetch_add((spent as f64 * share) as u64, Ordering::Relaxed);
        }
    }
    if i1 - i0 >= 1000 {
        let now = flushed_now();
        keep_longest(Long {
            idle_us: i1 - i0,
            prepare_us: parts[4],
            title_us: parts[2] + parts[3],
            lists,
            parts: spent,
            flushed: std::array::from_fn(|i| now[i].saturating_sub(flushed_at[i])),
            steps,
            closed: CLOSED_AT.with(Cell::get),
        });
    }
}

fn keep_longest(long: Long) {
    let mut kept = LONGEST.lock().unwrap_or_else(|e| e.into_inner());
    let at = kept.partition_point(|k| k.prepare_us >= long.prepare_us);
    if at < LONGEST_KEPT {
        kept.insert(at, long);
        kept.truncate(LONGEST_KEPT);
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
    let st: Vec<f64> = STEPS_IDLE
        .iter()
        .map(|v| v.swap(0, Ordering::Relaxed) as f64 / 1000.0 / frames)
        .collect();
    tracing::info!(
        "card idle prepare per frame before the parts: the submission lock {:.2} ms, flights waited for {:.2} ms, \
         sweep and heap sync {:.2} ms (VICTORY_FENCE_TIMELINE)",
        st[0],
        st[1],
        st[2],
    );
    let kept = std::mem::take(&mut *LONGEST.lock().unwrap_or_else(|e| e.into_inner()));
    for k in kept {
        let ms = |us: u64| us as f64 / 1000.0;
        tracing::info!(
            "card idle long stretch: {:.2} ms idle, {:.2} ms of it preparing, {:.2} ms waiting for the title; the submission \
             {} lists, preparing parts ask_once {:.2} ms, stale_flights {:.2} ms, run_other {:.2} ms, batch_flush {:.2} ms, \
             card_before {:.2} ms, sync_heaps {:.2} ms; its flushes {} copies, {:.2} MB, {} textures; in the stretch the lock \
             {:.2} ms, flights {:.2} ms, sweep {:.2} ms; its lists closed {:.2} to {:.2} ms before its preparing began \
             (VICTORY_FENCE_TIMELINE)",
            ms(k.idle_us),
            ms(k.prepare_us),
            ms(k.title_us),
            k.lists,
            ms(k.parts[2]),
            ms(k.parts[3]),
            ms(k.parts[5] + k.parts[4]),
            ms(k.parts[6]),
            ms(k.parts[8]),
            ms(k.parts[0]),
            k.flushed[0],
            k.flushed[1] as f64 / (1 << 20) as f64,
            k.flushed[2],
            ms(k.steps[0]),
            ms(k.steps[1]),
            ms(k.steps[2]),
            ms(k.closed.0),
            ms(k.closed.1),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn since_closed_takes_the_lists_it_finds() {
        let mut c = vec![(1, 100), (2, 300), (3, 50)];
        assert_eq!(since_closed(&mut c, &[2, 3, 9], 1000), (950, 700));
        assert_eq!(c, vec![(1, 100)]);
        assert_eq!(since_closed(&mut c, &[9], 1000), (0, 0));
    }

    #[test]
    fn within_cuts_to_the_stretch() {
        assert_eq!(within(50, 150, 100, 1000), 50);
        assert_eq!(within(200, 300, 100, 1000), 100);
        assert_eq!(within(10, 90, 100, 1000), 0);
    }

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
