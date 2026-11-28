//! On the card (`cargo test -p victory-gpu native_bvh_refit_cost -- --ignored
//! --nocapture`, under the `gpu` lock; not the game): what host DXR builds
//! and refits of the title's sizes cost (`VICTORY_NATIVE_BVH_REFIT`), and
//! what one shared scratch with a barrier after every build adds against
//! a scratch part each and one barrier (`VICTORY_NATIVE_BVH_BATCH`).

use super::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::System::Threading::{CreateEventW, INFINITE, WaitForSingleObject};

fn hardware() -> Option<ID3D12Device5> {
    // SAFETY: plain DXGI/D3D12 setup.
    unsafe {
        let factory: IDXGIFactory6 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)).ok()?;
        let adapter: IDXGIAdapter1 = factory
            .EnumAdapterByGpuPreference(0, DXGI_GPU_PREFERENCE_HIGH_PERFORMANCE)
            .ok()?;
        let mut device: Option<ID3D12Device5> = None;
        D3D12CreateDevice(
            &adapter,
            windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0,
            &mut device,
        )
        .ok()?;
        device
    }
}

fn upload(device: &ID3D12Device5, bytes: &[u8]) -> ID3D12Resource {
    let heap = D3D12_HEAP_PROPERTIES {
        Type: D3D12_HEAP_TYPE_UPLOAD,
        ..Default::default()
    };
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: bytes.len().max(256) as u64,
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
    // SAFETY: plain creation and a copy into the mapped buffer.
    unsafe {
        device
            .CreateCommittedResource(
                &heap,
                D3D12_HEAP_FLAG_NONE,
                &desc,
                D3D12_RESOURCE_STATE_GENERIC_READ,
                None,
                &mut out,
            )
            .unwrap();
        let out = out.unwrap();
        let mut data: *mut std::ffi::c_void = std::ptr::null_mut();
        out.Map(0, None, Some(&mut data)).unwrap();
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), data.cast::<u8>(), bytes.len());
        out.Unmap(0, None);
        out
    }
}

/// A grid of `side` x `side` quads (two triangles each): positions and
/// 16-bit indices (`side` <= 127), shifted by `wave` in height.
fn grid(side: u32, wave: f32) -> (Vec<u8>, Vec<u8>) {
    let mut v = Vec::new();
    for y in 0..=side {
        for x in 0..=side {
            let h = ((x + y) as f32 * 0.3 + wave).sin() * 0.2;
            for c in [x as f32, h, y as f32] {
                v.extend_from_slice(&c.to_le_bytes());
            }
        }
    }
    let mut i = Vec::new();
    let row = side + 1;
    for y in 0..side {
        for x in 0..side {
            let a = y * row + x;
            for k in [a, a + 1, a + row, a + 1, a + row + 1, a + row] {
                i.extend_from_slice(&(k as u16).to_le_bytes());
            }
        }
    }
    (v, i)
}

struct Timer {
    heap: ID3D12QueryHeap,
    stamps: ID3D12Resource,
    next: u32,
}

impl Timer {
    fn mark(&mut self, list: &ID3D12GraphicsCommandList4) -> u32 {
        // SAFETY: an open list and a heap with room (64 stamps).
        unsafe { list.EndQuery(&self.heap, D3D12_QUERY_TYPE_TIMESTAMP, self.next) };
        self.next += 1;
        self.next - 1
    }
}

#[test]
#[ignore = "on the card, under the gpu lock"]
fn native_bvh_refit_cost() {
    let Some(device) = hardware() else {
        return;
    };
    // SAFETY: a test harness of plain D3D12 calls on objects made here.
    unsafe {
        let queue: ID3D12CommandQueue = device
            .CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
                ..Default::default()
            })
            .unwrap();
        let allocator: ID3D12CommandAllocator = device
            .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
            .unwrap();
        let list: ID3D12GraphicsCommandList4 = device
            .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)
            .unwrap();
        let mut heap: Option<ID3D12QueryHeap> = None;
        device
            .CreateQueryHeap(
                &D3D12_QUERY_HEAP_DESC {
                    Type: D3D12_QUERY_HEAP_TYPE_TIMESTAMP,
                    Count: 64,
                    NodeMask: 0,
                },
                &mut heap,
            )
            .unwrap();
        let readback = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_READBACK,
            ..Default::default()
        };
        let mut stamps: Option<ID3D12Resource> = None;
        device
            .CreateCommittedResource(
                &readback,
                D3D12_HEAP_FLAG_NONE,
                &D3D12_RESOURCE_DESC {
                    Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                    Width: 64 * 8,
                    Height: 1,
                    DepthOrArraySize: 1,
                    MipLevels: 1,
                    SampleDesc: DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                    ..Default::default()
                },
                D3D12_RESOURCE_STATE_COPY_DEST,
                None,
                &mut stamps,
            )
            .unwrap();
        let mut timer = Timer {
            heap: heap.unwrap(),
            stamps: stamps.unwrap(),
            next: 0,
        };
        // The title's mix (bvh hook, batch 5): 300-1,900 bottom levels per 5 s,
        // 2-6 M triangles; 28-60 top levels of up to 3,072 instances.
        let cases = [("small", 512u32, 16u32), ("large", 256, 64)];
        let mut keep: Vec<ID3D12Resource> = Vec::new();
        let mut rows: Vec<(String, u32, u32)> = Vec::new();
        let mut last_blas: Vec<u64> = Vec::new();
        for (name, count, side) in cases {
            let (v0, idx) = grid(side, 0.0);
            let (v1, _) = grid(side, 1.0);
            let (vb0, vb1, ib) = (
                upload(&device, &v0),
                upload(&device, &v1),
                upload(&device, &idx),
            );
            let geometry = |vb: &ID3D12Resource| D3D12_RAYTRACING_GEOMETRY_DESC {
                Type: D3D12_RAYTRACING_GEOMETRY_TYPE_TRIANGLES,
                Flags: D3D12_RAYTRACING_GEOMETRY_FLAG_OPAQUE,
                Anonymous: D3D12_RAYTRACING_GEOMETRY_DESC_0 {
                    Triangles: D3D12_RAYTRACING_GEOMETRY_TRIANGLES_DESC {
                        Transform3x4: 0,
                        IndexFormat: DXGI_FORMAT(57),
                        VertexFormat: DXGI_FORMAT(6),
                        IndexCount: (idx.len() / 2) as u32,
                        VertexCount: (v0.len() / 12) as u32,
                        IndexBuffer: ib.GetGPUVirtualAddress(),
                        VertexBuffer: D3D12_GPU_VIRTUAL_ADDRESS_AND_STRIDE {
                            StartAddress: vb.GetGPUVirtualAddress(),
                            StrideInBytes: 12,
                        },
                    },
                },
            };
            let (g0, g1) = (geometry(&vb0), geometry(&vb1));
            let inputs = |g: &D3D12_RAYTRACING_GEOMETRY_DESC, update: bool| {
                D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS {
                    Type: D3D12_RAYTRACING_ACCELERATION_STRUCTURE_TYPE_BOTTOM_LEVEL,
                    Flags: flags(if update { 0x21 } else { 0x1 }, update),
                    NumDescs: 1,
                    DescsLayout: D3D12_ELEMENTS_LAYOUT_ARRAY,
                    Anonymous: D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS_0 {
                        pGeometryDescs: g,
                    },
                }
            };
            let mut info = D3D12_RAYTRACING_ACCELERATION_STRUCTURE_PREBUILD_INFO::default();
            device.GetRaytracingAccelerationStructurePrebuildInfo(&inputs(&g0, false), &mut info);
            let result = info.ResultDataMaxSizeInBytes.next_multiple_of(256);
            let scratch = info
                .ScratchDataSizeInBytes
                .max(info.UpdateScratchDataSizeInBytes)
                .next_multiple_of(256);
            let results: Vec<_> = (0..count)
                .map(|_| {
                    buffer(
                        &device,
                        result,
                        D3D12_RESOURCE_STATE_RAYTRACING_ACCELERATION_STRUCTURE,
                    )
                    .unwrap()
                })
                .collect();
            let scratches: Vec<_> = (0..count)
                .map(|_| buffer(&device, scratch, D3D12_RESOURCE_STATE_UNORDERED_ACCESS).unwrap())
                .collect();
            let tris = count * side * side * 2;
            let mut record = |label: &str,
                              g: &D3D12_RAYTRACING_GEOMETRY_DESC,
                              update: bool,
                              shared: bool,
                              timer: &mut Timer| {
                let start = timer.mark(&list);
                for (k, r) in results.iter().enumerate() {
                    let s = if shared { &scratches[0] } else { &scratches[k] };
                    let va = r.GetGPUVirtualAddress();
                    let desc = D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_DESC {
                        DestAccelerationStructureData: va,
                        Inputs: inputs(g, update),
                        SourceAccelerationStructureData: if update { va } else { 0 },
                        ScratchAccelerationStructureData: s.GetGPUVirtualAddress(),
                    };
                    list.BuildRaytracingAccelerationStructure(&desc, None);
                    if shared {
                        list.ResourceBarrier(&[uav_barrier(s), uav_barrier(r)]);
                    }
                }
                if !shared {
                    let all: Vec<_> = results
                        .iter()
                        .chain(scratches.iter())
                        .map(uav_barrier)
                        .collect();
                    list.ResourceBarrier(&all);
                }
                timer.mark(&list);
                rows.push((
                    format!("{name} bottom x{count} ({tris} triangles): {label}"),
                    start,
                    start + 1,
                ));
            };
            record("build, own scratch, one barrier (VICTORY_NATIVE_BVH_BATCH=1)", &g0, false, false, &mut timer);
            record("refit, own scratch, one barrier (_BATCH=1 _REFIT=1)", &g1, true, false, &mut timer);
            record(
                "build, one scratch + barrier each (both off)",
                &g0,
                false,
                true,
                &mut timer,
            );
            record(
                "refit, one scratch + barrier each (_REFIT=1 alone)",
                &g1,
                true,
                true,
                &mut timer,
            );
            last_blas = results.iter().map(|r| r.GetGPUVirtualAddress()).collect();
            keep.extend(results);
            keep.extend(scratches);
            keep.extend([vb0, vb1, ib]);
        }
        // A top level of 3,072 instances over the last bottom levels.
        let n = 3072usize;
        let mut raw = vec![0u8; n * INSTANCE_SIZE];
        for (k, inst) in raw.chunks_exact_mut(INSTANCE_SIZE).enumerate() {
            let m: [f32; 12] = [
                1.0,
                0.0,
                0.0,
                (k % 64) as f32 * 70.0,
                0.0,
                1.0,
                0.0,
                0.0,
                0.0,
                0.0,
                1.0,
                (k / 64) as f32 * 70.0,
            ];
            for (j, f) in m.iter().enumerate() {
                inst[j * 4..j * 4 + 4].copy_from_slice(&f.to_le_bytes());
            }
            inst[INSTANCE_MASK_AT..INSTANCE_MASK_AT + 4]
                .copy_from_slice(&0xff00_0000u32.to_le_bytes());
            inst[INSTANCE_BLAS_AT..].copy_from_slice(&last_blas[k % last_blas.len()].to_le_bytes());
        }
        let instances = upload(&device, &raw);
        let top = |update: bool| D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS {
            Type: D3D12_RAYTRACING_ACCELERATION_STRUCTURE_TYPE_TOP_LEVEL,
            Flags: flags(if update { 0x21 } else { 0x1 }, update),
            NumDescs: n as u32,
            DescsLayout: D3D12_ELEMENTS_LAYOUT_ARRAY,
            Anonymous: D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS_0 {
                InstanceDescs: instances.GetGPUVirtualAddress(),
            },
        };
        let mut info = D3D12_RAYTRACING_ACCELERATION_STRUCTURE_PREBUILD_INFO::default();
        device.GetRaytracingAccelerationStructurePrebuildInfo(&top(false), &mut info);
        let tlas = buffer(
            &device,
            info.ResultDataMaxSizeInBytes,
            D3D12_RESOURCE_STATE_RAYTRACING_ACCELERATION_STRUCTURE,
        )
        .unwrap();
        let scratch = buffer(
            &device,
            info.ScratchDataSizeInBytes
                .max(info.UpdateScratchDataSizeInBytes),
            D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
        )
        .unwrap();
        for update in [false, true] {
            let start = timer.mark(&list);
            let va = tlas.GetGPUVirtualAddress();
            list.BuildRaytracingAccelerationStructure(
                &D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_DESC {
                    DestAccelerationStructureData: va,
                    Inputs: top(update),
                    SourceAccelerationStructureData: if update { va } else { 0 },
                    ScratchAccelerationStructureData: scratch.GetGPUVirtualAddress(),
                },
                None,
            );
            list.ResourceBarrier(&[uav_barrier(&scratch), uav_barrier(&tlas)]);
            timer.mark(&list);
            rows.push((
                format!(
                    "top x1 ({n} instances): {}",
                    if update { "refit" } else { "build" }
                ),
                start,
                start + 1,
            ));
        }
        let used = timer.next;
        list.ResolveQueryData(
            &timer.heap,
            D3D12_QUERY_TYPE_TIMESTAMP,
            0,
            used,
            &timer.stamps,
            0,
        );
        list.Close().unwrap();
        queue.ExecuteCommandLists(&[Some(list.cast().unwrap())]);
        let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap();
        queue.Signal(&fence, 1).unwrap();
        let event = CreateEventW(None, false, false, None).unwrap();
        fence.SetEventOnCompletion(1, event).unwrap();
        WaitForSingleObject(event, INFINITE);
        assert!(
            device.GetDeviceRemovedReason().is_ok(),
            "the device was removed"
        );
        let freq = queue.GetTimestampFrequency().unwrap() as f64;
        let mut data: *mut std::ffi::c_void = std::ptr::null_mut();
        timer.stamps.Map(0, None, Some(&mut data)).unwrap();
        let t = std::slice::from_raw_parts(data.cast::<u64>(), used as usize);
        for (label, a, b) in &rows {
            let ms = (t[*b as usize] - t[*a as usize]) as f64 * 1e3 / freq;
            println!("native bvh refit cost: {label} {ms:.3} ms");
        }
        timer.stamps.Unmap(0, None);
        drop((keep, instances, tlas, scratch));
    }
}
