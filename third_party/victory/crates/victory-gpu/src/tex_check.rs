//! Parallel submission-wide texture checks on helper threads
//! (`VICTORY_PARALLEL_REFRESH`, ported from `fxceless/victory-fork` `ceb9e1f7a0`).
//!
//! Between title-memory writes, `Step::Texture` checks across every command list
//! of a submission are gathered and run side by side on the worker pool
//! (`crate::shadow::wide_chunks`). Each check takes its own texture lock, the
//! watch journal lock for its query, a shared mapping lock, and a thread-local
//! read buffer so hashing and detiling scale across cores.

use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Instant;

/// Mode values for [`parse_parallel_refresh`]: 0 off, 1 on, 2 alternating 20 s.
const MODE_OFF: u8 = 0;
const MODE_ON: u8 = 1;
const MODE_ALT: u8 = 2;

/// Parses `VICTORY_PARALLEL_REFRESH`: `"0"` turns parallel checks off (serial
/// at each `Step::Texture`), `"alt"` toggles on/off every 20 s for single-run
/// A/B measurement, and unset or `"1"` turns them on by default.
fn parse_parallel_refresh(value: Option<&str>) -> u8 {
    match value.map(str::trim) {
        Some("0") => MODE_OFF,
        Some("alt") => MODE_ALT,
        _ => MODE_ON,
    }
}

/// Whether a submission's texture checks (`crate::texture::Staging::refresh`)
/// are gathered across lists between title-memory writes and run side by side
/// on the helper threads (`VICTORY_PARALLEL_REFRESH`: on by default; `0`: off,
/// one after the other at each `Step::Texture`; `alt`: twenty seconds on,
/// twenty seconds off, in turn).
pub(crate) fn parallel_refresh() -> bool {
    static MODE: OnceLock<u8> = OnceLock::new();
    match *MODE.get_or_init(|| {
        parse_parallel_refresh(std::env::var("VICTORY_PARALLEL_REFRESH").ok().as_deref())
    }) {
        MODE_OFF => false,
        MODE_ON => true,
        _ => {
            static EPOCH: OnceLock<Instant> = OnceLock::new();
            let on = (EPOCH.get_or_init(Instant::now).elapsed().as_secs() / 20) % 2 == 1;
            static LAST: AtomicU8 = AtomicU8::new(2);
            if LAST.swap(u8::from(on), Ordering::Relaxed) != u8::from(on) {
                tracing::info!(
                    "a submission's texture checks run side by side from now (VICTORY_PARALLEL_REFRESH=alt): {}",
                    if on {
                        "ON"
                    } else {
                        "off, one after the other as usual"
                    }
                );
            }
            on
        }
    }
}

/// Since the last frame-meter line (and lifetime total for periodic logging):
/// `[batches, textures, summed_thread_us, wall_us]`.
static WINDOW_STATS: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];
static TOTAL_STATS: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

/// Excess microseconds to subtract from `PREPARING[2]` so the frame line's
/// `"textures N ms"` reflects wall time rather than the sum across helper
/// threads.
fn wall_adjustment(summed_us: u64, wall_us: u64) -> u64 {
    summed_us.saturating_sub(wall_us)
}

/// Runs `items` using `parallel_exec` when there are at least two items, or
/// directly inline with `run_one` for a single item.
fn dispatch_checks<T, FOne, FPar>(items: Vec<T>, mut run_one: FOne, parallel_exec: FPar)
where
    FOne: FnMut(T),
    FPar: FnOnce(Vec<T>),
{
    match items.len() {
        0 => {}
        1 => {
            for item in items {
                run_one(item);
            }
        }
        _ => parallel_exec(items),
    }
}

/// Runs `due` texture checks (`Staging::refresh`) side by side on the helper
/// threads (`crate::shadow::wide_chunks`, with the submitting thread joining
/// in), returning once every check has finished.
///
/// Updates `PREPARING[2]` so the frame line reports wall time rather than the
/// sum of worker-thread durations.
pub(crate) fn check_textures(due: Vec<Arc<crate::texture::Staging>>) {
    use std::sync::atomic::Ordering::Relaxed;
    if due.is_empty() {
        return;
    }
    let count = due.len() as u64;
    WINDOW_STATS[0].fetch_add(1, Relaxed);
    WINDOW_STATS[1].fetch_add(count, Relaxed);
    let batches = TOTAL_STATS[0].fetch_add(1, Relaxed) + 1;
    TOTAL_STATS[1].fetch_add(count, Relaxed);

    if due.len() < 2 {
        let started = Instant::now();
        for staging in due {
            staging.refresh();
        }
        let us = started.elapsed().as_micros() as u64;
        WINDOW_STATS[2].fetch_add(us, Relaxed);
        WINDOW_STATS[3].fetch_add(us, Relaxed);
        TOTAL_STATS[2].fetch_add(us, Relaxed);
        TOTAL_STATS[3].fetch_add(us, Relaxed);
        return;
    }

    let started = Instant::now();
    let before = crate::host::PREPARING[2].load(Relaxed);
    dispatch_checks(
        due,
        |staging| staging.refresh(),
        |batch| {
            let textures = Arc::new(batch);
            let total = textures.len();
            crate::shadow::wide_chunks(total, 1, move |start, end| {
                for staging in &textures[start..end] {
                    staging.refresh();
                }
            });
        },
    );
    let summed = crate::host::PREPARING[2]
        .load(Relaxed)
        .saturating_sub(before);
    let wall = started.elapsed().as_micros() as u64;
    let excess = wall_adjustment(summed, wall);
    if excess > 0 {
        // Clamp against current PREPARING[2] so a concurrent Meter::frame
        // swap(0) cannot underflow AtomicU64.
        let _ = crate::host::PREPARING[2]
            .fetch_update(Relaxed, Relaxed, |cur| Some(cur.saturating_sub(excess)));
    }
    WINDOW_STATS[2].fetch_add(summed, Relaxed);
    WINDOW_STATS[3].fetch_add(wall, Relaxed);
    TOTAL_STATS[2].fetch_add(summed, Relaxed);
    TOTAL_STATS[3].fetch_add(wall, Relaxed);

    if batches.is_multiple_of(1 << 14) {
        let [b, n, sum, w] = [0, 1, 2, 3].map(|i| TOTAL_STATS[i].load(Relaxed));
        tracing::info!(
            "texture checks side by side (VICTORY_PARALLEL_REFRESH) so far: {b} batches, {n} textures; \
             the threads' sum {} ms, the time taken {} ms",
            sum / 1000,
            w / 1000
        );
    }
}

/// Logs per-frame parallel texture check stats on the frame-meter cadence when
/// any batches ran in the interval.
pub(crate) fn log(frames: f64) {
    use std::sync::atomic::Ordering::Relaxed;
    crate::texture::blind_hash::log(frames);
    crate::texture::staged_ask::log(frames);
    crate::descriptor::host::host_textures::log(frames);
    let [batches, textures, sum_us, wall_us] =
        [0, 1, 2, 3].map(|i| WINDOW_STATS[i].swap(0, Relaxed));
    if batches == 0 {
        return;
    }
    let speedup = if wall_us > 0 {
        sum_us as f64 / wall_us as f64
    } else {
        1.0
    };
    tracing::info!(
        "texture checks side by side (VICTORY_PARALLEL_REFRESH) per frame: {:.1} batches, {:.0} textures \
         ({:.1}/batch); threads {:.2} ms -> wall {:.2} ms ({:.2}x)",
        batches as f64 / frames,
        textures as f64 / frames,
        textures as f64 / batches as f64,
        sum_us as f64 / 1e3 / frames,
        wall_us as f64 / 1e3 / frames,
        speedup
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn parse_parallel_refresh_modes() {
        assert_eq!(parse_parallel_refresh(None), MODE_ON);
        assert_eq!(parse_parallel_refresh(Some("1")), MODE_ON);
        assert_eq!(parse_parallel_refresh(Some(" 1 ")), MODE_ON);
        assert_eq!(parse_parallel_refresh(Some("0")), MODE_OFF);
        assert_eq!(parse_parallel_refresh(Some(" 0 ")), MODE_OFF);
        assert_eq!(parse_parallel_refresh(Some("alt")), MODE_ALT);
        assert_eq!(parse_parallel_refresh(Some(" alt ")), MODE_ALT);
    }

    #[test]
    fn wall_adjustment_subtracts_excess_without_underflow() {
        assert_eq!(wall_adjustment(4000, 1000), 3000);
        assert_eq!(wall_adjustment(1000, 1000), 0);
        assert_eq!(wall_adjustment(500, 1000), 0);
        assert_eq!(wall_adjustment(0, 0), 0);
    }

    #[test]
    fn dispatch_checks_routes_empty_single_and_multi() {
        let mut inline_hits = 0usize;
        let mut parallel_hits = 0usize;

        dispatch_checks(
            Vec::<u32>::new(),
            |_| inline_hits += 1,
            |_| parallel_hits += 1,
        );
        assert_eq!((inline_hits, parallel_hits), (0, 0));

        dispatch_checks(
            vec![42u32],
            |v| {
                assert_eq!(v, 42);
                inline_hits += 1;
            },
            |_| parallel_hits += 1,
        );
        assert_eq!((inline_hits, parallel_hits), (1, 0));

        dispatch_checks(
            vec![1u32, 2, 3],
            |_| inline_hits += 1,
            |v| {
                assert_eq!(v, vec![1, 2, 3]);
                parallel_hits += 1;
            },
        );
        assert_eq!((inline_hits, parallel_hits), (1, 1));
    }

    #[test]
    fn wide_chunks_runs_multi_texture_batch_completely() {
        let done = Arc::new(AtomicUsize::new(0));
        let items: Vec<Arc<AtomicUsize>> = (0..16).map(|_| done.clone()).collect();
        dispatch_checks(
            items,
            |c| {
                c.fetch_add(1, Ordering::Relaxed);
            },
            |batch| {
                let batch = Arc::new(batch);
                let total = batch.len();
                crate::shadow::wide_chunks(total, 1, move |start, end| {
                    for c in &batch[start..end] {
                        c.fetch_add(1, Ordering::Relaxed);
                    }
                });
            },
        );
        assert_eq!(done.load(Ordering::Relaxed), 16);
    }

    #[test]
    fn empty_check_textures_is_noop() {
        let before = TOTAL_STATS[0].load(Ordering::Relaxed);
        check_textures(Vec::new());
        assert_eq!(TOTAL_STATS[0].load(Ordering::Relaxed), before);
    }
}
