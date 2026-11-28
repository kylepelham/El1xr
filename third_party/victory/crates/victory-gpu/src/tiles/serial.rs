//! (worldstream, #26) Tile mappings and heaps made one at a time
//! (`VICTORY_TILES_SERIAL=1`; off by default).
//!
//! With `VICTORY_TILES_AHEAD=1` the four prepare threads (and the bake
//! passes' threads) map tiles side by side, beside the submission's own
//! mappings and the texture block thread's `CreateHeap` + `MakeResident`.
//! On the card, the title not running (`tiles::bench`: 2048x2048 BC7
//! textures, levels 1.. a call, 10 reps interleaved, two runs a and b):
//! - one thread alone: 73-83 us a call (p99 0.55-0.78 ms);
//! - beside the block thread (one heap made resident each 2.2-2.6 ms):
//!   74-115 us a call: one mapping thread is not slowed by it;
//! - four threads: 741-3,251 us a call (p99 11-43 ms, max 0.2-1.4 s),
//!   317-1,262 us of wall time a call in all; with one lock around each
//!   call 369-377 us of wall time a call (p99 18-19 ms, max 83-174 ms);
//! - four threads beside the block thread: 1,355-3,710 us a call,
//!   564-1,352 us of wall time a call; with the lock around the calls and
//!   the block thread's `CreateHeap` and `MakeResident`, 353-441 us of
//!   wall time a call (p99 16-25 ms, max 106-156 ms), 1.6-3.1x less;
//! - one thread mapping the four threads' textures in turn: 322 us of
//!   wall time a call: what is left grows with the mappings already held,
//!   not with the threads.
//!
//! With the switch every `UpdateTileMappings` (any thread), the tile
//! pool's `CreateHeap`, and the texture blocks' `CreateHeap` and
//! `MakeResident` hold one lock; the time spent waiting for it is counted
//! on its own line, outside the calls' microseconds.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

/// `VICTORY_TILES_SERIAL=1`.
pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        let on = parse(std::env::var("VICTORY_TILES_SERIAL").ok().as_deref());
        if on {
            tracing::info!(
                "UpdateTileMappings calls are made one at a time (VICTORY_TILES_SERIAL=1)"
            );
        }
        on
    })
}

fn parse(value: Option<&str>) -> bool {
    value.is_some_and(|v| v.trim() == "1")
}

static ONE: Mutex<()> = Mutex::new(());

/// Since the line before: waits for the lock, their microseconds, the
/// longest.
static WAITS: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

/// Runs `call` (one `UpdateTileMappings`, `CreateHeap` or
/// `MakeResident`), holding the lock when `on`.
pub(crate) fn call<R>(on: bool, call: impl FnOnce() -> R) -> R {
    if !on {
        return call();
    }
    let began = Instant::now();
    let _held = ONE.lock().unwrap_or_else(|e| e.into_inner());
    let waited = began.elapsed().as_micros() as u64;
    WAITS[0].fetch_add(1, Relaxed);
    WAITS[1].fetch_add(waited, Relaxed);
    WAITS[2].fetch_max(waited, Relaxed);
    call()
}

/// The line of the waits since the line before (none while off).
pub(super) fn line() {
    let [n, us, max] = std::array::from_fn(|i| WAITS[i].swap(0, Relaxed));
    if n == 0 {
        return;
    }
    tracing::info!(
        "tiles serial (VICTORY_TILES_SERIAL=1) since the line before: {n} tile mappings, heaps made and blocks made resident waited {:.1} ms for the lock in all, the longest {:.2} ms",
        us as f64 / 1000.0,
        max as f64 / 1000.0
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_takes_only_one() {
        assert!(parse(Some("1")));
        assert!(parse(Some(" 1 ")));
        assert!(!parse(Some("0")));
        assert!(!parse(Some("")));
        assert!(!parse(None));
    }

    #[test]
    fn calls_are_one_at_a_time_when_on() {
        use std::sync::atomic::AtomicUsize;
        static INSIDE: AtomicUsize = AtomicUsize::new(0);
        static MOST: AtomicUsize = AtomicUsize::new(0);
        let threads: Vec<_> = (0..4)
            .map(|_| {
                std::thread::spawn(|| {
                    for _ in 0..50 {
                        call(true, || {
                            let now = INSIDE.fetch_add(1, Relaxed) + 1;
                            MOST.fetch_max(now, Relaxed);
                            std::thread::sleep(std::time::Duration::from_micros(50));
                            INSIDE.fetch_sub(1, Relaxed);
                        });
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(MOST.load(Relaxed), 1);
        assert_eq!(call(false, || 7), 7);
    }
}
