//! `VICTORY_RAW_CHECK=1|2` (live, off by default): an indirect record that
//! `VICTORY_INDIRECT_RAW=2` runs as the title wrote it is read first.
//!
//! A raw record has no pass of ours and no budget (`super::budget`) behind
//! it: a record of garbage (4 billion vertices, say) is the card's to run
//! for seconds. Batch 5 saw the card stand 8.8 s and 8.2 s on the world's
//! first frame, the owed submission led by raw indirect draws.
//!
//! `=1`: where no shader wrote the record lately (not this list's, not any
//! submission's in the last frames), title memory holds what the card will
//! read, so the record is read there; one over the limits the sanitizer
//! uses (more than 2^24 vertices or instances or 2^26 vertex instances;
//! more than 65,535 groups in a dimension, or 2^22 mesh groups in all) is
//! not run, counted and its first few logged. A record shaders wrote runs
//! raw unchecked (counted). `=2`: as `=1`, and a record shaders wrote goes
//! to the clamp (bounded by the list's budget) instead of running raw.
//!
//! shortcut: title memory is read when the record is recorded; the host copy
//! is brought up to date at submission. A title that writes a record between
//! the two is read early (its draw is then judged by the older value).

use std::sync::atomic::{AtomicU64, Ordering};

use victory_core::settings::Live;

fn parse(v: Option<&str>) -> u64 {
    match v.map(str::trim) {
        Some("1") => 1,
        Some("2") => 2,
        _ => 0,
    }
}

static MODE: Live = Live::new("VICTORY_RAW_CHECK", parse);

pub(crate) fn mode() -> u64 {
    MODE.get()
}

const DRAW: u32 = 0;
const INDEXED: u32 = 1;
const DISPATCH: u32 = 2;
const MESH: u32 = 10;

/// The most records of one execution read (an execution of more is judged
/// by its first ones).
const MOST: u32 = 256;

/// Whether a record of `kind` whose first words are `w` asks for more than
/// the limits (the sanitizer's, `super::sanitized`; a dispatch only over
/// the 65,535 groups a dimension the card takes).
pub(crate) fn over(kind: u32, w: [u32; 3]) -> bool {
    match kind {
        DRAW | INDEXED => {
            let (count, instances) = (u64::from(w[0]), u64::from(w[1]));
            count > 1 << 24 || instances > 1 << 24 || count * instances > 1 << 26
        }
        MESH => {
            w.iter().any(|&g| g > 65_535)
                || u64::from(w[0]) * u64::from(w[1]) * u64::from(w[2]) > 1 << 22
        }
        DISPATCH => w.iter().any(|&g| g > 65_535),
        _ => false,
    }
}

/// What the caller does with the record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Check {
    /// Run it raw (fine, or not read).
    Run,
    /// Over the limits: not run.
    Drop,
    /// Shaders wrote it and `=2`: clamp it.
    Clamp,
}

/// Since the last frame line: records read and fine, records dropped, raw
/// executions shaders wrote run unchecked, and those sent to the clamp.
static COUNTS: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

/// The verdict for `n` records read with `read` (record index to its first
/// words), where `visible` says title memory holds what the card reads.
fn judge(
    mode: u64,
    kind: u32,
    n: u32,
    visible: bool,
    read: impl Fn(u32) -> [u32; 3],
) -> (Check, Option<(u32, [u32; 3])>) {
    if mode == 0 {
        return (Check::Run, None);
    }
    if !visible {
        return (if mode == 2 { Check::Clamp } else { Check::Run }, None);
    }
    for i in 0..n.min(MOST) {
        let w = read(i);
        if over(kind, w) {
            return (Check::Drop, Some((i, w)));
        }
    }
    (Check::Run, None)
}

/// Reads the raw execution of `kind` with `max` records of `stride` bytes at
/// title address `arguments` (and a count at `count`) in `list`. `name`: its
/// shaders, for the log.
pub(crate) fn check(
    list: &crate::host::HostList,
    (kind, stride, max): (u32, u32, u32),
    arguments: u64,
    count: Option<u64>,
    name: impl FnOnce() -> String,
) -> Check {
    let mode = mode();
    if mode == 0 {
        return Check::Run;
    }
    let unwritten = |at: u64, len: u64| {
        !list.gpu_written(at as usize, len as usize)
            && !crate::host::shader_written(at as usize, len as usize)
    };
    let max = max.max(1);
    let (n, visible) = match count {
        None => (max, true),
        // SAFETY: the count lies in a title buffer (`crate::shadow::find_any` found it).
        Some(at) if unwritten(at, 4) => (
            unsafe { (at as *const u32).read_unaligned() }.min(max),
            true,
        ),
        Some(_) => (0, false),
    };
    let visible = visible && unwritten(arguments, u64::from(stride) * u64::from(n.min(MOST)));
    let (verdict, bad) = judge(mode, kind, n, visible, |i| {
        let at = arguments + u64::from(i) * u64::from(stride);
        // SAFETY: inside the title buffer of `max` records the caller found.
        unsafe { (at as *const [u32; 3]).read_unaligned() }
    });
    let slot = match verdict {
        Check::Run if visible => 0,
        Check::Drop => 1,
        Check::Run => 2,
        Check::Clamp => 3,
    };
    COUNTS[slot].fetch_add(1, Ordering::Relaxed);
    if let Some((i, w)) = bad {
        static SHOWN: AtomicU64 = AtomicU64::new(0);
        if SHOWN.fetch_add(1, Ordering::Relaxed) < 16 {
            tracing::warn!(
                "raw check: an indirect record over the limits is not run (VICTORY_RAW_CHECK={mode}): victory list #{} kind {kind} record {i} of {n} at {:#x}: {} {} {} [{}]",
                list.ops.id(),
                arguments + u64::from(i) * u64::from(stride),
                w[0],
                w[1],
                w[2],
                name()
            );
        }
    }
    verdict
}

/// The frame line's counts, with the switch set.
pub(crate) fn line(frames: f64) {
    let [fine, dropped, unchecked, clamped] =
        [0, 1, 2, 3].map(|i| COUNTS[i].swap(0, Ordering::Relaxed));
    let mode = mode();
    if mode == 0 {
        return;
    }
    tracing::info!(
        "raw check (VICTORY_RAW_CHECK={mode}): per frame {:.1} raw executions read in title memory and run, {:.2} over the limits not run, {:.1} written by shaders run unchecked, {:.1} written by shaders sent to the clamp ({dropped} not run in all)",
        fine as f64 / frames,
        dropped as f64 / frames,
        unchecked as f64 / frames,
        clamped as f64 / frames,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn draws_over_the_sanitizers_limits_are_over() {
        assert!(!over(DRAW, [6, 1, 0]));
        assert!(!over(INDEXED, [1 << 24, 4, 0]));
        assert!(over(DRAW, [u32::MAX, 1, 0]));
        assert!(over(INDEXED, [3, (1 << 24) + 1, 0]));
        assert!(over(DRAW, [1 << 14, 1 << 13, 0]));
        // 1.0f read as a count.
        assert!(over(DRAW, [0x3f80_0000, 1, 0]));
    }

    #[test]
    fn mesh_and_dispatch_limits() {
        assert!(!over(MESH, [64, 64, 1]));
        assert!(over(MESH, [65_536, 1, 1]));
        assert!(over(MESH, [4096, 2048, 1]));
        assert!(!over(DISPATCH, [65_535, 65_535, 1]));
        assert!(over(DISPATCH, [97_824, 1, 1]));
    }

    #[test]
    fn off_runs_everything_unread() {
        let read = |_| panic!("read with the switch off");
        assert_eq!(judge(0, DRAW, 1, true, read), (Check::Run, None));
    }

    #[test]
    fn a_garbage_record_in_title_memory_is_dropped() {
        let records = [[3, 1, 0], [u32::MAX, u32::MAX, 0]];
        for mode in [1, 2] {
            let (verdict, bad) = judge(mode, DRAW, 2, true, |i| records[i as usize]);
            assert_eq!(verdict, Check::Drop);
            assert_eq!(bad, Some((1, [u32::MAX, u32::MAX, 0])));
        }
        assert_eq!(
            judge(1, DRAW, 1, true, |i| records[i as usize]).0,
            Check::Run
        );
    }

    #[test]
    fn a_record_shaders_wrote_runs_unchecked_or_goes_to_the_clamp() {
        let read = |_| panic!("title memory read where shaders wrote");
        assert_eq!(judge(1, DRAW, 1, false, read), (Check::Run, None));
        assert_eq!(judge(2, DRAW, 1, false, read), (Check::Clamp, None));
    }

    #[test]
    fn at_most_256_records_are_read() {
        let n = std::cell::Cell::new(0);
        let (verdict, _) = judge(1, DRAW, 100_000, true, |_| {
            n.set(n.get() + 1);
            [3, 1, 0]
        });
        assert_eq!((verdict, n.get()), (Check::Run, MOST));
    }

    #[test]
    fn the_switch_reads_one_and_two() {
        assert_eq!(parse(Some("1")), 1);
        assert_eq!(parse(Some(" 2 ")), 2);
        assert_eq!(parse(Some("0")), 0);
        assert_eq!(parse(None), 0);
    }
}
