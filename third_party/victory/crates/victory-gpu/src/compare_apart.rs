//! (feedcard, #88) `VICTORY_COMPARE_APART=1` (Live; off by default): the
//! compare of the copies in the card's memory (`CardPlan::check`) goes to the
//! card in an `ExecuteCommandLists` of its own, right after the submission's
//! lists and before its fence signal, instead of as the last list of the same
//! call.
//!
//! Why: a title list can leave a buffer implicitly promoted to `COPY_DEST`
//! (a copy into it). Buffers decay to `COMMON` only at the end of an
//! `ExecuteCommandLists`, so inside the same call the compare reads a buffer
//! still in `COPY_DEST` as a shader resource (D3D12 error 538), and nothing
//! orders the copy's writes before the compare's reads: the compare can miss
//! changes and leave the copy's CPU side stale. In a call of its own the
//! buffers have decayed and the copy is done. The fence is signalled after
//! both calls, so everything that waits for the submission waits for the
//! compare as before.
//!
//! After trustno1-coder/VIctory-NVIDIA PR #1 (`d1fe0bd`) and PR #3
//! (`fdc8bbc`). Not in the cache key: no shader or pipeline output.

use std::sync::atomic::{AtomicU64, Ordering};

static ON: victory_core::settings::Live =
    victory_core::settings::Live::flag("VICTORY_COMPARE_APART");

/// Live: read once per submission.
pub(crate) fn on() -> bool {
    ON.get_bool()
}

/// Compares sent in a call of their own since the last line.
static APART: AtomicU64 = AtomicU64::new(0);

/// The submission's compare `check`: pushed as its last list, as before, or
/// (`apart`) given back to be sent after the lists ([`send`]).
pub(crate) fn split<T>(check: Option<T>, lists: &mut Vec<Option<T>>, apart: bool) -> Option<T> {
    match check {
        Some(check) if !apart => {
            lists.push(Some(check));
            None
        }
        check => check,
    }
}

/// Sends the compare [`split`] gave back, right after the submission's lists.
///
/// # Safety
/// A closed list; the caller sends it where the compare was sent before
/// (after the submission's lists, before its fence signal).
#[cfg(windows)]
pub(crate) unsafe fn send(
    queue: &windows::Win32::Graphics::Direct3D12::ID3D12CommandQueue,
    check: &Option<windows::Win32::Graphics::Direct3D12::ID3D12CommandList>,
) {
    if check.is_some() {
        APART.fetch_add(1, Ordering::Relaxed);
        // SAFETY: as the caller says.
        unsafe { queue.ExecuteCommandLists(std::slice::from_ref(check)) };
    }
}

/// With the batch flush's lines.
pub(crate) fn line(frames: f64) {
    let apart = APART.swap(0, Ordering::Relaxed) as f64 / frames;
    if on() {
        tracing::info!(
            "buffer compares in an ExecuteCommandLists of their own per frame (VICTORY_COMPARE_APART=1): {apart:.1}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::split;

    #[test]
    fn off_keeps_the_compare_as_the_last_list() {
        let mut lists = vec![Some(1), Some(2)];
        assert_eq!(split(Some(9), &mut lists, false), None);
        assert_eq!(lists, vec![Some(1), Some(2), Some(9)]);
    }

    #[test]
    fn apart_leaves_the_lists_alone_and_gives_the_compare_back() {
        let mut lists = vec![Some(1), Some(2)];
        assert_eq!(split(Some(9), &mut lists, true), Some(9));
        assert_eq!(lists, vec![Some(1), Some(2)]);
    }

    #[test]
    fn no_compare_changes_nothing_either_way() {
        for apart in [false, true] {
            let mut lists = vec![Some(1)];
            assert_eq!(split(None, &mut lists, apart), None);
            assert_eq!(lists, vec![Some(1)]);
        }
    }
}
