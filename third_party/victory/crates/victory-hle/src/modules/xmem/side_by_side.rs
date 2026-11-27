//! A run of a scatter changed with its system calls on several threads
//! (`VICTORY_MAP_THREADS`, after cool_fox 565f4a5603 / ac11bbbe5f), and with
//! `VICTORY_MAP_THREADS_ALL=1` also the title's maps of several pages
//! (`XMemMapPhysicalPages`) and the GPU's page-mapping copies
//! (`CopyPageMappings*`: [`super::map_alias`], [`super::map_alias_constant`]).
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

use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::c_void;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::Instant;

use windows_sys::Win32::System::Memory::{
    MEM_PRESERVE_PLACEHOLDER, MEM_RELEASE, MEM_REPLACE_PLACEHOLDER, MEMORY_MAPPED_VIEW_ADDRESS,
    MapViewOfFile3, PAGE_READWRITE, UnmapViewOfFile2, VirtualFree,
};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

use super::{
    PAGE, Physical, SCATTER, Slice, WORK, commit, lean, map, map_pages, noted, remap_same, shows,
    shows_nothing, touched, unmap, unshow,
};

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

/// Whether maps of several pages and page-mapping copies use the threads
/// too (`VICTORY_MAP_THREADS_ALL=1`, with `VICTORY_MAP_THREADS` above 1;
/// off: only scatters do).
pub(super) fn all_on() -> bool {
    #[cfg(test)]
    if let Some(on) = super::tests::THREADS_ALL.with(|t| t.get()) {
        return on && threads() > 1;
    }
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_MAP_THREADS_ALL").is_some_and(|v| v == "1"))
        && threads() > 1
}

/// A system call for one view or page.
enum Op {
    /// Return the view at this address, `len` long, to its placeholder.
    Unmap(usize, usize),
    /// Map `len` bytes of slices from this one on over the placeholder of
    /// exactly that range at the address, and commit them unless all are.
    Map(usize, usize, Slice),
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
                WORK[11].fetch_add(taken.elapsed().as_micros() as u64, Ordering::Relaxed);
                if gone != 0 { DONE } else { NOT_MAPPED }
            }
            Op::Map(at, len, slice) => {
                let (at, len) = (*at, *len);
                let n = len / PAGE;
                // SAFETY: maps the section over the page's own placeholder;
                // acts only on one, failing if it is not exactly that.
                let made = Instant::now();
                let view = unsafe {
                    MapViewOfFile3(
                        slice.section.handle,
                        GetCurrentProcess(),
                        at as *const c_void,
                        (slice.index * PAGE) as u64,
                        len,
                        MEM_REPLACE_PLACEHOLDER,
                        PAGE_READWRITE,
                        std::ptr::null_mut(),
                        0,
                    )
                };
                if view.Value as usize != at {
                    return NOT_MAPPED;
                }
                // The calls' time, summed over the threads (the caller counts
                // the view, and the batch's time as the title waits for it).
                let done = || {
                    WORK[11].fetch_add(made.elapsed().as_micros() as u64, Ordering::Relaxed);
                };
                if slice.section.all_committed(slice.index, n) {
                    done();
                    return DONE_COMMITTED;
                }
                if !commit(at, len) {
                    // SAFETY: the view just mapped, back to a placeholder.
                    unsafe {
                        UnmapViewOfFile2(GetCurrentProcess(), view, MEM_PRESERVE_PLACEHOLDER)
                    };
                    return NOT_COMMITTED;
                }
                slice.section.mark_committed(slice.index, n);
                done();
                DONE
            }
        }
    }
}

/// `[base, base + len)` changed with [`super::PHYSICAL`] and the remapping
/// lock for it held: what is mapped there is taken away, then `maps`
/// (address, bytes, first slice: views inside the range, apart) are mapped.
/// A map whose placeholder turns out not to be its own is done again by
/// `again` (as one after the other does it). False if the host refuses one;
/// what was done is noted either way.
pub(super) fn change(
    physical: &mut Physical,
    base: usize,
    len: usize,
    maps: Vec<(usize, usize, Slice)>,
    again: impl Fn(&mut Physical, usize, usize, Slice) -> bool,
) -> bool {
    if len == 0 {
        return maps.is_empty();
    }
    change_many(physical, &[(base, len)], maps, again)
}

/// [`change`] for several ranges apart (`ranges`: address, bytes; none
/// empty), their calls side by side as one: what is mapped in each is
/// taken away, then `maps` (each inside one of them) are mapped.
pub(super) fn change_many(
    physical: &mut Physical,
    ranges: &[(usize, usize)],
    maps: Vec<(usize, usize, Slice)>,
    again: impl Fn(&mut Physical, usize, usize, Slice) -> bool,
) -> bool {
    let mut ops = Vec::new();
    for &(base, len) in ranges {
        let end = base + len;
        // The views there: whole ones go side by side, what reaches past
        // the range is taken away and mapped again in part by `unmap`, as
        // before.
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
    }
    // The placeholders the views taken away left: each exactly its view.
    let mut own = HashMap::new();
    for (op, how) in all(ops) {
        let Op::Unmap(at, size) = op else { continue };
        if how == DONE {
            own.insert(at, size);
            if lean() {
                physical.holes.insert(at, size);
            }
        }
    }
    let mut ops = Vec::new();
    for (at, len, slice) in maps {
        let hole = physical.holes.remove(&at);
        let inside: Vec<usize> = physical.holes.range(at + 1..at + len).map(|(&at, _)| at).collect();
        for at in inside {
            physical.holes.remove(&at);
        }
        if hole != Some(len) && own.get(&at) != Some(&len) {
            // SAFETY: acts only on placeholders; fails harmlessly when the
            // range is a whole placeholder already or spans several (the
            // map then fails, and `again` has it).
            unsafe {
                VirtualFree(
                    at as *mut c_void,
                    len,
                    MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER,
                )
            };
        } else {
            SCATTER[2].fetch_add(1, Ordering::Relaxed);
        }
        ops.push(Op::Map(at, len, slice));
    }
    let mut ok = true;
    for (op, how) in all(ops) {
        let Op::Map(at, len, slice) = op else { continue };
        match how {
            DONE | DONE_COMMITTED => {
                if how == DONE_COMMITTED {
                    SCATTER[3].fetch_add(1, Ordering::Relaxed);
                }
                noted(physical, at, len, slice, true, Instant::now());
            }
            // The placeholder was not the range's own after all.
            NOT_MAPPED => {
                SCATTER[4].fetch_add(1, Ordering::Relaxed);
                ok &= again(physical, at, len, slice);
            }
            _ => ok = false,
        }
    }
    ok
}

/// `XMemMapPhysicalPagesScatter` with `VICTORY_SCATTER_RUNS` off: what
/// page by page `map_pages` does, a page at each address, with the system
/// calls of every page up to an address given twice (or a page that is not
/// the title's) side by side. A page already showing what is asked is left
/// as `map_pages` leaves it. One remapping lock is held for each such
/// stretch, over all the addresses it touches (from the lowest to the
/// highest): reads of title memory in place there wait for the stretch.
pub(super) fn scatter(
    physical: &mut Physical,
    addresses: &[*mut c_void],
    want: impl Fn(usize) -> Option<usize>,
) -> bool {
    let n = addresses.len();
    let mode = remap_same();
    let mut i = 0;
    while i < n {
        let mut seen = HashSet::new();
        let mut batch: Vec<(usize, Option<Slice>)> = Vec::new();
        let mut j = i;
        let mut unknown = false;
        while j < n {
            let at = addresses[j] as usize;
            let page = want(j);
            let slice = match page {
                Some(p) => match physical.pages.get(&p) {
                    Some(s) => Some(s.clone()),
                    None => {
                        unknown = true;
                        break;
                    }
                },
                None => None,
            };
            if !seen.insert(at) {
                break;
            }
            j += 1;
            WORK[7].fetch_add(1, Ordering::Relaxed);
            if mode != 0 {
                if page.is_some_and(|p| shows(physical, at, &[p])) {
                    WORK[8].fetch_add(1, Ordering::Relaxed);
                    if mode == 1 {
                        victory_gpu::kernel_will_write(at, PAGE);
                    }
                    continue;
                }
                if page.is_none() && shows_nothing(physical, at, PAGE) {
                    WORK[9].fetch_add(1, Ordering::Relaxed);
                    continue;
                }
            }
            batch.push((at, slice));
        }
        if !batch.is_empty() {
            let (lo, hi) = batch.iter().fold((usize::MAX, 0), |(lo, hi), &(at, _)| {
                let (a, b) = touched(physical, at, PAGE);
                (lo.min(a), hi.max(b))
            });
            let _changing = victory_gpu::title_memory_remapping_range(lo, hi - lo);
            let ranges: Vec<(usize, usize)> = batch.iter().map(|&(at, _)| (at, PAGE)).collect();
            let maps = batch.into_iter().filter_map(|(at, s)| Some((at, PAGE, s?))).collect();
            if !change_many(physical, &ranges, maps, map) {
                return false;
            }
        }
        if unknown {
            // As one after the other: it fails, after what came before it.
            let page = want(j);
            return map_pages(physical, addresses[j] as usize, 1, page.as_ref().map(std::slice::from_ref));
        }
        i = j;
    }
    true
}

/// Consecutive slices of one section in `slices` (by page, from `base`),
/// as (address, bytes, first slice): one view each, as `map_pages` makes
/// them. Pages without a slice are left out (unmapped).
pub(super) fn runs(base: usize, slices: &[Option<Slice>]) -> Vec<(usize, usize, Slice)> {
    let mut out = Vec::new();
    let mut k = 0;
    while k < slices.len() {
        let Some(first) = &slices[k] else {
            k += 1;
            continue;
        };
        let run = 1 + slices[k + 1..]
            .iter()
            .zip(1..)
            .take_while(|&(s, j)| {
                s.as_ref().is_some_and(|s| {
                    Arc::ptr_eq(&s.section, &first.section) && s.index == first.index + j
                })
            })
            .count();
        out.push((base + k * PAGE, run * PAGE, first.clone()));
        k += run;
    }
    out
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

/// Runs `ops`, the caller too; each with how it ended. The time the
/// caller waits for them goes to the work line (`WORK[1]` for views taken
/// away, `WORK[3]` for views made), as one after the other would count
/// it; the calls' own time, summed over the threads, to `WORK[11]`. (Each
/// call's own time counted as the views' time made ops that wait for one
/// another in the kernel look several times as costly as they are: batch 1,
/// 0.28 ms an unmap against 0.07 with the threads off, with the title's
/// time in its calls lower.)
fn all(ops: Vec<Op>) -> Vec<(Op, u8)> {
    let unmaps = ops.first().is_some_and(|op| matches!(op, Op::Unmap(..)));
    let began = Instant::now();
    let done = all_untimed(ops);
    WORK[if unmaps { 1 } else { 3 }].fetch_add(began.elapsed().as_micros() as u64, Ordering::Relaxed);
    done
}

fn all_untimed(ops: Vec<Op>) -> Vec<(Op, u8)> {
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
                Op::Map(at, len, slice) => Op::Map(*at, *len, slice.clone()),
            };
            (op, how.load(Ordering::Acquire))
        })
        .collect()
}
