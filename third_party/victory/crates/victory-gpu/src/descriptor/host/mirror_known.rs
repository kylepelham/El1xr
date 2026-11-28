//! `VICTORY_MIRROR_KNOWN=1` (restart; off unless `1`): when a draw's table reads a texture, its
//! title slot is mirrored (`HostHeap::mirror`, so a shader picking the slot by index finds it
//! made). `State::tables` has just looked the slot's view up itself, and drops what `mirror`
//! returns; `mirror`'s fast path (the slot already shows that view, nearly always) looks the
//! view up again only to return it, a second take of the shared lookup lock per texture slot.
//!
//! With the switch, [`HostHeap::mirror_known`](super::HostHeap::mirror_known) leaves a slot that
//! already shows the record's view (`made_too` holds the record's entry and generation) without
//! that second lookup. `mirror` would have returned at the same test, its lookup being the one
//! the caller just made of the same record words (`descriptor::view`: a view only for a view
//! record whose entry is current); a slot not shown yet, or of a stale view, goes to `mirror`.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use super::VIEW_TAG;

/// The switch, read once.
pub(crate) fn on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_MIRROR_KNOWN").is_some_and(|v| v == "1"))
}

/// Whether a slot whose `made_too` is `shown` already shows the view of `record` (the slot's
/// record words: tag, entry, generation), as `mirror_inner`'s fast path tests it.
pub(crate) fn shows(record: [u32; 3], shown: u64) -> bool {
    let [tag, i, generation] = record;
    // Generations start at 1, so a made slot's key is never 0.
    tag == VIEW_TAG && shown != 0 && shown == u64::from(generation) << 32 | u64::from(i)
}

static SKIPPED: AtomicU64 = AtomicU64::new(0);

pub(crate) fn skipped() {
    SKIPPED.fetch_add(1, Relaxed);
}

/// With the frame line (`draw::bind_split_line`).
pub(crate) fn line(frames: f64) {
    if !on() || frames <= 0.0 {
        return;
    }
    let n = SKIPPED.swap(0, Relaxed) as f64 / frames;
    tracing::info!(
        "title slots of textures tables read, found shown without a second lookup per frame (VICTORY_MIRROR_KNOWN=1): {n:.0}"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_slot_showing_that_very_view_is_left() {
        let record = [VIEW_TAG, 7, 3];
        let key = 3u64 << 32 | 7;
        assert!(shows(record, key));
        // Another generation or entry shown, nothing shown, or not a view record: to `mirror`.
        assert!(!shows([VIEW_TAG, 7, 4], key));
        assert!(!shows([VIEW_TAG, 8, 3], key));
        assert!(!shows(record, 0));
        assert!(!shows([0, 7, 3], key));
        assert!(!shows([0, 0, 0], 0));
    }
}
