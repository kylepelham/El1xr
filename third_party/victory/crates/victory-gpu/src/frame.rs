//! Frame pacing: the title sets a frame interval and waits for each frame
//! to begin. Ours is a clock started by the first interval set, ticking
//! once per interval; the token is the frame number. Each event kind
//! (origin, vblank, flip) happens once per tick, so two waits for the same
//! kind never end on the same tick: the title polls the origin
//! with "token on time-out" (starting its next frame at once) and is paced
//! by a blocking wait for the flip before each present.
//!
//! shortcut: event offsets are ignored (every event is at the tick); tie
//! the clock to presentation if a title paces by them.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::object::{HRESULT, S_FALSE, S_OK, This};

/// What a wait returns when its time is up first.
const TIMEOUT: HRESULT = S_FALSE;
/// The frame-origin event: the one that starts frames and makes tokens.
const ORIGIN: u32 = 0;
/// Make the frame's token even if the wait times out (the title starts the
/// frame early).
const TOKEN_ON_TIMEOUT: u32 = 2;
const SIXTIETH: Duration = Duration::from_micros(16_667);

struct Clock {
    start: Instant,
    interval: Duration,
    /// The tick number at `start` (ticks go on across interval changes).
    first: u64,
}

static CLOCK: Mutex<Option<Clock>> = Mutex::new(None);
static FRAME: AtomicU64 = AtomicU64::new(0);

fn clock() -> std::sync::MutexGuard<'static, Option<Clock>> {
    CLOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Device method: a frame every `length` microseconds. The period is how
/// many intervals a frame may take to reach the display (pipeline depth),
/// not part of the rate.
pub(crate) extern "system" fn set_interval(
    _: This,
    _output: This,
    length: u32,
    _period: u32,
    _flags: u32,
) -> HRESULT {
    let interval = match length {
        0 => SIXTIETH,
        us => Duration::from_micros(u64::from(us)),
    };
    let interval = crate::load_rate::playing_interval().unwrap_or(interval);
    tracing::info!(?interval, "frame interval");
    // A new interval starts from now, after the last tick handed out.
    let first = LAST
        .iter()
        .map(|t| t.load(Ordering::Relaxed))
        .max()
        .unwrap_or(0);
    *clock() = Some(Clock {
        start: Instant::now(),
        interval,
        first,
    });
    S_OK
}

/// Our clock at `interval` from now, after the last tick handed out (as a
/// title's new interval starts).
pub(crate) fn restart(interval: Duration) {
    let first = LAST
        .iter()
        .map(|t| t.load(Ordering::Relaxed))
        .max()
        .unwrap_or(0);
    *clock() = Some(Clock {
        start: Instant::now(),
        interval,
        first,
    });
}

/// (bootfast) Our clock started again from now at its own interval, after
/// the last tick handed out: frame waits that did not sleep
/// (`VICTORY_LOAD_UNPACED`) took ticks ahead of the clock, and paced again
/// the next wait would sleep until them.
pub(crate) fn rebase() {
    let interval = clock().as_ref().map_or(SIXTIETH, |c| c.interval);
    restart(interval);
}

/// Device method: events are scheduled at every tick.
pub(crate) extern "system" fn schedule(
    _: This,
    _kind: u32,
    _offset: u32,
    _list: *const u8,
    _flags: u32,
) -> HRESULT {
    S_OK
}

/// (Neptune) Frame waits since they were last taken ([`waits_taken`]): counts of waits that
/// ended on a tick (origin, other) and that timed out (origin, other), the microseconds
/// slept in each, and the longest time-out asked for (ms).
static WAITS: [AtomicU64; 9] = [const { AtomicU64::new(0) }; 9];

/// [`WAITS`], taken.
pub(crate) fn waits_taken() -> [u64; 9] {
    std::array::from_fn(|i| WAITS[i].swap(0, Ordering::Relaxed))
}

/// Frames begun so far (frame waits that ended).
/// (Neptune) `VICTORY_PACE=0`: the waits do not sleep.
static PACED: victory_core::settings::Live =
    victory_core::settings::Live::new("VICTORY_PACE", |v| u64::from(v.is_none_or(|v| v != "0")));

pub(crate) fn current() -> u64 {
    FRAME.load(Ordering::Relaxed)
}

/// Per event kind (origin, vblank, flip and later kinds): the last tick a
/// wait for it ended on.
static LAST: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

fn last(kind: u32) -> &'static AtomicU64 {
    &LAST[(kind as usize).min(LAST.len() - 1)]
}

/// The next tick after now and after the last one a wait for `kind` ended
/// on (an event happens once per tick: a wait woken a little early must not
/// let the next one end on the same tick), and how long until it. When
/// `reserve` is `Some((limit, late_waits))`, reserves a successful tick under
/// the clock lock so concurrent waiters for the same event advance to
/// successive ticks.
fn next_tick(kind: u32, reserve: Option<(Duration, bool)>) -> (u64, Duration) {
    let mut clock = clock();
    let c = clock.get_or_insert_with(|| Clock {
        start: Instant::now(),
        interval: SIXTIETH,
        first: 0,
    });
    let elapsed = c.start.elapsed().as_nanos();
    let interval = c.interval.as_nanos().max(1);
    let tick =
        (c.first + (elapsed / interval + 1) as u64).max(last(kind).load(Ordering::Relaxed) + 1);
    let at = u128::from(tick - c.first) * interval;
    let delay = Duration::from_nanos(at.saturating_sub(elapsed) as u64);
    if let Some((limit, late_waits)) = reserve {
        let (tick, delay) =
            if !late_waits && limit >= delay && tick > last(kind).load(Ordering::Relaxed) + 1 {
                (tick - 1, Duration::ZERO)
            } else {
                (tick, delay)
            };
        if delay <= limit {
            last(kind).fetch_max(tick, Ordering::Relaxed);
        }
        return (tick, delay);
    }
    (tick, delay)
}

/// Device method: blocks until the next tick (or the time-out); an origin
/// wait then hands out the new frame's token. Other events keep the token
/// the title passes. Ticks are absolute (start plus whole intervals), so
/// lateness never accumulates.
pub(crate) extern "system" fn wait(
    _: This,
    kind: u32,
    timeout_ms: u32,
    _list: *const u8,
    flags: u32,
    token: *mut u64,
) -> HRESULT {
    let origin = kind == ORIGIN;
    crate::load_rate::gate(origin);
    if origin {
        crate::boot_prof::tick();
    }
    let limit = Duration::from_millis(u64::from(timeout_ms));
    // A blocking wait for an event that has happened since the last wait
    // for it ended (a frame took longer than a tick) ends at once, on the
    // tick that passed: sleeping on to the next one cost 17 ms of every
    // 730 ms frame (run 58: "2.0 frame waits sleeping 16.8 ms"). Polls (a
    // time-out shorter than a tick) answer as before.
    // `VICTORY_LATE_WAITS=1` sleeps to the next tick, as before.
    static SLEEP: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let late_waits =
        *SLEEP.get_or_init(|| std::env::var_os("VICTORY_LATE_WAITS").is_some_and(|v| v == "1"));
    let (tick, wait) = next_tick(kind, Some((limit, late_waits)));
    // (Neptune) `VICTORY_PACE=0`: no wait sleeps (a poll whose tick is not due times out at
    // once, a blocking wait ends at once on the next tick): an experiment, to see how much
    // of a slow frame is the title waiting for its own clock.
    // Live: each wait takes it as it is.
    let paced = PACED.get_bool() && crate::load_rate::paced_now();
    let started = Instant::now();
    let hr = if wait > limit {
        if paced {
            std::thread::sleep(limit);
        }
        TIMEOUT
    } else {
        if paced {
            std::thread::sleep(wait);
        }
        last(kind).fetch_max(tick, Ordering::Relaxed);
        S_OK
    };
    // Since the last line in the log: waits that ended on a tick and waits that timed out,
    // by kind (origin, other), and the microseconds slept in them.
    let slot = usize::from(hr != S_OK) * 2 + usize::from(!origin);
    WAITS[slot].fetch_add(1, Ordering::Relaxed);
    WAITS[4 + slot].fetch_add(started.elapsed().as_micros() as u64, Ordering::Relaxed);
    crate::boot_prof::paced(origin, started.elapsed().as_micros() as u64);
    WAITS[8].fetch_max(u64::from(timeout_ms.min(100_000)), Ordering::Relaxed);
    #[cfg(all(windows, feature = "diagnostics"))]
    {
        use crate::copystat::{C, add};
        add(C::PaceWaits, 1);
        add(C::PaceUs, wait.min(limit).as_micros() as u64);
    }
    // A frame begun on a time-out takes no tick: its flip still does.
    if origin && (hr == S_OK || flags & TOKEN_ON_TIMEOUT != 0) {
        // Tokens count frames (consecutive), not ticks.
        let frame = FRAME.fetch_add(1, Ordering::Relaxed) + 1;
        if !token.is_null() {
            // SAFETY: checked non-null; the title passes its token.
            unsafe { token.write(frame) };
        }
    }
    #[cfg(feature = "diagnostics")]
    tracing::debug!(
        kind,
        timeout_ms,
        flags,
        hr,
        tick,
        wait_us = wait.as_micros() as u64,
        frame = current(),
        "frame wait"
    );
    hr
}

/// Device method: no statistics are kept, so none are returned.
pub(crate) extern "system" fn statistics(
    _: This,
    _token: u64,
    _kinds: u32,
    count: *mut u32,
    _out: *mut u8,
) -> HRESULT {
    if !count.is_null() {
        // SAFETY: checked non-null.
        unsafe { count.write(0) };
    }
    S_OK
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pace_switch_is_live() {
        victory_core::settings::check_live("VICTORY_PACE", "0", || PACED.get_bool(), true, false);
    }

    #[test]
    fn waits_end_on_ticks_and_count_frames() {
        let null = std::ptr::null();
        assert_eq!(set_interval(null, null, 2_000, 1, 0), S_OK);
        let mut first = 0;
        assert_eq!(wait(null, 0, 1_000, std::ptr::null(), 0, &mut first), S_OK);
        let before = Instant::now();
        let mut second = 0;
        assert_eq!(wait(null, 0, 1_000, std::ptr::null(), 0, &mut second), S_OK);
        assert!(second > first);
        assert!(before.elapsed() <= Duration::from_millis(50));
        assert_eq!(wait(null, 0, 0, std::ptr::null(), 0, &mut second), TIMEOUT);
        // Tokens count frames, a frame started early on a time-out too.
        let mut early = 0;
        let flags = TOKEN_ON_TIMEOUT;
        assert_eq!(
            wait(null, 0, 0, std::ptr::null(), flags, &mut early),
            TIMEOUT
        );
        let mut next = 0;
        assert_eq!(wait(null, 0, 1_000, std::ptr::null(), 0, &mut next), S_OK);
        assert!(early == second + 1 && next == early + 1);
        assert!(LAST[0].load(Ordering::Relaxed) >= 2);

        // The title's loop (the clock is global, so in the same test): an
        // origin poll that times out (and starts the frame), then a
        // blocking flip wait. Each flip takes its own tick.
        const FLIP: u32 = 2;
        const FLIP_INTERVAL: Duration = Duration::from_millis(5);
        assert_eq!(set_interval(null, null, 5_000, 3, 0), S_OK);
        let (start, first) = {
            let c = clock();
            let c = c.as_ref().expect("interval set");
            (c.start, c.first)
        };
        let deadline = |tick: u64| start + FLIP_INTERVAL * tick.saturating_sub(first) as u32;
        let mut token = 0;
        let flips: Vec<(u64, bool)> = (0..4)
            .map(|_| {
                let _ = wait(null, 0, 0, std::ptr::null(), TOKEN_ON_TIMEOUT, &mut token);
                let previous = LAST[2].load(Ordering::Relaxed);
                assert_eq!(
                    wait(null, FLIP, u32::MAX, std::ptr::null(), 0, &mut token),
                    S_OK
                );
                // Asked after the tick following the last flip had passed (the test
                // thread was descheduled): the wait ends at once on the tick that
                // passed, a later one. Only a wait that ended after that can have.
                let late = Instant::now() >= deadline(previous + 2);
                (LAST[2].load(Ordering::Relaxed), late)
            })
            .collect();
        assert!(
            flips
                .windows(2)
                .all(|w| w[1].0 == w[0].0 + 1 || (w[1].1 && w[1].0 > w[0].0)),
            "{flips:?}"
        );
        // Even when a flip wait begins on the tick another just ended on.
        let (tick, _) = next_tick(FLIP, None);
        assert!(tick > flips[3].0);
        // Reserving two successive blocking waits under the clock lock yields distinct ticks.
        let (t1, _) = next_tick(FLIP, Some((Duration::from_secs(1), true)));
        let (t2, _) = next_tick(FLIP, Some((Duration::from_secs(1), true)));
        assert!(t2 > t1);

        // (bootfast) Waits that did not sleep (`VICTORY_LOAD_UNPACED`) took
        // ticks a second ahead of the clock; rebased, the next is within an
        // interval, after every tick handed out.
        let ahead = next_tick(ORIGIN, None).0 + 200;
        LAST[0].fetch_max(ahead, Ordering::Relaxed);
        let (_, delay) = next_tick(ORIGIN, None);
        assert!(delay > Duration::from_millis(500), "{delay:?}");
        rebase();
        let (tick, delay) = next_tick(ORIGIN, None);
        assert!(delay <= FLIP_INTERVAL, "{delay:?}");
        assert!(tick > ahead);
    }
}
