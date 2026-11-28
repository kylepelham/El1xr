use super::*;
use crate::native::{ComputeIo, HwStage, ResourceType, RootParam, RootType, StageIo, UsedResource};

fn stand_in(trace: &Trace, bytes: usize) -> NativeProgram {
    let mut code = vec![0u8; bytes];
    for w in code.chunks_exact_mut(4) {
        w.copy_from_slice(&0xbf80_0000u32.to_le_bytes()); // s_nop 0
    }
    for walk in trace.walks {
        code[walk.skip - 4..walk.skip].copy_from_slice(&walk.cmp.to_le_bytes());
        let skip = sopp(S_CBRANCH_SCC1, walk.skip, walk.past);
        code[walk.skip..walk.skip + 4].copy_from_slice(&skip.to_le_bytes());
    }
    NativeProgram {
        stage: HwStage::Cs,
        wave64: false,
        vgprs: 64,
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
    let mut native = stand_in(&CHECKERBOARD, 0x2400);
    let before = native.code.clone();
    assert!(
        !rewrite(&CHECKERBOARD.container, &mut native),
        "a stand-in is not the title's program"
    );
    assert!(!rewrite(&[0; 16], &mut native));
    assert_eq!(native.code, before);
}

#[test]
fn the_tables_agree() {
    assert_eq!(TRACES.iter().map(|t| t.hash).collect::<Vec<_>>(), SHADOW_TRACES);
    for t in TRACES {
        assert!(!t.walks.is_empty(), "{}", t.hash);
        for w in t.walks {
            assert!(
                w.skip < w.past && w.past <= w.merge && w.merge < t.bytes,
                "{}",
                t.hash
            );
            assert_eq!(w.cmp >> 23, 0x17e, "{}: a SOPC word", t.hash);
        }
    }
}

/// The splice on code of the known layout: the skip becomes a branch to the region; the
/// region decodes to the selector, the replayed comparison and skip, the way back into the
/// walk, the arm with one marker on `ttmp[0:3]` under the walk's lanes, and the jump to the
/// merge; nothing else moves.
#[test]
fn the_skip_goes_to_the_region_and_the_region_decodes() {
    let bytes = 0x2400;
    let mut native = stand_in(&CHECKERBOARD, bytes);
    let before = native.code.clone();
    apply(&CHECKERBOARD, &mut native);
    let walk = &CHECKERBOARD.walks[0];
    assert_eq!(native.vgprs, 64 + ARM_VGPRS);
    assert_eq!(&native.code[..walk.skip], &before[..walk.skip]);
    assert_eq!(&native.code[walk.skip + 4..bytes], &before[walk.skip + 4..]);
    let insts = crate::gfx10::decode(&native.code).expect("decodes");
    let text = |offset: usize| {
        insts
            .iter()
            .find(|i| i.offset == offset)
            .map(|i| i.text(false))
            .unwrap_or_default()
    };
    assert_eq!(text(walk.skip), format!("s_branch label_{bytes:04X}"));
    assert_eq!(text(bytes), "s_cmp_lg_u32 ttmp15, 0");
    assert_eq!(
        text(bytes + 4),
        format!("s_cbranch_scc1 label_{:04X}", bytes + 20)
    );
    assert_eq!(text(bytes + 8), "s_cmp_eq_u32 s22, 0");
    assert_eq!(text(bytes + 12), "s_cbranch_scc1 label_20B0");
    assert_eq!(text(bytes + 16), "s_branch label_1464");
    let region: Vec<_> = insts.iter().filter(|i| i.offset >= bytes).collect();
    let texts: Vec<String> = region.iter().map(|i| i.text(false)).collect();
    let marker = texts
        .iter()
        .position(|t| t.starts_with("image_bvh64_intersect_ray"))
        .expect("a marker");
    assert!(texts[marker].contains("ttmp[0:3]"), "{}", texts[marker]);
    assert_eq!(texts[marker - 1], "s_and_saveexec_b32 ttmp1, s22");
    assert_eq!(texts[marker + 1], "s_mov_b32 exec_lo, ttmp1");
    assert_eq!(
        texts.iter().filter(|t| t.starts_with("image_bvh")).count(),
        1
    );
    assert_eq!(texts.last().unwrap(), "s_branch label_20B8");
    assert!(region.iter().all(|i| i.name().is_some()), "{texts:?}");
}

/// The title's own programs (not in the repository: set `SHADOW_TRACE_DIR` to a folder of
/// their containers as `victory translate` dumps them, `<name>.<hash>.cdxil`): each known one,
/// spliced, translates, validates, has one marker per walk, asks for ray tracing 1.1 and the
/// host's word.
#[test]
fn the_shadow_traces_spliced_translate_and_validate() {
    let Some(dir) = std::env::var_os("SHADOW_TRACE_DIR") else {
        eprintln!("SHADOW_TRACE_DIR not set: skipped");
        return;
    };
    let files: Vec<_> = std::fs::read_dir(dir)
        .expect("the folder")
        .flatten()
        .map(|e| e.path())
        .collect();
    for trace in TRACES {
        let hash = trace.hash;
        let path = files
            .iter()
            .find(|p| p.to_string_lossy().contains(&format!(".{hash}.")))
            .unwrap_or_else(|| panic!("no container for {hash}"));
        let bytes = std::fs::read(path).expect("the container");
        let c = crate::dxbc::Container::parse(&bytes).expect("a container");
        let part = super::super::program_part(&c).expect("a program part");
        let mut native = crate::native_programs(part.data)
            .expect("native programs")
            .into_iter()
            .find(|p| p.stage == HwStage::Cs)
            .expect("a compute program");
        eprintln!(
            "{}: digest {:02x?}, {} bytes, fnv {:#018x}, {} vgprs",
            trace.hash,
            c.digest,
            native.code.len(),
            fnv(&native.code),
            native.vgprs
        );
        assert!(
            rewrite(&c.digest, &mut native),
            "{} is known byte for byte",
            trace.hash
        );
        let p = crate::spill::build(&native).expect("spill::build");
        let st = crate::structure::structure(&p).expect("structure");
        let res =
            crate::resource::track(&p, &native.layout, native.stage, Some(&st)).expect("track");
        let markers = res
            .bvh
            .iter()
            .filter(|b| matches!(b.1, crate::resource::BvhNodes::Native(_)))
            .count();
        assert_eq!(markers, trace.walks.len(), "{}", trace.hash);
        let io = crate::io::raise(&p, &native).expect("io");
        let shader = super::super::emit(&c, &native, &p, &st, &io, &res).expect("emit");
        let first = u32::from_le_bytes(shader.dxil[32..36].try_into().unwrap()) as usize;
        let features = u64::from_le_bytes(shader.dxil[first + 8..first + 16].try_into().unwrap());
        assert_ne!(features & 0x10_0000, 0, "{}: RaytracingTier1_1", trace.hash);
        assert!(
            shader
                .user_data
                .iter()
                .any(|u| u.root == RootType::Other(super::super::shader::USER_GLASS_NATIVE)),
            "{}",
            trace.hash
        );
        if let Ok(mut v) = super::super::validator::Validator::load(
            super::super::validator::Validator::default_path().as_deref(),
        ) {
            let mut copy = shader.dxil.clone();
            v.validate(&mut copy).expect("DXIL validation failed");
        }
    }
}

/// A wave32 program that runs `walk`'s arm alone: each thread's record (origin, direction,
/// extent, whether it is one of the walk's lanes) into the registers the walk keeps them in,
/// the lanes' SGPR from that flag as a compare makes it, `v17` 0 and `v24` 7 before; then
/// `v17 v15 v24` stored at 16 bytes a thread.
fn checkerboard_arm_program() -> NativeProgram {
    let walk = &MASKED;
    let mut w = vec![
        0xbe85_0380, // s_mov_b32 s5, 0
        0xf408_0202,
        0xfa00_0000, // s_load_dwordx4 s[8:11], s[4:5], 0x0
        0xf408_0302,
        0xfa00_0020, // s_load_dwordx4 s[12:15], s[4:5], 0x20
        0x3428_0085, // v_lshlrev_b32 v20, 5, v0
        0xe038_1000,
        0x8002_3c14, // buffer_load_dwordx4 v[60:63], v20, s[8:11], 0 offen
        0xe038_1010,
        0x8002_4014, // ... v[64:67] offset:16
        0xbf8c_3f70, // s_waitcnt vmcnt(0)
    ];
    let [ox, oy, oz] = walk.origin;
    let [dx, dy, dz] = walk.direction;
    for (dst, src) in [
        (ox, 60),
        (oy, 61),
        (oz, 62),
        (dx, 63),
        (dy, 64),
        (dz, 65),
        (walk.extent, 66),
    ] {
        w.push(v_mov(dst, v(src)));
    }
    w.push(vopc(V_CMP_NE_U32, ZERO, 67));
    w.push(s_mov(walk.lanes, VCC_LO));
    w.push(v_mov(17, ZERO));
    w.push(v_mov(24, 0x87)); // 7
    arm(&mut w, walk, 80);
    w.push(0x3428_0084); // v_lshlrev_b32 v20, 4, v0
    for (k, r) in [17u8, 15, 24].into_iter().enumerate() {
        w.push(v_mov(70 + k as u8, v(r)));
    }
    w.push(v_mov(73, ZERO));
    w.extend([0xe078_1000, 0x8003_4614]); // buffer_store_dwordx4 v[70:73], v20, s[12:15], 0 offen
    w.push(0xbf81_0000); // s_endpgm
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
        vgprs: 80 + ARM_VGPRS,
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
        io: StageIo::Cs(ComputeIo {
            rsrc2: 5 << 1,
            threads: [32, 1, 1],
        }),
        code: w.iter().flat_map(|w| w.to_le_bytes()).collect(),
    }
}

/// Checkerboard's walk with an instance mask of 0xc0, to see the mask reach the query (the
/// title's walk tests none: 0xff).
const MASKED: Walk = Walk { mask: 0xc0, ..CHECKERBOARD.walks[0] };

/// Checkerboard's arm on the card: a lane of the walk takes the closest opaque hit of an
/// instance in the walk's mask (an instance of mask 0x01 in front is not seen), `v17` 1 and `v15` the distance; a lane outside the walk, a miss and a hit past the
/// maximum distance leave both; `v24` is 0 on every lane.
#[test]
fn the_checkerboard_arm_finds_occluders_on_the_card() {
    use super::super::host::{Dispatch, Host, Root, Stride, View};
    use super::super::host_tlas::{Instance, Scene};
    let Ok(host) = Host::new(false) else {
        eprintln!("no SM 6.6 hardware device; skipped");
        return;
    };
    if !super::super::host_tlas::supported(&host.device) {
        eprintln!("no ray tracing tier 1.1; skipped");
        return;
    }
    let _one = super::super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let native = checkerboard_arm_program();
    let p = crate::ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let st = crate::structure::structure(&p).expect("structure");
    let res = crate::resource::track(&p, &native.layout, native.stage, Some(&st)).expect("track");
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
    assert_eq!(shader.root_srvs, [super::super::shader::SLOT_TLAS]);
    assert!(shader.root_cbvs.is_empty() && shader.root_uavs.is_empty());
    let (root, pso) = host.compute_pipeline(&shader).expect("compute_pipeline");
    // Threads: (one of the walk's lanes, direction z, extent).
    let lanes: [(bool, f32, f32); 4] = [
        (true, 1.0, 100.0),
        (false, 1.0, 100.0),
        (true, -1.0, 100.0),
        (true, 1.0, 4.0),
    ];
    let mut record = vec![0u32; 8 * 32];
    for t in 0..32 {
        let (walks, dz, extent) = lanes[t.min(lanes.len() - 1)];
        let r = &mut record[8 * t..];
        for (k, f) in [-0.5, -0.5, 0.0, 0.0, 0.0, dz, extent]
            .into_iter()
            .enumerate()
        {
            r[k] = f32::to_bits(f);
        }
        r[7] = u32::from(walks);
    }
    let triangle = [[-1.0, -1.0, 5.0], [1.0, -1.0, 5.0], [-1.0, 1.0, 5.0]];
    let scene = Scene {
        geometries: vec![(vec![triangle], true)],
        instances: vec![
            // Outside the mask, nearer.
            Instance {
                offset: [0.0, 0.0, -2.0],
                id: 1,
                contribution: 0,
                mask: 0x01,
            },
            Instance {
                offset: [0.0; 3],
                id: 2,
                contribution: 0,
                mask: 0x40,
            },
        ],
    };
    let got = host
        .dispatch(
            &root,
            &pso,
            &Dispatch {
                roots: vec![
                    (
                        0,
                        Root::Constants(vec![0; shader.user_data_dwords() as usize]),
                    ),
                    (1, Root::Tlas(Some(scene))),
                ],
                views: &[
                    View::Buffer {
                        data: &record,
                        stride: Stride::Raw,
                    },
                    View::RawUav(4 * 32),
                ],
                samplers: &[],
                groups: [1, 1, 1],
            },
        )
        .expect("dispatch");
    let f = f32::to_bits;
    let want = [
        [1, f(5.0), 0, 0],
        [0, f(100.0), 0, 0],
        [0, f(100.0), 0, 0],
        [0, f(4.0), 0, 0],
    ];
    for (t, want) in want.iter().enumerate() {
        let got = &got[0][4 * t..4 * t + 4];
        assert_eq!(got, want, "thread {t}: got {got:x?}");
    }
}
