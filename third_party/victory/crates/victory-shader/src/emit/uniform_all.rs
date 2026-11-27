//! `VICTORY_UNIFORM_STRICT_ALL=1`: a second, stricter proof for the first-lane reads that
//! [`super::uniform_lane::value_uniform`] takes as the same in both host halves of a console
//! wave (each half then reads its own first lane). Off by default.
//!
//! The first proof follows the instructions' named operands from the read. It leaves out:
//! - registers an instruction reads that are not among its source operands (`Inst::reads`
//!   is checked whole; a lane mask read there is one value per lane);
//! - a host half with no lane in the read's guard: its first-lane read (`Lower::first_lane`)
//!   then reads a lane outside the guard, so the value must be the same there too: no
//!   definition it depends on may be guarded (a guarded write leaves the other lanes' old
//!   value), however the guards of definition and read compare;
//! - definitions in a block that not every lane reaching the read went through (a lane that
//!   did not holds whatever the register held before);
//! - values made in a loop that each lane leaves on its own, read after it: each lane holds
//!   the value of the round it left in.
//!
//! [`holds`] checks all four along every value the read depends on. Where the halves run apart
//! and not every lane runs the read's block, a half with no lane there skips the read, so its
//! result must not be used where every lane runs ([`escapes`]).

use std::collections::HashSet;

use super::Lower;
use crate::ir::{EXEC, Inst, Program, Reg, Value};
use crate::structure::{Atom, Bool, LaneExit, Structured};

/// Whether `VICTORY_UNIFORM_STRICT_ALL=1`. A test can choose for its own thread.
pub(super) fn on() -> bool {
    #[cfg(test)]
    if let Some(on) = TEST_STRICT_ALL.get() {
        return on;
    }
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("VICTORY_UNIFORM_STRICT_ALL").is_ok_and(|v| v.trim() == "1"))
}

#[cfg(test)]
thread_local! {
    /// A test's choice for [`on`], for its thread only.
    pub(super) static TEST_STRICT_ALL: std::cell::Cell<Option<bool>> =
        const { std::cell::Cell::new(None) };
}

/// A loop of the program: its blocks, and whether a lane can leave it on its own (a branch
/// on a lane mask that goes out of it).
struct Loop {
    body: Vec<bool>,
    per_lane: bool,
}

struct Proof<'a> {
    p: &'a Program,
    st: &'a Structured,
    idom: Vec<usize>,
    loops: Vec<Loop>,
    /// The guard of the first-lane read.
    read: Option<Bool>,
    seen: HashSet<(Value, usize)>,
}

/// Whether the value `v` that instruction `read` of block `at` (a `v_readfirstlane_b32`)
/// reads holds the same in every lane running the read, by the rules of this module.
pub(super) fn holds(p: &Program, st: &Structured, at: usize, read: usize, v: Value) -> bool {
    let order = crate::ir::reverse_postorder(&p.blocks);
    let mut index = vec![usize::MAX; p.blocks.len()];
    for (k, &b) in order.iter().enumerate() {
        index[b] = k;
    }
    let idom = crate::ir::idoms(&p.blocks, &order, &index);
    let mut proof = Proof {
        p,
        st,
        idom,
        loops: Vec::new(),
        read: guard(st, at, read),
        seen: HashSet::new(),
    };
    proof.loops = proof.find_loops(&index);
    proof.value(v, at)
}

fn guard(st: &Structured, b: usize, i: usize) -> Option<Bool> {
    st.guards.get(b).and_then(|g| g.get(i)).copied().flatten()
}

impl Proof<'_> {
    fn dominates(&self, a: usize, mut b: usize) -> bool {
        loop {
            if a == b {
                return true;
            }
            let d = self.idom.get(b).copied().unwrap_or(usize::MAX);
            if d == b || d == usize::MAX {
                return false;
            }
            b = d;
        }
    }

    /// The natural loops (one per back edge, to a block that dominates its source).
    fn find_loops(&self, index: &[usize]) -> Vec<Loop> {
        let p = self.p;
        let n = p.blocks.len();
        let mut loops = Vec::new();
        for latch in (0..n).filter(|&b| index[b] != usize::MAX) {
            for head in p.blocks[latch].exit.successors() {
                if !self.dominates(head, latch) {
                    continue;
                }
                let mut body = vec![false; n];
                body[head] = true;
                let mut stack = vec![latch];
                while let Some(b) = stack.pop() {
                    if body[b] {
                        continue;
                    }
                    body[b] = true;
                    stack.extend(
                        p.blocks[b]
                            .preds
                            .iter()
                            .copied()
                            .filter(|&q| index[q] != usize::MAX),
                    );
                }
                let per_lane = (0..n)
                    .filter(|&b| body[b])
                    .any(|b| match self.st.exits.get(b) {
                        Some(&LaneExit::Branch {
                            when,
                            taken,
                            not_taken,
                        }) => (!body[taken] || !body[not_taken]) && self.lane_branch(b, when),
                        _ => false,
                    });
                loops.push(Loop { body, per_lane });
            }
        }
        loops
    }

    /// Whether the branch closing block `b` on `when` can send the lanes of one console wave
    /// different ways.
    fn lane_branch(&self, b: usize, when: Bool) -> bool {
        let together = self.st.wide.contains(&b) && !self.st.host_wide.contains(&b);
        !together
            && !self
                .st
                .bools
                .support(when)
                .iter()
                .all(|a| matches!(a, Atom::Uniform(_)))
    }

    fn is_mask(&self, v: Value) -> bool {
        self.st.masks.contains_key(&v) || self.st.highs.contains_key(&v)
    }

    /// Whether `v`, read in block `used`, is the same in every lane running the read.
    fn value(&mut self, v: Value, used: usize) -> bool {
        if !self.seen.insert((v, used)) {
            return true;
        }
        let p = self.p;
        for (b, block) in p.blocks.iter().enumerate() {
            for phi in &block.phis {
                if phi.value != v {
                    continue;
                }
                if !self.made_for_every_lane(b, used) {
                    return false;
                }
                let args: Vec<(Value, usize)> = phi
                    .args
                    .iter()
                    .copied()
                    .zip(block.preds.iter().copied())
                    .collect();
                return args.into_iter().all(|(a, q)| self.value(a, q));
            }
            for (i, inst) in block.insts.iter().enumerate() {
                if !inst.writes.iter().any(|&(_, w)| w == v) {
                    continue;
                }
                if !self.made_for_every_lane(b, used) {
                    return false;
                }
                if self.read.is_some() && guard(self.st, b, i).is_some() {
                    // A guarded read in a host half with no lane in its guard reads a lane
                    // outside it, which holds what the register held before a guarded write.
                    return false;
                }
                let name = inst.name();
                let lane_op = name.starts_with("v_mbcnt")
                    || name.contains("lane")
                    || name.starts_with("v_cmpx");
                let plain = match &inst.fields {
                    crate::gfx10::Fields::Valu(val) => {
                        val.dpp.is_none() && val.sdwa.is_none() && !lane_op
                    }
                    crate::gfx10::Fields::Ds { .. } => {
                        name.starts_with("ds_read") && !name.contains("addtid")
                    }
                    _ => false,
                };
                if !plain {
                    return false;
                }
                // (a register it also writes is read for the lanes the write skips, which the
                // guard rule above answers for)
                let reads: Vec<(Reg, Value)> = inst
                    .reads
                    .iter()
                    .copied()
                    .filter(|&(r, _)| !inst.writes.iter().any(|&(w, _)| w == r))
                    .collect();
                return reads.into_iter().all(|(reg, x)| match reg {
                    Reg::V(_) => self.value(x, b),
                    Reg::S(c) if c == EXEC || c == EXEC + 1 => true,
                    Reg::S(_) => !self.is_mask(x) || self.value(x, b),
                    Reg::Scc => true,
                });
            }
        }
        false
    }

    /// Whether every lane reaching block `used` went through block `b` (where the value is
    /// made) in the same round of every loop it went on: `b` dominates `used`, and no loop
    /// that each lane leaves on its own holds `b` but not `used`.
    fn made_for_every_lane(&self, b: usize, used: usize) -> bool {
        self.dominates(b, used)
            && !self
                .loops
                .iter()
                .any(|l| l.per_lane && l.body[b] && !l.body[used])
    }
}

impl Lower<'_> {
    /// [`holds`] for the first-lane read `inst` of the block being lowered, of `v`; and, where
    /// the halves of a console wave run apart and not every lane runs this block, that the
    /// read's result is not used where every lane runs ([`escapes`]).
    pub(super) fn strict_uniform(&self, inst: &Inst, v: Value) -> bool {
        let b = self.cur_ir;
        let Some(i) = self.x.p.blocks[b].insts.iter().position(|x| std::ptr::eq(x, inst)) else {
            return false;
        };
        if !holds(self.x.p, self.x.st, b, i, v) {
            return false;
        }
        match &self.exchange {
            Some((_, false, all)) if !self.local && !all.get(b).copied().unwrap_or(false) => {
                !escapes(self.x.p, all, inst)
            }
            _ => true,
        }
    }
}

/// Whether what `inst` writes reaches, through the instructions and phis of blocks not every
/// lane runs, one of a block every lane runs (`all`). A host half with no lane in `inst`'s
/// block skips it, so it never has the value; past the join it is the scalar the console
/// wave made, which that half must have too.
pub(super) fn escapes(p: &Program, all: &[bool], inst: &Inst) -> bool {
    let runs_all = |b: usize| all.get(b).copied().unwrap_or(false);
    let mut seen: HashSet<Value> = HashSet::new();
    let mut work: Vec<Value> = inst.writes.iter().map(|&(_, w)| w).collect();
    while let Some(v) = work.pop() {
        if !seen.insert(v) {
            continue;
        }
        for (b, block) in p.blocks.iter().enumerate() {
            // (a VGPR written in the block holds what the console's skipped lanes would: the
            // old value; only scalars carry the read on)
            for phi in block.phis.iter().filter(|phi| phi.args.contains(&v) && !matches!(phi.reg, Reg::V(_))) {
                if runs_all(b) {
                    return true;
                }
                work.push(phi.value);
            }
            for x in block.insts.iter().filter(|x| x.reads.iter().any(|&(_, r)| r == v)) {
                if runs_all(b) {
                    return true;
                }
                work.extend(x.writes.iter().filter(|(r, _)| !matches!(r, Reg::V(_))).map(|&(_, w)| w));
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::super::uniform_lane::{Uniform, value_uniform};
    use super::holds;
    use crate::ir::Reg;

    /// What the first `v_readfirstlane_b32` of `v1` in `words` finds: by the first proof, and
    /// by it and [`holds`] together (`VICTORY_UNIFORM_STRICT_ALL=1`).
    fn both(words: &[u32]) -> (bool, bool) {
        let code: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        let p = crate::ir::build(&code, true, 16).expect("ir");
        let st = crate::structure::structure(&p).expect("structure");
        for (b, block) in p.blocks.iter().enumerate() {
            for (i, inst) in block.insts.iter().enumerate() {
                if inst.name() == "v_readfirstlane_b32" {
                    let x = inst.read(Reg::V(1)).expect("source read");
                    let first = value_uniform(&p, &st, b, x, &mut HashSet::new()) == Uniform::Yes;
                    return (first, first && holds(&p, &st, b, i, x));
                }
            }
        }
        panic!("no v_readfirstlane_b32");
    }

    #[test]
    fn a_value_set_with_every_lane_running_stays_uniform() {
        let w = [
            0x7E02_0281, // v_mov_b32 v1, 1
            0x7E04_0501, // v_readfirstlane_b32 s2, v1
            0xBF81_0000, // s_endpgm
        ];
        assert_eq!(both(&w), (true, true));
    }

    /// A count each lane carries out of a loop it leaves on its own: lane 0 leaves after one
    /// round with 1, lane 32 after 32 rounds with 32. The upper host half must not read its
    /// own first lane.
    #[test]
    fn a_count_out_of_a_loop_each_lane_leaves_is_not_uniform() {
        let w = [
            0x7E02_0280, // v_mov_b32 v1, 0
            0xBE80_047E, // s_mov_b64 s[0:1], exec
            0x4A02_0281, // loop: v_add_nc_u32 v1, 1, v1
            0x7D82_0101, // v_cmp_lt_u32 vcc, v1, v0
            0x87FE_6A7E, // s_and_b64 exec, exec, vcc
            0xBF89_FFFC, // s_cbranch_execnz loop
            0xBEFE_0400, // s_mov_b64 exec, s[0:1]
            0x7E04_0501, // v_readfirstlane_b32 s2, v1
            0xBF81_0000, // s_endpgm
        ];
        assert_eq!(both(&w), (true, false));
    }

    /// A value written under a guard and read under it: a host half with no lane in the guard
    /// reads a lane that kept the register's old value.
    #[test]
    fn a_value_written_under_the_read_s_guard_is_not_uniform() {
        let w = [
            0xBE80_047E, // s_mov_b64 s[0:1], exec
            0x7DAA_0080, // v_cmpx_ne_u32 exec, 0, v0
            0x7E02_0281, // v_mov_b32 v1, 1
            0x7E04_0501, // v_readfirstlane_b32 s2, v1
            0xBEFE_0400, // s_mov_b64 exec, s[0:1]
            0xBF81_0000, // s_endpgm
        ];
        assert_eq!(both(&w), (true, false));
    }

    /// `v_cndmask_b32` picks per lane by VCC (the first proof finds this too).
    #[test]
    fn a_pick_by_a_lane_mask_is_not_uniform() {
        let w = [
            0x7E04_0281, // v_mov_b32 v2, 1
            0x7D84_0080, // v_cmp_eq_u32 vcc, 0, v0
            0x0202_0480, // v_cndmask_b32 v1, 0, v2, vcc
            0x7E04_0501, // v_readfirstlane_b32 s2, v1
            0xBF81_0000, // s_endpgm
        ];
        assert!(!both(&w).1);
    }
}
