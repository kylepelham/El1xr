//! (blackface, issue #35) The head blend-shape map: the title's
//! `CS_ComputeBlendshapeMappings` (`61e54e90adccaf6a`) builds the map that
//! fits the face's blend shapes to the head once, and is dispatched once.
//! Dispatched before its pipeline is made, it is dropped and the face is
//! never built: eyes and teeth without the skin around them (a 4090 measured
//! the pipeline 29.8 s after its creation).
//!
//! - `VICTORY_HEAD_MAP_WAIT=1` (off by default, read at start): it is a
//!   scene input ([`super::scene_input`]) whatever `VICTORY_SCENE_INPUT`
//!   says: queued when the title creates it, at the front of the workers'
//!   queue, and its first dispatch waits for it past the frame budget, at
//!   most `VICTORY_SCENE_INPUT_WAIT_MS` (3000) from the run's
//!   `VICTORY_SCENE_INPUT_TOTAL_MS` (15000); upstream waits 2 s.
//! - Always: its first dispatch logs one line `head blend-shape map at its
//!   first dispatch` (made before, made by the wait, or dropped).
//!
//! Source: TaynanGT/Vice-Tay #372 `3e3fd57` (Taynan Piza), after
//! avatarmanz/VIctory-7900XTX #166 and DoubleGhosty/cool_fox `700fc78460`.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use super::Pending;

/// The program's hash, as the pipeline diagnostics print it.
const HEAD_MAP: &str = "61e54e90adccaf6a";

/// Whether a pipeline of these (stage, hash) is the head map's: its one
/// compute stage.
pub(super) fn is_head_map<'a>(
    mut stages: impl ExactSizeIterator<Item = (&'a str, &'a str)>,
) -> bool {
    stages.len() == 1
        && stages
            .next()
            .is_some_and(|(stage, hash)| stage == "compute" && hash == HEAD_MAP)
}

/// Whether the switch is on (`VICTORY_HEAD_MAP_WAIT=1`).
pub(super) fn wait_on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_HEAD_MAP_WAIT").is_some_and(|v| v == "1"))
}

/// Whether a pipeline is waited for as a scene input: one of the switch's
/// own, or the head map's with the switch on.
pub(super) fn scene_input(listed: bool, head_map: bool, on: bool) -> bool {
    listed || head_map && on
}

/// The first dispatch's look at the head map: when it began, and whether
/// the pipeline was made before; `None` for any other pipeline or a later
/// dispatch.
pub(super) fn first(p: &Pending) -> Option<(Instant, bool)> {
    static SEEN: AtomicBool = AtomicBool::new(false);
    if !p.head_map || SEEN.load(Ordering::Relaxed) || SEEN.swap(true, Ordering::Relaxed) {
        return None;
    }
    Some((Instant::now(), p.made.get().is_some()))
}

/// Logs the first dispatch's look begun at `began`.
pub(super) fn said(p: &Pending, (began, before): (Instant, bool)) {
    tracing::info!(
        frame = crate::frame::current(),
        made_before = before,
        made = p.made.get().is_some_and(|m| m.is_some()),
        refused = matches!(p.made.get(), Some(None)),
        waited_ms = began.elapsed().as_millis() as u64,
        wait_on = p.scene_input,
        "head blend-shape map at its first dispatch (61e54e90adccaf6a, VICTORY_HEAD_MAP_WAIT)"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_head_maps_compute_stage_is_named() {
        assert!(is_head_map([("compute", HEAD_MAP)].into_iter()));
        assert!(!is_head_map([("pixel", HEAD_MAP)].into_iter()));
        assert!(!is_head_map([("compute", "970c4fbb3b017c0e")].into_iter()));
        assert!(!is_head_map(
            [("vertex", "0000000000000000"), ("compute", HEAD_MAP)].into_iter()
        ));
        assert!(!is_head_map(std::iter::empty::<(&str, &str)>()));
    }

    /// With the switch the head map is waited for as a scene input; without
    /// it only the scene inputs' own list is.
    #[test]
    fn the_head_map_is_a_scene_input_with_the_switch() {
        assert!(scene_input(false, true, true));
        assert!(!scene_input(false, true, false));
        assert!(!scene_input(false, false, true));
        assert!(scene_input(true, false, false));
    }
}
