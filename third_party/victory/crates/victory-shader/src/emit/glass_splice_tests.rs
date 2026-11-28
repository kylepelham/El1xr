//! The glass splice's two forms through the whole translator: a synthetic
//! compute program with a marker (`image_bvh64_intersect_ray` on a trap
//! temporary) and the selector (`ttmp15` read on entry) lowers to DXIL that
//! validates, asks for ray tracing tier 1.1, binds the host top level's root
//! SRV and asks the host for its glass word.

use super::glass_splice::{Marker, SELECTOR};
use super::shader::{SLOT_TLAS, SLOT_TLAS_NEAR, USER_GLASS_NATIVE};
use super::{lower, validator};
use crate::native::{
    ComputeIo, HwStage, NativeProgram, ResourceType, RootParam, RootType, StageIo, UsedResource,
};
use crate::resource::BvhNodes;
use crate::{io, ir, resource, structure};

/// Per thread: the ray from the record (v1-v12), the marker's result in
/// v13-v16, the selector added to v13, stored at 16 bytes a thread.
fn program(ttmp: u32) -> Vec<u32> {
    let mut w = vec![
        0xBE85_0380, // s_mov_b32 s5, 0
        0xF408_0202,
        0xFA00_0000, // s_load_dwordx4 s[8:11], s[4:5], 0x0
        0xF408_0302,
        0xFA00_0020, // s_load_dwordx4 s[12:15], s[4:5], 0x20
        0x3428_0086, // v_lshlrev_b32 v20, 6, v0
        0xE038_1000,
        0x8002_0114, // buffer_load_dwordx4 v[1:4], v20, s[8:11], 0 offen
        0xE038_1010,
        0x8002_0514, // buffer_load_dwordx4 v[5:8], v20, s[8:11], 0 offen offset:16
        0xE038_1020,
        0x8002_0914, // buffer_load_dwordx4 v[9:12], v20, s[8:11], 0 offen offset:32
        0xBF8C_3F70, // s_waitcnt vmcnt(0)
    ];
    // image_bvh64_intersect_ray v[13:16], [v1 .. v12], ttmp[k:k+3]
    let first = 0x0000_0D01 | (ttmp >> 2) << 16;
    w.extend([0xF19C_9F07, first, 0x0504_0302, 0x0908_0706, 0x000C_0B0A]);
    w.extend([
        0x4A1A_1A00 | u32::from(SELECTOR), // v_add_nc_u32 v13, ttmp15, v13
        0x342A_0084,                       // v_lshlrev_b32 v21, 4, v0
        0xE078_1000,
        0x8003_0D15, // buffer_store_dwordx4 v[13:16], v21, s[12:15], 0 offen
        0xBF81_0000, // s_endpgm
    ]);
    w
}

fn native(words: &[u32]) -> NativeProgram {
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
        vgprs: 22,
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
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    }
}

/// `SFI0`'s feature bits: the first part after the container header.
fn features(container: &[u8]) -> u64 {
    let first = u32::from_le_bytes(container[32..36].try_into().unwrap()) as usize;
    assert_eq!(&container[first..first + 4], b"SFI0");
    u64::from_le_bytes(container[first + 8..first + 16].try_into().unwrap())
}

/// Lowers the program with the marker on `ttmp`; validated when the
/// validator is there.
fn lowered(ttmp: u32) -> (lower::Lowered, resource::Resources) {
    let native = native(&program(ttmp));
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let st = structure::structure(&p).expect("structure");
    let res = resource::track(&p, &native.layout, native.stage, Some(&st)).expect("track");
    let io = io::raise(&p, &native).expect("io::raise");
    let out = lower::lower(&lower::Parts {
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
    .expect("the marker lowers");
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        let mut copy = out.dxil.clone();
        v.validate(&mut copy).expect("DXIL validation failed");
    }
    (out, res)
}

#[test]
fn each_trap_temporary_is_its_marker() {
    for (ttmp, near, indices) in [
        (108, false, false),
        (112, false, true),
        (116, true, false),
        (120, true, true),
    ] {
        let (_, res) = lowered(ttmp);
        assert_eq!(res.bvh.len(), 1);
        assert_eq!(
            res.bvh[0].1,
            BvhNodes::Native(Marker { near, indices }),
            "ttmp{}",
            ttmp - 108
        );
        // A marker names no descriptor of the title.
        assert!(res.descriptors.iter().all(|d| d.site != res.bvh[0].0));
    }
}

#[test]
fn a_marker_lowers_to_a_ray_query_on_its_top_level() {
    for (ttmp, slot) in [(108, SLOT_TLAS), (120, SLOT_TLAS_NEAR)] {
        let (out, _) = lowered(ttmp);
        assert_ne!(features(&out.dxil) & 0x10_0000, 0, "RaytracingTier1_1");
        assert!(
            out.root_srvs.iter().any(|&s| s == slot),
            "root SRV {slot} for ttmp{}: {:?}",
            ttmp - 108,
            out.root_srvs
        );
        // The selector: the host's glass word.
        assert!(
            out.user_data
                .iter()
                .any(|u| u.root == RootType::Other(USER_GLASS_NATIVE)),
            "{:?}",
            out.user_data
        );
    }
}
