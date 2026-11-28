//! The shadow splice in the console programs' code (`VICTORY_NATIVE_SHADOW=1`,
//! [`super::shadow_splice`]): each walk of a known shadow trace gets a second arm, an inline ray
//! query on the host top level, beside the title's own walk, which stays as the fallback. The
//! arm runs only when the host's word says the dispatch has its host top level bound (the
//! selector, `ttmp15`, as for glass).
//!
//! The title's shadow walks are wave-wide: one node pointer for the wave in SGPRs, the lanes that
//! still walk in a mask, and the walk is skipped (`s_cbranch_scc1 past`) when no lane takes it.
//! That skip is where the splice goes: it becomes `s_branch R`, R a region after the program's
//! end. R with selector 0 replays the comparison and the skip and goes on into the walk as the
//! title does; with selector 1 it runs the arm and jumps to the walk's merge.
//!
//! The arm, per walk ([`Walk`]):
//! - one marker ([`super::glass_splice::Marker`], the first top level's hit part) traces the
//!   ray the walk's node tests take (origin, direction, extent as the maximum distance, the
//!   title's instance mask), for the walk's lanes only (exec is narrowed around it);
//! - the walk's results as the code after its merge reads them: on a hit, for the walk's lanes,
//!   [`Out::Hit`] registers take their hit value; [`Out::Always`] registers take theirs on every
//!   lane, as the skip path or the walk leaves them for a miss.
//!
//! Opaque geometry only (`CULL_NON_OPAQUE`); the closest hit, as the title's walks shrink their
//! maximum distance at each accepted hit. Faces are not culled (the title culls one face per its
//! instance flags: a back face can occlude here where it does not on the console). The arm's
//! own values live in VGPRs past the program's and in trap temporaries `ttmp0`-`ttmp3`.

use super::glass_patch::{
    EXEC_LO, LITERAL, ONE, S_AND, S_BRANCH, S_CBRANCH_SCC1, TTMP0, TTMP15, V_CMP_NE_U32, VCC_LO,
    ZERO, cndmask, fnv, marker, s_cmp_lg, s_mov, sop2, sopp, v, v_mov, vopc,
};
use crate::native::NativeProgram;

/// What a walk leaves in a register for the code after its merge.
#[derive(Clone, Copy, Debug)]
pub(super) enum Out {
    /// On a hit of one of the walk's lanes: the hit's distance.
    HitT(u8),
    /// On a hit of one of the walk's lanes: this inline constant code (`0x80` 0 .. `0xc0` 64).
    Hit(u8, u32),
    /// On every lane: this inline constant code.
    Always(u8, u32),
}

/// One walk of a shadow trace.
#[derive(Clone, Copy, Debug)]
pub(super) struct Walk {
    /// The comparison that sets SCC for the skip (one SOPC word) and the skip
    /// (`s_cbranch_scc1 past`) right after it.
    pub cmp: u32,
    pub skip: usize,
    pub past: usize,
    /// Where the walk's paths meet again.
    pub merge: usize,
    /// The SGPR holding the walk's lanes.
    pub lanes: u32,
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
/// program length and code hash, its walks.
pub(super) struct Trace {
    pub hash: &'static str,
    pub container: [u8; 16],
    pub bytes: usize,
    pub code: u64,
    pub walks: &'static [Walk],
}

/// `CS_TraceShadowRays_Checkerboard` (`559d9f0184e15fe3`): one walk, from the skip at 0x1460 to
/// the merge at 0x20b8. After it, `v17` is the hit triangle's node pointer (0: no hit; bit 2
/// clear for a triangle), `v15` the hit distance (the maximum distance without one), `v24` the
/// walk's iteration count (above 0xfffe counts as occluded; 0 on the skip path). A hit sets
/// `v17` to the leaf node pointer (`v27 = v55` at 0x1c7c: bit 2 clear, so category 1 after the
/// merge); the arm writes 1, which the code after the merge reads the same. The walk tests no
/// instance mask (the top byte of the instance's contribution word is its flags: cull and
/// opaque bits).
///
/// What the code after the merge reads of the console structures: nothing (the walk itself reads
/// the instance node's +52/+56 words and the leaf's +48 word, all inside the replaced part).
const CHECKERBOARD: Trace = Trace {
    hash: "559d9f0184e15fe3",
    container: [
        0x64, 0x97, 0xe1, 0x58, 0x1c, 0xe5, 0x01, 0x23, 0xb8, 0x60, 0xaa, 0xe7, 0xc8, 0xba, 0x3a,
        0xfd,
    ],
    bytes: 11648,
    code: 0x8cf4_fea5_e616_778d,
    walks: &[Walk {
        cmp: 0xbf06_8016, // s_cmp_eq_u32 s22, 0
        skip: 0x1460,
        past: 0x20b0,
        merge: 0x20b8,
        lanes: 22,
        origin: [11, 14, 13],
        direction: [18, 19, 53],
        extent: 15,
        mask: 0xff,
        outs: &[Out::Hit(17, ONE), Out::HitT(15), Out::Always(24, ZERO)],
    }],
};

/// Every shadow trace the splice knows.
pub(super) const TRACES: &[Trace] = &[CHECKERBOARD];

/// The container hashes (as the run names programs) of [`TRACES`], for the host's binding.
pub const SHADOW_TRACES: &[&str] = &[CHECKERBOARD.hash];

/// The arm's VGPRs from `own`: the ray (mask, a pad, extent, origin, direction, inverse
/// direction), then the marker's four results.
const RAY_DWORDS: u8 = 12;
const HIT_DWORDS: u8 = 4;
/// VGPRs one walk's arm adds to the program's.
pub(super) const ARM_VGPRS: u16 = (RAY_DWORDS + HIT_DWORDS) as u16;

/// `s_and_saveexec_b32 dst, src`.
fn and_saveexec(dst: u32, src: u32) -> u32 {
    0xbe80_3c00 | dst << 16 | src
}

/// The region at byte `start` for `walk`, the arm's VGPRs from `own`.
pub(super) fn region(walk: &Walk, start: usize, own: u8) -> Vec<u32> {
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
    arm(&mut w, walk, own);
    w.push(sopp(S_BRANCH, here(&w), walk.merge));
    w
}

/// The native arm (see the module notes).
pub(super) fn arm(w: &mut Vec<u32>, walk: &Walk, own: u8) {
    let ray = own;
    let hit = own + RAY_DWORDS;
    let (extent, origin, direction) = (ray + 2, ray + 3, ray + 6);
    w.extend([v_mov(ray, LITERAL), walk.mask, v_mov(ray + 1, ZERO)]);
    w.push(v_mov(extent, v(walk.extent)));
    for k in 0..3u8 {
        w.push(v_mov(origin + k, v(walk.origin[usize::from(k)])));
        w.push(v_mov(direction + k, v(walk.direction[usize::from(k)])));
        w.push(v_mov(direction + 3 + k, ZERO));
    }
    // The walk's lanes only: ttmp1 keeps exec.
    w.push(and_saveexec(TTMP0 + 1, walk.lanes));
    w.extend(marker(hit, ray, TTMP0));
    w.push(s_mov(EXEC_LO, TTMP0 + 1));
    // ttmp0: the walk's lanes with a hit.
    w.push(vopc(V_CMP_NE_U32, ZERO, hit));
    w.push(sop2(S_AND, TTMP0, VCC_LO, walk.lanes));
    for out in walk.outs {
        match *out {
            Out::HitT(r) => w.extend(cndmask(r, v(r), v(hit + 1), TTMP0)),
            Out::Hit(r, value) => w.extend(cndmask(r, v(r), value, TTMP0)),
            Out::Always(r, value) => w.push(v_mov(r, value)),
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
    for walk in trace.walks {
        let start = native.code.len().next_multiple_of(4);
        native.code.resize(start, 0);
        let branch = sopp(S_BRANCH, walk.skip, start);
        native.code[walk.skip..walk.skip + 4].copy_from_slice(&branch.to_le_bytes());
        for word in region(walk, start, own) {
            native.code.extend_from_slice(&word.to_le_bytes());
        }
    }
    native.vgprs += ARM_VGPRS;
}

#[cfg(test)]
#[path = "shadow_patch_tests.rs"]
mod tests;
