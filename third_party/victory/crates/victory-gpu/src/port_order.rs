//! Renderer port #1's order (`VICTORY_PORT_CONSTANTS=1`: the title's 32-bit
//! graphics heap, its 174 MB constants ring among it, read and written by
//! the card in place): what keeps the title's order where the card reads
//! title memory when it runs a list, not when the list is submitted.
//!
//! `VICTORY_PORT_CONSTANTS_ORDER=1` (off by default):
//! - An indirect execution whose arguments or count lie in place is never
//!   clamped ahead of its place (in the list's prelude, `clamp_ahead`, or in
//!   a window opened by an earlier record): the culling passes the title
//!   records between the two write those records, and a clamp ahead read
//!   the ones of an earlier frame, or none (palms and trees missing for runs
//!   of frames). It is clamped (or drawn raw) at its own place.
//!   Source: avatarmanz/VIctory-7900XTX `VICTORY_PORT_CONSTANTS_ORDER`
//!   (`draw.rs::in_place_order`, `014d52dedf`), GPL-3.0, adapted: our tree
//!   has no clamp batches; its ahead paths are `clamp_ahead` and windows.
//! - A title buffer copy into memory in place goes on the card at its place
//!   in the list (`crate::list::card_copy`) instead of on the CPU at
//!   submission, where it wrote what draws recorded before it read when the
//!   card ran them (the same order as `crate::port_cbv::copy_order_on`).
//!
//! `VICTORY_PORT_CONSTANTS_ACCOUNT=1` (off by default): what shaders may
//! write in place goes into the flights' account of title memory
//! (`HostList::defer_gpu_write`), as a copy's range does, so that a CPU step
//! at a later submission that reads or writes it (a title copy, a texture's
//! memory, a step of ours) waits for those flights. Nothing is snapshot or
//! copied back for it (`Shadow::writing`, `copy_back`, `copy_back_done` and
//! the hand-backs pass over memory in place).
//! Source: fxceless/victory-fork `bcc542151d` (`VICTORY_INPLACE_CONSTANTS`,
//! `Shadow::gpu_writes`), GPL-3.0.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

fn switch(name: &str) -> bool {
    std::env::var(name).is_ok_and(|v| v.trim() == "1")
}

/// `VICTORY_PORT_CONSTANTS_ORDER=1`, with port 1 on.
pub(crate) fn order_on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    crate::port::on() && *ON.get_or_init(|| switch("VICTORY_PORT_CONSTANTS_ORDER"))
}

/// `VICTORY_PORT_CONSTANTS_ACCOUNT=1` (port 1 alone makes memory in place);
/// (El1xr) also with `VICTORY_INPLACE_WINDOWS=1`, whose shader writes in
/// place are told written when their submission is done
/// (`crate::inplace::told_written`).
pub(crate) fn account_on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| switch("VICTORY_PORT_CONSTANTS_ACCOUNT") || crate::inplace::windows_on())
}

/// Since the last line: indirect executions with arguments or count in
/// place, those kept at their place for [`order_on`]; title copies into
/// memory in place put on the card, those left on the CPU; ranges in place
/// put into the flights' account.
static STATS: [AtomicU64; 5] = [const { AtomicU64::new(0) }; 5];

/// An indirect execution with its arguments (`args_in_place`) or count
/// (`count_in_place`) in place or not: whether it is to be clamped at its
/// own place only.
pub(crate) fn indirect_at_place(args_in_place: bool, count_in_place: bool) -> bool {
    if !(args_in_place || count_in_place) {
        return false;
    }
    STATS[0].fetch_add(1, Relaxed);
    let kept = at_place(order_on(), args_in_place, count_in_place);
    if kept {
        STATS[1].fetch_add(1, Relaxed);
    }
    kept
}

/// [`indirect_at_place`]'s answer, with the switch `order`.
fn at_place(order: bool, args_in_place: bool, count_in_place: bool) -> bool {
    order && (args_in_place || count_in_place)
}

/// Whether a title copy into `target` goes on the card for [`order_on`].
pub(crate) fn copy_on_card(target: &crate::shadow::Shadow) -> bool {
    target.in_place() && order_on()
}

/// A copy [`copy_on_card`] asked for went on the card (`done`) or not.
pub(crate) fn copied(done: bool) {
    STATS[if done { 2 } else { 3 }].fetch_add(1, Relaxed);
}

/// A range in place went into the flights' account.
pub(crate) fn accounted() {
    STATS[4].fetch_add(1, Relaxed);
}

/// The frame line's, with port 1 on.
pub(crate) fn line(frames: f64) {
    if !crate::port::on() {
        return;
    }
    let [all, kept, done, left, accounted] =
        STATS.each_ref().map(|a| a.swap(0, Relaxed) as f64 / frames);
    tracing::info!(
        "renderer port 1 order per frame: {all:.1} indirect executions with arguments or count in place, {kept:.1} of them \
         clamped only at their place (VICTORY_PORT_CONSTANTS_ORDER={}); title copies into memory in place: {done:.1} on the \
         card at their place, {left:.1} left on the CPU; {accounted:.1} shader writes in place in the flights' account \
         (VICTORY_PORT_CONSTANTS_ACCOUNT={})",
        u8::from(order_on()),
        u8::from(account_on()),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_memory_in_place_is_kept_at_its_place_and_only_with_the_switch() {
        assert!(!at_place(true, false, false));
        assert!(at_place(true, true, false));
        assert!(at_place(true, false, true));
        assert!(at_place(true, true, true));
        assert!(!at_place(false, true, true));
        // Neither in place: not counted, not kept, whatever the switch.
        assert!(!indirect_at_place(false, false));
    }
}
