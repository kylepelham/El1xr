//! Guarded textures read only to look for a write the guards did not see
//! (`VICTORY_TEXTURE_HASH_EVERY=<frames>`, 8 by default): what those reads
//! cost per frame and how many of them found the texture changed. With the
//! guards seeing writes through every view of a page, such a read should
//! almost never find a change; if a run shows none, a longer interval saves
//! the reading and hashing of every guarded texture the frame looks at.
//!
//! Batch 3 (quiet runs 01-08) found 0-3 such changes a run, all in the
//! streaming window, while these reads were 15-60 MB and 3-19 ms a frame
//! (all threads) there and 16 MB and 3.5 ms settled, a third of the texture
//! refreshes' time. `VICTORY_TEXTURE_HASH_SAMPLED=<frames>` ([`full_every`])
//! keeps the 8-frame look but reads only a sample of the texture
//! ([`sample`]); the whole texture is hashed every `<frames>` frames, and
//! those full reads count the changes the sample did not see.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// `VICTORY_TEXTURE_HASH_EVERY=<frames>` (Neptune; 8): how often a guarded
/// texture no write was seen to is read and hashed all the same.
pub(super) fn hash_every() -> u64 {
    static EVERY: victory_core::settings::Live = victory_core::settings::Live::new("VICTORY_TEXTURE_HASH_EVERY", parse_every);
    EVERY.get()
}

fn parse_every(value: Option<&str>) -> u64 {
    value.and_then(|v| v.parse().ok()).unwrap_or(8).max(1)
}

/// `VICTORY_TEXTURE_HASH_SAMPLED=<frames>` (default off): of the looks
/// [`hash_every`] asks for, only one every `<frames>` frames (rounded up
/// to a multiple of the look's interval) hashes the whole texture; the
/// others hash a sample of it ([`sample`]) and read the whole texture only
/// when the sample changed. `None` (off) when unset, not a number, or not
/// above the look's interval.
///
/// Live: a kept sample counts only with the whole read it was taken with
/// ([`kept_sample`]), so one kept under an earlier value is never trusted
/// for a texture read whole since.
pub(super) fn full_every() -> Option<u64> {
    // Parsed again after any live change, `VICTORY_TEXTURE_HASH_EVERY`'s
    // too; 0 off (a full interval is above the look's).
    static FULL: victory_core::settings::Live = victory_core::settings::Live::new("VICTORY_TEXTURE_HASH_SAMPLED", |v| {
        parse_full(v, hash_every()).unwrap_or(0)
    });
    let full = FULL.get();
    (full != 0).then_some(full)
}

fn parse_full(value: Option<&str>, every: u64) -> Option<u64> {
    let full: u64 = value?.trim().parse().ok()?;
    (full > every).then(|| full.div_ceil(every) * every)
}

/// Whether the look at the texture at `start` in `frame` hashes it whole:
/// the same rotation as the looks themselves (by the texture's address),
/// so with `full` a multiple of their interval it is one look in
/// `full / every`.
pub(super) fn full_turn(frame: u64, start: usize, full: u64) -> bool {
    frame
        .wrapping_add((start >> 16) as u64)
        .is_multiple_of(full)
}

/// The first and the last [`EDGE`] bytes are sampled whole: a texture's
/// smallest levels lie at the start of its memory, and the title packs
/// small textures into shared pages.
const EDGE: usize = 4096;
/// Between them, one [`PIECE`] of every [`STEP`] bytes, at an offset that
/// moves from step to step, so a texture's tiles are not all sampled at the
/// same place within them.
const STEP: usize = 4096;
const PIECE: usize = 256;

/// The `(offset, bytes)` pieces of a texture of `size` bytes that
/// [`sample`] reads, in order and apart. Small textures are read whole.
fn pieces(size: usize) -> Vec<(usize, usize)> {
    if size <= 2 * EDGE + STEP {
        return vec![(0, size)];
    }
    let tail = size - EDGE;
    let mut out = vec![(0, EDGE)];
    let mut step = EDGE / STEP;
    while step * STEP < tail {
        let at = step * STEP + (step * 7 % (STEP / PIECE)) * PIECE;
        if at < tail {
            out.push((at, PIECE.min(tail - at)));
        }
        step += 1;
    }
    out.push((tail, EDGE));
    out
}

/// The bytes [`sample`] reads of a texture of `size` bytes.
pub(super) fn sampled_bytes(size: usize) -> usize {
    pieces(size).iter().map(|&(_, n)| n).sum()
}

/// A hash of the sampled pieces of `tiled` ([`pieces`]): only those bytes
/// are read.
pub(super) fn sample(tiled: &[u8]) -> blake3::Hash {
    let mut hasher = blake3::Hasher::new();
    for (at, n) in pieces(tiled.len()) {
        hasher.update(&tiled[at..at + n]);
    }
    hasher.finalize()
}

/// The sample kept with a texture's last full hash: `(full hash, sample)`,
/// taken from the same bytes. A sample stands only while the texture's hash
/// is still the one it was taken with (a read that set the hash some other
/// way, partly mapped or through the kernel, makes the next look read
/// whole).
pub(super) fn kept_sample(
    kept: Option<(blake3::Hash, blake3::Hash)>,
    hash: Option<blake3::Hash>,
) -> Option<blake3::Hash> {
    kept.filter(|&(full, _)| Some(full) == hash)
        .map(|(_, sample)| sample)
}

/// How a refresh read the texture, for [`STATS`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Read {
    /// A write was seen (or the texture is not guarded): not counted here.
    Seen,
    /// Read whole although no write was seen.
    Blind,
    /// Only the sample was read, and it had not changed.
    Sampled,
}

/// Since the last frame-meter line: such reads, their bytes, their time
/// (µs, all threads), and those that found the texture changed; then reads
/// of the sample alone, their bytes and time, and full reads that found a
/// change the sample did not.
static STATS: [AtomicU64; 8] = [const { AtomicU64::new(0) }; 8];

/// A refresh read a guarded texture no write was seen to, whole
/// ([`Read::Blind`]) or only its sample ([`Read::Sampled`]).
pub(super) fn note_read(read: Read, bytes: usize, us: u64) {
    let base = match read {
        Read::Seen => return,
        Read::Blind => 0,
        Read::Sampled => 4,
    };
    STATS[base].fetch_add(1, Relaxed);
    STATS[base + 1].fetch_add(bytes as u64, Relaxed);
    STATS[base + 2].fetch_add(us, Relaxed);
}

/// Such a read found the texture changed.
pub(super) fn note_changed() {
    STATS[3].fetch_add(1, Relaxed);
}

/// A full read found a change no write was seen to while the sample stood
/// as it was: a change a sampled look would have missed.
pub(super) fn note_sample_missed() {
    STATS[7].fetch_add(1, Relaxed);
}

fn line(stats: [u64; 8], frames: f64, every: u64, full: Option<u64>) -> Option<String> {
    let [
        reads,
        bytes,
        us,
        changed,
        sampled,
        sampled_bytes,
        sampled_us,
        missed,
    ] = stats;
    if reads + sampled == 0 || frames <= 0.0 {
        return None;
    }
    let mb = |b: u64| b as f64 / (1024.0 * 1024.0) / frames;
    let mut text = format!(
        "guarded textures read only to look for a write the guards missed, per frame (VICTORY_TEXTURE_HASH_EVERY={every}): \
         {:.1} textures, {:.1} MB, {:.2} ms all threads; {:.2} found changed ({changed} since the last line)",
        reads as f64 / frames,
        mb(bytes),
        us as f64 / 1000.0 / frames,
        changed as f64 / frames,
    );
    if let Some(full) = full {
        text += &format!(
            "; only sampled (VICTORY_TEXTURE_HASH_SAMPLED={full}, whole every {full} frames): {:.1} textures, {:.2} MB, {:.2} ms all threads; \
             {missed} changes the sample missed since the last line",
            sampled as f64 / frames,
            mb(sampled_bytes),
            sampled_us as f64 / 1000.0 / frames,
        );
    }
    Some(text)
}

/// Logs [`STATS`] on the frame-meter cadence and starts them over.
pub(crate) fn log(frames: f64) {
    let stats = std::array::from_fn(|i| STATS[i].swap(0, Relaxed));
    if let Some(line) = line(stats, frames, hash_every(), full_every()) {
        tracing::info!("{line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_switches_are_live() {
        use victory_core::settings::check_live;
        if check_live("VICTORY_TEXTURE_HASH_EVERY", "4", hash_every, 8, 4) {
            check_live("VICTORY_TEXTURE_HASH_SAMPLED", "64", full_every, None, Some(64));
        }
    }

    #[test]
    fn every_defaults_to_eight_and_is_at_least_one() {
        assert_eq!(parse_every(None), 8);
        assert_eq!(parse_every(Some("x")), 8);
        assert_eq!(parse_every(Some("0")), 1);
        assert_eq!(parse_every(Some("64")), 64);
    }

    #[test]
    fn line_is_per_frame_and_silent_without_reads() {
        assert_eq!(line([0; 8], 10.0, 8, None), None);
        let text = line([20, 20 << 20, 5_000, 1, 0, 0, 0, 0], 10.0, 64, None).unwrap();
        assert!(text.contains("HASH_EVERY=64"), "{text}");
        assert!(text.contains("2.0 textures, 2.0 MB, 0.50 ms"), "{text}");
        assert!(
            text.contains("0.10 found changed (1 since the last line)"),
            "{text}"
        );
        assert!(!text.contains("SAMPLED"), "{text}");
        let text = line([0, 0, 0, 0, 30, 3 << 20, 2_000, 2], 10.0, 8, Some(64)).unwrap();
        assert!(text.contains("HASH_SAMPLED=64"), "{text}");
        assert!(text.contains("3.0 textures, 0.30 MB, 0.20 ms"), "{text}");
        assert!(text.contains("2 changes the sample missed"), "{text}");
    }

    #[test]
    fn sampled_is_off_unless_above_the_look_and_rounds_up_to_it() {
        assert_eq!(parse_full(None, 8), None);
        assert_eq!(parse_full(Some("x"), 8), None);
        assert_eq!(parse_full(Some("0"), 8), None);
        assert_eq!(parse_full(Some("8"), 8), None);
        assert_eq!(parse_full(Some("64"), 8), Some(64));
        assert_eq!(parse_full(Some("60"), 8), Some(64));
        assert_eq!(parse_full(Some(" 9 "), 8), Some(16));
    }

    #[test]
    fn a_full_turn_is_one_look_in_full_over_every() {
        let (every, full) = (8u64, 64u64);
        for start in [0usize, 0x1_0000, 0x7_3000_0000, 0xb4e0_0000] {
            let look = |f: u64| f.wrapping_add((start >> 16) as u64).is_multiple_of(every);
            let looks: Vec<u64> = (0..512).filter(|&f| look(f)).collect();
            let fulls: Vec<u64> = (0..512).filter(|&f| full_turn(f, start, full)).collect();
            assert!(fulls.iter().all(|f| looks.contains(f)), "{start:#x}");
            assert_eq!(
                fulls.len() * (full / every) as usize,
                looks.len(),
                "{start:#x}"
            );
        }
    }

    #[test]
    fn pieces_take_both_edges_whole_and_one_piece_of_every_page_between() {
        assert_eq!(pieces(100), vec![(0, 100)]);
        assert_eq!(pieces(3 * 4096), vec![(0, 3 * 4096)]);
        for size in [
            3 * 4096 + 1,
            45_056,
            700_416,
            2_228_224,
            8_294_400,
            8_294_401,
        ] {
            let p = pieces(size);
            assert_eq!(p[0], (0, EDGE), "{size}");
            assert_eq!(*p.last().unwrap(), (size - EDGE, EDGE), "{size}");
            for w in p.windows(2) {
                assert!(w[0].0 + w[0].1 <= w[1].0, "{size}: {w:?}");
            }
            for &(at, n) in &p[1..p.len() - 1] {
                assert!(n > 0 && n <= PIECE && at + n <= size - EDGE, "{size}");
            }
            // Every whole page between the edges has its piece.
            let middle = (size - 2 * EDGE) / STEP;
            assert!(p.len() - 2 >= middle, "{size}: {} pieces", p.len());
            assert_eq!(sampled_bytes(size), p.iter().map(|x| x.1).sum::<usize>());
        }
        // An 8 MB plane reads about a sixteenth of itself.
        let n = sampled_bytes(8_294_400);
        assert!(n < 8_294_400 / 14 && n > 8_294_400 / 18, "{n}");
    }

    #[test]
    fn the_sample_changes_with_any_sampled_byte_and_not_with_others() {
        let size = 64 * 4096;
        let base: Vec<u8> = (0..size).map(|i| (i * 31 % 251) as u8).collect();
        let h = sample(&base);
        for &(at, n) in &pieces(size) {
            for byte in [at, at + n - 1] {
                let mut t = base.clone();
                t[byte] ^= 1;
                assert_ne!(sample(&t), h, "byte {byte}");
            }
        }
        // A whole page written anew always changes its piece.
        let mut t = base.clone();
        t[20 * 4096..21 * 4096].fill(0xcd);
        assert_ne!(sample(&t), h);
        // What it cannot see: a byte outside every piece (left to the full
        // look every `full` frames, which counts it as missed).
        let outside = (EDGE..size - EDGE)
            .find(|&b| pieces(size).iter().all(|&(at, n)| b < at || b >= at + n))
            .unwrap();
        let mut t = base.clone();
        t[outside] ^= 1;
        assert_eq!(sample(&t), h);
    }

    #[test]
    fn a_sample_stands_only_with_the_hash_it_was_taken_with() {
        let (a, b, s) = (blake3::hash(b"a"), blake3::hash(b"b"), blake3::hash(b"s"));
        assert_eq!(kept_sample(Some((a, s)), Some(a)), Some(s));
        assert_eq!(kept_sample(Some((a, s)), Some(b)), None);
        assert_eq!(kept_sample(Some((a, s)), None), None);
        assert_eq!(kept_sample(None, Some(a)), None);
    }
}
