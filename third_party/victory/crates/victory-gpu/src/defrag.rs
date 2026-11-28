//! GPU-side telemetry buffer for `CS_DefragHeap_ComputeRelocations` half-meets
//! (`SLOT_MEET`): three dwords tracking meetings, wait iterations, and wait atomics,
//! and with `VICTORY_MEET_POLLS` a fourth: the waits that gave up at the poll cap.

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
    /// The periodic read ([`log_capped`]) saw `armed` already: the list that zeroes the
    /// buffer has been submitted, so a read list may take it out of UAV state.
    armed_seen: bool,
}

/// The periodic read of the give-up dword with `VICTORY_MEET_POLLS`: its own list and fence,
/// never waited on; the next [`log_capped`] takes the value once the card has done it.
struct CapRead {
    allocator: ID3D12CommandAllocator,
    list: ID3D12GraphicsCommandList,
    fence: ID3D12Fence,
    /// The fence value of the read in flight; 0 before the first.
    sent: u64,
    /// The give-ups said so far.
    said: u32,
}

static CAP_READ: Mutex<Option<CapRead>> = Mutex::new(None);

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
            armed_seen: false,
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
    if meet_cap_on() {
        if let Some(host) = host() {
            log_capped(host);
        }
    }
}

/// `VICTORY_MEET_POLLS` is set (the translator caps the half-meet wait and counts give-ups).
fn meet_cap_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var("VICTORY_MEET_POLLS").is_ok_and(|v| v.trim().parse::<u32>().is_ok_and(|n| n > 0))
    })
}

/// The line for give-ups seen since the last one, or None when there are none new.
fn capped_line(meetings: u32, capped: u32, said: u32) -> Option<String> {
    (capped > said).then(|| {
        format!(
            "half meet: {} waits gave up at the VICTORY_MEET_POLLS cap ({capped} since start, \
             {meetings} meetings): a partner half never came, its values may be wrong",
            capped - said
        )
    })
}

/// Takes the value of the last read if the card has done it and says new give-ups, then
/// sends the next read. Never waits on the card.
fn log_capped(host: &Host) {
    let mut counter_slot = DEFRAG_COUNTER.lock().unwrap_or_else(|e| e.into_inner());
    let Some(counter) = counter_slot.as_mut() else {
        return;
    };
    if !counter.armed_seen {
        counter.armed_seen = counter.armed;
        return;
    }
    let mut slot = CAP_READ.lock().unwrap_or_else(|e| e.into_inner());
    // SAFETY: objects this process owns; the allocator and readback are touched only once
    // the fence says the card finished the read that used them.
    unsafe {
        if slot.is_none() {
            let Ok(allocator) = host
                .device
                .CreateCommandAllocator::<ID3D12CommandAllocator>(D3D12_COMMAND_LIST_TYPE_DIRECT)
            else {
                return;
            };
            let Ok(list) = host.device.CreateCommandList::<_, _, ID3D12GraphicsCommandList>(
                0,
                D3D12_COMMAND_LIST_TYPE_DIRECT,
                &allocator,
                None,
            ) else {
                return;
            };
            let _ = list.Close();
            let Ok(fence) = host.device.CreateFence::<ID3D12Fence>(0, D3D12_FENCE_FLAG_NONE) else {
                return;
            };
            *slot = Some(CapRead { allocator, list, fence, sent: 0, said: 0 });
        }
        let Some(read) = slot.as_mut() else {
            return;
        };
        if read.sent > 0 {
            if read.fence.GetCompletedValue() < read.sent {
                return;
            }
            let mut data = std::ptr::null_mut();
            if counter.readback.Map(0, None, Some(&mut data)).is_ok() {
                let words = std::slice::from_raw_parts(data as *const u32, 4);
                let (meetings, capped) = (words[0], words[3]);
                counter.readback.Unmap(0, None);
                if let Some(line) = capped_line(meetings, capped, read.said) {
                    tracing::warn!("{line}");
                    read.said = capped;
                }
            }
        }
        if read.allocator.Reset().is_err() || read.list.Reset(&read.allocator, None).is_err() {
            return;
        }
        read.list.ResourceBarrier(&[transition(
            &counter.gpu,
            D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
            D3D12_RESOURCE_STATE_COPY_SOURCE,
        )]);
        read.list.CopyBufferRegion(&counter.readback, 0, &counter.gpu, 0, 16);
        read.list.ResourceBarrier(&[transition(
            &counter.gpu,
            D3D12_RESOURCE_STATE_COPY_SOURCE,
            D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
        )]);
        if read.list.Close().is_err() {
            return;
        }
        let Ok(list) = read.list.cast::<ID3D12CommandList>() else {
            return;
        };
        host.queue.ExecuteCommandLists(&[Some(list)]);
        let next = read.sent + 1;
        if host.queue.Signal(&read.fence, next).is_ok() {
            read.sent = next;
        }
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
    let (meetings, polls, atomics, capped) = read.unwrap_or((0, 0, 0, 0));
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
        waits_capped = capped,
        meetings_per_group = format!("{per_group:.2}"),
        iterations_per_meeting = format!("{per_meet:.2}"),
        "CS_DefragHeap_ComputeRelocations counters"
    );
}

fn read_defrag_counters(host: &Host) -> Option<(u64, u64, u64, u64)> {
    let slot = DEFRAG_COUNTER.lock().unwrap_or_else(|e| e.into_inner());
    let counter = slot.as_ref()?;
    if !counter.armed {
        return Some((0, 0, 0, 0));
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
        let out = (
            u64::from(words[0]),
            u64::from(words[1]),
            u64::from(words[2]),
            u64::from(words[3]),
        );
        counter.readback.Unmap(0, None);
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::capped_line;

    #[test]
    fn capped_line_says_only_new_give_ups() {
        assert_eq!(capped_line(10, 0, 0), None);
        assert_eq!(capped_line(10, 3, 3), None);
        let line = capped_line(40, 5, 3).unwrap();
        assert!(line.starts_with("half meet: 2 waits gave up at the VICTORY_MEET_POLLS cap (5 since start, 40 meetings)"), "{line}");
    }
}
