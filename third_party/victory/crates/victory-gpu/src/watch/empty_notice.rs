//! `VICTORY_EMPTY_REMAP=1` (off by default): a remap or unmap notice of no
//! bytes (`len == 0`) changes nothing. Without it such a notice still goes
//! through [`crate::title_memory_remapped`]: it makes the texture slots
//! over its address due, and [`crate::inplace::views_gone`] then asks its
//! view map for the range `at + 1..at`, which panics while any view is
//! known. (In mc3yohan's tree it forgot the live in-place view holding the
//! address instead.) No such panic is in our logs so far, so the runtime
//! may never send one; the switch makes sure.
//!
//! Source: mc3yohan-lab/VIctory-RTX-Mc3yohan 7461bfe5b6 (from TaynanGT/Vice-Tay #351).

use std::sync::atomic::{AtomicU64, Ordering};

/// Zero-length notices left alone.
static SKIPPED: AtomicU64 = AtomicU64::new(0);

/// Whether the switch is on.
/// Live: each notice takes it as it is when it comes.
fn on() -> bool {
    static ON: victory_core::settings::Live =
        victory_core::settings::Live::flag("VICTORY_EMPTY_REMAP");
    ON.get_bool()
}

/// Whether a notice of `[at, at + len)` is to be left alone: with the switch
/// on, one of no bytes. Logs the first few and then each power of two.
pub(crate) fn skip(at: usize, len: usize) -> bool {
    if len != 0 || !on() {
        return false;
    }
    let n = SKIPPED.fetch_add(1, Ordering::Relaxed) + 1;
    if n <= 8 || n.is_power_of_two() {
        tracing::info!(
            "VICTORY_EMPTY_REMAP=1: a zero-length remap notice at {at:#x} left alone ({n} so far)"
        );
    }
    true
}

#[cfg(test)]
mod tests {
    use crate::inplace::{Cover, cover, title_memory_view};

    const K64: usize = 64 << 10;

    /// Zero-length notices inside a view: with the switch the view stays
    /// and nothing panics (without it `views_gone` panics on its range).
    /// Also the switch's live check, in the same test so no other test
    /// flips it while this one runs.
    #[test]
    fn a_zero_length_notice_keeps_the_view() {
        if !victory_core::settings::check_live("VICTORY_EMPTY_REMAP", "1", super::on, false, true) {
            return;
        }
        let at = 0x7_4800_0000usize;
        title_memory_view(at, 4 * K64, 78, 0);
        assert_eq!(cover(at, 4 * K64), Cover::Run(78, 0));
        victory_core::settings::set("VICTORY_EMPTY_REMAP", "1").unwrap();
        crate::title_memory_remapped(at, 0);
        crate::title_memory_remapped(at + K64 + 1, 0);
        crate::inplace::views_gone(at + 2 * K64, 0);
        let kept = cover(at, 4 * K64);
        // A notice with bytes still forgets the view.
        crate::title_memory_remapped(at + K64, K64);
        let gone = cover(at, K64);
        victory_core::settings::set("VICTORY_EMPTY_REMAP", "").unwrap();
        assert_eq!(kept, Cover::Run(78, 0));
        assert_eq!(gone, Cover::None);
    }
}
