//! `VICTORY_INDIRECT_REST=0|1` (live): what an indirect record that does not
//! fit in what is left of its list's budget takes.
//!
//! Unset, the clamp is the one it always was: such a record takes the rest
//! of the budget, so every record of the list after it runs empty too. One
//! garbage record then empties all the list's later draws, and which record
//! that is may change from frame to frame (pop-in, missing geometry,
//! flicker). `=0`: it takes nothing and runs empty alone; the next record
//! that fits runs (trustno1-coder/VIctory-NVIDIA 545f890 made that their
//! default). `=1`: the old behaviour, counted.
//!
//! Set either way, the clamp counts its empty records in the budget
//! buffer's words 7 (records over what was left: the ones that set it off)
//! and 8 (records that found the budget already spent), and [`line`] logs
//! them per frame with the lists whose budget ran out.
//!
//! shortcut: the amplification stage of `VICTORY_MESH_LAUNCH=1`
//! (`launch.hlsl`) keeps taking the rest.

use std::borrow::Cow;
use std::sync::atomic::{AtomicU64, Ordering};

use victory_core::settings::Live;

/// The clamp's charge as [`super::SOURCE`] has it: a record over what is
/// left takes the rest.
pub(super) const TAKE_REST_LINE: &str =
    "            uint next = (BUDGET - before <= cost) ? BUDGET : (before + cost);";
/// What `=0` charges instead: nothing for a record over what is left.
const TAKE_NOTHING_LINES: &str =
    "            if (BUDGET - before < cost) break;\n            uint next = before + cost;";
/// Where the clamp knows a record ran empty for want of the budget (the
/// same line [`super::budget`] counts at).
const OVER_LINE: &str = "over = before >= BUDGET || BUDGET - before < cost;";
/// The budget buffer's words the counts go to (words 0-6 are taken).
pub(crate) const OVER_WORD: usize = 7;
pub(crate) const SPENT_WORD: usize = 8;

/// The switch: unset, `=1` (rest, counted), `=0` (nothing, counted).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Rest {
    Unset = 0,
    Rest = 1,
    Nothing = 2,
}

fn parse(v: Option<&str>) -> u64 {
    match v.map(str::trim) {
        Some("1") => Rest::Rest as u64,
        Some("0") => Rest::Nothing as u64,
        _ => Rest::Unset as u64,
    }
}

static MODE: Live = Live::new("VICTORY_INDIRECT_REST", parse);

pub(crate) fn mode() -> Rest {
    match MODE.get() {
        1 => Rest::Rest,
        2 => Rest::Nothing,
        _ => Rest::Unset,
    }
}

/// The clamp's source (`source`: [`super::SOURCE`], with the budget line
/// as [`super::budget`] made it) for `rest`; untouched when unset.
pub(super) fn clamp_source(source: &str, rest: Rest) -> Cow<'_, str> {
    if rest == Rest::Unset {
        return Cow::Borrowed(source);
    }
    debug_assert!(source.contains(TAKE_REST_LINE) && source.contains(OVER_LINE));
    let counted = format!(
        "{OVER_LINE}\n        if (over) {{ uint told_rest; budget.InterlockedAdd(before < BUDGET ? {}u : {}u, 1u, told_rest); }}",
        OVER_WORD * 4,
        SPENT_WORD * 4
    );
    let source = source.replace(OVER_LINE, &counted);
    Cow::Owned(if rest == Rest::Nothing {
        source.replace(TAKE_REST_LINE, TAKE_NOTHING_LINES)
    } else {
        source
    })
}

/// Since the last frame line: records over what was left, records that
/// found the budget spent, lists read back, lists whose budget ran out.
static COUNTS: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

/// What a list's last run adds to [`COUNTS`]: its records over what was
/// left (`over`) and after the budget was spent (`after`), itself if it
/// used its budget at all, and itself again if its `spent` reached 2^`log2`.
pub(super) fn tally(spent: u32, over: u32, after: u32, log2: u32) -> [u64; 4] {
    [
        u64::from(over),
        u64::from(after),
        u64::from(spent > 0 || over > 0),
        u64::from(u64::from(spent) >= 1u64 << log2),
    ]
}

/// Takes a list's last run from its budget buffer's `words` (the list's
/// own, read before they are zeroed): the clamp's counts, and whether its
/// budget of 2^`log2` ran out.
///
/// # Safety
/// `words` points at the list's mapped budget buffer (at least 9 words),
/// which the card is not writing.
pub(crate) unsafe fn take(words: *const u32, log2: u32) {
    if mode() == Rest::Unset {
        return;
    }
    // SAFETY: the caller's words.
    let (spent, over, after) = unsafe {
        (
            words.read_volatile(),
            words.add(OVER_WORD).read_volatile(),
            words.add(SPENT_WORD).read_volatile(),
        )
    };
    for (count, n) in COUNTS.iter().zip(tally(spent, over, after, log2)) {
        count.fetch_add(n, Ordering::Relaxed);
    }
}

/// The frame line's counts, with the switch set.
pub(crate) fn line(frames: f64) {
    let [over, after, lists, out] = [0, 1, 2, 3].map(|i| COUNTS[i].swap(0, Ordering::Relaxed));
    let rest = mode();
    if rest == Rest::Unset {
        return;
    }
    tracing::info!(
        "indirect rest (VICTORY_INDIRECT_REST={}): per frame {:.2} records over what was left of their list's budget, {:.2} records after the budget was spent, {:.2} of {:.1} lists ran out of budget ({over}, {after}, {out} in all)",
        if rest == Rest::Rest { 1 } else { 0 },
        over as f64 / frames,
        after as f64 / frames,
        out as f64 / frames,
        lists as f64 / frames,
    );
}
