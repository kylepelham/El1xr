//! Programs whose every group barrier waits for the whole group (`VICTORY_GROUP_SYNC_FULL`),
//! whatever [`super::group_sync`] gives the others.
//!
//! On a host of 32-lane waves the default (`auto`) keeps the memory fence of a divergent barrier
//! in a group of one console wave but drops its wait: a thread that left a loop early passes the
//! barrier, and the other host wave reads group-shared memory before it is written. vilocity saw
//! it on GeForce in the face programs (subsurface tiles `CS_SSS*`, blend-shape normals,
//! `StickyLips`, `EyeCap`): faces in the prologue cutscenes black but for the eyes, lit with
//! every barrier waiting (2 of 2 runs, 6 cutscene frames each). In ped skinning and meshlet
//! culling (`Skinning`, `BatchCull`): a dark jagged hole on the jumpsuit shoulders, 4 of 8 world
//! frames, 0 of 6 waiting. In the hair tiles (`CS_HairTile`): a dark band across a bald head and
//! red instead of orange jumpsuits, gone waiting.
//!
//! `VICTORY_GROUP_SYNC_FULL`: unset, empty, `0` or `none`: no program; `1`: the programs whose
//! entry point's name holds one of [`NAMES`]; otherwise a comma list of name parts to use
//! instead (to bisect). `VICTORY_GROUP_SYNC=0` still wins (no barrier waits). Part of the
//! translation: the shader cache keys on it.
//!
//! Ported from voidopcode/vilocity `cb8bcc58e7` (with `d1e08f5e28` and `0871f6208a`),
//! `sync_full` in its `emit/mod.rs`, GPL-3.0. Its shadow entries (`CS_Shadow`,
//! `RasterClassification`) are left out here: our tiled shadow programs take the paired form's
//! waiting barriers under `VICTORY_WAVE_PAIRS_VT`.

use std::cell::Cell;
use std::sync::OnceLock;

/// The name parts `VICTORY_GROUP_SYNC_FULL=1` selects.
pub(super) const NAMES: [&str; 7] = [
    "CS_SSS",
    "Blendshape",
    "StickyLips",
    "EyeCap",
    "Skinning",
    "BatchCull",
    "CS_HairTile",
];

/// The switch's name parts; empty when off.
fn parse(value: Option<&str>) -> Vec<String> {
    match value.map(str::trim) {
        None | Some("" | "0") => Vec::new(),
        Some(v) if v.eq_ignore_ascii_case("none") => Vec::new(),
        Some("1") => NAMES.iter().map(|&n| n.to_owned()).collect(),
        Some(v) => v
            .split(',')
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .map(str::to_owned)
            .collect(),
    }
}

fn names() -> &'static [String] {
    static NAMES_ON: OnceLock<Vec<String>> = OnceLock::new();
    NAMES_ON.get_or_init(|| parse(std::env::var("VICTORY_GROUP_SYNC_FULL").ok().as_deref()))
}

/// The entry point's name in `container`: the identifier that ends in `_Wrapped` (after the
/// library's `name:`), as `CS_SSSTileClassification_49460c1d`.
fn entry_name(container: &[u8]) -> Option<&[u8]> {
    const END: &[u8] = b"_Wrapped";
    let at = container.windows(END.len()).position(|w| w == END)?;
    let before = &container[at.saturating_sub(100)..at];
    before
        .rsplit(|b| !b.is_ascii_alphanumeric() && *b != b'_')
        .next()
        .filter(|n| !n.is_empty())
}

/// Whether the program in `container` waits at every barrier, given the switch's `names`.
fn listed(container: &[u8], names: &[String]) -> bool {
    if names.is_empty() {
        return false;
    }
    let Some(name) = entry_name(container) else {
        return false;
    };
    names
        .iter()
        .any(|n| name.windows(n.len()).any(|w| w == n.as_bytes()))
}

thread_local! {
    /// The program being translated on this thread waits at every barrier ([`with_container`]).
    static HERE: Cell<bool> = const { Cell::new(false) };
}

/// Runs the translation `f` of the program in `c` with [`here`] set when the switch names it
/// (the name is in one of the container's parts).
pub(super) fn with_container<R>(c: &crate::dxbc::Container, f: impl FnOnce() -> R) -> R {
    let names = names();
    if names.is_empty() || !c.parts.iter().any(|part| listed(part.data, names)) {
        return f();
    }
    with_here(f)
}

/// Runs `f` with [`here`] set, restored after even when `f` panics.
fn with_here<R>(f: impl FnOnce() -> R) -> R {
    struct Restore(bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            HERE.set(self.0);
        }
    }
    let _restore = Restore(HERE.replace(true));
    f()
}

/// Whether the program being translated waits at every barrier.
pub(super) fn here() -> bool {
    HERE.get()
}

#[cfg(test)]
pub(super) fn with_here_for_test<R>(f: impl FnOnce() -> R) -> R {
    with_here(f)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_parse_to_name_parts() {
        assert!(parse(None).is_empty());
        assert!(parse(Some("0")).is_empty());
        assert!(parse(Some(" none ")).is_empty());
        assert!(parse(Some("")).is_empty());
        assert_eq!(parse(Some("1")), NAMES.map(str::to_owned));
        assert_eq!(
            parse(Some("CS_SSS, EyeCap")),
            ["CS_SSS", "EyeCap"].map(str::to_owned)
        );
    }

    #[test]
    fn the_face_skin_and_hair_programs_wait_at_every_barrier() {
        let on = parse(Some("1"));
        for yes in [
            &b"\0subsurfacescatter:CS_SSSTileClassification_49460c1d_Wrapped\0"[..],
            b"x CS_ReconstructBlendshapeNormals_0a1b2c3d_Wrapped",
            b"x CS_StickyLips_0a1b2c3d_Wrapped",
            b"x CS_EyeCapUpdate_0a1b2c3d_Wrapped",
            b"x CS_PedStandardSkinning_3_0a1b2c3d_Wrapped",
            b"x CSComputeSkinning_0a1b2c3d_Wrapped",
            b"x CS_BatchCullPreSkinnedMeshlets_Prev_b589f3cd_Wrapped",
            b"x CS_HairTileClassify_0a1b2c3d_Wrapped",
        ] {
            assert!(listed(yes, &on), "{}", String::from_utf8_lossy(yes));
        }
        for no in [
            &b"x CS_ShadowRenderTiledCascades_9945271c_Wrapped"[..],
            b"x CS_GenerateShadowMasks_Checkerboard_NoHair_5e982407_Wrapped",
            b"\0CS_FinalGather_0_1_1_0_a1d0fa7d_Wrapped\0",
            b"CS_SSS without the end",
        ] {
            assert!(!listed(no, &on), "{}", String::from_utf8_lossy(no));
        }
        assert!(!listed(b"x CS_SSSTileClassification_0_Wrapped", &[]));
        let bytes = crate::dxbc::build(&[(
            crate::dxbc::FourCC(*b"RDAT"),
            b"\0lib:CS_EyeCapUpdate_0a1b2c3d_Wrapped\0",
        )]);
        let c = crate::dxbc::Container::parse(&bytes).unwrap();
        let was = HERE.get();
        assert!(c.parts.iter().any(|part| listed(part.data, &on)));
        assert!(!with_container(&c, here) || !names().is_empty());
        assert_eq!(HERE.get(), was);
        let only = parse(Some("EyeCap"));
        assert!(listed(b"x CS_EyeCapUpdate_0_Wrapped", &only));
        assert!(!listed(b"x CS_SSSTileClassification_0_Wrapped", &only));
    }
}
