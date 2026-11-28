//! `VICTORY_NATIVE_BVH_GPU_INSTANCES=1` (off by default, with
//! `VICTORY_NATIVE_BVH=1`): a top level's instances are made on the card
//! from the host copy of title memory, not read from the CPU's view of it.
//! The title may write its instance descriptions with a compute pass; those
//! bytes are in the host copy on the card long before they land back on the
//! CPU, so the CPU read gave last frame's (or older) instances.
//!
//! A small compute pass reads each 64-byte `D3D12_RAYTRACING_INSTANCE_DESC`
//! from the host copy, looks its bottom level's title address up in a table
//! sorted by title address (binary search) and writes the description with
//! the host bottom level's address; one without a host bottom level gets
//! mask 0 (never hit), as the CPU path does. The top level is built from
//! that array. The CPU read stays only to keep the named bottom levels from
//! idling out and for the `instances_without_ours` count.

use std::sync::{Mutex, OnceLock};

use windows::Win32::Graphics::Direct3D12::*;

/// Whether top levels take their instances from the card
/// (`VICTORY_NATIVE_BVH_GPU_INSTANCES=1`).
pub fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var_os("VICTORY_NATIVE_BVH_GPU_INSTANCES").is_some_and(|v| v == "1")
    })
}

/// Threads per group of the pass.
const GROUP: u32 = 64;

/// Root parameters: 2 constants (instance count, table entries), the
/// title's instances (t0), the table (t1), the host instances (u0).
const SOURCE: &str = r#"
#define RS "RootConstants(num32BitConstants=2, b0), SRV(t0), SRV(t1), UAV(u0)"
cbuffer C : register(b0) { uint count; uint entries; };
ByteAddressBuffer title : register(t0);
ByteAddressBuffer table : register(t1);
RWByteAddressBuffer host : register(u0);

[RootSignature(RS)]
[numthreads(64, 1, 1)]
void main(uint3 id : SV_DispatchThreadID)
{
    uint i = id.x;
    if (i >= count)
        return;
    uint at = i * 64;
    uint4 a = title.Load4(at);
    uint4 b = title.Load4(at + 16);
    uint4 c = title.Load4(at + 32);
    uint4 d = title.Load4(at + 48);
    // d.x: instance id (low 24 bits) and mask (high 8); d.zw: the bottom level.
    uint first = 0;
    uint n = entries;
    while (n > 0) {
        uint half = n / 2;
        uint m = first + half;
        uint2 key = table.Load2(m * 16);
        if (key.y < d.w || (key.y == d.w && key.x < d.z)) {
            first = m + 1;
            n = n - half - 1;
        } else {
            n = half;
        }
    }
    uint2 found = uint2(0, 0);
    if (first < entries) {
        uint4 e = table.Load4(first * 16);
        if (e.x == d.z && e.y == d.w)
            found = e.zw;
    }
    if (found.x == 0 && found.y == 0)
        d.x = 0;
    d.zw = found;
    host.Store4(at, a);
    host.Store4(at + 16, b);
    host.Store4(at + 32, c);
    host.Store4(at + 48, d);
}
"#;

/// The pass's root signature and pipeline.
pub(super) struct Kit {
    root: ID3D12RootSignature,
    pso: ID3D12PipelineState,
}

// SAFETY: free-threaded D3D12 objects, only read after creation.
unsafe impl Send for Kit {}

/// Makes the pass on `device`.
pub(super) fn make(device: &ID3D12Device) -> windows::core::Result<Kit> {
    let code = crate::host::compile(
        SOURCE,
        windows::core::s!("main"),
        windows::core::s!("cs_5_1"),
    )?;
    // SAFETY: plain object creation; the code lives through the calls and
    // holds the root signature.
    unsafe {
        let root: ID3D12RootSignature = device.CreateRootSignature(0, &code)?;
        let pso = device.CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
            pRootSignature: std::mem::transmute_copy(&root),
            CS: D3D12_SHADER_BYTECODE {
                pShaderBytecode: code.as_ptr().cast(),
                BytecodeLength: code.len(),
            },
            ..Default::default()
        })?;
        Ok(Kit { root, pso })
    }
}

/// The host's pass, made once (none if it could not be made: logged).
fn kit(device: &ID3D12Device) -> Option<&'static Mutex<Kit>> {
    static KIT: OnceLock<Option<Mutex<Kit>>> = OnceLock::new();
    KIT.get_or_init(|| match make(device) {
        Ok(k) => Some(Mutex::new(k)),
        Err(e) => {
            tracing::warn!(error = %e, "native bvh: no pass to make instances on the card; top levels read them on the CPU");
            None
        }
    })
    .as_ref()
}

/// The lookup table: `(title, host)` bottom-level addresses sorted by title
/// address, one each, as 4 dwords per entry (title low, high, host low,
/// high). At least one entry's room, so the view is never empty.
pub(super) fn table(mut pairs: Vec<(u64, u64)>) -> (Vec<u8>, u32) {
    pairs.sort_unstable_by_key(|p| p.0);
    pairs.dedup_by_key(|p| p.0);
    let mut bytes = Vec::with_capacity(pairs.len().max(1) * 16);
    for (title, host) in &pairs {
        bytes.extend_from_slice(&title.to_le_bytes());
        bytes.extend_from_slice(&host.to_le_bytes());
    }
    bytes.resize(bytes.len().max(16), 0);
    (bytes, pairs.len() as u32)
}

/// Records the pass: `count` instances from `title_va` through the table at
/// `table_va` (`entries`) into `host_va`.
///
/// # Safety
/// `list` is open; the addresses name live buffers of the sizes the counts
/// say, the title's and the table readable, the host's writable.
pub(super) unsafe fn dispatch(
    list: &ID3D12GraphicsCommandList,
    kit: &Kit,
    title_va: u64,
    table_va: u64,
    entries: u32,
    host_va: u64,
    count: u32,
) {
    // SAFETY: by the caller.
    unsafe {
        list.SetComputeRootSignature(&kit.root);
        list.SetPipelineState(&kit.pso);
        list.SetComputeRoot32BitConstants(0, 2, [count, entries].as_ptr().cast(), 0);
        list.SetComputeRootShaderResourceView(1, title_va);
        list.SetComputeRootShaderResourceView(2, table_va);
        list.SetComputeRootUnorderedAccessView(3, host_va);
        list.Dispatch(count.div_ceil(GROUP), 1, 1);
    }
}

/// On the host list: the host instance array for `count` instances at title
/// address `title_va` (its host copy's GPU address), with `pairs` the
/// bottom levels kept. Returns the array, ready to build from (state
/// `NON_PIXEL_SHADER_RESOURCE`, held by the list).
pub(super) fn record(
    device: &ID3D12Device,
    host: &crate::host::HostList,
    title_va: u64,
    count: u32,
    pairs: Vec<(u64, u64)>,
) -> Option<ID3D12Resource> {
    let kit = kit(device)?;
    let kit = kit.lock().unwrap_or_else(|e| e.into_inner());
    let (bytes, entries) = table(pairs);
    let (upload, data) = crate::host::host()?.upload_buffer(bytes.len() as u64)?;
    // SAFETY: the mapped upload buffer holds at least `bytes`.
    unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), data, bytes.len()) };
    let size = u64::from(count.max(1)) * 64;
    let out = super::buffer(device, size, D3D12_RESOURCE_STATE_UNORDERED_ACCESS)?;
    // SAFETY: an open list; live buffers of the sizes given; the title's
    // writes to its instances (its compute passes) are done first.
    unsafe {
        host.list.ResourceBarrier(&[uav_all()]);
        dispatch(
            &host.list,
            &kit,
            title_va,
            upload.GetGPUVirtualAddress(),
            entries,
            out.GetGPUVirtualAddress(),
            count,
        );
        host.list.ResourceBarrier(&[crate::host::transition(
            &out,
            D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
            D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
        )]);
    }
    host.keep(&upload);
    host.keep(&out);
    Some(out)
}

fn uav_all() -> D3D12_RESOURCE_BARRIER {
    D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            UAV: std::mem::ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER {
                pResource: std::mem::ManuallyDrop::new(None),
            }),
        },
    }
}

#[cfg(test)]
#[path = "gpu_instances_tests.rs"]
mod tests;
