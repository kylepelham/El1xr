//! `VICTORY_BUFFER_FORMAT_CHECK=1`: a typed buffer view whose format the card
//! cannot view a buffer in is left null.
//!
//! [`super::buffer_view_ok`] checks a buffer view's range and that its format
//! has a size and is not typeless, but not that the card takes that format for
//! a buffer: sRGB and depth formats have a size and no buffer support, and
//! some buffer formats take shader resource views but not typed unordered
//! access views (`R32G32B32_FLOAT` on many cards). Making such a view is
//! undefined and can remove the device. With the switch, a typed view (not raw,
//! not structured) needs `D3D12_FORMAT_SUPPORT1_BUFFER`, an unordered access
//! view also `TYPED_UNORDERED_ACCESS_VIEW`; the answer is asked once per format.
//! A refused view takes the same null path as one `buffer_view_ok` refuses, and
//! the first refusal of each format and kind is logged:
//! `typed buffer view refused: the card has no buffer support for its format`.
//!
//! Live: views made while it was on stay as they were made until rewritten.
//!
//! Source: trustno1-coder/VIctory-NVIDIA PR #74 (`1124487`, mattiolocoding),
//! GPL-3.0.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT;

fn on() -> bool {
    static ON: victory_core::settings::Live =
        victory_core::settings::Live::flag("VICTORY_BUFFER_FORMAT_CHECK");
    ON.get_bool()
}

/// Formats asked about ([`support`]): every DXGI format with a size fits.
const FORMATS: usize = 256;

/// Support bits a typed buffer view of this kind needs.
fn required(uav: bool) -> i32 {
    if uav {
        D3D12_FORMAT_SUPPORT1_BUFFER.0 | D3D12_FORMAT_SUPPORT1_TYPED_UNORDERED_ACCESS_VIEW.0
    } else {
        D3D12_FORMAT_SUPPORT1_BUFFER.0
    }
}

/// The card's answer lacks what a typed buffer view of this kind needs. Pure, for testing.
fn lacks(support: i32, uav: bool) -> bool {
    let need = required(uav);
    support & need != need
}

/// The card's `Support1` for `format`, asked once; None without a card, for a format past
/// [`FORMATS`] or when the card does not answer (the view then goes on as before).
fn support(format: DXGI_FORMAT) -> Option<i32> {
    static ASKED: [OnceLock<Option<i32>>; FORMATS] = [const { OnceLock::new() }; FORMATS];
    let slot = ASKED.get(usize::try_from(format.0).ok()?)?;
    *slot.get_or_init(|| {
        let host = crate::host::host()?;
        let mut data = D3D12_FEATURE_DATA_FORMAT_SUPPORT {
            Format: format,
            ..Default::default()
        };
        // SAFETY: a plain query into our own structure.
        let asked = unsafe {
            host.device.CheckFeatureSupport(
                D3D12_FEATURE_FORMAT_SUPPORT,
                (&raw mut data).cast(),
                size_of_val(&data) as u32,
            )
        };
        asked.ok().map(|()| data.Support1.0)
    })
}

/// Formats and kinds whose first refusal was logged: bit `format * 2 + uav`.
static SAID: [AtomicU64; FORMATS * 2 / 64] = [const { AtomicU64::new(0) }; FORMATS * 2 / 64];

/// Views refused here since start.
pub(super) static REFUSED: AtomicU64 = AtomicU64::new(0);

/// With the switch on, a typed buffer view of `format` the card cannot make: the caller
/// leaves it null. Raw and structured views are not asked about.
pub(super) fn refused(format: DXGI_FORMAT, stride: u32, raw: bool, uav: bool) -> bool {
    if raw || stride > 0 || !on() {
        return false;
    }
    let Some(have) = support(format) else {
        return false;
    };
    if !lacks(have, uav) {
        return false;
    }
    let total = REFUSED.fetch_add(1, Relaxed) + 1;
    let bit = format.0 as usize * 2 + usize::from(uav);
    let mask = 1u64 << (bit % 64);
    if SAID[bit / 64].fetch_or(mask, Relaxed) & mask == 0 {
        tracing::warn!(
            format = format.0,
            uav,
            support = format!("{have:#x}"),
            total,
            "typed buffer view refused: the card has no buffer support for its format (VICTORY_BUFFER_FORMAT_CHECK)"
        );
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_view_needs_buffer_support_and_a_uav_typed_uav_support() {
        let buffer = D3D12_FORMAT_SUPPORT1_BUFFER.0;
        let typed_uav = D3D12_FORMAT_SUPPORT1_TYPED_UNORDERED_ACCESS_VIEW.0;
        // sRGB/depth: no buffer support at all
        assert!(lacks(D3D12_FORMAT_SUPPORT1_TEXTURE2D.0, false));
        assert!(lacks(D3D12_FORMAT_SUPPORT1_TEXTURE2D.0, true));
        // a shader resource buffer format without typed UAVs (R32G32B32_FLOAT)
        assert!(!lacks(buffer, false));
        assert!(lacks(buffer, true));
        assert!(!lacks(buffer | typed_uav, true));
    }

    #[test]
    fn raw_and_structured_views_are_not_asked_about() {
        use windows::Win32::Graphics::Dxgi::Common::*;
        assert!(!refused(DXGI_FORMAT_R32_TYPELESS, 0, true, true));
        assert!(!refused(DXGI_FORMAT_UNKNOWN, 16, false, false));
    }

    /// On a machine with a card: the formats PR #74 names, through the real query.
    #[test]
    fn the_card_refuses_srgb_and_depth_buffers() {
        use windows::Win32::Graphics::Dxgi::Common::*;
        if crate::host::host().is_none() {
            return;
        }
        for format in [DXGI_FORMAT_R8G8B8A8_UNORM_SRGB, DXGI_FORMAT_D32_FLOAT] {
            let have = support(format).expect("the card answers");
            assert!(lacks(have, false), "format {} has buffer support", format.0);
        }
        for format in [DXGI_FORMAT_R32_FLOAT, DXGI_FORMAT_R32_UINT] {
            let have = support(format).expect("the card answers");
            assert!(
                !lacks(have, false) && !lacks(have, true),
                "format {}",
                format.0
            );
        }
    }
}
