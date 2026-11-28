//! `VICTORY_SHADOW_RASTER_TILES=<1|2>` (unset by default; only with every ray a miss,
//! `VICTORY_BVH_NO_HIT=1` without the structure builder and without `VICTORY_NATIVE_BVH=1`):
//! the sun shadow's screen tiles are not handed to the ray-traced path.
//!
//! The title's sun shadow is tiled (`cascadeshadows_classify`). `CS_ShadowTileClassify` writes one
//! word of flags per screen tile and `CS_ShadowTileBalanceRTCost` adds the cost bits (16, 32)
//! and writes it back. The list builders (`CS_CalcRasterClassificationListCounts`,
//! `CS_WriteRasterClassificationResults`) then give a tile up to two places. One is a raster
//! place (categories 0-17: the `CS_ShadowRenderTiled*` programs over the cascades) if bit 0 is
//! set. The other is a traced place (categories 18-21: `CS_RTDistantShadowTrace*32_Tiled`; bit
//! 6 full or 4X-upsampled, bit 13 the leak-plug forms) if bit 2 or bit 13 is set.
//! `CS_CopyShadowTileCounts` turns those counts into the indirect arguments.
//!
//! With every ray a miss, a traced tile comes out fully lit: sun through walls, in tile-shaped
//! rectangles with stair-step edges (b5 part Q `04_face` f1179: Lucia's hair and hoodie and the
//! diner around her sunlit, where every other run has the room dark at that shot).
//!
//! The switch rewrites the flags each of the two programs stores (its last
//! `buffer_store_format_xyzw`), so every program that reads them sees the same tile:
//! - `1`: a tile that has a raster place loses its traced one (bits 2 and 13 cleared): the
//!   cascades shadow it. Tiles that are traced only (beyond the cascades) stay as they are.
//! - `2`: as `1`, and a traced-only tile gets a raster place instead (bit 0 set, bits 2 and 13
//!   cleared). Beyond the last cascade that lights it as the miss did; within one, it is shadowed.
//!
//! Part of the translation: the shader cache keys on the value.

use std::cell::Cell;
use std::sync::OnceLock;

/// The programs whose flag store is rewritten: the entry point's name in the container and
/// the byte offset of that `buffer_store_format_xyzw` in its code.
const PROGRAMS: [(&[u8], usize); 2] = [
    (
        b"\0CS_ShadowTileClassify_06677d61_Wrapped\0",
        0x2a80,
    ),
    (
        b"\0CS_ShadowTileBalanceRTCost_03da1e4a_Wrapped\0",
        0x2cc8,
    ),
];

/// The tile has a raster place.
pub(super) const RASTER: u32 = 1;
/// The tile has a traced place: bit 2, or bit 13 (the leak-plug forms).
pub(super) const TRACED: u32 = 0x2004;

/// The switch's level: 1 or 2; `None` when unset, `0` or unparsable.
fn parse(value: Option<&str>) -> Option<u8> {
    match value?.trim() {
        "1" => Some(1),
        "2" => Some(2),
        _ => None,
    }
}

/// The level, read once: `None` unless every ray is a miss and no host structures are built.
fn level() -> Option<u8> {
    static LEVEL: OnceLock<Option<u8>> = OnceLock::new();
    *LEVEL.get_or_init(|| {
        let native = std::env::var_os("VICTORY_NATIVE_BVH").is_some_and(|v| v == "1");
        (super::lower::intersect::no_hit() && !native)
            .then(|| parse(std::env::var("VICTORY_SHADOW_RASTER_TILES").ok().as_deref()))
            .flatten()
    })
}

/// For the shader cache key: the level, when it applies.
pub fn shadow_raster_key() -> Option<String> {
    level().map(|l| format!("VICTORY_SHADOW_RASTER_TILES={l}"))
}

/// The flags a tile's word `v` holds after the rewrite at `level` (what the emitted code does:
/// `Lower::shadow_raster_rewrite`).
#[cfg(test)]
fn rewrite(level: u8, v: u32) -> u32 {
    let cleared = v & !TRACED;
    match level {
        1 if v & RASTER != 0 => cleared,
        2 if v & TRACED != 0 => cleared | RASTER,
        _ => v,
    }
}

/// The flag store in the program `container` holds, at `level`.
fn store_in(container: &[u8], level: Option<u8>) -> Option<(usize, u8)> {
    let level = level?;
    let has = |name: &[u8]| container.windows(name.len()).any(|w| w == name);
    PROGRAMS
        .iter()
        .find(|p| has(p.0))
        .map(|&(_, offset)| (offset, level))
}

thread_local! {
    /// The store the translation on this thread rewrites ([`with_container`]).
    static STORE: Cell<Option<(usize, u8)>> = const { Cell::new(None) };
}

/// Runs the emission `f` of the program in `c` with its flag store known (the name is in one
/// of the container's parts).
pub(super) fn with_container<R>(c: &crate::dxbc::Container, f: impl FnOnce() -> R) -> R {
    let level = level();
    let store = level.and_then(|_| c.parts.iter().find_map(|part| store_in(part.data, level)));
    if store.is_none() {
        return f();
    }
    let before = STORE.replace(store);
    let r = f();
    STORE.set(before);
    r
}

/// The level for a typed buffer store at byte `offset` of the program being translated, if it
/// is the program's flag store.
pub(super) fn level_at(offset: usize) -> Option<u8> {
    STORE.get().and_then(|(at, l)| (at == offset).then_some(l))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_parse_to_a_level() {
        assert_eq!(parse(None), None);
        assert_eq!(parse(Some("")), None);
        assert_eq!(parse(Some("0")), None);
        assert_eq!(parse(Some("3")), None);
        assert_eq!(parse(Some(" 1 ")), Some(1));
        assert_eq!(parse(Some("2")), Some(2));
    }

    #[test]
    fn level_one_drops_the_traced_place_of_a_raster_tile_only() {
        // Raster and traced (bit 2, full resolution bit 6, cost bit 16).
        assert_eq!(rewrite(1, 1 | 4 | 64 | 16), 1 | 64 | 16);
        // Raster and leak-plug traced (bit 13).
        assert_eq!(rewrite(1, 1 | 0x2000 | 0x400), 1 | 0x400);
        // Traced only: left alone.
        assert_eq!(rewrite(1, 4 | 64), 4 | 64);
        assert_eq!(rewrite(1, 0x2000), 0x2000);
        // Raster only, and nothing: unchanged.
        assert_eq!(rewrite(1, 1 | 2 | 8 | 0x100), 1 | 2 | 8 | 0x100);
        assert_eq!(rewrite(1, 0), 0);
    }

    #[test]
    fn level_two_moves_every_traced_tile_to_the_raster_path() {
        assert_eq!(rewrite(2, 4 | 64), 1 | 64);
        assert_eq!(rewrite(2, 0x2000 | 32), 1 | 32);
        assert_eq!(rewrite(2, 1 | 4), 1);
        assert_eq!(rewrite(2, 1 | 8), 1 | 8);
        assert_eq!(rewrite(2, 0), 0, "a tile with no place gets none");
        for v in [0u32, 1, 4, 5, 0x2000, 0x2045, 0xffff_ffff] {
            assert_eq!(rewrite(2, v) & TRACED, 0);
            assert_eq!(rewrite(1, rewrite(1, v)), rewrite(1, v), "idempotent");
            assert_eq!(rewrite(2, rewrite(2, v)), rewrite(2, v), "idempotent");
        }
    }

    #[test]
    fn only_the_two_flag_stores_are_rewritten() {
        let classify = b"..\0CS_ShadowTileClassify_06677d61_Wrapped\0..";
        let balance =
            b"..\0CS_ShadowTileBalanceRTCost_03da1e4a_Wrapped\0..";
        let lists = b"..\0CS_CalcRasterClassificationListCounts_71bbdf9d_Wrapped\0..";
        assert_eq!(store_in(classify, Some(1)), Some((0x2a80, 1)));
        assert_eq!(store_in(balance, Some(2)), Some((0x2cc8, 2)));
        assert_eq!(store_in(lists, Some(1)), None);
        assert_eq!(store_in(classify, None), None);
        STORE.set(store_in(balance, Some(2)));
        assert_eq!(level_at(0x2cc8), Some(2));
        assert_eq!(level_at(0x2a80), None);
        STORE.set(None);
        assert_eq!(level_at(0x2cc8), None);
    }

    #[test]
    fn the_name_is_found_as_the_title_s_container_holds_it() {
        // Bytes 0x309.. of the 1.0.2.1 container (no archive prefix before the name).
        let real = b"\x00\x00\x00\x000\x00\x00\x00\x00CS_ShadowTileBalanceRTCost_03da1e4a_Wrapped\x00\x00\x00\x00HASH";
        assert_eq!(store_in(real, Some(1)), Some((0x2cc8, 1)));
        let longer = b"\x00CS_ShadowTileBalanceRTCost_03da1e4a_WrappedX\x00";
        assert_eq!(store_in(longer, Some(1)), None);
    }

    #[test]
    fn off_by_default() {
        assert_eq!(shadow_raster_key(), None);
    }
}
