//! `VICTORY_BIND_FAST_HASH=1` (restart; off unless `1`): the hash maps a bind touches use a
//! multiply-rotate hash of their keys instead of SipHash.
//!
//! The title's recording threads spend ~100 ms a frame (summed) binding ~9,300 draws
//! (`binding draws and dispatches per frame`), 72-75 ms of it in descriptor tables. In the
//! sample profile of those threads (`train/dev/profile_base`), SipHash and the maps' hashing
//! (`sip::impl$5::write`, `BuildHasher::hash_one`, `HashMap::insert`) were 54 of the 408 samples
//! under `State::bind`, more than any other function of ours there (`descriptor::view_at` 21,
//! d3d12's view making 31). Every table a draw binds asks `fresh_tables`, `seen_tables` and
//! `ready_tables`; every buffer view it reads asks the list's `refreshed` set; every texture,
//! `readable_once` and the list's sets. The keys are addresses, slot counts and a hash of the
//! table's records: nothing a title chooses to collide, so SipHash's keyed strength buys nothing.
//!
//! Only the hash changes: a map holds, finds and drops the same keys with either (iteration
//! order was already random per process with SipHash's random keys, and nothing here depends on
//! it), so what is bound and recorded is the same. The hash is rustc-hash 2's (add, multiply,
//! and a rotation at the end so the low bits a map indexes by come from the well-mixed high
//! bits of the product: aligned addresses would otherwise share their low bits).

use std::collections::hash_map::{DefaultHasher, RandomState};
use std::hash::{BuildHasher, Hasher};

/// The switch, read once (maps choose their hash when they are made).
pub(crate) fn on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        let on = std::env::var_os("VICTORY_BIND_FAST_HASH").is_some_and(|v| v == "1");
        if on {
            tracing::info!("the maps a bind asks hash their keys with a multiply-rotate hash, not SipHash (VICTORY_BIND_FAST_HASH=1)");
        }
        on
    })
}

/// A map of the bind path, hashed as the switch says.
pub(crate) type Map<K, V> = std::collections::HashMap<K, V, BindState>;
/// A set of the bind path, hashed as the switch says.
pub(crate) type Set<K> = std::collections::HashSet<K, BindState>;

/// SipHash with random keys (the standard maps' hash), or with the switch the fast hash.
#[derive(Clone, Debug)]
pub(crate) struct BindState(Option<RandomState>);

impl BindState {
    /// The fast hash (`fast`) or SipHash, whatever the switch says.
    pub(crate) fn with(fast: bool) -> Self {
        BindState((!fast).then(RandomState::new))
    }
}

impl Default for BindState {
    fn default() -> Self {
        Self::with(on())
    }
}

impl BuildHasher for BindState {
    type Hasher = BindHasher;

    #[inline]
    fn build_hasher(&self) -> BindHasher {
        match &self.0 {
            Some(sip) => BindHasher::Sip(sip.build_hasher()),
            None => BindHasher::Fast(0),
        }
    }
}

pub(crate) enum BindHasher {
    Sip(DefaultHasher),
    Fast(u64),
}

/// rustc-hash 2's multiplier.
const K: u64 = 0xf135_7aea_2e62_a9c5;

#[inline]
fn mix(hash: u64, n: u64) -> u64 {
    hash.wrapping_add(n).wrapping_mul(K)
}

impl Hasher for BindHasher {
    #[inline]
    fn finish(&self) -> u64 {
        match self {
            BindHasher::Sip(h) => h.finish(),
            BindHasher::Fast(h) => h.rotate_left(26),
        }
    }

    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        match self {
            BindHasher::Sip(h) => h.write(bytes),
            BindHasher::Fast(h) => {
                let mut chunks = bytes.chunks_exact(8);
                for c in &mut chunks {
                    let mut w = [0u8; 8];
                    w.copy_from_slice(c);
                    *h = mix(*h, u64::from_le_bytes(w));
                }
                let rest = chunks.remainder();
                if !rest.is_empty() {
                    let mut w = [0u8; 8];
                    w[..rest.len()].copy_from_slice(rest);
                    *h = mix(*h, u64::from_le_bytes(w) ^ ((rest.len() as u64) << 59));
                }
            }
        }
    }

    #[inline]
    fn write_u8(&mut self, n: u8) {
        match self {
            BindHasher::Sip(h) => h.write_u8(n),
            BindHasher::Fast(h) => *h = mix(*h, u64::from(n)),
        }
    }

    #[inline]
    fn write_u16(&mut self, n: u16) {
        match self {
            BindHasher::Sip(h) => h.write_u16(n),
            BindHasher::Fast(h) => *h = mix(*h, u64::from(n)),
        }
    }

    #[inline]
    fn write_u32(&mut self, n: u32) {
        match self {
            BindHasher::Sip(h) => h.write_u32(n),
            BindHasher::Fast(h) => *h = mix(*h, u64::from(n)),
        }
    }

    #[inline]
    fn write_u64(&mut self, n: u64) {
        match self {
            BindHasher::Sip(h) => h.write_u64(n),
            BindHasher::Fast(h) => *h = mix(*h, n),
        }
    }

    #[inline]
    fn write_usize(&mut self, n: usize) {
        match self {
            BindHasher::Sip(h) => h.write_usize(n),
            BindHasher::Fast(h) => *h = mix(*h, n as u64),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A table key as `State::tables` makes it (by content: address 0), a slot count, a hash.
    type TableKey = (u64, u32, u64);

    fn table_keys() -> Vec<TableKey> {
        (0..4000u64)
            .map(|i| {
                let address = if i % 3 == 0 {
                    0
                } else {
                    0xd_0000_0000 + i * 32 * 64
                };
                (
                    address,
                    1 + (i % 37) as u32,
                    i.wrapping_mul(0x9e37_79b9_7f4a_7c15),
                )
            })
            .collect()
    }

    /// What `State::tables` does to its maps over a list: asks, makes, notes ready, drops
    /// the note, and at a cut keeps only the tables without scratch data. The answers and
    /// what is left must be the same with either hash.
    fn run_tables(
        fast: bool,
    ) -> (
        Vec<String>,
        Vec<(TableKey, (u32, Option<u64>))>,
        Vec<TableKey>,
        Vec<TableKey>,
    ) {
        let mut fresh: Map<TableKey, (u32, Option<u64>)> = Map::with_hasher(BindState::with(fast));
        let mut ready: Map<TableKey, (usize, u64, Vec<usize>)> =
            Map::with_hasher(BindState::with(fast));
        let mut seen: Set<TableKey> = Set::with_hasher(BindState::with(fast));
        let mut answers = Vec::new();
        let keys = table_keys();
        for (step, &key) in keys.iter().chain(keys.iter().step_by(7)).enumerate() {
            let epoch = (step / 500) as u64;
            match fresh.get(&key) {
                Some(&(first, e)) if e.is_none_or(|e| e == epoch) => {
                    let new = seen.insert((key.0 ^ 1, key.1, key.2));
                    let r = ready.get(&key).map(|r| r.1);
                    answers.push(format!("found {first} {new} {r:?}"));
                    if step % 5 == 0 {
                        ready.remove(&key);
                    } else {
                        ready
                            .entry(key)
                            .or_insert_with(|| (step, epoch, vec![step]))
                            .1 = epoch;
                    }
                }
                _ => {
                    seen.insert(key);
                    fresh.insert(key, (step as u32, (step % 4 == 0).then_some(epoch)));
                    if step % 3 != 0 {
                        ready.insert(key, (step, epoch, Vec::new()));
                    }
                    answers.push(format!("made {step}"));
                }
            }
            if step % 1500 == 1499 {
                fresh.retain(|_, (_, e)| e.is_none());
            }
        }
        let mut f: Vec<_> = fresh.into_iter().collect();
        f.sort_unstable();
        let mut r: Vec<_> = ready.into_keys().collect();
        r.sort_unstable();
        let mut s: Vec<_> = seen.into_iter().collect();
        s.sort_unstable();
        (answers, f, r, s)
    }

    #[test]
    fn tables_find_and_keep_the_same_with_either_hash() {
        assert_eq!(run_tables(false), run_tables(true));
    }

    /// The list's sets of refreshes, bound pipelines and textures: the same answers to the
    /// same inserts with either hash.
    #[test]
    fn list_sets_answer_the_same_with_either_hash() {
        let run = |fast: bool| {
            let mut refreshed: Set<(usize, usize, usize)> = Set::with_hasher(BindState::with(fast));
            let mut spans: Map<usize, usize> = Map::with_hasher(BindState::with(fast));
            let mut out = Vec::new();
            for i in 0..20_000usize {
                let shadow = 0x2_8000_0000 + (i % 97) * 0x140;
                let address = 0xb_1000_0000 + (i % 613) * 0x1_0000;
                out.push(refreshed.insert((shadow, address, 256 << (i % 5))));
                let n = spans.len();
                out.push(spans.insert(shadow, n).is_none());
                if i % 4096 == 4095 {
                    refreshed.clear();
                }
            }
            let mut s: Vec<_> = spans.into_iter().collect();
            s.sort_unstable();
            (out, s)
        };
        assert_eq!(run(false), run(true));
    }

    #[test]
    fn the_fast_hash_spreads_aligned_addresses() {
        // Page-aligned addresses: the low bits a map indexes by must still differ.
        let state = BindState::with(true);
        let mut buckets = std::collections::HashSet::new();
        for i in 0..4096u64 {
            buckets.insert(state.hash_one(0xd_0000_0000 + i * 4096) & 4095);
        }
        assert!(buckets.len() > 2400, "{} of 4096 buckets", buckets.len());
        // And the top seven bits (the map's tag) take many values.
        let tags: std::collections::HashSet<u64> =
            (0..4096u64).map(|i| state.hash_one(i * 64) >> 57).collect();
        assert!(tags.len() > 100, "{} tags", tags.len());
    }

    #[test]
    fn off_the_hash_is_siphash() {
        assert!(matches!(
            BindState::with(false).build_hasher(),
            BindHasher::Sip(_)
        ));
        assert!(matches!(
            BindState::with(true).build_hasher(),
            BindHasher::Fast(0)
        ));
        // Bytes and words of a key hash alike in length-distinct ways.
        let s = BindState::with(true);
        assert_ne!(
            s.hash_one([0u8; 3].as_slice()),
            s.hash_one([0u8; 4].as_slice())
        );
    }
}
