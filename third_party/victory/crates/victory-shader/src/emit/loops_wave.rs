//! Loops whose closing and leaving branches each host wave takes together
//! (`VICTORY_LOOPS_WAVE=1`), for console wave64 compute programs on a host of 32-lane waves.
//!
//! Ported from readmet3xt-lab/foxhound `9f8838b109` (`loops_wave`, `emit_program` in its
//! `emit/mod.rs`, `loop_branches` in its `emit/lower.rs`, `structure_wide_host` in its
//! `structure.rs`), GPL-3.0.

use std::collections::HashSet;

use super::{HostShader, lower};
use crate::ir::{Exit, Program};
use crate::native::{HwStage, NativeProgram};
use crate::resource::Resources;
use crate::structure::{self, Atom, LaneExit, Structured};
use crate::{ShaderError, resource};

/// What a translation gives: the program, and the wave size it requires of the host.
type Emitted = Result<(HostShader, Option<u32>), ShaderError>;

/// `VICTORY_LOOPS_WAVE=1`: console wave64 compute programs on a host of 32-lane waves are
/// asked for [`again`]; off by default. A test can choose for its own thread.
fn on() -> bool {
    #[cfg(test)]
    if let Some(on) = TEST_LOOPS_WAVE.get() {
        return on;
    }
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("VICTORY_LOOPS_WAVE").is_ok_and(|v| v.trim() == "1"))
}

#[cfg(test)]
thread_local! {
    /// A test's choice for [`on`], for its thread only.
    pub(super) static TEST_LOOPS_WAVE: std::cell::Cell<Option<bool>> =
        const { std::cell::Cell::new(None) };
}

/// `first`, the plain translation of `p`; a program that came out depending on the host's
/// waves is translated once more (`emit`) with the per-lane branches that close or leave its
/// loops taken by each host wave together ([`structure::structure_wide_host`]), and that
/// translation is the one given if it needs no wider waves.
///
/// Why: the lowering turns the console's wave loops into loops each lane leaves on its own,
/// and takes the code behind a loop as code the whole wave runs together. On NVIDIA the lanes
/// that left a loop early run on as a wave of their own, so a ballot, a first-lane read or an
/// exchange between the halves of a console wave behind the loop saw a part of the wave.
/// foxhound saw it in `water_shading:CS_TransmissionVolumetrics_GenerateTiles` (the game,
/// build 1.0.2.1, RTX 4070 SUPER): the tiles with local lights never counted themselves in
/// (the lanes without one arrived later and wrote "no lane" over their ballot), their list
/// stayed empty and those tiles of the water kept no light: dark rectangles beside lit
/// objects.
pub(super) fn again(
    first: Emitted,
    native: &NativeProgram,
    p: &Program,
    st: &Structured,
    emit: impl FnOnce(&Structured, &Resources) -> Emitted,
) -> Emitted {
    let Ok((_, Some(wave))) = &first else {
        return first;
    };
    let wave = *wave;
    if native.stage != HwStage::Cs
        || !native.wave64
        || lower::host_max_wave() != 32
        || !on()
        || !st.host_wide.is_empty()
    {
        return first;
    }
    let loops = loop_branches(p, st);
    if loops.is_empty() {
        return first;
    }
    let needs = lower::take_needs();
    let again = structure::structure_wide_host(p, &st.wide, &loops).and_then(|st| {
        let res = resource::track(p, &native.layout, native.stage, Some(&st))?;
        emit(&st, &res)
    });
    let why = std::env::var_os("VICTORY_WAVE_WHY").is_some();
    match again {
        Ok((shader, Some(w))) if w <= wave => {
            if why {
                eprintln!(
                    "WAVE-LOOPS {} loop branches taken by each host wave together",
                    loops.len()
                );
            }
            lower::take_needs();
            Ok((shader, Some(w)))
        }
        other => {
            if why {
                let what = match &other {
                    Ok((_, w)) => format!("wave {w:?}"),
                    Err(e) => e.to_string(),
                };
                eprintln!(
                    "WAVE-LOOPS not taken ({} loop branches): {what}",
                    loops.len()
                );
            }
            lower::set_needs(needs);
            first
        }
    }
}

/// The per-lane branches (by block) that close a loop or leave one, whose wave-wide meaning
/// is known ([`Structured::wave_any`]): the ones a host wave takes together
/// ([`structure::structure_wide_host`]) so that its lanes stay one wave behind the loop. Not
/// the branches the wave takes together already, nor those on scalar conditions.
pub(super) fn loop_branches(p: &Program, st: &Structured) -> HashSet<usize> {
    let n = p.blocks.len();
    let mut rpo = vec![usize::MAX; n];
    for (i, b) in crate::ir::reverse_postorder(&p.blocks)
        .into_iter()
        .enumerate()
    {
        rpo[b] = i;
    }
    let succ = |b: usize| -> Vec<usize> {
        match p.blocks[b].exit {
            Exit::Jump(t) => vec![t],
            Exit::Branch { taken, not_taken } => vec![taken, not_taken],
            _ => Vec::new(),
        }
    };
    let mut out = HashSet::new();
    for latch in (0..n).filter(|&b| rpo[b] != usize::MAX) {
        for head in succ(latch).into_iter().filter(|&h| rpo[h] <= rpo[latch]) {
            // the loop's blocks: the head, and what reaches the latch without passing it
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
                        .filter(|&q| rpo[q] != usize::MAX),
                );
            }
            for b in (0..n).filter(|&b| body[b]) {
                let Some(&LaneExit::Branch {
                    when,
                    taken,
                    not_taken,
                }) = st.exits.get(b)
                else {
                    continue;
                };
                let leaves = !body[taken] || !body[not_taken] || b == latch;
                let per_lane = !st.wide.contains(&b)
                    && !st
                        .bools
                        .support(when)
                        .iter()
                        .all(|a| matches!(a, Atom::Uniform(_)));
                if leaves && per_lane && st.wave_any.get(b).copied().flatten().is_some() {
                    out.insert(b);
                }
            }
        }
    }
    out
}

#[cfg(all(test, windows))]
mod tests {
    use super::super::wave64_tests::native;
    use super::super::{emit_program, emit_program_plain, lower};
    use super::{TEST_LOOPS_WAVE, loop_branches};
    use crate::native::NativeProgram;
    use crate::{io, ir, resource, structure};

    /// A loop each lane of a console wave of 64 leaves on its own (lane t after t + 1 rounds),
    /// then, with every lane back, the count of lanes below each (`v_mbcnt`) stored.
    fn looping() -> NativeProgram {
        native(&[
            0x7E02_0300, // v_mov_b32 v1, v0
            0x4A02_02C1, // loop: v_add_nc_u32 v1, -1, v1
            0x7D02_0280, // v_cmp_lt_i32 vcc, 0, v1
            0x87FE_6A7E, // s_and_b64 exec, exec, vcc
            0xBF89_FFFC, // s_cbranch_execnz loop
            0xBEFE_04C1, // s_mov_b64 exec, -1
            0xD765_0002, // v_mbcnt_lo_u32_b32 v2, -1, 0
            0x0001_00C1,
            0xE070_0000, // buffer_store_dword v2, off, s[8:11], 0
            0x8002_0200,
        ])
    }

    #[test]
    fn a_loop_each_lane_leaves_has_its_branch_taken_by_the_host_wave() {
        let native = looping();
        let p = ir::build(&native.code, true, 16).unwrap();
        let st = structure::structure(&p).unwrap();
        let loops = loop_branches(&p, &st);
        assert_eq!(loops.len(), 1);
        let again = structure::structure_wide_host(&p, &st.wide, &loops).unwrap();
        assert_eq!(again.host_wide, loops);
        assert!(loops.iter().all(|b| again.wide.contains(b)));
    }

    /// Off, the translation is the plain one; on, the program on a host of 32-lane waves gets
    /// its loop taken wave by wave, which differs, at no wider waves.
    #[test]
    fn the_switch_takes_the_loop_wave_by_wave() {
        let native = looping();
        let p = ir::build(&native.code, true, 16).unwrap();
        let st = structure::structure(&p).unwrap();
        let res = resource::track(&p, &native.layout, native.stage, Some(&st)).unwrap();
        let io = io::raise(&p, &native).unwrap();
        let bytes = crate::dxbc::build(&[]);
        let c = super::super::Container::parse(&bytes).unwrap();
        let wave = lower::TEST_HOST_WAVE.replace(Some(32));
        let translate = |on: bool| {
            let was = TEST_LOOPS_WAVE.replace(Some(on));
            let r = emit_program(&c, &native, &p, &st, &io, &res);
            TEST_LOOPS_WAVE.set(was);
            r.unwrap()
        };
        let (off, on) = (translate(false), translate(true));
        let plain = emit_program_plain(&c, &native, &p, &st, &io, &res).unwrap();
        lower::TEST_HOST_WAVE.set(wave);
        assert_eq!(off.0.dxil, plain.0.dxil);
        assert_eq!(off.1, plain.1);
        assert!(plain.1.is_some());
        assert!(on.1 <= plain.1);
        assert_ne!(on.0.dxil, plain.0.dxil);
    }
}
