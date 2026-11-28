//! `VICTORY_NATIVE_BVH_BATCH=1` (off by default, with `VICTORY_NATIVE_BVH=1`):
//! the builds of one title call (a batched build of 10-20 items) each get
//! their own part of one scratch arena and run side by side on the card,
//! with one barrier where a build depends on another (a top level after
//! bottom levels, a second build of the same structure, a refit of one built
//! just before) or when the arena is full, and one at the end of the call.
//!
//! Without it every build shares one scratch buffer and is followed by a
//! barrier, which makes the card run them one by one: on the RTX 5090
//! (`refit_bench`) 512 small bottom levels took 62.9 ms that way against
//! 0.73 ms side by side, and 256 large ones 51.6 ms against 1.1 ms.

use std::sync::OnceLock;

/// Whether builds of one call share a scratch arena (`VICTORY_NATIVE_BVH_BATCH=1`).
pub fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_NATIVE_BVH_BATCH").is_some_and(|v| v == "1"))
}

/// The arena's least size: a call's bottom levels fit without a barrier
/// (the title's scratch sizes are tens of KiB to a few MiB each).
pub(super) const ARENA_MIN: u64 = 32 << 20;

/// The builds recorded since the last barrier in one title call.
#[derive(Debug, Default)]
pub(super) struct Batch {
    /// The arena bytes they took.
    offset: u64,
    /// Their destinations (title addresses).
    pending: Vec<u64>,
    /// One of them is a bottom level.
    bottoms: bool,
}

impl Batch {
    /// Whether a barrier must come before the next build, a top level
    /// (`top`) into `dest` that reads `source` (0: none) and needs `need`
    /// bytes of an arena of `arena` bytes: it would read or write what a
    /// pending build writes, read pending bottom levels, or not fit.
    pub(super) fn must_flush(
        &self,
        top: bool,
        dest: u64,
        source: u64,
        need: u64,
        arena: u64,
    ) -> bool {
        if self.pending.is_empty() {
            return false;
        }
        (top && self.bottoms)
            || self.pending.contains(&dest)
            || (source != 0 && self.pending.contains(&source))
            || self.offset + need > arena
    }

    /// A barrier was recorded: nothing is pending, the arena is free.
    pub(super) fn flushed(&mut self) {
        self.offset = 0;
        self.pending.clear();
        self.bottoms = false;
    }

    /// The arena offset for a build into `dest` needing `need` bytes
    /// (a multiple of 256, so every part stays aligned).
    pub(super) fn take(&mut self, top: bool, dest: u64, need: u64) -> u64 {
        let at = self.offset;
        self.offset += need;
        self.pending.push(dest);
        self.bottoms |= !top;
        at
    }

    /// The arena bytes in use: an arena this small must grow first.
    pub(super) fn used(&self) -> u64 {
        self.offset
    }

    pub(super) fn pending(&self) -> bool {
        !self.pending.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn off_by_default() {
        assert!(std::env::var_os("VICTORY_NATIVE_BVH_BATCH").is_none());
        assert!(!on());
    }

    #[test]
    fn independent_bottom_levels_share_one_barrier() {
        let mut b = Batch::default();
        let arena = 1 << 20;
        let mut offsets = Vec::new();
        for dest in [0x1000u64, 0x2000, 0x3000] {
            assert!(!b.must_flush(false, dest, 0, 0x4000, arena));
            offsets.push(b.take(false, dest, 0x4000));
        }
        // Each its own part of the arena.
        assert_eq!(offsets, [0, 0x4000, 0x8000]);
        assert!(b.pending());
    }

    #[test]
    fn dependencies_and_a_full_arena_need_a_barrier() {
        let mut b = Batch::default();
        let arena = 0x10000;
        b.take(false, 0x1000, 0x4000);
        // A top level reads the bottom levels.
        assert!(b.must_flush(true, 0x9000, 0, 0x100, arena));
        // The same structure again, or a refit of a pending one.
        assert!(b.must_flush(false, 0x1000, 0, 0x100, arena));
        assert!(b.must_flush(false, 0x2000, 0x1000, 0x100, arena));
        // More scratch than is left.
        assert!(b.must_flush(false, 0x2000, 0, 0xd000, arena));
        assert!(!b.must_flush(false, 0x2000, 0, 0xc000, arena));
        b.flushed();
        assert!(!b.pending());
        assert_eq!(b.used(), 0);
        // After the barrier the top level goes, and tops do not wait on tops.
        assert!(!b.must_flush(true, 0x9000, 0, 0x100, arena));
        b.take(true, 0x9000, 0x100);
        assert!(!b.must_flush(true, 0xa000, 0, 0x100, arena));
    }
}
