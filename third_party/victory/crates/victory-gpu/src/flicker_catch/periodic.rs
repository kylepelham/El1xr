//! (texflicker, #96, b5 part L) Our periodic work, named in the flicker
//! catcher's line: the diner's dark frames came every ~2.06 s of wall time
//! (frames 900-1863 of `train_b5l_flick/01_own`, 27-40 frames apart or a
//! multiple of that), so something of ours on a timer is the suspect. Each
//! task below sets its bit in the frame it ran in (from one present to the
//! next); the line says `periodic: dark=[..] before=[..] after=[..]` for the
//! dark frame, the one before and the one after.
//!
//! - `sweep`: the sweep that lets idle host objects go (`host::sweep`,
//!   once a second before a submission; `VICTORY_SWEEP=0` turns it off).
//! - `sweep_settle`: a submission settled every flight for that sweep
//!   (behind flights, once every `VICTORY_SWEEP_EVERY_MS`, 1.5 s).
//! - `shadows_gone`, `staging_gone`, `textures_gone`: that sweep let go of
//!   host copies of title memory windows, staging buffers of textures, host
//!   copies of textures (`VICTORY_TEXTURE_SWEEP=0` keeps the last).
//! - `prep_pruned`: textures made ready ahead and not taken for 2 s were
//!   let go (`texture::prepare::prune`, `VICTORY_TEXTURE_PREPARE_THREADS`).
//! - `levels_asked`: a level left without tiles was asked for again (about
//!   once a second, levels on demand).
//! - `snap`: a timed snap was recorded in the present (`VICTORY_SNAP_EVERY_S`);
//!   `snap_writing`: one was being written (its PNG encoded) meanwhile.
//!
//! A diagnostic: nothing is done otherwise.

use std::sync::atomic::{AtomicU32, Ordering::Relaxed};

/// A periodic task of ours.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Task {
    Sweep,
    SweepSettle,
    ShadowsGone,
    StagingGone,
    TexturesGone,
    PrepPruned,
    LevelsAsked,
    Snap,
    SnapWriting,
}

const NAMES: [&str; 9] = [
    "sweep",
    "sweep_settle",
    "shadows_gone",
    "staging_gone",
    "textures_gone",
    "prep_pruned",
    "levels_asked",
    "snap",
    "snap_writing",
];

/// The tasks run since the last present (bits of [`Task`]).
static NOW: AtomicU32 = AtomicU32::new(0);

/// `task` ran in this frame.
pub(crate) fn ran(task: Task) {
    if super::on() {
        NOW.fetch_or(1 << task as u32, Relaxed);
    }
}

/// At a present: the tasks of the frame that ends.
pub(super) fn take() -> u32 {
    #[allow(unused_mut)]
    let mut mask = NOW.swap(0, Relaxed);
    #[cfg(all(windows, not(test)))]
    if crate::snap::writing() {
        mask |= 1 << Task::SnapWriting as u32;
    }
    mask
}

/// The tasks in bits `mask`, `sweep+snap`; `-` for none.
pub(super) fn names(mask: u32) -> String {
    let names: Vec<&str> = (0..NAMES.len())
        .filter(|b| mask >> b & 1 != 0)
        .map(|b| NAMES[b])
        .collect();
    if names.is_empty() {
        "-".to_owned()
    } else {
        names.join("+")
    }
}

/// The line's part: the tasks of the dark frame, the one before and the one
/// after (`?` when that one was not kept).
pub(super) fn line(before: u32, dark: u32, after: Option<u32>) -> String {
    format!(
        "periodic: dark=[{}] before=[{}] after=[{}]",
        names(dark),
        names(before),
        after.map_or_else(|| "?".to_owned(), names)
    )
}
