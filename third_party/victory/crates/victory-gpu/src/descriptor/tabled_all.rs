//! Every slot of a table a draw binds is left to its draws by the pass over
//! records copied into the title's heaps as bytes (`VICTORY_TABLED_ALL=1`;
//! off by default), not only the slots that hold texture views.
//!
//! The title writes its per-draw tables into its heap as bytes, every
//! frame. A table that holds a constant buffer is made afresh for its draw
//! in slots of our own, so the title's own slots of it are read by nothing
//! unless a shader picks them by index. The pass (`super::sync_heaps`)
//! cannot know that: it left a table's texture slots to the draws that read
//! them and took every other changed record there for one a shader may
//! pick, a buffer view the once-a-frame pass over buffer views picked by
//! index (`State::heap_buffers`) then made, with its memory looked at, on a
//! recording thread.
//!
//! With the switch a draw stamps the title's own slots of every table it
//! binds ([`super::host::HostHeap::table_slot`]), and the pass leaves a
//! stamped slot alone for [`frames`] frames. A record it left that way has
//! its page read again when the stamp runs out ([`Retries`]), even if
//! nothing writes the page again: a slot that was a table's and is then
//! picked by index only is seen late, never not at all.
//!
//! Source: fxceless/victory-fork PR #1414 (KyleS98, with the retry fix of
//! Filip722), ported by hand. Their numbers, prologue cell, RTX 3090 Ti:
//! 80-91 -> 56-62 ms a frame.

use std::collections::BTreeMap;

/// The switch (`VICTORY_TABLED_ALL`; on only when `1`).
pub(crate) fn on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| switch(std::env::var_os("VICTORY_TABLED_ALL").as_deref()))
}

fn switch(value: Option<&std::ffi::OsStr>) -> bool {
    value.is_some_and(|v| v == "1")
}

/// [`frames`]'s default. The title makes its per-draw tables in a ring of
/// its heap's slots, so a slot is written again some frames after a draw
/// last bound its table, and the stamp has to outlast that. Upstream, in
/// the prologue's cell (some 4,200 draws a frame) the pass mirrored, a
/// frame: 18,645 records without the switch, 13,016 with 2 frames, 9,697
/// with 8, 1,765 with 60 and 218 with 240.
const DEFAULT_FRAMES: u64 = 60;

/// The most frames a stamp may last (the texture queue's own rule's).
const MOST_FRAMES: u64 = 240;

/// For how many frames after a draw bound its table a slot is left to its
/// draws (`VICTORY_TABLED_ALL_FRAMES`, 1 to 240; 60 unless set).
pub(crate) fn frames() -> u64 {
    static FRAMES: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *FRAMES.get_or_init(|| parse_frames(std::env::var("VICTORY_TABLED_ALL_FRAMES").ok().as_deref()))
}

fn parse_frames(value: Option<&str>) -> u64 {
    value
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map_or(DEFAULT_FRAMES, |n| n.clamp(1, MOST_FRAMES))
}

/// The stamp of frame `frame` (never 0, which is "never a table's").
pub(crate) fn stamp(frame: u64) -> u32 {
    (frame as u32).wrapping_add(1).max(1)
}

/// For a slot stamped `stamp`: if a draw bound its table within the last
/// `frames` frames before `frame`, the first frame in which it is left to
/// its draws no longer (unless a draw binds the table again).
pub(crate) fn until(stamp: u32, frame: u64, frames: u64) -> Option<u64> {
    let age = u64::from(self::stamp(frame).wrapping_sub(stamp));
    (stamp != 0 && age <= frames).then(|| frame.saturating_add(frames + 1 - age))
}

/// Pages of a title heap with a changed record the pass left to its draws
/// by the stamp, by the frame they are to be read again in.
#[derive(Default)]
pub(crate) struct Retries {
    pages: BTreeMap<usize, u64>,
    /// The earliest of them, if any.
    next: Option<u64>,
}

impl Retries {
    /// Adds to `ranges` (changed ranges of the heap's memory within
    /// `[start, end)`) the pages due in frame `frame`, merged with them.
    pub(crate) fn include_due(
        &mut self,
        frame: u64,
        start: usize,
        end: usize,
        ranges: &mut Vec<(usize, usize)>,
    ) {
        if self.next.is_none_or(|at| at > frame) {
            return;
        }
        self.next = None;
        let dirty = ranges.len();
        for (&page, &at) in &self.pages {
            if at <= frame {
                let from = page.max(start);
                let to = (page + crate::watch::PAGE).min(end);
                if from < to {
                    ranges.push((from, to));
                }
            } else {
                self.next = Some(self.next.map_or(at, |next| next.min(at)));
            }
        }
        if ranges.len() == dirty {
            return;
        }
        ranges.sort_unstable();
        let mut kept = 0;
        for i in 0..ranges.len() {
            let (from, to) = ranges[i];
            if kept > 0 && from <= ranges[kept - 1].1 {
                ranges[kept - 1].1 = ranges[kept - 1].1.max(to);
            } else {
                ranges[kept] = (from, to);
                kept += 1;
            }
        }
        ranges.truncate(kept);
    }

    /// Page `page` was looked at: to be read again in frame `retry_at`
    /// (`u64::MAX`: never, nothing left on it by the stamp).
    pub(crate) fn after_read(&mut self, page: usize, retry_at: u64) {
        if retry_at == u64::MAX {
            self.pages.remove(&page);
        } else {
            self.pages.insert(page, retry_at);
            self.next = Some(self.next.map_or(retry_at, |next| next.min(retry_at)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_switch_is_off_unless_one() {
        assert!(!switch(None));
        assert!(!switch(Some(std::ffi::OsStr::new("0"))));
        assert!(switch(Some(std::ffi::OsStr::new("1"))));
    }

    #[test]
    fn frames_default_to_60_within_1_to_240() {
        assert_eq!(parse_frames(None), 60);
        assert_eq!(parse_frames(Some("8")), 8);
        assert_eq!(parse_frames(Some("0")), 1);
        assert_eq!(parse_frames(Some("1000")), 240);
        assert_eq!(parse_frames(Some("x")), 60);
    }

    /// A slot is left for the frames asked and no longer; a slot never
    /// stamped never is.
    #[test]
    fn a_stamp_lasts_the_frames_asked_and_no_longer() {
        let frame = 1_000;
        let s = stamp(frame);
        for frames in [1, 8, 60, 240] {
            assert_eq!(until(s, frame, frames), Some(frame + frames + 1));
            assert_eq!(until(s, frame + frames, frames), Some(frame + frames + 1));
            assert_eq!(until(s, frame + frames + 1, frames), None);
        }
        assert_eq!(until(0, frame, 60), None);
        // Frame u32::MAX wraps to a stamp of 1, not 0.
        assert_eq!(stamp(u64::from(u32::MAX)), 1);
    }

    /// Upstream's test: due pages merge with the dirty ones, others wait
    /// for their frame, and a page read with nothing left goes.
    #[test]
    fn retries_merge_with_dirty_pages_when_due() {
        const PAGE: usize = crate::watch::PAGE;
        let mut retries = Retries::default();
        retries.after_read(PAGE, 8);
        retries.after_read(2 * PAGE, 8);
        retries.after_read(3 * PAGE, 9);

        let mut dirty = vec![(2 * PAGE, 3 * PAGE), (4 * PAGE, 5 * PAGE)];
        retries.include_due(7, 0, 5 * PAGE, &mut dirty);
        assert_eq!(dirty, [(2 * PAGE, 3 * PAGE), (4 * PAGE, 5 * PAGE)]);
        retries.include_due(8, 0, 5 * PAGE, &mut dirty);
        assert_eq!(dirty, [(PAGE, 3 * PAGE), (4 * PAGE, 5 * PAGE)]);
        retries.after_read(PAGE, 9);
        retries.after_read(2 * PAGE, u64::MAX);

        let mut due = Vec::new();
        retries.include_due(8, 0, 5 * PAGE, &mut due);
        assert!(due.is_empty());
        retries.include_due(9, 0, 5 * PAGE, &mut due);
        assert_eq!(due, [(PAGE, 2 * PAGE), (3 * PAGE, 4 * PAGE)]);
        retries.after_read(PAGE, u64::MAX);
        retries.after_read(3 * PAGE, 20);

        due.clear();
        retries.include_due(19, 0, 5 * PAGE, &mut due);
        assert!(due.is_empty());
        retries.include_due(20, 0, 5 * PAGE, &mut due);
        assert_eq!(due, [(3 * PAGE, 4 * PAGE)]);
        // Clipped to the heap's memory.
        due.clear();
        retries.after_read(3 * PAGE, 21);
        retries.include_due(21, 3 * PAGE + 64, 3 * PAGE + 128, &mut due);
        assert_eq!(due, [(3 * PAGE + 64, 3 * PAGE + 128)]);
    }
}
