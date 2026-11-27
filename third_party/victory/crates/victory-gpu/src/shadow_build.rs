//! A title buffer's host copy is made outside the table of title buffers'
//! lock (`VICTORY_SHADOW_ALLOC_OUTSIDE=1`; off unless set). Making one is
//! a buffer from the driver (and, for a big one, a second in the card's
//! memory), and every recording thread looks up every buffer view a draw
//! names in that table: with the lock held to write, all of them waited
//! for each copy made, in the streaming window most. The entry gets a
//! ticket instead; the thread that set it makes the copy holding only the
//! ticket, and puts it in if the entry is still the one the ask selects
//! and still holds that ticket (the title did not let the buffer go or
//! register another there meanwhile). Others that ask for the same buffer
//! wait for the ticket, not for the table, and then find the copy.
//!
//! Source: TaynanGT/Vice-Tay PR #199 (NotADonky43 #196), ported by hand:
//! their numbers, RTX 5070 Ti, the table's read lock waited for through
//! the first heavy present 21.9 s -> 0.2 s.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use super::{BUFFERS, Entry, Shadow, buffers, lookup_tables};

/// The switch (`VICTORY_SHADOW_ALLOC_OUTSIDE`; on only when `1`).
pub(super) fn on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| switch(std::env::var_os("VICTORY_SHADOW_ALLOC_OUTSIDE").as_deref()))
}

fn switch(value: Option<&std::ffi::OsStr>) -> bool {
    value.is_some_and(|v| v == "1")
}

/// A copy being made: the thread making it holds the lock.
pub(super) type Ticket = Arc<Mutex<()>>;

/// The title buffer an ask `[address, end)` selects, as `lookup_tables`
/// selects it: among the 64 that start at or below it, the nearest that
/// holds it all.
pub(super) fn buffer_key(
    map: &BTreeMap<(u64, usize), Entry>,
    address: u64,
    end: u64,
) -> Option<(u64, usize)> {
    map.range(..=(address, usize::MAX))
        .rev()
        .take(64)
        .find(|(_, e)| e.end >= end)
        .map(|(k, _)| *k)
}

/// Whether `key` is still what the ask selects, with `ticket` on it.
fn still(
    map: &BTreeMap<(u64, usize), Entry>,
    address: u64,
    end: u64,
    key: (u64, usize),
    ticket: &Ticket,
) -> bool {
    buffer_key(map, address, end) == Some(key)
        && map
            .get(&key)
            .and_then(|e| e.building.as_ref())
            .is_some_and(|t| Arc::ptr_eq(t, ticket))
}

/// Makes the copy of title buffer `key` (`len` bytes, `size` rounded) by
/// `make`, holding `ticket` and not the table, and puts it in if the entry
/// still is the ask's (`address`, `ask_len`, `any`; `end` its end) and
/// still holds `ticket`; otherwise asks again.
#[allow(clippy::too_many_arguments)]
pub(super) fn made_outside(
    address: u64,
    ask_len: u64,
    any: bool,
    end: u64,
    key: (u64, usize),
    ticket: Ticket,
    make: impl FnOnce() -> Option<Arc<Shadow>>,
) -> Option<Arc<Shadow>> {
    let _only = ticket.lock().unwrap_or_else(|e| e.into_inner());
    {
        let map = BUFFERS.read().unwrap_or_else(|e| e.into_inner());
        if !still(&map, address, end, key, &ticket) {
            drop(map);
            return lookup_tables(address, ask_len, any);
        }
    }
    let made = make();
    let mut map = buffers();
    if !still(&map, address, end, key, &ticket) {
        drop(map);
        // Not put in: it goes, outside the table's lock.
        drop(made);
        return lookup_tables(address, ask_len, any);
    }
    let entry = map.get_mut(&key)?;
    entry.building = None;
    // One put in meanwhile another way (in place, or its window's): that
    // one serves, so that the buffer has one copy.
    if let Some(there) = &entry.shadow {
        let there = there.clone();
        drop(map);
        drop(made);
        return Some(there);
    }
    let shadow = made?;
    entry.shadow = Some(shadow.clone());
    Some(shadow)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(end: u64, building: Option<Ticket>) -> Entry {
        Entry {
            end,
            shadow: None,
            building,
        }
    }

    #[test]
    fn the_switch_is_off_unless_one() {
        assert!(!switch(None));
        assert!(!switch(Some(std::ffi::OsStr::new("0"))));
        assert!(switch(Some(std::ffi::OsStr::new("1"))));
    }

    /// The nearest buffer below that holds the whole ask.
    #[test]
    fn the_ask_selects_the_nearest_buffer_that_holds_it() {
        let mut map = BTreeMap::new();
        map.insert((0x1000, 1), entry(0x9000, None));
        map.insert((0x2000, 2), entry(0x3000, None));
        assert_eq!(buffer_key(&map, 0x2100, 0x2200), Some((0x2000, 2)));
        assert_eq!(buffer_key(&map, 0x2100, 0x4000), Some((0x1000, 1)));
        assert_eq!(buffer_key(&map, 0x0100, 0x0200), None);
    }

    /// The copy is put in only while the entry the ask selects holds the
    /// ticket: not once the title registered another buffer there, nor
    /// once a buffer nearer the ask came.
    #[test]
    fn the_copy_goes_in_only_while_the_entry_holds_its_ticket() {
        let ticket: Ticket = Arc::default();
        let mut map = BTreeMap::new();
        map.insert((0x1000, 1), entry(0x9000, Some(ticket.clone())));
        assert!(still(&map, 0x2000, 0x2100, (0x1000, 1), &ticket));
        // Registered anew (no ticket, or another's).
        map.insert((0x1000, 1), entry(0x9000, None));
        assert!(!still(&map, 0x2000, 0x2100, (0x1000, 1), &ticket));
        map.insert((0x1000, 1), entry(0x9000, Some(Arc::default())));
        assert!(!still(&map, 0x2000, 0x2100, (0x1000, 1), &ticket));
        // A nearer buffer now holds the ask.
        map.insert((0x1000, 1), entry(0x9000, Some(ticket.clone())));
        map.insert((0x1800, 3), entry(0x2800, None));
        assert!(!still(&map, 0x2000, 0x2100, (0x1000, 1), &ticket));
        // Let go of.
        map.clear();
        assert!(!still(&map, 0x2000, 0x2100, (0x1000, 1), &ticket));
    }
}
