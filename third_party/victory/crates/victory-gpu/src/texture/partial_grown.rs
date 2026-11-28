//! `VICTORY_PARTIAL_GROWN=1` (off by default, live): a partly mapped staged
//! texture (a streamed texture read through a table, only some of its mips'
//! pages there) is read again when more of its memory is mapped than at its
//! last read, [`SETTLE`] frames after that was seen, instead of waiting out
//! the [`WINDOW`] frames since that read.
//!
//! Without it the check at submission reads a partly mapped texture at most
//! every [`WINDOW`] frames (reading around unmapped pages is slow): a mip that
//! streams in shows up to a second late at 30 fps (pop-in), and a write the
//! guard saw meanwhile is taken as read, so the texture waits for its hash
//! turn after the window too. The pass over the heap's views already uploads
//! a slot again as soon as more of it is mapped (`draw.rs`, `mapped <=
//! texture_slot_mapped`); this gives the textures tables read the same.
//!
//! The question is [`super::mapped_bytes`] (one `QueryWorkingSetEx` per
//! 16 MiB), asked at most once a frame per texture and only inside the
//! window. While a growth waits out [`SETTLE`] (by when a write under way into
//! the new pages has landed), a write the guard saw is kept for the next
//! check rather than taken as read.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// Frames a partly mapped texture waits between reads without the switch.
pub(super) const WINDOW: u64 = 30;
/// Frames after more of a texture's memory was seen mapped before it is read.
pub(super) const SETTLE: u64 = 2;
/// Reads made early because more was mapped, since the start.
static EARLY: AtomicU64 = AtomicU64::new(0);

/// Whether the switch is on (live).
pub(super) fn on() -> bool {
    static ON: victory_core::settings::Live =
        victory_core::settings::Live::flag("VICTORY_PARTIAL_GROWN");
    static SAID: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    let on = ON.get_bool();
    if on && !SAID.swap(true, Relaxed) {
        tracing::info!(
            "VICTORY_PARTIAL_GROWN=1: a partly mapped staged texture is read again when more of it is mapped"
        );
    }
    on
}

/// What a texture's partly mapped reads know of its mapping.
#[derive(Default, Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Grown {
    /// Bytes mapped at the last read.
    read: u64,
    /// The frame of the last question, plus one (0: none).
    probed: u64,
    /// The frame more than [`Grown::read`] was first seen mapped.
    since: Option<u64>,
}

impl Grown {
    /// Whether more was seen mapped and the read it calls for is still to come.
    pub(super) fn waiting(&self) -> bool {
        self.since.is_some()
    }
}

/// Whether a partly mapped texture last read in frame `at` is left as it is
/// at `frame` (true) or read now. `probe` gives the bytes of its memory
/// mapped now; asked at most once a frame, only with the switch `on`, and
/// only inside the window.
pub(super) fn holds(
    at: Option<u64>,
    frame: u64,
    on: bool,
    grown: &mut Grown,
    probe: impl FnOnce() -> u64,
) -> bool {
    let Some(at) = at else {
        return false;
    };
    if frame >= at.saturating_add(WINDOW) {
        return false;
    }
    if !on {
        return true;
    }
    if grown.since.is_none() && grown.probed != frame.wrapping_add(1) {
        grown.probed = frame.wrapping_add(1);
        if probe() > grown.read {
            grown.since = Some(frame);
        }
    }
    !grown
        .since
        .is_some_and(|since| frame >= since.saturating_add(SETTLE))
}

/// The texture was read with `mapped` bytes of its memory there; `early`:
/// before its window was out.
pub(super) fn read(grown: &mut Grown, mapped: u64, early: bool) {
    *grown = Grown {
        read: mapped,
        ..Grown::default()
    };
    if early {
        let n = EARLY.fetch_add(1, Relaxed) + 1;
        if n <= 4 || n.is_multiple_of(4096) {
            tracing::info!(
                so_far = n,
                mapped,
                "partial grown: a partly mapped staged texture read again early, more of it mapped"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reads of a texture over `frames` with its mapped bytes per frame
    /// `mapped`, as the branch at submission takes them (a check each frame).
    fn reads(on: bool, frames: u64, mapped: impl Fn(u64) -> u64) -> Vec<u64> {
        let (mut at, mut grown, mut out) = (None, Grown::default(), Vec::new());
        for frame in 100..100 + frames {
            if holds(at, frame, on, &mut grown, || mapped(frame)) {
                continue;
            }
            let early = at.is_some_and(|a| frame < a + WINDOW);
            at = Some(frame);
            read(&mut grown, mapped(frame), early);
            out.push(frame);
        }
        out
    }

    #[test]
    fn a_mip_mapped_after_a_read_is_read_two_frames_later() {
        // 64 KiB of small mips at first; the next mip's block arrives at frame 105.
        let mapped = |frame: u64| if frame < 105 { 64 << 10 } else { 192 << 10 };
        assert_eq!(
            reads(false, 40, mapped),
            vec![100, 130],
            "without the switch: the window"
        );
        assert_eq!(reads(true, 40, mapped), vec![100, 107, 137]);
    }

    #[test]
    fn nothing_more_mapped_keeps_the_window() {
        assert_eq!(reads(true, 70, |_| 64 << 10), vec![100, 130, 160]);
    }

    #[test]
    fn asked_once_a_frame_and_not_after_growth() {
        let mut grown = Grown::default();
        read(&mut grown, 4096, false);
        let mut asked = 0;
        for _ in 0..3 {
            assert!(holds(Some(10), 11, true, &mut grown, || {
                asked += 1;
                4096
            }));
        }
        assert_eq!(asked, 1);
        assert!(holds(Some(10), 12, true, &mut grown, || {
            asked += 1;
            8192
        }));
        assert!(grown.waiting());
        assert!(holds(Some(10), 13, true, &mut grown, || {
            asked += 1;
            8192
        }));
        assert_eq!(asked, 2);
        assert!(!holds(Some(10), 14, true, &mut grown, || unreachable!()));
    }

    #[test]
    fn real_memory_grows_by_block() {
        use windows_sys::Win32::System::Memory::*;
        const BLOCK: usize = 64 << 10;
        // SAFETY: a private reservation of this test; blocks committed in turn.
        unsafe {
            let base =
                VirtualAlloc(std::ptr::null(), 4 * BLOCK, MEM_RESERVE, PAGE_NOACCESS).cast::<u8>();
            assert!(!base.is_null());
            assert!(!VirtualAlloc(base.cast(), BLOCK, MEM_COMMIT, PAGE_READWRITE).is_null());
            base.write_bytes(1, BLOCK);
            let mut grown = Grown::default();
            read(
                &mut grown,
                super::super::mapped_bytes(base, 4 * BLOCK),
                false,
            );
            let probe = || super::super::mapped_bytes(base, 4 * BLOCK);
            assert!(holds(Some(0), 1, true, &mut grown, probe));
            assert!(!grown.waiting());
            let next = base.add(BLOCK);
            assert!(!VirtualAlloc(next.cast(), BLOCK, MEM_COMMIT, PAGE_READWRITE).is_null());
            next.write_bytes(2, BLOCK);
            assert!(holds(Some(0), 2, true, &mut grown, probe));
            assert!(grown.waiting());
            assert!(!holds(Some(0), 4, true, &mut grown, probe));
            VirtualFree(base.cast(), 0, MEM_RELEASE);
        }
    }
}
