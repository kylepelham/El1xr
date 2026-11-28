//! `VICTORY_UNMAP_LATER=1`: a title call that only takes pages away
//! (`XMemMapPhysicalPages` with no pages, a scatter's page 0, a page-mapping
//! copy from 0) no longer waits for the kernel to unmap its views. Each whole
//! view inside the range is taken out of the page table and told to the GPU
//! side at once, as before, and its `UnmapViewOfFile2` is handed to a thread
//! of its own. An unmap costs 9 µs on a quiet machine and 118-189 µs under
//! load (the section's lock, every core's TLB); the title's streaming threads
//! made thousands a second.
//!
//! Until that thread has unmapped a view, the old pages stay readable there.
//! Nothing of ours reads them (the page table and the GPU side see the range
//! unmapped), and a title does not touch memory it unmapped. Anything that
//! maps there first ([`settle`], from `map_view` and `side_by_side`) takes the
//! view back and unmaps it itself, or waits for the one the thread is doing.
//! The thread unmaps a view only with the remapping lock of its addresses,
//! taken without waiting ([`victory_gpu::title_memory_remapping_try`]): a
//! title thread holding that lock may be waiting for it. Views reaching past
//! the range, or across two of the lock's 16 MB spans, are unmapped as before.

use std::collections::BTreeMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::{Condvar, LazyLock, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use windows_sys::Win32::System::Memory::{
    MEM_PRESERVE_PLACEHOLDER, MEMORY_MAPPED_VIEW_ADDRESS, UnmapViewOfFile2,
};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

use super::{PAGE, Physical, Slice, lean, side_by_side, unmap, unshow};

/// The remapping lock's span (`victory_gpu` watch.rs `SHARD_BITS`).
const SPAN_BITS: u32 = 24;
/// Views waiting past this many are unmapped by the caller, as before.
const MOST_WAITING: usize = 8192;

/// Whether the switch is on (a test's thread may set its own).
pub(super) fn on() -> bool {
    #[cfg(test)]
    if let Some(on) = FORCE.with(|f| f.get()) {
        return on;
    }
    static ON: LazyLock<bool> =
        LazyLock::new(|| std::env::var("VICTORY_UNMAP_LATER").is_ok_and(|v| v.trim() == "1"));
    *ON
}

#[cfg(test)]
thread_local! {
    pub(super) static FORCE: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

#[derive(Default)]
struct Later {
    /// Views to unmap, by address: bytes, and what they show (kept alive).
    waiting: BTreeMap<usize, (usize, Slice)>,
    /// The one the thread is unmapping.
    busy: Option<(usize, usize)>,
}

static LATER: LazyLock<(Mutex<Later>, Condvar)> = LazyLock::new(Default::default);
/// Views handed on and not unmapped yet (the fast path of [`settle`]).
static LEFT: AtomicUsize = AtomicUsize::new(0);

/// Since the last line: [views handed on, unmapped by the thread, its µs, taken back and
/// unmapped by a caller, their µs, the thread's tries that found the lock held].
pub(super) static COUNTS: [AtomicU64; 6] = [const { AtomicU64::new(0) }; 6];

fn later() -> MutexGuard<'static, Later> {
    LATER.0.lock().unwrap_or_else(|e| e.into_inner())
}

fn one_span(at: usize, size: usize) -> bool {
    at >> SPAN_BITS == (at + size - 1) >> SPAN_BITS
}

/// The kernel's unmap of the view at `at`, back to a placeholder.
fn unmap_view(at: usize) -> bool {
    // SAFETY: a view we mapped and nobody maps over before it is gone (`settle`).
    unsafe {
        UnmapViewOfFile2(
            GetCurrentProcess(),
            MEMORY_MAPPED_VIEW_ADDRESS {
                Value: at as *mut c_void,
            },
            MEM_PRESERVE_PLACEHOLDER,
        ) != 0
    }
}

/// [`unmap`] of `[base, base + len)` for a call that maps nothing there
/// afterwards: whole views inside one span are handed on, the rest as before.
pub(super) fn unmap_only(physical: &mut Physical, base: usize, len: usize) {
    hand_on(physical, base, len);
    unmap(physical, base, len);
}

/// The whole views inside `[base, base + len)` and one span, handed on (switch
/// on, `VICTORY_MAP_THREADS` off); what reaches past the range is left.
pub(super) fn hand_on(physical: &mut Physical, base: usize, len: usize) {
    if len == 0 || !on() || side_by_side::threads() > 1 {
        return;
    }
    let end = base.saturating_add(len);
    let whole: Vec<usize> = physical
        .views
        .range(base..end)
        .filter(|&(&at, &(size, _))| at + size <= end && one_span(at, size))
        .map(|(&at, _)| at)
        .collect();
    for at in whole {
        if LEFT.load(Relaxed) >= MOST_WAITING {
            break;
        }
        let Some((size, slice)) = physical.views.remove(&at) else {
            continue;
        };
        // What `unmap` does before its call.
        super::probe::gone(at, size);
        for k in 0..size / PAGE {
            unshow(physical, &slice, k, at + k * PAGE);
        }
        victory_gpu::titlemem::title_range_free(at);
        super::told_gpu(|| victory_gpu::title_memory_remapped(at, size));
        // The placeholder the unmap will leave, exactly the view.
        if lean() {
            physical.holes.insert(at, size);
        }
        super::WORK[0].fetch_add(1, Relaxed);
        COUNTS[0].fetch_add(1, Relaxed);
        LEFT.fetch_add(1, Relaxed);
        let (lock, more) = &*LATER;
        lock.lock()
            .unwrap_or_else(|e| e.into_inner())
            .waiting
            .insert(at, (size, slice));
        more.notify_all();
        started();
    }
}

/// Before anything maps over `[base, base + len)`: every view handed on there
/// is unmapped, by the caller (taken back) or by the thread (waited for).
pub(super) fn settle(base: usize, len: usize) {
    if LEFT.load(Relaxed) == 0 || len == 0 {
        return;
    }
    let end = base.saturating_add(len);
    let meets = |at: usize, size: usize| at < end && at + size > base;
    let (_, done) = &*LATER;
    let mut later = later();
    while later.busy.is_some_and(|(at, size)| meets(at, size)) {
        later = done.wait(later).unwrap_or_else(|e| e.into_inner());
    }
    // Views are at most one span long.
    let from = base.saturating_sub(1 << SPAN_BITS);
    let mine: Vec<usize> = later
        .waiting
        .range(from..end)
        .filter(|&(&at, &(size, _))| meets(at, size))
        .map(|(&at, _)| at)
        .collect();
    let taken: Vec<(usize, Slice)> = mine
        .into_iter()
        .filter_map(|at| later.waiting.remove(&at).map(|(_, s)| (at, s)))
        .collect();
    drop(later);
    for (at, _slice) in taken {
        let began = Instant::now();
        unmap_view(at);
        let us = began.elapsed().as_micros() as u64;
        COUNTS[3].fetch_add(1, Relaxed);
        COUNTS[4].fetch_add(us, Relaxed);
        super::WORK[1].fetch_add(us, Relaxed);
        LEFT.fetch_sub(1, Relaxed);
    }
}

/// Starts the unmapping thread once.
fn started() {
    static STARTED: std::sync::Once = std::sync::Once::new();
    STARTED.call_once(|| {
        let ok = std::thread::Builder::new()
            .name("unmapping".into())
            .spawn(serve)
            .is_ok();
        tracing::info!(
            started = ok,
            "a thread that unmaps the views title calls took away (VICTORY_UNMAP_LATER)"
        );
    });
}

fn serve() {
    let (_, more) = &*LATER;
    let mut later = later();
    loop {
        if later.waiting.is_empty() {
            later = more.wait(later).unwrap_or_else(|e| e.into_inner());
            continue;
        }
        #[cfg(test)]
        if PAUSED.load(Relaxed) > 0 {
            drop(later);
            std::thread::sleep(Duration::from_micros(200));
            later = self::later();
            continue;
        }
        let took = later.waiting.iter().take(64).find_map(|(&at, &(size, _))| {
            victory_gpu::title_memory_remapping_try(at, size).map(|l| (at, l))
        });
        let Some((at, lock)) = took else {
            // Title threads change or read those addresses now: after them.
            COUNTS[5].fetch_add(1, Relaxed);
            drop(later);
            std::thread::sleep(Duration::from_micros(100));
            later = self::later();
            continue;
        };
        let Some((size, slice)) = later.waiting.remove(&at) else {
            continue;
        };
        later.busy = Some((at, size));
        drop(later);
        let began = Instant::now();
        unmap_view(at);
        COUNTS[1].fetch_add(1, Relaxed);
        COUNTS[2].fetch_add(began.elapsed().as_micros() as u64, Relaxed);
        drop(lock);
        drop(slice);
        LEFT.fetch_sub(1, Relaxed);
        later = self::later();
        later.busy = None;
        more.notify_all();
    }
}

/// While above 0 the thread unmaps nothing (tests).
#[cfg(test)]
pub(super) static PAUSED: AtomicUsize = AtomicUsize::new(0);

/// Until every view handed on is unmapped (tests).
#[cfg(test)]
pub(super) fn drain() {
    while LEFT.load(Relaxed) != 0 {
        std::thread::sleep(Duration::from_micros(200));
    }
}

/// The work line's part: counts per second over `secs`.
pub(super) fn line(secs: f64) -> String {
    let c = [0, 1, 2, 3, 4, 5].map(|i| COUNTS[i].swap(0, Relaxed));
    format!(
        "unmaps handed on (VICTORY_UNMAP_LATER): {:.0} ({:.0} unmapped by the unmapping thread in {:.1} ms, {:.0} taken back by a caller in {:.1} ms, {:.0} tries found the lock held), {} waiting",
        c[0] as f64 / secs,
        c[1] as f64 / secs,
        c[2] as f64 / 1e3 / secs,
        c[3] as f64 / secs,
        c[4] as f64 / 1e3 / secs,
        c[5] as f64 / secs,
        LEFT.load(Relaxed),
    )
}

#[cfg(test)]
#[path = "unmap_later_tests.rs"]
mod tests;
