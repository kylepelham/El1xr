//! (phonescreen, #115, round 8) The buffers `VICTORY_HEAP_FIND_REFS`
//! searches, and what it spends on them.
//!
//! Part K hit the 256 MiB a frame cap (`refs_full=true`) on the walked
//! frames past 52 s, so buffers went unread. A bigger cap is not the fix:
//! most of those bytes are the same buffers frame after frame. Here:
//! - a buffer of at most [`SMALL`] bytes (constant buffers, small
//!   structured buffers) is read whole, from a budget of its own
//!   ([`SMALL_BUDGET`]), so big buffers cannot crowd them out; a larger one
//!   is read up to [`MOST_LARGE`] bytes, from [`LARGE_BUDGET`];
//! - a buffer read before whose bytes hash the same (blake3) is not
//!   searched again: its last result stands and its bytes are counted as
//!   unchanged, not against a budget;
//! - each line says what was searched, what was unchanged, how many
//!   buffers and binds, and whether a budget ran out.

use std::collections::HashMap;

use super::{Wanted, search};

/// Buffers up to this many bytes are read whole.
pub(crate) const SMALL: u64 = 64 << 10;
/// Bytes of a larger buffer read, from its start.
pub(crate) const MOST_LARGE: u64 = 1 << 20;
/// Bytes of small buffers searched anew in one walked frame at most.
const SMALL_BUDGET: u64 = 192 << 20;
/// Bytes of large buffers searched anew in one walked frame at most.
const LARGE_BUDGET: u64 = 64 << 20;
/// Bytes read in one walked frame at most, unchanged ones included.
const MOST_READ: u64 = 1 << 30;
/// Buffers whose hash is kept at most (the map starts over past it).
const MOST_KEPT: usize = 1 << 17;

/// What a search found: the word's place, its value, what it names.
pub(crate) type Found = Option<(usize, u32, String)>;

/// What one walked frame spent.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Spent {
    /// Bytes searched anew: small buffers, large ones.
    pub(crate) searched: [u64; 2],
    /// Bytes read whose hash had not changed (not searched again).
    pub(crate) unchanged: u64,
    pub(crate) buffers: u32,
    /// Binds whose buffers were searched.
    pub(crate) binds: u32,
    /// A budget ran out: some buffers were not read.
    pub(crate) full: bool,
}

/// A buffer: start, bytes read, the bind's table set.
type Key = (u64, u64, u64);

#[derive(Default)]
pub(crate) struct Refs {
    spent: Spent,
    /// This frame's results.
    now: HashMap<Key, Found>,
    /// Earlier frames' hashes and results.
    kept: HashMap<Key, (u64, Found)>,
}

/// The bytes read of a buffer of `len` bytes.
pub(crate) fn taken(len: u64) -> u64 {
    (if len <= SMALL {
        len
    } else {
        len.min(MOST_LARGE)
    }) & !3
}

fn hash(bytes: &[u8]) -> u64 {
    let mut first = [0u8; 8];
    first.copy_from_slice(&blake3::hash(bytes).as_bytes()[..8]);
    u64::from_le_bytes(first)
}

impl Refs {
    /// A new walked frame: what the last one spent.
    pub(crate) fn start(&mut self) -> Spent {
        self.now.clear();
        std::mem::take(&mut self.spent)
    }

    /// A bind's buffers are being searched.
    pub(crate) fn bind(&mut self) {
        self.spent.binds += 1;
    }

    fn read(&self) -> u64 {
        self.spent.searched[0] + self.spent.searched[1] + self.spent.unchanged
    }

    /// This frame's result for `key`, if it has one.
    pub(crate) fn known(&self, key: Key) -> Option<Found> {
        self.now.get(&key).cloned()
    }

    /// Whether `len` more bytes may be read this frame.
    pub(crate) fn may_read(&mut self, len: u64) -> bool {
        let ok = self.read() + len <= MOST_READ;
        self.spent.full |= !ok;
        ok
    }

    /// `bytes` of `key` were read: the kept result if they hash as before,
    /// else `search` of them if the budget allows (`None` and `full` if
    /// not).
    pub(crate) fn take(
        &mut self,
        key: Key,
        bytes: &[u8],
        search: impl FnOnce(&[u8]) -> Found,
    ) -> Found {
        let len = bytes.len() as u64;
        let h = hash(bytes);
        self.spent.buffers += 1;
        if let Some((kept, found)) = self.kept.get(&key)
            && *kept == h
        {
            let found = found.clone();
            self.spent.unchanged += len;
            self.now.insert(key, found.clone());
            return found;
        }
        let large = usize::from(len > SMALL);
        let budget = [SMALL_BUDGET, LARGE_BUDGET][large];
        if self.spent.searched[large] + len > budget {
            self.spent.full = true;
            return None;
        }
        self.spent.searched[large] += len;
        let found = search(bytes);
        if self.kept.len() >= MOST_KEPT {
            self.kept.clear();
        }
        self.kept.insert(key, (h, found.clone()));
        self.now.insert(key, found.clone());
        found
    }
}

/// [`search`] of the words of `bytes`.
pub(crate) fn search_bytes(bytes: &[u8], wanted: &[Wanted]) -> Found {
    let words: Vec<u32> = bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|b| u32::from_le_bytes(*b))
        .collect();
    search(&words, wanted).map(|(i, v, what)| (i, v, what.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(words: &[u32]) -> Vec<u8> {
        words.iter().flat_map(|w| w.to_le_bytes()).collect()
    }

    /// Small buffers are read whole, large ones up to their first MiB.
    #[test]
    fn small_buffers_are_read_whole() {
        assert_eq!(taken(48 << 10), 48 << 10);
        assert_eq!(taken(SMALL), SMALL);
        assert_eq!(taken(8 << 20), MOST_LARGE);
        assert_eq!(taken(7), 4);
    }

    /// A buffer that hashes as before is not searched again and costs no
    /// budget; one that changed is searched.
    #[test]
    fn unchanged_buffers_are_not_searched_again() {
        let mut refs = Refs::default();
        let wanted: Vec<Wanted> = vec![(91_711, "slot".into())];
        let b = bytes(&[1, 91_711]);
        let mut searches = 0;
        let mut look = |refs: &mut Refs, b: &[u8]| {
            refs.take((0x1000, 8, 0), b, |b| {
                searches += 1;
                search_bytes(b, &wanted)
            })
        };
        assert_eq!(look(&mut refs, &b).map(|f| f.0), Some(1));
        let spent = refs.start();
        assert_eq!(
            (spent.searched, spent.unchanged, spent.buffers),
            ([8, 0], 0, 1)
        );
        assert_eq!(look(&mut refs, &b).map(|f| f.0), Some(1), "the kept result");
        let spent = refs.start();
        assert_eq!((spent.searched, spent.unchanged), ([0, 0], 8));
        assert_eq!(
            look(&mut refs, &bytes(&[2, 3])),
            None,
            "changed: searched again"
        );
        assert_eq!(searches, 2);
    }

    /// Large buffers spend their own budget: once it runs out the frame
    /// says so, and small buffers are still searched.
    #[test]
    fn large_buffers_cannot_crowd_out_small_ones() {
        let mut refs = Refs::default();
        let big = vec![0u8; MOST_LARGE as usize];
        for n in 0..(LARGE_BUDGET / MOST_LARGE) {
            assert_eq!(refs.take((n, MOST_LARGE, 0), &big, |_| None), None);
        }
        assert!(!refs.spent.full);
        let mut again = big.clone();
        again[0] = 1;
        refs.take((u64::MAX, MOST_LARGE, 0), &again, |_| None);
        assert!(refs.spent.full, "the large budget ran out");
        let wanted: Vec<Wanted> = vec![(5_000, "slot".into())];
        let small = bytes(&[5_000]);
        assert_eq!(
            refs.take((1, 4, 0), &small, |b| search_bytes(b, &wanted))
                .map(|f| f.1),
            Some(5_000)
        );
        assert_eq!(refs.start().searched[0], 4);
    }
}
