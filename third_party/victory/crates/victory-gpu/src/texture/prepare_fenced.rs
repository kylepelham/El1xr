//! (texflicker, #96) `VICTORY_PREPARE_FENCED=1` (live; off unless set):
//! what the prepare queue lets go (entries the prune, a take, a served
//! slot, a released texture or a replaced entry drop: their staging
//! buffers) and the upload blocks the staging pool lets go
//! (`crate::texpool`, `VICTORY_STAGING_BLOCKS`) wait until every submission
//! begun when they were let go, and [`LAG`] more, has finished, and are then
//! dropped on `victory-reaper` (`crate::host::retire`), never on the thread
//! that let them go.
//!
//! Without it they are dropped where they are let go: the prune and the
//! takes run on the threads that record the title's lists (the texture pass,
//! a table's read), and a staging block is let go (unmapped and freed, 20-26
//! ms each in b5 part P's `pool time (staging)` lines) by whichever thread
//! takes the next piece. In b5 part P the diner's dark frames came every
//! ~2.06 s, one prune in the frame before in 10-13 of 18, with a cohort of
//! the same entries (4 entries, 45,225 KiB) made, never taken and pruned
//! every ~2.0 s; a dark frame ran a part of its work (`ran=5722/8364/13904`
//! dark/before/after): the rest came a frame late, as from a list held up.
//!
//! What the prune drops was already held to the card's order: a list that
//! recorded an upload from a staging buffer holds it until its flight has
//! landed (its fence reached), and a staging piece given back rests until
//! the submissions begun before it have finished (`crate::texpool`, test
//! `a_piece_rests_until_the_submissions_begun_before_have_finished`). The
//! switch adds that order to the entries themselves, in case a holder is
//! missed, and takes the drops off the threads that record.

use std::any::Any;
use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// Submissions past the newest begun that must finish too: one may be in
/// preparation, its lists recorded, its number not taken yet (as
/// `crate::texpool`'s pieces rest).
const LAG: u64 = 2;

pub(crate) fn on() -> bool {
    static ON: victory_core::settings::Live =
        victory_core::settings::Live::flag("VICTORY_PREPARE_FENCED");
    ON.get_bool()
}

/// What was let go, by the submissions begun when it was.
pub(crate) struct Graves<T> {
    held: VecDeque<(u64, T)>,
}

impl<T> Graves<T> {
    pub(crate) const fn new() -> Graves<T> {
        Graves {
            held: VecDeque::new(),
        }
    }

    /// `object`, let go while `begun` submissions had begun.
    pub(crate) fn bury(&mut self, begun: u64, object: T) {
        self.held.push_back((begun, object));
    }

    /// What every submission begun before it was let go (and [`LAG`] more)
    /// has passed, now that `finished` have finished.
    pub(crate) fn ripe(&mut self, finished: u64) -> Vec<T> {
        let mut ripe = Vec::new();
        while let Some(&(begun, _)) = self.held.front()
            && begun + LAG <= finished
        {
            ripe.extend(self.held.pop_front().map(|(_, object)| object));
        }
        ripe
    }

    pub(crate) fn len(&self) -> usize {
        self.held.len()
    }
}

struct Sendable(#[allow(dead_code)] Box<dyn Any>);
// SAFETY: staging buffers and upload blocks of the device: buffers mapped
// once, whose drop gives them back or releases them, on any thread (they
// were dropped by whichever thread let them go before).
unsafe impl Send for Sendable {}

static GRAVES: Mutex<Graves<Sendable>> = Mutex::new(Graves::new());

/// Since the last `prepare fenced:` line: entries and blocks buried; drops
/// done where they were let go (switch off), their microseconds, the
/// longest.
static COUNTS: [AtomicU64; 5] = [const { AtomicU64::new(0) }; 5];
const ENTRIES: usize = 0;
const BLOCKS: usize = 1;
const HERE: usize = 2;
const HERE_US: usize = 3;
const LONGEST_US: usize = 4;

/// What becomes of what is let go.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Fate {
    /// Dropped where it is let go, at once (as before).
    Here,
    /// Buried until the submissions begun before have finished.
    Buried,
}

pub(crate) fn fate(fenced: bool, count: usize) -> Fate {
    if fenced && count > 0 {
        Fate::Buried
    } else {
        Fate::Here
    }
}

/// Lets go of `gone`, `count` entries of the prepare queue (`blocks`: of
/// the staging pool): with the switch, buried; else dropped here, timed.
/// The microseconds it took here.
pub(crate) fn let_go<T: 'static>(gone: T, count: usize, blocks: bool) -> u64 {
    let started = std::time::Instant::now();
    match fate(on(), count) {
        Fate::Buried => {
            COUNTS[if blocks { BLOCKS } else { ENTRIES }].fetch_add(count as u64, Relaxed);
            let ripe = {
                let mut graves = GRAVES.lock().unwrap_or_else(|e| e.into_inner());
                graves.bury(crate::host::submissions_started(), Sendable(Box::new(gone)));
                graves.ripe(crate::host::submissions_finished())
            };
            if !ripe.is_empty() {
                crate::host::retire(ripe);
            }
        }
        Fate::Here => {
            drop(gone);
            if count > 0 {
                let us = started.elapsed().as_micros() as u64;
                COUNTS[HERE].fetch_add(1, Relaxed);
                COUNTS[HERE_US].fetch_add(us, Relaxed);
                COUNTS[LONGEST_US].fetch_max(us, Relaxed);
            }
        }
    }
    started.elapsed().as_micros() as u64
}

/// Called by every prune (each texture pass): hands what has ripened to
/// `victory-reaper`, and at most every five seconds writes the line (grep
/// `prepare fenced:`).
pub(crate) fn tick() {
    let (ripe, waiting) = {
        let mut graves = GRAVES.lock().unwrap_or_else(|e| e.into_inner());
        let ripe = graves.ripe(crate::host::submissions_finished());
        (ripe, graves.len())
    };
    if !ripe.is_empty() {
        crate::host::retire(ripe);
    }
    static LAST: AtomicU64 = AtomicU64::new(0);
    let now = crate::host::now_ms();
    let last = LAST.load(Relaxed);
    if now < last + 5000 || LAST.compare_exchange(last, now, Relaxed, Relaxed).is_err() {
        return;
    }
    let c = COUNTS.each_ref().map(|c| c.swap(0, Relaxed));
    if c.iter().all(|&v| v == 0) && waiting == 0 {
        return;
    }
    tracing::info!("{}", line(c, waiting, on()));
}

fn line(c: [u64; 5], waiting: usize, on: bool) -> String {
    format!(
        "prepare fenced: {} entries and {} staging blocks let go after the submissions begun before had finished \
         (dropped on victory-reaper), {waiting} lots waiting; dropped where let go: {} drops in {:.1} ms, \
         the longest {} us (VICTORY_PREPARE_FENCED={})",
        c[ENTRIES],
        c[BLOCKS],
        c[HERE],
        c[HERE_US] as f64 / 1e3,
        c[LONGEST_US],
        u8::from(on)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// The ordering the switch keeps: a staging buffer the prune lets go
    /// while a list that copies from it may still be on the card (submission
    /// 5 begun, 3 finished) is not given back for reuse until submission 5
    /// and the [`LAG`] after it have finished. Without it, it went at once.
    #[test]
    fn a_let_go_waits_for_the_submissions_begun_before() {
        let buffer = Arc::new(());
        let mut graves = Graves::new();
        assert_eq!(fate(true, 1), Fate::Buried);
        graves.bury(5, Arc::clone(&buffer));
        for finished in [3, 5, 6] {
            assert!(graves.ripe(finished).is_empty(), "given back at {finished}");
            assert_eq!(Arc::strong_count(&buffer), 2);
        }
        let ripe = graves.ripe(7);
        assert_eq!(ripe.len(), 1);
        drop(ripe);
        assert_eq!(Arc::strong_count(&buffer), 1);
        assert_eq!(graves.len(), 0);
    }

    /// Off (as before), what is let go is dropped where it is, at once:
    /// nothing is buried.
    #[test]
    fn off_drops_at_once() {
        assert_eq!(fate(false, 4), Fate::Here);
        assert_eq!(fate(true, 0), Fate::Here);
    }

    /// Lots ripen in the order they were let go.
    #[test]
    fn lots_ripen_in_order() {
        let mut graves = Graves::new();
        graves.bury(1, 'a');
        graves.bury(4, 'b');
        graves.bury(4, 'c');
        assert_eq!(graves.ripe(3), ['a']);
        assert_eq!(graves.ripe(5), [] as [char; 0]);
        assert_eq!(graves.ripe(6), ['b', 'c']);
    }

    #[test]
    fn the_line_names_the_switch() {
        let text = line([3, 1, 2, 1500, 900], 4, true);
        assert!(text.starts_with("prepare fenced: 3 entries and 1 staging blocks"));
        assert!(text.contains("2 drops in 1.5 ms, the longest 900 us"));
        assert!(text.ends_with("(VICTORY_PREPARE_FENCED=1)"));
    }

    #[test]
    fn the_switch_is_live_and_off_unless_set() {
        if std::env::var_os("VICTORY_PREPARE_FENCED").is_none() {
            assert!(!on());
        }
        victory_core::settings::check_live("VICTORY_PREPARE_FENCED", "1", on, false, true);
    }
}
