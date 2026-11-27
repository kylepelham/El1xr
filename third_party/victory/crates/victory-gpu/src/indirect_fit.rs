//! `VICTORY_INDIRECT_FIT=1`: an indirect execution of many records whose
//! records run past the end of the title buffer holding the first one is
//! run with as many records as the buffer holds, instead of being dropped
//! whole ("arguments outside title buffers", `crate::drop_why`).
//!
//! Every record the host then reads lies in the title's buffer (its copy):
//! nothing is read past its end. A count buffer still says how many of them
//! run (the card takes the smaller of it and the records kept). Records past
//! the buffer's end are not drawn, as before; only those that fit are made.

use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::shadow::Shadow;

/// Whether executions are cut to their buffer (`VICTORY_INDIRECT_FIT=1`).
fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        let on = std::env::var_os("VICTORY_INDIRECT_FIT").is_some_and(|v| v == "1");
        if on {
            tracing::info!("indirect executions running past their buffer are cut to it (VICTORY_INDIRECT_FIT=1)");
        }
        on
    })
}

/// Since the last frame line: executions cut, and records left out by it.
pub(crate) static FIT: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

/// Of `max` records `stride` bytes apart, how many lie in the `left` bytes
/// from the first to the buffer's end; `None` for none, or for all of them
/// (nothing to cut).
fn records_in(left: u64, stride: u32, max: u32) -> Option<u32> {
    if stride == 0 || max <= 1 {
        return None;
    }
    let fit = left / u64::from(stride);
    (fit >= 1 && fit < u64::from(max)).then_some(fit as u32)
}

/// For an execution at `arguments` of `max` records of `stride` bytes whose
/// records the title buffers do not all hold: the copy of the buffer
/// holding the first record and the records it holds, while switched on.
pub(crate) fn fitted(arguments: u64, stride: u32, max: u32) -> Option<(Arc<Shadow>, u32)> {
    if !on() || max <= 1 {
        return None;
    }
    let args = crate::shadow::find_any(arguments, u64::from(stride))?;
    let kept = records_in(args.left(arguments), stride, max)?;
    FIT[0].fetch_add(1, Ordering::Relaxed);
    FIT[1].fetch_add(u64::from(max - kept), Ordering::Relaxed);
    once_said(arguments, stride, max, kept);
    Some((args, kept))
}

fn once_said(arguments: u64, stride: u32, max: u32, kept: u32) {
    static SAID: AtomicU64 = AtomicU64::new(0);
    if SAID.fetch_add(1, Ordering::Relaxed) < 8 {
        tracing::info!(
            arguments = format_args!("{arguments:#x}"),
            stride,
            max,
            kept,
            "VICTORY_INDIRECT_FIT: an indirect execution past its buffer's end cut to the records it holds"
        );
    }
}

/// Frames presented at the last line.
static FRAMES_AT: AtomicU64 = AtomicU64::new(0);

/// Beside the frame line: executions cut and records left out per frame.
pub(crate) fn line(presented: u64) {
    if !on() {
        return;
    }
    let frames = presented.saturating_sub(FRAMES_AT.swap(presented, Ordering::Relaxed)).max(1) as f64;
    let [cut, left_out] = [0, 1].map(|i| FIT[i].swap(0, Ordering::Relaxed) as f64 / frames);
    if cut > 0.0 {
        tracing::info!(
            "indirect executions per frame cut to their buffer (VICTORY_INDIRECT_FIT=1): {cut:.1}, {left_out:.1} records past its end left out"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_records_wholly_in_the_buffer_are_kept() {
        // 100 bytes left, 20-byte records: 5 fit of 8.
        assert_eq!(records_in(100, 20, 8), Some(5));
        // A record cut by the end is not kept.
        assert_eq!(records_in(99, 20, 8), Some(4));
        // Not even the first fits: nothing to run.
        assert_eq!(records_in(19, 20, 8), None);
        // All fit: nothing to cut (the execution was not outside).
        assert_eq!(records_in(160, 20, 8), None);
        assert_eq!(records_in(1 << 20, 20, 8), None);
        // One record, or no stride: nothing to cut.
        assert_eq!(records_in(100, 20, 1), None);
        assert_eq!(records_in(100, 0, 8), None);
    }

    #[test]
    fn kept_records_never_reach_past_the_end() {
        for left in 0..300u64 {
            for stride in [4u32, 12, 20, 32] {
                for max in [2u32, 3, 16, 4096] {
                    if let Some(kept) = records_in(left, stride, max) {
                        assert!(u64::from(kept) * u64::from(stride) <= left);
                        assert!(kept >= 1 && kept < max);
                    }
                }
            }
        }
    }
}
