//! The look again at a texture uploaded whole (`VICTORY_HEAP_RECHECK`) also
//! reads the start of every mip of every slice (`VICTORY_HEAP_RECHECK_MIPS=1`;
//! off by default, read once).
//!
//! Ported from fxceless/victory-fork #559 (NickJBc), as darkworldhm carries
//! it. [`super::fingerprint`] reads twelve 4 KiB pieces spread evenly over
//! the memory. A texture streams in whole mips; a mip, an array slice or a
//! cube face that arrives after the upload between two pieces is never seen,
//! and the host copy keeps the old one (stale late mips: pop-in that stays).
//! With the switch [`super::staged_memory`] notes, for the memory it names,
//! the ranges [`victory_formats::texture::Layout::sample_ranges`] gives (up
//! to [`MOST`] of [`WINDOW`] bytes), and the fingerprint of that memory
//! reads them too. Every fingerprint of the same memory reads the same
//! ranges, so the one at the upload and the later looks agree.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};

/// Bytes read at the start of each mip.
const WINDOW: u64 = 1 << 10;
/// Ranges read of one texture at most (spread over its slices and mips when
/// it has more). A 4096-square texture with all its mips: about 13 ranges;
/// a cube map with 10 mips: about 40.
const MOST: usize = 64;
/// Memory [`super::fingerprint`] reads whole: nothing to add.
const WHOLE_BYTES: u64 = 48 << 10;
/// Textures noted at most; past that, new ones keep the twelve pieces alone
/// (their fingerprints stay consistent, only without the ranges).
const MOST_NOTED: usize = 1 << 16;

/// Ranges (offsets from the address, bytes) by texture memory (address,
/// bytes).
pub(super) type Ranges = Arc<[(u64, u64)]>;

static NOTED: Mutex<Option<HashMap<(u64, u64), Ranges>>> = Mutex::new(None);

/// Since the start: textures noted, fingerprints that read ranges, and the
/// bytes those ranges read.
pub(crate) static RECHECK_MIPS: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

pub(super) fn on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        let on = parse(std::env::var("VICTORY_HEAP_RECHECK_MIPS").ok().as_deref());
        if on {
            tracing::info!(
                window = WINDOW,
                most = MOST,
                "texture looks read the start of every mip of every slice (VICTORY_HEAP_RECHECK_MIPS=1; fxceless #559)"
            );
        }
        on
    })
}

fn parse(value: Option<&str>) -> bool {
    value.map(str::trim).is_some_and(|v| v == "1")
}

/// The ranges of `layout` inside its first `len` bytes (a texture made
/// level by level holds only its smallest levels, which lie first).
fn ranges(layout: &victory_formats::texture::Layout, len: u64) -> Vec<(u64, u64)> {
    layout
        .sample_ranges(WINDOW, MOST)
        .into_iter()
        .filter(|&(at, _)| at < len)
        .map(|(at, n)| (at, n.min(len - at)))
        .collect()
}

/// Notes the ranges of the texture laid out as `layout` whose memory is
/// `[at, at + len)`, when the switch is on.
pub(super) fn note(at: u64, len: u64, layout: &victory_formats::texture::Layout) {
    // The fingerprint reads memory this small whole.
    if !on() || len <= WHOLE_BYTES {
        return;
    }
    let mut map = NOTED.lock().unwrap_or_else(|e| e.into_inner());
    let map = map.get_or_insert_with(HashMap::new);
    if map.contains_key(&(at, len)) {
        return;
    }
    if map.len() >= MOST_NOTED {
        static FULL: std::sync::Once = std::sync::Once::new();
        FULL.call_once(|| {
            tracing::warn!(
                noted = map.len(),
                "recheck mips: table full, newer textures are looked at through the twelve pieces alone"
            );
        });
        return;
    }
    map.insert((at, len), ranges(layout, len).into());
    let n = RECHECK_MIPS[0].fetch_add(1, Relaxed) + 1;
    if n <= 4 || n.is_multiple_of(8192) {
        tracing::info!(
            noted = n,
            fingerprints = RECHECK_MIPS[1].load(Relaxed),
            read_mb = RECHECK_MIPS[2].load(Relaxed) >> 20,
            "recheck mips: textures whose looks read every mip (VICTORY_HEAP_RECHECK_MIPS)"
        );
    }
}

/// The ranges noted for the memory `[at, at + len)`.
pub(super) fn noted(at: u64, len: u64) -> Option<Ranges> {
    if !on() {
        return None;
    }
    let map = NOTED.lock().unwrap_or_else(|e| e.into_inner());
    let ranges = map.as_ref()?.get(&(at, len))?.clone();
    RECHECK_MIPS[1].fetch_add(1, Relaxed);
    RECHECK_MIPS[2].fetch_add(ranges.iter().map(|r| r.1).sum(), Relaxed);
    Some(ranges)
}

#[cfg(test)]
mod tests {
    use super::*;
    use victory_formats::texture::{Desc, Dimension, Layout, Swizzle};

    #[test]
    fn switch_is_off_unless_one() {
        assert!(!parse(None));
        assert!(!parse(Some("0")));
        assert!(!parse(Some("x")));
        assert!(parse(Some("1")));
        assert!(parse(Some(" 1 ")));
    }

    fn cube() -> Layout {
        Layout::new(&Desc {
            dimension: Dimension::Tex2d,
            width: 512,
            height: 512,
            depth: 6,
            mips: 10,
            element_bytes: 8,
            block: 4,
            samples: 1,
            swizzle: Swizzle::SW_64KB_D,
        })
        .unwrap()
    }

    /// A late mip of a cube face the twelve pieces step over: the plain
    /// fingerprint misses its arrival, the one with the noted ranges sees it.
    #[test]
    fn a_late_mip_between_the_pieces_is_seen_with_the_ranges() {
        let layout = cube();
        let len = layout.size;
        let ranges = ranges(&layout, len);
        let mut memory = vec![0u8; len as usize];
        let at = memory.as_ptr() as u64;
        // The twelve pieces: where the plain fingerprint reads.
        let pieces: Vec<(u64, u64)> = (0..11u64)
            .map(|n| (((len - 4096) / 11 * n) & !0xfff, 4096))
            .chain([(len - 4096, 4096)])
            .collect();
        let missed = ranges
            .iter()
            .find(|&&(o, n)| pieces.iter().all(|&(p, m)| o + n <= p || o >= p + m))
            .copied()
            .expect("a mip range outside the twelve pieces");
        let before = (
            super::super::fingerprint(at, len),
            super::super::fingerprint_ranges(at, len, &ranges),
        );
        memory[missed.0 as usize] = 0xab;
        let after = (
            super::super::fingerprint(at, len),
            super::super::fingerprint_ranges(at, len, &ranges),
        );
        assert_eq!(before.0, after.0, "the twelve pieces do not see it");
        assert_ne!(before.1, after.1, "the mip ranges do");
        assert_eq!(super::super::fingerprint_ranges(at, len, &[]), after.0);
    }

    /// A texture made level by level: only ranges inside the bytes it holds.
    #[test]
    fn ranges_stay_inside_the_held_bytes() {
        let layout = cube();
        let held = layout.slice_size / 3;
        let r = ranges(&layout, held);
        assert!(!r.is_empty());
        assert!(r.iter().all(|&(at, n)| n > 0 && at + n <= held), "{r:x?}");
    }
}
