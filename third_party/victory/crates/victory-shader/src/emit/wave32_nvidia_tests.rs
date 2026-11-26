//! The NVIDIA Wave32 lowering ([`super::nvidia`], `VICTORY_WAVE32_NVIDIA=1`, from
//! codec-d/cool_fox 8cc31ab3): each piece off by default (the translation is what it was),
//! and on where the console program needs it. Lowered only, on a host of 32-lane waves:
//! the pieces change what translates, not what a translated program computes on a device.

use std::collections::HashSet;

use super::nvidia;
use super::wave64_tests::{lowered_on, native};
use crate::{ir, structure};

/// "Some lane is left" of a mask operation as a number: the SCC of `s_and_b64 .., exec` read
/// by `s_cselect` (an upscaler waits for other groups in a loop that ends when every lane
/// saw its flag, or after enough rounds).
fn scc_select_program() -> crate::native::NativeProgram {
    native(&[
        0xD765_0001,
        0x0001_00C1, // v_mbcnt_lo_u32_b32 v1, -1, 0
        0xD766_0001,
        0x0002_02C1, // v_mbcnt_hi_u32_b32 v1, -1, v1
        0x7D84_0200, // v_cmp_eq_u32 vcc, s0, v1
        0x8792_7E6A, // s_and_b64 s[18:19], vcc, exec
        0x8592_80C1, // s_cselect_b64 s[18:19], -1, 0
        0xBF09_8501, // s_cmp_ge_u32 s1, 5
        0x8594_80C1, // s_cselect_b64 s[20:21], -1, 0
        0x8B92_1214, // s_orn2_b64 s[18:19], s[20:21], s[18:19]
        0x8506_8081, // s_cselect_b32 s6, 1, 0
        0x7E04_0206, // v_mov_b32 v2, s6
        0xE070_0000,
        0x8002_0200, // buffer_store_dword v2, off, s[8:11], 0
    ])
}

fn program_of(native: &crate::native::NativeProgram) -> ir::Program {
    ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build")
}

#[test]
fn a_scalar_select_on_the_scc_of_a_mask_operation_reads_a_mask_as_data_only_with_the_switch() {
    let native = scc_select_program();
    let p = program_of(&native);
    let st = structure::structure(&p).expect("structure");
    let select = p
        .blocks
        .iter()
        .flat_map(|b| &b.insts)
        .find(|i| i.name() == "s_cselect_b32")
        .expect("the 32-bit select");
    // (its SCC is the compare of s1 and 5, no mask operation's: only the 64-bit select that
    // follows the AND of the mask reads the SCC of a mask operation)
    let mask_select = p
        .blocks
        .iter()
        .flat_map(|b| &b.insts)
        .find(|i| {
            i.name() == "s_cselect_b64"
                && i.read(crate::ir::Reg::Scc)
                    .is_some_and(|s| st.scc_some.contains_key(&s))
        })
        .expect("the select on the mask operation's SCC");
    nvidia::with(false, || {
        assert!(!super::lower::reads_mask_as_data(&st, mask_select));
    });
    nvidia::with(true, || {
        assert!(super::lower::reads_mask_as_data(&st, mask_select));
        // (a select on another SCC stays as it was)
        assert_eq!(
            super::lower::reads_mask_as_data(&st, select),
            nvidia::with(false, || super::lower::reads_mask_as_data(&st, select))
        );
    });
}

#[test]
fn a_program_that_reads_the_scc_of_a_mask_operation_translates_only_with_the_switch() {
    let native = scc_select_program();
    let p = program_of(&native);
    for host_wave in [32, 64] {
        let off = nvidia::with(false, || {
            lowered_on(&native, &p, &HashSet::new(), host_wave)
        });
        let why = off.expect_err("refused as it was").to_string();
        assert!(why.contains("a lane mask read as data"), "{why}");
        let on = nvidia::with(true, || lowered_on(&native, &p, &HashSet::new(), host_wave));
        assert!(on.is_ok(), "host wave {host_wave}: {:?}", on.err());
    }
}

/// A loop of one block: its latch `s_andn2_wrexec_b64` (EXEC to the unfinished lanes), then
/// `branch` back to its top; block 1 is the loop, block 2 what follows.
fn loop_with(branch: u32) -> ir::Program {
    let words = [
        0xBE82_047E, // s_mov_b64 s[2:3], exec
        0x7E02_0281, // 4: v_mov_b32 v1, 1
        0xBE82_3A02, // s_andn2_wrexec_b64 s[2:3], s[2:3]
        branch,      // back to 4
        0xBF81_0000, // s_endpgm
    ];
    let code: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    ir::build(&code, true, 16).expect("ir::build")
}

#[test]
fn only_the_active_lane_loop_latch_is_widened_for_a_driver_compute_program() {
    let back_on_scc = loop_with(0xBF85_FFFD); // s_cbranch_scc1
    assert_eq!(back_on_scc.blocks.len(), 3);
    let none = HashSet::new();
    assert!(super::active_loop_latch(&back_on_scc, &none, &[1]));
    // more than the latch, or something widened before: left alone
    assert!(!super::active_loop_latch(&back_on_scc, &none, &[1, 2]));
    assert!(!super::active_loop_latch(&back_on_scc, &none, &[]));
    assert!(!super::active_loop_latch(
        &back_on_scc,
        &HashSet::from([0]),
        &[1]
    ));
    // the other sense of the branch is another loop
    let back_on_scc0 = loop_with(0xBF84_FFFD); // s_cbranch_scc0
    assert!(!super::active_loop_latch(&back_on_scc0, &none, &[1]));
    // no EXEC update before the branch: a plain loop
    let words = [
        0xBE82_047E,
        0x7E02_0281,
        0xBF09_8001,
        0xBF85_FFFD,
        0xBF81_0000,
    ];
    let code: Vec<u8> = words.iter().flat_map(|w: &u32| w.to_le_bytes()).collect();
    let plain = ir::build(&code, true, 16).expect("ir::build");
    assert!(!super::active_loop_latch(&plain, &none, &[1]));
}

#[test]
fn a_shared_memory_dword_at_a_uniform_address_is_uniform_only_with_the_switch() {
    let words = [
        0x7E02_0280u32, // v_mov_b32 v1, 0
        0xD8D8_0000,
        0x0200_0001, // ds_read_b32 v2, v1
        0xBF81_0000, // s_endpgm
    ];
    let code: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    let p = ir::build(&code, true, 16).expect("ir::build");
    let st = structure::structure(&p).expect("structure");
    let insts: Vec<_> = p.blocks.iter().flat_map(|b| &b.insts).collect();
    let (mov, read) = (insts[0].writes[0].1, insts[1].writes[0].1);
    let all = vec![true; p.blocks.len()];
    let off = super::lower::lane_uniform(&p, &st, &all, false);
    assert!(off.contains(&mov) && !off.contains(&read));
    let on = super::lower::lane_uniform(&p, &st, &all, true);
    assert!(on.contains(&mov) && on.contains(&read));
}
