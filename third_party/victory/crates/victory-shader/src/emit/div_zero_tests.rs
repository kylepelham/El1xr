//! `VICTORY_DIV_INF_ZERO` (`super::div_zero`): `v_div_fixup` given its quotient, denominator and
//! numerator directly (not only after a whole division sequence), on the software adapter, with
//! the switch off and on. After trustno1-coder/VIctory-NVIDIA `feb90adeb1`'s corner test.

/// A compute program: lane `i` loads (q, den, num) from dword `4 i` of buffer 0, stores
/// `v_div_fixup(q, den, num)` at dword `i` of buffer 1.
fn fixup_program(on: bool) -> super::lower::Lowered {
    let words: &[u32] = &[
        0xBE85_0380, // s_mov_b32 s5, 0
        0xF408_0202,
        0xFA00_0000, // s_load_dwordx4 s[8:11], s[4:5], 0x0
        0xF408_0302,
        0xFA00_0020, // s_load_dwordx4 s[12:15], s[4:5], 0x20
        0x340E_0084, // v_lshlrev_b32 v7, 4, v0
        0xE038_1000,
        0x8002_0107, // buffer_load_dwordx4 v[1:4], v7, s[8:11], 0 offen
        0xD55F_0001,
        0x040E_0501, // v_div_fixup_f32 v1, v1, v2, v3
        0x340E_0082, // v_lshlrev_b32 v7, 2, v0
        0xE070_1000,
        0x8003_0107, // buffer_store_dword v1, v7, s[12:15], 0 offen
        0xBF81_0000, // s_endpgm
    ];
    let native = super::NativeProgram {
        stage: super::HwStage::Cs,
        wave64: false,
        vgprs: 9,
        layout: vec![
            crate::RootParam {
                root: crate::RootType::Constants,
                table_offset_dwords: 0,
                sgprs: 4,
                base_register: 0,
                user_data: 0,
                slot: 0,
            },
            crate::RootParam {
                root: crate::RootType::Table,
                table_offset_dwords: 0,
                sgprs: 1,
                base_register: 0,
                user_data: 4,
                slot: 1,
            },
        ],
        used: vec![
            crate::UsedResource {
                kind: crate::ResourceType::Srv,
                root: crate::RootType::Table,
                table_offset_dwords: 0,
                lower_bound: 0,
                stride: 4,
                dwords: 4,
                user_data: 4,
                slot: 1,
            },
            crate::UsedResource {
                kind: crate::ResourceType::Uav,
                root: crate::RootType::Table,
                table_offset_dwords: 8,
                lower_bound: 0,
                stride: 4,
                dwords: 4,
                user_data: 4,
                slot: 1,
            },
        ],
        io: crate::native::StageIo::Cs(crate::native::ComputeIo {
            rsrc2: 5 << 1,
            threads: [12, 1, 1],
        }),
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    };
    let p = crate::ir::build(&native.code, native.wave64, native.vgprs).unwrap();
    let res = crate::resource::track(&p, &native.layout, native.stage, None).unwrap();
    let io = crate::io::raise(&p, &native).unwrap();
    let st = crate::structure::structure(&p).unwrap();
    super::div_zero::FORCED.set(on);
    let l = super::lower::lower(&super::lower::Parts {
        native: &native,
        p: &p,
        st: &st,
        io: &io,
        res: &res,
        inputs: Vec::new(),
        outputs: Vec::new(),
        prims: Vec::new(),
        mesh_stride: 0,
    });
    super::div_zero::FORCED.set(false);
    l.unwrap()
}

/// (q, den, num, with the switch off, with it on).
const NAN: u32 = 0x7FC0_0000;
const CASES: [(u32, u32, u32, u32, u32); 12] = [
    (NAN, 0x3F80_0000, 0x0000_0000, 0x7F80_0000, 0x0000_0000), // 0 / 1
    (NAN, 0xBF80_0000, 0x0000_0000, 0xFF80_0000, 0x8000_0000), // 0 / -1
    (NAN, 0xBF80_0000, 0x8000_0000, 0x7F80_0000, 0x0000_0000), // -0 / -1
    (NAN, 0x7F80_0000, 0x3F80_0000, 0x7F80_0000, 0x0000_0000), // 1 / inf
    (NAN, 0xFF80_0000, 0x4040_0000, 0xFF80_0000, 0x8000_0000), // 3 / -inf
    (NAN, 0x0000_0000, 0x3F80_0000, 0x7F80_0000, 0x7F80_0000), // 1 / 0
    (NAN, 0x3F80_0000, 0x7F80_0000, 0x7F80_0000, 0x7F80_0000), // inf / 1
    (NAN, 0x3F80_0000, 0x3F80_0000, 0x7F80_0000, 0x7F80_0000), // a NaN quotient of numbers
    (NAN, 0x0000_0000, 0x0000_0000, 0xFFC0_0000, 0xFFC0_0000), // 0 / 0
    (NAN, 0x7F80_0000, 0x7F80_0000, 0xFFC0_0000, 0xFFC0_0000), // inf / inf
    (
        0x4000_0000,
        0x3F80_0000,
        0x7FA0_0000,
        0x7FE0_0000,
        0x7FE0_0000,
    ), // a signalling NaN numerator
    (
        0x4000_0000,
        0xFFA0_0000,
        0x3F80_0000,
        0xFFE0_0000,
        0xFFE0_0000,
    ), // a signalling NaN denominator
];

#[test]
fn a_division_by_infinity_is_zero_with_its_sign_only_with_the_switch() {
    let Ok(host) = super::host::Host::new(true) else {
        eprintln!("no WARP device: skipped");
        return;
    };
    let inputs: Vec<u32> = CASES
        .iter()
        .flat_map(|&(q, d, n, _, _)| [q, d, n, 0])
        .collect();
    for on in [false, true] {
        let l = fixup_program(on);
        let mut dxil = l.dxil;
        if let Ok(mut v) = super::validator::Validator::load(
            super::validator::Validator::default_path().as_deref(),
        ) {
            v.validate(&mut dxil).unwrap();
        }
        let cs = super::HostShader {
            stage: super::HwStage::Cs,
            dxil,
            user_data: l.user_data,
            samplers: Vec::new(),
            static_samplers: Vec::new(),
            root_cbvs: l.root_cbvs,
            root_srvs: l.root_srvs,
            root_uavs: l.root_uavs,
            root_indexed_views: Vec::new(),
            half_views: l.half_views,
        };
        let (root, pso) = host.compute_pipeline(&cs).unwrap();
        let out = host
            .dispatch(
                &root,
                &pso,
                &super::host::Dispatch {
                    roots: vec![(0, super::host::Root::Constants(vec![0, 0, 0, 0, 0]))],
                    views: &[
                        super::host::View::Buffer {
                            data: &inputs,
                            stride: super::host::Stride::Raw,
                        },
                        super::host::View::RawUav(64),
                    ],
                    samplers: &[],
                    groups: [1, 1, 1],
                },
            )
            .unwrap();
        for (i, &(q, d, n, off, with)) in CASES.iter().enumerate() {
            let want = if on { with } else { off };
            assert_eq!(
                out[0][i], want,
                "switch {on}, case {i}: q={q:#010x} den={d:#010x} num={n:#010x}"
            );
        }
    }
}
