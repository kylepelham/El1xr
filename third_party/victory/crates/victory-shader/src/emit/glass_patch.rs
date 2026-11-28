//! The glass splice in the console program's code (`VICTORY_NATIVE_GLASS=1`,
//! [`super::glass_splice`]): the glass trace `6d7fe7745ab9c6d8`'s near walk,
//! from its path split at 0x674 to the merge at 0x9024, gets a second arm of
//! inline ray queries on the host top levels. The title's own walk stays as
//! the fallback: the arm runs only when the host's word says the dispatch has
//! both host top levels bound (the selector, `ttmp15`).
//!
//! What changes in the code (any other program is left as it is):
//! - 0x674 (`s_cbranch_vccnz 0x8f8`, the uniform pick of path A or B)
//!   becomes `s_branch R`, R a region appended after the program's end;
//! - R: selector 0 replays the original branch and goes on at 0x678, as the
//!   title runs; selector 1 runs the arm and jumps to the merge 0x9024.
//!
//! The arm, from the walk as read offline (nativebvh pieces 5, 6, 8):
//! - `s54 = exec` (0x9024 restores exec from it; every lane of the split is
//!   in the arm);
//! - the ray as the walk sets it up at 0x828: origin `v31+v32, v39+v37,
//!   v30+v33`, direction `v0, v7, v9`, maximum distance 10000;
//! - four markers ([`super::glass_splice::Marker`]): hit and indices on the
//!   first top level (the title's `t355`) and on the second (`t354`);
//! - per lane, as the loops choose: lanes with a ray offset longer than
//!   `|CB 0x7c8|` (`s3`, loops L1/L3) take `t355`; the others (L2/L4) take
//!   `t354`, and on a miss with `CB 0x7c8 > 0` `t355` again (L5/L6);
//! - the result set the code after 0x9024 reads: `v40` hit, `v36` distance
//!   (-1.0 on a miss), `v41`/`v26` barycentrics, `v25` instance index, `v35`
//!   primitive, `v23` geometry, `v24` material (contribution + geometry; -1
//!   on a miss), `v27` 1 for an `s3` lane's hit, and `v34`, `v45`-`v47`,
//!   `v22`, `v42`, `v43` 0, `v21` 31 (no far march, as for a title miss).
//!
//! Opaque geometry only (`CULL_NON_OPAQUE`); the geometric normal (`v45`-
//! `v47`) is 0, so the screen-reuse test fails and every hit goes to the
//! resolve. The arm's own values live in VGPRs past the program's (v86 up)
//! and in trap temporaries `ttmp0`-`ttmp3`.

use crate::native::NativeProgram;

/// The glass trace's container digest (bytes 4..20 of the container).
pub(super) const CONTAINER: [u8; 16] = [
    0xc9, 0xba, 0x3f, 0xca, 0x4b, 0xe4, 0x49, 0x45, 0xf4, 0xe1, 0x11, 0x52, 0x31, 0x1c, 0x00, 0x59,
];
/// Its program's length and the FNV-1a hash of its bytes.
const BYTES: usize = 57152;
const CODE: u64 = 0x819b_5549_368a_f520;

/// The uniform path split and where its two targets go on.
const SPLIT: usize = 0x674;
const PATH_A: usize = 0x678;
const PATH_B: usize = 0x8f8;
/// The merge of every near walk: `s_mov_b32 exec_lo, s54`.
const MERGE: usize = 0x9024;
/// VGPRs the program has: the arm's own start here.
const OWN: u8 = 86;

// Operand codes.
const ZERO: u32 = 0x80;
const ONE: u32 = 0x81;
const MINUS_ONE: u32 = 0xc1;
const MINUS_ONE_F: u32 = 0xf3;
const LITERAL: u32 = 0xff;
const VCC_LO: u32 = 106;
const EXEC_LO: u32 = 126;
const TTMP0: u32 = 108;
const TTMP15: u32 = 123;
/// The program's SGPRs the arm reads: the lanes of loops L1/L3, the
/// exec the merge restores, `CB 0x7c8`.
const S3: u32 = 3;
const S54: u32 = 54;
const S53: u32 = 53;

fn v(n: u8) -> u32 {
    256 + u32::from(n)
}

fn s_mov(dst: u32, src: u32) -> u32 {
    0xbe80_0300 | dst << 16 | src
}

/// SOP2 `op` (`s_and_b32` 14, `s_or_b32` 16, `s_andn2_b32` 20).
fn sop2(op: u32, dst: u32, s0: u32, s1: u32) -> u32 {
    0x8000_0000 | op << 23 | dst << 16 | s1 << 8 | s0
}
const S_AND: u32 = 14;
const S_OR: u32 = 16;
const S_ANDN2: u32 = 20;

fn v_mov(dst: u8, src: u32) -> u32 {
    0x7e00_0200 | u32::from(dst) << 17 | src
}

/// VOP2 `op` (`v_add_f32` 3, `v_add_nc_u32` 0x25).
fn vop2(op: u32, dst: u8, src0: u32, vsrc1: u8) -> u32 {
    op << 25 | u32::from(dst) << 17 | u32::from(vsrc1) << 9 | src0
}
const V_ADD_F32: u32 = 3;
const V_ADD_NC_U32: u32 = 0x25;

/// VOPC `op` into VCC (`v_cmp_gt_f32` 4, `v_cmp_ne_u32` 0xc5).
fn vopc(op: u32, src0: u32, vsrc1: u8) -> u32 {
    0x7c00_0000 | op << 17 | u32::from(vsrc1) << 9 | src0
}
const V_CMP_GT_F32: u32 = 4;
const V_CMP_NE_U32: u32 = 0xc5;

/// `v_cndmask_b32 dst, src0, src1, mask` (VOP3: `src1` where `mask`).
fn cndmask(dst: u8, src0: u32, src1: u32, mask: u32) -> [u32; 2] {
    [0xd501_0000 | u32::from(dst), mask << 18 | src1 << 9 | src0]
}

/// SOPP `op` (`s_branch` 2, `s_cbranch_scc1` 5, `s_cbranch_vccnz` 7) to
/// `target` from the instruction at `at`.
fn sopp(op: u32, at: usize, target: usize) -> u32 {
    let delta = (target as i64 - (at as i64 + 4)) / 4;
    let delta = i16::try_from(delta).expect("a branch within 128 KiB");
    0xbf80_0000 | op << 16 | u32::from(delta as u16)
}
const S_BRANCH: u32 = 2;
const S_CBRANCH_SCC1: u32 = 5;
const S_CBRANCH_VCCNZ: u32 = 7;

/// `s_cmp_lg_u32 s0, s1`.
fn s_cmp_lg(s0: u32, s1: u32) -> u32 {
    0xbf07_0000 | s1 << 8 | s0
}

/// A marker: `image_bvh64_intersect_ray v[data:data+3], v[addr:addr+11],
/// ttmp[k:k+3]` (the twelve addresses in order, NSA form).
fn marker(data: u8, addr: u8, ttmp: u32) -> [u32; 5] {
    let a = |k: u8| u32::from(addr + k);
    [
        0xf19c_9f07,
        u32::from(data) << 8 | u32::from(addr) | (ttmp >> 2) << 16,
        a(1) | a(2) << 8 | a(3) << 16 | a(4) << 24,
        a(5) | a(6) << 8 | a(7) << 16 | a(8) << 24,
        a(9) | a(10) << 8 | a(11) << 16,
    ]
}

/// The arm's registers.
mod at {
    /// The ray: two node-pointer dwords (unused), the extent, origin,
    /// direction, inverse direction (unused).
    pub const RAY: u8 = super::OWN;
    pub const EXTENT: u8 = RAY + 2;
    pub const ORIGIN: u8 = RAY + 3;
    pub const DIRECTION: u8 = RAY + 6;
    /// The four markers' results.
    pub const HIT_FAR: u8 = RAY + 12;
    pub const IDS_FAR: u8 = HIT_FAR + 4;
    pub const HIT_NEAR: u8 = IDS_FAR + 4;
    pub const IDS_NEAR: u8 = HIT_NEAR + 4;
    /// Materials (contribution + geometry) of both.
    pub const MATERIAL_FAR: u8 = IDS_NEAR + 4;
    pub const MATERIAL_NEAR: u8 = MATERIAL_FAR + 1;
    pub const MATERIAL: u8 = MATERIAL_NEAR + 1;
    /// VGPRs the spliced program needs.
    pub const END: u8 = MATERIAL + 1;
}

/// The region R at byte `start`: the selector and both arms.
fn region(start: usize) -> Vec<u32> {
    let mut w: Vec<u32> = Vec::new();
    let here = |w: &Vec<u32>| start + 4 * w.len();
    w.push(s_cmp_lg(TTMP15, ZERO));
    // The native arm comes after this branch and the title arm's two.
    let native = here(&w) + 3 * 4;
    w.push(sopp(S_CBRANCH_SCC1, here(&w), native));
    // The title's own walk, as before the splice.
    w.push(sopp(S_CBRANCH_VCCNZ, here(&w), PATH_B));
    w.push(sopp(S_BRANCH, here(&w), PATH_A));
    debug_assert_eq!(here(&w), native);
    arm(&mut w);
    w.push(sopp(S_BRANCH, here(&w), MERGE));
    w
}

/// The native arm (see the module notes).
fn arm(w: &mut Vec<u32>) {
    use at::*;
    w.push(s_mov(S54, EXEC_LO));
    w.extend([v_mov(RAY, ZERO), v_mov(RAY + 1, ZERO)]);
    w.extend([v_mov(EXTENT, LITERAL), 10000.0f32.to_bits()]);
    w.push(vop2(V_ADD_F32, ORIGIN, v(31), 32));
    w.push(vop2(V_ADD_F32, ORIGIN + 1, v(39), 37));
    w.push(vop2(V_ADD_F32, ORIGIN + 2, v(30), 33));
    w.push(v_mov(DIRECTION, v(0)));
    w.push(v_mov(DIRECTION + 1, v(7)));
    w.push(v_mov(DIRECTION + 2, v(9)));
    for k in 0..3 {
        w.push(v_mov(DIRECTION + 3 + k, ZERO));
    }
    w.extend(marker(HIT_FAR, RAY, TTMP0));
    w.extend(marker(IDS_FAR, RAY, TTMP0 + 4));
    w.extend(marker(HIT_NEAR, RAY, TTMP0 + 8));
    w.extend(marker(IDS_NEAR, RAY, TTMP0 + 12));
    // ttmp2: the second structure hit; ttmp0: lanes that take it (not s3).
    w.push(vopc(V_CMP_NE_U32, ZERO, HIT_NEAR));
    w.push(s_mov(TTMP0 + 2, VCC_LO));
    w.push(sop2(S_ANDN2, TTMP0, VCC_LO, S3));
    // ttmp1: lanes that take the first: s3, and the others' second chance
    // (CB 0x7c8 > 0, the second missed).
    w.push(vopc(V_CMP_GT_F32, S53, RAY));
    w.push(sop2(S_ANDN2, TTMP0 + 1, VCC_LO, S3));
    w.push(sop2(S_ANDN2, TTMP0 + 1, TTMP0 + 1, TTMP0 + 2));
    w.push(sop2(S_OR, TTMP0 + 1, TTMP0 + 1, S3));
    // Materials: contribution + geometry.
    w.push(vop2(
        V_ADD_NC_U32,
        MATERIAL_FAR,
        v(IDS_FAR + 3),
        IDS_FAR + 2,
    ));
    w.push(vop2(
        V_ADD_NC_U32,
        MATERIAL_NEAR,
        v(IDS_NEAR + 3),
        IDS_NEAR + 2,
    ));
    // Each result: the miss value, the first structure's where it is taken,
    // the second's where that is.
    let results: [(u8, u32, u8, u8); 8] = [
        (40, ZERO, HIT_FAR, HIT_NEAR),
        (36, MINUS_ONE_F, HIT_FAR + 1, HIT_NEAR + 1),
        (41, ZERO, HIT_FAR + 2, HIT_NEAR + 2),
        (26, ZERO, HIT_FAR + 3, HIT_NEAR + 3),
        (25, MINUS_ONE, IDS_FAR, IDS_NEAR),
        (35, MINUS_ONE, IDS_FAR + 1, IDS_NEAR + 1),
        (23, MINUS_ONE, IDS_FAR + 2, IDS_NEAR + 2),
        (MATERIAL, MINUS_ONE, MATERIAL_FAR, MATERIAL_NEAR),
    ];
    for (dst, miss, far, near) in results {
        w.extend(cndmask(dst, miss, v(far), TTMP0 + 1));
        w.extend(cndmask(dst, v(dst), v(near), TTMP0));
    }
    // ttmp3: the lanes with a hit. A miss of the structure taken leaves the
    // marker's miss values (0, -1.0, 0, 0, and -1 indices), as the title's.
    w.push(vopc(V_CMP_NE_U32, ZERO, 40));
    w.push(s_mov(TTMP0 + 3, VCC_LO));
    w.extend(cndmask(24, MINUS_ONE, v(MATERIAL), TTMP0 + 3));
    // v27: 1 for a hit of an s3 lane (loops L1/L3).
    w.push(sop2(S_AND, TTMP0 + 2, S3, TTMP0 + 3));
    w.extend(cndmask(27, ZERO, ONE, TTMP0 + 2));
    for r in [34, 45, 46, 47, 22, 42, 43] {
        w.push(v_mov(r, ZERO));
    }
    w.push(v_mov(21, 31 + ZERO));
}

fn fnv(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

/// Whether `native` is the glass trace's program, byte for byte.
fn known(digest: &[u8; 16], native: &NativeProgram) -> bool {
    *digest == CONTAINER && native.code.len() == BYTES && fnv(&native.code) == CODE
}

/// Splices the known program; any other is left as it is. Returns whether
/// it changed the program.
pub(super) fn rewrite(digest: &[u8; 16], native: &mut NativeProgram) -> bool {
    if !known(digest, native) {
        return false;
    }
    apply(native);
    true
}

/// The splice, on the program of the known layout.
fn apply(native: &mut NativeProgram) {
    let start = native.code.len().next_multiple_of(4);
    native.code.resize(start, 0);
    let branch = sopp(S_BRANCH, SPLIT, start);
    native.code[SPLIT..SPLIT + 4].copy_from_slice(&branch.to_le_bytes());
    for word in region(start) {
        native.code.extend_from_slice(&word.to_le_bytes());
    }
    native.vgprs = native.vgprs.max(u16::from(at::END));
}

#[cfg(test)]
#[path = "glass_patch_tests.rs"]
mod tests;
