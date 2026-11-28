//! (feedcard, #88) `VICTORY_CARD_BATCH=1` (Live; off by default): when
//! several submissions settle together, the pieces their card compares
//! found differing are fetched with one copy list and one wait for the card
//! for all of their plans, not one list and one wait per plan.
//!
//! Every plan's compare has run before the fetch (the card was waited for),
//! the fetch reads the card's copies as they are after all of them, and the
//! pieces already fetched since that wait are skipped through the same
//! `seen` set either way. So one fetch for all reads the pieces one fetch
//! per plan read, in the same order, and merges them into title memory in
//! that order, before the CPU-side copies are handed back, as before; only
//! the waits go (darkworldhm saw 5.3 a frame, 94 ms in one world frame).
//!
//! After darkworldhm `840243e` (`VICTORY_CARD_BATCH`, `Host::card_back`).
//! Line: card fetches per frame.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};

static ON: victory_core::settings::Live = victory_core::settings::Live::flag("VICTORY_CARD_BATCH");

/// Settles with a card plan, settles with several, the plans in those, and
/// the fetch calls made for them, since the last line.
static COUNTS: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

/// The card plans of the submissions settled together, given to `fetch`:
/// all at once with the switch on, else one at a time, in their order.
pub(super) fn fetch_all<P>(plans: &[P], fetch: impl FnMut(&[P])) {
    fetch_all_with(ON.get_bool(), plans, fetch);
}

fn fetch_all_with<P>(on: bool, plans: &[P], mut fetch: impl FnMut(&[P])) {
    if plans.is_empty() {
        return;
    }
    COUNTS[0].fetch_add(1, Ordering::Relaxed);
    let several = plans.len() > 1;
    if several {
        COUNTS[1].fetch_add(1, Ordering::Relaxed);
        COUNTS[2].fetch_add(plans.len() as u64, Ordering::Relaxed);
    }
    if on {
        if several {
            COUNTS[3].fetch_add(1, Ordering::Relaxed);
        }
        fetch(plans);
    } else {
        for plan in plans.chunks(1) {
            if several {
                COUNTS[3].fetch_add(1, Ordering::Relaxed);
            }
            fetch(plan);
        }
    }
}

/// The pieces to fetch for `ranges`, in their order: the range's index, the
/// piece's offsets in its copy and where its bytes come down in the fetch,
/// and the bytes in all. `pieces_of` names a range's candidate pieces
/// (`keep(lo, hi)`); one already in `seen` for the same copy (`copy_of`) is
/// fetched already since the card was last waited for.
pub(super) fn gather<R>(
    ranges: &[R],
    mut pieces_of: impl FnMut(&R, &mut dyn FnMut(usize, usize)),
    copy_of: impl Fn(&R) -> usize,
    seen: &mut HashSet<(usize, usize, usize)>,
) -> (Vec<(usize, usize, usize, u64)>, u64) {
    let mut pieces = Vec::new();
    let mut total = 0u64;
    for (i, r) in ranges.iter().enumerate() {
        let copy = copy_of(r);
        pieces_of(r, &mut |lo, hi| {
            if seen.insert((copy, lo, hi)) {
                pieces.push((i, lo, hi, total));
                total += (hi - lo) as u64;
            }
        });
    }
    (pieces, total)
}

/// One line a frame, once a settle had a card plan.
pub(super) fn line(frames: f64) {
    let c = COUNTS.each_ref().map(|a| a.swap(0, Ordering::Relaxed));
    if frames <= 0.0 || c[0] == 0 {
        return;
    }
    tracing::info!(
        "card fetches per frame (VICTORY_CARD_BATCH={}): {:.1} settles with a card plan, \
         {:.2} with several holding {:.2} plans, fetched in {:.2} calls",
        u8::from(ON.get_bool()),
        c[0] as f64 / frames,
        c[1] as f64 / frames,
        c[2] as f64 / frames,
        c[3] as f64 / frames,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A range: its copy and its candidate pieces.
    type Range = (usize, Vec<(usize, usize)>);

    fn gather_ranges(
        ranges: &[&Range],
        seen: &mut HashSet<(usize, usize, usize)>,
    ) -> (Vec<(usize, usize, usize, u64)>, u64) {
        gather(
            ranges,
            |r, keep| r.1.iter().for_each(|&(lo, hi)| keep(lo, hi)),
            |r| r.0,
            seen,
        )
    }

    /// The pieces gathered for all plans at once are those gathered plan
    /// after plan with the same `seen`, in the same order and of the same
    /// bytes; only where they come down in the one fetch differs.
    #[test]
    fn one_fetch_for_all_plans_takes_the_pieces_of_one_fetch_each() {
        let plans: Vec<Vec<Range>> = vec![
            vec![(1, vec![(0, 4096), (8192, 12288)]), (2, vec![(0, 65536)])],
            // The same pieces of copy 1 again (named by two submissions), and new ones.
            vec![(1, vec![(0, 4096), (4096, 8192)]), (3, vec![(100, 200)])],
            vec![],
            vec![
                (2, vec![(0, 65536), (65536, 70000)]),
                (1, vec![(8192, 12288)]),
            ],
        ];
        let mut seen = HashSet::new();
        let mut each = Vec::new();
        let mut each_total = 0;
        for plan in &plans {
            let refs: Vec<&Range> = plan.iter().collect();
            let (pieces, total) = gather_ranges(&refs, &mut seen);
            each.extend(pieces.into_iter().map(|(i, lo, hi, _)| (plan[i].0, lo, hi)));
            each_total += total;
        }
        let mut seen = HashSet::new();
        let flat: Vec<&Range> = plans.iter().flatten().collect();
        let (pieces, total) = gather_ranges(&flat, &mut seen);
        let all: Vec<(usize, usize, usize)> = pieces
            .iter()
            .map(|&(i, lo, hi, _)| (flat[i].0, lo, hi))
            .collect();
        assert_eq!(all, each);
        assert_eq!(total, each_total);
        assert_eq!(all.len(), 6);
        // Where they come down: one after another in the one fetch.
        let mut at = 0;
        for &(_, lo, hi, offset) in &pieces {
            assert_eq!(offset, at);
            at += (hi - lo) as u64;
        }
        assert_eq!(at, total);
    }

    /// Off, each plan gets its own fetch, in order; on, one fetch for all
    /// (one plan: the same either way).
    #[test]
    fn plans_go_to_one_fetch_only_with_the_switch() {
        let calls = |on: bool, plans: &[u32]| {
            let mut calls: Vec<Vec<u32>> = Vec::new();
            fetch_all_with(on, plans, |p| calls.push(p.to_vec()));
            calls
        };
        assert_eq!(calls(false, &[1, 2, 3]), vec![vec![1], vec![2], vec![3]]);
        assert_eq!(calls(true, &[1, 2, 3]), vec![vec![1, 2, 3]]);
        assert_eq!(calls(true, &[7]), vec![vec![7]]);
        assert_eq!(calls(false, &[7]), vec![vec![7]]);
        assert!(calls(true, &[]).is_empty());
    }
}
