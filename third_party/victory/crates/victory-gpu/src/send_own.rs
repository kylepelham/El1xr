//! `VICTORY_SEND_OWN=1` (off by default; after rorodev6/victory-megafork-amd
//! `18c01292113e`, avatarmanz lineage): each submission's sends to the card
//! sides of title buffer copies read bytes the submission owns, taken from
//! the CPU side when it is prepared, instead of the CPU side itself when the
//! card comes to the copy.
//!
//! The sync list (`Host::card_before`) copies a copy's changed ranges, and
//! its masked blocks (`CARD_PATCH`), from the CPU side (`staging`) at the
//! time the card runs it. With the relay (`VICTORY_RELAY=2`) submissions are
//! prepared while earlier ones are still in the air, and the next refresh
//! writes newer title bytes into that same CPU side: a send still queued
//! then puts the later submission's bytes under the earlier one's shaders
//! (upstream test: three submissions copying one value each to the same
//! bytes all read the last, 12 of 12). Taken into a pooled upload chunk
//! (`crate::windowup`) at preparing, a send reads what its own submission
//! was prepared with; the chunk lives until the plan is let go
//! (`Host::card_done`, after its submission ran).
//!
//! Masked blocks go into the chunk whole (4 KiB each, in table order); the
//! masked pass is told so by bit 31 of its block count and reads block `n`
//! of a run at `n * 4096` from where the run's blocks begin ([`patch_source`]).

use std::borrow::Cow;
use std::sync::atomic::{AtomicU64, Ordering};

/// A masked block's bytes (`CARD_PATCH`).
pub(crate) const BLOCK: usize = 4096;
/// Bit of the masked pass's block count: its blocks are read from owned bytes.
pub(crate) const OWN_BIT: u32 = 1 << 31;

/// The switch, read once.
pub(crate) fn on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        let on = std::env::var("VICTORY_SEND_OWN").is_ok_and(|v| v.trim() == "1");
        if on {
            tracing::info!(
                "sends to card copies read bytes their submission took when prepared (VICTORY_SEND_OWN)"
            );
        }
        on
    })
}

const PLAIN_LOAD: &str = "uint a = entry.x + word * 4u;
    to.Store(a, (to.Load(a) & ~keep) | (from.Load(a) & keep));";
const PLAIN_SLOT: &str = "if (slot >= blocks) return;";

/// The masked pass's source: with `on`, a block count with [`OWN_BIT`] set
/// reads block `slot` at `slot * 4096` of `from` (the run's owned blocks);
/// without the bit it is the pass as it was. Without `on`, `source` untouched.
pub(crate) fn patch_source(source: &str, on: bool) -> Cow<'_, str> {
    if !on {
        return Cow::Borrowed(source);
    }
    debug_assert!(source.contains(PLAIN_LOAD) && source.contains(PLAIN_SLOT));
    Cow::Owned(
        source
            .replace(PLAIN_SLOT, "if (slot >= (blocks & 0x7FFFFFFFu)) return;")
            .replace(
                PLAIN_LOAD,
                "uint a = entry.x + word * 4u;
    uint src = (blocks & 0x80000000u) != 0u ? slot * 4096u + word * 4u : a;
    to.Store(a, (to.Load(a) & ~keep) | (from.Load(src) & keep));",
            ),
    )
}

/// Where each piece goes in one chunk: masked blocks first (each [`BLOCK`]
/// bytes, so a run's first block stays 4-byte aligned), then the copies'
/// ranges back to back. Returns (offsets of the blocks, offsets of the
/// ranges, bytes in all).
pub(crate) fn layout(blocks: usize, ranges: &[(usize, usize)]) -> (Vec<u64>, Vec<u64>, u64) {
    let block_at: Vec<u64> = (0..blocks).map(|i| (i * BLOCK) as u64).collect();
    let mut at = (blocks * BLOCK) as u64;
    let range_at = ranges
        .iter()
        .map(|&(a, b)| {
            let here = at;
            at += (b - a) as u64;
            here
        })
        .collect();
    (block_at, range_at, at)
}

/// Bytes taken, sends read from owned bytes, masked blocks taken, chunks
/// that could not be had (the send read the CPU side as before), since the
/// last frame line.
pub(crate) static TAKEN: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

/// The frame line, with the switch on.
pub(crate) fn line(frames: f64) {
    let [bytes, sends, blocks, missed] = [0, 1, 2, 3].map(|i| TAKEN[i].swap(0, Ordering::Relaxed));
    if on() {
        tracing::info!(
            "send own: {:.2} MB a frame taken for sends to card copies at preparing ({:.0} copies, {:.0} masked blocks a frame), {missed} copies without a chunk",
            bytes as f64 / frames / (1u64 << 20) as f64,
            sends as f64 / frames,
            blocks as f64 / frames
        );
    }
}

#[cfg(test)]
#[path = "send_own_tests.rs"]
mod tests;
