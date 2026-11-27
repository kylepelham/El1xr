//! Mesh draws launched by the card (`VICTORY_MESH_LAUNCH=1`; off by
//! default): an amplification shader of ours (`launch.hlsl`, kept as bytes
//! in `launch_code.rs`) in front of every translated mesh shader.
//!
//! Source: fxceless/victory-fork `b1d777a` (`launch.rs`, after fork PR
//! #1381), ported the other way round: here a mesh pipeline's own host
//! pipeline stays the one without the stage (its translated mesh shader
//! declares the payload, which the driver takes without an amplification
//! shader), and the one with the stage is a second pipeline of the same
//! root signature ([`crate::pipeline::host::Launch`]) that only the draws
//! it launches bind. Every other path (direct mesh dispatches, records with
//! a count buffer or several records, `VICTORY_INDIRECT_RAW`, the clamps)
//! is the one it was.
//!
//! The title draws its meshes one indirect record a call, each from the
//! next slot of an argument buffer its own compute passes wrote (1,600
//! records a frame in the first rooms upstream, 3,000 to 5,000 in heavy
//! frames). Clamped, each was a compute dispatch of ours between two waits
//! for every earlier shader write (`crate::indirect::bound`); upstream
//! measured the clamp at 102 of the card's 130 ms a frame on an RTX 5080,
//! and the launch at about 5 ms of card time a frame less than the clamp.
//! Here the record is read by the draw itself: the amplification stage
//! loads its three words where the title's passes wrote them, bounds them
//! as the clamp does (65,535 groups, one-dimensional), charges the list's
//! budget as the clamp does (a record that does not fit runs empty), and
//! launches the mesh groups. The title's own barrier orders its passes
//! before the draw, as with `VICTORY_INDIRECT_RAW`.
//!
//! The draws' amplification groups may run in another order than the
//! list's, so when the list's budget runs out, which of its records run
//! empty is the card's order and no longer the list's; the bound on the
//! list's total work is the same.

use std::sync::atomic::{AtomicU64, Ordering};

use windows::Win32::Graphics::Direct3D12::*;

use crate::host::HostList;

#[path = "launch_code.rs"]
mod launch_code;

/// The amplification shader's code (see `launch.hlsl`).
const CODE: &[u8] = launch_code::CODE;

/// The root constant that has the card read the record.
const DIRECT_NONE: u32 = u32::MAX;

/// Where the amplification shader binds: `b0`, `t0`, `u0` in this space.
const SPACE: u32 = victory_shader::emit::LAUNCH_SPACE;

/// Whether mesh pipelines get the amplification stage and single indirect
/// mesh records are launched by it (`VICTORY_MESH_LAUNCH=1`). The
/// translated mesh shaders declare the payload the stage needs under the
/// same switch (`victory_shader::emit::mesh_payload_bytes`), so that the
/// switch is part of the shader cache's key.
pub(crate) fn on() -> bool {
    victory_shader::emit::mesh_payload_bytes() != 0
}

/// The amplification stage's code.
pub(crate) fn code() -> D3D12_SHADER_BYTECODE {
    D3D12_SHADER_BYTECODE {
        pShaderBytecode: CODE.as_ptr().cast(),
        BytecodeLength: CODE.len(),
    }
}

/// The bytes of [`code`], for the pipeline library's names.
pub(crate) fn bytes() -> &'static [u8] {
    CODE
}

/// The three root parameters the stage reads, in order: the direct group
/// count (one constant), the record (a root SRV), the list's budget (a
/// root UAV). Five root words.
pub(crate) fn params() -> [D3D12_ROOT_PARAMETER1; 3] {
    let descriptor = |kind| D3D12_ROOT_PARAMETER1 {
        ParameterType: kind,
        Anonymous: D3D12_ROOT_PARAMETER1_0 {
            Descriptor: D3D12_ROOT_DESCRIPTOR1 {
                ShaderRegister: 0,
                RegisterSpace: SPACE,
                Flags: D3D12_ROOT_DESCRIPTOR_FLAG_DATA_VOLATILE,
            },
        },
        ShaderVisibility: D3D12_SHADER_VISIBILITY_AMPLIFICATION,
    };
    [
        D3D12_ROOT_PARAMETER1 {
            ParameterType: D3D12_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS,
            Anonymous: D3D12_ROOT_PARAMETER1_0 {
                Constants: D3D12_ROOT_CONSTANTS {
                    ShaderRegister: 0,
                    RegisterSpace: SPACE,
                    Num32BitValues: 1,
                },
            },
            ShaderVisibility: D3D12_SHADER_VISIBILITY_AMPLIFICATION,
        },
        descriptor(D3D12_ROOT_PARAMETER_TYPE_SRV),
        descriptor(D3D12_ROOT_PARAMETER_TYPE_UAV),
    ]
}

/// What one launch reads.
#[derive(Clone, Copy)]
pub(crate) enum Launch {
    /// The record at this address on the card.
    Record(u64),
    /// A group count known on the CPU (tests only: the title's direct mesh
    /// dispatches stay on the pipeline without the stage).
    #[cfg_attr(not(test), allow(dead_code))]
    Direct(u32),
}

/// Records the launch of one mesh draw, the pipeline with the stage bound
/// and the stage's parameters at `roots`. False if the list has no budget
/// buffer (nothing was recorded).
pub(crate) fn record(list: &HostList, roots: [u32; 3], what: Launch) -> bool {
    let Ok(l6) = windows::core::Interface::cast::<ID3D12GraphicsCommandList6>(&list.list) else {
        return false;
    };
    let budget = list.budget();
    let (direct, record, budget) = match what {
        Launch::Record(address) => {
            let Some(budget) = budget else {
                return false;
            };
            (DIRECT_NONE, address, budget)
        }
        // The stage reads neither the record nor the budget.
        Launch::Direct(x) => (x, budget.unwrap_or(0), budget.unwrap_or(0)),
    };
    // SAFETY: an open list with the launch pipeline and its root signature
    // set; the record and the budget are live buffers the list keeps.
    unsafe {
        list.list.SetGraphicsRoot32BitConstant(roots[0], direct, 0);
        list.list
            .SetGraphicsRootShaderResourceView(roots[1], record);
        list.list
            .SetGraphicsRootUnorderedAccessView(roots[2], budget);
        l6.DispatchMesh(1, 1, 1);
    }
    true
}

/// Single indirect mesh records since the last line: launched by the card,
/// and not launched, by why: the pipeline has no launch stage (its root
/// signature or pipeline was refused with it), the record is not 4-byte
/// aligned, the list has no budget buffer, the draw's state was not ready.
pub(crate) static RECORDS: [AtomicU64; 5] = [const { AtomicU64::new(0) }; 5];
/// Index in [`RECORDS`].
pub(crate) const LAUNCHED: usize = 0;
pub(crate) const NO_STAGE: usize = 1;
pub(crate) const UNALIGNED: usize = 2;
pub(crate) const NO_BUDGET: usize = 3;
pub(crate) const UNPREPARED: usize = 4;
/// Indirect mesh calls the stage does not take (a count buffer or more than
/// one record), since the last line: they run as before.
pub(crate) static OTHER_CALLS: AtomicU64 = AtomicU64::new(0);

/// Notes one outcome and says [`line`] once a second.
pub(crate) fn note(outcome: usize) {
    RECORDS[outcome].fetch_add(1, Ordering::Relaxed);
    each_second();
}

/// Says [`line`] once a second, from the mesh calls themselves, per frame
/// begun meanwhile.
pub(crate) fn each_second() {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    static SECOND: AtomicU64 = AtomicU64::new(0);
    static FRAME: AtomicU64 = AtomicU64::new(0);
    let now = START
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_secs();
    let last = SECOND.load(Ordering::Relaxed);
    if now > last
        && SECOND
            .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
    {
        let frame = crate::frame::current();
        let frames = frame
            .saturating_sub(FRAME.swap(frame, Ordering::Relaxed))
            .max(1);
        line(frames as f64);
    }
}

/// The counts since the last line, per frame.
fn line(frames: f64) {
    let [launched, no_stage, unaligned, no_budget, unprepared] =
        [0, 1, 2, 3, 4].map(|i| RECORDS[i].swap(0, Ordering::Relaxed));
    let other = OTHER_CALLS.swap(0, Ordering::Relaxed);
    let not = no_stage + unaligned + no_budget + unprepared;
    if launched + not + other > 0 {
        let per = |n: u64| n as f64 / frames;
        tracing::info!(
            "mesh records per frame (VICTORY_MESH_LAUNCH=1): {:.0} launched by the card, {:.0} not ({:.0} pipeline without the stage, \
             {:.0} unaligned, {:.0} no budget buffer, {:.0} not prepared); {:.0} mesh calls with a count or several records run as before",
            per(launched),
            per(not),
            per(no_stage),
            per(unaligned),
            per(no_budget),
            per(unprepared),
            per(other),
        );
    }
}

thread_local! {
    /// Set while a launched draw binds its pipeline: `State::bind` then
    /// sets the pipeline with the stage ([`pso`]).
    static LAUNCHING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// While alive, binds on this thread choose the pipeline with the stage.
pub(crate) struct Launching(());

impl Launching {
    pub(crate) fn start() -> Self {
        LAUNCHING.with(|l| l.set(true));
        Launching(())
    }
}

impl Drop for Launching {
    fn drop(&mut self) {
        LAUNCHING.with(|l| l.set(false));
    }
}

/// The pipeline a bind of `p` sets: the one with the launch stage inside
/// [`Launching`], else `p`'s own.
pub(crate) fn pso(p: &crate::pipeline::host::HostPipeline) -> &ID3D12PipelineState {
    match &p.launch {
        Some(launch) if LAUNCHING.with(std::cell::Cell::get) => &launch.pso,
        _ => &p.pso,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_stage_binds_five_root_words_in_its_own_space() {
        let p = params();
        // SAFETY: the union members these parameter types name.
        let words: u32 = p
            .iter()
            .map(|p| match p.ParameterType {
                D3D12_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS => unsafe {
                    p.Anonymous.Constants.Num32BitValues
                },
                _ => 2,
            })
            .sum();
        assert_eq!(words, 5);
        for p in &p {
            assert_eq!(p.ShaderVisibility, D3D12_SHADER_VISIBILITY_AMPLIFICATION);
        }
        assert!(CODE.starts_with(b"DXBC"), "a signed DXIL container");
    }

    #[test]
    fn binds_choose_the_launch_pipeline_only_while_launching() {
        assert!(!LAUNCHING.with(std::cell::Cell::get));
        {
            let _l = Launching::start();
            assert!(LAUNCHING.with(std::cell::Cell::get));
        }
        assert!(!LAUNCHING.with(std::cell::Cell::get));
    }
}
