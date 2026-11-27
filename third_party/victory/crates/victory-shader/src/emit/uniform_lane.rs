//! Wave-uniform value analysis for first-lane reads across split wave64 halves.

use std::collections::HashSet;

use crate::gfx10::{Fields, Operand};
use crate::ir::{Inst, Program, Reg, Value};
use crate::structure::{Bool, Structured};

/// Whether a register value is uniform across all lanes of its console wave.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Uniform {
    Yes,
    No,
    /// Only reachable through the value being checked (a loop back edge).
    Cycle,
}

fn lds_stable(p: &Program, at: usize) -> bool {
    p.blocks.iter().skip(at).all(|b| {
        b.insts
            .iter()
            .all(|i| !i.name().starts_with("ds_") || i.name().starts_with("ds_read"))
    })
}

/// Whether every lane reaching `b` brings its phis' values along one path:
/// one edge from outside a loop and at most one back edge. At a join of
/// per-lane branches each lane takes the value of its own predecessor, so
/// uniform arguments do not make a uniform phi.
fn single_path(p: &Program, b: usize) -> bool {
    let at = p.blocks[b].offset;
    let back = p.blocks[b].preds.iter().filter(|&&q| p.blocks[q].offset >= at).count();
    p.blocks[b].preds.len() - back == 1 && back <= 1
}

fn guard(st: &Structured, b: usize, i: usize) -> Option<Bool> {
    st.guards.get(b).and_then(|g| g.get(i)).copied().flatten()
}

/// Whether instruction `i` of block `b` can leave a lane of the first-lane
/// read in block `at` out: a lane it skips keeps its old value of what it
/// writes. It cannot when every lane running the read also meets its guard.
fn skips_reader(p: &Program, st: &Structured, at: usize, b: usize, i: usize) -> bool {
    let Some(def) = guard(st, b, i) else {
        return false;
    };
    let reader = p.blocks.get(at).and_then(|block| {
        block
            .insts
            .iter()
            .position(|inst| inst.name() == "v_readfirstlane_b32")
            .map(|j| guard(st, at, j))
    });
    match reader {
        Some(Some(read)) => st.bools.meet(&[(read, true), (def, false)]),
        _ => true,
    }
}

fn operand_uniform(
    p: &Program,
    st: &Structured,
    at: usize,
    inst: &Inst,
    op: &Operand,
    path: &mut HashSet<Value>,
) -> Uniform {
    match *op {
        Operand::Vector { reg, .. } => inst
            .read(Reg::V(reg))
            .map_or(Uniform::No, |x| value_uniform(p, st, at, x, path)),
        Operand::Scalar { code, .. } => inst
            .read(Reg::S(code))
            .filter(|x| st.masks.contains_key(x) || st.highs.contains_key(x))
            .map_or(Uniform::Yes, |_| Uniform::No),
        _ => Uniform::Yes,
    }
}

/// Whether `v` is the same in every lane of its console wave in the
/// surrounding control flow: constants, scalar reads, and lane operations
/// whose inputs all are, through loop-carried phis. An LDS read at a
/// uniform address counts only while no later LDS write can intervene:
/// every lane asks for the same dword, but both halves must observe the
/// same write (the shader uses this for a wave's running scan).
pub(super) fn value_uniform(
    p: &Program,
    st: &Structured,
    at: usize,
    v: Value,
    path: &mut HashSet<Value>,
) -> Uniform {
    if !path.insert(v) {
        return Uniform::Cycle;
    }
    let mut result = Uniform::No;
    'find: {
        for (b, block) in p.blocks.iter().enumerate() {
            for phi in &block.phis {
                if phi.value != v {
                    continue;
                }
                if !single_path(p, b) {
                    result = Uniform::No;
                    break 'find;
                }
                let (mut yes, mut cycle) = (false, false);
                for &a in &phi.args {
                    match value_uniform(p, st, at, a, path) {
                        Uniform::No => {
                            result = Uniform::No;
                            break 'find;
                        }
                        Uniform::Yes => yes = true,
                        Uniform::Cycle => cycle = true,
                    }
                }
                // A loop's back edge alone is not a seed; at least one
                // incoming value must come from outside the cycle.
                result = if yes {
                    Uniform::Yes
                } else if cycle {
                    Uniform::Cycle
                } else {
                    Uniform::Yes
                };
                break 'find;
            }
            for (i, inst) in block.insts.iter().enumerate() {
                if !inst.writes.iter().any(|&(_, w)| w == v) {
                    continue;
                }
                if skips_reader(p, st, at, b, i) {
                    result = Uniform::No;
                    break 'find;
                }
                let name = inst.name();
                if name.starts_with("ds_read") {
                    if !lds_stable(p, at) {
                        result = Uniform::No;
                        break 'find;
                    }
                    let Fields::Ds { addr, .. } = &inst.fields else {
                        result = Uniform::No;
                        break 'find;
                    };
                    result = addr
                        .as_ref()
                        .map_or(Uniform::Yes, |a| operand_uniform(p, st, at, inst, a, path));
                    break 'find;
                }
                if name.starts_with("v_mbcnt")
                    || name.contains("lane")
                    || name.starts_with("v_cmpx")
                {
                    result = Uniform::No;
                    break 'find;
                }
                let Fields::Valu(val) = &inst.fields else {
                    result = Uniform::No;
                    break 'find;
                };
                if val.dpp.is_some() || val.sdwa.is_some() {
                    result = Uniform::No;
                    break 'find;
                }
                let mut yes = false;
                let mut cycle = false;
                for op in &val.src {
                    match operand_uniform(p, st, at, inst, op, path) {
                        Uniform::No => {
                            result = Uniform::No;
                            break 'find;
                        }
                        Uniform::Yes => yes = true,
                        Uniform::Cycle => cycle = true,
                    }
                }
                result = if yes {
                    Uniform::Yes
                } else if cycle {
                    Uniform::Cycle
                } else {
                    Uniform::Yes
                };
                break 'find;
            }
        }
    }
    path.remove(&v);
    result
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::{Uniform, value_uniform};
    use crate::ir::Reg;

    fn first_lane_source(words: &[u32]) -> Uniform {
        let code: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        let p = crate::ir::build(&code, true, 8).expect("ir");
        let st = crate::structure::structure(&p).expect("structure");
        for (b, block) in p.blocks.iter().enumerate() {
            for inst in &block.insts {
                if inst.name() == "v_readfirstlane_b32" {
                    let x = inst.read(Reg::V(1)).expect("v1 read");
                    return value_uniform(&p, &st, b, x, &mut HashSet::new());
                }
            }
        }
        panic!("no v_readfirstlane_b32");
    }

    #[test]
    fn a_value_set_in_a_lane_branch_is_not_uniform_after_the_join() {
        // Lane 0 keeps 0 and every other lane takes 1: the console's first
        // lane reads 0, but the upper wave32 half's first lane holds 1, so
        // that half must not read its own first lane.
        let words = [
            0x7E02_0280, // v_mov_b32 v1, 0
            0xBE80_047E, // s_mov_b64 s[0:1], exec
            0x7DAA_0080, // v_cmpx_ne_u32 exec, 0, v0
            0xBF88_0001, // s_cbranch_execz +1
            0x7E02_0281, // v_mov_b32 v1, 1
            0xBEFE_0400, // s_mov_b64 exec, s[0:1]
            0x7E04_0501, // v_readfirstlane_b32 s2, v1
            0xBF81_0000, // s_endpgm
        ];
        assert_eq!(first_lane_source(&words), Uniform::No);
    }

    #[test]
    fn a_value_set_with_every_lane_running_stays_uniform() {
        let words = [
            0x7E02_0281, // v_mov_b32 v1, 1
            0x7E04_0501, // v_readfirstlane_b32 s2, v1
            0xBF81_0000, // s_endpgm
        ];
        assert_eq!(first_lane_source(&words), Uniform::Yes);
    }
}
