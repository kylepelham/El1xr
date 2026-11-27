//! Why draws, mesh dispatches, dispatches and indirect executions were not
//! run on the host (`VICTORY_DROP_REASONS=1`): one log line per frame line, each
//! reason counted per frame by where it was dropped. The frame line's
//! "dropped ... without a host pipeline" and "... indirect" counts are these
//! drops summed (a release build names no reason otherwise).
//!
//! A failure point notes its reason on its thread ([`set`]); the drop site
//! counts it ([`count`]) and clears it. A drop with no reason noted is
//! counted as "unclassified".

use std::cell::Cell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

/// Reasons per frame line shown at most.
const SHOWN: usize = 24;

/// Whether drops are counted by reason (`VICTORY_DROP_REASONS=1`).
pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_DROP_REASONS").is_some_and(|v| v == "1"))
}

thread_local! {
    static WHY: Cell<&'static str> = const { Cell::new("") };
}

/// (site, reason) -> drops since the last line.
static COUNTS: Mutex<Option<HashMap<(&'static str, &'static str), u64>>> = Mutex::new(None);
/// Frames presented at the last line.
static FRAMES_AT: AtomicU64 = AtomicU64::new(0);

/// Notes why the draw or dispatch being prepared on this thread fails.
pub(crate) fn set(why: &'static str) {
    if on() {
        WHY.set(why);
    }
}

/// Counts a drop at `site` (draw, indexed, mesh, dispatch, indirect) for
/// the reason last noted on this thread.
pub(crate) fn count(site: &'static str) {
    if !on() {
        return;
    }
    let why = match WHY.replace("") {
        "" => "unclassified",
        why => why,
    };
    *COUNTS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_or_insert_with(HashMap::new)
        .entry((site, why))
        .or_default() += 1;
}

/// Logs the drops counted since the last call, per frame presented.
pub(crate) fn line(presented: u64) {
    if !on() {
        return;
    }
    let frames = presented.saturating_sub(FRAMES_AT.swap(presented, Ordering::Relaxed));
    let counts = COUNTS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
        .unwrap_or_default();
    if counts.is_empty() {
        return;
    }
    tracing::info!(
        "drop reasons per frame (VICTORY_DROP_REASONS=1, over {frames} frames): {}",
        summary(counts, frames)
    );
}

/// `site: reason count`, most first, per frame, joined by `; `.
fn summary(counts: HashMap<(&'static str, &'static str), u64>, frames: u64) -> String {
    let mut all: Vec<_> = counts.into_iter().collect();
    // Ties in a fixed order, so lines compare from run to run.
    all.sort_unstable_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let frames = frames.max(1) as f64;
    all.iter()
        .take(SHOWN)
        .map(|((site, why), n)| format!("{site}: {why} {:.1}", *n as f64 / frames))
        .collect::<Vec<_>>()
        .join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_is_per_frame_and_most_first() {
        let counts = HashMap::from([
            (("draw", "IndexBuffer"), 30),
            (("indirect", "ray-only skinning"), 300),
            (("dispatch", "no host pipeline"), 30),
        ]);
        assert_eq!(
            summary(counts, 10),
            "indirect: ray-only skinning 30.0; dispatch: no host pipeline 3.0; draw: IndexBuffer 3.0"
        );
    }

    #[test]
    fn summary_survives_no_frames() {
        let counts = HashMap::from([(("mesh", "unclassified"), 4)]);
        assert_eq!(summary(counts, 0), "mesh: unclassified 4.0");
    }
}
