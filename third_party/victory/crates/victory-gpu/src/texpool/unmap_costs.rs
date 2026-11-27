//! What the pools' memory does to the title's unmaps (batch 1: with the
//! five pool switches on, each `UnmapViewOfFile2` of title memory took
//! 0.5-0.93 ms instead of 0.07-0.18). Views of a pagefile section are
//! mapped into placeholders and unmapped as `xmem` does, while busy threads
//! run (as the title's do), before and after each kind of block is made.
//! `cargo test -p victory-gpu unmap_costs -- --ignored --nocapture --test-threads=1`.

use std::sync::atomic::{AtomicBool, Ordering};

use windows::Win32::Graphics::Direct3D12::*;
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, MEM_PRESERVE_PLACEHOLDER, MEM_RELEASE, MEM_REPLACE_PLACEHOLDER,
    MEM_RESERVE, MEM_RESERVE_PLACEHOLDER, MapViewOfFile3, PAGE_NOACCESS, PAGE_READWRITE,
    UnmapViewOfFile2, VirtualAlloc2, VirtualFree,
};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

const VIEW: usize = 64 << 10;
const VIEWS: usize = 1024;

/// Microseconds an unmap of a 64 KiB view takes, on average over `rounds`
/// maps and unmaps of `VIEWS` placeholders (each view touched once, as the
/// title touches its pages).
fn unmap_us(rounds: usize) -> f64 {
    unmap_us_with(rounds, None)
}

/// [`unmap_us`]; with a device, the section is also mapped whole at an
/// address of ours and opened there as a heap, as the renderer port opens
/// the title's (`crate::port`).
fn unmap_us_with(rounds: usize, port: Option<&ID3D12Device>) -> f64 {
    // SAFETY: a fresh placeholder and section of our own, split into views,
    // all given back at the end.
    unsafe {
        let process = GetCurrentProcess();
        let p = VirtualAlloc2(
            process,
            std::ptr::null(),
            VIEWS * VIEW,
            MEM_RESERVE | MEM_RESERVE_PLACEHOLDER,
            PAGE_NOACCESS,
            std::ptr::null_mut(),
            0,
        ) as usize;
        assert_ne!(p, 0);
        for i in 0..VIEWS - 1 {
            VirtualFree(
                (p + i * VIEW) as *mut _,
                VIEW,
                MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER,
            );
        }
        let size = (VIEWS * VIEW) as u64;
        let section = CreateFileMappingW(
            -1isize as _,
            std::ptr::null(),
            PAGE_READWRITE,
            (size >> 32) as u32,
            size as u32,
            std::ptr::null(),
        );
        assert!(!section.is_null());
        let ours = port.map(|device| {
            let view = windows_sys::Win32::System::Memory::MapViewOfFile(
                section,
                windows_sys::Win32::System::Memory::FILE_MAP_WRITE,
                0,
                0,
                size as usize,
            );
            assert!(!view.Value.is_null());
            let heap = crate::inplace::open(device, crate::inplace::From::Part(view.Value as usize, size as usize))
                .expect("a heap opened on our view");
            (view, heap)
        });
        let mut took = std::time::Duration::ZERO;
        let mut n = 0u32;
        for round in 0..rounds {
            let mut views = Vec::with_capacity(VIEWS);
            for i in 0..VIEWS {
                // Another part of the section each round, as remaps do.
                let from = (((i + round * 7) % VIEWS) * VIEW) as u64;
                let view = MapViewOfFile3(
                    section,
                    process,
                    (p + i * VIEW) as *const _,
                    from,
                    VIEW,
                    MEM_REPLACE_PLACEHOLDER,
                    PAGE_READWRITE,
                    std::ptr::null_mut(),
                    0,
                );
                assert_eq!(view.Value as usize, p + i * VIEW);
                (view.Value as *mut u8).write_volatile(1);
                views.push(view);
            }
            for view in views {
                let began = std::time::Instant::now();
                assert_ne!(UnmapViewOfFile2(process, view, MEM_PRESERVE_PLACEHOLDER), 0);
                took += began.elapsed();
                n += 1;
            }
        }
        if let Some((view, heap)) = ours {
            drop(heap);
            windows_sys::Win32::System::Memory::UnmapViewOfFile(view);
        }
        for i in 0..VIEWS {
            VirtualFree((p + i * VIEW) as *mut _, 0, MEM_RELEASE);
        }
        windows_sys::Win32::Foundation::CloseHandle(section);
        took.as_secs_f64() * 1e6 / f64::from(n)
    }
}

fn heap(
    device: &ID3D12Device,
    props: D3D12_HEAP_PROPERTIES,
    flags: D3D12_HEAP_FLAGS,
) -> ID3D12Heap {
    let desc = D3D12_HEAP_DESC {
        SizeInBytes: super::BLOCK,
        Properties: props,
        Alignment: 0,
        Flags: flags,
    };
    let mut heap = None;
    // SAFETY: plain creation.
    unsafe { device.CreateHeap(&desc, &mut heap) }.expect("heap");
    heap.unwrap()
}

fn default() -> D3D12_HEAP_PROPERTIES {
    D3D12_HEAP_PROPERTIES {
        Type: D3D12_HEAP_TYPE_DEFAULT,
        ..Default::default()
    }
}

fn upload() -> D3D12_HEAP_PROPERTIES {
    D3D12_HEAP_PROPERTIES {
        Type: D3D12_HEAP_TYPE_UPLOAD,
        ..Default::default()
    }
}

fn write_back() -> D3D12_HEAP_PROPERTIES {
    D3D12_HEAP_PROPERTIES {
        Type: D3D12_HEAP_TYPE_CUSTOM,
        CPUPageProperty: D3D12_CPU_PAGE_PROPERTY_WRITE_BACK,
        MemoryPoolPreference: D3D12_MEMORY_POOL_L0,
        ..Default::default()
    }
}

/// A buffer over all of `heap`, mapped and written through, as the pools'
/// CPU blocks are.
fn mapped_whole(
    device: &ID3D12Device,
    heap: &ID3D12Heap,
    state: D3D12_RESOURCE_STATES,
) -> ID3D12Resource {
    let desc = super::buffers::buffer_desc(super::BLOCK, D3D12_RESOURCE_FLAG_NONE);
    let mut out: Option<ID3D12Resource> = None;
    // SAFETY: a live heap; the mapping covers the block, ours alone.
    unsafe {
        device
            .CreatePlacedResource(heap, 0, &desc, state, None, &mut out)
            .expect("placed");
        let r = out.unwrap();
        let mut data = std::ptr::null_mut();
        r.Map(0, None, Some(&mut data)).unwrap();
        std::ptr::write_bytes(data.cast::<u8>(), 0, super::BLOCK as usize);
        r
    }
}

#[test]
#[ignore = "a measurement, not a check"]
fn unmap_costs() {
    let Some(host) = crate::host::host() else {
        return;
    };
    let device = &host.device;
    // Busy threads, as the title's are: an unmap's flush reaches every
    // processor one of them runs on.
    let stop = AtomicBool::new(false);
    std::thread::scope(|s| {
        for _ in 0..16 {
            s.spawn(|| {
                let mut x = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    x = std::hint::black_box(x.wrapping_mul(6364136223846793005).wrapping_add(1));
                }
            });
        }
        let say = |what: &str| println!("{what}: {:.1} us an unmap", unmap_us(4));
        say("nothing made");
        type Made = Box<dyn std::any::Any>;
        let phases: Vec<(&str, Box<dyn Fn() -> Made>)> = vec![
            (
                "64 texture blocks, resident when made",
                Box::new(|| {
                    Box::new(
                        (0..64)
                            .map(|_| {
                                heap(
                                    device,
                                    default(),
                                    D3D12_HEAP_FLAG_ALLOW_ONLY_NON_RT_DS_TEXTURES,
                                )
                            })
                            .collect::<Vec<_>>(),
                    ) as Made
                }),
            ),
            (
                "64 texture blocks, NOT_RESIDENT + MakeResident + fenced copy (the pool's)",
                Box::new(|| {
                    Box::new(
                        (0..64)
                            .map(|_| {
                                let h = heap(
                                    device,
                                    default(),
                                    D3D12_HEAP_FLAG_ALLOW_ONLY_NON_RT_DS_TEXTURES
                                        | D3D12_HEAP_FLAG_CREATE_NOT_ZEROED
                                        | D3D12_HEAP_FLAG_CREATE_NOT_RESIDENT,
                                );
                                super::made_resident(device, &h).unwrap();
                                h
                            })
                            .collect::<Vec<_>>(),
                    ) as Made
                }),
            ),
            (
                "64 card buffer blocks (the pool's)",
                Box::new(|| {
                    Box::new(
                        (0..64)
                            .map(|_| {
                                let h = heap(
                                    device,
                                    default(),
                                    D3D12_HEAP_FLAG_ALLOW_ONLY_BUFFERS
                                        | D3D12_HEAP_FLAG_CREATE_NOT_RESIDENT,
                                );
                                super::made_resident(device, &h).unwrap();
                                h
                            })
                            .collect::<Vec<_>>(),
                    ) as Made
                }),
            ),
            (
                "32 upload blocks, mapped whole and written",
                Box::new(|| {
                    Box::new(
                        (0..32)
                            .map(|_| {
                                let h = heap(device, upload(), D3D12_HEAP_FLAG_ALLOW_ONLY_BUFFERS);
                                let r = mapped_whole(device, &h, D3D12_RESOURCE_STATE_GENERIC_READ);
                                (h, r)
                            })
                            .collect::<Vec<_>>(),
                    ) as Made
                }),
            ),
            (
                "32 upload blocks, NOT_RESIDENT + MakeResident, mapped whole and written (the pool's)",
                Box::new(|| {
                    Box::new(
                        (0..32)
                            .map(|_| {
                                let h = heap(
                                    device,
                                    upload(),
                                    D3D12_HEAP_FLAG_ALLOW_ONLY_BUFFERS
                                        | D3D12_HEAP_FLAG_CREATE_NOT_RESIDENT,
                                );
                                super::made_resident(device, &h).unwrap();
                                let r = mapped_whole(device, &h, D3D12_RESOURCE_STATE_GENERIC_READ);
                                (h, r)
                            })
                            .collect::<Vec<_>>(),
                    ) as Made
                }),
            ),
            (
                "32 write-back blocks, NOT_RESIDENT + MakeResident, mapped whole and written (the shared pool's)",
                Box::new(|| {
                    Box::new(
                        (0..32)
                            .map(|_| {
                                let h = heap(
                                    device,
                                    write_back(),
                                    D3D12_HEAP_FLAG_ALLOW_ONLY_BUFFERS
                                        | D3D12_HEAP_FLAG_CREATE_NOT_RESIDENT,
                                );
                                super::made_resident(device, &h).unwrap();
                                let r = mapped_whole(device, &h, D3D12_RESOURCE_STATE_COMMON);
                                (h, r)
                            })
                            .collect::<Vec<_>>(),
                    ) as Made
                }),
            ),
            (
                "2048 committed upload buffers of 1 MiB, mapped and written (as without the pools)",
                Box::new(|| {
                    Box::new(
                        (0..2048)
                            .map(|_| {
                                let (r, data) = host.upload_buffer(1 << 20).unwrap();
                                // SAFETY: its mapping.
                                unsafe { std::ptr::write_bytes(data, 0, 1 << 20) };
                                r
                            })
                            .collect::<Vec<_>>(),
                    ) as Made
                }),
            ),
        ];
        for (what, make) in phases {
            let made = make();
            say(&format!("with {what}"));
            drop(made);
            say("  and after they went");
        }
        stop.store(true, Ordering::Relaxed);
    });
}

/// The same while one kind of pool work runs on four threads, as the
/// title's recording threads would make it: the unmap's cost with each.
/// Run with the five switches on.
#[test]
#[ignore = "a measurement, not a check"]
fn unmap_costs_while_pools_work() {
    let Some(host) = crate::host::host() else { return };
    let device = &host.device;
    let tex = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
        Width: 256,
        Height: 256,
        DepthOrArraySize: 1,
        MipLevels: 1,
        Format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_BC7_UNORM,
        SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        ..Default::default()
    };
    let committed_tex = || {
        let mut out: Option<ID3D12Resource> = None;
        // SAFETY: plain creation.
        unsafe { device.CreateCommittedResource(&default(), D3D12_HEAP_FLAG_NONE, &tex, D3D12_RESOURCE_STATE_COMMON, None, &mut out) }
            .unwrap();
        out
    };
    type Work<'a> = (&'a str, Box<dyn Fn() + Sync + 'a>);
    let works: Vec<Work> = vec![
        ("nothing", Box::new(|| std::thread::sleep(std::time::Duration::from_millis(1)))),
        ("committed textures made and let go", Box::new(|| drop(committed_tex()))),
        // SAFETY: a valid description.
        ("pool textures made and let go", Box::new(|| drop(unsafe { super::texture(device, &tex, D3D12_RESOURCE_STATE_COMMON) }))),
        ("committed staging of 256 KiB made, written, let go", Box::new(|| {
            if let Some((b, data)) = host.upload_buffer(256 << 10) {
                // SAFETY: its mapping.
                unsafe { std::ptr::write_bytes(data, 1, 256 << 10) };
                drop(b);
            }
        })),
        ("pool staging of 256 KiB taken, written, given back", Box::new(|| {
            if let Some((piece, _, _, data)) = super::staging(host, 256 << 10) {
                // SAFETY: the piece's mapping.
                unsafe { std::ptr::write_bytes(data, 1, 256 << 10) };
                drop(piece);
            }
        })),
        ("pool card buffers of 64 KiB made and let go", Box::new(|| drop(super::card_buffer(device, 64 << 10)))),
        ("pool shared buffers of 64 KiB made, written, let go", Box::new(|| {
            if let Some((b, data)) = super::shared_buffer(device, 64 << 10) {
                // SAFETY: its mapping.
                unsafe { std::ptr::write_bytes(data, 1, 64 << 10) };
                drop(b);
            }
        })),
        ("new blocks made resident (MakeResident + fenced copy)", Box::new(|| {
            let h = heap(device, default(), D3D12_HEAP_FLAG_ALLOW_ONLY_BUFFERS | D3D12_HEAP_FLAG_CREATE_NOT_RESIDENT);
            super::made_resident(device, &h).unwrap();
        })),
        ("new upload blocks made and mapped whole", Box::new(|| {
            let h = heap(device, upload(), D3D12_HEAP_FLAG_ALLOW_ONLY_BUFFERS);
            drop(mapped_whole(device, &h, D3D12_RESOURCE_STATE_GENERIC_READ));
        })),
    ];
    for (what, work) in &works {
        let stop = AtomicBool::new(false);
        let done = std::sync::atomic::AtomicU64::new(0);
        let us = std::thread::scope(|s| {
            for _ in 0..4 {
                s.spawn(|| {
                    while !stop.load(Ordering::Relaxed) {
                        work();
                        done.fetch_add(1, Ordering::Relaxed);
                    }
                });
            }
            // About two seconds of maps and unmaps.
            let us = unmap_us(400);
            stop.store(true, Ordering::Relaxed);
            us
        });
        println!("while {what} ({} done): {us:.1} us an unmap", done.load(Ordering::Relaxed));
    }
}

/// The title's unmaps with the renderer port's heap opened on the same
/// section, with and without the pools' blocks.
#[test]
#[ignore = "a measurement, not a check"]
fn unmap_costs_with_a_port_heap() {
    let Some(host) = crate::host::host() else { return };
    let device = &host.device;
    let say = |what: &str| {
        println!(
            "{what}: {:.1} us an unmap, {:.1} with a port heap on the section",
            unmap_us_with(20, None),
            unmap_us_with(20, Some(device))
        )
    };
    say("nothing made");
    let blocks: Vec<ID3D12Heap> = (0..64)
        .map(|_| {
            let h = heap(
                device,
                default(),
                D3D12_HEAP_FLAG_ALLOW_ONLY_NON_RT_DS_TEXTURES | D3D12_HEAP_FLAG_CREATE_NOT_ZEROED | D3D12_HEAP_FLAG_CREATE_NOT_RESIDENT,
            );
            super::made_resident(device, &h).unwrap();
            h
        })
        .collect();
    let cpu: Vec<_> = (0..64)
        .map(|i| {
            let props = if i % 2 == 0 { upload() } else { write_back() };
            let h = heap(device, props, D3D12_HEAP_FLAG_ALLOW_ONLY_BUFFERS | D3D12_HEAP_FLAG_CREATE_NOT_RESIDENT);
            super::made_resident(device, &h).unwrap();
            let state = if i % 2 == 0 { D3D12_RESOURCE_STATE_GENERIC_READ } else { D3D12_RESOURCE_STATE_COMMON };
            let r = mapped_whole(device, &h, state);
            (h, r)
        })
        .collect();
    say("with 64 texture blocks and 64 mapped CPU blocks (the pools' 8 GB)");
    drop((blocks, cpu));
    say("after they went");
}
