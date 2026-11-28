//! (feedcard, #88; deepscout B #7) How fast the card reads, and does atomics on, a buffer placed
//! in a heap opened on private memory (`OpenExistingHeapFromAddress1`, what `crate::inplace` and
//! `crate::port` give the title's memory in place) against the heap our host copies use (custom,
//! write-back, L0: `Host::shared_buffer`), an upload heap, and the card's own memory.
//!
//! xcl (#2844, #2938, Kalmarv) measured the opened heap 8-24x slower for loads and 13-22x for
//! atomics than a custom L0 heap on an RTX 5070 Ti. This decides whether opening more of the title's
//! memory in place (`VICTORY_PORT_CONSTANTS`, a `PORT_PRIVATE`-style port) saves the CPU its copies
//! at the GPU's expense on our card.
//!
//! Ignored: it opens the real card. Run it under the `gpu` lock only, never beside a game:
//! `cargo test -p victory-gpu --lib --release inplace::bench -- --ignored --nocapture`.

use super::*;

const MIB: u64 = 1 << 20;
/// The streaming test's buffer.
const SIZE: u64 = 64 * MIB;
/// The hot test's: what a constant buffer or a small table is, read by every thread.
const HOT: u64 = 64 << 10;

/// Loads: `n` threads each read `per` 16-byte words, a pass over the buffer's `mask + 1` bytes
/// per round, and xor them into one word they store. Atomics: each thread adds to one of 4,096
/// words of the buffer.
const SOURCE: &str = r#"#define RS "RootConstants(num32BitConstants=4, b0), SRV(t0), UAV(u0)"
cbuffer C : register(b0) { uint mask; uint per; uint stride; uint mode; };
ByteAddressBuffer src : register(t0);
RWByteAddressBuffer dst : register(u0);
[RootSignature(RS)]
[numthreads(256, 1, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    if (mode == 1) {
        uint was;
        dst.InterlockedAdd(((id.x * 2654435761u) & 4095u) * 4u, 1u, was);
        return;
    }
    uint4 acc = 0;
    uint at = id.x * 16u;
    [loop] for (uint i = 0; i < per; i++) {
        acc ^= src.Load4(at & mask);
        at += stride;
    }
    if (acc.x == 0x9e3779b9u && acc.y == acc.z) { dst.Store(0, acc.w); }
}
"#;

fn hardware() -> Option<ID3D12Device> {
    use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
    use windows::Win32::Graphics::Dxgi::*;
    // SAFETY: plain DXGI queries and device creation on the fastest adapter.
    unsafe {
        let factory: IDXGIFactory6 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)).ok()?;
        let adapter: IDXGIAdapter1 = factory
            .EnumAdapterByGpuPreference(0, DXGI_GPU_PREFERENCE_HIGH_PERFORMANCE)
            .ok()?;
        let desc = adapter.GetDesc1().ok()?;
        if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0 {
            return None;
        }
        let name = String::from_utf16_lossy(&desc.Description);
        println!("adapter: {}", name.trim_end_matches('\0'));
        let mut device: Option<ID3D12Device> = None;
        D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_12_0, &mut device).ok()?;
        device
    }
}

fn custom(
    device: &ID3D12Device,
    size: u64,
    page: D3D12_CPU_PAGE_PROPERTY,
    pool: D3D12_MEMORY_POOL,
) -> windows::core::Result<ID3D12Resource> {
    let heap = D3D12_HEAP_PROPERTIES {
        Type: D3D12_HEAP_TYPE_CUSTOM,
        CPUPageProperty: page,
        MemoryPoolPreference: pool,
        ..Default::default()
    };
    let mut buffer: Option<ID3D12Resource> = None;
    // SAFETY: plain creation.
    unsafe {
        device.CreateCommittedResource(
            &heap,
            D3D12_HEAP_FLAG_NONE,
            &buffer_desc(size, D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS),
            D3D12_RESOURCE_STATE_COMMON,
            None,
            &mut buffer,
        )?
    };
    buffer.ok_or_else(windows::core::Error::empty)
}

fn default_heap(device: &ID3D12Device, size: u64) -> windows::core::Result<ID3D12Resource> {
    let heap = D3D12_HEAP_PROPERTIES {
        Type: D3D12_HEAP_TYPE_DEFAULT,
        ..Default::default()
    };
    let mut buffer: Option<ID3D12Resource> = None;
    // SAFETY: plain creation.
    unsafe {
        device.CreateCommittedResource(
            &heap,
            D3D12_HEAP_FLAG_NONE,
            &buffer_desc(size, D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS),
            D3D12_RESOURCE_STATE_COMMON,
            None,
            &mut buffer,
        )?
    };
    buffer.ok_or_else(windows::core::Error::empty)
}

struct Timer {
    root: ID3D12RootSignature,
    pso: ID3D12PipelineState,
    queries: ID3D12QueryHeap,
    back: ID3D12Resource,
    ticks: f64,
}

impl Timer {
    fn new(rig: &Rig) -> windows::core::Result<Timer> {
        let mut code: Option<ID3DBlob> = None;
        let mut errors: Option<ID3DBlob> = None;
        // SAFETY: the source and names live through the call; then the blob's own bytes.
        unsafe {
            let made = D3DCompile(
                SOURCE.as_ptr().cast(),
                SOURCE.len(),
                s!("import bench"),
                None,
                None,
                s!("main"),
                s!("cs_5_1"),
                0,
                0,
                &mut code,
                Some(&mut errors),
            );
            if let Some(e) = &errors {
                println!(
                    "{}",
                    String::from_utf8_lossy(std::slice::from_raw_parts(
                        e.GetBufferPointer().cast::<u8>(),
                        e.GetBufferSize()
                    ))
                );
            }
            made?;
            let code = code.ok_or_else(windows::core::Error::empty)?;
            let bytes = std::slice::from_raw_parts(
                code.GetBufferPointer().cast::<u8>(),
                code.GetBufferSize(),
            );
            let root: ID3D12RootSignature = rig.device.CreateRootSignature(0, bytes)?;
            let pso =
                rig.device
                    .CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                        pRootSignature: std::mem::transmute_copy(&root),
                        CS: D3D12_SHADER_BYTECODE {
                            pShaderBytecode: bytes.as_ptr().cast(),
                            BytecodeLength: bytes.len(),
                        },
                        ..Default::default()
                    })?;
            let mut queries: Option<ID3D12QueryHeap> = None;
            rig.device.CreateQueryHeap(
                &D3D12_QUERY_HEAP_DESC {
                    Type: D3D12_QUERY_HEAP_TYPE_TIMESTAMP,
                    Count: 2,
                    NodeMask: 0,
                },
                &mut queries,
            )?;
            let back =
                rig.committed(D3D12_HEAP_TYPE_READBACK, 16, D3D12_RESOURCE_STATE_COPY_DEST)?;
            let ticks = rig.queue.GetTimestampFrequency()? as f64;
            Ok(Timer {
                root,
                pso,
                queries: queries.ok_or_else(windows::core::Error::empty)?,
                back,
                ticks,
            })
        }
    }

    /// Card microseconds of one dispatch of `groups` groups reading `src` (at `src_gpu`) and
    /// writing `dst_gpu`, the best of five.
    fn time(
        &self,
        rig: &mut Rig,
        src_gpu: u64,
        dst_gpu: u64,
        consts: [u32; 4],
        groups: u32,
    ) -> windows::core::Result<f64> {
        let mut best = f64::MAX;
        for _ in 0..5 {
            // SAFETY: an open list of ours; the views name live buffers.
            rig.run(|list| unsafe {
                list.SetComputeRootSignature(&self.root);
                list.SetPipelineState(&self.pso);
                list.SetComputeRoot32BitConstants(0, 4, consts.as_ptr().cast(), 0);
                list.SetComputeRootShaderResourceView(1, src_gpu);
                list.SetComputeRootUnorderedAccessView(2, dst_gpu);
                list.EndQuery(&self.queries, D3D12_QUERY_TYPE_TIMESTAMP, 0);
                list.Dispatch(groups, 1, 1);
                list.EndQuery(&self.queries, D3D12_QUERY_TYPE_TIMESTAMP, 1);
                list.ResolveQueryData(
                    &self.queries,
                    D3D12_QUERY_TYPE_TIMESTAMP,
                    0,
                    2,
                    &self.back,
                    0,
                );
            })?;
            let mut data = std::ptr::null_mut();
            // SAFETY: the readback holds two timestamps.
            let (a, b) = unsafe {
                self.back.Map(0, None, Some(&mut data))?;
                let t = std::slice::from_raw_parts(data.cast::<u64>(), 2);
                let got = (t[0], t[1]);
                self.back.Unmap(0, None);
                got
            };
            best = best.min(b.saturating_sub(a) as f64 / self.ticks * 1e6);
        }
        Ok(best)
    }
}

#[test]
#[ignore]
fn bench_import_heap_against_custom_l0_on_the_card() {
    let Some(device) = hardware() else {
        println!("no hardware adapter");
        return;
    };
    if device.cast::<ID3D12Device13>().is_err() {
        println!("no ID3D12Device13: parts of an allocation cannot be opened");
        return;
    }
    let mut rig = Rig::new(&device).unwrap();
    let timer = Timer::new(&rig).unwrap();
    let out = default_heap(&device, 64 << 10).unwrap();
    // SAFETY: a live buffer.
    let out_gpu = unsafe { out.GetGPUVirtualAddress() };
    // Private memory as the title's is (write-watched), committed and touched.
    // SAFETY: an allocation of the test's own, freed at the end.
    let base = unsafe {
        mem::VirtualAlloc(
            std::ptr::null(),
            (SIZE + 2 * K64 as u64) as usize,
            mem::MEM_RESERVE | mem::MEM_COMMIT | MEM_WRITE_WATCH,
            mem::PAGE_READWRITE,
        )
        .cast::<u8>()
    };
    assert!(!base.is_null());
    fill(base, (SIZE + 2 * K64 as u64) as usize, 3);
    let start = (base as u64).next_multiple_of(K64 as u64);
    let placed = place_part(&device, start, SIZE).unwrap_or_else(|e| panic!("not placed: {e:?}"));
    let l0 = custom(
        &device,
        SIZE,
        D3D12_CPU_PAGE_PROPERTY_WRITE_BACK,
        D3D12_MEMORY_POOL_L0,
    )
    .unwrap();
    let wc = custom(
        &device,
        SIZE,
        D3D12_CPU_PAGE_PROPERTY_WRITE_COMBINE,
        D3D12_MEMORY_POOL_L0,
    )
    .unwrap();
    let vram = default_heap(&device, SIZE).unwrap();
    // SAFETY: live buffers.
    let heaps: [(&str, u64); 4] = unsafe {
        [
            ("opened (import heap)", placed.gpu),
            (
                "custom write-back L0 (our host copies)",
                l0.GetGPUVirtualAddress(),
            ),
            (
                "custom write-combine L0 (upload-like)",
                wc.GetGPUVirtualAddress(),
            ),
            ("default (card memory)", vram.GetGPUVirtualAddress()),
        ]
    };
    // Streaming: 1M threads, 4 words each, every 16 bytes of 64 MiB read once.
    let threads: u32 = 1 << 20;
    let stream = [(SIZE - 1) as u32 & !15, 4, threads * 16, 0];
    // Hot: the same 64 KiB read 16 times by 1M threads (from the card's caches after the first).
    let hot = [(HOT - 1) as u32 & !15, 16, 4096, 0];
    let atomics = [0, 0, 0, 1];
    let mut rows = Vec::new();
    for (name, gpu) in heaps {
        let s = timer
            .time(&mut rig, gpu, out_gpu, stream, threads / 256)
            .unwrap();
        let h = timer
            .time(&mut rig, gpu, out_gpu, hot, threads / 256)
            .unwrap();
        let a = timer
            .time(&mut rig, gpu, gpu, atomics, threads / 256)
            .unwrap();
        let gbs = SIZE as f64 / (s * 1e-6) / 1e9;
        println!(
            "{name}: stream 64 MiB {s:.0} us ({gbs:.1} GB/s); hot 64 KiB x16 {h:.0} us; 1M atomics on 4,096 words {a:.0} us"
        );
        rows.push((s, h, a));
    }
    let (s0, h0, a0) = rows[0];
    let (s1, h1, a1) = rows[1];
    println!(
        "opened / custom L0: stream {:.2}x, hot {:.2}x, atomics {:.2}x (above 1: the opened heap is slower)",
        s0 / s1,
        h0 / h1,
        a0 / a1
    );
    drop(placed);
    // SAFETY: ours; the heap's pages may be let go a little after the last reference.
    unsafe {
        let mut tries = 0;
        while mem::VirtualFree(base.cast(), 0, mem::MEM_RELEASE) == 0 && tries < 100 {
            tries += 1;
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
}

/// The process's commit (private bytes) and the system's, in MB.
fn commit_mb() -> (i64, i64) {
    use windows_sys::Win32::System::ProcessStatus::{K32GetPerformanceInfo, K32GetProcessMemoryInfo, PERFORMANCE_INFORMATION, PROCESS_MEMORY_COUNTERS};
    let mut counters = PROCESS_MEMORY_COUNTERS { cb: size_of::<PROCESS_MEMORY_COUNTERS>() as u32, ..Default::default() };
    let mut perf = PERFORMANCE_INFORMATION { cb: size_of::<PERFORMANCE_INFORMATION>() as u32, ..Default::default() };
    // SAFETY: our own process and sized structures.
    unsafe {
        K32GetProcessMemoryInfo(windows_sys::Win32::System::Threading::GetCurrentProcess(), &mut counters, counters.cb);
        K32GetPerformanceInfo(&mut perf, perf.cb);
    }
    ((counters.PagefileUsage >> 20) as i64, ((perf.CommitTotal * perf.PageSize) >> 20) as i64)
}

/// (El1xr, `VICTORY_INPLACE_WINDOWS`) What a buffer in a heap opened on private memory adds to
/// commit, against what a host copy (custom write-back L0, `Host::shared_buffer`) of the same
/// bytes adds: the in-place saving is real only if the opened heap charges nothing of its own.
/// Both are made resident by a copy the card runs. Ignored: it opens the real card; under the
/// `gpu` lock only:
/// `cargo test -p victory-gpu --lib inplace::bench::commit -- --ignored --nocapture`.
#[test]
#[ignore]
fn commit_of_an_opened_heap_against_a_host_copy_on_the_card() {
    const MB: u64 = 256;
    let Some(device) = hardware() else {
        println!("no hardware adapter");
        return;
    };
    if device.cast::<ID3D12Device13>().is_err() {
        println!("no ID3D12Device13");
        return;
    }
    let size = MB * MIB;
    let mut rig = Rig::new(&device).unwrap();
    let start = commit_mb();
    // SAFETY: an allocation of the test's own, committed and touched, freed at the end.
    let base = unsafe { mem::VirtualAlloc(std::ptr::null(), size as usize, mem::MEM_RESERVE | mem::MEM_COMMIT | MEM_WRITE_WATCH, mem::PAGE_READWRITE) }.cast::<u8>();
    assert!(!base.is_null());
    fill(base, size as usize, 3);
    let title = commit_mb();
    let heap = open(&device, From::Part(base as usize, size as usize)).unwrap();
    let placed = place(&device, &heap, 0, size).unwrap();
    let ms = rig.copy_ms(&placed, size).unwrap();
    let opened = commit_mb();
    let copy = custom(&device, size, D3D12_CPU_PAGE_PROPERTY_WRITE_BACK, D3D12_MEMORY_POOL_L0).unwrap();
    let copied_ms = rig.copy_ms(&copy, size).unwrap();
    let copied = commit_mb();
    println!(
        "{MB} MB of private memory: process/system commit +{}/+{} MB; opened in place and read by the card ({ms:.2} ms): +{}/+{} MB; \
         a host copy (custom L0 write-back) read by the card ({copied_ms:.2} ms): +{}/+{} MB",
        title.0 - start.0,
        title.1 - start.1,
        opened.0 - title.0,
        opened.1 - title.1,
        copied.0 - opened.0,
        copied.1 - opened.1
    );
    drop((copy, placed, heap));
    // SAFETY: the test's own allocation; the heap's pages are let go a little after its last reference.
    unsafe {
        let mut tries = 0;
        while mem::VirtualFree(base.cast(), 0, mem::MEM_RELEASE) == 0 && tries < 100 {
            tries += 1;
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
}

/// (El1xr, `VICTORY_INPLACE_WINDOWS`) What the title's own free of private memory does while a heap
/// opened on it stands, and after: whether `VirtualFree` (decommit, then release) fails (the title
/// would see its free fail) or goes through (the card would read freed pages). Ignored: it opens
/// the real card; under the `gpu` lock only:
/// `cargo test -p victory-gpu --lib inplace::bench::title_free -- --ignored --nocapture`.
#[test]
#[ignore]
fn title_free_under_an_opened_heap_on_the_card() {
    let Some(device) = hardware() else {
        println!("no hardware adapter");
        return;
    };
    if device.cast::<ID3D12Device13>().is_err() {
        println!("no ID3D12Device13");
        return;
    }
    let size = 4 * MIB;
    let try_free = |base: *mut u8, what: u32, len: usize| {
        // SAFETY: the test's own allocation.
        let ok = unsafe { mem::VirtualFree(base.cast(), len, what) } != 0;
        (ok, if ok { 0 } else { std::io::Error::last_os_error().raw_os_error().unwrap_or(0) })
    };
    // SAFETY: an allocation of the test's own.
    let base = unsafe { mem::VirtualAlloc(std::ptr::null(), size as usize, mem::MEM_RESERVE | mem::MEM_COMMIT | MEM_WRITE_WATCH, mem::PAGE_READWRITE) }.cast::<u8>();
    fill(base, size as usize, 5);
    let heap = open(&device, From::Part(base as usize, size as usize)).unwrap();
    let placed = place(&device, &heap, 0, size).unwrap();
    println!("while the heap stands: decommit of one 64 KiB page {:?}, release {:?}", try_free(base, mem::MEM_DECOMMIT, 64 << 10), try_free(base, mem::MEM_RELEASE, 0));
    drop((placed, heap));
    let mut tries = 0;
    let mut freed = try_free(base, mem::MEM_RELEASE, 0);
    while !freed.0 && tries < 200 {
        tries += 1;
        std::thread::sleep(std::time::Duration::from_millis(5));
        freed = try_free(base, mem::MEM_RELEASE, 0);
    }
    println!("after the heap's last reference: release {freed:?} after {tries} retries of 5 ms");
}
