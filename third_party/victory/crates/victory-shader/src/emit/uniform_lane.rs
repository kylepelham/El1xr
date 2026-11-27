//! Wave-uniform value analysis for first-lane reads across split wave64 halves.

use std::collections::HashSet;

use crate::gfx10::{Fields, Operand};
use crate::ir::{Inst, Program, Reg, Value};
use crate::structure::{Bool, LaneExit, Structured};

/// Whether a register value is uniform across all lanes of its console wave.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Uniform {
    Yes,
    No,
    /// Only reachable through the value being checked (a loop back edge).
    Cycle,
}

/// `VICTORY_LDS_UNIFORM_STRICT=1`: an LDS read counts as the same in both
/// host halves of a console wave only when every lane reads one address
/// and no LDS write can land between the halves' reads ([`lds_quiet`]).
/// Off: [`lds_stable`] alone, as before (it looks only at the blocks from
/// the first-lane read on: a write some lanes made before the read, in an
/// earlier block, was let through).
fn strict() -> bool {
    #[cfg(test)]
    if let Some(on) = TEST_STRICT.get() {
        return on;
    }
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_LDS_UNIFORM_STRICT").is_some_and(|v| v == "1"))
}

#[cfg(test)]
thread_local! {
    pub(super) static TEST_STRICT: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

/// Whether `inst` changes LDS: every DS instruction but the reads and the
/// cross-lane ones that do not touch memory, and buffer loads into LDS.
fn writes_lds(inst: &Inst) -> bool {
    match &inst.fields {
        Fields::Ds { .. } => {
            let name = inst.name();
            !(name.starts_with("ds_read") || name.starts_with("ds_swizzle") || name.contains("permute"))
        }
        Fields::Buffer { lds, .. } => *lds,
        _ => false,
    }
}

/// The instructions a lane can run after (`forward`) or before instruction
/// `i` of block `b` without passing one for which `stop` holds, as (block,
/// instruction); the `stop` ones are left out.
fn region(
    p: &Program,
    b: usize,
    i: usize,
    forward: bool,
    stop: &dyn Fn(usize, &Inst) -> bool,
) -> Vec<(usize, usize)> {
    let mut found = Vec::new();
    let mut entered = HashSet::new();
    let mut work = Vec::new();
    // Each item: a block and the instructions of it to walk, in order.
    let first: Vec<usize> = if forward {
        (i + 1..p.blocks[b].insts.len()).collect()
    } else {
        (0..i).rev().collect()
    };
    work.push((b, first));
    while let Some((q, order)) = work.pop() {
        let mut through = true;
        for j in order {
            if stop(q, &p.blocks[q].insts[j]) {
                through = false;
                break;
            }
            found.push((q, j));
        }
        if !through {
            continue;
        }
        let next = if forward { p.blocks[q].exit.successors() } else { p.blocks[q].preds.clone() };
        for n in next {
            if entered.insert(n) {
                let len = p.blocks[n].insts.len();
                let order = if forward { (0..len).collect() } else { (0..len).rev().collect() };
                work.push((n, order));
            }
        }
    }
    found
}

/// Whether a write to LDS leaves the same bytes whether one host half runs
/// it or both do: a plain store that every lane runs (`full`: its block is
/// one every lane runs), of one value to one address.
fn same_from_both_halves(
    p: &Program,
    st: &Structured,
    q: usize,
    j: usize,
    full: bool,
    path: &mut HashSet<Value>,
) -> bool {
    let inst = &p.blocks[q].insts[j];
    let name = inst.name();
    if !full || guard(st, q, j).is_some() || !name.starts_with("ds_write") || name.contains("addtid") {
        return false;
    }
    let Fields::Ds { addr: Some(addr), data, gds: false, .. } = &inst.fields else {
        return false;
    };
    std::iter::once(addr).chain(data.iter()).all(|op| match *op {
        Operand::Vector { reg, dwords } => (0..dwords.max(1)).all(|d| {
            inst.read(Reg::V(reg.wrapping_add(d)))
                .is_some_and(|x| value_uniform(p, st, q, x, path) == Uniform::Yes)
        }),
        _ => operand_uniform(p, st, q, inst, op, path) == Uniform::Yes,
    })
}

/// Whether no LDS write can land between the two host halves' runs of the
/// LDS read, instruction `i` of block `b`: the halves run apart up to a
/// group barrier, so a write either half makes on the way to the read, or
/// after it, can be seen by one half's read and not by the other's. None
/// after the read; before it, at most one, and one that writes the same
/// bytes whichever half runs it ([`same_from_both_halves`]). A barrier
/// counts where the translation keeps it a group barrier: in a block every
/// lane runs, in a program no lane can leave early (see `lower.rs`'s
/// `fence_only`), with group sync on.
fn lds_quiet(p: &Program, st: &Structured, b: usize, i: usize, path: &mut HashSet<Value>) -> bool {
    let all = super::all_lane_blocks(st);
    let full = |q: usize| all.as_ref().is_some_and(|a| a.get(q).copied().unwrap_or(false));
    let cut_short = st.exits.iter().enumerate().any(|(q, e)| {
        matches!(*e, LaneExit::Jump(t) if p.blocks[t].offset <= p.blocks[q].offset)
    });
    let sync = super::super::group_sync() != 0;
    let stop = |q: usize, inst: &Inst| inst.name() == "s_barrier" && sync && !cut_short && full(q);
    let after = region(p, b, i, true, &stop);
    if after.iter().any(|&(q, j)| writes_lds(&p.blocks[q].insts[j])) {
        return false;
    }
    let before: Vec<(usize, usize)> = region(p, b, i, false, &stop)
        .into_iter()
        .filter(|&(q, j)| writes_lds(&p.blocks[q].insts[j]))
        .collect();
    match before[..] {
        [] => true,
        [(q, j)] => same_from_both_halves(p, st, q, j, full(q), path),
        _ => false,
    }
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
                    let Fields::Ds { addr, gds, .. } = &inst.fields else {
                        result = Uniform::No;
                        break 'find;
                    };
                    // Every lane reads one address (`ds_read_addtid` has
                    // none: each lane reads its own dword), and nothing
                    // writes there between the halves' reads.
                    if strict()
                        && (addr.is_none() || *gds || name.contains("addtid") || !lds_quiet(p, st, b, i, path))
                    {
                        result = Uniform::No;
                        break 'find;
                    }
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
        first_lane_of(words, 1)
    }

    fn first_lane_of(words: &[u32], reg: u8) -> Uniform {
        let code: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        let p = crate::ir::build(&code, true, 16).expect("ir");
        let st = crate::structure::structure(&p).expect("structure");
        for (b, block) in p.blocks.iter().enumerate() {
            for inst in &block.insts {
                if inst.name() == "v_readfirstlane_b32" {
                    let x = inst.read(Reg::V(reg)).expect("source read");
                    return value_uniform(&p, &st, b, x, &mut HashSet::new());
                }
            }
        }
        panic!("no v_readfirstlane_b32");
    }

    /// What the first-lane read of `v1` in `words` finds without and with
    /// `VICTORY_LDS_UNIFORM_STRICT`.
    fn both(words: &[u32]) -> (Uniform, Uniform) {
        let old = super::TEST_STRICT.replace(Some(false));
        let before = first_lane_source(words);
        super::TEST_STRICT.set(Some(true));
        let after = first_lane_source(words);
        super::TEST_STRICT.set(old);
        (before, after)
    }

    // Machine words for the LDS cases. Every lane reads LDS at v2 = 0.
    const ADDR_ZERO: u32 = 0x7E04_0280; // v_mov_b32 v2, 0
    const DATA_FIVE: u32 = 0x7E06_0285; // v_mov_b32 v3, 5
    const DATA_LANE: u32 = 0x7E06_0300; // v_mov_b32 v3, v0 (the lane's own)
    const WRITE: [u32; 2] = [0xD834_0000, 0x0000_0302]; // ds_write_b32 v2, v3
    const ADD: [u32; 2] = [0xD800_0000, 0x0000_0302]; // ds_add_u32 v2, v3
    const READ: [u32; 2] = [0xD8D8_0000, 0x0100_0002]; // ds_read_b32 v1, v2
    const SAVE: u32 = 0xBE80_047E; // s_mov_b64 s[0:1], exec
    const NOT_LANE0: u32 = 0x7DAA_0080; // v_cmpx_ne_u32 exec, 0, v0
    const RESTORE: u32 = 0xBEFE_0400; // s_mov_b64 exec, s[0:1]
    const NEXT: u32 = 0xBF82_0000; // s_branch +0 (a new block, every lane in it)
    const BARRIER: u32 = 0xBF8A_0000; // s_barrier
    const FIRST: u32 = 0x7E04_0501; // v_readfirstlane_b32 s2, v1
    const END: u32 = 0xBF81_0000; // s_endpgm

    fn words(parts: &[&[u32]]) -> Vec<u32> {
        parts.concat()
    }

    /// No LDS write anywhere: the read is the same in both halves.
    #[test]
    fn an_lds_read_with_no_write_stays_uniform() {
        let w = words(&[&[ADDR_ZERO], &READ, &[FIRST, END]]);
        assert_eq!(both(&w), (Uniform::Yes, Uniform::Yes));
    }

    /// Lanes 1-63 write the dword in a branch before the read: the low host
    /// half may read before the high half's write lands. The old rule let
    /// it through (the write is in a block before the first-lane read's).
    #[test]
    fn a_write_some_lanes_make_before_the_read_is_not_uniform() {
        let w = words(&[
            &[ADDR_ZERO, DATA_FIVE, SAVE, NOT_LANE0, 0xBF88_0002], // s_cbranch_execz over the write
            &WRITE,
            &[RESTORE],
            &READ,
            &[FIRST, END],
        ]);
        assert_eq!(both(&w), (Uniform::Yes, Uniform::No));
    }

    /// The same write with a group barrier between it and the read: both
    /// halves read after it.
    #[test]
    fn a_barrier_after_a_partial_write_keeps_the_read_uniform() {
        let w = words(&[
            &[ADDR_ZERO, DATA_FIVE, SAVE, NOT_LANE0, 0xBF88_0002],
            &WRITE,
            &[RESTORE, BARRIER],
            &READ,
            &[FIRST, END],
        ]);
        assert_eq!(both(&w), (Uniform::Yes, Uniform::Yes));
    }

    /// A barrier with group sync off is no barrier.
    #[test]
    fn a_barrier_without_group_sync_does_not_count() {
        let w = words(&[
            &[ADDR_ZERO, DATA_FIVE, SAVE, NOT_LANE0, 0xBF88_0002],
            &WRITE,
            &[RESTORE, BARRIER],
            &READ,
            &[FIRST, END],
        ]);
        let old = crate::emit::TEST_GROUP_SYNC.replace(Some(0));
        let got = both(&w);
        crate::emit::TEST_GROUP_SYNC.set(old);
        assert_eq!(got, (Uniform::Yes, Uniform::No));
    }

    /// Every lane writes one value to one address before the read: the
    /// second half's write leaves the bytes as the first half's did.
    #[test]
    fn one_write_of_one_value_by_every_lane_keeps_the_read_uniform() {
        let w = words(&[&[ADDR_ZERO, DATA_FIVE], &WRITE, &[NEXT], &READ, &[FIRST, END]]);
        assert_eq!(both(&w), (Uniform::Yes, Uniform::Yes));
    }

    /// Every lane writes its own value: which one is left depends on which
    /// half wrote last.
    #[test]
    fn a_write_of_each_lane_s_value_is_not_uniform() {
        let w = words(&[&[ADDR_ZERO, DATA_LANE], &WRITE, &[NEXT], &READ, &[FIRST, END]]);
        assert_eq!(both(&w), (Uniform::Yes, Uniform::No));
    }

    /// Two writes before the read: the high half's first can land between
    /// the low half's second and its read.
    #[test]
    fn two_writes_before_the_read_are_not_uniform() {
        let w = words(&[
            &[ADDR_ZERO, DATA_FIVE],
            &WRITE,
            &WRITE,
            &[NEXT],
            &READ,
            &[FIRST, END],
        ]);
        assert_eq!(both(&w), (Uniform::Yes, Uniform::No));
    }

    /// An atomic adds once per lane: the low half's read sees half the sum.
    #[test]
    fn an_atomic_before_the_read_is_not_uniform() {
        let w = words(&[&[ADDR_ZERO, DATA_FIVE], &ADD, &[NEXT], &READ, &[FIRST, END]]);
        assert_eq!(both(&w), (Uniform::Yes, Uniform::No));
    }

    /// A write after the read (before the first-lane read's block): the
    /// late half's read can see it.
    #[test]
    fn a_write_after_the_read_is_not_uniform() {
        let w = words(&[&[ADDR_ZERO, DATA_FIVE], &READ, &WRITE, &[NEXT, FIRST, END]]);
        assert_eq!(both(&w), (Uniform::Yes, Uniform::No));
    }

    /// `ds_read_addtid_b32` reads each lane's own dword.
    #[test]
    fn a_read_by_lane_index_is_not_uniform() {
        let w = [0xDAC4_0000, 0x0100_0000, FIRST, END]; // ds_read_addtid_b32 v1
        assert_eq!(both(&w).1, Uniform::No);
    }

    /// The VItoriaBr scan (`wave64_tests::a_first_lane_read_of_a_uniform_scan_carries_across_the_halves`)
    /// writes no LDS: the strict rule finds what the old one does.
    #[test]
    fn the_uniform_scan_is_unchanged_by_the_strict_rule() {
        let w = [
            0x2C0E_0086, 0x7E10_0280, 0xBE80_047E, 0x7DAA_0E80, 0xBF88_0010, 0xBE92_0380, 0xBE91_0380,
            0xBE86_047E, 0xBF82_0000, 0x7E22_0508, 0x8F10_8212, 0x8112_8112, 0x7E10_0210, 0xD8D8_0000,
            0x0800_0008, 0x4A10_1011, 0x7DA4_0E12, 0x8A86_7E06, 0xBF85_FFF6, 0xE070_0000, 0x8002_0800,
            0xBF81_0000,
        ];
        let old = super::TEST_STRICT.replace(Some(false));
        let before = first_lane_of(&w, 8);
        super::TEST_STRICT.set(Some(true));
        let after = first_lane_of(&w, 8);
        super::TEST_STRICT.set(old);
        assert_eq!(before, after);
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
