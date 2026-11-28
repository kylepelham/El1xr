//! (El1xr) `VICTORY_FETCH_CHECK=1`: what the relay fetches of copies in the
//! card's memory is checked against the hash the card's compare made of it.
//!
//! Port of anoyarou/VIctory-RDNA4-linux `017fb0b` (`host/torn.rs`, after
//! avatarmanz/VIctory-7900XTX #13, #138, #148). There a fetch over the copy
//! queue (`VICTORY_RELAY_COPY`) ran beside later flights and could bring a
//! piece in the middle of one: a counter cleared and not counted up again.
//! The compare flags a piece only if it ends otherwise than at the compare
//! before, so a piece cleared and filled again with the same was never
//! fetched again, and the CPU's side kept the bytes from in between (skin
//! black or flat with `VICTORY_RELAY=2`, fxceless/victory-fork #89).
//!
//! Our relay has no such fetch: a piece the compare kept comes from its
//! slot, written right behind its flight (`relay_own_queue`), and the
//! others from the live buffers on the drawing queue, between submissions.
//! The check makes that a measurement and a guard on every path. The
//! compare also stores each piece's hash (behind its results, at
//! [`HASH_AT`]); the landing fetches each piece whole, hashes what came
//! down and compares:
//!
//! - a kept piece must be exactly what its compare hashed: not the same is
//!   a torn fetch (the slot was written over, or the card wrote the piece
//!   while it was hashed and copied);
//! - a piece from the live buffers ran behind every flight sent since, so
//!   a later flight may have changed it (its own landing then finds it
//!   merged already): not the same is not proof of a tear.
//!
//! Either way the piece is suspect, and the next flight whose compare
//! covers it fetches it whatever its compare says, until a fetch is found
//! whole, as upstream does. The frame-rate line counts both.
//!
//! What it costs: pieces are fetched whole where a flight declared a part
//! of one, and each piece that came down is hashed (some 4 us for 64 KiB)
//! on the landing thread before it is merged.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use crate::shadow::Shadow;

/// `VICTORY_FETCH_CHECK=1`; off by default.
pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_FETCH_CHECK").is_some_and(|v| v == "1"))
}

/// Where the compare stores the hashes in its results buffer, in bytes:
/// right behind the results (the buffer is twice as long with the check).
pub(crate) const HASH_AT: u64 = 1 << 20;

/// The compare's source (`crate::host::CARD_COMPARE`, with `SNAP` or not)
/// as compiled: with the check it also stores each piece's hash.
pub(crate) fn compare_source(base: &str) -> String {
    compare_source_for(base, on())
}

pub(crate) fn compare_source_for(base: &str, check: bool) -> String {
    if check {
        format!("#define HASH_AT {HASH_AT}u\n{base}")
    } else {
        base.to_string()
    }
}

/// A piece: its copy's number (`Shadow::id`) and the piece.
type Piece = (u64, u32);

/// Since the last frame-rate line: pieces checked, of them kept by the
/// compare, kept pieces not what their compare hashed, pieces from the
/// live buffers not what the last compare hashed, suspects found whole
/// again, pieces fetched only because they were suspect.
static STATS: [AtomicU64; 6] = [const { AtomicU64::new(0) }; 6];

/// The pieces whose last fetch was not what their flight ended with.
static SUSPECTS: Mutex<Option<HashSet<Piece>>> = Mutex::new(None);

/// The suspects now, for one landing.
pub(crate) fn suspects() -> HashSet<Piece> {
    SUSPECTS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .unwrap_or_default()
}

/// A piece fetched only because it was suspect.
pub(crate) fn fetched_as_suspect() {
    STATS[5].fetch_add(1, Ordering::Relaxed);
}

/// `piece` came down; `same`: it is what the compare hashed.
fn checked(set: &mut HashSet<Piece>, piece: Piece, kept: bool, same: bool) {
    STATS[0].fetch_add(1, Ordering::Relaxed);
    if kept {
        STATS[1].fetch_add(1, Ordering::Relaxed);
    }
    if same {
        if set.remove(&piece) {
            STATS[4].fetch_add(1, Ordering::Relaxed);
        }
    } else {
        set.insert(piece);
        STATS[if kept { 2 } else { 3 }].fetch_add(1, Ordering::Relaxed);
    }
}

/// The offsets the compare hashes of piece `piece` of a copy that starts
/// `skew` bytes into its piece 0, in a card buffer of `size` bytes: as
/// `CARD_COMPARE` cuts them, widened to 16 bytes.
pub(crate) fn hashed(piece: u32, skew: u32, size: usize) -> (usize, usize) {
    let lo = (piece as usize * 65536).saturating_sub(skew as usize);
    let hi = ((piece as usize + 1) * 65536 - skew as usize).min(size);
    (lo & !15, (hi + 15) & !15)
}

/// A piece fetched from the live buffers: copy, offsets, where it comes down.
pub(crate) type Live<T = Shadow> = (Arc<T>, usize, usize, u64);
/// A piece fetched from the compare's kept copy: as [`Live`], then where it
/// is kept and its plan's send number.
pub(crate) type Kept<T = Shadow> = (Arc<T>, usize, usize, u64, u64, u64);

/// What [`widen`] asks of a copy.
pub(crate) trait Whole {
    /// `Shadow::card_hashed`.
    fn hashed_at(&self, at: usize) -> Option<(u32, usize, usize)>;
    /// `Shadow::id`.
    fn number(&self) -> u64;
}

impl Whole for Shadow {
    fn hashed_at(&self, at: usize) -> Option<(u32, usize, usize)> {
        self.card_hashed(at)
    }
    fn number(&self) -> u64 {
        self.id()
    }
}

/// A piece to check once it is down.
pub(crate) struct Check {
    piece: Piece,
    kept: bool,
    at: u64,
    len: usize,
    expect: u32,
}

/// What a landing fetches with the check: each piece whole, once.
pub(crate) struct Wide<T = Shadow> {
    pub(crate) live: Vec<Live<T>>,
    pub(crate) live_total: u64,
    pub(crate) kept: Vec<Kept<T>>,
    pub(crate) kept_total: u64,
    pub(crate) checks: Vec<Check>,
}

/// The pieces `live` and `kept` (what is merged) fetched whole: the fetches
/// are returned, and each merge's offset where its bytes come down is moved
/// to its place in them. `expect(copy, piece, kept)`: the hash the piece's
/// compare made, if it compared it.
pub(crate) fn widen<T: Whole>(
    live: &mut [Live<T>],
    kept: &mut [Kept<T>],
    expect: impl Fn(usize, u32, bool) -> Option<u32>,
) -> Wide<T> {
    let mut wide = Wide {
        live: Vec::new(),
        live_total: 0,
        kept: Vec::new(),
        kept_total: 0,
        checks: Vec::new(),
    };
    let mut index: HashMap<(usize, u32), u64> = HashMap::new();
    for p in live.iter_mut() {
        let copy = Arc::as_ptr(&p.0) as usize;
        let Some((piece, lo, hi)) =
            p.0.hashed_at(p.1)
                .filter(|&(_, lo, hi)| lo <= p.1 && p.2 <= hi)
        else {
            // Not a whole piece to be had: fetched as it is, unchecked.
            wide.live.push((p.0.clone(), p.1, p.2, wide.live_total));
            p.3 = wide.live_total;
            wide.live_total += (p.2 - p.1) as u64;
            continue;
        };
        let at = *index.entry((copy, piece)).or_insert_with(|| {
            let at = wide.live_total;
            wide.live.push((p.0.clone(), lo, hi, at));
            wide.live_total += (hi - lo) as u64;
            if let Some(hash) = expect(copy, piece, false) {
                wide.checks.push(Check {
                    piece: (p.0.number(), piece),
                    kept: false,
                    at,
                    len: hi - lo,
                    expect: hash,
                });
            }
            at
        });
        p.3 = at + (p.1 - lo) as u64;
    }
    index.clear();
    for p in kept.iter_mut() {
        let copy = Arc::as_ptr(&p.0) as usize;
        // The slot holds the piece as hashed: from its first byte on.
        let Some((piece, lo, hi)) =
            p.0.hashed_at(p.1)
                .filter(|&(_, lo, hi)| lo <= p.1 && p.2 <= hi && p.4 >= (p.1 - lo) as u64)
        else {
            wide.kept
                .push((p.0.clone(), p.1, p.2, wide.kept_total, p.4, p.5));
            p.3 = wide.kept_total;
            wide.kept_total += (p.2 - p.1) as u64;
            continue;
        };
        let slot = p.4 - (p.1 - lo) as u64;
        let at = *index.entry((copy, piece)).or_insert_with(|| {
            let at = wide.kept_total;
            wide.kept.push((p.0.clone(), lo, hi, at, slot, p.5));
            wide.kept_total += (hi - lo) as u64;
            if let Some(hash) = expect(copy, piece, true) {
                wide.checks.push(Check {
                    piece: (p.0.number(), piece),
                    kept: true,
                    at,
                    len: hi - lo,
                    expect: hash,
                });
            }
            at
        });
        p.3 = at + (p.1 - lo) as u64;
    }
    wide
}

/// Once the fetches are down (`live` and `kept`: where their bytes are
/// mapped), each checked piece is hashed and the suspects brought up to
/// date.
pub(crate) fn verify(checks: &[Check], live: Option<usize>, kept: Option<usize>) {
    if checks.is_empty() {
        return;
    }
    let mut guard = SUSPECTS.lock().unwrap_or_else(|e| e.into_inner());
    let set = guard.get_or_insert_with(HashSet::new);
    for c in checks {
        let Some(data) = (if c.kept { kept } else { live }) else {
            continue;
        };
        // SAFETY: the mapping of a fetch whose copy has run; `at` and `len`
        // lie in what it fetched (`widen`).
        let bytes =
            unsafe { std::slice::from_raw_parts((data + c.at as usize) as *const u8, c.len) };
        checked(set, c.piece, c.kept, hash(bytes) == c.expect);
    }
}

/// The frame-rate line's words, if the check ran.
pub(crate) fn log(frames: f64) {
    let s: Vec<f64> = STATS
        .iter()
        .map(|s| s.swap(0, Ordering::Relaxed) as f64 / frames)
        .collect();
    if s[0] > 0.0 {
        tracing::info!(
            "fetched pieces checked per frame (VICTORY_FETCH_CHECK=1): {:.1}, {:.1} of them as the compare kept them; \
             torn kept {:.2} (not what their compare hashed), live changed {:.2} (not what the last compare hashed: \
             a later flight's bytes, or torn); suspects found whole {:.2}; fetched only as suspect {:.2}",
            s[0],
            s[1],
            s[2],
            s[3],
            s[4],
            s[5],
        );
    }
}

/// The hash `CARD_COMPARE` makes of a piece that holds `bytes` (a multiple
/// of 16 of them), folded to 32 bits as it stores it at [`HASH_AT`].
pub(crate) fn hash(bytes: &[u8]) -> u32 {
    #[cfg(target_arch = "x86_64")]
    if std::is_x86_feature_detected!("avx2") {
        // SAFETY: the feature is there.
        return unsafe { hash_avx2(bytes) };
    }
    hash_plain(bytes)
}

const STEP: [u32; 4] = [0x0100_0193, 0x9E37_79B1, 0x85EB_CA77, 0xC2B2_AE3D];
const SEED: [u32; 4] = [0x9E37_79B9, 0x85EB_CA6B, 0xC2B2_AE35, 0x27D4_EB2F];

/// `h = (h ^ v) * STEP; h = h.yzwx ^ (h >> 15)`.
fn step(h: &mut [u32; 4], with: [u32; 4]) {
    let x: [u32; 4] = std::array::from_fn(|i| (h[i] ^ with[i]).wrapping_mul(STEP[i]));
    *h = std::array::from_fn(|i| x[(i + 1) % 4] ^ (x[i] >> 15));
}

/// The first thread's join of the 64 threads' hashes, folded.
fn finish(parts: &[[u32; 4]; 64]) -> u32 {
    let mut all = SEED;
    for part in parts {
        step(&mut all, *part);
    }
    all[0] ^ all[1].wrapping_mul(3) ^ all[2].wrapping_mul(5) ^ all[3].wrapping_mul(7)
}

fn seeds() -> [[u32; 4]; 64] {
    std::array::from_fn(|thread| SEED.map(|s| s.wrapping_add(thread as u32)))
}

/// A row of up to 1,024 bytes: thread `t` takes its 16 at `16 t`.
fn row(parts: &mut [[u32; 4]; 64], bytes: &[u8]) {
    for (thread, chunk) in bytes.chunks_exact(16).enumerate() {
        let with = std::array::from_fn(|i| {
            u32::from_le_bytes([
                chunk[i * 4],
                chunk[i * 4 + 1],
                chunk[i * 4 + 2],
                chunk[i * 4 + 3],
            ])
        });
        step(&mut parts[thread], with);
    }
}

fn hash_plain(bytes: &[u8]) -> u32 {
    let mut parts = seeds();
    for r in bytes.chunks(1024) {
        row(&mut parts, r);
    }
    finish(&parts)
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
fn hash_avx2(bytes: &[u8]) -> u32 {
    use std::arch::x86_64::*;
    let mut parts = seeds();
    let rows = bytes.chunks_exact(1024);
    let rest = rows.remainder();
    let mul = _mm256_setr_epi32(
        STEP[0] as i32,
        STEP[1] as i32,
        STEP[2] as i32,
        STEP[3] as i32,
        STEP[0] as i32,
        STEP[1] as i32,
        STEP[2] as i32,
        STEP[3] as i32,
    );
    let state = parts.as_mut_ptr().cast::<__m256i>();
    for r in rows {
        let from = r.as_ptr().cast::<__m256i>();
        for k in 0..32 {
            // SAFETY: two threads' hashes of 16 bytes each, and their 32 of
            // the row's 1,024 bytes.
            unsafe {
                let h = _mm256_xor_si256(
                    _mm256_loadu_si256(state.add(k)),
                    _mm256_loadu_si256(from.add(k)),
                );
                let x = _mm256_mullo_epi32(h, mul);
                // y, z, w, x of each.
                let turned = _mm256_shuffle_epi32::<0b00_11_10_01>(x);
                _mm256_storeu_si256(
                    state.add(k),
                    _mm256_xor_si256(turned, _mm256_srli_epi32::<15>(x)),
                );
            }
        }
    }
    row(&mut parts, rest);
    finish(&parts)
}

#[cfg(test)]
#[path = "fetch_check_tests.rs"]
mod tests;
