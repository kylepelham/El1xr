//! The glass splice's two forms through the whole translator: a synthetic
//! compute program with a marker (`image_bvh64_intersect_ray` on a trap
//! temporary) and the selector (`ttmp15` read on entry) lowers to DXIL that
//! validates, asks for ray tracing tier 1.1, binds the host top level's root
//! SRV and asks the host for its glass word; on the card, the ray query it
//! makes finds what DXR says a tiny host scene holds ([`super::host_tlas`]).

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

/// One thread's ray: origin, direction, maximum distance.
struct Ray {
    origin: [f32; 3],
    direction: [f32; 3],
    extent: f32,
}

/// The scene the markers trace: one bottom level of three geometries,
/// instanced twice.
/// - geometry 0, opaque: a small triangle at z = 5 over x, y in -1..1;
/// - geometry 1, opaque: a large triangle at z = 10;
/// - geometry 2, not opaque: a large triangle at z = 7 (culled: opaque only);
/// - instance 0 (id 7, contribution 3) moved 100 along x; instance 1 (id 9,
///   contribution 5) where it is.
fn scene() -> super::host_tlas::Scene {
    use super::host_tlas::{Instance, Scene};
    let small = [[-1.0, -1.0, 5.0], [1.0, -1.0, 5.0], [-1.0, 1.0, 5.0]];
    let large = |z| [[-50.0, -50.0, z], [150.0, -50.0, z], [-50.0, 150.0, z]];
    Scene {
        geometries: vec![
            (vec![small], true),
            (vec![large(10.0)], true),
            (vec![large(7.0)], false),
        ],
        instances: vec![
            Instance { offset: [100.0, 0.0, 0.0], id: 7, contribution: 3, mask: 0xff },
            Instance { offset: [0.0, 0.0, 0.0], id: 9, contribution: 5, mask: 0xff },
        ],
    }
}

fn rays() -> Vec<Ray> {
    let ahead = |x, y, extent| Ray { origin: [x, y, 0.0], direction: [0.0, 0.0, 1.0], extent };
    vec![
        // Instance 1's small triangle at its (0.25, 0.25).
        ahead(-0.5, -0.5, 10000.0),
        // Past the small triangle, through the culled one, onto the large.
        ahead(0.8, 0.8, 10000.0),
        // Instance 0's small triangle, in front of instance 1's large one.
        ahead(99.5, -0.5, 10000.0),
        // The small triangle beyond the maximum distance.
        ahead(-0.5, -0.5, 3.0),
        // Away from everything.
        Ray { origin: [0.0, 0.0, 0.0], direction: [0.0, 0.0, -1.0], extent: 10000.0 },
    ]
}

/// What each ray's two markers give: the hit part and the indices part.
fn expected(ray: usize) -> ([u32; 4], [u32; 4]) {
    let f = f32::to_bits;
    let miss = ([0, f(-1.0), 0, 0], [u32::MAX; 4]);
    match ray {
        0 => ([1, f(5.0), f(0.25), f(0.25)], [1, 0, 0, 5]),
        1 => ([1, f(10.0), f(0.254), f(0.254)], [1, 0, 1, 5]),
        2 => ([1, f(5.0), f(0.25), f(0.25)], [0, 0, 0, 3]),
        _ => miss,
    }
}

/// The marker on `ttmp` run on a D3D12 device with `scene` bound at its
/// top level's root SRV and the host's glass word `selector`; each
/// thread's four result dwords.
fn traced(
    host: &super::host::Host,
    ttmp: u32,
    scene: Option<super::host_tlas::Scene>,
    selector: u32,
) -> Vec<[u32; 4]> {
    use super::host::{Dispatch, Root, Stride, View};
    let (out, res) = lowered(ttmp);
    let shader = super::HostShader {
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
    let (root, pso) = host.compute_pipeline(&shader).expect("compute_pipeline");
    let mut user = vec![0u32; shader.user_data_dwords() as usize];
    for u in &shader.user_data {
        if u.root == RootType::Other(USER_GLASS_NATIVE) {
            user[u.index as usize] = selector;
        }
    }
    // Root views follow the user data, by slot then kind.
    let mut views: Vec<(u8, u8)> = shader.root_cbvs.iter().map(|&r| (r, 0)).collect();
    views.extend(shader.root_srvs.iter().map(|&r| (r, 1)));
    views.extend(shader.root_uavs.iter().map(|&r| (r, 2)));
    views.sort_unstable();
    views.dedup();
    let slot = if ttmp >= 116 { SLOT_TLAS_NEAR } else { SLOT_TLAS };
    let param = 1 + views.iter().position(|&v| v == (slot, 1)).expect("the top level's root SRV");
    assert_eq!(views.len(), 1, "no root view but the top level: {views:?}");
    let mut record = vec![0u32; 16 * 32];
    // Threads past the rays repeat the last.
    let all = rays();
    for t in 0..32 {
        let r = &all[t.min(all.len() - 1)];
        let words = &mut record[16 * t..];
        words[2] = r.extent.to_bits();
        for k in 0..3 {
            words[3 + k] = r.origin[k].to_bits();
            words[6 + k] = r.direction[k].to_bits();
            words[9 + k] = (1.0 / r.direction[k]).to_bits();
        }
    }
    let got = host
        .dispatch(
            &root,
            &pso,
            &Dispatch {
                roots: vec![(0, Root::Constants(user)), (param as u32, Root::Tlas(scene))],
                views: &[
                    View::Buffer { data: &record, stride: Stride::Raw },
                    View::RawUav(4 * 32),
                ],
                samplers: &[],
                groups: [1, 1, 1],
            },
        )
        .expect("dispatch");
    got[0].chunks_exact(4).map(|c| [c[0], c[1], c[2], c[3]]).collect()
}

/// The marker's ray query on the graphics card: hits on the right instance,
/// geometry and distance with DXR's barycentrics, the non-opaque geometry
/// culled, the maximum distance kept, the title's miss values, a null top
/// level missing, and the host's glass word reaching the program. Not on
/// WARP: it removes its device for this program (a ray query beside
/// resources from the heap; the root-view-only `ray_query_warp_tests` runs
/// there), with the top level built or null alike.
#[test]
fn a_marker_traces_the_host_top_level_on_the_card() {
    let Ok(host) = super::host::Host::new(false) else {
        eprintln!("no SM 6.6 hardware device; skipped");
        return;
    };
    if !super::host_tlas::supported(&host.device) {
        eprintln!("no ray tracing tier 1.1 on the device; skipped");
        return;
    }
    let _one = super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let close = |a: u32, b: u32| (f32::from_bits(a) - f32::from_bits(b)).abs() < 1e-4;
    for ttmp in [108, 112, 116, 120] {
        let indices = ttmp == 112 || ttmp == 120;
        let got = traced(&host, ttmp, Some(scene()), 0);
        for ray in 0..rays().len() {
            let (hit, ids) = expected(ray);
            let (want, got) = (if indices { ids } else { hit }, got[ray]);
            let same = if indices {
                got == want
            } else {
                got[0] == want[0] && (0..4).skip(1).all(|k| close(got[k], want[k]))
            };
            assert!(same, "ttmp{} ray {ray}: got {got:x?}, want {want:x?}", ttmp - 108);
        }
        // A null top level: every ray misses.
        let none = traced(&host, ttmp, None, 0);
        let (hit, ids) = expected(usize::MAX);
        let miss = if indices { ids } else { hit };
        assert!(none[..rays().len()].iter().all(|&r| r == miss), "ttmp{}: {none:x?}", ttmp - 108);
    }
    // The selector the program adds to the first word is the host's word.
    let got = traced(&host, 108, Some(scene()), 1);
    assert_eq!(got[0][0], 2);
    assert_eq!(got[4][0], 1);
}
