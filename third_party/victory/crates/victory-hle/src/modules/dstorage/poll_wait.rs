//! (worldstream, #26) `VICTORY_DSTORAGE_POLL_WAIT`: the title waits for its
//! DirectStorage reads by polling a status entry (`IsComplete`) in a
//! `Sleep(1)` loop. With the switch, a poll that finds the entry still
//! pending, from a thread that polled it pending a moment ago (in that
//! loop), waits for the entry here, up to the switch's microseconds, and
//! returns as soon as the workers write it, instead of returning at once
//! and leaving the title asleep until its next timer tick.
//!
//! Why: before the first bake pass (0-15.2 s after the first frame in b5q
//! `01_bo`), the loading screen streams at 87-162 MB/s with 0.64-1.81 of 32
//! workers busy: the reads are not the limit, the round trips are. Each
//! status the title waits on costs it what the reads take plus the rest of
//! a `Sleep(1)` (about 1-2 ms with the 1 ms timer) after they finish.
//!
//! Values: `0` (or unset) off; `-1` counts only (the line, no waiting: the
//! slack an OFF run leaves); `1` waits up to 2000 us; `N` >= 2 up to N us
//! (at most 20000). A first poll, or one more than [`LOOP_GAP_US`] after
//! the thread's last pending poll of the same entry (a check once a frame,
//! say), never waits.

use std::cell::Cell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicI32, AtomicU64, Ordering::Relaxed};
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Polls of the same entry this close together are the title's wait loop.
pub(super) const LOOP_GAP_US: u64 = 8_000;
const MOST_US: i64 = 20_000;
const DEFAULT_US: u64 = 2_000;

/// What the switch says: `None` off, `Some(0)` count only, `Some(us)` wait.
pub(super) fn mode() -> Option<u64> {
    static MODE: OnceLock<Option<u64>> = OnceLock::new();
    *MODE.get_or_init(|| {
        let mode = parse(std::env::var("VICTORY_DSTORAGE_POLL_WAIT").ok().as_deref());
        if let Some(us) = mode {
            tracing::info!(
                wait_us = us,
                loop_gap_us = LOOP_GAP_US,
                "dstorage: a status poll in the title's wait loop waits for its entry (VICTORY_DSTORAGE_POLL_WAIT; 0 us: counts only)"
            );
        }
        mode
    })
}

fn parse(value: Option<&str>) -> Option<u64> {
    match value.and_then(|v| v.trim().parse::<i64>().ok())? {
        0 => None,
        -1 => Some(0),
        1 => Some(DEFAULT_US),
        n if n >= 2 => Some(n.min(MOST_US) as u64),
        _ => None,
    }
}

fn epoch() -> Instant {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}

fn now_us() -> u64 {
    epoch().elapsed().as_micros() as u64
}

/// Rung when the workers write status entries.
static BELL: (Mutex<()>, Condvar) = (Mutex::new(()), Condvar::new());

/// When each entry polled pending was written: `(array, index)` -> us.
static WRITTEN: Mutex<Option<HashMap<(usize, u32), u64>>> = Mutex::new(None);

/// Since the last line: pending polls, of them in the loop, waits, waits
/// that saw the entry written, us waited, entries seen written after a
/// pending poll, us from their writing to the title seeing it (the slack),
/// us between a thread's pending polls (the title's sleep).
const PENDING: usize = 0;
const LOOPED: usize = 1;
const WAITS: usize = 2;
const WAITS_DONE: usize = 3;
const WAITED_US: usize = 4;
const SEEN: usize = 5;
const SLACK_US: usize = 6;
const GAP_US: usize = 7;
const SLACK_MAX_US: usize = 8;
static STATS: [AtomicU64; 9] = [const { AtomicU64::new(0) }; 9];

thread_local! {
    /// This thread's last pending poll: the entry and when.
    static LAST: Cell<Option<(usize, u32, u64)>> = const { Cell::new(None) };
}

/// Whether a pending poll at `now` of `key` follows this thread's last one
/// (`last`) closely enough to be its wait loop; the gap if so.
pub(super) fn in_loop(last: Option<(usize, u32, u64)>, key: (usize, u32), now: u64) -> Option<u64> {
    let (array, index, at) = last?;
    let gap = now.checked_sub(at)?;
    ((array, index) == key && gap <= LOOP_GAP_US).then_some(gap)
}

/// The workers wrote entry `index` of `array` (under the scheduler lock).
pub(super) fn written(array: usize, index: u32) {
    if mode().is_none() {
        return;
    }
    {
        let mut map = WRITTEN.lock().unwrap_or_else(|e| e.into_inner());
        let map = map.get_or_insert_with(HashMap::new);
        if map.len() >= 4096 {
            map.clear();
        }
        map.insert((array, index), now_us());
    }
    let _ring = BELL.0.lock().unwrap_or_else(|e| e.into_inner());
    BELL.1.notify_all();
}

/// `IsComplete` of `entry` (entry `index` of `array`): `pending` reads it.
pub(super) fn poll(
    array: usize,
    index: u32,
    entry: &AtomicI32,
    pending: impl Fn(&AtomicI32) -> bool,
) -> bool {
    let Some(wait_us) = mode() else {
        return !pending(entry);
    };
    let key = (array, index);
    let now = now_us();
    if !pending(entry) {
        seen(key, now);
        return true;
    }
    STATS[PENDING].fetch_add(1, Relaxed);
    let looped = in_loop(LAST.with(Cell::get), key, now);
    LAST.with(|l| l.set(Some((array, index, now))));
    let Some(gap) = looped else {
        line();
        return false;
    };
    STATS[LOOPED].fetch_add(1, Relaxed);
    STATS[GAP_US].fetch_add(gap, Relaxed);
    if wait_us == 0 {
        line();
        return false;
    }
    STATS[WAITS].fetch_add(1, Relaxed);
    let began = Instant::now();
    let done = wait_for(entry, &pending, Duration::from_micros(wait_us));
    STATS[WAITED_US].fetch_add(began.elapsed().as_micros() as u64, Relaxed);
    if done {
        STATS[WAITS_DONE].fetch_add(1, Relaxed);
        seen(key, now_us());
    }
    line();
    done
}

/// Waits up to `most` for `entry` to be written: whether it was.
fn wait_for(entry: &AtomicI32, pending: &impl Fn(&AtomicI32) -> bool, most: Duration) -> bool {
    let deadline = Instant::now() + most;
    let mut guard = BELL.0.lock().unwrap_or_else(|e| e.into_inner());
    loop {
        if !pending(entry) {
            return true;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return false;
        }
        guard = BELL
            .1
            .wait_timeout(guard, left)
            .unwrap_or_else(|e| e.into_inner())
            .0;
    }
}

/// The title sees `key` written at `now`: the slack since the workers wrote
/// it, if this thread polled it pending.
fn seen(key: (usize, u32), now: u64) {
    let polled = LAST.with(|l| match l.get() {
        Some((a, i, _)) if (a, i) == key => {
            l.set(None);
            true
        }
        _ => false,
    });
    if !polled {
        return;
    }
    let at = WRITTEN
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_mut()
        .and_then(|m| m.remove(&key));
    if let Some(at) = at {
        let slack = now.saturating_sub(at);
        STATS[SEEN].fetch_add(1, Relaxed);
        STATS[SLACK_US].fetch_add(slack, Relaxed);
        STATS[SLACK_MAX_US].fetch_max(slack, Relaxed);
    }
}

/// Every 5 s, a line of [`STATS`] (taken back to 0).
fn line() {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let now = now_us();
    let next = NEXT.load(Relaxed);
    if now < next
        || NEXT
            .compare_exchange(next, now + 5_000_000, Relaxed, Relaxed)
            .is_err()
    {
        return;
    }
    if next == 0 {
        return;
    }
    let s = STATS.each_ref().map(|s| s.swap(0, Relaxed));
    let avg = |sum: u64, n: u64| {
        if n == 0 {
            0.0
        } else {
            sum as f64 / n as f64 / 1000.0
        }
    };
    tracing::info!(
        "dstorage poll wait (VICTORY_DSTORAGE_POLL_WAIT) over 5 s: {} polls found their entry pending, {} in a wait loop \
         (the title slept {:.2} ms between them on average); {} waited here ({} saw it written, {:.1} ms waited in all); \
         {} entries seen written after a pending poll, {:.2} ms after the workers wrote them on average ({:.2} ms at most, {:.0} ms in all)",
        s[PENDING],
        s[LOOPED],
        avg(s[GAP_US], s[LOOPED]),
        s[WAITS],
        s[WAITS_DONE],
        s[WAITED_US] as f64 / 1000.0,
        s[SEEN],
        avg(s[SLACK_US], s[SEEN]),
        s[SLACK_MAX_US] as f64 / 1000.0,
        s[SLACK_US] as f64 / 1000.0,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_switch_reads_off_count_and_microseconds() {
        assert_eq!(parse(None), None);
        assert_eq!(parse(Some("0")), None);
        assert_eq!(parse(Some("-1")), Some(0));
        assert_eq!(parse(Some("1")), Some(DEFAULT_US));
        assert_eq!(parse(Some("500")), Some(500));
        assert_eq!(parse(Some("999999")), Some(MOST_US as u64));
        assert_eq!(parse(Some("-5")), None);
        assert_eq!(parse(Some("x")), None);
    }

    #[test]
    fn only_a_close_repeat_of_the_same_entry_is_the_loop() {
        assert_eq!(in_loop(None, (1, 0), 100), None);
        assert_eq!(in_loop(Some((1, 0, 100)), (1, 0), 1600), Some(1500));
        assert_eq!(
            in_loop(Some((1, 0, 100)), (1, 1), 1600),
            None,
            "another entry"
        );
        assert_eq!(
            in_loop(Some((2, 0, 100)), (1, 0), 1600),
            None,
            "another array"
        );
        assert_eq!(
            in_loop(Some((1, 0, 100)), (1, 0), 100 + LOOP_GAP_US + 1),
            None,
            "once a frame"
        );
    }

    /// A wait ends as soon as another thread writes the entry and rings.
    #[test]
    fn a_wait_ends_when_the_entry_is_written() {
        static ENTRY: AtomicI32 = AtomicI32::new(1);
        let pending = |e: &AtomicI32| e.load(std::sync::atomic::Ordering::Acquire) == 1;
        let writer = std::thread::spawn(|| {
            std::thread::sleep(Duration::from_millis(5));
            ENTRY.store(0, std::sync::atomic::Ordering::Release);
            let _ring = BELL.0.lock().unwrap();
            BELL.1.notify_all();
        });
        let began = Instant::now();
        assert!(wait_for(&ENTRY, &pending, Duration::from_secs(5)));
        assert!(began.elapsed() < Duration::from_secs(2));
        writer.join().unwrap();
        // Still pending past the time: false, no hang.
        static NEVER: AtomicI32 = AtomicI32::new(1);
        let began = Instant::now();
        assert!(!wait_for(&NEVER, &pending, Duration::from_millis(3)));
        assert!(began.elapsed() >= Duration::from_millis(3));
    }

    /// The dstorage suite's status test with the switch on (a child
    /// process: the switch is read once): every status completes, in order,
    /// with its result, through polls that wait.
    #[test]
    fn statuses_complete_with_the_switch_on() {
        const TEST: &str = "modules::dstorage::tests::statuses_complete_in_order_with_the_first_failure";
        if std::env::var_os("VICTORY_DSTORAGE_POLL_WAIT").is_some() {
            return;
        }
        for value in ["1", "-1"] {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([TEST, "--exact", "--test-threads=1"])
                .env("VICTORY_DSTORAGE_POLL_WAIT", value)
                .output()
                .unwrap();
            let out = String::from_utf8_lossy(&output.stdout);
            assert!(output.status.success(), "POLL_WAIT={value}: {out}");
            assert!(out.contains("1 passed"), "POLL_WAIT={value}: {out}");
        }
    }
}
