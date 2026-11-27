//! On the card (`cargo test -p victory-gpu camera_copy_cost -- --ignored
//! --nocapture`, under the `gpu` lock): what the HUD-less copy and the
//! motion check's copies cost.

use super::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::System::Threading::{CreateEventW, INFINITE, WaitForSingleObject};
use windows::core::Interface;

fn hardware() -> Option<ID3D12Device> {
    // SAFETY: plain DXGI/D3D12 setup.
    unsafe {
        let factory: IDXGIFactory6 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)).ok()?;
        let adapter: IDXGIAdapter1 = factory
            .EnumAdapterByGpuPreference(0, DXGI_GPU_PREFERENCE_HIGH_PERFORMANCE)
            .ok()?;
        let mut device: Option<ID3D12Device> = None;
        windows::Win32::Graphics::Direct3D12::D3D12CreateDevice(
            &adapter,
            windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0,
            &mut device,
        )
        .ok()?;
        device
    }
}

fn tex(device: &ID3D12Device, format: DXGI_FORMAT, flags: D3D12_RESOURCE_FLAGS, state: u32) -> ID3D12Resource {
    let mut desc = copy_desc(&D3D12_RESOURCE_DESC {
        Width: 3840,
        Height: 2160,
        Format: format,
        ..Default::default()
    });
    desc.Flags = flags;
    texture(device, &desc, state).expect("texture")
}

#[test]
#[ignore = "on the card, under the gpu lock"]
fn camera_copy_cost() {
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
        let allocator: ID3D12CommandAllocator =
            device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).unwrap();
        let list: ID3D12GraphicsCommandList = device
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
        let heap = heap.unwrap();
        let rb_heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_READBACK,
            ..Default::default()
        };
        let stamps_desc = D3D12_RESOURCE_DESC {
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
        };
        let mut stamps: Option<ID3D12Resource> = None;
        device
            .CreateCommittedResource(
                &rb_heap,
                D3D12_HEAP_FLAG_NONE,
                &stamps_desc,
                D3D12_RESOURCE_STATE_COPY_DEST,
                None,
                &mut stamps,
            )
            .unwrap();
        let stamps = stamps.unwrap();
        let rt = D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET;
        let none = D3D12_RESOURCE_FLAG_NONE;
        let pairs = [
            ("hudless rgba16f", tex(&device, DXGI_FORMAT_R16G16B16A16_FLOAT, rt, COPY_SOURCE), tex(&device, DXGI_FORMAT_R16G16B16A16_FLOAT, none, COPY_DEST)),
            ("hudless rgb10a2", tex(&device, DXGI_FORMAT_R10G10B10A2_UNORM, rt, COPY_SOURCE), tex(&device, DXGI_FORMAT_R10G10B10A2_UNORM, none, COPY_DEST)),
            ("hudless rgba8", tex(&device, DXGI_FORMAT_R8G8B8A8_UNORM, rt, COPY_SOURCE), tex(&device, DXGI_FORMAT_R8G8B8A8_UNORM, none, COPY_DEST)),
            (
                "mv check: depth plane whole",
                tex(&device, DXGI_FORMAT_R32G8X24_TYPELESS, D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL, COPY_SOURCE),
                tex(&device, DXGI_FORMAT_R32_TYPELESS, none, COPY_DEST),
            ),
        ];
        let motion = tex(&device, DXGI_FORMAT_R16G16_FLOAT, none, COPY_SOURCE);
        let buffer = crate::camera_mv::bench_readback(&device);
        let rounds = 20u32;
        let mut q = 0u32;
        for _ in 0..rounds {
            for (_, from, to) in &pairs {
                list.EndQuery(&heap, D3D12_QUERY_TYPE_TIMESTAMP, q);
                list.CopyTextureRegion(&location(to), 0, 0, 0, &location(from), None);
                list.EndQuery(&heap, D3D12_QUERY_TYPE_TIMESTAMP, q + 1);
                q += 2;
                if q >= 60 {
                    q = 0;
                }
            }
        }
        // The last round alone, timed per item, and the 64-pixel copies.
        let mut q = 0u32;
        for (_, from, to) in &pairs {
            list.EndQuery(&heap, D3D12_QUERY_TYPE_TIMESTAMP, q);
            list.CopyTextureRegion(&location(to), 0, 0, 0, &location(from), None);
            list.EndQuery(&heap, D3D12_QUERY_TYPE_TIMESTAMP, q + 1);
            q += 2;
        }
        list.EndQuery(&heap, D3D12_QUERY_TYPE_TIMESTAMP, q);
        crate::camera_mv::bench_pixels(&list, &buffer, &motion, DXGI_FORMAT_R16G16_FLOAT, [3840, 2160]);
        list.EndQuery(&heap, D3D12_QUERY_TYPE_TIMESTAMP, q + 1);
        let n = q + 2;
        list.ResolveQueryData(&heap, D3D12_QUERY_TYPE_TIMESTAMP, 0, n, &stamps, 0);
        list.Close().unwrap();
        queue.ExecuteCommandLists(&[Some(list.cast().unwrap())]);
        let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap();
        queue.Signal(&fence, 1).unwrap();
        let event = CreateEventW(None, false, false, None).unwrap();
        fence.SetEventOnCompletion(1, event).unwrap();
        WaitForSingleObject(event, INFINITE);
        let freq = queue.GetTimestampFrequency().unwrap() as f64;
        let mut data: *mut std::ffi::c_void = std::ptr::null_mut();
        stamps.Map(0, None, Some(&mut data)).unwrap();
        let t = std::slice::from_raw_parts(data.cast::<u64>(), n as usize);
        let ms = |i: usize| (t[i + 1] - t[i]) as f64 * 1e3 / freq;
        for (k, (name, _, _)) in pairs.iter().enumerate() {
            println!("camera copy cost: {name} 3840x2160 {:.3} ms", ms(2 * k));
        }
        println!("camera copy cost: mv check 64 motion pixels {:.3} ms", ms(2 * pairs.len()));
        stamps.Unmap(0, None);
    }
}
