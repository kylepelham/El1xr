//! `VICTORY_CS_STORE_CLAMP=<1|2>` (unset by default): the float image stores of compute
//! programs (typed UAV stores, `image_store`) are held to what the console's HDR formats hold.
//! An infinity or a value past ±65024 (R11G11B10_FLOAT's largest, the smallest end of the HDR
//! formats) is stored as ±65024, a NaN as 0, in place of the NaN scrub (which makes an infinity
//! 0) or guard.
//!
//! #96: soft dark squares over the bright street seen through the diner's window glass. The
//! window's transmission pyramid (`CS_AlphTransmissionPrefilterMip` from the scene colour, then
//! `CS_AlphaTransmissionBlurGaussianX/Y`, `Denoise`, `HistoryResolve`, all R11G11B10_FLOAT) is
//! sampled by the glass draw; the glass reflection chain around the trace
//! (`CS_RaytraceVBuffer_*` 6d7fe7745ab9c6d8: `CS_DeferredFallbacksAlpha_SkyProbe`,
//! `CS_RayMiss`, `CS_ResolveScreenSpace`, `CS_RayBudgeting_Fill`, ...) stores its light the same
//! way. An infinity there made 0 by the scrub is a black texel per level, a square once
//! sampled. `VICTORY_HDR_CLAMP` holds only pixel colour outputs; this covers the compute stores.
//!
//! - `1`: held as above.
//! - `2`: as `1`, and a texel with a NaN in any channel is not written: it keeps what the
//!   image held (last frame's value in a history or pyramid image) instead of becoming black.
//!
//! `VICTORY_CS_STORE_CLAMP_ONLY` picks the programs (to bisect): unset, empty or `all`: every
//! compute program; `glass`: the programs whose name holds one of [`GLASS`]; otherwise a comma
//! list of program hashes (16 hex digits, the runtime's `blake3` short hash) and name parts.
//! A 32-bit float image whose numbers are data (a far distance past 65024) is held too: keep
//! such programs off the list (the glass list holds none; `CS_RayPrePass_Alpha` writes ray
//! distances into R32_FLOAT and is left out).
//!
//! Both are part of the translation: the shader cache keys on every `VICTORY_*` name this crate
//! reads.

use std::cell::Cell;
use std::sync::OnceLock;

/// How a listed program's float image stores are held.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Hold {
    /// `1`: NaN 0, everything else within ±65024.
    Clamp,
    /// `2`: as [`Hold::Clamp`], and a texel with a NaN is not written.
    KeepNan,
}

/// The name parts `VICTORY_CS_STORE_CLAMP_ONLY=glass` selects: the window transmission pyramid's
/// colour stages and the glass reflection chain around the alpha trace (frame ops dump
/// `blackface/dev/cap_face_gs2a/frame-1024-ops.txt`). Not the pyramid's R16_FLOAT depth tiles
/// nor `CS_GatherAlphaVBuffer`: depths, not light.
pub(super) const GLASS: [&str; 14] = [
    "CS_AlphTransmissionPrefilterMip",
    "CS_AlphaTransmissionBlurGaussian",
    "CS_AlphaTransmissionDenoise",
    "CS_AlphaTransmissionHistoryResolve",
    "CS_DeferredFallbacksAlpha",
    "CS_RayMiss_",
    "CS_ResolveScreenSpace",
    "CS_DummyResolve",
    "CS_RayBudgeting_Fill",
    "CS_ShadeAlpha",
    "CS_VarianceFilterAlpha",
    "CS_SpatialFilterAlpha",
    "CS_SpatialFilter_",
    "CS_ScreenProbes_TemporalResolve_ReflectiveCaustics",
];

fn parse_mode(value: Option<&str>) -> Option<Hold> {
    match value?.trim() {
        "1" => Some(Hold::Clamp),
        "2" => Some(Hold::KeepNan),
        _ => None,
    }
}

/// The switch's program list; empty: every compute program.
fn parse_only(value: Option<&str>) -> Vec<String> {
    match value.map(str::trim) {
        None | Some("") => Vec::new(),
        Some(v) if v.eq_ignore_ascii_case("all") => Vec::new(),
        Some(v) if v.eq_ignore_ascii_case("glass") => GLASS.iter().map(|&n| n.to_owned()).collect(),
        Some(v) => v
            .split(',')
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .map(|n| {
                if is_hash(n) {
                    n.to_ascii_lowercase()
                } else {
                    n.to_owned()
                }
            })
            .collect(),
    }
}

fn is_hash(entry: &str) -> bool {
    entry.len() == 16 && entry.bytes().all(|b| b.is_ascii_hexdigit())
}

fn mode() -> Option<Hold> {
    static MODE: OnceLock<Option<Hold>> = OnceLock::new();
    *MODE.get_or_init(|| parse_mode(std::env::var("VICTORY_CS_STORE_CLAMP").ok().as_deref()))
}

fn only() -> &'static [String] {
    static ONLY: OnceLock<Vec<String>> = OnceLock::new();
    ONLY.get_or_init(|| parse_only(std::env::var("VICTORY_CS_STORE_CLAMP_ONLY").ok().as_deref()))
}

/// Whether the program in `container` is on the list `only` (empty: every program).
fn listed(container: &[u8], only: &[String]) -> bool {
    if only.is_empty() {
        return true;
    }
    let mut hash = None;
    only.iter().any(|entry| {
        if is_hash(entry) {
            let short = hash.get_or_insert_with(|| blake3::hash(container).to_hex());
            &short[..16] == entry.as_str()
        } else {
            let part = entry.as_bytes();
            container.windows(part.len()).any(|w| w == part)
        }
    })
}

thread_local! {
    /// The hold for the program being translated on this thread ([`with_container`]).
    static HERE: Cell<Option<Hold>> = const { Cell::new(None) };
}

/// Runs the translation `f` of `container` with [`here`] set when the switch is on and lists it.
pub(super) fn with_container<R>(container: &[u8], f: impl FnOnce() -> R) -> R {
    let hold = mode().filter(|_| listed(container, only()));
    if hold.is_none() {
        return f();
    }
    with_hold(hold, f)
}

/// Runs `f` with [`here`] at `hold`, restored after even when `f` panics.
fn with_hold<R>(hold: Option<Hold>, f: impl FnOnce() -> R) -> R {
    struct Restore(Option<Hold>);
    impl Drop for Restore {
        fn drop(&mut self) {
            HERE.set(self.0);
        }
    }
    let _restore = Restore(HERE.replace(hold));
    f()
}

/// The hold for the float image stores of the program being translated (compute stage only;
/// the caller checks the stage).
pub(super) fn here() -> Option<Hold> {
    HERE.get()
}

#[cfg(test)]
pub(super) fn with_hold_for_test<R>(hold: Option<Hold>, f: impl FnOnce() -> R) -> R {
    with_hold(hold, f)
}

/// What a held store writes for a channel `v` (what `Lower::hold_store` emits): `None` for a
/// NaN under [`Hold::KeepNan`] (the texel is not written).
#[cfg(test)]
pub(super) fn held(hold: Hold, v: f32) -> Option<f32> {
    use super::lower::hdr_hold::HDR_MAX;
    if v.is_nan() {
        return (hold == Hold::Clamp).then_some(0.0);
    }
    Some(v.clamp(-HDR_MAX, HDR_MAX))
}

#[cfg(test)]
#[path = "store_hold_tests.rs"]
mod tests;
