//! (phonescreen, #115, round 10) `VICTORY_HEAP_FIND_ENTRY=1` (live, with
//! `VICTORY_HEAP_FIND=bink` and `VICTORY_HEAP_FIND_REFS=1`): part P found
//! the Bink slot (94434) as dword 12985 of one 4 MiB buffer every material
//! draw binds (a bindless material table), in frames 692-708 only. A
//! translated material shader reads a texture's view index from that
//! table at a per-draw offset and samples `ResourceDescriptorHeap[index]`
//! (heap base 0, bits 27-31 of the word dropped). Here:
//! - a word whose low 27 bits ([`super::refs::INDEX_BITS`]) name a found
//!   slot counts as a hit too (flags in the high bits);
//! - the first [`MOST_ENTRIES`] buffer words found naming a found slot are
//!   entries: `heap find: entry found` (address, buffer, the words around
//!   it), then every frame the word is read again and `heap find: entry
//!   changed` logs each new value (old, new, low bits, whether it still
//!   names the slot);
//! - each bind of a walked frame that binds an entry's buffer is a reader;
//!   a reader whose root constants, root views or constant buffers hold a
//!   number at most [`NEAR_BYTES`] before the entry (a byte offset, a dword
//!   index or a 16-byte element index into the buffer) logs `entry reader`
//!   on its `heap find: draw` line with that number: the material whose
//!   record holds the entry, i.e. the phone screen's draw;
//! - `heap find: entry readers` after each walk counts the readers and
//!   those with a near number.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use victory_core::settings::Live;

static ENTRY: Live = Live::new("VICTORY_HEAP_FIND_ENTRY", |v| {
    u64::from(v.map(str::trim) == Some("1"))
});

/// Entries watched at most.
const MOST_ENTRIES: usize = 4;
/// `heap find: entry changed` lines of a run at most.
const MOST_CHANGE_LINES: u32 = 300;
/// A number this many bytes or fewer before an entry may be the record's
/// start (a material record is some dwords long).
pub(crate) const NEAR_BYTES: u64 = 1024;
/// Words logged on each side of an entry when it is found.
const AROUND: u64 = 8;
/// Bytes of a root view or constant buffer looked at for near numbers.
pub(crate) const MOST_NEAR_SCAN: u64 = 4 << 10;

/// Whether the switch is on.
pub(crate) fn on() -> bool {
    ENTRY.get() != 0
}

/// A buffer word that named a found slot: its address, the buffer's start
/// and bytes, its last value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Entry {
    pub(crate) address: u64,
    pub(crate) buffer: u64,
    pub(crate) len: u64,
    value: u32,
    slot: u32,
}

static ENTRIES: Mutex<Vec<Entry>> = Mutex::new(Vec::new());
static CHANGE_LINES: AtomicU32 = AtomicU32::new(0);
static WATCHED: AtomicU64 = AtomicU64::new(u64::MAX);
static READERS: AtomicU32 = AtomicU32::new(0);
static NEAR: AtomicU32 = AtomicU32::new(0);

fn read_word(address: u64) -> Option<u32> {
    let mut b = [0u8; 4];
    crate::host::read_into(address as *const u8, &mut b).then(|| u32::from_le_bytes(b))
}

/// Up to `bytes` of title memory at `address`, as words (`None` if it
/// cannot be read).
pub(crate) fn read_words(address: u64, bytes: u64) -> Option<Vec<u32>> {
    let bytes = bytes & !3;
    if address == 0 || address % 4 != 0 || bytes == 0 {
        return None;
    }
    let mut b = vec![0u8; bytes as usize];
    crate::host::read_into(address as *const u8, &mut b).then(|| {
        b.as_chunks::<4>()
            .0
            .iter()
            .map(|w| u32::from_le_bytes(*w))
            .collect()
    })
}

/// Adds an entry unless known or [`MOST_ENTRIES`] are watched: whether it
/// was added.
fn add(list: &mut Vec<Entry>, e: Entry) -> bool {
    if list.len() >= MOST_ENTRIES || list.iter().any(|k| k.address == e.address) {
        return false;
    }
    list.push(e);
    true
}

/// A buffer of `len` bytes at `buffer` holds, at dword `at`, `value`
/// naming found slot `slot` (`what`): watched from now on.
pub(crate) fn found(buffer: u64, len: u64, at: usize, slot: u32, what: &str) {
    let address = buffer + 4 * at as u64;
    let Some(raw) = read_word(address) else {
        return;
    };
    let e = Entry {
        address,
        buffer,
        len,
        value: raw,
        slot,
    };
    if !add(&mut ENTRIES.lock().unwrap_or_else(|p| p.into_inner()), e) {
        return;
    }
    let from = address.saturating_sub(4 * AROUND).max(buffer);
    let around =
        read_words(from, (address + 4 * (AROUND + 1)).min(buffer + len) - from).unwrap_or_default();
    tracing::warn!(
        frame = crate::frame::current(),
        at_s = format_args!("{:.2}", crate::snap::seconds().unwrap_or(0.0)),
        address = format_args!("{address:#x}"),
        buffer = format_args!("{buffer:#x}+{len:#x}"),
        dword = at,
        raw = format_args!("{raw:#x}"),
        low_bits = format_args!("{:#x}", raw & super::refs::INDEX_BITS),
        slot,
        what,
        around_from_dword = (from - buffer) / 4,
        around = format_args!("{around:08x?}"),
        "heap find: entry found"
    );
}

/// What a watched word's change says: its low bits name the slot.
fn names_slot(value: u32, slot: u32) -> bool {
    value & super::refs::INDEX_BITS == slot
}

/// Once a frame: every entry read again, each new value logged.
pub(crate) fn watch(frame: u64) {
    if !on() || WATCHED.swap(frame, Ordering::Relaxed) == frame {
        return;
    }
    let mut list = ENTRIES.lock().unwrap_or_else(|p| p.into_inner());
    for e in list.iter_mut() {
        let Some(now) = read_word(e.address) else {
            continue;
        };
        if now == e.value {
            continue;
        }
        let old = std::mem::replace(&mut e.value, now);
        if CHANGE_LINES.fetch_add(1, Ordering::Relaxed) < MOST_CHANGE_LINES {
            tracing::warn!(
                frame,
                at_s = format_args!("{:.2}", crate::snap::seconds().unwrap_or(0.0)),
                address = format_args!("{:#x}", e.address),
                dword = (e.address - e.buffer) / 4,
                old = format_args!("{old:#x}"),
                new = format_args!("{now:#x}"),
                low_bits = format_args!("{:#x}", now & super::refs::INDEX_BITS),
                names_slot = names_slot(now, e.slot),
                slot = e.slot,
                "heap find: entry changed"
            );
        }
    }
}

/// The entries watched.
pub(crate) fn entries() -> Vec<Entry> {
    if !on() {
        return Vec::new();
    }
    ENTRIES.lock().unwrap_or_else(|p| p.into_inner()).clone()
}

/// A bind read an entry's buffer; `near`: it holds a near number.
pub(crate) fn reader(near: bool) {
    READERS.fetch_add(1, Ordering::Relaxed);
    if near {
        NEAR.fetch_add(1, Ordering::Relaxed);
    }
}

/// After a walk: the readers of the last walked frame, counted.
pub(crate) fn summary(frame: u64) {
    if !on() {
        return;
    }
    let readers = READERS.swap(0, Ordering::Relaxed);
    let near = NEAR.swap(0, Ordering::Relaxed);
    let entries = ENTRIES.lock().unwrap_or_else(|p| p.into_inner()).len();
    tracing::warn!(
        frame,
        entries,
        readers_of_last_walked_frame = readers,
        near_of_last_walked_frame = near,
        "heap find: entry readers"
    );
}

/// The word of `words` that lies closest before an entry `off` bytes into
/// its buffer, at most [`NEAR_BYTES`] before it, read as a byte offset, a
/// dword index or a 16-byte element index: (place, value, how, bytes
/// before the entry). Zero is never taken.
pub(crate) fn nearest(words: &[u32], off: u64) -> Option<(usize, u32, &'static str, u64)> {
    const UNITS: [(u64, &str); 3] = [
        (1, "byte offset"),
        (4, "dword index"),
        (16, "16-byte element"),
    ];
    let mut best: Option<(usize, u32, &'static str, u64)> = None;
    for (i, &w) in words.iter().enumerate() {
        if w == 0 {
            continue;
        }
        for (unit, how) in UNITS {
            let at = u64::from(w) * unit;
            if at > off || off - at >= NEAR_BYTES {
                continue;
            }
            let d = off - at;
            if best.is_none_or(|b| d < b.3) {
                best = Some((i, w, how, d));
            }
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Part P: entry dword 12985 (byte 51940). A record start as a byte
    /// offset, a dword index or a 16-byte element is found, the closest
    /// first; numbers past the entry, too far before it or zero are not.
    #[test]
    fn numbers_just_before_an_entry_are_found() {
        let off = 12_985 * 4;
        assert_eq!(
            nearest(&[7, 51_900, 0], off),
            Some((1, 51_900, "byte offset", 40))
        );
        assert_eq!(
            nearest(&[12_980], off),
            Some((0, 12_980, "dword index", 20))
        );
        assert_eq!(
            nearest(&[3_246], off),
            Some((0, 3_246, "16-byte element", 4))
        );
        assert_eq!(
            nearest(&[51_000, 12_984], off),
            Some((1, 12_984, "dword index", 4)),
            "the closest wins"
        );
        assert_eq!(nearest(&[51_944, 12_986], off), None, "past the entry");
        assert_eq!(nearest(&[50_000], off), None, "too far before it");
        assert_eq!(nearest(&[0], 0), None);
    }

    /// A word that keeps the slot under flags still names it.
    #[test]
    fn flagged_values_still_name_the_slot() {
        assert!(names_slot(94_434, 94_434));
        assert!(names_slot(0x8000_0000 | 94_434, 94_434));
        assert!(!names_slot(94_435, 94_434));
    }

    /// Entries are kept once each, at most [`MOST_ENTRIES`].
    #[test]
    fn entries_are_kept_once_and_capped() {
        let e = |address| Entry {
            address,
            buffer: 0x1000,
            len: 0x1000,
            value: 0,
            slot: 1,
        };
        let mut list = Vec::new();
        assert!(add(&mut list, e(0x1004)));
        assert!(!add(&mut list, e(0x1004)), "known");
        for a in 1..MOST_ENTRIES as u64 {
            assert!(add(&mut list, e(0x1004 + 4 * a)));
        }
        assert!(!add(&mut list, e(0x1f00)), "capped");
        assert_eq!(list.len(), MOST_ENTRIES);
    }

    /// Words are read from memory as they lie; unaligned or empty reads
    /// give nothing.
    #[test]
    fn words_are_read_from_memory() {
        let data: Vec<u32> = vec![1, 94_434, 3];
        let at = data.as_ptr() as u64;
        assert_eq!(read_words(at, 12), Some(data.clone()));
        assert_eq!(read_word(at + 4), Some(94_434));
        assert_eq!(read_words(at + 2, 8), None);
        assert_eq!(read_words(at, 0), None);
    }

    #[test]
    fn the_switch_is_live() {
        victory_core::settings::check_live("VICTORY_HEAP_FIND_ENTRY", "1", on, false, true);
    }
}
