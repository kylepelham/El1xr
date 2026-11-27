//! Three more ways the stand-in for the indirect light (`crate::ambient`)
//! keeps reflections to the light around them while the title's rays find
//! nothing. Each is off by default and needs `VICTORY_AMBIENT` set; the
//! passes are in `crate::ambient`'s source, the places they act on here.
//!
//! - `VICTORY_AMBIENT_SPECULAR_BOUND=1` (trustno1-coder/VIctory-NVIDIA#124,
//!   1mosce): a dispatch of `CS_RayMiss` whose tables hold the rays' image but
//!   no depth of the picture (`crate::draw` `miss_views` found none, the
//!   frame's table included) is followed all the same: no point, no normal, so
//!   no reflected direction; each texel takes the light the points around the
//!   surface seen there hold. Needs `VICTORY_AMBIENT_SPECULAR`.
//! - `VICTORY_AMBIENT_REFLECT=<multiple>` (TaynanGT/Vice-Tay fc9ef8a, fa90adf,
//!   from Serph91P's avatarmanz/VIctory-7900XTX#314; the idea and the name are
//!   Belobug's, VIctory-Belobug-AMD 44dcd437): right after the reflections'
//!   temporal resolves (`VICTORY_AMBIENT_REFLECT_IN=<text>[,<text>]`, names in
//!   the programs; unset: [`REFLECT_IN`]) each texel of the image they write
//!   (R9G9B9E5 in R32_UINT at [`REFLECTIONS`]) is held to no more than that
//!   multiple of the light around it, hue kept.
//! - `VICTORY_REFLECT_KNEE=<multiple>` (JozefDeTremmerie/CCS03 647cf15,
//!   37e5ef3, e7f4bb6): right after `CS_UpdateHistory`, its reflection history
//!   ([`KNEE_IMAGE`]) goes through a soft knee: as it is up to half the
//!   ceiling, bent towards the ceiling above that, hue kept. The ceiling is
//!   that multiple of the light around each texel (CCS03: of one level for the
//!   whole picture).
//!
//! The holds follow direct dispatches of some groups and indirect ones
//! without a count buffer: after a resolve that ran no group, or a count the
//! card reads that may be 0, the image holds what it held before, and the
//! knee would bend it again.

use std::sync::OnceLock;

/// The reflections' temporal resolves when `VICTORY_AMBIENT_REFLECT_IN` is
/// unset (Vice-Tay's documented names; the water's is left alone).
pub(crate) const REFLECT_IN: &str = "CS_TemporalResolve2_Opaque,CS_TemporalResolve1_Alpha";

/// Where those resolves write: the first of these places that holds a 2D
/// image of R32_UINT (the opaque resolve's at the first, the alpha one's at
/// the second, Vice-Tay).
pub(crate) const REFLECTIONS: [(u8, u32); 2] = [(1, 128), (1, 136)];

/// The history pass the knee follows, and where it writes the reflections.
pub(crate) const KNEE_PROGRAM: &[u8] = b"CS_UpdateHistory_";
pub(crate) const KNEE_IMAGE: (u8, u32) = (1, 40);

/// R32_UINT, holding R9G9B9E5.
pub(crate) const FORMAT: u32 = 42;

fn positive(set: &str) -> Option<f32> {
    set.trim()
        .parse()
        .ok()
        .filter(|v: &f32| v.is_finite() && *v > 0.0)
}

fn names(set: &str) -> Vec<String> {
    set.split(',')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_owned)
        .collect()
}

/// `VICTORY_AMBIENT_SPECULAR_BOUND=1`.
pub(crate) fn bound() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var("VICTORY_AMBIENT_SPECULAR_BOUND").is_ok_and(|v| v.trim() == "1")
    })
}

/// `VICTORY_AMBIENT_REFLECT=<multiple>`; `None`: off.
pub(crate) fn reflect() -> Option<f32> {
    static SET: OnceLock<Option<f32>> = OnceLock::new();
    *SET.get_or_init(|| positive(&std::env::var("VICTORY_AMBIENT_REFLECT").unwrap_or_default()))
}

/// The texts naming the reflections' resolves; none while the hold is off.
pub(crate) fn reflect_in() -> &'static [String] {
    static SET: OnceLock<Vec<String>> = OnceLock::new();
    SET.get_or_init(|| {
        if reflect().is_none() {
            return Vec::new();
        }
        let set = std::env::var("VICTORY_AMBIENT_REFLECT_IN").unwrap_or_default();
        let given = names(&set);
        if given.is_empty() {
            names(REFLECT_IN)
        } else {
            given
        }
    })
}

/// `VICTORY_REFLECT_KNEE=<multiple>`; `None`: off (CCS03's default was on, at 1).
pub(crate) fn knee() -> Option<f32> {
    static SET: OnceLock<Option<f32>> = OnceLock::new();
    *SET.get_or_init(|| positive(&std::env::var("VICTORY_REFLECT_KNEE").unwrap_or_default()))
}

/// Whether `container` is one of the reflections' resolves of `reflect_in`.
pub(crate) fn resolves(container: &[u8], reflect_in: &[String]) -> bool {
    let has = |name: &[u8]| !name.is_empty() && container.windows(name.len()).any(|w| w == name);
    reflect_in.iter().any(|name| has(name.as_bytes()))
}

/// Whether `container` is the knee's history pass (`CS_UpdateHistory_`, not
/// `CS_BilateralBlurAndUpdateHistory_`).
pub(crate) fn keeps_history(container: &[u8]) -> bool {
    container
        .windows(KNEE_PROGRAM.len())
        .any(|w| w == KNEE_PROGRAM)
}

/// Whether a hold follows a dispatch of `groups` (direct) or an indirect one
/// with a count buffer or not (`counted`).
pub(crate) fn follows(groups: Option<[u32; 3]>, counted: bool) -> bool {
    match groups {
        Some(g) => g.iter().all(|&n| n > 0),
        None => !counted,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_parse() {
        assert_eq!(positive(""), None);
        assert_eq!(positive("off"), None);
        assert_eq!(positive("0"), None);
        assert_eq!(positive("-1"), None);
        assert_eq!(positive("nan"), None);
        assert_eq!(positive(" 1.5 "), Some(1.5));
        assert!(names(" , ,").is_empty());
        assert_eq!(names("Ab, Cd ,"), ["Ab", "Cd"]);
        assert_eq!(names(REFLECT_IN).len(), 2);
    }

    #[test]
    fn programs_by_name() {
        let given = names(REFLECT_IN);
        assert!(resolves(
            b"..CS_TemporalResolve2_Opaque_FrameStatistics_357e0818_Wrapped..",
            &given
        ));
        assert!(resolves(
            b"..CS_TemporalResolve1_Alpha_68979fba_Wrapped..",
            &given
        ));
        assert!(!resolves(
            b"..CS_TemporalResolve2_Water_357e0818_Wrapped..",
            &given
        ));
        assert!(!resolves(b"..CS_TemporalResolve1_Alpha_68979fba..", &[]));
        assert!(keeps_history(b"..CS_UpdateHistory_e877ef6e_Wrapped.."));
        assert!(!keeps_history(
            b"..CS_BilateralBlurAndUpdateHistory_e877ef6e_Wrapped.."
        ));
    }

    #[test]
    fn holds_follow_dispatches_that_ran() {
        assert!(follows(Some([4, 2, 1]), false));
        assert!(!follows(Some([0, 2, 1]), false));
        assert!(!follows(Some([4, 2, 0]), false));
        assert!(follows(None, false));
        assert!(!follows(None, true));
    }
}
