//! GFX10 `image_get_resinfo` sizing/packing, 9-bit mesh primitive index mask,
//! and optional compute raw page-window global coherence (`0049-xcl-fixes`).

use crate::emit::bitcode::{Bin, Module, V, pred};
use crate::gfx10::Fields;

/// Out-of-range mip zeroing and console dimension shapes for `image_get_resinfo`:
/// - when `mip_to_check` >= level count, all three sizes (including layer count) become 0 while
///   the level count survives;
/// - with `console_shape`, depth is 0 for non-array 1D/2D, 1D arrays report layers in both y
///   and z, and cube maps report 6 faces.
pub(super) fn resinfo_sizes(
    m: &mut Module,
    block: usize,
    kind: u32,
    dimensions: [V; 4],
    mip_to_check: Option<V>,
    console_shape: bool,
) -> [V; 4] {
    let [width, height, depth, levels] = dimensions;
    let zero = m.i32(0);
    let one = m.i32(1);
    let mut sizes = match kind {
        1 => [width, one, if console_shape { zero } else { one }],
        2 => [width, height, if console_shape { zero } else { one }],
        3 => [width, height, zero],
        5 => [width, height, if console_shape { m.i32(6) } else { one }],
        6 => [width, height, if console_shape { height } else { one }],
        _ => [width, height, depth],
    };
    if let Some(mip) = mip_to_check {
        let exists = m.cmp(block, pred::IULT, mip, levels);
        for size in &mut sizes {
            *size = m.select(block, exists, *size, zero);
        }
    }
    let levels = if matches!(kind, 3 | 8) { one } else { levels };
    [sizes[0], sizes[1], sizes[2], levels]
}

/// D16 resource queries pack selected integer sizes into consecutive 16-bit
/// halves (low half first). An odd trailing component leaves its upper half 0.
pub(super) fn packed_resinfo(m: &mut Module, block: usize, components: &[V]) -> Vec<V> {
    let mask = m.i32(0xffff);
    let shift = m.i32(16);
    components
        .chunks(2)
        .map(|pair| {
            let low = m.bin(block, Bin::And, pair[0], mask);
            if let Some(&upper) = pair.get(1) {
                let high = m.bin(block, Bin::Shl, upper, shift);
                m.bin(block, Bin::Or, low, high)
            } else {
                low
            }
        })
        .collect()
}

/// Each packed mesh primitive index uses 9 bits in a 10-bit field; bit 9 is
/// reserved and counting it can cull a valid primitive.
pub(super) fn primitive_index_mask(nine_bits: bool) -> u32 {
    if nine_bits { 0x1ff } else { 0x3ff }
}

/// Marks the raw page-window UAV globally coherent when a compute shader uses
/// cache coherence flags (`glc`/`slc`/`dlc`), memory atomics or cache
/// invalidations (`VICTORY_COMPUTE_WINDOW_COHERENCE=1`).
pub(super) fn window_properties<'a>(
    compute: bool,
    instructions: impl Iterator<Item = (&'a str, &'a Fields)>,
) -> u32 {
    const RAW_UAV: u32 = 0x100b;
    const GLOBALLY_COHERENT: u32 = 1 << 14;
    if compute {
        for (name, fields) in instructions {
            let invalidates = matches!(
                name,
                "buffer_gl0_inv" | "buffer_gl1_inv" | "s_gl1_inv" | "s_dcache_inv"
            );
            let needs_visibility = match fields {
                Fields::Buffer { cache, .. }
                | Fields::Flat { cache, .. }
                | Fields::Smem { cache, .. } => {
                    cache.glc || cache.slc || cache.dlc || name.contains("_atomic_")
                }
                _ => false,
            };
            if invalidates || needs_visibility {
                return RAW_UAV | GLOBALLY_COHERENT;
            }
        }
    }
    RAW_UAV
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gfx10::{Cache, Fields, Operand};

    #[test]
    fn primitive_index_padding_is_independent_for_all_three_vertices() {
        let packed = 0x201 | (0x302 << 10) | (0x3ff << 20);
        let decode =
            |nine| [0, 10, 20].map(|shift: u32| (packed >> shift) & primitive_index_mask(nine));
        assert_eq!(decode(true), [1, 258, 511]);
        assert_eq!(decode(false), [513, 770, 1023]);
    }

    #[test]
    fn only_compute_windows_with_cross_group_visibility_are_coherent() {
        let read = |glc| Fields::Smem {
            data: None,
            base: Operand::Const(0),
            offset: 0,
            soffset: Operand::Const(0),
            cache: Cache {
                glc,
                ..Default::default()
            },
        };
        for (name, fields, expected) in [
            ("s_load_dword", read(false), 0x100b),
            ("s_load_dword", read(true), 0x500b),
            ("s_atomic_add", read(false), 0x500b),
            ("buffer_gl1_inv", Fields::Sopp { imm: 0 }, 0x500b),
            ("s_barrier", Fields::Sopp { imm: 0 }, 0x100b),
        ] {
            assert_eq!(
                window_properties(true, [(name, &fields)].into_iter()),
                expected
            );
            assert_eq!(
                window_properties(false, [(name, &fields)].into_iter()),
                0x100b
            );
        }
    }

    #[test]
    fn resinfo_shapes_and_d16_packing_produce_expected_ir() {
        let mut s = crate::emit::shader::Shader::new(false);
        let entry = s.entry;
        let dims = [s.m.i32(64), s.m.i32(32), s.m.i32(4), s.m.i32(5)];
        let mip = s.m.i32(6);
        let out = resinfo_sizes(&mut s.m, entry, 5, dims, Some(mip), true);
        assert_eq!(out[3], dims[3], "level count survives mip bounds");
        let packed = packed_resinfo(&mut s.m, entry, &out[..3]);
        assert_eq!(packed.len(), 2, "three components pack into two dwords");
    }
}
