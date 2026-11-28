//! (phonescreen, #115) `VICTORY_ALIAS_COMPATIBLE=1` (restart; off by
//! default): a texture the title places at the start of a texture the host
//! writes (a render target or unordered-access texture), with the same top
//! level size and texels of the same width but another count of levels or
//! layers, is linked to it. [`super::alias_of`] links only a twin of the
//! same levels and layers (any format of one texel width: SRGB and UNORM,
//! typeless) and `VICTORY_PLACED_INSIDE` only a view whose every level
//! and layer lies on one of the target's, so such a view (a full mip chain
//! over a one-level target: the phone's video is drawn into a 736x1280
//! one-level target) was filled from title memory, which the host's draws
//! never reach: black. Linked, a draw that reads it gets each level and
//! layer the target has, copied on the host as an alias copy is
//! (`crate::texture::alias`); its levels and layers the target lacks stay
//! as they were filled.

use std::sync::{Arc, OnceLock};

use super::{ResourceDesc, element, resource, target_format};
use crate::object::This;

/// In [`parts`]: the view's subresource has none of the target's.
pub(crate) const NONE: u32 = u32::MAX;

/// The most levels times layers a link copies (as `crate::texture::alias`).
const MOST_PARTS: u32 = 54;

pub(crate) fn on() -> bool {
    #[cfg(test)]
    if let Some(on) = TEST_ON.get() {
        return on;
    }
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_ALIAS_COMPATIBLE").is_some_and(|v| v == "1"))
}

#[cfg(test)]
thread_local! {
    pub(crate) static TEST_ON: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

/// For each subresource of `view` (level + layer * levels) placed at the
/// start of `target`, the target's subresource of the same level and layer
/// ([`NONE`] past the target's levels or layers), when both are 2D colour
/// textures of one sample whose top levels are of one size and whose texels
/// are of one width. `None` otherwise.
pub(crate) fn parts(target: &ResourceDesc, view: &ResourceDesc) -> Option<Vec<u32>> {
    const DEPTH: u32 = 0x2;
    const TEXTURE_2D: u32 = 3;
    let (o, r) = (target, view);
    let (levels, layers) = (u32::from(r.mip_levels), u32::from(r.depth_or_array_size));
    let (o_levels, o_layers) = (u32::from(o.mip_levels), u32::from(o.depth_or_array_size));
    let fits = r.dimension == TEXTURE_2D
        && o.dimension == TEXTURE_2D
        && o.flags & DEPTH == 0
        && r.sample_count <= 1
        && o.sample_count <= 1
        && levels >= 1
        && layers >= 1
        && o_levels >= 1
        && o_layers >= 1
        && levels * layers <= MOST_PARTS
        && (r.width, r.height) == (o.width, o.height)
        && target_format(o.format) == o.format
        && element(o.format).is_some_and(|e| e.1 == 1 && Some(e) == element(r.format));
    if !fits {
        return None;
    }
    let mut parts = Vec::with_capacity((levels * layers) as usize);
    for layer in 0..layers {
        for level in 0..levels {
            parts.push(if level < o_levels && layer < o_layers {
                level + layer * o_levels
            } else {
                NONE
            });
        }
    }
    Some(parts)
}

/// [`super::alias_of`]'s answer from the table of targets, held locked, for
/// a placed texture no target twins: the target starting where it does,
/// if [`parts`] links them.
pub(super) fn linked(
    targets: &std::collections::BTreeMap<u64, usize>,
    r: &super::Resource,
) -> Option<This> {
    if !on() {
        return None;
    }
    let other = *targets.get(&(r.data() as u64))? as This;
    parts(&resource(other).desc, &r.desc).map(|_| other)
}

/// For `crate::texture::alias`: [`parts`] of `placed` at the start of
/// `source`, unless they are every subresource as it is (a twin, copied
/// whole).
pub(crate) fn copy_parts(placed: This, source: This) -> Option<Arc<[u32]>> {
    if !on() || placed.is_null() || source.is_null() {
        return None;
    }
    let (r, o) = (resource(placed), resource(source));
    if r.data() != o.data() {
        return None;
    }
    let parts = parts(&o.desc, &r.desc)?;
    let all = u32::from(o.desc.mip_levels) * u32::from(o.desc.depth_or_array_size);
    let twin =
        parts.len() == all as usize && parts.iter().enumerate().all(|(n, &p)| p as usize == n);
    (!twin).then(|| Arc::from(parts))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn desc(
        width: u64,
        height: u32,
        levels: u16,
        layers: u16,
        format: u32,
        flags: u32,
    ) -> ResourceDesc {
        ResourceDesc {
            dimension: 3,
            alignment: 0,
            width,
            height,
            depth_or_array_size: layers,
            mip_levels: levels,
            format,
            sample_count: 1,
            sample_quality: 0,
            layout: 0,
            flags,
        }
    }

    /// The phone's case: a mip chain in UNORM over a one-level SRGB target
    /// gets the target's level 0 for its own, and nothing for the rest.
    #[test]
    fn a_mip_chain_over_a_one_level_target_takes_its_top_level() {
        let target = desc(736, 1280, 1, 1, 29, 1);
        let view = desc(736, 1280, 11, 1, 28, 0);
        let mut want = vec![NONE; 11];
        want[0] = 0;
        assert_eq!(parts(&target, &view), Some(want));
        // Fewer levels than the target: each the target's own.
        let chain = desc(736, 1280, 3, 1, 29, 1);
        let top = desc(736, 1280, 1, 1, 27, 0);
        assert_eq!(parts(&chain, &top), Some(vec![0]));
        // Layers: level 1 of layer 1 is the target's 1 + 1 * 3 = 4.
        let layered = desc(64, 64, 3, 2, 28, 4);
        let view = desc(64, 64, 2, 2, 28, 0);
        assert_eq!(parts(&layered, &view), Some(vec![0, 1, 3, 4]));
    }

    /// Another size, another texel width, depth, or a block format: no link.
    #[test]
    fn what_is_not_compatible_is_refused() {
        let target = desc(736, 1280, 1, 1, 29, 1);
        assert_eq!(parts(&target, &desc(768, 1280, 1, 1, 29, 0)), None);
        assert_eq!(parts(&target, &desc(736, 1280, 1, 1, 61, 0)), None);
        assert_eq!(parts(&target, &desc(736, 1280, 1, 1, 71, 0)), None);
        assert_eq!(
            parts(&desc(736, 1280, 1, 1, 40, 2), &desc(736, 1280, 1, 1, 41, 0)),
            None
        );
        let mut volume = desc(64, 64, 1, 1, 28, 1);
        volume.dimension = 4;
        assert_eq!(parts(&volume, &desc(64, 64, 1, 1, 28, 0)), None);
    }
}
