//! Coalescing adjacent `CopyBufferRegion` calls on relay readback command lists
//! (`VICTORY_RELAY_COPY_RUNS=0` disables coalescing). Keeps piece boundaries
//! intact for CPU merges and alias bookkeeping.

use std::sync::{Arc, OnceLock};

pub(crate) fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_RELAY_COPY_RUNS").is_none_or(|v| v != "0"))
}

pub(crate) type RelayPiece<T> = (Arc<T>, usize, usize, u64);

/// Joins adjacent `(shadow, lo, hi, dst)` readback pieces that share the same
/// source buffer and contiguous source and destination offsets.
pub(crate) fn relay_copy_runs<T>(
    pieces: &[RelayPiece<T>],
    join: bool,
) -> impl Iterator<Item = (&T, usize, usize, u64)> {
    pieces
        .chunk_by(move |left, right| {
            join && Arc::ptr_eq(&left.0, &right.0)
                && left.2 == right.1
                && left.3.checked_add((left.2 - left.1) as u64) == Some(right.3)
        })
        .map(|run| {
            let (shadow, start, _, dest) = &run[0];
            (shadow.as_ref(), *start, run[run.len() - 1].2, *dest)
        })
}

/// Joins adjacent snapshot readback entries `(shadow, lo, hi, dst, src_from, slot)`
/// that copy contiguous ranges out of the shared snapshot buffer into contiguous
/// destination offsets.
pub(crate) fn relay_kept_runs<T, U>(
    kept: &[(Arc<T>, usize, usize, u64, u64, U)],
    join: bool,
) -> impl Iterator<Item = (u64, u64, u64)> + '_ {
    kept.chunk_by(move |left, right| {
        let len = (left.2 - left.1) as u64;
        join && left.3.checked_add(len) == Some(right.3) && left.4.checked_add(len) == Some(right.4)
    })
    .map(|run| {
        let dst = run[0].3;
        let src = run[0].4;
        let len: u64 = run.iter().map(|r| (r.2 - r.1) as u64).sum();
        (dst, src, len)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joined_readback_preserves_bytes_order_holes_and_distinct_resources() {
        let first = Arc::new([1u8, 2, 3, 4, 5, 6, 7, 8]);
        let second = Arc::new([11u8, 12, 13, 14, 15, 16, 17, 18]);
        let pieces = [
            (first.clone(), 0, 2, 0),
            (first.clone(), 2, 4, 2),
            (second.clone(), 4, 6, 4),
            (second.clone(), 6, 7, 8),
            (second.clone(), 0, 2, 9),
            (first.clone(), 3, 5, 0),
        ];
        for (join, calls) in [(false, 6), (true, 5)] {
            let mut bytes = [0xff; 11];
            let mut count = 0;
            for (source, lo, hi, dest) in relay_copy_runs(&pieces, join) {
                let at = dest as usize;
                bytes[at..at + hi - lo].copy_from_slice(&source[lo..hi]);
                count += 1;
            }
            assert_eq!(count, calls);
            assert_eq!(bytes, [4, 5, 3, 4, 15, 16, 0xff, 0xff, 17, 11, 12]);
        }
        let overflow = [(first.clone(), 0, 1, u64::MAX), (first, 1, 2, 0)];
        assert_eq!(relay_copy_runs(&overflow, true).count(), 2);
    }

    #[test]
    fn joined_kept_readback_coalesces_adjacent_snapshot_slots() {
        let s = Arc::new(());
        let kept = [
            (s.clone(), 0, 4, 0, 100, 0),
            (s.clone(), 4, 8, 4, 104, 1),
            (s.clone(), 0, 4, 8, 200, 2),
            (s, 0, 4, 16, 204, 3),
        ];
        let joined: Vec<_> = relay_kept_runs(&kept, true).collect();
        assert_eq!(joined, [(0, 100, 8), (8, 200, 4), (16, 204, 4)]);
        let raw: Vec<_> = relay_kept_runs(&kept, false).collect();
        assert_eq!(raw.len(), 4);
    }
}
