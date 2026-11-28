//! `VICTORY_NATIVE_BVH_REFIT=1` (off by default, with `VICTORY_NATIVE_BVH=1`):
//! when the title asks for an update of a structure (its build flags carry
//! `PERFORM_UPDATE`: skinned or moving meshes, top levels whose instances
//! moved), the host structure is updated (refit) from its source instead of
//! being built again from scratch. A refit keeps the topology and only moves
//! the boxes, several times cheaper than a build on the card.
//!
//! DXR allows an update only from a structure built with `ALLOW_UPDATE`, with
//! the same flags (besides the two update flags) and the same shape: for a
//! bottom level the same geometries, counts, formats, index buffer and
//! transform presence (vertices and transforms may move); for a top level the
//! same instance count (instances may change completely). Anything else is
//! built as before. Without the switch an update request is a full build, as
//! the port did, but it is counted (`updates_asked` in the summary line).

use std::sync::OnceLock;

use super::Triangles;

/// `D3D12_RAYTRACING_ACCELERATION_STRUCTURE_BUILD_FLAG_ALLOW_UPDATE`.
pub(super) const ALLOW_UPDATE: u32 = 0x1;
/// `D3D12_RAYTRACING_ACCELERATION_STRUCTURE_BUILD_FLAG_PERFORM_UPDATE`.
pub(super) const PERFORM_UPDATE: u32 = 0x20;
/// The build flags DXR knows besides the update ones (fast trace, fast
/// build, minimize memory, ...): an update must carry the source's.
const OTHER_FLAGS: u32 = 0x1f & !ALLOW_UPDATE;

/// Whether update requests are refit on the host (`VICTORY_NATIVE_BVH_REFIT=1`).
pub fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_NATIVE_BVH_REFIT").is_some_and(|v| v == "1"))
}

/// Whether the title asked for an update rather than a build.
pub(super) fn asked(title_flags: u32) -> bool {
    title_flags & PERFORM_UPDATE != 0
}

/// What must not change between a structure and an update of it, as a hash:
/// the flags besides the update ones, and for a bottom level each geometry's
/// opacity, transform presence, formats, counts, stride and index buffer.
/// Vertex buffer and transform addresses are left out: DXR lets them move.
pub(super) fn bottom_shape(title_flags: u32, geometries: &[Triangles]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (0u8, title_flags & OTHER_FLAGS).hash(&mut h);
    for g in geometries {
        (
            g.opaque,
            g.transform != 0,
            g.index_format,
            g.vertex_format,
            g.index_count,
            g.vertex_count,
            g.index_buffer,
            g.stride,
        )
            .hash(&mut h);
    }
    h.finish() | 1
}

/// [`bottom_shape`] for a top level: the flags besides the update ones and
/// the instance count.
pub(super) fn top_shape(title_flags: u32, count: u32) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (1u8, title_flags & OTHER_FLAGS, count).hash(&mut h);
    h.finish() | 1
}

/// The host flags of a build: the title's DXR flags (`PERFORM_UPDATE` only
/// for a refit). With the switch on, a structure the title updates is built
/// with `ALLOW_UPDATE` even when the request named only `PERFORM_UPDATE`, so
/// that the next update can be a refit.
pub(super) fn host_flags(title_flags: u32, refit: bool) -> u32 {
    let base = title_flags & 0x1f;
    if refit {
        base | ALLOW_UPDATE | PERFORM_UPDATE
    } else if on() && asked(title_flags) {
        base | ALLOW_UPDATE
    } else {
        base
    }
}

/// What the host holds for the title's update source, as [`plan`] needs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Source {
    /// Built (or last refit) with `ALLOW_UPDATE`.
    pub updatable: bool,
    /// Its [`bottom_shape`] or [`top_shape`].
    pub shape: u64,
    /// Its own result buffer's size (0 for an alias of another's buffer).
    pub size: u64,
    /// Its top level was recorded (a bottom level: true once built).
    pub built: bool,
}

/// Why an update request is built from scratch instead (summary counts).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Refused {
    /// The host holds no structure for the title's source.
    NoSource,
    /// The source was built without `ALLOW_UPDATE` or never recorded.
    NotUpdatable,
    /// Counts, formats, flags or instance count changed.
    Shape,
    /// In place on a buffer shared with another title address (a clone or
    /// compaction alias): updating it would change that one too.
    Alias,
}

/// Whether an update the title asked for (`shape` now) can be a host refit
/// from `source`, done in place (`in_place`: the title's destination is its
/// source).
pub(super) fn plan(source: Option<Source>, shape: u64, in_place: bool) -> Result<(), Refused> {
    let s = source.ok_or(Refused::NoSource)?;
    if !s.updatable || !s.built {
        return Err(Refused::NotUpdatable);
    }
    if s.shape != shape {
        return Err(Refused::Shape);
    }
    if in_place && s.size == 0 {
        return Err(Refused::Alias);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tri(vb: u64, transform: u64) -> Triangles {
        Triangles {
            opaque: true,
            transform,
            index_format: 57,
            vertex_format: 6,
            index_count: 3,
            vertex_count: 3,
            index_buffer: 0x100,
            vertex_buffer: vb,
            stride: 12,
        }
    }

    fn source(shape: u64) -> Option<Source> {
        Some(Source {
            updatable: true,
            shape,
            size: 4096,
            built: true,
        })
    }

    #[test]
    fn off_by_default() {
        assert!(std::env::var_os("VICTORY_NATIVE_BVH_REFIT").is_none());
        assert!(!on());
        // Off: an update request is built from scratch with the title's own
        // flags, as before the switch.
        assert_eq!(host_flags(0x20 | 0x4, false), 0x4);
        assert_eq!(host_flags(0x21, false), 0x1);
    }

    #[test]
    fn update_requests_are_seen() {
        assert!(asked(0x20));
        assert!(asked(0x21));
        assert!(!asked(0x1));
        assert!(!asked(0x4));
    }

    #[test]
    fn a_refit_keeps_updates_allowed() {
        // DXR: an update of an updatable structure, keeping its other flags.
        assert_eq!(
            host_flags(0x20 | 0x4, true),
            0x4 | ALLOW_UPDATE | PERFORM_UPDATE
        );
        assert_eq!(host_flags(0x21, true), ALLOW_UPDATE | PERFORM_UPDATE);
    }

    #[test]
    fn moving_vertices_and_transforms_keep_the_shape() {
        let a = bottom_shape(0x1, &[tri(0x200, 0x900)]);
        // Skinning writes another vertex buffer, the transform moves: same shape.
        assert_eq!(a, bottom_shape(0x1, &[tri(0x300, 0xa00)]));
        // The update flags do not count; the others do.
        assert_eq!(a, bottom_shape(0x21, &[tri(0x200, 0x900)]));
        assert_eq!(a, bottom_shape(0x20, &[tri(0x200, 0x900)]));
        assert_ne!(a, bottom_shape(0x1 | 0x4, &[tri(0x200, 0x900)]));
        // A transform appearing or going: another shape (DXR forbids it).
        assert_ne!(a, bottom_shape(0x1, &[tri(0x200, 0)]));
        // Counts, index buffer, geometry count: another shape.
        let more = Triangles {
            index_count: 6,
            ..tri(0x200, 0x900)
        };
        assert_ne!(a, bottom_shape(0x1, &[more]));
        let other_ib = Triangles {
            index_buffer: 0x180,
            ..tri(0x200, 0x900)
        };
        assert_ne!(a, bottom_shape(0x1, &[other_ib]));
        assert_ne!(
            a,
            bottom_shape(0x1, &[tri(0x200, 0x900), tri(0x200, 0x900)])
        );
    }

    #[test]
    fn top_levels_refit_only_with_the_same_count() {
        assert_eq!(top_shape(0x1, 3072), top_shape(0x21, 3072));
        assert_ne!(top_shape(0x1, 3072), top_shape(0x1, 3073));
        // Never the same as a bottom level with no geometry.
        assert_ne!(top_shape(0x1, 0), bottom_shape(0x1, &[]));
    }

    #[test]
    fn plans_follow_the_dxr_rules() {
        let shape = bottom_shape(0x1, &[tri(0x200, 0)]);
        assert_eq!(plan(source(shape), shape, true), Ok(()));
        assert_eq!(plan(source(shape), shape, false), Ok(()));
        assert_eq!(plan(None, shape, true), Err(Refused::NoSource));
        assert_eq!(plan(source(shape ^ 2), shape, true), Err(Refused::Shape));
        let not_updatable = Source {
            updatable: false,
            ..source(shape).unwrap()
        };
        assert_eq!(
            plan(Some(not_updatable), shape, true),
            Err(Refused::NotUpdatable)
        );
        let unbuilt = Source {
            built: false,
            ..source(shape).unwrap()
        };
        assert_eq!(plan(Some(unbuilt), shape, true), Err(Refused::NotUpdatable));
        // An alias (a clone's view of another buffer): read from, never updated in place.
        let alias = Source {
            size: 0,
            ..source(shape).unwrap()
        };
        assert_eq!(plan(Some(alias), shape, true), Err(Refused::Alias));
        assert_eq!(plan(Some(alias), shape, false), Ok(()));
    }
}
