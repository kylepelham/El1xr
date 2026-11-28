//! (texflicker, #96) `VICTORY_PRESENT_OWN_FRAME` (live; off unless set): a
//! present waits for its own frame. After MAXAWER d6e8925688, whose fix for
//! one-frame dark patches and pictures stepping back (tile flashes per run
//! 163-207 down to 36-39) is that a present is no longer sent ahead of its
//! frame's submissions.
//!
//! - `1`: the present is not sent ahead ([`crate::host::late_free_present`]
//!   says no while set): the flights in the air land before the back buffer
//!   is copied, the copy runs and is waited for, and then the swap chain
//!   presents. That is `VICTORY_LATE_WAIT=2`'s present; the relay and the
//!   rest of the late stages stay as they are.
//! - `2`: as `1`, and the title's present call returns only once its present
//!   has run. With `VICTORY_ASYNC_PRESENT` (on with `VICTORY_ASYNC_SUBMIT`)
//!   the call returns at once and the title records the next frame, its
//!   submissions kept aside, while this frame's last submissions are
//!   prepared and sent.
//!
//! Lines: `present own frame:` when it is first seen set and then every 600
//! presents while set: the level, presents, the mean ms a present waited
//! for the flights to land and the mean ms the title waited in its call.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Condvar, Mutex};

/// `VICTORY_PRESENT_OWN_FRAME`: 0 (off), 1 or 2.
pub(crate) fn level() -> u64 {
    static LEVEL: victory_core::settings::Live =
        victory_core::settings::Live::new("VICTORY_PRESENT_OWN_FRAME", parse);
    LEVEL.get()
}

fn parse(value: Option<&str>) -> u64 {
    value
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map_or(0, |n| n.min(2))
}

/// Whether a present at late stage `stage` (`host::late_stage`) is sent
/// ahead as a flight of its own with the switch at `level`.
pub(crate) fn sent_ahead(stage: u8, level: u64) -> bool {
    stage > 2 && level == 0
}

/// Whether the title's present call waits until its present has run.
pub(crate) fn title_waits(level: u64) -> bool {
    level >= 2
}

/// Presents handed to the thread that runs the queues' lines, and those it
/// has run, in order: a present's ticket is its number among those handed.
pub(crate) struct Turns {
    counts: Mutex<(u64, u64)>,
    moved: Condvar,
}

impl Turns {
    pub(crate) const fn new() -> Self {
        Turns {
            counts: Mutex::new((0, 0)),
            moved: Condvar::new(),
        }
    }

    /// A present is handed over: its ticket.
    pub(crate) fn ask(&self) -> u64 {
        let mut c = self.counts.lock().unwrap_or_else(|e| e.into_inner());
        c.0 += 1;
        c.0
    }

    /// The oldest present handed over and not run has run.
    pub(crate) fn ran(&self) {
        let mut c = self.counts.lock().unwrap_or_else(|e| e.into_inner());
        c.1 = (c.1 + 1).min(c.0);
        drop(c);
        self.moved.notify_all();
    }

    /// Whether the present with `ticket` has run.
    pub(crate) fn done(&self, ticket: u64) -> bool {
        self.counts.lock().unwrap_or_else(|e| e.into_inner()).1 >= ticket
    }

    /// Waits until the present with `ticket` has run.
    pub(crate) fn wait(&self, ticket: u64) {
        let mut c = self.counts.lock().unwrap_or_else(|e| e.into_inner());
        while c.1 < ticket {
            c = self.moved.wait(c).unwrap_or_else(|e| e.into_inner());
        }
    }
}

/// The presents of the title's queues.
pub(crate) static TURNS: Turns = Turns::new();

/// Presents counted since the last line, microseconds waiting for flights
/// to land, microseconds the title waited, all presents while set.
static STATS: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

/// A present ran at `level` (above 0) and waited `settle_us` for the
/// flights to land.
pub(crate) fn presented(level: u64, settle_us: u64) {
    if level == 0 {
        return;
    }
    let all = STATS[3].fetch_add(1, Relaxed);
    STATS[1].fetch_add(settle_us, Relaxed);
    let n = STATS[0].fetch_add(1, Relaxed) + 1;
    if all == 0 || n >= 600 {
        let settle = STATS[1].swap(0, Relaxed);
        let title = STATS[2].swap(0, Relaxed);
        STATS[0].store(0, Relaxed);
        tracing::info!(
            level,
            presents = n,
            settle_ms = format_args!("{:.2}", settle as f64 / n as f64 / 1000.0),
            title_wait_ms = format_args!("{:.2}", title as f64 / n as f64 / 1000.0),
            presents_while_set = all + 1,
            "present own frame: presents wait for their frame's flights to land, not sent ahead (VICTORY_PRESENT_OWN_FRAME; 2: the title's call waits too)"
        );
    }
}

/// The title waited `us` in its present call for its present to run.
pub(crate) fn title_waited(us: u64) {
    STATS[2].fetch_add(us, Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    #[test]
    fn levels_parse() {
        assert_eq!(parse(None), 0);
        assert_eq!(parse(Some("")), 0);
        assert_eq!(parse(Some("1")), 1);
        assert_eq!(parse(Some(" 2 ")), 2);
        assert_eq!(parse(Some("9")), 2);
        assert_eq!(parse(Some("x")), 0);
    }

    #[test]
    fn a_present_is_sent_ahead_only_at_stage_3_with_the_switch_off() {
        // The launcher's relay runs stage 3: presents sent ahead of their frame.
        assert!(sent_ahead(3, 0));
        assert!(!sent_ahead(3, 1), "PRESENT_OWN_FRAME=1 waits for the frame");
        assert!(!sent_ahead(3, 2));
        assert!(!sent_ahead(2, 0), "stage 2 settles at the present already");
        assert!(!sent_ahead(0, 0));
        assert!(!title_waits(0) && !title_waits(1) && title_waits(2));
    }

    #[test]
    fn level_is_live() {
        victory_core::settings::check_live("VICTORY_PRESENT_OWN_FRAME", "2", level, 0, 2);
    }

    #[test]
    fn a_ticket_is_done_by_its_own_present_not_an_earlier_one() {
        let turns = Turns::new();
        let first = turns.ask();
        let second = turns.ask();
        assert!(!turns.done(first) && !turns.done(second));
        turns.ran();
        assert!(turns.done(first));
        assert!(!turns.done(second), "the earlier present ran, not this one");
        turns.ran();
        assert!(turns.done(second));
        // More runs than presents handed over do not count ahead.
        turns.ran();
        let third = turns.ask();
        assert!(!turns.done(third));
    }

    #[test]
    fn the_title_waits_until_its_present_has_run() {
        let turns = Arc::new(Turns::new());
        let ticket = turns.ask();
        let returned = Arc::new(AtomicBool::new(false));
        let waiter = {
            let (turns, returned) = (turns.clone(), returned.clone());
            std::thread::spawn(move || {
                turns.wait(ticket);
                returned.store(true, Relaxed);
            })
        };
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(!returned.load(Relaxed), "returned before its present ran");
        turns.ran();
        waiter.join().unwrap();
        assert!(returned.load(Relaxed));
    }
}
