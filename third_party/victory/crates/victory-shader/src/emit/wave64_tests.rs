//! A console wave64 compute program on a host of 32-lane waves: a wave reduction inside a
//! per-lane branch, with that branch taken wave-wide ([`crate::structure::structure_wide`])
//! so that the two host waves can exchange their halves inside it; and programs in the
//! paired form (one host wave for each console wave, two lanes a thread: `super::paired`),
//! whose LDS, lane reads and masks cross the halves without barriers, checked against what
//! the console's 64 lanes compute. Lowered, validated by `dxil.dll` when present, and run on
//! the host GPU when it takes waves of 32 (WARP's are 4 lanes: neither form runs there).

use std::collections::HashSet;

use super::host::{Dispatch, Host, Root, View};
use super::{HostShader, lower, validator};
use crate::native::{
    ComputeIo, HwStage, NativeProgram, ResourceType, RootParam, RootType, StageIo, UsedResource,
};
use crate::{ShaderError, io, ir, resource, structure};

const S_ENDPGM: u32 = 0xBF81_0000;

/// 64 threads in one console wave. Each thread t holds the bit `1 << (t >> 1)`; the threads
/// below s0 (a root constant) OR theirs over the wave inside `if (t < s0) { ... }` and store
/// the result to dword 0 of the raw UAV.
/// `skip`: the branch past the body when no thread is below s0: `s_cbranch_execz`, or
/// `s_cbranch_scc0` on the SCC `s_and_saveexec` sets ("some lane is left").
fn program(skip: u32) -> NativeProgram {
    let mut words = vec![
        0x7E0A_0281, // v_mov_b32 v5, 1
        0x2C0C_0081, // v_lshrrev_b32 v6, 1, v0
        0x340E_0B06, // v_lshlrev_b32 v7, v6, v5
        0x7D88_0000, // v_cmp_gt_u32 vcc, s0, v0
        0xBE8C_246A, // s_and_saveexec_b64 s[12:13], vcc
        skip | 23,   // the branch to the restore of EXEC
    ];
    words.extend_from_slice(&super::wave::WAVE_OR_V7_TO_S7);
    words.extend_from_slice(&[
        0x7E00_0207, // v_mov_b32 v0, s7
        0x38 << 26 | 0x1C << 18,
        0x80 << 24 | 2 << 16, // buffer_store_dword v0, off, s[8:11], 0
        0xBEFE_040C,          // s_mov_b64 exec, s[12:13]
    ]);
    native(&words)
}

/// A wave64 compute program of 64 threads (one console wave; the thread index in v0) with 4
/// root constants in s0..s3 and a raw UAV's descriptor in s[8:11], then `body`.
pub(super) fn native(body: &[u32]) -> NativeProgram {
    native_of(body, true, 64)
}

/// The same for a console of 32-lane waves when `wave64` is not set, with `threads` threads.
pub(super) fn native_of(body: &[u32], wave64: bool, threads: u32) -> NativeProgram {
    native_with(body, wave64, threads, 0)
}

/// [`native_of`] with `lds` bytes of LDS (512-byte granules).
fn native_with(body: &[u32], wave64: bool, threads: u32, lds: u32) -> NativeProgram {
    let mut words = vec![
        0xBE85_0380, // s_mov_b32 s5, 0
        0x3D << 26 | 0x02 << 18 | 8 << 6 | 2,
        125 << 25, // s_load_dwordx4 s[8:11], s[4:5], 0
    ];
    words.extend_from_slice(body);
    words.push(S_ENDPGM);
    NativeProgram {
        stage: HwStage::Cs,
        wave64,
        vgprs: 16,
        layout: vec![
            RootParam {
                root: RootType::Constants,
                table_offset_dwords: 0,
                sgprs: 4,
                base_register: 0,
                user_data: 0,
                slot: 0,
            },
            RootParam {
                root: RootType::Table,
                table_offset_dwords: 0,
                sgprs: 1,
                base_register: 0,
                user_data: 4,
                slot: 1,
            },
        ],
        used: vec![UsedResource {
            kind: ResourceType::Uav,
            root: RootType::Table,
            table_offset_dwords: 0,
            lower_bound: 0,
            stride: 0,
            dwords: 4,
            user_data: 4,
            slot: 1,
        }],
        io: StageIo::Cs(ComputeIo {
            rsrc2: 5 << 1 | lds.div_ceil(512) << 15, // 5 user SGPRs: s0..s4
            threads: [threads, 1, 1],
        }),
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    }
}

/// `program()` for a host of 32-lane waves, with the branches closing `wide` taken wave-wide.
fn lowered(
    native: &NativeProgram,
    p: &ir::Program,
    wide: &HashSet<usize>,
) -> Result<HostShader, ShaderError> {
    lowered_on(native, p, wide, 32)
}

/// [`lowered`] for a host whose waves have at most `host_wave` lanes.
pub(super) fn lowered_on(
    native: &NativeProgram,
    p: &ir::Program,
    wide: &HashSet<usize>,
    host_wave: u32,
) -> Result<HostShader, ShaderError> {
    let st = structure::structure_wide(p, wide).expect("structure");
    let res = resource::track(p, &native.layout, native.stage, Some(&st)).expect("resource::track");
    let io = io::raise(p, native).expect("io::raise");
    let was = lower::TEST_HOST_WAVE.replace(Some(host_wave));
    let lowered = lower::lower(&lower::Parts {
        native,
        p,
        st: &st,
        io: &io,
        res: &res,
        inputs: Vec::new(),
        outputs: Vec::new(),
        prims: Vec::new(),
        mesh_stride: 0,
    });
    lower::TEST_HOST_WAVE.set(was);
    let lowered = lowered?;
    Ok(HostShader {
        stage: HwStage::Cs,
        dxil: lowered.dxil,
        user_data: lowered.user_data,
        samplers: lowered.samplers,
        static_samplers: Vec::new(),
        root_cbvs: lowered.root_cbvs,
        root_srvs: lowered.root_srvs,
        root_uavs: lowered.root_uavs,
        root_indexed_views: res.root_indexed_views,
        half_views: lowered.half_views,
    })
}

/// 64 threads in one console wave: thread t holds the 16-bit float with bits 0x3C00 + 37 t
/// (1.0 and up, with odd mantissas, so that additions round); all of them store the wave's sum
/// (`WaveActiveSum` of halves) to dword 0. With the console's sum (its tree) and the terms'
/// sum added in a row, which differs from it.
fn half_sum_program() -> (NativeProgram, u16, u16) {
    let mut words = vec![
        0x340C_0085, // v_lshlrev_b32 v6, 5, v0
        0x340E_0082, // v_lshlrev_b32 v7, 2, v0
        0x4A0E_0F06, // v_add_nc_u32 v7, v6, v7
        0x4A0E_0F00, // v_add_nc_u32 v7, v0, v7
        0x4A0E_0EFF,
        0x0000_3C00, // v_add_nc_u32 v7, 0x3c00, v7
    ];
    // (the wave OR's words with v_add_f16 for v_or_b32: opcode 50 for 28)
    words.extend(super::wave::WAVE_OR_V7_TO_S7.iter().map(|&w| {
        if w >> 25 == 28 {
            w & 0x01FF_FFFF | 50 << 25
        } else {
            w
        }
    }));
    words.extend_from_slice(&[
        0x7E00_0207, // v_mov_b32 v0, s7
        0x38 << 26 | 0x1C << 18,
        0x80 << 24 | 2 << 16, // buffer_store_dword v0, off, s[8:11], 0
    ]);
    // the console's sum: lanes 2k and 2k+1, then pairs, ... each addition rounded to 16 bits
    // (to nearest, ties to even); values here are normal numbers
    let wide =
        |h: u16| f32::from_bits((u32::from(h >> 10) + 112) << 23 | u32::from(h & 0x3FF) << 13);
    let narrow = |f: f32| {
        let b = f.to_bits();
        let h = ((b >> 23) - 112) << 10 | (b >> 13 & 0x3FF);
        let rest = b & 0x1FFF;
        (h + u32::from(rest > 0x1000 || rest == 0x1000 && h & 1 == 1)) as u16
    };
    let add = |a: u16, b: u16| narrow(wide(a) + wide(b));
    let terms: Vec<u16> = (0..64u16).map(|t| 0x3C00 + 37 * t).collect();
    let mut tree = terms.clone();
    while tree.len() > 1 {
        tree = tree.chunks(2).map(|c| add(c[0], c[1])).collect();
    }
    let in_a_row = terms.iter().copied().reduce(add).unwrap();
    (native(&words), tree[0], in_a_row)
}

#[test]
fn a_sum_of_halves_over_the_wave_is_the_consoles_tree_bit_for_bit() {
    let (native, tree, in_a_row) = half_sum_program();
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    assert_eq!(
        p.blocks
            .iter()
            .flat_map(|b| &b.insts)
            .filter(|i| i.name() == "v_add_f16")
            .count(),
        6
    );
    let shader = lowered(&native, &p, &HashSet::new()).expect("lower");
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        let mut copy = shader.dxil.clone();
        v.validate(&mut copy).expect("DXIL validation failed");
    }
    assert_ne!(
        tree, in_a_row,
        "these terms tell the tree from another order"
    );
    let _one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let Ok(host) = Host::new(false) else {
        eprintln!("no SM 6.6 hardware device; not run");
        return;
    };
    let (root, pso) = match host.compute_pipeline(&shader) {
        Ok(x) => x,
        Err(e) => {
            eprintln!(
                "the hardware device does not take this program (waves of 32?): {e}; not run"
            );
            return;
        }
    };
    let ud = vec![0u32; shader.user_data_dwords().max(1) as usize];
    let out = host
        .dispatch(
            &root,
            &pso,
            &Dispatch {
                roots: vec![(0, Root::Constants(ud))],
                views: &[View::RawUav(1)],
                samplers: &[],
                groups: [1, 1, 1],
            },
        )
        .unwrap_or_else(|e| panic!("dispatch failed: {e}"));
    // (the high half is lane 0's term's: zero)
    assert_eq!(
        out[0][0],
        u32::from(tree),
        "tree {tree:#x}, in a row {in_a_row:#x}"
    );
    eprintln!("ran on the hardware device: the sum {tree:#x} of the console's tree");
}

/// The console's 16-bit wave sum where the lanes at or past s0 have left the program (a
/// thread past the end of the data): on the console they add +0, and after each step of the
/// tree they hold their run's partial sum all the same. On a host of 64-lane waves (AMD) the
/// lanes that are here read each partner run's sum from a lane of it that is here.
#[test]
fn a_sum_of_halves_where_lanes_have_left_is_the_consoles_tree_bit_for_bit() {
    let mut words = vec![
        0x7DA8_0000, // v_cmpx_gt_u32 exec, s0, v0: the lanes below s0 stay
        0,           // s_cbranch_execz to the end (filled in below)
        0x340C_0085, // v_lshlrev_b32 v6, 5, v0
        0x340E_0082, // v_lshlrev_b32 v7, 2, v0
        0x4A0E_0F06, // v_add_nc_u32 v7, v6, v7
        0x4A0E_0F00, // v_add_nc_u32 v7, v0, v7
        0x4A0E_0EFF,
        0x0000_3C00, // v_add_nc_u32 v7, 0x3c00, v7
    ];
    words.extend(super::wave::WAVE_OR_V7_TO_S7.iter().map(|&w| {
        if w >> 25 == 28 {
            w & 0x01FF_FFFF | 50 << 25
        } else {
            w
        }
    }));
    words.extend_from_slice(&[
        0x7E00_0207, // v_mov_b32 v0, s7
        0x38 << 26 | 0x1C << 18,
        0x80 << 24 | 2 << 16, // buffer_store_dword v0, off, s[8:11], 0
    ]);
    // past every word after it, to the program's end
    words[1] = 0xBF88_0000 | (words.len() as u32 - 2);
    let native = native(&words);
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let shader = lowered_on(&native, &p, &HashSet::new(), 64).expect("lower");
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        let mut copy = shader.dxil.clone();
        v.validate(&mut copy).expect("DXIL validation failed");
    }
    let wide = |h: u16| {
        if h == 0 {
            0.0
        } else {
            f32::from_bits((u32::from(h >> 10) + 112) << 23 | u32::from(h & 0x3FF) << 13)
        }
    };
    let narrow = |f: f32| {
        if f == 0.0 {
            return 0;
        }
        let b = f.to_bits();
        let h = ((b >> 23) - 112) << 10 | (b >> 13 & 0x3FF);
        let rest = b & 0x1FFF;
        (h + u32::from(rest > 0x1000 || rest == 0x1000 && h & 1 == 1)) as u16
    };
    let add = |a: u16, b: u16| narrow(wide(a) + wide(b));
    let _one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let mut ran = false;
    for stay in [64u32, 63, 40, 33, 32, 17, 5, 1] {
        let terms: Vec<u16> = (0..64u32)
            .map(|t| {
                if t < stay {
                    (0x3C00 + 37 * t) as u16
                } else {
                    0
                }
            })
            .collect();
        let mut tree = terms.clone();
        while tree.len() > 1 {
            tree = tree.chunks(2).map(|c| add(c[0], c[1])).collect();
        }
        let Some(got) = on_the_card(&shader, [stay, 0]) else {
            return;
        };
        ran = true;
        // (the high half is lane 0's term's: zero)
        assert_eq!(
            got,
            u32::from(tree[0]),
            "{stay} lanes stay: got {got:#x}, the console's tree {:#x}",
            tree[0]
        );
    }
    if ran {
        eprintln!("ran on the hardware device: the console's tree with lanes gone, in every case");
    }
}

/// One `v_add_f16` whose exact result lies between two halves: 1 + 3/1024 (0x3C03) plus 2
/// (0x4000) is 3 + 1.5 steps of that range, which the console rounds up to 0x4202. The
/// host's `f32tof16` may give 0x4201 (toward zero); with `VICTORY_F16_NEAREST=1` the
/// translation gives the console's whatever the host does.
#[test]
fn a_16_bit_sum_rounds_to_nearest_when_asked_to() {
    let words = [
        0x7E0E_02FF,
        0x0000_3C03, // v_mov_b32 v7, 0x3c03
        0x7E0C_02FF,
        0x0000_4000, // v_mov_b32 v6, 0x4000
        0x6400_0D07, // v_add_f16 v0, v7, v6
        0x38 << 26 | 0x1C << 18,
        0x80 << 24 | 2 << 16, // buffer_store_dword v0, off, s[8:11], 0
    ];
    let native = native(&words);
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    assert!(
        p.blocks
            .iter()
            .flat_map(|b| &b.insts)
            .any(|i| i.name() == "v_add_f16")
    );
    let _one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    for nearest in [false, true] {
        let was = lower::TEST_F16_NEAREST.replace(Some(nearest));
        let shader = lowered(&native, &p, &HashSet::new());
        lower::TEST_F16_NEAREST.set(was);
        let shader = shader.expect("lower");
        if let Ok(mut v) =
            validator::Validator::load(validator::Validator::default_path().as_deref())
        {
            let mut copy = shader.dxil.clone();
            v.validate(&mut copy).expect("DXIL validation failed");
        }
        let Ok(host) = Host::new(false) else {
            eprintln!("no SM 6.6 hardware device; not run");
            return;
        };
        let Ok((root, pso)) = host.compute_pipeline(&shader) else {
            eprintln!("the hardware device does not take this program; not run");
            return;
        };
        let ud = vec![0u32; shader.user_data_dwords().max(1) as usize];
        let out = host
            .dispatch(
                &root,
                &pso,
                &Dispatch {
                    roots: vec![(0, Root::Constants(ud))],
                    views: &[View::RawUav(1)],
                    samplers: &[],
                    groups: [1, 1, 1],
                },
            )
            .unwrap_or_else(|e| panic!("dispatch failed: {e}"));
        let got = out[0][0] & 0xFFFF;
        eprintln!("v_add_f16 0x3c03 + 0x4000 on the hardware device, nearest {nearest}: {got:#x}");
        if nearest {
            assert_eq!(got, 0x4202);
        } else {
            assert!(got == 0x4201 || got == 0x4202, "{got:#x}");
        }
    }
}

#[test]
fn a_wave_or_inside_a_lane_branch_needs_the_branch_wave_wide() {
    for (skip, name) in [
        (0xBF88_0000, "s_cbranch_execz"),
        (0xBF84_0000, "s_cbranch_scc0"),
    ] {
        a_wave_or_inside(skip, name);
    }
}

fn a_wave_or_inside(skip: u32, name: &str) {
    let native = program(skip);
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let branch = (0..p.blocks.len())
        .find(|&b| p.blocks[b].insts.last().is_some_and(|i| i.name() == name))
        .expect("the branch");
    // per lane, the two host waves cannot meet inside the branch
    lower::take_needs();
    let per_lane = lowered(&native, &p, &HashSet::new());
    assert!(
        matches!(&per_lane, Err(ShaderError::Unsupported { what, .. }) if *what == "a wave reduction where not every lane runs"),
        "{:?}",
        per_lane.err()
    );
    // ... and the lowering names the block, around which that branch is the one to widen
    let needs = lower::take_needs();
    let st = structure::structure(&p).expect("structure");
    assert_eq!(lower::branches_around(&st, &needs), Some(vec![branch]));
    // (taken, past the body, when every lane's bit is clear)
    assert_eq!(st.wave_any[branch], Some(false));
    // wave-wide, every lane runs the reduction (EXEC stays a guard on what it does there)
    let wide = HashSet::from([branch]);
    let st = structure::structure_wide(&p, &wide).expect("structure");
    let inside = match p.blocks[branch].exit {
        ir::Exit::Branch { not_taken, .. } => not_taken,
        _ => unreachable!(),
    };
    assert!(st.guards[inside].iter().any(Option::is_some));
    let shader = lowered(&native, &p, &wide).expect("lower");
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        let mut copy = shader.dxil.clone();
        v.validate(&mut copy).expect("DXIL validation failed");
    }
    // On a GPU that takes waves of 32 (two host waves for the console's one): the OR is of
    // both halves, whichever half's lanes take the branch.
    let _one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let Ok(host) = Host::new(false) else {
        eprintln!("no SM 6.6 hardware device; not run");
        return;
    };
    let (root, pso) = match host.compute_pipeline(&shader) {
        Ok(x) => x,
        Err(e) => {
            eprintln!(
                "the hardware device does not take this program (waves of 32?): {e}; not run"
            );
            return;
        }
    };
    // (threads below s0 take the branch; thread t gives bit t >> 1)
    for (below, expected) in [
        (64u32, u32::MAX),
        (40, 0x000F_FFFF),
        (33, 0x0001_FFFF),
        (10, 0x1F),
        (1, 1),
    ] {
        let mut ud = vec![0u32; shader.user_data_dwords().max(1) as usize];
        for u in &shader.user_data {
            if let (RootType::Constants, 0, 0) = (u.root, u.slot, u.dword) {
                ud[u.index as usize] = below;
            }
        }
        let out = host
            .dispatch(
                &root,
                &pso,
                &Dispatch {
                    roots: vec![(0, Root::Constants(ud))],
                    views: &[View::RawUav(1)],
                    samplers: &[],
                    groups: [1, 1, 1],
                },
            )
            .unwrap_or_else(|e| panic!("dispatch failed: {e}"));
        assert_eq!(out[0][0], expected, "{name}: {below} threads in the branch");
    }
    eprintln!("ran on the hardware device ({name}): the OR of both halves in every case");
}

/// Runs `shader` on the hardware device with the root constants `s0`, `s1` and gives dword 0
/// of the UAV; None (and a line) without a device that takes it.
fn on_the_card(shader: &HostShader, constants: [u32; 2]) -> Option<u32> {
    whole_uav(shader, constants, 1).map(|out| out[0])
}

/// As [`on_the_card`], and gives the first `dwords` dwords of the UAV.
fn whole_uav(shader: &HostShader, constants: [u32; 2], dwords: u32) -> Option<Vec<u32>> {
    let Ok(host) = Host::new(false) else {
        eprintln!("no SM 6.6 hardware device; not run");
        return None;
    };
    let Ok((root, pso)) = host.compute_pipeline(shader) else {
        eprintln!("the hardware device does not take this program; not run");
        return None;
    };
    let mut ud = vec![0u32; shader.user_data_dwords().max(1) as usize];
    for u in &shader.user_data {
        if let (RootType::Constants, 0, d @ 0..=1) = (u.root, u.slot, u.dword) {
            ud[u.index as usize] = constants[d as usize];
        }
    }
    let out = host
        .dispatch(
            &root,
            &pso,
            &Dispatch {
                roots: vec![(0, Root::Constants(ud))],
                views: &[View::RawUav(dwords)],
                samplers: &[],
                groups: [1, 1, 1],
            },
        )
        .unwrap_or_else(|e| panic!("dispatch failed: {e}"));
    Some(out[0].clone())
}

/// Each active lane (threads below s0) stores to its own dword in the arm a branch on
/// `s_and_b64 s, vcc, exec` (thread s1's bit) chose: SCC is "some active lane has it", so every
/// active lane takes the same arm and keeps EXEC there (cool_fox #66, test of `bd8b4f524a`).
/// The address is set before EXEC narrows, so a store by an inactive lane would show.
fn stores_in_scc_arms(on_set: bool) -> NativeProgram {
    let (branch, fallthrough, taken) = if on_set {
        (0xBF85_0004, 1, 2) // s_cbranch_scc1
    } else {
        (0xBF84_0004, 2, 1) // s_cbranch_scc0
    };
    native(&[
        0xBF8C_0000, // s_waitcnt 0 for the descriptor
        0x3402_0082, // v_lshlrev_b32 v1, 2, v0
        0x7D88_0000, // v_cmp_gt_u32 vcc, s0, v0
        0xBE8C_246A, // s_and_saveexec_b64 s[12:13], vcc
        0xBF88_000A, // s_cbranch_execz to the restore
        0x7D84_0001, // v_cmp_eq_u32 vcc, s1, v0
        0x8792_7E6A, // s_and_b64 s[18:19], vcc, exec
        branch,
        0x7E04_0280 | fallthrough, // v_mov_b32 v2, fallthrough
        0xE070_1000,
        0x8002_0201, // buffer_store_dword v2, v1, s[8:11], 0 offen
        0xBF82_0003, // s_branch to the restore
        0x7E04_0280 | taken, // v_mov_b32 v2, taken
        0xE070_1000,
        0x8002_0201, // buffer_store_dword v2, v1, s[8:11], 0 offen
        0xBEFE_040C, // s_mov_b64 exec, s[12:13]
    ])
}

/// `VICTORY_LANE_MASK_WIDE=1` ([`crate::lane_split`]): the branch on the mask's SCC is the
/// wave's, so both arms keep EXEC as a guard on their store, and on a host of 32-lane waves every
/// active lane stores the arm the wave chose (2 when thread s1 is active, else 1). Off, the
/// branch is split per lane and one arm's store loses its guard.
#[test]
fn a_branch_on_a_masks_scc_is_the_waves_and_its_arms_keep_their_guards() {
    for on_set in [false, true] {
        let native = stores_in_scc_arms(on_set);
        let p = ir::build(&native.code, native.wave64, native.vgprs).expect("IR");
        let form = ir::without_skips(&p, false).expect("removable EXEC skip");
        let scc = (0..form.blocks.len())
            .find(|&b| form.blocks[b].insts.last().is_some_and(|i| i.name().starts_with("s_cbranch_scc")))
            .expect("the branch on SCC");
        let guarded = |on: bool| {
            let was = crate::lane_split::TEST_LANE_MASK_WIDE.replace(Some(on));
            let st = structure::structure(&form).expect("structure");
            crate::lane_split::TEST_LANE_MASK_WIDE.set(was);
            let mut stores = Vec::new();
            for (block, guards) in form.blocks.iter().zip(&st.guards) {
                for (inst, guard) in block.insts.iter().zip(guards) {
                    if inst.name() == "buffer_store_dword" {
                        stores.push(guard.is_some());
                    }
                }
            }
            (st.wide.contains(&scc), stores)
        };
        assert_eq!(guarded(true), (true, vec![true, true]), "on_set={on_set}");
        let (wide, stores) = guarded(false);
        assert!(!wide && stores.contains(&false), "on_set={on_set}: off, split per lane");

        let was = crate::lane_split::TEST_LANE_MASK_WIDE.replace(Some(true));
        lower::take_needs();
        let shader = lowered(&native, &form, &HashSet::new());
        crate::lane_split::TEST_LANE_MASK_WIDE.set(was);
        let shader = shader.expect("lower");
        if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
            let mut copy = shader.dxil.clone();
            v.validate(&mut copy).expect("DXIL validation failed");
        }
        let _one = super::valu_tests::ONE_AT_A_TIME
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for (below, lane) in [(40u32, 5u32), (40, 36), (64, 63), (10, 50)] {
            let Some(out) = whole_uav(&shader, [below, lane], 64) else {
                return;
            };
            let chosen = if lane < below { 2 } else { 1 };
            let want: Vec<u32> = (0..64).map(|t| if t < below { chosen } else { 0 }).collect();
            assert_eq!(out, want, "on_set={on_set}, {below} lanes active, lane {lane}");
        }
    }
}

/// `ptfx_CS_lightAtlas_Sprite`'s light loop (`2561af9577c47289`) in small: the threads below s0
/// enter it behind `s_cbranch_execz`, count rounds in s2 and v2, and leave on `s_cbranch_vccz`
/// once a vector compare finds the round equal to s1 (v3, which the structure cannot see is the
/// same in every lane); then each stores its count.
fn vcc_closed_loop() -> NativeProgram {
    native(&[
        0xBF8C_0000, // s_waitcnt 0 for the descriptor
        0x3402_0082, // v_lshlrev_b32 v1, 2, v0
        0x7D88_0000, // v_cmp_gt_u32 vcc, s0, v0
        0xBE8C_246A, // s_and_saveexec_b64 s[12:13], vcc
        0xBF88_000A, // s_cbranch_execz to the restore
        0x2C06_0086, // v_lshrrev_b32 v3, 6, v0 (0 in every thread)
        0x4A06_0601, // v_add_nc_u32 v3, s1, v3
        0xBE82_0380, // s_mov_b32 s2, 0
        0x7E04_0280, // v_mov_b32 v2, 0
        0x8002_8102, // s_add_u32 s2, s2, 1 (the loop)
        0x4A04_0481, // v_add_nc_u32 v2, 1, v2
        0x7D84_0602, // v_cmp_eq_u32 vcc, s2, v3
        0xBF86_FFFC, // s_cbranch_vccz to the loop
        0xE070_1000,
        0x8002_0201, // buffer_store_dword v2, v1, s[8:11], 0 offen
        0xBEFE_040C, // s_mov_b64 exec, s[12:13]
    ])
}

/// `VICTORY_LANE_MASK_WIDE=1` takes the loop's closing `s_cbranch_vccz` for the wave. The form
/// that takes it so leaves the entry's skip out and puts the two host waves' exchange of that
/// decision into the loop, where a console wave with no thread below s0 never sets VCC and
/// goes round for ever (batch 5 part J: device removed on `2561af9577c47289`). Such a program
/// keeps the per-lane form, the one the switch off gives; that one ends on the card, with no
/// thread in the loop and with some.
#[test]
fn a_wave_wide_loop_exit_that_puts_a_barrier_in_the_loop_keeps_the_per_lane_form() {
    let native = vcc_closed_loop();
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("IR");
    let bytes = crate::dxbc::build(&[]);
    let c = super::Container::parse(&bytes).expect("container");
    let translate = |on: bool| {
        let was = crate::lane_split::TEST_LANE_MASK_WIDE.replace(Some(on));
        let st = structure::structure(&p).expect("structure");
        let res = resource::track(&p, &native.layout, native.stage, Some(&st)).expect("resource::track");
        let io = io::raise(&p, &native).expect("io::raise");
        let host = lower::TEST_HOST_WAVE.replace(Some(32));
        let r = super::emit(&c, &native, &p, &st, &io, &res);
        lower::TEST_HOST_WAVE.set(host);
        crate::lane_split::TEST_LANE_MASK_WIDE.set(was);
        (st.wide.len(), r.expect("translates"))
    };
    let (wide_off, off) = translate(false);
    let (wide_on, on) = translate(true);
    assert_eq!(wide_off, 0);
    assert!(wide_on > 0, "on, the loop's closing branch is the wave's");
    // the wave-wide form alone: the skip left out, and a barrier of the translator's in the loop
    let form = ir::without_skips(&p, false).expect("removable EXEC skip");
    let was = crate::lane_split::TEST_LANE_MASK_WIDE.replace(Some(true));
    let (wide_form, looped) = crate::lane_split::watch(
        || lowered(&native, &form, &HashSet::new()).map(|s| (s, None)),
        |s: &HostShader| &s.dxil,
    );
    crate::lane_split::TEST_LANE_MASK_WIDE.set(was);
    let wide_form = wide_form.expect("the wave-wide form lowers").0;
    assert!(looped, "the wave-wide form exchanges the loop's decision inside the loop");
    assert_ne!(wide_form.dxil, off.dxil);
    assert_eq!(on.dxil, off.dxil, "on, the program keeps the per-lane form");

    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        let mut copy = on.dxil.clone();
        v.validate(&mut copy).expect("DXIL validation failed");
    }
    let _one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    for (below, rounds) in [(0u32, 3u32), (40, 3), (64, 1), (10, 7)] {
        let Some(out) = whole_uav(&on, [below, rounds], 64) else {
            return;
        };
        let want: Vec<u32> = (0..64).map(|t| if t < below { rounds } else { 0 }).collect();
        assert_eq!(out, want, "{below} threads in the loop, {rounds} rounds");
    }
}

/// A 64-bit mask the program does not track (s[0:1], root constants), read as a number by a
/// select, in a console wave of 64 run as two host waves of 32 (the DPP move makes it take the
/// console's lanes, so the halves form): each lane takes its own bit, lanes 32-63 the high
/// dword's. Their host lanes are 0-31, so with `VICTORY_HALVES_LANE=1` they are numbered by
/// their console lane (PR #108 of trustno1-coder/VIctory-NVIDIA); off, they take the low dword.
#[test]
fn lanes_past_32_take_the_high_dword_of_a_mask_held_as_a_number() {
    let words = [
        0x7E0E_02FA,
        0xFFFF_B100, // v_mov_b32_dpp v7, v0 quad_perm:[1,0,3,2] row_mask:0xf bank_mask:0xf
        0xD501_0005,
        0x0001_0280, // v_cndmask_b32 v5, 0, 1, s[0:1]
        0x340C_0082, // v_lshlrev_b32 v6, 2, v0
        0xE070_1000,
        0x8002_0506, // buffer_store_dword v5, v6, s[8:11], 0 offen
    ];
    let native = native(&words);
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let lower_with = |on: bool| {
        let was = lower::halves_lane::TEST_HALVES_LANE.replace(Some(on));
        let r = lowered_on(&native, &p, &HashSet::new(), 32);
        lower::halves_lane::TEST_HALVES_LANE.set(was);
        r.expect("lower")
    };
    let (off, on) = (lower_with(false), lower_with(true));
    assert_ne!(off.dxil, on.dxil, "the halves form numbers lanes by console lane");
    let _one = super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    for (lo, hi) in [(0x5555_0F0F, 0xA0F0_1234), (u32::MAX, 0), (0, u32::MAX)] {
        let Some(out) = whole_uav(&on, [lo, hi], 64) else {
            return;
        };
        for lane in 0..64u32 {
            let bit = if lane < 32 { (lo >> lane) & 1 } else { (hi >> (lane - 32)) & 1 };
            assert_eq!(out[lane as usize], bit, "lane {lane} of the mask {hi:08x}{lo:08x}");
        }
        let old = whole_uav(&off, [lo, hi], 64).expect("ran once");
        assert!((32..64).all(|l| old[l] == (lo >> (l - 32)) & 1), "off: the low dword's bits");
    }
}

/// A 32-lane console wave. A register holds a lane mask on one path (the lanes below s0)
/// and a number on the other, and is read as a number after the join, as the trace programs
/// keep their lanes' masks and their node numbers on one stack
/// (`deferred_tiled:CS_LaserLightShadowRaytracing`, s46 at 0x104, 0x75c and 0x188). The
/// mask arrives as the ballot of its lanes.
#[test]
fn a_lane_mask_joined_with_a_number_is_read_as_the_number_it_is() {
    let words = [
        0x7D88_0000, // v_cmp_gt_u32 vcc_lo, s0, v0
        0xBE94_036A, // s_mov_b32 s20, vcc_lo
        0xBF06_8001, // s_cmp_eq_u32 s1, 0
        0xBF85_0001, // s_cbranch_scc1 over the next
        0xBE94_0385, // s_mov_b32 s20, 5
        0x7E00_0214, // v_mov_b32 v0, s20
        0x38 << 26 | 0x1C << 18,
        0x80 << 24 | 2 << 16, // buffer_store_dword v0, off, s[8:11], 0
    ];
    let native = native_of(&words, false, 32);
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let shader = lowered(&native, &p, &HashSet::new()).expect("lower");
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        let mut copy = shader.dxil.clone();
        v.validate(&mut copy).expect("DXIL validation failed");
    }
    let _one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    for (below, other, expected) in [
        (32u32, 0u32, u32::MAX),
        (10, 0, 0x3FF),
        (0, 0, 0),
        (10, 1, 5),
    ] {
        let Some(got) = on_the_card(&shader, [below, other]) else {
            return;
        };
        assert_eq!(got, expected, "{below} lanes in the mask, s1 = {other}");
    }
    eprintln!("ran on the hardware device: the mask's bits, or the number");
}

/// A 32-lane console wave. `if (t < s0) { ... SCC = (s1 == 7) }` and a branch on SCC after
/// the join: the skip brings the SCC of `s_and_saveexec` ("some lane is left") along
/// (`rtdirectionalshadows:CS_RTShadowTrace32HQ_TiledFarOnly`, 0x360-0x39c). On the console
/// the skip is taken by the wave, so with some lane inside every lane sees the inner test.
/// A lane that skipped on its own would see "some lane is left" instead: refused per lane,
/// right with the branch taken wave-wide. Lane 31 stores what it saw.
#[test]
fn scc_of_a_mask_operation_joined_with_a_scalar_test_needs_the_skip_wave_wide() {
    let words = [
        0x7D88_0000, // v_cmp_gt_u32 vcc_lo, s0, v0
        0xBE8D_3C6A, // s_and_saveexec_b32 s13, vcc_lo
        0xBF88_0001, // s_cbranch_execz over the next
        0xBF06_8701, // s_cmp_eq_u32 s1, 7
        0xBEFE_030D, // s_mov_b32 exec_lo, s13
        0xBF85_0002, // s_cbranch_scc1 to "seven"
        0x7E0A_0281, // v_mov_b32 v5, 1
        0xBF82_0001, // s_branch over the next
        0x7E0A_0282, // seven: v_mov_b32 v5, 2
        0x7DA4_009F, // v_cmpx_eq_u32 exec_lo, 31, v0
        0x38 << 26 | 0x1C << 18,
        0x80 << 24 | 2 << 16 | 5 << 8, // buffer_store_dword v5, off, s[8:11], 0
    ];
    let native = native_of(&words, false, 32);
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let branch = (0..p.blocks.len())
        .find(|&b| {
            p.blocks[b]
                .insts
                .last()
                .is_some_and(|i| i.name() == "s_cbranch_execz")
        })
        .expect("the skip");
    lower::take_needs();
    let per_lane = lowered(&native, &p, &HashSet::new());
    assert!(
        matches!(&per_lane, Err(ShaderError::Unsupported { what, .. }) if *what == "a lane mask read as data"),
        "{:?}",
        per_lane.err()
    );
    let st = structure::structure(&p).expect("structure");
    assert_eq!(
        lower::branches_around(&st, &lower::take_needs()),
        Some(vec![branch])
    );
    let shader = lowered(&native, &p, &HashSet::from([branch])).expect("lower");
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        let mut copy = shader.dxil.clone();
        v.validate(&mut copy).expect("DXIL validation failed");
    }
    let _one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    // (lanes below s0 inside, s1) -> what lane 31 stores: 2 if SCC was set at the branch
    for (below, s1, expected) in [
        (0u32, 7u32, 1u32),
        (10, 7, 2),
        (10, 3, 1),
        (32, 3, 1),
        (32, 7, 2),
    ] {
        let Some(got) = on_the_card(&shader, [below, s1]) else {
            return;
        };
        assert_eq!(got, expected, "{below} lanes inside, s1 = {s1}");
    }
    eprintln!(
        "ran on the hardware device: the inner test's SCC for every lane once some lane is inside"
    );
}

/// One console wave of 64 threads, thread t holding t + 1. A branch on s0 (the wave's) picks
/// `WaveActiveMax` (s0 not 0) or `WaveActiveMin`, and the two reductions share their end:
/// each arm saves EXEC and reduces up to the read of lane 32, the join reads lane 0 and
/// restores EXEC (`deferred_tiled:CS_WaterTransmittanceOpticalDepthProcessing`, 0xb3c-0xbe0).
/// Neither block holds a whole section; `v_permlanex16_b32` lowered on its own reads the lanes
/// the host has, so the translation is the one with the join's code in each arm
/// ([`ir::whole_sections`]), where each section is whole.
#[test]
fn two_reductions_that_share_their_end_translate_as_two_sections() {
    // an arm: save EXEC, the identity `init`, then `op` over the wave up to lane 32's read
    let arm = |init: u32, op: u32| {
        let mut w = vec![
            0xBE8C_25C1, // s_or_saveexec_b64 s[12:13], -1
            0x7E04_02FF,
            init, // v_mov_b32 v2, init
            0xD501_0002,
            0x0032_0902, // v_cndmask_b32 v2, v2, v4, s[12:13]
        ];
        for dpp in [0xDE54_C102, 0x9360_1202, 0x0009_2402] {
            w.extend([op << 25 | 0x0004_04E9, dpp]); // v_<op>_f32 v2, v2, v2 dpp8:[...]
        }
        w.extend([
            op << 25 | 0x0004_04FA,
            0xFF09_4002, // v_<op>_f32 v2, v2, v2 row_mirror bound_ctrl:1
            0xD778_0003,
            0x0201_0102,            // v_permlanex16_b32 v3, v2, 0, 0
            op << 25 | 0x0004_0503, // v_<op>_f32 v2, v3, v2
            0xD760_000E,
            0x0001_4102,            // v_readlane_b32 s14, v2, 32
            op << 25 | 0x0004_040E, // v_<op>_f32 v2, s14, v2
        ]);
        w
    };
    let mut words = vec![
        0x7E08_0D00, // v_cvt_f32_u32 v4, v0
        0x0608_08F2, // v_add_f32 v4, 1.0, v4
        0xBF06_8000, // s_cmp_eq_u32 s0, 0
        0xBF85_0014, // s_cbranch_scc1 to the minimum
    ];
    words.extend(arm(0xFF80_0000, 0x10)); // the maximum from -inf
    words.push(0xBF82_0013); // s_branch to the join
    words.extend(arm(0x7F80_0000, 0x0F)); // the minimum from +inf
    words.extend([
        0xD760_000E,
        0x0001_0102, // v_readlane_b32 s14, v2, 0
        0xBEFE_040C, // s_mov_b64 exec, s[12:13]
        0x7E00_020E, // v_mov_b32 v0, s14
        0x38 << 26 | 0x1C << 18,
        0x80 << 24 | 2 << 16, // buffer_store_dword v0, off, s[8:11], 0
    ]);
    let native = native(&words);
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    // the section starts in the arms and ends in the join; afterwards each block holds whole ones
    let sections = |p: &ir::Program| {
        (0..p.blocks.len())
            .map(|b| super::wave::sections(p, b).len())
            .sum::<usize>()
    };
    assert_eq!(sections(&p), 0);
    let q = ir::whole_sections(&p).expect("sections to join");
    assert_eq!(sections(&q), 2);
    let st = structure::structure(&p).expect("structure");
    let res =
        resource::track(&p, &native.layout, native.stage, Some(&st)).expect("resource::track");
    let io = io::raise(&p, &native).expect("io::raise");
    let bytes = crate::dxbc::build(&[]);
    let c = super::Container::parse(&bytes).expect("container");
    let st_q = structure::structure(&q).expect("structure");
    let io_q = io::raise(&q, &native).expect("io::raise");
    let res_q = resource::track(&q, &native.layout, native.stage, Some(&st_q)).expect("track");
    let was = lower::TEST_HOST_WAVE.replace(Some(32));
    let joined = super::emit_any(&c, &native, &q, &st_q, &io_q, &res_q);
    let shader = super::emit(&c, &native, &p, &st, &io, &res);
    lower::TEST_HOST_WAVE.set(was);
    let shader = shader.expect("emit");
    let joined = joined.expect("the joined sections");
    assert!(
        shader.dxil == joined.dxil,
        "the split sections' translation was taken"
    );
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        let mut copy = shader.dxil.clone();
        v.validate(&mut copy).expect("DXIL validation failed");
    }
    let _one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    for (s0, expected) in [(1u32, 64.0f32), (0, 1.0)] {
        let Some(got) = on_the_card(&shader, [s0, 0]) else {
            return;
        };
        assert_eq!(f32::from_bits(got), expected, "s0 = {s0}");
    }
    eprintln!("ran on the hardware device: the maximum or the minimum over the console's 64 lanes");
}

/// One console wave of 64 threads summing doubles over the lanes below s0 as the title's
/// water volumetrics count their lanes (`water_shading:CS_TransmissionVolumetrics`,
/// 0x5c4-0x68c): `v_add_f64` of each dword pair moved by DPP, `v_permlanex16` and lane 32,
/// the sum moved to s[20:21] and taken back as a float (`v_cvt_f32_f64`). The terms here are
/// the thread index as a double (`v_cvt_f64_f32`), so the sum is s0 (s0 - 1) / 2.
#[test]
fn a_wave_sum_of_doubles_is_the_consoles() {
    let mut words = vec![
        0x7E3E_0D00, // v_cvt_f32_u32 v31, v0
        0x7E3A_211F, // v_cvt_f64_f32 v[29:30], v31
        0x7D88_0000, // v_cmp_gt_u32 vcc, s0, v0
        0xBE8C_046A, // s_mov_b64 s[12:13], vcc
        0xBE90_25C1, // s_or_saveexec_b64 s[16:17], -1
        0x7E04_0280, // v_mov_b32 v2, 0
        0x7E06_0280, // v_mov_b32 v3, 0
        0x8792_100C, // s_and_b64 s[18:19], s[12:13], s[16:17]
        0xD501_0002,
        0x004A_3B02, // v_cndmask_b32 v2, v2, v29, s[18:19]
        0xD501_0003,
        0x004A_3D03, // v_cndmask_b32 v3, v3, v30, s[18:19]
    ];
    // v_mov_b32 v4, v2 / v5, v3 with each DPP pattern, then v_add_f64 v[2:3], v[4:5], v[2:3]
    for (code, dpp) in [
        (0xE9, 0xDE54_C100),
        (0xE9, 0x9360_1200),
        (0xE9, 0x0009_2400),
        (0xFA, 0xFF09_4000),
    ] {
        words.extend([
            0x7E08_0200 | code,
            dpp | 2,
            0x7E0A_0200 | code,
            dpp | 3,
            0xD564_0002,
            0x0002_0504,
        ]);
    }
    words.extend([
        0xD778_0004,
        0x0201_0102, // v_permlanex16_b32 v4, v2, 0, 0
        0xD778_0005,
        0x0201_0103, // v_permlanex16_b32 v5, v3, 0, 0
        0xD564_0002,
        0x0002_0504, // v_add_f64 v[2:3], v[4:5], v[2:3]
        0xD760_0012,
        0x0001_4102, // v_readlane_b32 s18, v2, 32
        0xD760_0013,
        0x0001_4103, // v_readlane_b32 s19, v3, 32
        0xD564_0002,
        0x0002_0412, // v_add_f64 v[2:3], s[18:19], v[2:3]
        0xD760_0012,
        0x0001_0102, // v_readlane_b32 s18, v2, 0
        0xD760_0013,
        0x0001_0103, // v_readlane_b32 s19, v3, 0
        0xBE94_0312, // s_mov_b32 s20, s18
        0xBE95_0313, // s_mov_b32 s21, s19
        0xBEFE_0410, // s_mov_b64 exec, s[16:17]
        0x7E00_1E14, // v_cvt_f32_f64 v0, s[20:21]
        0x38 << 26 | 0x1C << 18,
        0x80 << 24 | 2 << 16, // buffer_store_dword v0, off, s[8:11], 0
    ]);
    let native = native(&words);
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    // the section leaves the sum's two dwords in s20 and s21
    let block = (0..p.blocks.len())
        .find(|&b| !super::wave::sections(&p, b).is_empty())
        .expect("a section");
    let sums = super::wave::sections(&p, block)
        .into_values()
        .flat_map(|s| s.shapes.into_values())
        .filter(|s| matches!(s, super::wave::Shape::Sum64 { .. }))
        .count();
    assert!(sums >= 2, "{sums} dwords of 64-bit sums");
    let st = structure::structure(&p).expect("structure");
    let res =
        resource::track(&p, &native.layout, native.stage, Some(&st)).expect("resource::track");
    let io = io::raise(&p, &native).expect("io::raise");
    let bytes = crate::dxbc::build(&[]);
    let c = super::Container::parse(&bytes).expect("container");
    let was = lower::TEST_HOST_WAVE.replace(Some(32));
    let shader = super::emit(&c, &native, &p, &st, &io, &res);
    lower::TEST_HOST_WAVE.set(was);
    let shader = shader.expect("emit");
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        let mut copy = shader.dxil.clone();
        v.validate(&mut copy).expect("DXIL validation failed");
    }
    let _one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    for s0 in [64u32, 40, 33, 1, 0] {
        let Some(got) = on_the_card(&shader, [s0, 0]) else {
            return;
        };
        let want = (s0 * s0.saturating_sub(1) / 2) as f32;
        assert_eq!(f32::from_bits(got), want, "the lanes below {s0}");
    }
    eprintln!("ran on the hardware device: the sums of doubles over the console's 64 lanes");
}

// ---- the paired form: a host wave of 32 for each console wave, two lanes a thread ----

// Operands as the hardware numbers them: SGPR n is n, VCC 106 (its high dword 107), EXEC 126,
// an inline integer n 128 + n (-1: 193), a literal (the next dword) 255, VGPR n 256 + n.
const VCC: u32 = 106;
const EXEC: u32 = 126;
const LIT: u32 = 255;

const fn v(n: u32) -> u32 {
    256 + n
}

const fn k(n: u32) -> u32 {
    128 + n
}

const fn sop1(op: u32, sdst: u32, src0: u32) -> u32 {
    0xBE80_0000 | sdst << 16 | op << 8 | src0
}

const fn sop2(op: u32, sdst: u32, src0: u32, src1: u32) -> u32 {
    0x8000_0000 | op << 23 | sdst << 16 | src1 << 8 | src0
}

/// A branch by `imm` dwords from the next instruction (or `s_barrier`, `imm` 0).
const fn sopp(op: u32, imm: i16) -> u32 {
    0xBF80_0000 | op << 16 | imm as u16 as u32
}

const fn vop1(op: u32, vdst: u32, src0: u32) -> u32 {
    0x7E00_0000 | vdst << 17 | op << 9 | src0
}

const fn vop2(op: u32, vdst: u32, src0: u32, vsrc1: u32) -> u32 {
    op << 25 | vdst << 17 | vsrc1 << 9 | src0
}

const fn vopc(op: u32, src0: u32, vsrc1: u32) -> u32 {
    0x7C00_0000 | op << 17 | vsrc1 << 9 | src0
}

const fn vop3(op: u32, dst: u32, src0: u32, src1: u32) -> [u32; 2] {
    [0xD400_0000 | op << 16 | dst, src0 | src1 << 9]
}

const fn ds(op: u32, vdst: u32, addr: u32, data: u32, offset: u32) -> [u32; 2] {
    [
        0xD800_0000 | op << 18 | offset,
        vdst << 24 | data << 8 | addr,
    ]
}

/// `buffer_store_dword v<data>, v<addr>, s[8:11], 0 offen offset:<offset>`
const fn store(data: u32, addr: u32, offset: u32) -> [u32; 2] {
    [0xE070_1000 | offset, 0x8002_0000 | data << 8 | addr]
}

/// `v<regs>` stored as each thread's dwords of the UAV, the thread's index (v0) times 16 on.
fn stores(regs: [u32; 4]) -> Vec<u32> {
    let mut w = vec![vop2(0x1A, 15, k(4), 0)]; // v_lshlrev_b32 v15, 4, v0
    for (n, r) in regs.into_iter().enumerate() {
        w.extend(store(r, 15, 4 * n as u32));
    }
    w
}

/// `native` in the paired form on a host of 32-lane waves, validated by `dxil.dll` when
/// present. Where no other form gives the host the program (each still asks for 64-lane
/// waves, or fails), it is also what [`super::emit`] gives it.
fn paired(native: &NativeProgram) -> HostShader {
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let st = structure::structure(&p).expect("structure");
    let res =
        resource::track(&p, &native.layout, native.stage, Some(&st)).expect("resource::track");
    let io = io::raise(&p, native).expect("io::raise");
    let bytes = crate::dxbc::build(&[]);
    let c = super::Container::parse(&bytes).expect("container");
    let was = lower::TEST_HOST_WAVE.replace(Some(32));
    let others = super::emit_forms(&c, native, &p, &st, &io, &res);
    let shader = super::paired(&c, native, &p, &io, &res);
    let chosen = super::emit(&c, native, &p, &st, &io, &res);
    lower::TEST_HOST_WAVE.set(was);
    let (shader, wave) = shader.expect("the paired form");
    assert_eq!(wave, Some(32));
    if !matches!(others, Ok((_, wave)) if wave != Some(64)) {
        assert_eq!(chosen.expect("emit").dxil, shader.dxil, "emit's choice");
    }
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        let mut copy = shader.dxil.clone();
        v.validate(&mut copy).expect("DXIL validation failed");
    }
    shader
}

/// `shader` on the hardware device: one group, root constants `s0..s3`, a raw UAV of `dwords`
/// zeroed dwords, which come back; None (and a line) without a device that takes it.
fn run_paired(shader: &HostShader, constants: [u32; 4], dwords: u32) -> Option<Vec<u32>> {
    let Ok(host) = Host::new(false) else {
        eprintln!("no SM 6.6 hardware device; not run");
        return None;
    };
    let (root, pso) = match host.compute_pipeline(shader) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("the hardware device does not take this program: {e}; not run");
            return None;
        }
    };
    let mut ud = vec![0u32; shader.user_data_dwords().max(1) as usize];
    for u in &shader.user_data {
        if let (RootType::Constants, 0, d @ 0..=3) = (u.root, u.slot, u.dword) {
            ud[u.index as usize] = constants[d as usize];
        }
    }
    let mut out = host
        .dispatch(
            &root,
            &pso,
            &Dispatch {
                roots: vec![(0, Root::Constants(ud))],
                views: &[View::RawUav(dwords)],
                samplers: &[],
                groups: [1, 1, 1],
            },
        )
        .unwrap_or_else(|e| panic!("dispatch failed: {e}"));
    Some(out.swap_remove(0))
}

/// Panics at the first dword of `got` that is not the console's (`want`), naming the thread
/// and which of its four dwords.
fn same_as_console(got: &[u32], want: &[u32]) {
    assert_eq!(got.len(), want.len());
    if let Some(n) = (0..want.len()).find(|&n| got[n] != want[n]) {
        panic!(
            "thread {}, dword {}: {:#x}, the console's {:#x}",
            n / 4,
            n % 4,
            got[n],
            want[n]
        );
    }
}

/// One console wave of 64 threads with 256 bytes of LDS. The even lanes write their dword in
/// one arm of a per-lane branch; the odd lanes theirs in the other arm, then, in that arm and
/// with no barrier, read their mirror lane's (63 - n, an even lane's); the even lanes read
/// theirs (an odd lane's) after the join. Then lanes 5 and 40 are read (`v_readlane_b32`), the
/// lanes holding more than 250 are counted (`s_bcnt1_i32_b64` of the compare's mask) and
/// counted below each lane (`v_mbcnt_lo/hi`): each of these takes lanes of both halves.
#[test]
fn paired_lanes_share_lds_and_lanes_across_halves_without_barriers() {
    let mut w = vec![vop2(0x1A, 1, k(2), 0)]; // v_lshlrev_b32 v1, 2, v0: this lane's dword
    w.extend(vop3(0x169, 2, v(0), k(7))); // v_mul_lo_u32 v2, v0, 7
    w.extend([
        vop2(0x26, 5, k(63), 0), // v_sub_nc_u32 v5, 63, v0: the mirror lane
        vop2(0x1A, 5, k(2), 5),  // v_lshlrev_b32 v5, 2, v5: its dword
        vop2(0x1B, 3, k(1), 0),  // v_and_b32 v3, 1, v0
        vopc(0xC2, k(0), 3),     // v_cmp_eq_u32 vcc, 0, v3: the even lanes
        sop1(0x24, 12, VCC),     // s_and_saveexec_b64 s[12:13], vcc
        sopp(0x08, 4),           // s_cbranch_execz past the even lanes' arm
        vop2(0x25, 4, LIT, 2),   // v_add_nc_u32 v4, 100, v2
        100,
    ]);
    w.extend(ds(0x0D, 0, 1, 4, 0)); // ds_write_b32 v1, v4
    w.extend([
        sop2(0x15, EXEC, 12, EXEC), // s_andn2_b64 exec, s[12:13], exec: the odd lanes
        sopp(0x08, 6),              // s_cbranch_execz past their arm
        vop2(0x25, 4, LIT, 2),      // v_add_nc_u32 v4, 200, v2
        200,
    ]);
    w.extend(ds(0x0D, 0, 1, 4, 0)); // ds_write_b32 v1, v4
    w.extend(ds(0x36, 6, 5, 0, 0)); // ds_read_b32 v6, v5: an even lane's, from the other arm
    w.extend([
        sop1(0x04, EXEC, 12), // s_mov_b64 exec, s[12:13]
        sop1(0x24, 12, VCC),  // s_and_saveexec_b64 s[12:13], vcc: the even lanes again
    ]);
    w.extend(ds(0x36, 6, 5, 0, 0)); // ds_read_b32 v6, v5: an odd lane's
    w.push(sop1(0x04, EXEC, 12)); // s_mov_b64 exec, s[12:13]
    w.extend(vop3(0x360, 14, v(6), k(5))); // v_readlane_b32 s14, v6, 5
    w.extend(vop3(0x360, 15, v(6), k(40))); // v_readlane_b32 s15, v6, 40
    w.extend([
        sop2(0x00, 14, 14, 15), // s_add_u32 s14, s14, s15
        vopc(0xC1, LIT, 6),     // v_cmp_lt_u32 vcc, 250, v6
        250,
        sop1(0x10, 16, VCC), // s_bcnt1_i32_b64 s16, vcc
    ]);
    w.extend(vop3(0x365, 7, VCC, k(0))); // v_mbcnt_lo_u32_b32 v7, vcc_lo, 0
    w.extend(vop3(0x366, 7, VCC + 1, v(7))); // v_mbcnt_hi_u32_b32 v7, vcc_hi, v7
    w.extend([
        vop1(0x01, 9, 14),  // v_mov_b32 v9, s14
        vop1(0x01, 10, 16), // v_mov_b32 v10, s16
    ]);
    w.extend(stores([6, 9, 10, 7]));
    let shader = paired(&native_with(&w, true, 64, 256));
    // the console: lane n's dword is 7n + 100 (even n) or 7n + 200, and lane n reads 63 - n's
    let dword = |n: u32| 7 * n + if n.is_multiple_of(2) { 100 } else { 200 };
    let read: Vec<u32> = (0..64).map(|n| dword(63 - n)).collect();
    let big = (0..64)
        .filter(|&n| read[n] > 250)
        .fold(0u64, |m, n| m | 1 << n);
    assert!(
        big as u32 != 0 && big >> 32 != 0,
        "lanes of both halves counted"
    );
    let want: Vec<u32> = (0..64)
        .flat_map(|n| {
            [
                read[n],
                read[5] + read[40],
                big.count_ones(),
                (big & ((1 << n) - 1)).count_ones(),
            ]
        })
        .collect();
    let _one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let Some(got) = run_paired(&shader, [0; 4], 256) else {
        return;
    };
    same_as_console(&got, &want);
    eprintln!(
        "ran on the hardware device: LDS, lane reads and masks across the halves as on the console"
    );
}

/// Two console waves of 64 in a group of 128 (two host waves of 32, 512 bytes of LDS). In
/// each, lane 0 zeroes the wave's dword of LDS; every lane loops (lane & 3) + 1 + wave times,
/// adding its lane number to its sum and 1 to the wave's dword each round (`ds_add_u32`; the
/// loop's branch is the wave's: it goes round while some lane is left); after the loop each
/// lane reads the wave's total, with no barrier; lanes 40-63 read their first lane's sum
/// (`v_readfirstlane_b32`: lane 40, an upper lane), then lanes 20-63 (lane 20, a lower one,
/// though upper lanes have one too); past the barrier, each lane reads the other wave's total.
#[test]
fn paired_lanes_loop_apart_and_meet_the_other_wave_at_the_barrier() {
    let mut w = vec![
        vop2(0x16, 1, k(6), 0),  // v_lshrrev_b32 v1, 6, v0: the console wave
        vop2(0x1B, 2, k(63), 0), // v_and_b32 v2, 63, v0: the lane in it
        vop2(0x1A, 11, k(2), 1), // v_lshlrev_b32 v11, 2, v1: the wave's dword
        vop1(0x01, 12, k(0)),    // v_mov_b32 v12, 0
        sop1(0x04, 22, EXEC),    // s_mov_b64 s[22:23], exec
        vopc(0xD2, k(0), 2),     // v_cmpx_eq_u32 exec, 0, v2: lane 0
    ];
    w.extend(ds(0x0D, 0, 11, 12, 0)); // ds_write_b32 v11, v12
    w.extend([
        sop1(0x04, EXEC, 22),   // s_mov_b64 exec, s[22:23]
        vop2(0x1B, 3, k(3), 2), // v_and_b32 v3, 3, v2
        vop2(0x25, 3, k(1), 3), // v_add_nc_u32 v3, 1, v3
        vop2(0x25, 3, v(1), 3), // v_add_nc_u32 v3, v1, v3: the rounds
        vop1(0x01, 4, k(0)),    // v_mov_b32 v4, 0
        vop1(0x01, 13, k(1)),   // v_mov_b32 v13, 1
        sop1(0x04, 20, EXEC),   // s_mov_b64 s[20:21], exec
        vop2(0x25, 4, v(2), 4), // loop: v_add_nc_u32 v4, v2, v4
    ]);
    w.extend(ds(0x00, 0, 11, 13, 0)); // ds_add_u32 v11, v13
    w.extend([
        vop2(0x25, 3, 193, 3),       // v_add_nc_u32 v3, -1, v3
        vopc(0xC5, k(0), 3),         // v_cmp_ne_u32 vcc, 0, v3
        sop2(0x0F, EXEC, EXEC, VCC), // s_and_b64 exec, exec, vcc
        sopp(0x09, -7),              // s_cbranch_execnz loop
        sop1(0x04, EXEC, 20),        // s_mov_b64 exec, s[20:21]
    ]);
    w.extend(ds(0x36, 5, 11, 0, 0)); // ds_read_b32 v5, v11: the wave's total
    w.extend([
        sop1(0x04, 24, EXEC),          // s_mov_b64 s[24:25], exec
        vopc(0xD1, k(39), 2),          // v_cmpx_lt_u32 exec, 39, v2: lanes 40-63
        vop1(0x02, 6, v(4)),           // v_readfirstlane_b32 s6, v4
        sop1(0x04, EXEC, 24),          // s_mov_b64 exec, s[24:25]
        vopc(0xD1, k(19), 2),          // v_cmpx_lt_u32 exec, 19, v2: lanes 20-63
        vop1(0x02, 7, v(4)),           // v_readfirstlane_b32 s7, v4
        sop1(0x04, EXEC, 24),          // s_mov_b64 exec, s[24:25]
        vop1(0x01, 6, 6),              // v_mov_b32 v6, s6
        vop1(0x01, 9, 7),              // v_mov_b32 v9, s7
        0xD400_0000 | 0x346 << 16 | 6, // v_lshl_add_u32 v6, v9, 16, v6
        v(9) | k(16) << 9 | v(6) << 18,
        sopp(0x0A, 0),            // s_barrier
        vop2(0x1D, 14, k(4), 11), // v_xor_b32 v14, 4, v11: the other wave's dword
    ]);
    w.extend(ds(0x36, 7, 14, 0, 0)); // ds_read_b32 v7, v14
    w.extend(stores([4, 5, 6, 7]));
    let shader = paired(&native_with(&w, true, 128, 512));
    let rounds = |wave: u32, lane: u32| (lane & 3) + 1 + wave;
    let total = |wave: u32| (0..64).map(|lane| rounds(wave, lane)).sum::<u32>();
    let first = |wave: u32, lane: u32| lane * rounds(wave, lane);
    let want: Vec<u32> = (0..128)
        .flat_map(|t| {
            let (wave, lane) = (t / 64, t % 64);
            [
                first(wave, lane),
                total(wave),
                first(wave, 20) << 16 | first(wave, 40),
                total(1 - wave),
            ]
        })
        .collect();
    let _one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let Some(got) = run_paired(&shader, [0; 4], 512) else {
        return;
    };
    same_as_console(&got, &want);
    eprintln!(
        "ran on the hardware device: two console waves, their loops and LDS as on the console"
    );
}

/// One console wave: `ds_bpermute_b32` from the mirror lane; a DPP row shift whose row mask
/// leaves rows 0 and 3 alone (rows 2 and 3 are lanes 32-63: rows are counted in console lanes); a
/// `ds_swizzle_b32` swapping the two 16-lane rows of each 32 lanes; `v_writelane_b32` of s0
/// into lane 45.
#[test]
fn paired_lanes_permute_shift_and_write_lanes_of_both_halves() {
    let mut w = vop3(0x169, 1, v(0), k(3)).to_vec(); // v_mul_lo_u32 v1, v0, 3
    w.extend([
        vop2(0x25, 1, k(1), 1),  // v_add_nc_u32 v1, 1, v1
        vop2(0x26, 2, k(63), 0), // v_sub_nc_u32 v2, 63, v0
        vop2(0x1A, 2, k(2), 2),  // v_lshlrev_b32 v2, 2, v2: the mirror lane's address
    ]);
    w.extend(ds(0xB3, 3, 2, 1, 0)); // ds_bpermute_b32 v3, v2, v1
    w.extend([
        vop1(0x01, 4, LIT), // v_mov_b32 v4, 1000
        1000,
        vop1(0x01, 4, 0xFA), // v_mov_b32 v4, v1 row_shr:1 row_mask:0x6 bank_mask:0xf
        0x6 << 28 | 0xF << 24 | 0x111 << 8 | 1,
    ]);
    w.extend(ds(0x35, 5, 1, 0, 0x401F)); // ds_swizzle_b32 v5, v1 offset:swizzle(BITMASK_PERM, xor 16)
    w.push(vop1(0x01, 6, v(0))); // v_mov_b32 v6, v0
    w.extend(vop3(0x361, 6, 0, k(45))); // v_writelane_b32 v6, s0, 45
    w.extend(stores([3, 4, 5, 6]));
    let shader = paired(&native_with(&w, true, 64, 0));
    let s0 = 0xABCD_1234;
    let x = |n: u32| 3 * n + 1;
    let want: Vec<u32> = (0..64)
        .flat_map(|n| {
            let shifted = if matches!(n >> 4, 1 | 2) && n & 15 != 0 {
                x(n - 1)
            } else {
                1000
            };
            let swizzled = x(n & 32 | (n & 31) ^ 16);
            [x(63 - n), shifted, swizzled, if n == 45 { s0 } else { n }]
        })
        .collect();
    let _one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let Some(got) = run_paired(&shader, [s0, 0, 0, 0], 256) else {
        return;
    };
    same_as_console(&got, &want);
    eprintln!(
        "ran on the hardware device: permutes, row masks, swizzles and lane writes as on the console"
    );
}

#[test]
fn readfirstlane_uses_the_whole_console_wave() {
    use windows::Win32::Graphics::Direct3D12::{
        D3D12_FEATURE_D3D12_OPTIONS1, D3D12_FEATURE_DATA_D3D12_OPTIONS1,
    };

    let mut words = vec![
        vop2(0x1B, 1, k(63), 0), // v1 = flat thread ID modulo 64
        sop1(0x04, 12, EXEC),    // save EXEC
        vopc(0xD3, 0, 1),        // v_cmpx_le_u32 s0, v1
        vop1(0x02, 6, v(0)),     // v_readfirstlane_b32 s6, v0
        sop1(0x04, EXEC, 12),    // restore EXEC for the store
        vop1(0x01, 2, 6),        // v_mov_b32 v2, s6
        vop2(0x1A, 15, k(2), 0), // v15 = byte offset of this thread's result
    ];
    words.extend(store(2, 15, 0));
    let container = crate::dxbc::build(&[]);
    let container = super::Container::parse(&container).expect("container");
    let _one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let wave32 = Host::new(false).ok().is_some_and(|host| {
        let mut options = D3D12_FEATURE_DATA_D3D12_OPTIONS1::default();
        // SAFETY: D3D12 writes its OPTIONS1 structure into the matching buffer.
        let queried = unsafe {
            host.device.CheckFeatureSupport(
                D3D12_FEATURE_D3D12_OPTIONS1,
                (&raw mut options).cast(),
                size_of_val(&options) as u32,
            )
        };
        queried.is_ok()
            && options.WaveOps.as_bool()
            && options.WaveLaneCountMin <= 32
            && options.WaveLaneCountMax >= 32
    });
    for threads in [64, 128] {
        let native = native_of(&words, true, threads);
        let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
        let st = structure::structure(&p).expect("structure");
        let io = io::raise(&p, &native).expect("io::raise");
        let res =
            resource::track(&p, &native.layout, native.stage, Some(&st)).expect("resource::track");
        let old = lower::TEST_HOST_WAVE.replace(Some(32));
        let pairs = super::TEST_PAIRS.replace(Some(true));
        let shader = super::emit(&container, &native, &p, &st, &io, &res);
        super::TEST_PAIRS.set(pairs);
        lower::TEST_HOST_WAVE.set(old);
        let shader = shader.expect("emit a first-lane read for a 32-lane host");
        if let Ok(mut v) =
            validator::Validator::load(validator::Validator::default_path().as_deref())
        {
            v.validate(&mut shader.dxil.clone())
                .expect("DXIL validation failed");
        }
        if !wave32 {
            continue;
        }
        for cutoff in [0, 10, 40, 63, 64] {
            let got = run_paired(&shader, [cutoff, 0, 0, 0], threads)
                .expect("dispatch on a 32-lane device");
            assert_eq!(got.len(), threads as usize);
            for (lane, value) in got.into_iter().enumerate() {
                let first = (lane as u32 & !63) + if cutoff < 64 { cutoff } else { 0 };
                assert_eq!(
                    value, first,
                    "{threads} threads, EXEC cutoff {cutoff}, lane {lane}"
                );
            }
        }
    }
    if wave32 {
        eprintln!(
            "ran on the hardware device: the first active lane of each console wave for both group sizes"
        );
    } else {
        eprintln!("no SM 6.6 device with 32-lane waves; dispatch skipped");
    }
}

#[test]
fn readfirstlane_with_a_uniform_source_lowers_without_pairing() {
    let mut words = vec![
        vop1(0x01, 1, 0),    // v_mov_b32 v1, s0: every lane gets the same value
        vop1(0x02, 6, v(1)), // v_readfirstlane_b32 s6, v1
        vop1(0x01, 2, 6),    // v_mov_b32 v2, s6
        vop2(0x1A, 15, k(2), 0),
    ];
    words.extend(store(2, 15, 0));
    let native = native(&words);
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    lowered(&native, &p, &HashSet::new()).expect("lower the wave-uniform first-lane read");
}

/// [`program`]'s wave OR inside a per-lane branch, in the paired form: the OR of both halves'
/// lanes, whichever take the branch; nothing stored when none does.
#[test]
fn paired_lanes_or_a_wave_inside_a_lane_branch() {
    for (skip, name) in [
        (0xBF88_0000, "s_cbranch_execz"),
        (0xBF84_0000, "s_cbranch_scc0"),
    ] {
        let shader = paired(&program(skip));
        let _one = super::valu_tests::ONE_AT_A_TIME
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let cases = [
            (64u32, u32::MAX),
            (40, 0x000F_FFFF),
            (33, 0x0001_FFFF),
            (10, 0x1F),
            (1, 1),
            (0, 0),
        ];
        for (below, expected) in cases {
            let Some(got) = on_the_card(&shader, [below, 0]) else {
                return;
            };
            assert_eq!(got, expected, "{name}: {below} threads in the branch");
        }
    }
    eprintln!("ran on the hardware device: the OR of both halves in every case");
}

/// One console wave of 64 threads, thread t holding the bit `1 << (t >> 1)`: the threads below
/// s0 OR theirs over the wave inside `if (t < s0) { ... }` ([`super::wave::WAVE_OR_V7_TO_S7`]
/// into s7, which holds 0x77 before), and every thread stores s7 after the branch: the wave's
/// OR, or 0x77 when no thread took the branch and the wave skipped it. A wave-wide value used
/// past a per-lane branch, as the title's tile classification and light clustering programs
/// use theirs: no form in two host waves takes it (they would meet inside the branch, and the
/// skip cannot be left out: s7 would no longer keep 0x77), and the approximate form, each host
/// wave a wave of its own, gives each half its own OR, 0x77 to the upper half while s0 <= 32.
/// The title's 39 such programs ran approximate; [`super::emit`] now gives the paired form.
#[test]
fn a_wave_value_used_past_a_lane_branch_takes_the_paired_form_not_an_approximate_one() {
    let mut w = vec![
        sop1(0x03, 7, LIT), // s_mov_b32 s7, 0x77
        0x77,
        vop1(0x01, 5, k(1)),    // v_mov_b32 v5, 1
        vop2(0x16, 6, k(1), 0), // v_lshrrev_b32 v6, 1, v0
        vop2(0x1A, 7, v(6), 5), // v_lshlrev_b32 v7, v6, v5
        vopc(0xC4, 0, 0),       // v_cmp_gt_u32 vcc, s0, v0
        sop1(0x24, 12, VCC),    // s_and_saveexec_b64 s[12:13], vcc
        sopp(0x08, 20),         // s_cbranch_execz past the OR
    ];
    w.extend_from_slice(&super::wave::WAVE_OR_V7_TO_S7);
    w.extend([
        sop1(0x04, EXEC, 12), // s_mov_b64 exec, s[12:13]
        vop1(0x01, 8, 7),     // v_mov_b32 v8, s7
    ]);
    w.extend(stores([8, 7, 6, 0]));
    let native = native_with(&w, true, 64, 0);
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let st = structure::structure(&p).expect("structure");
    let res =
        resource::track(&p, &native.layout, native.stage, Some(&st)).expect("resource::track");
    let io = io::raise(&p, &native).expect("io::raise");
    let bytes = crate::dxbc::build(&[]);
    let c = super::Container::parse(&bytes).expect("container");
    let was = lower::TEST_HOST_WAVE.replace(Some(32));
    let pairs = super::paired(&c, &native, &p, &io, &res);
    let chosen = super::emit(&c, &native, &p, &st, &io, &res);
    // (as before: the paired form only where no other form takes the program)
    let off = super::TEST_PAIRS.replace(Some(false));
    let approximate = super::emit(&c, &native, &p, &st, &io, &res);
    super::TEST_PAIRS.set(off);
    lower::TEST_HOST_WAVE.set(was);
    let (pairs, wave) = pairs.expect("the paired form");
    assert_eq!(wave, Some(32));
    let (chosen, approximate) = (
        chosen.expect("emit"),
        approximate.expect("an approximate form"),
    );
    assert!(
        chosen.dxil == pairs.dxil,
        "emit's choice is not the paired form"
    );
    assert!(
        approximate.dxil != pairs.dxil,
        "without the paired form, emit gives it anyway"
    );
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        let mut copy = chosen.dxil.clone();
        v.validate(&mut copy).expect("DXIL validation failed");
    }
    // the console: every thread stores the OR of the threads below s0, or 0x77 with none
    let want = |s0: u32| -> Vec<u32> {
        let or = (0..64u32)
            .filter(|&t| t < s0)
            .fold(0u32, |m, t| m | 1 << (t >> 1));
        let s7 = if s0 == 0 { 0x77 } else { or };
        (0..64u32)
            .flat_map(|t| [s7, 1 << (t >> 1), t >> 1, t])
            .collect()
    };
    let _one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    for s0 in [64u32, 40, 33, 32, 10, 1, 0] {
        let Some(got) = run_paired(&chosen, [s0, 0, 0, 0], 256) else {
            return;
        };
        same_as_console(&got, &want(s0));
    }
    // each half on its own: with s0 = 10 the upper half took no branch and kept 0x77
    let Some(halves) = run_paired(&approximate, [10, 0, 0, 0], 256) else {
        return;
    };
    assert_eq!((want(10)[0], want(10)[4 * 40]), (0x1F, 0x1F));
    assert_eq!(
        (halves[0], halves[4 * 40]),
        (0x1F, 0x77),
        "the approximate form's lanes 0 and 40"
    );
    eprintln!(
        "ran on the hardware device: the wave's OR past the branch in every lane, not each half's"
    );
}

/// [`half_sum_program`] in the paired form: each half's tree over its 32 lanes, then the two
/// halves' sums, the console's last addition.
#[test]
fn paired_lanes_sum_halves_as_the_consoles_tree() {
    let (native, tree, _) = half_sum_program();
    let shader = paired(&native);
    let _one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let Some(got) = on_the_card(&shader, [0, 0]) else {
        return;
    };
    assert_eq!(got, u32::from(tree));
    eprintln!("ran on the hardware device: the sum {tree:#x} of the console's tree");
}

/// The lens-flare scans' allocation (`lens_flares:CS_ScanMip1`, 0x5c0-0x61c): in a branch
/// of the lanes below s0, the first lane adds the wave's count times a size (s1) to a
/// counter, and each lane takes base + below × size with `v_mad_u32_u24`. That is the
/// idiom a host wave may run alone ([`super::idiom::wave_local`]): each half reserves its own
/// count's slots. The same with the count where the base goes is not.
#[test]
fn an_allocation_in_units_of_a_size_is_the_idiom() {
    let program = |base: u32| -> Vec<u8> {
        let words = [
            0x7D88_0000, // v_cmp_gt_u32 vcc, s0, v0
            0xBE8C_246A, // s_and_saveexec_b64 s[12:13], vcc
            0xBF88_0016, // s_cbranch_execz past the allocation
            0xBE82_047E, // s_mov_b64 s[2:3], exec
            0xBE84_147E, // s_ff1_i32_b64 s4, exec
            0xD765_0004,
            0x0001_00C1, // v_mbcnt_lo_u32_b32 v4, -1, 0
            0xD766_0004,
            0x0002_08C1, // v_mbcnt_hi_u32_b32 v4, -1, v4: the lane
            0xBE86_047E, // s_mov_b64 s[6:7], exec
            0x7DA4_0804, // v_cmpx_eq_u32 exec, s4, v4: the first lane
            0xBE82_1002, // s_bcnt1_i32_b64 s2, s[2:3]: the count
            0x7E02_0201, // v_mov_b32 v1, s1: the size
            0x160A_0202, // v_mul_u32_u24 v5, s2, v1
            0x7E16_0280, // v_mov_b32 v11, 0
            0xE0C8_6000,
            0x8002_050B, // buffer_atomic_add v5, v11, s[8:11], 0 idxen glc
            0xBEFE_0406, // s_mov_b64 exec, s[6:7]
            0xD765_0004,
            0x0001_007E, // v_mbcnt_lo_u32_b32 v4, exec_lo, 0
            0xD766_0004,
            0x0002_087F, // v_mbcnt_hi_u32_b32 v4, exec_hi, v4: the lanes below
            0x7E0A_0505, // v_readfirstlane_b32 s5, v5: the base
            0xD543_0006,
            0x0002_0304 | base << 18, // v_mad_u32_u24 v6, v4, v1, s<base>
            0xBEFE_040C,              // s_mov_b64 exec, s[12:13]
            S_ENDPGM,
        ];
        words.iter().flat_map(|w| w.to_le_bytes()).collect()
    };
    let idiom = |code: &[u8]| {
        let p = ir::build(code, true, 256).unwrap();
        let st = structure::structure(&p).unwrap();
        super::idiom::wave_local(&p, &st)
    };
    assert_eq!(idiom(&program(5)), Ok(()));
    assert!(idiom(&program(2)).is_err(), "the count as the base");
}

/// Two console waves (128 threads) in the paired form reading `Input::GroupInfo` (`s5`, enabled
/// via `rsrc2` bit 10): bits 0-5 hold the console wave count (2) and bits 6-11 hold each wave's
/// console wave index (0 for threads 0..63, 1 for threads 64..127).
#[test]
fn paired_lanes_group_info_reports_console_wave_index() {
    let mut w = vec![
        vop1(0x01, 4, 6), // v_mov_b32 v4, s6 (GroupInfo; s5 is GroupId.x and clobbered by native_with prologue)
        vop1(0x01, 5, v(0)),
        vop1(0x01, 6, k(0)),
        vop1(0x01, 7, k(0)),
    ];
    w.extend(stores([4, 5, 6, 7]));
    let mut native = native_with(&w, true, 128, 512);
    if let StageIo::Cs(cs) = &mut native.io {
        cs.rsrc2 |= (1 << 7) | (1 << 10); // GroupId.x in s5, GroupInfo (tg_size) in s6
    }
    let shader = paired(&native);
    let want: Vec<u32> = (0..128)
        .flat_map(|t| [((t / 64) << 6) | 2, t, 0, 0])
        .collect();
    let _one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let Some(got) = run_paired(&shader, [0; 4], 512) else {
        return;
    };
    same_as_console(&got, &want);
    eprintln!("ran on the hardware device: paired GroupInfo gave wave 0 -> 2 and wave 1 -> 66");
}

/// A wave64 program of 128 threads (two console waves) that leaves early when no lane of its
/// wave is below s0, as the console's compiler does when the end of the program is too far for
/// a branch: `s_cbranch_execnz` over a far jump to the end. `extra` are words that go into the
/// far jump's block before it, and `work_at_end` puts an instruction between the jump's target
/// and the end of the program. Then the gate: every thread stores 1, or 2 when some lane of its
/// console wave that is still running has the thread number s1 (the lanes that fail a test).
fn far_early_out(extra: &[u32], work_at_end: bool) -> NativeProgram {
    let gate = [
        0x7D84_0001, // v_cmp_eq_u32 vcc, s1, v0
        0xBE90_106A, // s_bcnt1_i32_b64 s16, vcc
        0xBF06_8010, // s_cmp_eq_u32 s16, 0
        0x8511_8281, // s_cselect_b32 s17, 1, 2
        0x7E02_0211, // v_mov_b32 v1, s17
        0x3404_0082, // v_lshlrev_b32 v2, 2, v0
        0x38 << 26 | 0x1C << 18 | 1 << 12,
        0x80 << 24 | 2 << 16 | 1 << 8 | 2, // buffer_store_dword v1, v2, s[8:11], 0 offen
    ];
    let jump = [
        0xBE8E_1F00, // s_getpc_b64 s[14:15]
        0x800E_FF0E,
        4 * (4 + gate.len() as u32), // s_add_u32 s14, s14, the way to the end
        0x820F_800F,                 // s_addc_u32 s15, s15, 0
        0xBE80_200E,                 // s_setpc_b64 s[14:15]
    ];
    let mut words = vec![
        0x7D88_0000,                                     // v_cmp_gt_u32 vcc, s0, v0
        0xBE8C_246A,                                     // s_and_saveexec_b64 s[12:13], vcc
        0xBF89_0000 | (extra.len() + jump.len()) as u32, // s_cbranch_execnz over the jump
    ];
    words.extend_from_slice(extra);
    words.extend_from_slice(&jump);
    words.extend_from_slice(&gate);
    if work_at_end {
        words.push(0x7E06_0281); // v_mov_b32 v3, 1
    }
    native_of(&words, true, 128)
}

fn branches_named(p: &ir::Program, name: &str) -> usize {
    p.blocks
        .iter()
        .filter(|b| b.insts.last().is_some_and(|i| i.name() == name))
        .count()
}

/// What the gate of [`far_early_out`] stores in each of 128 threads.
fn gate_expected(below: u32, failing: u32) -> Vec<u32> {
    (0..128u32)
        .map(|t| {
            let some_fail = failing < below && failing / 64 == t / 64;
            match (t < below, some_fail) {
                (false, _) => 0,
                (true, false) => 1,
                (true, true) => 2,
            }
        })
        .collect()
}

#[test]
fn a_far_early_out_is_left_out_and_the_gate_counts_the_lanes_of_both_halves() {
    let native = far_early_out(&[], false);
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    assert_eq!(branches_named(&p, "s_cbranch_execnz"), 1);
    lower::take_needs();
    let kept = lowered(&native, &p, &HashSet::new());
    assert!(
        matches!(&kept, Err(ShaderError::Unsupported { what, .. }) if what.ends_with("where not every lane runs")),
        "{:?}",
        kept.err()
    );
    for (name, form) in [
        ("early-outs", ir::without_early_outs(&p)),
        ("skips", ir::without_skips(&p, false)),
    ] {
        let form = form.unwrap_or_else(|| panic!("without {name}: the far early-out stays"));
        assert_eq!(
            branches_named(&form, "s_cbranch_execnz"),
            0,
            "without {name}"
        );
    }
    let form = ir::without_early_outs(&p).expect("left out");
    let shader = lowered(&native, &form, &HashSet::new()).expect("lower");
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        let mut copy = shader.dxil.clone();
        v.validate(&mut copy).expect("DXIL validation failed");
    }
    let _one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    // (threads below s0 are in bounds; the one thread s1 fails the test) A failing thread of the
    // upper half gives 2 to the lower half's threads as well, and a failing thread out of
    // bounds counts for nothing
    for (below, failing) in [
        (128u32, 40u32),
        (128, 100),
        (128, 63),
        (40, 50),
        (40, 10),
        (33, 32),
        (1, 0),
        (0, 5),
    ] {
        let Some(got) = whole_uav(&shader, [below, failing], 128) else {
            return;
        };
        assert_eq!(
            got,
            gate_expected(below, failing),
            "{below} threads in bounds, thread {failing} fails"
        );
    }
    eprintln!("ran on the hardware device: the gate's count over both halves of each console wave");
}

#[test]
fn a_far_jump_that_does_work_is_not_an_early_out() {
    // work in the jump's block, and work between the jump's target and the end
    for (name, native) in [
        ("work in the jump", far_early_out(&[0xBE94_0385], false)), // s_mov_b32 s20, 5
        ("work at the target", far_early_out(&[], true)),
    ] {
        let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
        assert_eq!(branches_named(&p, "s_cbranch_execnz"), 1, "{name}");
        assert!(ir::without_early_outs(&p).is_none(), "{name}");
        assert!(ir::without_skips(&p, false).is_none(), "{name}");
        assert!(ir::without_skips(&p, true).is_none(), "{name}");
    }
}

/// `if (t < s0) { park s[20:21]; if (s1 == 0) {..} else {..}; take it back }` and a read of
/// s[20:21] after the join, as the console's compiler leaves a parked scalar across an inner
/// branch. `then` is the inner branch's first arm; the second one leaves the parked scalar alone.
fn parked_across_an_inner_branch(then: u32) -> ir::Program {
    let body = [
        0xBE96_0414, // s_mov_b64 s[22:23], s[20:21]
        0xBF06_8001, // s_cmp_eq_u32 s1, 0
        0xBF85_0002, // s_cbranch_scc1 to the second arm
        then,
        0xBF82_0001, // s_branch to the join
        0xBE98_0382, // s_mov_b32 s24, 2
        0xBE94_0416, // s_mov_b64 s[20:21], s[22:23]
    ];
    let mut words = vec![
        0xBE94_0406,                     // s_mov_b64 s[20:21], s[6:7]
        0x7D88_0000,                     // v_cmp_gt_u32 vcc, s0, v0
        0xBE8C_246A,                     // s_and_saveexec_b64 s[12:13], vcc
        0xBF88_0000 | body.len() as u32, // s_cbranch_execz past the body
    ];
    words.extend_from_slice(&body);
    words.extend_from_slice(&[
        0xBEFE_040C, // s_mov_b64 exec, s[12:13]
        0x7E02_0214, // v_mov_b32 v1, s20
    ]);
    let native = native(&words);
    ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build")
}

#[test]
fn a_skip_whose_parked_scalars_pass_an_inner_branch_unchanged_is_left_out() {
    let p = parked_across_an_inner_branch(0xBE96_0314); // s_mov_b32 s22, s20
    assert_eq!(branches_named(&p, "s_cbranch_execz"), 1);
    // (s22 comes back through a phi of the inner join whose two arguments are copies of one
    // value: only `deep` follows it)
    assert!(ir::without_skips(&p, false).is_none());
    let deep = ir::without_skips(&p, true).expect("left out");
    assert_eq!(branches_named(&deep, "s_cbranch_execz"), 0);
}

#[test]
fn a_skip_whose_parked_scalar_an_inner_branch_changes_is_kept() {
    let p = parked_across_an_inner_branch(0xBE96_0387); // s_mov_b32 s22, 7
    assert_eq!(branches_named(&p, "s_cbranch_execz"), 1);
    assert!(ir::without_skips(&p, false).is_none());
    assert!(ir::without_skips(&p, true).is_none());
}

/// A console wave32 program with console lanes (`v_mbcnt`) in a 32 x 2 group: thread (x, y)
/// stores 1 at dword 32y + x. Every dword must be written once: the host's relabelled thread
/// ids are a bijection of the group. A 32-lane host need not give a host wave a 32-aligned run
/// of flat ids (2-D groups are filled from 16 x 2 tiles on NVIDIA and RDNA), so the first flat
/// id of a wave rounded down gave two waves the same ids (issue #1243); the waves now take
/// their threads from a group counter. With `VICTORY_THREAD_TICKET=0` (the old rounding) the
/// count of dwords written is printed, not checked: it depends on the card.
#[test]
fn the_console_threads_of_a_two_dimensional_group_are_each_given_once() {
    let words = [
        0xD765_0003, // v_mbcnt_lo_u32_b32 v3, exec_lo, 0
        0x0001_007E,
        0x3408_0285, // v_lshlrev_b32 v4, 5, v1
        0x4A08_0104, // v_add_nc_u32 v4, v4, v0
        0x3408_0882, // v_lshlrev_b32 v4, 2, v4
        0x7E0A_0281, // v_mov_b32 v5, 1
        0x38 << 26 | 0x1C << 18 | 1 << 12,
        0x80 << 24 | 2 << 16 | 5 << 8 | 4, // buffer_store_dword v5, v4, s[8:11], 0 offen
    ];
    let mut native = native_of(&words, false, 64);
    native.io = StageIo::Cs(ComputeIo {
        rsrc2: 5 << 1 | 1 << 11, // 5 user SGPRs, thread ids x and y in v0, v1
        threads: [32, 2, 1],
    });
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let _one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let lower_with = |ticket: bool| {
        // SAFETY: tests that read this switch hold ONE_AT_A_TIME
        unsafe { std::env::set_var("VICTORY_THREAD_TICKET", if ticket { "1" } else { "0" }) };
        let shader = lowered_on(&native, &p, &HashSet::new(), 32);
        unsafe { std::env::remove_var("VICTORY_THREAD_TICKET") };
        shader.expect("lower")
    };
    let shader = lower_with(true);
    let old = lower_with(false);
    assert_ne!(
        shader.dxil, old.dxil,
        "the counter is used for a group of two host waves"
    );
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        let mut copy = shader.dxil.clone();
        v.validate(&mut copy).expect("DXIL validation failed");
    }
    let Ok(host) = Host::new(false) else {
        eprintln!("no SM 6.6 hardware device; not run");
        return;
    };
    let run = |shader: &HostShader| -> Option<Vec<u32>> {
        let (root, pso) = match host.compute_pipeline(shader) {
            Ok(x) => x,
            Err(e) => {
                eprintln!("the hardware device does not take this program: {e}; not run");
                return None;
            }
        };
        let ud = vec![0u32; shader.user_data_dwords().max(1) as usize];
        let out = host
            .dispatch(
                &root,
                &pso,
                &Dispatch {
                    roots: vec![(0, Root::Constants(ud))],
                    views: &[View::RawUav(64)],
                    samplers: &[],
                    groups: [1, 1, 1],
                },
            )
            .unwrap_or_else(|e| panic!("dispatch failed: {e}"));
        Some(out[0][..64].to_vec())
    };
    let Some(got) = run(&shader) else { return };
    let missing: Vec<usize> = (0..64).filter(|&i| got[i] != 1).collect();
    assert!(missing.is_empty(), "dwords not written once: {missing:?}");
    if let Some(before) = run(&old) {
        let written = before.iter().filter(|&&d| d == 1).count();
        eprintln!("the old rounding on this device: {written} of 64 dwords written");
    }
    eprintln!("ran on the hardware device: 64 of 64 console threads, each once");
}

/// Lowers `body` as a console program of one 64-lane wave (or 32 when `wave64` is not set)
/// for a host of 32-lane waves, validates it and runs it there; each lane stores v3 to dword
/// v0 of the raw UAV. None where the host device does not run it.
fn permuted(body: &[u32], wave64: bool) -> Option<Vec<u32>> {
    let lanes = if wave64 { 64 } else { 32 };
    let mut words = body.to_vec();
    words.extend_from_slice(&[
        0x340C_0082, // v_lshlrev_b32 v6, 2, v0
        0xE070_1000,
        0x8002_0306, // buffer_store_dword v3, v6, s[8:11], 0 offen
    ]);
    let native = native_of(&words, wave64, lanes);
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let shader = lowered(&native, &p, &HashSet::new()).expect("lower");
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        let mut copy = shader.dxil.clone();
        v.validate(&mut copy).expect("DXIL validation failed");
    }
    // The same program for a host of 64-lane waves lowers too.
    let wide = lowered_on(&native, &p, &HashSet::new(), 64).expect("lower for 64-lane waves");
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        let mut copy = wide.dxil.clone();
        v.validate(&mut copy)
            .expect("DXIL validation failed (64-lane waves)");
    }
    let _one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let Ok(host) = Host::new(false) else {
        eprintln!("no SM 6.6 hardware device; not run");
        return None;
    };
    let Ok((root, pso)) = host.compute_pipeline(&shader) else {
        eprintln!("the hardware device does not take this program; not run");
        return None;
    };
    let out = host
        .dispatch(
            &root,
            &pso,
            &Dispatch {
                roots: vec![(
                    0,
                    Root::Constants(vec![0; shader.user_data_dwords().max(1) as usize]),
                )],
                views: &[View::RawUav(lanes)],
                samplers: &[],
                groups: [1, 1, 1],
            },
        )
        .unwrap_or_else(|e| panic!("dispatch failed: {e}"));
    Some(out[0][..lanes as usize].to_vec())
}

/// A wave64 `ds_bpermute_b32` permutes lanes 0..31 and 32..63 apart (RDNA 2 ISA, section
/// 10.4.4; upstream 0665a0e8, #573): v1 = ((lane & 31) << 2) + 0x103 with offset 12 drops the
/// high and sub-dword address bits and wraps inside each half (lanes 29..31 read 0..2, lanes
/// 61..63 read 32..34, lane 32 reads lane 35, not lane 3). Run in host waves of 32, the
/// program needs no exchange between them.
#[test]
fn a_wave64_backward_permute_stays_within_each_32_lane_half() {
    let body = [
        0x4A04_00FF,
        0x0000_4000, // v_add_nc_u32 v2, 0x4000, v0
        0x3602_009F, // v_and_b32 v1, 31, v0
        0x3402_0282, // v_lshlrev_b32 v1, 2, v1
        0x4A02_02FF,
        0x0000_0103, // v_add_nc_u32 v1, 0x103, v1
        0xDACC_000C,
        0x0300_0201, // ds_bpermute_b32 v3, v1, v2 offset:12
    ];
    let Some(got) = permuted(&body, true) else {
        return;
    };
    let want: Vec<u32> = (0..64)
        .map(|l| 0x4000 + ((l & !31) | ((l + 3) & 31)))
        .collect();
    assert_eq!(got, want);
    eprintln!("ran on the hardware device: each half permuted on its own");
}

/// EXEC is per half too: with lanes 0..39 active, lanes 32..39 read upper-half lanes 40..47,
/// which are inactive, and get 0 although lower-half lanes 8..15 are active. Lanes 40..63
/// keep their v3.
#[test]
fn a_wave64_backward_permute_reads_exec_within_each_half() {
    let body = [
        0x4A04_00FF,
        0x0000_5000, // v_add_nc_u32 v2, 0x5000, v0
        0x4A06_00FF,
        0xCAFE_0000, // v_add_nc_u32 v3, 0xcafe0000, v0
        0x4A02_0088, // v_add_nc_u32 v1, 8, v0
        0x3602_029F, // v_and_b32 v1, 31, v1
        0x3402_0282, // v_lshlrev_b32 v1, 2, v1
        0x7D88_00A8, // v_cmp_gt_u32 vcc, 40, v0
        0xBE94_246A, // s_and_saveexec_b64 s[20:21], vcc
        0xDACC_0000,
        0x0300_0201, // ds_bpermute_b32 v3, v1, v2
        0xBEFE_0414, // s_mov_b64 exec, s[20:21]
    ];
    let Some(got) = permuted(&body, true) else {
        return;
    };
    let want: Vec<u32> = (0..64)
        .map(|l| match l {
            0..32 => 0x5000 + ((l + 8) & 31),
            32..40 => 0,
            _ => 0xCAFE_0000 + l,
        })
        .collect();
    assert_eq!(got, want);
    eprintln!("ran on the hardware device: inactive lanes of the same half read 0");
}

/// A wave32 backward permute names lanes by bits 6:2 of the address: the high bits (0x100)
/// and sub-dword bits (+3) are dropped.
#[test]
fn a_wave32_backward_permute_drops_the_high_and_sub_dword_address_bits() {
    let body = [
        0x4A04_00FF,
        0x0000_3000, // v_add_nc_u32 v2, 0x3000, v0
        0x4A02_0088, // v_add_nc_u32 v1, 8, v0
        0x3402_0282, // v_lshlrev_b32 v1, 2, v1
        0x4A02_02FF,
        0x0000_0103, // v_add_nc_u32 v1, 0x103, v1
        0xDACC_0000,
        0x0300_0201, // ds_bpermute_b32 v3, v1, v2
    ];
    let Some(got) = permuted(&body, false) else {
        return;
    };
    let want: Vec<u32> = (0..32).map(|l| 0x3000 + ((l + 8) & 31)).collect();
    assert_eq!(got, want);
}

/// RDNA 2 DPP16 and DPP8 inactive source-lane semantics (`EXEC`, `bound_ctrl`, `fi`):
/// - With `fi:0` and `bound_ctrl:0`, reading an inactive source lane leaves `vdst` untouched.
/// - With `fi:0` and `bound_ctrl:1` (DPP16), reading an inactive source lane supplies `0`.
/// - With `fi:1` (fetch-inactive), reading an inactive source lane reads its VGPR value.
/// - Reading an active source lane transfers its live value.
/// Tested in guarded wave32, divergent-branched wave32, and split wave64-on-wave32.
#[test]
fn dpp16_and_dpp8_honor_inactive_source_lanes_and_fi() {
    // dpp8:[1,0,3,2,5,4,7,6] (each lane swaps with its odd/even neighbour)
    const DPP8_SWAP1: u32 = 1 | 0 << 3 | 3 << 6 | 2 << 9 | 5 << 12 | 4 << 15 | 7 << 18 | 6 << 21;
    let dpp16 = |ctrl: u32, bound_ctrl: bool, fi: bool| -> u32 {
        0xFF00_0000 | u32::from(bound_ctrl) << 19 | u32::from(fi) << 18 | ctrl << 8 | 1
    };
    let emitted = |native: &NativeProgram, host_wave: u32| -> HostShader {
        let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
        let shader = lowered_on(native, &p, &HashSet::new(), host_wave)
            .unwrap_or_else(|e| panic!("lower on {host_wave}-lane host: {e}"));
        if let Ok(mut v) =
            validator::Validator::load(validator::Validator::default_path().as_deref())
        {
            let mut copy = shader.dxil.clone();
            v.validate(&mut copy).expect("DXIL validation failed");
        }
        shader
    };

    let build_guarded = |wave64: bool| -> Vec<u32> {
        let mut w = vop3(0x169, 1, v(0), k(10)).to_vec(); // v_mul_lo_u32 v1, v0, 10
        w.extend([
            vop2(0x25, 1, k(1), 1), // v_add_nc_u32 v1, 1, v1 (10*lane + 1)
            vop1(0x01, 4, LIT),
            0xAAAA,
            vop1(0x01, 5, LIT),
            0xBBBB,
            vop1(0x01, 6, LIT),
            0xCCCC,
            vop1(0x01, 7, LIT),
            0xDDDD,
            vop2(0x1B, 2, k(1), 0), // v_and_b32 v2, 1, v0
            vopc(0xC2, k(0), 2),    // v_cmp_eq_u32 vcc, 0, v2 (even lanes active)
            sop1(if wave64 { 0x24 } else { 0x3C }, 12, VCC), // s_and_saveexec_b64/b32 s12, vcc
            // 1. DPP16 quad_perm:[1,0,3,2] (0xB1) bound_ctrl:0 fi:0 -> inactive odd source: keep v4
            vop1(0x01, 4, 0xFA),
            dpp16(0xB1, false, false),
            // 2. DPP16 quad_perm:[1,0,3,2] (0xB1) bound_ctrl:1 fi:0 -> inactive odd source: write 0
            vop1(0x01, 5, 0xFA),
            dpp16(0xB1, true, false),
            // 3. DPP16 quad_perm:[1,0,3,2] (0xB1) bound_ctrl:0 fi:1 -> fetch inactive odd source
            vop1(0x01, 6, 0xFA),
            dpp16(0xB1, false, true),
            // 4. DPP8 swap-1 fi:0 (0xE9) -> inactive odd source: keep v7
            vop1(0x01, 7, 0xE9),
            DPP8_SWAP1 << 8 | 1,
            // 5. DPP16 quad_perm:[2,3,0,1] (0x4E) bound_ctrl:0 fi:0 -> active even source added to v5 (0)
            vop2(0x25, 5, 0xFA, 5),
            dpp16(0x4E, false, false),
            sop1(if wave64 { 0x04 } else { 0x03 }, EXEC, 12),
        ]);
        w.extend(stores([4, 5, 6, 7]));
        w
    };

    let want_for = |lanes: u32| -> Vec<u32> {
        let val = |n: u32| 10 * n + 1;
        (0..lanes)
            .flat_map(|n| {
                if n % 2 == 0 {
                    [0xAAAA, val(n ^ 2), val(n ^ 1), 0xDDDD]
                } else {
                    [0xAAAA, 0xBBBB, 0xCCCC, 0xDDDD]
                }
            })
            .collect()
    };

    let _one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());

    // 1. Guarded wave32 compute
    let shader32 = emitted(&native_of(&build_guarded(false), false, 32), 32);
    if let Some(got32) = run_paired(&shader32, [0; 4], 128) {
        same_as_console(&got32, &want_for(32));
    }

    // 2. Divergent wave32 branch (guard == None; odd host lanes branched away)
    let mut w32_br = vop3(0x169, 1, v(0), k(10)).to_vec();
    w32_br.extend([
        vop2(0x25, 1, k(1), 1),
        vop1(0x01, 4, LIT),
        0xAAAA,
        vop1(0x01, 5, LIT),
        0xBBBB,
        vop1(0x01, 6, LIT),
        0xCCCC,
        vop1(0x01, 7, LIT),
        0xDDDD,
        vop2(0x1B, 2, k(1), 0),
        vopc(0xC2, k(0), 2),
        sop1(0x3C, 12, VCC), // s_and_saveexec_b32 s12, vcc
        sopp(0x08, 8),       // s_cbranch_execz over 4 x 2-dword DPP instructions
        vop1(0x01, 4, 0xFA),
        dpp16(0xB1, false, false),
        vop1(0x01, 5, 0xFA),
        dpp16(0xB1, true, false),
        vop1(0x01, 7, 0xE9),
        DPP8_SWAP1 << 8 | 1,
        vop2(0x25, 5, 0xFA, 5),
        dpp16(0x4E, false, false),
        sop1(0x03, EXEC, 12),
    ]);
    w32_br.extend(stores([4, 5, 6, 7]));
    let shader32_br = emitted(&native_of(&w32_br, false, 32), 32);
    let want32_br: Vec<u32> = (0..32u32)
        .flat_map(|n| {
            if n % 2 == 0 {
                [0xAAAA, 10 * (n ^ 2) + 1, 0xCCCC, 0xDDDD]
            } else {
                [0xAAAA, 0xBBBB, 0xCCCC, 0xDDDD]
            }
        })
        .collect();
    if let Some(got32_br) = run_paired(&shader32_br, [0; 4], 128) {
        same_as_console(&got32_br, &want32_br);
    }

    // 3. Guarded wave64 compute on both 64-lane and 32-lane hosts
    let native64 = native_of(&build_guarded(true), true, 64);
    for host_wave in [64, 32] {
        let shader64 = emitted(&native64, host_wave);
        if let Some(got64) = run_paired(&shader64, [0; 4], 256) {
            same_as_console(&got64, &want_for(64));
        }
    }
    eprintln!("ran on the hardware device: DPP16 and DPP8 honored EXEC, bound_ctrl, and fi");
}

/// RDNA 2 `v_permlane16_b32` and `v_permlanex16_b32` inactive source-lane semantics
/// (`op_sel[0]` = `fi`, `op_sel[1]` = `bound_ctrl`):
/// - `fi:0 bound_ctrl:0`: reading an inactive source lane leaves `vdst` untouched.
/// - `fi:0 bound_ctrl:1`: reading an inactive source lane writes `0`.
/// - `fi:1`: fetches the inactive source lane's VGPR.
/// - Reading an active source lane transfers its live value.
/// Tested in guarded wave32, divergent-branch wave32, and paired wave64 on a 32-lane host.
#[test]
fn permlane16_and_x16_honor_inactive_source_lanes_fi_and_bound_ctrl() {
    // VOP3 with op_sel (bit 0 = fi, bit 1 = bound_ctrl)
    let vop3_sel = |op: u32, dst: u32, src0: u32, src1: u32, src2: u32, opsel: u32| -> [u32; 2] {
        [
            0xD400_0000 | op << 16 | opsel << 11 | dst,
            src0 | src1 << 9 | src2 << 18,
        ]
    };
    // Selector swapping even/odd pairs within each row of 16: nibble i = i ^ 1
    // lanes 0..7: 0x6745_2301, lanes 8..15: 0xEFCD_AB89
    const SEL_ODD_LO: u32 = 0x6745_2301;
    const SEL_ODD_HI: u32 = 0xEFCD_AB89;
    // Selector swapping by 2 (even -> active even): nibble i = i ^ 2
    // lanes 0..7: 0x5476_1032, lanes 8..15: 0xDCFE_98BA
    const SEL_EVEN_LO: u32 = 0x5476_1032;
    const SEL_EVEN_HI: u32 = 0xDCFE_98BA;

    let build_body = |wave64: bool, branch: bool, cross: bool| -> Vec<u32> {
        let op = if cross { 0x378 } else { 0x377 };
        let mut w = vop3(0x169, 1, v(0), k(10)).to_vec(); // v_mul_lo_u32 v1, v0, 10
        w.extend([
            vop2(0x25, 1, k(1), 1), // v1 = 10*lane + 1
            vop1(0x01, 4, LIT),
            0xAAAA, // v4 sentinel
            vop1(0x01, 5, LIT),
            0xBBBB, // v5 sentinel
            vop1(0x01, 6, LIT),
            0xCCCC, // v6 sentinel
            vop1(0x01, 7, LIT),
            0xDDDD,                 // v7 sentinel
            vop2(0x1B, 2, k(1), 0), // v2 = v0 & 1
            vopc(0xC2, k(0), 2),    // vcc = (v0 % 2 == 0)
            sop1(if wave64 { 0x24 } else { 0x3C }, 12, VCC),
        ]);
        if branch {
            // 4 x 2-dword v_permlane* instructions (or 3 when skipping fi:1)
            w.push(sopp(0x08, if wave64 { 8 } else { 6 }));
        }
        // 1. fi:0, bc:0 reading inactive odd lane -> preserve v4 (0xAAAA)
        w.extend(vop3_sel(op, 4, v(1), 0, 1, 0b00));
        // 2. fi:0, bc:1 reading inactive odd lane -> write 0 into v5
        w.extend(vop3_sel(op, 5, v(1), 0, 1, 0b10));
        // 3. fi:0, bc:0 reading active even lane (n ^ 2, other row if cross) -> write live value into v7
        w.extend(vop3_sel(op, 7, v(1), 2, 3, 0b00));
        // 4. fi:1, bc:0 reading inactive odd lane (in guarded / paired modes where inactive lanes stay live)
        if !branch || wave64 {
            w.extend(vop3_sel(op, 6, v(1), 0, 1, 0b01));
        }
        w.push(sop1(if wave64 { 0x04 } else { 0x03 }, EXEC, 12));
        w.extend(stores([4, 5, 6, 7]));
        w
    };

    let want_for = |lanes: u32, cross: bool, check_fi: bool| -> Vec<u32> {
        let val = |n: u32| 10 * n + 1;
        let row_flip = if cross { 16 } else { 0 };
        (0..lanes)
            .flat_map(|n| {
                if n % 2 == 0 {
                    let v6 = if check_fi {
                        val((n ^ row_flip) ^ 1)
                    } else {
                        0xCCCC
                    };
                    [0xAAAA, 0, v6, val((n ^ row_flip) ^ 2)]
                } else {
                    [0xAAAA, 0xBBBB, 0xCCCC, 0xDDDD]
                }
            })
            .collect()
    };

    let consts = [SEL_ODD_LO, SEL_ODD_HI, SEL_EVEN_LO, SEL_EVEN_HI];
    let _one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());

    for cross in [false, true] {
        // 1. Guarded wave32 compute
        let w32 = build_body(false, false, cross);
        let native32 = native_of(&w32, false, 32);
        let p32 = ir::build(&native32.code, native32.wave64, native32.vgprs).expect("ir::build");
        let shader32 = lowered(&native32, &p32, &HashSet::new()).expect("lower wave32");
        if let Ok(mut v) =
            validator::Validator::load(validator::Validator::default_path().as_deref())
        {
            let mut copy = shader32.dxil.clone();
            v.validate(&mut copy).expect("DXIL validation failed");
        }
        let Some(got32) = run_paired(&shader32, consts, 128) else {
            return;
        };
        same_as_console(&got32, &want_for(32, cross, true));

        // 2. Divergent-branch wave32 compute
        let w32_br = build_body(false, true, cross);
        let native32_br = native_of(&w32_br, false, 32);
        let p32_br =
            ir::build(&native32_br.code, native32_br.wave64, native32_br.vgprs).expect("ir::build");
        let shader32_br =
            lowered(&native32_br, &p32_br, &HashSet::new()).expect("lower wave32 branch");
        if let Ok(mut v) =
            validator::Validator::load(validator::Validator::default_path().as_deref())
        {
            let mut copy = shader32_br.dxil.clone();
            v.validate(&mut copy).expect("DXIL validation failed");
        }
        let Some(got32_br) = run_paired(&shader32_br, consts, 128) else {
            return;
        };
        same_as_console(&got32_br, &want_for(32, cross, false));

        // 3. Paired wave64 compute on a 32-lane host
        let w64 = build_body(true, true, cross);
        let shader64 = paired(&native_with(&w64, true, 64, 0));
        let Some(got64) = run_paired(&shader64, consts, 256) else {
            return;
        };
        same_as_console(&got64, &want_for(64, cross, true));
    }
    eprintln!(
        "ran on the hardware device: v_permlane16 and v_permlanex16 honored EXEC, fi, and bound_ctrl in wave32 and paired wave64"
    );
}

#[test]
fn empty_and_copied_wave_masks_can_be_read_in_a_lane_branch() {
    // Threads below s0 read both dwords of a scalar mask and store 1 + (lo ^ hi).
    // The XOR distinguishes an empty upper half from a partly active one.
    let lower_mask = |op, on| {
        let native = native(&[
            0x7D88_0000, // v_cmp_gt_u32 vcc, s0, v0
            0xBE8C_246A, // s_and_saveexec_b64 s[12:13], vcc
            0xBF88_0007, // s_cbranch_execz to the restore of EXEC
            op,
            0x7E0A_020E, // v_mov_b32 v5, s14
            0x7E0C_020F, // v_mov_b32 v6, s15
            0x3A0A_0D05, // v_xor_b32 v5, v5, v6
            0x4A0A_0A81, // v_add_nc_u32 v5, 1, v5
            0x38 << 26 | 0x1C << 18,
            0x80 << 24 | 2 << 16 | 5 << 8, // buffer_store_dword v5, off, s[8:11], 0
            0xBEFE_040C,                   // s_mov_b64 exec, s[12:13]
        ]);
        let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
        let previous = lower::TEST_ZERO_MASK.replace(Some(on));
        let result = lowered(&native, &p, &HashSet::new());
        lower::TEST_ZERO_MASK.set(previous);
        result
    };
    const XOR_EXEC: u32 = 0x898E_7E7E; // s_xor_b64 s[14:15], exec, exec
    let baseline = lower_mask(XOR_EXEC, false);
    assert!(matches!(
        baseline,
        Err(ShaderError::Unsupported { what, .. }) if what == "a lane mask exchanged where not every lane runs"
    ));
    let dynamic = lower_mask(0x878E_817E, true); // s_and_b64 s[14:15], exec, 1
    assert!(
        matches!(&dynamic, Err(ShaderError::Unsupported { what, .. }) if *what == "a lane mask exchanged where not every lane runs"),
        "{:?}",
        dynamic.err()
    );
    let empty = lower_mask(XOR_EXEC, true).expect("empty mask");
    let copied = lower_mask(0x888E_7E7E, true).expect("copy of EXEC");
    let _one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    for (shader, copied_mask) in [(&empty, false), (&copied, true)] {
        if let Ok(mut v) =
            validator::Validator::load(validator::Validator::default_path().as_deref())
        {
            v.validate(&mut shader.dxil.clone())
                .expect("DXIL validation failed");
        }
        for below in [1, 32, 40, 64] {
            let bits = if copied_mask {
                u64::MAX >> (64 - below)
            } else {
                0
            };
            let expected = ((bits as u32) ^ (bits >> 32) as u32).wrapping_add(1);
            if let Some(actual) = on_the_card(shader, [below, 0]) {
                assert_eq!(actual, expected, "{below} active lanes");
            }
        }
    }
}

#[test]
fn automatic_barriers_keep_fences_and_only_drop_unsafe_sync() {
    for writes in [false, true] {
        let fence = 8 | if writes { 2 } else { 0 };
        for (host, console, threads, divergent, waits) in [
            (32, 64, Some(64), true, false),
            (32, 64, Some(64), false, true),
            (32, 64, Some(128), true, true),
            (32, 64, None, true, true),
            (64, 64, Some(64), true, true),
            (32, 32, Some(32), true, true),
            (0, 64, Some(64), true, true),
        ] {
            assert_eq!(
                lower::barrier_mode(3, host, console, threads, divergent, writes),
                fence | u32::from(waits),
            );
        }
        assert_eq!(
            lower::barrier_mode(0, 32, 64, Some(64), false, writes),
            fence
        );
        assert_eq!(
            lower::barrier_mode(1, 64, 64, Some(64), true, writes),
            fence
        );
        assert_eq!(
            lower::barrier_mode(1, 32, 64, Some(64), false, writes),
            fence | 1
        );
        assert_eq!(
            lower::barrier_mode(2, 32, 64, Some(64), true, writes),
            fence | 1
        );
    }
}

#[test]
fn automatic_barrier_flags_reach_the_emitted_shader() {
    const S_ENDPGM: u32 = 0xBF81_0000;
    let divergent = [
        0x7D88_0090, // v_cmp_gt_u32 vcc, 16, v0
        0xBE8C_246A, // s_and_saveexec_b64 s[12:13], vcc
        0xBF88_0001, // s_cbranch_execz past the barrier
        0xBF8A_0000, // s_barrier
        0xBEFE_040C, // s_mov_b64 exec, s[12:13]
    ];
    let mut divergent32 = divergent;
    divergent32[1] = 0xBE8C_3C6A; // s_and_saveexec_b32 s12, vcc_lo
    divergent32[4] = 0xBEFE_030C; // s_mov_b32 exec_lo, s12
    for (body, wave64, threads, host, expected) in [
        (&divergent[..], true, 64, 32, 8),
        (&divergent[..], true, 128, 32, 9),
        (&divergent[..], true, 64, 64, 9),
        (&divergent32[..], false, 64, 32, 9),
        (&[0xBF8A_0000][..], true, 64, 32, 9),
    ] {
        let native = NativeProgram {
            stage: HwStage::Cs,
            wave64,
            vgprs: 16,
            layout: vec![],
            used: vec![],
            io: StageIo::Cs(ComputeIo {
                rsrc2: 0,
                threads: [threads, 1, 1],
            }),
            code: body
                .iter()
                .copied()
                .chain([S_ENDPGM])
                .flat_map(u32::to_le_bytes)
                .collect(),
        };
        let p = ir::build(&native.code, wave64, native.vgprs).expect("IR");
        let st = structure::structure(&p).expect("structure");
        let res = resource::track(&p, &[], native.stage, Some(&st)).expect("resources");
        let io = io::raise(&p, &native).expect("IO");
        let old_wave = lower::TEST_HOST_WAVE.replace(Some(host));
        let old_mode = super::TEST_GROUP_SYNC.replace(Some(3));
        let shader = lower::lower(&lower::Parts {
            native: &native,
            p: &p,
            st: &st,
            io: &io,
            res: &res,
            inputs: vec![],
            outputs: vec![],
            prims: vec![],
            mesh_stride: 0,
        });
        lower::TEST_HOST_WAVE.set(old_wave);
        super::TEST_GROUP_SYNC.set(old_mode);
        let shader = shader.unwrap_or_else(|e| panic!("wave64={wave64}, host={host}: {e:?}"));
        assert_eq!(shader.barrier_modes, vec![expected]);
    }
}

/// `VICTORY_GROUP_SYNC_FULL` ([`super::sync_full`]): a program it names keeps the wait of a
/// divergent barrier that `auto` drops on a host of 32-lane waves (mode 8, the fence only, to 9),
/// and `VICTORY_GROUP_SYNC=0` still wins.
#[test]
fn a_named_program_keeps_the_wait_of_a_divergent_barrier() {
    let native = NativeProgram {
        stage: HwStage::Cs,
        wave64: true,
        vgprs: 16,
        layout: vec![],
        used: vec![],
        io: StageIo::Cs(ComputeIo {
            rsrc2: 0,
            threads: [64, 1, 1],
        }),
        code: [
            0x7D88_0090, // v_cmp_gt_u32 vcc, 16, v0
            0xBE8C_246A, // s_and_saveexec_b64 s[12:13], vcc
            0xBF88_0001, // s_cbranch_execz past the barrier
            0xBF8A_0000, // s_barrier
            0xBEFE_040C, // s_mov_b64 exec, s[12:13]
            S_ENDPGM,
        ]
        .into_iter()
        .flat_map(u32::to_le_bytes)
        .collect(),
    };
    let p = ir::build(&native.code, true, native.vgprs).expect("IR");
    let st = structure::structure(&p).expect("structure");
    let res = resource::track(&p, &[], native.stage, Some(&st)).expect("resources");
    let io = io::raise(&p, &native).expect("IO");
    let modes = |setting: u8, full: bool| {
        let old_wave = lower::TEST_HOST_WAVE.replace(Some(32));
        let old_mode = super::TEST_GROUP_SYNC.replace(Some(setting));
        let parts = lower::Parts {
            native: &native,
            p: &p,
            st: &st,
            io: &io,
            res: &res,
            inputs: vec![],
            outputs: vec![],
            prims: vec![],
            mesh_stride: 0,
        };
        let shader = if full {
            super::sync_full::with_here_for_test(|| lower::lower(&parts))
        } else {
            lower::lower(&parts)
        };
        lower::TEST_HOST_WAVE.set(old_wave);
        super::TEST_GROUP_SYNC.set(old_mode);
        shader.expect("lower").barrier_modes
    };
    assert_eq!(modes(3, false), vec![8]);
    assert_eq!(modes(3, true), vec![9]);
    assert_eq!(modes(0, true), vec![8]);
}

/// A barrier entered by only part of a console wave must complete on a
/// wave32 GPU without disabling uniform barriers or translation-added sync.
#[test]
fn an_automatic_barrier_inside_a_lane_branch_runs_on_narrow_hardware() {
    let words = [
        0x7D88_0000, // v_cmp_gt_u32 vcc, s0, v0
        0xBE8C_246A, // s_and_saveexec_b64 s[12:13], vcc
        0xBF88_0001, // s_cbranch_execz over the barrier
        0xBF8A_0000, // s_barrier
        0xBEFE_040C, // s_mov_b64 exec, s[12:13]
        0x7E00_0293, // v_mov_b32 v0, 19
        0x38 << 26 | 0x1C << 18,
        0x80 << 24 | 2 << 16, // buffer_store_dword v0, off, s[8:11], 0
    ];
    let native = native(&words);
    let p = ir::build(&native.code, true, native.vgprs).expect("ir::build");
    let old_mode = super::TEST_GROUP_SYNC.replace(Some(3));
    let shader = lowered(&native, &p, &HashSet::new());
    super::TEST_GROUP_SYNC.set(old_mode);
    let shader = shader.expect("lower divergent barrier");
    if let Ok(mut validator) =
        validator::Validator::load(validator::Validator::default_path().as_deref())
    {
        let mut dxil = shader.dxil.clone();
        validator.validate(&mut dxil).expect("DXIL validation");
    }
    let _one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let Ok(host) = Host::new(false) else {
        eprintln!("no SM 6.6 hardware device; not run");
        return;
    };
    let mut waves =
        windows::Win32::Graphics::Direct3D12::D3D12_FEATURE_DATA_D3D12_OPTIONS1::default();
    // SAFETY: a plain feature query on the live device.
    unsafe {
        host.device
            .CheckFeatureSupport(
                windows::Win32::Graphics::Direct3D12::D3D12_FEATURE_D3D12_OPTIONS1,
                (&raw mut waves).cast(),
                size_of_val(&waves) as u32,
            )
            .expect("host wave capabilities");
    }
    if waves.WaveLaneCountMax >= 64 || waves.WaveLaneCountMax == 0 {
        eprintln!("no narrow-wave hardware device; not run");
        return;
    }
    let (root, pso) = host
        .compute_pipeline(&shader)
        .expect("pipeline on narrow-wave hardware");
    for below in [1, 10, 32, 33, 64] {
        let mut ud = vec![0; shader.user_data_dwords().max(1) as usize];
        for u in &shader.user_data {
            if let (RootType::Constants, 0, 0) = (u.root, u.slot, u.dword) {
                ud[u.index as usize] = below;
            }
        }
        let out = host
            .dispatch(
                &root,
                &pso,
                &Dispatch {
                    roots: vec![(0, Root::Constants(ud))],
                    views: &[View::RawUav(1)],
                    samplers: &[],
                    groups: [1, 1, 1],
                },
            )
            .expect("divergent barrier dispatch completes");
        assert_eq!(out[0][0], 19, "{below} lanes enter the barrier");
    }
    eprintln!(
        "ran automatic divergent barriers on hardware (max wave {}): 1, 10, 32, 33 and 64 active lanes",
        waves.WaveLaneCountMax
    );
}

#[test]
fn a_first_lane_read_of_a_uniform_scan_carries_across_the_halves() {
    // A per-console-wave scan: the low half and the high half share the
    // scalar running sum and read the same LDS dword, so the first-lane
    // value is the same in both and needs no group barrier (which a group
    // of four console waves could not reach together anyway).
    let words = [
        0x2C0E_0086, // v_lshrrev_b32 v7, 6, v0
        0x7E10_0280, // v_mov_b32 v8, 0
        0xBE80_047E, // s_mov_b64 s[0:1], exec
        0x7DAA_0E80, // v_cmpx_ne_u32 exec, 0, v7
        0xBF88_0010, // s_cbranch_execz label_0015 (word 21)
        0xBE92_0380, // s_mov_b32 s18, 0
        0xBE91_0380, // s_mov_b32 s17, 0
        0xBE86_047E, // s_mov_b64 s[6:7], exec
        0xBF82_0000, // s_branch label_0009 (word 9)
        0x7E22_0508, // v_readfirstlane_b32 s17, v8
        0x8F10_8212, // s_lshl_b32 s16, s18, 2
        0x8112_8112, // s_add_i32 s18, s18, 1
        0x7E10_0210, // v_mov_b32 v8, s16
        0xD8D8_0000,
        0x0800_0008, // ds_read_b32 v8, v8
        0x4A10_1011, // v_add_nc_u32 v8, s17, v8
        0x7DA4_0E12, // v_cmpx_eq_u32 exec, s18, v7
        0x8A86_7E06, // s_andn2_b64 s[6:7], s[6:7], exec
        0xBF85_FFF6, // s_cbranch_scc1 label_0009
        0xE070_0000,
        0x8002_0800, // buffer_store_dword v8, off, s[8:11], 0
        0xBF81_0000, // s_endpgm
    ];
    let native = native_with(&words, true, 256, 256);
    let p = ir::build(&native.code, native.wave64, native.vgprs).unwrap();
    let old_wave = lower::TEST_HOST_WAVE.replace(Some(32));
    let shader = lowered(&native, &p, &HashSet::new());
    lower::TEST_HOST_WAVE.set(old_wave);
    let shader = shader.expect("uniform first-lane scan should lower on wave32");
    if let Ok(mut validator) =
        validator::Validator::load(validator::Validator::default_path().as_deref())
    {
        validator.validate(&mut shader.dxil.clone()).unwrap();
    }
}

/// Guarded `v_readfirstlane_b32` in halves mode exchanges `(has, mine)` in one
/// pair of barriers (`exchange_halves_two`) instead of two pairs (4 barriers),
/// while returning the upper half's first active lane when the lower half is
/// empty and the lower half's when both halves have active lanes.
#[test]
fn guarded_readfirstlane_in_halves_mode_runs_correctly_and_halves_barriers() {
    let mut w = vec![
        vop2(0x1B, 2, k(63), 0), // v_and_b32 v2, 63, v0 (lane)
        vop2(0x25, 4, k(10), 2), // v_add_nc_u32 v4, 10, v2 (lane + 10)
    ];
    // Force crosses=true so halves mode is chosen (v_readlane_b32 of lane 33).
    w.extend(vop3(0x360, 5, v(4), k(33)));
    w.extend([
        sop1(0x04, 24, EXEC), // s_mov_b64 s[24:25], exec
        vopc(0xD1, k(39), 2), // v_cmpx_lt_u32 exec, 39, v2: lanes 40-63 only
        vop1(0x02, 6, v(4)),  // v_readfirstlane_b32 s6, v4 -> 50
        sop1(0x04, EXEC, 24), // s_mov_b64 exec, s[24:25]
        vopc(0xD1, k(19), 2), // v_cmpx_lt_u32 exec, 19, v2: lanes 20-63
        vop1(0x02, 7, v(4)),  // v_readfirstlane_b32 s7, v4 -> 30
        sop1(0x04, EXEC, 24), // s_mov_b64 exec, s[24:25]
        vop1(0x01, 6, 6),     // v_mov_b32 v6, s6
        vop1(0x01, 7, 7),     // v_mov_b32 v7, s7
        vop1(0x01, 5, 5),     // v_mov_b32 v5, s5
    ]);
    w.extend(stores([5, 6, 7, 4]));
    let native = native_with(&w, true, 64, 0);
    let old_wave = lower::TEST_HOST_WAVE.replace(Some(32));
    let old_pairs = super::TEST_PAIRS.replace(Some(false));
    let old_collapse = super::TEST_COLLAPSE.replace(Some(true));
    let p = ir::build(&native.code, native.wave64, native.vgprs).unwrap();
    let res = lowered(&native, &p, &HashSet::new());
    super::TEST_COLLAPSE.set(old_collapse);
    super::TEST_PAIRS.set(old_pairs);
    lower::TEST_HOST_WAVE.set(old_wave);
    let shader = res.expect("guarded readfirstlane halves shader should lower");
    if let Ok(mut validator) =
        validator::Validator::load(validator::Validator::default_path().as_deref())
    {
        validator.validate(&mut shader.dxil.clone()).unwrap();
    }
    let want: Vec<u32> = (0..64).flat_map(|t| [43, 50, 30, t + 10]).collect();
    let _one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(got) = run_paired(&shader, [0; 4], 256) {
        same_as_console(&got, &want);
    }
}

/// 64 threads (one console wave): `if (t < s0) { if (t < s1) { n = WaveActiveCountBits(t ==
/// s2); store n ? 2 : 1 } }`, the inner skip (`s_cbranch_execz`) ending at the outer one's
/// join as the cascade-shadow rasters' inner skip ends with their outer one, or (`own_join`)
/// at a restore of EXEC of its own. `looped`: all of it in a uniform loop.
/// (Mikey970, PR #281 of trustno1-coder/VIctory-NVIDIA.)
fn count_behind_two_skips(own_join: bool, looped: bool) -> NativeProgram {
    let gate = [
        0x7D84_0002, // v_cmp_eq_u32 vcc, s2, v0
        0xBE90_106A, // s_bcnt1_i32_b64 s16, vcc
        0xBF06_8010, // s_cmp_eq_u32 s16, 0
        0x8511_8281, // s_cselect_b32 s17, 1, 2
        0x7E02_0211, // v_mov_b32 v1, s17
        0x3404_0082, // v_lshlrev_b32 v2, 2, v0
        0x38 << 26 | 0x1C << 18 | 1 << 12,
        0x80 << 24 | 2 << 16 | 1 << 8 | 2, // buffer_store_dword v1, v2, s[8:11], 0 offen
    ];
    let inner = gate.len() as u32;
    let outer = 3 + inner + u32::from(own_join);
    let mut words = vec![
        0x7D88_0000,         // v_cmp_gt_u32 vcc, s0, v0
        0xBE8C_246A,         // s_and_saveexec_b64 s[12:13], vcc
        0xBF88_0000 | outer, // s_cbranch_execz to the join
        0x7D88_0001,         // v_cmp_gt_u32 vcc, s1, v0
        0xBE8E_246A,         // s_and_saveexec_b64 s[14:15], vcc
        0xBF88_0000 | inner, // s_cbranch_execz to the join, or to its own
    ];
    words.extend_from_slice(&gate);
    if own_join {
        words.push(0xBEFE_040E); // s_mov_b64 exec, s[14:15]
    }
    words.push(0xBEFE_040C); // s_mov_b64 exec, s[12:13]
    if looped {
        words.push(0xBF06_8003); // s_cmp_eq_u32 s3, 0
        let back = -(words.len() as i32 + 1);
        words.push(0xBF85_0000 | (back as u32 & 0xFFFF)); // s_cbranch_scc1 to the start
    }
    native_of(&words, true, 64)
}

/// Where the program's `s_bcnt1_i32_b64` is.
fn the_count(p: &ir::Program) -> usize {
    let insts = p.blocks.iter().flat_map(|b| &b.insts);
    let count = insts.clone().find(|i| i.name() == "s_bcnt1_i32_b64");
    count.expect("the count").machine.offset
}

/// Whether [`super::emit`] on a host of 32-lane waves, with `VICTORY_WAVE_WIDE_AUTO` on or
/// off, took the wave-wide form ahead (None: it did not try it), and the shader (validated by
/// `dxil.dll` where there is one).
fn wide_auto_taken(native: &NativeProgram, on: bool) -> (Option<bool>, HostShader) {
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let st = structure::structure(&p).expect("structure");
    let res =
        resource::track(&p, &native.layout, native.stage, Some(&st)).expect("resource::track");
    let io = io::raise(&p, native).expect("io::raise");
    let bytes = crate::dxbc::build(&[]);
    let c = super::Container::parse(&bytes).expect("container");
    let was = lower::TEST_HOST_WAVE.replace(Some(32));
    let auto = lower::wide_auto::TEST_WIDE_AUTO.replace(Some(on));
    lower::wide_auto::TAKEN.set(None);
    let r = super::emit(&c, native, &p, &st, &io, &res);
    lower::wide_auto::TEST_WIDE_AUTO.set(auto);
    lower::TEST_HOST_WAVE.set(was);
    let shader = r.expect("translates");
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        let mut copy = shader.dxil.clone();
        v.validate(&mut copy).expect("DXIL validation failed");
    }
    (lower::wide_auto::TAKEN.get(), shader)
}

/// The cascade-shadow rasters' pattern: a wave-wide count behind an inner skip that ends
/// together with an outer one. Only the deep form of `ir::without_skips` leaves that skip out,
/// and "without skips, scalars followed" then places the exchange after per-lane branching
/// with no barrier. With `VICTORY_WAVE_WIDE_AUTO=1` such a program takes the wave-wide form
/// (and counts both halves on the card); one whose inner skip ends on its own does not try it;
/// and where the wave-wide form would put a barrier in a loop, it is not taken.
#[test]
fn a_count_behind_a_skip_that_ends_with_an_outer_one_takes_the_wave_wide_form() {
    let native = count_behind_two_skips(false, false);
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let count = the_count(&p);
    let first = p.blocks.iter().flat_map(|b| &b.insts).next().expect("an instruction");
    assert!(ir::deep_skip_holds(&p, count), "the count lies behind the deep-only skip");
    assert!(!ir::deep_skip_holds(&p, first.machine.offset), "the program's start does not");
    let (off_taken, off) = wide_auto_taken(&native, false);
    assert_eq!(off_taken, None, "off: not tried");
    let (taken, on) = wide_auto_taken(&native, true);
    assert_eq!(taken, Some(true));
    assert_ne!(off.dxil, on.dxil);
    let _one = super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    // s2 is 0: the count finds lane 0 (in the lower host wave), so every lane that runs it,
    // lanes 32 and up included, stores 2 (1 would be a half-wave's own count)
    for (s0, s1) in [(64u32, 48u32), (40, 64), (64, 64)] {
        let Some(out) = whole_uav(&on, [s0, s1], 64) else {
            return;
        };
        let runs = s0.min(s1) as usize;
        assert!(out[..runs].iter().all(|&v| v == 2), "threads below {s0} and {s1}: {out:?}");
    }

    let own = count_behind_two_skips(true, false);
    let own_taken = wide_auto_taken(&own, true).0;
    assert_eq!(own_taken, None, "an inner skip with a join of its own goes as before");

    let looped = count_behind_two_skips(false, true);
    let p = ir::build(&looped.code, looped.wave64, looped.vgprs).expect("ir::build");
    assert!(ir::deep_skip_holds(&p, the_count(&p)));
    assert_eq!(wide_auto_taken(&looped, true).0, Some(false), "a barrier in the loop: not taken");
}
