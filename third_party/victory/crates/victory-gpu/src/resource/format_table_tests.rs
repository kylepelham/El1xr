//! (El1xr) The host's format tables checked against DXGI's definitions and the console's
//! additions, format by format (#190 r13): bytes and texels per element, typeless family,
//! sRGB twins, depth formats and their readable views, the console's 16-bit depth and its
//! own colour formats.

use super::{DEPTH_FORMATS, SRGB, element, family, pc_format};

/// (format, bytes per element, element width in texels, typeless family) from DXGI's
/// definitions (`dxgiformat.h`) and the console's (116-120, 189-191). A family of the format
/// itself: none. Formats the host does not size (1-bit, video) are left out.
const REFERENCE: &[(u32, u64, u64, u32)] = &[
    // R32G32B32A32: TYPELESS, FLOAT, UINT, SINT.
    (1, 16, 1, 1),
    (2, 16, 1, 1),
    (3, 16, 1, 1),
    (4, 16, 1, 1),
    // R32G32B32.
    (5, 12, 1, 5),
    (6, 12, 1, 5),
    (7, 12, 1, 5),
    (8, 12, 1, 5),
    // R16G16B16A16: TYPELESS, FLOAT, UNORM, UINT, SNORM, SINT.
    (9, 8, 1, 9),
    (10, 8, 1, 9),
    (11, 8, 1, 9),
    (12, 8, 1, 9),
    (13, 8, 1, 9),
    (14, 8, 1, 9),
    // R32G32.
    (15, 8, 1, 15),
    (16, 8, 1, 15),
    (17, 8, 1, 15),
    (18, 8, 1, 15),
    // R32G8X24_TYPELESS, D32_FLOAT_S8X24_UINT, R32_FLOAT_X8X24_TYPELESS, X32_TYPELESS_G8X24_UINT.
    (19, 8, 1, 19),
    (20, 8, 1, 19),
    (21, 8, 1, 19),
    (22, 8, 1, 19),
    // R10G10B10A2: TYPELESS, UNORM, UINT; R11G11B10_FLOAT.
    (23, 4, 1, 23),
    (24, 4, 1, 23),
    (25, 4, 1, 23),
    (26, 4, 1, 26),
    // R8G8B8A8: TYPELESS, UNORM, UNORM_SRGB, UINT, SNORM, SINT.
    (27, 4, 1, 27),
    (28, 4, 1, 27),
    (29, 4, 1, 27),
    (30, 4, 1, 27),
    (31, 4, 1, 27),
    (32, 4, 1, 27),
    // R16G16: TYPELESS, FLOAT, UNORM, UINT, SNORM, SINT.
    (33, 4, 1, 33),
    (34, 4, 1, 33),
    (35, 4, 1, 33),
    (36, 4, 1, 33),
    (37, 4, 1, 33),
    (38, 4, 1, 33),
    // R32: TYPELESS, D32_FLOAT, FLOAT, UINT, SINT.
    (39, 4, 1, 39),
    (40, 4, 1, 39),
    (41, 4, 1, 39),
    (42, 4, 1, 39),
    (43, 4, 1, 39),
    // R24G8_TYPELESS, D24_UNORM_S8_UINT, R24_UNORM_X8_TYPELESS, X24_TYPELESS_G8_UINT.
    (44, 4, 1, 44),
    (45, 4, 1, 44),
    (46, 4, 1, 44),
    (47, 4, 1, 44),
    // R8G8: TYPELESS, UNORM, UINT, SNORM, SINT.
    (48, 2, 1, 48),
    (49, 2, 1, 48),
    (50, 2, 1, 48),
    (51, 2, 1, 48),
    (52, 2, 1, 48),
    // R16: TYPELESS, FLOAT, D16_UNORM, UNORM, UINT, SNORM, SINT.
    (53, 2, 1, 53),
    (54, 2, 1, 53),
    (55, 2, 1, 53),
    (56, 2, 1, 53),
    (57, 2, 1, 53),
    (58, 2, 1, 53),
    (59, 2, 1, 53),
    // R8: TYPELESS, UNORM, UINT, SNORM, SINT; A8_UNORM (no typeless).
    (60, 1, 1, 60),
    (61, 1, 1, 60),
    (62, 1, 1, 60),
    (63, 1, 1, 60),
    (64, 1, 1, 60),
    (65, 1, 1, 65),
    // R9G9B9E5_SHAREDEXP; R8G8_B8G8 and G8R8_G8B8 (4 bytes a pair of texels).
    (67, 4, 1, 67),
    (68, 2, 1, 68),
    (69, 2, 1, 69),
    // BC1, BC2, BC3: TYPELESS, UNORM, UNORM_SRGB.
    (70, 8, 4, 70),
    (71, 8, 4, 70),
    (72, 8, 4, 70),
    (73, 16, 4, 73),
    (74, 16, 4, 73),
    (75, 16, 4, 73),
    (76, 16, 4, 76),
    (77, 16, 4, 76),
    (78, 16, 4, 76),
    // BC4, BC5: TYPELESS, UNORM, SNORM.
    (79, 8, 4, 79),
    (80, 8, 4, 79),
    (81, 8, 4, 79),
    (82, 16, 4, 82),
    (83, 16, 4, 82),
    (84, 16, 4, 82),
    // B5G6R5, B5G5R5A1; B8G8R8A8_UNORM, B8G8R8X8_UNORM, R10G10B10_XR_BIAS_A2_UNORM;
    // B8G8R8A8 TYPELESS, SRGB; B8G8R8X8 TYPELESS, SRGB.
    (85, 2, 1, 85),
    (86, 2, 1, 86),
    (87, 4, 1, 90),
    (88, 4, 1, 92),
    (89, 4, 1, 89),
    (90, 4, 1, 90),
    (91, 4, 1, 90),
    (92, 4, 1, 92),
    (93, 4, 1, 92),
    // BC6H: TYPELESS, UF16, SF16; BC7: TYPELESS, UNORM, UNORM_SRGB.
    (94, 16, 4, 94),
    (95, 16, 4, 94),
    (96, 16, 4, 94),
    (97, 16, 4, 97),
    (98, 16, 4, 97),
    (99, 16, 4, 97),
    // B4G4R4A4_UNORM.
    (115, 2, 1, 115),
    // The console: R10G10B10_7E3_A2_FLOAT, R10G10B10_6E4_A2_FLOAT, D16_UNORM_S8_UINT,
    // R16_UNORM_X8_TYPELESS, X16_TYPELESS_G8_UINT (two planes: 3 bytes a texel).
    (116, 4, 1, 116),
    (117, 4, 1, 117),
    (118, 3, 1, 118),
    (119, 3, 1, 119),
    (120, 3, 1, 120),
    // R10G10B10_SNORM_A2_UNORM, R4G4_UNORM, and 191 (4 bytes a texel on the console).
    (189, 4, 1, 189),
    (190, 1, 1, 190),
    (191, 4, 1, 191),
];

#[test]
fn element_sizes_and_families_match_the_definitions() {
    for &(format, bytes, width, typeless) in REFERENCE {
        assert_eq!(element(format), Some((bytes, width)), "element of {format}");
        assert_eq!(family(format), typeless, "family of {format}");
    }
    // 1-bit, video and unknown formats are not sized.
    for format in [
        0, 66, 100, 103, 106, 107, 110, 113, 114, 121, 130, 131, 132, 192,
    ] {
        assert_eq!(element(format), None, "element of {format}");
    }
}

#[test]
fn every_srgb_format_is_paired_with_its_linear_twin() {
    // (sRGB, linear): RGBA8, BGRA8, BGRX8 (BC1-3 and BC7 are paired in `srgb_cast`).
    assert_eq!(SRGB, [(29, 28), (91, 87), (93, 88)]);
    for (srgb, linear) in SRGB {
        assert_eq!(family(srgb), family(linear), "{srgb}/{linear}");
    }
}

#[test]
fn depth_formats_name_their_typeless_family() {
    for (typed, typeless) in DEPTH_FORMATS {
        assert_eq!(family(typed), typeless, "{typed}");
    }
    // D32_FLOAT_S8X24, D32_FLOAT, D24_UNORM_S8, D16_UNORM: every PC depth format.
    let mut typed: Vec<u32> = DEPTH_FORMATS.iter().map(|&(t, _)| t).collect();
    typed.sort_unstable();
    assert_eq!(typed, [20, 40, 45, 55]);
}

#[test]
fn the_console_16_bit_depth_maps_to_the_24_bit_one_plane_by_plane() {
    // D16_UNORM_S8_UINT -> D24_UNORM_S8_UINT; its depth view -> R24_UNORM_X8; its stencil
    // view -> X24_TYPELESS_G8_UINT (stencil in G on both).
    assert_eq!(
        [pc_format(118), pc_format(119), pc_format(120)],
        [45, 46, 47]
    );
    for format in (1..=115).chain([116, 117, 189, 190, 191]) {
        assert_eq!(pc_format(format), format, "{format} is the host's own");
    }
}
