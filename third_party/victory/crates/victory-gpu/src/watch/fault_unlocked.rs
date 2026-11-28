//! (feedcard, #88) `VICTORY_FAULT_UNLOCKED=1` (Live, off by default): a write fault does not wait
//! for a guard walk that is closing its block's 4 KiB pages.
//!
//! Ported from trustno1-coder's `VICTORY_FAULT_UNLOCKED` (`watch.rs` `open_unlocked`, VIctory-NVIDIA
//! 64d4931 and after; measured by KyleS98's VIctory-Neo 38b768c71e: 29.87 -> 28.97 ms a frame). There
//! the faults waited for the guards' one lock: 252 of 281 ms in the handler. Ours take that lock
//! shared (`VICTORY_PORT_GUARD=1`, b5: 0.29 ms a frame of waiting) and wait instead for the block's
//! claim: part Q had about 900 claim waits a frame, 7-8 ms, nearly all write faults on the title's
//! threads. `VICTORY_GUARD_CLAIM_NARROW` took away the waits of faults for each other; what is left
//! is a fault waiting for a guard walk, which holds the claim while it makes the block's open pages
//! read-only again (`guard4k::close`, a `VirtualProtect` per run).
//!
//! With the switch (and `VICTORY_GUARD_4K=1`, `VICTORY_GUARD_4K_KEEP=0`) the walk says which block it
//! is closing ([`Closing`]); a fault on that block, protected and not aliased, opens its 4 KiB page
//! beside the close without the claim ([`fault`]), as trustno1's fault opens its block without the
//! lock. Why no write is missed:
//!
//! - The fault notes the page open before it makes it writable. If the close took the note in
//!   between (and so may have made the page read-only after the fault made it writable), the fault
//!   notes it open again after: the page is then noted open whatever its protection.
//! - The walk ends its close before it looks at anything else of the block ([`Closing::end`]): no
//!   fault starts beside it after that (the fault looks at the walk's mark after counting itself
//!   in; the walk clears the mark before it waits for that count to drain), and the faults inside
//!   are waited for. A page noted open then keeps the block not clean (the walk does not note it
//!   clean, publish it, or guard its other views with it): the next check closes the page and notes
//!   it written, as it would have for a fault that came after the walk.
//! - Everything else of the block the fault reads (protected, aliased) is the walk's to change,
//!   and the walk holds the claim until the faults inside are out.
//!
//! Not in the cache key: no shader or pipeline output.

use std::sync::atomic::Ordering::{Relaxed, SeqCst};
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, fence};

use super::{BLOCK, Guards, guard4k};

static ON: victory_core::settings::Live =
    victory_core::settings::Live::flag("VICTORY_FAULT_UNLOCKED");

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

const SLOTS: usize = 1 << 12;

/// By a few bits of the block's number: the block a walk is closing, plus one; 0 for none.
static CLOSING: [AtomicUsize; SLOTS] = [const { AtomicUsize::new(0) }; SLOTS];

/// By the same bits: faults inside [`fault`] for a block of the slot.
static INSIDE: [AtomicU32; SLOTS] = [const { AtomicU32::new(0) }; SLOTS];

/// Since the last line: faults opened beside a close, nanoseconds in them; of them, those whose
/// note the close took (noted again); closes that waited for faults inside, nanoseconds waited;
/// closes that found a page opened beside them (the block left not clean).
pub(super) static STATS: [AtomicU64; 6] = [const { AtomicU64::new(0) }; 6];

/// A walk's close of one block, its claim held (`guard4k::close`).
pub(super) struct Closing {
    block: usize,
    armed: bool,
}

impl Closing {
    pub(super) fn begin(block: usize) -> Self {
        let armed = on() && guard4k::keep_for() == 0;
        if armed {
            CLOSING[block % SLOTS].store(block + 1, SeqCst);
        }
        Self { block, armed }
    }

    /// The close is done: no fault starts beside it from here, and those inside are waited for.
    /// Whether one of them left a page of the block noted open (it must not be noted clean).
    pub(super) fn end(mut self) -> bool {
        self.finish()
    }

    fn finish(&mut self) -> bool {
        if !std::mem::take(&mut self.armed) {
            return false;
        }
        let slot = self.block % SLOTS;
        // (Another walk's block of the same slot may have taken the mark: then no fault of this
        // block goes beside, and that walk's mark is left.)
        let _ = CLOSING[slot].compare_exchange(self.block + 1, 0, SeqCst, SeqCst);
        // Against `fault`: its count raised, then the mark read; one of us sees the other.
        fence(SeqCst);
        if INSIDE[slot].load(SeqCst) != 0 {
            let began = std::time::Instant::now();
            let mut tries = 0u32;
            while INSIDE[slot].load(SeqCst) != 0 {
                tries += 1;
                if tries < 64 {
                    std::hint::spin_loop();
                } else {
                    std::thread::yield_now();
                }
            }
            STATS[3].fetch_add(1, Relaxed);
            STATS[4].fetch_add(began.elapsed().as_nanos() as u64, Relaxed);
        }
        let opened = guard4k::open_pages(self.block) != 0;
        if opened {
            STATS[5].fetch_add(1, Relaxed);
        }
        opened
    }
}

impl Drop for Closing {
    fn drop(&mut self) {
        self.finish();
    }
}

struct Inside(usize);

impl Drop for Inside {
    fn drop(&mut self) {
        INSIDE[self.0].fetch_sub(1, SeqCst);
    }
}

/// A write fault at `at` in `block` while another thread holds the block's claim: opens the 4 KiB
/// page beside the walk closing it. `None`: no walk is closing it (or the block is aliased, or the
/// page could not be opened): wait for the claim as before.
pub(super) fn fault(g: &Guards, block: usize, at: usize) -> Option<(usize, bool)> {
    debug_assert_eq!(at / BLOCK, block);
    let began = std::time::Instant::now();
    let slot = block % SLOTS;
    INSIDE[slot].fetch_add(1, SeqCst);
    let _inside = Inside(slot);
    fence(SeqCst);
    if CLOSING[slot].load(SeqCst) != block + 1 {
        return None;
    }
    // The walk holds the claim until this thread is out: these stay as read.
    // (A walk closes only blocks not clean.)
    if !g.protected.contains(block) || g.aliased.contains(block) || g.clean.contains(block) {
        return None;
    }
    let (answer, noted_again) = guard4k::fault_beside(block, at)?;
    STATS[0].fetch_add(1, Relaxed);
    STATS[1].fetch_add(began.elapsed().as_nanos() as u64, Relaxed);
    if noted_again {
        STATS[2].fetch_add(1, Relaxed);
    }
    Some(answer)
}

/// With the 4 KiB guard line.
pub(super) fn line(frames: f64) {
    if frames <= 0.0 {
        return;
    }
    let [faults, ns, again, waits, waited, left] =
        [0, 1, 2, 3, 4, 5].map(|i| STATS[i].swap(0, Relaxed) as f64);
    tracing::info!(
        "write faults beside a closing walk per frame (VICTORY_FAULT_UNLOCKED={}): {:.0} opened without the claim \
         in {:.2} ms ({:.1} noted again after the close took them); closes that waited for them {:.1} ({:.2} ms), \
         left not clean {:.1}",
        u8::from(on()),
        faults / frames,
        ns / frames / 1e6,
        again / frames,
        waits / frames,
        waited / frames / 1e6,
        left / frames,
    );
}
