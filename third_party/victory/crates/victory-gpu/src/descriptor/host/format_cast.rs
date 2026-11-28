//! (El1xr) Views of a texture in a format other than the one it was made in (#190 r12).
//!
//! The console lets the title view a fully typed texture in any format of its family: a
//! BC5_UNORM normal map as BC5_SNORM, R8G8B8A8_UNORM as _UINT or _SNORM, BC6H_UF16 as _SF16.
//! Before this, the host gave such a view the texture's own format ([`super::fits`]), so it
//! read without the view's conversion: a normal map read as UNORM has every normal in the
//! positive half (lighting gone flat or dark), an SNORM material read as UNORM is off by half.
//! Nothing logged it.
//!
//! Always on: the first time each (view format, texture format, kind of view) is seen, a
//! `view format:` line says what the host did with it. That includes views without a
//! description of a texture whose host copy is typeless (sRGB textures with unordered access,
//! depth textures shaders read), which were left null (black).
//!
//! `VICTORY_FORMAT_CAST=1` (restart): on a host that casts fully typed formats
//! (`CastingFullyTypedFormatSupported`), such a view keeps the format the title named, as the
//! relaxed casting rules allow: within the family, never between a float and a non-float
//! format, never between SNORM and UNORM. `=2` allows those two as well (the runtime lets them
//! through; the spec warns that some hardware compresses them differently: textures the title
//! fills are not compressed by the card). Unordered access views never name an sRGB format.
//!
//! `VICTORY_TYPELESS_WHOLE_VIEW=1` (restart): a view without a description of a texture whose
//! host copy is typeless gets a description of the whole texture in the title's format (the
//! readable colour format of a depth one; the linear twin of an sRGB one for unordered access).
//! Without it, that view is left null.

use std::sync::{Mutex, OnceLock};

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;

/// What kind of number a format holds, for the casting rules.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Kind {
    Float,
    Unorm,
    Snorm,
    Int,
    Other,
}

/// The kind of `format` (sRGB counts as UNORM; typeless and mixed formats are `Other`).
pub(super) fn kind(format: u32) -> Kind {
    match format {
        2 | 6 | 10 | 16 | 20 | 21 | 26 | 34 | 40 | 41 | 54 | 67 | 95 | 96 => Kind::Float,
        11
        | 24
        | 28
        | 29
        | 35
        | 45
        | 46
        | 49
        | 55
        | 56
        | 61
        | 65
        | 71
        | 72
        | 74
        | 75
        | 77
        | 78
        | 80
        | 83
        | 85..=89
        | 91
        | 93
        | 98
        | 99
        | 115 => Kind::Unorm,
        13 | 31 | 37 | 51 | 58 | 63 | 81 | 84 => Kind::Snorm,
        3 | 4 | 7 | 8 | 12 | 14 | 17 | 18 | 22 | 25 | 30 | 32 | 36 | 38 | 42 | 43 | 47 | 50 | 52
        | 57 | 59 | 62 | 64 => Kind::Int,
        _ => Kind::Other,
    }
}

/// `VICTORY_FORMAT_CAST`: 0 off, 1 the relaxed rules, 2 also float/non-float and SNORM/UNORM.
fn level() -> u8 {
    static LEVEL: OnceLock<u8> = OnceLock::new();
    *LEVEL.get_or_init(|| {
        let level = match std::env::var("VICTORY_FORMAT_CAST").ok().as_deref().map(str::trim) {
            Some("1") => 1,
            Some("2") => 2,
            _ => 0,
        };
        if level > 0 {
            tracing::info!(
                level,
                "VICTORY_FORMAT_CAST: views of typed textures in another format of their family keep it where the host casts"
            );
        }
        level
    })
}

/// Whether a view of a typed texture in `own` keeps `view` (same family, checked by the
/// caller) instead of taking `own`.
pub(super) fn keeps(view: u32, own: u32, unordered: bool) -> bool {
    decide(view, own, unordered, level(), super::srgb_cast::casts_typed)
}

fn decide(view: u32, own: u32, unordered: bool, level: u8, casts: impl FnOnce() -> bool) -> bool {
    if level == 0 || view == own {
        return false;
    }
    let (v, o) = (kind(view), kind(own));
    if v == Kind::Other || o == Kind::Other {
        return false;
    }
    if unordered && crate::resource::SRGB.iter().any(|&(srgb, _)| srgb == view) {
        return false;
    }
    let float = (v == Kind::Float) != (o == Kind::Float);
    let norm = matches!(
        (v, o),
        (Kind::Snorm, Kind::Unorm) | (Kind::Unorm, Kind::Snorm)
    );
    (level >= 2 || !(float || norm)) && casts()
}

/// What the host did with a view in a format other than its texture's.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Outcome {
    /// It keeps the title's format (sRGB or format cast).
    Kept,
    /// It reads in the texture's own format.
    Own,
    /// Its format is of another family: left null.
    OtherFamily,
    /// No description, and the host copy is typeless: left null.
    NoDescription,
    /// No description, and the host copy is typeless: the whole texture in the title's format.
    Whole,
}

/// Each (view, texture, unordered, outcome) seen, so each is logged once.
static SEEN: Mutex<Vec<(u32, u32, bool, Outcome)>> = Mutex::new(Vec::new());

/// Logs the first of each (view format, texture format, kind of view, outcome).
pub(super) fn note(view: u32, own: u32, unordered: bool, outcome: Outcome) {
    let key = (view, own, unordered, outcome);
    {
        let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
        if seen.len() >= 256 || seen.contains(&key) {
            return;
        }
        seen.push(key);
    }
    tracing::info!(
        view_format = view,
        texture_format = own,
        unordered,
        ?outcome,
        view_kind = ?kind(view),
        texture_kind = ?kind(own),
        "view format: a view of a texture in a format other than its host copy's"
    );
}

/// `VICTORY_TYPELESS_WHOLE_VIEW=1`.
pub(super) fn whole_view() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_TYPELESS_WHOLE_VIEW").is_some_and(|v| v == "1"))
}

/// The format a view without a description of a texture made in `own` (the title's format)
/// names: the readable colour format of a depth format; for unordered access, the linear
/// twin of an sRGB one.
pub(super) fn readable(own: u32, unordered: bool) -> u32 {
    match own {
        20 => 21,
        40 => 41,
        45 => 46,
        55 => 56,
        f if unordered => crate::resource::SRGB
            .iter()
            .find(|&&(srgb, _)| srgb == f)
            .map_or(f, |&(_, linear)| linear),
        f => f,
    }
}

/// The view of all of `d` (a texture) in `format`, as the console makes for a view without a
/// description.
pub(super) fn whole_srv(d: &D3D12_RESOURCE_DESC, format: u32) -> D3D12_SHADER_RESOURCE_VIEW_DESC {
    let mut v = D3D12_SHADER_RESOURCE_VIEW_DESC {
        Format: DXGI_FORMAT(format as i32),
        Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
        ..Default::default()
    };
    let slices = u32::from(d.DepthOrArraySize);
    match (d.Dimension, slices > 1, d.SampleDesc.Count > 1) {
        (D3D12_RESOURCE_DIMENSION_TEXTURE1D, false, _) => {
            v.ViewDimension = D3D12_SRV_DIMENSION_TEXTURE1D;
            v.Anonymous.Texture1D = D3D12_TEX1D_SRV {
                MipLevels: u32::MAX,
                ..Default::default()
            };
        }
        (D3D12_RESOURCE_DIMENSION_TEXTURE1D, true, _) => {
            v.ViewDimension = D3D12_SRV_DIMENSION_TEXTURE1DARRAY;
            v.Anonymous.Texture1DArray = D3D12_TEX1D_ARRAY_SRV {
                MipLevels: u32::MAX,
                ArraySize: slices,
                ..Default::default()
            };
        }
        (D3D12_RESOURCE_DIMENSION_TEXTURE3D, ..) => {
            v.ViewDimension = D3D12_SRV_DIMENSION_TEXTURE3D;
            v.Anonymous.Texture3D = D3D12_TEX3D_SRV {
                MipLevels: u32::MAX,
                ..Default::default()
            };
        }
        (_, false, true) => v.ViewDimension = D3D12_SRV_DIMENSION_TEXTURE2DMS,
        (_, true, true) => {
            v.ViewDimension = D3D12_SRV_DIMENSION_TEXTURE2DMSARRAY;
            v.Anonymous.Texture2DMSArray = D3D12_TEX2DMS_ARRAY_SRV {
                FirstArraySlice: 0,
                ArraySize: slices,
            };
        }
        (_, false, false) => {
            v.ViewDimension = D3D12_SRV_DIMENSION_TEXTURE2D;
            v.Anonymous.Texture2D = D3D12_TEX2D_SRV {
                MipLevels: u32::MAX,
                ..Default::default()
            };
        }
        (_, true, false) => {
            v.ViewDimension = D3D12_SRV_DIMENSION_TEXTURE2DARRAY;
            v.Anonymous.Texture2DArray = D3D12_TEX2D_ARRAY_SRV {
                MipLevels: u32::MAX,
                ArraySize: slices,
                ..Default::default()
            };
        }
    }
    v
}

/// The unordered access view of mip 0 of all of `d` in `format` (a view without a
/// description).
pub(super) fn whole_uav(d: &D3D12_RESOURCE_DESC, format: u32) -> D3D12_UNORDERED_ACCESS_VIEW_DESC {
    let mut v = D3D12_UNORDERED_ACCESS_VIEW_DESC {
        Format: DXGI_FORMAT(format as i32),
        ..Default::default()
    };
    let slices = u32::from(d.DepthOrArraySize);
    match (d.Dimension, slices > 1) {
        (D3D12_RESOURCE_DIMENSION_TEXTURE1D, false) => {
            v.ViewDimension = D3D12_UAV_DIMENSION_TEXTURE1D
        }
        (D3D12_RESOURCE_DIMENSION_TEXTURE1D, true) => {
            v.ViewDimension = D3D12_UAV_DIMENSION_TEXTURE1DARRAY;
            v.Anonymous.Texture1DArray = D3D12_TEX1D_ARRAY_UAV {
                ArraySize: slices,
                ..Default::default()
            };
        }
        (D3D12_RESOURCE_DIMENSION_TEXTURE3D, _) => {
            v.ViewDimension = D3D12_UAV_DIMENSION_TEXTURE3D;
            v.Anonymous.Texture3D = D3D12_TEX3D_UAV {
                WSize: u32::MAX,
                ..Default::default()
            };
        }
        (_, false) => v.ViewDimension = D3D12_UAV_DIMENSION_TEXTURE2D,
        (_, true) => {
            v.ViewDimension = D3D12_UAV_DIMENSION_TEXTURE2DARRAY;
            v.Anonymous.Texture2DArray = D3D12_TEX2D_ARRAY_UAV {
                ArraySize: slices,
                ..Default::default()
            };
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Off: nothing kept. On: within the relaxed rules only; at 2 also float/non-float and
    /// SNORM/UNORM; never without casting, never an sRGB unordered access view, never a
    /// typeless or mixed format.
    #[test]
    fn a_view_keeps_its_format_only_as_the_casting_rules_allow() {
        let yes = || true;
        // BC5 SNORM view of a BC5 UNORM normal map: only at 2.
        assert!(!decide(84, 83, false, 1, yes));
        assert!(decide(84, 83, false, 2, yes));
        // R8G8B8A8 UINT of UNORM, SINT of SNORM: at 1.
        assert!(decide(30, 28, false, 1, yes));
        assert!(decide(32, 31, true, 1, yes));
        // sRGB of UNORM as a shader view: at 1; as unordered access: never.
        assert!(decide(29, 28, false, 1, yes));
        assert!(!decide(29, 28, true, 2, yes));
        // R32 UINT of R32 FLOAT: only at 2. BC6H SF of UF (both float): at 1.
        assert!(!decide(42, 41, false, 1, yes));
        assert!(decide(42, 41, false, 2, yes));
        assert!(decide(96, 95, false, 1, yes));
        // Off, no casting, same format, typeless: no.
        assert!(!decide(30, 28, false, 0, yes));
        assert!(!decide(30, 28, false, 1, || false));
        assert!(!decide(28, 28, false, 2, yes));
        assert!(!decide(27, 28, false, 2, yes));
    }

    /// Every format of the families the host knows (1 to 99, and 115) that is not typeless
    /// has a kind; typeless ones have none.
    #[test]
    fn every_typed_format_has_a_kind() {
        const TYPELESS: [u32; 20] = [
            1, 5, 9, 15, 19, 23, 27, 33, 39, 44, 48, 53, 60, 70, 73, 76, 79, 82, 94, 97,
        ];
        // Mixed or plane formats with no one kind: R32G8X24 stencil view (23 is typeless too),
        // R10G10B10_XR_BIAS_A2 (89 is UNORM-like), packed video, typeless BGRA.
        const NONE: [u32; 6] = [0, 66, 68, 69, 90, 92];
        for f in (0..=99).chain([115]) {
            let k = kind(f);
            if TYPELESS.contains(&f) || NONE.contains(&f) {
                assert_eq!(k, Kind::Other, "{f}");
            } else {
                assert_ne!(k, Kind::Other, "{f}");
            }
        }
    }

    /// A view without a description names the readable format of a depth texture and the
    /// linear twin of an sRGB one for unordered access, and covers the whole texture.
    #[test]
    fn a_whole_view_names_a_readable_format_and_all_of_the_texture() {
        assert_eq!(readable(40, false), 41);
        assert_eq!(readable(20, false), 21);
        assert_eq!(readable(45, false), 46);
        assert_eq!(readable(55, false), 56);
        assert_eq!(readable(29, false), 29);
        assert_eq!(readable(29, true), 28);
        assert_eq!(readable(91, true), 87);
        let mut d = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Width: 64,
            Height: 64,
            DepthOrArraySize: 6,
            MipLevels: 7,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            ..Default::default()
        };
        let v = whole_srv(&d, 29);
        assert_eq!(v.ViewDimension, D3D12_SRV_DIMENSION_TEXTURE2DARRAY);
        // SAFETY: the member the dimension names.
        let a = unsafe { v.Anonymous.Texture2DArray };
        assert_eq!(
            (a.MipLevels, a.ArraySize, a.MostDetailedMip),
            (u32::MAX, 6, 0)
        );
        assert_eq!(v.Format.0, 29);
        d.DepthOrArraySize = 1;
        assert_eq!(
            whole_srv(&d, 41).ViewDimension,
            D3D12_SRV_DIMENSION_TEXTURE2D
        );
        d.SampleDesc.Count = 4;
        assert_eq!(
            whole_srv(&d, 41).ViewDimension,
            D3D12_SRV_DIMENSION_TEXTURE2DMS
        );
        d.Dimension = D3D12_RESOURCE_DIMENSION_TEXTURE3D;
        assert_eq!(
            whole_uav(&d, 28).ViewDimension,
            D3D12_UAV_DIMENSION_TEXTURE3D
        );
    }

    /// The switch is not inert here: a host with a D3D12 device casts fully typed formats.
    #[test]
    fn the_host_casts_typed_formats() {
        if crate::host::host().is_none() {
            return; // no host GPU here
        }
        assert!(super::super::srgb_cast::casts_typed());
    }
}
