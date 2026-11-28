//! (El1xr) `VICTORY_RAY_TRACING=off|on`: the menu's one ray-tracing switch
//! over the set of switches that makes the title's ray tracing run.
//!
//! - `off` (the default): nothing is added. The launcher's stand-in stays:
//!   the structure builder off, every ray a miss (`VICTORY_BVH_NO_HIT=1`),
//!   the ambient light in place of what the rays would find.
//! - `on`: [`init`](super::init) adds [`ON`] to the environment, after the
//!   settings file and before any reader (and the shader cache key) looks.
//!   A switch of [`ON`] the environment or the settings file already sets
//!   keeps its value, so a single switch can still be turned back
//!   (`VICTORY_BVH_PREMAP=0`) or added (`VICTORY_BVH_QUARANTINE=1`).
//!
//! What the stand-in does where the rays now find something steps aside in
//! the GPU crate (`ambient::steps_aside`): with only the reflections
//! walking the structures ([`ON`]'s `VICTORY_RT_HIT_ONLY`), the reflection
//! stand-ins (ambient specular, sky bound) step aside and the diffuse one
//! stays, because the light probes' rays still miss. `VICTORY_BVH_BUILDER=1`
//! already overrides `VICTORY_BVH_NO_HIT` and the miss-sky scales.
//!
//! Several switches of [`ON`] are in the shader cache key, so `on` and `off`
//! are two cache variants; `lancer_victory.ps1 -Precompile
//! -PrecompileRayTracing` (`dev.py prewarm --ray-tracing-both`) warms both.

/// The setting's name.
pub const NAME: &str = "VICTORY_RAY_TRACING";

/// What `on` adds: the builder with its fixes (batch 5 part F arm R3: no
/// quarantine, the title gets its read-backs), hits for the glass and
/// opaque reflections' trace only, and room on the card for the windows the
/// builder writes, so their writes are compared there and not snapshotted on
/// the CPU (part F: 9.7 GB snapshotted a frame, 0.75 fps). 2,048 MB, not
/// the 8,192 of part I (5.4 fps, then out of the PC's commit at 70 s); past
/// the windows' own total a card side also needs the card under eight
/// tenths and the PC's commit above a floor (`VICTORY_BVH_COMMIT_FLOOR_MB`).
/// The texture prepare threads commit their first MiB of stack at start
/// (part I's stack overflow on a PC out of commit).
pub const ON: &[(&str, &str)] = &[
    ("VICTORY_BVH_BUILDER", "1"),
    ("VICTORY_BVH_BUILDER_EARLY", "1"),
    ("VICTORY_BVH_CARD_WINDOWS_MB", "2048"),
    ("VICTORY_BVH_HOOK", "1"),
    ("VICTORY_BVH_PREMAP", "1"),
    ("VICTORY_DISPATCHX_MASK", "1"),
    ("VICTORY_DISPATCHX_THREADS", "1"),
    ("VICTORY_PREPARE_STACK_COMMIT", "1"),
    ("VICTORY_RT_HIT_ONLY", "6d7fe7745ab9c6d8"),
];

/// Whether `value` turns ray tracing on.
pub fn is_on(value: Option<&str>) -> bool {
    value.is_some_and(|v| v.trim().eq_ignore_ascii_case("on"))
}

/// The switches to add for `value`: [`ON`]'s that `set` says are not set
/// yet; none unless `value` is `on`.
pub fn expansion(
    value: Option<&str>,
    set: impl Fn(&str) -> bool,
) -> Vec<(&'static str, &'static str)> {
    if !is_on(value) {
        return Vec::new();
    }
    ON.iter().copied().filter(|(name, _)| !set(name)).collect()
}

/// Whether this run started with ray tracing on.
pub fn on() -> bool {
    is_on(std::env::var(NAME).ok().as_deref())
}

/// The log line for what [`expansion`] added; none when it added nothing
/// and ray tracing is off.
pub fn line(value: Option<&str>, added: &[(&str, &str)]) -> Option<String> {
    if !is_on(value) {
        return None;
    }
    let added: Vec<String> = added.iter().map(|(n, v)| format!("{n}={v}")).collect();
    let kept: Vec<&str> = ON
        .iter()
        .map(|(n, _)| *n)
        .filter(|n| !added.iter().any(|a| a.split('=').next() == Some(n)))
        .collect();
    Some(format!(
        "settings: ray tracing on: added {} ({}), {} kept as set ({})",
        added.len(),
        added.join(" "),
        kept.len(),
        kept.join(" ")
    ))
}

#[cfg(test)]
mod tests {
    use super::super::{Apply, Kind, info, valid};
    use super::*;

    #[test]
    fn on_adds_the_whole_set_and_off_adds_nothing() {
        assert_eq!(expansion(Some("on"), |_| false), ON);
        assert_eq!(expansion(Some(" ON "), |_| false), ON);
        for value in [None, Some(""), Some("off"), Some("1"), Some("yes")] {
            assert!(expansion(value, |_| false).is_empty(), "{value:?}");
            assert_eq!(line(value, &[]), None);
        }
    }

    #[test]
    fn a_switch_already_set_keeps_its_value() {
        let added = expansion(Some("on"), |n| n == "VICTORY_BVH_PREMAP" || n == "VICTORY_RT_HIT_ONLY");
        assert_eq!(added.len(), ON.len() - 2);
        assert!(added.iter().all(|(n, _)| *n != "VICTORY_BVH_PREMAP" && *n != "VICTORY_RT_HIT_ONLY"));
        let line = line(Some("on"), &added).unwrap();
        assert!(line.contains("added 7 (VICTORY_BVH_BUILDER=1 "), "{line}");
        assert!(line.ends_with("2 kept as set (VICTORY_BVH_PREMAP VICTORY_RT_HIT_ONLY)"), "{line}");
    }

    #[test]
    fn the_set_is_registered_valid_and_its_own_shader_cache_variant() {
        for (name, value) in ON {
            let i = info(name).unwrap_or_else(|| panic!("{name} is not registered"));
            assert!(valid(i.kind, value), "{name}={value}");
        }
        let keyed: Vec<&str> = ON
            .iter()
            .map(|(n, _)| *n)
            .filter(|n| info(n).unwrap().apply == Apply::Retranslate)
            .collect();
        assert_eq!(keyed, ["VICTORY_BVH_BUILDER", "VICTORY_DISPATCHX_MASK", "VICTORY_RT_HIT_ONLY"]);
        assert!(ON.iter().all(|(n, _)| *n != "VICTORY_BVH_NO_HIT" && *n != "VICTORY_BVH_QUARANTINE"));
    }

    #[test]
    fn the_menu_shows_it_under_display_with_a_restart() {
        let i = info(NAME).unwrap();
        assert_eq!((i.group, i.label, i.default), ("Display", "Ray tracing", "off"));
        assert_eq!(i.kind, Kind::Choice(&["off", "on"]));
        assert_eq!(i.apply, Apply::Restart);
        assert!(!i.expert);
    }
}
