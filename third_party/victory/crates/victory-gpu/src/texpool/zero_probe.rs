//! Whether memory a device hands out fresh holds zeros, on this PC's card and
//! on the software adapter (El1xr, after the card block reuse zeroer flake:
//! D3D12 only says committed resources and heaps are "almost always" zeroed,
//! and WARP handed back a test's freed buffer with its bytes). Each kind the
//! pools make is filled with 0xCD, let go, made again at the same size and
//! read. `cargo test -p victory-gpu zero_probe -- --ignored --nocapture
//! --test-threads=1`; the card one is a GPU run of its own (the `gpu` lock).

use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
use windows::Win32::Graphics::Direct3D12::*;
use windows::core::Interface;

use super::buffers::buffer_desc;

const ROUNDS: usize = 24;
const MADE: usize = 8;

#[derive(Clone, Copy, Debug)]
enum Kind {
    CommittedCard,
    HeapCard,
    CommittedUpload,
    HeapShared,
}

fn props(kind: Kind) -> D3D12_HEAP_PROPERTIES {
    match kind {
        Kind::CommittedCard | Kind::HeapCard => D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_DEFAULT,
            ..Default::default()
        },
        Kind::CommittedUpload => D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_UPLOAD,
            ..Default::default()
        },
        Kind::HeapShared => D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_CUSTOM,
            CPUPageProperty: D3D12_CPU_PAGE_PROPERTY_WRITE_BACK,
            MemoryPoolPreference: D3D12_MEMORY_POOL_L0,
            ..Default::default()
        },
    }
}

struct Gpu {
    device: ID3D12Device,
    queue: ID3D12CommandQueue,
    allocator: ID3D12CommandAllocator,
    fence: ID3D12Fence,
    next: u64,
}

impl Gpu {
    fn new(device: ID3D12Device) -> Gpu {
        // SAFETY: plain object creation on a device of our own.
        unsafe {
            let queue = device
                .CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                    Type: D3D12_COMMAND_LIST_TYPE_COPY,
                    ..Default::default()
                })
                .unwrap();
            let allocator = device
                .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_COPY)
                .unwrap();
            let fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap();
            Gpu {
                device,
                queue,
                allocator,
                fence,
                next: 0,
            }
        }
    }

    fn copy(&mut self, to: &ID3D12Resource, from: &ID3D12Resource, size: u64) {
        // SAFETY: both buffers live and are at least `size` long; the list
        // has run when this returns.
        unsafe {
            let list: ID3D12GraphicsCommandList = self
                .device
                .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_COPY, &self.allocator, None)
                .unwrap();
            list.CopyBufferRegion(to, 0, from, 0, size);
            list.Close().unwrap();
            self.queue
                .ExecuteCommandLists(&[Some(list.cast().unwrap())]);
            self.next += 1;
            self.queue.Signal(&self.fence, self.next).unwrap();
            while self.fence.GetCompletedValue() < self.next {
                std::thread::yield_now();
            }
            self.allocator.Reset().unwrap();
        }
    }

    fn committed(
        &self,
        kind: D3D12_HEAP_TYPE,
        size: u64,
        state: D3D12_RESOURCE_STATES,
    ) -> ID3D12Resource {
        let mut made: Option<ID3D12Resource> = None;
        // SAFETY: a plain buffer description.
        unsafe {
            self.device
                .CreateCommittedResource(
                    &D3D12_HEAP_PROPERTIES {
                        Type: kind,
                        ..Default::default()
                    },
                    D3D12_HEAP_FLAG_NONE,
                    &buffer_desc(size, D3D12_RESOURCE_FLAG_NONE),
                    state,
                    None,
                    &mut made,
                )
                .unwrap();
        }
        made.unwrap()
    }

    /// A buffer of `kind` (with the heap it is placed in, for the heap kinds).
    fn make(&self, kind: Kind, size: u64) -> (ID3D12Resource, Option<ID3D12Heap>) {
        let state = match kind {
            Kind::CommittedUpload => D3D12_RESOURCE_STATE_GENERIC_READ,
            _ => D3D12_RESOURCE_STATE_COMMON,
        };
        let mut made: Option<ID3D12Resource> = None;
        // SAFETY: plain descriptions; the placed buffer fills its heap.
        unsafe {
            match kind {
                Kind::CommittedCard | Kind::CommittedUpload => {
                    self.device
                        .CreateCommittedResource(
                            &props(kind),
                            D3D12_HEAP_FLAG_NONE,
                            &buffer_desc(size, D3D12_RESOURCE_FLAG_NONE),
                            state,
                            None,
                            &mut made,
                        )
                        .unwrap();
                    (made.unwrap(), None)
                }
                Kind::HeapCard | Kind::HeapShared => {
                    let mut heap: Option<ID3D12Heap> = None;
                    self.device
                        .CreateHeap(
                            &D3D12_HEAP_DESC {
                                SizeInBytes: size,
                                Properties: props(kind),
                                Alignment: 0,
                                Flags: D3D12_HEAP_FLAG_ALLOW_ONLY_BUFFERS,
                            },
                            &mut heap,
                        )
                        .unwrap();
                    let heap = heap.unwrap();
                    self.device
                        .CreatePlacedResource(
                            &heap,
                            0,
                            &buffer_desc(size, D3D12_RESOURCE_FLAG_NONE),
                            state,
                            None,
                            &mut made,
                        )
                        .unwrap();
                    (made.unwrap(), Some(heap))
                }
            }
        }
    }
}

/// For each kind: how many of `ROUNDS` rounds handed out a fresh buffer
/// holding something other than zeros, and the first such byte seen.
fn probe(device: ID3D12Device, size: u64) -> Vec<(Kind, usize, Option<(usize, u8)>)> {
    let mut gpu = Gpu::new(device);
    let up = gpu.committed(
        D3D12_HEAP_TYPE_UPLOAD,
        size,
        D3D12_RESOURCE_STATE_GENERIC_READ,
    );
    let back = gpu.committed(
        D3D12_HEAP_TYPE_READBACK,
        size,
        D3D12_RESOURCE_STATE_COPY_DEST,
    );
    // SAFETY: both buffers live to the end, mapped whole.
    let (fill, read) = unsafe {
        let mut f = std::ptr::null_mut();
        up.Map(0, None, Some(&mut f)).unwrap();
        std::ptr::write_bytes(f.cast::<u8>(), 0xCD, size as usize);
        let mut r = std::ptr::null_mut();
        back.Map(0, None, Some(&mut r)).unwrap();
        (f.cast::<u8>(), r.cast::<u8>())
    };
    let mut out = Vec::new();
    for kind in [
        Kind::CommittedCard,
        Kind::HeapCard,
        Kind::CommittedUpload,
        Kind::HeapShared,
    ] {
        let cpu = matches!(kind, Kind::CommittedUpload | Kind::HeapShared);
        let mut dirty = 0;
        let mut first = None;
        for _ in 0..ROUNDS {
            for _ in 0..MADE {
                let (used, _heap) = gpu.make(kind, size);
                if cpu {
                    // SAFETY: a live CPU-visible buffer of `size`.
                    unsafe {
                        let mut p = std::ptr::null_mut();
                        used.Map(0, None, Some(&mut p)).unwrap();
                        std::ptr::copy_nonoverlapping(fill, p.cast::<u8>(), size as usize);
                        used.Unmap(0, None);
                    }
                } else {
                    gpu.copy(&used, &up, size);
                }
            }
            let (fresh, _heap) = gpu.make(kind, size);
            // SAFETY: `read` is the readback buffer's mapping, or the fresh
            // CPU-visible buffer's, of `size` bytes.
            let bytes = unsafe {
                if cpu {
                    let mut p = std::ptr::null_mut();
                    fresh.Map(0, None, Some(&mut p)).unwrap();
                    std::slice::from_raw_parts(p.cast::<u8>(), size as usize).to_vec()
                } else {
                    gpu.copy(&back, &fresh, size);
                    std::slice::from_raw_parts(read, size as usize).to_vec()
                }
            };
            if let Some(at) = bytes.iter().position(|&b| b != 0) {
                dirty += 1;
                first.get_or_insert((at, bytes[at]));
            }
        }
        out.push((kind, dirty, first));
    }
    out
}

fn say(on: &str, size: u64, found: &[(Kind, usize, Option<(usize, u8)>)]) {
    for (kind, dirty, first) in found {
        println!(
            "zero probe {on} {} MiB {kind:?}: {dirty} of {ROUNDS} rounds handed out memory not zeroed, first {first:?}",
            size >> 20
        );
    }
}

#[test]
#[ignore = "a measurement on the card, a GPU run of its own"]
fn zero_probe_card() {
    // `ZERO_PROBE_MB=4` for one size only (a shorter run: both take about 7 minutes).
    let sizes: Vec<u64> = match std::env::var("ZERO_PROBE_MB").ok().and_then(|v| v.parse::<u64>().ok()) {
        Some(mb) => vec![mb << 20],
        None => vec![4 << 20, 64 << 20],
    };
    for size in sizes {
        let mut device: Option<ID3D12Device> = None;
        // SAFETY: the default adapter.
        unsafe { D3D12CreateDevice(None, D3D_FEATURE_LEVEL_12_0, &mut device) }.unwrap();
        say("card", size, &probe(device.unwrap(), size));
    }
}

#[test]
#[ignore = "a measurement"]
fn zero_probe_warp() {
    let Ok(adapter) = crate::host::warp_adapter() else {
        return;
    };
    let mut device: Option<ID3D12Device> = None;
    // SAFETY: the software adapter.
    unsafe { D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_12_0, &mut device) }.unwrap();
    say("warp", 4 << 20, &probe(device.unwrap(), 4 << 20));
}
