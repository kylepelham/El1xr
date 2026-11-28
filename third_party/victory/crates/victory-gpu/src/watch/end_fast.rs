//! (worldstream) `VICTORY_ASK_END_FAST=1` (live; off by default): ending the
//! submission's write-watch asking (`ask_once_end`) under
//! `VICTORY_WATCH_RESERVATION=1` takes the journal's write lock only when
//! there is something to end: an asking stands (`KEPT`), one is under way
//! ([`ASKING`]), or an asking ahead left its ranges ([`AHEAD_OPEN`]).
//!
//! Without the switch every end took the lock, at every submission's asking
//! (which ends the last one first) and after every batch flush, and waited
//! for the refresh threads that held it to read. With nothing standing the
//! end changed nothing: no asking kept, no ranges, and the ranges' order is
//! read only with ranges. The count of ends ([`super::reservation::outside::ENDS`])
//! is still taken first, so an asking made outside the lock meanwhile is
//! published ended as before.
//!
//! Order: the end counts itself, then reads [`ASKING`], `KEPT` and
//! [`AHEAD_OPEN`] (all sequentially consistent). An asking counts itself in
//! [`ASKING`] before its kernel calls and leaves after it has published
//! `KEPT`; an asking ahead sets [`AHEAD_OPEN`] before its kernel calls. So
//! an end that finds all three clear came before any asking it did not end:
//! as the lock would have ordered it, with the end first.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst};
use victory_core::settings::Live;

/// Whether an end with nothing standing leaves the journal's lock alone.
pub(in crate::watch) fn on() -> bool {
    static ON: Live = Live::flag("VICTORY_ASK_END_FAST");
    ON.get_bool()
}

/// Reservation askings under way: from before their kernel calls to after
/// they have published their asking (`KEPT`).
pub(in crate::watch) static ASKING: AtomicUsize = AtomicUsize::new(0);

/// An asking ahead (`ask_ahead`, kept false) may have left ranges in the
/// journal: set before its kernel calls, cleared with its ranges.
pub(in crate::watch) static AHEAD_OPEN: AtomicBool = AtomicBool::new(false);

/// Counts a reservation asking in [`ASKING`] while it lives.
pub(in crate::watch) struct Under(());

impl Under {
    pub(in crate::watch) fn begin() -> Self {
        ASKING.fetch_add(1, SeqCst);
        Self(())
    }
}

impl Drop for Under {
    fn drop(&mut self) {
        ASKING.fetch_sub(1, SeqCst);
    }
}

/// Whether an end must take the lock, from what stands.
fn needed_from(asking: usize, kept: bool, ahead_open: bool) -> bool {
    asking != 0 || kept || ahead_open
}

/// Whether an end may return without the lock (the switch on and nothing to
/// end). Read after the end was counted.
pub(in crate::watch) fn skips() -> bool {
    if !on() {
        return false;
    }
    let asking = ASKING.load(SeqCst);
    let kept = super::KEPT.load(SeqCst);
    let ahead_open = AHEAD_OPEN.load(SeqCst);
    !needed_from(asking, kept, ahead_open)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Anything standing or under way takes the lock; nothing at all does not.
    #[test]
    fn an_end_takes_the_lock_only_with_something_to_end() {
        assert!(!needed_from(0, false, false));
        assert!(needed_from(1, false, false));
        assert!(needed_from(0, true, false));
        assert!(needed_from(0, false, true));
        assert!(needed_from(2, true, true));
    }

    /// An asking counts itself while it lives, nested ones too.
    #[test]
    fn askings_under_way_are_counted_while_they_live() {
        let before = ASKING.load(SeqCst);
        {
            let _a = Under::begin();
            assert!(ASKING.load(SeqCst) > before);
            {
                let _b = Under::begin();
                assert!(ASKING.load(SeqCst) >= before + 2);
            }
        }
        assert!(ASKING.load(SeqCst) <= before + 1);
    }

    /// The journal an end with nothing standing leaves is the one the end
    /// under the lock leaves: no asking kept, no ranges.
    #[test]
    fn an_end_with_nothing_standing_changes_nothing() {
        let mut j = super::super::Journal {
            found: Vec::new(),
            ahead: Vec::new(),
            ordered: true,
            ahead_asked: 9,
            kept: false,
        };
        let covered = |j: &super::super::Journal| super::super::reservation::covered(j, 3, 4);
        let before = (j.kept, j.ahead.clone(), j.ahead_asked, covered(&j));
        // What the end under the lock does.
        j.kept = false;
        j.ahead.clear();
        j.ordered = false;
        assert_eq!(
            before,
            (j.kept, j.ahead.clone(), j.ahead_asked, covered(&j))
        );
    }
}
