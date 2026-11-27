//! Cached uncommitted hole spans (`VICTORY_HOLES_ONCE=<frames>`, off by default).
//!
//! Adapted from `avatarmanz/VIctory-7900XTX` PR #377 (`arogan178`): remembers
//! [`super::uncommitted_end`] answers by hole interval for up to N frames so
//! repeated guard walks over ordinary uncommitted holes do not issue redundant
//! `VirtualQuery` calls, forgetting any hole overlapping a remapped range.

use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};

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

/// Forgets cached holes overlapping `[at, at + len)`.
pub(super) fn forget(at: usize, len: usize) {
    if len == 0 || ttl_frames() == 0 {
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
