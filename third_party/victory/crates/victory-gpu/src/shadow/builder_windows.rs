//! (El1xr) `VICTORY_BVH_CARD_WINDOWS_MB=<MB>` (off by default; only with
//! `VICTORY_BVH_BUILDER=1`): windows outside buffers go to the card up to
//! this many MB more than `window_card_most` gives them otherwise.
//!
//! The structure builder and the traces read and write title memory by
//! address (`crate::memory`): every list with a builder dispatch declares
//! every numbered copy written. A window with a card side is compared on the
//! card (`Host::card_before`) and only the pieces that changed come back. A
//! window without one is snapshotted on the CPU at each such submission and
//! compared on the CPU when it lands. Batch 5 part F (R3, RTX 5090): the
//! windows grew from 1.7 GB to 6-8 GB while the launcher's
//! `VICTORY_CARD_WINDOWS_MB=1536` put 1.5 GB of them on the card. Per frame,
//! 9.7 GB were snapshotted (`snapshots` 580-770 ms of preparing) and 9.7-13 GB
//! compared (the relay 690 ms landing), with the card about 5 % busy. That is
//! where the 0.75 fps went.
//!
//! Batch 5 part I (8192 MB): 5.4 fps, snapshots near 0, then the PC ran out
//! of commit at about 70 s (card copies 10.3 GB, private bytes 40 GB, free
//! commit 1.5 GB, then 5 MB at the crash): every allocation failed with
//! `E_OUTOFMEMORY` and the next thread that needed a new page of its stack
//! died of a stack overflow. Memory on the card counts against the PC's
//! commit too. So a window gets a card side beyond `window_card_most`'s own
//! total only while ([`admits`]):
//! - the card's use stays under eight tenths of its budget
//!   (`placement::window_line` without room kept: the texture queue keeps
//!   no room for these windows, `placement::kept_room` caps windows at what
//!   was seen without the builder);
//! - the PC keeps at least `VICTORY_BVH_COMMIT_FLOOR_MB` (4,096 by default)
//!   left to commit after it.
//!
//! A window turned down is made as before (in the PC's memory, snapshotted
//! at builder submissions): slower, not broken. With
//! `VICTORY_BVH_DECLARE_OUTPUTS=1` (`crate::bvh_declare`) a window the
//! builder only reads gets none past the total either: nothing compares it.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

const MIB: u64 = 1 << 20;

/// The commit the PC keeps free by default, in MiB
/// (`VICTORY_BVH_COMMIT_FLOOR_MB`).
const COMMIT_FLOOR_MB: u64 = 4096;

/// The bytes the switch adds: `switch_mb` MB with the builder on, else none.
fn extra(switch_mb: Option<u64>, builder: bool) -> u64 {
    match switch_mb {
        Some(mb) if builder => mb.saturating_mul(MIB),
        _ => 0,
    }
}

/// The bytes of windows the card holds beyond `window_card_most`'s own
/// total ([`extra`] of the switches), read once and said once.
pub(super) fn more() -> u64 {
    static MORE: OnceLock<u64> = OnceLock::new();
    *MORE.get_or_init(|| {
        let mb = std::env::var("VICTORY_BVH_CARD_WINDOWS_MB")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok());
        let builder = std::env::var("VICTORY_BVH_BUILDER").is_ok_and(|v| v.trim() == "1");
        let more = extra(mb, builder);
        if more > 0 {
            tracing::info!(
                "the structure builder's windows: windows outside buffers on the card up to {} MB more \
                 (VICTORY_BVH_CARD_WINDOWS_MB): their shader writes are compared on the card, not snapshotted; \
                 only while the card stays under eight tenths of its budget and the PC keeps {} MB to commit \
                 (VICTORY_BVH_COMMIT_FLOOR_MB)",
                more >> 20,
                floor() >> 20
            );
        } else if mb.is_some_and(|mb| mb > 0) {
            tracing::info!("VICTORY_BVH_CARD_WINDOWS_MB is set but the structure builder is off: windows placed as before");
        }
        more
    })
}

/// The bytes the PC keeps left to commit (`VICTORY_BVH_COMMIT_FLOOR_MB`).
fn floor() -> u64 {
    static FLOOR: OnceLock<u64> = OnceLock::new();
    *FLOOR.get_or_init(|| {
        std::env::var("VICTORY_BVH_COMMIT_FLOOR_MB")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(COMMIT_FLOOR_MB)
            .saturating_mul(MIB)
    })
}

/// Why a window got no card side beyond the windows' own total.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum No {
    /// The card would pass eight tenths of its budget.
    Card,
    /// The PC would have less than the floor left to commit.
    Commit,
}

/// Whether a window of `size` bytes whose card side would pass the windows'
/// own total gets one: `card` is the card's `[budget, used]`, `free_commit`
/// what the PC has left to commit, `floor` what it keeps. Unknown numbers
/// do not turn it down.
fn judge(
    size: u64,
    card: Option<[u64; 2]>,
    free_commit: Option<u64>,
    floor: u64,
) -> Result<(), No> {
    if card.is_some_and(|[budget, used]| used.saturating_add(size) > budget / 10 * 8) {
        return Err(No::Card);
    }
    if free_commit.is_some_and(|free| free < size.saturating_add(floor)) {
        return Err(No::Commit);
    }
    Ok(())
}

/// What the PC has left to commit, in bytes.
fn free_commit() -> Option<u64> {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    // SAFETY: plain data the call fills in.
    let mut m: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    m.dwLength = size_of::<MEMORYSTATUSEX>() as u32;
    // SAFETY: a sized structure.
    (unsafe { GlobalMemoryStatusEx(&mut m) } != 0).then_some(m.ullAvailPageFile)
}

/// Windows turned down for the card's budget and for the PC's commit, and
/// their bytes.
static TURNED_DOWN: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

/// Whether the window of `size` bytes at `start` gets a card side that would
/// take the windows past their own total (`base`, held `held` bytes before
/// it), with the card's numbers `card` (`[budget, used]`). Within the total:
/// always (the caller's own line holds). Past it, not for a window the
/// builder only reads (`crate::bvh_declare::card_side`). Turned down windows
/// are counted and said (the first 16, then every 256th).
pub(super) fn admits(start: u64, size: u64, base: u64, held: u64, card: Option<[u64; 2]>) -> bool {
    if held.saturating_add(size) <= base {
        return true;
    }
    if !crate::bvh_declare::card_side(start, size) {
        return false;
    }
    let free = free_commit();
    let Err(why) = judge(size, card, free, floor()) else {
        return true;
    };
    let at = if why == No::Card { 0 } else { 2 };
    let n = TURNED_DOWN[at].fetch_add(1, Ordering::Relaxed) + 1;
    let bytes = TURNED_DOWN[at + 1].fetch_add(size, Ordering::Relaxed) + size;
    if n <= 16 || n.is_multiple_of(256) {
        tracing::info!(
            "the structure builder's windows: no card side for a window of {} KiB ({}): the card {} MB in use of {} MB, \
             the PC {} MB left to commit (keeps {} MB); windows on the card {} MB of {} MB; {} turned down so far for it ({} MB)",
            size >> 10,
            if why == No::Card {
                "the card past eight tenths"
            } else {
                "the PC's commit"
            },
            card.map_or(0, |[_, used]| used >> 20),
            card.map_or(0, |[budget, _]| budget >> 20),
            free.map_or(0, |f| f >> 20),
            floor() >> 20,
            held >> 20,
            base.saturating_add(more()) >> 20,
            n,
            bytes >> 20
        );
    }
    false
}

#[cfg(test)]
mod tests {
    use super::{MIB, No, extra, judge};

    #[test]
    fn adds_only_with_the_builder_on() {
        assert_eq!(extra(Some(8192), true), 8192 << 20);
        assert_eq!(extra(Some(8192), false), 0);
        assert_eq!(extra(None, true), 0);
        assert_eq!(extra(Some(0), true), 0);
        assert_eq!(extra(Some(u64::MAX), true), u64::MAX);
    }

    /// Part I's numbers: the card at 25,940 MB of 30,071 and 1,506 MB left
    /// to commit turned a 4 MiB window down for both; with room on both it
    /// goes; unknown numbers never turn it down.
    #[test]
    fn the_card_line_and_the_commit_floor_hold() {
        let size = 4 * MIB;
        let floor = 4096 * MIB;
        assert_eq!(
            judge(
                size,
                Some([30071 * MIB, 25940 * MIB]),
                Some(1506 * MIB),
                floor
            ),
            Err(No::Card)
        );
        assert_eq!(
            judge(
                size,
                Some([30071 * MIB, 20000 * MIB]),
                Some(1506 * MIB),
                floor
            ),
            Err(No::Commit)
        );
        assert_eq!(
            judge(
                size,
                Some([30071 * MIB, 20000 * MIB]),
                Some(8316 * MIB),
                floor
            ),
            Ok(())
        );
        assert_eq!(judge(size, None, None, floor), Ok(()));
        // Exactly at the floor after it: still given.
        assert_eq!(judge(size, None, Some(floor + size), floor), Ok(()));
        assert_eq!(
            judge(size, None, Some(floor + size - 1), floor),
            Err(No::Commit)
        );
        // Eight tenths, not the 97 hundredths a window within the total may go to.
        assert_eq!(
            judge(size, Some([1000 * MIB, 796 * MIB]), None, floor),
            Ok(())
        );
        assert_eq!(
            judge(size, Some([1000 * MIB, 797 * MIB]), None, floor),
            Err(No::Card)
        );
    }

    /// Within the windows' own total nothing is asked: the caller's line holds.
    #[test]
    fn within_the_total_it_is_always_given() {
        assert!(super::admits(
            0,
            4 * MIB,
            3072 * MIB,
            1000 * MIB,
            Some([1000 * MIB, 999 * MIB])
        ));
    }
}
