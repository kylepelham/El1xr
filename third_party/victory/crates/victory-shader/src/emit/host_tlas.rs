//! A tiny host DXR scene for the test host's dispatches ([`super::host::Root::Tlas`]):
//! triangle geometries in one bottom level, instances of it in a top level,
//! built on the dispatch's own command list before the program runs.

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::Interface;

/// A triangle: three vertices, x y z each.
pub type Triangle = [[f32; 3]; 3];

/// One instance of the scene's bottom level.
#[derive(Clone, Copy, Debug)]
pub struct Instance {
    /// Added to every vertex (the transform is a translation).
    pub offset: [f32; 3],
    pub id: u32,
    pub contribution: u32,
    pub mask: u8,
}

/// The scene: geometries of the one bottom level (with whether each is
/// opaque), and the top level's instances of it.
pub struct Scene {
    pub geometries: Vec<(Vec<Triangle>, bool)>,
    pub instances: Vec<Instance>,
}

fn err(e: windows::core::Error) -> String {
    e.message().to_string()
}

/// Builds `scene` on `list`; returns the top level's address. What the
/// card reads stays alive in `keep` until the list is done.
///
/// # Safety
/// `list` is open and recording; `keep` outlives its execution.
pub unsafe fn build(
    dev: &ID3D12Device,
    list: &ID3D12GraphicsCommandList,
    scene: &Scene,
    keep: &mut Vec<ID3D12Resource>,
) -> Result<u64, String> {
    let dev5: ID3D12Device5 = dev.cast().map_err(err)?;
    let list4: ID3D12GraphicsCommandList4 = list.cast().map_err(err)?;
    let buffer =
        |size: u64, heap: D3D12_HEAP_TYPE, flags, state| -> Result<ID3D12Resource, String> {
            let mut r: Option<ID3D12Resource> = None;
            // SAFETY: descriptions that outlive the call.
            unsafe {
                dev.CreateCommittedResource(
                    &D3D12_HEAP_PROPERTIES {
                        Type: heap,
                        ..Default::default()
                    },
                    D3D12_HEAP_FLAG_NONE,
                    &D3D12_RESOURCE_DESC {
                        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                        Width: size.max(256).next_multiple_of(256),
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
                .map_err(err)?;
            }
            r.ok_or_else(|| "no resource".to_owned())
        };
    let upload = |bytes: &[u8]| -> Result<ID3D12Resource, String> {
        let b = buffer(
            bytes.len() as u64,
            D3D12_HEAP_TYPE_UPLOAD,
            D3D12_RESOURCE_FLAG_NONE,
            D3D12_RESOURCE_STATE_GENERIC_READ,
        )?;
        let mut p = std::ptr::null_mut();
        // SAFETY: an upload buffer at least `bytes` long.
        unsafe {
            b.Map(0, None, Some(&mut p)).map_err(err)?;
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), p.cast::<u8>(), bytes.len());
            b.Unmap(0, None);
        }
        Ok(b)
    };
    // Builds `inputs` into a new structure; returns its address.
    let structure = |inputs: &D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS,
                         keep: &mut Vec<ID3D12Resource>|
     -> Result<u64, String> {
        let mut info = D3D12_RAYTRACING_ACCELERATION_STRUCTURE_PREBUILD_INFO::default();
        // SAFETY: inputs point at live descriptions.
        unsafe { dev5.GetRaytracingAccelerationStructurePrebuildInfo(inputs, &mut info) };
        let dest = buffer(
            info.ResultDataMaxSizeInBytes,
            D3D12_HEAP_TYPE_DEFAULT,
            D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
            D3D12_RESOURCE_STATE_RAYTRACING_ACCELERATION_STRUCTURE,
        )?;
        let scratch = buffer(
            info.ScratchDataSizeInBytes,
            D3D12_HEAP_TYPE_DEFAULT,
            D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
            D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
        )?;
        // SAFETY: live buffers of the sizes asked for.
        let at = unsafe {
            let at = dest.GetGPUVirtualAddress();
            list4.BuildRaytracingAccelerationStructure(
                &D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_DESC {
                    DestAccelerationStructureData: at,
                    Inputs: *inputs,
                    SourceAccelerationStructureData: 0,
                    ScratchAccelerationStructureData: scratch.GetGPUVirtualAddress(),
                },
                None,
            );
            list.ResourceBarrier(&[D3D12_RESOURCE_BARRIER {
                Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
                Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
                Anonymous: D3D12_RESOURCE_BARRIER_0 {
                    UAV: std::mem::ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER {
                        pResource: std::mem::transmute_copy(&dest),
                    }),
                },
            }]);
            at
        };
        keep.extend([dest, scratch]);
        Ok(at)
    };
    let mut geometries = Vec::new();
    for (triangles, opaque) in &scene.geometries {
        let bytes: Vec<u8> = triangles
            .iter()
            .flatten()
            .flatten()
            .flat_map(|f| f.to_le_bytes())
            .collect();
        let vertices = upload(&bytes)?;
        // SAFETY: a live buffer.
        let at = unsafe { vertices.GetGPUVirtualAddress() };
        keep.push(vertices);
        geometries.push(D3D12_RAYTRACING_GEOMETRY_DESC {
            Type: D3D12_RAYTRACING_GEOMETRY_TYPE_TRIANGLES,
            Flags: if *opaque {
                D3D12_RAYTRACING_GEOMETRY_FLAG_OPAQUE
            } else {
                D3D12_RAYTRACING_GEOMETRY_FLAG_NONE
            },
            Anonymous: D3D12_RAYTRACING_GEOMETRY_DESC_0 {
                Triangles: D3D12_RAYTRACING_GEOMETRY_TRIANGLES_DESC {
                    Transform3x4: 0,
                    IndexFormat: DXGI_FORMAT_UNKNOWN,
                    VertexFormat: DXGI_FORMAT_R32G32B32_FLOAT,
                    IndexCount: 0,
                    VertexCount: 3 * triangles.len() as u32,
                    IndexBuffer: 0,
                    VertexBuffer: D3D12_GPU_VIRTUAL_ADDRESS_AND_STRIDE {
                        StartAddress: at,
                        StrideInBytes: 12,
                    },
                },
            },
        });
    }
    let bottom = structure(
        &D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS {
            Type: D3D12_RAYTRACING_ACCELERATION_STRUCTURE_TYPE_BOTTOM_LEVEL,
            Flags: D3D12_RAYTRACING_ACCELERATION_STRUCTURE_BUILD_FLAG_PREFER_FAST_TRACE,
            NumDescs: geometries.len() as u32,
            DescsLayout: D3D12_ELEMENTS_LAYOUT_ARRAY,
            Anonymous: D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS_0 {
                pGeometryDescs: geometries.as_ptr(),
            },
        },
        keep,
    )?;
    // D3D12_RAYTRACING_INSTANCE_DESC: a 3x4 row-major transform, id and
    // mask, contribution and flags, the bottom level's address.
    let mut descs = Vec::new();
    for i in &scene.instances {
        let [x, y, z] = i.offset;
        for f in [1.0, 0.0, 0.0, x, 0.0, 1.0, 0.0, y, 0.0, 0.0, 1.0, z] {
            descs.extend(f32::to_le_bytes(f));
        }
        descs.extend((i.id & 0xff_ffff | u32::from(i.mask) << 24).to_le_bytes());
        descs.extend((i.contribution & 0xff_ffff).to_le_bytes());
        descs.extend(bottom.to_le_bytes());
    }
    let instances = upload(&descs)?;
    // SAFETY: a live buffer.
    let at = unsafe { instances.GetGPUVirtualAddress() };
    keep.push(instances);
    structure(
        &D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS {
            Type: D3D12_RAYTRACING_ACCELERATION_STRUCTURE_TYPE_TOP_LEVEL,
            Flags: D3D12_RAYTRACING_ACCELERATION_STRUCTURE_BUILD_FLAG_PREFER_FAST_TRACE,
            NumDescs: scene.instances.len() as u32,
            DescsLayout: D3D12_ELEMENTS_LAYOUT_ARRAY,
            Anonymous: D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS_0 { InstanceDescs: at },
        },
        keep,
    )
}

/// Whether `dev` has inline ray queries (ray tracing tier 1.1).
pub fn supported(dev: &ID3D12Device) -> bool {
    let mut o5 = D3D12_FEATURE_DATA_D3D12_OPTIONS5::default();
    // SAFETY: a plain query with a matching out structure.
    unsafe {
        dev.CheckFeatureSupport(
            D3D12_FEATURE_D3D12_OPTIONS5,
            (&raw mut o5).cast(),
            size_of_val(&o5) as u32,
        )
    }
    .is_ok()
        && o5.RaytracingTier.0 >= D3D12_RAYTRACING_TIER_1_1.0
}
