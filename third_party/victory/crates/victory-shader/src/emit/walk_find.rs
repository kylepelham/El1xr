//! Finds the title's structure walks in a console program by their shape, the first step of a
//! splice made without a hand-written table per program ([`super::shadow_patch`] has one row
//! per walk, found offline).
//!
//! The title's inline ray queries compile to wave-wide walks of this shape (the hand-found
//! shadow walks and the GPURT reference, `RayQuery1_1.hlsl`, agree on it):
//!
//! ```text
//!   s_cmp_eq_u32 s<lanes>, 0          ; no lane takes the walk
//!   s_cbranch_scc1 past               ; the skip
//!   <straight set-up>                 ; copies of the ray, inverse direction, the stack
//! head:
//!   ... image_bvh_intersect_ray ...   ; one node test per iteration, nested loops for the
//!   ... s_cbranch_* head              ; bottom levels, a backward branch to the head
//! past:
//! ```
//!
//! For each walk this gives the skip and its comparison, where the skip goes, the loop's head,
//! the node tests, and the ray as the first node test takes it: its registers taken back
//! through the set-up's plain copies to the ones live at the skip (what a splice's arm reads).
//! Where the walk's results are read after it (a splice's `resume` and outputs) is not found
//! here.

use crate::gfx10::{self, Encoding, Fields, Instruction, Operand};

/// One walk found in a program.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Found {
    /// The comparison word before the skip, and the skip's offset.
    pub cmp: u32,
    pub skip: usize,
    /// Where the skip goes (past the walk).
    pub past: usize,
    /// The loop's head: the target of its backward branches; the last of them.
    pub head: usize,
    pub end: usize,
    /// The SGPR the comparison tests against 0: the walk's lanes.
    pub lanes: u32,
    /// The node tests in the loop, by offset.
    pub tests: Vec<usize>,
    /// 64-bit node pointers (`image_bvh64_intersect_ray`).
    pub wide: bool,
    /// The ray at the skip: origin, direction and extent VGPRs.
    pub origin: [u8; 3],
    pub direction: [u8; 3],
    pub extent: u8,
}

fn is_test(i: &Instruction) -> bool {
    i.encoding == Encoding::Mimg
        && matches!(
            i.name(),
            Some("image_bvh_intersect_ray" | "image_bvh64_intersect_ray")
        )
}

fn is_branch(i: &Instruction) -> bool {
    i.encoding == Encoding::Sopp
        && i.name()
            .is_some_and(|n| n == "s_branch" || n.starts_with("s_cbranch_"))
}

/// A SOPP branch's target.
fn target(i: &Instruction) -> usize {
    let next = i.offset as i64 + 4;
    (next + 4 * i64::from(i.words[0] as u16 as i16)) as usize
}

/// A VGPR operand's first register.
fn vgpr(o: &Operand) -> Option<u8> {
    match *o {
        Operand::Vector { reg, .. } => Some(reg),
        _ => None,
    }
}

/// The node test's address VGPRs, one per dword (a range is split).
fn addresses(i: &Instruction) -> Option<(Vec<u8>, bool)> {
    let Fields::Image { addr, a16, .. } = i.fields(false) else {
        return None;
    };
    let mut regs = Vec::new();
    for a in &addr {
        let Operand::Vector { reg, dwords } = *a else {
            return None;
        };
        regs.extend((0..dwords).map(|k| reg + k));
    }
    Some((regs, a16))
}

/// Every walk of `code`'s shape, in program order.
pub(super) fn walks(code: &[u8]) -> Vec<Found> {
    let Ok(insts) = gfx10::decode(code) else {
        return Vec::new();
    };
    // Backward branches: (from, head).
    let back: Vec<(usize, usize)> = insts
        .iter()
        .filter(|i| is_branch(i) && target(i) <= i.offset)
        .map(|i| (i.offset, target(i)))
        .collect();
    let mut found: Vec<Found> = Vec::new();
    for t in insts.iter().filter(|i| is_test(i)) {
        if found
            .iter()
            .any(|f| f.head <= t.offset && t.offset < f.end)
        {
            continue;
        }
        // The loops around the test, innermost first.
        let mut loops: Vec<(usize, usize)> = back
            .iter()
            .filter(|(from, head)| *head <= t.offset && t.offset < *from)
            .copied()
            .collect();
        loops.sort_by_key(|(_, head)| std::cmp::Reverse(*head));
        loops.dedup_by_key(|(_, head)| *head);
        for (_, head) in loops {
            if let Some(f) = walk_at(&insts, head, &back) {
                found.push(f);
                break;
            }
        }
    }
    found.sort_by_key(|f| f.skip);
    found
}

/// The walk whose loop starts at `head`, if the code before it has the skip's shape.
fn walk_at(insts: &[Instruction], head: usize, back: &[(usize, usize)]) -> Option<Found> {
    let at = insts.iter().position(|i| i.offset == head)?;
    let last_back = back
        .iter()
        .filter(|(_, h)| *h == head)
        .map(|(from, _)| *from)
        .max()?;
    // Straight set-up back to the skip.
    let mut k = at;
    let skip = loop {
        k = k.checked_sub(1)?;
        let i = &insts[k];
        if i.offset + 4 * i.words.len() != insts[k + 1].offset {
            return None;
        }
        if is_branch(i) {
            break i;
        }
    };
    let past = target(skip);
    if skip.name() != Some("s_cbranch_scc1") || past <= last_back {
        return None;
    }
    let cmp = insts.get(k.checked_sub(1)?)?;
    if cmp.encoding != Encoding::Sopc
        || cmp.name() != Some("s_cmp_eq_u32")
        || cmp.offset + 4 != skip.offset
        || cmp.words.len() != 1
    {
        return None;
    }
    let Fields::Sop { src, .. } = cmp.fields(false) else {
        return None;
    };
    let (Some(Operand::Scalar { code, .. }), Some(Operand::Const(128))) = (src.first(), src.get(1))
    else {
        return None;
    };
    let tests: Vec<usize> = insts
        .iter()
        .filter(|i| head <= i.offset && i.offset < last_back && is_test(i))
        .map(|i| i.offset)
        .collect();
    let first_at = *tests.first()?;
    let first = insts.iter().find(|i| i.offset == first_at)?;
    let (regs, a16) = addresses(first)?;
    let wide = first.name() == Some("image_bvh64_intersect_ray");
    let ray = if wide { 2 } else { 1 };
    if a16 || regs.len() != ray + 10 {
        return None;
    }
    let setup = &insts[k + 1..at];
    let back_to_skip = |reg: u8| resolve(setup, reg);
    Some(Found {
        cmp: cmp.words[0],
        skip: skip.offset,
        past,
        head,
        end: last_back,
        lanes: u32::from(*code),
        tests,
        wide,
        origin: std::array::from_fn(|c| back_to_skip(regs[ray + 1 + c])),
        direction: std::array::from_fn(|c| back_to_skip(regs[ray + 4 + c])),
        extent: back_to_skip(regs[ray]),
    })
}

/// `reg` at the end of `setup`, taken back through its plain copies (`v_mov_b32 vX, vY`, the
/// last write of vX) to the register it holds at the start; a register the set-up writes any
/// other way is kept.
fn resolve(setup: &[Instruction], mut reg: u8) -> u8 {
    let mut end = setup.len();
    while let Some(w) = setup[..end].iter().rposition(|i| writes(i) == Some(reg)) {
        let i = &setup[w];
        let Fields::Valu(v) = i.fields(false) else {
            break;
        };
        let plain = i.encoding == Encoding::Vop1
            && i.name() == Some("v_mov_b32")
            && v.sdwa.is_none()
            && v.dpp.is_none();
        match (plain, v.src.first().and_then(vgpr)) {
            (true, Some(from)) => {
                reg = from;
                end = w;
            }
            _ => break,
        }
    }
    reg
}

/// The VGPR a VALU instruction writes, if one.
fn writes(i: &Instruction) -> Option<u8> {
    match i.fields(false) {
        Fields::Valu(v) => v.dst.as_ref().and_then(vgpr),
        _ => None,
    }
}

#[cfg(test)]
#[path = "walk_find_tests.rs"]
mod tests;
