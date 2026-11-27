use super::*;

fn shift(dx_ndc: f32) -> Mat4 {
    [
        [1.0, 0.0, 0.0, dx_ndc],
        [0.0, 1.0, 0.0, 0.0],
        [0.0, 0.0, 1.0, 0.0],
        [0.0, 0.0, 0.0, 1.0],
    ]
}

#[test]
fn the_grid_takes_cell_centres_inside_the_texture() {
    let g = grid([3840, 2160]);
    assert_eq!(g.len(), SAMPLES);
    assert_eq!(g[0], [240, 135]);
    assert_eq!(g[SAMPLES - 1], [3600, 2025]);
    assert!(grid([4, 2]).iter().all(|p| p[0] < 4 && p[1] < 2));
}

#[test]
fn texels_are_read_in_the_formats_depth_and_motion_take() {
    let one = 0.75f32.to_bits().to_le_bytes();
    assert_eq!(depth_texel(DXGI_FORMAT_R32_TYPELESS.0, &one), Some(0.75));
    assert_eq!(depth_texel(DXGI_FORMAT_D32_FLOAT.0, &one), Some(0.75));
    assert_eq!(depth_texel(DXGI_FORMAT_R24G8_TYPELESS.0, &[0xff, 0xff, 0xff, 0x12]), Some(1.0));
    assert_eq!(depth_texel(DXGI_FORMAT_R16_UNORM.0, &[0, 0, 0, 0]), Some(0.0));
    assert_eq!(depth_texel(DXGI_FORMAT_R8G8B8A8_UNORM.0, &one), None);
    // Halves: 1.5 = 0x3e00, -2.0 = 0xc000, 2^-24 = 0x0001.
    let mut p = [0u8; 16];
    p[..2].copy_from_slice(&0x3e00u16.to_le_bytes());
    p[2..4].copy_from_slice(&0xc000u16.to_le_bytes());
    assert_eq!(motion_texel(DXGI_FORMAT_R16G16_FLOAT.0, &p), Some([1.5, -2.0]));
    assert_eq!(motion_texel(DXGI_FORMAT_R16G16B16A16_TYPELESS.0, &p), Some([1.5, -2.0]));
    p[..4].copy_from_slice(&3.25f32.to_le_bytes());
    p[4..8].copy_from_slice(&(-1.0f32).to_le_bytes());
    assert_eq!(motion_texel(DXGI_FORMAT_R32G32_FLOAT.0, &p), Some([3.25, -1.0]));
    assert_eq!(motion_texel(DXGI_FORMAT_R11G11B10_FLOAT.0, &p), None);
    assert_eq!(half(0x0001), 2f32.powi(-24));
    assert!(half(0x7c00).is_infinite());
}

#[test]
fn judge_counts_agreement_either_sign_and_leaves_the_sky_out() {
    let size = [1920u32, 1080];
    // Clip x moves by 0.01: 9.6 pixels to the right in the previous frame.
    let m = shift(0.01);
    let mut samples: Vec<MotionSample> = grid(size)
        .iter()
        .map(|p| MotionSample {
            pixel: [p[0] as f32 + 0.5, p[1] as f32 + 0.5],
            depth: 0.5,
            motion: [9.6, 0.0],
        })
        .collect();
    // A car with motion of its own, and a sky pixel (depth 0) that would
    // not agree.
    samples[3].motion = [40.0, 2.0];
    samples[5].depth = 0.0;
    samples[5].motion = [0.0, 0.0];
    let v = judge(&m, &samples, size);
    assert_eq!(v.n, SAMPLES - 1);
    assert!((v.agree - (SAMPLES - 2) as f32 / (SAMPLES - 1) as f32).abs() < 1e-6, "{v:?}");
    assert_eq!(v.flipped, 0.0);
    assert!((v.median_px - 9.6).abs() < 1e-4);
    // Vectors written as current minus previous: found by the flipped count.
    for s in &mut samples {
        s.motion = [-s.motion[0], -s.motion[1]];
    }
    let v = judge(&m, &samples, size);
    assert!(v.agree < 0.05 && v.flipped > 0.9, "{v:?}");
    assert_eq!(judge(&m, &[], size), Verdict::default());
}
