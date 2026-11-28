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
//!   of what it holds, and the pools are trimmed at every line. Staging buffers of textures
//!   go once out of use 2 s instead of 10 s ([`staging_idle_ms`]).
//! - `VICTORY_COMMIT_FLOOR_MB=<MB>`: with less free commit than that, host copies of textures
//!   the title fills that no slot and no list holds go once out of use 2.5 s and 60 frames
//!   (instead of 5 s; `crate::resource::let_go_idle`), each one logged, and staging buffers
//!   as past the cap. It never refuses a new host copy: the first version also closed the
//!   texture queue (`crate::resource::room`) and cut the idle time to 1 s, and the diner went
//!   dark after world entry (`train_b5k_single/02_mem`: textures a draw needed were never
//!   made while free commit stayed under the floor for 50 s).

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use victory_core::settings::Live;
use windows::Win32::Graphics::Direct3D12::ID3D12Resource;

/// Committed upload buffers alive (`crate::host::Host::upload_buffer` outside the blocks) and
/// their bytes.
static UPLOAD: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

/// Times the cap trimmed the pools, blocks let go by it, idle sweeps under the floor, host
/// copies of textures let go by the floor alone and their bytes, sweeps of staging buffers
/// shortened by the cap or the floor.
static ACTED: [AtomicU64; 6] = [const { AtomicU64::new(0) }; 6];

/// Under the floor, host copies of textures go once out of use this long (never sooner)...
const FLOOR_IDLE_MS: u64 = 2_500;
/// ...and this many frames (at 20 to 30 frames a second, 2 to 3 s).
const FLOOR_IDLE_FRAMES: u64 = 60;

/// A size in MB as bytes; unset, unparsed or 0: 0 (off).
fn mb_bits(value: Option<&str>) -> u64 {
    value.and_then(|v| v.trim().parse::<u64>().ok()).map_or(0, |mb| mb.saturating_mul(1 << 20))
}

static CAP: Live = Live::new("VICTORY_UPLOAD_CAP_MB", mb_bits);
static FLOOR: Live = Live::new("VICTORY_COMMIT_FLOOR_MB", mb_bits);

/// `VICTORY_UPLOAD_CAP_MB` in bytes, if set (live).
fn upload_cap() -> Option<u64> {
    Some(CAP.get()).filter(|&v| v > 0)
}

/// `VICTORY_COMMIT_FLOOR_MB` in bytes, if set (live).
fn commit_floor() -> Option<u64> {
    Some(FLOOR.get()).filter(|&v| v > 0)
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
fn pressed() -> bool {
    let floor = commit_floor();
    floor.is_some() && under(free_commit(), floor)
}

/// The floor's idle time and frames for an idle sweep's `ms` and `frames`: shorter only where
/// `ms` is longer than its own, and then never fewer frames than its own.
fn floor_times(ms: u64, frames: u64) -> Option<(u64, u64)> {
    (ms > FLOOR_IDLE_MS).then_some((FLOOR_IDLE_MS, frames.max(FLOOR_IDLE_FRAMES)))
}

/// For the idle sweep of host copies of textures the title fills (`crate::resource::let_go_idle`),
/// given its usual idle time and frames: those to use, and whether the floor shortened them.
/// Under the floor the pools of the PC's memory are also trimmed, once a second at most.
pub(crate) fn floor_idle(ms: u64, frames: u64) -> (u64, u64, bool) {
    if !pressed() {
        return (ms, frames, false);
    }
    ACTED[2].fetch_add(1, Relaxed);
    match floor_times(ms, frames) {
        Some((ms, frames)) => (ms, frames, true),
        None => (ms, frames, false),
    }
}

/// Staging buffers of textures are kept this long out of use past the cap or under the floor.
const STAGING_IDLE_MS: u64 = 2_000;

/// The idle time of the staging buffers of textures (`crate::texture::let_go_idle`), given the
/// usual one (10 s with 4 GB of RAM free): past `VICTORY_UPLOAD_CAP_MB` or under
/// `VICTORY_COMMIT_FLOOR_MB`, 2 s. Only the texture's next upload pays (it detiles into a new
/// one); its card copy stays. In `train_b5k_single/02_mem` they held 2.3 GB of upload memory
/// (27 of the staging pool's 64 MB blocks) past the 1 GB cap: the cap let go of empty blocks
/// alone, and the 10 s follows free RAM (30 GB), not commit.
pub(crate) fn staging_idle_ms(ms: u64) -> u64 {
    if ms > STAGING_IDLE_MS && (upload_over_cap() || pressed()) {
        ACTED[5].fetch_add(1, Relaxed);
        return STAGING_IDLE_MS;
    }
    ms
}

/// How long ago a [`crate::host::moment`] was: milliseconds and frames.
fn idle_of(moment: u64) -> (u64, u64) {
    let now = crate::host::moment();
    let tenths = (now & 0xffff_ffff).saturating_sub(moment & 0xffff_ffff);
    (tenths * 100, (now >> 32).saturating_sub(moment >> 32))
}

/// The host copies of textures (title texture, bytes, last use as a moment) an idle sweep let
/// go only because of the floor: counted, and a line naming the first few.
pub(crate) fn floor_report(went: &[(usize, u64, u64)]) {
    if went.is_empty() {
        return;
    }
    let bytes: u64 = went.iter().map(|&(_, b, _)| b).sum();
    ACTED[3].fetch_add(went.len() as u64, Relaxed);
    ACTED[4].fetch_add(bytes, Relaxed);
    let first: Vec<String> = went
        .iter()
        .take(4)
        .map(|&(at, b, used)| {
            let (ms, frames) = idle_of(used);
            format!("{at:#x} {} KiB idle {ms} ms {frames} frames", b >> 10)
        })
        .collect();
    tracing::info!(
        "commit floor: free {} MB under {} MB: let go {} host copies of textures ({} MB) out of use {FLOOR_IDLE_MS} ms and {FLOOR_IDLE_FRAMES} frames that the usual time keeps, none held by a slot or a list; first: {}; since the start {} ({} MB)",
        free_commit() >> 20,
        commit_floor().unwrap_or(0) >> 20,
        went.len(),
        bytes >> 20,
        first.join(", "),
        ACTED[3].load(Relaxed),
        ACTED[4].load(Relaxed) >> 20,
    );
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
    let usage = crate::texpool::pool_usage();
    let pools = ["texture", "staging", "card", "shared", "upload"]
        .iter()
        .zip(usage)
        .enumerate()
        .map(|(i, (name, [held, cuttable]))| {
            format!("{name} {}/{}/{}", block_mb(i), mb(held), mb(cuttable))
        })
        .collect::<Vec<_>>()
        .join(", ");
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
         blocks MB/held by pieces MB/still cuttable MB: {}; \
         VICTORY_UPLOAD_CAP_MB {} (over now {}, trims {}, blocks and buffers let go {}, now {}), VICTORY_COMMIT_FLOOR_MB {} (pressed {} sweeps, host copies let go {} ({} MB)), staging sweeps at 2 s {}",
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
        pools,
        upload_cap().map_or(0, mb),
        upload_over_cap(),
        ACTED[0].load(Relaxed),
        ACTED[1].load(Relaxed),
        trimmed,
        commit_floor().map_or(0, mb),
        ACTED[2].load(Relaxed),
        ACTED[3].load(Relaxed),
        mb(ACTED[4].load(Relaxed)),
        ACTED[5].load(Relaxed),
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

    /// The floor shortens only a longer idle time, to its own, with at least its frames: a
    /// copy a draw used in the last 2.5 s or 60 frames is never let go by it.
    #[test]
    fn the_floor_never_lets_go_of_a_copy_used_lately() {
        assert_eq!(floor_times(5_000, 4), Some((FLOOR_IDLE_MS, FLOOR_IDLE_FRAMES)));
        assert_eq!(floor_times(5_000, 200), Some((FLOOR_IDLE_MS, 200)));
        assert_eq!(floor_times(FLOOR_IDLE_MS, 4), None);
        assert_eq!(floor_times(1_000, 4), None);
        assert!(FLOOR_IDLE_MS >= 2_500 && FLOOR_IDLE_FRAMES >= 60);
    }

    /// Both switches are live: a launch can A/B them, and off is 0.
    #[test]
    fn the_switches_are_live_and_off_by_default() {
        use victory_core::settings::check_live;
        assert_eq!(mb_bits(None), 0);
        assert_eq!(mb_bits(Some(" 512 ")), 512 << 20);
        assert_eq!(mb_bits(Some("x")), 0);
        check_live("VICTORY_UPLOAD_CAP_MB", "1024", upload_cap, None, Some(1 << 30));
        check_live("VICTORY_COMMIT_FLOOR_MB", "4096", commit_floor, None, Some(4 << 30));
    }

    /// The system's commit is read, its limit above what is charged.
    #[test]
    fn system_commit_is_read() {
        let (charged, limit, peak) = system_commit();
        assert!(charged > 0 && limit >= charged && peak >= charged);
        assert!(memory_status().ullAvailPageFile <= limit);
    }
}
