//! `VICTORY_BAKE_REUSE_WATCH=1` (#115, live, diagnostic): what becomes of the memory of a texture
//! the card wrote (a bake's copy into a character's texture) once the title releases it.
//!
//! Train scan (phonescreen round 15, 151 runs with diner snaps): Lucia's hoodie is dark instead of
//! grey in 15 of 378 diner snaps (13 runs), and the waitress's shirt dark instead of pink in two,
//! and it flips within one run: grey, dark for one shot or several, grey again (b5r2 `03_fen`
//! f1041 grey, f1084 dark, f1120 grey; b5j `01_jw` dark f973-1005, grey f1036-1224, dark
//! f1240-1409, grey f1445). No switch tells the dark snaps apart; every bad run had
//! `VICTORY_BAKE_INPUTS=1`. A colour that comes and goes with the title's cuts fits the title
//! making its decoration result textures again over the same memory: on the console that memory
//! holds what the bake's copy wrote, here the copy reached only the released texture's host copy
//! ([`crate::texture::host_written`]), and [`crate::texture::forget`] drops the memory's link. The
//! new texture (or a placed view of that memory) is then filled from title memory, which holds
//! zeros or an older texture's bytes, until the title bakes again.
//!
//! With the switch, the memory of each released card-written texture is kept (at most
//! [`KEPT`] spans, oldest dropped first), and:
//! - a texture placed in that memory gives a `bake reuse: placed` line (same layout or not,
//!   offset, the released texture, the frame it was released in);
//! - an upload of title memory into a texture over that memory gives a `bake reuse: title upload`
//!   line (the texels the bake wrote replaced by bytes the card never wrote);
//! - a card write into a texture over that memory forgets the span (the title baked again).
//!
//! The first [`MAX_LINES`] events of each kind get a line, then every power of two; one
//! `bake reuse frame` line per frame with any event carries the counts.

pub(crate) mod keep;
#[cfg(all(test, windows))]
mod keep_tests;

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// Live: `0` off, `1` on.
pub(crate) static ON: victory_core::settings::Live =
    victory_core::settings::Live::new("VICTORY_BAKE_REUSE_WATCH", |v| u64::from(v == Some("1")));

pub(crate) fn on() -> bool {
    ON.get() > 0
}

pub(crate) fn keep_on() -> bool {
    keep::on()
}

/// Released card-written spans kept at most.
const KEPT: usize = 8192;
/// Lines per kind before only powers of two are logged.
const MAX_LINES: u64 = 64;

/// A released card-written texture's memory: bytes, description words 4, 6, 7 and 8 (width,
/// height, layers and levels, format), the released resource and the frame it was released in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Span {
    pub(crate) bytes: u64,
    pub(crate) layout: [u32; 4],
    pub(crate) resource: usize,
    pub(crate) released: u64,
}

/// The kept spans by address, oldest released first out.
#[derive(Default)]
pub(crate) struct Grave {
    spans: BTreeMap<u64, Span>,
}

impl Grave {
    pub(crate) fn keep(&mut self, at: u64, span: Span) {
        self.spans.insert(at, span);
        while self.spans.len() > KEPT {
            let oldest = self
                .spans
                .iter()
                .min_by_key(|(_, s)| s.released)
                .map(|(a, _)| *a);
            match oldest {
                Some(a) => self.spans.remove(&a),
                None => break,
            };
        }
    }

    /// The kept span overlapping `[at, at + len)`: its address and itself.
    pub(crate) fn overlap(&self, at: u64, len: u64) -> Option<(u64, Span)> {
        let end = at.saturating_add(len.max(1));
        let before = self.spans.range(..=at).next_back();
        let inside = self.spans.range(at..end);
        before
            .into_iter()
            .chain(inside)
            .find(|(a, s)| **a < end && at < a.saturating_add(s.bytes))
            .map(|(a, s)| (*a, *s))
    }

    /// The card wrote `[at, at + len)` again: the spans in it are the title's bake now.
    pub(crate) fn rewritten(&mut self, at: u64, len: u64) -> usize {
        let mut gone = Vec::new();
        while let Some((a, _)) = self.overlap(at, len) {
            if gone.contains(&a) {
                break;
            }
            self.spans.remove(&a);
            gone.push(a);
        }
        gone.len()
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.spans.len()
    }
}

static GRAVE: Mutex<Option<Grave>> = Mutex::new(None);

const PLACED: usize = 0;
const SAME: usize = 1;
const UPLOAD: usize = 2;
const REBAKED: usize = 3;
const RELEASED: usize = 4;
const ADOPTED: usize = 5;
const INVALIDATED: usize = 6;
static COUNTS: [AtomicU64; 7] = [const { AtomicU64::new(0) }; 7];
static FRAME: AtomicU64 = AtomicU64::new(u64::MAX);

fn grave<R>(f: impl FnOnce(&mut Grave) -> R) -> R {
    let mut g = GRAVE.lock().unwrap_or_else(|e| e.into_inner());
    f(g.get_or_insert_with(Grave::default))
}

/// At the first event of a new frame: the line for the frame before.
pub(crate) fn frame_line() -> u64 {
    let now = crate::frame::current();
    let prev = FRAME.swap(now, Relaxed);
    if prev != now && prev != u64::MAX {
        let c: [u64; 7] = std::array::from_fn(|i| COUNTS[i].swap(0, Relaxed));
        if c.iter().any(|&n| n > 0) {
            let kept = keep::kept_count();
            let kept_mib = keep::kept_bytes() as f64 / (1024.0 * 1024.0);
            tracing::info!(
                frame = prev,
                released = c[RELEASED],
                placed = c[PLACED],
                placed_same_layout = c[SAME],
                title_uploads = c[UPLOAD],
                rebaked = c[REBAKED],
                adopted = c[ADOPTED],
                invalidated = c[INVALIDATED],
                kept,
                kept_mib = format_args!("{kept_mib:.2}"),
                "bake reuse frame"
            );
        }
    }
    now
}

fn said(kind: usize) -> bool {
    static LINES: [AtomicU64; 7] = [const { AtomicU64::new(0) }; 7];
    let n = LINES[kind].fetch_add(1, Relaxed) + 1;
    n <= MAX_LINES || n.is_power_of_two()
}

pub(crate) fn note_adopted(at: u64, layout: keep::LayoutKey, age_frames: u64, bytes: u64) {
    let frame = frame_line();
    COUNTS[ADOPTED].fetch_add(1, Relaxed);
    if said(ADOPTED) {
        tracing::info!(
            frame,
            address = format_args!("{at:#x}"),
            width = layout.width,
            height = layout.height,
            mips = layout.mip_levels,
            layers = layout.depth_or_array_size,
            format = layout.format,
            bytes,
            age_frames,
            "bake reuse: adopted retained GPU-written host copy"
        );
    }
}

pub(crate) fn note_invalidated(n: u64) {
    if n > 0 {
        frame_line();
        COUNTS[INVALIDATED].fetch_add(n, Relaxed);
    }
}

/// [`crate::texture::forget`]: the title released `resource`, which the card wrote, at `at`.
pub(crate) fn released(at: u64, bytes: u64, layout: [u32; 4], resource: usize) {
    let released = frame_line();
    COUNTS[RELEASED].fetch_add(1, Relaxed);
    grave(|g| {
        g.keep(
            at,
            Span {
                bytes,
                layout,
                resource,
                released,
            },
        )
    });
}

/// [`crate::texture::host_written`]: the card wrote a texture at `[at, at + bytes)`.
pub(crate) fn card_wrote(at: u64, bytes: u64) {
    keep::invalidate(at, bytes);
    if on() {
        frame_line();
        let n = grave(|g| g.rewritten(at, bytes));
        COUNTS[REBAKED].fetch_add(n as u64, Relaxed);
    }
}

/// A texture was placed at `address` with description words `desc`.
pub(crate) fn placed(address: u64, desc: &[u32; 14]) {
    let frame = frame_line();
    let bytes = crate::resource::placed_bytes(desc).unwrap_or(0);
    let Some((at, span)) = grave(|g| g.overlap(address, bytes)) else {
        return;
    };
    let same = at == address && span.layout == [desc[4], desc[6], desc[7], desc[8]];
    COUNTS[PLACED].fetch_add(1, Relaxed);
    COUNTS[SAME].fetch_add(u64::from(same), Relaxed);
    if said(PLACED) {
        tracing::info!(
            frame,
            address = format_args!("{address:#x}"),
            offset = address.wrapping_sub(at) as i64,
            width = desc[4],
            height = desc[6],
            layers_levels = format_args!("{:#x}", desc[7]),
            format = desc[8],
            same_layout = same,
            released_resource = format_args!("{:#x}", span.resource),
            released_frame = span.released,
            released_width = span.layout[0],
            released_height = span.layout[1],
            released_format = span.layout[3],
            "bake reuse: placed (a texture made in the memory of a released texture the card wrote)"
        );
    }
}

/// [`crate::texture::PreludeUpload::finish_refresh`]: title memory `[at, at + len)` is uploaded
/// into `resource`'s host copy.
pub(crate) fn title_upload(at: u64, len: u64, resource: usize) {
    let frame = frame_line();
    let Some((span_at, span)) = grave(|g| g.overlap(at, len)) else {
        return;
    };
    COUNTS[UPLOAD].fetch_add(1, Relaxed);
    if said(UPLOAD) {
        tracing::info!(
            frame,
            resource = format_args!("{resource:#x}"),
            address = format_args!("{at:#x}"),
            bytes = len,
            offset = at.wrapping_sub(span_at) as i64,
            released_resource = format_args!("{:#x}", span.resource),
            released_frame = span.released,
            released_width = span.layout[0],
            released_height = span.layout[1],
            released_format = span.layout[3],
            "bake reuse: title upload (title memory the card never wrote, uploaded where a released bake result was)"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(bytes: u64, resource: usize, released: u64) -> Span {
        Span {
            bytes,
            layout: [256, 256, 0x9, 78],
            resource,
            released,
        }
    }

    #[test]
    fn overlap_finds_a_span_before_and_inside() {
        let mut g = Grave::default();
        g.keep(0x1000, span(0x100, 1, 5));
        assert_eq!(g.overlap(0x1080, 0x10).map(|(a, _)| a), Some(0x1000));
        assert_eq!(g.overlap(0x0f00, 0x200).map(|(a, _)| a), Some(0x1000));
        assert_eq!(g.overlap(0x1100, 0x10), None);
        assert_eq!(g.overlap(0x0f00, 0x100), None);
    }

    #[test]
    fn a_card_write_forgets_the_spans_it_covers() {
        let mut g = Grave::default();
        g.keep(0x1000, span(0x100, 1, 5));
        g.keep(0x1100, span(0x100, 2, 6));
        g.keep(0x3000, span(0x100, 3, 7));
        assert_eq!(g.rewritten(0x1000, 0x200), 2);
        assert_eq!(g.len(), 1);
        assert!(g.overlap(0x3000, 1).is_some());
    }

    #[test]
    fn the_oldest_released_goes_first_past_the_cap() {
        let mut g = Grave::default();
        for i in 0..=KEPT as u64 {
            g.keep(i * 0x1000, span(0x100, i as usize, 1000 - (i % 1000)));
        }
        assert_eq!(g.len(), KEPT);
        // Address 999 * 0x1000 was released at frame 1, the earliest.
        assert!(g.overlap(999 * 0x1000, 1).is_none());
    }

    #[test]
    fn off_by_default() {
        if std::env::var_os("VICTORY_BAKE_REUSE_WATCH").is_none() {
            assert!(!on());
        }
    }
}
