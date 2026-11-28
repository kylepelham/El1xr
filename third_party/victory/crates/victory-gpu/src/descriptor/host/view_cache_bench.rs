//! (entryhitch, #16) What a fresh table's slot costs made and copied: the
//! driver's view calls against `CopyDescriptorsSimple`, and the view
//! cache's whole hit path against `make`. CPU-only descriptor writes on the
//! host device (nothing submitted). Run alone:
//! `cargo test -p victory-gpu --release --lib view_cache::bench -- --ignored --nocapture`.

use super::*;
use windows::Win32::Graphics::Dxgi::Common::*;

const SLOTS: u32 = 4096;
const ROUNDS: u32 = 200_000;

fn cpu_heap(device: &ID3D12Device) -> (ID3D12DescriptorHeap, usize, usize) {
    // SAFETY: plain object creation and queries; a heap the card does not see.
    unsafe {
        let heap: ID3D12DescriptorHeap = device
            .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                NumDescriptors: SLOTS,
                ..Default::default()
            })
            .unwrap();
        let cpu = heap.GetCPUDescriptorHandleForHeapStart().ptr;
        let step = device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV)
            as usize;
        (heap, cpu, step)
    }
}

fn committed(device: &ID3D12Device, desc: &D3D12_RESOURCE_DESC) -> ID3D12Resource {
    let heap = D3D12_HEAP_PROPERTIES {
        Type: D3D12_HEAP_TYPE_DEFAULT,
        ..Default::default()
    };
    let mut made: Option<ID3D12Resource> = None;
    // SAFETY: a valid description and output.
    unsafe {
        device
            .CreateCommittedResource(
                &heap,
                D3D12_HEAP_FLAG_NONE,
                desc,
                D3D12_RESOURCE_STATE_COMMON,
                None,
                &mut made,
            )
            .unwrap();
    }
    made.unwrap()
}

/// Median ns per call of `f(slot)` over 9 timed passes.
fn ns_per(mut f: impl FnMut(D3D12_CPU_DESCRIPTOR_HANDLE), cpu: usize, step: usize) -> f64 {
    let mut passes = Vec::new();
    for pass in 0..10 {
        let started = std::time::Instant::now();
        for i in 0..ROUNDS {
            f(D3D12_CPU_DESCRIPTOR_HANDLE {
                ptr: cpu + (i % SLOTS) as usize * step,
            });
        }
        if pass > 0 {
            passes.push(started.elapsed().as_nanos() as f64 / f64::from(ROUNDS));
        }
    }
    passes.sort_by(f64::total_cmp);
    passes[passes.len() / 2]
}

#[test]
#[ignore = "timing; run alone"]
fn table_slot_costs() {
    let Some(host) = crate::host::host() else {
        return;
    };
    let device = &host.device;
    let (_heap, cpu, step) = cpu_heap(device);
    let (_src_heap, src, _) = cpu_heap(device);
    let texture = committed(
        device,
        &D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Width: 1024,
            Height: 1024,
            DepthOrArraySize: 1,
            MipLevels: 11,
            Format: DXGI_FORMAT_BC1_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            ..Default::default()
        },
    );
    let buffer = committed(
        device,
        &D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: 1 << 20,
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
            ..Default::default()
        },
    );
    let tex_srv = D3D12_SHADER_RESOURCE_VIEW_DESC {
        Format: DXGI_FORMAT_BC1_UNORM,
        ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2D,
        Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
        Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
            Texture2D: D3D12_TEX2D_SRV {
                MostDetailedMip: 2,
                MipLevels: u32::MAX,
                PlaneSlice: 0,
                ResourceMinLODClamp: 0.0,
            },
        },
    };
    let buf_srv = |format, flags, first| D3D12_SHADER_RESOURCE_VIEW_DESC {
        Format: format,
        ViewDimension: D3D12_SRV_DIMENSION_BUFFER,
        Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
        Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
            Buffer: D3D12_BUFFER_SRV {
                FirstElement: first,
                NumElements: 4096,
                StructureByteStride: 0,
                Flags: flags,
            },
        },
    };
    let raw = buf_srv(DXGI_FORMAT_R32_TYPELESS, D3D12_BUFFER_SRV_FLAG_RAW, 64);
    let typed = buf_srv(
        DXGI_FORMAT_R32G32B32A32_FLOAT,
        D3D12_BUFFER_SRV_FLAG_NONE,
        16,
    );
    let uav = D3D12_UNORDERED_ACCESS_VIEW_DESC {
        Format: DXGI_FORMAT_R32_TYPELESS,
        ViewDimension: D3D12_UAV_DIMENSION_BUFFER,
        Anonymous: D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
            Buffer: D3D12_BUFFER_UAV {
                FirstElement: 0,
                NumElements: 4096,
                StructureByteStride: 0,
                CounterOffsetInBytes: 0,
                Flags: D3D12_BUFFER_UAV_FLAG_RAW,
            },
        },
    };
    // SAFETY: a live buffer.
    let gpu = unsafe { buffer.GetGPUVirtualAddress() };
    let null = D3D12_SHADER_RESOURCE_VIEW_DESC {
        Format: DXGI_FORMAT_R8G8B8A8_UNORM,
        ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2D,
        Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
        Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
            Texture2D: D3D12_TEX2D_SRV {
                MipLevels: 1,
                ..Default::default()
            },
        },
    };
    let src0 = D3D12_CPU_DESCRIPTOR_HANDLE { ptr: src };
    // SAFETY (every call below): live resources, slots of our CPU heaps.
    unsafe {
        device.CreateShaderResourceView(&texture, Some(&tex_srv), src0);
        let rows = [
            (
                "texture SRV (driver)",
                ns_per(
                    |at| device.CreateShaderResourceView(&texture, Some(&tex_srv), at),
                    cpu,
                    step,
                ),
            ),
            (
                "buffer SRV raw (driver)",
                ns_per(
                    |at| device.CreateShaderResourceView(&buffer, Some(&raw), at),
                    cpu,
                    step,
                ),
            ),
            (
                "buffer SRV typed (driver)",
                ns_per(
                    |at| device.CreateShaderResourceView(&buffer, Some(&typed), at),
                    cpu,
                    step,
                ),
            ),
            (
                "buffer UAV raw (driver)",
                ns_per(
                    |at| device.CreateUnorderedAccessView(&buffer, None, Some(&uav), at),
                    cpu,
                    step,
                ),
            ),
            (
                "CBV (driver)",
                ns_per(
                    |at| {
                        device.CreateConstantBufferView(
                            Some(&D3D12_CONSTANT_BUFFER_VIEW_DESC {
                                BufferLocation: gpu + 256,
                                SizeInBytes: 512,
                            }),
                            at,
                        )
                    },
                    cpu,
                    step,
                ),
            ),
            (
                "null SRV (driver)",
                ns_per(
                    |at| device.CreateShaderResourceView(None, Some(&null), at),
                    cpu,
                    step,
                ),
            ),
            (
                "CopyDescriptorsSimple(1)",
                ns_per(
                    |at| {
                        device.CopyDescriptorsSimple(
                            1,
                            at,
                            src0,
                            D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                        )
                    },
                    cpu,
                    step,
                ),
            ),
        ];
        for (name, ns) in rows {
            println!("TABLE_SLOT_BENCH,{name},{ns:.1}");
        }
        // Eight slots in one call, per slot.
        let eight = ns_per(
            |at| {
                let at = D3D12_CPU_DESCRIPTOR_HANDLE {
                    ptr: cpu + (at.ptr - cpu) / step / 8 * 8 * step,
                };
                device.CopyDescriptorsSimple(8, at, src0, D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV)
            },
            cpu,
            step,
        ) / 8.0;
        println!("TABLE_SLOT_BENCH,CopyDescriptorsSimple(8) per slot,{eight:.1}");
    }
    // Our paths: `make` of a title texture's view against the cache's hit.
    let Some(cache) = cache() else {
        return;
    };
    let to = D3D12_CPU_DESCRIPTOR_HANDLE { ptr: src };
    let image = crate::resource::test_lazy_texture();
    let view = View::Shader(image as usize, None);
    let key = [super::super::VIEW_TAG, 0x7c_0002, 78];
    cache.copy(&view, key, to).unwrap();
    let made = ns_per(|at| drop(make(&view, at)), cpu, step);
    let hit = ns_per(|at| drop(cache.copy(&view, key, at)), cpu, step);
    println!("TABLE_SLOT_BENCH,make texture view (ours),{made:.1}");
    println!("TABLE_SLOT_BENCH,view cache hit texture (ours),{hit:.1}");
    let title = super::tests::TitleBuffer::new(0x7e57_be01);
    let (view, key) = title.view();
    cache.copy_buffer(&view, key, to).unwrap();
    let made = ns_per(|at| drop(make(&view, at)), cpu, step);
    let hit = ns_per(|at| drop(cache.copy_buffer(&view, key, at)), cpu, step);
    println!("TABLE_SLOT_BENCH,make placed buffer view (ours),{made:.1}");
    println!("TABLE_SLOT_BENCH,view cache hit placed buffer (ours),{hit:.1}");
    drop(title);
    crate::resource::test_release_texture(image);
}
