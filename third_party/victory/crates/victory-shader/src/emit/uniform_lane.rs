//! Wave-uniform value analysis for first-lane reads across split wave64 halves.

use std::collections::HashSet;

use crate::gfx10::{Fields, Operand};
use crate::ir::{Inst, Program, Reg, Value};
use crate::structure::Structured;

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
        for block in &p.blocks {
            for phi in &block.phis {
                if phi.value != v {
                    continue;
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
            for inst in &block.insts {
                if !inst.writes.iter().any(|&(_, w)| w == v) {
                    continue;
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
