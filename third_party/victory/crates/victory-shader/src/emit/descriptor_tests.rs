//! End-to-end tests of descriptors that no instruction names alone: an image's or a buffer's
//! descriptor the code reads only the size of, and an immediate sampler each arm of a uniform
//! branch sets its own of. Synthetic GFX10 compute programs translated ([`super::emit`]),
//! validated by `dxil.dll` when present, and run on the host GPU and WARP.

use super::host::{Dispatch, Host, Root, View};
use super::{HostShader, validator};
use crate::native::{
    ComputeIo, HwStage, NativeProgram, ResourceType, RootParam, RootType, StageIo, UsedResource,
};
use crate::{Container, dxbc, io, ir, resource, structure};

const S_ENDPGM: u32 = 0xBF81_0000;

/// A wave32 compute program of one thread: 4 root constants in s0..s3 and, in s4, a table
/// with a texture's descriptor at dword 0 and a raw UAV's at dword 8; `body` follows the
/// loads of the texture's descriptor into s[8:15] and the UAV's into s[16:19].
fn native(body: &[u32]) -> NativeProgram {
    native_of(body, 8, 0)
}

/// [`native`] with a view of `dwords` dwords (8: a texture, 4: a buffer of elements of
/// `stride` bytes) at dword 0, loaded into s8 on.
fn native_of(body: &[u32], dwords: u8, stride: u32) -> NativeProgram {
    let load = match dwords {
        8 => 0xF40C_0202, // s_load_dwordx8 s[8:15], s[4:5], null
        _ => 0xF408_0202, // s_load_dwordx4 s[8:11], s[4:5], null
    };
    let mut words = vec![
        0xBE85_0380, // s_mov_b32 s5, 0
        load,
        0xFA00_0000,
        0xF408_0402,
        0xFA00_0020, // s_load_dwordx4 s[16:19], s[4:5], 0x20
    ];
    words.extend_from_slice(body);
    words.push(S_ENDPGM);
    let range = |kind, table_offset_dwords, dwords, stride| UsedResource {
        kind,
        root: RootType::Table,
        table_offset_dwords,
        lower_bound: 0,
        stride,
        dwords,
        user_data: 4,
        slot: 1,
    };
    NativeProgram {
        stage: HwStage::Cs,
        wave64: false,
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
        used: vec![
            range(ResourceType::Srv, 0, dwords, stride),
            range(ResourceType::Uav, 8, 4, 0),
        ],
        io: StageIo::Cs(ComputeIo {
            rsrc2: 5 << 1, // 5 user SGPRs: s0..s4
            threads: [1, 1, 1],
        }),
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    }
}

/// `native` translated as the title's programs are, and validated.
fn translate(native: &NativeProgram) -> HostShader {
    let container = dxbc::build(&[]);
    let c = Container::parse(&container).expect("container");
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let st = structure::structure(&p).expect("structure");
    let res = resource::track(&p, &native.layout, native.stage, Some(&st)).expect("track");
    let io = io::raise(&p, native).expect("io::raise");
    let shader = super::emit(&c, native, &p, &st, &io, &res).expect("emit");
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        v.validate(&mut shader.dxil.clone())
            .expect("DXIL validation failed");
    } else {
        eprintln!("no dxil.dll; validation skipped");
    }
    shader
}

/// Runs `shader` with root constant 0 at `s0`, the texture and a UAV of 4 dwords at heap
/// indices 0 and 1, on the hardware GPU and on WARP (each skipped without a device), and
/// returns per one that ran the UAV's dwords.
fn run(shader: &HostShader, s0: u32, texture: View) -> Vec<(&'static str, Vec<u32>)> {
    let views = [texture, View::RawUav(4)];
    let mut out = Vec::new();
    let _one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    for (label, warp) in [("hardware", false), ("warp", true)] {
        let Ok(host) = Host::new(warp) else {
            eprintln!("no SM 6.6 {label} device; skipped");
            continue;
        };
        let (root, pso) = host
            .compute_pipeline(shader)
            .unwrap_or_else(|e| panic!("{label}: compute_pipeline failed: {e}"));
        // The constant is s0; the table starts at heap index 0.
        let mut ud = vec![0u32; shader.user_data_dwords().max(1) as usize];
        for u in &shader.user_data {
            ud[u.index as usize] = match (u.root, u.dword) {
                (RootType::Constants, 0) => s0,
                _ => 0,
            };
        }
        let words = host
            .dispatch(
                &root,
                &pso,
                &Dispatch {
                    roots: vec![(0, Root::Constants(ud))],
                    views: &views,
                    samplers: &[],
                    groups: [1, 1, 1],
                },
            )
            .unwrap_or_else(|e| panic!("{label}: dispatch failed: {e}"));
        out.push((label, words[0].clone()));
    }
    out
}

#[test]
fn a_texture_read_only_for_its_size_gives_the_views_size() {
    // The console compiler's `GetDimensions` of a texture the program does not sample: width
    // and height from the descriptor's bits, and its last level; stored to the UAV.
    let shader = translate(&native(&[
        0x9014_9E09, // s_lshr_b32 s20, s9, 30
        0x8F15_820A, // s_lshl_b32 s21, s10, 2
        0x8814_1514, // s_or_b32 s20, s20, s21
        0x8714_FF14,
        0x0000_3FFF, // s_and_b32 s20, s20, 0x3fff
        0x8114_8114, // s_add_i32 s20, s20, 1
        0x9396_FF0A,
        0x000E_000E, // s_bfe_u32 s22, s10, 0x000e000e (bits 14-27)
        0x8116_8116, // s_add_i32 s22, s22, 1
        0x9397_FF0B,
        0x0004_0010, // s_bfe_u32 s23, s11, 0x00040010 (bits 16-19)
        0x7E02_0214, // v_mov_b32 v1, s20
        0x7E04_0216, // v_mov_b32 v2, s22
        0x7E06_0217, // v_mov_b32 v3, s23
        0xE070_0000,
        0x8004_0100, // buffer_store_dword v1, off, s[16:19], 0
        0xE070_0004,
        0x8004_0200, // buffer_store_dword v2, off, s[16:19], 0 offset:4
        0xE070_0008,
        0x8004_0300, // buffer_store_dword v3, off, s[16:19], 0 offset:8
    ]));
    let texels = [[0.0f32; 4]; 13 * 7];
    let results = run(&shader, 0, View::Texture(13, 7, &texels));
    for (label, words) in results {
        // 13 x 7 texels, one level (the last is level 0).
        assert_eq!(words, [13, 7, 0, 0], "{label}");
    }
}

#[test]
fn a_null_texture_read_only_for_its_size_is_one_texel_under_the_switch() {
    // The program above, shown a null view: no texels and no levels. With
    // VICTORY_NULL_VIEW_SIZE each count is taken as at least 1 (one texel, the last level
    // 0), as the console's empty descriptor reads; without it 0 - 1 fills the fields.
    let program = native(&[
        0x9014_9E09, // s_lshr_b32 s20, s9, 30
        0x8F15_820A, // s_lshl_b32 s21, s10, 2
        0x8814_1514, // s_or_b32 s20, s20, s21
        0x8714_FF14,
        0x0000_3FFF, // s_and_b32 s20, s20, 0x3fff
        0x8114_8114, // s_add_i32 s20, s20, 1
        0x9396_FF0A,
        0x000E_000E, // s_bfe_u32 s22, s10, 0x000e000e (bits 14-27)
        0x8116_8116, // s_add_i32 s22, s22, 1
        0x9397_FF0B,
        0x0004_0010, // s_bfe_u32 s23, s11, 0x00040010 (bits 16-19)
        0x7E02_0214, // v_mov_b32 v1, s20
        0x7E04_0216, // v_mov_b32 v2, s22
        0x7E06_0217, // v_mov_b32 v3, s23
        0xE070_0000,
        0x8004_0100, // buffer_store_dword v1, off, s[16:19], 0
        0xE070_0004,
        0x8004_0200, // buffer_store_dword v2, off, s[16:19], 0 offset:4
        0xE070_0008,
        0x8004_0300, // buffer_store_dword v3, off, s[16:19], 0 offset:8
    ]);
    let switch = |on| {
        let was = super::null_view_size::TEST_ON.replace(Some(on));
        let shader = translate(&program);
        super::null_view_size::TEST_ON.set(was);
        shader
    };
    let (floored, before) = (switch(true), switch(false));
    for (label, words) in run(&floored, 0, View::NullTexture) {
        assert_eq!(words, [1, 1, 0, 0], "{label}");
    }
    for (label, words) in run(&before, 0, View::NullTexture) {
        assert_eq!(words, [16384, 16384, 15, 0], "{label}: without the switch");
    }
    // A texture that is there keeps its size either way.
    let texels = [[0.0f32; 4]; 13 * 7];
    for (label, words) in run(&floored, 0, View::Texture(13, 7, &texels)) {
        assert_eq!(words, [13, 7, 0, 0], "{label}");
    }
}

#[test]
fn a_buffer_read_only_for_its_size_gives_the_views_element_count() {
    // The title's water simulation: whether a structured buffer has any elements, from its
    // descriptor's record count, the buffer itself not read; stored to the UAV.
    let shader = translate(&native_of(
        &[
            0x7E02_020A, // v_mov_b32 v1, s10
            0xE070_0000,
            0x8004_0100, // buffer_store_dword v1, off, s[16:19], 0
        ],
        4,
        16,
    ));
    let data = [0u32; 4 * 9];
    let buffer = View::Buffer {
        data: &data,
        stride: super::host::Stride::Structured(16),
    };
    for (label, words) in run(&shader, 0, buffer) {
        // nine elements of 16 bytes
        assert_eq!(words[0], 9, "{label}");
    }
}

#[test]
fn a_sampler_each_arm_of_a_uniform_branch_sets_samples_as_that_arm_says() {
    // The title's terrain programs: an immediate sampler that clamps (0x92: clamp in x, y and
    // z) or wraps (0) by a root constant, joined before one sample. Point filtering at u =
    // 1.25 of a 2 x 1 texture: wrapped, texel 0 (red 1.0); clamped, texel 1 (red 2.0).
    let shader = translate(&native(&[
        0xBF06_8000, // s_cmp_eq_u32 s0, 0
        0xBF85_0002, // s_cbranch_scc1 -> s_mov_b32 s20, 0
        0xB014_0092, // s_movk_i32 s20, 0x92
        0xBF82_0001, // s_branch -> the join
        0xBE94_0380, // s_mov_b32 s20, 0
        0xBE95_03FF,
        0x00C0_0000, // s_mov_b32 s21, 0x00c00000 (levels up to 12)
        0xBE96_0380, // s_mov_b32 s22, 0 (point filtering)
        0xBE97_0380, // s_mov_b32 s23, 0
        0x7E02_02FF,
        0x3FA0_0000, // v_mov_b32 v1, 1.25
        0x7E04_02FF,
        0x3F00_0000, // v_mov_b32 v2, 0.5
        0xF09C_0F08,
        0x00A2_0301, // image_sample_lz v[3:6], v[1:2], s[8:15], s[20:23] dmask:0xf 2D
        0xE070_0000,
        0x8004_0300, // buffer_store_dword v3, off, s[16:19], 0
    ]));
    // One immediate sampler per arm, each the arm's own.
    assert_eq!(shader.static_samplers.len(), 2);
    let texels = [[1.0f32, 0.0, 0.0, 1.0], [2.0, 0.0, 0.0, 1.0]];
    for (s0, red) in [(0, 1.0f32), (1, 2.0)] {
        for (label, words) in run(&shader, s0, View::Texture(2, 1, &texels)) {
            assert_eq!(f32::from_bits(words[0]), red, "{label}, s0 = {s0}");
        }
    }
}

/// (heapcap) A program that picks its texture from the view heap by a root constant (root type
/// `ResourceHeap`: the index is the title's own, `s0 << 5` bytes in) and stores its size to the
/// raw UAV at heap index 1, as the first test does.
fn heap_indexed() -> NativeProgram {
    let mut program = native(&[]);
    let mut words = vec![
        0x8F14_8500, // s_lshl_b32 s20, s0, 5
        0xBE85_0380, // s_mov_b32 s5, 0
        0xF40C_0202,
        0x2800_0000, // s_load_dwordx8 s[8:15], s[4:5], s20
        0xF408_0402,
        0xFA00_0020, // s_load_dwordx4 s[16:19], s[4:5], 0x20
        0x9014_9E09, // s_lshr_b32 s20, s9, 30
        0x8F15_820A, // s_lshl_b32 s21, s10, 2
        0x8814_1514, // s_or_b32 s20, s20, s21
        0x8714_FF14,
        0x0000_3FFF, // s_and_b32 s20, s20, 0x3fff
        0x8114_8114, // s_add_i32 s20, s20, 1
        0x9396_FF0A,
        0x000E_000E, // s_bfe_u32 s22, s10, 0x000e000e (bits 14-27)
        0x8116_8116, // s_add_i32 s22, s22, 1
        0x7E02_0214, // v_mov_b32 v1, s20
        0x7E04_0216, // v_mov_b32 v2, s22
        0xE070_0000,
        0x8004_0100, // buffer_store_dword v1, off, s[16:19], 0
        0xE070_0004,
        0x8004_0200, // buffer_store_dword v2, off, s[16:19], 0 offset:4
    ];
    words.push(S_ENDPGM);
    program.code = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    program.layout[1].root = RootType::ResourceHeap;
    program.used.clear();
    program
}

/// `native` translated with the title view limit at `limit` (None: not set) on this thread.
fn translate_limited(native: &NativeProgram, limit: Option<u32>) -> HostShader {
    let was = super::shader::TITLE_VIEW_LIMIT_HERE.replace(limit);
    let shader = translate(native);
    super::shader::TITLE_VIEW_LIMIT_HERE.set(was);
    shader
}

#[test]
fn a_view_index_the_title_computes_is_clamped_to_the_title_null_slot_under_heap_cap() {
    let program = heap_indexed();
    let before = translate_limited(&program, None);
    assert_eq!(translate_limited(&program, Some(0)).dxil, before.dxil, "0 is off: as before");
    let capped = translate_limited(&program, Some(2));
    assert_ne!(capped.dxil, before.dxil, "the clamp is in the program");
    // Heap: the texture at 0, the UAV at 1, a null texture at 2 (the title null slot).
    let texels = [[0.0f32; 4]; 13 * 7];
    let views = [View::Texture(13, 7, &texels), View::RawUav(4), View::NullTexture];
    let _one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    for (label, warp) in [("hardware", false), ("warp", true)] {
        let Ok(host) = Host::new(warp) else {
            eprintln!("no SM 6.6 {label} device; skipped");
            continue;
        };
        let (root, pso) = host
            .compute_pipeline(&capped)
            .unwrap_or_else(|e| panic!("{label}: compute_pipeline failed: {e}"));
        let mut sizes = Vec::new();
        for s0 in [0u32, 2, 3, 898_339, (1 << 27) - 1] {
            let mut ud = vec![0u32; capped.user_data_dwords().max(1) as usize];
            for u in &capped.user_data {
                if (u.root, u.dword) == (RootType::Constants, 0) {
                    ud[u.index as usize] = s0;
                }
            }
            let words = host
                .dispatch(
                    &root,
                    &pso,
                    &Dispatch {
                        roots: vec![(0, Root::Constants(ud))],
                        views: &views,
                        samplers: &[],
                        groups: [1, 1, 1],
                    },
                )
                .unwrap_or_else(|e| panic!("{label}: dispatch failed: {e}"));
            sizes.push(words[0][..2].to_vec());
        }
        assert_eq!(sizes[0], [13, 7], "{label}: index 0 reads its texture");
        // past the title's slots: every index reads the null slot 2, none the UAV or past the heap
        for (k, size) in sizes.iter().enumerate().skip(1) {
            assert_eq!(size, &sizes[1], "{label}: index {k} reads the null slot");
        }
        assert_ne!(sizes[1], [13, 7], "{label}");
    }
}

#[test]
fn a_view_index_a_table_gives_is_left_alone_under_heap_cap() {
    // The first test's program: both views from a table the host made (root type `Table`).
    let program = native(&[
        0x7E02_020A, // v_mov_b32 v1, s10
        0xE070_0000,
        0x8004_0100, // buffer_store_dword v1, off, s[16:19], 0
    ]);
    assert_eq!(
        translate_limited(&program, Some(2)).dxil,
        translate_limited(&program, None).dxil
    );
}

/// `native` translated with the title view limit at `limit` and the remap table `len` long.
fn translate_remapped(native: &NativeProgram, limit: u32, len: Option<u32>) -> HostShader {
    let was = super::shader::TITLE_REMAP_LEN_HERE.replace(len);
    let shader = translate_limited(native, Some(limit));
    super::shader::TITLE_REMAP_LEN_HERE.set(was);
    shader
}

#[test]
fn a_view_index_the_title_computes_past_the_limit_reads_the_remapped_slot_under_heap_cap_3() {
    // (heapcap, VICTORY_HEAP_CAP=3) Title slots 0 and 1 are the host's own; slot 2 is the null
    // slot; title slots from 2 on read the table: title 2 -> host 3, title 4 -> host 4, title 5
    // -> host 5, the rest (3 dead, 6.., past the table) -> the null slot 2.
    let program = heap_indexed();
    let remapped = translate_remapped(&program, 2, Some(8));
    assert_ne!(remapped.dxil, translate_remapped(&program, 2, None).dxil);
    assert_eq!(
        translate_remapped(&program, 2, Some(0)).dxil,
        translate_remapped(&program, 2, None).dxil,
        "no table: the clamp of =1"
    );
    let (t0, t3, t4, t5) = (
        [[0.0f32; 4]; 13 * 7],
        [[0.0f32; 4]; 5 * 3],
        [[0.0f32; 4]; 4 * 4],
        [[0.0f32; 4]; 9 * 2],
    );
    let views = [
        View::Texture(13, 7, &t0),
        View::RawUav(4),
        View::NullTexture,
        View::Texture(5, 3, &t3),
        View::Texture(4, 4, &t4),
        View::Texture(9, 2, &t5),
    ];
    // dword k: the host slot of title slot 2 + k; dword 8 (past the table): the null slot
    let table = vec![3, 2, 4, 5, 2, 2, 2, 2, 2];
    let _one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    for (label, warp) in [("hardware", false), ("warp", true)] {
        let Ok(host) = Host::new(warp) else {
            eprintln!("no SM 6.6 {label} device; skipped");
            continue;
        };
        let (root, pso) = host
            .compute_pipeline(&remapped)
            .unwrap_or_else(|e| panic!("{label}: compute_pipeline failed: {e}"));
        let size = |s0: u32| {
            let mut ud = vec![0u32; remapped.user_data_dwords().max(1) as usize];
            for u in &remapped.user_data {
                if (u.root, u.dword) == (RootType::Constants, 0) {
                    ud[u.index as usize] = s0;
                }
            }
            let words = host
                .dispatch(
                    &root,
                    &pso,
                    &Dispatch {
                        roots: vec![
                            (0, Root::Constants(ud)),
                            (
                                1,
                                Root::View {
                                    srv: true,
                                    data: table.clone(),
                                },
                            ),
                        ],
                        views: &views,
                        samplers: &[],
                        groups: [1, 1, 1],
                    },
                )
                .unwrap_or_else(|e| panic!("{label}: dispatch failed: {e}"));
            words[0][..2].to_vec()
        };
        assert_eq!(size(0), [13, 7], "{label}: below the limit, its own slot");
        assert_eq!(size(2), [5, 3], "{label}: title 2 reads host 3");
        assert_eq!(size(4), [4, 4], "{label}: title 4 reads host 4");
        assert_eq!(size(5), [9, 2], "{label}: title 5 reads host 5");
        let null = size(3);
        assert!(![[13, 7], [5, 3], [4, 4], [9, 2]].iter().any(|t| null == t), "{label}: {null:?}");
        for s0 in [6, 9, 10, 898_339, (1 << 27) - 1] {
            assert_eq!(size(s0), null, "{label}: title {s0} reads the null slot");
        }
    }
}

#[test]
fn every_view_index_the_title_computes_reads_the_whole_heap_table_under_heap_cap_4() {
    // (heapcap, VICTORY_HEAP_CAP=4) The table names the host slot of every title slot: here title
    // 0 -> host 0 (its own), title 1 -> host 1 (the UAV the program stores to), title 2 ->
    // host 2 (null), title 3 -> host 3, title 5 -> host 5, title 6 -> host 4; the last dword (FULL_REMAP - 1) is null, and every index past it reads it.
    let program = heap_indexed();
    let full = {
        let was = super::shader::TITLE_REMAP_FULL_HERE.replace(Some(true));
        let shader = translate_limited(&program, None);
        super::shader::TITLE_REMAP_FULL_HERE.set(was);
        shader
    };
    assert_ne!(full.dxil, translate_limited(&program, None).dxil);
    {
        let was = super::shader::TITLE_REMAP_FULL_HERE.replace(Some(false));
        assert_eq!(
            translate_limited(&program, None).dxil,
            translate(&program).dxil,
            "off: as before"
        );
        super::shader::TITLE_REMAP_FULL_HERE.set(was);
    }
    let (t0, t3, t4, t5) = (
        [[0.0f32; 4]; 13 * 7],
        [[0.0f32; 4]; 5 * 3],
        [[0.0f32; 4]; 4 * 4],
        [[0.0f32; 4]; 9 * 2],
    );
    let views = [
        View::Texture(13, 7, &t0),
        View::RawUav(4),
        View::NullTexture,
        View::Texture(5, 3, &t3),
        View::Texture(4, 4, &t4),
        View::Texture(9, 2, &t5),
    ];
    let mut table = vec![2u32; super::shader::FULL_REMAP as usize];
    table[0] = 0;
    table[1] = 1;
    table[6] = 4;
    table[3] = 3;
    table[5] = 5;
    table[898_339] = 4;
    let _one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    for (label, warp) in [("hardware", false), ("warp", true)] {
        let Ok(host) = Host::new(warp) else {
            eprintln!("no SM 6.6 {label} device; skipped");
            continue;
        };
        let (root, pso) = host
            .compute_pipeline(&full)
            .unwrap_or_else(|e| panic!("{label}: compute_pipeline failed: {e}"));
        let size = |s0: u32| {
            let mut ud = vec![0u32; full.user_data_dwords().max(1) as usize];
            for u in &full.user_data {
                if (u.root, u.dword) == (RootType::Constants, 0) {
                    ud[u.index as usize] = s0;
                }
            }
            let words = host
                .dispatch(
                    &root,
                    &pso,
                    &Dispatch {
                        roots: vec![
                            (0, Root::Constants(ud)),
                            (
                                1,
                                Root::View {
                                    srv: true,
                                    data: table.clone(),
                                },
                            ),
                        ],
                        views: &views,
                        samplers: &[],
                        groups: [1, 1, 1],
                    },
                )
                .unwrap_or_else(|e| panic!("{label}: dispatch failed: {e}"));
            words[0][..2].to_vec()
        };
        assert_eq!(size(0), [13, 7], "{label}: title 0 reads host 0");
        assert_eq!(size(6), [4, 4], "{label}: title 6 reads host 4");
        assert_eq!(size(3), [5, 3], "{label}: title 3 reads host 3");
        assert_eq!(size(5), [9, 2], "{label}: title 5 reads host 5");
        assert_eq!(size(898_339), [4, 4], "{label}: a high title slot, packed");
        let null = size(2);
        assert!(![[13, 7], [5, 3], [4, 4], [9, 2]].iter().any(|t| null == t), "{label}: {null:?}");
        for s0 in [4, 7, 1_245_183, (1 << 21) - 1, (1 << 27) - 1] {
            assert_eq!(size(s0), null, "{label}: title {s0} reads the null slot");
        }
    }
}

#[test]
fn a_flagged_table_entry_notes_its_title_slot_in_the_fault_buffer_under_heap_cap_5() {
    // (heapcap, VICTORY_HEAP_CAP=5) Entries as the host keeps them: title 0 -> 0, title 1 -> 1
    // (the UAV the program stores to), title 3 -> 3 (packed, shown), title 5 -> 5 | FAULT_FLAG
    // (packed, armed: read, and noted as touched), title 898,339 -> 2 | FAULT_FLAG (live, not
    // packed: null, and noted), title 4 -> 2 (dead: null, not noted). The fault buffer is the
    // raw UAV at slot 6.
    use super::shader::{FAULT_DWORDS, FAULT_FLAG, FULL_REMAP};
    let program = heap_indexed();
    let translate_with = |fault: Option<u32>| {
        let full = super::shader::TITLE_REMAP_FULL_HERE.replace(Some(true));
        let slot = super::shader::TITLE_FAULT_SLOT_HERE.replace(fault);
        let shader = translate_limited(&program, None);
        super::shader::TITLE_FAULT_SLOT_HERE.set(slot);
        super::shader::TITLE_REMAP_FULL_HERE.set(full);
        shader
    };
    let faulting = translate_with(Some(6));
    assert_ne!(faulting.dxil, translate_with(None).dxil);
    assert_eq!(translate_with(Some(0)).dxil, translate_with(None).dxil, "0: =4's");
    let (t0, t3, t5) = (
        [[0.0f32; 4]; 13 * 7],
        [[0.0f32; 4]; 5 * 3],
        [[0.0f32; 4]; 9 * 2],
    );
    let zeros = vec![0u32; FAULT_DWORDS as usize];
    let views = [
        View::Texture(13, 7, &t0),
        View::RawUav(4),
        View::NullTexture,
        View::Texture(5, 3, &t3),
        View::NullTexture,
        View::Texture(9, 2, &t5),
        View::RawUavData(&zeros),
    ];
    let mut table = vec![2u32; FULL_REMAP as usize];
    table[0] = 0;
    table[1] = 1;
    table[3] = 3;
    table[5] = 5 | FAULT_FLAG;
    table[898_339] = 2 | FAULT_FLAG;
    let place = |k: u32| (1 + k % (FAULT_DWORDS - 1)) as usize;
    let _one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    for (label, warp) in [("hardware", false), ("warp", true)] {
        let Ok(host) = Host::new(warp) else {
            eprintln!("no SM 6.6 {label} device; skipped");
            continue;
        };
        let (root, pso) = host
            .compute_pipeline(&faulting)
            .unwrap_or_else(|e| panic!("{label}: compute_pipeline failed: {e}"));
        let run = |s0: u32| {
            let mut ud = vec![0u32; faulting.user_data_dwords().max(1) as usize];
            for u in &faulting.user_data {
                if (u.root, u.dword) == (RootType::Constants, 0) {
                    ud[u.index as usize] = s0;
                }
            }
            host.dispatch(
                &root,
                &pso,
                &Dispatch {
                    roots: vec![
                        (0, Root::Constants(ud)),
                        (
                            1,
                            Root::View {
                                srv: true,
                                data: table.clone(),
                            },
                        ),
                    ],
                    views: &views,
                    samplers: &[],
                    groups: [1, 1, 1],
                },
            )
            .unwrap_or_else(|e| panic!("{label}: dispatch failed: {e}"))
        };
        let noted = |words: &[Vec<u32>]| -> Vec<u32> {
            words[1].iter().copied().filter(|&w| w != 0).collect()
        };
        let w = run(3);
        assert_eq!(w[0][..2], [5, 3], "{label}: shown: its own view");
        assert!(noted(&w).is_empty(), "{label}: shown, not armed: nothing noted");
        let w = run(5);
        assert_eq!(w[0][..2], [9, 2], "{label}: armed: still its own view");
        assert_eq!(w[1][place(5)], 5, "{label}: armed: noted as touched");
        assert_eq!(noted(&w), [5], "{label}");
        let w = run(898_339);
        assert_eq!(w[1][place(898_339)], 898_339, "{label}: live, not packed: noted");
        assert_eq!(noted(&w), [898_339], "{label}");
        let null = w[0][..2].to_vec();
        assert!(![[13, 7], [5, 3], [9, 2]].iter().any(|t| null == t), "{label}: {null:?}");
        let w = run(4);
        assert_eq!(w[0][..2], null[..], "{label}: dead: null");
        assert!(noted(&w).is_empty(), "{label}: dead: nothing noted");
        let w = run(0);
        assert_eq!(w[0][..2], [13, 7], "{label}");
        assert!(noted(&w).is_empty(), "{label}");
    }
}

/// (`VICTORY_HEAP_BUFFER_ADDRESS=1`) A program that picks a raw buffer from the view heap by a
/// root constant (`s0`, root type `ResourceHeap`), loads two dwords at byte `s1` and stores them
/// to the raw UAV at heap index 1.
fn heap_buffer_program() -> NativeProgram {
    let mut program = native(&[]);
    let words = [
        0x8F14_8500, // s_lshl_b32 s20, s0, 5
        0xBE85_0380, // s_mov_b32 s5, 0
        0xF408_0202,
        0x2800_0000, // s_load_dwordx4 s[8:11], s[4:5], s20
        0xF408_0402,
        0xFA00_0020, // s_load_dwordx4 s[16:19], s[4:5], 0x20
        0x7E00_0201, // v_mov_b32 v0, s1
        0xE034_1000,
        0x8002_0100, // buffer_load_dwordx2 v[1:2], v0, s[8:11], 0 offen
        0xE074_0000,
        0x8004_0100, // buffer_store_dwordx2 v[1:2], off, s[16:19], 0
        S_ENDPGM,
    ];
    program.code = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    program.layout[1].root = RootType::ResourceHeap;
    program.used.clear();
    program
}

fn translate_buffer_address(native: &NativeProgram, on: bool) -> HostShader {
    let was = super::lower::buffer_address::FORCED.replace(Some(on));
    let shader = translate(native);
    super::lower::buffer_address::FORCED.set(was);
    shader
}

/// Runs `shader` (one of [`heap_buffer_program`]'s kind) on `host` with root constants `s0`,
/// `s1`, the heap `views`, the buffer ranges table `ranges` and the memory directory
/// `directory` (zeroed strides and faults), and returns the first `n` dwords of the UAV at heap
/// index 1.
#[allow(clippy::too_many_arguments)]
fn run_by_address(
    host: &Host,
    label: &str,
    shader: &HostShader,
    views: &[View],
    ranges: &[u32],
    directory: &[u32],
    [s0, s1]: [u32; 2],
    n: usize,
) -> Vec<u32> {
    let words = |root: RootType, dword: u32| match (root, dword) {
        (RootType::Constants, 0) => Some(s0),
        (RootType::Constants, 1) => Some(s1),
        _ => None,
    };
    run_by_address_with(host, label, shader, views, ranges, directory, &words, n)
}

/// [`run_by_address`] with the user-data dwords `words` gives (0 for the rest, 1 for
/// `USER_MEMORY_BASE`).
#[allow(clippy::too_many_arguments)]
fn run_by_address_with(
    host: &Host,
    label: &str,
    shader: &HostShader,
    views: &[View],
    ranges: &[u32],
    directory: &[u32],
    words: &dyn Fn(RootType, u32) -> Option<u32>,
    n: usize,
) -> Vec<u32> {
    use super::shader::{
        MEMORY_FAULTS, SLOT_BUFFER_RANGES, SLOT_FAULTS, SLOT_MEMORY, SLOT_STRIDES,
        USER_MEMORY_BASE,
    };
    let (root, pso) = host
        .compute_pipeline(shader)
        .unwrap_or_else(|e| panic!("{label}: compute_pipeline failed: {e}"));
    let mut ud = vec![0u32; shader.user_data_dwords().max(1) as usize];
    for u in &shader.user_data {
        ud[u.index as usize] = match (words(u.root, u.dword), u.root) {
            (Some(w), _) => w,
            (None, RootType::Other(USER_MEMORY_BASE)) => 1,
            (None, _) => 0,
        };
    }
    // root views after the constants, by slot, as the harness orders them
    let mut roots = vec![(0, Root::Constants(ud))];
    let mut slots: Vec<(u8, bool)> = shader
        .root_srvs
        .iter()
        .map(|&s| (s, true))
        .chain(shader.root_uavs.iter().map(|&s| (s, false)))
        .collect();
    slots.sort_unstable();
    for (k, (slot, srv)) in slots.into_iter().enumerate() {
        let data = match slot {
            SLOT_STRIDES => vec![0; 64],
            SLOT_BUFFER_RANGES => ranges.to_vec(),
            SLOT_MEMORY => directory.to_vec(),
            SLOT_FAULTS => vec![0; MEMORY_FAULTS as usize],
            other => panic!("root view {other}"),
        };
        roots.push((k as u32 + 1, Root::View { srv, data }));
    }
    let words = host
        .dispatch(
            &root,
            &pso,
            &Dispatch {
                roots,
                views,
                samplers: &[],
                groups: [1, 1, 1],
            },
        )
        .unwrap_or_else(|e| panic!("{label}: dispatch failed: {e}"));
    words[0][..n].to_vec()
}

#[test]
fn a_buffer_picked_from_the_heap_reads_the_same_bytes_by_address_as_through_its_descriptor() {
    use super::shader::{SLOT_BUFFER_RANGES, SLOT_FAULTS, SLOT_MEMORY};
    let program = heap_buffer_program();
    let plain = translate_buffer_address(&program, false);
    let by_address = translate_buffer_address(&program, true);
    assert_ne!(plain.dxil, by_address.dxil);
    assert!(
        !plain.root_srvs.contains(&SLOT_BUFFER_RANGES) && !plain.root_srvs.contains(&SLOT_MEMORY),
        "off: through the descriptor"
    );
    assert!(by_address.root_srvs.contains(&SLOT_BUFFER_RANGES));
    assert!(by_address.root_srvs.contains(&SLOT_MEMORY));
    assert!(by_address.root_uavs.contains(&SLOT_FAULTS));
    // The buffer: six dwords. Title memory page 3 (the host copy at heap slot 2) holds them at
    // byte 0x10, with other bytes after them.
    let data = [0x11u32, 0x22, 0x33, 0x44, 0x55, 0x66];
    let mut page = vec![0xDEAD_BEEFu32; 16384];
    page[4..10].copy_from_slice(&data);
    let views = [
        View::Buffer {
            data: &data,
            stride: super::host::Stride::Raw,
        },
        View::RawUav(4),
        View::RawUavData(&page),
    ];
    // Ranges by title slot: 0 the buffer; 3 (no host descriptor at all) its second half.
    let mut ranges = vec![0u32; 4 * 8];
    ranges[..4].copy_from_slice(&[0x3_0010, 0, 24, 4]);
    ranges[12..16].copy_from_slice(&[0x3_0018, 0, 16, 4]);
    // The memory directory: page 3's copy is copy 1 (heap slot USER_MEMORY_BASE + 1 = 2).
    let mut directory = vec![0u32; 2 * 8];
    directory[6] = 1;
    let _one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    for (label, warp) in [("hardware", false), ("warp", true)] {
        let Ok(host) = Host::new(warp) else {
            eprintln!("no SM 6.6 {label} device; skipped");
            continue;
        };
        let run = |shader: &HostShader, s0: u32, s1: u32| {
            run_by_address(&host, label, shader, &views, &ranges, &directory, [s0, s1], 2)
        };
        for (s1, want) in [(0, [0x11, 0x22]), (8, [0x33, 0x44]), (16, [0x55, 0x66])] {
            assert_eq!(run(&plain, 0, s1), want, "{label}: descriptor, byte {s1}");
            assert_eq!(run(&by_address, 0, s1), want, "{label}: by address, byte {s1}");
        }
        // past the end: zeros, as a raw view gives
        for s1 in [24, 4096, 0xFFFF_FFF8] {
            assert_eq!(run(&plain, 0, s1), [0, 0], "{label}: descriptor, byte {s1}");
            assert_eq!(run(&by_address, 0, s1), [0, 0], "{label}: by address, byte {s1}");
        }
        // the last dword in, the next out: dword by dword, as the raw view
        assert_eq!(run(&plain, 0, 20), [0x66, 0], "{label}: descriptor, byte 20");
        assert_eq!(run(&by_address, 0, 20), [0x66, 0], "{label}: by address, byte 20");
        // a title slot the host has no descriptor for reads its own buffer by address
        assert_eq!(run(&by_address, 3, 0), [0x33, 0x44], "{label}: title slot 3");
        assert_eq!(run(&by_address, 3, 16), [0, 0], "{label}: title slot 3 past its end");
        // a slot with no buffer view (size 0) reads zeros
        assert_eq!(run(&by_address, 5, 0), [0, 0], "{label}: no view");
    }
}

/// A program that picks a constant buffer (or a buffer) from the view heap by a root constant
/// (`s0`, root type `ResourceHeap`), reads eight dwords of it at byte `s1` with a scalar load and
/// stores them to the raw UAV at heap index 1.
fn heap_constants_program() -> NativeProgram {
    let mut program = heap_buffer_program();
    let mut words = vec![
        0x8F14_8500, // s_lshl_b32 s20, s0, 5
        0xBE85_0380, // s_mov_b32 s5, 0
        0xF408_0202,
        0x2800_0000, // s_load_dwordx4 s[8:11], s[4:5], s20
        0xF408_0402,
        0xFA00_0020, // s_load_dwordx4 s[16:19], s[4:5], 0x20
        0xF42C_0604,
        0x0200_0000, // s_buffer_load_dwordx8 s[24:31], s[8:11], s1
    ];
    for out in 0..8u32 {
        // v_mov_b32 v1, s<24 + out>; buffer_store_dword v1, off, s[16:19], 0 offset:4*out
        words.push(0x7E00_0000 | 1 << 17 | 1 << 9 | (24 + out));
        words.push(0x38 << 26 | 0x1C << 18 | (4 * out));
        words.push(0x80 << 24 | (16 >> 2) << 16 | 1 << 8);
    }
    words.push(S_ENDPGM);
    program.code = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    program
}

#[test]
fn a_constant_buffer_picked_from_the_heap_reads_the_same_constants_by_address() {
    use super::shader::{SLOT_BUFFER_RANGES, SLOT_MEMORY};
    let program = heap_constants_program();
    let plain = translate_buffer_address(&program, false);
    let by_address = translate_buffer_address(&program, true);
    assert!(!plain.root_srvs.contains(&SLOT_BUFFER_RANGES));
    assert!(by_address.root_srvs.contains(&SLOT_BUFFER_RANGES));
    assert!(by_address.root_srvs.contains(&SLOT_MEMORY));
    // 256 bytes of constants, at byte 0x100 of title memory page 3.
    let constants: Vec<u32> = (0..64).map(|k| 0x100 + k).collect();
    let mut page = vec![0xDEAD_BEEFu32; 16384];
    page[64..128].copy_from_slice(&constants);
    let views = [View::Cbv(&constants), View::RawUav(8), View::RawUavData(&page)];
    // Title slot 0: the constant buffer (stride 0); 3: a raw buffer over its last 32 bytes.
    let mut ranges = vec![0u32; 4 * 8];
    ranges[..4].copy_from_slice(&[0x3_0100, 0, 256, 0]);
    ranges[12..16].copy_from_slice(&[0x3_01E0, 0, 32, 1]);
    let mut directory = vec![0u32; 2 * 8];
    directory[6] = 1;
    let _one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    for (label, warp) in [("hardware", false), ("warp", true)] {
        let Ok(host) = Host::new(warp) else {
            eprintln!("no SM 6.6 {label} device; skipped");
            continue;
        };
        let run = |shader: &HostShader, s0: u32, s1: u32| {
            run_by_address(&host, label, shader, &views, &ranges, &directory, [s0, s1], 8)
        };
        for s1 in [0u32, 32, 128] {
            let want = constants[(s1 / 4) as usize..][..8].to_vec();
            assert_eq!(run(&plain, 0, s1), want, "{label}: descriptor, byte {s1}");
            assert_eq!(run(&by_address, 0, s1), want, "{label}: by address, byte {s1}");
        }
        // the last 16 bytes in, the rest out
        let mut want = constants[60..].to_vec();
        want.extend([0; 4]);
        assert_eq!(run(&by_address, 0, 240), want, "{label}: by address, byte 240");
        // a buffer view the host has no descriptor for, by its own range
        assert_eq!(run(&by_address, 3, 0), constants[56..], "{label}: title slot 3");
    }
}

/// A program that reads eight dwords at byte `s1` of the constant buffer at dword 0 of its table
/// (root parameter 1, `s4`) with a scalar load and stores them to the raw UAV at dword 8 of the
/// table.
fn table_constants_program() -> NativeProgram {
    let mut program = native_of(&[], 4, 0);
    let mut words = vec![
        0xBE85_0380, // s_mov_b32 s5, 0
        0xF408_0202,
        0xFA00_0000, // s_load_dwordx4 s[8:11], s[4:5], 0
        0xF408_0402,
        0xFA00_0020, // s_load_dwordx4 s[16:19], s[4:5], 0x20
        0xF42C_0604,
        0x0200_0000, // s_buffer_load_dwordx8 s[24:31], s[8:11], s1
    ];
    for out in 0..8u32 {
        // v_mov_b32 v1, s<24 + out>; buffer_store_dword v1, off, s[16:19], 0 offset:4*out
        words.push(0x7E00_0000 | 1 << 17 | 1 << 9 | (24 + out));
        words.push(0x38 << 26 | 0x1C << 18 | (4 * out));
        words.push(0x80 << 24 | (16 >> 2) << 16 | 1 << 8);
    }
    words.push(S_ENDPGM);
    program.code = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    program.used[0].kind = ResourceType::Cbv;
    program
}

#[test]
fn a_tables_constant_buffer_reads_the_same_constants_by_title_address() {
    use super::shader::{
        BUFFER_RANGES_COUNT, NO_TABLE_SLOT, SLOT_BUFFER_RANGES, SLOT_MEMORY, TABLE_SLOT_KEPT,
        USER_TABLE_SLOT,
    };
    let program = table_constants_program();
    let plain = translate_buffer_address(&program, false);
    let by_address = translate_buffer_address(&program, true);
    let asks = |s: &HostShader, dword: u32| {
        s.user_data
            .iter()
            .any(|u| u.root == RootType::Other(USER_TABLE_SLOT) && u.slot == 1 && u.dword == dword)
    };
    assert!(!plain.root_srvs.contains(&SLOT_BUFFER_RANGES) && !asks(&plain, 0));
    assert!(by_address.root_srvs.contains(&SLOT_BUFFER_RANGES));
    assert!(by_address.root_srvs.contains(&SLOT_MEMORY));
    // the host is told the table's title slot, and that no constant buffer of it is read
    // through its descriptor (so it need not make the table afresh for one)
    assert!(asks(&by_address, 0) && !asks(&by_address, TABLE_SLOT_KEPT));
    // 256 bytes of constants, at byte 0x100 of title memory page 3; the host's descriptor at
    // heap slot 0 holds the same.
    let constants: Vec<u32> = (0..64).map(|k| 0x200 + k).collect();
    let mut page = vec![0xDEAD_BEEFu32; 16384];
    page[64..128].copy_from_slice(&constants);
    let views = [View::Cbv(&constants), View::RawUav(8), View::RawUavData(&page)];
    // The table's title slots start at 4: entry 4 is the constant buffer's range.
    let mut ranges = vec![0u32; 4 * 8];
    ranges[16..20].copy_from_slice(&[0x3_0100, 0, 256, 0]);
    let mut directory = vec![0u32; 2 * 8];
    directory[6] = 1;
    let _one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    for (label, warp) in [("hardware", false), ("warp", true)] {
        let Ok(host) = Host::new(warp) else {
            eprintln!("no SM 6.6 {label} device; skipped");
            continue;
        };
        // the table bound at host slot 0 (a fresh copy, or in place), its title slot `title`
        let run = |shader: &HostShader, s1: u32, title: u32| {
            let words = move |root: RootType, dword: u32| match (root, dword) {
                (RootType::Constants, 1) => Some(s1),
                (RootType::Other(USER_TABLE_SLOT), 0) => Some(title),
                _ => None,
            };
            run_by_address_with(&host, label, shader, &views, &ranges, &directory, &words, 8)
        };
        for s1 in [0u32, 32, 128] {
            let want = constants[(s1 / 4) as usize..][..8].to_vec();
            assert_eq!(run(&plain, s1, 4), want, "{label}: descriptor, byte {s1}");
            assert_eq!(run(&by_address, s1, 4), want, "{label}: by address, byte {s1}");
        }
        // the last 16 bytes in, the rest out, as for a constant buffer picked from the heap
        let mut want = constants[60..].to_vec();
        want.extend([0; 4]);
        assert_eq!(run(&by_address, 240, 4), want, "{label}: by address, byte 240");
        // the title slot, not the bound one, names the range: none there reads zeros
        assert_eq!(run(&by_address, 0, 5), [0; 8], "{label}: title slot 5");
    }
    // No table: past every entry of the host's ranges table (its last entry, never written,
    // reads 0); not run here, where the table is 8 entries long and a root view has no bounds.
    const { assert!(NO_TABLE_SLOT >= BUFFER_RANGES_COUNT && NO_TABLE_SLOT < u32::MAX / 2) };
}

#[test]
fn a_direct_buffer_range_reads_its_copy_without_touching_the_memory_directory() {
    use super::shader::{BUFFER_RANGE_DIRECT, SLOT_BUFFER_RANGES, SLOT_MEMORY};
    let program = heap_buffer_program();
    let by_address = translate_buffer_address(&program, true);
    assert!(by_address.root_srvs.contains(&SLOT_BUFFER_RANGES));
    assert!(by_address.root_srvs.contains(&SLOT_MEMORY));
    // A small unaligned 24-byte copy at offset 0x10 inside copy 1 (heap slot USER_MEMORY_BASE + 1 = 2).
    let data = [0xAA11u32, 0xBB22, 0xCC33, 0xDD44, 0xEE55, 0xFF66];
    let mut copy_bytes = vec![0xDEAD_BEEFu32; 64];
    copy_bytes[4..10].copy_from_slice(&data);
    let views = [
        View::Buffer {
            data: &data,
            stride: super::host::Stride::Raw,
        },
        View::RawUav(4),
        View::RawUavData(&copy_bytes),
    ];
    // Slot 0: direct copy 1 at byte offset 0x10, size 24, stride 4.
    // Slot 2: direct copy 1 at byte offset 0x18, size 16, stride 4.
    // Slot 5: empty [0, 0, 0, 0].
    let mut ranges = vec![0u32; 4 * 8];
    ranges[..4].copy_from_slice(&[0x10, BUFFER_RANGE_DIRECT | 1, 24, 4]);
    ranges[8..12].copy_from_slice(&[0x18, BUFFER_RANGE_DIRECT | 1, 16, 4]);
    // Empty memory directory: proves direct entries never touch SLOT_MEMORY.
    let directory = vec![0u32; 2 * 8];
    let _one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let Ok(host) = Host::new(true) else {
        eprintln!("no SM 6.6 warp device; skipped");
        return;
    };
    let run = |s0: u32, s1: u32| {
        run_by_address(&host, "warp", &by_address, &views, &ranges, &directory, [s0, s1], 2)
    };
    assert_eq!(run(0, 0), [0xAA11, 0xBB22], "direct copy at offset 0x10");
    assert_eq!(run(0, 8), [0xCC33, 0xDD44], "direct copy at offset 0x18");
    assert_eq!(run(0, 20), [0xFF66, 0], "partial OOB returns 0 for trailing dword");
    assert_eq!(run(0, 24), [0, 0], "OOB returns 0");
    assert_eq!(run(2, 0), [0xCC33, 0xDD44], "second direct slot");
    assert_eq!(run(5, 0), [0, 0], "empty slot returns 0");
}
