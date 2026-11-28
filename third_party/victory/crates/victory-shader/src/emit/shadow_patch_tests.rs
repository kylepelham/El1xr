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
        for out in walk.outs {
            if let Out::Primitive { at, .. } = *out {
                let title = v_mov(29, 0xc1); // v_mov_b32 v29, -1
                code[at..at + 4].copy_from_slice(&title.to_le_bytes());
            }
        }
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

/// Every row is consistent: a SOPC comparison, the set-up between the skip and the loop's
/// head, the arm going on at the merge or after it; a walk whose results take exec ends with
/// [`Out::Exec`] and has the indices it needs from the first top level; a trace that picks its
/// top level per wave has two candidate views and every walk names the picked one, copies the
/// title's prefix from past the walk and has the alpha test's word after where it goes on; the
/// host finds each trace's binding by its hash.
#[test]
fn the_tables_agree() {
    for t in TRACES {
        assert!(!t.walks.is_empty(), "{}", t.hash);
        assert_eq!(
            shadow_binding(t.hash),
            Some(ShadowBinding {
                hash: t.hash,
                registers: t.structures,
                per_wave: t.per_wave,
                any: false,
            })
        );
        if t.per_wave {
            assert!(t.structures.iter().all(Option::is_some), "{}", t.hash);
            assert_ne!(t.structures[0], t.structures[1], "{}", t.hash);
        }
        for w in t.walks {
            assert_eq!(w.choose.is_some(), t.per_wave, "{}", t.hash);
            for out in w.outs {
                match *out {
                    Out::Prefix { from, to, execz } => {
                        assert!(w.past <= from && from < to && to < w.resume, "{}", t.hash);
                        assert!(execz > w.resume, "{}", t.hash);
                    }
                    Out::Primitive { at, .. } => assert!(at > w.resume, "{}", t.hash),
                    _ => {}
                }
            }
            assert!(
                w.skip < w.head && w.head < w.past && w.past <= w.resume,
                "{}: {w:?}",
                t.hash
            );
            assert_eq!(w.cmp >> 23, 0x17e, "{}: a SOPC word", t.hash);
            assert!(w.structure < 2);
            let exec = w.outs.iter().position(|o| matches!(o, Out::Exec { .. }));
            assert!(
                exec.is_none_or(|k| k == w.outs.len() - 1),
                "{}: Exec last",
                t.hash
            );
            if exec.is_some() {
                assert!(
                    w.resume > w.past,
                    "{}: going on past the console reads",
                    t.hash
                );
            }
        }
    }
    let registers = |hash| shadow_binding(hash).map(|b| b.registers);
    assert_eq!(registers("a1b20bc47e3c9597"), Some([Some(355), Some(356)]));
    assert_eq!(registers("559d9f0184e15fe3"), Some([None, None]));
    assert_eq!(registers("6d7fe7745ab9c6d8"), None);
    let quarter = shadow_binding("0e64f14fae060d04").expect("Quarter");
    assert_eq!(quarter.registers, [Some(357), Some(353)]);
    assert!(quarter.per_wave);
    assert!(!shadow_binding("a1b20bc47e3c9597").unwrap().per_wave);
}

/// A per-wave walk's region: with the selector set, the picked base against the host's first
/// candidate (`ttmp13`) and second (`ttmp14`), each to its arm, else (and with the selector
/// clear) the title's comparison and skip; the first arm's markers on the first top level
/// (`ttmp[0:3]`, `ttmp[4:7]`), the second's on the second (`ttmp[8:11]`, `ttmp[12:15]`); each
/// arm runs the title's prefix to its `s_cbranch_execz`, clears the alpha test's mask and goes
/// on after the reads; the alpha test's -1 becomes the arm's primitive index.
#[test]
fn a_per_wave_walk_picks_its_arm_by_the_base() {
    let bytes = 0x8800;
    let mut native = stand_in(&QUARTER, bytes);
    apply(&QUARTER, &mut native);
    let walk = &QUARTER.walks[0];
    let insts = crate::gfx10::decode(&native.code).expect("decodes");
    let text = |offset: usize| {
        insts
            .iter()
            .find(|i| i.offset == offset)
            .map(|i| i.text(false))
            .unwrap_or_default()
    };
    let region = 0x8800;
    assert_eq!(text(walk.skip), format!("s_branch label_{region:04X}"));
    assert_eq!(text(region), "s_cmp_lg_u32 ttmp15, 0");
    assert_eq!(text(region + 4), format!("s_cbranch_scc0 label_{:04X}", region + 24));
    assert_eq!(text(region + 8), "s_cmp_eq_u32 s12, ttmp13");
    assert_eq!(text(region + 16), "s_cmp_eq_u32 s12, ttmp14");
    assert_eq!(text(region + 24), "s_cmp_eq_u32 s90, 0");
    assert_eq!(text(region + 28), format!("s_cbranch_scc1 label_{:04X}", walk.past));
    assert_eq!(text(region + 32), format!("s_branch label_{:04X}", walk.skip + 4));
    let target = |offset: usize| {
        let t = text(offset);
        usize::from_str_radix(t.rsplit("label_").next().unwrap(), 16).unwrap()
    };
    let (first, second) = (target(region + 12), target(region + 20));
    assert_eq!(first, region + 36);
    let arm = |from: usize, to: usize| -> Vec<String> {
        insts
            .iter()
            .filter(|i| i.offset >= from && i.offset < to)
            .map(|i| i.text(false))
            .collect()
    };
    let next = target(QUARTER.walks[1].skip);
    for (from, to, pairs) in [
        (first, second, ["ttmp[0:3]", "ttmp[4:7]"]),
        (second, next, ["ttmp[8:11]", "ttmp[12:15]"]),
    ] {
        let t = arm(from, to);
        let markers: Vec<&String> = t.iter().filter(|x| x.starts_with("image_bvh64")).collect();
        assert_eq!(markers.len(), 2, "{t:?}");
        assert!(markers[0].contains(pairs[0]) && markers[1].contains(pairs[1]), "{markers:?}");
        assert!(t.iter().any(|x| x == "s_cbranch_execz label_8448"), "{t:?}");
        assert!(t.iter().any(|x| x == "s_mov_b32 s8, 0"), "{t:?}");
        assert_eq!(t.last().unwrap(), &format!("s_branch label_{:04X}", walk.resume));
    }
    let primitive = 64 + RAY_DWORDS + HIT_DWORDS + 1;
    assert_eq!(text(0x7504), format!("v_mov_b32 v29, v{primitive}"));
    assert_eq!(text(0x78b4), format!("v_mov_b32 v29, v{primitive}"));
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

/// A near walk of the distant trace: the set-up copied after the title arm, both markers on
/// the first top level (`ttmp[0:3]`, `ttmp[4:7]`), exec handed over, and the arm going on
/// past the console reads; a far walk's marker on the second (`ttmp[8:11]`).
#[test]
fn a_distant_walk_runs_its_set_up_and_goes_on_past_the_reads() {
    let bytes = 0x5600;
    let mut native = stand_in(&DISTANT, bytes);
    for w in DISTANT.walks {
        for (k, word) in native.code[w.skip + 4..w.head]
            .chunks_exact_mut(4)
            .enumerate()
        {
            word.copy_from_slice(&v_mov(200, 0x80 + k as u32 % 64).to_le_bytes());
        }
    }
    apply(&DISTANT, &mut native);
    let insts = crate::gfx10::decode(&native.code).expect("decodes");
    let texts = |from: usize, to: usize| -> Vec<String> {
        insts
            .iter()
            .filter(|i| i.offset >= from && i.offset < to)
            .map(|i| i.text(false))
            .collect()
    };
    let starts: Vec<usize> = DISTANT
        .walks
        .iter()
        .map(|w| {
            let word = u32::from_le_bytes(native.code[w.skip..w.skip + 4].try_into().unwrap());
            let delta = i64::from(word as u16 as i16);
            (w.skip as i64 + 4 + 4 * delta) as usize
        })
        .collect();
    for (k, w) in DISTANT.walks.iter().enumerate() {
        let end = starts.get(k + 1).copied().unwrap_or(native.code.len());
        let t = texts(starts[k], end);
        let setup = (w.head - w.skip - 4) / 4;
        let copied = texts(w.skip + 4, w.head);
        assert_eq!(&t[5..5 + setup], &copied[..], "walk {k}: the set-up");
        let markers: Vec<&String> = t.iter().filter(|x| x.starts_with("image_bvh64")).collect();
        if w.structure == 0 {
            assert_eq!(markers.len(), 2, "walk {k}");
            assert!(markers[0].contains("ttmp[0:3]") && markers[1].contains("ttmp[4:7]"));
            assert_eq!(t[t.len() - 2], "s_mov_b32 exec_lo, ttmp0", "walk {k}");
        } else {
            assert_eq!(markers.len(), 1, "walk {k}");
            assert!(markers[0].contains("ttmp[8:11]"));
        }
        assert_eq!(
            t.last().unwrap(),
            &format!("s_branch label_{:04X}", w.resume),
            "walk {k}"
        );
    }
}

/// The title's own programs (not in the repository: set `SHADOW_TRACE_DIR` to a folder of
/// their containers as `victory translate` dumps them, `<name>.<hash>.cdxil`): each known one,
/// spliced, translates, validates, has its markers, asks for ray tracing 1.1 and the host's
/// word.
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
    let mut programs = Vec::new();
    for trace in TRACES {
        let hash = trace.hash;
        let path = files
            .iter()
            .find(|p| p.to_string_lossy().contains(&format!(".{hash}.")))
            .unwrap_or_else(|| panic!("no container for {hash}"));
        let bytes = std::fs::read(path).expect("the container");
        let c = crate::dxbc::Container::parse(&bytes).expect("a container");
        let part = super::super::program_part(&c).expect("a program part");
        let native = crate::native_programs(part.data)
            .expect("native programs")
            .into_iter()
            .find(|p| p.stage == HwStage::Cs)
            .expect("a compute program");
        eprintln!(
            "{}: {} bytes, fnv {:#018x}, {} vgprs",
            trace.hash,
            native.code.len(),
            fnv(&native.code),
            native.vgprs
        );
        programs.push((trace, bytes, native));
    }
    for (trace, bytes, mut native) in programs {
        let c = crate::dxbc::Container::parse(&bytes).expect("a container");
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
        let want: usize = trace
            .walks
            .iter()
            .map(|w| {
                let arms = if w.choose.is_some() { 2 } else { 1 };
                arms * (1 + usize::from(
                    w.outs
                        .iter()
                        .any(|o| matches!(o, Out::Contribution(_) | Out::Geometry(_))),
                ))
            })
            .sum();
        assert_eq!(markers, want, "{}", trace.hash);
        let io = crate::io::raise(&p, &native).expect("io");
        let shader = super::super::emit(&c, &native, &p, &st, &io, &res);
        let shader = if trace.per_wave {
            // The title's own walk reads its structure's descriptor, loaded from the resource
            // heap at an offset a per-wave record names, as numbers (the picked base): refused,
            // spliced or not, without the host's view addresses (`VICTORY_HEAP_VIEW_ADDRESS`).
            let unspliced = translate_unspliced(&c);
            for got in [shader.map(|_| ()), unspliced] {
                assert!(
                    matches!(got, Err(crate::ShaderError::Unsupported { what, .. })
                        if what == "a descriptor read only as data"),
                    "{}: {got:?}",
                    trace.hash
                );
            }
            let forced = &super::super::lower::heap_address::FORCED;
            forced.set(true);
            let shader = super::super::emit(&c, &native, &p, &st, &io, &res);
            forced.set(false);
            shader.expect("emit with heap view addresses")
        } else {
            shader.expect("emit")
        };
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
        for base in [
            super::super::shader::USER_SHADOW_BASE_FIRST,
            super::super::shader::USER_SHADOW_BASE_SECOND,
        ] {
            let asks = shader.user_data.iter().any(|u| u.root == RootType::Other(base));
            assert_eq!(asks, trace.per_wave, "{}: {base:#x}", trace.hash);
        }
        if let Ok(mut v) = super::super::validator::Validator::load(
            super::super::validator::Validator::default_path().as_deref(),
        ) {
            let mut copy = shader.dxil.clone();
            v.validate(&mut copy).expect("DXIL validation failed");
        }
    }
}

/// `c`'s compute program translated as the title wrote it.
fn translate_unspliced(c: &crate::dxbc::Container) -> Result<(), crate::ShaderError> {
    let part = super::super::program_part(c).expect("a program part");
    let native = crate::native_programs(part.data)
        .expect("native programs")
        .into_iter()
        .find(|p| p.stage == HwStage::Cs)
        .expect("a compute program");
    let p = crate::spill::build(&native).expect("spill::build");
    let st = crate::structure::structure(&p).expect("structure");
    let res = crate::resource::track(&p, &native.layout, native.stage, Some(&st)).expect("track");
    let io = crate::io::raise(&p, &native).expect("io");
    super::super::emit(c, &native, &p, &st, &io, &res).map(|_| ())
}

/// One thread of a card test: whether it is one of the walk's lanes, the ray's origin,
/// direction z (x and y 0) and maximum distance.
#[derive(Clone, Copy)]
struct Lane {
    walks: bool,
    origin: [f32; 3],
    dz: f32,
    extent: f32,
}

/// A wave32 program that runs `walk`'s arm alone: each thread's record (origin, direction,
/// extent, whether it is one of the walk's lanes) into the registers the walk keeps them in,
/// the lanes' SGPR from that flag as a compare makes it, `preset` registers 7 before; after
/// the arm, exec back from the walk's saved SGPR where its results took it, then the four
/// `stores` operand codes stored at 16 bytes a thread.
fn arm_program(walk: &Walk, preset: &[u8], stores: [u32; 4]) -> NativeProgram {
    let mut w = prologue(walk, preset);
    arm(&mut w, walk, 80, walk.structure, &[], 0);
    if let Some(Out::Exec { saved, .. }) = walk.outs.last() {
        w.push(s_mov(EXEC_LO, *saved));
    }
    program(w, stores)
}

/// [`arm_program`]'s code before the arm.
fn prologue(walk: &Walk, preset: &[u8]) -> Vec<u32> {
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
    for &r in preset {
        w.push(v_mov(r, 0x87)); // 7
    }
    w
}

/// [`arm_program`]'s program: `w`, then the four `stores` stored at 16 bytes a thread.
fn program(mut w: Vec<u32>, stores: [u32; 4]) -> NativeProgram {
    w.push(0x3428_0084); // v_lshlrev_b32 v20, 4, v0
    for (k, src) in stores.into_iter().enumerate() {
        w.push(v_mov(70 + k as u8, src));
    }
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

/// Runs `native` on the card with `scenes` at the two top levels' root SRVs (the first at
/// [`super::super::shader::SLOT_TLAS`], the second at `SLOT_TLAS_NEAR`), one thread a lane
/// (threads past them repeat the last), the user-data dwords of `words` (root kind, dword
/// within it, value; the others 0); each thread's four stored dwords.
fn run_on_card(
    host: &super::super::host::Host,
    native: &NativeProgram,
    mut scenes: [Option<super::super::host_tlas::Scene>; 2],
    lanes: &[Lane],
    words: &[(RootType, u32, u32)],
) -> Vec<[u32; 4]> {
    use super::super::host::{Dispatch, Root, Stride, View};
    use super::super::shader::{SLOT_TLAS, SLOT_TLAS_NEAR};
    let p = crate::ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let st = crate::structure::structure(&p).expect("structure");
    let res = crate::resource::track(&p, &native.layout, native.stage, Some(&st)).expect("track");
    let io = crate::io::raise(&p, native).expect("io");
    let out = super::super::lower::lower(&super::super::lower::Parts {
        native,
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
    let mut srvs = shader.root_srvs.clone();
    srvs.sort_unstable();
    srvs.dedup();
    let (root, pso) = host.compute_pipeline(&shader).expect("compute_pipeline");
    let mut constants = vec![0; shader.user_data_dwords() as usize];
    for u in &shader.user_data {
        if let Some(&(.., value)) = words.iter().find(|w| (w.0, w.1) == (u.root, u.dword)) {
            constants[u.index as usize] = value;
        }
    }
    let mut roots = vec![(0, Root::Constants(constants))];
    for (k, slot) in srvs.iter().enumerate() {
        let which = if *slot == SLOT_TLAS {
            0
        } else {
            assert_eq!(*slot, SLOT_TLAS_NEAR);
            1
        };
        roots.push((1 + k as u32, Root::Tlas(scenes[which].take())));
    }
    let mut record = vec![0u32; 8 * 32];
    for t in 0..32 {
        let l = lanes[t.min(lanes.len() - 1)];
        let r = &mut record[8 * t..];
        let [x, y, z] = l.origin;
        for (k, f) in [x, y, z, 0.0, 0.0, l.dz, l.extent].into_iter().enumerate() {
            r[k] = f32::to_bits(f);
        }
        r[7] = u32::from(l.walks);
    }
    let got = host
        .dispatch(
            &root,
            &pso,
            &Dispatch {
                roots,
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
    got[0]
        .chunks_exact(4)
        .map(|c| [c[0], c[1], c[2], c[3]])
        .collect()
}

fn card() -> Option<super::super::host::Host> {
    let Ok(host) = super::super::host::Host::new(false) else {
        eprintln!("no SM 6.6 hardware device; skipped");
        return None;
    };
    if !super::super::host_tlas::supported(&host.device) {
        eprintln!("no ray tracing tier 1.1; skipped");
        return None;
    }
    Some(host)
}

/// A small triangle at z = 5 over x, y in -1..1, instanced as `instances` (offset, mask,
/// contribution), in a scene of `geometries` copies of it (the hit is the first).
fn triangles(
    instances: &[([f32; 3], u8, u32)],
    geometries: usize,
) -> super::super::host_tlas::Scene {
    use super::super::host_tlas::{Instance, Scene};
    let triangle = [[-1.0, -1.0, 5.0], [1.0, -1.0, 5.0], [-1.0, 1.0, 5.0]];
    let far = [[-1.0, -1.0, 500.0], [1.0, -1.0, 500.0], [-1.0, 1.0, 500.0]];
    Scene {
        geometries: (0..geometries)
            .map(|g| (vec![if g + 1 == geometries { triangle } else { far }], true))
            .collect(),
        instances: instances
            .iter()
            .enumerate()
            .map(|(k, &(offset, mask, contribution))| Instance {
                offset,
                id: k as u32,
                contribution,
                mask,
            })
            .collect(),
    }
}

/// Checkerboard's walk with an instance mask of 0xc0, to see the mask reach the query (the
/// title's walk tests none: 0xff).
const MASKED: Walk<'static> = Walk {
    mask: 0xc0,
    ..CHECKERBOARD.walks[0]
};

/// Checkerboard's arm on the card: a lane of the walk takes the closest opaque hit of an
/// instance in the walk's mask (an instance of mask 0x01 in front is not seen), `v17` 1 and
/// `v15` the distance; a lane outside the walk, a miss and a hit past the maximum distance
/// leave both; `v24` is 0 on every lane.
#[test]
fn the_checkerboard_arm_finds_occluders_on_the_card() {
    let Some(host) = card() else { return };
    let _one = super::super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let native = arm_program(&MASKED, &[], [v(17), v(15), v(24), ZERO]);
    let lane = |walks, dz, extent| Lane {
        walks,
        origin: [-0.5, -0.5, 0.0],
        dz,
        extent,
    };
    let lanes = [
        lane(true, 1.0, 100.0),
        lane(false, 1.0, 100.0),
        lane(true, -1.0, 100.0),
        lane(true, 1.0, 4.0),
    ];
    // Outside the mask and nearer; in it.
    let scene = triangles(&[([0.0, 0.0, -2.0], 0x01, 0), ([0.0; 3], 0x40, 0)], 1);
    let got = run_on_card(&host, &native, [Some(scene), None], &lanes, &[]);
    let f = f32::to_bits;
    let want = [
        [1, f(5.0), 0, 0],
        [0, f(100.0), 0, 0],
        [0, f(100.0), 0, 0],
        [0, f(4.0), 0, 0],
    ];
    for (t, want) in want.iter().enumerate() {
        assert_eq!(&got[t], want, "thread {t}: got {:x?}", got[t]);
    }
}

/// The distant trace's near walk on the card: hit lanes of the walk get the instance's
/// contribution and the geometry index (the second instance, contribution 40; the third
/// geometry), the others 0 in both; exec and the walk's `hits` SGPR are the hit lanes and
/// `saved` the exec before. Its far walk: the leaf `v25` 1 and the distance `v21` on a hit,
/// on the second top level only (the first holds nothing there).
#[test]
fn the_distant_walks_hand_over_their_results_on_the_card() {
    let Some(host) = card() else { return };
    let _one = super::super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let lane = |walks, x, dz| Lane {
        walks,
        origin: [x, -0.5, 0.0],
        dz,
        extent: 100.0,
    };
    // Hit, outside the walk, a miss, a hit.
    let lanes = [
        lane(true, -0.5, 1.0),
        lane(false, -0.5, 1.0),
        lane(true, -0.5, -1.0),
        lane(true, -0.5, 1.0),
    ];
    let near = &DISTANT.walks[0];
    let Some(&Out::Exec { saved, hits }) = near.outs.last() else {
        panic!("{near:?}")
    };
    let native = arm_program(near, &[22, 23], [v(22), v(23), saved, hits]);
    let instances = [([1000.0, 0.0, 0.0], 0xff, 30), ([0.0; 3], 0xff, 40)];
    let got = run_on_card(
        &host,
        &native,
        [Some(triangles(&instances, 3)), None],
        &lanes,
        &[],
    );
    // Lanes 0 and 3 hit (and the threads past the lanes, as lane 3); lane 2 misses.
    let hit_lanes: u32 = !0b0110;
    for (t, want) in [[40, 2], [0, 0], [0, 0], [40, 2]].iter().enumerate() {
        assert_eq!(
            &got[t][..2],
            want,
            "near walk, thread {t}: got {:x?}",
            got[t]
        );
        assert_eq!(got[t][2], u32::MAX, "saved: the whole wave");
        assert_eq!(got[t][3], hit_lanes, "hits");
    }
    let far = &DISTANT.walks[1];
    let native = arm_program(far, &[25], [v(25), v(21), ZERO, ZERO]);
    let got = run_on_card(
        &host,
        &native,
        [None, Some(triangles(&instances, 1))],
        &lanes,
        &[],
    );
    let f = f32::to_bits;
    for (t, want) in [[1, f(5.0)], [7, f(100.0)], [7, f(100.0)], [1, f(5.0)]]
        .iter()
        .enumerate()
    {
        assert_eq!(
            &got[t][..2],
            want,
            "far walk, thread {t}: got {:x?}",
            got[t]
        );
    }
}

/// A per-wave walk spliced into a program of the title's shape and run on the card: the picked
/// base (`s30`, from root constant 0) against the host's candidates. Equal to the first: the
/// first top level's hit (t 5); to the second: the second's (t 7); to neither, or with the
/// selector clear: the title's walk (`v50` 1, `v17` and `v15` as before). The walk's set-up
/// (`v51` 3) runs on every path a walk lane takes; a lane outside the walk keeps its values,
/// and on the title's path skips the set-up too (the title's skip, split per lane).
#[test]
fn a_per_wave_walk_takes_the_picked_top_level_on_the_card() {
    use super::super::shader::{USER_GLASS_NATIVE, USER_SHADOW_BASE_FIRST, USER_SHADOW_BASE_SECOND};
    let Some(host) = card() else { return };
    let _one = super::super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let model = &CHECKERBOARD.walks[0];
    let mut w = prologue(model, &[17]);
    w.push(v_mov(50, ZERO));
    w.push(s_mov(30, 0)); // s_mov_b32 s30, s0
    let skip = 4 * (w.len() + 1);
    let head = skip + 8;
    let past = head + 4;
    let walk = Walk {
        cmp: s_cmp_eq(model.lanes, 0x80),
        skip,
        past,
        head,
        resume: past,
        structure: 0,
        choose: Some(30),
        ..*model
    };
    w.push(walk.cmp);
    w.push(sopp(S_CBRANCH_SCC1, skip, past));
    w.push(v_mov(51, 0x83)); // the set-up: 3
    w.push(v_mov(50, ONE)); // the title's walk
    assert_eq!(4 * w.len(), past);
    let mut native = program(w, [v(17), v(15), v(50), v(51)]);
    native.vgprs = 80;
    let trace = Trace {
        walks: Box::leak(Box::new([walk])),
        per_wave: true,
        ..CHECKERBOARD
    };
    apply(&trace, &mut native);
    let lane = |walks| Lane {
        walks,
        origin: [-0.5, -0.5, 0.0],
        dz: 1.0,
        extent: 100.0,
    };
    let lanes = [lane(true), lane(false)];
    let (a, b) = (0x00ab_cd01, 0x00ab_cd02);
    let scene = |z| Some(triangles(&[([0.0, 0.0, z], 0xff, 0)], 1));
    let f = f32::to_bits;
    for (picked, selector, want) in [
        (a, 1, [1, f(5.0), 0, 3]),
        (b, 1, [1, f(7.0), 0, 3]),
        (0x00ab_cd03, 1, [7, f(100.0), 1, 3]),
        (a, 0, [7, f(100.0), 1, 3]),
    ] {
        let words = [
            (RootType::Constants, 0, picked),
            (RootType::Other(USER_GLASS_NATIVE), 0, selector),
            (RootType::Other(USER_SHADOW_BASE_FIRST), 0, a),
            (RootType::Other(USER_SHADOW_BASE_SECOND), 0, b),
        ];
        let got = run_on_card(&host, &native, [scene(0.0), scene(2.0)], &lanes, &words);
        assert_eq!(got[0], want, "picked {picked:#x}, selector {selector}: got {:x?}", got[0]);
        // On the title's path the lane takes the title's skip, split per lane.
        let outside = [7, f(100.0), 0, if want[2] == 1 { 0 } else { 3 }];
        assert_eq!(got[1], outside, "outside the walk: got {:x?}", got[1]);
    }
}
