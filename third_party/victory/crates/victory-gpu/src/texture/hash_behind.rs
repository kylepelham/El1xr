//! (critpath, #88) The looks for a write the guards missed taken behind the
//! submitting thread (`VICTORY_TEXTURE_HASH_BEHIND=1`, Live, off by default).
//!
//! Every [`super::blind_hash::hash_every`] frames, a guarded texture no write
//! was seen to is read whole and hashed all the same, in case a write went
//! past the guards ([`super::blind_hash`]). That look runs inside the
//! submission's texture checks, which the submitting thread waits for at
//! the next host step, title write, prelude or flush (`run_other` and
//! `batch_flush` on the "preparing breakdown" line). b5 part I settled: 33
//! joins a frame, 204 textures, 4.4 ms of check time on the threads and
//! 3.2 ms of the submitting thread at the joins, of which the looks were
//! 48 textures, 16.8 MB and 3.2 ms; they found no change. Batch 3 saw 0-3
//! changes a run, all while the world streamed in.
//!
//! With the switch such a look (and only that: a seen write, a texture in
//! its window after an unseen write, a movie's picture, a partly mapped one
//! are read at the check as before) is handed to threads of its own and the
//! check goes on as if the texture were unchanged, which it nearly always
//! is. The thread hashes the texture's memory in place under the same
//! mapping lock and compares with the hash the staging buffer was detiled
//! from. If it differs, the texture's next check reads it whole at once, as
//! the look would have, and counts the unseen write (its window of checks
//! every frame starts then). So a missed write shows at most one check
//! later than without the switch, after up to `hash_every` frames before
//! the look came round anyway. Nothing else changes: the same staging
//! bytes and the same uploads, from the same title memory, whenever the
//! guards saw every write.
//!
//! A texture has at most one look under way ([`Behind::Pending`]); a turn
//! that comes while it runs is that look. A result for a staging buffer
//! read since the look began is dropped: that read saw newer memory.
//!
//! Not in the cache key: no shader or pipeline output.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::Instant;

static ON: victory_core::settings::Live =
    victory_core::settings::Live::flag("VICTORY_TEXTURE_HASH_BEHIND");

#[cfg(test)]
thread_local! {
    /// Tests: the switch for calls made on this thread.
    pub(super) static FORCED: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

pub(super) fn on() -> bool {
    #[cfg(test)]
    if let Some(on) = FORCED.with(std::cell::Cell::get) {
        return on;
    }
    ON.get_bool()
}

/// A texture's look behind, kept with its staging buffer's `Last`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum Behind {
    #[default]
    Idle,
    /// A look under way, against the hash the buffer was detiled from.
    Pending(blake3::Hash),
    /// A look found the memory changed, or could not read it in place: the
    /// next check reads the texture whole.
    Found,
}

/// What a guarded texture's check does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Look {
    /// Nothing changed: the check is done.
    Skip,
    /// Read and hash the texture now, as without the switch.
    Read,
    /// Hand the look to the threads ([`queue`]) and treat the texture as
    /// unchanged; its result lands with [`landed`].
    Behind(blake3::Hash),
}

/// The check of a guarded texture: `unwritten`, no write was seen since its
/// last check; `turn`, its look for a write the guards missed is due;
/// `exempt`, it is read at every check (a movie's picture, a window after
/// an unseen write); `can`, a look may run behind (an in-place read, whole
/// hashes only); `hash`, what the buffer was detiled from. With the switch
/// off and no look under way this is the check as it was: skip only when
/// unwritten, exempt from nothing, and not the turn.
pub(super) fn look(
    behind: &mut Behind,
    on: bool,
    unwritten: bool,
    turn: bool,
    exempt: bool,
    can: bool,
    hash: Option<blake3::Hash>,
) -> Look {
    if *behind == Behind::Found {
        *behind = Behind::Idle;
        return Look::Read;
    }
    if !unwritten || exempt {
        return Look::Read;
    }
    if !turn {
        return Look::Skip;
    }
    match hash {
        Some(_) if matches!(behind, Behind::Pending(_)) && on && can => Look::Skip,
        Some(hash) if on && can => {
            *behind = Behind::Pending(hash);
            STATS[QUEUED].fetch_add(1, Relaxed);
            Look::Behind(hash)
        }
        _ => Look::Read,
    }
}

/// What a look found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Seen {
    Same,
    Changed,
    /// Not readable in place now (partly mapped, or being remapped).
    Unread,
}

/// A look against `expect` came back with `seen`; `hash` is the hash the
/// buffer holds now. Returns whether the result stands (the buffer was not
/// read since the look began).
pub(super) fn landed(
    behind: &mut Behind,
    hash: Option<blake3::Hash>,
    expect: blake3::Hash,
    seen: Seen,
) -> bool {
    if *behind != Behind::Pending(expect) {
        STATS[DROPPED].fetch_add(1, Relaxed);
        return false;
    }
    if hash != Some(expect) {
        *behind = Behind::Idle;
        STATS[DROPPED].fetch_add(1, Relaxed);
        return false;
    }
    *behind = match seen {
        Seen::Same => Behind::Idle,
        Seen::Changed => {
            STATS[FOUND].fetch_add(1, Relaxed);
            Behind::Found
        }
        Seen::Unread => {
            STATS[UNREAD].fetch_add(1, Relaxed);
            Behind::Found
        }
    };
    true
}

/// `tiled`, the texture's memory read in place, against `expect`; timed.
pub(super) fn hash_against(tiled: &[u8], expect: blake3::Hash) -> Seen {
    let at = Instant::now();
    let seen = if blake3::hash(tiled) == expect {
        Seen::Same
    } else {
        Seen::Changed
    };
    STATS[DONE].fetch_add(1, Relaxed);
    STATS[BYTES].fetch_add(tiled.len() as u64, Relaxed);
    STATS[NS].fetch_add(at.elapsed().as_nanos() as u64, Relaxed);
    seen
}

const QUEUED: usize = 0;
const DONE: usize = 1;
const BYTES: usize = 2;
const NS: usize = 3;
const FOUND: usize = 4;
const UNREAD: usize = 5;
const DROPPED: usize = 6;
const SLOTS: usize = 7;

static STATS: [AtomicU64; SLOTS] = [const { AtomicU64::new(0) }; SLOTS];

/// The threads the looks run on: few, the looks are a few megabytes a
/// frame and nothing waits for them.
const THREADS: usize = 2;

type Job = Box<dyn FnOnce() + Send>;

struct Pool {
    state: Mutex<(VecDeque<Job>, usize)>,
    ready: Condvar,
    idle: Condvar,
}

fn pool() -> &'static Pool {
    static POOL: OnceLock<&'static Pool> = OnceLock::new();
    POOL.get_or_init(|| {
        let pool: &'static Pool = Box::leak(Box::new(Pool {
            state: Mutex::new((VecDeque::new(), 0)),
            ready: Condvar::new(),
            idle: Condvar::new(),
        }));
        for i in 0..THREADS {
            let spawned = std::thread::Builder::new()
                .name(format!("victory-hash-behind-{i}"))
                .spawn(move || work(pool));
            if let Err(e) = spawned {
                tracing::warn!("a texture look thread did not start ({e})");
            }
        }
        pool
    })
}

fn work(pool: &'static Pool) {
    loop {
        let job = {
            let mut state = pool.state.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if let Some(job) = state.0.pop_front() {
                    state.1 += 1;
                    break job;
                }
                state = pool.ready.wait(state).unwrap_or_else(|e| e.into_inner());
            }
        };
        job();
        let mut state = pool.state.lock().unwrap_or_else(|e| e.into_inner());
        state.1 -= 1;
        if state.0.is_empty() && state.1 == 0 {
            pool.idle.notify_all();
        }
    }
}

/// Runs `job` on a look thread.
pub(super) fn queue(job: impl FnOnce() + Send + 'static) {
    let pool = pool();
    pool.state
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .0
        .push_back(Box::new(job));
    pool.ready.notify_one();
}

/// Tests: returns once every queued look has run.
#[cfg(test)]
pub(super) fn wait_idle() {
    let pool = pool();
    let mut state = pool.state.lock().unwrap_or_else(|e| e.into_inner());
    while !(state.0.is_empty() && state.1 == 0) {
        state = pool.idle.wait(state).unwrap_or_else(|e| e.into_inner());
    }
}

fn line(stats: [u64; SLOTS], frames: f64, on: bool) -> Option<String> {
    let [queued, done, bytes, ns, found, unread, dropped] = stats;
    if queued + done + dropped == 0 || frames <= 0.0 {
        return None;
    }
    Some(format!(
        "texture looks for unseen writes behind the submitting thread per frame (VICTORY_TEXTURE_HASH_BEHIND={}): \
         {:.1} queued, {:.1} hashed, {:.1} MB, {:.2} ms on their threads; {found} found changed, {unread} not readable \
         in place, both read at the texture's next check; {dropped} dropped (the texture was read since) (since the last line)",
        u8::from(on),
        queued as f64 / frames,
        done as f64 / frames,
        bytes as f64 / (1024.0 * 1024.0) / frames,
        ns as f64 / 1e6 / frames,
    ))
}

/// Logs [`STATS`] on the frame-meter cadence and starts them over.
pub(super) fn log(frames: f64) {
    let stats = std::array::from_fn(|i| STATS[i].swap(0, Relaxed));
    if let Some(line) = line(stats, frames, on()) {
        tracing::info!("{line}");
    }
}

#[cfg(test)]
#[path = "hash_behind_tests.rs"]
mod tests;
