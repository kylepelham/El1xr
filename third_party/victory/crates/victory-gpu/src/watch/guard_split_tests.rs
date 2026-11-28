//! (feedcard, #88) `VICTORY_GUARD_NOTE_ONCE`: same stamps and 4 KiB entries as one lock per page.

use super::super::{
    DIRTY4K_IN_TEST, KERNEL_PAGE, PAGE, next_asking, note_kernel_batch, subpages, written_in,
    written_set,
};
use super::{NOTE_ONCE_FORCED, PUBLISH_TRY_FORCED, note_written};

/// Reserved address space of our own: page numbers no other test notes.
/// Their stamps and 4 KiB entries start cleared: a test that noted the same
/// addresses before they were freed and given to us leaves its own (in 4
/// full suites at once with 16 threads each, 26 of 100 failed here, #78).
fn reserve(pages: usize) -> usize {
    use windows_sys::Win32::System::Memory::{MEM_RESERVE, PAGE_NOACCESS, VirtualAlloc};
    // SAFETY: a fresh reservation, never committed, freed by `release`.
    let at = unsafe { VirtualAlloc(std::ptr::null(), pages * PAGE, MEM_RESERVE, PAGE_NOACCESS) }
        as usize;
    assert_ne!(at, 0);
    clear_stamps(at, pages * PAGE);
    at
}

fn release(at: usize, pages: usize) {
    use windows_sys::Win32::System::Memory::{MEM_RELEASE, VirtualFree};
    clear_stamps(at, pages * PAGE);
    super::super::before_virtual_free(at, pages * PAGE);
    // SAFETY: the reservation made by `reserve`.
    unsafe { VirtualFree(at as *mut _, 0, MEM_RELEASE) };
}

/// The pages' stamps and 4 KiB entries forgotten, for whoever gets the addresses next.
fn clear_stamps(at: usize, len: usize) {
    use super::super::{PART, WHOLE, clear_subpages_range, part, part_of};
    let (first, last) = (at / PAGE, (at + len).div_ceil(PAGE));
    clear_subpages_range(first, last);
    for page in first..last {
        for p in [part(page, false), part_of(&WHOLE, page, false)].into_iter().flatten() {
            p.0[page % PART].store(0, std::sync::atomic::Ordering::Release);
        }
    }
}

struct Reset;
impl Drop for Reset {
    fn drop(&mut self) {
        DIRTY4K_IN_TEST.set(u8::MAX);
        super::super::holes::KEPT_FORCED.set(None);
        super::super::hole_marks::FORCED.set(None);
        super::super::alias_skip::FORCED.set(None);
        super::super::claim_narrow::FORCED.set(None);
        super::super::fault_unlocked::FORCED.set(None);
        super::super::guard4k::RUNS_FORCED.set(None);
        NOTE_ONCE_FORCED.set(None);
        PUBLISH_TRY_FORCED.set(None);
        super::super::guard4k::STAMP_FRESH_FORCED.set(None);
        super::super::PORT_GUARD_IN_TEST.set(false);
    }
}

/// The 4 KiB entries of `pages` from `first`, with page numbers from 0.
fn entries(first: usize, pages: usize) -> Vec<(usize, u64, u64, u16)> {
    subpages(first * PAGE, (first + pages) * PAGE)
        .into_iter()
        .map(|(p, a, b, bits)| (p - first, a, b, bits))
        .collect()
}

fn stamps(first: usize, pages: usize) -> Vec<(usize, u64)> {
    written_in(first, first + pages)
        .into_iter()
        .map(|(p, w)| (p - first, w))
        .collect()
}

#[test]
fn noting_once_leaves_the_same_stamps_and_entries() {
    let _reset = Reset;
    DIRTY4K_IN_TEST.set(1);
    const N: usize = 8;
    let at = reserve(2 * N);
    let (a, b) = (at / PAGE, at / PAGE + N);
    // The same history on both: every page found written by the kernel at 4 KiB pages.
    let seed = next_asking();
    for first in [a, b] {
        let found: Vec<usize> = (0..N)
            .flat_map(|i| {
                [
                    (first + i) * PAGE + KERNEL_PAGE,
                    (first + i) * PAGE + 7 * KERNEL_PAGE,
                ]
            })
            .collect();
        note_kernel_batch(&found, seed);
    }
    assert_eq!(entries(a, N), entries(b, N));
    assert_eq!(entries(a, N).len(), N);
    let now = next_asking();
    let written = [0usize, 2, 3, 5];
    NOTE_ONCE_FORCED.set(Some(false));
    for &i in &written {
        written_set(a + i, now);
    }
    NOTE_ONCE_FORCED.set(Some(true));
    let pages: Vec<usize> = written.iter().map(|&i| b + i).collect();
    note_written(&pages, now);
    assert_eq!(stamps(a, N), stamps(b, N));
    assert_eq!(entries(a, N), entries(b, N));
    // The pages written whole lost their 4 KiB entries; the others kept theirs.
    let kept: Vec<usize> = entries(b, N).iter().map(|e| e.0).collect();
    assert_eq!(kept, vec![1, 4, 6, 7]);
    assert!(
        stamps(b, N)
            .iter()
            .all(|&(p, w)| w == if written.contains(&p) { now } else { seed })
    );
    release(at, 2 * N);
}

#[test]
fn noting_once_takes_the_table_lock_once() {
    let _reset = Reset;
    DIRTY4K_IN_TEST.set(1);
    let at = reserve(8);
    let pages: Vec<usize> = (0..6).map(|i| at / PAGE + i).collect();
    for (on, takings) in [(false, 6), (true, 1)] {
        NOTE_ONCE_FORCED.set(Some(on));
        super::super::NOTE_LOCKS.set(0);
        note_written(&pages, next_asking());
        assert_eq!(
            super::super::NOTE_LOCKS.get(),
            takings,
            "VICTORY_GUARD_NOTE_ONCE={on}"
        );
    }
    release(at, 8);
}

/// (#88) Sizes the switch: 8 threads note the guards' written pages (32 a check, as a walk of a
/// few dirty blocks) while 8 threads read the 4 KiB table as refreshes do (`subpages` over 64 MiB
/// holding 4,096 entries) and one kernel asking a millisecond notes 2,000 pages.
/// `cargo test -p victory-gpu --release --lib bench_note_once -- --ignored --nocapture`
#[test]
#[ignore]
fn bench_note_once() {
    const WRITERS: usize = 8;
    const READERS: usize = 8;
    const BATCH: usize = 32;
    const ROUNDS: usize = 4000;
    const READ_PAGES: usize = 1024;
    let _reset = Reset;
    DIRTY4K_IN_TEST.set(1);
    let pool = reserve(WRITERS * BATCH + 2 * READ_PAGES);
    let first = pool / PAGE;
    let read_first = first + WRITERS * BATCH;
    let kernel_first = read_first + READ_PAGES;
    let found: Vec<usize> = (0..READ_PAGES)
        .flat_map(|i| (0..4).map(move |k| (read_first + i) * PAGE + k * KERNEL_PAGE))
        .collect();
    note_kernel_batch(&found, next_asking());
    for on in [false, true, false, true] {
        let stop = std::sync::atomic::AtomicBool::new(false);
        let reads = std::sync::atomic::AtomicU64::new(0);
        let worst = std::sync::atomic::AtomicU64::new(0);
        let started = std::time::Instant::now();
        let busy_ns: u64 = std::thread::scope(|s| {
            for _ in 0..READERS {
                s.spawn(|| {
                    DIRTY4K_IN_TEST.set(1);
                    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                        let e = subpages(read_first * PAGE, (read_first + READ_PAGES) * PAGE);
                        std::hint::black_box(e);
                        reads.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                });
            }
            s.spawn(|| {
                DIRTY4K_IN_TEST.set(1);
                let kernel: Vec<usize> = (0..2000)
                    .map(|i| kernel_first * PAGE + i * KERNEL_PAGE)
                    .collect();
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    note_kernel_batch(&kernel, next_asking());
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
            });
            let writers: Vec<_> = (0..WRITERS)
                .map(|w| {
                    let worst = &worst;
                    s.spawn(move || {
                        DIRTY4K_IN_TEST.set(1);
                        NOTE_ONCE_FORCED.set(Some(on));
                        let pages: Vec<usize> = (0..BATCH).map(|i| first + w * BATCH + i).collect();
                        let mut busy = 0u64;
                        for _ in 0..ROUNDS {
                            let t = std::time::Instant::now();
                            note_written(&pages, next_asking());
                            let ns = t.elapsed().as_nanos() as u64;
                            busy += ns;
                            worst.fetch_max(ns, std::sync::atomic::Ordering::Relaxed);
                        }
                        busy
                    })
                })
                .collect();
            let busy = writers.into_iter().map(|h| h.join().unwrap()).sum();
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
            busy
        });
        let wall = started.elapsed();
        let checks = (WRITERS * ROUNDS) as f64;
        println!(
            "VICTORY_GUARD_NOTE_ONCE={}: {:.2} us a check of {BATCH} pages ({:.0} ns a page), worst {:.0} us; \
             wall {:.0} ms; readers {:.0} reads/ms",
            u8::from(on),
            busy_ns as f64 / checks / 1000.0,
            busy_ns as f64 / (checks * BATCH as f64),
            worst.load(std::sync::atomic::Ordering::Relaxed) as f64 / 1000.0,
            wall.as_secs_f64() * 1000.0,
            reads.load(std::sync::atomic::Ordering::Relaxed) as f64 / (wall.as_secs_f64() * 1000.0),
        );
    }
    release(pool, WRITERS * BATCH + 2 * READ_PAGES);
}

/// A pagefile section's view of `blocks` blocks, known to the guards as title view memory.
struct View {
    section: windows_sys::Win32::Foundation::HANDLE,
    at: usize,
    len: usize,
}

// SAFETY: the section handle is only closed by `drop`; the view is plain shared memory.
unsafe impl Sync for View {}

impl View {
    fn new(blocks: usize) -> Self {
        use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, FILE_MAP_WRITE, MapViewOfFile, PAGE_READWRITE,
        };
        let len = blocks * super::super::BLOCK;
        // SAFETY: a fresh pagefile section and one view of it, closed by `drop`.
        unsafe {
            let section = CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                len as u32,
                std::ptr::null(),
            );
            assert!(!section.is_null());
            let at = MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, len).Value as usize;
            assert_ne!(at, 0);
            crate::inplace::title_memory_view(at, len, section as usize, 0);
            Self { section, at, len }
        }
    }

    fn write(&self, block: usize, value: u8) {
        // SAFETY: inside our live view; the guards' handler opens a guarded block.
        unsafe { ((self.at + block * super::super::BLOCK + 16) as *mut u8).write_volatile(value) };
    }

    /// What a refresh of the whole view is told: its asking and the pages (from 0) written since.
    fn refresh(&self, since: u64) -> (u64, Vec<usize>) {
        let (asked, pages) = super::super::writes(self.at, self.at + self.len, true).expect("a view");
        let first = self.at / PAGE;
        (asked, pages.into_iter().filter(|&(_, w)| w > since).map(|(p, _)| p - first).collect())
    }
}

impl Drop for View {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Memory::{MEMORY_MAPPED_VIEW_ADDRESS, UnmapViewOfFile};
        clear_stamps(self.at, self.len);
        super::super::title_memory_remapped(self.at, self.len);
        // SAFETY: our view and section.
        unsafe {
            UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: self.at as *mut _ });
            CloseHandle(self.section);
        }
    }
}

/// The same writes and refreshes tell the same pages written with and without the switch.
#[test]
fn publish_try_reports_the_same_writes() {
    let _reset = Reset;
    super::super::PORT_GUARD_IN_TEST.set(true);
    const BLOCKS: usize = 6;
    let script: [&[usize]; 6] = [&[0, 3], &[], &[1, 2, 5], &[3], &[0, 1, 2, 3, 4, 5], &[4]];
    let mut told = Vec::new();
    for on in [false, true] {
        PUBLISH_TRY_FORCED.set(Some(on));
        let view = View::new(BLOCKS);
        let (mut since, _) = view.refresh(0);
        let mut rounds = Vec::new();
        for (i, blocks) in script.iter().enumerate() {
            for &b in *blocks {
                view.write(b, i as u8);
            }
            let (asked, pages) = view.refresh(since);
            rounds.push(pages);
            since = asked;
        }
        // Nothing written since: nothing told, and every block published again.
        let (_, pages) = view.refresh(since);
        rounds.push(pages);
        let first = view.at / super::super::BLOCK;
        assert!(super::super::published_all(first, first + BLOCKS), "VICTORY_GUARD_PUBLISH_TRY={on}");
        told.push(rounds);
    }
    assert_eq!(told[0], told[1]);
    assert_eq!(told[0][0], vec![0, 3]);
    assert!(told[0][6].is_empty());
}

/// A block whose claim is held is left unpublished at once; writes to it are still told.
#[test]
fn publish_try_leaves_a_claimed_block_unpublished() {
    use super::super::{BUSY, BLOCK, clean_epoch, publish, published_all, published_set};
    let _reset = Reset;
    super::super::PORT_GUARD_IN_TEST.set(true);
    PUBLISH_TRY_FORCED.set(Some(true));
    let view = View::new(2);
    let (asked, _) = view.refresh(0);
    let block = view.at / BLOCK;
    assert!(published_all(block, block + 2));
    published_set(block, 0);
    let epoch = clean_epoch(block);
    assert_ne!(epoch, 0);
    assert!(BUSY.try_set(block));
    publish(&[(block, block + 1, epoch)]);
    assert!(!published_all(block, block + 1));
    BUSY.clear(block);
    // Still clean, so a refresh takes the clean blocks' answer instead, and tells nothing.
    let (asked, pages) = view.refresh(asked);
    assert!(pages.is_empty());
    // Written, it is opened, told and closed again, and published with its walk.
    view.write(0, 7);
    assert_eq!(view.refresh(asked).1, vec![0]);
    assert!(published_all(block, block + 2));
}

/// (#88) Sizes `VICTORY_GUARD_PUBLISH_TRY`: 8 threads publish 16 clean blocks after each "walk"
/// while 8 threads stand in for write faults, each holding a block's claim for about the time
/// of a `VirtualProtect` (3 us) as the fault handler does.
/// `cargo test -p victory-gpu --release --lib bench_publish_try -- --ignored --nocapture`
#[test]
#[ignore]
fn bench_publish_try() {
    use super::super::{BUSY, BLOCK, clean_epoch, publish};
    const PUBLISHERS: usize = 8;
    const FAULTERS: usize = 8;
    const BLOCKS: usize = 16;
    const ROUNDS: usize = 3000;
    let _reset = Reset;
    super::super::PORT_GUARD_IN_TEST.set(true);
    let views: Vec<View> = (0..PUBLISHERS).map(|_| View::new(BLOCKS)).collect();
    for v in &views {
        v.refresh(0);
    }
    let spans: Vec<(usize, usize, u64)> = views
        .iter()
        .map(|v| {
            let b = v.at / BLOCK;
            (b, b + BLOCKS, clean_epoch(b))
        })
        .collect();
    for on in [false, true, false, true] {
        let stop = std::sync::atomic::AtomicBool::new(false);
        let started = std::time::Instant::now();
        let busy_ns: u64 = std::thread::scope(|s| {
            for f in 0..FAULTERS {
                let (spans, stop) = (&spans, &stop);
                s.spawn(move || {
                    let mut k = f;
                    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                        let (b, _, _) = spans[k % PUBLISHERS];
                        let block = b + (k * 7) % BLOCKS;
                        if BUSY.try_set(block) {
                            let t = std::time::Instant::now();
                            while t.elapsed() < std::time::Duration::from_micros(3) {
                                std::hint::spin_loop();
                            }
                            BUSY.clear(block);
                        }
                        k += 1;
                    }
                });
            }
            let publishers: Vec<_> = (0..PUBLISHERS)
                .map(|p| {
                    let spans = &spans;
                    s.spawn(move || {
                        super::super::PORT_GUARD_IN_TEST.set(true);
                        PUBLISH_TRY_FORCED.set(Some(on));
                        let mine = [spans[p]];
                        let mut busy = 0u64;
                        for _ in 0..ROUNDS {
                            let t = std::time::Instant::now();
                            publish(&mine);
                            busy += t.elapsed().as_nanos() as u64;
                        }
                        busy
                    })
                })
                .collect();
            let busy = publishers.into_iter().map(|h| h.join().unwrap()).sum();
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
            busy
        });
        println!(
            "VICTORY_GUARD_PUBLISH_TRY={}: {:.2} us a publish of {BLOCKS} blocks, wall {:.0} ms",
            u8::from(on),
            busy_ns as f64 / (PUBLISHERS * ROUNDS) as f64 / 1000.0,
            started.elapsed().as_secs_f64() * 1000.0,
        );
    }
}

/// (#88, batch 5 part M) Refreshes racing each other and the title's write faults on one view
/// never lose a write: each refresher keeps its own copy, taking a page when its stamp is newer
/// than the asking it last took it at (as `Shadow::copy_stale`), and once the writers are quiet
/// its copy is the memory. With `VICTORY_GUARD_PUBLISH_TRY` the crash's claim bug made writes to
/// read-only blocks go unhandled here (the test process died); with `VICTORY_GUARD_4K` a page
/// closed late in a walk was stamped with the walk's first asking, and lost writes (still so
/// with `VICTORY_GUARD_STAMP_FRESH` off).
fn racing_refreshes_lose_nothing(publish_try: bool, guard_4k: bool, stamp_fresh: bool) -> usize {
    racing_refreshes_lose_nothing_with(publish_try, guard_4k, stamp_fresh, false)
}

/// [`racing_refreshes_lose_nothing_all`] with `VICTORY_FAULT_UNLOCKED` off.
fn racing_refreshes_lose_nothing_with(publish_try: bool, guard_4k: bool, stamp_fresh: bool, walk_cuts: bool) -> usize {
    racing_refreshes_lose_nothing_all(publish_try, guard_4k, stamp_fresh, walk_cuts, false)
}

/// [`racing_refreshes_lose_nothing`]; `walk_cuts`: R25's guard walk switches on
/// (`VICTORY_GUARD_ALIAS_SKIP`, `VICTORY_GUARD_CLAIM_NARROW`, `VICTORY_GUARD_4K_RUNS`).
fn racing_refreshes_lose_nothing_all(
    publish_try: bool,
    guard_4k: bool,
    stamp_fresh: bool,
    walk_cuts: bool,
    unlocked: bool,
) -> usize {
    use std::sync::Barrier;
    use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
    const BLOCKS: usize = 8;
    const WRITERS: usize = 3;
    const REFRESHERS: usize = 4;
    const ROUNDS: usize = 300;
    let _reset = Reset;
    let setup = move || {
        super::super::PORT_GUARD_IN_TEST.set(true);
        PUBLISH_TRY_FORCED.set(Some(publish_try));
        NOTE_ONCE_FORCED.set(Some(publish_try));
        super::super::guard4k::FORCED.with(|f| f.set(guard_4k));
        super::super::guard4k::STAMP_FRESH_FORCED.set(Some(stamp_fresh));
        super::super::alias_skip::FORCED.set(Some(walk_cuts));
        super::super::claim_narrow::FORCED.set(Some(walk_cuts));
        super::super::guard4k::RUNS_FORCED.set(Some(walk_cuts));
        super::super::fault_unlocked::FORCED.set(Some(unlocked));
    };
    setup();
    let view = View::new(BLOCKS);
    view.refresh(0);
    let missed = AtomicUsize::new(0);
    let round_start = Barrier::new(REFRESHERS + WRITERS);
    let round_end = Barrier::new(REFRESHERS + WRITERS);
    let settle = Barrier::new(REFRESHERS);
    std::thread::scope(|s| {
        for w in 0..WRITERS {
            let (view, round_start, round_end) = (&view, &round_start, &round_end);
            s.spawn(move || {
                setup();
                let mut seed = 0x9e37_79b9_7f4a_7c15u64 ^ (w as u64 + 1);
                let mut next = move || {
                    seed ^= seed << 13;
                    seed ^= seed >> 7;
                    seed ^= seed << 17;
                    seed
                };
                let mut write = |n: usize, yield_now: bool| {
                    for _ in 0..n {
                        let at = view.at + (next() as usize % (view.len / 8)) * 8;
                        // SAFETY: inside our live view; the guards' handler opens it.
                        unsafe { (at as *mut u64).write_volatile(next() | 1) };
                        if yield_now {
                            std::thread::yield_now();
                        }
                    }
                };
                for _ in 0..ROUNDS {
                    write(24, false);
                    round_start.wait();
                    // Beside the refreshes.
                    write(16, true);
                    round_end.wait();
                    // Quiet while the refreshers take their last look and compare.
                    round_end.wait();
                }
            });
        }
        for _ in 0..REFRESHERS {
            let (view, round_start, round_end, settle, missed) =
                (&view, &round_start, &round_end, &settle, &missed);
            s.spawn(move || {
                setup();
                // SAFETY: our live view, read only.
                let memory = unsafe { std::slice::from_raw_parts(view.at as *const u8, view.len) };
                let first = view.at / PAGE;
                let (asked, _) = view.refresh(0);
                let mut copy = memory.to_vec();
                let mut copied = vec![asked; view.len / PAGE];
                let mut take = |copy: &mut [u8]| {
                    let (asked, pages) =
                        super::super::writes(view.at, view.at + view.len, true).expect("a view");
                    for (p, w) in pages {
                        let p = p - first;
                        if w > copied[p] {
                            let r = p * PAGE..(p + 1) * PAGE;
                            copy[r.clone()].copy_from_slice(&memory[r]);
                            copied[p] = asked;
                        }
                    }
                };
                for _ in 0..ROUNDS {
                    round_start.wait();
                    for _ in 0..4 {
                        take(&mut copy);
                    }
                    round_end.wait();
                    // A walk can find a block another walk just closed clean before that one
                    // notes it written; the next asking is told it.
                    take(&mut copy);
                    settle.wait();
                    take(&mut copy);
                    if copy.as_slice() != memory {
                        missed.fetch_add(1, Relaxed);
                        copy.copy_from_slice(memory);
                    }
                    round_end.wait();
                }
            });
        }
    });
    missed.into_inner()
}

#[test]
fn racing_refreshes_lose_nothing_blocks() {
    for publish_try in [false, true] {
        let missed = racing_refreshes_lose_nothing(publish_try, false, false);
        assert_eq!(missed, 0, "VICTORY_GUARD_PUBLISH_TRY={publish_try}, block guards");
    }
}

#[test]
fn racing_refreshes_lose_nothing_4k() {
    for publish_try in [false, true] {
        let missed = racing_refreshes_lose_nothing(publish_try, true, true);
        assert_eq!(missed, 0, "VICTORY_GUARD_PUBLISH_TRY={publish_try}, 4 KiB guards");
    }
}

/// (#88 R25) The guard walk's cuts on (alias skip, narrow claims with 4 KiB faults sharing a
/// block, 4 KiB closes joined over blocks): no write lost, block or 4 KiB guards.
#[test]
fn racing_refreshes_lose_nothing_walk_cuts() {
    for (guard_4k, publish_try) in [(false, false), (true, false), (true, true)] {
        let missed = racing_refreshes_lose_nothing_with(publish_try, guard_4k, true, true);
        assert_eq!(missed, 0, "walk cuts on, VICTORY_GUARD_4K={guard_4k}, PUBLISH_TRY={publish_try}");
    }
}

/// `VICTORY_GUARD_STAMP_FRESH` off keeps the old walk-wide stamp, and its stale copies.
#[test]
fn racing_refreshes_stale_without_stamp_fresh() {
    // A race: on a loaded machine one pass of 600 rounds can miss it, so up to four passes.
    let mut missed = 0;
    for pass in 1..=4 {
        missed = [false, true]
            .into_iter()
            .map(|publish_try| racing_refreshes_lose_nothing(publish_try, true, false))
            .sum::<usize>();
        eprintln!("VICTORY_GUARD_STAMP_FRESH=0, pass {pass}: {missed} of 600 rounds stale");
        if missed > 0 {
            break;
        }
    }
    assert!(missed > 0, "the walk-wide stamp lost no write in 4 x 600 rounds");
}

/// (Batch 5 part M's crash) A `try_get` that finds the block claimed leaves the holder's claim:
/// it used to build the claim before knowing, and dropping it cleared the holder's bit, so the
/// guards' walks and write faults no longer kept off each other's blocks.
#[test]
fn a_missed_try_leaves_the_holders_claim() {
    use super::super::{BLOCK, BUSY, ClaimOne};
    let at = reserve(1);
    let block = at / BLOCK;
    assert!(BUSY.try_set(block));
    assert!(ClaimOne::try_get(block).is_none());
    assert!(!BUSY.try_set(block), "the holder's claim was cleared");
    BUSY.clear(block);
    assert!(ClaimOne::try_get(block).is_some());
    assert!(BUSY.try_set(block), "dropped with its owner");
    BUSY.clear(block);
    release(at, 1);
}

/// A reserved pagefile section's view of `blocks` blocks with only `committed` committed, known
/// to the guards as title view memory (the other blocks are uncommitted holes).
fn holed_view(blocks: usize, committed: &[std::ops::Range<usize>]) -> View {
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::System::Memory::{
        CreateFileMappingW, FILE_MAP_WRITE, MEM_COMMIT, MapViewOfFile, PAGE_READWRITE, SEC_RESERVE,
        VirtualAlloc,
    };
    use super::super::BLOCK;
    let len = blocks * BLOCK;
    // SAFETY: a fresh reserved pagefile section and one view of it, closed by `View`'s drop.
    unsafe {
        let section = CreateFileMappingW(
            INVALID_HANDLE_VALUE,
            std::ptr::null(),
            PAGE_READWRITE | SEC_RESERVE,
            0,
            len as u32,
            std::ptr::null(),
        );
        assert!(!section.is_null());
        let at = MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, len).Value as usize;
        assert_ne!(at, 0);
        for r in committed {
            let p = VirtualAlloc((at + r.start * BLOCK) as *const _, r.len() * BLOCK, MEM_COMMIT, PAGE_READWRITE);
            assert!(!p.is_null());
        }
        crate::inplace::title_memory_view(at, len, section as usize, 0);
        View { section, at, len }
    }
}

/// (#88, b5 part M) `VICTORY_GUARD_HOLES_KEPT=1`: the same writes over a view with holes tell
/// the same pages with and without the switch, with far fewer `VirtualQuery`s; a block of a
/// kept hole committed (and told) is guarded and its writes told in the same frame.
#[test]
fn kept_holes_tell_the_same_pages_with_fewer_queries() {
    use windows_sys::Win32::System::Memory::{MEM_COMMIT, PAGE_READWRITE, VirtualAlloc};
    use super::super::BLOCK;
    let _reset = Reset;
    super::super::PORT_GUARD_IN_TEST.set(true);
    let mut told = Vec::new();
    let mut queries = Vec::new();
    for kept in [false, true] {
        super::super::holes::KEPT_FORCED.set(Some(kept));
        // Holes [2, 5) and [7, 12) between committed blocks.
        let view = holed_view(14, &[0..2, 5..7, 12..14]);
        let (mut since, _) = view.refresh(0);
        let before = super::super::tests::syscall_counts().1;
        let mut pages = Vec::new();
        for round in 0..24u8 {
            for b in [0, 5, 12, 13] {
                if (b + round as usize) % 3 != 0 {
                    view.write(b, round);
                }
            }
            let (asked, p) = view.refresh(since);
            since = asked;
            pages.push(p);
        }
        queries.push(super::super::tests::syscall_counts().1 - before);
        // Commit a block of the hole [7, 12), as `XMemVirtualAlloc` does, and write it.
        let at = view.at + 9 * BLOCK;
        // SAFETY: inside our reserved view.
        assert!(!unsafe { VirtualAlloc(at as *const _, BLOCK, MEM_COMMIT, PAGE_READWRITE) }.is_null());
        crate::title_pages_committed(at, BLOCK);
        let (asked, _) = view.refresh(since);
        view.write(9, 7);
        let (_, p) = view.refresh(asked);
        assert!(p.contains(&(9 * BLOCK / PAGE)), "kept={kept}: the committed block's write was told: {p:?}");
        pages.push(p);
        told.push(pages);
    }
    assert_eq!(told[0], told[1], "the same pages told");
    assert!(queries[1] * 2 < queries[0], "queries off {} vs kept {}", queries[0], queries[1]);
}

/// (#88) `VICTORY_GUARD_HOLE_MARKS=1|2`: the same writes over a view with holes tell the same
/// pages as with the switch off, with far fewer `VirtualQuery`s; a block of a marked hole
/// committed (and told) is guarded and its writes told in the same frame; a hole a commit was
/// not told of is still seen at the next frame.
#[test]
fn marked_holes_tell_the_same_pages_with_fewer_queries() {
    use windows_sys::Win32::System::Memory::{MEM_COMMIT, PAGE_READWRITE, VirtualAlloc};
    use super::super::BLOCK;
    let _reset = Reset;
    super::super::PORT_GUARD_IN_TEST.set(true);
    let mut told = Vec::new();
    let mut queries = Vec::new();
    for mode in [0, 1, 2] {
        super::super::hole_marks::FORCED.set(Some(mode));
        // Holes [2, 5), [7, 12) and [14, 20) between committed blocks.
        let view = holed_view(22, &[0..2, 5..7, 12..14, 20..22]);
        let (mut since, _) = view.refresh(0);
        let before = super::super::tests::syscall_counts().1;
        let mut pages = Vec::new();
        for round in 0..24u8 {
            for b in [0, 5, 12, 13, 21] {
                if (b + round as usize) % 3 != 0 {
                    view.write(b, round);
                }
            }
            let (asked, p) = view.refresh(since);
            since = asked;
            pages.push(p);
        }
        queries.push(super::super::tests::syscall_counts().1 - before);
        // Commit a block of the hole [7, 12), as `XMemVirtualAlloc` does, and write it.
        let at = view.at + 9 * BLOCK;
        // SAFETY: inside our reserved view.
        assert!(!unsafe { VirtualAlloc(at as *const _, BLOCK, MEM_COMMIT, PAGE_READWRITE) }.is_null());
        crate::title_pages_committed(at, BLOCK);
        let (asked, _) = view.refresh(since);
        view.write(9, 7);
        let (asked, p) = view.refresh(asked);
        assert!(p.contains(&(9 * BLOCK / PAGE)), "mode={mode}: the committed block's write was told: {p:?}");
        pages.push(p);
        // Commit a block of the hole [14, 20) without telling: seen once the frame is over.
        let at = view.at + 16 * BLOCK;
        // SAFETY: inside our reserved view.
        assert!(!unsafe { VirtualAlloc(at as *const _, BLOCK, MEM_COMMIT, PAGE_READWRITE) }.is_null());
        super::super::hole_marks::next_frame_for_test();
        let (asked, _) = view.refresh(asked);
        view.write(16, 9);
        let (_, p) = view.refresh(asked);
        assert!(p.contains(&(16 * BLOCK / PAGE)), "mode={mode}: an untold commit is seen the next frame: {p:?}");
        pages.push(p);
        told.push(pages);
    }
    assert_eq!(told[0], told[1], "=1: the same pages told");
    assert_eq!(told[0], told[2], "=2: the same pages told");
    assert!(queries[1] * 2 < queries[0], "queries off {} vs marks {}", queries[0], queries[1]);
}

/// The kept-holes table: a run is kept for its frame, never when free, never when the runtime
/// told of a change while it was asked about, and forgotten where told.
#[test]
fn kept_holes_follow_frames_and_what_is_told() {
    use super::super::holes::{forget_in_table, kept_end as kept_end_in, kept_table};
    let table = kept_table();
    let kept_end = |b, f, span: &mut dyn FnMut(usize) -> (usize, bool)| kept_end_in(&table, b, f, span);
    let forget = |at, len| forget_in_table(&table, at, len);
    let b = 100usize;
    let frame = 7;
    let mut asked = 0;
    let mut ask = |end: usize, reserved: bool| {
        asked += 1;
        (end, reserved)
    };
    assert_eq!(kept_end(b, frame, &mut |_| ask(b + 4, true)), b + 4);
    assert_eq!(kept_end(b + 2, frame, &mut |_| ask(b + 9, true)), b + 4, "kept");
    assert_eq!(kept_end(b + 2, frame + 1, &mut |_| ask(b + 3, true)), b + 3, "a new frame asks again");
    assert_eq!(kept_end(b + 2, frame, &mut |_| ask(b + 9, true)), b + 9, "an older frame is not kept");
    assert_eq!(kept_end(b + 2, frame + 1, &mut |_| ask(b + 9, true)), b + 3, "kept for its frame");
    forget((b + 2) * super::super::BLOCK, 1);
    assert_eq!(kept_end(b + 2, frame + 1, &mut |_| ask(b + 2, false)), b + 2, "forgotten where told");
    assert_eq!(kept_end(b + 5, frame + 1, &mut |_| ask(b + 8, false)), b + 8);
    assert_eq!(kept_end(b + 5, frame + 1, &mut |_| ask(b + 6, false)), b + 6, "a free run is not kept");
    // Told while asking: not kept.
    let at = (b + 20) * super::super::BLOCK;
    assert_eq!(
        kept_end(b + 20, frame + 1, &mut |_| {
            forget(at, 1);
            (b + 30, true)
        }),
        b + 30
    );
    assert_eq!(kept_end(b + 20, frame + 1, &mut |_| ask(b + 21, true)), b + 21, "told while asked");
    assert_eq!(asked, 7);
}

/// (#88) 16 threads checking one view with holes, as the refresh threads of a flush do, while
/// another thread tells of commits elsewhere as the title's host heap does (`xmem`, thousands a
/// frame): a check's time and its `VirtualQuery`s with the switches off, `VICTORY_GUARD_HOLES_KEPT`
/// and `VICTORY_GUARD_HOLE_MARKS=1`, alternated.
/// `cargo test --release -p victory-gpu --lib bench_holes_kept -- --ignored --nocapture`
#[test]
#[ignore]
fn bench_holes_kept() {
    use std::sync::Barrier;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
    const THREADS: usize = 16;
    const CHECKS: usize = 2000;
    let _reset = Reset;
    // Committed blocks between holes, as streamed buffers lie: 6 holes in 32 blocks.
    let view = holed_view(32, &[0..2, 5..8, 11..12, 15..20, 24..25, 28..29, 31..32]);
    let elsewhere = reserve(64);
    for round in 0..4 {
        for (name, kept, marks) in [("off", false, 0), ("kept", true, 0), ("marks", false, 1)] {
            let busy = AtomicU64::new(0);
            let queries = AtomicU64::new(0);
            let start = Barrier::new(THREADS + 1);
            let done = AtomicBool::new(false);
            // Frames go by as in the game (here one every 2 ms), and the marks and kept runs with them.
            let frame = AtomicU64::new(round * 1000);
            let wall = std::time::Instant::now();
            std::thread::scope(|s| {
                let (start, done, frame) = (&start, &done, &frame);
                s.spawn(move || {
                    start.wait();
                    let (mut i, began) = (0usize, std::time::Instant::now());
                    while !done.load(Relaxed) {
                        crate::title_pages_committed(elsewhere + (i % 64) * PAGE, 4096);
                        i += 1;
                        if i % 8 == 0 {
                            frame.store(round * 1000 + began.elapsed().as_millis() as u64 / 2, Relaxed);
                            std::thread::yield_now();
                        }
                    }
                });
                std::thread::scope(|s| {
                    for t in 0..THREADS {
                        let (view, busy, queries) = (&view, &busy, &queries);
                        s.spawn(move || {
                            super::super::PORT_GUARD_IN_TEST.set(true);
                            super::super::holes::KEPT_FORCED.set(Some(kept));
                            super::super::hole_marks::FORCED.set(Some(marks));
                            start.wait();
                            let q = super::super::tests::syscall_counts().1;
                            let t0 = std::time::Instant::now();
                            for i in 0..CHECKS {
                                // A write now and then, so checks walk as well as settle early.
                                if i % 64 == t % 64 {
                                    view.write([0, 5, 15][t % 3], i as u8);
                                }
                                super::super::hole_marks::frames_on_for_test(frame.load(Relaxed));
                                let _ = super::super::writes(view.at, view.at + view.len, true);
                            }
                            busy.fetch_add(t0.elapsed().as_nanos() as u64, Relaxed);
                            queries.fetch_add(super::super::tests::syscall_counts().1 - q, Relaxed);
                        });
                    }
                });
                done.store(true, Relaxed);
            });
            let checks = (THREADS * CHECKS) as f64;
            eprintln!(
                "round {round} {name}: {:.2} us a check, {:.2} queries a check, wall {:.1} ms",
                busy.load(Relaxed) as f64 / (checks * 1000.0),
                queries.load(Relaxed) as f64 / checks,
                wall.elapsed().as_secs_f64() * 1000.0
            );
        }
    }
    release(elsewhere, 64);
}

#[path = "walk_cuts_tests.rs"]
mod walk_cuts;

#[path = "fault_unlocked_tests.rs"]
mod fault_unlocked_t;
