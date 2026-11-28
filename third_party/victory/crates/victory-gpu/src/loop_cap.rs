//! `VICTORY_LOOP_CAP_WATCH=1` (diag): translated programs count the lanes their loops stop at the
//! translator's iteration limits (128 pixel, 256 vertex and mesh, 4096 compute, 1024 ray walks,
//! or `VICTORY_LOOP_ROOM`) in a buffer of ours (`victory_shader::emit::SLOT_LOOP_CAP`, two dwords
//! a program: ended, kept). This side numbers the programs as their draws bind them
//! (`USER_LOOP_CAP_ID`), binds the buffer and reads it back with the periodic meter line, never
//! waiting on the card, then logs the programs whose counts grew since the last read:
//!
//! `loop cap: <hash> <stage> hits=<n> ended=<e> kept=<k> since_start=<t> <name>`
//!
//! `ended`: lanes that left the program at the limit (a pixel discarded: black or missing pixels;
//! a compute thread's later stores skipped). `kept`: lanes that left the loop at the limit by its
//! own exit with what they had computed (a loop closed by a branch, or `VICTORY_LOOP_KEEP=1`).

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use victory_shader::emit::LOOP_CAP_PROGRAMS;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;
use windows::core::Interface;

use crate::host::{Host, host};

/// Bytes of the buffer: two dwords a program.
const BYTES: u64 = LOOP_CAP_PROGRAMS as u64 * 8;
/// Programs a read names at most.
const TOP: usize = 12;

/// The programs by number (number `i + 1` at `i`): stage, hash, name.
static PROGRAMS: Mutex<Vec<(&'static str, String, String)>> = Mutex::new(Vec::new());

/// The buffer's GPU address once made (0: not yet).
static VA: AtomicU64 = AtomicU64::new(0);

struct Watch {
    gpu: ID3D12Resource,
    readback: ID3D12Resource,
    allocator: ID3D12CommandAllocator,
    list: ID3D12GraphicsCommandList,
    fence: ID3D12Fence,
    /// The fence value of the read in flight; 0 before the first.
    sent: u64,
    /// The dwords of the last read taken.
    seen: Vec<u32>,
}

static WATCH: Mutex<Option<Watch>> = Mutex::new(None);

/// The number of a program in the buffer, given on its first bind (0: past the last).
pub(crate) fn register(stage: &'static str, hash: &str, name: Option<&str>) -> u32 {
    let mut programs = PROGRAMS.lock().unwrap_or_else(|e| e.into_inner());
    if programs.len() + 1 >= LOOP_CAP_PROGRAMS as usize {
        return 0;
    }
    programs.push((stage, hash.to_owned(), name.unwrap_or("").to_owned()));
    programs.len() as u32
}

fn buffer(
    device: &ID3D12Device,
    kind: D3D12_HEAP_TYPE,
    flags: D3D12_RESOURCE_FLAGS,
    state: D3D12_RESOURCE_STATES,
) -> windows::core::Result<ID3D12Resource> {
    let mut made: Option<ID3D12Resource> = None;
    // SAFETY: a plain buffer description; the caller keeps the buffer.
    unsafe {
        device.CreateCommittedResource(
            &D3D12_HEAP_PROPERTIES {
                Type: kind,
                ..Default::default()
            },
            D3D12_HEAP_FLAG_NONE,
            &D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                Width: BYTES,
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

fn make(host: &Host) -> windows::core::Result<Watch> {
    // A committed buffer starts zeroed, in the common state: each list's first use promotes it
    // to unordered access, and it decays back when the list is done, so the read needs no
    // barrier.
    let gpu = buffer(
        &host.device,
        D3D12_HEAP_TYPE_DEFAULT,
        D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
        D3D12_RESOURCE_STATE_COMMON,
    )?;
    let readback = buffer(
        &host.device,
        D3D12_HEAP_TYPE_READBACK,
        D3D12_RESOURCE_FLAG_NONE,
        D3D12_RESOURCE_STATE_COPY_DEST,
    )?;
    // SAFETY: plain object creation.
    unsafe {
        let allocator: ID3D12CommandAllocator = host
            .device
            .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)?;
        crate::alloc_count::track(&allocator);
        let list: ID3D12GraphicsCommandList =
            host.device
                .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)?;
        list.Close()?;
        let fence: ID3D12Fence = host.device.CreateFence(0, D3D12_FENCE_FLAG_NONE)?;
        Ok(Watch {
            gpu,
            readback,
            allocator,
            list,
            fence,
            sent: 0,
            seen: vec![0; LOOP_CAP_PROGRAMS as usize * 2],
        })
    }
}

/// The buffer's GPU address for a root UAV, made on the first call.
pub(crate) fn bind() -> Option<u64> {
    let va = VA.load(Ordering::Acquire);
    if va != 0 {
        return Some(va);
    }
    let host = host()?;
    let mut slot = WATCH.lock().unwrap_or_else(|e| e.into_inner());
    if slot.is_none() {
        match make(host) {
            Ok(w) => *slot = Some(w),
            Err(e) => {
                tracing::warn!(error = %e, "loop cap: the counter buffer could not be made");
                return None;
            }
        }
    }
    // SAFETY: a live buffer.
    let va = unsafe { slot.as_ref()?.gpu.GetGPUVirtualAddress() };
    VA.store(va, Ordering::Release);
    Some(va)
}

/// One program's line: counts grown since the last read.
#[derive(Debug, PartialEq, Eq)]
struct Grown {
    id: usize,
    ended: u32,
    kept: u32,
    since_start: u64,
}

/// The programs whose counts grew from `seen` to `now`, most first, and the totals grown
/// (ended, kept). Number 0 (programs past the last) is listed too.
fn grown(seen: &[u32], now: &[u32]) -> (Vec<Grown>, u64, u64) {
    let mut out = Vec::new();
    let (mut ended, mut kept) = (0u64, 0u64);
    for (id, (s, n)) in seen.chunks_exact(2).zip(now.chunks_exact(2)).enumerate() {
        let e = n[0].wrapping_sub(s[0]);
        let k = n[1].wrapping_sub(s[1]);
        if e == 0 && k == 0 {
            continue;
        }
        ended += u64::from(e);
        kept += u64::from(k);
        out.push(Grown {
            id,
            ended: e,
            kept: k,
            since_start: u64::from(n[0]) + u64::from(n[1]),
        });
    }
    out.sort_by_key(|g| std::cmp::Reverse(u64::from(g.ended) + u64::from(g.kept)));
    (out, ended, kept)
}

/// The log line of one program.
fn line(g: &Grown, program: Option<&(&'static str, String, String)>) -> String {
    let (stage, hash, name) = match program {
        Some((stage, hash, name)) => (*stage, hash.as_str(), name.as_str()),
        None => ("-", "past_the_last", ""),
    };
    format!(
        "loop cap: {hash} {stage} hits={} ended={} kept={} since_start={} {name}",
        u64::from(g.ended) + u64::from(g.kept),
        g.ended,
        g.kept,
        g.since_start
    )
    .trim_end()
    .to_owned()
}

/// With the periodic meter line: takes the last read if the card has done it and logs the
/// programs whose counts grew, then sends the next read. Never waits on the card; nothing
/// before the first bind.
pub(crate) fn log() {
    if VA.load(Ordering::Acquire) == 0 {
        return;
    }
    let Some(host) = host() else {
        return;
    };
    let mut slot = WATCH.lock().unwrap_or_else(|e| e.into_inner());
    let Some(w) = slot.as_mut() else {
        return;
    };
    // SAFETY: objects this process owns; the allocator and read-back buffer are touched only
    // once the fence says the card finished the read that used them.
    unsafe {
        if w.sent > 0 {
            if w.fence.GetCompletedValue() < w.sent {
                return;
            }
            let mut data = std::ptr::null_mut();
            if w.readback.Map(0, None, Some(&mut data)).is_ok() {
                let now = std::slice::from_raw_parts(data as *const u32, w.seen.len()).to_vec();
                w.readback.Unmap(0, None);
                let (list, ended, kept) = grown(&w.seen, &now);
                if !list.is_empty() {
                    let programs = PROGRAMS.lock().unwrap_or_else(|e| e.into_inner());
                    tracing::warn!(
                        "loop cap: frame {} programs={} hits={} ended={ended} kept={kept} \
                         (lanes stopped at a loop's iteration limit since the last read)",
                        crate::camera::frame(),
                        list.len(),
                        ended + kept
                    );
                    for g in list.iter().take(TOP) {
                        let program = g.id.checked_sub(1).and_then(|i| programs.get(i));
                        tracing::warn!("{}", line(g, program));
                    }
                }
                w.seen = now;
            }
        }
        if w.allocator.Reset().is_err() || w.list.Reset(&w.allocator, None).is_err() {
            return;
        }
        w.list.CopyBufferRegion(&w.readback, 0, &w.gpu, 0, BYTES);
        if w.list.Close().is_err() {
            return;
        }
        let Ok(list) = w.list.cast::<ID3D12CommandList>() else {
            return;
        };
        crate::host::send_thread::wait_idle();
        host.queue.ExecuteCommandLists(&[Some(list)]);
        let next = w.sent + 1;
        if host.queue.Signal(&w.fence, next).is_ok() {
            w.sent = next;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Grown, grown, line};

    #[test]
    fn only_grown_programs_are_listed_most_first() {
        let seen = [0, 0, 5, 1, 0, 0, 7, 7];
        let now = [0, 0, 5, 4, 10, 2, 7, 7];
        let (list, ended, kept) = grown(&seen, &now);
        assert_eq!((ended, kept), (10, 5));
        assert_eq!(
            list,
            vec![
                Grown {
                    id: 2,
                    ended: 10,
                    kept: 2,
                    since_start: 12
                },
                Grown {
                    id: 1,
                    ended: 0,
                    kept: 3,
                    since_start: 9
                },
            ]
        );
        let program = (
            "pixel",
            "677ea27c05f1a634".to_owned(),
            "TL_PS_SSS".to_owned(),
        );
        assert_eq!(
            line(&list[0], Some(&program)),
            "loop cap: 677ea27c05f1a634 pixel hits=12 ended=10 kept=2 since_start=12 TL_PS_SSS"
        );
        assert_eq!(
            line(&list[1], None),
            "loop cap: past_the_last - hits=3 ended=0 kept=3 since_start=9"
        );
    }
}
