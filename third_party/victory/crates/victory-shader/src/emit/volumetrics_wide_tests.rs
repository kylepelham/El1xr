//! `VICTORY_VOLUMETRICS_WIDE=1` on a host of 32-lane waves with the paired form off (the RTX
//! 5090's `VICTORY_WAVE_PAIRS=0`): the untiled `CS_TransmissionVolumetrics` takes the
//! wave-wide form.

use super::super::{Container, HostShader, emit_forms, lower, validator};
use super::{TEST_VOLUMETRICS_WIDE, UNTILED};
use crate::native::{HwStage, NativeProgram};
use crate::{ShaderError, io, ir, resource, structure};

/// `native` (its program `p`) in the container `bytes` through [`emit_forms`] on a 32-lane
/// host without the paired form, with the switch `on`.
fn forms(
    native: &NativeProgram,
    p: &ir::Program,
    bytes: &[u8],
    on: bool,
) -> Result<(HostShader, Option<u32>), ShaderError> {
    let c = Container::parse(bytes).expect("a container");
    let st = structure::structure(p).expect("structure");
    let res = resource::track(p, &native.layout, native.stage, Some(&st)).expect("resource::track");
    let io = io::raise(p, native).expect("io::raise");
    let wave = lower::TEST_HOST_WAVE.replace(Some(32));
    let pairs = super::super::TEST_PAIRS.replace(Some(false));
    let switch = TEST_VOLUMETRICS_WIDE.replace(Some(on));
    let chosen = emit_forms(&c, native, p, &st, &io, &res);
    TEST_VOLUMETRICS_WIDE.set(switch);
    super::super::TEST_PAIRS.set(pairs);
    lower::TEST_HOST_WAVE.set(wave);
    chosen
}

fn validates(shader: &HostShader) {
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        let mut copy = shader.dxil.clone();
        v.validate(&mut copy).expect("DXIL validation failed");
    }
}

/// The title's program (not in the repository: set `MASK_TRACE_DIR` to a folder of
/// containers `<name>.<hash>.cdxil`, as `mask_elsewhere_tests` reads): with the switch it
/// comes out with 32-lane waves and validates.
#[test]
fn the_title_program_comes_out_with_32_lane_waves() {
    let Some(dir) = std::env::var_os("MASK_TRACE_DIR") else {
        eprintln!("MASK_TRACE_DIR not set: skipped");
        return;
    };
    let path = std::fs::read_dir(dir)
        .expect("the folder")
        .flatten()
        .map(|e| e.path())
        .find(|p| p.to_string_lossy().contains(".6871d6e4b3ac9a10."))
        .expect("no container for 6871d6e4b3ac9a10");
    let bytes = std::fs::read(path).expect("the container");
    assert_eq!(bytes[4..20], UNTILED);
    let c = Container::parse(&bytes).expect("a container");
    let part = super::super::program_part(&c).expect("a program part");
    let native = crate::native_programs(part.data)
        .expect("native programs")
        .into_iter()
        .find(|p| p.stage == HwStage::Cs)
        .expect("a compute program");
    let p = crate::spill::build(&native).expect("spill::build");
    let on = forms(&native, &p, &bytes, true);
    let on = on.expect("on");
    assert_eq!(on.1, Some(32), "32-lane waves");
    validates(&on.0);
    let off = forms(&native, &p, &bytes, false);
    assert!(
        !matches!(&off, Ok((s, Some(32))) if s.dxil == on.0.dxil),
        "off: refused, 64-lane, or another form"
    );
}
