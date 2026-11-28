//! (`VICTORY_BAKE_REUSE_KEEP=1`, #115) End-to-end D3D12 (WARP/GPU) tests:
//! 1. Release a GPU-written texture and recreate it at the same address:
//!    - Switch OFF: uploads stale/zero title memory instead of GPU-written pixels.
//!    - Switch ON: adopts the retained host copy and preserves the GPU-written pixels.
//! 2. Overwriting title CPU memory between release and recreation drops the retained
//!    copy and uploads the fresh title memory.
//! 3. Remapping title memory between release and recreation (`title_memory_remapped`)
//!    immediately invalidates the retained copy and uploads fresh title memory.

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::Interface;

use super::keep::TEST_ON;
use crate::object::{S_OK, This, guid, slot};

const SIDE: u32 = 64;
const BYTES: u64 = (SIDE * SIDE * 4) as u64;
const BLOCK_BYTES: usize = 64 << 10;

const BAKED_PIXEL: u32 = 0xFF33_CC66;
const FRESH_TITLE_PIXEL: u32 = 0xFF44_88AA;
const STALE_TITLE_PIXEL: u32 = 0xFF11_2233;

/// 64x64 RGBA8 title-filled texture (linear layout = 1 = `D3D12_TEXTURE_LAYOUT_ROW_MAJOR`,
/// or console swizzle 0; uniform u32 fill produces identical pixels either way).
fn tex_desc() -> [u32; 14] {
    [
        3, // TEXTURE2D
        0,
        0,
        0,
        SIDE,
        0,
        SIDE,
        1 | (1 << 16), // 1 layer, 1 mip
        28,            // DXGI_FORMAT_R8G8B8A8_UNORM
        1,             // sample count
        0,
        0, // layout
        0, // flags (title-filled)
        0,
    ]
}

fn title_memory(fill_word: u32) -> u64 {
    let layout = std::alloc::Layout::from_size_align(BLOCK_BYTES, 1 << 16).unwrap();
    // SAFETY: non-zero aligned allocation; leaked as title memory outlives the test.
    let ptr = unsafe { std::alloc::alloc_zeroed(layout) };
    assert!(!ptr.is_null());
    if fill_word != 0 {
        // SAFETY: `BLOCK_BYTES` is a multiple of 4 and aligned to 64 KiB.
        let words = unsafe { std::slice::from_raw_parts_mut(ptr.cast::<u32>(), BLOCK_BYTES / 4) };
        words.fill(fill_word);
    }
    ptr as u64
}

fn create_texture_at(address: u64) -> This {
    let desc = tex_desc();
    let i = ID3D12Resource::IID;
    let iid = guid(i.data1, i.data2, i.data3, i.data4);
    let mut out = std::ptr::null();
    let hr = crate::resource::create_at_address(
        std::ptr::null(),
        address,
        desc.as_ptr().cast(),
        0,
        std::ptr::null(),
        &iid,
        &mut out,
    );
    assert_eq!(hr, S_OK);
    out
}

fn release(t: This) {
    let _ = slot::<extern "system" fn(This) -> u32>(t, 0x10)(t);
}

fn make_buffer(
    host: &crate::host::Host,
    kind: D3D12_HEAP_TYPE,
    state: D3D12_RESOURCE_STATES,
) -> ID3D12Resource {
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: BYTES,
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
    // SAFETY: valid buffer description.
    unsafe {
        host.device
            .CreateCommittedResource(
                &D3D12_HEAP_PROPERTIES {
                    Type: kind,
                    ..Default::default()
                },
                D3D12_HEAP_FLAG_NONE,
                &desc,
                state,
                None,
                &mut out,
            )
            .unwrap();
    }
    out.unwrap()
}

fn buf_loc(buffer: &ID3D12Resource) -> D3D12_TEXTURE_COPY_LOCATION {
    D3D12_TEXTURE_COPY_LOCATION {
        // SAFETY: borrowed pointer, never released by the location struct.
        pResource: unsafe { std::mem::transmute_copy(buffer) },
        Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
            PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                Offset: 0,
                Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                    Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                    Width: SIDE,
                    Height: SIDE,
                    Depth: 1,
                    RowPitch: SIDE * 4,
                },
            },
        },
    }
}

fn tex_loc(texture: &ID3D12Resource) -> D3D12_TEXTURE_COPY_LOCATION {
    D3D12_TEXTURE_COPY_LOCATION {
        // SAFETY: as above.
        pResource: unsafe { std::mem::transmute_copy(texture) },
        Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
            SubresourceIndex: 0,
        },
    }
}

/// Writes `pixel` onto `t`'s host texture on the GPU and marks `t` as `host_written`.
fn gpu_bake_into(host: &crate::host::Host, t: This, pixel: u32) {
    let upload = make_buffer(
        host,
        D3D12_HEAP_TYPE_UPLOAD,
        D3D12_RESOURCE_STATE_GENERIC_READ,
    );
    // SAFETY: upload buffer mapped for CPU fill.
    unsafe {
        let mut ptr = std::ptr::null_mut();
        upload.Map(0, None, Some(&mut ptr)).unwrap();
        let words = std::slice::from_raw_parts_mut(ptr.cast::<u32>(), (BYTES / 4) as usize);
        words.fill(pixel);
        upload.Unmap(0, None);
    }
    let host_tex = crate::resource::host_resource(t).unwrap();
    let list = host.new_list().unwrap();
    crate::list::ensure_state(&list, t, 0x400); // COPY_DEST
    // SAFETY: open list with `host_tex` in COPY_DEST and `upload` in GENERIC_READ.
    unsafe {
        list.list
            .CopyTextureRegion(&tex_loc(host_tex), 0, 0, 0, &buf_loc(&upload), None);
    }
    crate::texture::host_written(t);
    list.close();
    host.execute(&[&list]);
}

/// Runs the normal title-upload path on `t` and reads back `(first_pixel, last_pixel)`.
fn upload_and_readback(host: &crate::host::Host, t: This) -> (u32, u32) {
    let back = make_buffer(
        host,
        D3D12_HEAP_TYPE_READBACK,
        D3D12_RESOURCE_STATE_COPY_DEST,
    );
    let host_tex = crate::resource::host_resource(t).unwrap();
    let list = host.new_list().unwrap();
    crate::texture::upload(&list, t, host_tex);
    crate::list::ensure_state(&list, t, 0x800); // COPY_SOURCE
    // SAFETY: open list; prelude upload (if any) ran before main list commands, and
    // `host_tex` is now in COPY_SOURCE.
    unsafe {
        list.list
            .CopyTextureRegion(&buf_loc(&back), 0, 0, 0, &tex_loc(host_tex), None);
    }
    list.close();
    host.execute(&[&list]);
    // SAFETY: GPU execution finished; read back first and last texel.
    unsafe {
        let mut ptr = std::ptr::null_mut();
        back.Map(0, None, Some(&mut ptr)).unwrap();
        let words = std::slice::from_raw_parts(ptr.cast::<u32>(), (BYTES / 4) as usize);
        let pair = (words[0], words[words.len() - 1]);
        back.Unmap(0, None);
        pair
    }
}

struct TestSwitchGuard(Option<bool>);

impl TestSwitchGuard {
    fn set(val: bool) -> Self {
        Self(TEST_ON.replace(Some(val)))
    }
}

impl Drop for TestSwitchGuard {
    fn drop(&mut self) {
        TEST_ON.set(self.0);
    }
}

#[test]
fn off_uploads_stale_title_memory_while_on_preserves_gpu_written_pixels() {
    let Some(host) = crate::host::host() else {
        return; // no host GPU / WARP available
    };

    // Switch OFF: releasing a GPU-written texture and recreating at the same address
    // loses the GPU-written pixels and uploads stale/zero title memory.
    {
        let _off = TestSwitchGuard::set(false);
        let addr = title_memory(0);
        let t1 = create_texture_at(addr);
        gpu_bake_into(host, t1, BAKED_PIXEL);
        release(t1);

        let t2 = create_texture_at(addr);
        assert!(!crate::resource::host_written_mark(t2));
        let got = upload_and_readback(host, t2);
        assert_eq!(
            got,
            (0, 0),
            "switch OFF must upload stale/zero title memory"
        );
        release(t2);
    }

    // Switch ON: releasing a GPU-written texture and recreating at the same address
    // with untouched title memory adopts the retained host copy and preserves GPU pixels.
    {
        let _on = TestSwitchGuard::set(true);
        let addr = title_memory(0);
        let t1 = create_texture_at(addr);
        gpu_bake_into(host, t1, BAKED_PIXEL);
        release(t1);

        let t2 = create_texture_at(addr);
        assert!(
            crate::resource::host_written_mark(t2),
            "recreated texture should adopt retained GPU-written copy"
        );
        let got = upload_and_readback(host, t2);
        assert_eq!(
            got,
            (BAKED_PIXEL, BAKED_PIXEL),
            "switch ON must preserve the GPU-written pixels across release and recreate"
        );
        release(t2);
    }
}

#[test]
fn cpu_overwrite_between_release_and_recreate_uploads_fresh_title_memory() {
    let Some(host) = crate::host::host() else {
        return;
    };
    let _on = TestSwitchGuard::set(true);
    let addr = title_memory(0);
    let t1 = create_texture_at(addr);
    gpu_bake_into(host, t1, BAKED_PIXEL);
    release(t1);

    // Title CPU overwrites the backing memory with fresh texels before recreation.
    // SAFETY: `addr` points to a leaked 64 KiB aligned allocation of `BLOCK_BYTES`.
    unsafe {
        let words = std::slice::from_raw_parts_mut(addr as *mut u32, BLOCK_BYTES / 4);
        words.fill(FRESH_TITLE_PIXEL);
    }

    let t2 = create_texture_at(addr);
    assert!(
        !crate::resource::host_written_mark(t2),
        "CPU overwrite must drop the retained GPU copy instead of adopting it"
    );
    let got = upload_and_readback(host, t2);
    assert_eq!(
        got,
        (FRESH_TITLE_PIXEL, FRESH_TITLE_PIXEL),
        "recreated texture must upload fresh title memory after CPU overwrite"
    );
    release(t2);
}

#[test]
fn remap_between_release_and_recreate_invalidates_and_uploads_fresh_title_memory() {
    let Some(host) = crate::host::host() else {
        return;
    };
    let _on = TestSwitchGuard::set(true);
    let addr = title_memory(STALE_TITLE_PIXEL);
    let t1 = create_texture_at(addr);
    gpu_bake_into(host, t1, BAKED_PIXEL);
    release(t1);

    // Memory at `addr` is remapped before the new texture is created.
    crate::watch::title_memory_remapped(addr as usize, BLOCK_BYTES);

    let t2 = create_texture_at(addr);
    assert!(
        !crate::resource::host_written_mark(t2),
        "title_memory_remapped must immediately invalidate the retained GPU copy"
    );
    let got = upload_and_readback(host, t2);
    assert_eq!(
        got,
        (STALE_TITLE_PIXEL, STALE_TITLE_PIXEL),
        "recreated texture must upload title memory after remap invalidation"
    );
    release(t2);
}
