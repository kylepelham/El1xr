//! (racefix, #57) What `VICTORY_SCENE_INPUT` decided for each of its
//! pipelines at the first draw or dispatch: made before (nothing to wait
//! for), made by the wait, or not made and its draw dropped. Batch 5 part H
//! (`VICTORY_SCENE_INPUT_MORE=1`) logged no `scene-input wait:` line at all;
//! the wait lines are written only when a pipeline is not made yet, so they
//! could not tell a warm library from a match that never reached a draw.
//!
//! One line `scene-input at first use:` per pipeline whose outcome is not
//! "made before", and for the first [`SAID_BEFORE`] (then every
//! [`EVERY_BEFORE`]th) that were; each line carries the run's tallies.
//! `more_only` says the pipeline is a scene input only through
//! `VICTORY_SCENE_INPUT_MORE`'s additions. Nothing changes without
//! `VICTORY_SCENE_INPUT=1` (no pipeline is a scene input then).

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use super::Pending;

/// "Made before first use" lines written in full.
const SAID_BEFORE: u64 = 32;
/// After them, one in this many.
const EVERY_BEFORE: u64 = 100;

/// What became of a scene-input pipeline at its first use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Outcome {
    /// Made (from the library or by a worker) before its first draw.
    MadeBefore,
    /// Refused or not translated before its first draw: dropped.
    RefusedBefore,
    /// Not made at its first draw, made while the draw waited.
    WaitedMade,
    /// Not made at its first draw, refused while it waited: dropped.
    WaitedRefused,
    /// Not made in time: its first draw dropped.
    Dropped,
}

impl Outcome {
    const ALL: usize = 5;

    /// From whether it was made at the first look, and after the wait
    /// whether it is (`None`: still being made; `Some(false)`: refused).
    pub(super) fn of(made_before: bool, made: Option<bool>) -> Outcome {
        match (made_before, made) {
            (true, Some(true)) => Outcome::MadeBefore,
            (true, _) => Outcome::RefusedBefore,
            (false, Some(true)) => Outcome::WaitedMade,
            (false, Some(false)) => Outcome::WaitedRefused,
            (false, None) => Outcome::Dropped,
        }
    }

    fn says(self) -> &'static str {
        match self {
            Outcome::MadeBefore => "made before first use",
            Outcome::RefusedBefore => "refused before first use, draw dropped",
            Outcome::WaitedMade => "waited, made",
            Outcome::WaitedRefused => "waited, refused, draw dropped",
            Outcome::Dropped => "waited, not made in time, draw dropped",
        }
    }
}

static TALLY: [AtomicU64; Outcome::ALL] = [const { AtomicU64::new(0) }; Outcome::ALL];

/// Whether the `n`th outcome of its kind (from 1) gets a line.
pub(super) fn worth_a_line(outcome: Outcome, n: u64) -> bool {
    outcome != Outcome::MadeBefore || n <= SAID_BEFORE || n % EVERY_BEFORE == 0
}

/// The first look at a scene-input pipeline at its first use (`first`: the
/// first draw or dispatch of it): when, and whether it was made then.
pub(super) fn first(p: &Pending, first: bool) -> Option<(Instant, bool)> {
    (first && p.scene_input).then(|| (Instant::now(), p.made.get().is_some()))
}

/// Logs what became of `p` since [`first`] looked at `began`.
pub(super) fn said(p: &Pending, (began, made_before): (Instant, bool)) {
    let outcome = Outcome::of(made_before, p.made.get().map(Option::is_some));
    let n = TALLY[outcome as usize].fetch_add(1, Ordering::Relaxed) + 1;
    if !worth_a_line(outcome, n) {
        return;
    }
    let t = |o: Outcome| TALLY[o as usize].load(Ordering::Relaxed);
    let more_only = !p
        .shaders
        .iter()
        .any(|s| super::scene_input_named(s.stage, &s.hash, &s.container, false));
    tracing::info!(
        shaders = ?p.shaders,
        frame = crate::frame::current(),
        waited_ms = began.elapsed().as_millis() as u64,
        more_only,
        made_before = t(Outcome::MadeBefore),
        waited_made = t(Outcome::WaitedMade),
        dropped = t(Outcome::Dropped) + t(Outcome::WaitedRefused) + t(Outcome::RefusedBefore),
        "scene-input at first use: {} (VICTORY_SCENE_INPUT)",
        outcome.says()
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_outcome_names_what_the_first_draw_found() {
        assert_eq!(Outcome::of(true, Some(true)), Outcome::MadeBefore);
        assert_eq!(Outcome::of(true, Some(false)), Outcome::RefusedBefore);
        assert_eq!(Outcome::of(false, Some(true)), Outcome::WaitedMade);
        assert_eq!(Outcome::of(false, Some(false)), Outcome::WaitedRefused);
        assert_eq!(Outcome::of(false, None), Outcome::Dropped);
    }

    /// Every wait and drop is said; "made before" only the first ones and
    /// then one in [`EVERY_BEFORE`], so a warm run of hundreds stays short.
    #[test]
    fn made_before_is_said_sparingly_and_the_rest_always() {
        assert!(worth_a_line(Outcome::MadeBefore, 1));
        assert!(worth_a_line(Outcome::MadeBefore, SAID_BEFORE));
        assert!(!worth_a_line(Outcome::MadeBefore, SAID_BEFORE + 1));
        assert!(worth_a_line(Outcome::MadeBefore, EVERY_BEFORE));
        for n in [1, 33, 101, 5000] {
            assert!(worth_a_line(Outcome::Dropped, n));
            assert!(worth_a_line(Outcome::WaitedMade, n));
            assert!(worth_a_line(Outcome::RefusedBefore, n));
        }
    }

    /// The first use of a scene input is looked at; any other use, or a
    /// pipeline that is not one, is not.
    #[test]
    fn only_a_scene_inputs_first_use_is_looked_at() {
        let mut p = Pending::new(Vec::new(), None);
        assert!(first(&p, true).is_none());
        p.scene_input = true;
        assert!(first(&p, false).is_none());
        assert_eq!(first(&p, true).map(|(_, before)| before), Some(false));
    }

    /// `ready` counts a scene input's first use once, by what it found: a
    /// pipeline refused before its first draw is a drop, a second draw adds
    /// nothing.
    #[test]
    fn ready_tallies_a_scene_inputs_first_use_once() {
        let mut p = Pending::new(Vec::new(), None);
        p.scene_input = true;
        p.make_with(|| None);
        let p = std::sync::Arc::new(p);
        let refused = || TALLY[Outcome::RefusedBefore as usize].load(Ordering::Relaxed);
        let before = refused();
        assert!(super::super::ready(&p).is_none());
        assert_eq!(refused(), before + 1);
        assert!(super::super::ready(&p).is_none());
        assert_eq!(refused(), before + 1, "once per pipeline");
    }
}
