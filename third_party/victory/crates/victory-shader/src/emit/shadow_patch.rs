//! The shadow splice in the console programs' code (`VICTORY_NATIVE_SHADOW=1`,
//! [`super::shadow_splice`]): each walk of a known shadow trace gets a second arm, inline ray
//! queries on a host top level, beside the title's own walk, which stays as the fallback. The
//! arm runs only when the host's word says the dispatch has its host top levels bound (the
//! selector, `ttmp15`, as for glass).
//!
//! The title's shadow walks are wave-wide: one node pointer for the wave in SGPRs, the lanes that
//! still walk in a mask, and the walk is skipped (`s_cbranch_scc1 past`) when no lane takes it.
//! That skip is where the splice goes: it becomes `s_branch R`, R a region after the program's
//! end. R with selector 0 replays the comparison and the skip and goes on into the walk as the
//! title does; with selector 1 it runs the arm and goes on where the walk's results are read.
//!
//! The arm, per walk ([`Walk`]):
//! - the walk's own set-up, copied from the program (from the skip to the loop's head: straight
//!   code that sets the maximum distance and the walk's state as the code after it expects);
//! - a marker ([`super::glass_splice::Marker`]: the hit part on the walk's top level) traces the
//!   ray the walk's node tests take (origin, direction, extent as the maximum distance, the
//!   instance mask), for the walk's lanes only (exec is narrowed around it); a second marker
//!   gives the hit's indices where the walk's results need them;
//! - the walk's results ([`Out`]) as the code after it reads them.
//!
//! Two kinds of walk in the distant traces (`CS_RTDistantShadowTrace*_Tiled`):
//! - on the near top level (t355): after the merge the title reads the hit instance node's +52
//!   word (its contribution to the hit group index) and, through its +60 word (the bottom
//!   level's base), the hit leaf's +48 word (geometry index << 8), and adds the two: a
//!   per-geometry table index. The arm goes on right after those reads (where exec is the hit
//!   lanes again) with both values from the ray query, so no console structure is read: the
//!   hit lanes in exec, the walk's saved exec and the hit lanes in their SGPRs ([`Out::Exec`]);
//! - on the far top level (t356): the hit leaf's node pointer (non-zero, bit 2 clear: a
//!   triangle) and the maximum distance shrunk to the hit's, read after the merge as in
//!   Checkerboard.
//!
//! Opaque geometry only (`CULL_NON_OPAQUE`); the closest hit, as the title's walks shrink their
//! maximum distance at each accepted hit. Faces are not culled (the title culls one face per its
//! instance flags: a back face can occlude here where it does not on the console). The walks
//! test no instance mask (0xff). The arm's own values live in VGPRs past the program's and in
//! trap temporaries `ttmp0`-`ttmp3`.

use super::glass_patch::{
    EXEC_LO, LITERAL, ONE, S_AND, S_BRANCH, S_CBRANCH_SCC1, TTMP0, TTMP15, V_CMP_NE_U32, VCC_LO,
    ZERO, cndmask, fnv, marker, s_cmp_lg, s_mov, sop2, sopp, v, v_mov, vopc,
};
use crate::native::NativeProgram;

/// What a walk leaves for the code after it.
#[derive(Clone, Copy, Debug)]
pub(super) enum Out {
    /// On a hit of one of the walk's lanes: the hit's distance.
    HitT(u8),
    /// On a hit of one of the walk's lanes: this inline constant code (`0x80` 0 .. `0xc0` 64).
    Hit(u8, u32),
    /// On every lane: this inline constant code.
    Always(u8, u32),
    /// On a hit of one of the walk's lanes: the instance's contribution to the hit group index.
    Contribution(u8),
    /// On a hit of one of the walk's lanes: the geometry index.
    Geometry(u8),
    /// Last: `saved` takes exec, `hits` and exec the walk's lanes with a hit.
    Exec { saved: u32, hits: u32 },
}

/// One walk of a shadow trace.
#[derive(Clone, Copy, Debug)]
pub(super) struct Walk {
    /// The comparison that sets SCC for the skip (one SOPC word) and the skip
    /// (`s_cbranch_scc1 past`) right after it.
    pub cmp: u32,
    pub skip: usize,
    pub past: usize,
    /// The walk loop's head: the arm runs the code from the skip to here first.
    pub head: usize,
    /// Where the arm goes on: the merge, or past the console reads after it.
    pub resume: usize,
    /// The SGPR holding the walk's lanes.
    pub lanes: u32,
    /// The host top level: 0 the first ([`super::shader::SLOT_TLAS`]), 1 the second
    /// ([`super::shader::SLOT_TLAS_NEAR`]).
    pub structure: u8,
    /// The ray as the walk's node tests take it.
    pub origin: [u8; 3],
    pub direction: [u8; 3],
    pub extent: u8,
    /// The instance mask for the query (`InstanceMask & mask != 0`): 0xff where the title's
    /// walk tests none.
    pub mask: u32,
    pub outs: &'static [Out],
}

/// A known shadow trace: its container hash as the run names programs, container digest,
/// program length and code hash, the registers of its two top levels' views (`None`: the one
/// structure view it has), its walks.
pub(super) struct Trace {
    pub hash: &'static str,
    pub container: [u8; 16],
    pub bytes: usize,
    pub code: u64,
    pub structures: [Option<u32>; 2],
    pub walks: &'static [Walk],
}

/// `CS_TraceShadowRays_Checkerboard` (`559d9f0184e15fe3`): one walk, from the skip at 0x1460 to
/// the merge at 0x20b8. After it, `v17` is the hit triangle's node pointer (0: no hit; bit 2
/// clear for a triangle), `v15` the hit distance (the maximum distance without one), `v24` the
/// walk's iteration count (above 0xfffe counts as occluded; 0 on the skip path). A hit sets
/// `v17` to the leaf node pointer (`v27 = v55` at 0x1c7c: bit 2 clear, so category 1 after the
/// merge); the arm writes 1, which the code after the merge reads the same. The walk tests no
/// instance mask (the top byte of the instance's contribution word is its flags: cull and
/// opaque bits). Nothing after its merge reads console structures.
const CHECKERBOARD: Trace = Trace {
    hash: "559d9f0184e15fe3",
    container: [
        0x64, 0x97, 0xe1, 0x58, 0x1c, 0xe5, 0x01, 0x23, 0xb8, 0x60, 0xaa, 0xe7, 0xc8, 0xba, 0x3a,
        0xfd,
    ],
    bytes: 11648,
    code: 0x8cf4_fea5_e616_778d,
    structures: [None, None],
    walks: &[Walk {
        cmp: 0xbf06_8016, // s_cmp_eq_u32 s22, 0
        skip: 0x1460,
        past: 0x20b0,
        head: 0x1464,
        resume: 0x20b8,
        lanes: 22,
        structure: 0,
        origin: [11, 14, 13],
        direction: [18, 19, 53],
        extent: 15,
        mask: 0xff,
        outs: &[Out::Hit(17, ONE), Out::HitT(15), Out::Always(24, ZERO)],
    }],
};

/// The distant traces' top levels: t355 (near, table dword 24), t356 (far, dword 8).
const DISTANT_STRUCTURES: [Option<u32>; 2] = [Some(355), Some(356)];

/// `CS_RTDistantShadowTrace32_Tiled_d6296256` (`a1b20bc47e3c9597`): four walks, two samples of a
/// near (t355) and a far (t356) walk.
const DISTANT: Trace = Trace {
    hash: "a1b20bc47e3c9597",
    container: [0x7b, 0x52, 0x38, 0x21, 0xb8, 0x06, 0x16, 0x11, 0x0b, 0x9c, 0x4f, 0xb8, 0x7f, 0x30, 0x35, 0x9e],
    bytes: 21952,
    code: 0x3e83_9da5_b0de_010d,
    structures: DISTANT_STRUCTURES,
    walks: DISTANT_WALKS,
};

/// `CS_RTDistantShadowTraceLeakPlug32_Tiled_d6296256` (`e8c962358d862e3f`): its four near and
/// far walks (its first, the leak plug's short ray, is not spliced: its direction is made in the
/// loop).
const DISTANT_LEAK_PLUG: Trace = Trace {
    hash: "e8c962358d862e3f",
    container: [0x0b, 0xd2, 0x42, 0x2c, 0x36, 0xa7, 0xf4, 0x9a, 0xeb, 0x54, 0x31, 0xe7, 0x58, 0x09, 0xbe, 0x59],
    bytes: 25408,
    code: 0x137b_85e3_b21e_102d,
    structures: DISTANT_STRUCTURES,
    walks: DISTANT_LEAK_PLUG_WALKS,
};

/// `CS_RTDistantShadowTraceTemporalUpsample4X32_Tiled_d6296256` (`17829ac71aab4e8d`): a near and
/// a far walk.
const DISTANT_UPSAMPLE: Trace = Trace {
    hash: "17829ac71aab4e8d",
    container: [0xb8, 0x69, 0x67, 0x07, 0x66, 0xfa, 0xe9, 0x14, 0xb3, 0xf6, 0xdc, 0x75, 0x38, 0x8a, 0x9d, 0xa9],
    bytes: 19840,
    code: 0x3c62_bdb9_5644_e81d,
    structures: DISTANT_STRUCTURES,
    walks: DISTANT_UPSAMPLE_WALKS,
};

/// `CS_RTDistantShadowTraceLeakPlugTemporalUpsample4X32_Tiled_d6296256` (`afeea48bbb9913ad`): its
/// near and far walk (the leak plug's, first, is not spliced).
const DISTANT_LEAK_PLUG_UPSAMPLE: Trace = Trace {
    hash: "afeea48bbb9913ad",
    container: [0x61, 0x81, 0x7d, 0xce, 0x4f, 0xa7, 0xf8, 0xfa, 0x8e, 0x16, 0x63, 0x3c, 0x4f, 0x3b, 0x60, 0x5c],
    bytes: 23488,
    code: 0x9a0d_6767_f598_525d,
    structures: DISTANT_STRUCTURES,
    walks: DISTANT_LEAK_PLUG_UPSAMPLE_WALKS,
};

#[rustfmt::skip]
const DISTANT_WALKS: &[Walk] = &[
    Walk { cmp: 0xbf06802e, skip: 0x8bc, past: 0x1718, head: 0x908, resume: 0x17d8, lanes: 46, structure: 0, origin: [12, 17, 18], direction: [14, 15, 16], extent: 27, mask: 0xff, outs: &[Out::Always(22, ZERO), Out::Always(23, ZERO), Out::Contribution(22), Out::Geometry(23), Out::Exec { saved: 1, hits: 0 }] },
    Walk { cmp: 0xbf06802e, skip: 0x1968, past: 0x27a8, head: 0x19a0, resume: 0x27a8, lanes: 46, structure: 1, origin: [12, 17, 18], direction: [14, 15, 16], extent: 21, mask: 0xff, outs: &[Out::Hit(25, ONE), Out::HitT(21)] },
    Walk { cmp: 0xbf068022, skip: 0x2ccc, past: 0x3b28, head: 0x2d18, resume: 0x3be8, lanes: 34, structure: 0, origin: [17, 18, 19], direction: [14, 15, 16], extent: 28, mask: 0xff, outs: &[Out::Always(23, ZERO), Out::Always(24, ZERO), Out::Contribution(23), Out::Geometry(24), Out::Exec { saved: 1, hits: 0 }] },
    Walk { cmp: 0xbf068023, skip: 0x3d74, past: 0x4bb8, head: 0x3db0, resume: 0x4bb8, lanes: 35, structure: 1, origin: [17, 18, 19], direction: [14, 15, 16], extent: 22, mask: 0xff, outs: &[Out::Hit(26, ONE), Out::HitT(22)] },
];
#[rustfmt::skip]
const DISTANT_LEAK_PLUG_WALKS: &[Walk] = &[
    Walk { cmp: 0xbf06802e, skip: 0x1688, past: 0x24d8, head: 0x16d0, resume: 0x2598, lanes: 46, structure: 0, origin: [18, 19, 20], direction: [14, 15, 16], extent: 32, mask: 0xff, outs: &[Out::Always(24, ZERO), Out::Always(25, ZERO), Out::Contribution(24), Out::Geometry(25), Out::Exec { saved: 1, hits: 0 }] },
    Walk { cmp: 0xbf06802e, skip: 0x2728, past: 0x3560, head: 0x2760, resume: 0x3560, lanes: 46, structure: 1, origin: [18, 19, 20], direction: [14, 15, 16], extent: 21, mask: 0xff, outs: &[Out::Hit(29, ONE), Out::HitT(21)] },
    Walk { cmp: 0xbf068024, skip: 0x3a88, past: 0x48d0, head: 0x3ad0, resume: 0x4990, lanes: 36, structure: 0, origin: [17, 18, 19], direction: [14, 15, 16], extent: 31, mask: 0xff, outs: &[Out::Always(23, ZERO), Out::Always(24, ZERO), Out::Contribution(23), Out::Geometry(24), Out::Exec { saved: 2, hits: 1 }] },
    Walk { cmp: 0xbf068024, skip: 0x4b1c, past: 0x5950, head: 0x4b50, resume: 0x5950, lanes: 36, structure: 1, origin: [17, 18, 19], direction: [14, 15, 16], extent: 22, mask: 0xff, outs: &[Out::Hit(28, ONE), Out::HitT(22)] },
];
#[rustfmt::skip]
const DISTANT_UPSAMPLE_WALKS: &[Walk] = &[
    Walk { cmp: 0xbf06801d, skip: 0x910, past: 0x17d4, head: 0x970, resume: 0x1890, lanes: 29, structure: 0, origin: [19, 20, 21], direction: [17, 18, 22], extent: 30, mask: 0xff, outs: &[Out::Always(25, ZERO), Out::Always(27, ZERO), Out::Contribution(25), Out::Geometry(27), Out::Exec { saved: 8, hits: 0 }] },
    Walk { cmp: 0xbf06801e, skip: 0x1a4c, past: 0x28f0, head: 0x1a98, resume: 0x28f0, lanes: 30, structure: 1, origin: [19, 20, 21], direction: [17, 18, 22], extent: 26, mask: 0xff, outs: &[Out::Hit(28, ONE), Out::HitT(26)] },
];
#[rustfmt::skip]
const DISTANT_LEAK_PLUG_UPSAMPLE_WALKS: &[Walk] = &[
    Walk { cmp: 0xbf068013, skip: 0x1730, past: 0x25d4, head: 0x1788, resume: 0x2690, lanes: 19, structure: 0, origin: [21, 22, 23], direction: [18, 19, 20], extent: 38, mask: 0xff, outs: &[Out::Always(28, ZERO), Out::Always(29, ZERO), Out::Contribution(28), Out::Geometry(29), Out::Exec { saved: 2, hits: 1 }] },
    Walk { cmp: 0xbf068013, skip: 0x2840, past: 0x36c8, head: 0x2888, resume: 0x36c8, lanes: 19, structure: 1, origin: [21, 22, 23], direction: [18, 19, 20], extent: 25, mask: 0xff, outs: &[Out::Hit(30, ONE), Out::HitT(25)] },
];

/// Every shadow trace the splice knows.
pub(super) const TRACES: &[Trace] =
    &[CHECKERBOARD, DISTANT, DISTANT_LEAK_PLUG, DISTANT_UPSAMPLE, DISTANT_LEAK_PLUG_UPSAMPLE];

/// For the host's binding: the registers of the two top levels' views of the shadow trace of
/// container hash `hash` (as the run names programs; `None` in both: its one structure view),
/// `None` for any other program.
pub fn shadow_structures(hash: &str) -> Option<[Option<u32>; 2]> {
    TRACES.iter().find(|t| hash.starts_with(t.hash)).map(|t| t.structures)
}

/// The arm's VGPRs from `own`: the ray (mask, a pad, extent, origin, direction, inverse
/// direction), then the two markers' four results each.
const RAY_DWORDS: u8 = 12;
const HIT_DWORDS: u8 = 4;
/// VGPRs one walk's arm adds to the program's.
pub(super) const ARM_VGPRS: u16 = (RAY_DWORDS + 2 * HIT_DWORDS) as u16;

/// `s_and_saveexec_b32 dst, src`.
fn and_saveexec(dst: u32, src: u32) -> u32 {
    0xbe80_3c00 | dst << 16 | src
}

/// The region at byte `start` for `walk` of the program `code`, the arm's VGPRs from `own`.
pub(super) fn region(walk: &Walk, code: &[u8], start: usize, own: u8) -> Vec<u32> {
    let mut w: Vec<u32> = Vec::new();
    let here = |w: &Vec<u32>| start + 4 * w.len();
    w.push(s_cmp_lg(TTMP15, ZERO));
    // The native arm comes after this branch and the title arm's three words.
    let native = here(&w) + 4 * 4;
    w.push(sopp(S_CBRANCH_SCC1, here(&w), native));
    // The title's own walk, as before the splice.
    w.push(walk.cmp);
    w.push(sopp(S_CBRANCH_SCC1, here(&w), walk.past));
    w.push(sopp(S_BRANCH, here(&w), walk.skip + 4));
    debug_assert_eq!(here(&w), native);
    // The walk's set-up: straight code, the same wherever it runs.
    let setup = &code[walk.skip + 4..walk.head];
    w.extend(setup.chunks_exact(4).map(|b| u32::from_le_bytes(b.try_into().unwrap())));
    arm(&mut w, walk, own);
    w.push(sopp(S_BRANCH, here(&w), walk.resume));
    w
}

/// The native arm after the walk's set-up (see the module notes).
pub(super) fn arm(w: &mut Vec<u32>, walk: &Walk, own: u8) {
    let ray = own;
    let hit = own + RAY_DWORDS;
    let ids = hit + HIT_DWORDS;
    let (extent, origin, direction) = (ray + 2, ray + 3, ray + 6);
    w.extend([v_mov(ray, LITERAL), walk.mask, v_mov(ray + 1, ZERO)]);
    w.push(v_mov(extent, v(walk.extent)));
    for k in 0..3u8 {
        w.push(v_mov(origin + k, v(walk.origin[usize::from(k)])));
        w.push(v_mov(direction + k, v(walk.direction[usize::from(k)])));
        w.push(v_mov(direction + 3 + k, ZERO));
    }
    let indices = walk.outs.iter().any(|o| matches!(o, Out::Contribution(_) | Out::Geometry(_)));
    // The walk's lanes only: ttmp1 keeps exec. ttmp0/4 trace the first top level, ttmp8/12
    // the second; the first of each pair gives the hit part, the second the indices.
    let first = TTMP0 + 8 * u32::from(walk.structure);
    w.push(and_saveexec(TTMP0 + 1, walk.lanes));
    w.extend(marker(hit, ray, first));
    if indices {
        w.extend(marker(ids, ray, first + 4));
    }
    w.push(s_mov(EXEC_LO, TTMP0 + 1));
    // ttmp0: the walk's lanes with a hit.
    w.push(vopc(V_CMP_NE_U32, ZERO, hit));
    w.push(sop2(S_AND, TTMP0, VCC_LO, walk.lanes));
    for out in walk.outs {
        match *out {
            Out::HitT(r) => w.extend(cndmask(r, v(r), v(hit + 1), TTMP0)),
            Out::Hit(r, value) => w.extend(cndmask(r, v(r), value, TTMP0)),
            Out::Always(r, value) => w.push(v_mov(r, value)),
            Out::Contribution(r) => w.extend(cndmask(r, v(r), v(ids + 3), TTMP0)),
            Out::Geometry(r) => w.extend(cndmask(r, v(r), v(ids + 2), TTMP0)),
            Out::Exec { saved, hits } => {
                w.push(s_mov(saved, EXEC_LO));
                w.push(s_mov(hits, TTMP0));
                w.push(s_mov(EXEC_LO, TTMP0));
            }
        }
    }
}

/// The trace `native` is, byte for byte, if any.
fn known(digest: &[u8; 16], native: &NativeProgram) -> Option<&'static Trace> {
    TRACES.iter().find(|t| {
        *digest == t.container && native.code.len() == t.bytes && fnv(&native.code) == t.code
    })
}

/// Splices a known trace; any other program is left as it is. Returns whether it changed the
/// program.
pub(super) fn rewrite(digest: &[u8; 16], native: &mut NativeProgram) -> bool {
    let Some(trace) = known(digest, native) else {
        return false;
    };
    apply(trace, native);
    true
}

/// The splice of every walk of `trace`, on its program.
pub(super) fn apply(trace: &Trace, native: &mut NativeProgram) {
    let own = u8::try_from(native.vgprs).expect("a program under 256 VGPRs");
    let code = native.code.clone();
    for walk in trace.walks {
        let start = native.code.len().next_multiple_of(4);
        native.code.resize(start, 0);
        let branch = sopp(S_BRANCH, walk.skip, start);
        native.code[walk.skip..walk.skip + 4].copy_from_slice(&branch.to_le_bytes());
        for word in region(walk, &code, start, own) {
            native.code.extend_from_slice(&word.to_le_bytes());
        }
    }
    native.vgprs += ARM_VGPRS;
}

#[cfg(test)]
#[path = "shadow_patch_tests.rs"]
mod tests;
