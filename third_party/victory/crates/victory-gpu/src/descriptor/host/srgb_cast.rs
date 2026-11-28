//! (blackface, issue #35) `VICTORY_SRGB_CAST=1` (off by default, read at
//! start): a shader view of a typed texture in its sRGB or linear twin (an
//! sRGB view of an R8G8B8A8_UNORM texture, a linear view of a BC7_UNORM_SRGB
//! one) keeps the format the title named when the host casts fully typed
//! formats (`CastingFullyTypedFormatSupported`), so the hardware converts,
//! or not, as the console does. Off, such a view takes the texture's own
//! format ([`super::fits`]) and reads without the conversion: skin, albedo
//! bakes, outfits and screens come out too bright or too dark. Unordered
//! access views keep their rules (no sRGB UAV on the host).
//!
//! Source: TaynanGT/Vice-Tay 84e6476 (AnoHunted), from Marvisak/VIctory
//! #331 `ccdecb8`.

use std::sync::OnceLock;

use windows::Win32::Graphics::Direct3D12::*;

/// Uncompressed sRGB formats and their linear twins
/// ([`crate::resource::SRGB`]), and the BC ones: (sRGB, linear).
const BC_SRGB: [(u32, u32); 4] = [(72, 71), (75, 74), (78, 77), (99, 98)];

/// Whether `a` and `b` are the sRGB and linear twins of one format, either way.
pub(super) fn twins(a: u32, b: u32) -> bool {
    let pair = (a.max(b), a.min(b));
    crate::resource::SRGB.contains(&pair) || BC_SRGB.contains(&pair)
}

/// Whether the switch is on (`VICTORY_SRGB_CAST=1`).
fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        let on = std::env::var_os("VICTORY_SRGB_CAST").is_some_and(|v| v == "1");
        if on {
            tracing::info!("VICTORY_SRGB_CAST: sRGB and linear views of typed textures keep their own format where the host casts");
        }
        on
    })
}

/// Whether the host lets a view of a typed texture name another format of
/// its family (`CastingFullyTypedFormatSupported`), asked once.
pub(super) fn casts_typed() -> bool {
    static CASTS: OnceLock<bool> = OnceLock::new();
    *CASTS.get_or_init(|| {
        let Some(host) = crate::host::host() else {
            return false;
        };
        let mut options = D3D12_FEATURE_DATA_D3D12_OPTIONS3::default();
        // SAFETY: the query writes its own structure of the given size.
        let asked = unsafe {
            host.device.CheckFeatureSupport(
                D3D12_FEATURE_D3D12_OPTIONS3,
                (&raw mut options).cast(),
                size_of_val(&options) as u32,
            )
        };
        let casts = asked.is_ok() && options.CastingFullyTypedFormatSupported.as_bool();
        if !casts {
            tracing::warn!(
                "VICTORY_SRGB_CAST: the host does not cast typed formats: sRGB views of linear textures (and the reverse) read in the texture's own format"
            );
        }
        casts
    })
}

/// Whether a view of a typed texture in `own` keeps `view`, the format the
/// title named, instead of taking `own`.
pub(super) fn keeps(view: u32, own: u32, unordered: bool) -> bool {
    decide(view, own, unordered, on(), casts_typed)
}

fn decide(view: u32, own: u32, unordered: bool, on: bool, casts: impl FnOnce() -> bool) -> bool {
    on && !unordered && twins(view, own) && casts()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn twins_pair_srgb_and_linear_either_way() {
        for (srgb, linear) in [
            (29, 28),
            (91, 87),
            (93, 88),
            (72, 71),
            (75, 74),
            (78, 77),
            (99, 98),
        ] {
            assert!(
                twins(srgb, linear) && twins(linear, srgb),
                "{srgb}/{linear}"
            );
        }
        // Same format, UINT of UNORM, SNORM of UNORM, BC1 of BC2: no.
        for (a, b) in [(28, 28), (30, 28), (31, 28), (71, 75), (29, 91)] {
            assert!(!twins(a, b), "{a}/{b}");
        }
    }

    /// With the switch and the host's casting, an sRGB shader view of a
    /// linear texture (and the reverse) keeps its format; never a UAV, a
    /// format that is not a twin, without casting, or with the switch off.
    #[test]
    fn a_twin_view_keeps_its_format_only_with_the_switch_and_casting() {
        assert!(decide(29, 28, false, true, || true));
        assert!(decide(28, 29, false, true, || true));
        assert!(decide(72, 71, false, true, || true));
        assert!(!decide(29, 28, false, false, || true), "switch off");
        assert!(!decide(29, 28, false, true, || false), "no casting");
        assert!(!decide(29, 28, true, true, || true), "unordered access");
        assert!(!decide(30, 28, false, true, || true), "not a twin");
    }
}
