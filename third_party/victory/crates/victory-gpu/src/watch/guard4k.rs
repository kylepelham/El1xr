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
/// closed and written whole too, pages that could not be made read-only again.
static COUNTS: [std::sync::atomic::AtomicU64; 4] = [const { std::sync::atomic::AtomicU64::new(0) }; 4];

fn count(what: usize, n: u64) {
    COUNTS[what].fetch_add(n, std::sync::atomic::Ordering::Relaxed);
}

/// With the guard line.
pub(super) fn line(frames: f64) {
    if !on() || frames <= 0.0 {
        return;
    }
    let [opened, alone, whole, failed] =
        [0, 1, 2, 3].map(|i| COUNTS[i].swap(0, std::sync::atomic::Ordering::Relaxed) as f64 / frames);
    tracing::info!(
        "4 KiB guards per frame (VICTORY_GUARD_4K=1): {opened:.0} pages opened alone; checks noted {alone:.0} blocks \
         at their 4 KiB pages alone and {whole:.0} written whole too; {failed:.1} pages not made read-only again"
    );
}

const SUBS: u64 = (1 << (BLOCK / KERNEL_PAGE)) - 1;
const ONLY: u64 = 1 << 62;

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

/// Blocks `[from, to)` were made read-only by the guards: room for their open pages, made here
/// (never in the fault handler, which must not allocate).
pub(super) fn ready(from: usize, to: usize) {
    if !on() {
        return;
    }
    let mut block = from;
    while block < to {
        let _ = part_of(&OPEN, block, true);
        block = (block / super::PART + 1) * super::PART;
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
    if cell.load(Acquire) & bit != 0 {
        // Open already: the fault is not ours.
        return Some((0, writable(at)));
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
}

/// The guard's check of `block` (protected, not clean, its claim held): its open pages made
/// read-only and noted written at asking `now()`.
pub(super) fn close(block: usize, now: &mut dyn FnMut() -> u64) -> Closed {
    let Some(part) = part_of(&OPEN, block, false) else {
        return Closed::None;
    };
    let cell = &part.0[block % super::PART];
    let taken = cell.swap(0, AcqRel);
    let pages = taken & SUBS;
    if pages == 0 {
        return Closed::None;
    }
    let base = block * BLOCK;
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
        let (from, len) = (base + run * KERNEL_PAGE, (k - run) * KERNEL_PAGE);
        if !protect_run(from, len, windows_sys::Win32::System::Memory::PAGE_READONLY) {
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
    use super::{FORCED, open_of};

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
}
