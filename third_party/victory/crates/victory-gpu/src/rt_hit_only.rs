//! `VICTORY_RT_HIT_ONLY=<list>` (unset by default): every ray is a miss
//! except in the listed programs, which walk the structures.
//!
//! With the structure builder running (`VICTORY_BVH_BUILDER=1`) every
//! traced pass walks its structures: GI probes, shadows, reflections, about
//! 1,000 resolve programs. The list keeps the others' node tests a miss, as
//! `VICTORY_BVH_NO_HIT=1` makes them (`victory_shader::emit::with_rays_missed`),
//! so a few passes can be tried with hits at a fraction of full ray-tracing
//! cost: the glass and opaque reflections' `CS_RaytraceVBuffer`
//! (`6d7fe7745ab9c6d8`), say.
//!
//! The list is comma-separated: a 16-digit shader hash as the log names
//! shaders, or else a piece of the program's entry-point name the
//! container holds (`CS_RaytraceVBuffer`). Programs without node tests are
//! not changed by it. The list is part of the shader cache key.

use std::sync::OnceLock;

/// The list, read once; empty when unset.
fn list() -> &'static [String] {
    static LIST: OnceLock<Vec<String>> = OnceLock::new();
    LIST.get_or_init(|| parse(std::env::var("VICTORY_RT_HIT_ONLY").ok().as_deref()))
}

fn is_hash(token: &str) -> bool {
    token.len() == 16 && token.bytes().all(|b| b.is_ascii_hexdigit())
}

fn parse(value: Option<&str>) -> Vec<String> {
    let mut list: Vec<String> = value
        .unwrap_or("")
        .split(',')
        .map(str::trim)
        .filter(|t| !t.is_empty() && *t != "0")
        .map(|t| {
            if is_hash(t) {
                t.to_ascii_lowercase()
            } else {
                t.to_owned()
            }
        })
        .collect();
    list.sort();
    list.dedup();
    list
}

/// Whether `container` (whose log hash is `hash`) is named by `list`.
fn listed(container: &[u8], hash: &str, list: &[String]) -> bool {
    list.iter().any(|t| {
        if is_hash(t) {
            t == hash
        } else {
            container.windows(t.len()).any(|w| w == t.as_bytes())
        }
    })
}

/// Whether `container`'s node tests all miss: a list is set and does not
/// name it.
pub(crate) fn misses(container: &[u8]) -> bool {
    let list = list();
    if list.is_empty() {
        return false;
    }
    let hash = blake3::hash(container).to_hex();
    !listed(container, &hash[..16], list)
}

/// The glass and opaque reflections' trace, by entry-point name
/// ([`crate::pipeline::GLASS_TRACE`] by hash).
const REFLECTIONS: &str = "CS_RaytraceVBuffer_RayBinning_ShaderBinning_Mixed_WithFar";
/// The light probes' traces, by the part of the name they share (their
/// hashes are not known here).
const PROBES: &str = "CS_ScreenProbes_TraceRays_";

/// Whether the program `name` (log hash `hash`, empty when not known) walks
/// the structures by `list`: unset, or naming it.
fn walks(name: &str, hash: &str, list: &[String]) -> bool {
    list.is_empty() || listed(name.as_bytes(), hash, list)
}

/// Whether the reflections' trace walks the structures (with the builder
/// running): for the reflection stand-ins (`crate::ambient`).
pub(crate) fn reflections_walk() -> bool {
    walks(REFLECTIONS, crate::pipeline::GLASS_TRACE, list())
}

/// Whether the light probes' traces walk the structures (with the builder
/// running): for the diffuse stand-in (`crate::ambient`).
pub(crate) fn probes_walk() -> bool {
    walks(PROBES, "", list())
}

/// For the shader cache key: the list, when set.
pub(crate) fn key() -> Option<String> {
    key_of(list())
}

fn key_of(list: &[String]) -> Option<String> {
    (!list.is_empty()).then(|| format!("VICTORY_RT_HIT_ONLY={}", list.join(",")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unset_lists_nothing() {
        assert!(parse(None).is_empty());
        assert!(parse(Some("")).is_empty());
        assert!(parse(Some("0")).is_empty());
        assert!(key().is_none());
        assert!(!misses(b"anything"));
    }

    #[test]
    fn hashes_and_names_are_listed() {
        let list = parse(Some(
            " 6D7FE7745AB9C6D8 ,CS_RaytraceVBuffer,6d7fe7745ab9c6d8",
        ));
        assert_eq!(list, ["6d7fe7745ab9c6d8", "CS_RaytraceVBuffer"]);
        let glass = b"..\0CS_RaytraceVBuffer_RayBinning_ShaderBinning_Mixed_WithFar\0..";
        let probes = b"..\0CS_ScreenProbes_TraceRays_RayBinning_BVH_Mixed\0..";
        assert!(listed(glass, "0000000000000000", &list));
        assert!(listed(probes, "6d7fe7745ab9c6d8", &list));
        assert!(!listed(probes, "0000000000000000", &list));
        let only_hash = parse(Some("6d7fe7745ab9c6d8"));
        assert!(!listed(glass, "0000000000000000", &only_hash));
    }

    /// (rtlight) `VICTORY_RAY_TRACING=on` is a shader cache variant of its own: every switch
    /// it adds that changes translations goes into the key (`crate::shader_cache`), by its
    /// value in the environment (the translator's fingerprint names it) or by the list's key.
    #[test]
    fn ray_tracing_on_is_its_own_cache_variant() {
        use victory_core::settings::{self, Apply};
        let fingerprint = victory_shader::emit::translation_fingerprint();
        let named = |name: &str| {
            let name = format!("{name}=");
            fingerprint.windows(name.len()).any(|w| w == name.as_bytes())
        };
        let mut keyed = 0;
        for (name, _) in settings::ray_tracing::ON {
            if settings::info(name).unwrap().apply != Apply::Retranslate {
                continue;
            }
            keyed += 1;
            assert!(named(name) || *name == "VICTORY_RT_HIT_ONLY", "{name} is not in the key");
        }
        assert_eq!(keyed, 3);
        assert!(named("VICTORY_BVH_BUILDER") && named("VICTORY_DISPATCHX_MASK"));
    }

    /// `VICTORY_RAY_TRACING=on`'s list is a key of its own.
    #[test]
    fn the_ray_tracing_list_is_keyed() {
        let (name, value) = victory_core::settings::ray_tracing::ON
            .iter()
            .find(|(n, _)| *n == "VICTORY_RT_HIT_ONLY")
            .unwrap();
        assert_eq!(key_of(&parse(Some(value))), Some(format!("{name}={value}")));
        assert_eq!(key_of(&[]), None);
    }

    /// `VICTORY_RAY_TRACING=on`'s list walks the reflections, not the
    /// probes; no list walks both; a probe name walks the probes.
    #[test]
    fn reflections_and_probes_walk_by_the_list() {
        let glass = crate::pipeline::GLASS_TRACE;
        let on = parse(Some("6d7fe7745ab9c6d8"));
        assert!(walks(REFLECTIONS, glass, &on));
        assert!(!walks(PROBES, "", &on));
        assert!(walks(REFLECTIONS, glass, &[]) && walks(PROBES, "", &[]));
        let probes = parse(Some("CS_ScreenProbes"));
        assert!(walks(PROBES, "", &probes) && !walks(REFLECTIONS, glass, &probes));
        let by_name = parse(Some("CS_RaytraceVBuffer"));
        assert!(walks(REFLECTIONS, glass, &by_name) && !walks(PROBES, "", &by_name));
    }
}
