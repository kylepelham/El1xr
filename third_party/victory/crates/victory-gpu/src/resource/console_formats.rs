//! (El1xr) Render targets and unordered access textures in the console's own colour formats
//! (#190 r13).
//!
//! The console's DXGI adds 116 (R10G10B10_7E3_A2_FLOAT), 117 (R10G10B10_6E4_A2_FLOAT),
//! 189 (R10G10B10_SNORM_A2_UNORM) and 190 (R4G4_UNORM). The host has none of them: 116/117
//! are no format there, and 189/190 are its sampler feedback formats. A texture in one was
//! passed to the host as is, which refuses it, so every view of it read zeros (black) and
//! every pass that drew into it drew nothing, with only the `console format:` line once.
//!
//! `VICTORY_CONSOLE_FORMATS=1` (restart): where the title renders to or writes such a texture
//! (the host makes its texels, the title never fills it itself), the host copy is made in a
//! stand-in that holds every value the console's format does, through the same path as the
//! other target stand-ins ([`super::target_format`]): views, render target and clear formats
//! and pipelines name the stand-in. With the switch off the host could not make pipelines
//! drawing into these formats at all, so no cached pipeline names the console format.
//! - 116, 117: R16G16B16A16_FLOAT (10): 11-bit mantissas over the 7e3/6e4 ranges, alpha too.
//! - 189: R16G16B16A16_SNORM (13): signed colour clamped to [-1, 1] as the console does.
//! - 190: R8G8_UNORM (49): every 4-bit level exact.
//!
//! shortcut: textures in these formats the title fills itself (never rendered to) stay
//! refused; their bytes would need converting on upload.

use std::sync::OnceLock;

/// The console format and its stand-in.
const STAND_INS: [(u32, u32); 4] = [(116, 10), (117, 10), (189, 13), (190, 49)];

/// `VICTORY_CONSOLE_FORMATS=1`.
fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        let on = parse(std::env::var("VICTORY_CONSOLE_FORMATS").ok().as_deref());
        if on {
            tracing::info!(
                "VICTORY_CONSOLE_FORMATS: targets in the console's own colour formats (116, 117, 189, 190) are made in a stand-in"
            );
        }
        on
    })
}

fn parse(value: Option<&str>) -> bool {
    value.map(str::trim) == Some("1")
}

/// The stand-ins with the switch as `on` says.
fn listed(on: bool) -> &'static [(u32, u32)] {
    if on { &STAND_INS } else { &[] }
}

/// The console formats that get a stand-in where the host refuses them, and the stand-in.
pub(crate) fn stand_ins() -> &'static [(u32, u32)] {
    listed(on())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_switch_lists_the_stand_ins_only_when_set() {
        assert!(listed(false).is_empty());
        assert_eq!(listed(true), &[(116, 10), (117, 10), (189, 13), (190, 49)]);
        assert!(parse(Some("1")) && parse(Some(" 1 ")));
        assert!(!parse(None) && !parse(Some("0")) && !parse(Some("2")));
    }

    /// Every stand-in keeps at least the console format's channels and range: bits per
    /// channel (r, g, b, a) and whether it is signed, float.
    #[test]
    fn each_stand_in_holds_its_console_format() {
        // (format, channel bits, signed colour, float)
        let shape = |f: u32| match f {
            116 | 117 => ([10, 10, 10, 2], false, true),
            189 => ([10, 10, 10, 2], true, false),
            190 => ([4, 4, 0, 0], false, false),
            10 => ([16, 16, 16, 16], true, true),
            13 => ([16, 16, 16, 16], true, false),
            49 => ([8, 8, 0, 0], false, false),
            _ => unreachable!(),
        };
        for &(console, host) in &STAND_INS {
            let (cb, cs, cf) = shape(console);
            let (hb, hs, hf) = shape(host);
            for k in 0..4 {
                assert!(hb[k] >= cb[k], "{console} -> {host} channel {k}");
            }
            assert!(hs || !cs, "{console} -> {host}: signed");
            assert_eq!(cf, hf, "{console} -> {host}: float");
        }
    }

    /// Without the switch the host refuses textures in these formats as targets (so the
    /// title's draws into them went nowhere); with it the stand-in it gets is made. Real
    /// device; skipped without one.
    #[cfg(windows)]
    #[test]
    fn the_host_refuses_console_formats_and_makes_their_stand_ins() {
        use windows::Win32::Graphics::Direct3D12::*;
        use windows::Win32::Graphics::Dxgi::Common::*;
        let Some(host) = crate::host::host() else {
            eprintln!("skipped: no host GPU here");
            return;
        };
        let make = |format: u32, flags: D3D12_RESOURCE_FLAGS| {
            let desc = D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
                Width: 64,
                Height: 64,
                DepthOrArraySize: 1,
                MipLevels: 1,
                Format: DXGI_FORMAT(format as i32),
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Flags: flags,
                ..Default::default()
            };
            let heap = D3D12_HEAP_PROPERTIES {
                Type: D3D12_HEAP_TYPE_DEFAULT,
                ..Default::default()
            };
            let mut made: Option<ID3D12Resource> = None;
            // SAFETY: plain descriptions of our own.
            let r = unsafe {
                host.device.CreateCommittedResource(
                    &heap,
                    D3D12_HEAP_FLAG_NONE,
                    &desc,
                    D3D12_RESOURCE_STATE_COMMON,
                    None,
                    &mut made,
                )
            };
            r.is_ok() && made.is_some()
        };
        let target = D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET;
        let both = target | D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS;
        for &(console, host_format) in &STAND_INS {
            assert!(!make(console, target), "the host made a {console} target");
            assert!(
                make(host_format, target),
                "stand-in {host_format} of {console} as a target"
            );
            assert!(
                make(host_format, both),
                "stand-in {host_format} of {console} with unordered access"
            );
        }
    }
}
