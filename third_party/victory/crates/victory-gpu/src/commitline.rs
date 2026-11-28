//! (El1xr) The process's commit: a `commit:` line beside the ram line, and two switches that
//! hold it down (job `memcut`, all off by default but the line).
//!
//! Windows charges a process's commit for its private memory and also for every allocation in
//! the card's memory and in the PC's memory shared with the card (whatever it may have to page
//! out needs room in the pagefile). Measured in `train_b5i_stale`: private less the card's
//! memory in use less the shared memory in use stayed between 8.3 and 12.4 GB while private
//! went from 11.7 to 36.7 GB; the host copies of textures (the card's memory) and of title
//! buffers (the shared memory) are most of the rest. When the system's free commit runs out
//! (it fell from 20 GB to under 1 GB within 30 s of world entry, with jobs building beside the
//! game), every new allocation waits for the pagefile to grow: frames of 4 to 11 s.
//!
//! - The `commit:` line, every five seconds: the system's commit and its limit, the process's
//!   private, and who holds it.
//! - `VICTORY_UPLOAD_CAP_MB=<MB>`: past that much upload memory alive (committed upload
//!   buffers and blocks of the upload pool), the upload, staging and shared pools keep no
//!   empty block, the staging buffers' pool (`VICTORY_STAGE_POOL_MB`) keeps nothing and lets go
//!   of what it holds, and the pools are trimmed at every line.
//! - `VICTORY_COMMIT_FLOOR_MB=<MB>`: with less free commit than that, host copies of textures
//!   the title fills go after the shortest idle time (`crate::host::vram_idle_ms_by_room`)
//!   and the texture queue makes no new ones (`crate::resource::room`), as when the card is
//!   full.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use windows::Win32::Graphics::Direct3D12::ID3D12Resource;

/// Committed upload buffers alive (`crate::host::Host::upload_buffer` outside the blocks) and
/// their bytes.
static UPLOAD: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

/// Times the cap trimmed the pools, blocks let go by it, and times the floor was found pressed.
static ACTED: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

fn mb_switch(name: &str) -> Option<u64> {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&v| v > 0)
}

/// `VICTORY_UPLOAD_CAP_MB` in bytes, if set.
fn upload_cap() -> Option<u64> {
    static CAP: OnceLock<Option<u64>> = OnceLock::new();
    *CAP.get_or_init(|| {
        let cap = mb_switch("VICTORY_UPLOAD_CAP_MB").map(|mb| mb << 20);
        if let Some(cap) = cap {
            tracing::info!(cap_mb = cap >> 20, "upload memory past the cap keeps no empty block and no pooled staging buffer (VICTORY_UPLOAD_CAP_MB)");
        }
        cap
    })
}

/// `VICTORY_COMMIT_FLOOR_MB` in bytes, if set.
fn commit_floor() -> Option<u64> {
    static FLOOR: OnceLock<Option<u64>> = OnceLock::new();
    *FLOOR.get_or_init(|| {
        let floor = mb_switch("VICTORY_COMMIT_FLOOR_MB").map(|mb| mb << 20);
        if let Some(floor) = floor {
            tracing::info!(floor_mb = floor >> 20, "under this much free commit textures go sooner and the texture queue waits (VICTORY_COMMIT_FLOOR_MB)");
        }
        floor
    })
}

/// Counts a committed upload buffer of `bytes` while it lives.
pub(crate) fn count_upload(buffer: &ID3D12Resource, bytes: u64) {
    UPLOAD[0].fetch_add(1, Relaxed);
    UPLOAD[1].fetch_add(bytes, Relaxed);
    crate::texpool::on_release(buffer, bytes);
}

/// A committed upload buffer of `bytes` counted by [`count_upload`] is gone.
pub(crate) fn upload_gone(bytes: u64) {
    UPLOAD[0].fetch_sub(1, Relaxed);
    UPLOAD[1].fetch_sub(bytes, Relaxed);
}

/// Upload memory alive: committed upload buffers and the upload pool's blocks.
fn upload_bytes() -> u64 {
    UPLOAD[1].load(Relaxed) + crate::texpool::blocks_alive()[4] * crate::texpool::BLOCK_BYTES
}

/// Whether `alive` bytes are past `cap`.
fn over(alive: u64, cap: Option<u64>) -> bool {
    cap.is_some_and(|cap| alive > cap)
}

/// Past `VICTORY_UPLOAD_CAP_MB`: the pools of the PC's memory keep no empty block and the
/// staging buffers' pool nothing.
pub(crate) fn upload_over_cap() -> bool {
    over(upload_bytes(), upload_cap())
}

/// Whether `free` bytes of commit are under `floor`.
fn under(free: u64, floor: Option<u64>) -> bool {
    floor.is_some_and(|floor| free < floor)
}

/// The system's free commit (bytes), asked at most every 100 ms.
fn free_commit() -> u64 {
    static LAST: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];
    let tenth = crate::host::now_ms() / 100 + 1;
    if LAST[0].load(Relaxed) == tenth {
        return LAST[1].load(Relaxed);
    }
    let free = memory_status().ullAvailPageFile;
    LAST[1].store(free, Relaxed);
    LAST[0].store(tenth, Relaxed);
    free
}

/// Under `VICTORY_COMMIT_FLOOR_MB` of free commit.
pub(crate) fn pressed() -> bool {
    let floor = commit_floor();
    if floor.is_none() {
        return false;
    }
    let pressed = under(free_commit(), floor);
    if pressed {
        ACTED[2].fetch_add(1, Relaxed);
    }
    pressed
}

fn memory_status() -> windows_sys::Win32::System::SystemInformation::MEMORYSTATUSEX {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    // SAFETY: plain data the call fills in.
    let mut m: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    m.dwLength = size_of::<MEMORYSTATUSEX>() as u32;
    // SAFETY: a sized structure.
    unsafe { GlobalMemoryStatusEx(&mut m) };
    m
}

/// The system's commit in bytes: (charged, limit, peak).
fn system_commit() -> (u64, u64, u64) {
    use windows_sys::Win32::System::ProcessStatus::{
        K32GetPerformanceInfo, PERFORMANCE_INFORMATION,
    };
    // SAFETY: plain data the call fills in.
    let mut p: PERFORMANCE_INFORMATION = unsafe { std::mem::zeroed() };
    p.cb = size_of::<PERFORMANCE_INFORMATION>() as u32;
    // SAFETY: a sized structure.
    if unsafe { K32GetPerformanceInfo(&mut p, p.cb) } == 0 {
        return (0, 0, 0);
    }
    let page = p.PageSize as u64;
    (
        p.CommitTotal as u64 * page,
        p.CommitLimit as u64 * page,
        p.CommitPeak as u64 * page,
    )
}

/// Bytes the title registered as virtual allocations and as views of its pages.
fn title_bytes() -> (u64, u64) {
    let (mut virtual_bytes, mut views) = (0u64, 0u64);
    for (_, size, kind) in crate::titlemem::snapshot() {
        if kind == crate::titlemem::VIRTUAL {
            virtual_bytes += size as u64;
        } else if kind == crate::titlemem::VIEW {
            views += size as u64;
        }
    }
    (virtual_bytes, views)
}

/// Every five seconds, from the ram line: the line, and the cap's trim.
pub(crate) fn log(card: [u64; 4], private: u64, peak_private: u64) {
    let trimmed = if upload_over_cap() {
        ACTED[0].fetch_add(1, Relaxed);
        let n = crate::texpool::trim_host_pools() + crate::texture::trim_stage_pool();
        ACTED[1].fetch_add(n, Relaxed);
        n
    } else {
        0
    };
    let mb = |v: u64| v >> 20;
    let (charged, limit, peak) = system_commit();
    let m = memory_status();
    let blocks = crate::texpool::blocks_alive();
    let block_mb = |i: usize| mb(blocks[i] * crate::texpool::BLOCK_BYTES);
    let copies = crate::shadow::memory();
    let staged = crate::texture::memory();
    let htex = crate::resource::HOST_TEXTURES[1].load(Relaxed);
    let filled = crate::resource::KINDS[1].load(Relaxed);
    let card_copies = crate::shadow::card_copy_bytes()[0];
    let (title_virtual, title_views) = title_bytes();
    let k = crate::pipeline::kept();
    let allocators = crate::alloc_count::counts();
    // What the system charges for the card's memory and the shared memory is in private.
    let rest = private
        .saturating_sub(card[1])
        .saturating_sub(card[3])
        .saturating_sub(title_virtual);
    tracing::info!(
        "commit: system {} MB of {} MB (peak {} MB), free {} MB, PC free {} MB; process private {} MB (peak {} MB), of it: \
         the card's memory in use {} MB (host textures {} MB, of them title texture host copies {} MB; card copies of title buffers {} MB; texture blocks {} MB, card buffer blocks {} MB); \
         the PC's memory shared with the card {} MB (title memory host copies {} MB, shared blocks {} MB; upload memory {} MB: committed upload buffers {} {} MB with staging blocks {} MB, upload blocks {} MB, staging buffers' pool kept {} MB; texture staging alive {} MB); \
         title virtual allocations {} MB (views of title pages {} MB, mapped, outside private); \
         the rest {} MB (driver, heaps: pipeline library file {} MB, translated shaders {} MB, containers {} MB, command allocators {}); \
         VICTORY_UPLOAD_CAP_MB {} (over now {}, trims {}, blocks and buffers let go {}, now {}), VICTORY_COMMIT_FLOOR_MB {} (pressed {} asks)",
        mb(charged),
        mb(limit),
        mb(peak),
        mb(m.ullAvailPageFile),
        mb(m.ullAvailPhys),
        mb(private),
        mb(peak_private),
        mb(card[1]),
        mb(htex),
        mb(filled),
        mb(card_copies),
        block_mb(0),
        block_mb(2),
        mb(card[3]),
        mb(copies[1]),
        block_mb(3),
        mb(upload_bytes()),
        UPLOAD[0].load(Relaxed),
        mb(UPLOAD[1].load(Relaxed)),
        block_mb(1),
        block_mb(4),
        mb(crate::texture::stage_pool_bytes()),
        mb(staged[1]),
        mb(title_virtual),
        mb(title_views),
        mb(rest),
        mb(k[4]),
        mb(k[3]),
        mb(k[1]),
        allocators.0,
        upload_cap().map_or(0, mb),
        upload_over_cap(),
        ACTED[0].load(Relaxed),
        ACTED[1].load(Relaxed),
        trimmed,
        commit_floor().map_or(0, mb),
        ACTED[2].load(Relaxed),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The cap and the floor act only when set, and only past them.
    #[test]
    fn cap_and_floor_act_only_past_their_values() {
        assert!(!over(u64::MAX, None));
        assert!(!over(512 << 20, Some(512 << 20)));
        assert!(over((512 << 20) + 1, Some(512 << 20)));
        assert!(!under(0, None));
        assert!(!under(4 << 30, Some(4 << 30)));
        assert!(under((4 << 30) - 1, Some(4 << 30)));
    }

    /// The system's commit is read, its limit above what is charged.
    #[test]
    fn system_commit_is_read() {
        let (charged, limit, peak) = system_commit();
        assert!(charged > 0 && limit >= charged && peak >= charged);
        assert!(memory_status().ullAvailPageFile <= limit);
    }
}
