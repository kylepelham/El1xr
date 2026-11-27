//! Inline ray queries on a host top level (`VICTORY_NATIVE_BVH`):
//! `AllocateRayQuery`, `RayQuery_TraceRayInline`, `RayQuery_Proceed` and the
//! committed status and distance, on the `RTAccelerationStructure` bound as
//! root SRV [`SLOT_TLAS`] in [`ROOT_CBV_SPACE`] (ray tracing tier 1.1).
//!
//! Groundwork only: no lowering calls these yet. The title walks its own
//! console-format structures in its own compute programs and asks the
//! hardware only for one node test at a time (`image_bvh*_intersect_ray`),
//! so it cannot be handed a host top level without rewriting that walk.
//!
//! Source: Linox-IDK/LNX-victory 44280ad213 (GPL-3.0-or-later).
#![allow(dead_code)] // groundwork: no lowering emits ray queries yet

use super::bitcode::{Attr, Md, V};
use super::shader::{ROOT_CBV_SPACE, SLOT_TLAS, Shader, flags};

/// DXIL resource kind of a ray-tracing acceleration structure.
const KIND_RTAS: u32 = 16;
/// DXIL resource kind of a raw buffer.
const KIND_RAW: u32 = 11;

/// The metadata of root SRV `slot`: its variable (`raw` for a raw buffer),
/// shape, and extra properties (a ray-tracing structure's element type).
pub(super) fn srv_md(s: &mut Shader, slot: u8, raw: V) -> (V, u32, Option<Md>) {
    if slot != SLOT_TLAS {
        return (raw, KIND_RAW, None);
    }
    let i32t = s.t.i32;
    let ty =
        s.m.named_struct("struct.RaytracingAccelerationStructure", vec![i32t]);
    let p = s.m.ptr(ty);
    let var = s.m.undef(p);
    // Element type (tag 0): u32 (5).
    let tag = s.m.i32(0);
    let tag = s.m.md_value(tag);
    let u32k = s.m.i32(5);
    let u32k = s.m.md_value(u32k);
    let extra = s.m.md_node(vec![Some(tag), Some(u32k)]);
    (var, KIND_RTAS, Some(extra))
}

impl Shader {
    /// The handle of the host ray-tracing structure ([`SLOT_TLAS`]).
    pub fn tlas_handle(&mut self) -> V {
        if let Some(&(_, h)) = self.root_srvs.iter().find(|r| r.0 == SLOT_TLAS) {
            return h;
        }
        let r = self.m.i32(u32::from(SLOT_TLAS));
        let bind = [r, r, self.m.i32(ROOT_CBV_SPACE), self.m.i8(0)];
        let bind = self.m.aggregate(self.t.bind, &bind);
        let f = self.m.bool(false);
        let block = self.entry;
        let h = self.op(
            block,
            "createHandleFromBinding",
            Attr::ReadNone,
            self.t.handle,
            217,
            &[bind, r, f],
        );
        let h = self.annotate(block, h, KIND_RTAS, 0);
        self.flags |= flags::RAYTRACING_1_1;
        self.root_srvs.push((SLOT_TLAS, h));
        h
    }

    /// `AllocateRayQuery` with constant ray `flags`: a ray query's handle.
    pub fn ray_query(&mut self, block: usize, flags: u32) -> V {
        let f = self.m.i32(flags);
        let i32t = self.t.i32;
        self.op(block, "allocateRayQuery", Attr::NoUnwind, i32t, 178, &[f])
    }

    /// `RayQuery_TraceRayInline`: starts `rq` on the host structure.
    #[allow(clippy::too_many_arguments)]
    pub fn trace_ray_inline(
        &mut self,
        block: usize,
        rq: V,
        flags: V,
        mask: V,
        origin: [V; 3],
        tmin: V,
        dir: [V; 3],
        tmax: V,
    ) {
        let h = self.tlas_handle();
        let args = [
            rq, h, flags, mask, origin[0], origin[1], origin[2], tmin, dir[0], dir[1], dir[2], tmax,
        ];
        let void = self.t.void;
        self.op(
            block,
            "rayQuery_TraceRayInline",
            Attr::NoUnwind,
            void,
            179,
            &args,
        );
    }

    /// `RayQuery_Proceed`: whether a candidate needs the shader (never, for
    /// opaque geometry and the flags we pass).
    pub fn ray_query_proceed(&mut self, block: usize, rq: V) -> V {
        let i1 = self.t.i1;
        self.op(block, "rayQuery_Proceed.i1", Attr::NoUnwind, i1, 180, &[rq])
    }

    /// `RayQuery_CommittedStatus` (0 nothing, 1 triangle, 2 procedural).
    pub fn ray_query_committed_status(&mut self, block: usize, rq: V) -> V {
        let i32t = self.t.i32;
        self.op(
            block,
            "rayQuery_StateScalar.i32",
            Attr::ReadOnly,
            i32t,
            184,
            &[rq],
        )
    }

    /// `RayQuery_CommittedRayT`.
    pub fn ray_query_committed_t(&mut self, block: usize, rq: V) -> V {
        let f32t = self.t.f32;
        self.op(
            block,
            "rayQuery_StateScalar.f32",
            Attr::ReadOnly,
            f32t,
            200,
            &[rq],
        )
    }
}

#[cfg(test)]
mod tests {
    use super::super::bitcode::Term;
    use super::*;

    /// A compute shader that traces one ray against the host structure
    /// ([`SLOT_TLAS`]) with an inline ray query.
    fn ray_query_shader() -> Vec<u8> {
        let mut s = Shader::new(false);
        s.threads = Some([64, 1, 1]);
        let b = s.entry;
        // FORCE_OPAQUE | ACCEPT_FIRST_HIT_AND_END_SEARCH.
        let rq = s.ray_query(b, 1 | 4);
        let ray_flags = s.m.i32(0);
        let mask = s.m.i32(0xff);
        let zero = s.m.f32(0.0f32.to_bits());
        let one = s.m.f32(1.0f32.to_bits());
        let far = s.m.f32(1000.0f32.to_bits());
        s.trace_ray_inline(
            b,
            rq,
            ray_flags,
            mask,
            [zero, zero, zero],
            zero,
            [zero, zero, one],
            far,
        );
        assert_ne!(s.flags & flags::RAYTRACING_1_1, 0);
        let (lp, done) = (s.m.new_block(), s.m.new_block());
        s.m.terminate(b, Term::Br(lp));
        let more = s.ray_query_proceed(lp, rq);
        s.m.terminate(lp, Term::CondBr(more, lp, done));
        let _status = s.ray_query_committed_status(done, rq);
        let _t = s.ray_query_committed_t(done, rq);
        s.m.terminate(done, Term::Ret);
        s.finish()
    }

    /// `SFI0`'s feature bits: the first part after the container header
    /// (`DXBC`, hash, version, size, part count, offsets).
    fn features(container: &[u8]) -> u64 {
        let first = u32::from_le_bytes(container[32..36].try_into().unwrap()) as usize;
        assert_eq!(&container[first..first + 4], b"SFI0");
        u64::from_le_bytes(container[first + 8..first + 16].try_into().unwrap())
    }

    #[test]
    fn ray_query_shader_validates() {
        let mut dxil = ray_query_shader();
        assert_ne!(features(&dxil) & 0x10_0000, 0, "RaytracingTier1_1");
        #[cfg(windows)]
        if let Ok(mut v) = super::super::validator::Validator::load(
            super::super::validator::Validator::default_path().as_deref(),
        ) {
            v.validate(&mut dxil).unwrap();
        }
        assert!(!dxil.is_empty());
    }

    /// A shader with only raw-buffer root SRVs keeps their metadata and
    /// does not ask for ray tracing.
    #[test]
    fn raw_root_srvs_are_unchanged() {
        let mut s = Shader::new(false);
        s.threads = Some([64, 1, 1]);
        let b = s.entry;
        let _ = s.root_srv_handle(7);
        s.m.terminate(b, Term::Ret);
        assert_eq!(s.flags & flags::RAYTRACING_1_1, 0);
        let dxil = s.finish();
        assert_eq!(features(&dxil) & 0x10_0000, 0);
    }
}
