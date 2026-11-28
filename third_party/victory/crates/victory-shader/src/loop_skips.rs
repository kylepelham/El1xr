//! Skips around scalar-tested loops kept in the wave-wide forms (`VICTORY_WAVE_LOOP_SKIPS=1`).
//!
//! [`crate::ir::without_skips`] leaves out a branch that only skips code no lane is left for,
//! and the wave-wide lowering (`emit::widen`) then walks that code with EXEC clear. Where the
//! code is a loop that goes round on a scalar test (`s_cmp_*`, not a lane mask), the console
//! never runs it with no lane left; walked with EXEC clear, its bounds are whatever a
//! first-lane read found in the registers of lanes that never wrote them, and it goes round
//! until the iteration limit (`LOOP_LIMIT`, 4,096 rounds of group barriers in a compute
//! program), where every thread of the group leaves the program. Inside [`with_kept`],
//! such a skip stays: a branch the wave takes together, as on the console.
//!
//! Ported from trustno1-coder/VIctory-NVIDIA PR #326 (wike1701, `0b881e0`, `loops_kept` in its
//! `ir.rs` `without_skips`), GPL-3.0.

use std::cell::Cell;

use crate::ir::{Program, Reg};

thread_local! {
    static KEPT: Cell<bool> = const { Cell::new(false) };
}

/// `VICTORY_WAVE_LOOP_SKIPS=1` (off by default): the wave-wide forms are tried first on the
/// program with these skips kept.
pub(crate) fn on() -> bool {
    std::env::var("VICTORY_WAVE_LOOP_SKIPS").is_ok_and(|v| v.trim() == "1")
}

/// `f` with [`keeps`] in force on this thread.
pub(crate) fn with_kept<T>(f: impl FnOnce() -> T) -> T {
    struct Reset(bool);
    impl Drop for Reset {
        fn drop(&mut self) {
            KEPT.set(self.0);
        }
    }
    let _reset = Reset(KEPT.replace(true));
    f()
}

/// Inside [`with_kept`]: whether the region `inside` (blocks a skip leaves out) holds a block
/// whose last instruction branches on a scalar test back round to itself within the region.
pub(crate) fn keeps(p: &Program, inside: &[bool]) -> bool {
    KEPT.get() && scalar_loop(p, inside)
}

fn scalar_loop(p: &Program, inside: &[bool]) -> bool {
    let n = p.blocks.len();
    (0..n).filter(|&x| inside[x]).any(|x| {
        let Some(last) = p.blocks[x].insts.last() else {
            return false;
        };
        if !matches!(last.name(), "s_cbranch_scc0" | "s_cbranch_scc1") {
            return false;
        }
        let scalar = last.read(Reg::Scc).is_none_or(|scc| {
            p.blocks
                .iter()
                .flat_map(|b| &b.insts)
                .find(|i| i.writes.iter().any(|w| w.1 == scc))
                .is_none_or(|i| i.name().starts_with("s_cmp_") || i.name().starts_with("s_bitcmp"))
        });
        if !scalar {
            return false;
        }
        // (round: the branch leads back to its own block inside the region)
        let mut seen = vec![false; n];
        let mut stack = p.blocks[x].exit.successors();
        while let Some(y) = stack.pop() {
            if y == x {
                return true;
            }
            if y >= n || !inside[y] || std::mem::replace(&mut seen[y], true) {
                continue;
            }
            stack.extend(p.blocks[y].exit.successors());
        }
        false
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{build, without_skips};

    /// `if (v0 < s0) { for (s8 = 0; s8 < s1; s8++) v1 = s8; }`, or with `vector` the loop
    /// closed on a lane mask instead (`s_cbranch_execnz`).
    fn skipped_loop(vector: bool) -> Program {
        let close = if vector { 0xBF89_FFFC } else { 0xBF85_FFFC }; // execnz / scc1, to the loop's top
        let words = [
            0xBE94_0306u32, // s_mov_b32 s20, s6
            0x7D88_0000,    // v_cmp_gt_u32 vcc, s0, v0
            0xBE8C_246A,    // s_and_saveexec_b64 s[12:13], vcc
            0xBF88_0005,    // s_cbranch_execz past the loop
            0xBE88_0380,    // s_mov_b32 s8, 0
            0x7E02_0208,    // v_mov_b32 v1, s8
            0x8008_8108,    // s_add_u32 s8, s8, 1
            0xBF0A_0108,    // s_cmp_lt_u32 s8, s1
            close,
            0xBEFE_040C, // s_mov_b64 exec, s[12:13]
            0xBF81_0000, // s_endpgm
        ];
        let code: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        build(&code, true, 256).unwrap()
    }

    fn skips(p: &Program) -> usize {
        p.blocks
            .iter()
            .filter(|b| {
                b.insts
                    .last()
                    .is_some_and(|i| i.name() == "s_cbranch_execz")
            })
            .count()
    }

    #[test]
    fn a_skip_around_a_scalar_loop_stays_only_when_kept() {
        let p = skipped_loop(false);
        assert_eq!(skips(&p), 1);
        let out = without_skips(&p, true).expect("left out without the switch");
        assert_eq!(skips(&out), 0);
        assert!(with_kept(|| without_skips(&p, true)).is_none());
        // (the choice ends with `with_kept`)
        assert!(without_skips(&p, true).is_some());
    }

    #[test]
    fn a_loop_on_a_lane_mask_is_left_out_either_way() {
        let p = skipped_loop(true);
        assert_eq!(skips(&p), 1);
        let kept = with_kept(|| without_skips(&p, true));
        assert_eq!(skips(&kept.expect("left out")), 0);
    }
}
