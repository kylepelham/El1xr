use crate::native::HwStage;

/// Translates the compute program of the container at `path`, with the heap view addresses
/// (`heap_address`) and, when `on`, this switch.
fn translate(path: &std::path::Path, on: bool) -> Result<super::super::super::HostShader, crate::ShaderError> {
    let bytes = std::fs::read(path).expect("the container");
    let c = crate::dxbc::Container::parse(&bytes).expect("a container");
    let part = super::super::super::program_part(&c).expect("a program part");
    let native = crate::native_programs(part.data)
        .expect("native programs")
        .into_iter()
        .find(|p| p.stage == HwStage::Cs)
        .expect("a compute program");
    let p = crate::spill::build(&native).expect("spill::build");
    let st = crate::structure::structure(&p).expect("structure");
    let res = crate::resource::track(&p, &native.layout, native.stage, Some(&st)).expect("track");
    let io = crate::io::raise(&p, &native).expect("io");
    super::super::heap_address::FORCED.set(true);
    super::super::mask_elsewhere::FORCED.set(on);
    let r = super::super::super::emit(&c, &native, &p, &st, &io, &res);
    super::super::heap_address::FORCED.set(false);
    super::super::mask_elsewhere::FORCED.set(false);
    r
}

/// The title's programs refused with "a lane mask holding lanes elsewhere read as data" (not
/// in the repository: set `MASK_TRACE_DIR` to a folder of their containers,
/// `<name>.<hash>.cdxil`): refused without the switch, with it each translates and
/// validates.
#[test]
fn the_refused_programs_translate_and_validate() {
    let Some(dir) = std::env::var_os("MASK_TRACE_DIR") else {
        eprintln!("MASK_TRACE_DIR not set: skipped");
        return;
    };
    let files: Vec<_> = std::fs::read_dir(dir)
        .expect("the folder")
        .flatten()
        .map(|e| e.path())
        .collect();
    for hash in ["dbdbf36298727135", "6871d6e4b3ac9a10", "926c46bab3b86bd6"] {
        let path = files
            .iter()
            .find(|p| p.to_string_lossy().contains(&format!(".{hash}.")))
            .unwrap_or_else(|| panic!("no container for {hash}"));
        let off = translate(path, false);
        assert!(
            matches!(&off, Err(crate::ShaderError::Unsupported { what, .. })
                if *what == super::super::MASK_ELSEWHERE),
            "{hash}: {:?}",
            off.map(|_| ())
        );
        let shader = translate(path, true).unwrap_or_else(|e| panic!("{hash}: {e}"));
        if let Ok(mut v) =
            super::super::super::validator::Validator::load(super::super::super::validator::Validator::default_path().as_deref())
        {
            let mut copy = shader.dxil.clone();
            v.validate(&mut copy)
                .unwrap_or_else(|e| panic!("{hash}: DXIL validation failed: {e:?}"));
        } else {
            eprintln!("no validator: {hash} not validated");
        }
    }
}

/// A wave32 program that reads two masks as numbers in a block only lanes 0-7 run: `s31`, a
/// literal's bits of `s27`, which joins a number (root constant 2, when root constant 0 is 0)
/// with EXEC (otherwise), as FullAdaptive's candidate test does; and `s23`, the lanes that ran
/// before (`s22`) but not now, a mask of lanes elsewhere. Lanes 0-7 store both.
fn reads_masks_elsewhere() -> crate::native::NativeProgram {
    use super::super::super::glass_patch::{S_BRANCH, S_CBRANCH_SCC1, sopp, v_mov};
    use crate::native::{ComputeIo, NativeProgram, ResourceType, RootParam, StageIo, UsedResource};
    let mut w: Vec<u32> = vec![
        0xbe85_0380, // s_mov_b32 s5, 0
        0xf408_0302,
        0xfa00_0000, // s_load_dwordx4 s[12:15], s[4:5], 0x0 (the table's UAV)
        0xbf8c_c07f, // s_waitcnt lgkmcnt(0)
        0xbe96_037e, // s_mov_b32 s22, exec_lo
        0xbf06_8000, // s_cmp_eq_u32 s0, 0
    ];
    let to_data = w.len();
    w.push(0); // s_cbranch_scc1 data
    w.push(0xbe9b_037e); // s_mov_b32 s27, exec_lo
    let to_join = w.len();
    w.push(0); // s_branch join
    let data = w.len();
    w.push(0xbe9b_0302); // s_mov_b32 s27, s2
    let join = w.len();
    w[to_data] = sopp(S_CBRANCH_SCC1, 4 * to_data, 4 * data);
    w[to_join] = sopp(S_BRANCH, 4 * to_join, 4 * join);
    w.push(0x7da8_0088); // v_cmpx_gt_u32 exec_lo, 8, v0
    let to_end = w.len();
    w.push(0); // s_cbranch_execz end
    w.extend([
        0x871f_ff1b,
        0x001f_ffff, // s_and_b32 s31, s27, 0x1fffff
        0x8a17_7e16, // s_andn2_b32 s23, s22, exec_lo
        v_mov(70, 31),
        v_mov(71, 23),
        v_mov(72, 0x80),
        v_mov(73, 0x80),
        0x3428_0084, // v_lshlrev_b32 v20, 4, v0
        0xe078_1000,
        0x8003_4614, // buffer_store_dwordx4 v[70:73], v20, s[12:15], 0 offen
    ]);
    let end = w.len();
    w[to_end] = sopp(8, 4 * to_end, 4 * end); // s_cbranch_execz
    w.push(0xbf81_0000); // s_endpgm
    let param = |root, sgprs, user_data, slot| RootParam {
        root,
        table_offset_dwords: 0,
        sgprs,
        base_register: 0,
        user_data,
        slot,
    };
    NativeProgram {
        stage: HwStage::Cs,
        wave64: false,
        vgprs: 80,
        layout: vec![
            param(crate::native::RootType::Constants, 4, 0, 0),
            param(crate::native::RootType::Table, 1, 4, 1),
        ],
        used: vec![UsedResource {
            kind: ResourceType::Uav,
            root: crate::native::RootType::Table,
            table_offset_dwords: 0,
            lower_bound: 0,
            stride: 4,
            dwords: 4,
            user_data: 4,
            slot: 1,
        }],
        io: StageIo::Cs(ComputeIo {
            rsrc2: 5 << 1,
            threads: [32, 1, 1],
        }),
        code: w.iter().flat_map(|w| w.to_le_bytes()).collect(),
    }
}

/// [`reads_masks_elsewhere`] on the card with the switch: lanes 0-7 store `s27`'s number
/// masked (root constant 2's bits on the number's path, 0x1fffff on EXEC's, the whole wave)
/// and the lanes elsewhere (0xffffff00); lanes 8-31 store nothing. Refused without it.
#[test]
fn masks_read_as_numbers_where_not_every_lane_runs_on_the_card() {
    use super::super::super::host::{Dispatch, Host, Root, View};
    use crate::native::RootType;
    let native = reads_masks_elsewhere();
    let lower = || -> Result<super::super::super::HostShader, crate::ShaderError> {
        let p = crate::ir::build(&native.code, native.wave64, native.vgprs)?;
        let st = crate::structure::structure(&p)?;
        let res = crate::resource::track(&p, &native.layout, native.stage, Some(&st))?;
        let io = crate::io::raise(&p, &native)?;
        let out = super::super::lower(&super::super::Parts {
            native: &native,
            p: &p,
            st: &st,
            io: &io,
            res: &res,
            inputs: Vec::new(),
            outputs: Vec::new(),
            prims: Vec::new(),
            mesh_stride: 0,
        })?;
        Ok(super::super::super::HostShader {
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
        })
    };
    let off = lower();
    assert!(
        matches!(&off, Err(crate::ShaderError::Unsupported { what, .. })
            if *what == super::super::MASK_ELSEWHERE),
        "{:?}",
        off.map(|_| ())
    );
    super::FORCED.set(true);
    let shader = lower();
    super::FORCED.set(false);
    let shader = shader.expect("lowers with the switch");
    if let Ok(mut v) = super::super::super::validator::Validator::load(
        super::super::super::validator::Validator::default_path().as_deref(),
    ) {
        let mut copy = shader.dxil.clone();
        v.validate(&mut copy).expect("DXIL validation failed");
    }
    let Ok(host) = Host::new(false) else {
        eprintln!("no SM 6.6 hardware device; skipped");
        return;
    };
    let _one = super::super::super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let (root, pso) = host.compute_pipeline(&shader).expect("compute_pipeline");
    let number = 0xabc1_2345u32;
    for (selector, want) in [(0, number & 0x1f_ffff), (1, 0x1f_ffff)] {
        let mut constants = vec![0; shader.user_data_dwords() as usize];
        for u in &shader.user_data {
            match (u.root, u.dword) {
                (RootType::Constants, 0) => constants[u.index as usize] = selector,
                (RootType::Constants, 2) => constants[u.index as usize] = number,
                _ => {}
            }
        }
        let got = host
            .dispatch(
                &root,
                &pso,
                &Dispatch {
                    roots: vec![(0, Root::Constants(constants))],
                    views: &[View::RawUav(4 * 32)],
                    samplers: &[],
                    groups: [1, 1, 1],
                },
            )
            .expect("dispatch");
        for t in 0..32 {
            let row = &got[0][4 * t..4 * t + 4];
            let expect = if t < 8 { [want, 0xffff_ff00, 0, 0] } else { [0; 4] };
            assert_eq!(row, &expect, "selector {selector}, thread {t}: got {row:x?}");
        }
    }
}
