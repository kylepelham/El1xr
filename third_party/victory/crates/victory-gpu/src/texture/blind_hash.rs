//! Guarded textures read only to look for a write the guards did not see
//! (`VICTORY_TEXTURE_HASH_EVERY=<frames>`, 8 by default): what those reads
//! cost per frame and how many of them found the texture changed. With the
//! guards seeing writes through every view of a page, such a read should
//! almost never find a change; if a run shows none, a longer interval saves
//! the reading and hashing of every guarded texture the frame looks at.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// `VICTORY_TEXTURE_HASH_EVERY=<frames>` (Neptune; 8): how often a guarded
/// texture no write was seen to is read and hashed all the same.
pub(super) fn hash_every() -> u64 {
    static EVERY: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *EVERY.get_or_init(|| parse_every(std::env::var("VICTORY_TEXTURE_HASH_EVERY").ok().as_deref()))
}

fn parse_every(value: Option<&str>) -> u64 {
    value.and_then(|v| v.parse().ok()).unwrap_or(8).max(1)
}

/// Since the last frame-meter line: such reads, their bytes, their time
/// (µs, all threads), and those that found the texture changed.
static STATS: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

/// A refresh read a guarded texture no write was seen to.
pub(super) fn note_read(bytes: usize, us: u64) {
    STATS[0].fetch_add(1, Relaxed);
    STATS[1].fetch_add(bytes as u64, Relaxed);
    STATS[2].fetch_add(us, Relaxed);
}

/// Such a read found the texture changed.
pub(super) fn note_changed() {
    STATS[3].fetch_add(1, Relaxed);
}

fn line(stats: [u64; 4], frames: f64, every: u64) -> Option<String> {
    let [reads, bytes, us, changed] = stats;
    if reads == 0 || frames <= 0.0 {
        return None;
    }
    Some(format!(
        "guarded textures read only to look for a write the guards missed, per frame (VICTORY_TEXTURE_HASH_EVERY={every}): \
         {:.1} textures, {:.1} MB, {:.2} ms all threads; {:.2} found changed ({changed} since the last line)",
        reads as f64 / frames,
        bytes as f64 / (1024.0 * 1024.0) / frames,
        us as f64 / 1000.0 / frames,
        changed as f64 / frames,
    ))
}

/// Logs [`STATS`] on the frame-meter cadence and starts them over.
pub(crate) fn log(frames: f64) {
    let stats = [0, 1, 2, 3].map(|i| STATS[i].swap(0, Relaxed));
    if let Some(line) = line(stats, frames, hash_every()) {
        tracing::info!("{line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_defaults_to_eight_and_is_at_least_one() {
        assert_eq!(parse_every(None), 8);
        assert_eq!(parse_every(Some("x")), 8);
        assert_eq!(parse_every(Some("0")), 1);
        assert_eq!(parse_every(Some("64")), 64);
    }

    #[test]
    fn line_is_per_frame_and_silent_without_reads() {
        assert_eq!(line([0, 0, 0, 0], 10.0, 8), None);
        let text = line([20, 20 << 20, 5_000, 1], 10.0, 64).unwrap();
        assert!(text.contains("HASH_EVERY=64"), "{text}");
        assert!(text.contains("2.0 textures, 2.0 MB, 0.50 ms"), "{text}");
        assert!(
            text.contains("0.10 found changed (1 since the last line)"),
            "{text}"
        );
    }
}
