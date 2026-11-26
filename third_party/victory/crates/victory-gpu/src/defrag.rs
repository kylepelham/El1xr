//! GPU-side telemetry buffer for `CS_DefragHeap_ComputeRelocations` half-meets
//! (`SLOT_MEET`): three dwords tracking meetings, wait iterations, and wait atomics.

use std::sync::Mutex;

use windows::Win32::Foundation::CloseHandle;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;
use windows::Win32::System::Threading::{CreateEventW, INFINITE, WaitForSingleObject};
use windows::core::Interface;

use crate::host::{Host, host, transition};

struct DefragCounter {
    gpu: ID3D12Resource,
    upload: ID3D12Resource,
    readback: ID3D12Resource,
    va: u64,
    /// The zeroing copy has been recorded on some list.
    armed: bool,
}

static DEFRAG_COUNTER: Mutex<Option<DefragCounter>> = Mutex::new(None);

fn defrag_buffer(
    device: &ID3D12Device,
    kind: D3D12_HEAP_TYPE,
    flags: D3D12_RESOURCE_FLAGS,
    state: D3D12_RESOURCE_STATES,
) -> windows::core::Result<ID3D12Resource> {
    let mut made: Option<ID3D12Resource> = None;
    // SAFETY: a 16-byte buffer. The caller keeps it.
    unsafe {
        device.CreateCommittedResource(
            &D3D12_HEAP_PROPERTIES {
                Type: kind,
                ..Default::default()
            },
            D3D12_HEAP_FLAG_NONE,
            &D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                Width: 16,
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
            },
            state,
            None,
            &mut made,
        )?;
    }
    made.ok_or_else(windows::core::Error::empty)
}

/// The face-relocation counter buffer. The first list to bind it copies zeros
/// in before the dispatch, on that same list.
pub(crate) fn bind_defrag_counter(list: &ID3D12GraphicsCommandList) -> Option<u64> {
    let host = host()?;
    let mut slot = DEFRAG_COUNTER.lock().unwrap_or_else(|e| e.into_inner());
    if slot.is_none() {
        let gpu = defrag_buffer(
            &host.device,
            D3D12_HEAP_TYPE_DEFAULT,
            D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
            D3D12_RESOURCE_STATE_COPY_DEST,
        )
        .ok()?;
        let upload = defrag_buffer(
            &host.device,
            D3D12_HEAP_TYPE_UPLOAD,
            D3D12_RESOURCE_FLAG_NONE,
            D3D12_RESOURCE_STATE_GENERIC_READ,
        )
        .ok()?;
        let readback = defrag_buffer(
            &host.device,
            D3D12_HEAP_TYPE_READBACK,
            D3D12_RESOURCE_FLAG_NONE,
            D3D12_RESOURCE_STATE_COPY_DEST,
        )
        .ok()?;
        // SAFETY: the upload mapping covers the 16 bytes just created.
        unsafe {
            let mut data = std::ptr::null_mut();
            upload.Map(0, None, Some(&mut data)).ok()?;
            std::ptr::write_bytes(data, 0, 16);
            upload.Unmap(0, None);
        }
        let va = unsafe { gpu.GetGPUVirtualAddress() };
        *slot = Some(DefragCounter {
            gpu,
            upload,
            readback,
            va,
            armed: false,
        });
    }
    let counter = slot.as_mut()?;
    if !counter.armed {
        // SAFETY: the list is open and records this before the dispatch that adds.
        unsafe {
            list.CopyBufferRegion(&counter.gpu, 0, &counter.upload, 0, 16);
            list.ResourceBarrier(&[transition(
                &counter.gpu,
                D3D12_RESOURCE_STATE_COPY_DEST,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
            )]);
        }
        counter.armed = true;
    }
    Some(counter.va)
}

/// Periodic progress log from `Meter` once `CS_DefragHeap_ComputeRelocations`
/// has dispatched at least once.
pub(crate) fn log_progress() {
    let (defrag_dispatches, defrag_groups, defrag_indirect) = crate::draw::defrag_counts();
    if defrag_dispatches > 0 || defrag_indirect > 0 {
        tracing::info!(
            dispatches = defrag_dispatches,
            groups = defrag_groups,
            indirect = defrag_indirect,
            sites = victory_shader::emit::defrag_meet_sites(),
            lds_reads = victory_shader::emit::defrag_lds_sites().0,
            lds_writes = victory_shader::emit::defrag_lds_sites().1,
            "CS_DefragHeap_ComputeRelocations since start"
        );
    }
}

/// Reads the face-relocation counters after the queue has finished what it
/// already submitted, and logs them with the host's dispatch counts.
pub fn log_defrag_counters() {
    let (dispatches, groups, indirect) = crate::draw::defrag_counts();
    let sites = victory_shader::emit::defrag_meet_sites();
    let (lds_reads, lds_writes) = victory_shader::emit::defrag_lds_sites();
    let Some(host) = host() else {
        tracing::info!(
            dispatches,
            groups,
            indirect,
            sites,
            lds_reads,
            lds_writes,
            "CS_DefragHeap_ComputeRelocations counters, no device to read the waits"
        );
        return;
    };
    let read = read_defrag_counters(host);
    let (meetings, polls, atomics) = read.unwrap_or((0, 0, 0));
    let per_group = if groups == 0 {
        0.0
    } else {
        meetings as f64 / groups as f64
    };
    let per_meet = if meetings == 0 {
        0.0
    } else {
        polls as f64 / meetings as f64
    };
    tracing::info!(
        dispatches,
        groups,
        indirect,
        sites,
        lds_reads,
        lds_writes,
        meetings,
        wait_iterations = polls,
        wait_atomics = atomics,
        meetings_per_group = format!("{per_group:.2}"),
        iterations_per_meeting = format!("{per_meet:.2}"),
        "CS_DefragHeap_ComputeRelocations counters"
    );
}

fn read_defrag_counters(host: &Host) -> Option<(u64, u64, u64)> {
    let slot = DEFRAG_COUNTER.lock().unwrap_or_else(|e| e.into_inner());
    let counter = slot.as_ref()?;
    if !counter.armed {
        return Some((0, 0, 0));
    }
    // SAFETY: objects this process owns. The fence waits for the copy.
    unsafe {
        let allocator: ID3D12CommandAllocator = host
            .device
            .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
            .ok()?;
        let list: ID3D12GraphicsCommandList = host
            .device
            .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)
            .ok()?;
        list.ResourceBarrier(&[transition(
            &counter.gpu,
            D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
            D3D12_RESOURCE_STATE_COPY_SOURCE,
        )]);
        list.CopyBufferRegion(&counter.readback, 0, &counter.gpu, 0, 16);
        list.Close().ok()?;
        host.queue.ExecuteCommandLists(&[Some(list.cast().ok()?)]);
        let fence: ID3D12Fence = host.device.CreateFence(0, D3D12_FENCE_FLAG_NONE).ok()?;
        host.queue.Signal(&fence, 1).ok()?;
        if fence.GetCompletedValue() < 1 {
            let event = CreateEventW(None, false, false, None).ok()?;
            fence.SetEventOnCompletion(1, event).ok()?;
            WaitForSingleObject(event, INFINITE);
            let _ = CloseHandle(event);
        }
        let mut data = std::ptr::null_mut();
        counter.readback.Map(0, None, Some(&mut data)).ok()?;
        let words = std::slice::from_raw_parts(data as *const u32, 4);
        let out = (u64::from(words[0]), u64::from(words[1]), u64::from(words[2]));
        counter.readback.Unmap(0, None);
        Some(out)
    }
}
