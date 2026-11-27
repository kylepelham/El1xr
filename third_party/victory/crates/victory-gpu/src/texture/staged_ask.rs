//! (trustno1 `36c4c37`, `6b3960d`) Whether a staged texture's memory is all
//! there, asked by the pass over textures picked by index for every texture
//! it uploads or finds staged ([`super::staged`]).
//!
//! `VICTORY_STAGED_ASK_ALL=0`: asked only of a texture made at an address the
//! title mapped ([`crate::resource::at_title_address`]). A committed
//! resource's memory and a heap's are ours, allocated whole when made, and
//! cannot lose pages while they live. Unset: asked of every texture, as
//! before. Upstream measured 20,000-57,000 questions per 5 s, one
//! `QueryWorkingSetEx` per 256 pages each (18-111 us for 1-16 MB resident,
//! milliseconds when trimmed), and the pass's unattributed time going with
//! their count, 13 us each.
//!
//! `VICTORY_MAPPED_BY_VIEWS=1`: of a texture that is asked, the runtime's
//! views of committed sections answer "all there" when they show the whole
//! range ([`crate::inplace::all_shown`]); anything else (no view, a hole, a
//! section not committed) is asked of Windows as before. Here only for this
//! question, not for `mapped_bytes`.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use crate::object::This;
use victory_core::settings::Live;

/// Whether every staged texture is asked (unset or anything but `0`).
pub(crate) fn ask_all() -> bool {
    static ALL: Live = Live::new("VICTORY_STAGED_ASK_ALL", |v| {
        u64::from(v.is_none_or(|v| v.trim() != "0"))
    });
    ALL.get_bool()
}

/// Whether the runtime's views answer first (`VICTORY_MAPPED_BY_VIEWS=1`).
pub(crate) fn by_views() -> bool {
    static ON: Live = Live::flag("VICTORY_MAPPED_BY_VIEWS");
    ON.get_bool()
}

/// Answers since the last line: not asked (not at a title address), answered
/// by the views, asked of Windows.
static STATS: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

/// Whether `[at, at + len)`, the memory of staged texture `resource`, is all
/// there; `committed` asks Windows.
pub(super) fn whole(
    resource: This,
    at: *const u8,
    len: usize,
    committed: impl FnOnce(*const u8, usize) -> bool,
) -> bool {
    answer(
        ask_all(),
        by_views(),
        || crate::resource::at_title_address(resource),
        || crate::inplace::all_shown(at as usize, len),
        || committed(at, len),
    )
}

fn answer(
    all: bool,
    views: bool,
    title: impl FnOnce() -> bool,
    shown: impl FnOnce() -> Option<bool>,
    windows: impl FnOnce() -> bool,
) -> bool {
    if !all && !title() {
        STATS[0].fetch_add(1, Relaxed);
        return true;
    }
    if views && shown() == Some(true) {
        STATS[1].fetch_add(1, Relaxed);
        return true;
    }
    STATS[2].fetch_add(1, Relaxed);
    windows()
}

/// Logs [`STATS`] on the frame-meter cadence when either switch is on, and
/// starts them over.
pub(crate) fn log(frames: f64) {
    let stats: [u64; 3] = std::array::from_fn(|i| STATS[i].swap(0, Relaxed));
    let (all, views) = (ask_all(), by_views());
    if (all && !views) || frames <= 0.0 || stats.iter().sum::<u64>() == 0 {
        return;
    }
    let per = |n: u64| n as f64 / frames;
    tracing::info!(
        "staged textures asked whether all their memory is there, per frame (VICTORY_STAGED_ASK_ALL={}, VICTORY_MAPPED_BY_VIEWS={}): \
         {:.1} not asked (not at a title address), {:.1} answered by the runtime's views, {:.1} asked of Windows",
        u8::from(all),
        u8::from(views),
        per(stats[0]),
        per(stats[1]),
        per(stats[2]),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_switches_are_live() {
        use victory_core::settings::check_live;
        check_live("VICTORY_STAGED_ASK_ALL", "0", ask_all, true, false);
        check_live("VICTORY_MAPPED_BY_VIEWS", "1", by_views, false, true);
    }

    /// Off (both switches as launched), Windows is asked of every texture
    /// and its answer is the answer, whatever the views say.
    #[test]
    fn off_asks_windows_of_every_texture() {
        for title in [false, true] {
            for there in [false, true] {
                let asked = std::cell::Cell::new(false);
                let got = answer(
                    true,
                    false,
                    || title,
                    || Some(true),
                    || {
                        asked.set(true);
                        there
                    },
                );
                assert!(asked.get());
                assert_eq!(got, there);
            }
        }
    }

    /// With `VICTORY_STAGED_ASK_ALL=0` a texture not at a title address is
    /// all there without a question; one at a title address is asked.
    #[test]
    fn only_title_addresses_are_asked() {
        assert!(answer(false, false, || false, || None, || panic!("asked")));
        assert!(!answer(false, false, || true, || None, || false));
    }

    /// The views answer only "all there"; anything else asks Windows.
    #[test]
    fn the_views_answer_only_when_they_show_it_all() {
        assert!(answer(
            true,
            true,
            || true,
            || Some(true),
            || panic!("asked")
        ));
        assert!(!answer(true, true, || true, || None, || false));
        assert!(answer(true, true, || true, || None, || true));
        // Without the switch they are not looked at.
        assert!(!answer(true, false, || true, || panic!("looked"), || false));
    }
}
