//! (worldstream, #26) `VICTORY_TEXTURE_BLOCK_THREAD=1` (with
//! `VICTORY_TEXTURE_HEAPS=1`): the texture pool's 64 MiB blocks are made,
//! and let go, by one thread of their own (`victory-texture-blocks`, below
//! normal, on the background cores), never by a thread that takes a piece.
//! Off (unset or 0): as before.
//!
//! Before, a thread that placed a texture made the block when none had room,
//! and the block made ahead (`VICTORY_RIPE_BLOCKS`) after its cut: a
//! `CreateHeap`, `MakeResident` and a wait for the card to map it, 11-45 ms
//! each (b3 `11_texheaps`: "15 blocks made in 686.2 ms (662.8 ms of it making
//! them resident)" in 5 s while loading), on the recording thread, a prepare
//! thread or a bake thread. A second thread that needed a block meanwhile
//! made a committed texture of its own ("left to committed while a block was
//! made"), the 1 ms allocation the pool is there to avoid. Blocks let go
//! (empty past the spares) were released by the taking thread too.
//!
//! The pool was dropped in batch 1 as one of five pools whose set made
//! every title unmap cost 0.5-0.93 ms (0.07-0.18 without). Alone in batch 3
//! it did not (unmaps 573 ms vs 480-545 for the bases, 76 ms in the last
//! 60 s vs 37-159), and offline the pools' memory did not slow unmaps
//! (texpool round 4). What is left of the suspicion is kernel memory work
//! (heaps made, made resident, released) on the threads that race the
//! title's mapping calls in the frame. Here all of it runs on one idle
//! thread, mostly while the title loads: it keeps
//! `min(VICTORY_RIPE_BLOCKS, spares)` blocks empty ahead (at least 4), so a
//! piece is cut from a block made long before, and the empty blocks the pool
//! keeps stay below what it lets go of, so blocks are not made and released
//! in turn. A take that finds no room wakes the thread and makes a committed
//! texture this once; it never waits.
//!
//! What does not change: a block is released only when every piece of it is
//! back and rested (no live placed texture is ever in one that goes), pieces
//! come back with their texture's last reference, and are cut again after
//! the submissions begun before have finished.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, Once, OnceLock};

use windows::Win32::Graphics::Direct3D12::ID3D12Heap;

use super::{
    Piece, Pool, Ripe, T_BUSY, T_FREE_US, T_FREED, T_MADE, T_MAKE_US, T_PIECES, T_TAKE_US,
    T_WAIT_US, TEXTURES, Which, ahead, lock, now, texture_block, time, time_lines, too_big, us,
};

/// Whether the switch is on (read once).
pub(super) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_TEXTURE_BLOCK_THREAD").is_some_and(|v| v == "1"))
}

/// Empty blocks kept ahead at least (while the card has room for them).
const LEAST: usize = 4;

/// Since the last line: takes that found no room, blocks made on the
/// thread and their microseconds, blocks let go on it.
static STATS: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

/// The thread's mailbox: woken, and blocks to let go.
struct Mail {
    woken: bool,
    gone: Vec<ID3D12Heap>,
}

static MAIL: Mutex<Mail> = Mutex::new(Mail {
    woken: false,
    gone: Vec::new(),
});
static BELL: Condvar = Condvar::new();

/// A piece for a texture of `len` bytes from the texture pool, no block
/// made here ([`take_behind`]).
pub(super) fn take(len: u64, align: u64, ripe: Ripe) -> Option<(Piece, ID3D12Heap, bool)> {
    let want = wanted(&lock(&TEXTURES));
    take_behind(
        &TEXTURES,
        Which::Texture,
        len,
        align,
        ripe,
        want,
        wake,
        later,
    )
}

/// How many empty blocks the thread keeps: [`ahead`] (at least [`LEAST`]),
/// never more than the pool keeps empty now (fewer as the card fills).
fn wanted<B: Clone>(pool: &Pool<B>) -> usize {
    want_of(ahead(), pool.spares())
}

fn want_of(ahead: usize, spares: usize) -> usize {
    ahead.max(LEAST).min(spares)
}

/// Empty blocks in `pool`, as [`Pool::empty`] counts them.
fn empties<B: Clone>(pool: &Pool<B>) -> usize {
    pool.blocks
        .iter()
        .flatten()
        .filter(|b| pool.empty(b))
        .count()
}

/// A piece of `len` bytes of `pool` cut from the blocks there are; the
/// blocks the cut let go of go to `later`, and `wake` is rung when no block
/// had room or fewer than `want` are empty. Nothing waits for a block.
#[allow(clippy::too_many_arguments)]
fn take_behind<B: Clone + 'static>(
    pool: &'static Mutex<Pool<B>>,
    which: Which,
    len: u64,
    align: u64,
    ripe: Ripe,
    want: usize,
    wake: impl Fn(),
    later: impl FnOnce(Vec<B>),
) -> Option<(Piece, B, bool)> {
    if too_big(len, ripe) {
        return None;
    }
    let began = std::time::Instant::now();
    let at = now();
    time_lines(at);
    let mut freed = Vec::new();
    let waited = std::time::Instant::now();
    let mut p = lock(pool);
    time(which, T_WAIT_US, us(waited));
    let taken = p.cut(len, align, at, ripe, &mut freed);
    let low = empties(&p) < want;
    drop(p);
    if !freed.is_empty() {
        later(freed);
    }
    if taken.is_none() || low {
        wake();
    }
    if taken.is_some() {
        time(which, T_PIECES, 1);
    } else {
        time(which, T_BUSY, 1);
        STATS[0].fetch_add(1, Ordering::Relaxed);
    }
    time(which, T_TAKE_US, us(began));
    taken
}

/// Makes blocks for `pool` with `make`, one at a time outside its lock,
/// until `want` are empty or the host refuses one (then none is asked for
/// for a while, `Pool::held_back`): how many were made.
fn fill<B: Clone>(
    pool: &'static Mutex<Pool<B>>,
    which: Which,
    want: usize,
    make: impl Fn() -> Option<B>,
) -> usize {
    let mut n = 0;
    loop {
        {
            let p = lock(pool);
            if empties(&p) >= want || p.held_back(now()) {
                return n;
            }
        }
        let started = crate::host::submissions_started();
        let began = std::time::Instant::now();
        let memory = make();
        let made = us(began);
        let mut p = lock(pool);
        let Some(memory) = memory else {
            p.refused(now());
            return n;
        };
        p.add(memory, started);
        drop(p);
        n += 1;
        time(which, T_MADE, 1);
        time(which, T_MAKE_US, made);
        STATS[1].fetch_add(1, Ordering::Relaxed);
        STATS[2].fetch_add(made, Ordering::Relaxed);
    }
}

/// Rings the thread (started on first use).
fn wake() {
    start();
    lock(&MAIL).woken = true;
    BELL.notify_one();
}

/// Blocks the pool let go of, released on the thread.
fn later(gone: Vec<ID3D12Heap>) {
    start();
    let mut mail = lock(&MAIL);
    mail.gone.extend(gone);
    mail.woken = true;
    drop(mail);
    BELL.notify_one();
}

fn start() {
    static STARTED: Once = Once::new();
    STARTED.call_once(|| {
        let spawned = std::thread::Builder::new().name("victory-texture-blocks".into()).spawn(run);
        match spawned {
            Ok(_) => tracing::info!(
                ahead = ahead(),
                least = LEAST,
                "texture blocks made and let go on a thread of their own (VICTORY_TEXTURE_BLOCK_THREAD=1)"
            ),
            Err(e) => tracing::warn!(error = %e, "the texture block thread could not be started"),
        }
    });
}

fn run() {
    use windows::Win32::System::Threading::{
        GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL,
    };
    crate::topology::pin_background("victory-texture-blocks");
    // SAFETY: plain call on this thread's pseudo handle.
    let _ = unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL) };
    loop {
        let gone = {
            let mut mail = lock(&MAIL);
            if !mail.woken && mail.gone.is_empty() {
                mail = BELL
                    .wait_timeout(mail, std::time::Duration::from_millis(250))
                    .unwrap_or_else(|e| e.into_inner())
                    .0;
            }
            mail.woken = false;
            std::mem::take(&mut mail.gone)
        };
        if !gone.is_empty() {
            let began = std::time::Instant::now();
            let n = gone.len() as u64;
            drop(gone);
            time(Which::Texture, T_FREED, n);
            time(Which::Texture, T_FREE_US, us(began));
            STATS[3].fetch_add(n, Ordering::Relaxed);
        }
        super::refresh_spares(now());
        if let Some(host) = crate::host::host() {
            let want = wanted(&lock(&TEXTURES));
            fill(&TEXTURES, Which::Texture, want, || {
                texture_block(&host.device)
            });
        }
        line();
    }
}

/// Every 5 s (from the thread), a line of [`STATS`] and the pool as it stands.
fn line() {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let at = now();
    if at < NEXT.load(Ordering::Relaxed) {
        return;
    }
    NEXT.store(at + 5000, Ordering::Relaxed);
    let s = STATS.each_ref().map(|s| s.swap(0, Ordering::Relaxed));
    if s.iter().all(|&v| v == 0) {
        return;
    }
    let (empty, alive, want) = {
        let p = lock(&TEXTURES);
        (empties(&p), p.blocks.iter().flatten().count(), wanted(&p))
    };
    tracing::info!(
        "texture block thread (VICTORY_TEXTURE_BLOCK_THREAD=1) over 5 s: {} blocks made in {:.1} ms, {} let go; \
         {} takes found no room (committed this once); now {alive} blocks, {empty} empty, {want} kept empty ahead",
        s[1],
        s[2] as f64 / 1e3,
        s[3],
        s[0],
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_thread_keeps_ahead_blocks_within_the_spares() {
        assert_eq!(want_of(12, 16), 12);
        assert_eq!(want_of(0, 16), LEAST);
        assert_eq!(want_of(12, 2), 2);
        assert_eq!(want_of(40, 16), 16);
    }

    /// A take never makes a block: with no room it rings the thread and
    /// gets nothing; once the thread made blocks it cuts from them, and it
    /// rings again when fewer than `want` are empty.
    #[test]
    fn a_take_never_makes_a_block_and_the_thread_fills_ahead() {
        static ALIVE: AtomicU64 = AtomicU64::new(0);
        static POOL: Mutex<Pool<u64>> = Mutex::new(Pool::new(8, &ALIVE));
        let rung = AtomicU64::new(0);
        let wake = || {
            rung.fetch_add(1, Ordering::Relaxed);
        };
        let taken = take_behind(
            &POOL,
            Which::Upload,
            65536,
            65536,
            Ripe::ALL,
            2,
            wake,
            |_| {},
        );
        assert!(taken.is_none());
        assert_eq!(rung.load(Ordering::Relaxed), 1);
        assert_eq!(lock(&POOL).blocks(), 0, "no block made by the take");
        // The thread: two empty blocks, made outside the lock.
        let made = AtomicU64::new(0);
        let make = || {
            assert!(POOL.try_lock().is_ok());
            Some(made.fetch_add(1, Ordering::Relaxed) + 1)
        };
        assert_eq!(fill(&POOL, Which::Upload, 2, make), 2);
        assert_eq!(fill(&POOL, Which::Upload, 2, make), 0, "enough empty");
        let (piece, block, _) = take_behind(
            &POOL,
            Which::Upload,
            65536,
            65536,
            Ripe::ALL,
            2,
            wake,
            |_| {},
        )
        .unwrap();
        assert_eq!((piece.start, block), (0, 1));
        assert_eq!(
            rung.load(Ordering::Relaxed),
            2,
            "one block left empty: rung"
        );
        assert_eq!(fill(&POOL, Which::Upload, 2, make), 1);
        assert_eq!(lock(&POOL).blocks(), 3);
    }

    /// A refusal stops the filling and holds the next ask back.
    #[test]
    fn a_refusal_stops_the_filling_and_holds_back() {
        static ALIVE: AtomicU64 = AtomicU64::new(0);
        static REFUSING: Mutex<Pool<u64>> = Mutex::new(Pool::new(8, &ALIVE));
        let asked = AtomicU64::new(0);
        let refuse = || {
            asked.fetch_add(1, Ordering::Relaxed);
            None
        };
        assert_eq!(fill(&REFUSING, Which::Upload, 4, refuse), 0);
        assert_eq!(fill(&REFUSING, Which::Upload, 4, refuse), 0);
        assert_eq!(asked.load(Ordering::Relaxed), 1);
    }
}
