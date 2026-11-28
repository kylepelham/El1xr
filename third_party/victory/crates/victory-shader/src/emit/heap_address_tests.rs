use super::*;
use crate::native::HwStage;

#[test]
fn off_by_default() {
    assert!(!on());
}

/// The title's TraceShadowRays programs (not in the repository: set `SHADOW_TRACE_DIR` to a
/// folder of their containers, `<name>.<hash>.cdxil`): refused without the switch ("a
/// descriptor read only as data"); with it each translates, validates and reads the host's
/// table of view addresses, but FullAdaptive, which stops next at a lane mask read as data in
/// its third walk's candidate test (0xae80) without `VICTORY_MASK_ELSEWHERE`
/// (`mask_elsewhere`, whose tests take it with both).
#[test]
fn the_trace_shadow_rays_programs_translate_with_heap_view_addresses() {
    let Some(dir) = std::env::var_os("SHADOW_TRACE_DIR") else {
        eprintln!("SHADOW_TRACE_DIR not set: skipped");
        return;
    };
    let files: Vec<_> = std::fs::read_dir(dir)
        .expect("the folder")
        .flatten()
        .map(|e| e.path())
        .collect();
    let mut failed = Vec::new();
    for hash in [
        "0e64f14fae060d04",
        "002e93a6cc2aa355",
        "96ccb14ef981fd85",
        "dbdbf36298727135",
        "07c93bfb77fecebb",
    ] {
        let path = files
            .iter()
            .find(|p| p.to_string_lossy().contains(&format!(".{hash}.")))
            .unwrap_or_else(|| panic!("no container for {hash}"));
        let bytes = std::fs::read(path).expect("the container");
        let translate = || {
            let c = crate::dxbc::Container::parse(&bytes).expect("a container");
            let part = super::super::super::program_part(&c).expect("a program part");
            let native = crate::native_programs(part.data)
                .expect("native programs")
                .into_iter()
                .find(|p| p.stage == HwStage::Cs)
                .expect("a compute program");
            let p = crate::spill::build(&native).expect("spill::build");
            let st = crate::structure::structure(&p).expect("structure");
            let res = crate::resource::track(&p, &native.layout, native.stage, Some(&st))
                .expect("track");
            let io = crate::io::raise(&p, &native).expect("io");
            super::super::super::emit(&c, &native, &p, &st, &io, &res)
        };
        let off = translate();
        assert!(
            matches!(&off, Err(crate::ShaderError::Unsupported { what, .. })
                if *what == "a descriptor read only as data"),
            "{hash}: {:?}",
            off.map(|_| ())
        );
        FORCED.set(true);
        let on = translate();
        FORCED.set(false);
        let shader = match on {
            Ok(s) => s,
            Err(e) => {
                failed.push((hash, e.to_string()));
                continue;
            }
        };
        assert!(
            shader
                .root_srvs
                .contains(&super::super::super::shader::SLOT_VIEW_ADDRESSES),
            "{hash}"
        );
        if let Ok(mut v) = super::super::super::validator::Validator::load(
            super::super::super::validator::Validator::default_path().as_deref(),
        ) {
            let mut copy = shader.dxil.clone();
            v.validate(&mut copy)
                .unwrap_or_else(|e| panic!("{hash}: DXIL validation failed: {e:?}"));
        } else {
            eprintln!("no validator: {hash} not validated");
        }
    }
    let full_adaptive = (
        "dbdbf36298727135",
        "GFX10 code at 0xae80: a lane mask holding lanes elsewhere read as data is not supported"
            .to_owned(),
    );
    assert_eq!(failed, [full_adaptive]);
}

/// A wave32 program that loads a descriptor from the view heap at byte offset 96 (taken from
/// lane 0, as the title takes its record) and reads its address as numbers, as the TraceShadowRays programs do: dword 0,
/// dword 1 masked to its address bits, the pair shifted down by 8; each thread stores those
/// and the offset.
fn reads_a_heap_descriptor() -> crate::native::NativeProgram {
    use super::super::super::glass_patch::v_mov;
    use crate::native::{
        ComputeIo, NativeProgram, ResourceType, RootParam, StageIo, UsedResource,
    };
    let w: Vec<u32> = vec![
        0xbe85_0380, // s_mov_b32 s5, 0
        0xf408_0302,
        0xfa00_0000, // s_load_dwordx4 s[12:15], s[4:5], 0x0 (the table's UAV)
        0x4a02_00ff,
        96,          // v_add_nc_u32 v1, 96, v0
        0x7e02_0501, // v_readfirstlane_b32 s1, v1: lane 0's, 96
        0xbe87_0380, // s_mov_b32 s7, 0
        0xf408_0203,
        0x0200_0000, // s_load_dwordx4 s[8:11], s[6:7], s1 (the heap's descriptor)
        0xbf8c_c07f, // s_waitcnt lgkmcnt(0)
        0x8709_ff09,
        0x0000_ffff, // s_and_b32 s9, s9, 0xffff
        0x9090_8808, // s_lshr_b64 s[16:17], s[8:9], 8
        v_mov(70, 8),
        v_mov(71, 9),
        v_mov(72, 16),
        v_mov(73, 1),
        0x3428_0084, // v_lshlrev_b32 v20, 4, v0
        0xe078_1000,
        0x8003_4614, // buffer_store_dwordx4 v[70:73], v20, s[12:15], 0 offen
        0xbf81_0000, // s_endpgm
    ];
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
            param(RootType::Constants, 4, 0, 0),
            param(RootType::Table, 1, 4, 1),
            param(RootType::ResourceHeap, 1, 6, 2),
        ],
        used: vec![UsedResource {
            kind: ResourceType::Uav,
            root: RootType::Table,
            table_offset_dwords: 0,
            lower_bound: 0,
            stride: 4,
            dwords: 4,
            user_data: 4,
            slot: 1,
        }],
        io: StageIo::Cs(ComputeIo {
            rsrc2: 7 << 1,
            threads: [32, 1, 1],
        }),
        code: w.iter().flat_map(|w| w.to_le_bytes()).collect(),
    }
}

/// [`reads_a_heap_descriptor`] on the card with the switch: the view at title slot 3 (byte 96)
/// reads as its address from the host's table (`SLOT_VIEW_ADDRESSES`), 0x12_3456_7800, the
/// other slots' addresses unread; without the switch the program is refused.
#[test]
fn a_heap_descriptor_read_as_data_reads_the_hosts_address_on_the_card() {
    use super::super::super::host::{Dispatch, Host, Root, View};
    use super::super::super::shader::SLOT_VIEW_ADDRESSES;
    let native = reads_a_heap_descriptor();
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
            if *what == "a descriptor read only as data"),
        "{:?}",
        off.map(|_| ())
    );
    FORCED.set(true);
    let shader = lower();
    FORCED.set(false);
    let shader = shader.expect("lowers with the switch");
    assert_eq!(shader.root_srvs, [SLOT_VIEW_ADDRESSES]);
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
    let constants = vec![0; shader.user_data_dwords() as usize];
    let mut table = vec![0u32; 16];
    for (slot, at) in [(2u64, 0x77_0000_0100u64), (3, 0x12_3456_7800), (4, 0x99_0000_0200)] {
        table[2 * slot as usize] = at as u32;
        table[2 * slot as usize + 1] = (at >> 32) as u32;
    }
    let got = host
        .dispatch(
            &root,
            &pso,
            &Dispatch {
                roots: vec![
                    (0, Root::Constants(constants)),
                    (1, Root::View { srv: true, data: table }),
                ],
                views: &[View::RawUav(4 * 32)],
                samplers: &[],
                groups: [1, 1, 1],
            },
        )
        .expect("dispatch");
    for t in [0, 31] {
        assert_eq!(
            &got[0][4 * t..4 * t + 4],
            &[0x3456_7800, 0x12, 0x1234_5678, 96],
            "thread {t}"
        );
    }
}
