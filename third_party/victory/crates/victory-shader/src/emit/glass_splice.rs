//! `VICTORY_NATIVE_GLASS=1` (off by default; needs `VICTORY_NATIVE_BVH=1`):
//! the glass trace `6d7fe7745ab9c6d8`'s near walk on host top levels, by
//! inline ray queries on root SRVs [`super::SLOT_TLAS`] (its `t355`) and
//! [`super::SLOT_TLAS_NEAR`] (its `t354`), which the host binds per dispatch
//! (`victory-gpu`'s `rt_bind`). The switch is part of the shader cache key.
//!
//! The splice is made in the console program's code before translation
//! (`glass_patch`, from `host_form`), and two forms carry it into the
//! translator:
//!
//! - a **marker**: `image_bvh64_intersect_ray` with a trap temporary as its
//!   descriptor (`ttmp0`, `ttmp4`, `ttmp8`, `ttmp12`; no title program names
//!   trap temporaries, which only a trap handler sets). It is lowered to a
//!   ray query on the instruction's ray (`lower::glass_query`), not a node
//!   test ([`Marker`]);
//! - the **selector**: `ttmp15` read before any write is the host's word
//!   [`super::USER_GLASS_NATIVE`] (1 when the dispatch has both host top
//!   levels, else 0), so the spliced program keeps the title's own walk as
//!   the fallback.

use crate::gfx10::Operand;

/// The glass trace's container hash, as the log names shaders.
pub const GLASS_TRACE: &str = "6d7fe7745ab9c6d8";

/// Whether the glass trace's walk goes to host top levels
/// (`VICTORY_NATIVE_GLASS=1` with `VICTORY_NATIVE_BVH=1`).
pub fn native_glass() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        let on = |name: &str| std::env::var_os(name).is_some_and(|v| v == "1");
        on("VICTORY_NATIVE_GLASS") && on("VICTORY_NATIVE_BVH")
    })
}

/// `ttmp0`: the first trap temporary's scalar code.
const TTMP0: u8 = 108;
/// `ttmp15`, read on entry: the host's [`super::USER_GLASS_NATIVE`] word.
pub(crate) const SELECTOR: u8 = TTMP0 + 15;

/// What a marker asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Marker {
    /// The second top level ([`super::SLOT_TLAS_NEAR`], the title's `t354`)
    /// rather than the first ([`super::SLOT_TLAS`], `t355`).
    pub near: bool,
    /// The committed hit's indices (instance index, primitive, geometry,
    /// contribution) rather than the hit, distance and barycentrics.
    pub indices: bool,
}

/// The marker a node test's descriptor operand `rsrc` makes, if it is one:
/// `ttmp[0:3]` the first top level's hit, `ttmp[4:7]` its indices,
/// `ttmp[8:11]` and `ttmp[12:15]` the same on the second.
pub(crate) fn marker(rsrc: &Operand) -> Option<Marker> {
    let Operand::Scalar { code, dwords: 4 } = *rsrc else {
        return None;
    };
    let k = code.checked_sub(TTMP0)?;
    (k < 16 && k % 4 == 0).then_some(Marker {
        near: k >= 8,
        indices: k % 8 == 4,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn off_by_default() {
        assert!(!native_glass());
    }

    #[test]
    fn trap_temporaries_name_the_four_markers() {
        let at = |code| marker(&Operand::Scalar { code, dwords: 4 });
        assert_eq!(at(108), Some(Marker { near: false, indices: false }));
        assert_eq!(at(112), Some(Marker { near: false, indices: true }));
        assert_eq!(at(116), Some(Marker { near: true, indices: false }));
        assert_eq!(at(120), Some(Marker { near: true, indices: true }));
        // The title's descriptors: ordinary SGPRs, never a marker.
        assert_eq!(at(8), None);
        assert_eq!(at(104), None);
        assert_eq!(at(110), None);
        assert_eq!(marker(&Operand::Scalar { code: 108, dwords: 8 }), None);
        assert_eq!(SELECTOR, 123);
    }
}
