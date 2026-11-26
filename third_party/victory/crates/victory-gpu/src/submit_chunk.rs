//! Submissions sent to the card in chunks, and what their preparing costs.
//!
//! A submission sent ahead (`Host::execute_late`) is prepared whole on the
//! CPU (its host copies brought up to date, its textures checked, its
//! fills done) before one `ExecuteCommandLists` gives the card all of it.
//! Behind a big one the card stands idle for the whole preparing: in round
//! 18 the frame line counted 13.7 ms a frame of 45 with the card idle
//! while the submitting thread worked.
//!
//! With `VICTORY_SUBMIT_CHUNKS=N` a submission of more than `N` lists is
//! sent as several flights, in order: the first `N` lists, then twice as
//! many, and so on. Each is a flight of its own, as if the title had made
//! that many submissions to the queue, so the card runs the first chunk
//! while the next one is prepared. What keeps that right is what keeps
//! consecutive submissions right already: a chunk whose preparing reads
//! what a flight's shaders write waits for that flight to land, and a
//! refresh under a flight's writes merges the title's bytes beside them.
//! A list's bottom-of-pipe writes are told after its own chunk has run,
//! which is when the console tells them.
//!
//! The frame line's companion counts submissions by how long they took to
//! prepare, so where the card's idle time comes from can be read.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

/// `VICTORY_SUBMIT_CHUNKS`: the lists in a submission's first chunk (0 or
/// unset: submissions are sent whole).
pub(crate) fn first_chunk() -> usize {
    static FIRST: OnceLock<usize> = OnceLock::new();
    *FIRST.get_or_init(|| {
        std::env::var("VICTORY_SUBMIT_CHUNKS")
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .unwrap_or(0)
    })
}

/// The sizes of the chunks `lists` lists are sent in, the first `first`
/// lists and each next one twice the one before; one chunk when `first`
/// is 0 or the submission is no larger.
pub(crate) fn chunk_sizes(lists: usize, first: usize) -> Vec<usize> {
    if first == 0 || lists <= first {
        return vec![lists];
    }
    let mut sizes = Vec::new();
    let mut left = lists;
    let mut size = first;
    while left > 0 {
        let take = size.min(left);
        sizes.push(take);
        left -= take;
        size = size.saturating_mul(2);
    }
    sizes
}

/// `lists` in `order` cut into chunks of `sizes`, each with its own order.
/// Only a submission that names each list once, in the order they were
/// taken (`order` is `0..lists.len()`), is cut: a list named twice keeps
/// one host list, which cannot fly in two chunks. `None`: send it whole.
pub(crate) fn split<T>(
    mut lists: Vec<T>,
    order: &[usize],
    sizes: &[usize],
) -> Result<Vec<(Vec<T>, Vec<usize>)>, Vec<T>> {
    let plain = order.len() == lists.len() && order.iter().enumerate().all(|(i, &o)| i == o);
    if sizes.len() < 2 || !plain || sizes.iter().sum::<usize>() != lists.len() {
        return Err(lists);
    }
    let mut chunks = Vec::with_capacity(sizes.len());
    for &size in sizes.iter().rev() {
        let at = lists.len() - size;
        let tail = lists.split_off(at);
        chunks.push((tail, (0..size).collect()));
    }
    chunks.reverse();
    Ok(chunks)
}

/// Chunks sent and submissions cut since the last frame line.
static CUT: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

/// A submission was cut into `chunks` chunks.
pub(crate) fn cut(chunks: usize) {
    CUT[0].fetch_add(1, Ordering::Relaxed);
    CUT[1].fetch_add(chunks as u64, Ordering::Relaxed);
}

/// Upper bounds of the buckets of preparing time, in microseconds.
const BUCKETS_US: [u64; 4] = [250, 1000, 4000, u64::MAX];

/// Per bucket since the last frame line: submissions, their microseconds
/// of preparing, their lists. Then the slowest one's microseconds and its
/// lists.
static PREP: [AtomicU64; 3 * BUCKETS_US.len() + 2] =
    [const { AtomicU64::new(0) }; 3 * BUCKETS_US.len() + 2];

/// The parts of preparing (`PREP_PARTS`), then the microseconds and bytes
/// of copies (`PREPARING`), as counted so far.
pub(crate) type Parts = [u64; PARTS];
const PARTS: usize = 12;

/// The counters a submission's preparing adds to, read before it starts.
pub(crate) fn parts() -> Parts {
    let mut now = [0; PARTS];
    for (i, part) in crate::host::PREP_PARTS.iter().enumerate() {
        now[i] = part.load(Ordering::Relaxed);
    }
    now[10] = crate::host::PREPARING[0].load(Ordering::Relaxed);
    now[11] = crate::host::PREPARING[1].load(Ordering::Relaxed);
    now
}

/// For the submissions that took 4 ms and more to prepare: what each part
/// of their preparing added (another thread's copies may count in too).
static SLOW_PARTS: [AtomicU64; PARTS] = [const { AtomicU64::new(0) }; PARTS];

/// A submission (or chunk) of `lists` lists took `us` microseconds to
/// prepare; `before` is what [`parts`] read when it started.
pub(crate) fn prepared(lists: usize, us: u64, before: &Parts) {
    let b = BUCKETS_US
        .iter()
        .position(|&top| us < top)
        .unwrap_or(BUCKETS_US.len() - 1);
    PREP[3 * b].fetch_add(1, Ordering::Relaxed);
    PREP[3 * b + 1].fetch_add(us, Ordering::Relaxed);
    PREP[3 * b + 2].fetch_add(lists as u64, Ordering::Relaxed);
    let slowest = 3 * BUCKETS_US.len();
    if PREP[slowest].fetch_max(us, Ordering::Relaxed) < us {
        PREP[slowest + 1].store(lists as u64, Ordering::Relaxed);
    }
    if b == BUCKETS_US.len() - 1 {
        for ((slow, now), was) in SLOW_PARTS.iter().zip(parts()).zip(before) {
            slow.fetch_add(now.saturating_sub(*was), Ordering::Relaxed);
        }
    }
}

/// The frame line's companion: submissions by preparing time, per frame.
pub(crate) fn line(frames: f64) {
    let take = |i: usize| PREP[i].swap(0, Ordering::Relaxed) as f64;
    let names = ["under 0.25 ms", "0.25-1 ms", "1-4 ms", "4 ms and more"];
    let mut parts = Vec::with_capacity(names.len());
    for (b, name) in names.iter().enumerate() {
        let (count, us, lists) = (take(3 * b), take(3 * b + 1), take(3 * b + 2));
        parts.push(format!(
            "{name}: {:.1} ({:.1} ms, {:.1} lists)",
            count / frames,
            us / 1000.0 / frames,
            lists / frames
        ));
    }
    let slowest = 3 * BUCKETS_US.len();
    let (slow_us, slow_lists) = (take(slowest), take(slowest + 1));
    let [cut, chunks] = [0, 1].map(|i| CUT[i].swap(0, Ordering::Relaxed) as f64 / frames);
    tracing::info!(
        "submissions sent ahead by their preparing per frame: {}; the slowest {:.1} ms for {slow_lists:.0} lists; \
         {cut:.1} cut into {chunks:.1} chunks (VICTORY_SUBMIT_CHUNKS={})",
        parts.join(", "),
        slow_us / 1000.0,
        first_chunk()
    );
    let slow: Vec<f64> = SLOW_PARTS
        .iter()
        .map(|p| p.swap(0, Ordering::Relaxed) as f64 / frames)
        .collect();
    let names = [
        "sync_heaps",
        "make_waiting",
        "ask_once",
        "stale_flights",
        "run_flush_one",
        "run_other",
        "batch_flush",
        "snapshots",
        "card_before",
        "exec_submit",
    ];
    let text: Vec<String> = names
        .iter()
        .zip(&slow)
        .map(|(name, us)| format!("{name} {:.2} ms", us / 1000.0))
        .collect();
    tracing::info!(
        "of that, the submissions of 4 ms and more per frame: {}; copies {:.1} ms {:.1} MB",
        text.join(", "),
        slow[10] / 1000.0,
        slow[11] / 1e6
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_double_from_the_first_chunk() {
        assert_eq!(chunk_sizes(10, 0), vec![10]);
        assert_eq!(chunk_sizes(4, 4), vec![4]);
        assert_eq!(chunk_sizes(5, 4), vec![4, 1]);
        assert_eq!(chunk_sizes(15, 1), vec![1, 2, 4, 8]);
        assert_eq!(chunk_sizes(20, 2), vec![2, 4, 8, 6]);
        assert_eq!(chunk_sizes(0, 3), vec![0]);
    }

    #[test]
    fn split_keeps_the_lists_in_order() {
        let chunks = split(vec!['a', 'b', 'c', 'd', 'e'], &[0, 1, 2, 3, 4], &[1, 2, 2]).unwrap();
        assert_eq!(
            chunks,
            vec![
                (vec!['a'], vec![0]),
                (vec!['b', 'c'], vec![0, 1]),
                (vec!['d', 'e'], vec![0, 1]),
            ]
        );
    }

    #[test]
    fn a_list_named_twice_is_not_split() {
        let whole = split(vec!['a', 'b'], &[0, 1, 0], &[1, 2]).unwrap_err();
        assert_eq!(whole, vec!['a', 'b']);
        let reordered = split(vec!['a', 'b'], &[1, 0], &[1, 1]).unwrap_err();
        assert_eq!(reordered, vec!['a', 'b']);
    }

    #[test]
    fn one_chunk_is_not_split() {
        assert!(split(vec!['a', 'b'], &[0, 1], &[2]).is_err());
    }
}
