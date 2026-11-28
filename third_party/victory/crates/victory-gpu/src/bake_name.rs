//! `VICTORY_BAKE_BY_NAME=1|2` (#35; flipflapov/victory_upifork `e5c58d3480` after edsuuu
//! `10ac68f9d2`, snaxky915-dotcom `03ab74a2ee`, cyrex39 `9cfcf45e48`): the title's decoration
//! bake programs are known by the name inside their container, not only by the 7 hashes of
//! [`crate::draw::BAKE_PIXEL_SHADERS`].
//!
//! The title has some 51 bake pixel programs (`PS_BakeDecorations_*`, `PS_DecorationBlend_*`).
//! Every bake switch (`VICTORY_BAKE_INPUTS`, `_FRESH`, `_WHY`, `_CHECK`, `_PUBLISH`, the bake
//! budget of the early texture pass, `VICTORY_SCENE_INPUT` for bakes) acts only on draws whose
//! pipeline is a bake (`HostPipeline::is_bake`), which was the 7 hashes alone: a face baked by
//! another permutation got no pass of its own and read whatever its slots showed (flipflapov: the
//! cutscene's faces, baked by a permutation not in their list, came out black).
//!
//! `1`: a pixel program whose container names `PS_BakeDecorations_` or `PS_DecorationBlend_`
//! is a bake too. Its parameters are found as the known ones' are (`crate::bake_inputs`): 96
//! bytes after the records' descriptor for `PS_BakeDecorations_*`, 32 for the blends, and the
//! other offset when that one holds no constant buffer. `2`: also the two geometry stages
//! snaxky915 names (the full-screen ped baker vertex stage `089d6271be6650ef`, the decoration
//! projection mesh stage `d4111a6e6ea49d91`): those draws get the bake budget of the early pass
//! and `VICTORY_SCENE_INPUT`'s wait. Read once (pipelines are made once).

use std::sync::OnceLock;

/// The two families of bake pixel programs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    /// `PS_BakeDecorations_*`: parameters 96 bytes after the records' descriptor.
    Decorations,
    /// `PS_DecorationBlend_*`: 32 bytes after.
    Blend,
}

/// The geometry stages of bakes (`2`).
pub(crate) const GEOMETRY: [&str; 2] = ["089d6271be6650ef", "d4111a6e6ea49d91"];

const DECORATIONS: &[u8] = b"PS_BakeDecorations_";
const BLEND: &[u8] = b"PS_DecorationBlend_";

/// 0 off, 1 pixel programs by name, 2 and the geometry stages.
pub(crate) fn mode() -> u8 {
    static MODE: OnceLock<u8> = OnceLock::new();
    *MODE.get_or_init(|| parse(std::env::var("VICTORY_BAKE_BY_NAME").ok().as_deref()))
}

fn parse(v: Option<&str>) -> u8 {
    match v.map(str::trim) {
        Some("1") => 1,
        Some("2") => 2,
        _ => 0,
    }
}

/// The bake family a container names, if any.
pub(crate) fn kind_of(container: &[u8]) -> Option<Kind> {
    let names = |name: &[u8]| container.windows(name.len()).any(|w| w == name);
    if names(DECORATIONS) {
        Some(Kind::Decorations)
    } else if names(BLEND) {
        Some(Kind::Blend)
    } else {
        None
    }
}

/// The byte offsets from the records' descriptor to the parameters' to try, in order: a known
/// hash's own, else by family (`by_name`: the switch is on and the program is a pixel stage).
pub(crate) fn offsets_for(hash: &str, kind: Option<Kind>, by_name: bool) -> Option<[u32; 2]> {
    if let Some(o) = crate::bake_inputs::params_offset(hash) {
        return Some([o, o]);
    }
    match (by_name, kind?) {
        (false, _) => None,
        (true, Kind::Decorations) => Some([96, 32]),
        (true, Kind::Blend) => Some([32, 96]),
    }
}

/// Whether `hash` (a stage of `stage`, its container naming `kind`) is a bake the 7 hashes miss.
pub(crate) fn extra_for(mode: u8, stage: &str, hash: &str, kind: Option<Kind>) -> bool {
    if mode == 0 || crate::draw::BAKE_PIXEL_SHADERS.contains(&hash) {
        return false;
    }
    (stage == "pixel" && kind.is_some()) || (mode >= 2 && GEOMETRY.contains(&hash))
}

/// Whether stage `s` makes its pipeline a bake that the 7 hashes miss. Said once per program.
#[cfg(windows)]
pub(crate) fn extra(s: &crate::pipeline::Shader) -> bool {
    let mode = mode();
    if mode == 0 {
        return false;
    }
    let hit = extra_for(mode, s.stage, &s.hash, s.bake_kind());
    if hit {
        static SAID: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
        let mut said = SAID.lock().unwrap_or_else(|e| e.into_inner());
        if !said.contains(&s.hash) {
            said.push(s.hash.clone());
            tracing::info!(
                stage = s.stage,
                hash = %s.hash,
                name = %s.camera_name(),
                kind = ?s.bake_kind(),
                known = said.len(),
                "bake by name: a decoration bake program not among the 7 hashes (VICTORY_BAKE_BY_NAME)"
            );
        }
    }
    hit
}

/// The parameter offsets of bake stage `s` (None: not a bake pixel stage).
#[cfg(windows)]
pub(crate) fn offsets(s: &crate::pipeline::Shader) -> Option<[u32; 2]> {
    let by_name = mode() > 0 && s.stage == "pixel";
    offsets_for(&s.hash, if by_name { s.bake_kind() } else { None }, by_name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bake_programs_are_known_by_name() {
        assert_eq!(
            kind_of(b"..\0PS_DecorationBlend_0011_ff00f5de_Wrapped\0.."),
            Some(Kind::Blend)
        );
        assert_eq!(
            kind_of(b"PS_BakeDecorations_0100_583f45fe_Wrapped"),
            Some(Kind::Decorations)
        );
        assert_eq!(kind_of(b"PS_PedSkin_0001"), None);
    }

    /// Off, nothing changes: only the 7 hashes are bakes. On, a permutation the list misses is
    /// one, and with `2` the two geometry stages too.
    #[test]
    fn the_seven_hashes_miss_other_permutations() {
        let unlisted = "1234567890abcdef";
        assert!(!extra_for(0, "pixel", unlisted, Some(Kind::Blend)));
        assert!(extra_for(1, "pixel", unlisted, Some(Kind::Blend)));
        assert!(
            !extra_for(1, "vertex", unlisted, Some(Kind::Blend)),
            "a pixel stage only"
        );
        assert!(!extra_for(1, "pixel", unlisted, None));
        assert!(
            !extra_for(1, "pixel", "c0a7fe77b1862e10", Some(Kind::Decorations)),
            "listed"
        );
        assert!(!extra_for(1, "vertex", GEOMETRY[0], None));
        assert!(extra_for(2, "vertex", GEOMETRY[0], None));
        assert!(extra_for(2, "mesh", GEOMETRY[1], None));
    }

    #[test]
    fn parameters_are_found_by_family() {
        assert_eq!(offsets_for("c0a7fe77b1862e10", None, false), Some([96, 96]));
        assert_eq!(offsets_for("0d1575a396dfe513", None, false), Some([32, 32]));
        let unlisted = "1234567890abcdef";
        assert_eq!(offsets_for(unlisted, Some(Kind::Blend), false), None, "off");
        assert_eq!(
            offsets_for(unlisted, Some(Kind::Blend), true),
            Some([32, 96])
        );
        assert_eq!(
            offsets_for(unlisted, Some(Kind::Decorations), true),
            Some([96, 32])
        );
        assert_eq!(offsets_for(unlisted, None, true), None);
    }

    #[test]
    fn the_switch_reads_its_modes() {
        assert_eq!(parse(None), 0);
        assert_eq!(parse(Some("1")), 1);
        assert_eq!(parse(Some(" 2 ")), 2);
        assert_eq!(parse(Some("yes")), 0);
    }
}
