use super::*;
use windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

use crate::host::transition;

#[test]
fn the_switch_and_the_names_read_as_meant() {
    assert_eq!(parse(None), 0);
    assert_eq!(parse(Some("1")), 1);
    assert_eq!(parse(Some(" 2 ")), 2);
    assert_eq!(parse(Some("3")), 0);
    assert_eq!(pass_of(b"..CS_ApplyCombinedDeferred_1e5750b4.."), Some(0));
    assert_eq!(pass_of(b"..CS_TAA2_Resolve_12345678.."), Some(2));
    assert_eq!(pass_of(b"..CS_TransmissionVolumetrics.."), Some(5));
    assert_eq!(pass_of(b"CS_TemporalResolve"), None);
    // The post passes only when asked for; the transmission volume is not the glass.
    assert_eq!(pass_among(b"..CS_AlphaTransmissionDenoise_2871f2e9..", BASE), None);
    assert_eq!(pass_among(b"..CS_AlphaTransmissionDenoise_2871f2e9..", NAMES.len()), Some(8));
    assert_eq!(pass_among(b"CS_AlphaTransmissionDepthTileMipDilate_7f", NAMES.len()), Some(6));
    assert_eq!(pass_among(b"CS_TransmissionVolumetrics", NAMES.len()), Some(5));
    assert_eq!(pass_among(b"CS_MainBlur_446b3af2_Wrapped", NAMES.len()), Some(12));
    assert_eq!(POST.len(), POST_END - BASE);
    assert_eq!(POST.last(), Some(&"CS_BilateralBlur"));
    assert_eq!(GLASS.len(), NAMES.len() - POST_END);
    // The glass chain only with its switch; the pyramid's first stage by the title's spelling.
    let prefilter = b"..CS_AlphTransmissionPrefilterMip_610a2fad_Wrapped..";
    assert_eq!(pass_among(prefilter, POST_END), None);
    assert_eq!(pass_among(prefilter, NAMES.len()), Some(15));
    assert_eq!(pass_among(b"CS_RayMiss_de6b91a6_Wrapped", NAMES.len()), Some(17));
    assert_eq!(pass_among(b"CS_ScreenProbe_RayMiss_80a88544_Wrapped", NAMES.len()), None);
    assert_eq!(pass_among(b"CS_SpatialFilterAlpha_08e7839f", NAMES.len()), Some(23));
    assert_eq!(pass_among(b"CS_SpatialFilter_Std_be0b86ce", NAMES.len()), Some(24));
    assert_eq!(kind_of(26), Some(Kind::Float3));
    assert_eq!(kind_of(10), Some(Kind::Float4));
    assert_eq!(kind_of(41), Some(Kind::Float1));
    assert_eq!(kind_of(28), None);
    let mut found = [[0u32; 2]; NAMES.len()];
    found[2] = [3, 1];
    assert_eq!(line(&found, 0), "NaN/inf texels: CS_TAA2_Resolve nan=3 inf=1");
    found[18] = [0, 2];
    assert_eq!(
        line(&found, 19),
        "NaN/inf texels: CS_TAA2_Resolve nan=3 inf=1, CS_ResolveScreenSpace nan=0 inf=2 first=CS_ResolveScreenSpace"
    );
    assert_eq!(first_at(0) as u64, COUNT_BYTES - RING * 4);
    assert_eq!(first_at(RING - 1) as u64, COUNT_BYTES - 4);
}

const W: u32 = 20;
const H: u32 = 12;

/// Runs the pass on WARP over a `format` image whose texels are `texel(x, y)` (`channels`
/// floats each): the counts at entry 3 and the image after. The pass tags ring frame 1's
/// first-writer word with 9 if it finds any.
fn on_warp(
    format: DXGI_FORMAT,
    kind: Kind,
    channels: usize,
    scrub: bool,
    texel: impl Fn(u32, u32) -> Vec<f32>,
) -> Option<([u32; 2], Vec<f32>)> {
    let _serial = crate::indirect::tests::WARP_SERIAL
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    // SAFETY: plain object creation and use on a device of our own; every submission is
    // waited for before what it wrote is read.
    unsafe {
        crate::test_d3d12_debug::enable();
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
        let committed =
            |heap: D3D12_HEAP_TYPE, desc: &D3D12_RESOURCE_DESC, state: D3D12_RESOURCE_STATES| {
                let mut made: Option<ID3D12Resource> = None;
                device
                    .CreateCommittedResource(
                        &D3D12_HEAP_PROPERTIES {
                            Type: heap,
                            ..Default::default()
                        },
                        D3D12_HEAP_FLAG_NONE,
                        desc,
                        state,
                        None,
                        &mut made,
                    )
                    .unwrap();
                made.unwrap()
            };
        let buffer_desc = |bytes: u64, flags| D3D12_RESOURCE_DESC {
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
        };
        let image_desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Width: u64::from(W),
            Height: H,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: format,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
            ..Default::default()
        };
        let image = committed(
            D3D12_HEAP_TYPE_DEFAULT,
            &image_desc,
            D3D12_RESOURCE_STATE_COPY_DEST,
        );
        let pitch = (W * channels as u32 * 4).next_multiple_of(256);
        let bytes = u64::from(pitch * H);
        let upload = committed(
            D3D12_HEAP_TYPE_UPLOAD,
            &buffer_desc(bytes, D3D12_RESOURCE_FLAG_NONE),
            D3D12_RESOURCE_STATE_GENERIC_READ,
        );
        let mut at = std::ptr::null_mut();
        upload.Map(0, None, Some(&mut at)).unwrap();
        for y in 0..H {
            for x in 0..W {
                let t = texel(x, y);
                assert_eq!(t.len(), channels);
                let to = at
                    .cast::<u8>()
                    .add((y * pitch) as usize + x as usize * channels * 4)
                    .cast::<f32>();
                std::ptr::copy_nonoverlapping(t.as_ptr(), to, channels);
            }
        }
        upload.Unmap(0, None);
        let footprint = |resource: &ID3D12Resource| D3D12_TEXTURE_COPY_LOCATION {
            pResource: std::mem::transmute_copy(resource),
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                    Offset: 0,
                    Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                        Format: format,
                        Width: W,
                        Height: H,
                        Depth: 1,
                        RowPitch: pitch,
                    },
                },
            },
        };
        let whole = D3D12_TEXTURE_COPY_LOCATION {
            pResource: std::mem::transmute_copy(&image),
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                SubresourceIndex: 0,
            },
        };
        list.CopyTextureRegion(&whole, 0, 0, 0, &footprint(&upload), None);
        list.ResourceBarrier(&[transition(
            &image,
            D3D12_RESOURCE_STATE_COPY_DEST,
            D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
        )]);
        let views: ID3D12DescriptorHeap = device
            .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                NumDescriptors: 1,
                Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                NodeMask: 0,
            })
            .unwrap();
        device.CreateUnorderedAccessView(
            &image,
            None,
            None,
            views.GetCPUDescriptorHandleForHeapStart(),
        );
        let counts = committed(
            D3D12_HEAP_TYPE_DEFAULT,
            &buffer_desc(COUNT_BYTES, D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS),
            D3D12_RESOURCE_STATE_COMMON,
        );
        let zero = committed(
            D3D12_HEAP_TYPE_UPLOAD,
            &buffer_desc(COUNT_BYTES, D3D12_RESOURCE_FLAG_NONE),
            D3D12_RESOURCE_STATE_GENERIC_READ,
        );
        let mut z = std::ptr::null_mut();
        zero.Map(0, None, Some(&mut z)).unwrap();
        std::ptr::write_bytes(z.cast::<u8>(), 0, COUNT_BYTES as usize);
        zero.Unmap(0, None);
        list.CopyBufferRegion(&counts, 0, &zero, 0, COUNT_BYTES);
        list.ResourceBarrier(&[transition(
            &counts,
            D3D12_RESOURCE_STATE_COPY_DEST,
            D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
        )]);
        let pass = Pass::new(&device).expect("the pass");
        list.SetDescriptorHeaps(&[Some(views.clone())]);
        pass.record(
            &list,
            kind,
            views.GetGPUDescriptorHandleForHeapStart(),
            [W, H],
            3,
            scrub,
            counts.GetGPUVirtualAddress(),
            first_at(1),
            9,
        );
        let back = committed(
            D3D12_HEAP_TYPE_READBACK,
            &buffer_desc(bytes + COUNT_BYTES, D3D12_RESOURCE_FLAG_NONE),
            D3D12_RESOURCE_STATE_COPY_DEST,
        );
        list.ResourceBarrier(&[
            transition(
                &image,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                D3D12_RESOURCE_STATE_COPY_SOURCE,
            ),
            transition(
                &counts,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                D3D12_RESOURCE_STATE_COPY_SOURCE,
            ),
        ]);
        list.CopyTextureRegion(&footprint(&back), 0, 0, 0, &whole, None);
        list.CopyBufferRegion(&back, bytes, &counts, 0, COUNT_BYTES);
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
        crate::test_d3d12_debug::assert_clean(&device, "hdr_scrub");
        let mut read = std::ptr::null_mut();
        back.Map(0, None, Some(&mut read)).unwrap();
        let read = read.cast::<u8>();
        let mut texels = Vec::new();
        for y in 0..H {
            let row = read.add((y * pitch) as usize).cast::<f32>();
            texels.extend_from_slice(std::slice::from_raw_parts(row, W as usize * channels));
        }
        let words = read.add(bytes as usize).cast::<u32>();
        let found = [words.add(6).read(), words.add(7).read()];
        let tag_word = first_at(1) as usize / 4;
        let tag = words.add(tag_word).read();
        assert_eq!(tag, if found != [0, 0] { 9 } else { 0 }, "the first writer is tagged");
        let others = (0..(COUNT_BYTES / 4) as usize)
            .filter(|&i| i != 6 && i != 7 && i != tag_word)
            .all(|i| words.add(i).read() == 0);
        assert!(others, "counts only at the entry asked");
        back.Unmap(0, None);
        Some((found, texels))
    }
}

/// A RGBA32F image with 3 NaN texels (one with only alpha NaN) and 2 infinite ones: counted
/// 3 and 2; scrubbed, those channels are 0 and the rest as it was.
#[test]
fn warp_the_pass_counts_and_scrubs_float4_images() {
    let texel = |x: u32, y: u32| match (x, y) {
        (1, 1) => vec![f32::NAN, 1.0, 2.0, 1.0],
        (19, 0) => vec![0.5, 0.5, 0.5, f32::NAN],
        (7, 11) => vec![f32::NAN, f32::INFINITY, 0.0, 1.0],
        (3, 5) => vec![f32::INFINITY, 1.0, 1.0, 1.0],
        (4, 5) => vec![1.0, f32::NEG_INFINITY, 1.0, 1.0],
        _ => vec![x as f32, y as f32, 70_000.0, -1.0],
    };
    let format = DXGI_FORMAT_R32G32B32A32_FLOAT;
    let Some((found, kept)) = on_warp(format, Kind::Float4, 4, false, texel) else {
        return;
    };
    assert_eq!(found, [3, 2]);
    let at = |t: &[f32], x: u32, y: u32| t[((y * W + x) * 4) as usize..][..4].to_vec();
    assert!(at(&kept, 1, 1)[0].is_nan(), "counted only");
    assert_eq!(at(&kept, 3, 5)[0], f32::INFINITY);
    let (found, scrubbed) = on_warp(format, Kind::Float4, 4, true, texel).unwrap();
    assert_eq!(found, [3, 2]);
    assert_eq!(at(&scrubbed, 1, 1), [0.0, 1.0, 2.0, 1.0]);
    assert_eq!(at(&scrubbed, 19, 0), [0.5, 0.5, 0.5, 0.0]);
    assert_eq!(at(&scrubbed, 7, 11), [0.0, 0.0, 0.0, 1.0]);
    assert_eq!(at(&scrubbed, 3, 5), [0.0, 1.0, 1.0, 1.0]);
    assert_eq!(at(&scrubbed, 4, 5), [1.0, 0.0, 1.0, 1.0]);
    assert_eq!(
        at(&scrubbed, 2, 9),
        [2.0, 9.0, 70_000.0, -1.0],
        "numbers kept"
    );
}

/// A R32F image: one NaN, one infinity, counted; nothing else changes.
#[test]
fn warp_the_pass_counts_float1_images() {
    let texel = |x: u32, y: u32| match (x, y) {
        (0, 0) => vec![f32::NAN],
        (19, 11) => vec![f32::NEG_INFINITY],
        _ => vec![(x * y) as f32],
    };
    let Some((found, scrubbed)) = on_warp(DXGI_FORMAT_R32_FLOAT, Kind::Float1, 1, true, texel)
    else {
        return;
    };
    assert_eq!(found, [1, 1]);
    assert_eq!(scrubbed[0], 0.0);
    assert_eq!(scrubbed[(W * H - 1) as usize], 0.0);
    assert_eq!(scrubbed[(5 * W + 3) as usize], 15.0);
}
