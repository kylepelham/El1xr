//! A run of a scatter changed with its system calls on several threads
//! (`VICTORY_MAP_THREADS`, after cool_fox 565f4a5603 / ac11bbbe5f).
//!
//! Mapping and unmapping a view wait for the process's address-space lock,
//! which every protection change takes too, and our guards make 100,000 of
//! those a second in the world: 0.1 to 0.4 ms a page. One thread stands in
//! that queue once at a time; in cool_fox's bench (60,000 views, five threads
//! changing protections) one thread mapped 1,500 pages a second, eight
//! 30,000.
//!
//! Only the system calls run side by side. What we note of views (the
//! physical state, the GPU side's guards and census, the shown pages) is
//! noted by the caller, with [`super::PHYSICAL`] and the remapping lock held
//! as before: what goes is forgotten before the calls, what comes is noted
//! after them. Placeholders are split by the caller too, before: a thread
//! that maps a piece of one while another cuts it can fail.

use std::collections::{HashSet, VecDeque};
use std::ffi::c_void;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::Instant;

use windows_sys::Win32::System::Memory::{
    MEM_PRESERVE_PLACEHOLDER, MEM_RELEASE, MEM_REPLACE_PLACEHOLDER, MEMORY_MAPPED_VIEW_ADDRESS,
    MapViewOfFile3, PAGE_READWRITE, UnmapViewOfFile2, VirtualFree,
};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

use super::{PAGE, Physical, SCATTER, Slice, WORK, commit, lean, map, noted, unmap, unshow};

/// How many threads make the system calls of one run of a scatter
/// (`VICTORY_MAP_THREADS`, 1 to 32; unset or 1: the caller alone, a page
/// after the other, as before). Needs `VICTORY_SCATTER_RUNS` (on unless 0).
pub(super) fn threads() -> usize {
    #[cfg(test)]
    if let Some(n) = super::tests::THREADS.with(|t| t.get()) {
        return n;
    }
    static N: OnceLock<usize> = OnceLock::new();
    *N.get_or_init(|| {
        std::env::var("VICTORY_MAP_THREADS")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(1)
            .clamp(1, 32)
    })
}

/// A system call for one view or page.
enum Op {
    /// Return the view at this address, `len` long, to its placeholder.
    Unmap(usize, usize),
    /// Map this slice over the page's own placeholder at the address, and
    /// commit it unless all its pages are.
    Map(usize, Slice),
}

/// How an op ended.
const NOT_YET: u8 = 0;
const DONE: u8 = 1;
/// A map over the placeholder failed: nothing changed.
const NOT_MAPPED: u8 = 2;
/// The commit failed: the view went back to a placeholder.
const NOT_COMMITTED: u8 = 3;
/// A map done that committed nothing (its pages were committed).
const DONE_COMMITTED: u8 = 4;

impl Op {
    fn run(&self) -> u8 {
        match self {
            Op::Unmap(at, _) => {
                let taken = Instant::now();
                // SAFETY: a view we mapped, forgotten by the caller; it
                // becomes a placeholder again.
                let gone = unsafe {
                    UnmapViewOfFile2(
                        GetCurrentProcess(),
                        MEMORY_MAPPED_VIEW_ADDRESS {
                            Value: *at as *mut c_void,
                        },
                        MEM_PRESERVE_PLACEHOLDER,
                    )
                };
                WORK[0].fetch_add(1, Ordering::Relaxed);
                WORK[1].fetch_add(taken.elapsed().as_micros() as u64, Ordering::Relaxed);
                if gone != 0 { DONE } else { NOT_MAPPED }
            }
            Op::Map(at, slice) => {
                // SAFETY: maps the section over the page's own placeholder;
                // acts only on one, failing if it is not exactly that.
                let made = Instant::now();
                let view = unsafe {
                    MapViewOfFile3(
                        slice.section.handle,
                        GetCurrentProcess(),
                        *at as *const c_void,
                        (slice.index * PAGE) as u64,
                        PAGE,
                        MEM_REPLACE_PLACEHOLDER,
                        PAGE_READWRITE,
                        std::ptr::null_mut(),
                        0,
                    )
                };
                if view.Value as usize != *at {
                    return NOT_MAPPED;
                }
                // The calls' time, as `Made` counts it in `map_view` (the
                // caller counts the view and its notes).
                let done = || {
                    WORK[3].fetch_add(made.elapsed().as_micros() as u64, Ordering::Relaxed);
                };
                if slice.section.all_committed(slice.index, 1) {
                    done();
                    return DONE_COMMITTED;
                }
                if !commit(*at, PAGE) {
                    // SAFETY: the view just mapped, back to a placeholder.
                    unsafe {
                        UnmapViewOfFile2(GetCurrentProcess(), view, MEM_PRESERVE_PLACEHOLDER)
                    };
                    return NOT_COMMITTED;
                }
                slice.section.mark_committed(slice.index, 1);
                done();
                DONE
            }
        }
    }
}

/// [`super::map_pages`] for `want` at `base`, a run of a scatter in which
/// every page changes (`want[k]`: a page to map, or none to unmap), with
/// [`super::PHYSICAL`] and the remapping lock for the run held. False if a
/// page is not the title's or the host refuses one; what was done is noted
/// either way.
pub(super) fn change(physical: &mut Physical, base: usize, want: &[Option<usize>]) -> bool {
    let end = base + want.len() * PAGE;
    // The views there: whole ones go side by side, what reaches past the
    // run is taken away and mapped again in part by `unmap`, as before.
    let first = physical
        .views
        .range(..=base)
        .next_back()
        .filter(|(at, (size, _))| *at + size > base)
        .map(|(at, _)| *at);
    let hit: Vec<usize> = first
        .into_iter()
        .chain(physical.views.range(base + 1..end).map(|(at, _)| *at))
        .collect();
    let mut ops = Vec::new();
    for at in hit {
        let size = physical.views[&at].0;
        if at < base || at + size > end {
            let (lo, hi) = (base.max(at), end.min(at + size));
            unmap(physical, lo, hi - lo);
            continue;
        }
        let Some((size, slice)) = physical.views.remove(&at) else {
            continue;
        };
        // As `unmap` does before its call.
        for k in 0..size / PAGE {
            unshow(physical, &slice, k, at + k * PAGE);
        }
        victory_gpu::titlemem::title_range_free(at);
        victory_gpu::title_memory_remapped(at, size);
        ops.push(Op::Unmap(at, size));
    }
    let mut own = HashSet::new();
    for (op, how) in all(ops) {
        let Op::Unmap(at, size) = op else { continue };
        if how == DONE {
            if size == PAGE {
                own.insert(at);
            }
            if lean() {
                physical.holes.insert(at, size);
            }
        }
    }
    let mut ok = true;
    let mut ops = Vec::new();
    for (k, page) in want.iter().enumerate() {
        let Some(page) = page else { continue };
        let Some(slice) = physical.pages.get(page).cloned() else {
            tracing::warn!(page, "XMemMapPhysicalPagesScatter: unknown page");
            ok = false;
            break;
        };
        let at = base + k * PAGE;
        if physical.holes.remove(&at) != Some(PAGE) && !own.contains(&at) {
            // SAFETY: acts only on placeholders; fails harmlessly when the
            // page is a whole placeholder already.
            unsafe {
                VirtualFree(
                    at as *mut c_void,
                    PAGE,
                    MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER,
                )
            };
        } else {
            SCATTER[2].fetch_add(1, Ordering::Relaxed);
        }
        ops.push(Op::Map(at, slice));
    }
    for (op, how) in all(ops) {
        let Op::Map(at, slice) = op else { continue };
        match how {
            DONE | DONE_COMMITTED => {
                if how == DONE_COMMITTED {
                    SCATTER[3].fetch_add(1, Ordering::Relaxed);
                }
                noted(physical, at, PAGE, slice, true, Instant::now());
            }
            // The placeholder was not the page's own after all: as one
            // after the other does (split, map).
            NOT_MAPPED => {
                SCATTER[4].fetch_add(1, Ordering::Relaxed);
                ok &= map(physical, at, PAGE, slice);
            }
            _ => ok = false,
        }
    }
    ok
}

/// A batch of ops the threads take one at a time.
struct Batch {
    ops: Vec<(Op, AtomicU8)>,
    next: AtomicUsize,
    /// Ops not done yet.
    left: AtomicUsize,
    over: (Mutex<bool>, Condvar),
}

impl Batch {
    /// Does ops until none is left to begin.
    fn work(&self) {
        loop {
            let i = self.next.fetch_add(1, Ordering::Relaxed);
            let Some((op, how)) = self.ops.get(i) else {
                return;
            };
            how.store(op.run(), Ordering::Release);
            if self.left.fetch_sub(1, Ordering::AcqRel) == 1 {
                *self.over.0.lock().unwrap_or_else(|e| e.into_inner()) = true;
                self.over.1.notify_all();
            }
        }
    }
}

struct Pool {
    batches: Mutex<VecDeque<Arc<Batch>>>,
    more: Condvar,
}

fn pool() -> &'static Pool {
    static POOL: OnceLock<Pool> = OnceLock::new();
    static STARTED: std::sync::Once = std::sync::Once::new();
    let pool = POOL.get_or_init(|| Pool {
        batches: Mutex::default(),
        more: Condvar::new(),
    });
    STARTED.call_once(|| {
        let started = (1..threads())
            .filter(|i| {
                std::thread::Builder::new()
                    .name(format!("mapping {i}"))
                    .spawn(|| serve(pool))
                    .is_ok()
            })
            .count();
        tracing::info!(
            started,
            "threads that map title pages side by side (VICTORY_MAP_THREADS)"
        );
    });
    pool
}

fn serve(pool: &Pool) {
    use windows_sys::Win32::System::Threading::{
        GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL,
    };
    // SAFETY: a plain call on our own thread.
    unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL) };
    let mut batches = pool.batches.lock().unwrap_or_else(|e| e.into_inner());
    loop {
        let Some(batch) = batches.front().cloned() else {
            batches = pool.more.wait(batches).unwrap_or_else(|e| e.into_inner());
            continue;
        };
        if batch.next.load(Ordering::Relaxed) >= batch.ops.len() {
            // All begun: nobody needs to see it any more.
            batches.pop_front();
            continue;
        }
        drop(batches);
        batch.work();
        batches = pool.batches.lock().unwrap_or_else(|e| e.into_inner());
    }
}

/// Runs `ops`, the caller too; each with how it ended.
fn all(ops: Vec<Op>) -> Vec<(Op, u8)> {
    // Waking the others takes longer than a few quick calls.
    if ops.len() < 3 {
        return ops
            .into_iter()
            .map(|op| {
                let how = op.run();
                (op, how)
            })
            .collect();
    }
    let n = ops.len();
    let batch = Arc::new(Batch {
        ops: ops
            .into_iter()
            .map(|op| (op, AtomicU8::new(NOT_YET)))
            .collect(),
        next: AtomicUsize::new(0),
        left: AtomicUsize::new(n),
        over: Default::default(),
    });
    let pool = pool();
    pool.batches
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push_back(batch.clone());
    if n > threads() {
        pool.more.notify_all();
    } else {
        (1..n).for_each(|_| pool.more.notify_one());
    }
    batch.work();
    let mut over = batch.over.0.lock().unwrap_or_else(|e| e.into_inner());
    while !*over {
        over = batch.over.1.wait(over).unwrap_or_else(|e| e.into_inner());
    }
    drop(over);
    // Every op is done; a worker may still hold the batch for a moment, so
    // the ops are taken out of it rather than unwrapped.
    batch
        .ops
        .iter()
        .map(|(op, how)| {
            let op = match op {
                Op::Unmap(at, len) => Op::Unmap(*at, *len),
                Op::Map(at, slice) => Op::Map(*at, slice.clone()),
            };
            (op, how.load(Ordering::Acquire))
        })
        .collect()
}
