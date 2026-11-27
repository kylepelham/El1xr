//! The card's detiling against the PC's, byte for byte
//! (fxceless/victory-fork 3461f52faa's test, with partial stagings and
//! the real card added).

use super::*;
use victory_formats::texture::{Dimension, Swizzle, Texture, TextureDesc};
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

struct Rig {
    device: ID3D12Device,
    queue: ID3D12CommandQueue,
    allocator: ID3D12CommandAllocator,
    list: ID3D12GraphicsCommandList,
    fence: ID3D12Fence,
    event: HANDLE,
    submitted: u64,
}

impl Drop for Rig {
    fn drop(&mut self) {
        // SAFETY: our own event.
        unsafe {
            let _ = CloseHandle(self.event);
        }
    }
}

impl Rig {
    /// The software adapter (`card` false), or the system's default
    /// adapter, the card.
    fn new(card: bool) -> Option<Rig> {
        // SAFETY: plain object creation on a device of our own.
        unsafe {
            let mut device: Option<ID3D12Device> = None;
            if card {
                D3D12CreateDevice(None, D3D_FEATURE_LEVEL_12_0, &mut device).ok()?;
            } else {
                let adapter = crate::host::warp_adapter().ok()?;
                D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_12_0, &mut device).ok()?;
            }
            let device = device?;
            let queue: ID3D12CommandQueue = device
                .CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                    Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
                    ..Default::default()
                })
                .ok()?;
            let allocator: ID3D12CommandAllocator = device
                .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
                .ok()?;
            let list: ID3D12GraphicsCommandList = device
                .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)
                .ok()?;
            let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE).ok()?;
            let event = CreateEventW(None, false, false, None).ok()?;
            Some(Rig {
                device,
                queue,
                allocator,
                list,
                fence,
                event,
                submitted: 0,
            })
        }
    }

    fn run(&mut self, record: impl FnOnce(&ID3D12GraphicsCommandList)) {
        record(&self.list);
        self.submitted += 1;
        // SAFETY: our list, queue and fence; the allocator is reset only
        // after the device finished.
        unsafe {
            self.list.Close().unwrap();
            let run: ID3D12CommandList = windows::core::Interface::cast(&self.list).unwrap();
            self.queue.ExecuteCommandLists(&[Some(run)]);
            self.queue.Signal(&self.fence, self.submitted).unwrap();
            self.fence
                .SetEventOnCompletion(self.submitted, self.event)
                .unwrap();
            assert_eq!(
                WaitForSingleObject(self.event, 60_000),
                WAIT_OBJECT_0,
                "the device did not finish"
            );
            let removed = self.device.GetDeviceRemovedReason();
            assert!(removed.is_ok(), "device lost: {removed:?}");
            self.allocator.Reset().unwrap();
            self.list.Reset(&self.allocator, None).unwrap();
        }
    }

    fn buffer(
        &self,
        kind: D3D12_HEAP_TYPE,
        flags: D3D12_RESOURCE_FLAGS,
        state: D3D12_RESOURCE_STATES,
        bytes: u64,
    ) -> ID3D12Resource {
        let heap = D3D12_HEAP_PROPERTIES {
            Type: kind,
            ..Default::default()
        };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: bytes.max(1),
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
        let mut out: Option<ID3D12Resource> = None;
        // SAFETY: plain buffer creation.
        unsafe {
            self.device
                .CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &desc, state, None, &mut out)
                .unwrap()
        };
        out.unwrap()
    }
}

fn map(buffer: &ID3D12Resource) -> *mut u8 {
    let mut data = std::ptr::null_mut();
    // SAFETY: a CPU-visible buffer of our own.
    unsafe { buffer.Map(0, None, Some(&mut data)).unwrap() };
    data.cast()
}

/// A texture case: the console's description and the host's.
#[derive(Clone, Copy)]
struct Case {
    dimension: Dimension,
    width: u32,
    height: u32,
    depth: u32,
    mips: u32,
    format: DXGI_FORMAT,
    swizzle: Swizzle,
    /// The first level staged (a view of the smaller levels only).
    first: u32,
}

fn t2(width: u32, height: u32, mips: u32, format: DXGI_FORMAT, swizzle: Swizzle) -> Case {
    Case {
        dimension: Dimension::Tex2d,
        width,
        height,
        depth: 1,
        mips,
        format,
        swizzle,
        first: 0,
    }
}

/// Whether the PC's detiling (as `Staging::detile` does it) and the
/// card's leave the same bytes in every row of every part, for `case`,
/// from the same pseudo-random tiled bytes, `held` the mask of held
/// levels (zeros for the others).
fn both(rig: &mut Rig, d: &Detiler, case: &Case, held: u32) -> bool {
    let texture = Texture::new(&TextureDesc {
        dimension: case.dimension,
        width: case.width,
        height: case.height,
        depth_or_array: case.depth,
        mips: case.mips,
        format: case.format.0 as u32,
        samples: 1,
        swizzle: case.swizzle,
    })
    .unwrap();
    let layout = texture.planes[0].layout.clone();
    let three_d = case.dimension == Dimension::Tex3d;
    let host_desc = D3D12_RESOURCE_DESC {
        Dimension: if three_d {
            D3D12_RESOURCE_DIMENSION_TEXTURE3D
        } else {
            D3D12_RESOURCE_DIMENSION_TEXTURE2D
        },
        Width: u64::from(case.width),
        Height: case.height,
        DepthOrArraySize: case.depth as u16,
        MipLevels: case.mips as u16,
        Format: case.format,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        ..Default::default()
    };
    // As `build_staging_untimed`: parts from the first staged level, and
    // the memory up to the end of those levels.
    let (first, size) = super::super::staged_part(&layout, case.mips, case.first);
    let count = if three_d {
        case.mips
    } else {
        case.mips * case.depth - first
    };
    let part = |i: usize| {
        let mips = case.mips - first;
        (first + i as u32 % mips, i as u32 / mips)
    };
    let mut footprints = vec![D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); count as usize];
    let mut rows = vec![0u32; count as usize];
    let mut total = 0u64;
    // SAFETY: outputs for `count` subresources.
    unsafe {
        rig.device.GetCopyableFootprints(
            &host_desc,
            first,
            count,
            0,
            Some(footprints.as_mut_ptr()),
            Some(rows.as_mut_ptr()),
            None,
            Some(&mut total),
        );
    }
    let mut seed = 0x9E37_79B9_7F4A_7C15u64 ^ u64::from(case.width * 31 + case.height);
    let tiled: Vec<u8> = (0..size)
        .map(|_| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed >> 24) as u8
        })
        .collect();
    // The PC's way.
    let mut cpu = vec![0xA5u8; total as usize + 4096];
    for (i, (footprint, &r)) in footprints.iter().zip(&rows).enumerate() {
        let (mip, slice) = part(i);
        let f = footprint.Footprint;
        let pitch = f.RowPitch as usize;
        let len = pitch * r as usize * f.Depth as usize;
        let linear = &mut cpu[footprint.Offset as usize..][..len];
        if mip < 32 && held & (1 << mip) == 0 {
            linear.fill(0);
            continue;
        }
        layout
            .detile(&tiled, mip, slice, linear, pitch, pitch * r as usize)
            .unwrap();
    }
    // The card's way, from an offset in a shared upload buffer (as a piece
    // of a staging block).
    const BASE: u64 = 4096;
    let raw = raw_layout(&layout, size as u64);
    let jobs = jobs(&layout, &footprints, &rows, part, &raw).expect("detilable on the card");
    let upload = rig.buffer(
        D3D12_HEAP_TYPE_UPLOAD,
        D3D12_RESOURCE_FLAG_NONE,
        D3D12_RESOURCE_STATE_GENERIC_READ,
        BASE + raw.bytes,
    );
    // SAFETY: the mapping holds `BASE + raw.bytes`.
    let data = unsafe { map(&upload).add(BASE as usize) };
    let linear = rig.buffer(
        D3D12_HEAP_TYPE_DEFAULT,
        D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
        D3D12_RESOURCE_STATE_COMMON,
        total,
    );
    let gpu = Gpu {
        linear,
        pooled: 0,
        raw,
        jobs,
    };
    // SAFETY: the mapping holds `raw.bytes` past `BASE`, ours alone.
    unsafe {
        prepare(&gpu, &layout, data);
        stage(&gpu, data, &tiled, held);
    }
    let back = rig.buffer(
        D3D12_HEAP_TYPE_READBACK,
        D3D12_RESOURCE_FLAG_NONE,
        D3D12_RESOURCE_STATE_COPY_DEST,
        total,
    );
    rig.run(|list| {
        record_with(list, d, &upload, BASE, &gpu);
        // SAFETY: an open list; the buffers outlive the run.
        unsafe { list.CopyBufferRegion(&back, 0, copy_source(&gpu), 0, total) };
        copied(list, &gpu);
    });
    // SAFETY: the read-back buffer holds `total` bytes the list wrote.
    let card = unsafe { std::slice::from_raw_parts(map(&back).cast_const(), total as usize) };
    // A row's own bytes must agree (the rest of a pitch is padding the
    // copy into the texture never reads).
    let n = layout.desc.element_bytes as usize;
    let mut same = true;
    for (i, (footprint, &r)) in footprints.iter().zip(&rows).enumerate() {
        let (mip, _) = part(i);
        let m = &layout.mips[mip as usize];
        let f = footprint.Footprint;
        for z in 0..m.depth as usize {
            for y in 0..m.height as usize {
                let at = footprint.Offset as usize
                    + z * f.RowPitch as usize * r as usize
                    + y * f.RowPitch as usize;
                let own = m.width as usize * n;
                if cpu[at..at + own] != card[at..at + own] {
                    if same {
                        eprintln!(
                            "{:?} {}x{}x{} {:?} from mip {}: part {i} (mip {mip}) row {y} depth {z} differs",
                            case.swizzle,
                            case.width,
                            case.height,
                            case.depth,
                            case.format,
                            case.first
                        );
                    }
                    same = false;
                }
            }
        }
    }
    same
}

/// Every tiling the card covers, BC and plain formats of every element
/// size, mip tails, arrays, thick 3D, linear, partial stagings, and levels
/// on demand that are not the title's.
fn cases() -> Vec<(Case, u32)> {
    let all = u32::MAX;
    let mut out = vec![
        (
            t2(1024, 1024, 11, DXGI_FORMAT_BC7_UNORM, Swizzle::SW_64KB_S),
            all,
        ),
        (
            t2(512, 256, 10, DXGI_FORMAT_BC1_UNORM, Swizzle::SW_64KB_S),
            all,
        ),
        (
            t2(256, 256, 9, DXGI_FORMAT_BC3_UNORM, Swizzle::SW_4KB_S),
            all,
        ),
        (
            t2(100, 60, 7, DXGI_FORMAT_BC5_UNORM, Swizzle::SW_4KB_S),
            all,
        ),
        (
            t2(300, 200, 9, DXGI_FORMAT_R8G8B8A8_UNORM, Swizzle::SW_64KB_S),
            all,
        ),
        (
            t2(129, 65, 8, DXGI_FORMAT_R8G8B8A8_UNORM, Swizzle::SW_4KB_S),
            all,
        ),
        (t2(97, 61, 7, DXGI_FORMAT_R8_UNORM, Swizzle::SW_64KB_S), all),
        (t2(77, 33, 7, DXGI_FORMAT_R16_UNORM, Swizzle::SW_4KB_D), all),
        (
            t2(
                64,
                64,
                7,
                DXGI_FORMAT_R16G16B16A16_FLOAT,
                Swizzle::SW_64KB_D,
            ),
            all,
        ),
        (
            t2(
                50,
                30,
                6,
                DXGI_FORMAT_R32G32B32A32_FLOAT,
                Swizzle::SW_256B_S,
            ),
            all,
        ),
        (t2(33, 17, 6, DXGI_FORMAT_R8_UNORM, Swizzle::SW_256B_D), all),
        (
            t2(130, 70, 8, DXGI_FORMAT_R8G8B8A8_UNORM, Swizzle::LINEAR),
            all,
        ),
        (t2(61, 3, 6, DXGI_FORMAT_R8_UNORM, Swizzle::LINEAR), all),
        (
            Case {
                depth: 6,
                ..t2(128, 128, 8, DXGI_FORMAT_BC1_UNORM, Swizzle::SW_4KB_S)
            },
            all,
        ),
        (
            Case {
                depth: 3,
                ..t2(70, 70, 4, DXGI_FORMAT_R8G8B8A8_UNORM, Swizzle::SW_64KB_S)
            },
            all,
        ),
        (
            Case {
                dimension: Dimension::Tex3d,
                depth: 16,
                ..t2(64, 32, 7, DXGI_FORMAT_R8G8B8A8_UNORM, Swizzle::SW_4KB_S)
            },
            all,
        ),
        (
            Case {
                dimension: Dimension::Tex3d,
                depth: 40,
                ..t2(33, 20, 6, DXGI_FORMAT_R16_UNORM, Swizzle::SW_64KB_S)
            },
            all,
        ),
        // A view of the smaller levels only: the staging holds them alone.
        (
            Case {
                first: 4,
                ..t2(1024, 1024, 11, DXGI_FORMAT_BC1_UNORM, Swizzle::SW_64KB_S)
            },
            all,
        ),
    ];
    // Levels on demand: the largest levels not the title's (zeros).
    out.push((
        t2(1024, 1024, 11, DXGI_FORMAT_BC7_UNORM, Swizzle::SW_64KB_S),
        !0b11,
    ));
    out.push((
        t2(300, 200, 9, DXGI_FORMAT_R8G8B8A8_UNORM, Swizzle::SW_64KB_S),
        !0b1,
    ));
    out
}

fn check(rig: &mut Rig) {
    let d = make(&rig.device).expect("the detile shader compiles");
    let mut all = true;
    for (case, held) in cases() {
        all &= both(rig, &d, &case, held);
    }
    assert!(all, "the card's detiling differs from the PC's (see above)");
}

/// On the software adapter (every machine, the gate).
#[test]
fn the_software_adapter_detiles_as_the_pc_does() {
    let Some(mut rig) = Rig::new(false) else {
        eprintln!("no software adapter: not run");
        return;
    };
    check(&mut rig);
}

/// On the card (run by hand, with the `gpu` lock held:
/// `cargo test -p victory-gpu --lib the_card_detiles -- --ignored`).
#[test]
#[ignore = "runs on the card: take the gpu lock"]
fn the_card_detiles_as_the_pc_does() {
    let rig = Rig::new(true).expect("a hardware device");
    // SAFETY: a live device.
    let luid = unsafe { rig.device.GetAdapterLuid() };
    eprintln!("on adapter {:x}:{:x}", luid.HighPart, luid.LowPart);
    let mut rig = rig;
    check(&mut rig);
}

#[test]
fn the_switch_is_off_unless_one() {
    assert!(!switch(None));
    assert!(!switch(Some(std::ffi::OsStr::new("0"))));
    assert!(switch(Some(std::ffi::OsStr::new("1"))));
}

/// The mask and the tables follow the staged bytes, 16-byte aligned.
#[test]
fn the_mask_and_tables_follow_the_staged_bytes() {
    let l = Texture::new(&TextureDesc {
        dimension: Dimension::Tex2d,
        width: 256,
        height: 256,
        depth_or_array: 1,
        mips: 9,
        format: DXGI_FORMAT_BC1_UNORM.0 as u32,
        samples: 1,
        swizzle: Swizzle::SW_4KB_S,
    })
    .unwrap()
    .planes[0]
        .layout
        .clone();
    let raw = raw_layout(&l, 1000);
    assert_eq!(raw.mask_at, 1008);
    assert_eq!(raw.tables_at, 1024);
    let words: usize = l.pattern_tables().iter().map(|t| t.len()).sum();
    assert_eq!(raw.bytes, 1024 + 4 * words as u64);
}

/// The XOR modes go the PC's way, by the mode.
#[test]
fn the_xor_modes_fall_back_to_the_pc() {
    let l = Texture::new(&TextureDesc {
        dimension: Dimension::Tex2d,
        width: 64,
        height: 64,
        depth_or_array: 1,
        mips: 1,
        format: DXGI_FORMAT_R8G8B8A8_UNORM.0 as u32,
        samples: 1,
        swizzle: Swizzle::SW_64KB_S_X,
    })
    .unwrap()
    .planes[0]
        .layout
        .clone();
    let footprint = D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
        Offset: 0,
        Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            Width: 64,
            Height: 64,
            Depth: 1,
            RowPitch: 256,
        },
    };
    let raw = raw_layout(&l, l.size);
    assert_eq!(
        jobs(&l, &[footprint], &[64], |_| (0, 0), &raw),
        Err(Fallback::Mode)
    );
    // Rows the host lays out narrower than the texture's: the PC's way.
    let plain = Texture::new(&TextureDesc {
        swizzle: Swizzle::SW_64KB_S,
        dimension: Dimension::Tex2d,
        width: 64,
        height: 64,
        depth_or_array: 1,
        mips: 1,
        format: DXGI_FORMAT_R8G8B8A8_UNORM.0 as u32,
        samples: 1,
    })
    .unwrap()
    .planes[0]
        .layout
        .clone();
    let narrow = D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
        Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
            RowPitch: 128,
            ..footprint.Footprint
        },
        ..footprint
    };
    let raw = raw_layout(&plain, plain.size);
    assert_eq!(
        jobs(&plain, &[narrow], &[64], |_| (0, 0), &raw),
        Err(Fallback::Footprint)
    );
    assert!(jobs(&plain, &[footprint], &[64], |_| (0, 0), &raw).is_ok());
}
