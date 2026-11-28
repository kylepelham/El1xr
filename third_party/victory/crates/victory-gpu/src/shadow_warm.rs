//! (entryhitch, #16) A copy's bytes brought in ahead of any list
//! (`crate::heap_warm`, `VICTORY_HEAP_WARM=1`): the refresh a submission
//! would make of `[from, from + len)`, without the refresh stamp (it belongs
//! to a submission's stretch) and only where it is plainly a buffer's copy:
//! not in place, not a window, watched, no shader writes pending on it.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use super::Shadow;

/// Whether `heap_warm` may bring this copy in ahead.
pub(crate) fn warmable(shadow: &Shadow) -> bool {
    shadow.place.is_none()
        && !shadow.window
        && !shadow.rebar
        && !shadow.transient()
        && !shadow.unwatched()
        && !shadow.has_pending.load(Ordering::Acquire)
}

/// Copies what changed of `[from, from + len)` since the copy last took it
/// (all of it the first time), as [`Shadow::update`] does; `None` if the
/// copy is not [`warmable`]. Notes the copy used, as an ask would.
pub(crate) fn fill(shadow: &Arc<Shadow>, from: usize, len: usize) -> Option<usize> {
    if !warmable(shadow) {
        return None;
    }
    let (start, end) = (shadow.start as usize, (shadow.start + shadow.len) as usize);
    let (a, e) = (from.max(start), from.saturating_add(len).min(end));
    if a >= e {
        return Some(0);
    }
    shadow.used.store(crate::host::moment(), Ordering::Relaxed);
    Some(shadow.update_free(a, e, &[]))
}
