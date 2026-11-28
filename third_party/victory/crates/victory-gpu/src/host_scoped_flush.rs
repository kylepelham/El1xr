//! (feedcard, #88) `VICTORY_SCOPED_FLUSH=1` (Live; off by default): a copy
//! the CPU does into title memory at submission (`Step::Copy`, a card copy
//! done on the CPU too) flushes before it only the host copies whose pending
//! refreshes or shader-write ranges overlap the bytes it writes or reads; the
//! others keep their steps for a later flush. Off, every such copy flushes
//! the whole batch (`Batch::title_written`).
//!
//! A refresh reads title memory into a host copy and writes no title memory;
//! a shader-write range (`Shadow::writing`) may touch title memory in its own
//! range only. So a refresh or range clear of both ends of the copy reads the
//! same bytes after the copy as before it. A kept copy whose last step is a
//! refresh gets a barrier, so a refresh recorded after the title write does
//! not merge with it across bytes the write changed. Upload fills and the
//! texture checks still run whole before the copy, as before.
//!
//! After jzvgrow-dot/Victory-4080Super `7925597acf` (overlap-scoped flushes).
//! Line: title copies scoped per frame.

use super::{Batch, CopyOps, ShadowBatch, ShadowStep};
use std::sync::atomic::{AtomicU64, Ordering};

static ON: victory_core::settings::Live =
    victory_core::settings::Live::flag("VICTORY_SCOPED_FLUSH");

/// Title copies scoped, of them those that flushed no host copy, host copies
/// flushed and kept, since the last line.
static COUNTS: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

fn overlaps(from: usize, to: usize, (a, e): (usize, usize)) -> bool {
    from < e && a < to
}

/// Whether one of `copy`'s steps touches one of `ranges`.
fn touches<C>(copy: &ShadowBatch<C>, ranges: &[(usize, usize)]) -> bool {
    copy.steps.iter().any(|&step| {
        let (from, to) = match step {
            ShadowStep::Refresh(from, to) => (from, to),
            ShadowStep::Write(address, len) => (address, address + len),
            ShadowStep::Barrier => return false,
        };
        ranges.iter().any(|&r| overlaps(from, to, r))
    })
}

/// `copies` split into those to flush before a title write over `ranges` and
/// those to keep, each in its order.
fn split<C>(
    copies: Vec<ShadowBatch<C>>,
    ranges: &[(usize, usize)],
) -> (Vec<ShadowBatch<C>>, Vec<ShadowBatch<C>>) {
    copies.into_iter().partition(|copy| touches(copy, ranges))
}

impl<C: Clone> CopyOps<C> {
    /// The copies `split` kept go back, indexed by `key`, behind what was
    /// recorded since (nothing: a flush has just taken it all).
    fn put_back(&mut self, kept: Vec<ShadowBatch<C>>, key: impl Fn(&C) -> usize) {
        for mut copy in kept {
            if matches!(copy.steps.last(), Some(ShadowStep::Refresh(..))) {
                copy.steps.push(ShadowStep::Barrier);
            }
            self.index.insert(key(&copy.shadow), self.copies.len());
            self.copies.push(copy);
        }
        self.last_ahead = (0, usize::MAX);
    }
}

impl Batch {
    /// Before a CPU copy of `len` bytes from `src` to `dst` in title memory:
    /// [`Batch::title_written`], or with the switch only the host copies the
    /// copy overlaps flushed.
    pub(super) fn title_copy(&mut self, dst: usize, src: usize, len: usize) {
        if !ON.get_bool() {
            self.title_written();
            return;
        }
        let ranges = [
            (dst, dst.saturating_add(len)),
            (src, src.saturating_add(len)),
        ];
        let (now, kept) = split(self.ops.take(), &ranges);
        COUNTS[0].fetch_add(1, Ordering::Relaxed);
        COUNTS[2].fetch_add(now.len() as u64, Ordering::Relaxed);
        COUNTS[3].fetch_add(kept.len() as u64, Ordering::Relaxed);
        self.flush_fills();
        self.flush_textures();
        if now.is_empty() {
            COUNTS[1].fetch_add(1, Ordering::Relaxed);
        } else {
            self.ops.copies = now;
            self.flush();
        }
        crate::shadow::refresh_stamp::expire();
        self.textures.clear();
        self.ops
            .put_back(kept, |shadow| std::sync::Arc::as_ptr(shadow) as usize);
    }
}

/// With the frame line, when the switch is on.
pub(super) fn line(frames: f64) {
    let c: Vec<u64> = COUNTS
        .iter()
        .map(|a| a.swap(0, Ordering::Relaxed))
        .collect();
    if !ON.get_bool() || frames <= 0.0 {
        return;
    }
    tracing::info!(
        "title copies scoped per frame (VICTORY_SCOPED_FLUSH=1): {:.1} copies, {:.1} of them flushing no host copy; host copies flushed {:.1}, kept for a later flush {:.1}",
        c[0] as f64 / frames,
        c[1] as f64 / frames,
        c[2] as f64 / frames,
        c[3] as f64 / frames,
    );
}

#[cfg(test)]
mod tests {
    use super::{CopyOps, ShadowStep, split};

    /// Copies keyed by a number (as `Arc::as_ptr` keys them in the batch).
    fn ops(copies: &[(usize, &[ShadowStep])]) -> CopyOps<usize> {
        let mut ops = CopyOps::<usize>::default();
        for &(key, steps) in copies {
            for &step in steps {
                match step {
                    ShadowStep::Refresh(from, to) => ops.refresh(key, &key, from, to),
                    ShadowStep::Write(address, len) => ops.gpu_write(key, &key, address, len),
                    ShadowStep::Barrier => {}
                }
            }
        }
        ops
    }

    fn keys(copies: &[super::ShadowBatch<usize>]) -> Vec<usize> {
        copies.iter().map(|c| c.shadow).collect()
    }

    #[test]
    fn only_overlapping_copies_flush() {
        let mut ops = ops(&[
            (1, &[ShadowStep::Refresh(100, 150)]),
            (2, &[ShadowStep::Refresh(1000, 1050)]),
            // A shader-write range over the copy's source.
            (
                3,
                &[ShadowStep::Refresh(5000, 5010), ShadowStep::Write(2000, 64)],
            ),
            // Ends where the write begins: half-open, clear of it.
            (4, &[ShadowStep::Refresh(60, 120)]),
        ]);
        let (now, kept) = split(ops.take(), &[(120, 130), (2050, 2060)]);
        assert_eq!(keys(&now), vec![1, 3]);
        assert_eq!(keys(&kept), vec![2, 4]);
    }

    #[test]
    fn a_copy_around_the_write_is_kept_until_a_step_touches_it() {
        // The covering span would overlap, the steps do not: kept.
        let mut ops = ops(&[(
            7,
            &[
                ShadowStep::Refresh(0, 10),
                ShadowStep::Write(500, 4),
                ShadowStep::Refresh(20, 30),
            ],
        )]);
        let (now, kept) = split(ops.take(), &[(10, 20), (600, 610)]);
        assert!(now.is_empty());
        assert_eq!(keys(&kept), vec![7]);
        let mut ops = ops_back(kept);
        // A refresh over the write lands on a copy that touches it now.
        ops.refresh(7, &7, 12, 18);
        let (now, _) = split(ops.take(), &[(10, 20), (10, 20)]);
        assert_eq!(keys(&now), vec![7]);
    }

    fn ops_back(kept: Vec<super::ShadowBatch<usize>>) -> CopyOps<usize> {
        let mut ops = CopyOps::<usize>::default();
        ops.put_back(kept, |&k| k);
        ops
    }

    #[test]
    fn kept_copies_take_later_steps_without_merging_across_the_write() {
        let mut ops = ops(&[
            (1, &[ShadowStep::Refresh(0, 10)]),
            (2, &[ShadowStep::Refresh(100, 110)]),
        ]);
        let (now, kept) = split(ops.take(), &[(100, 105), (900, 905)]);
        assert_eq!(keys(&now), vec![2]);
        let mut ops = ops_back(kept);
        // Copy 1 again after the title write: into its kept entry, behind a barrier.
        ops.refresh(1, &1, 10, 20);
        // Copy 2 again: an entry of its own, as after a full flush.
        ops.refresh(2, &2, 100, 110);
        let copies = ops.take();
        assert_eq!(keys(&copies), vec![1, 2]);
        assert_eq!(
            copies[0].steps,
            vec![
                ShadowStep::Refresh(0, 10),
                ShadowStep::Barrier,
                ShadowStep::Refresh(10, 20)
            ]
        );
        assert_eq!(copies[0].refresh_span, Some((0, 20)));
        assert_eq!(copies[1].steps, vec![ShadowStep::Refresh(100, 110)]);
    }

    #[test]
    fn declared_ranges_stay_known_across_a_scoped_flush() {
        let mut ops = ops(&[
            (1, &[ShadowStep::Write(0, 16)]),
            (2, &[ShadowStep::Write(64, 16)]),
        ]);
        let (_, kept) = split(ops.take(), &[(0, 8), (0, 8)]);
        let declared = std::mem::take(&mut ops.declared);
        let mut ops = ops_back(kept);
        ops.declared = declared;
        // Declared before: no second write step, as after a full flush.
        ops.gpu_write(1, &1, 0, 16);
        ops.gpu_write(2, &2, 64, 16);
        let copies = ops.take();
        assert_eq!(keys(&copies), vec![2]);
        assert_eq!(copies[0].steps, vec![ShadowStep::Write(64, 16)]);
    }
}
