//! (feedcard) Guards that open one 4 KiB page of a 64 KiB block (`VICTORY_GUARD_4K=1`; off by
//! default; with `VICTORY_PORT_GUARD=1` and `VICTORY_WATCH_4K=1`, as the tuned set has them).
//!
//! View memory cannot be write-watched, so it is guarded: made read-only, and a write fault opens
//! the 64 KiB block it hit. Once open, later writes elsewhere in the block are not seen, so the
//! next refresh takes the whole block: the copy profile (round copy137mb) found 55.5 MB a frame,
//! 63% of the bytes refreshed, taken whole for that reason.
//!
//! With the switch a fault in a block that is not aliased opens only its 4 KiB page; the block
//! stays protected, the page is noted open here ([`OPEN`]). The guard's next check of the block
//! makes those pages read-only again and notes them written as 4 KiB pages (`found_set`), as the
//! kernel's write watch does for private memory, so the refresh copies only them. The block counts
//! written whole as before unless every reason it was not clean is a page opened here: the first
//! page opened while it was clean sets [`ONLY`], and anything else that makes it not clean
//! (`clean_set` to false) takes [`ONLY`] away.
//!
//! A write is not missed: an open page is always noted (its bit is set before it is made
//! writable, under the block's claim, which the check takes too), and the check makes the page
//! read-only before it notes it, so a write after that faults again. Where a page cannot be made
//! writable alone, the fault opens the whole block as before; where it cannot be made read-only
//! again, the block stays not clean and written whole.

use std::sync::OnceLock;
use std::sync::atomic::Ordering::{AcqRel, Acquire};

use super::{BLOCK, Guards, KERNEL_PAGE, Part, clean_set, part_of, writable};

/// Bits 0-15: the 4 KiB pages of the block open; [`ONLY`]: nothing else made it not clean.
static OPEN: [std::sync::atomic::AtomicPtr<Part>; 1 << 16] =
    [const { std::sync::atomic::AtomicPtr::new(std::ptr::null_mut()) }; 1 << 16];

/// Since the last line: pages opened alone, blocks closed noted at their pages alone, blocks
/// closed and written whole too, pages that could not be made read-only again; with
/// [`keep_for`], pages left open and noted written, kept pages closed by a probe.
static COUNTS: [std::sync::atomic::AtomicU64; 6] = [const { std::sync::atomic::AtomicU64::new(0) }; 6];

fn count(what: usize, n: u64) {
    COUNTS[what].fetch_add(n, std::sync::atomic::Ordering::Relaxed);
}

/// With the guard line.
pub(super) fn line(frames: f64) {
    if !on() || frames <= 0.0 {
        return;
    }
    let [opened, alone, whole, failed, kept, probed] =
        [0, 1, 2, 3, 4, 5].map(|i| COUNTS[i].swap(0, std::sync::atomic::Ordering::Relaxed) as f64 / frames);
    tracing::info!(
        "4 KiB guards per frame (VICTORY_GUARD_4K=1): {opened:.0} pages opened alone; checks noted {alone:.0} blocks \
         at their 4 KiB pages alone and {whole:.0} written whole too; {failed:.1} pages not made read-only again; \
         {kept:.0} pages left open and noted written, {probed:.0} of them closed to see (VICTORY_GUARD_4K_KEEP={})",
        keep_for()
    );
}

const SUBS: u64 = (1 << (BLOCK / KERNEL_PAGE)) - 1;
const ONLY: u64 = 1 << 62;
/// With [`keep_for`]: bits 16-31 the pages the last check closed (written in the interval before
/// it), bits 32-47 the pages it left open, bits 48-55 how many checks in a row left pages open.
const PREV: u32 = 16;
const KEPT: u32 = 32;
const AGE: u32 = 48;

/// `VICTORY_GUARD_4K=1`, with the shared guards and 4 KiB stamps it needs.
pub(super) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    #[cfg(test)]
    if FORCED.with(std::cell::Cell::get) {
        return true;
    }
    *ON.get_or_init(|| {
        std::env::var_os("VICTORY_GUARD_4K").is_some_and(|v| v == "1")
            && super::port_guard()
            && super::watch_4k()
    })
}

#[cfg(test)]
thread_local! {
    pub(super) static FORCED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Blocks `[from, to)` were made read-only whole by the guards (their claims held): room for
/// their open pages, made here (never in the fault handler, which must not allocate), and the
/// pages noted open before forgotten. Those were opened while the block was protected; since then
/// it lost its protection another way (opened whole: a copy-back of ours, a remap, an alias), so
/// what they wrote is taken whole already, and they are read-only again now. Left noted, the next
/// write fault on one would find it "open already" and not handle it (batch 1: the title's write
/// to a read-only page, unhandled).
pub(super) fn ready(from: usize, to: usize) {
    if !on() {
        return;
    }
    let mut block = from;
    while block < to {
        let end = ((block / super::PART + 1) * super::PART).min(to);
        if let Some(part) = part_of(&OPEN, block, true) {
            for b in block..end {
                part.0[b % super::PART].store(0, std::sync::atomic::Ordering::Release);
            }
        }
        block = end;
    }
}

/// [`clean_set`] made `block` not clean: something other than a page opened here may have
/// written it.
pub(super) fn not_clean(block: usize) {
    if let Some(part) = part_of(&OPEN, block, false) {
        part.0[block % super::PART].fetch_and(!ONLY, AcqRel);
    }
}

/// A write fault at `at` in `block`, protected and not aliased, its claim held: opens the 4 KiB
/// page alone. `None`: not done, open the block as before; else what the handler returns.
pub(super) fn fault(g: &Guards, block: usize, at: usize) -> Option<(usize, bool)> {
    let cell = &part_of(&OPEN, block, false)?.0[block % super::PART];
    let bit = 1u64 << ((at % BLOCK) / KERNEL_PAGE);
    if cell.load(Acquire) & bit != 0 && writable(at) {
        // Open already (another thread's fault opened it first): the fault is not ours. Noted
        // open and still read-only (a check that could not make the whole run read-only again
        // left it so): opened again below; it stays noted, so the next check takes it.
        return Some((0, true));
    }
    let was_clean = g.clean.contains(block);
    if was_clean {
        g.clean.remove(block);
        // Takes `ONLY` away, put back just below.
        clean_set(block, false);
        super::published_set(block, 0);
        cell.fetch_or(bit | ONLY, AcqRel);
    } else {
        cell.fetch_or(bit, AcqRel);
    }
    if !protect_page(
        at / KERNEL_PAGE * KERNEL_PAGE,
        windows_sys::Win32::System::Memory::PAGE_READWRITE,
    ) {
        // Left noted open: the block's check protects and notes it, harmless.
        return None;
    }
    super::port6_count(super::P6_FAULTS, 1);
    count(0, 1);
    Some((1, true))
}

/// What the check of a protected, not clean block found of its open pages.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Closed {
    /// None open.
    None,
    /// Made read-only again and noted written at the 4 KiB pages; `only`: nothing else made the
    /// block not clean, so it need not count written whole.
    Pages { only: bool },
    /// A page could not be made read-only again: the block stays not clean, written whole.
    Failed,
    /// ([`keep_for`]) As `Pages { only: true }`, but pages written in two intervals in a row are
    /// left open and noted written: the block stays not clean, so the next check notes them again.
    Kept,
}

/// The guard's check of `block` (protected, not clean, its claim held): its open pages made
/// read-only and noted written at asking `now()`.
pub(super) fn close(block: usize, now: &mut dyn FnMut() -> u64) -> Closed {
    let Some(part) = part_of(&OPEN, block, false) else {
        return Closed::None;
    };
    let cell = &part.0[block % super::PART];
    let most = keep_for();
    if most > 0 {
        return close_keeping(cell, block, most, now);
    }
    // (Kept pages of a switch turned off since are in `pages` too: closed here with the rest.)
    let taken = cell.swap(0, AcqRel);
    let pages = taken & SUBS;
    if pages == 0 {
        return Closed::None;
    }
    let base = block * BLOCK;
    for (run, end) in close_runs(pages, join()) {
        let (from, len) = (base + run * KERNEL_PAGE, (end - run) * KERNEL_PAGE);
        // (#88) Timed apart from the block guards' calls: the split line's 4 KiB part.
        let t = crate::refresh_prof::start();
        let done = protect_run(from, len, windows_sys::Win32::System::Memory::PAGE_READONLY);
        crate::refresh_prof::add(crate::refresh_prof::P::Protect4kCalls, 1);
        crate::refresh_prof::finish(crate::refresh_prof::P::Protect4kUs, t);
        if !done {
            // Noted open again, and the block written whole.
            cell.fetch_or(pages, AcqRel);
            count(3, 1);
            return Closed::Failed;
        }
    }
    let asked = now();
    for k in 0..BLOCK / KERNEL_PAGE {
        if pages & (1 << k) != 0 {
            super::found_set(base + k * KERNEL_PAGE, asked);
        }
    }
    let only = taken & ONLY != 0;
    count(if only { 1 } else { 2 }, 1);
    Closed::Pages { only }
}

/// (feedcard, #88) `VICTORY_GUARD_4K_KEEP=N` (Live; 0, off, by default): a 4 KiB page written in
/// two check intervals in a row is left open by the check, and noted written at every check after,
/// for up to `N` checks; then a probe closes it, and it faults again if still written.
///
/// Why: b5g's FLUSH_PROF split put the 4 KiB guards at 533 `VirtualProtect` closes, 4.9 ms a
/// frame, and the faults that open them again at 474 opens (1.3 ms, plus the exceptions on the
/// title's own threads): the same pages, written every frame (constants, per-frame data), closed
/// and opened again every frame. A kept page costs its 4 KiB copy at each refresh instead (well
/// under a microsecond against about 9 us a close and as much again for the fault).
///
/// A write is not missed: a kept page stays noted open, its block is not made clean (so its piece's
/// next refresh asks, and the walk checks it again), and every check notes it written at its own
/// asking, after which the copy is taken; a write after the check is noted by the next one. Only a
/// block whose sole reason not to be clean is its open pages ([`ONLY`]) keeps any; otherwise all
/// are closed and the block counts written whole, as before. Anything that makes the block
/// read-only whole again forgets the kept pages with the rest ([`ready`]). Not in the cache key: no
/// shader or pipeline output.
static KEEP: victory_core::settings::Live =
    victory_core::settings::Live::new("VICTORY_GUARD_4K_KEEP", |v| {
        v.and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(0).min(255)
    });

pub(super) fn keep_for() -> u64 {
    #[cfg(test)]
    if let Some(forced) = KEEP_FORCED.with(std::cell::Cell::get) {
        return forced;
    }
    KEEP.get()
}

#[cfg(test)]
thread_local! {
    static KEEP_FORCED: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
}

/// Which open pages a check keeps open and which it closes: `(keep, close)`. `pages` open now,
/// `kept` left open by the last check, `prev` closed by it, `age` checks in a row that kept any.
fn keep_split(pages: u64, kept: u64, prev: u64, age: u64, only: bool, most: u64) -> (u64, u64) {
    if !only || age + 1 >= most {
        return (0, pages);
    }
    let keep = (kept | (pages & prev)) & pages;
    (keep, pages & !keep)
}

/// [`close`] with [`keep_for`] at `most`.
fn close_keeping(
    cell: &std::sync::atomic::AtomicU64,
    block: usize,
    most: u64,
    now: &mut dyn FnMut() -> u64,
) -> Closed {
    // The claim is held, so no fault changes the cell meanwhile; `not_clean` may take [`ONLY`].
    let (pages, keep, close, only, probed) = loop {
        let taken = cell.load(Acquire);
        let pages = taken & SUBS;
        if pages == 0 {
            return Closed::None;
        }
        let kept = (taken >> KEPT) & SUBS;
        let (prev, age, only) = ((taken >> PREV) & SUBS, (taken >> AGE) & 0xff, taken & ONLY != 0);
        let (keep, close) = keep_split(pages, kept, prev, age, only, most);
        let new = if keep != 0 {
            keep | (close << PREV) | (keep << KEPT) | ((age + 1) << AGE) | ONLY
        } else {
            close << PREV
        };
        if cell.compare_exchange(taken, new, AcqRel, Acquire).is_ok() {
            break (pages, keep, close, only, kept & close);
        }
    };
    let base = block * BLOCK;
    for (run, end) in close_runs(close, keep == 0 && join()) {
        let (from, len) = (base + run * KERNEL_PAGE, (end - run) * KERNEL_PAGE);
        let t = crate::refresh_prof::start();
        let done = protect_run(from, len, windows_sys::Win32::System::Memory::PAGE_READONLY);
        crate::refresh_prof::add(crate::refresh_prof::P::Protect4kCalls, 1);
        crate::refresh_prof::finish(crate::refresh_prof::P::Protect4kUs, t);
        if !done {
            cell.fetch_or(close, AcqRel);
            count(3, 1);
            return Closed::Failed;
        }
    }
    let asked = now();
    for k in 0..BLOCK / KERNEL_PAGE {
        if pages & (1 << k) != 0 {
            super::found_set(base + k * KERNEL_PAGE, asked);
        }
    }
    count(4, u64::from(keep.count_ones()));
    count(5, u64::from(probed.count_ones()));
    if keep != 0 {
        count(1, 1);
        return Closed::Kept;
    }
    count(if only { 1 } else { 2 }, 1);
    Closed::Pages { only }
}

/// (feedcard, #88) `VICTORY_GUARD_4K_JOIN=1` (Live; off by default): the check makes a block's
/// open pages read-only in one `VirtualProtect`, from the first open page to the end of the last,
/// not one call per run of open pages. The pages between the runs are read-only already (the block
/// is protected, its claim is held, and only [`fault`] makes a page of it writable, noting it
/// first), so the protection after the call is the same; only the number of calls (each a trip
/// under the process's address-space lock and, for a cut in rights, a TLB flush on every core
/// running the title) goes down. Not in the cache key: no shader or pipeline output.
static JOIN: victory_core::settings::Live =
    victory_core::settings::Live::new("VICTORY_GUARD_4K_JOIN", |v| u64::from(v == Some("1")));

pub(super) fn join() -> bool {
    #[cfg(test)]
    if let Some(forced) = JOIN_FORCED.with(std::cell::Cell::get) {
        return forced;
    }
    JOIN.get_bool()
}

#[cfg(test)]
thread_local! {
    static JOIN_FORCED: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

/// (feedcard, #88) `VICTORY_GUARD_STAMP_FRESH=1` (Live; off by default): with `VICTORY_GUARD_4K`
/// a guard walk stamps each block's closed pages with a fresh asking, taken after they are
/// read-only again, not with the one asking it took at its first block. That one predates a later
/// block's close, so a refresh that copied such a page at an asking in between saw a stamp no
/// newer than its own and never took a write made before the close: a stale copy until the page
/// was written again (racing-refresh test: 10-40 of 300 rounds stale when off, none on). A
/// suspect for the one-frame dark flicker beside the texture prepare threads. Costs one atomic
/// increment per closed block. Not in the cache key: no shader or pipeline output.
static STAMP_FRESH: victory_core::settings::Live = victory_core::settings::Live::flag("VICTORY_GUARD_STAMP_FRESH");

pub(super) fn stamp_fresh() -> bool {
    #[cfg(test)]
    if let Some(forced) = STAMP_FRESH_FORCED.with(std::cell::Cell::get) {
        return forced;
    }
    STAMP_FRESH.get_bool()
}

#[cfg(test)]
thread_local! {
    pub(super) static STAMP_FRESH_FORCED: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

/// The 4 KiB page ranges `[a, b)` of a block whose open pages are `pages` that the check makes
/// read-only: each run of open pages, or with `join` one range over all of them.
fn close_runs(pages: u64, join: bool) -> Vec<(usize, usize)> {
    let mut runs = Vec::new();
    if pages == 0 {
        return runs;
    }
    if join {
        let first = pages.trailing_zeros() as usize;
        let last = 63 - pages.leading_zeros() as usize;
        runs.push((first, last + 1));
        return runs;
    }
    let mut k = 0;
    while k < BLOCK / KERNEL_PAGE {
        if pages & (1 << k) == 0 {
            k += 1;
            continue;
        }
        let run = k;
        while k < BLOCK / KERNEL_PAGE && pages & (1 << k) != 0 {
            k += 1;
        }
        runs.push((run, k));
    }
    runs
}

fn protect_page(at: usize, flags: u32) -> bool {
    protect_run(at, KERNEL_PAGE, flags)
}

fn protect_run(at: usize, len: usize, flags: u32) -> bool {
    let mut old = 0;
    // SAFETY: title view memory inside one guarded block; only its protection changes.
    unsafe {
        windows_sys::Win32::System::Memory::VirtualProtect(at as *const _, len, flags, &mut old)
            != 0
    }
}

/// The open pages of `block` (bits) and whether [`ONLY`] is set, for tests.
#[cfg(test)]
pub(super) fn open_of(block: usize) -> (u64, bool) {
    // (The kept-page state above the open bits is not part of the answer.)
    part_of(&OPEN, block, false).map_or((0, false), |p| {
        let v = p.0[block % super::PART].load(Acquire);
        (v & SUBS, v & ONLY != 0)
    })
}

#[cfg(test)]
mod tests {
    use super::super::{
        BLOCK, KERNEL_PAGE, PAGE, VIEWS, clean_set, guard_shared, install, title_memory_remapped,
        written_in,
    };
    use super::{FORCED, JOIN_FORCED, KEEP_FORCED, close_runs, keep_split, open_of};

    /// A fresh view of a pagefile section, `bytes` long, taken for title memory.
    fn view(bytes: usize) -> (usize, usize) {
        use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, FILE_MAP_WRITE, MapViewOfFile, PAGE_READWRITE,
        };
        // SAFETY: a fresh pagefile section mapped once; dropped by `drop_view`.
        unsafe {
            let section = CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                bytes as u32,
                std::ptr::null(),
            );
            assert!(!section.is_null());
            let at = MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, bytes).Value as usize;
            assert_ne!(at, 0);
            crate::inplace::title_memory_view(at, bytes, section as usize, 0);
            (section as usize, at)
        }
    }

    fn drop_view(section: usize, at: usize, bytes: usize) {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Memory::{MEMORY_MAPPED_VIEW_ADDRESS, UnmapViewOfFile};
        title_memory_remapped(at, bytes);
        // SAFETY: the view and section made by `view`.
        unsafe {
            UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: at as *mut _ });
            CloseHandle(section as *mut std::ffi::c_void);
        }
    }

    fn check(first: usize, last: usize) -> Vec<usize> {
        VIEWS.set_range(first, last);
        guard_shared(first, last, 0, crate::queue::presented(), false, true, false).0
    }

    fn stamp(block: usize) -> u64 {
        let page = block * BLOCK / PAGE;
        written_in(page, page + 1).first().map_or(0, |&(_, s)| s)
    }

    /// SAFETY: `at` is inside a live view.
    unsafe fn poke(at: usize) {
        unsafe { (at as *mut u8).write_volatile(7) };
    }

    #[test]
    fn a_page_noted_open_before_the_block_was_opened_whole_and_guarded_again_faults_and_is_seen() {
        // Batch 1's crash: page 3 opened alone, then the block opened whole by a write through
        // the kernel (`writing`, the title's file read) and guarded again by the next check. Page 3 was still noted open,
        // so the title's next write to it, read-only again, was taken for another thread's and
        // left unhandled.
        const SIZE: usize = 2 * BLOCK;
        FORCED.with(|f| f.set(true));
        install();
        let (section, at) = view(SIZE);
        let (first, last) = (at / BLOCK, (at + SIZE) / BLOCK);
        let b = first + 1;
        let page3 = at + BLOCK + 3 * KERNEL_PAGE;
        check(first, last);
        check(first, last);
        // SAFETY: the view, guarded: the handler opens the 4 KiB page written.
        unsafe { poke(page3) };
        assert_eq!(open_of(b), (1 << 3, true));
        super::super::writing(at + BLOCK + 9 * KERNEL_PAGE, 16);
        assert_eq!(check(first, last), vec![b], "opened whole: written whole");
        assert_eq!(open_of(b), (0, false), "guarded again whole: nothing noted open");
        let before = stamp(b);
        // SAFETY: as above; without the fix this write is not handled and the test process dies.
        unsafe { poke(page3) };
        assert_eq!(open_of(b), (1 << 3, true));
        assert_eq!(check(first, last), Vec::<usize>::new());
        assert!(stamp(b) > before, "the write is noted at its 4 KiB page");
        FORCED.with(|f| f.set(false));
        drop_view(section, at, SIZE);
    }

    #[test]
    fn a_page_noted_open_but_read_only_is_opened_again_on_its_fault() {
        // A check that could not make every open run read-only again leaves pages it did make
        // read-only noted open (`Closed::Failed`): their next write must still be handled.
        const SIZE: usize = 2 * BLOCK;
        FORCED.with(|f| f.set(true));
        install();
        let (section, at) = view(SIZE);
        let (first, last) = (at / BLOCK, (at + SIZE) / BLOCK);
        let b = first + 1;
        let page5 = at + BLOCK + 5 * KERNEL_PAGE;
        check(first, last);
        check(first, last);
        // SAFETY: as in the other tests.
        unsafe { poke(page5) };
        assert!(super::super::writable(page5));
        assert!(super::protect_page(page5, windows_sys::Win32::System::Memory::PAGE_READONLY));
        assert_eq!(open_of(b), (1 << 5, true), "noted open, read-only");
        // SAFETY: as above; handled, or the test process dies.
        unsafe { poke(page5) };
        assert!(super::super::writable(page5));
        assert_eq!(check(first, last), Vec::<usize>::new());
        assert_eq!(open_of(b), (0, false));
        FORCED.with(|f| f.set(false));
        drop_view(section, at, SIZE);
    }

    #[test]
    fn a_fault_opens_its_4k_page_alone_and_the_check_closes_and_notes_it() {
        const SIZE: usize = 2 * BLOCK;
        FORCED.with(|f| f.set(true));
        install();
        let (section, at) = view(SIZE);
        let (first, last) = (at / BLOCK, (at + SIZE) / BLOCK);
        let b = first + 1;
        assert_eq!(check(first, last), vec![first, b], "the first check takes it in whole");
        assert_eq!(check(first, last), Vec::<usize>::new(), "nothing written yet");
        // SAFETY: the view, guarded: the handler opens the 4 KiB pages written.
        unsafe {
            poke(at + BLOCK + 3 * KERNEL_PAGE + 8);
            poke(at + BLOCK + 3 * KERNEL_PAGE + 9);
            poke(at + BLOCK + 5 * KERNEL_PAGE);
        }
        assert_eq!(open_of(b), ((1 << 3) | (1 << 5), true));
        let before = stamp(b);
        assert_eq!(check(first, last), Vec::<usize>::new(), "not written whole");
        assert!(stamp(b) > before, "its 4 KiB pages are noted written");
        assert_eq!(open_of(b), (0, false));
        // Read-only again: the next write faults and is noted again.
        // SAFETY: as above.
        unsafe { poke(at + BLOCK + 3 * KERNEL_PAGE) };
        assert_eq!(open_of(b), (1 << 3, true));
        // Made not clean another way meanwhile: written whole, the open page closed too.
        clean_set(b, false);
        assert_eq!(open_of(b), (1 << 3, false));
        assert_eq!(check(first, last), vec![b]);
        assert_eq!(open_of(b), (0, false));
        // SAFETY: as above.
        unsafe { poke(at + BLOCK + 3 * KERNEL_PAGE) };
        assert_eq!(open_of(b), (1 << 3, true), "closed again after the whole check");
        FORCED.with(|f| f.set(false));
        drop_view(section, at, SIZE);
    }
    #[test]
    fn joined_closing_is_one_range_over_the_runs() {
        assert_eq!(close_runs(0, false), Vec::<(usize, usize)>::new());
        assert_eq!(close_runs(0, true), Vec::<(usize, usize)>::new());
        let pages = (1 << 1) | (1 << 2) | (1 << 5) | (1 << 9) | (1 << 15);
        assert_eq!(close_runs(pages, false), vec![(1, 3), (5, 6), (9, 10), (15, 16)]);
        assert_eq!(close_runs(pages, true), vec![(1, 16)]);
        assert_eq!(close_runs(1 << 4, true), vec![(4, 5)]);
        assert_eq!(close_runs(0xffff, false), vec![(0, 16)]);
        assert_eq!(close_runs(0xffff, true), vec![(0, 16)]);
    }

    /// Protection of every 4 KiB page of the block at `at`: read-only or not.
    fn read_only_pages(at: usize) -> Vec<bool> {
        (0..BLOCK / KERNEL_PAGE).map(|k| !super::super::writable(at + k * KERNEL_PAGE)).collect()
    }

    #[test]
    fn a_joined_check_leaves_the_same_pages_read_only_and_noted_as_runs_do() {
        // The same writes checked with and without the switch: the same blocks written, the block
        // noted written, every page read-only again, and the next write faults again.
        const SIZE: usize = 2 * BLOCK;
        FORCED.with(|f| f.set(true));
        install();
        let mut seen = Vec::new();
        for joined in [false, true] {
            JOIN_FORCED.with(|f| f.set(Some(joined)));
            let (section, at) = view(SIZE);
            let (first, last) = (at / BLOCK, (at + SIZE) / BLOCK);
            let b = first + 1;
            let block = at + BLOCK;
            check(first, last);
            check(first, last);
            // SAFETY: the view, guarded: the handler opens the 4 KiB pages written.
            unsafe {
                for k in [1, 2, 5, 9, 15] {
                    poke(block + k * KERNEL_PAGE);
                }
            }
            assert_eq!(open_of(b).0, (1 << 1) | (1 << 2) | (1 << 5) | (1 << 9) | (1 << 15));
            let before = stamp(b);
            let written = check(first, last);
            assert!(stamp(b) > before, "noted written");
            assert_eq!(read_only_pages(block), vec![true; BLOCK / KERNEL_PAGE], "joined={joined}");
            assert_eq!(open_of(b), (0, false));
            // SAFETY: as above; read-only again, so handled and noted again.
            unsafe { poke(block + 7 * KERNEL_PAGE) };
            assert_eq!(open_of(b), (1 << 7, true));
            check(first, last);
            seen.push(written);
            drop_view(section, at, SIZE);
        }
        JOIN_FORCED.with(|f| f.set(None));
        FORCED.with(|f| f.set(false));
        assert_eq!(seen[0], seen[1], "the same with the switch as without");
    }

    #[test]
    fn a_page_is_kept_after_two_intervals_in_a_row_and_probed_after_the_most() {
        // Opened for the first time: closed.
        assert_eq!(keep_split(1 << 3, 0, 0, 0, true, 4), (0, 1 << 3));
        // Written again in the next interval: kept; a page new this interval is closed.
        assert_eq!(keep_split(1 << 3 | 1 << 5, 0, 1 << 3, 0, true, 4), (1 << 3, 1 << 5));
        // Kept pages stay kept until the age reaches the most, then all close.
        assert_eq!(keep_split(1 << 3, 1 << 3, 0, 2, true, 4), (1 << 3, 0));
        assert_eq!(keep_split(1 << 3, 1 << 3, 0, 3, true, 4), (0, 1 << 3));
        // Something else made the block not clean: nothing kept.
        assert_eq!(keep_split(1 << 3, 1 << 3, 1 << 3, 0, false, 4), (0, 1 << 3));
        // At most 1: every check is a probe, as without the switch.
        assert_eq!(keep_split(1 << 3, 0, 1 << 3, 0, true, 1), (0, 1 << 3));
    }

    #[test]
    fn a_kept_page_is_noted_at_every_check_until_the_probe_closes_it() {
        const SIZE: usize = 2 * BLOCK;
        FORCED.with(|f| f.set(true));
        KEEP_FORCED.with(|f| f.set(Some(3)));
        install();
        let (section, at) = view(SIZE);
        let (first, last) = (at / BLOCK, (at + SIZE) / BLOCK);
        let b = first + 1;
        let page = at + BLOCK + 3 * KERNEL_PAGE;
        check(first, last);
        check(first, last);
        // First interval: opened by the fault, closed by the check.
        // SAFETY: the view, guarded: the handler opens the 4 KiB pages written.
        unsafe { poke(page) };
        assert_eq!(check(first, last), Vec::<usize>::new());
        assert!(!super::super::writable(page), "closed after one interval");
        // Second interval in a row: kept open, noted, and the block not written whole.
        // SAFETY: as above.
        unsafe { poke(page) };
        let before = stamp(b);
        assert_eq!(check(first, last), Vec::<usize>::new(), "not written whole");
        assert!(stamp(b) > before, "noted written");
        assert!(super::super::writable(page), "left open");
        assert_eq!(open_of(b), (1 << 3, true), "still noted open");
        // Written with no fault: the next check notes it again all the same.
        // SAFETY: open, a plain write.
        unsafe { poke(page) };
        let before = stamp(b);
        assert_eq!(check(first, last), Vec::<usize>::new());
        assert!(stamp(b) > before, "a kept page is noted at every check");
        // The third check in a row that would keep it is the probe: closed again.
        let before = stamp(b);
        check(first, last);
        assert!(stamp(b) > before);
        assert!(!super::super::writable(page), "the probe closes it");
        // And the next write faults and is seen.
        // SAFETY: the view, guarded.
        unsafe { poke(page) };
        assert_eq!(open_of(b), (1 << 3, true));
        // Kept again (closed by the probe, written again), then made not clean another way:
        // closed and written whole, as without the switch.
        check(first, last);
        // SAFETY: open (kept), a plain write.
        unsafe { poke(page) };
        clean_set(b, false);
        assert_eq!(check(first, last), vec![b], "written whole");
        assert!(!super::super::writable(page), "closed with the rest");
        KEEP_FORCED.with(|f| f.set(None));
        FORCED.with(|f| f.set(false));
        drop_view(section, at, SIZE);
    }

    /// (#88) Sizes `VICTORY_GUARD_4K_KEEP`: 64 guarded blocks with the same five pages each
    /// written every round (faults on the title's side) and checked (closes on ours), without
    /// and with the switch. Ignored; `--ignored --nocapture`, release for real numbers.
    #[test]
    #[ignore]
    fn bench_keep_pages_written_every_round() {
        const BLOCKS: usize = 64;
        const ROUNDS: usize = 200;
        FORCED.with(|f| f.set(true));
        install();
        for most in [0u64, 8, 0, 8] {
            KEEP_FORCED.with(|f| f.set(Some(most)));
            let size = (BLOCKS + 1) * BLOCK;
            let (section, at) = view(size);
            let base = at.div_ceil(BLOCK) * BLOCK;
            let (first, last) = (at / BLOCK, (at + size) / BLOCK);
            check(first, last);
            check(first, last);
            let (mut writing, mut checking) = (0u128, 0u128);
            for _ in 0..ROUNDS {
                let t = std::time::Instant::now();
                for b in 0..BLOCKS {
                    for k in [1, 2, 5, 9, 15] {
                        // SAFETY: the view; guarded pages fault and are opened by the handler.
                        unsafe { poke(base + b * BLOCK + k * KERNEL_PAGE) };
                    }
                }
                writing += t.elapsed().as_nanos();
                let t = std::time::Instant::now();
                check(first, last);
                checking += t.elapsed().as_nanos();
            }
            let pages = (BLOCKS * 5 * ROUNDS) as f64;
            println!(
                "GUARD_4K_KEEP={most}: writes {:.2} us a page (faults), checks {:.2} us a page, {:.2} ms a round",
                writing as f64 / 1e3 / pages,
                checking as f64 / 1e3 / pages,
                (writing + checking) as f64 / 1e6 / ROUNDS as f64
            );
            drop_view(section, at, size);
        }
        KEEP_FORCED.with(|f| f.set(None));
        FORCED.with(|f| f.set(false));
    }

    /// (#88) Sizes the switch: `blocks` guarded blocks with five open pages in four runs each,
    /// closed run by run and joined. Ignored; `--ignored --nocapture`, release for real numbers.
    #[test]
    #[ignore]
    fn bench_close_joined() {
        use windows_sys::Win32::System::Memory::{PAGE_READONLY, PAGE_READWRITE};
        const BLOCKS: usize = 400;
        const ROUNDS: usize = 20;
        let (section, at) = view(BLOCKS * BLOCK);
        let base = at.div_ceil(BLOCK) * BLOCK;
        let blocks = (at + BLOCKS * BLOCK - base) / BLOCK;
        // SAFETY: the view is ours and not guarded (no check ran on it); touch every page once.
        for k in 0..blocks * BLOCK / KERNEL_PAGE {
            unsafe { poke(base + k * KERNEL_PAGE) };
        }
        assert!(super::protect_run(base, blocks * BLOCK, PAGE_READONLY));
        let pages: u64 = (1 << 1) | (1 << 2) | (1 << 5) | (1 << 9) | (1 << 15);
        // Threads closing their own blocks at once, as the kept threads of a wide flush do.
        for threads in [1usize, 16] {
            for joined in [false, true, false, true] {
                let spent_ns = std::sync::atomic::AtomicU64::new(0);
                let calls = std::sync::atomic::AtomicUsize::new(0);
                let wall = std::time::Instant::now();
                std::thread::scope(|sc| {
                    for t in 0..threads {
                        let (spent_ns, calls) = (&spent_ns, &calls);
                        sc.spawn(move || {
                            let mine = (t..blocks).step_by(threads);
                            for _ in 0..ROUNDS {
                                for b in mine.clone() {
                                    for k in 0..16 {
                                        if pages & (1 << k) != 0 {
                                            assert!(super::protect_page(
                                                base + b * BLOCK + k * KERNEL_PAGE,
                                                PAGE_READWRITE
                                            ));
                                        }
                                    }
                                }
                                let at = std::time::Instant::now();
                                for b in mine.clone() {
                                    for (r, e) in close_runs(pages, joined) {
                                        assert!(super::protect_run(
                                            base + b * BLOCK + r * KERNEL_PAGE,
                                            (e - r) * KERNEL_PAGE,
                                            PAGE_READONLY
                                        ));
                                        calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                    }
                                }
                                spent_ns.fetch_add(
                                    at.elapsed().as_nanos() as u64,
                                    std::sync::atomic::Ordering::Relaxed,
                                );
                            }
                        });
                    }
                });
                let spent = spent_ns.into_inner() as f64 / 1e3;
                let calls = calls.into_inner();
                println!(
                    "closing {blocks} blocks x {ROUNDS} (5 open pages in 4 runs each) on {threads} threads, \
                     joined={joined}: {:.2} us a block, {:.2} us a call (all threads), {calls} calls, wall {:.1} ms",
                    spent / (blocks * ROUNDS) as f64,
                    spent / calls as f64,
                    wall.elapsed().as_secs_f64() * 1e3
                );
            }
        }
        assert!(super::protect_run(base, blocks * BLOCK, PAGE_READWRITE));
        drop_view(section, at, BLOCKS * BLOCK);
    }
}
