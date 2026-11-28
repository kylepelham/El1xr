//! (Coldtrain, Belobug #329) How many D3D12 command allocators are alive,
//! for the ram line and the hang watch. Coldtrain tied device resets
//! (`0x887A0007`, empty DRED, debug layer message 834) to 1,997-2,045 live
//! `ID3D12CommandAllocator` near a reported 2,048 threshold (AMD). Our
//! launcher keeps up to `VICTORY_LIST_POOL_MOST=2048` idle host lists, two
//! allocators each when their prelude is made, so the count may pass that
//! on our side too; nothing said how many there were.
//!
//! Each allocator we make is handed a tiny COM object as private data
//! ([`track`]); the allocator releases it when it is destroyed, and its drop
//! takes one off [`LIVE`]. Only counting: no command changes.

use std::sync::Arc;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use windows::Win32::Graphics::Direct3D12::ID3D12CommandAllocator;
use windows::Win32::Foundation::{E_NOINTERFACE, E_POINTER, S_OK};
use windows::core::{GUID, HRESULT, IUnknown, IUnknown_Vtbl, Interface};

/// Command allocators alive now (made through [`track`] and not destroyed).
pub(crate) static LIVE: AtomicU64 = AtomicU64::new(0);
/// Command allocators made since the start.
pub(crate) static MADE: AtomicU64 = AtomicU64::new(0);
/// The most alive at once.
pub(crate) static PEAK: AtomicU64 = AtomicU64::new(0);

/// The private-data slot the tracker sits in (our own name).
const SLOT: GUID = GUID::from_u128(0x5b1c_7e0a_33d4_4f19_9a6e_c0a1_1c0c_a11c);

/// Takes one off [`LIVE`] when the allocator holding it is destroyed (and
/// in tests, says so through its flag). A bare COM object (only `IUnknown`)
/// made by hand: the crate has no `windows-core` for `#[implement]` in
/// release builds.
#[repr(C)]
struct Tracker {
    vtable: *const IUnknown_Vtbl,
    refs: AtomicU32,
    gone: Option<Arc<AtomicBool>>,
}

static VTABLE: IUnknown_Vtbl = IUnknown_Vtbl { QueryInterface: query, AddRef: add_ref, Release: release };

unsafe extern "system" fn query(this: *mut c_void, iid: *const GUID, out: *mut *mut c_void) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    // SAFETY: COM's contract: `iid` and `out` point at valid memory.
    unsafe {
        if !iid.is_null() && *iid == IUnknown::IID {
            add_ref(this);
            *out = this;
            return S_OK;
        }
        *out = std::ptr::null_mut();
    }
    E_NOINTERFACE
}

unsafe extern "system" fn add_ref(this: *mut c_void) -> u32 {
    // SAFETY: `this` is a live tracker (COM's contract).
    let tracker = unsafe { &*this.cast::<Tracker>() };
    tracker.refs.fetch_add(1, Ordering::Relaxed) + 1
}

unsafe extern "system" fn release(this: *mut c_void) -> u32 {
    // SAFETY: `this` is a live tracker (COM's contract).
    let left = unsafe { &*this.cast::<Tracker>() }.refs.fetch_sub(1, Ordering::AcqRel) - 1;
    if left == 0 {
        // SAFETY: the last reference: made by `Box::into_raw` in `tracker`.
        drop(unsafe { Box::from_raw(this.cast::<Tracker>()) });
    }
    left
}

impl Drop for Tracker {
    fn drop(&mut self) {
        LIVE.fetch_sub(1, Ordering::Relaxed);
        if let Some(gone) = &self.gone {
            gone.store(true, Ordering::Relaxed);
        }
    }
}

/// A new tracker, one reference held by the result.
fn tracker(gone: Option<Arc<AtomicBool>>) -> IUnknown {
    let raw = Box::into_raw(Box::new(Tracker { vtable: &VTABLE, refs: AtomicU32::new(1), gone }));
    // SAFETY: a COM object whose first field is its vtable, with one reference.
    unsafe { IUnknown::from_raw(raw.cast()) }
}

/// Counts `allocator` in [`LIVE`] until it is destroyed.
pub(crate) fn track(allocator: &ID3D12CommandAllocator) {
    track_with(allocator, None);
}

fn track_with(allocator: &ID3D12CommandAllocator, gone: Option<Arc<AtomicBool>>) {
    MADE.fetch_add(1, Ordering::Relaxed);
    PEAK.fetch_max(LIVE.fetch_add(1, Ordering::Relaxed) + 1, Ordering::Relaxed);
    let tracker = tracker(gone);
    // SAFETY: a live allocator; it keeps a reference to the tracker and
    // releases it when it is destroyed. If the call fails, the tracker goes
    // with `tracker` below and takes itself off again.
    let _ = unsafe { allocator.SetPrivateDataInterface(&SLOT, &tracker) };
}

/// `(alive, made, peak)`.
pub(crate) fn counts() -> (u64, u64, u64) {
    (LIVE.load(Ordering::Relaxed), MADE.load(Ordering::Relaxed), PEAK.load(Ordering::Relaxed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
    use windows::Win32::Graphics::Direct3D12::{D3D12_COMMAND_LIST_TYPE_DIRECT, D3D12CreateDevice, ID3D12Device};

    /// On the software adapter: an allocator holds its tracker while it
    /// lives and lets it go when it is destroyed.
    #[test]
    fn an_allocator_is_counted_until_it_is_destroyed() {
        // SAFETY: plain object creation on a device of our own.
        unsafe {
            crate::test_d3d12_debug::enable();
            let Ok(adapter) = crate::host::warp_adapter() else {
                eprintln!("no WARP adapter: skipped");
                return;
            };
            let mut device: Option<ID3D12Device> = None;
            if D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_12_0, &mut device).is_err() {
                eprintln!("no WARP device: skipped");
                return;
            }
            let device = device.unwrap();
            let allocator: ID3D12CommandAllocator =
                device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).unwrap();
            let gone = Arc::new(AtomicBool::new(false));
            let made = MADE.load(Ordering::Relaxed);
            track_with(&allocator, Some(gone.clone()));
            assert!(MADE.load(Ordering::Relaxed) > made);
            assert!(!gone.load(Ordering::Relaxed), "held while the allocator lives");
            assert!(counts().0 >= 1 && counts().2 >= 1);
            drop(allocator);
            assert!(gone.load(Ordering::Relaxed), "let go when the allocator is destroyed");
            crate::test_d3d12_debug::assert_clean(&device, "alloc_count");
        }
    }

    /// (El1xr) What a direct command allocator keeps of the process's commit
    /// on the host card: 128 allocators, each with a list that records `n`
    /// commands (copies of 256 bytes and barriers, about what a host list
    /// records per draw), measured after recording, after a reset (kept for
    /// the next recording) and after the allocators go; then the same sizes
    /// again. `cargo test -p victory-gpu allocator_memory -- --ignored --nocapture`.
    ///
    /// RTX 5090, 2026-11-28: about 20 KiB an allocator at 100 commands, 100 KiB
    /// at 1,000, 1.4 MiB at 10,000, 6 MiB at 50,000; a reset keeps all of it,
    /// and allocators destroyed give none back to the process's commit: the
    /// driver keeps it and hands it to the next allocators (the second 1,000
    /// and 50,000 rounds took 3-12 KiB each). So destroying or remaking large
    /// idle allocators cuts no commit; only fewer commands held at once do.
    #[test]
    #[ignore = "a measurement, not a check"]
    fn allocator_memory() {
        use windows::Win32::Graphics::Direct3D12::*;
        use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;
        let Some(host) = crate::host::host() else {
            return;
        };
        let device = &host.device;
        let private = || {
            use windows_sys::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
            use windows_sys::Win32::System::Threading::GetCurrentProcess;
            let mut c = PROCESS_MEMORY_COUNTERS { cb: size_of::<PROCESS_MEMORY_COUNTERS>() as u32, ..Default::default() };
            // SAFETY: our own process and a sized structure.
            unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) };
            c.PagefileUsage as i64
        };
        // SAFETY: plain object creation and recording on the host device;
        // nothing is executed.
        unsafe {
            let buffer = |size: u64| {
                let mut made: Option<ID3D12Resource> = None;
                device
                    .CreateCommittedResource(
                        &D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() },
                        D3D12_HEAP_FLAG_NONE,
                        &D3D12_RESOURCE_DESC {
                            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                            Width: size,
                            Height: 1,
                            DepthOrArraySize: 1,
                            MipLevels: 1,
                            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                            ..Default::default()
                        },
                        D3D12_RESOURCE_STATE_COMMON,
                        None,
                        &mut made,
                    )
                    .unwrap();
                made.unwrap()
            };
            let (a, b) = (buffer(1 << 20), buffer(1 << 20));
            const N: usize = 128;
            let first = private();
            for n in [0usize, 100, 1000, 10_000, 50_000, 1000, 50_000] {
                let start = private();
                let mut kept = Vec::with_capacity(N);
                for _ in 0..N {
                    let allocator: ID3D12CommandAllocator = device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).unwrap();
                    let list: ID3D12GraphicsCommandList =
                        device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None).unwrap();
                    for i in 0..n {
                        let at = (i as u64 % 4096) * 256;
                        if i % 8 == 7 {
                            list.ResourceBarrier(&[D3D12_RESOURCE_BARRIER {
                                Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
                                Anonymous: D3D12_RESOURCE_BARRIER_0 {
                                    UAV: std::mem::ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER {
                                        pResource: std::mem::ManuallyDrop::new(None),
                                    }),
                                },
                                ..Default::default()
                            }]);
                        } else {
                            list.CopyBufferRegion(&b, at, &a, at, 256);
                        }
                    }
                    list.Close().unwrap();
                    kept.push((allocator, list));
                }
                let recorded = private();
                for (allocator, list) in &kept {
                    allocator.Reset().unwrap();
                    list.Reset(allocator, None).unwrap();
                    list.Close().unwrap();
                }
                let reset = private();
                drop(kept);
                std::thread::sleep(std::time::Duration::from_millis(500));
                let gone = private();
                let kib = |v: i64| v as f64 / N as f64 / 1024.0;
                eprintln!(
                    "allocator memory: {n} commands: {:.0} KiB each recorded, {:.0} KiB kept after a reset, {:.0} KiB left after they go",
                    kib(recorded - start),
                    kib(reset - start),
                    kib(gone - start),
                );
                eprintln!("allocator memory: process private {} MiB over the first", (private() - first) >> 20);
            }
        }
    }
}
