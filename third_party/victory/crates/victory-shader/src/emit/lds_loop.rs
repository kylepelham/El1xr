//! LDS atomics in loops that need no meeting of the host waves (`VICTORY_LDS_LOOP_ATOMICS=1`).
//!
//! Ported from readmet3xt-lab/foxhound `9f8838b109` (`LdsOrder::looped` in its `emit/lower.rs`),
//! GPL-3.0; foxhound's analysis is taken as it is, only the `looped` part of its `lds_order`.

use std::collections::HashSet;

use crate::gfx10::Fields;
use crate::ir::Program;
use crate::structure::{LaneExit, Structured};

/// Whether LDS atomics in loops go without a meeting of the host waves ([`looped_atomics`]):
/// `VICTORY_LDS_LOOP_ATOMICS=1`; off by default (they meet, as before). A test can choose for
/// its own thread.
pub(super) fn loop_atomics_on() -> bool {
    #[cfg(test)]
    if let Some(on) = TEST_LOOP_ATOMICS.get() {
        return on;
    }
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("VICTORY_LDS_LOOP_ATOMICS").is_ok_and(|v| v.trim() == "1"))
}

#[cfg(test)]
thread_local! {
    /// A test's choice for [`loop_atomics_on`], for its thread only.
    pub(super) static TEST_LOOP_ATOMICS: std::cell::Cell<Option<bool>> =
        const { std::cell::Cell::new(None) };
}

/// The LDS atomics (block, instruction) of `p` that stand in a loop which no plain LDS write or
/// read leads into or stands in: they get a fence instead of a meeting of the host waves.
///
/// A meeting is a group barrier, and the host waves take a loop each on its own: one that
/// makes fewer rounds stands at the barrier behind the loop while the others stand at the one
/// inside it, the host lets them all go on, and the first reads what the others have not
/// written yet. foxhound saw it in `CS_SortRayTiles` (the cloud pass, 256 threads; RTX 4070
/// SUPER): its counting and scattering loops take 15 entries a thread, the last threads none;
/// of the 1,068 tiles of the sorted class 971 were left, and a tile not in the list gets no
/// ray (a square of clear sky in a cloud for a second or two after a cut).
///
/// Without the meeting the atomic's order against the accesses around the loop is kept by
/// their own meetings and the title's barriers: what comes before the loop is nothing but
/// atomics back to a barrier, and what comes behind it meets first.
///
/// The program is cut at its `s_barrier`s into pieces joined by the edges between blocks; an
/// atomic's piece must lie on a cycle, and every access in it and in the pieces that reach it
/// must be an atomic.
pub(super) fn looped_atomics(p: &Program, st: &Structured) -> HashSet<(usize, usize)> {
    #[derive(Clone, Copy, PartialEq)]
    enum Kind {
        Read,
        Write,
        Atomic,
    }
    let n = st.exits.len().min(p.blocks.len());
    // pieces: per block, the barriers' places; piece k of block b lies after its k-th barrier
    let mut first = vec![0usize; n + 1];
    for b in 0..n {
        let barriers = p.blocks[b]
            .insts
            .iter()
            .filter(|i| i.name() == "s_barrier")
            .count();
        first[b + 1] = first[b] + barriers + 1;
    }
    let pieces = first[n];
    let mut succ: Vec<Vec<usize>> = vec![Vec::new(); pieces];
    let mut accesses: Vec<Vec<(usize, usize, Kind)>> = vec![Vec::new(); pieces];
    for b in 0..n {
        let mut k = first[b];
        for (i, inst) in p.blocks[b].insts.iter().enumerate() {
            let name = inst.name();
            if name == "s_barrier" {
                k += 1;
                continue;
            }
            if !matches!(inst.fields, Fields::Ds { gds: false, .. })
                || name.starts_with("ds_swizzle")
                || name.contains("permute")
            {
                continue;
            }
            let kind = if name.starts_with("ds_read") {
                Kind::Read
            } else if name.starts_with("ds_write") {
                Kind::Write
            } else {
                Kind::Atomic
            };
            accesses[k].push((b, i, kind));
        }
        let last = first[b + 1] - 1;
        let to: Vec<usize> = match st.exits[b] {
            LaneExit::Jump(t) => vec![t],
            LaneExit::Branch {
                taken, not_taken, ..
            } => vec![taken, not_taken],
            LaneExit::End => Vec::new(),
            // (anywhere: every access keeps its meeting)
            LaneExit::Indirect => return HashSet::new(),
        };
        succ[last].extend(to.into_iter().filter(|&t| t < n).map(|t| first[t]));
    }
    let mut pred: Vec<Vec<usize>> = vec![Vec::new(); pieces];
    for (a, s) in succ.iter().enumerate() {
        for &t in s {
            pred[t].push(a);
        }
    }
    // the pieces reachable from each and reaching it, itself only on a cycle
    let reach = |edges: &Vec<Vec<usize>>, from: usize| -> Vec<bool> {
        let mut seen = vec![false; pieces];
        let mut stack = edges[from].clone();
        while let Some(x) = stack.pop() {
            if !seen[x] {
                seen[x] = true;
                stack.extend(edges[x].iter().copied());
            }
        }
        seen
    };
    let mut out = HashSet::new();
    for k in 0..pieces {
        if accesses[k].is_empty() {
            continue;
        }
        let (fwd, bwd) = (reach(&succ, k), reach(&pred, k));
        // (on a cycle, and nothing but atomics in it and in what leads to it)
        if fwd[k]
            && (0..pieces)
                .filter(|&x| x == k || bwd[x])
                .flat_map(|x| accesses[x].iter())
                .all(|a| a.2 == Kind::Atomic)
        {
            out.extend(accesses[k].iter().map(|a| (a.0, a.1)));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::super::{Lowered, Parts, TEST_HOST_WAVE, lower};
    use super::{TEST_LOOP_ATOMICS, looped_atomics};
    use crate::native::{ComputeIo, HwStage, NativeProgram, RootParam, RootType, StageIo};
    use crate::{io, ir, resource, structure};

    const S_ENDPGM: u32 = 0xBF81_0000;

    /// A wave64 group of 128 threads with 64 bytes of LDS: v1 = 1, v2 = 0, a swizzle, then four
    /// rounds of a scalar loop whose body is `body` (LDS accesses of v1 at address v2).
    fn looping(body: &[u32]) -> NativeProgram {
        let mut words = vec![
            0x7E02_0281, // v_mov_b32 v1, 1
            0x7E04_0280, // v_mov_b32 v2, 0
            0xBE84_0384, // s_mov_b32 s4, 4
            // ds_swizzle_b32 v4, v1 (lane code within 32-lane halves: host waves of 32)
            0x36 << 26 | 0x35 << 18,
            4 << 24 | 1,
        ];
        words.extend_from_slice(body);
        words.push(0x8084_8104); // s_sub_u32 s4, s4, 1
        words.push(0xBF07_8004); // s_cmp_lg_u32 s4, 0
        // s_cbranch_scc1 back to the body
        words.push(0xBF85_0000 | (0x1_0000 - (body.len() as u32 + 3)));
        words.push(S_ENDPGM);
        let code: Vec<u8> = words.into_iter().flat_map(u32::to_le_bytes).collect();
        NativeProgram {
            stage: HwStage::Cs,
            wave64: true,
            vgprs: 16,
            layout: vec![RootParam {
                root: RootType::Constants,
                table_offset_dwords: 0,
                sgprs: 2,
                base_register: 0,
                user_data: 0,
                slot: 0,
            }],
            used: vec![],
            io: StageIo::Cs(ComputeIo {
                rsrc2: 1 << 15 | 2 << 1,
                threads: [128, 1, 1],
            }),
            code,
        }
    }

    const DS_ADD: [u32; 2] = [0x36 << 26, 1 << 8 | 2]; // ds_add_u32 v2, v1
    const DS_READ: [u32; 2] = [0x36 << 26 | 0x36 << 18, 3 << 24 | 2]; // ds_read_b32 v3, v2

    fn looped(native: &NativeProgram) -> usize {
        let p = ir::build(&native.code, true, 16).unwrap();
        let st = structure::structure(&p).unwrap();
        looped_atomics(&p, &st).len()
    }

    fn lowered(native: &NativeProgram, on: bool) -> Lowered {
        let p = ir::build(&native.code, true, 16).unwrap();
        let st = structure::structure(&p).unwrap();
        let res = resource::track(&p, &native.layout, native.stage, Some(&st)).unwrap();
        let io = io::raise(&p, native).unwrap();
        let wave = TEST_HOST_WAVE.replace(Some(32));
        let was = TEST_LOOP_ATOMICS.replace(Some(on));
        let lowered = lower(&Parts {
            native,
            p: &p,
            st: &st,
            io: &io,
            res: &res,
            inputs: vec![],
            outputs: vec![],
            prims: vec![],
            mesh_stride: 0,
        });
        TEST_LOOP_ATOMICS.set(was);
        TEST_HOST_WAVE.set(wave);
        lowered.expect("a wave64 group of 128 in host waves of 32")
    }

    #[test]
    fn an_atomic_alone_in_a_loop_needs_no_meeting() {
        assert_eq!(looped(&looping(&DS_ADD)), 1);
    }

    #[test]
    fn an_atomic_in_a_loop_with_a_read_keeps_its_meeting() {
        let mut body = DS_ADD.to_vec();
        body.extend_from_slice(&DS_READ);
        assert_eq!(looped(&looping(&body)), 0);
    }

    #[test]
    fn an_atomic_behind_a_read_before_the_loop_keeps_its_meeting() {
        let native = looping(&DS_ADD);
        let mut words: Vec<u32> = native
            .code
            .chunks(4)
            .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
            .collect();
        words.splice(0..0, DS_READ);
        let code = words.into_iter().flat_map(u32::to_le_bytes).collect();
        assert_eq!(looped(&NativeProgram { code, ..native }), 0);
    }

    /// `VICTORY_LDS_LOOP_ATOMICS=1` takes the group barrier from the atomic in the loop (a
    /// fence stays); off, the translation keeps it.
    #[test]
    fn the_switch_takes_the_group_barrier_from_a_looped_atomic() {
        let native = looping(&DS_ADD);
        let off = lowered(&native, false);
        let on = lowered(&native, true);
        assert_eq!(off.wave_size, Some(32));
        assert_eq!(on.wave_size, Some(32));
        assert_eq!(off.group_barriers, on.group_barriers + 1);
        assert_ne!(off.dxil, on.dxil);
    }
}
