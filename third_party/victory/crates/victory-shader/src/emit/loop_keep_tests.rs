//! `VICTORY_LOOP_KEEP` and `VICTORY_LOOP_ROOM` ([`super`]).

use super::{FORCED, cap, parse_room};
use crate::emit::container::{self, SigElement};
use crate::emit::{Container, FourCC, emit};
use crate::native::{HwStage, NativeProgram, StageIo};
use crate::{io, ir, resource, structure};

const LIMIT: u32 = super::LOOP_LIMIT;

#[test]
fn room_raises_every_limit_but_the_ray_walks() {
    assert_eq!(parse_room(None), 0);
    assert_eq!(parse_room(Some(" 1024 ")), 1024);
    assert_eq!(parse_room(Some("lots")), 0);
    // pixel, vertex or mesh, ray walk, compute
    let all = |room| {
        [
            cap(LIMIT, true, false, false, room),
            cap(LIMIT, false, true, false, room),
            cap(LIMIT, false, false, true, room),
            cap(LIMIT, false, false, false, room),
        ]
    };
    assert_eq!(all(0), [128, 256, 1024, 4096]);
    assert_eq!(all(1024), [1024, 1024, 1024, 4096]);
    assert_eq!(all(8192), [8192, 8192, 1024, 8192]);
    // The diagnostics override names one limit for all.
    assert_eq!(cap(64, true, false, false, 8192), 64);
}

/// A pixel program counting to 300 in v0 round a loop closed by `s_branch`, its exit a
/// `s_cbranch_vccnz` in the body (as the skin light loop of `677ea27c05f1a634` leaves), then
/// writing the count to target 0.
fn count_to_300() -> NativeProgram {
    let words = [
        0x7E00_0280u32, // v_mov_b32 v0, 0
        0x7D84_00FF,
        300,         // v_cmp_eq_u32 vcc, 300, v0 (the loop)
        0xBF87_0002, // s_cbranch_vccnz to the export
        0x4A00_0081, // v_add_nc_u32 v0, 1, v0
        0xBF82_FFFB, // s_branch to the loop
        0x7E02_0D00, // v_cvt_f32_u32 v1, v0
        0xF800_180F,
        0x0101_0101, // exp mrt0 v1, v1, v1, v1 done vm
        0xBF81_0000, // s_endpgm
    ];
    NativeProgram {
        stage: HwStage::Ps,
        wave64: false,
        vgprs: 2,
        layout: vec![],
        used: vec![],
        io: StageIo::Ps {
            inputs: vec![],
            input_addr: 0,
            db_shader_control: 0,
            patches: vec![],
        },
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    }
}

fn target() -> SigElement {
    SigElement {
        name: "SV_Target".into(),
        index: 0,
        kind: 16,
        comp: 9,
        interp: 0,
        row: Some(0),
        col: 0,
        cols: 4,
        usage: 15,
        half: false,
    }
}

/// [`count_to_300`] translated with `(keep, room)`, checked by dxil.dll where there is one.
fn translated(forced: (bool, u32)) -> crate::emit::HostShader {
    let native = count_to_300();
    let osg1 = container::signature(&[target()], false);
    let bytes = crate::dxbc::build(&[(FourCC::OSG1, &osg1)]);
    let c = Container::parse(&bytes).unwrap();
    let p = ir::build(&native.patched_code(), native.wave64, native.vgprs).unwrap();
    let st = structure::structure(&p).unwrap();
    assert!(
        st.exits
            .iter()
            .any(|e| matches!(e, structure::LaneExit::Jump(_))),
        "the loop is closed by a plain jump"
    );
    let res = resource::track(&p, &native.layout, native.stage, Some(&st)).unwrap();
    let io = io::raise(&p, &native).unwrap();
    let was = FORCED.replace(Some(forced));
    let out = emit(&c, &native, &p, &st, &io, &res);
    FORCED.set(was);
    let out = out.unwrap();
    #[cfg(windows)]
    if let Ok(mut v) = crate::emit::validator::Validator::load(
        crate::emit::validator::Validator::default_path().as_deref(),
    ) {
        v.validate(&mut out.dxil.clone())
            .unwrap_or_else(|e| panic!("{forced:?}: {e}"));
    }
    out
}

#[test]
fn a_loop_closed_by_a_jump_translates_and_validates_either_way() {
    let off = translated((false, 0));
    let keep = translated((true, 0));
    let room = translated((false, 1024));
    assert_ne!(off.dxil, keep.dxil, "keep adds the exit at the limit");
    assert_ne!(off.dxil, room.dxil, "room raises the limit");
    assert_eq!(translated((false, 0)).dxil, off.dxil);
}

/// A vertex shader passing POSITION through to SV_Position.
#[cfg(windows)]
fn position_vs() -> crate::emit::HostShader {
    use crate::emit::bitcode::{Attr, Term};
    let el = |name: &str, kind, interp| SigElement {
        name: name.into(),
        index: 0,
        kind,
        comp: 9,
        interp,
        row: Some(0),
        col: 0,
        cols: 4,
        usage: 15,
        half: false,
    };
    let mut s = crate::emit::shader::Shader::new(false);
    s.inputs = vec![el("POSITION", 0, 0)];
    s.outputs = vec![el("SV_Position", 3, 4)];
    let b = s.entry;
    for col in 0..4 {
        let args = [s.m.i32(0), s.m.i32(0), s.m.i8(col), s.m.undef(s.t.i32)];
        let v = s.op(b, "loadInput.f32", Attr::ReadNone, s.t.f32, 4, &args);
        let args = [s.m.i32(0), s.m.i32(0), s.m.i8(col), v];
        s.op(b, "storeOutput.f32", Attr::NoUnwind, s.t.void, 5, &args);
    }
    s.m.terminate(b, Term::Ret);
    crate::emit::HostShader {
        stage: HwStage::Vs,
        dxil: s.finish(),
        user_data: Vec::new(),
        samplers: Vec::new(),
        static_samplers: Vec::new(),
        root_cbvs: Vec::new(),
        root_srvs: Vec::new(),
        root_uavs: Vec::new(),
        root_indexed_views: Vec::new(),
        half_views: Vec::new(),
    }
}

/// On the card: the 300-iteration loop is discarded at the pixel limit of 128 (the target keeps
/// its clear), keeps its count at the limit with `VICTORY_LOOP_KEEP=1`, and runs to 300 with
/// `VICTORY_LOOP_ROOM=1024`.
#[test]
#[cfg(windows)]
fn a_300_iteration_pixel_loop_on_the_card() {
    use crate::emit::host::{Draw, Host};
    let Ok(host) = Host::new(std::env::var_os("VICTORY_WARP").is_some()) else {
        eprintln!("no SM 6.6 device; skipped");
        return;
    };
    let vs = position_vs();
    let vertices = [
        -1.0f32, -1.0, 0.5, 1.0, //
        -1.0, 3.0, 0.5, 1.0, //
        3.0, -1.0, 0.5, 1.0,
    ];
    let drawn = |forced| {
        let ps = translated(forced);
        let (root, pso) = host.pipeline(&vs, &ps).unwrap();
        let d = Draw {
            roots: Vec::new(),
            textures: &[],
            samplers: &[],
            vertices: &vertices,
            stride: 16,
            size: (8, 8),
            mesh_groups: 0,
        };
        host.draw(&root, &pso, &d).unwrap()
    };
    let off = drawn((false, 0));
    let keep = drawn((true, 0));
    let room = drawn((false, 1024));
    let clear = off[0];
    assert_ne!(clear[0], 300.0, "off: discarded at 128");
    assert!(
        off.iter().all(|t| *t == clear),
        "off: every pixel discarded"
    );
    assert!(keep.iter().all(|t| *t == [127.0; 4]), "keep: {:?}", keep[0]);
    assert!(room.iter().all(|t| *t == [300.0; 4]), "room: {:?}", room[0]);
}
