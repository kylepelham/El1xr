//! (phonescreen, #115) Looks at uploaded textures that go on after the
//! back-off is over.
//!
//! A texture picked by index is uploaded whole, then its memory is looked
//! at again after 4, 12, 40, 120 and 400 frames ([`super::RECHECK_FRAMES`],
//! about 20 s at the world's frame rate). After the last look the slot
//! leaves the looks for good: if the title fills that memory in place only
//! later, with no descriptor write (a phone or TV screen set up blank when
//! the scene loads and written when the screen turns on), the host copy
//! keeps the blank image for the rest of the run. The diner's cracked phone
//! shows a black screen in the table shot after it turned on (#115).
//!
//! `VICTORY_RECHECK_TAIL=N` (frames, 1-10000; off by default): after the
//! last look the slot is looked at every `N` frames instead, for as long as
//! its view stays. A change found there brings the slot up to date (and
//! live, as any change does with `VICTORY_LIVE_TEXTURES`) and is logged:
//! `recheck tail: a texture changed after its looks were over` (slot,
//! address, bytes, frames after the upload; the first 200, then every
//! 1000th).

use super::{RECHECK_FRAMES, Recheck, TextureQueue};

/// The `looks` of a slot in the tail: past the last of [`RECHECK_FRAMES`].
pub(crate) const TAIL_LOOKS: u8 = RECHECK_FRAMES.len() as u8;

/// The switch (`VICTORY_RECHECK_TAIL`): frames between two tail looks.
pub(crate) fn every() -> Option<u64> {
    static EVERY: std::sync::OnceLock<Option<u64>> = std::sync::OnceLock::new();
    *EVERY.get_or_init(|| parse(std::env::var("VICTORY_RECHECK_TAIL").ok().as_deref()))
}

fn parse(value: Option<&str>) -> Option<u64> {
    value
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|n| (1..=10_000).contains(n))
}

impl TextureQueue {
    /// Slot `index`'s last look (`seen`, in `frame`) found it unchanged: it
    /// is looked at every `every` frames from now on, or leaves the looks.
    pub(super) fn looks_over(
        &mut self,
        index: u32,
        seen: &Recheck,
        frame: u64,
        every: Option<u64>,
    ) {
        match every {
            Some(every) => {
                self.rechecks.insert(
                    index,
                    Recheck {
                        due: frame + every,
                        looks: TAIL_LOOKS,
                        ..*seen
                    },
                );
            }
            None => {
                self.rechecks.remove(&index);
            }
        }
    }
}

/// Whether a look at `seen` was a tail look.
pub(crate) fn in_tail(seen: &Recheck) -> bool {
    !seen.live && seen.looks >= TAIL_LOOKS
}

/// A look at slot `index` found it changed: logged when it was a tail look.
pub(crate) fn changed(index: u32, seen: &Recheck, frame: u64) {
    if !in_tail(seen) {
        return;
    }
    static SAID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = SAID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if n < 200 || n.is_multiple_of(1000) {
        tracing::info!(
            slot = index,
            address = format_args!("{:#x}", seen.at.0),
            bytes = seen.at.1,
            frame,
            frames_after_upload = frame.saturating_sub(seen.uploaded),
            changed = n + 1,
            "recheck tail: a texture changed after its looks were over"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_switch_takes_frames_between_looks() {
        assert_eq!(parse(None), None);
        assert_eq!(parse(Some("")), None);
        assert_eq!(parse(Some("0")), None);
        assert_eq!(parse(Some("x")), None);
        assert_eq!(parse(Some("30")), Some(30));
        assert_eq!(parse(Some(" 8 ")), Some(8));
        assert_eq!(parse(Some("10000")), Some(10_000));
        assert_eq!(parse(Some("10001")), None);
    }

    /// Runs slot 7 through every look of the back-off, unchanged; returns
    /// the frame of the last look and what it saw.
    fn through_the_looks(q: &mut TextureQueue) -> (u64, Recheck) {
        q.recheck(7, 1, (0x1000, 64), 5, false);
        let mut frame = 1;
        let mut last = None;
        for wait in RECHECK_FRAMES {
            frame += wait;
            let due = q.rechecks_due(frame, 9);
            assert_eq!(due.len(), 1);
            last = Some(due[0].1);
            if usize::from(due[0].1.looks) + 1 < RECHECK_FRAMES.len() {
                q.rechecked(7, &due[0].1, false, frame, false, None);
            }
        }
        (frame, last.unwrap())
    }

    #[test]
    fn off_the_slot_leaves_the_looks() {
        let mut q = TextureQueue::default();
        let (frame, last) = through_the_looks(&mut q);
        assert!(!in_tail(&last));
        q.looks_over(7, &last, frame, None);
        assert!(q.rechecks_due(u64::MAX, 9).is_empty());
    }

    #[test]
    fn on_the_slot_is_looked_at_every_n_frames_and_a_change_brings_it_up_to_date() {
        let mut q = TextureQueue::default();
        let (frame, last) = through_the_looks(&mut q);
        q.looks_over(7, &last, frame, Some(30));
        assert!(q.rechecks_due(frame + 29, 9).is_empty());
        let due = q.rechecks_due(frame + 30, 9);
        assert_eq!(due.len(), 1);
        let tail = due[0].1;
        assert!(in_tail(&tail));
        assert_eq!(tail.print, 5, "the print of the upload is kept");
        assert_eq!(tail.uploaded, 1);
        // Unchanged: past the back-off, so `rechecked` hands it to the tail
        // again (with the switch off in tests it leaves; looks_over keeps it).
        q.looks_over(7, &tail, frame + 30, Some(30));
        let tail = q.rechecks_due(frame + 60, 9)[0].1;
        assert!(in_tail(&tail));
        // Changed: due for upload now, out of the looks.
        q.rechecked(7, &tail, true, frame + 60, true, None);
        assert_eq!(q.due(frame + 60, 9)[0].0, 7);
        assert!(q.rechecks_due(u64::MAX, 9).is_empty());
    }

    #[test]
    fn live_slots_are_not_tail_looks() {
        let seen = Recheck {
            due: 0,
            at: (0, 0),
            print: 0,
            looks: TAIL_LOOKS,
            uploaded: 0,
            live: true,
        };
        assert!(!in_tail(&seen));
        assert!(in_tail(&Recheck {
            live: false,
            ..seen
        }));
    }
}
