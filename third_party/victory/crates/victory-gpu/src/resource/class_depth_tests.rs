//! The tiled lighting's material-class depth test on the card, through the
//! real stand-in path: class numbers go into the colour twin of the depth
//! plane of a console 16-bit depth-stencil texture (host D24S8), the list's
//! close draws them into the plane (`HostList::pass_plane`), and one draw a
//! class then tests the plane at the depth the title's lighting computes
//! (the class times asfloat(0x37800080), w 1, as the translated
//! `VS_TiledLighting` writes it). Every class 0..1024 is drawn over a
//! 512x256 plane: the left half in whole 16x16 blocks of one class (the
//! blocks a coarse depth range covers), the right half a different class
//! almost every texel. The depth is cleared to 1.0 first, and each plane is
//! drawn twice with different classes, so the second pass finds the depth of
//! the first in whatever coarse ranges the card keeps. Each texel must be lit
//! once, by its own class.
//!
//! five-star-recomp (ThatRecompDev `4143737`, `6f670f2`, `74afbd8`) found
//! "equal" failing on desktop cards against a depth twin they copy into the
//! depth texture, and test "greater or equal" with a depth-bounds floor
//! instead. Here the plane is drawn (written as each pixel's depth), and
//! "equal" lights exactly each class's texels on an RTX 5090, also at
//! 3840x2160; their form is checked too, should it ever be needed.

use std::sync::atomic::Ordering::Relaxed;

use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_SAMPLE_DESC};
use windows::core::{Interface, s};

use super::{
    RESOURCE_IIDS, ResourceDesc, create_committed, host_copy, host_resource, twin_plane_resource,
};

const WIDTH: u32 = 512;
const HEIGHT: u32 = 256;
/// Classes drawn: every one under 1024 (the title's are well under 256).
const CLASSES: u32 = 1024;
const R16_TYPELESS: i32 = 53;
const R32_FLOAT: i32 = 41;
const D24S8: i32 = 45;
const PROPERTIES: super::HeapProperties = super::HeapProperties {
    kind: 1,
    cpu_pages: 0,
    pool: 0,
    creation_nodes: 0,
    visible_nodes: 0,
};

/// How a class draw tests the plane.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Test {
    /// The title's state: "equal", no writes, the depth in the write state.
    Equal,
    /// "equal" against a read-only depth view in the depth-read state.
    EqualReadOnly,
    /// five-star-recomp's form: "greater or equal" and the depth bounds
    /// test with the class's floor ([`floor_bounds`]).
    Floor,
}

/// The depth bounds of class `class` in five-star-recomp's form: half a
/// step (asfloat(0x37800080)) below its depth, up to 1.0. With "greater or
/// equal" the draw keeps the classes up to its own and the floor drops the
/// lower ones.
fn floor_bounds(class: u32) -> (f32, f32) {
    let step = f32::from_bits(0x3780_0080);
    ((class as f32 * step - 0.5 * step).max(0.0), 1.0)
}

/// The class of texel (x, y) in pass `second` of a plane.
fn class_at(x: u32, y: u32, second: bool) -> u32 {
    if x < 256 {
        let block = ((y / 16) * 16 + x / 16) % 256;
        if second { 255 - block } else { block }
    } else {
        let mixed = (x * 7 + y * 13 + (x * y) % 17) % CLASSES;
        if second {
            (mixed + 511) % CLASSES
        } else {
            mixed
        }
    }
}

fn committed(
    host: &crate::host::Host,
    kind: D3D12_HEAP_TYPE,
    desc: &D3D12_RESOURCE_DESC,
    state: D3D12_RESOURCE_STATES,
) -> ID3D12Resource {
    let heap = D3D12_HEAP_PROPERTIES {
        Type: kind,
        ..Default::default()
    };
    let mut made: Option<ID3D12Resource> = None;
    // SAFETY: plain creation.
    unsafe {
        host.device
            .CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, desc, state, None, &mut made)
            .unwrap();
    }
    made.unwrap()
}

fn buffer_desc(bytes: u64) -> D3D12_RESOURCE_DESC {
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

fn footprint(format: i32, pitch: u32) -> D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
    D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
        Offset: 0,
        Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
            Format: DXGI_FORMAT(format),
            Width: WIDTH,
            Height: HEIGHT,
            Depth: 1,
            RowPitch: pitch,
        },
    }
}

fn whole(texture: &ID3D12Resource) -> D3D12_TEXTURE_COPY_LOCATION {
    D3D12_TEXTURE_COPY_LOCATION {
        // SAFETY: a borrowed pointer the location never releases.
        pResource: unsafe { std::mem::transmute_copy(texture) },
        Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
            SubresourceIndex: 0,
        },
    }
}

fn rows(
    buffer: &ID3D12Resource,
    placed: D3D12_PLACED_SUBRESOURCE_FOOTPRINT,
) -> D3D12_TEXTURE_COPY_LOCATION {
    D3D12_TEXTURE_COPY_LOCATION {
        // SAFETY: as above.
        pResource: unsafe { std::mem::transmute_copy(buffer) },
        Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
            PlacedFootprint: placed,
        },
    }
}

const SOURCE: &str = r#"
#define RS "RootConstants(num32BitConstants = 1, b0)"
cbuffer Drawn : register(b0) { uint number; };
[RootSignature(RS)]
float4 corner(uint i : SV_VertexID) : SV_Position {
    float z = float(number) * asfloat(0x37800080);
    return float4(float(i & 1) * 4.0 - 1.0, float(i >> 1) * 4.0 - 1.0, z, 1.0);
}
struct Lit { float count : SV_Target0; float number : SV_Target1; };
Lit lit() {
    Lit o;
    o.count = 1.0;
    o.number = float(number + 1);
    return o;
}
"#;

/// A stream subobject: its type, then its description, pointer-aligned.
#[repr(C, align(8))]
struct Sub<T>(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE, T);

/// The class draws' root signature and pipeline for `test`: two R32 float
/// targets, a count added up and the class plus one written.
fn class_pipeline(
    host: &crate::host::Host,
    test: Test,
) -> (ID3D12RootSignature, ID3D12PipelineState) {
    let vs = crate::host::compile(SOURCE, s!("corner"), s!("vs_5_1")).unwrap();
    let ps = crate::host::compile(SOURCE, s!("lit"), s!("ps_5_1")).unwrap();
    let code = |bytes: &[u8]| D3D12_SHADER_BYTECODE {
        pShaderBytecode: bytes.as_ptr().cast(),
        BytecodeLength: bytes.len(),
    };
    let mut blend = D3D12_BLEND_DESC {
        IndependentBlendEnable: true.into(),
        ..Default::default()
    };
    blend.RenderTarget[0] = D3D12_RENDER_TARGET_BLEND_DESC {
        BlendEnable: true.into(),
        SrcBlend: D3D12_BLEND_ONE,
        DestBlend: D3D12_BLEND_ONE,
        BlendOp: D3D12_BLEND_OP_ADD,
        SrcBlendAlpha: D3D12_BLEND_ONE,
        DestBlendAlpha: D3D12_BLEND_ONE,
        BlendOpAlpha: D3D12_BLEND_OP_ADD,
        RenderTargetWriteMask: 0xf,
        ..Default::default()
    };
    blend.RenderTarget[1].RenderTargetWriteMask = 0xf;
    let mut formats = [DXGI_FORMAT(0); 8];
    formats[..2].fill(DXGI_FORMAT(R32_FLOAT));
    let floor = test == Test::Floor;
    let depth = D3D12_DEPTH_STENCIL_DESC1 {
        DepthEnable: true.into(),
        DepthWriteMask: D3D12_DEPTH_WRITE_MASK_ZERO,
        DepthFunc: if floor {
            D3D12_COMPARISON_FUNC_GREATER_EQUAL
        } else {
            D3D12_COMPARISON_FUNC_EQUAL
        },
        DepthBoundsTestEnable: floor.into(),
        ..Default::default()
    };
    // SAFETY: plain creation; the stream points at live data.
    unsafe {
        let root: ID3D12RootSignature = host.device.CreateRootSignature(0, &vs[..]).unwrap();
        #[repr(C)]
        struct Stream {
            root: Sub<*mut std::ffi::c_void>,
            vs: Sub<D3D12_SHADER_BYTECODE>,
            ps: Sub<D3D12_SHADER_BYTECODE>,
            blend: Sub<D3D12_BLEND_DESC>,
            mask: Sub<u32>,
            raster: Sub<D3D12_RASTERIZER_DESC>,
            depth: Sub<D3D12_DEPTH_STENCIL_DESC1>,
            topology: Sub<D3D12_PRIMITIVE_TOPOLOGY_TYPE>,
            targets: Sub<D3D12_RT_FORMAT_ARRAY>,
            dsv: Sub<DXGI_FORMAT>,
            samples: Sub<DXGI_SAMPLE_DESC>,
        }
        let mut stream = Stream {
            root: Sub(
                D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_ROOT_SIGNATURE,
                root.as_raw(),
            ),
            vs: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_VS, code(&vs)),
            ps: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_PS, code(&ps)),
            blend: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_BLEND, blend),
            mask: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_SAMPLE_MASK, u32::MAX),
            raster: Sub(
                D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_RASTERIZER,
                D3D12_RASTERIZER_DESC {
                    FillMode: D3D12_FILL_MODE_SOLID,
                    CullMode: D3D12_CULL_MODE_NONE,
                    DepthClipEnable: true.into(),
                    ..Default::default()
                },
            ),
            depth: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DEPTH_STENCIL1, depth),
            topology: Sub(
                D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_PRIMITIVE_TOPOLOGY,
                D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
            ),
            targets: Sub(
                D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_RENDER_TARGET_FORMATS,
                D3D12_RT_FORMAT_ARRAY {
                    RTFormats: formats,
                    NumRenderTargets: 2,
                },
            ),
            dsv: Sub(
                D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DEPTH_STENCIL_FORMAT,
                DXGI_FORMAT(D24S8),
            ),
            samples: Sub(
                D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_SAMPLE_DESC,
                DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
            ),
        };
        let desc = D3D12_PIPELINE_STATE_STREAM_DESC {
            SizeInBytes: size_of::<Stream>(),
            pPipelineStateSubobjectStream: std::ptr::from_mut(&mut stream).cast(),
        };
        let device = host.device.cast::<ID3D12Device2>().unwrap();
        let pipeline: ID3D12PipelineState = device.CreatePipelineState(&desc).unwrap();
        (root, pipeline)
    }
}

/// Texels lit other than once by their own class, as (x, y, class, times
/// lit, class + 1 written), after drawing pass `second`'s classes into the
/// plane of `depth` and testing every class against it with `test`.
fn wrong_texels(
    host: &crate::host::Host,
    depth: super::This,
    test: Test,
    second: bool,
) -> Vec<(u32, u32, u32, f32, f32)> {
    let host_depth = host_resource(depth).unwrap();
    let plane = twin_plane_resource(depth).unwrap();
    // The class numbers into the twin, where the title's shader writes them.
    let pitch16 = WIDTH * 2;
    let bytes16 = u64::from(pitch16 * HEIGHT);
    let (upload, data) = host.upload_buffer(bytes16).unwrap();
    let mut pattern = vec![0u8; bytes16 as usize];
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            let at = (y * pitch16 + x * 2) as usize;
            pattern[at..at + 2].copy_from_slice(&(class_at(x, y, second) as u16).to_le_bytes());
        }
    }
    // SAFETY: the buffer's mapping, `bytes16` bytes.
    unsafe { std::ptr::copy_nonoverlapping(pattern.as_ptr(), data, pattern.len()) };
    let moved = crate::host::transition;
    let (written, dest) = (
        D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
        D3D12_RESOURCE_STATE_COPY_DEST,
    );
    let fill = host.new_list().unwrap();
    // SAFETY: an open list and live resources.
    unsafe {
        let l = &fill.list;
        l.ResourceBarrier(&[moved(plane, written, dest)]);
        l.CopyTextureRegion(
            &whole(plane),
            0,
            0,
            0,
            &rows(&upload, footprint(R16_TYPELESS, pitch16)),
            None,
        );
        l.ResourceBarrier(&[moved(plane, dest, written)]);
    }
    fill.close();
    host.execute(&[&fill]);
    // A list in which a shader writes the twins: its close draws the depth
    // plane's twin into the plane, as for the title's material masks.
    let list = host.new_list().unwrap();
    assert!(list.twin_use(depth, true, true));
    list.twins_written();
    list.close();
    host.execute(&[&list]);

    let target_desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
        Width: u64::from(WIDTH),
        Height: HEIGHT,
        DepthOrArraySize: 1,
        MipLevels: 1,
        Format: DXGI_FORMAT(R32_FLOAT),
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Flags: D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
        ..Default::default()
    };
    let drawn = D3D12_RESOURCE_STATE_RENDER_TARGET;
    let targets = [0, 1].map(|_| committed(host, D3D12_HEAP_TYPE_DEFAULT, &target_desc, drawn));
    let pitch32 = WIDTH * 4;
    let bytes32 = u64::from(pitch32 * HEIGHT);
    let readbacks =
        [0, 1].map(|_| committed(host, D3D12_HEAP_TYPE_READBACK, &buffer_desc(bytes32), dest));
    let (signature, pipeline) = class_pipeline(host, test);
    let tested = if test == Test::EqualReadOnly {
        D3D12_RESOURCE_STATE_DEPTH_READ
    } else {
        D3D12_RESOURCE_STATE_DEPTH_WRITE
    };
    let held = host_copy(depth).unwrap();
    let state = D3D12_RESOURCE_STATES(held.state.load(Relaxed) as i32);
    let back = host.new_list().unwrap();
    // SAFETY: an open list and live resources and views.
    unsafe {
        let l = &back.list;
        if state != tested {
            l.ResourceBarrier(&[moved(host_depth, state, tested)]);
        }
        host.output_views(|views| {
            let colours = [
                views.rtv(Some(&targets[0]), None),
                views.rtv(Some(&targets[1]), None),
            ];
            let flags = if test == Test::EqualReadOnly {
                D3D12_DSV_FLAG_READ_ONLY_DEPTH | D3D12_DSV_FLAG_READ_ONLY_STENCIL
            } else {
                D3D12_DSV_FLAG_NONE
            };
            let depth_view = views.dsv(
                host_depth,
                Some(&D3D12_DEPTH_STENCIL_VIEW_DESC {
                    Format: DXGI_FORMAT(D24S8),
                    ViewDimension: D3D12_DSV_DIMENSION_TEXTURE2D,
                    Flags: flags,
                    ..Default::default()
                }),
            );
            for colour in colours {
                l.ClearRenderTargetView(colour, &[0.0; 4], None);
            }
            l.SetGraphicsRootSignature(&signature);
            l.SetPipelineState(&pipeline);
            l.OMSetRenderTargets(2, Some(colours.as_ptr()), false, Some(&depth_view));
        });
        l.RSSetViewports(&[D3D12_VIEWPORT {
            TopLeftX: 0.0,
            TopLeftY: 0.0,
            Width: WIDTH as f32,
            Height: HEIGHT as f32,
            MinDepth: 0.0,
            MaxDepth: 1.0,
        }]);
        l.RSSetScissorRects(&[RECT {
            left: 0,
            top: 0,
            right: WIDTH as i32,
            bottom: HEIGHT as i32,
        }]);
        l.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY(4));
        let bounds = l.cast::<ID3D12GraphicsCommandList1>().unwrap();
        for class in 0..CLASSES {
            l.SetGraphicsRoot32BitConstant(0, class, 0);
            if test == Test::Floor {
                let (min, max) = floor_bounds(class);
                bounds.OMSetDepthBounds(min, max);
            }
            l.DrawInstanced(3, 1, 0, 0);
        }
        for (target, readback) in targets.iter().zip(&readbacks) {
            l.ResourceBarrier(&[moved(target, drawn, D3D12_RESOURCE_STATE_COPY_SOURCE)]);
            l.CopyTextureRegion(
                &rows(readback, footprint(R32_FLOAT, pitch32)),
                0,
                0,
                0,
                &whole(target),
                None,
            );
        }
        // The depth back in the state the host keeps for it.
        if state != tested {
            l.ResourceBarrier(&[moved(host_depth, tested, state)]);
        }
    }
    back.close();
    host.execute(&[&back]);
    let read = |readback: &ID3D12Resource| -> Vec<f32> {
        let mut mapped = std::ptr::null_mut();
        // SAFETY: the copy finished (execute waits); the buffer holds `bytes32` bytes.
        unsafe {
            readback.Map(0, None, Some(&mut mapped)).unwrap();
            let floats =
                std::slice::from_raw_parts(mapped.cast::<f32>(), (bytes32 / 4) as usize).to_vec();
            readback.Unmap(0, None);
            floats
        }
    };
    let (count, number) = (read(&readbacks[0]), read(&readbacks[1]));
    let mut wrong = Vec::new();
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            let at = (y * WIDTH + x) as usize;
            let class = class_at(x, y, second);
            if count[at] != 1.0 || number[at] != (class + 1) as f32 {
                wrong.push((x, y, class, count[at], number[at]));
            }
        }
    }
    wrong
}

/// Clears the depth of `depth` to 1.0, as the title clears it before a
/// frame's material masks (a fast clear the card may keep compressed).
fn clear(host: &crate::host::Host, depth: super::This) {
    let host_depth = host_resource(depth).unwrap();
    let held = host_copy(depth).unwrap();
    let state = D3D12_RESOURCE_STATES(held.state.load(Relaxed) as i32);
    let write = D3D12_RESOURCE_STATE_DEPTH_WRITE;
    let moved = crate::host::transition;
    let list = host.new_list().unwrap();
    // SAFETY: an open list and a live resource.
    unsafe {
        let l = &list.list;
        if state != write {
            l.ResourceBarrier(&[moved(host_depth, state, write)]);
        }
        host.output_views(|views| {
            let view = views.dsv(
                host_depth,
                Some(&D3D12_DEPTH_STENCIL_VIEW_DESC {
                    Format: DXGI_FORMAT(D24S8),
                    ViewDimension: D3D12_DSV_DIMENSION_TEXTURE2D,
                    ..Default::default()
                }),
            );
            l.ClearDepthStencilView(
                view,
                D3D12_CLEAR_FLAG_DEPTH | D3D12_CLEAR_FLAG_STENCIL,
                1.0,
                0,
                None,
            );
        });
        if state != write {
            l.ResourceBarrier(&[moved(host_depth, write, state)]);
        }
    }
    list.close();
    host.execute(&[&list]);
}

/// Runs `test` over two planes in a row and fails naming the texels lit
/// wrongly, by class.
fn every_class_lights_its_own_texels(test: Test) {
    let Some(host) = crate::host::host() else {
        return; // no host GPU here
    };
    if test == Test::Floor && !host.depth_bounds() {
        return; // no depth bounds test on this card
    }
    let null = std::ptr::null();
    const DEPTH_WRITE: u32 = 0x10;
    const D16S8_CONSOLE: u32 = 118;
    let desc = ResourceDesc {
        dimension: 3,
        alignment: 0,
        width: u64::from(WIDTH),
        height: HEIGHT,
        depth_or_array_size: 1,
        mip_levels: 1,
        format: D16S8_CONSOLE,
        sample_count: 1,
        sample_quality: 0,
        layout: 0,
        flags: 6,
    };
    let mut depth = null;
    assert_eq!(
        create_committed(
            null,
            &PROPERTIES,
            0,
            &desc,
            DEPTH_WRITE,
            null.cast(),
            &RESOURCE_IIDS[0],
            &mut depth
        ),
        super::S_OK
    );
    if twin_plane_resource(depth).is_none() {
        return; // these twins are turned off
    }
    clear(host, depth);
    for second in [false, true] {
        let wrong = wrong_texels(host, depth, test, second);
        let mut classes: Vec<u32> = wrong.iter().map(|w| w.2).collect();
        classes.sort_unstable();
        classes.dedup();
        assert!(
            wrong.is_empty(),
            "{test:?} pass {}: {} of {} texels lit wrongly, classes {:?}, first {:?}",
            u8::from(second) + 1,
            wrong.len(),
            WIDTH * HEIGHT,
            &classes[..classes.len().min(32)],
            &wrong[..wrong.len().min(8)]
        );
    }
}

#[test]
fn every_class_lights_its_own_texels_with_equal() {
    every_class_lights_its_own_texels(Test::Equal);
}

#[test]
fn every_class_lights_its_own_texels_with_equal_read_only() {
    every_class_lights_its_own_texels(Test::EqualReadOnly);
}

#[test]
fn every_class_lights_its_own_texels_with_the_floor() {
    every_class_lights_its_own_texels(Test::Floor);
}
