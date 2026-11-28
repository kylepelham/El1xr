use super::*;
use crate::native::{ComputeIo, HwStage, StageIo};

/// The encoders give the words the title's own code has for the same
/// instructions (read from the glass trace's listing), and the marker the
/// node-test tests' form.
#[test]
fn the_encoders_match_known_words() {
    assert_eq!(vop2(V_ADD_F32, 23, v(31), 32), 0x062e_411f); // v_add_f32 v23, v31, v32
    assert_eq!(sop2(S_ANDN2, VCC_LO, EXEC_LO, 65), 0x8a6a_417e); // s_andn2_b32 vcc_lo, exec_lo, s65
    assert_eq!(sop2(S_AND, 3, 69, VCC_LO), 0x8703_6a45); // s_and_b32 s3, s69, vcc_lo
    assert_eq!(sop2(S_OR, 7, 3, 7), 0x8807_0703); // s_or_b32 s7, s3, s7
    assert_eq!(cndmask(34, ZERO, ONE, VCC_LO), [0xd501_0022, 0x01a9_0280]); // v_cndmask_b32 v34, 0, 1, vcc_lo
    assert_eq!(v_mov(21, 31 + ZERO), 0x7e2a_029f); // v_mov_b32 v21, 31
    assert_eq!(v_mov(40, ONE), 0x7e50_0281); // v_mov_b32 v40, 1
    assert_eq!(s_mov(EXEC_LO, S54), 0xbefe_0336); // s_mov_b32 exec_lo, s54
    assert_eq!(s_cmp_lg(6, ZERO), 0xbf07_8006); // s_cmp_lg_u32 s6, 0
    assert_eq!(vopc(V_CMP_NE_U32, 31 + ZERO, 21), 0x7d8a_2a9f); // v_cmp_ne_u32 vcc_lo, 31, v21
    assert_eq!(sopp(S_CBRANCH_VCCNZ, SPLIT, PATH_B), 0xbf87_00a0); // the split itself
    assert_eq!(
        marker(13, 1, 8),
        [
            0xf19c_9f07,
            0x0002_0d01,
            0x0504_0302,
            0x0908_0706,
            0x000c_0b0a
        ]
    );
}

fn stand_in() -> NativeProgram {
    let mut code = vec![0u8; BYTES];
    for w in code.chunks_exact_mut(4) {
        w.copy_from_slice(&0xbf80_0000u32.to_le_bytes()); // s_nop 0
    }
    code[SPLIT..SPLIT + 4].copy_from_slice(&sopp(S_CBRANCH_VCCNZ, SPLIT, PATH_B).to_le_bytes());
    NativeProgram {
        stage: HwStage::Cs,
        wave64: false,
        vgprs: 86,
        layout: Vec::new(),
        used: Vec::new(),
        io: StageIo::Cs(ComputeIo {
            rsrc2: 0,
            threads: [8, 8, 1],
        }),
        code,
    }
}

#[test]
fn another_program_is_left_as_it_is() {
    let mut native = stand_in();
    let before = native.code.clone();
    assert!(
        !rewrite(&CONTAINER, &mut native),
        "a stand-in is not the title's program"
    );
    assert!(!rewrite(&[0; 16], &mut native));
    assert_eq!(native.code, before);
}

/// The splice on code of the known layout: the split becomes a branch to
/// the region at the end; the region decodes to the selector, the title's
/// own two branches, the arm with its four markers, and the jump to the
/// merge; nothing else moves.
#[test]
fn the_split_goes_to_the_region_and_the_region_decodes() {
    let mut native = stand_in();
    let before = native.code.clone();
    apply(&mut native);
    assert_eq!(native.vgprs, u16::from(at::END));
    assert_eq!(&native.code[..SPLIT], &before[..SPLIT]);
    assert_eq!(&native.code[SPLIT + 4..BYTES], &before[SPLIT + 4..]);
    let insts = crate::gfx10::decode(&native.code).expect("decodes");
    let text = |offset: usize| {
        insts
            .iter()
            .find(|i| i.offset == offset)
            .map(|i| i.text(false))
            .unwrap_or_default()
    };
    assert_eq!(text(SPLIT), format!("s_branch label_{BYTES:04X}"));
    assert_eq!(text(BYTES), "s_cmp_lg_u32 ttmp15, 0");
    assert_eq!(
        text(BYTES + 4),
        format!("s_cbranch_scc1 label_{:04X}", BYTES + 16)
    );
    assert_eq!(text(BYTES + 8), "s_cbranch_vccnz label_08F8");
    assert_eq!(text(BYTES + 12), "s_branch label_0678");
    assert_eq!(text(BYTES + 16), "s_mov_b32 s54, exec_lo");
    let region: Vec<_> = insts.iter().filter(|i| i.offset >= BYTES).collect();
    let markers: Vec<String> = region
        .iter()
        .filter(|i| i.name() == Some("image_bvh64_intersect_ray"))
        .map(|i| i.text(false))
        .collect();
    assert_eq!(markers.len(), 4, "{markers:?}");
    for (k, m) in markers.iter().enumerate() {
        assert!(m.contains(&format!("ttmp[{}:{}]", 4 * k, 4 * k + 3)), "{m}");
    }
    let last = region.last().expect("a region");
    assert_eq!(last.text(false), "s_branch label_9024");
    // Every instruction of the region decodes as a real one.
    assert!(region.iter().all(|i| i.name().is_some()));
}

/// The title's own program (not in the repository: set
/// `GLASS_TRACE_CONTAINER` to its container, as `victory translate`
/// dumps it): spliced, it translates through every stage, validates, asks
/// for ray tracing 1.1 and the host's glass word; unspliced, it still
/// translates as before.
#[test]
fn the_glass_trace_spliced_translates_and_validates() {
    let Some(path) = std::env::var_os("GLASS_TRACE_CONTAINER") else {
        eprintln!("GLASS_TRACE_CONTAINER not set: skipped");
        return;
    };
    let bytes = std::fs::read(path).expect("the container");
    let c = crate::dxbc::Container::parse(&bytes).expect("a container");
    let part = super::super::program_part(&c).expect("a program part");
    let mut native = crate::native_programs(part.data)
        .expect("native programs")
        .into_iter()
        .find(|p| p.stage == HwStage::Cs)
        .expect("a compute program");
    eprintln!(
        "glass trace: {} bytes, fnv {:#018x}",
        native.code.len(),
        fnv(&native.code)
    );
    assert!(
        rewrite(&c.digest, &mut native),
        "the glass trace is known byte for byte"
    );
    let p = crate::spill::build(&native).expect("spill::build");
    let st = crate::structure::structure(&p).expect("structure");
    let res = crate::resource::track(&p, &native.layout, native.stage, Some(&st)).expect("track");
    assert_eq!(
        res.bvh
            .iter()
            .filter(|b| matches!(b.1, crate::resource::BvhNodes::Native(_)))
            .count(),
        4
    );
    let io = crate::io::raise(&p, &native).expect("io");
    let shader = super::super::emit(&c, &native, &p, &st, &io, &res).expect("emit");
    let first = u32::from_le_bytes(shader.dxil[32..36].try_into().unwrap()) as usize;
    let features = u64::from_le_bytes(shader.dxil[first + 8..first + 16].try_into().unwrap());
    assert_ne!(features & 0x10_0000, 0, "RaytracingTier1_1");
    assert!(
        shader
            .user_data
            .iter()
            .any(|u| u.root
                == crate::native::RootType::Other(super::super::shader::USER_GLASS_NATIVE))
    );
    if let Ok(mut v) = super::super::validator::Validator::load(
        super::super::validator::Validator::default_path().as_deref(),
    ) {
        let mut copy = shader.dxil.clone();
        v.validate(&mut copy).expect("DXIL validation failed");
    }
}

/// A wave32 program that runs the native arm alone: each thread's record
/// (direction, the six origin halves, whether it is an L1/L3 lane) into the
/// registers the arm reads (`s3` from that flag), `s53` the user-data dword 2
/// (`CB 0x7c8`),
/// then the arm's results stored at 64 bytes a thread: `v40 v36 v41 v26`,
/// `v25 v35 v23 v24`, `v27 v21`.
fn arm_program() -> NativeProgram {
    use crate::native::{ResourceType, RootParam, RootType, UsedResource};
    let mut w = vec![
        0xbe85_0380, // s_mov_b32 s5, 0
        0xf408_0202,
        0xfa00_0000, // s_load_dwordx4 s[8:11], s[4:5], 0x0
        0xf408_0302,
        0xfa00_0020, // s_load_dwordx4 s[12:15], s[4:5], 0x20
        0x3428_0086, // v_lshlrev_b32 v20, 6, v0
        0xe038_1000,
        0x8002_3c14, // buffer_load_dwordx4 v[60:63], v20, s[8:11], 0 offen
        0xe038_1010,
        0x8002_4014, // ... v[64:67] offset:16
        0xe038_1020,
        0x8002_4414, // ... v[68:71] offset:32
        0xbf8c_3f70, // s_waitcnt vmcnt(0)
        s_mov(S53, 2),
    ];
    // s3: the lanes whose record word 9 is not 0, as a compare makes it.
    w.push(vopc(V_CMP_NE_U32, ZERO, 69));
    w.push(s_mov(S3, VCC_LO));
    for (dst, src) in [(0, 60), (7, 61), (9, 62), (31, 63), (32, 64), (39, 65), (37, 66), (30, 67), (33, 68)] {
        w.push(v_mov(dst, v(src)));
    }
    arm(&mut w);
    for (k, r) in [40, 36, 41, 26, 25, 35, 23, 24, 27, 21, 34, 22].into_iter().enumerate() {
        w.push(v_mov(119 + k as u8, v(r)));
    }
    w.extend([
        0xe078_1000,
        0x8003_7714, // buffer_store_dwordx4 v[119:122], v20, s[12:15], 0 offen
        0xe078_1010,
        0x8003_7b14, // ... v[123:126] offset:16
        0xe078_1020,
        0x8003_7f14, // ... v[127:130] offset:32
        0xbf81_0000, // s_endpgm
    ]);
    let table = |kind, at| UsedResource {
        kind,
        root: RootType::Table,
        table_offset_dwords: at,
        lower_bound: 0,
        stride: 4,
        dwords: 4,
        user_data: 4,
        slot: 1,
    };
    NativeProgram {
        stage: HwStage::Cs,
        wave64: false,
        vgprs: 131,
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
        used: vec![table(ResourceType::Srv, 0), table(ResourceType::Uav, 8)],
        io: StageIo::Cs(ComputeIo { rsrc2: 5 << 1, threads: [32, 1, 1] }),
        code: w.iter().flat_map(|w| w.to_le_bytes()).collect(),
    }
}

/// The two host top levels: `t354` (near, [`SLOT_TLAS_NEAR`]) one small
/// triangle at z = 5, contribution 20; `t355` (far, [`SLOT_TLAS`]) a large
/// one at z = 8 as its second instance, contribution 40.
fn arm_scenes() -> (super::super::host_tlas::Scene, super::super::host_tlas::Scene) {
    use super::super::host_tlas::{Instance, Scene};
    let near = Scene {
        geometries: vec![(vec![[[-1.0, -1.0, 5.0], [1.0, -1.0, 5.0], [-1.0, 1.0, 5.0]]], true)],
        instances: vec![Instance { offset: [0.0; 3], id: 1, contribution: 20, mask: 0xff }],
    };
    let far = Scene {
        geometries: vec![(vec![[[-50.0, -50.0, 8.0], [150.0, -50.0, 8.0], [-50.0, 150.0, 8.0]]], true)],
        instances: vec![
            Instance { offset: [1000.0, 0.0, 0.0], id: 2, contribution: 30, mask: 0xff },
            Instance { offset: [0.0; 3], id: 3, contribution: 40, mask: 0xff },
        ],
    };
    (near, far)
}

/// The native arm on the card against both host top levels: an `s3` lane
/// takes `t355` (and `v27`), another takes `t354`, and on its miss `t355`
/// only when `CB 0x7c8 > 0`; the result registers hold the hit or the
/// title's miss values, `v21` 31.
#[test]
fn the_native_arm_picks_its_top_level_per_lane_on_the_card() {
    use super::super::host::{Dispatch, Host, Root, Stride, View};
    use super::super::shader::{SLOT_TLAS, SLOT_TLAS_NEAR};
    let Ok(host) = Host::new(false) else {
        eprintln!("no SM 6.6 hardware device; skipped");
        return;
    };
    if !super::super::host_tlas::supported(&host.device) {
        eprintln!("no ray tracing tier 1.1; skipped");
        return;
    }
    let native = arm_program();
    let p = crate::ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let st = crate::structure::structure(&p).expect("structure");
    let res = crate::resource::track(&p, &native.layout, native.stage, Some(&st)).expect("track");
    assert_eq!(res.bvh.len(), 4);
    let io = crate::io::raise(&p, &native).expect("io");
    let out = super::super::lower::lower(&super::super::lower::Parts {
        native: &native,
        p: &p,
        st: &st,
        io: &io,
        res: &res,
        inputs: Vec::new(),
        outputs: Vec::new(),
        prims: Vec::new(),
        mesh_stride: 0,
    })
    .expect("the arm lowers");
    if let Ok(mut v) = super::super::validator::Validator::load(
        super::super::validator::Validator::default_path().as_deref(),
    ) {
        let mut copy = out.dxil.clone();
        v.validate(&mut copy).expect("DXIL validation failed");
    }
    let shader = super::super::HostShader {
        stage: HwStage::Cs,
        dxil: out.dxil,
        user_data: out.user_data,
        samplers: out.samplers,
        static_samplers: Vec::new(),
        root_cbvs: out.root_cbvs,
        root_srvs: out.root_srvs,
        root_uavs: out.root_uavs,
        root_indexed_views: res.root_indexed_views.clone(),
        half_views: out.half_views,
    };
    assert!(shader.root_cbvs.is_empty() && shader.root_uavs.is_empty());
    assert_eq!(shader.root_srvs.iter().copied().collect::<std::collections::BTreeSet<_>>(), [SLOT_TLAS_NEAR, SLOT_TLAS].into());
    let (root, pso) = host.compute_pipeline(&shader).expect("compute_pipeline");
    // Threads: (in s3, x, y, direction z).
    let lanes: [(bool, f32, f32, f32); 6] = [
        (true, -0.5, -0.5, 1.0),
        (false, -0.5, -0.5, 1.0),
        (false, 0.8, 0.8, 1.0),
        (true, 0.8, 0.8, 1.0),
        (false, 0.0, 0.0, -1.0),
        (true, 0.0, 0.0, -1.0),
    ];
    let mut record = vec![0u32; 16 * 32];
    for t in 0..32 {
        let (s3, x, y, dz) = lanes[t.min(lanes.len() - 1)];
        let r = &mut record[16 * t..];
        // Direction, then origin halves: v31 + v32, v39 + v37, v30 + v33.
        for (k, f) in [0.0, 0.0, dz, x - 1.0, 1.0, y - 2.0, 2.0, -3.0, 3.0].into_iter().enumerate() {
            r[k] = f32::to_bits(f);
        }
        r[9] = u32::from(s3);
    }
    let f = f32::to_bits;
    let miss = [0, f(-1.0), 0, 0, u32::MAX, u32::MAX, u32::MAX, u32::MAX, 0, 31];
    let near = [1, f(5.0), f(0.25), f(0.25), 0, 0, 0, 20, 0, 31];
    let far = |b: f32, s3: bool| [1, f(8.0), f(b), f(b), 1, 0, 0, 40, u32::from(s3), 31];
    for cb in [1.0f32, -1.0] {
        let mut user = vec![0u32; shader.user_data_dwords() as usize];
        user[2] = cb.to_bits();
        let (n, fa) = arm_scenes();
        let got = host
            .dispatch(
                &root,
                &pso,
                &Dispatch {
                    roots: vec![
                        (0, Root::Constants(user)),
                        (1, Root::Tlas(Some(n))),
                        (2, Root::Tlas(Some(fa))),
                    ],
                    views: &[View::Buffer { data: &record, stride: Stride::Raw }, View::RawUav(16 * 32)],
                    samplers: &[],
                    groups: [1, 1, 1],
                },
            )
            .expect("dispatch");
        let want = [
            far(0.2475, true),
            near,
            if cb > 0.0 { far(0.254, false) } else { miss },
            far(0.254, true),
            miss,
            miss,
        ];
        for (t, want) in want.iter().enumerate() {
            let got = &got[0][16 * t..16 * t + 12];
            let floats = [1, 2, 3];
            let same = (0..10).all(|k| {
                if floats.contains(&k) {
                    (f32::from_bits(got[k]) - f32::from_bits(want[k])).abs() < 1e-4
                } else {
                    got[k] == want[k]
                }
            });
            assert!(same, "CB {cb}, thread {t}: got {got:x?}, want {want:x?}");
            assert_eq!(&got[10..12], &[0, 0], "v34, v22");
        }
    }
}
