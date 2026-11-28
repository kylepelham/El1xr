//! (entryhitch, #16) `VICTORY_WIDE_COPY=1` (Live, off by default): the
//! submitting thread's big plain copies cut into pieces for the kept
//! threads of [`crate::shadow::wide_run`] (LooperSalty/AnyXboxX
//! `c9c9a64255`, idea from fever308's JMAN730/victory `b975c652`).
//!
//! Upstream cut every `copy_mapped` of 8 MiB or more into 2 MiB pieces;
//! there the title's own buffer copies moved 64-76 MB a frame on one
//! thread. Here those copies run on the card (`VICTORY_CARD_COPIES`), the
//! CPU ones are about 2 a frame and well under a megabyte, and the bulk of
//! a frame's bytes (the host copies' refreshes, ~90 MB) already goes to the
//! kept threads in 64 KiB pieces (`Shadow::copy_runs_wide`). What is left on
//! one thread: a submission's queued fills of upload memory, spread across
//! the threads one whole fill per job, so one big fill holds the wait; and
//! a big title copy (`Step::Copy`), if one comes. With the switch fills of
//! [`FILL_MIN`] or more are cut into [`FILL_PIECE`] pieces before they are
//! spread, and title copies of [`STEP_MIN`] or more go through the kept
//! threads in [`STEP_PIECE`] pieces. Each piece is copied by the same
//! function as the whole was, so the same bytes land in the same places.
//!
//! Not in the cache key: no shader or pipeline output changes.

use victory_core::settings::Live;

static ON: Live = Live::flag("VICTORY_WIDE_COPY");

pub(crate) fn on() -> bool {
    ON.get_bool()
}

/// Fills of upload memory this long or longer are cut.
pub(crate) const FILL_MIN: usize = 2 << 20;
/// The pieces fills are cut into.
pub(crate) const FILL_PIECE: usize = 1 << 20;
/// Title copies this long or longer are cut (upstream's bound).
pub(crate) const STEP_MIN: usize = 8 << 20;
/// The pieces title copies are cut into (upstream's).
pub(crate) const STEP_PIECE: usize = 2 << 20;

/// The `[from, to)` pieces that cover `len` bytes, `piece` bytes each (the
/// last one may be shorter).
pub(crate) fn pieces(len: usize, piece: usize) -> impl Iterator<Item = (usize, usize)> {
    let piece = piece.max(1);
    (0..len.div_ceil(piece)).map(move |i| (i * piece, ((i + 1) * piece).min(len)))
}

/// Cuts each fill `(dst, src, len)` of [`FILL_MIN`] bytes or more into
/// [`FILL_PIECE`] pieces, in place and in order; whether any was cut (the
/// fills then go to the kept threads however few they are).
pub(crate) fn split_fills(fills: &mut Vec<(usize, usize, usize)>) -> bool {
    let (cut, made) = split_counted(fills);
    crate::prep_cut::add(crate::prep_cut::F::WideFills, cut);
    crate::prep_cut::add(crate::prep_cut::F::WidePieces, made);
    cut > 0
}

/// [`split_fills`]: the fills cut and the pieces they became.
fn split_counted(fills: &mut Vec<(usize, usize, usize)>) -> (u64, u64) {
    if !fills.iter().any(|f| f.2 >= FILL_MIN) {
        return (0, 0);
    }
    let (mut cut, mut made) = (0, 0);
    let mut out = Vec::with_capacity(fills.len() + 8);
    for &(dst, src, len) in fills.iter() {
        if len < FILL_MIN {
            out.push((dst, src, len));
            continue;
        }
        cut += 1;
        for (a, b) in pieces(len, FILL_PIECE) {
            out.push((dst + a, src + a, b - a));
            made += 1;
        }
    }
    *fills = out;
    (cut, made)
}

/// Whether a title copy goes in pieces: the switch is on, it is
/// [`STEP_MIN`] bytes or more, and its two sides do not overlap (pieces
/// copied side by side would read bytes another piece already wrote).
pub(crate) fn cut_step(dst: usize, src: usize, len: usize) -> bool {
    let cut = len >= STEP_MIN && (src + len <= dst || dst + len <= src) && on();
    if cut {
        crate::prep_cut::add(crate::prep_cut::F::WideSteps, 1);
    }
    cut
}

/// `copy(dst, src, len)` of a big copy, piece by piece on the kept threads
/// and this one; returns when every piece is copied.
pub(crate) fn copy_pieces(
    dst: *mut u8,
    src: *const u8,
    len: usize,
    piece: usize,
    copy: fn(*mut u8, *const u8, usize),
) {
    let (d, s) = (dst as usize, src as usize);
    let jobs: Vec<Box<dyn FnOnce() + Send>> = pieces(len, piece)
        .map(|(a, b)| {
            Box::new(move || copy((d + a) as *mut u8, (s + a) as *const u8, b - a))
                as Box<dyn FnOnce() + Send>
        })
        .collect();
    crate::shadow::wide_run(jobs);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlapping_or_small_title_copies_stay_whole() {
        let big = STEP_MIN;
        assert!(!cut_step(0x1000_0000, 0x1000_0000 + big / 2, big));
        assert!(!cut_step(0x1000_0000 + big / 2, 0x1000_0000, big));
        assert!(!cut_step(0x1000_0000, 0x2000_0000, big - 1));
    }

    #[test]
    fn pieces_cover_the_copy_once() {
        assert_eq!(pieces(0, 4).count(), 0);
        assert_eq!(pieces(4, 4).collect::<Vec<_>>(), [(0, 4)]);
        assert_eq!(pieces(10, 4).collect::<Vec<_>>(), [(0, 4), (4, 8), (8, 10)]);
        let len = (9 << 20) + 3;
        let cut: Vec<_> = pieces(len, STEP_PIECE).collect();
        assert_eq!(cut.len(), 5);
        assert_eq!(cut.iter().map(|(a, b)| b - a).sum::<usize>(), len);
        assert!(cut.windows(2).all(|w| w[0].1 == w[1].0));
    }

    /// Fills cut and run (here, one after another, and spread in any order
    /// as the kept threads would) write the same bytes as the fills whole.
    #[test]
    fn cut_fills_write_the_same_bytes() {
        let src: Vec<u8> = (0..(12 << 20))
            .map(|i: usize| (i * 131 + i / 4099) as u8)
            .collect();
        let lens = [
            100,
            FILL_MIN - 1,
            FILL_MIN,
            FILL_MIN + 4097,
            (5 << 20) + 1,
            7,
        ];
        let mut fills = Vec::new();
        let (mut at_src, mut at_dst) = (0usize, 0usize);
        for &len in &lens {
            fills.push((at_dst, at_src, len));
            at_src += len / 3 + 11;
            at_dst += len;
        }
        let run = |fills: &[(usize, usize, usize)], rev: bool| {
            let mut dst = vec![0u8; at_dst];
            let mut order: Vec<_> = fills.to_vec();
            if rev {
                order.reverse();
            }
            for (d, s, len) in order {
                dst[d..d + len].copy_from_slice(&src[s..s + len]);
            }
            dst
        };
        let whole = run(&fills, false);
        let mut cut = fills.clone();
        let (n, made) = split_counted(&mut cut);
        assert_eq!(n, 3);
        assert_eq!(made, 2 + 3 + 6);
        assert!(cut.iter().all(|f| f.2 < FILL_MIN));
        assert_eq!(
            cut.iter().map(|f| f.2).sum::<usize>(),
            lens.iter().sum::<usize>()
        );
        assert!(run(&cut, false) == whole);
        assert!(run(&cut, true) == whole);
        let mut small = vec![(0, 0, 10), (10, 10, FILL_MIN - 1)];
        assert_eq!(split_counted(&mut small), (0, 0));
        assert_eq!(small, [(0, 0, 10), (10, 10, FILL_MIN - 1)]);
    }

    fn plain(dst: *mut u8, src: *const u8, len: usize) {
        // SAFETY: the test's own buffers, `len` bytes inside each.
        unsafe { std::ptr::copy_nonoverlapping(src, dst, len) };
    }

    fn mapped(dst: *mut u8, src: *const u8, len: usize) {
        crate::host::copy_mapped(dst, src, len, false);
    }

    /// A big copy made on several threads copies every byte, the last
    /// partial piece included, through either copy function.
    #[test]
    fn a_big_copy_in_pieces_copies_every_byte() {
        let len = (9 << 20) + 4097;
        let src: Vec<u8> = (0..len).map(|i| (i * 131 + i / 4099) as u8).collect();
        for copy in [plain as fn(*mut u8, *const u8, usize), mapped] {
            let mut dst = vec![0u8; len];
            copy_pieces(dst.as_mut_ptr(), src.as_ptr(), len, STEP_PIECE, copy);
            assert!(dst == src, "the pieces did not copy the same bytes");
        }
    }
}
