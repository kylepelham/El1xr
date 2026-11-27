//! A mesh pipeline's second host pipeline, with the amplification stage of
//! `crate::launch` in front of its mesh shader (`VICTORY_MESH_LAUNCH=1`).
//!
//! Source: fxceless/victory-fork `b1d777a` (`pipeline/host.rs`: `Launch`,
//! the stream's amplification subobject, and the card test of the stage).
//! There the pipeline with the stage was the title pipeline's host
//! pipeline and the one without was made for the calls the stage does not
//! take; here it is the other way round (see `crate::launch`), and the
//! stream of the pipeline without the stage is the one it was, so that
//! with the switch off nothing changes and with it on no stored pipeline
//! is looked up with another stream.

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;

use super::{GraphicsState, Sub, host_blend, stream_pipeline};

/// What a mesh pipeline with the launch stage has besides its own host
/// pipeline: the stage's root parameters and the pipeline with the stage.
pub(crate) struct Launch {
    /// The direct group count, the record, the budget.
    pub(crate) roots: [u32; 3],
    pub(crate) pso: ID3D12PipelineState,
}

/// Appends the stage's root parameters to a mesh pipeline's `params`;
/// their indices.
pub(super) fn add_params(params: &mut Vec<D3D12_ROOT_PARAMETER1>) -> [u32; 3] {
    let first = params.len() as u32;
    params.extend(crate::launch::params());
    [first, first + 1, first + 2]
}

/// The pipeline with the stage, `None` (said in the log) if the host
/// refuses it: the pipeline's records then run as before.
#[allow(clippy::too_many_arguments)]
pub(super) fn make(
    device: &ID3D12Device,
    root: &ID3D12RootSignature,
    root_blob: &[u8],
    ms: D3D12_SHADER_BYTECODE,
    ps: D3D12_SHADER_BYTECODE,
    g: &GraphicsState,
    depth_bounds: bool,
    roots: [u32; 3],
    shaders: &dyn std::fmt::Debug,
) -> Option<Launch> {
    let name = super::super::library::name(
        "mesh launched",
        root_blob,
        &[
            &super::code_bytes(&ms),
            &super::code_bytes(&ps),
            crate::launch::bytes(),
        ],
        &g.describe(),
    );
    match launched_pipeline(
        device,
        root,
        crate::launch::code(),
        ms,
        ps,
        g,
        depth_bounds,
        &name,
    ) {
        Ok(pso) => {
            crate::host::name(&pso, || format!("{shaders:?} with the launch stage"));
            Some(Launch { roots, pso })
        }
        Err(e) => {
            tracing::warn!(error = %e, ?shaders, "host mesh pipeline with the launch stage refused: its records run as before");
            if let Some(host) = crate::host::host() {
                host.log_messages();
            }
            None
        }
    }
}

/// `super::mesh_pipeline` with an amplification subobject.
#[allow(clippy::too_many_arguments)]
fn launched_pipeline(
    device: &ID3D12Device,
    root: &ID3D12RootSignature,
    amplification: D3D12_SHADER_BYTECODE,
    ms: D3D12_SHADER_BYTECODE,
    ps: D3D12_SHADER_BYTECODE,
    g: &GraphicsState,
    depth_bounds: bool,
    name: &str,
) -> windows::core::Result<ID3D12PipelineState> {
    use windows::core::Interface;
    #[repr(C)]
    struct Stream<D> {
        root: Sub<*mut std::ffi::c_void>,
        amplification: Sub<D3D12_SHADER_BYTECODE>,
        ms: Sub<D3D12_SHADER_BYTECODE>,
        ps: Sub<D3D12_SHADER_BYTECODE>,
        blend: Sub<D3D12_BLEND_DESC>,
        mask: Sub<u32>,
        raster: Sub<D3D12_RASTERIZER_DESC>,
        depth: Sub<D>,
        targets: Sub<D3D12_RT_FORMAT_ARRAY>,
        dsv: Sub<DXGI_FORMAT>,
        samples: Sub<DXGI_SAMPLE_DESC>,
        topology: Sub<D3D12_PRIMITIVE_TOPOLOGY_TYPE>,
    }
    fn build<D>(
        device: &ID3D12Device,
        root: &ID3D12RootSignature,
        (amplification, ms, ps): (
            D3D12_SHADER_BYTECODE,
            D3D12_SHADER_BYTECODE,
            D3D12_SHADER_BYTECODE,
        ),
        g: &GraphicsState,
        depth: Sub<D>,
        name: &str,
    ) -> windows::core::Result<ID3D12PipelineState> {
        let mut stream = Stream {
            root: Sub(
                D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_ROOT_SIGNATURE,
                root.as_raw(),
            ),
            amplification: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_AS, amplification),
            ms: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_MS, ms),
            ps: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_PS, ps),
            blend: Sub(
                D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_BLEND,
                host_blend(g.blend, &g.targets.RTFormats),
            ),
            mask: Sub(
                D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_SAMPLE_MASK,
                g.sample_mask,
            ),
            raster: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_RASTERIZER, g.rasterizer),
            depth,
            targets: Sub(
                D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_RENDER_TARGET_FORMATS,
                g.targets,
            ),
            dsv: Sub(
                D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DEPTH_STENCIL_FORMAT,
                g.depth_format,
            ),
            samples: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_SAMPLE_DESC, g.samples),
            topology: Sub(
                D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_PRIMITIVE_TOPOLOGY,
                D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
            ),
        };
        stream_pipeline(device, &mut stream, name)
    }
    let shaders = (amplification, ms, ps);
    if depth_bounds {
        build(
            device,
            root,
            shaders,
            g,
            Sub(
                D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DEPTH_STENCIL1,
                g.depth1(),
            ),
            name,
        )
    } else {
        build(
            device,
            root,
            shaders,
            g,
            Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DEPTH_STENCIL, g.depth),
            name,
        )
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::super::{mesh_pipeline, root_signature};
    use super::*;
    use crate::host::host;

    fn dxc() -> Option<PathBuf> {
        let dxc = match std::env::var_os("VICTORY_PC_DXC") {
            Some(p) => PathBuf::from(p),
            None => victory_shader::emit::validator::Validator::default_path()?
                .with_file_name("dxc.exe"),
        };
        dxc.exists().then_some(dxc)
    }

    fn compile(dxc: &Path, dir: &Path, src: &str, profile: &str) -> Vec<u8> {
        let (hlsl, out) = (
            dir.join(format!("{profile}.hlsl")),
            dir.join(format!("{profile}.dxil")),
        );
        std::fs::write(&hlsl, src).unwrap();
        let status = std::process::Command::new(dxc)
            .args(["-nologo", "-T", profile, "-E", "main", "-Fo"])
            .arg(&out)
            .arg(&hlsl)
            .status()
            .unwrap();
        assert!(status.success(), "dxc failed for {profile}");
        std::fs::read(out).unwrap()
    }

    fn code(bytes: &[u8]) -> D3D12_SHADER_BYTECODE {
        D3D12_SHADER_BYTECODE {
            pShaderBytecode: bytes.as_ptr().cast(),
            BytecodeLength: bytes.len(),
        }
    }

    /// The launch stage in front of a mesh shader that declares the
    /// payload: records read on the card where they were written, a count
    /// the CPU knows, the bound at 65,535 groups, an empty dimension, and
    /// the list's budget, which empties the records past it as the clamp
    /// does. The mesh shader counts its groups. The same mesh shader is
    /// taken without the stage too (the pipeline every other path binds).
    #[test]
    fn mesh_records_are_launched_by_the_amplification_stage() {
        let Some(host) = host() else {
            return;
        };
        if !host.mesh_shaders() {
            eprintln!("no host mesh shader support; skipped");
            return;
        }
        let Some(dxc) = dxc() else {
            eprintln!("no PC dxc.exe; skipped");
            return;
        };
        const COUNTING: &str = "struct V { float4 p : SV_Position; };
            struct P { uint unused; };
            RWByteAddressBuffer groups : register(u0, space0);
            [outputtopology(\"triangle\")] [numthreads(1, 1, 1)]
            void main(in payload P pl, out vertices V v[3], out indices uint3 t[1]) {
                SetMeshOutputCounts(3, 1);
                v[0].p = float4(0, 0, 0, 1);
                v[1].p = float4(0, 0, 0, 1);
                v[2].p = float4(0, 0, 0, 1);
                t[0] = uint3(0, 1, 2);
                uint was;
                groups.InterlockedAdd(0, 1, was);
            }";
        let dir = tempfile::tempdir().unwrap();
        let ms = compile(&dxc, dir.path(), COUNTING, "ms_6_6");
        let mut params = vec![D3D12_ROOT_PARAMETER1 {
            ParameterType: D3D12_ROOT_PARAMETER_TYPE_UAV,
            Anonymous: D3D12_ROOT_PARAMETER1_0 {
                Descriptor: D3D12_ROOT_DESCRIPTOR1 {
                    ShaderRegister: 0,
                    RegisterSpace: 0,
                    Flags: D3D12_ROOT_DESCRIPTOR_FLAG_DATA_VOLATILE,
                },
            },
            ShaderVisibility: D3D12_SHADER_VISIBILITY_MESH,
        }];
        let roots = add_params(&mut params);
        assert_eq!(roots, [1, 2, 3]);
        let (root, root_blob) = root_signature(&host.device, &params, &[], false).unwrap();
        let mut g = GraphicsState::default();
        g.depth.DepthEnable = false.into();
        let none = D3D12_SHADER_BYTECODE::default();
        let launch = make(
            &host.device,
            &root,
            &root_blob,
            code(&ms),
            none,
            &g,
            false,
            roots,
            &"launch test",
        )
        .expect("the mesh pipeline with the launch stage");
        let plain_name = super::super::super::library::name(
            "launch test plain",
            &root_blob,
            &[&ms],
            &g.describe(),
        );
        mesh_pipeline(&host.device, &root, code(&ms), none, &g, false, &plain_name)
            .expect("the mesh pipeline without the launch stage");

        // The groups one list runs: the direct count first if any, then the
        // records as the title writes them, 16 bytes apart.
        let run = |direct: Option<u32>, records: &[[u32; 3]]| -> u32 {
            let (buffer, data) = host.shared_buffer(4096).unwrap();
            for (i, r) in records.iter().enumerate() {
                // SAFETY: 4,096 mapped bytes; at most 23 records of 16 bytes.
                unsafe {
                    std::ptr::copy_nonoverlapping(r.as_ptr(), data.add(16 * i).cast::<u32>(), 3)
                };
            }
            // SAFETY: a live buffer.
            let base = unsafe { buffer.GetGPUVirtualAddress() };
            let groups = host.uav_buffer(256).unwrap();
            let readback = {
                let heap = D3D12_HEAP_PROPERTIES {
                    Type: D3D12_HEAP_TYPE_READBACK,
                    ..Default::default()
                };
                let rd = D3D12_RESOURCE_DESC {
                    Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                    Width: 256,
                    Height: 1,
                    DepthOrArraySize: 1,
                    MipLevels: 1,
                    SampleDesc: DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                    ..Default::default()
                };
                let mut out: Option<ID3D12Resource> = None;
                // SAFETY: a valid read-back buffer description.
                unsafe {
                    host.device
                        .CreateCommittedResource(
                            &heap,
                            D3D12_HEAP_FLAG_NONE,
                            &rd,
                            D3D12_RESOURCE_STATE_COPY_DEST,
                            None,
                            &mut out,
                        )
                        .unwrap();
                }
                out.unwrap()
            };
            let list = host.new_list().unwrap();
            let list2 =
                windows::core::Interface::cast::<ID3D12GraphicsCommandList2>(&list.list).unwrap();
            // SAFETY: an open list and live buffers.
            unsafe {
                let at = groups.GetGPUVirtualAddress();
                list.list.ResourceBarrier(&[crate::host::transition(
                    &groups,
                    D3D12_RESOURCE_STATE_COMMON,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                )]);
                list2.WriteBufferImmediate(
                    1,
                    &D3D12_WRITEBUFFERIMMEDIATE_PARAMETER { Dest: at, Value: 0 },
                    None,
                );
                list.list.ResourceBarrier(&[crate::host::transition(
                    &groups,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                    D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                )]);
                list.list.RSSetViewports(&[D3D12_VIEWPORT {
                    Width: 4.0,
                    Height: 4.0,
                    MaxDepth: 1.0,
                    ..Default::default()
                }]);
                list.list
                    .RSSetScissorRects(&[windows::Win32::Foundation::RECT {
                        left: 0,
                        top: 0,
                        right: 4,
                        bottom: 4,
                    }]);
                list.list.SetGraphicsRootSignature(&root);
                list.list.SetPipelineState(&launch.pso);
                list.list.SetGraphicsRootUnorderedAccessView(0, at);
            }
            use crate::launch::{Launch, record};
            if let Some(x) = direct {
                assert!(record(&list, launch.roots, Launch::Direct(x)));
            }
            for i in 0..records.len() {
                assert!(record(
                    &list,
                    launch.roots,
                    Launch::Record(base + 16 * i as u64)
                ));
            }
            // SAFETY: an open list and live buffers.
            unsafe {
                list.list.ResourceBarrier(&[crate::host::transition(
                    &groups,
                    D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                    D3D12_RESOURCE_STATE_COPY_SOURCE,
                )]);
                list.list.CopyBufferRegion(&readback, 0, &groups, 0, 4);
                list.list.ResourceBarrier(&[crate::host::transition(
                    &groups,
                    D3D12_RESOURCE_STATE_COPY_SOURCE,
                    D3D12_RESOURCE_STATE_COMMON,
                )]);
            }
            list.close();
            host.execute(&[&list]);
            host.log_messages();
            let mut ptr = std::ptr::null_mut();
            // SAFETY: `execute` waited for the card; four bytes were copied.
            unsafe {
                readback.Map(0, None, Some(&mut ptr)).unwrap();
                let n = *ptr.cast::<u32>();
                readback.Unmap(0, None);
                n
            }
        };
        assert_eq!(run(None, &[[5, 1, 1]]), 5, "one record");
        assert_eq!(run(Some(3), &[]), 3, "a count the CPU knows");
        assert_eq!(
            run(None, &[[70_000, 1, 1], [7, 0, 1]]),
            65_535,
            "bounded; an empty dimension"
        );
        // The list's budget (2^27, a mesh group charged as 128): of twenty
        // records of the most groups a dispatch runs, sixteen fit and the
        // others run empty.
        let big = vec![[65_535, 1, 1]; 20];
        assert_eq!(
            u64::from(run(None, &big)),
            16 * 65_535,
            "records past the budget"
        );
        assert_eq!((1u64 << 27) / (65_535 * 128), 16);
    }
}
