//! (popin, #55) `VICTORY_PLACED_FIRST_MIP_ARRAY=1` (live; off unless set):
//! extracts `MostDetailedMip` (`view[4]`) for 1-slice `TEXTURE2DARRAY` views
//! (`ViewDimension == 5`, `FirstArraySlice == 0`, `ArraySize == 1`) in
//! [`crate::resource::first_mip`], just as `TEXTURE2D` (`dim == 4`) already does.
//!
//! In batch 5 part V (`01_sv.log`), placed 2D textures viewed as 1-slice
//! `TEXTURE2DARRAY` with `MostDetailedMip > 0` (e.g. `512x512` BC1 `mips 8`,
//! `view [47, 5, 1688, 0, 3, 5, 0, 1, 0, 0]`) got `first_mip = 0`, inflating
//! their `placed_bytes_from` span from 4 KiB to 172 KiB. That oversized span
//! both falsely evicted neighbouring 4 KiB-packed textures as stale (`STALE`)
//! and caused the view itself to collide as `Claim::Other` in `texture_sweep`
//! so it was never rescued.
//!
//! Multi-slice arrays (`ArraySize != 1` or `FirstArraySlice != 0`) still return
//! `0` (and `Layout::bytes_from` / `staged_part` also enforce `slices == 1`).
//!
//! Metric line: `placed first_mip_array:`.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};

/// `D3D12_SRV_DIMENSION_TEXTURE2D`.
const TEXTURE_2D_VIEW: u32 = 4;
/// `D3D12_SRV_DIMENSION_TEXTURE2DARRAY`.
const TEXTURE_2D_ARRAY_VIEW: u32 = 5;

/// Whether `VICTORY_PLACED_FIRST_MIP_ARRAY=1` is enabled (live, default off).
pub(crate) fn on() -> bool {
    static ON: victory_core::settings::Live =
        victory_core::settings::Live::flag("VICTORY_PLACED_FIRST_MIP_ARRAY");
    ON.get_bool()
}

static HITS: AtomicU64 = AtomicU64::new(0);
static LAST_LOG_MS: AtomicU64 = AtomicU64::new(0);

fn note_hit() {
    let total = HITS.fetch_add(1, Relaxed) + 1;
    static SAID: AtomicBool = AtomicBool::new(false);
    if !SAID.swap(true, Relaxed) {
        tracing::info!(
            "1-slice TEXTURE2DARRAY first_mip enabled (VICTORY_PLACED_FIRST_MIP_ARRAY=1)"
        );
    }
    let now = crate::host::now_ms();
    let last = LAST_LOG_MS.load(Relaxed);
    if now >= last + 5000
        && LAST_LOG_MS
            .compare_exchange(last, now, Relaxed, Relaxed)
            .is_ok()
    {
        tracing::info!(hits = total, "placed first_mip_array:");
    }
}

/// Pure core of [`first_mip`], parameterized by `enabled` for unit tests.
pub(crate) fn first_mip_with(view: &[u32], enabled: bool) -> (u32, bool) {
    match *view {
        [_, TEXTURE_2D_VIEW, _, _, first, ..] => (first.min(15), false),
        [_, TEXTURE_2D_ARRAY_VIEW, _, _, first, _, 0, 1, ..] if enabled => {
            let first = first.min(15);
            (first, first > 0)
        }
        _ => (0, false),
    }
}

/// First mip shown by a shader resource view, honoring `VICTORY_PLACED_FIRST_MIP_ARRAY=1`.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn first_mip(view: &[u32]) -> u32 {
    let (first, array_hit) = first_mip_with(view, on());
    if array_hit {
        note_hit();
    }
    first
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_switch_is_live_and_off_by_default() {
        assert!(!on());
        victory_core::settings::check_live("VICTORY_PLACED_FIRST_MIP_ARRAY", "1", on, false, true);
    }

    #[test]
    fn one_slice_2d_array_extracts_first_mip_only_when_enabled() {
        // Exact view shape from 01_sv.log L27683: 512x512 BC1 mips 8, dim 5, mip 3+5, slice 0+1.
        let one_slice = [71, 5, 0x1688, 0, 3, 5, 0, 1, 0, 0];
        assert_eq!(first_mip_with(&one_slice, false), (0, false));
        assert_eq!(first_mip_with(&one_slice, true), (3, true));

        // Clamped at 15.
        let high = [71, 5, 0x1688, 0, 99, 5, 0, 1, 0, 0];
        assert_eq!(first_mip_with(&high, true), (15, true));

        // Multi-slice or non-zero first slice still returns 0.
        let multi = [71, 5, 0x1688, 0, 3, 5, 0, 8, 0, 0];
        let shifted = [71, 5, 0x1688, 0, 3, 5, 1, 1, 0, 0];
        assert_eq!(first_mip_with(&multi, true), (0, false));
        assert_eq!(first_mip_with(&shifted, true), (0, false));

        // 2D view unchanged either way.
        let tex2d = [71, 4, 0x1688, 0, 4, 5, 0, 0, 0, 0];
        assert_eq!(first_mip_with(&tex2d, false), (4, false));
        assert_eq!(first_mip_with(&tex2d, true), (4, false));
    }

    #[cfg(windows)]
    #[test]
    fn placed_bytes_from_shrinks_one_slice_array_tail_when_enabled() {
        // 512x512x1 BC1 (fmt 71), 8 mips, Xbox tiled layout (0x105), as in 01_sv.log L27683.
        let desc = [
            3,
            0,
            0,
            0,
            512,
            0,
            512,
            1 | (8 << 16),
            71,
            1,
            0,
            0x105,
            0,
            0,
        ];
        let view = [71, 5, 0x1688, 0, 3, 5, 0, 1, 0, 0];

        let whole = crate::resource::placed_bytes_from(&desc, first_mip_with(&view, false).0)
            .expect("valid BC1 layout");
        let tail = crate::resource::placed_bytes_from(&desc, first_mip_with(&view, true).0)
            .expect("valid BC1 layout");

        assert!(
            whole >= 128 * 1024,
            "off path treats mip-3+ tail as whole 512x512 ({whole} bytes)"
        );
        assert!(
            tail < whole / 8,
            "on path shrinks mip-3+ tail ({tail} bytes) well below whole ({whole} bytes)"
        );
    }
}
