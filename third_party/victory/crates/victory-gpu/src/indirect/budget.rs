//! `VICTORY_INDIRECT_BUDGET_LOG2=<20..31>`: each list's indirect budget
//! (vertex instances or threads; `BUDGET` in the clamp's [`super::SOURCE`])
//! as a power of two, and a count of the records the budget empties.
//!
//! Unset, the clamp is compiled as before: 2^27, no count. Set (27 too, to
//! count at the old budget), the clamp is compiled with 2^N and adds one to
//! the budget buffer's word 6 for each record with work that ran empty for
//! want of its list's budget; [`line`] logs those per frame. At 2^27 the
//! title's own lists ran out in normal play and the records cut changed from
//! frame to frame (flicker, the ocean vanishing on an RTX 5080);
//! trustno1-coder/VIctory-NVIDIA #72 raised it to 2^30. A list of garbage
//! records still stops at the budget: 2^30 threads is a few tenths of a
//! second of card time, under the TDR timeout.
//!
//! shortcut: the amplification stage of `VICTORY_MESH_LAUNCH=1`
//! (`launch.hlsl`, compiled ahead into `launch_code.rs`) keeps 2^27; it
//! charges the same budget word, so with both on its records stop at 2^27
//! while the clamp's run on to 2^N.

use std::borrow::Cow;
use std::sync::atomic::{AtomicU64, Ordering};

/// The clamp's budget line as [`super::SOURCE`] has it.
const DEFAULT_LINE: &str = "static const uint BUDGET = 1u << 27;";
/// Where the clamp knows a record ran empty for want of the budget.
const OVER_LINE: &str = "over = before >= BUDGET || BUDGET - before < cost;";
/// The budget buffer's word the count goes to (words 0-5 are taken).
pub(crate) const EMPTIED_WORD: usize = 6;

/// The switch's power of two, if set to one in range.
pub(crate) fn log2() -> Option<u32> {
    static LOG2: std::sync::OnceLock<Option<u32>> = std::sync::OnceLock::new();
    *LOG2.get_or_init(|| {
        let v = std::env::var("VICTORY_INDIRECT_BUDGET_LOG2").ok()?;
        match v.trim().parse::<u32>() {
            Ok(n) if (20..=31).contains(&n) => {
                tracing::info!("indirect budget: 2^{n} per list (VICTORY_INDIRECT_BUDGET_LOG2)");
                Some(n)
            }
            _ => {
                tracing::warn!(value = %v, "VICTORY_INDIRECT_BUDGET_LOG2 must be 20..31: the default 2^27, no count");
                None
            }
        }
    })
}

/// The clamp's source for budget 2^`log2` with the count, or `source`
/// untouched for `None`.
pub(super) fn clamp_source(source: &str, log2: Option<u32>) -> Cow<'_, str> {
    let Some(n) = log2 else {
        return Cow::Borrowed(source);
    };
    debug_assert!(source.contains(DEFAULT_LINE) && source.contains(OVER_LINE));
    let counted = format!(
        "{OVER_LINE}\n        if (over) {{ uint emptied; budget.InterlockedAdd({}, 1u, emptied); }}",
        EMPTIED_WORD * 4
    );
    Cow::Owned(
        source
            .replace(
                DEFAULT_LINE,
                &format!("static const uint BUDGET = 1u << {n};"),
            )
            .replace(OVER_LINE, &counted),
    )
}

/// Records emptied by the budget since the last frame line (counted on the
/// card, taken at a later submission).
pub(crate) static EMPTIED: AtomicU64 = AtomicU64::new(0);

/// The frame line's count, with the switch set.
pub(crate) fn line(frames: f64) {
    let emptied = EMPTIED.swap(0, Ordering::Relaxed);
    if let Some(n) = log2() {
        tracing::info!(
            "indirect budget: {:.2} records a frame ran empty for want of their list's budget (2^{n}), {emptied} in all",
            emptied as f64 / frames
        );
    }
}
