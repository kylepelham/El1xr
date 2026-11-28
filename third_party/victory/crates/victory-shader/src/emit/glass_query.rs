//! The glass splice's ray query (`VICTORY_NATIVE_GLASS`, [`super::super::glass_splice`]):
//! an `image_bvh64_intersect_ray` whose descriptor operand is a trap
//! temporary is not a node test but a marker the splice put in the console
//! program. It traces the instruction's ray (origin, direction, extent as
//! the maximum distance) with an inline ray query on the host top level at
//! root SRV [`SLOT_TLAS`] or [`SLOT_TLAS_NEAR`], opaque geometry only, and
//! writes four dwords of the committed hit:
//!
//! - the hit part: 1 for a triangle hit (else 0), the distance, barycentric
//!   x and y (DXR's: the weights of the triangle's second and third vertex);
//! - the indices part: the instance's index in the top level (the title's
//!   walk returns the instance node's index), the primitive index, the
//!   geometry index, the instance's contribution to the hit group index.
//!
//! A miss writes what the title's walk leaves for one: 0, -1.0 and zeros for
//! the hit part, -1 for each index. A null top level (none matched for the
//! dispatch) misses.

use super::super::glass_splice::Marker;
use super::super::ray_query::Committed;
use super::super::shader::{SLOT_TLAS, SLOT_TLAS_NEAR};
use super::{Inst, Lower, Reg, V, pred};

/// `RAY_FLAG_CULL_NON_OPAQUE`: alpha-tested geometry is skipped (it needs
/// the title's own test per candidate, not yet spliced), so no candidate
/// ever comes back to the shader and one `Proceed` ends the walk.
pub(in crate::emit) const CULL_NON_OPAQUE: u32 = 0x80;
/// The instance mask: every instance.
const MASK: u32 = 0xff;
/// The distance a miss reports (the title's own miss value in `v36`).
const MISS_T: f32 = -1.0;

impl<'a> Lower<'a> {
    /// The marker `marker` at instruction `inst`: the ray query, its four
    /// result dwords written from VGPR `dst` on, under `guard`.
    pub(super) fn glass_query(
        &mut self,
        inst: &Inst,
        dst: u8,
        guard: Option<V>,
        marker: Marker,
        origin: [V; 3],
        direction: [V; 3],
        extent: V,
    ) {
        let slot = if marker.near {
            SLOT_TLAS_NEAR
        } else {
            SLOT_TLAS
        };
        let zero = self.s.m.i32(0);
        let gate = self.begin_guarded(guard);
        let c = self.cur;
        let rq = self.s.ray_query(c, CULL_NON_OPAQUE);
        let (flags, mask) = (self.s.m.i32(0), self.s.m.i32(MASK));
        let tmin = self.s.m.f32(0);
        self.s
            .trace_ray_inline(c, rq, slot, flags, mask, origin, tmin, direction, extent);
        let _ = self.s.ray_query_proceed(c, rq);
        let status = self.s.ray_query_committed_status(c, rq);
        let one = self.s.m.i32(1);
        let hit = self.s.m.cmp(c, pred::EQ, status, one);
        let none = self.s.m.i32(u32::MAX);
        let words: [V; 4] = if marker.indices {
            [
                Committed::InstanceIndex,
                Committed::PrimitiveIndex,
                Committed::GeometryIndex,
                Committed::Contribution,
            ]
            .map(|what| {
                let v = self.s.ray_query_committed(c, rq, what);
                self.s.m.select(c, hit, v, none)
            })
        } else {
            let t = self.s.ray_query_committed_t(c, rq);
            let miss_t = self.s.m.f32(MISS_T.to_bits());
            let t = self.s.m.select(c, hit, t, miss_t);
            let fzero = self.s.m.f32(0);
            let b0 = self.s.ray_query_committed_barycentric(c, rq, 0);
            let b1 = self.s.ray_query_committed_barycentric(c, rq, 1);
            let b0 = self.s.m.select(c, hit, b0, fzero);
            let b1 = self.s.m.select(c, hit, b1, fzero);
            [
                self.s.m.select(c, hit, one, zero),
                self.as_int(t),
                self.as_int(b0),
                self.as_int(b1),
            ]
        };
        let inside: Vec<(V, V)> = words.iter().map(|&w| (w, zero)).collect();
        let out = self.end_guarded(gate, &inside);
        for (k, &r) in out.iter().enumerate() {
            self.write(inst, Reg::V(dst + k as u8), r, guard);
        }
    }
}
