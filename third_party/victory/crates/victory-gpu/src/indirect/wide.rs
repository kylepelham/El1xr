//! `VICTORY_INDIRECT_WIDE=1` (off by default): an indirect compute record that
//! asks for more than 65,535 groups in x runs them all, up to the clamp's
//! `GROUPS` (2^20), instead of being cut to 65,535.
//!
//! The title's tile lists are dispatches of one group per tile; at 4K about
//! one record a frame asks for 98,000-108,000 groups (the frame line's
//! "largest group count in x"). Cut, the last tiles of the list (the lowest
//! of the picture) never ran and kept what they showed before, or black.
//!
//! - A program with a group base (`victory_shader::emit::USER_GROUP_BASE`)
//!   is already run in [`super::SPLIT`] parts of at most 65,535 groups, each
//!   part told its first group: within D3D12's limit up to 131,070 groups.
//!   Past that the last part takes the rest, so it is more than 65,535.
//! - Any other program reads its group number from the dispatch alone, so
//!   splitting it would run the first groups again: the record runs whole,
//!   in one dispatch of more than 65,535 groups in x.
//!
//! D3D12 leaves an indirect dispatch of more than 65,535 groups in a
//! dimension undefined; NVIDIA and AMD cards run every group of it under its
//! own number (`split_raw_tests` on this PC's RTX 5090 at 115,200;
//! bolado-dev/mandy-port `e5e11b3e` on an RX 6700 XT and an RTX 4060 up to
//! 2^20). The clamp still bounds y and z at 65,535, the groups of a record at
//! 2^20 and every list's budget, so garbage records stay cheap.
//!
//! Counted in the budget buffer's word [`WIDE_WORD`]: records run with more
//! than 65,535 groups in one dispatch; [`line`] logs them per frame with the
//! switch on.

use std::borrow::Cow;
use std::sync::atomic::{AtomicU64, Ordering};

/// The budget buffer's word the count goes to (words 0-8 are taken).
pub(crate) const WIDE_WORD: usize = 9;

/// The clamp's group bound of a compute record as [`super::SOURCE`] has it.
const GROUPS_CHECK: &str = "        // x * y fits 32 bits; the whole count may not.
        if (g.z != 0 && g.x * g.y > GROUPS / g.z) {
            g = uint3(0, 0, 0);
        }
";
/// Where the parts of a split dispatch take their groups.
const PART_LINE: &str = "uint x = groups.x > before ? min(groups.x - before, 65535u) : 0u;";

/// The switch, read once.
pub(crate) fn on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        let on = std::env::var("VICTORY_INDIRECT_WIDE").is_ok_and(|v| v.trim() == "1");
        if on {
            tracing::info!(
                "indirect compute records run up to 2^20 groups in x (VICTORY_INDIRECT_WIDE)"
            );
        }
        on
    })
}

/// The clamp's source with wide records when `on`, or `source` untouched.
pub(super) fn clamp_source(source: &str, on: bool) -> Cow<'_, str> {
    if !on {
        return Cow::Borrowed(source);
    }
    debug_assert!(source.contains(GROUPS_CHECK) && source.contains(PART_LINE));
    let wide = format!(
        "        // (VICTORY_INDIRECT_WIDE) Up to GROUPS in x run as asked.
        if (asked.x <= GROUPS) {{
            g.x = asked.x;
        }}
        // x * y may not fit 32 bits now; the whole count may not either.
        if ((g.y != 0 && g.x > GROUPS / g.y) || (g.z != 0 && g.x * g.y > GROUPS / g.z)) {{
            g = uint3(0, 0, 0);
        }}
        if (plausible && g.x > (split ? SPLIT * 65535u : 65535u)) {{
            budget.InterlockedAdd({}, 1u, noted);
        }}
",
        WIDE_WORD * 4
    );
    let last_part = "uint x = groups.x > before \
                     ? (part + 1 == SPLIT ? groups.x - before : min(groups.x - before, 65535u)) : 0u;";
    Cow::Owned(
        source
            .replace(GROUPS_CHECK, &wide)
            .replace(PART_LINE, last_part),
    )
}

/// Records run wide since the last frame line (counted on the card, taken at
/// a later submission).
pub(crate) static WIDE: AtomicU64 = AtomicU64::new(0);

/// Takes the count from a list's budget words (`words`: the mapped buffer,
/// at least [`WIDE_WORD`] + 1 words) and zeroes it.
///
/// # Safety
/// `words` points at the list's mapped budget buffer, which nothing runs on.
pub(crate) unsafe fn take(words: *mut u32) {
    // SAFETY: as the caller promises.
    let n = unsafe {
        let at = words.add(WIDE_WORD);
        let n = at.read_volatile();
        at.write_volatile(0);
        n
    };
    WIDE.fetch_add(u64::from(n), Ordering::Relaxed);
}

/// The frame line's count, with the switch on.
pub(crate) fn line(frames: f64) {
    let wide = WIDE.swap(0, Ordering::Relaxed);
    if on() {
        tracing::info!(
            "indirect wide: {:.2} compute records a frame ran more than 65535 groups in x in one dispatch, {wide} in all",
            wide as f64 / frames
        );
    }
}
