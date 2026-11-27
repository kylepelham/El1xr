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

/// The second half of the port (Vice-Tay PR #238): a window of title
/// memory outside buffers (`super::loose`) is made outside the windows'
/// table's lock too (`VICTORY_LOOSE_ALLOC_OUTSIDE=1`; off unless set).
pub(super) fn loose_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| switch(std::env::var_os("VICTORY_LOOSE_ALLOC_OUTSIDE").as_deref()))
}

type Windows = BTreeMap<(u64, u64), Arc<Shadow>>;

/// Windows being made, by key: the thread making one holds its ticket.
static LOOSE_BUILDING: Mutex<BTreeMap<(u64, u64), std::sync::Weak<Mutex<()>>>> =
    Mutex::new(BTreeMap::new());

/// The ticket of window `key`: the one being held, or a new one.
fn pending_ticket(
    table: &Mutex<BTreeMap<(u64, u64), std::sync::Weak<Mutex<()>>>>,
    key: (u64, u64),
) -> Ticket {
    let mut map = table.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(there) = map.get(&key).and_then(std::sync::Weak::upgrade) {
        return there;
    }
    if map.len() >= 64 {
        map.retain(|_, weak| weak.strong_count() > 0);
    }
    let ticket = Ticket::default();
    map.insert(key, Arc::downgrade(&ticket));
    ticket
}

/// What a window's maker does once it holds the ticket and looked again.
#[derive(Debug, PartialEq, Eq)]
enum Found {
    /// One is there now: take it.
    There,
    /// The ask selects another window now: ask again.
    Other,
    /// Still none: make it (or put the one made in).
    Make,
}

fn found(map: &Windows, key: (u64, u64), selected: (u64, u64)) -> Found {
    if map.contains_key(&selected) {
        Found::There
    } else if selected != key {
        Found::Other
    } else {
        Found::Make
    }
}

/// Makes window `key` by `make` holding its ticket, not the windows'
/// table, and puts it in unless one the ask selects (`select`) came
/// meanwhile, which is taken instead. `note`: what an ask does to the
/// window it takes. `None`: the ask selects another window now, and is to
/// be asked again.
pub(super) fn loose_outside(
    key: (u64, u64),
    select: impl Fn(&Windows) -> (u64, u64),
    make: impl FnOnce() -> Option<Arc<Shadow>>,
    note: impl Fn(&Shadow),
) -> Option<Option<Arc<Shadow>>> {
    let ticket = pending_ticket(&LOOSE_BUILDING, key);
    let _only = ticket.lock().unwrap_or_else(|e| e.into_inner());
    {
        let map = super::LOOSE.read().unwrap_or_else(|e| e.into_inner());
        let selected = select(&map);
        match found(&map, key, selected) {
            Found::There => {
                let shadow = map.get(&selected)?.clone();
                note(&shadow);
                return Some(Some(shadow));
            }
            Found::Other => return None,
            Found::Make => {}
        }
    }
    let Some(made) = make() else {
        return Some(None);
    };
    let mut map = super::loose_write();
    let selected = select(&map);
    match found(&map, key, selected) {
        Found::There => {
            let there = map.get(&selected)?.clone();
            note(&there);
            drop(map);
            // Not put in: it goes, outside the table's lock.
            drop(made);
            Some(Some(there))
        }
        Found::Other => {
            drop(map);
            drop(made);
            None
        }
        Found::Make => {
            note(&made);
            map.insert(key, made.clone());
            Some(Some(made))
        }
    }
}

/// The rest of #238: the tables' read-only walks take them to read, not
/// to write (which also made every recording thread forget what its
/// look-ups found, `super::SHAPE`), and a copy of a view's own bytes that
/// is there already is found beside the other readers
/// (`VICTORY_SHADOW_READ_LOCKS=1`; off unless set).
pub(super) fn read_locks_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| switch(std::env::var_os("VICTORY_SHADOW_READ_LOCKS").as_deref()))
}

/// A table taken for a walk that changes nothing: to read with
/// [`read_locks_on`], to write as before without.
pub(super) enum Walk<'a, T> {
    Read(std::sync::RwLockReadGuard<'a, T>),
    Write(std::sync::RwLockWriteGuard<'a, T>),
}

impl<T> std::ops::Deref for Walk<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        match self {
            Walk::Read(guard) => guard,
            Walk::Write(guard) => guard,
        }
    }
}

/// `table` for a walk: `write` takes it as before (and counts the taking).
pub(super) fn walk<'a, T>(
    table: &'a std::sync::RwLock<T>,
    read: bool,
    write: impl FnOnce() -> std::sync::RwLockWriteGuard<'a, T>,
) -> Walk<'a, T> {
    if read {
        Walk::Read(table.read().unwrap_or_else(|e| e.into_inner()))
    } else {
        Walk::Write(write())
    }
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

    /// One ticket per window being made: a second asker of the same
    /// window waits for the first; another window's does not.
    #[test]
    fn askers_of_one_window_share_its_ticket() {
        let table = Mutex::new(BTreeMap::new());
        let a = pending_ticket(&table, (1, 2));
        let b = pending_ticket(&table, (1, 2));
        assert!(Arc::ptr_eq(&a, &b));
        let c = pending_ticket(&table, (3, 2));
        assert!(!Arc::ptr_eq(&a, &c));
        drop((a, b));
        // Let go of: the next asker makes a new one.
        let d = pending_ticket(&table, (1, 2));
        assert_eq!(Arc::strong_count(&d), 1);
    }

    /// The window is taken if one the ask selects is there, asked for
    /// again if the ask selects another, and made otherwise.
    #[test]
    fn a_maker_takes_what_came_meanwhile_or_asks_again() {
        let map: Windows = BTreeMap::new();
        assert_eq!(found(&map, (0x1000, 64), (0x1000, 64)), Found::Make);
        assert_eq!(found(&map, (0x1000, 64), (0x1000, 128)), Found::Other);
    }

    /// A walk to read leaves the table to other readers; one to write
    /// takes it alone, as before.
    #[test]
    fn a_walk_reads_beside_others_only_when_asked() {
        let table = std::sync::RwLock::new(7u32);
        {
            let walk = walk(&table, true, || table.write().unwrap());
            assert_eq!(*walk, 7);
            assert!(table.try_read().is_ok());
        }
        {
            let walk = walk(&table, false, || table.write().unwrap());
            assert_eq!(*walk, 7);
            assert!(table.try_read().is_err());
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
