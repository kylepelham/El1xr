//! (feedcard, #88) `VICTORY_FRESH_OPEN=1` (Live; off by default): the new
//! host list each of the title's lists gets when it is submitted
//! (`crate::list::detach`, `detach_parts`) is left open for the title's next
//! call on its list, a reset, instead of closed at once and then reset again
//! by that reset (allocator and list). About 200 empty closes a frame on the
//! submitting thread upstream (3 % of its samples, about 1 ms a frame), and
//! as many needless resets on the threads that record.
//!
//! The list comes open and empty from the pool of landed lists (reset when
//! they landed) or new. Marked [`OPEN`], it stays so until:
//! - the title's reset (`crate::list`'s `reset`) marks it [`RESET`], and
//!   [`HostList::reset`](crate::host::HostList::reset) skips the D3D12 reset
//!   and the timing stamp a reset begins with: the list is still as its last
//!   reset left it (the rest of the reset, scratch, states, steps, is done
//!   as before);
//! - anything else of the title's reaches the list (`crate::list::host_list`,
//!   a submission again without a reset, a recording without one): it is
//!   closed then ([`touched`]), which is what it was before, only later;
//! - our own close of it, which ends the mark.
//!
//! What runs on the card is the same: those lists are empty either way. Off
//! while GPU timings (`crate::gpucost`) are on: they stamp each reset.
//!
//! After readmet3xt-lab/foxhound `a7b8667`. Not in the cache key.

use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};

static ON: victory_core::settings::Live = victory_core::settings::Live::flag("VICTORY_FRESH_OPEN");

/// Not left open, or no longer.
pub(crate) const NO: u8 = 0;
/// Left open, nothing has reached it since.
pub(crate) const OPEN: u8 = 1;
/// Left open, and the title's reset is under way: the D3D12 reset is skipped.
pub(crate) const RESET: u8 = 2;

/// Since the last line: lists left open, title resets that found one so,
/// those closed after all.
static COUNTS: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

#[cfg(test)]
thread_local! {
    /// Tests: the switch for calls made on this thread.
    pub(crate) static FORCED: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

fn on() -> bool {
    #[cfg(test)]
    if let Some(on) = FORCED.with(std::cell::Cell::get) {
        return on;
    }
    ON.get_bool()
}

/// A new host list for one of the title's lists, open and empty: left so,
/// or closed now as before.
#[cfg(windows)]
pub(crate) fn leave_open(host: &crate::host::HostList) {
    if !on() || crate::gpucost::on() {
        host.close();
        return;
    }
    host.left_open.store(OPEN, Ordering::Release);
    COUNTS[0].fetch_add(1, Ordering::Relaxed);
}

/// Something other than the title's reset reached the list: closed first if
/// it was left open, as it was before.
#[cfg(windows)]
#[inline]
pub(crate) fn touched(host: &crate::host::HostList) {
    if host.left_open.load(Ordering::Relaxed) == OPEN
        && host
            .left_open
            .compare_exchange(OPEN, NO, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
    {
        COUNTS[2].fetch_add(1, Ordering::Relaxed);
        host.close();
    }
}

/// The title resets its list: one left open has nothing to undo.
#[cfg(windows)]
pub(crate) fn title_reset(host: &crate::host::HostList) {
    let _ = host
        .left_open
        .compare_exchange(OPEN, RESET, Ordering::AcqRel, Ordering::Relaxed);
}

/// What the state `was` means for the reset: skip the D3D12 reset
/// ([`RESET`]), close the list first ([`OPEN`]: a reset that is not the
/// title's), or reset as usual.
pub(crate) fn at_reset(was: u8) -> (bool, bool) {
    match was {
        RESET => (true, false),
        OPEN => (false, true),
        _ => (false, false),
    }
}

/// In [`HostList::reset`](crate::host::HostList::reset), before the D3D12
/// reset: whether to skip it. The mark ends here.
#[cfg(windows)]
pub(crate) fn skip_reset(host: &crate::host::HostList) -> bool {
    let (skip, close) = at_reset(host.left_open.swap(NO, Ordering::AcqRel));
    if skip {
        COUNTS[1].fetch_add(1, Ordering::Relaxed);
    }
    if close {
        COUNTS[2].fetch_add(1, Ordering::Relaxed);
        // SAFETY: our list, open and empty; the reset follows.
        let _ = unsafe { host.list.Close() };
    }
    skip
}

/// The state's initial value.
pub(crate) fn mark() -> AtomicU8 {
    AtomicU8::new(NO)
}

/// With the batch flush's lines.
pub(crate) fn line(frames: f64) {
    let [open, kept, closed] =
        [0, 1, 2].map(|i| COUNTS[i].swap(0, Ordering::Relaxed) as f64 / frames);
    if open + kept + closed > 0.0 {
        tracing::info!(
            "new host lists for the title's lists left open per frame (VICTORY_FRESH_OPEN=1): {open:.1}, \
             {kept:.1} resets of the title found them so and had nothing to undo, {closed:.2} closed after all"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_title_reset_of_a_list_left_open_skips() {
        assert_eq!(at_reset(RESET), (true, false));
        assert_eq!(at_reset(OPEN), (false, true));
        assert_eq!(at_reset(NO), (false, false));
    }
}
