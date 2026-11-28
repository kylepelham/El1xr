use super::*;
use crate::emit::lower::{Lowered, Parts, lower};
use crate::native::{
    ComputeIo, HwStage, NativeProgram, ResourceType, RootParam, RootType, StageIo, UsedResource,
};
use crate::{io, ir, resource, structure};

#[test]
fn values_parse() {
    assert_eq!(parse_mode(None), None);
    assert_eq!(parse_mode(Some("0")), None);
    assert_eq!(parse_mode(Some("3")), None);
    assert_eq!(parse_mode(Some(" 1 ")), Some(Hold::Clamp));
    assert_eq!(parse_mode(Some("2")), Some(Hold::KeepNan));
    assert!(parse_only(None).is_empty());
    assert!(parse_only(Some(" ")).is_empty());
    assert!(parse_only(Some("ALL")).is_empty());
    assert_eq!(parse_only(Some("glass")), GLASS.map(str::to_owned));
    assert_eq!(
        parse_only(Some("2DA4BEB386509B77, CS_AlphaTransmissionDenoise,")),
        ["2da4beb386509b77", "CS_AlphaTransmissionDenoise"].map(str::to_owned)
    );
}

#[test]
fn programs_are_listed_by_hash_or_name_part() {
    let container = b"..\0CS_AlphTransmissionPrefilterMip_610a2fad_Wrapped\0..";
    let hash = blake3::hash(container).to_hex()[..16].to_owned();
    assert!(listed(container, &[]), "an empty list takes every program");
    assert!(listed(container, &parse_only(Some("glass"))));
    assert!(listed(
        container,
        &parse_only(Some(&hash.to_ascii_uppercase()))
    ));
    assert!(listed(container, &["0000000000000000".to_owned(), hash]));
    assert!(!listed(container, &["0000000000000000".to_owned()]));
    assert!(!listed(container, &["CS_RayPrePass_Alpha".to_owned()]));
    let prepass = b"\0CS_RayPrePass_Alpha_a80bdf75_Wrapped\0";
    assert!(
        !listed(prepass, &parse_only(Some("glass"))),
        "ray distances are data"
    );
    let probe_miss = b"\0CS_ScreenProbe_RayMiss_80a88544_Wrapped\0";
    assert!(!listed(probe_miss, &parse_only(Some("glass"))));
    assert!(listed(
        b"\0CS_RayMiss_de6b91a6_Wrapped\0",
        &parse_only(Some("glass"))
    ));
    let tiles = b"\0CS_AlphaTransmissionDepthTileMipDilate_7f19bae0_Wrapped\0";
    assert!(
        !listed(tiles, &parse_only(Some("glass"))),
        "depth tiles are data"
    );
    let denoise = b"\0CS_AlphaTransmissionDenoise_2871f2e9_Wrapped\0";
    assert!(listed(denoise, &parse_only(Some("glass"))));
}

#[test]
fn a_held_channel_stays_in_the_hdr_range() {
    for hold in [Hold::Clamp, Hold::KeepNan] {
        assert_eq!(held(hold, f32::INFINITY), Some(65024.0));
        assert_eq!(held(hold, f32::NEG_INFINITY), Some(-65024.0));
        assert_eq!(held(hold, 70000.0), Some(65024.0));
        assert_eq!(held(hold, 65504.0), Some(65024.0));
        assert_eq!(held(hold, 1.5), Some(1.5));
        assert_eq!(held(hold, -3.0), Some(-3.0));
        assert_eq!(held(hold, 0.0), Some(0.0));
    }
    assert_eq!(held(Hold::Clamp, f32::NAN), Some(0.0));
    assert_eq!(
        held(Hold::KeepNan, f32::NAN),
        None,
        "the texel keeps what it held"
    );
}

#[test]
fn off_by_default() {
    assert_eq!(mode(), None);
    assert_eq!(here(), None);
}

/// 64 threads in x; each stores its thread number's bits in all four channels of texel (0, 0)
/// of the 2D UAV at table slot 0 (`image_store v[4:7], v[2:3], s[4:11] dmask:0xf`).
fn program() -> NativeProgram {
    let words = [
        0xBE82_0300, // s_mov_b32 s2, s0
        0xBE83_0380, // s_mov_b32 s3, 0
        0xF40C_0101,
        0xFA00_0000, // s_load_dwordx8 s[4:11], s[2:3], null
        0x7E04_0280, // v_mov_b32 v2, 0
        0x7E06_0280, // v_mov_b32 v3, 0
        0x7E08_0300, // v_mov_b32 v4, v0
        0x7E0A_0300, // v_mov_b32 v5, v0
        0x7E0C_0300, // v_mov_b32 v6, v0
        0x7E0E_0300, // v_mov_b32 v7, v0
        0xF020_3F08,
        0x0001_0402, // image_store v[4:7], v[2:3], s[4:11] dmask:0xf 2D unorm glc
        0xBF81_0000, // s_endpgm
    ];
    NativeProgram {
        stage: HwStage::Cs,
        wave64: false,
        vgprs: 8,
        layout: vec![RootParam {
            root: RootType::Table,
            table_offset_dwords: 0,
            sgprs: 1,
            base_register: 0,
            user_data: 0,
            slot: 0,
        }],
        used: vec![UsedResource {
            kind: ResourceType::Uav,
            root: RootType::Table,
            table_offset_dwords: 0,
            lower_bound: 0,
            stride: 0,
            dwords: 8,
            user_data: 0,
            slot: 0,
        }],
        io: StageIo::Cs(ComputeIo {
            rsrc2: 1 << 1,
            threads: [64, 1, 1],
        }),
        code: words.into_iter().flat_map(u32::to_le_bytes).collect(),
    }
}

fn lowered(hold: Option<Hold>) -> Lowered {
    let native = program();
    let p = ir::build(&native.code, native.wave64, native.vgprs).unwrap();
    let st = structure::structure(&p).unwrap();
    let res = resource::track(&p, &native.layout, native.stage, Some(&st)).unwrap();
    let io = io::raise(&p, &native).unwrap();
    with_hold_for_test(hold, || {
        lower(&Parts {
            native: &native,
            p: &p,
            st: &st,
            io: &io,
            res: &res,
            inputs: vec![],
            outputs: vec![],
            prims: vec![],
            mesh_stride: 0,
        })
    })
    .unwrap_or_else(|e| panic!("{e}"))
}

fn validate(l: &Lowered) {
    if let Ok(mut v) = crate::emit::validator::Validator::load(
        crate::emit::validator::Validator::default_path().as_deref(),
    ) {
        let mut copy = l.dxil.clone();
        v.validate(&mut copy).expect("DXIL validation failed");
    }
}

/// Off, the store translates as before (the NaN scrub's); each level gives another program,
/// and the same one each time.
#[test]
fn a_compute_image_store_is_held_when_listed() {
    let off = lowered(None);
    let clamp = lowered(Some(Hold::Clamp));
    let keep = lowered(Some(Hold::KeepNan));
    assert_ne!(off.dxil, clamp.dxil, "the clamp changed nothing");
    assert_ne!(clamp.dxil, keep.dxil, "a NaN texel is still written");
    assert_eq!(lowered(None).dxil, off.dxil);
    assert_eq!(lowered(Some(Hold::Clamp)).dxil, clamp.dxil);
    assert_eq!(here(), None, "the hold ends with the translation");
    validate(&clamp);
    validate(&keep);
}
