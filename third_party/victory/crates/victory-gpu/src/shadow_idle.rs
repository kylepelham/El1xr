//! (El1xr) `VICTORY_SHADOW_TRACE_IDLE`: with no ray-tracing structure to hit (every ray a miss,
//! `VICTORY_BVH_NO_HIT=1` or `VICTORY_RT_MISS=1`, the structure builder off, and not
//! `VICTORY_NATIVE_SHADOW=1`), the sun shadow's ray trace is bound and its writes declared as
//! for any dispatch, but runs no groups. Its program writes what its rays found over the shadow
//! mask the cascades made; with nothing to hit they find nothing and those screen tiles come out
//! fully lit (whole-tile steps along shadow edges, sun through walls). Not run, the mask stays as
//! the cascades made it. Leaving the dispatch out instead dropped what goes with it (its writes
//! were not declared nor brought back to title memory) and the open world stuttered 150 ms and
//! more, 29 to 116 times a walk.
//!
//! - `1`: `CS_TraceShadowRays_Checkerboard` only.
//! - `2`: every `CS_TraceShadowRays_*` (Quarter, Half, Eighth, Full, FullAdaptive, Checkerboard).
//!
//! After Skyfrye/anyXboxx-diffbase `2903af0f35` (`VICTORY_SHADOW_CHECKERBOARD`, GPL-3.0-or-later).
//! Not in the shader or pipeline key: nothing translated or made changes, only whether the
//! dispatch's groups run.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// Dispatches that ran no groups ([`idle`]), since the start.
pub(crate) static IDLED: AtomicU64 = AtomicU64::new(0);

/// `VICTORY_SHADOW_TRACE_IDLE`: 0 off, 1 Checkerboard, 2 every TraceShadowRays.
fn mode_of(value: Option<&str>) -> u8 {
    match value.map(str::trim) {
        Some("1") => 1,
        Some("2") => 2,
        _ => 0,
    }
}

/// Whether nothing a shadow ray could hit is there, from the switches `on` reads.
fn nothing_to_hit(on: impl Fn(&str) -> bool) -> bool {
    (on("VICTORY_BVH_NO_HIT") || on("VICTORY_RT_MISS"))
        && !on("VICTORY_BVH_BUILDER")
        && !on("VICTORY_NATIVE_SHADOW")
}

/// Whether a compute container is one this mode idles.
fn named(mode: u8, container: &[u8]) -> bool {
    let has = |name: &[u8]| container.windows(name.len()).any(|w| w == name);
    match mode {
        1 => has(b"CS_TraceShadowRays_Checkerboard"),
        2 => has(b"CS_TraceShadowRays"),
        _ => false,
    }
}

/// The mode in force: 0 when the switch is off or a structure may be hit.
fn mode() -> u8 {
    static MODE: OnceLock<u8> = OnceLock::new();
    *MODE.get_or_init(|| {
        let mode = mode_of(std::env::var("VICTORY_SHADOW_TRACE_IDLE").ok().as_deref());
        let on = |name: &str| std::env::var_os(name).is_some_and(|v| v == "1");
        let stands = mode != 0 && nothing_to_hit(on) && !victory_shader::emit::native_shadow();
        if mode != 0 && !stands {
            tracing::info!(
                "shadow trace idle (VICTORY_SHADOW_TRACE_IDLE={mode}) stands down: a structure may be hit \
                 (needs VICTORY_BVH_NO_HIT=1 or VICTORY_RT_MISS=1, no VICTORY_BVH_BUILDER=1, no VICTORY_NATIVE_SHADOW=1)"
            );
        }
        if stands { mode } else { 0 }
    })
}

/// Whether a pipeline with these compute containers is idled, when made.
pub(crate) fn of<'a>(containers: impl IntoIterator<Item = &'a [u8]>) -> bool {
    let mode = mode();
    mode != 0 && containers.into_iter().any(|c| named(mode, c))
}

/// Counts one idled dispatch, saying so the first time.
pub(crate) fn idled() {
    if IDLED.fetch_add(1, Relaxed) == 0 {
        tracing::info!(
            mode = mode(),
            "shadow trace idle: a sun shadow ray trace dispatch bound and declared, run with no groups \
             (VICTORY_SHADOW_TRACE_IDLE; nothing to hit)"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_mode_and_the_programs_it_idles() {
        assert_eq!(
            [None, Some("0"), Some("1"), Some(" 2 "), Some("x")].map(mode_of),
            [0, 0, 1, 2, 0]
        );
        let checker = b"..\0CS_TraceShadowRays_Checkerboard_559d9f01_Wrapped\0..".as_slice();
        let half = b"..\0CS_TraceShadowRays_Half_002e93a6_Wrapped\0..".as_slice();
        let other = b"..\0CS_RTDistantShadowTrace\0..".as_slice();
        assert!(named(1, checker) && !named(1, half) && !named(1, other));
        assert!(named(2, checker) && named(2, half) && !named(2, other));
        assert!(!named(0, checker));
    }

    #[test]
    fn it_stands_down_where_a_structure_may_be_hit() {
        let with = |set: &'static [&'static str]| move |name: &str| set.contains(&name);
        assert!(nothing_to_hit(with(&["VICTORY_BVH_NO_HIT"])));
        assert!(nothing_to_hit(with(&["VICTORY_RT_MISS"])));
        assert!(!nothing_to_hit(with(&[])));
        assert!(!nothing_to_hit(with(&[
            "VICTORY_BVH_NO_HIT",
            "VICTORY_BVH_BUILDER"
        ])));
        assert!(!nothing_to_hit(with(&[
            "VICTORY_BVH_NO_HIT",
            "VICTORY_NATIVE_SHADOW"
        ])));
    }
}
