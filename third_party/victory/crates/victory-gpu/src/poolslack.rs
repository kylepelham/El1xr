//! (El1xr) `VICTORY_POOL_SLACK_MB=<MB>` (live, off by default; job `memcut`): under that much
//! free system commit, the host pools (`crate::texpool`: texture, staging, card, shared,
//! upload) keep one empty 64 MiB block each instead of 4 to 16.
//!
//! A block is charged to commit whole while any piece of it lives. The commit line of
//! `train_b5q/01_bo` (launcher set, card block reuse on) puts 2.2 GB of the pools' blocks out of
//! any piece at the run's lowest free commit (295 MB: staging 1113, upload 497, card 331,
//! shared 313 MB) and 3.4 GB at its end (shared 1658, card 685, staging 570, upload 468 MB).
//! Of it, empty blocks: the card, shared and upload pools keep and make ahead up to 4 each,
//! the staging pool keeps up to 8 and the texture pool up to 16 (while the card is under 85%
//! of its budget): about 1.3 GB with the texture pool off, as the launcher has it. The rest is
//! holes between pieces in use, which no block choice removes: in a simulated churn of mixed
//! sizes and lifetimes (written while making this), cutting from the fullest block with room
//! held 122 blocks in the quiet stretches, from the oldest 112, from the first 111; so this
//! switch leaves the choice as it is.
//!
//! Under the free commit set, each pool keeps and makes ahead at most [`KEEP`] empty blocks,
//! and at every commit line (5 s) every pool frees its rested pieces and lets go of its empty
//! blocks past [`KEEP`]; a trim that let go of any logs them with the process's private bytes
//! and the system's free commit before and after (`pool slack:`). The commit line names every
//! pool's empty blocks, switch set or not. Nothing a piece holds changes: pieces are cut,
//! zeroed and rested as before, and only empty blocks go, as before. Not a shader or pipeline
//! key.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use victory_core::settings::Live;

/// Empty blocks a pool keeps (and makes ahead) under the free commit set: one, so that the
/// block a piece is cut from has stood while the lists before it ran (`VICTORY_RIPE_BLOCKS`).
pub(crate) const KEEP: usize = 1;

static SLACK: Live = Live::new("VICTORY_POOL_SLACK_MB", mb_bits);

/// Trims, blocks let go by them, their bytes.
static ACTED: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

fn mb_bits(value: Option<&str>) -> u64 {
    value
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map_or(0, |mb| mb.saturating_mul(1 << 20))
}

/// `VICTORY_POOL_SLACK_MB` in bytes, if set (live).
fn slack() -> Option<u64> {
    Some(SLACK.get()).filter(|&v| v > 0)
}

/// Whether `free` bytes of commit are under `floor`.
fn under(free: u64, floor: Option<u64>) -> bool {
    floor.is_some_and(|floor| free < floor)
}

/// Set, and the system's free commit is under it.
pub(crate) fn pressed() -> bool {
    let floor = slack();
    floor.is_some() && under(crate::commitline::free_commit(), floor)
}

/// Empty blocks a pool that keeps `n` keeps now: at most [`KEEP`] while [`pressed`].
pub(crate) fn spares(n: usize) -> usize {
    kept(n, pressed())
}

fn kept(n: usize, pressed: bool) -> usize {
    if pressed { n.min(KEEP) } else { n }
}

/// The process's private bytes (what Windows charges it).
pub(crate) fn private_bytes() -> u64 {
    use windows_sys::Win32::System::ProcessStatus::{
        K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let mut c = PROCESS_MEMORY_COUNTERS {
        cb: size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        ..Default::default()
    };
    // SAFETY: our own process and a sized structure.
    unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) };
    c.PagefileUsage as u64
}

const NAMES: [&str; 5] = ["texture", "staging", "card", "shared", "upload"];

/// At every commit line: while [`pressed`], every pool is trimmed to [`KEEP`] empty blocks,
/// and a trim that let go of any logs what it did to commit.
pub(crate) fn tick() {
    if !pressed() {
        return;
    }
    let mb = |v: u64| v >> 20;
    let before = (
        private_bytes(),
        crate::commitline::memory_status().ullAvailPageFile,
    );
    let gone = crate::texpool::trim_slack(KEEP);
    let blocks: u64 = gone.iter().sum();
    ACTED[0].fetch_add(1, Relaxed);
    if blocks == 0 {
        return;
    }
    let bytes = blocks * crate::texpool::BLOCK_BYTES;
    ACTED[1].fetch_add(blocks, Relaxed);
    ACTED[2].fetch_add(bytes, Relaxed);
    let after = (
        private_bytes(),
        crate::commitline::memory_status().ullAvailPageFile,
    );
    let each = NAMES
        .iter()
        .zip(gone)
        .map(|(name, n)| format!("{name} {n}"))
        .collect::<Vec<_>>()
        .join(", ");
    tracing::info!(
        "pool slack: free commit {} MB under {} MB: let go {blocks} empty blocks ({} MB): {each}; process private {} -> {} MB, free commit {} -> {} MB; since the start {} blocks ({} MB) in {} trims",
        mb(before.1),
        mb(slack().unwrap_or(0)),
        mb(bytes),
        mb(before.0),
        mb(after.0),
        mb(before.1),
        mb(after.1),
        ACTED[1].load(Relaxed),
        mb(ACTED[2].load(Relaxed)),
        ACTED[0].load(Relaxed),
    );
}

/// The commit line's part: the switch, what it did, and every pool's empty blocks now.
pub(crate) fn summary() -> String {
    let empty = crate::texpool::empty_blocks();
    let each = NAMES
        .iter()
        .zip(empty)
        .map(|(name, n)| format!("{name} {n}"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "VICTORY_POOL_SLACK_MB {} (pressed now {}, trims {}, empty blocks let go {} ({} MB)), empty blocks now: {each}",
        slack().map_or(0, |v| v >> 20),
        pressed(),
        ACTED[0].load(Relaxed),
        ACTED[1].load(Relaxed),
        ACTED[2].load(Relaxed) >> 20,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Live and off by default; under its free commit a pool keeps at most one empty block.
    #[test]
    fn the_switch_is_live_and_keeps_one_block_only_under_its_free_commit() {
        use victory_core::settings::check_live;
        assert_eq!(mb_bits(None), 0);
        assert_eq!(mb_bits(Some(" 4096 ")), 4 << 30);
        check_live("VICTORY_POOL_SLACK_MB", "4096", slack, None, Some(4 << 30));
        assert!(!under(1, None));
        assert!(!under(4 << 30, Some(4 << 30)));
        assert!(under((4 << 30) - 1, Some(4 << 30)));
        assert_eq!((kept(8, false), kept(4, false)), (8, 4));
        assert_eq!(
            (kept(8, true), kept(4, true), kept(0, true)),
            (KEEP, KEEP, 0)
        );
        assert_eq!(
            KEEP, 1,
            "one block made ahead stays: pieces come from ripe blocks"
        );
    }
}
