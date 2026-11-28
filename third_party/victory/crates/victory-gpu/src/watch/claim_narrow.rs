//! (feedcard, #88) `VICTORY_GUARD_CLAIM_NARROW=1` (Live, off by default): block claims held for
//! less, so write faults wait less for them.
//!
//! Why: part Q's split had about 900 claim waits a frame, 7-8 ms, nearly all write faults on the
//! title's own threads (with `VICTORY_GUARD_PUBLISH_TRY=1` publish never waits, and 876 waits
//! remained). A fault waits for whoever holds its block: a guard walk closing it, or another
//! fault. Two parts:
//!
//! 1. With `VICTORY_GUARD_4K` a fault on a protected, unaliased block opens only its 4 KiB page.
//!    Faults on different pages of one block do not touch the same state (each sets its own open
//!    bit and makes its own page writable; the clean bit, `ONLY` and `PUBLISHED` they may change
//!    end the same in any order), so they take a shared claim of the block ([`SharedClaim`]): they
//!    wait for walks and whole-block opens, not for each other. Every other claim (walks,
//!    whole-block opens, publish) is exclusive: it also needs no shared holder, and one that
//!    waits keeps the block's busy bit while the shared holders drain, so none new come in.
//! 2. A guard walk protecting a run of blocks lets go of each finished part of it (and of the
//!    clean or absent blocks it only looked at) instead of holding them all to the end.
//!
//! A shared holder may need the block whole after all (the page could not be opened alone): it
//! lets go and takes the exclusive claim as before. Not in the cache key: no shader or pipeline
//! output.

use std::sync::atomic::Ordering::SeqCst;
use std::sync::atomic::{AtomicPtr, AtomicU64};

use super::{PART, Part, part_of};

/// Per block: how many faults hold its shared claim.
static SHARED: [AtomicPtr<Part>; 1 << 16] =
    [const { AtomicPtr::new(std::ptr::null_mut()) }; 1 << 16];

static ON: victory_core::settings::Live =
    victory_core::settings::Live::flag("VICTORY_GUARD_CLAIM_NARROW");

#[cfg(test)]
thread_local! {
    pub(super) static FORCED: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

pub(super) fn on() -> bool {
    #[cfg(test)]
    if let Some(forced) = FORCED.with(std::cell::Cell::get) {
        return forced;
    }
    ON.get_bool()
}

/// Since the last frame line: faults that took a shared claim, of them those that found another
/// shared holder there (they would have waited before), and shared holders that had to take the
/// block whole after all.
pub(super) static STATS: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

/// Blocks `[from, to)` were protected (claims held, never in the fault handler): room for their
/// shared counts, which the fault handler must not allocate.
pub(super) fn ready(from: usize, to: usize) {
    let mut block = from;
    while block < to {
        let _ = part_of(&SHARED, block, true);
        block = (block / PART + 1) * PART;
    }
}

/// How many faults hold `block`'s shared claim.
pub(super) fn shared(block: usize) -> u64 {
    part_of(&SHARED, block, false).map_or(0, |p| p.0[block % PART].load(SeqCst))
}

/// A fault's shared claim of a block.
pub(super) struct SharedClaim(&'static AtomicU64);

impl SharedClaim {
    /// Waits until no exclusive claim holds `block`, then holds it shared. `None`: no room made
    /// for the block (take the exclusive claim).
    pub(super) fn wait(block: usize, busy: impl Fn() -> bool) -> Option<Self> {
        let count = &part_of(&SHARED, block, false)?.0[block % PART];
        let mut tries = 0u32;
        loop {
            let before = count.fetch_add(1, SeqCst);
            // Against `busy_try`: its busy bit set, then this count read; one of us sees the other.
            std::sync::atomic::fence(SeqCst);
            if !busy() {
                STATS[0].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if before != 0 {
                    STATS[1].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                return Some(Self(count));
            }
            count.fetch_sub(1, SeqCst);
            while busy() {
                tries += 1;
                if tries < 64 {
                    std::hint::spin_loop();
                } else {
                    std::thread::yield_now();
                }
            }
        }
    }
}

impl Drop for SharedClaim {
    fn drop(&mut self) {
        self.0.fetch_sub(1, SeqCst);
    }
}

/// A shared holder had to take its block whole.
pub(super) fn upgraded() {
    STATS[2].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}
