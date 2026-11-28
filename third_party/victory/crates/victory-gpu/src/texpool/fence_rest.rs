//! (racefix #57) `VICTORY_POOL_FENCE_REST=1` (Live; off by default; not in
//! shader/pipeline cache key): two fence-lifetime guards for the launcher-active
//! 64 MiB host memory pools (`UPLOAD_BLOCKS`, `SHARED_BUFFER_BLOCKS`,
//! `CARD_BUFFER_BLOCKS`, `TEXTURE_HEAPS` + `TEXTURE_BLOCK_THREAD`,
//! `STAGING_BLOCKS`):
//!
//! 1. **No 2.5 s wall-clock reclaim while flights are still in flight:**
//!    `Pool::reclaim` used `Resting::over` (`now >= at + 2500 || done`), so any
//!    multi-second GPU stall, shader compile hitch, or `victory-send` backlog
//!    past 2.5 s (under this PC's 10 s TDR) reclaimed a returned piece while the
//!    submission referencing it had not finished on the card. On the next cut
//!    (`again = true`) the CPU zeroed mapped `UPLOAD`/`SHARED`/`STAGING` memory
//!    under the in-flight GPU list, aliased a live `TEXTURE_HEAPS` texture, or
//!    dropped the 64 MiB `ID3D12Heap` via `let_go_spares`/`trim`/`trim_slack` or
//!    `victory-texture-blocks`. (`CARD_BLOCK_REUSE` already bypassed the 2.5 s
//!    timer with `Resting::done`.) With this switch on, a piece returned after
//!    any submission has begun waits for `finished >= begun + LAG` (no wall-clock
//!    shortcut).
//! 2. **Stamp new blocks AFTER `CreateHeap` + `MakeResident`:**
//!    Both `texpool::maker::fill` (`VICTORY_TEXTURE_BLOCK_THREAD=1`) and
//!    `texpool::take_from` sampled `submissions_started()` *before* `make()`
//!    ran `CreateHeap` + `MakeResident` (11–45 ms). Submissions begun during
//!    `make()` could finish the old stamp while still in flight, making a
//!    brand-new block immediately look "ripe" (`block.made <= finished`). With
//!    this switch on, `block.made` is stamped after `make()` returns.

use std::sync::atomic::{AtomicU64, Ordering};

use super::{REST, Resting};

static ON: victory_core::settings::Live =
    victory_core::settings::Live::flag("VICTORY_POOL_FENCE_REST");

#[cfg(test)]
thread_local! {
    /// Tests: overrides `VICTORY_POOL_FENCE_REST` on this thread.
    pub(super) static FORCED: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

/// Whether `VICTORY_POOL_FENCE_REST=1` is active (Live).
pub(super) fn on() -> bool {
    #[cfg(test)]
    if let Some(v) = FORCED.with(std::cell::Cell::get) {
        return v;
    }
    ON.get_bool()
}

/// Counts since the last line:
/// `[0]` pieces held past the 2.5 s `REST` timeout because flights had not finished;
/// `[1]` newly made blocks whose `made` stamp moved forward after `make()`.
static STATS: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

/// Whether a resting piece in a non-reuse pool may be reclaimed at `(now, started, finished)`.
pub(super) fn piece_ready(resting: Resting, now: u64, started: u64, finished: u64) -> bool {
    if !on() {
        return resting.over(now, finished);
    }
    if resting.done(now, finished) {
        return true;
    }
    // Before any submission has ever started (startup or unit tests with no
    // GPU submissions), nothing can be in flight on the card.
    if resting.begun == 0 && started == 0 {
        return now >= resting.at + REST;
    }
    if now >= resting.at + REST {
        STATS[0].fetch_add(1, Ordering::Relaxed);
    }
    false
}

/// The `block.made` submission stamp for a block whose `make()` began at
/// `before` and finished at `after`.
pub(super) fn made_stamp(before: u64, after: u64) -> u64 {
    if !on() {
        return before;
    }
    if after > before {
        STATS[1].fetch_add(1, Ordering::Relaxed);
    }
    after
}

/// Periodic diagnostic line (at most every 5 s while on or non-zero).
/// Grep: `pool fence rest (VICTORY_POOL_FENCE_REST=1):`.
pub(super) fn maybe_log(now: u64) {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let next = NEXT.load(Ordering::Relaxed);
    if now < next
        || NEXT
            .compare_exchange(next, now + 5000, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
    {
        return;
    }
    let held = STATS[0].swap(0, Ordering::Relaxed);
    let stamped = STATS[1].swap(0, Ordering::Relaxed);
    if !on() && held == 0 && stamped == 0 {
        return;
    }
    tracing::info!(
        "pool fence rest (VICTORY_POOL_FENCE_REST=1): held {held} pieces past 2.5 s while flights ran, \
         stamped {stamped} blocks after make"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::texpool::{LAG, Piece, Pool, Ripe, SETTLE};
    use std::collections::VecDeque;

    const K64: u64 = 64 << 10;

    fn next_rng(seed: &mut u64) -> u64 {
        let mut x = *seed;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *seed = x;
        x
    }

    #[test]
    fn the_switch_is_live_and_off_by_default() {
        if std::env::var_os("VICTORY_POOL_FENCE_REST").is_some() {
            return;
        }
        assert!(!on(), "off by default");
        victory_core::settings::set("VICTORY_POOL_FENCE_REST", "1").unwrap();
        let now = on();
        victory_core::settings::set("VICTORY_POOL_FENCE_REST", "").unwrap();
        assert!(now, "live toggle turns on");
        assert!(!on(), "live toggle turns off");
    }

    /// Simulates pooled buffer/texture allocations, returns, trims, and GPU
    /// flights (including multi-second stalls up to 9 s under a 10 s TDR).
    /// Returns `(write_under_flight, drop_under_flight)` hazard counts.
    fn run_stall_model(fence_rest: bool) -> (usize, usize) {
        static ALIVE: AtomicU64 = AtomicU64::new(0);
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                FORCED.with(|f| f.set(None));
            }
        }
        let _reset = Reset;
        FORCED.with(|f| f.set(Some(fence_rest)));

        let mut pool: Pool<u64> = Pool::new(2, &ALIVE);
        let mut next_block_id = 1u64;
        let mut seed = 0x51a7_cafe_1234_5678u64;
        let mut now_ms = 0u64;
        let mut started = 10u64;
        let mut finished = 10u64;

        // Live GPU flights: `(submission_number, completion_ms, pieces_referenced, block_ids_referenced)`.
        let mut flights: VecDeque<(u64, u64, Vec<Piece>, Vec<u64>)> = VecDeque::new();
        let mut live_pieces: Vec<(Piece, u64)> = Vec::new();
        let mut write_under_flight = 0usize;
        let mut drop_under_flight = 0usize;

        for _step in 0..2000 {
            // Advance clock; occasionally inject a 2.6 s - 8.5 s hitch/stall (under 10 s TDR).
            let r = next_rng(&mut seed);
            let dt = if r % 19 == 0 {
                2600 + (r % 5900)
            } else {
                5 + (r % 30)
            };
            now_ms += dt;

            // Retire flights whose GPU completion time has passed (only when not stalled).
            while let Some(&(num, done_ms, _, _)) = flights.front() {
                if done_ms <= now_ms {
                    flights.pop_front();
                    finished = finished.max(num);
                } else {
                    break;
                }
            }

            // Occasionally start a new flight (which may stall for 3-9 s).
            if next_rng(&mut seed) % 3 == 0 && !live_pieces.is_empty() {
                started += 1;
                let stall = if next_rng(&mut seed) % 7 == 0 {
                    3000 + (next_rng(&mut seed) % 6000)
                } else {
                    16 + (next_rng(&mut seed) % 50)
                };
                let prev_done = flights.back().map_or(now_ms, |f| f.1);
                let done_ms = prev_done.max(now_ms) + stall;
                let count = 1 + (next_rng(&mut seed) as usize % live_pieces.len().min(4));
                let mut refs = Vec::new();
                let mut bids = Vec::new();
                for _ in 0..count {
                    let idx = next_rng(&mut seed) as usize % live_pieces.len();
                    let (p, bid) = live_pieces[idx];
                    refs.push(p);
                    bids.push(bid);
                }
                flights.push_back((started, done_ms, refs, bids));
            }

            // Return a CPU reference to a piece (even while a flight still uses it).
            if !live_pieces.is_empty() && next_rng(&mut seed) % 2 == 0 {
                let idx = next_rng(&mut seed) as usize % live_pieces.len();
                let (piece, _) = live_pieces.swap_remove(idx);
                pool.resting.push_back((
                    Resting {
                        at: now_ms,
                        begun: started,
                    },
                    piece,
                ));
            }

            // Reclaim / trim check.
            let gone = if next_rng(&mut seed) % 11 == 0 {
                pool.reclaim_at(now_ms, started, finished, 0)
            } else {
                pool.reclaim_at(now_ms, started, finished, 2)
            };
            for dropped_bid in gone {
                let still_in_flight = flights
                    .iter()
                    .any(|(_, _, _, bids)| bids.contains(&dropped_bid));
                if still_in_flight {
                    drop_under_flight += 1;
                }
            }

            // Allocate a new piece (which CPU zeroes when `again == true`).
            let len = K64 * (1 + (next_rng(&mut seed) % 8));
            let mut freed = Vec::new();
            let cut = pool.cut_at(len, K64, now_ms, started, finished, Ripe::ALL, &mut freed);
            for dropped_bid in freed {
                if flights
                    .iter()
                    .any(|(_, _, _, bids)| bids.contains(&dropped_bid))
                {
                    drop_under_flight += 1;
                }
            }
            let taken = match cut {
                Some(t) => Some(t),
                None => {
                    let id = next_block_id;
                    next_block_id += 1;
                    pool.add(id, started);
                    let mut f2 = Vec::new();
                    pool.cut_at(len, K64, now_ms, started, finished, Ripe::ALL, &mut f2)
                }
            };
            if let Some((piece, bid, again)) = taken {
                if again {
                    let overlap_flight = flights.iter().any(|(_, _, refs, bids)| {
                        refs.iter().zip(bids).any(|(r, &rb)| {
                            rb == bid
                                && piece.start < r.start + r.len
                                && r.start < piece.start + piece.len
                        })
                    });
                    if overlap_flight {
                        write_under_flight += 1;
                    }
                }
                live_pieces.push((piece, bid));
            }
        }

        // Drain remaining flights and verify every resting piece eventually reclaims.
        for (piece, _) in live_pieces.drain(..) {
            pool.resting.push_back((
                Resting {
                    at: now_ms,
                    begun: started,
                },
                piece,
            ));
        }
        now_ms += SETTLE + REST;
        finished = started + LAG;
        let _ = pool.reclaim_at(now_ms, started, finished, 0);
        assert!(
            pool.resting.is_empty(),
            "all resting pieces reclaim once flights finish (fence_rest={fence_rest})"
        );

        (write_under_flight, drop_under_flight)
    }

    #[test]
    fn randomized_stall_reproduces_write_under_flight_when_off_and_zero_when_on() {
        let (off_writes, off_drops) = run_stall_model(false);
        assert!(
            off_writes > 0 || off_drops > 0,
            "off must reproduce premature reuse/drop under >2.5 s stalls: writes={off_writes} drops={off_drops}"
        );
        let (on_writes, on_drops) = run_stall_model(true);
        assert_eq!(
            (on_writes, on_drops),
            (0, 0),
            "VICTORY_POOL_FENCE_REST=1 must have zero writes or drops under flight"
        );
    }

    /// Randomized model of `maker::fill` / `take_from` where `CreateHeap` +
    /// `MakeResident` takes 15-45 ms while 1-4 submissions begin and earlier
    /// ones finish during `make()`.
    #[test]
    fn randomized_block_make_reproduces_unripe_cut_when_off_and_zero_when_on() {
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                FORCED.with(|f| f.set(None));
            }
        }
        let _reset = Reset;

        let run = |fence_rest: bool| -> usize {
            FORCED.with(|f| f.set(Some(fence_rest)));
            let mut seed = 0xdead_beef_9876_5432u64;
            let mut started = 100u64;
            let mut finished = 99u64;
            let mut premature_ripe = 0usize;

            for _ in 0..500 {
                let before = started;
                // Submissions arrive and earlier ones finish while `CreateHeap + MakeResident` runs.
                let new_subs = 1 + (next_rng(&mut seed) % 4);
                started += new_subs;
                let completed = 1 + (next_rng(&mut seed) % new_subs);
                finished = (finished + completed).min(started - 1);
                let after = started;

                let stamp = made_stamp(before, after);
                // Ripe::is checks `block.made <= finished`; a block is only truly
                // ripe once every submission started by the time `make()` finished has completed.
                let thought_ripe = stamp <= finished;
                let actually_ripe = after <= finished;
                if thought_ripe && !actually_ripe {
                    premature_ripe += 1;
                }
            }
            premature_ripe
        };

        let off_premature = run(false);
        assert!(
            off_premature > 0,
            "off must reproduce premature ripe classification when make() spans submissions"
        );
        let on_premature = run(true);
        assert_eq!(
            on_premature, 0,
            "VICTORY_POOL_FENCE_REST=1 must stamp blocks after make() so none are prematurely ripe"
        );
    }
}
