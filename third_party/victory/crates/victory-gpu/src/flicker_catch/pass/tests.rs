use super::*;
use windows::Win32::Foundation::{CloseHandle, RECT, WAIT_OBJECT_0};
use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

const W: u32 = 40;
const H: u32 = 24;

/// On the software adapter: a texture red on its left half and black on
/// its right is drawn small red on the left and black on the right, and
/// its tiles say so.
#[test]
fn the_pass_draws_the_presented_texture_small() {
    let _serial = crate::indirect::tests::WARP_SERIAL
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    // SAFETY: plain object creation and use on a device of our own; every
    // submission is waited for before what it wrote is read.
    unsafe {
        crate::test_d3d12_debug::enable();
        let Ok(adapter) = crate::host::warp_adapter() else {
            eprintln!("no WARP adapter: skipped");
            return;
        };
        let mut device: Option<ID3D12Device> = None;
        if D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_12_0, &mut device).is_err() {
            eprintln!("no WARP device: skipped");
            return;
        }
        let device = device.unwrap();
        let queue: ID3D12CommandQueue = device
            .CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
                ..Default::default()
            })
            .unwrap();
        let allocator: ID3D12CommandAllocator = device
            .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
            .unwrap();
        let list: ID3D12GraphicsCommandList = device
            .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)
            .unwrap();
        let targets: ID3D12DescriptorHeap = device
            .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                Type: D3D12_DESCRIPTOR_HEAP_TYPE_RTV,
                NumDescriptors: 1,
                Flags: D3D12_DESCRIPTOR_HEAP_FLAG_NONE,
                NodeMask: 0,
            })
            .unwrap();
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Width: u64::from(W),
            Height: H,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Flags: D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
            ..Default::default()
        };
        let mut texture: Option<ID3D12Resource> = None;
        device
            .CreateCommittedResource(
                &D3D12_HEAP_PROPERTIES {
                    Type: D3D12_HEAP_TYPE_DEFAULT,
                    ..Default::default()
                },
                D3D12_HEAP_FLAG_NONE,
                &desc,
                D3D12_RESOURCE_STATE_RENDER_TARGET,
                None,
                &mut texture,
            )
            .unwrap();
        let texture = texture.unwrap();
        let target = targets.GetCPUDescriptorHandleForHeapStart();
        device.CreateRenderTargetView(&texture, None, target);
        list.ClearRenderTargetView(target, &[0.0, 0.0, 0.0, 1.0], None);
        let left = RECT {
            left: 0,
            top: 0,
            right: (W / 2) as i32,
            bottom: H as i32,
        };
        list.ClearRenderTargetView(target, &[1.0, 0.0, 0.0, 1.0], Some(&[left]));
        list.ResourceBarrier(&[transition(
            &texture,
            D3D12_RESOURCE_STATE_RENDER_TARGET,
            crate::hold_frame::READ,
        )]);
        let readback = buffer(
            &device,
            D3D12_HEAP_TYPE_READBACK,
            D3D12_RESOURCE_FLAG_NONE,
            D3D12_RESOURCE_STATE_COPY_DEST,
        )
        .unwrap();
        let mut pass = Pass::new(&device).expect("the pass");
        let format = crate::hold_frame::view_format(desc.Format).unwrap();
        pass.record(&device, &list, &texture, &desc, format, &readback);
        list.Close().unwrap();
        let run: ID3D12CommandList = windows::core::Interface::cast(&list).unwrap();
        queue.ExecuteCommandLists(&[Some(run)]);
        let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap();
        let event = CreateEventW(None, false, false, None).unwrap();
        queue.Signal(&fence, 1).unwrap();
        fence.SetEventOnCompletion(1, event).unwrap();
        assert_eq!(WaitForSingleObject(event, 60_000), WAIT_OBJECT_0);
        let _ = CloseHandle(event);
        assert!(device.GetDeviceRemovedReason().is_ok());
        // ID 820: ClearRenderTargetView without optimized clear value in test RT setup.
        crate::test_d3d12_debug::assert_clean_except(&device, "flicker_catch::pass", &[820]);
        let pixels = pixels_of(&readback).unwrap();
        assert_eq!(pixels[0], 0xff, "red at the top left");
        assert_eq!(pixels[WIDE / 2 - 1], 0xff, "red left of the middle");
        assert_eq!(pixels[WIDE / 2 + 1], 0, "black right of the middle");
        assert_eq!(pixels[WIDE * HIGH - 1], 0, "black at the bottom right");
        let tiles = detect::tiles_of(&pixels);
        assert!((tiles[0] - 0.2126 * 255.0).abs() < 0.5);
        assert!(tiles[detect::ACROSS - 1] < 0.01);
    }
}
