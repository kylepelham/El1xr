use super::*;
use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::System::Threading::{CreateEventW, INFINITE, WaitForSingleObject};

use crate::host::transition;
use windows::core::Interface;

#[test]
fn off_by_default() {
    assert!(std::env::var_os("VICTORY_NATIVE_BVH_GPU_INSTANCES").is_none());
    assert!(!on());
}

#[test]
fn the_table_is_sorted_by_title_address_once_each() {
    let (bytes, n) = table(vec![(0x30, 3), (0x1_0000_0010, 9), (0x10, 1), (0x30, 4)]);
    assert_eq!(n, 3);
    let words: Vec<u64> = bytes
        .chunks_exact(8)
        .map(|c| u64::from_le_bytes(c.try_into().unwrap()))
        .collect();
    assert_eq!(words, [0x10, 1, 0x30, 3, 0x1_0000_0010, 9]);
    // Never an empty view.
    let (bytes, n) = table(Vec::new());
    assert_eq!((bytes.len(), n), (16, 0));
}

/// One 64-byte instance description: the transform's first word `tag`,
/// instance id 0x123456 with mask 0xff, contribution `tag`, bottom level `blas`.
fn instance(tag: u32, blas: u64) -> [u8; 64] {
    let mut d = [0u8; 64];
    d[0..4].copy_from_slice(&tag.to_le_bytes());
    d[44..48].copy_from_slice(&(tag * 3).to_le_bytes());
    d[48..52].copy_from_slice(&0xff12_3456u32.to_le_bytes());
    d[52..56].copy_from_slice(&tag.to_le_bytes());
    d[56..64].copy_from_slice(&blas.to_le_bytes());
    d
}

/// Runs the pass on WARP over `instances` with `pairs`: the host array.
fn on_warp(instances: &[[u8; 64]], pairs: Vec<(u64, u64)>) -> Option<Vec<u8>> {
    let _serial = crate::indirect::tests::WARP_SERIAL
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    // SAFETY: plain object creation and use on a device of our own; the
    // submission is waited for before what it wrote is read.
    unsafe {
        let Ok(adapter) = crate::host::warp_adapter() else {
            eprintln!("no WARP adapter: skipped");
            return None;
        };
        let mut device: Option<ID3D12Device> = None;
        if D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_12_0, &mut device).is_err() {
            eprintln!("no WARP device: skipped");
            return None;
        }
        let device = device.unwrap();
        let kit = make(&device).expect("the pass compiles and its pipeline is made");
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
        let made = |heap: D3D12_HEAP_TYPE, bytes: u64, flags, state| {
            let mut out: Option<ID3D12Resource> = None;
            device
                .CreateCommittedResource(
                    &D3D12_HEAP_PROPERTIES {
                        Type: heap,
                        ..Default::default()
                    },
                    D3D12_HEAP_FLAG_NONE,
                    &D3D12_RESOURCE_DESC {
                        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                        Width: bytes,
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
                    &mut out,
                )
                .unwrap();
            out.unwrap()
        };
        let filled = |bytes: &[u8]| {
            let r = made(
                D3D12_HEAP_TYPE_UPLOAD,
                bytes.len() as u64,
                D3D12_RESOURCE_FLAG_NONE,
                D3D12_RESOURCE_STATE_GENERIC_READ,
            );
            let mut at = std::ptr::null_mut();
            r.Map(0, None, Some(&mut at)).unwrap();
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), at.cast::<u8>(), bytes.len());
            r.Unmap(0, None);
            r
        };
        let raw: Vec<u8> = instances.iter().flatten().copied().collect();
        let title = filled(&raw);
        let (bytes, entries) = table(pairs);
        let lookup = filled(&bytes);
        let size = raw.len() as u64;
        let host = made(
            D3D12_HEAP_TYPE_DEFAULT,
            size,
            D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
            D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
        );
        let back = made(
            D3D12_HEAP_TYPE_READBACK,
            size,
            D3D12_RESOURCE_FLAG_NONE,
            D3D12_RESOURCE_STATE_COPY_DEST,
        );
        dispatch(
            &list,
            &kit,
            title.GetGPUVirtualAddress(),
            lookup.GetGPUVirtualAddress(),
            entries,
            host.GetGPUVirtualAddress(),
            instances.len() as u32,
        );
        list.ResourceBarrier(&[transition(
            &host,
            D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
            D3D12_RESOURCE_STATE_COPY_SOURCE,
        )]);
        list.CopyBufferRegion(&back, 0, &host, 0, size);
        list.Close().unwrap();
        queue.ExecuteCommandLists(&[Some(list.cast().unwrap())]);
        let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap();
        queue.Signal(&fence, 1).unwrap();
        let event = CreateEventW(None, false, false, None).unwrap();
        fence.SetEventOnCompletion(1, event).unwrap();
        WaitForSingleObject(event, INFINITE);
        let mut at = std::ptr::null_mut();
        back.Map(0, None, Some(&mut at)).unwrap();
        let out = std::slice::from_raw_parts(at.cast::<u8>(), size as usize).to_vec();
        back.Unmap(0, None);
        Some(out)
    }
}

/// The pass on the card's software twin: each instance gets its host bottom
/// level by binary search (also across the high dword), one without gets
/// mask 0, every other byte is the title's.
#[test]
fn instances_get_host_bottom_levels_on_the_card() {
    let (a, b, c) = (0xd_1000_0100u64, 0xd_1000_0200u64, 0xe_0000_0100u64);
    let instances: Vec<[u8; 64]> = [(1, a), (2, b), (3, c), (4, a), (5, 0)]
        .into_iter()
        .map(|(t, blas)| instance(t, blas))
        .collect();
    // Many entries around them, so the search takes several steps.
    let mut pairs: Vec<(u64, u64)> = (0..100u64)
        .map(|k| (0xd_0000_0000 + k * 0x1000, 0x9000 + k))
        .collect();
    pairs.extend([(a, 0xaaaa_0000), (c, 0x1_cccc_0000)]);
    let Some(out) = on_warp(&instances, pairs) else {
        return;
    };
    let host = |i: usize| u64::from_le_bytes(out[i * 64 + 56..i * 64 + 64].try_into().unwrap());
    let mask = |i: usize| u32::from_le_bytes(out[i * 64 + 48..i * 64 + 52].try_into().unwrap());
    assert_eq!((host(0), mask(0)), (0xaaaa_0000, 0xff12_3456));
    assert_eq!(
        (host(1), mask(1)),
        (0, 0),
        "no host bottom level: never hit"
    );
    assert_eq!((host(2), mask(2)), (0x1_cccc_0000, 0xff12_3456));
    assert_eq!((host(3), mask(3)), (0xaaaa_0000, 0xff12_3456));
    assert_eq!((host(4), mask(4)), (0, 0));
    for (i, inst) in instances.iter().enumerate() {
        assert_eq!(
            &out[i * 64..i * 64 + 48],
            &inst[..48],
            "instance {i}: transform"
        );
        assert_eq!(
            &out[i * 64 + 52..i * 64 + 56],
            &inst[52..56],
            "instance {i}: contribution"
        );
    }
}
