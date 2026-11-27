//! (El1xr) `VICTORY_SPLIT_AHEAD`: a dispatch split into parts
//! (`super::bound_split`) is clamped in its list's prelude
//! (`super::record_ahead`) instead of by a pass of its own at its place.
//!
//! Batch 4 named `VICTORY_SPLIT_AHEAD=1` (as first written) alone for the
//! prison cell's left wall gone (the sky through it) and the bunk shredded,
//! from about 105 s on. Two things the pass in place gave that the prelude
//! does not:
//!
//! - It read the record at its place. The prelude reads it at the list's
//!   start, so a record that changes in between runs with its old groups.
//!   Only writes the list itself declared so far kept a record in place
//!   (`HostList::gpu_written`); a record that shaders of any list wrote in
//!   this frame or the two before (`crate::host::shader_written`) is made by
//!   the card, and a write to it the list does not declare (another list, a
//!   part after a cut, a path that does not declare) is that change.
//! - It stood between two waits for every earlier shader write: the
//!   dispatch ran after everything before it, including the pass that made
//!   its inputs where the title's console sync before it (`crate::list`'s
//!   `sync`) is no wait on the host.
//!
//! The switch's values (no rebuild between them):
//! - `1`: both guards: records shaders wrote lately stay in place, and the
//!   dispatches clamped ahead keep one wait for every earlier shader write
//!   at their place (the in-place pass's clamp dispatch, its second wait and
//!   its two pipeline changes are still saved).
//! - `2`: the wait only (records written lately go ahead too).
//! - `3`: the records only (no wait at the place).
//!
//! Counted per frame on the line after "indirect records per frame".

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// What the switch keeps of the pass in place.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Guards {
    /// Records shaders wrote lately stay in place.
    pub(crate) written: bool,
    /// One wait for every earlier shader write before each dispatch
    /// clamped ahead.
    pub(crate) wait: bool,
}

/// The guards of a value of `VICTORY_SPLIT_AHEAD`; `None`: off.
pub(crate) fn guards_of(value: Option<&str>) -> Option<Guards> {
    match value.map(str::trim) {
        Some("1") => Some(Guards {
            written: true,
            wait: true,
        }),
        Some("2") => Some(Guards {
            written: false,
            wait: true,
        }),
        Some("3") => Some(Guards {
            written: true,
            wait: false,
        }),
        _ => None,
    }
}

/// The switch's guards, read once; `None`: off (in place, as before).
pub(crate) fn guards() -> Option<Guards> {
    static ON: OnceLock<Option<Guards>> = OnceLock::new();
    *ON.get_or_init(|| guards_of(std::env::var("VICTORY_SPLIT_AHEAD").ok().as_deref()))
}

/// Since the last frame line: dispatches clamped ahead, those kept in place
/// for a record shaders wrote lately, and the waits put before dispatches
/// clamped ahead.
static COUNTS: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];
const AHEAD: usize = 0;
const KEPT: usize = 1;
const WAITS: usize = 2;

/// Whether a dispatch in parts whose record is `written_lately` (by shaders
/// of any list, `crate::host::shader_written`) may be clamped ahead under
/// `guards`, counted.
pub(crate) fn may_go_ahead(guards: Guards, written_lately: bool) -> bool {
    let keep = guards.written && written_lately;
    if keep {
        COUNTS[KEPT].fetch_add(1, Relaxed);
    }
    !keep
}

/// A dispatch was clamped ahead; whether it gets a wait at its place.
pub(crate) fn went_ahead(guards: Guards) -> bool {
    COUNTS[AHEAD].fetch_add(1, Relaxed);
    if guards.wait {
        COUNTS[WAITS].fetch_add(1, Relaxed);
    }
    guards.wait
}

/// The frame line, over `frames`, with the switch on.
pub(crate) fn line(frames: f64) {
    if guards().is_none() || frames <= 0.0 {
        return;
    }
    let [ahead, kept, waits] =
        [AHEAD, KEPT, WAITS].map(|i| COUNTS[i].swap(0, Relaxed) as f64 / frames);
    tracing::info!(
        "split dispatches per frame (VICTORY_SPLIT_AHEAD): {ahead:.0} clamped ahead, {kept:.0} kept in place (record written by shaders lately), {waits:.0} waits at their place"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_name_the_guards() {
        assert_eq!(guards_of(None), None);
        assert_eq!(guards_of(Some("0")), None);
        assert_eq!(
            guards_of(Some("1")),
            Some(Guards {
                written: true,
                wait: true
            })
        );
        assert_eq!(
            guards_of(Some("2")),
            Some(Guards {
                written: false,
                wait: true
            })
        );
        assert_eq!(
            guards_of(Some("3")),
            Some(Guards {
                written: true,
                wait: false
            })
        );
    }

    /// A record shaders wrote lately (the late change) stays in place
    /// unless the switch keeps only the wait.
    #[test]
    fn records_written_lately_stay_in_place() {
        let both = guards_of(Some("1")).unwrap();
        let wait = guards_of(Some("2")).unwrap();
        let records = guards_of(Some("3")).unwrap();
        assert!(!may_go_ahead(both, true));
        assert!(may_go_ahead(both, false));
        assert!(may_go_ahead(wait, true));
        assert!(!may_go_ahead(records, true));
        assert!(went_ahead(both));
        assert!(went_ahead(wait));
        assert!(!went_ahead(records));
    }
}
