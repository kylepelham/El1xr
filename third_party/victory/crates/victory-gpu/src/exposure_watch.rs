//! (phonescreen, #115, round 12) `VICTORY_EXPOSURE_WATCH=1` (live, off by
//! default, diag only): the exposure passes the phone screen's emissive
//! gate hangs on.
//!
//! trustno1-coder/VIctory-NVIDIA #545 (RTX 5090, the same diner shot): the
//! screen draw carries the video colour, but its G-buffer emissive bit is
//! set only if texel (0,0) of a 1x1 R32G32B32A32 texture times the
//! emissive luminance reaches 0.25, and that texel is `[2^-10, -10, -10,
//! 0]` (both ping-pong textures). `PS_ResetExposure` (42ac930cbc57650f)
//! writes a 1x1 target with `.y == .z`, and with its frame constants all
//! zero (its constant buffer at heap slot `(b0[2].y + 160) >> 5`, register
//! 65 = 0) its log path is `-10 + 0`: exactly that texel. A constant
//! buffer the host shows as null (a title slot past the capped host heap,
//! 737,855 on the RTX 5090, or a slot not made) gives those zeros.
//!
//! For each draw or dispatch whose program is an exposure or luminance
//! pass (name holds `Exposure`, `avgLuminanceCompute` or `Luminance` and
//! `Histogram`), once per frame and program: its target 0, and for the
//! heap byte offsets in `b0` dwords 5 (register 1.y: the compute passes)
//! and 9 (register 2.y: the pixel passes), each of the slots at `+0` to
//! `+160` (the offsets the passes add): the slot, what the host shows there
//! (`made`, `null`, `other_view`, `past_the_host_heap`) and, for a constant
//! buffer, registers 54-56 and 63-66 (`x` of each) of the title's memory
//! and whether all are zero (`heap find`-style text: `exposure watch:`).
//!
//! The 1x1 targets' texels: with `VICTORY_OCEAN_READBACK=1
//! VICTORY_TEXTURE_BACK=1x1f2` too, a list that draws an exposure pass into
//! a 1x1 R32G32B32A32 target copies it back into title memory when it has
//! run (`crate::texback`, the ocean's copy-back, which otherwise takes only
//! unordered-access writes), and each watched line shows the target's
//! first texel there (`rt0_texel`: what the last run left).

use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};

use victory_core::settings::Live;

static WATCH: Live = Live::new("VICTORY_EXPOSURE_WATCH", |v| {
    u64::from(v.map(str::trim) == Some("1"))
});

/// Lines of one frame at most.
const MOST_FRAME_LINES: usize = 32;
/// Lines of a run at most.
const MOST_LINES: u32 = 4000;
/// The `b0` dwords that hold heap byte offsets: register 1.y (compute
/// passes) and 2.y (pixel passes).
pub(crate) const BASE_DWORDS: [usize; 2] = [5, 9];
/// The offsets the passes add to a base before `>> 5`.
pub(crate) const OFFSETS: [u32; 6] = [0, 32, 64, 96, 128, 160];
/// The constant registers whose `x` is logged (the reset pass's).
pub(crate) const REGISTERS: [usize; 7] = [54, 55, 56, 63, 64, 65, 66];

static FRAME: Mutex<(u64, Vec<u64>)> = Mutex::new((u64::MAX, Vec::new()));
static LINES: AtomicU32 = AtomicU32::new(0);

/// Whether the switch is on.
pub(crate) fn on() -> bool {
    WATCH.get() != 0
}

/// Whether a program of this name is watched.
pub(crate) fn watched(name: &str) -> bool {
    name.contains("Exposure")
        || name.contains("avgLuminanceCompute")
        || (name.contains("Luminance") && name.contains("Histogram"))
}

/// Whether program `key` gets its line this frame (once per frame and
/// program, within the caps).
pub(crate) fn first_this_frame(frame: u64, key: u64) -> bool {
    let mut f = FRAME.lock().unwrap_or_else(|p| p.into_inner());
    if f.0 != frame {
        *f = (frame, Vec::new());
    }
    if f.1.contains(&key) || f.1.len() >= MOST_FRAME_LINES {
        return false;
    }
    if LINES.fetch_add(1, Ordering::Relaxed) >= MOST_LINES {
        return false;
    }
    f.1.push(key);
    true
}

/// The heap slot a base and an offset name.
pub(crate) fn slot(base: u32, offset: u32) -> u32 {
    base.wrapping_add(offset) >> 5
}

/// `x` of each of [`REGISTERS`] in a constant buffer's words (`None` past
/// its end), and whether every one read is zero.
pub(crate) fn registers(words: &[u32]) -> (Vec<Option<u32>>, bool) {
    let xs: Vec<Option<u32>> = REGISTERS
        .iter()
        .map(|&r| words.get(4 * r).copied())
        .collect();
    let zero = xs.iter().flatten().all(|&w| w == 0);
    (xs, zero)
}

/// The first texel of a 1x1 R32G32B32A32 target as its title memory holds
/// it (texel (0,0) is at the start in every console layout).
pub(crate) fn texel(words: &[u32]) -> Option<[f32; 4]> {
    let w = words.get(..4)?;
    Some([w[0], w[1], w[2], w[3]].map(f32::from_bits))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exposure_and_luminance_passes_are_watched() {
        for name in [
            "PS_ResetExposure",
            "PS_CalculateExposure",
            "PS_SetExposurePrevious",
            "avgLuminanceCompute_LuminanceFromHDRBinkCS",
            "CS_ComputeAverageLuminanceFromHistogramCS",
        ] {
            assert!(watched(name), "{name}");
        }
        assert!(!watched("PS_BinkNoAlpha"));
        assert!(!watched("CS_ComputeLuminanceBilateralGrid"));
    }

    /// Once per frame and program; a new frame starts over.
    #[test]
    fn a_program_gets_one_line_a_frame() {
        assert!(first_this_frame(10, 1));
        assert!(!first_this_frame(10, 1));
        assert!(first_this_frame(10, 2));
        assert!(first_this_frame(11, 1));
    }

    /// The reset pass's constant buffer: `(b0[2].y + 160) >> 5`.
    #[test]
    fn slots_are_base_plus_offset_over_32() {
        assert_eq!(slot(32 * 900_000, 160), 900_005);
        assert_eq!(slot(u32::MAX, 160), (u32::MAX.wrapping_add(160)) >> 5);
    }

    /// All-zero constants (a null view) are told apart from real ones;
    /// registers past the buffer are not read.
    #[test]
    fn zero_constants_are_told() {
        let mut words = vec![0u32; 4 * 67];
        assert_eq!(registers(&words).1, true);
        words[4 * 65] = 1;
        let (xs, zero) = registers(&words);
        assert!(!zero);
        assert_eq!(xs[5], Some(1));
        assert_eq!(registers(&words[..4 * 60]).0[3], None);
    }

    /// #545's texel reads back as itself.
    #[test]
    fn the_floor_texel_reads_back() {
        let words = [2f32.powi(-10), -10.0, -10.0, 0.0].map(f32::to_bits);
        assert_eq!(texel(&words), Some([0.000_976_562_5, -10.0, -10.0, 0.0]));
        assert_eq!(texel(&words[..3]), None);
    }

    #[test]
    fn the_switch_is_live() {
        victory_core::settings::check_live("VICTORY_EXPOSURE_WATCH", "1", on, false, true);
    }
}
