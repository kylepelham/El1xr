//! `VICTORY_COMPUTE_QUAD_LOD=1`: a compute program's implicit-level samples take their level
//! from the console's lane quads (`quad_lod.rs`), lowered, validated by `dxil.dll` when
//! present, and run on the host GPU and WARP.

use super::host::View;
use super::lower::isa_fixes::{Fix, set_for_test};
use super::waterfall_tests::{lower_cs32, run};

/// 32 threads, a quad (lanes 4k..4k + 3) each, sample a 32x32 texture whose level `l` is red
/// `l` at `(0.5 + i d, 0.5 + j d)`, `i`, `j` the lane's quad column and row and
/// `d = 1.2 2^k / 32`, along x in even quads and along y in odd ones (the other 0): a footprint
/// of `1.2 2^k` texels, level `k + 0.26`, so the nearest level `k` (`+ 1` with a bias of 1),
/// and the deepest, 5, past it. `bias`: `image_sample_b` with a bias of 1.0.
fn program(bias: bool) -> Vec<u32> {
    let d = (1.2f32 / 32.0).to_bits();
    let mut w = vec![
        0xBE84_0300, // s_mov_b32 s4, s0
        0xBE85_0380, // s_mov_b32 s5, 0
        0xBE86_0301, // s_mov_b32 s6, s1
        0xBE87_0380, // s_mov_b32 s7, 0
        0xF40C_0602,
        0xFA00_0000, // s_load_dwordx8 s[24:31], s[4:5], 0x0
        0xF408_0203,
        0xFA00_0000, // s_load_dwordx4 s[8:11], s[6:7], 0x0
        0xF408_0A02,
        0xFA00_0040, // s_load_dwordx4 s[40:43], s[4:5], 0x40
        0x3420_0082, // v_lshlrev_b32 v16, 2, v0
        0x2C02_0082, // v_lshrrev_b32 v1, 2, v0 (the quad k)
        0x3604_0081, // v_and_b32 v2, 1, v0 (i)
        0x2C06_0081, // v_lshrrev_b32 v3, 1, v0
        0x3606_0681, // v_and_b32 v3, 1, v3 (j)
        0x3608_0281, // v_and_b32 v4, 1, v1 (odd k)
        0x4C0A_0881, // v_sub_nc_u32 v5, 1, v4
        0x3604_0505, // v_and_b32 v2, v5, v2
        0x3606_0704, // v_and_b32 v3, v4, v3
        0x7E04_0D02, // v_cvt_f32_u32 v2, v2
        0x7E06_0D03, // v_cvt_f32_u32 v3, v3
        0x7E0C_0281, // v_mov_b32 v6, 1
        0x340C_0D01, // v_lshlrev_b32 v6, v1, v6
        0x7E0C_0D06, // v_cvt_f32_u32 v6, v6
        0x100C_0CFF,
        d, // v_mul_f32 v6, 1.2 / 32, v6
        0x1004_0506, // v_mul_f32 v2, v6, v2
        0x1006_0706, // v_mul_f32 v3, v6, v3
        0x0604_04F0, // v_add_f32 v2, 0.5, v2
        0x0606_06F0, // v_add_f32 v3, 0.5, v3
        0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
    ];
    if bias {
        w.extend([
            0x7E0E_02F2, // v_mov_b32 v7, 1.0
            0x7E10_0302, // v_mov_b32 v8, v2
            0x7E12_0303, // v_mov_b32 v9, v3
            0xF094_0108,
            0x0046_0C07, // image_sample_b v12, v[7:9], s[24:31], s[8:11] dmask:0x1 dim:2D
        ]);
    } else {
        w.extend([
            0xF080_0108,
            0x0046_0C02, // image_sample v12, v[2:3], s[24:31], s[8:11] dmask:0x1 dim:2D
        ]);
    }
    w.extend([
        0xBF8C_3F70, // s_waitcnt vmcnt(0)
        0xE070_1000,
        0x800A_0C10, // buffer_store_dword v12, v16, s[40:43], 0 offen
        0xBF81_0000, // s_endpgm
    ]);
    w
}

/// The 32x32 texture's levels, level `l` red `l`.
fn mip_chain() -> Vec<[f32; 4]> {
    (0..6u32)
        .flat_map(|l| {
            let n = (32 >> l) * (32 >> l);
            std::iter::repeat_n([l as f32, 0.0, 0.0, 1.0], n)
        })
        .collect()
}

#[test]
fn compute_samples_take_their_level_from_the_lane_quads() {
    let texels = mip_chain();
    let views = [
        View::Mipped(32, 32, &texels),
        View::NullTexture,
        View::RawUav(32),
    ];
    for bias in [false, true] {
        for on in [true, false] {
            set_for_test(Fix::ComputeQuadLod, Some(on));
            let shader = lower_cs32(&program(bias));
            set_for_test(Fix::ComputeQuadLod, None);
            let results = run(&shader, &views);
            assert!(!results.is_empty());
            for (label, words) in results {
                let got: Vec<f32> = words.into_iter().map(f32::from_bits).collect();
                // Off: level 0, or the bias as the level.
                let want: Vec<f32> = (0..32u32)
                    .map(|lane| match (on, bias) {
                        (true, _) => (lane / 4 + u32::from(bias)).min(5) as f32,
                        (false, false) => 0.0,
                        (false, true) => 1.0,
                    })
                    .collect();
                assert_eq!(got, want, "{label}: quad LOD {on}, bias {bias}");
            }
        }
    }
}
