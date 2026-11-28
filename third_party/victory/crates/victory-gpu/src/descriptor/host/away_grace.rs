//! `VICTORY_AWAY_GRACE=<frames>` (unset or 0: off; live): the pass over a
//! heap's views keeps a slot whose texture memory it finds unmapped for up
//! to that many frames since the memory went, when the slot was made from
//! the very view the title holds there now, instead of making it null at
//! once. The slot shows its texture's last upload (stale) rather than
//! nothing (black) while the title moves or re-streams the pages under a
//! view it still uses; the remap that brings them back makes the slot due
//! and it is uploaded again. A slot made from another view (or never made)
//! is made null as before, so no other texture is ever shown in its place.
//!
//! Idea: fxceless/victory-fork issue #1284 (keep the last good view while
//! texture memory is away; ours had only the bake form,
//! `VICTORY_BAKE_KEEP_GONE`).

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// Frames at most: some seconds.
const MOST: u64 = 600;
/// Slots kept so far, and slots made null after their grace ran out.
static AWAY_GRACE: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

/// The grace in frames (0: off).
fn grace() -> u64 {
    static ON: victory_core::settings::Live =
        victory_core::settings::Live::new("VICTORY_AWAY_GRACE", |v| {
            v.and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(0)
        });
    static SAID: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    let frames = ON.get().min(MOST);
    if frames > 0 && !SAID.swap(true, Relaxed) {
        tracing::info!(
            frames,
            "VICTORY_AWAY_GRACE: a slot whose texture memory went keeps its last upload for some frames"
        );
    }
    frames
}

/// Whether a slot is kept: made from the view the title holds now
/// (`made_from`), its memory away since frame `since` (None: unknown), at
/// `frame`, with `grace` frames.
fn keeps(grace: u64, made_from: bool, since: Option<u64>, frame: u64) -> bool {
    grace > 0 && made_from && since.is_some_and(|since| frame.saturating_sub(since) < grace)
}

impl super::HostHeap {
    /// Whether the pass keeps title slot `index` (its descriptor at
    /// `handle`) although its texture memory is not mapped at `frame`.
    pub(crate) fn texture_slot_keeps_away(
        &self,
        index: u32,
        handle: Option<u64>,
        frame: u64,
    ) -> bool {
        let grace = grace();
        if grace == 0 {
            return false;
        }
        let made_from = handle.is_some_and(|handle| self.texture_slot_made_from(index, handle));
        if !made_from {
            return false;
        }
        let since = self
            .textures
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .written_at(index);
        let kept = keeps(grace, made_from, since, frame);
        let n = AWAY_GRACE[usize::from(!kept)].fetch_add(1, Relaxed) + 1;
        if n <= 4 || n.is_multiple_of(4096) {
            tracing::info!(
                kept = AWAY_GRACE[0].load(Relaxed),
                nulled = AWAY_GRACE[1].load(Relaxed),
                grace,
                "away grace: slots whose texture memory went, kept with their last upload / made null after the grace"
            );
        }
        kept
    }
}

#[cfg(test)]
mod tests {
    use super::super::TextureQueue;
    use super::keeps;

    #[test]
    fn kept_only_for_its_own_view_within_the_grace() {
        assert!(keeps(30, true, Some(100), 100));
        assert!(keeps(30, true, Some(100), 129));
        assert!(!keeps(30, true, Some(100), 130), "grace over: null");
        assert!(
            !keeps(30, false, Some(100), 101),
            "another view's slot: null at once"
        );
        assert!(!keeps(0, true, Some(100), 101), "off");
        assert!(!keeps(30, true, None, 101), "not in the queue");
    }

    #[test]
    fn the_queue_tells_when_the_memory_went() {
        let mut q = TextureQueue::default();
        q.written(7, 50, 51, false);
        assert_eq!(q.written_at(7), Some(50));
        // The remap that took its memory away makes it due as written now.
        q.changed(7, 90, 91);
        assert_eq!(q.written_at(7), Some(90));
        // Looks that find it away keep that frame.
        let write = q.slots[&7].write;
        q.later(7, write, 91, 64);
        q.later(7, write, 92, 64);
        assert_eq!(q.written_at(7), Some(90));
        assert_eq!(q.written_at(8), None);
    }
}
