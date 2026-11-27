//! `VICTORY_MEET_PRUNE=1` (off by default, in the shader cache key): where
//! [`crate::structure::Bools::meet`] runs out of case splits it answers "some
//! lane may", and every proof that asked it fails. Two answers it can give
//! without that search:
//!
//! - a condition asked to hold and not to hold at once: no lane meets both;
//! - a search that gave up, asked again without each condition in turn: no
//!   lane meeting some of the conditions is no lane meeting all of them.
//!
//! A block's reach is usually the large function among the conditions; without
//! it the search finishes. The waterfall loops of a wave64 program run as two
//! host waves (`CS_ProjDecalReduceClassification_Scalarized`) were refused
//! their proof that way, each pass then reading the first lane across both
//! host waves through LDS at two group barriers, up to 4096 passes.

use crate::structure::Bool;

/// Conditions past which a search that gave up is not asked again.
const MOST: usize = 8;

/// Whether the shortcuts are on (`VICTORY_MEET_PRUNE=1`); a test can choose
/// for its own thread.
pub fn enabled() -> bool {
    #[cfg(test)]
    if let Some(on) = TEST_PRUNE.get() {
        return on;
    }
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("VICTORY_MEET_PRUNE").is_ok_and(|v| v.trim() == "1"))
}

#[cfg(test)]
thread_local! {
    /// A test's choice for [`enabled`], for its thread only.
    pub(crate) static TEST_PRUNE: std::cell::Cell<Option<bool>> =
        const { std::cell::Cell::new(None) };
}

/// Whether some lane meets every `(b, holds)` of `all`, by `search`, which
/// answers that for a list of conditions together with the case splits it
/// had left (none left with a yes: it gave up).
pub(crate) fn pruned(
    all: &[(Bool, bool)],
    mut search: impl FnMut(&[(Bool, bool)]) -> (bool, usize),
) -> bool {
    let both = |(i, a): (usize, &(Bool, bool))| all[..i].iter().any(|b| b.0 == a.0 && b.1 != a.1);
    if all.iter().enumerate().any(both) {
        return false;
    }
    let (found, left) = search(all);
    if !found || left > 0 || all.len() < 2 || all.len() > MOST {
        return found;
    }
    (0..all.len()).all(|k| {
        let fewer: Vec<_> = (0..all.len()).filter(|&i| i != k).map(|i| all[i]).collect();
        search(&fewer).0
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: Bool = Bool::FALSE;
    const B: Bool = Bool::TRUE;

    /// A search that gives up on three conditions or more and finds no lane
    /// where `B` must not hold.
    fn search(all: &[(Bool, bool)], asked: &mut Vec<usize>) -> (bool, usize) {
        asked.push(all.len());
        if all.len() >= 3 {
            return (true, 0);
        }
        (!all.contains(&(B, false)), 5)
    }

    #[test]
    fn a_condition_held_and_negated_meets_no_lane_unsearched() {
        let mut asked = Vec::new();
        assert!(!pruned(&[(A, true), (B, true), (A, false)], |c| search(
            c, &mut asked
        )));
        assert!(asked.is_empty());
    }

    #[test]
    fn a_search_that_gave_up_is_asked_again_without_each_condition() {
        let mut asked = Vec::new();
        // Without the first condition: (B, false) alone meets no lane.
        assert!(!pruned(&[(A, true), (A, true), (B, false)], |c| {
            search(c, &mut asked)
        }));
        assert_eq!(asked, [3, 2]);
        // Every smaller search finds a lane: the answer stays yes.
        asked.clear();
        assert!(pruned(&[(A, true), (A, true), (B, true)], |c| search(
            c, &mut asked
        )));
        assert_eq!(asked, [3, 2, 2, 2]);
    }

    #[test]
    fn a_finished_search_is_not_asked_again() {
        let mut asked = Vec::new();
        assert!(pruned(&[(A, true), (B, true)], |c| search(c, &mut asked)));
        assert!(!pruned(&[(A, true), (B, false)], |c| search(c, &mut asked)));
        assert_eq!(asked, [2, 2]);
    }
}
