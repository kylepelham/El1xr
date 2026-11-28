//! (feedcard, #88) `VICTORY_GUARD_ALIAS_SKIP=1` (Live, off by default): the guard walk asks
//! whether a block it closes has other views (`cover_aliases_claimed`) from a lock-free bit per
//! block, and takes the views' lock only for a block that has some.
//!
//! Why: the walk asks for every block it closes, each time under `inplace`'s one `VIEWS` mutex
//! (a `BTreeMap` lookup per block), from all refresh threads at once, while it holds the block's
//! claim: part Q split, `alias lookups` 3-11 ms a frame of the walk's 9-26 ms, and every write
//! fault on a block being closed waits for that claim (`claims waited for` 7 ms). A block with no
//! other view gets nothing from the lookup: both of its forms return no blocks and change nothing.
//!
//! The bits are kept by `inplace` under the views' lock wherever a page's views change, always
//! (so the switch can go on at any time): a block's bit is set while the page it shows is shown in
//! 2 to `MANY_VIEWS` views, the views the lookup takes, and cleared when that stops. A walk that
//! finds no bit in its range is ordered before the change that would set one, as a lookup that
//! took the lock first would be. Not in the cache key: no shader or pipeline output.

use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering::Relaxed;

/// One bit per block: it shows a page other views show too.
static ALIASED: super::Bits = super::Bits::new();

static ON: victory_core::settings::Live =
    victory_core::settings::Live::flag("VICTORY_GUARD_ALIAS_SKIP");

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

/// Since the last frame line: lookups answered by the bits, lookups that took the lock.
pub(super) static STATS: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

/// (`inplace`, under its views' lock) The pages at `addresses` are now shown in 2 to
/// `MANY_VIEWS` views (`aliased`), or not.
pub(crate) fn note(addresses: &[usize], aliased: bool) {
    for &at in addresses {
        let block = at / super::BLOCK;
        if aliased {
            let _ = ALIASED.set_was(block);
        } else {
            ALIASED.clear_was(block);
        }
    }
}

/// Whether the alias lookup of blocks `[from, to)` can be left out: on, and none of them has
/// another view.
pub(super) fn skip(from: usize, to: usize) -> bool {
    if !on() {
        return false;
    }
    let none = !ALIASED.any(from, to);
    STATS[usize::from(!none)].fetch_add(1, Relaxed);
    none
}

/// (Tests) Whether `block` is marked as having another view.
#[cfg(test)]
pub(super) fn marked(block: usize) -> bool {
    ALIASED.any(block, block + 1)
}
