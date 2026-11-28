use super::super::container::{self, SigElement};
use super::super::{HostShader, emit};
use super::{Linked, mesh_fits, with_linked};
use crate::dxbc::{Container, FourCC};
use crate::native::{HwStage, MeshIo, NativeProgram, StageIo, Usage, VsOutput};
use crate::{io, ir, resource, structure};

const S_ENDPGM: u32 = 0xBF81_0000;
/// float4 TEXCOORD0..16 beside SV_Position: 72 components of 128 vertices, 36,864 bytes (the
/// title's 71 are 65 to 71 components).
const PARAMS: u8 = 17;

fn element(name: &str, index: u32, kind: u8, row: u32) -> SigElement {
    SigElement {
        name: name.into(),
        index,
        kind,
        comp: 9,
        interp: 0,
        row: Some(row),
        col: 0,
        cols: 4,
        usage: 0,
        half: false,
    }
}

/// A mesh program of 128 lanes, vertices and triangles that exports a primitive, a position
/// and `PARAMS` float4 parameters, lowered with `linked` when given.
fn mesh(linked: Option<Linked>) -> HostShader {
    let mut words = vec![
        0x7E02_02F2, // v_mov_b32 v1, 1.0
        0xF800_0941,
        0x0000_0001, // exp prim, v1, off, off, off done
        0xF800_08CF,
        0x0101_0101, // exp pos0, v1, v1, v1, v1 done
    ];
    for n in 0..u32::from(PARAMS) {
        words.extend([0xF800_020F + (n << 4), 0x0101_0101]); // exp param<n>, v1, v1, v1, v1
    }
    words.push(S_ENDPGM);
    let native = NativeProgram {
        stage: HwStage::Ms,
        wave64: false,
        vgprs: 8,
        layout: vec![],
        used: vec![],
        io: StageIo::Ms(MeshIo {
            outputs: (0..PARAMS)
                .map(|n| VsOutput {
                    usage: Usage::Generic,
                    row: n + 1,
                    param: n,
                    fp16: false,
                    pack2: false,
                })
                .collect(),
            clip_cntl: 0,
            threads: 128,
            max_vertices: 128,
            max_primitives: 128,
            topology: 2,
            lds_bytes: 0,
        }),
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    };
    let mut osg1 = vec![element("SV_Position", 0, 3, 0)];
    osg1.extend((0..u32::from(PARAMS)).map(|n| element("TEXCOORD", n, 0, n + 1)));
    let isg1 = container::signature(&[], true);
    let osg1 = container::signature(&osg1, false);
    let bytes = crate::dxbc::build(&[(FourCC::ISG1, &isg1), (FourCC::OSG1, &osg1)]);
    let c = Container::parse(&bytes).unwrap();
    let p = ir::build(&native.patched_code(), native.wave64, native.vgprs).unwrap();
    let res = resource::track(&p, &native.layout, native.stage, None).unwrap();
    let io = io::raise(&p, &native).unwrap();
    let st = structure::structure(&p).unwrap();
    let lower = || emit(&c, &native, &p, &st, &io, &res).unwrap();
    match linked {
        Some(l) => with_linked(l, lower),
        None => lower(),
    }
}

/// `VICTORY_MESH_LINK`'s pair: a mesh program past D3D12's 32 KiB of outputs does not pass
/// the validator alone; linked to a pixel program that reads TEXCOORD0 only, it declares
/// SV_Position and TEXCOORD0, fits and validates.
#[test]
fn a_mesh_program_past_32_kib_fits_and_validates_linked_to_what_its_pixel_program_reads() {
    let whole = mesh(None);
    assert!(!mesh_fits(&whole.dxil).unwrap());
    let unread = (1..u32::from(PARAMS))
        .map(|n| ("TEXCOORD".to_owned(), n))
        .collect();
    let linked = mesh(Some(Linked {
        inputs: Vec::new(),
        outputs: unread,
    }));
    assert!(mesh_fits(&linked.dxil).unwrap());
    let c = Container::parse(&linked.dxil).unwrap();
    let out = super::elements(&linked.dxil, FourCC::OSG1).unwrap();
    let names: Vec<(&str, u32)> = out.iter().map(|e| (e.name.as_str(), e.index)).collect();
    assert_eq!(names, [("SV_Position", 0), ("TEXCOORD", 0)]);
    assert!(c.part(FourCC::PSV0).is_some());
    assert_eq!(
        HostShader {
            dxil: Vec::new(),
            ..whole.clone()
        },
        HostShader {
            dxil: Vec::new(),
            ..linked.clone()
        }
    );
    #[cfg(windows)]
    if let Ok(mut v) = super::super::validator::Validator::load(
        super::super::validator::Validator::default_path().as_deref(),
    ) {
        let refused = v.validate(&mut whole.dxil.clone()).unwrap_err();
        assert!(
            refused.contains("output size is greater than 32768"),
            "{refused}"
        );
        v.validate(&mut linked.dxil.clone()).unwrap();
    }
}
