//! The committed-hit ray-query ops (`ray_query.rs`) run on WARP against a
//! host top level with a known hit: the indices the glass splice will put in
//! the title's result registers, and the record they make
//! (`glass_record::pack_hit`). Skipped without WARP ray tracing tier 1.1 or
//! without the validator (an unsigned program is refused).

use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::core::Interface;

use super::bitcode::{Attr, Term, V};
use super::glass_record::{Hit, pack_hit, unpack};
use super::ray_query::Committed;
use super::shader::{ROOT_CBV_SPACE, SLOT_TLAS, Shader};

const OUT_DWORDS: u32 = 9;
/// `RAY_FLAG_CULL_NON_OPAQUE` (0x40 is `CULL_OPAQUE`).
const CULL_NON_OPAQUE: u32 = 0x80;

fn store(s: &mut Shader, block: usize, at: u32, v: V, float: bool) {
    let h = s.root_uav_handle(1);
    let ty = if float { s.t.f32 } else { s.t.i32 };
    let off = s.m.i32(at * 4);
    let i32t = s.t.i32;
    let iu = s.m.undef(i32t);
    let u = s.m.undef(ty);
    let mask = s.m.i8(1);
    let align = s.m.i32(4);
    let name = if float {
        "rawBufferStore.f32"
    } else {
        "rawBufferStore.i32"
    };
    let void = s.t.void;
    s.op(
        block,
        name,
        Attr::NoUnwind,
        void,
        140,
        &[h, off, iu, v, u, u, u, mask, align],
    );
}

/// One thread traces (-0.5, 0, -1) along +z and stores status, t, the five
/// indices and both barycentrics in `u1`.
fn tracer() -> Vec<u8> {
    let mut s = Shader::new(false);
    s.threads = Some([1, 1, 1]);
    let b = s.entry;
    let rq = s.ray_query(b, CULL_NON_OPAQUE);
    let f = |s: &mut Shader, x: f32| s.m.f32(x.to_bits());
    let (zero, one, half, far) = (
        f(&mut s, 0.0),
        f(&mut s, 1.0),
        f(&mut s, -0.5),
        f(&mut s, 100.0),
    );
    let minus_one = f(&mut s, -1.0);
    let flags = s.m.i32(0);
    let mask = s.m.i32(0xff);
    s.trace_ray_inline(
        b,
        rq,
        flags,
        mask,
        [half, zero, minus_one],
        zero,
        [zero, zero, one],
        far,
    );
    let (lp, done) = (s.m.new_block(), s.m.new_block());
    s.m.terminate(b, Term::Br(lp));
    let more = s.ray_query_proceed(lp, rq);
    s.m.terminate(lp, Term::CondBr(more, lp, done));
    let status = s.ray_query_committed_status(done, rq);
    store(&mut s, done, 0, status, false);
    let t = s.ray_query_committed_t(done, rq);
    store(&mut s, done, 1, t, true);
    for (k, what) in [
        Committed::InstanceIndex,
        Committed::InstanceId,
        Committed::GeometryIndex,
        Committed::PrimitiveIndex,
        Committed::Contribution,
    ]
    .into_iter()
    .enumerate()
    {
        let v = s.ray_query_committed(done, rq, what);
        store(&mut s, done, 2 + k as u32, v, false);
    }
    for c in 0..2 {
        let v = s.ray_query_committed_barycentric(done, rq, c);
        store(&mut s, done, 7 + u32::from(c), v, true);
    }
    s.m.terminate(done, Term::Ret);
    s.finish()
}

struct Gpu {
    device: ID3D12Device5,
    queue: ID3D12CommandQueue,
}

fn warp() -> Option<Gpu> {
    // SAFETY: plain D3D12/DXGI calls with valid out pointers.
    unsafe {
        let factory: IDXGIFactory4 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)).ok()?;
        let adapter: IDXGIAdapter = factory.EnumWarpAdapter().ok()?;
        let mut device: Option<ID3D12Device> = None;
        D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_12_0, &mut device).ok()?;
        let device: ID3D12Device5 = device?.cast().ok()?;
        let mut o = D3D12_FEATURE_DATA_D3D12_OPTIONS5::default();
        device
            .CheckFeatureSupport(
                D3D12_FEATURE_D3D12_OPTIONS5,
                &mut o as *mut _ as *mut _,
                std::mem::size_of_val(&o) as u32,
            )
            .ok()?;
        if o.RaytracingTier.0 < D3D12_RAYTRACING_TIER_1_1.0 {
            return None;
        }
        let queue = device
            .CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
                ..Default::default()
            })
            .ok()?;
        Some(Gpu { device, queue })
    }
}

impl Gpu {
    fn buffer(
        &self,
        heap: D3D12_HEAP_TYPE,
        size: u64,
        state: D3D12_RESOURCE_STATES,
    ) -> ID3D12Resource {
        let flags = if heap == D3D12_HEAP_TYPE_DEFAULT {
            D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS
        } else {
            D3D12_RESOURCE_FLAG_NONE
        };
        let mut r: Option<ID3D12Resource> = None;
        // SAFETY: descriptions that outlive the call.
        unsafe {
            self.device
                .CreateCommittedResource(
                    &D3D12_HEAP_PROPERTIES {
                        Type: heap,
                        ..Default::default()
                    },
                    D3D12_HEAP_FLAG_NONE,
                    &D3D12_RESOURCE_DESC {
                        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                        Width: size.max(256),
                        Height: 1,
                        DepthOrArraySize: 1,
                        MipLevels: 1,
                        SampleDesc: DXGI_SAMPLE_DESC {
                            Count: 1,
                            Quality: 0,
                        },
                        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                        Flags: flags,
                        ..Default::default()
                    },
                    state,
                    None,
                    &mut r,
                )
                .unwrap();
        }
        r.unwrap()
    }

    fn upload(&self, bytes: &[u8]) -> ID3D12Resource {
        let r = self.buffer(
            D3D12_HEAP_TYPE_UPLOAD,
            bytes.len() as u64,
            D3D12_RESOURCE_STATE_GENERIC_READ,
        );
        // SAFETY: a mapped upload buffer at least `bytes` long.
        unsafe {
            let mut p = std::ptr::null_mut();
            r.Map(0, None, Some(&mut p)).unwrap();
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), p.cast::<u8>(), bytes.len());
            r.Unmap(0, None);
        }
        r
    }

    /// Builds `inputs` into a new structure (recorded on `list`).
    fn build(
        &self,
        list: &ID3D12GraphicsCommandList4,
        inputs: D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS,
        keep: &mut Vec<ID3D12Resource>,
    ) -> ID3D12Resource {
        let mut info = D3D12_RAYTRACING_ACCELERATION_STRUCTURE_PREBUILD_INFO::default();
        // SAFETY: inputs naming live buffers; an open list.
        unsafe {
            self.device
                .GetRaytracingAccelerationStructurePrebuildInfo(&inputs, &mut info);
            let result = self.buffer(
                D3D12_HEAP_TYPE_DEFAULT,
                info.ResultDataMaxSizeInBytes,
                D3D12_RESOURCE_STATE_RAYTRACING_ACCELERATION_STRUCTURE,
            );
            let scratch = self.buffer(
                D3D12_HEAP_TYPE_DEFAULT,
                info.ScratchDataSizeInBytes,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
            );
            list.BuildRaytracingAccelerationStructure(
                &D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_DESC {
                    DestAccelerationStructureData: result.GetGPUVirtualAddress(),
                    Inputs: inputs,
                    SourceAccelerationStructureData: 0,
                    ScratchAccelerationStructureData: scratch.GetGPUVirtualAddress(),
                },
                None,
            );
            list.ResourceBarrier(&[uav_barrier(&result)]);
            keep.push(scratch);
            result
        }
    }
}

fn uav_barrier(r: &ID3D12Resource) -> D3D12_RESOURCE_BARRIER {
    D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            UAV: std::mem::ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER {
                // SAFETY: a borrowed copy of the interface pointer, not released by the barrier.
                pResource: unsafe { std::mem::transmute_copy(r) },
            }),
        },
    }
}

fn floats(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
}

/// A bottom level of two geometries: geometry 0 a triangle far off the ray,
/// geometry 1 two triangles of which the ray hits the second at
/// (-0.5, 0, 0). Three instances; the ray hits only the third (index 2),
/// `InstanceID` 0x12_3456, contribution 5. Returns the eight dwords the
/// tracer stores.
fn trace_known_scene(gpu: &Gpu, dxil: &[u8]) -> Vec<u32> {
    let dev = &gpu.device;
    let mut keep = Vec::new();
    // Geometry 0: off at x = 50. Geometry 1: triangle 0 off at y = 50;
    // triangle 1 (-1,-1,0) (1,-1,0) (-1,1,0): the hit (-0.5, 0) is
    // v0 + 0.25 (v1 - v0) + 0.5 (v2 - v0).
    let g0 = gpu.upload(&floats(&[50.0, 0.0, 0.0, 51.0, 0.0, 0.0, 50.0, 1.0, 0.0]));
    let g1 = gpu.upload(&floats(&[
        0.0, 50.0, 0.0, 1.0, 50.0, 0.0, 0.0, 51.0, 0.0, //
        -1.0, -1.0, 0.0, 1.0, -1.0, 0.0, -1.0, 1.0, 0.0,
    ]));
    let tri = |vb: &ID3D12Resource, count: u32| D3D12_RAYTRACING_GEOMETRY_DESC {
        Type: D3D12_RAYTRACING_GEOMETRY_TYPE_TRIANGLES,
        Flags: D3D12_RAYTRACING_GEOMETRY_FLAG_OPAQUE,
        Anonymous: D3D12_RAYTRACING_GEOMETRY_DESC_0 {
            Triangles: D3D12_RAYTRACING_GEOMETRY_TRIANGLES_DESC {
                Transform3x4: 0,
                IndexFormat: DXGI_FORMAT_UNKNOWN,
                VertexFormat: DXGI_FORMAT_R32G32B32_FLOAT,
                IndexCount: 0,
                VertexCount: count,
                IndexBuffer: 0,
                VertexBuffer: D3D12_GPU_VIRTUAL_ADDRESS_AND_STRIDE {
                    // SAFETY: a live buffer.
                    StartAddress: unsafe { vb.GetGPUVirtualAddress() },
                    StrideInBytes: 12,
                },
            },
        },
    };
    let geometry = [tri(&g0, 3), tri(&g1, 6)];
    // SAFETY: D3D12 calls with descriptions that outlive them; every
    // resource lives until the fence says the GPU is done.
    unsafe {
        let alloc: ID3D12CommandAllocator = dev
            .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
            .unwrap();
        let list: ID3D12GraphicsCommandList4 = dev
            .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &alloc, None)
            .unwrap();
        let blas = gpu.build(
            &list,
            D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS {
                Type: D3D12_RAYTRACING_ACCELERATION_STRUCTURE_TYPE_BOTTOM_LEVEL,
                Flags: D3D12_RAYTRACING_ACCELERATION_STRUCTURE_BUILD_FLAG_NONE,
                NumDescs: 2,
                DescsLayout: D3D12_ELEMENTS_LAYOUT_ARRAY,
                Anonymous: D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS_0 {
                    pGeometryDescs: geometry.as_ptr(),
                },
            },
            &mut keep,
        );
        // Instances (64 bytes): a 3x4 transform, InstanceID | mask << 24,
        // contribution | flags << 24, the bottom level's address.
        let mut inst = Vec::new();
        for (dx, id, contribution) in [(100.0f32, 7u32, 1u32), (-100.0, 8, 2), (0.0, 0x12_3456, 5)]
        {
            inst.extend(floats(&[
                1.0, 0.0, 0.0, dx, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0,
            ]));
            inst.extend((id | 0xff << 24).to_le_bytes());
            inst.extend(contribution.to_le_bytes());
            inst.extend(blas.GetGPUVirtualAddress().to_le_bytes());
        }
        let descs = gpu.upload(&inst);
        let tlas = gpu.build(
            &list,
            D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS {
                Type: D3D12_RAYTRACING_ACCELERATION_STRUCTURE_TYPE_TOP_LEVEL,
                Flags: D3D12_RAYTRACING_ACCELERATION_STRUCTURE_BUILD_FLAG_NONE,
                NumDescs: 3,
                DescsLayout: D3D12_ELEMENTS_LAYOUT_ARRAY,
                Anonymous: D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS_0 {
                    InstanceDescs: descs.GetGPUVirtualAddress(),
                },
            },
            &mut keep,
        );
        // Root: t251 (the structure) and u1 (the results), both in the root space.
        let params = [
            D3D12_ROOT_PARAMETER {
                ParameterType: D3D12_ROOT_PARAMETER_TYPE_SRV,
                Anonymous: D3D12_ROOT_PARAMETER_0 {
                    Descriptor: D3D12_ROOT_DESCRIPTOR {
                        ShaderRegister: u32::from(SLOT_TLAS),
                        RegisterSpace: ROOT_CBV_SPACE,
                    },
                },
                ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
            },
            D3D12_ROOT_PARAMETER {
                ParameterType: D3D12_ROOT_PARAMETER_TYPE_UAV,
                Anonymous: D3D12_ROOT_PARAMETER_0 {
                    Descriptor: D3D12_ROOT_DESCRIPTOR {
                        ShaderRegister: 1,
                        RegisterSpace: ROOT_CBV_SPACE,
                    },
                },
                ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
            },
        ];
        let mut blob = None;
        D3D12SerializeRootSignature(
            &D3D12_ROOT_SIGNATURE_DESC {
                NumParameters: 2,
                pParameters: params.as_ptr(),
                ..Default::default()
            },
            D3D_ROOT_SIGNATURE_VERSION_1,
            &mut blob,
            None,
        )
        .unwrap();
        let blob = blob.unwrap();
        let bytes =
            std::slice::from_raw_parts(blob.GetBufferPointer().cast::<u8>(), blob.GetBufferSize());
        let root: ID3D12RootSignature = dev.CreateRootSignature(0, bytes).unwrap();
        let pso: ID3D12PipelineState = dev
            .CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                pRootSignature: std::mem::transmute_copy(&root),
                CS: D3D12_SHADER_BYTECODE {
                    pShaderBytecode: dxil.as_ptr().cast(),
                    BytecodeLength: dxil.len(),
                },
                ..Default::default()
            })
            .unwrap();
        let out = gpu.buffer(
            D3D12_HEAP_TYPE_DEFAULT,
            u64::from(OUT_DWORDS) * 4,
            D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
        );
        let back = gpu.buffer(
            D3D12_HEAP_TYPE_READBACK,
            u64::from(OUT_DWORDS) * 4,
            D3D12_RESOURCE_STATE_COPY_DEST,
        );
        list.SetPipelineState(&pso);
        list.SetComputeRootSignature(&root);
        list.SetComputeRootShaderResourceView(0, tlas.GetGPUVirtualAddress());
        list.SetComputeRootUnorderedAccessView(1, out.GetGPUVirtualAddress());
        list.Dispatch(1, 1, 1);
        list.ResourceBarrier(&[D3D12_RESOURCE_BARRIER {
            Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
            Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
            Anonymous: D3D12_RESOURCE_BARRIER_0 {
                Transition: std::mem::ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                    pResource: std::mem::transmute_copy(&out),
                    Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                    StateBefore: D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                    StateAfter: D3D12_RESOURCE_STATE_COPY_SOURCE,
                }),
            },
        }]);
        list.CopyResource(&back, &out);
        list.Close().unwrap();
        gpu.queue.ExecuteCommandLists(&[Some(list.cast().unwrap())]);
        let fence: ID3D12Fence = dev.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap();
        gpu.queue.Signal(&fence, 1).unwrap();
        while fence.GetCompletedValue() < 1 {
            std::thread::yield_now();
        }
        let mut p = std::ptr::null_mut();
        back.Map(0, None, Some(&mut p)).unwrap();
        let words = std::slice::from_raw_parts(p.cast::<u32>(), OUT_DWORDS as usize).to_vec();
        back.Unmap(0, None);
        drop((g0, g1, blas, tlas, descs, keep));
        words
    }
}

#[test]
fn committed_hit_indices_on_warp() {
    let mut dxil = tracer();
    let Ok(mut v) =
        super::validator::Validator::load(super::validator::Validator::default_path().as_deref())
    else {
        return;
    };
    v.validate(&mut dxil).unwrap();
    let Some(gpu) = warp() else {
        eprintln!("WARP has no ray tracing tier 1.1: skipped");
        return;
    };
    let w = trace_known_scene(&gpu, &dxil);
    let f = |i: usize| f32::from_bits(w[i]);
    assert_eq!(w[0], 1, "committed a triangle");
    assert!((f(1) - 1.0).abs() < 1e-5, "t = {}", f(1));
    assert_eq!(
        &w[2..7],
        &[2, 0x12_3456, 1, 1, 5],
        "instance, id, geometry, primitive, contribution"
    );
    assert!(
        (f(7) - 0.25).abs() < 1e-5 && (f(8) - 0.5).abs() < 1e-5,
        "barycentrics {} {}",
        f(7),
        f(8)
    );
    // What the splice puts in the result registers, through the record.
    let hit = Hit {
        t: f(1),
        flag15: false,
        field27: 31,
        b0: f(7),
        b1: f(8),
        instance: w[2],
        geometry: w[4],
        field10: 0,
        primitive: w[5],
        flag28: false,
        second: true,
        flag30: false,
    };
    let back = unpack(pack_hit(&hit)).expect("a hit");
    assert_eq!((back.instance, back.geometry, back.primitive), (2, 1, 1));
    assert_eq!(back.t, 1.0);
    // Material index: contribution + geometry, as the title's v24.
    assert_eq!(w[6] + w[4], 6);
}
