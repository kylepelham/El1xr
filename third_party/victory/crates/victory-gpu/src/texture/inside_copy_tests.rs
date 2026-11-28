//! (El1xr, #115) A view the title places at a layer of a render target
//! (`crate::resource::placed_inside`) reads that layer as the host drew
//! it, with `VICTORY_PLACED_INSIDE`; without, it is not linked.

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::Interface;

use crate::object::{S_OK, This, guid, slot};
use crate::resource::placed_inside::{TEST_ON, inside_parts};

const SIDE: u32 = 64;
const LAYERS: u32 = 4;
const RENDER_TARGET: u32 = 0x1;
/// `0x100 | SW_64KB_R_X`: the layout the library picks for the target.
const R_X: u32 = 0x100 | 27;
const LAYER_BYTES: u64 = (SIDE * SIDE * 4) as u64;

/// A 64x64 title texture description of `layers` layers in `format`.
fn words(layers: u32, format: u32, layout: u32, flags: u32) -> [u32; 14] {
    [
        3,
        0,
        0,
        0,
        SIDE,
        0,
        SIDE,
        layers | 1 << 16,
        format,
        1,
        0,
        layout,
        flags,
        0,
    ]
}

fn title_memory(bytes: usize) -> u64 {
    let layout = std::alloc::Layout::from_size_align(bytes, 1 << 16).unwrap();
    // SAFETY: a non-zero size; leaked, as title memory outlives a test.
    unsafe { std::alloc::alloc_zeroed(layout) as u64 }
}

fn target_at(address: u64) -> This {
    let w = words(LAYERS, 28, 0, RENDER_TARGET);
    let i = ID3D12Resource::IID;
    let iid = guid(i.data1, i.data2, i.data3, i.data4);
    let mut out = std::ptr::null();
    let made = crate::resource::create_at_address(
        std::ptr::null(),
        address,
        w.as_ptr().cast(),
        0,
        std::ptr::null(),
        &iid,
        &mut out,
    );
    assert_eq!(made, S_OK);
    out
}

/// Bytes into the target where its layer `layer` starts.
fn layer_offset(layer: u32) -> u64 {
    use victory_formats::texture::{Dimension, Swizzle, Texture, TextureDesc};
    let t = Texture::new(&TextureDesc {
        dimension: Dimension::Tex2d,
        width: SIDE,
        height: SIDE,
        depth_or_array: LAYERS,
        mips: 1,
        format: 28,
        samples: 1,
        swizzle: Swizzle::SW_64KB_R_X,
    })
    .unwrap();
    u64::from(layer) * t.planes[0].layout.slice_size
}

fn buffer(
    host: &crate::host::Host,
    kind: D3D12_HEAP_TYPE,
    state: D3D12_RESOURCE_STATES,
) -> ID3D12Resource {
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: LAYER_BYTES * u64::from(LAYERS),
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
    // SAFETY: a plain buffer.
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

fn rows(buffer: &ID3D12Resource, layer: u32, format: DXGI_FORMAT) -> D3D12_TEXTURE_COPY_LOCATION {
    D3D12_TEXTURE_COPY_LOCATION {
        // SAFETY: a borrowed pointer the location never releases.
        pResource: unsafe { std::mem::transmute_copy(buffer) },
        Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
            PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                Offset: u64::from(layer) * LAYER_BYTES,
                Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                    Format: format,
                    Width: SIDE,
                    Height: SIDE,
                    Depth: 1,
                    RowPitch: SIDE * 4,
                },
            },
        },
    }
}

fn part(texture: &ID3D12Resource, index: u32) -> D3D12_TEXTURE_COPY_LOCATION {
    D3D12_TEXTURE_COPY_LOCATION {
        // SAFETY: as above.
        pResource: unsafe { std::mem::transmute_copy(texture) },
        Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
            SubresourceIndex: index,
        },
    }
}

fn pattern(layer: u32) -> u32 {
    0x1020_3040 ^ layer.wrapping_mul(0x0101_0101) ^ 0x8000_0000
}

/// Layer 2 read through an RGBA8 view (a direct copy) and layer 3 through
/// an R32_UINT view (through a buffer) show what the host wrote to those
/// layers of the target.
#[test]
fn views_at_a_targets_layers_read_those_layers() {
    let Some(host) = crate::host::host() else {
        return; // no host GPU here
    };
    let at = title_memory(1 << 20);
    let was = TEST_ON.replace(Some(true));
    let target = target_at(at);
    let views = [(2u32, 28u32), (3, 42)].map(|(layer, format)| {
        let w = words(1, format, R_X, 0);
        let placed = crate::resource::texture_at(at + layer_offset(layer), &w).unwrap();
        (layer, format, w, placed)
    });
    for &(layer, _, _, placed) in &views {
        assert_eq!(
            crate::resource::alias_of(placed).map(|h| h.get()),
            Some(target),
            "the view at layer {layer} finds the target"
        );
        assert_eq!(inside_parts(placed, target).as_deref(), Some(&[layer][..]));
    }

    let upload = buffer(
        host,
        D3D12_HEAP_TYPE_UPLOAD,
        D3D12_RESOURCE_STATE_GENERIC_READ,
    );
    let back = buffer(
        host,
        D3D12_HEAP_TYPE_READBACK,
        D3D12_RESOURCE_STATE_COPY_DEST,
    );
    // SAFETY: an upload buffer of every layer's words.
    unsafe {
        let mut ptr = std::ptr::null_mut();
        upload.Map(0, None, Some(&mut ptr)).unwrap();
        let all = std::slice::from_raw_parts_mut(
            ptr.cast::<u32>(),
            (LAYER_BYTES / 4 * u64::from(LAYERS)) as usize,
        );
        for (layer, words) in all.chunks_mut((LAYER_BYTES / 4) as usize).enumerate() {
            words.fill(pattern(layer as u32));
        }
        upload.Unmap(0, None);
    }
    let texture = crate::resource::host_resource(target).unwrap();
    let list = host.new_list().unwrap();
    crate::list::ensure_state(&list, target, 0x400); // COPY_DEST
    for layer in 0..LAYERS {
        // SAFETY: an open list; the target in the copy destination state.
        unsafe {
            list.list.CopyTextureRegion(
                &part(texture, layer),
                0,
                0,
                0,
                &rows(&upload, layer, DXGI_FORMAT_R8G8B8A8_UNORM),
                None,
            )
        };
    }
    crate::list::ensure_state(&list, target, 0x4); // RENDER_TARGET
    for (slot, &(_, format, _, placed)) in (0u32..).zip(&views) {
        assert!(crate::texture::alias(&list, placed, target));
        crate::list::ensure_state(&list, placed, 0x800); // COPY_SOURCE
        let read = crate::resource::host_resource(placed).unwrap();
        // SAFETY: an open list; the view's texture in the copy source state.
        unsafe {
            list.list.CopyTextureRegion(
                &rows(&back, slot, DXGI_FORMAT(format as i32)),
                0,
                0,
                0,
                &part(read, 0),
                None,
            )
        };
    }
    list.close();
    host.execute(&[&list]);
    // SAFETY: the list ran; the read-back buffer holds every layer's room.
    let got = unsafe {
        let mut ptr = std::ptr::null_mut();
        back.Map(0, None, Some(&mut ptr)).unwrap();
        let words = ptr.cast::<u32>();
        let got = [0usize, 1].map(|slot| {
            let first = *words.add(slot * (LAYER_BYTES / 4) as usize);
            let last =
                *words.add(slot * (LAYER_BYTES / 4) as usize + (LAYER_BYTES / 4) as usize - 1);
            (first, last)
        });
        back.Unmap(0, None);
        got
    };
    assert_eq!(
        got[0],
        (pattern(2), pattern(2)),
        "the RGBA8 view shows layer 2"
    );
    assert_eq!(
        got[1],
        (pattern(3), pattern(3)),
        "the R32_UINT view shows layer 3"
    );

    TEST_ON.set(Some(false));
    for &(_, _, _, placed) in &views {
        assert!(
            crate::resource::alias_of(placed).is_none(),
            "not linked without the switch"
        );
        assert!(inside_parts(placed, target).is_none());
    }
    TEST_ON.set(was);
    list.reset();
    for &(layer, _, w, _) in &views {
        crate::resource::release_test_placed_texture(at + layer_offset(layer), w);
    }
    let _ = slot::<extern "system" fn(This) -> u32>(target, 0x10)(target);
}

/// (`VICTORY_ALIAS_COMPATIBLE`) A UNORM view of seven levels placed at the
/// start of a one-level SRGB target (the phone's video target) reads the
/// target's level 0 as its own level 0; without the switch it is not linked.
#[test]
fn a_mip_chain_at_a_one_level_targets_start_reads_its_top_level() {
    use crate::resource::alias_compatible::{NONE, TEST_ON as COMPATIBLE, copy_parts};
    let Some(host) = crate::host::host() else {
        return; // no host GPU here
    };
    let at = title_memory(1 << 20);
    let was = COMPATIBLE.replace(Some(true));
    let mut w = words(1, 29, 0, RENDER_TARGET);
    let i = ID3D12Resource::IID;
    let iid = guid(i.data1, i.data2, i.data3, i.data4);
    let mut target = std::ptr::null();
    let made = crate::resource::create_at_address(
        std::ptr::null(),
        at,
        w.as_ptr().cast(),
        0,
        std::ptr::null(),
        &iid,
        &mut target,
    );
    assert_eq!(made, S_OK);
    w = words(1, 28, 0, 0);
    w[7] = 1 | 7 << 16;
    let placed = crate::resource::texture_at(at, &w).unwrap();
    assert_eq!(crate::resource::alias_of(placed).map(|h| h.get()), Some(target));
    let mut want = vec![NONE; 7];
    want[0] = 0;
    assert_eq!(copy_parts(placed, target).as_deref(), Some(&want[..]));

    let upload = buffer(host, D3D12_HEAP_TYPE_UPLOAD, D3D12_RESOURCE_STATE_GENERIC_READ);
    let back = buffer(host, D3D12_HEAP_TYPE_READBACK, D3D12_RESOURCE_STATE_COPY_DEST);
    // SAFETY: an upload buffer of at least one layer's words.
    unsafe {
        let mut ptr = std::ptr::null_mut();
        upload.Map(0, None, Some(&mut ptr)).unwrap();
        std::slice::from_raw_parts_mut(ptr.cast::<u32>(), (LAYER_BYTES / 4) as usize).fill(pattern(5));
        upload.Unmap(0, None);
    }
    let texture = crate::resource::host_resource(target).unwrap();
    let list = host.new_list().unwrap();
    crate::list::ensure_state(&list, target, 0x400); // COPY_DEST
    // SAFETY: an open list; the target in the copy destination state.
    unsafe {
        list.list.CopyTextureRegion(
            &part(texture, 0),
            0,
            0,
            0,
            &rows(&upload, 0, DXGI_FORMAT_R8G8B8A8_UNORM_SRGB),
            None,
        )
    };
    crate::list::ensure_state(&list, target, 0x4); // RENDER_TARGET
    assert!(crate::texture::alias(&list, placed, target));
    crate::list::ensure_state(&list, placed, 0x800); // COPY_SOURCE
    let read = crate::resource::host_resource(placed).unwrap();
    // SAFETY: an open list; the view's texture in the copy source state.
    unsafe {
        list.list.CopyTextureRegion(
            &rows(&back, 0, DXGI_FORMAT_R8G8B8A8_UNORM),
            0,
            0,
            0,
            &part(read, 0),
            None,
        )
    };
    list.close();
    host.execute(&[&list]);
    // SAFETY: the list ran; the read-back buffer holds a layer's room.
    let got = unsafe {
        let mut ptr = std::ptr::null_mut();
        back.Map(0, None, Some(&mut ptr)).unwrap();
        let words = ptr.cast::<u32>();
        let got = (*words, *words.add((LAYER_BYTES / 4) as usize - 1));
        back.Unmap(0, None);
        got
    };
    assert_eq!(got, (pattern(5), pattern(5)), "level 0 shows the target");

    COMPATIBLE.set(Some(false));
    assert!(crate::resource::alias_of(placed).is_none(), "not linked without the switch");
    assert!(copy_parts(placed, target).is_none());
    COMPATIBLE.set(was);
    list.reset();
    crate::resource::release_test_placed_texture(at, w);
    let _ = slot::<extern "system" fn(This) -> u32>(target, 0x10)(target);
}
