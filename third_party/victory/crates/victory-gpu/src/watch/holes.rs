//! Cached uncommitted hole spans (`VICTORY_HOLES_ONCE=<frames>`, off by default).
//!
//! Adapted from `avatarmanz/VIctory-7900XTX` PR #377 (`arogan178`): remembers
//! [`super::uncommitted_end`] answers by hole interval for up to N frames so
//! repeated guard walks over ordinary uncommitted holes do not issue redundant
//! `VirtualQuery` calls, forgetting any hole overlapping a remapped range.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock, RwLock};

use super::BLOCK;

/// Holes asked about: by first block, `(exclusive end block, frame asked)`.
/// No two entries overlap.
static HOLES_ASKED: Mutex<BTreeMap<usize, (usize, u64)>> = Mutex::new(BTreeMap::new());

/// How many frames a hole's answer stands (`VICTORY_HOLES_ONCE=<frames>`,
/// `0` by default on native Windows where placeholder holes are already skipped
/// without `VirtualQuery`).
fn ttl_frames() -> u64 {
    if cfg!(test) {
        return 0;
    }
    static FRAMES: OnceLock<u64> = OnceLock::new();
    *FRAMES.get_or_init(|| parse_ttl(std::env::var("VICTORY_HOLES_ONCE").ok().as_deref()))
}

fn parse_ttl(value: Option<&str>) -> u64 {
    value.and_then(|v| v.trim().parse().ok()).unwrap_or(0)
}

/// [`super::uncommitted_end`], or the cached answer for the hole holding
/// `block` from fewer than `VICTORY_HOLES_ONCE` frames ago.
pub(super) fn uncommitted_end_cached(block: usize) -> usize {
    let frames = ttl_frames();
    if frames == 0 {
        if kept_on() {
            return kept_end(&KEPT, block, crate::frame::current(), super::uncommitted_span);
        }
        return super::uncommitted_end(block);
    }
    lookup_or_query(
        &HOLES_ASKED,
        block,
        crate::frame::current(),
        frames,
        super::uncommitted_end,
    )
}

fn lookup_or_query(
    cache: &Mutex<BTreeMap<usize, (usize, u64)>>,
    block: usize,
    frame: u64,
    ttl: u64,
    query: impl FnOnce(usize) -> usize,
) -> usize {
    if let Some((_, &(end, at))) = cache
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .range(..=block)
        .next_back()
        && block < end
        && frame.saturating_sub(at) < ttl
    {
        return end;
    }
    let end = query(block);
    if end > block {
        let mut holes = cache.lock().unwrap_or_else(|e| e.into_inner());
        forget_in(&mut holes, block, end);
        holes.insert(block, (end, frame));
    }
    end
}

/// (feedcard, #88) `VICTORY_GUARD_HOLES_KEPT=1`, live: a reserved, uncommitted run of blocks
/// found by one guard check is kept for the rest of the frame, so the checks of every refresh
/// thread (each asked again: b5 part M, 9,853 `VirtualQuery` a frame, 11.9 ms, and 16.8 ms of
/// checks before the walk) and the walk after the checks take it without asking Windows.
/// Forgotten at once where the runtime maps, unmaps or commits ([`forget`]). Free runs are
/// never kept (anyone may allocate there). Shortcut: a commit the runtime is not told of (a
/// host heap growing inside a reservation the guards walk) is seen at the next frame.
pub(super) fn kept_on() -> bool {
    #[cfg(test)]
    if let Some(forced) = KEPT_FORCED.with(std::cell::Cell::get) {
        return forced;
    }
    static ON: victory_core::settings::Live = victory_core::settings::Live::flag("VICTORY_GUARD_HOLES_KEPT");
    ON.get_bool()
}

#[cfg(test)]
thread_local! {
    pub(super) static KEPT_FORCED: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

/// The kept runs: by first block, the exclusive end block; all found in `frame`. `told` is bumped
/// (under the write lock) by every forget: a run asked about before a forget is not kept after it.
pub(super) struct Kept {
    frame: u64,
    told: u64,
    holes: BTreeMap<usize, usize>,
}

pub(super) type KeptTable = RwLock<Kept>;

pub(super) const fn kept_table() -> KeptTable {
    RwLock::new(Kept { frame: 0, told: 0, holes: BTreeMap::new() })
}

static KEPT: KeptTable = kept_table();

/// Answers taken from the kept runs since the last frame line.
pub(super) static KEPT_HITS: AtomicU64 = AtomicU64::new(0);

/// [`super::uncommitted_end`] of `block`, from `table` when `frame` found its run already.
/// `span` asks Windows: the end block of the uncommitted run at `block` and whether it is
/// reserved (not free).
pub(super) fn kept_end(
    table: &KeptTable,
    block: usize,
    frame: u64,
    span: impl FnOnce(usize) -> (usize, bool),
) -> usize {
    let told = {
        let kept = table.read().unwrap_or_else(|e| e.into_inner());
        if kept.frame == frame
            && let Some((_, &end)) = kept.holes.range(..=block).next_back()
            && block < end
        {
            KEPT_HITS.fetch_add(1, Ordering::Relaxed);
            return end;
        }
        kept.told
    };
    let (end, reserved) = span(block);
    if reserved && end > block {
        let mut kept = table.write().unwrap_or_else(|e| e.into_inner());
        if kept.told == told && frame >= kept.frame {
            if frame > kept.frame {
                kept.holes.clear();
                kept.frame = frame;
            }
            forget_kept(&mut kept.holes, block, end);
            kept.holes.insert(block, end);
        }
    }
    end
}

/// Forgets the runs of `table` overlapping `[at, at + len)`.
pub(super) fn forget_in_table(table: &KeptTable, at: usize, len: usize) {
    let (first, last) = (at / BLOCK, at.saturating_add(len).div_ceil(BLOCK));
    let mut kept = table.write().unwrap_or_else(|e| e.into_inner());
    kept.told += 1;
    forget_kept(&mut kept.holes, first, last);
}

/// Takes the kept runs overlapping blocks `[first, last)` out of `holes`.
fn forget_kept(holes: &mut BTreeMap<usize, usize>, first: usize, last: usize) {
    let before = holes.range(..first).next_back().filter(|&(_, &end)| end > first).map(|(&b, _)| b);
    let inside: Vec<usize> = holes.range(first..last).map(|(&b, _)| b).collect();
    for b in before.into_iter().chain(inside) {
        holes.remove(&b);
    }
}

/// Forgets cached holes overlapping `[at, at + len)`: the runtime mapped, unmapped or
/// committed there.
pub(super) fn forget(at: usize, len: usize) {
    if len == 0 {
        return;
    }
    forget_in_table(&KEPT, at, len);
    if ttl_frames() == 0 {
        return;
    }
    forget_in(
        &mut HOLES_ASKED.lock().unwrap_or_else(|e| e.into_inner()),
        at / BLOCK,
        at.saturating_add(len).div_ceil(BLOCK),
    );
}

/// Takes the holes overlapping blocks `[first, last)` out of `holes`.
fn forget_in(holes: &mut BTreeMap<usize, (usize, u64)>, first: usize, last: usize) {
    let before = holes
        .range(..first)
        .next_back()
        .filter(|(_, (end, _))| *end > first)
        .map(|(&start, _)| start);
    let inside: Vec<usize> = holes.range(first..last).map(|(&start, _)| start).collect();
    for start in before.into_iter().chain(inside) {
        holes.remove(&start);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forgetting_takes_only_the_holes_touched() {
        let mut holes: BTreeMap<usize, (usize, u64)> =
            [(10, (20, 0)), (30, (40, 0)), (50, (60, 0))].into();
        forget_in(&mut holes, 15, 16);
        assert_eq!(holes.keys().copied().collect::<Vec<_>>(), [30, 50]);
        forget_in(&mut holes, 40, 50);
        assert_eq!(holes.keys().copied().collect::<Vec<_>>(), [30, 50]);
        forget_in(&mut holes, 39, 51);
        assert!(holes.is_empty());
    }

    #[test]
    fn cached_hole_is_reused_until_ttl_expires_or_forgotten() {
        let cache = Mutex::new(BTreeMap::new());
        let mut calls = 0u32;
        assert_eq!(
            lookup_or_query(&cache, 10, 100, 30, |_| {
                calls += 1;
                25
            }),
            25
        );
        assert_eq!(calls, 1);
        // Any block inside [10, 25) within TTL hits cache without querying.
        assert_eq!(
            lookup_or_query(&cache, 18, 129, 30, |_| {
                calls += 1;
                99
            }),
            25
        );
        assert_eq!(calls, 1);
        // At frame 130 (delta == 30), TTL expired -> queries again.
        assert_eq!(
            lookup_or_query(&cache, 18, 130, 30, |_| {
                calls += 1;
                22
            }),
            22
        );
        assert_eq!(calls, 2);
        // Opt-in parsing defaults to 0.
        assert_eq!(parse_ttl(None), 0);
        assert_eq!(parse_ttl(Some("30")), 30);
    }
}
