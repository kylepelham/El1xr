//! `VICTORY_LOOP_CAP_WATCH` ([`super`]).

use super::{ENDED, FORCED, KEPT};
use crate::emit::container::{self, SigElement};
use crate::emit::shader::{SLOT_LOOP_CAP, USER_LOOP_CAP_ID};
use crate::emit::{Container, FourCC, HostShader, emit};
use crate::native::{HwStage, NativeProgram, RootType, StageIo};
use crate::{io, ir, resource, structure};

/// A pixel program counting to `n` in v0, then writing the count to target 0. `jump`: the loop
/// is closed by `s_branch`, its exit a `s_cbranch_vccnz` in the body (as the skin light loop of
/// `677ea27c05f1a634`); otherwise it is closed by a `s_cbranch_vccnz` back.
fn count_to(n: u32, jump: bool) -> NativeProgram {
    let words: Vec<u32> = if jump {
        vec![
            0x7E00_0280, // v_mov_b32 v0, 0
            0x7D84_00FF,
            n,           // v_cmp_eq_u32 vcc, n, v0 (the loop)
            0xBF87_0002, // s_cbranch_vccnz to the export
            0x4A00_0081, // v_add_nc_u32 v0, 1, v0
            0xBF82_FFFB, // s_branch to the loop
            0x7E02_0D00, // v_cvt_f32_u32 v1, v0
            0xF800_180F,
            0x0101_0101, // exp mrt0 v1, v1, v1, v1 done vm
            0xBF81_0000, // s_endpgm
        ]
    } else {
        vec![
            0x7E00_0280, // v_mov_b32 v0, 0
            0x4A00_0081, // v_add_nc_u32 v0, 1, v0 (the loop)
            0x7D8A_00FF,
            n,           // v_cmp_ne_u32 vcc, n, v0
            0xBF87_FFFC, // s_cbranch_vccnz to the loop
            0x7E02_0D00, // v_cvt_f32_u32 v1, v0
            0xF800_180F,
            0x0101_0101, // exp mrt0 v1, v1, v1, v1 done vm
            0xBF81_0000, // s_endpgm
        ]
    };
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

/// [`count_to`] translated with the watch `on` and `VICTORY_LOOP_KEEP` `keep`, checked by
/// dxil.dll where there is one.
fn translated(n: u32, jump: bool, on: bool, keep: bool) -> HostShader {
    let native = count_to(n, jump);
    let osg1 = container::signature(&[target()], false);
    let bytes = crate::dxbc::build(&[(FourCC::OSG1, &osg1)]);
    let c = Container::parse(&bytes).unwrap();
    let p = ir::build(&native.patched_code(), native.wave64, native.vgprs).unwrap();
    let st = structure::structure(&p).unwrap();
    assert_eq!(
        st.exits
            .iter()
            .enumerate()
            .any(|(b, e)| matches!(e, structure::LaneExit::Jump(t) if *t <= b)),
        jump,
        "how the loop is closed"
    );
    let res = resource::track(&p, &native.layout, native.stage, Some(&st)).unwrap();
    let io = io::raise(&p, &native).unwrap();
    let was = FORCED.replace(Some(on));
    let was_keep = super::super::loop_keep::FORCED.replace(Some((keep, 0)));
    let out = emit(&c, &native, &p, &st, &io, &res);
    FORCED.set(was);
    super::super::loop_keep::FORCED.set(was_keep);
    let out = out.unwrap();
    #[cfg(windows)]
    if let Ok(mut v) = crate::emit::validator::Validator::load(
        crate::emit::validator::Validator::default_path().as_deref(),
    ) {
        v.validate(&mut out.dxil.clone())
            .unwrap_or_else(|e| panic!("n={n} jump={jump} on={on} keep={keep}: {e}"));
    }
    out
}

/// The user-data dword the host puts the program's number in.
fn id_dword(s: &HostShader) -> Option<u32> {
    s.user_data
        .iter()
        .find(|u| u.root == RootType::Other(USER_LOOP_CAP_ID))
        .map(|u| u.index)
}

#[test]
fn the_watch_adds_its_buffer_and_number_only_when_on() {
    for jump in [true, false] {
        for keep in [false, true] {
            let off = translated(300, jump, false, keep);
            assert!(
                !off.root_uavs.contains(&SLOT_LOOP_CAP),
                "jump={jump} keep={keep}"
            );
            assert_eq!(id_dword(&off), None);
            let on = translated(300, jump, true, keep);
            assert!(
                on.root_uavs.contains(&SLOT_LOOP_CAP),
                "jump={jump} keep={keep}"
            );
            assert!(id_dword(&on).is_some());
            assert_ne!(off.dxil, on.dxil);
            assert_eq!(translated(300, jump, false, keep).dxil, off.dxil);
        }
    }
}

/// A vertex shader passing POSITION through to SV_Position.
#[cfg(windows)]
fn position_vs() -> HostShader {
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
    HostShader {
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

/// On the card, 8x8 pixels as program number 5: (target texel, ended, kept, every other dword
/// of the buffer zero).
#[cfg(windows)]
fn on_the_card(
    host: &crate::emit::host::Host,
    n: u32,
    jump: bool,
    keep: bool,
) -> ([f32; 4], u32, u32, bool) {
    use crate::emit::host::{Draw, Root};
    const ID: u32 = 5;
    let vs = position_vs();
    let ps = translated(n, jump, true, keep);
    let (root, pso) = host.pipeline(&vs, &ps).unwrap();
    let mut user = vec![0u32; ps.user_data_dwords() as usize];
    user[id_dword(&ps).unwrap() as usize] = ID;
    let vertices = [
        -1.0f32, -1.0, 0.5, 1.0, //
        -1.0, 3.0, 0.5, 1.0, //
        3.0, -1.0, 0.5, 1.0,
    ];
    let counts = std::cell::RefCell::new(vec![0u32; 64]);
    let d = Draw {
        roots: vec![(1, Root::Constants(user)), (2, Root::Uav(counts))],
        textures: &[],
        samplers: &[],
        vertices: &vertices,
        stride: 16,
        size: (8, 8),
        mesh_groups: 0,
    };
    let texels = host.draw(&root, &pso, &d).unwrap();
    assert!(texels.iter().all(|t| *t == texels[0]), "{:?}", texels[0]);
    let Root::Uav(counts) = &d.roots[1].1 else {
        unreachable!()
    };
    let counts = counts.borrow();
    let at = (ID * 2) as usize;
    let rest = counts
        .iter()
        .enumerate()
        .all(|(i, &w)| i == at + ENDED as usize || i == at + KEPT as usize || w == 0);
    (
        texels[0],
        counts[at + ENDED as usize],
        counts[at + KEPT as usize],
        rest,
    )
}

/// Each lane at the pixel limit of 128 is counted once, in its program's pair: as ended where
/// the loop discards it, as kept where it leaves the loop with its count. A loop that stays
/// under the limit counts nothing.
#[test]
#[cfg(windows)]
fn lanes_at_the_limit_are_counted_on_the_card() {
    let Ok(host) = crate::emit::host::Host::new(std::env::var_os("VICTORY_WARP").is_some()) else {
        eprintln!("no SM 6.6 device; skipped");
        return;
    };
    // Closed by a jump: discarded (target keeps its clear), 64 ended.
    let (texel, ended, kept, rest) = on_the_card(&host, 300, true, false);
    assert_ne!(texel, [300.0; 4]);
    assert_eq!((ended, kept, rest), (64, 0, true), "{texel:?}");
    // With VICTORY_LOOP_KEEP: leaves at its exit with 127, 64 kept.
    let (texel, ended, kept, rest) = on_the_card(&host, 300, true, true);
    assert_eq!(texel, [127.0; 4]);
    assert_eq!((ended, kept, rest), (0, 64, true));
    // Closed by a branch: leaves at 128, 64 kept.
    let (texel, ended, kept, rest) = on_the_card(&host, 300, false, false);
    assert_eq!(texel, [128.0; 4]);
    assert_eq!((ended, kept, rest), (0, 64, true));
    // Under the limit: the program's own count, nothing counted.
    for (jump, keep) in [(true, false), (true, true), (false, false)] {
        let (texel, ended, kept, rest) = on_the_card(&host, 100, jump, keep);
        assert_eq!(texel, [100.0; 4], "jump={jump} keep={keep}");
        assert_eq!((ended, kept, rest), (0, 0, true), "jump={jump} keep={keep}");
    }
}
