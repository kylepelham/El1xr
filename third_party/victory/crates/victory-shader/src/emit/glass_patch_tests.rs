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
