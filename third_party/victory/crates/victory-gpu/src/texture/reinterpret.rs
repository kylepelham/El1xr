//! (El1xr) An alias copy ([`super::alias`]) between formats D3D12 copies
//! texture to texture with no conversion ("Reinterpret copy" of
//! `CopyResource` and `CopyTextureRegion`): the title's lighting textures are
//! written as `R32_UINT` and read as `R9G9B9E5_SHAREDEXP` (77 copies a frame,
//! 1.48 ms of the card in `VICTORY_GPU_COST`), which went through a buffer, two
//! copies of every texel. Here one copy per level and layer, and no buffer.

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;

/// `VICTORY_ALIAS_DIRECT=1`: alias copies between such formats are direct
/// (default: through a buffer, as before).
pub(super) fn on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_ALIAS_DIRECT").is_some_and(|v| v == "1"))
}

/// Whether texels of `written` copy as they stand into a texture of `read`
/// of the same size (texel for texel, the same bits).
pub(super) fn direct(written: DXGI_FORMAT, read: DXGI_FORMAT) -> bool {
    let word = |f: DXGI_FORMAT| f == DXGI_FORMAT_R32_UINT || f == DXGI_FORMAT_R32_SINT;
    let shared = |f: DXGI_FORMAT| f == DXGI_FORMAT_R9G9B9E5_SHAREDEXP;
    (word(written) && shared(read)) || (shared(written) && word(read))
}

/// Records the copy of `parts` subresources of `from` (copy source state)
/// into `to` (copy destination state).
pub(super) fn record(
    list: &ID3D12GraphicsCommandList,
    from: &ID3D12Resource,
    to: &ID3D12Resource,
    parts: usize,
) {
    let part = |texture: &ID3D12Resource, index: usize| D3D12_TEXTURE_COPY_LOCATION {
        // SAFETY: a borrowed pointer the location never releases.
        pResource: unsafe { std::mem::transmute_copy(texture) },
        Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
            SubresourceIndex: index as u32,
        },
    };
    for index in 0..parts {
        // SAFETY: an open list; live textures of the same size per part, in
        // the copy states.
        unsafe { list.CopyTextureRegion(&part(to, index), 0, 0, 0, &part(from, index), None) };
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
    use windows::Win32::Graphics::Dxgi::*;
    use windows::Win32::System::Threading::{CreateEventW, INFINITE, WaitForSingleObject};
    use windows::core::Interface;

    const SIZE: u32 = 8;

    #[test]
    fn only_the_reinterpret_pairs_copy_directly() {
        assert!(direct(DXGI_FORMAT_R32_UINT, DXGI_FORMAT_R9G9B9E5_SHAREDEXP));
        assert!(direct(DXGI_FORMAT_R9G9B9E5_SHAREDEXP, DXGI_FORMAT_R32_SINT));
        assert!(!direct(
            DXGI_FORMAT_R32_FLOAT,
            DXGI_FORMAT_R9G9B9E5_SHAREDEXP
        ));
        assert!(!direct(DXGI_FORMAT_R32_UINT, DXGI_FORMAT_R8G8B8A8_UNORM));
        assert!(!direct(DXGI_FORMAT_R32_UINT, DXGI_FORMAT_R32_UINT));
    }

    fn warp() -> Option<ID3D12Device> {
        // SAFETY: plain DXGI and D3D12 queries.
        unsafe {
            crate::test_d3d12_debug::enable();
            let factory: IDXGIFactory4 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)).ok()?;
            let adapter: IDXGIAdapter = factory.EnumWarpAdapter().ok()?;
            let mut device: Option<ID3D12Device> = None;
            D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_12_0, &mut device).ok()?;
            device
        }
    }

    fn committed(
        device: &ID3D12Device,
        heap: D3D12_HEAP_TYPE,
        desc: &D3D12_RESOURCE_DESC,
        state: D3D12_RESOURCE_STATES,
    ) -> ID3D12Resource {
        let mut out: Option<ID3D12Resource> = None;
        // SAFETY: plain resource creation.
        unsafe {
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
                    &mut out,
                )
                .unwrap();
        }
        out.unwrap()
    }

    fn texture(format: DXGI_FORMAT) -> D3D12_RESOURCE_DESC {
        D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Width: u64::from(SIZE),
            Height: SIZE,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: format,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            ..Default::default()
        }
    }

    fn buffer(bytes: u64) -> D3D12_RESOURCE_DESC {
        D3D12_RESOURCE_DESC {
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
            ..Default::default()
        }
    }

    /// Words written as `R32_UINT` read back, bit for bit, from the
    /// `R9G9B9E5_SHAREDEXP` texture they were copied into directly.
    #[test]
    fn a_direct_copy_keeps_every_bit() {
        let Some(device) = warp() else {
            eprintln!("no WARP device: skipped");
            return;
        };
        let words: Vec<u32> = (0..SIZE * SIZE)
            .map(|i| i.wrapping_mul(0x9e37_79b9) ^ 0x5a5a_0f0f)
            .collect();
        let (written, read) = (
            texture(DXGI_FORMAT_R32_UINT),
            texture(DXGI_FORMAT_R9G9B9E5_SHAREDEXP),
        );
        let mut footprint = D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default();
        let mut bytes = 0u64;
        // SAFETY: outputs for one subresource.
        unsafe {
            device.GetCopyableFootprints(
                &written,
                0,
                1,
                0,
                Some(&mut footprint),
                None,
                None,
                Some(&mut bytes),
            )
        };
        let pitch = footprint.Footprint.RowPitch as usize;
        let upload = committed(
            &device,
            D3D12_HEAP_TYPE_UPLOAD,
            &buffer(bytes),
            D3D12_RESOURCE_STATE_GENERIC_READ,
        );
        let back = committed(
            &device,
            D3D12_HEAP_TYPE_READBACK,
            &buffer(bytes),
            D3D12_RESOURCE_STATE_COPY_DEST,
        );
        let from = committed(
            &device,
            D3D12_HEAP_TYPE_DEFAULT,
            &written,
            D3D12_RESOURCE_STATE_COPY_DEST,
        );
        let to = committed(
            &device,
            D3D12_HEAP_TYPE_DEFAULT,
            &read,
            D3D12_RESOURCE_STATE_COPY_DEST,
        );
        let at = |r: &ID3D12Resource| D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: a borrowed pointer the location never releases.
            pResource: unsafe { std::mem::transmute_copy(r) },
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                PlacedFootprint: footprint,
            },
        };
        let index = |r: &ID3D12Resource| D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: as above.
            pResource: unsafe { std::mem::transmute_copy(r) },
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                SubresourceIndex: 0,
            },
        };
        // SAFETY: a test's own device, list and resources; every copy is
        // waited for before the read-back buffer is read.
        unsafe {
            let mut data = std::ptr::null_mut();
            upload.Map(0, None, Some(&mut data)).unwrap();
            for row in 0..SIZE as usize {
                let src = &words[row * SIZE as usize..(row + 1) * SIZE as usize];
                std::ptr::copy_nonoverlapping(
                    src.as_ptr().cast::<u8>(),
                    data.cast::<u8>().add(row * pitch),
                    SIZE as usize * 4,
                );
            }
            upload.Unmap(0, None);
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
            list.CopyTextureRegion(&index(&from), 0, 0, 0, &at(&upload), None);
            list.ResourceBarrier(&[crate::host::transition(
                &from,
                D3D12_RESOURCE_STATE_COPY_DEST,
                D3D12_RESOURCE_STATE_COPY_SOURCE,
            )]);
            record(&list, &from, &to, 1);
            list.ResourceBarrier(&[crate::host::transition(
                &to,
                D3D12_RESOURCE_STATE_COPY_DEST,
                D3D12_RESOURCE_STATE_COPY_SOURCE,
            )]);
            list.CopyTextureRegion(&at(&back), 0, 0, 0, &index(&to), None);
            list.Close().unwrap();
            queue.ExecuteCommandLists(&[Some(list.cast().unwrap())]);
            let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap();
            let event: HANDLE = CreateEventW(None, false, false, None).unwrap();
            queue.Signal(&fence, 1).unwrap();
            fence.SetEventOnCompletion(1, event).unwrap();
            WaitForSingleObject(event, INFINITE);
            let mut data = std::ptr::null_mut();
            back.Map(0, None, Some(&mut data)).unwrap();
            for row in 0..SIZE as usize {
                let got = std::slice::from_raw_parts(
                    data.cast::<u8>().add(row * pitch).cast::<u32>(),
                    SIZE as usize,
                );
                assert_eq!(got, &words[row * SIZE as usize..(row + 1) * SIZE as usize]);
            }
            back.Unmap(0, None);
            crate::test_d3d12_debug::assert_clean(&device, "texture::reinterpret");
        }
    }
}
