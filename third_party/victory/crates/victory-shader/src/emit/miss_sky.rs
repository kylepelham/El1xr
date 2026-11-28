//! `VICTORY_RT_MISS_SKY=<0..1>` and `VICTORY_RT_GLASS_SKY=<0..1>` (unset by
//! default; only with every ray a miss, `VICTORY_BVH_NO_HIT=1` without the
//! structure builder): how much of the sky and probe light a reflection ray
//! gets when it misses.
//!
//! With no structures every reflection ray misses, and a miss is lit as a
//! ray that left the scene: `sr_tracing`'s `CS_RayMiss` (the opaque
//! reflections) and `CS_DeferredFallbacksAlpha_SkyProbe` (the glass) shade it
//! from the sky and the environment probes and store that as the ray's
//! light. Indoors on the console those rays hit the room, so the prison cell
//! and the diner show the bright sky on glass and dark surfaces. Each program
//! stores the ray's light once, as `colour * exposure` with alpha 0; the
//! switch scales those three channels of that one store (0 lights a miss as
//! occluded). Both values are part of the shader cache key.

use std::cell::Cell;
use std::sync::OnceLock;

/// A program whose miss light is scaled: its entry point's name in the
/// container, the byte offset of the `image_store` of the ray's light in its
/// code, and the switch.
const PROGRAMS: [(&[u8], usize, Switch); 2] = [
    (
        b"sr_tracing:CS_RayMiss_de6b91a6_Wrapped\0",
        0x5948,
        Switch::Miss,
    ),
    (
        b"sr_tracing:CS_DeferredFallbacksAlpha_SkyProbe_99ec4412_Wrapped\0",
        0x3dd8,
        Switch::Glass,
    ),
];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Switch {
    Miss,
    Glass,
}

impl Switch {
    fn name(self) -> &'static str {
        match self {
            Switch::Miss => "VICTORY_RT_MISS_SKY",
            Switch::Glass => "VICTORY_RT_GLASS_SKY",
        }
    }
}

/// A switch's value: a scale in `0..=1`, `None` when unset, unparsable or 1.
fn parse(value: Option<&str>) -> Option<f32> {
    let v: f32 = value?.trim().parse().ok()?;
    (v.is_finite() && v < 1.0).then(|| v.max(0.0))
}

/// The scales of `VICTORY_RT_MISS_SKY` and `VICTORY_RT_GLASS_SKY`, read once
/// (both `None` unless every ray is a miss).
fn scales() -> [Option<f32>; 2] {
    static SCALES: OnceLock<[Option<f32>; 2]> = OnceLock::new();
    *SCALES.get_or_init(|| {
        let read = |s: Switch| {
            super::lower::intersect::no_hit()
                .then(|| parse(std::env::var(s.name()).ok().as_deref()))
                .flatten()
        };
        [read(Switch::Miss), read(Switch::Glass)]
    })
}

fn scale_of(s: Switch, scales: [Option<f32>; 2]) -> Option<f32> {
    scales[match s {
        Switch::Miss => 0,
        Switch::Glass => 1,
    }]
}

/// The store to scale in the program `container` holds, and by how much.
fn store_in(container: &[u8], scales: [Option<f32>; 2]) -> Option<(usize, f32)> {
    let has = |name: &[u8]| container.windows(name.len()).any(|w| w == name);
    PROGRAMS
        .iter()
        .find(|p| has(p.0))
        .and_then(|&(_, offset, s)| Some((offset, scale_of(s, scales)?)))
}

/// For the shader cache key: the switches' values, when either applies.
pub fn miss_sky_key() -> Option<String> {
    let [m, g] = scales();
    (m.is_some() || g.is_some())
        .then(|| format!("VICTORY_RT_MISS_SKY={m:?};VICTORY_RT_GLASS_SKY={g:?}"))
}

thread_local! {
    /// The store the translation on this thread scales ([`with_container`]).
    static STORE: Cell<Option<(usize, f32)>> = const { Cell::new(None) };
}

/// Runs the translation `f` of `container` with its miss store known.
pub(super) fn with_container<R>(container: &[u8], f: impl FnOnce() -> R) -> R {
    let store = store_in(container, scales());
    if store.is_none() {
        return f();
    }
    let before = STORE.replace(store);
    let r = f();
    STORE.set(before);
    r
}

/// The scale for the light an `image_store` at byte `offset` of the program
/// being translated stores, if it is the program's miss store.
pub(super) fn scale_at(offset: usize) -> Option<f32> {
    STORE.get().and_then(|(at, s)| (at == offset).then_some(s))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_parse_to_a_scale_below_one() {
        assert_eq!(parse(None), None);
        assert_eq!(parse(Some("")), None);
        assert_eq!(parse(Some("1")), None);
        assert_eq!(parse(Some("2")), None);
        assert_eq!(parse(Some("nan")), None);
        assert_eq!(parse(Some("0")), Some(0.0));
        assert_eq!(parse(Some("0.25")), Some(0.25));
        assert_eq!(parse(Some("-1")), Some(0.0));
    }

    #[test]
    fn each_program_takes_its_own_switch() {
        let miss = b"..\0sr_tracing:CS_RayMiss_de6b91a6_Wrapped\0..";
        let vol = b"..\0sr_tracing:CS_RayMiss_Volumetrics_3c5d8c00_Wrapped\0..";
        let glass = b"..\0sr_tracing:CS_DeferredFallbacksAlpha_SkyProbe_99ec4412_Wrapped\0..";
        let water = b"..\0sr_tracing:CS_DeferredFallbacksWater_SkyProbe_05c8acba_Wrapped\0..";
        let both = [Some(0.0), Some(0.5)];
        assert_eq!(store_in(miss, both), Some((0x5948, 0.0)));
        assert_eq!(store_in(glass, both), Some((0x3dd8, 0.5)));
        assert_eq!(store_in(vol, both), None);
        assert_eq!(store_in(water, both), None);
        assert_eq!(store_in(miss, [None, Some(0.5)]), None);
        assert_eq!(store_in(glass, [Some(0.0), None]), None);
    }

    #[test]
    fn only_the_named_store_is_scaled() {
        let c = b"..\0sr_tracing:CS_RayMiss_de6b91a6_Wrapped\0..";
        STORE.set(store_in(c, [Some(0.25), None]));
        assert_eq!(scale_at(0x5948), Some(0.25));
        assert_eq!(scale_at(0x3dd8), None);
        STORE.set(None);
        assert_eq!(scale_at(0x5948), None);
    }

    #[test]
    fn off_by_default() {
        assert_eq!(miss_sky_key(), None);
    }
}
