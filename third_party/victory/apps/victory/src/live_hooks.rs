//! (El1xr) The settings registry's live hooks for what this process can change
//! while the title runs (`victory_core::settings::on_change`): the upscaler,
//! the DLSS quality and sharpness (`victory_gpu::upscale_live`), and the
//! FidelityFX render scale (`victory_loader::resolution`). A setting gets its
//! hook only when the startup made a live change possible; otherwise it stays
//! a restart setting, and the in-game menu says so.

use victory_core::settings::on_change;
use victory_gpu::upscale_live::{Live, Request, parse_quality, request};

/// Registers the hooks once the image is patched: `upscale` when the title's
/// upscale dispatch is ours (`VICTORY_UPSCALE` was set and taken).
pub fn register(upscale: bool) {
    if upscale {
        on_change("VICTORY_UPSCALE", |v| match Live::parse(v) {
            Some(mode) => request(Request {
                mode: Some(mode),
                ..Request::default()
            }),
            None => tracing::warn!(value = v, "VICTORY_UPSCALE: not an upscaler; unchanged"),
        });
        on_change("VICTORY_DLSS_QUALITY", |v| match parse_quality(v) {
            Some(q) => request(Request {
                quality: Some(q),
                ..Request::default()
            }),
            None => tracing::warn!(
                value = v,
                "VICTORY_DLSS_QUALITY: not a quality mode; unchanged"
            ),
        });
        on_change("VICTORY_DLSS_SHARPNESS", |v| {
            match v.trim().parse::<f32>() {
                Ok(s) => request(Request {
                    sharpness: Some(s),
                    ..Request::default()
                }),
                Err(_) => {
                    tracing::warn!(value = v, "VICTORY_DLSS_SHARPNESS: not a number; unchanged")
                }
            }
        });
    }
    if victory_loader::resolution::ffx_scale_live() {
        on_change("VICTORY_FFX_SCALE", |v| {
            let asked = v.trim().parse::<f32>();
            let done = asked
                .map_err(|_| "not a number")
                .and_then(victory_loader::resolution::request_ffx_scale);
            if let Err(why) = done {
                tracing::warn!(
                    value = v,
                    why,
                    "VICTORY_FFX_SCALE: render scale not changed"
                );
            }
        });
    }
}
