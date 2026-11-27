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

/// For the shader cache key: the list, when set.
pub(crate) fn key() -> Option<String> {
    let list = list();
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
}
