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
//!
//! (round 9) Part N: the large budget ran out on every walked frame of the
//! Bink window (67 MB of 64 MiB, `refs_unchanged` 0): a buffer was read
//! once per table set binding it (the numbers wanted from a table depend
//! on the table), so one big buffer bound by hundreds of draws took the
//! budget hundreds of times. Now a buffer is read once a frame whatever
//! binds it: its words that may name a found slot ([`Filter`]: from
//! [`super::SMALLEST`] up to the largest slot's byte offset, or a found
//! slot's handle) are kept sorted ([`Candidates`]) and each bind looks its
//! own numbers up in them.

use std::collections::HashMap;
use std::sync::Arc;

use super::Wanted;

/// Buffers up to this many bytes are read whole.
pub(crate) const SMALL: u64 = 64 << 10;
/// Bytes of a larger buffer read, from its start.
pub(crate) const MOST_LARGE: u64 = 8 << 20;
/// Bytes of small buffers searched anew in one walked frame at most.
const SMALL_BUDGET: u64 = 192 << 20;
/// Bytes of large buffers searched anew in one walked frame at most.
const LARGE_BUDGET: u64 = 256 << 20;
/// Bytes read in one walked frame at most, unchanged ones included.
const MOST_READ: u64 = 1 << 30;
/// Candidate words kept over all buffers at most (the map starts over
/// past it).
const MOST_KEPT: usize = 1 << 22;

/// What a search found: the word's place, its value, what it names.
pub(crate) type Found = Option<(usize, u32, String)>;

/// The words of a buffer that may name a found slot: (value, place of its
/// first word), sorted by value, one per value.
pub(crate) type Candidates = Arc<Vec<(u32, u32)>>;

/// Which words may name a found slot: from [`super::SMALLEST`] up to
/// `bound` (an index or byte offset, from the heap's or a table's first
/// slot, is never past the largest slot's byte offset), or one of `extra`
/// (the found slots' handles, low half).
/// (round 10, `low_bits`) A word whose bits under [`INDEX_BITS`] name a
/// found slot is kept under those bits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Filter {
    pub(crate) bound: u32,
    pub(crate) extra: Vec<u32>,
    pub(crate) low_bits: bool,
}

/// The bits of a word a translated shader keeps of a view index: it
/// multiplies the index by 32 in 32-bit arithmetic (`index << 5`, then the
/// heap's byte address over 32), so bits 27-31 never reach the slot and
/// the title may keep flags there.
pub(crate) const INDEX_BITS: u32 = 0x07ff_ffff;

impl Filter {
    fn plain(&self, word: u32) -> bool {
        (super::SMALLEST..=self.bound).contains(&word) || self.extra.contains(&word)
    }

    /// The number `word` is kept as, if it may name a found slot.
    fn key(&self, word: u32) -> Option<u32> {
        if self.plain(word) {
            return Some(word);
        }
        let low = word & INDEX_BITS;
        (self.low_bits && low != word && self.plain(low)).then_some(low)
    }

    /// A number for the buffer keys: buffers searched with another filter
    /// are searched again.
    pub(crate) fn id(&self) -> u64 {
        self.extra.iter().fold(u64::from(self.bound), |k, &e| {
            k.rotate_left(13) ^ u64::from(e)
        }) ^ u64::from(self.low_bits) << 63
    }
}

/// What one walked frame spent.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Spent {
    /// The walked frame these are of (`None`: no frame walked yet).
    pub(crate) frame: Option<u64>,
    /// Bytes searched anew: small buffers, large ones.
    pub(crate) searched: [u64; 2],
    /// Bytes read whose hash had not changed (not searched again).
    pub(crate) unchanged: u64,
    /// Buffers read (each once a frame).
    pub(crate) buffers: u32,
    /// Binds whose buffers were searched.
    pub(crate) binds: u32,
    /// Binds of the Bink window not searched: rt0 the target, tiny, or
    /// none.
    pub(crate) passed_over: u32,
    /// Draws and dispatches not run whose roots were searched.
    pub(crate) dropped: u32,
    /// A budget ran out: some buffers were not read.
    pub(crate) full: bool,
}

/// A buffer: start, bytes read, [`Filter::id`].
type Key = (u64, u64, u64);

#[derive(Default)]
pub(crate) struct Refs {
    spent: Spent,
    /// This frame's buffers (`None`: not read, a budget ran out).
    now: HashMap<Key, Option<Candidates>>,
    /// Earlier frames' hashes and candidates.
    kept: HashMap<Key, (u64, Candidates)>,
    /// Candidate words in `kept`.
    kept_words: usize,
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

/// The words of `bytes` `filter` takes ([`Candidates`]).
pub(crate) fn candidates(bytes: &[u8], filter: &Filter) -> Vec<(u32, u32)> {
    let mut out: Vec<(u32, u32)> = bytes
        .as_chunks::<4>()
        .0
        .iter()
        .enumerate()
        .filter_map(|(i, b)| filter.key(u32::from_le_bytes(*b)).map(|k| (k, i as u32)))
        .collect();
    out.sort_unstable();
    out.dedup_by_key(|c| c.0);
    out
}

/// The first word of `candidates` (by place) that is a `wanted` number.
pub(crate) fn lookup(candidates: &[(u32, u32)], wanted: &[Wanted]) -> Found {
    wanted
        .iter()
        .filter_map(|(v, what)| {
            candidates
                .binary_search_by_key(v, |c| c.0)
                .ok()
                .map(|at| (candidates[at].1 as usize, *v, what.clone()))
        })
        .min_by_key(|f| f.0)
}

impl Refs {
    /// A new walked frame (`frame`): what the last one spent.
    pub(crate) fn start(&mut self, frame: u64) -> Spent {
        self.now.clear();
        let spent = std::mem::take(&mut self.spent);
        self.spent.frame = Some(frame);
        spent
    }

    /// A bind's buffers are being searched.
    pub(crate) fn bind(&mut self) {
        self.spent.binds += 1;
    }

    /// A bind of the Bink window was not searched.
    pub(crate) fn passed_over(&mut self) {
        self.spent.passed_over += 1;
    }

    /// A draw or dispatch not run had its roots searched.
    pub(crate) fn dropped(&mut self) {
        self.spent.dropped += 1;
    }

    fn read(&self) -> u64 {
        self.spent.searched[0] + self.spent.searched[1] + self.spent.unchanged
    }

    /// This frame's candidates for `key`, if it was read or refused.
    pub(crate) fn known(&self, key: Key) -> Option<Option<Candidates>> {
        self.now.get(&key).cloned()
    }

    /// Whether `len` more bytes may be read this frame.
    pub(crate) fn may_read(&mut self, len: u64) -> bool {
        let ok = self.read() + len <= MOST_READ;
        self.spent.full |= !ok;
        ok
    }

    /// `bytes` of `key` were read: the kept candidates if they hash as
    /// before, else those `filter` takes if the budget allows (`None` and
    /// `full` if not). Either is this frame's answer for `key`.
    pub(crate) fn take(&mut self, key: Key, bytes: &[u8], filter: &Filter) -> Option<Candidates> {
        let len = bytes.len() as u64;
        let h = hash(bytes);
        self.spent.buffers += 1;
        if let Some((kept, c)) = self.kept.get(&key)
            && *kept == h
        {
            let c = Arc::clone(c);
            self.spent.unchanged += len;
            self.now.insert(key, Some(Arc::clone(&c)));
            return Some(c);
        }
        let large = usize::from(len > SMALL);
        let budget = [SMALL_BUDGET, LARGE_BUDGET][large];
        if self.spent.searched[large] + len > budget {
            self.spent.full = true;
            self.now.insert(key, None);
            return None;
        }
        self.spent.searched[large] += len;
        let c: Candidates = Arc::new(candidates(bytes, filter));
        if self.kept_words + c.len() > MOST_KEPT {
            self.kept.clear();
            self.kept_words = 0;
        }
        self.kept_words += c.len();
        if let Some((_, old)) = self.kept.insert(key, (h, Arc::clone(&c))) {
            self.kept_words -= old.len();
        }
        self.now.insert(key, Some(Arc::clone(&c)));
        Some(c)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(words: &[u32]) -> Vec<u8> {
        words.iter().flat_map(|w| w.to_le_bytes()).collect()
    }

    /// Part N's found slot 109463 of heap 0xb690000.
    fn filter() -> Filter {
        Filter {
            bound: 109_463 * 32,
            extra: vec![0x0b69_0000 + 109_463 * 32],
            low_bits: false,
        }
    }

    /// Small buffers are read whole, large ones up to their first bytes.
    #[test]
    fn small_buffers_are_read_whole() {
        assert_eq!(taken(48 << 10), 48 << 10);
        assert_eq!(taken(SMALL), SMALL);
        assert_eq!(taken(64 << 20), MOST_LARGE);
        assert_eq!(taken(7), 4);
    }

    /// Floats, small numbers and numbers past every slot are not kept; an
    /// index, a byte offset and a handle are, at their first place.
    #[test]
    fn candidates_keep_what_may_name_a_slot() {
        let f = filter();
        let words = [
            1.0f32.to_bits(),
            7,
            109_463,
            0x0b69_0000 + 109_463 * 32,
            109_463,
            u32::MAX,
            109_463 * 32 + 1,
        ];
        let c = candidates(&bytes(&words), &f);
        assert_eq!(c, vec![(109_463, 2), (0x0b69_0000 + 109_463 * 32, 3)]);
        let wanted: Vec<Wanted> = vec![
            (0x0b69_0000 + 109_463 * 32, "handle".into()),
            (109_463, "index".into()),
        ];
        assert_eq!(
            lookup(&c, &wanted),
            Some((2, 109_463, "index".into())),
            "the first by place"
        );
        assert_eq!(lookup(&c, &[(5_000, "x".into())]), None);
    }

    /// A buffer bound by many table sets is read once a frame: every bind
    /// looks its own numbers up, and the budget is spent once.
    #[test]
    fn a_buffer_is_read_once_whatever_binds_it() {
        let mut refs = Refs::default();
        let f = filter();
        let big = {
            let mut w = vec![0x3f80_0000u32; (2 << 20) / 4];
            w[300_000] = 463 * 32;
            bytes(&w)
        };
        let key = (0x7_0000_0000, big.len() as u64, f.id());
        let c = refs.take(key, &big, &f).expect("in the budget");
        assert_eq!(refs.known(key).flatten().map(|c| c.len()), Some(1));
        for table in [109_000u32, 108_000, 109_400] {
            let rel = (109_463 - table) * 32;
            let wanted: Vec<Wanted> = vec![(rel, format!("from {table}"))];
            let hit = lookup(&c, &wanted).map(|h| h.0);
            assert_eq!(hit, (table == 109_000).then_some(300_000));
        }
        let spent = refs.start(1);
        assert_eq!((spent.searched, spent.buffers), ([0, 2 << 20], 1));
        assert_eq!(refs.start(2).frame, Some(1), "the frame they are of");
    }

    /// A buffer that hashes as before is not searched again and costs no
    /// budget; one that changed is searched.
    #[test]
    fn unchanged_buffers_are_not_searched_again() {
        let mut refs = Refs::default();
        let f = filter();
        let b = bytes(&[1, 109_463]);
        let key = (0x1000, 8, f.id());
        assert_eq!(refs.take(key, &b, &f).map(|c| c.len()), Some(1));
        let spent = refs.start(1);
        assert_eq!(
            (spent.searched, spent.unchanged, spent.buffers),
            ([8, 0], 0, 1)
        );
        assert_eq!(
            refs.take(key, &b, &f).map(|c| c.len()),
            Some(1),
            "the kept words"
        );
        let spent = refs.start(2);
        assert_eq!((spent.searched, spent.unchanged), ([0, 0], 8));
        assert_eq!(
            refs.take(key, &bytes(&[2, 3]), &f).map(|c| c.len()),
            Some(0)
        );
        assert_eq!(refs.start(3).searched, [8, 0], "changed: searched again");
    }

    /// (round 10) With `low_bits`, a word with flags in bits 27-31 over a
    /// found slot's index is kept as that index; without, it is not. A
    /// plain index still wins its place.
    #[test]
    fn flagged_indices_are_kept_under_their_low_bits() {
        let plain = Filter {
            bound: 94_434 * 32,
            extra: Vec::new(),
            low_bits: false,
        };
        let low = Filter {
            low_bits: true,
            ..plain.clone()
        };
        let words = [1.0f32.to_bits(), 0x8000_0000 | 94_434, 0xf800_0007];
        assert!(candidates(&bytes(&words), &plain).is_empty());
        assert_eq!(candidates(&bytes(&words), &low), vec![(94_434, 1)]);
        assert_ne!(plain.id(), low.id(), "searched again when the switch flips");
        let wanted: Vec<Wanted> = vec![(94_434, "slot 94434 as an index".into())];
        let c = candidates(&bytes(&words), &low);
        assert_eq!(lookup(&c, &wanted).map(|f| f.0), Some(1));
    }

    /// Large buffers spend their own budget: once it runs out the frame
    /// says so (and does not read that buffer again), and small buffers
    /// are still searched.
    #[test]
    fn large_buffers_cannot_crowd_out_small_ones() {
        let mut refs = Refs::default();
        let f = filter();
        let big = vec![0u8; MOST_LARGE as usize];
        for n in 0..(LARGE_BUDGET / MOST_LARGE) {
            assert!(refs.take((n, MOST_LARGE, 0), &big, &f).is_some());
        }
        assert!(!refs.spent.full);
        let key = (u64::MAX, MOST_LARGE, 0);
        assert!(refs.take(key, &big, &f).is_none());
        assert!(refs.spent.full, "the large budget ran out");
        assert_eq!(refs.known(key), Some(None), "refused for the frame");
        let small = bytes(&[109_463]);
        assert_eq!(
            refs.take((1, 4, 0), &small, &f).map(|c| c[0].0),
            Some(109_463)
        );
        assert_eq!(refs.start(1).searched[0], 4);
    }
}
