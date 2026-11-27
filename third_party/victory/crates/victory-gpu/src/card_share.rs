//! (El1xr) `VICTORY_CARD_COMPARE_SHARE=1`: the card's compare of what a
//! submission's shaders may have written (`CARD_COMPARE`, "card check" in
//! `VICTORY_GPU_COST`, ~1.03 ms a frame in ~425 passes) hashes each 64 KiB
//! piece of a copy once per submission.
//!
//! A submission's declared ranges of one copy are joined only where they
//! overlap, so two ranges that lie in the same piece, or in pieces next to
//! each other, were each compared on their own: the shared piece hashed
//! twice and, with the relay (compare and store), the second pass of it
//! raced the first one's store of the new hash. Shared, ranges of one copy
//! whose pieces overlap or touch take their results from one run of slots,
//! and one pass hashes the run's pieces. What reads the results takes each
//! range's pieces from its own slots, as before; a piece is flagged for
//! every range that holds it (the readers already fetch a piece for every
//! range of the copy when any flags it).

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

/// Whether compares are shared (default off).
pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_CARD_COMPARE_SHARE").is_some_and(|v| v == "1"))
}

/// The run of result slots of one copy's ranges laid out so far: its first
/// piece, the piece after its last, its first slot.
#[derive(Default)]
pub(crate) struct Run {
    shared: bool,
    run: Option<(u32, u32, u32)>,
}

impl Run {
    /// For one copy's ranges in the order of their start; `shared`: whether
    /// its ranges share slots.
    pub(crate) fn new(shared: bool) -> Self {
        Run { shared, run: None }
    }

    /// The first result slot of a range of `pieces` pieces from `first` on,
    /// or `None`: there is no room (`most` slots), and it is not compared.
    /// `taken`: the slots handed out so far, of every copy.
    pub(crate) fn slot(
        &mut self,
        first: u32,
        pieces: u32,
        taken: &mut u32,
        most: u64,
    ) -> Option<u32> {
        let end = first + pieces;
        if self.shared
            && let Some((run_first, run_end, run_at)) = self.run
            && first <= run_end
            && first >= run_first
            && *taken == run_at + (run_end - run_first)
        {
            let grown = end.saturating_sub(run_end);
            if u64::from(*taken) + u64::from(grown) > most {
                return None;
            }
            *taken += grown;
            self.run = Some((run_first, run_end.max(end), run_at));
            return Some(run_at + (first - run_first));
        }
        if u64::from(*taken) + u64::from(pieces) > most {
            return None;
        }
        let at = *taken;
        *taken += pieces;
        self.run = Some((first, end, at));
        Some(at)
    }
}

/// Compared ranges, as (copy, first piece, pieces, first slot), and how
/// many passed so far: the compare passes, as (the index of a range whose
/// copy it is, first piece, pieces, first slot). Shared, one pass for each
/// run of slots; else one for each range.
pub(crate) fn passes(
    ranges: impl Iterator<Item = Option<(usize, u32, u32, u32)>>,
) -> Vec<(usize, u32, u32, u32)> {
    passes_of(on(), ranges)
}

fn passes_of(
    shared: bool,
    ranges: impl Iterator<Item = Option<(usize, u32, u32, u32)>>,
) -> Vec<(usize, u32, u32, u32)> {
    let mut out: Vec<(usize, u32, u32, u32)> = Vec::new();
    let mut copies: Vec<usize> = Vec::new();
    let (mut asked, mut asked_pieces) = (0u64, 0u64);
    for (index, range) in ranges.enumerate() {
        let Some((copy, first, pieces, at)) = range else {
            continue;
        };
        asked += 1;
        asked_pieces += u64::from(pieces);
        if shared
            && let (Some(last), Some(&last_copy)) = (out.last_mut(), copies.last())
            && last_copy == copy
            && first >= last.1
            && first <= last.1 + last.2
            && at.wrapping_sub(first) == last.3.wrapping_sub(last.1)
        {
            last.2 = last.2.max(first + pieces - last.1);
            continue;
        }
        out.push((index, first, pieces, at));
        copies.push(copy);
    }
    if shared {
        count(asked, asked_pieces, &out);
    }
    out
}

/// Ranges, their pieces, passes and their pieces, since the last line.
static COUNTS: [AtomicU64; 5] = [const { AtomicU64::new(0) }; 5];

fn count(asked: u64, asked_pieces: u64, out: &[(usize, u32, u32, u32)]) {
    COUNTS[0].fetch_add(asked, Ordering::Relaxed);
    COUNTS[1].fetch_add(asked_pieces, Ordering::Relaxed);
    COUNTS[2].fetch_add(out.len() as u64, Ordering::Relaxed);
    COUNTS[3].fetch_add(out.iter().map(|p| u64::from(p.2)).sum(), Ordering::Relaxed);
    if COUNTS[4].fetch_add(1, Ordering::Relaxed) % 2000 == 1999 {
        let [ranges, pieces, passes, hashed, _] =
            COUNTS.each_ref().map(|c| c.swap(0, Ordering::Relaxed));
        tracing::info!(
            "card compares shared (VICTORY_CARD_COMPARE_SHARE=1), last 2000 submissions: {ranges} ranges over {pieces} pieces \
             compared in {passes} passes over {hashed} pieces"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lay(shared: bool, spans: &[(u32, u32)], most: u64) -> (Vec<Option<u32>>, u32) {
        let mut taken = 0;
        let mut run = Run::new(shared);
        let slots = spans
            .iter()
            .map(|&(first, pieces)| run.slot(first, pieces, &mut taken, most))
            .collect();
        (slots, taken)
    }

    #[test]
    fn unshared_ranges_each_take_their_own_slots() {
        let (slots, taken) = lay(false, &[(0, 1), (0, 1), (1, 2), (9, 1)], 100);
        assert_eq!(slots, [Some(0), Some(1), Some(2), Some(4)]);
        assert_eq!(taken, 5);
        let (slots, taken) = lay(false, &[(0, 2), (5, 2)], 3);
        assert_eq!(slots, [Some(0), None]);
        assert_eq!(taken, 2);
    }

    #[test]
    fn ranges_in_the_same_or_next_pieces_share_one_run() {
        // Same piece; overlapping; touching; then a gap starts a new run.
        let (slots, taken) = lay(true, &[(3, 1), (3, 2), (5, 1), (4, 1), (9, 2)], 100);
        assert_eq!(slots, [Some(0), Some(0), Some(2), Some(1), Some(3)]);
        assert_eq!(taken, 5);
        // No room to grow the run: not compared, and the run stays.
        let (slots, taken) = lay(true, &[(0, 1), (1, 3), (0, 1)], 2);
        assert_eq!(slots, [Some(0), None, Some(0)]);
        assert_eq!(taken, 1);
    }

    #[test]
    fn one_pass_for_each_run_of_slots() {
        // Copy 1: three ranges in one run, a gap, one more; copy 2 in between
        // of nothing; an uncompared range; copy 2.
        let spans = [(1, 3, 1), (1, 3, 2), (1, 4, 1), (1, 9, 1), (2, 4, 1)];
        let mut taken = 0;
        let mut runs = std::collections::HashMap::new();
        let mut ranges: Vec<Option<(usize, u32, u32, u32)>> = spans
            .iter()
            .map(|&(copy, first, pieces)| {
                let run = runs.entry(copy).or_insert_with(|| Run::new(true));
                run.slot(first, pieces, &mut taken, 100)
                    .map(|at| (copy, first, pieces, at))
            })
            .collect();
        ranges.insert(4, None);
        let passes = passes_of(true, ranges.iter().copied());
        assert_eq!(passes, [(0, 3, 2, 0), (3, 9, 1, 2), (5, 4, 1, 3)]);
        // Unshared: one for each compared range, as recorded.
        let alone = passes_of(false, ranges.iter().copied());
        assert_eq!(alone.len(), 5);
        assert!(alone.iter().all(|&(i, first, pieces, at)| ranges[i]
            == Some((spans[if i > 4 { i - 1 } else { i }].0, first, pieces, at))));
    }

    #[test]
    fn every_slot_of_a_range_is_its_own_piece_of_the_run() {
        let spans = [(10, 3), (11, 1), (12, 4), (13, 1), (20, 1)];
        let mut taken = 0;
        let mut run = Run::new(true);
        let mut piece_of = std::collections::HashMap::new();
        for &(first, pieces) in &spans {
            let at = run.slot(first, pieces, &mut taken, 100).unwrap();
            for p in 0..pieces {
                let piece = *piece_of.entry(at + p).or_insert(first + p);
                assert_eq!(piece, first + p, "slot {} holds two pieces", at + p);
            }
        }
        assert_eq!(taken as usize, piece_of.len());
    }
}
