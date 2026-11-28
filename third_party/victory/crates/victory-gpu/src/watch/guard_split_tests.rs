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
        NOTE_ONCE_FORCED.set(None);
        PUBLISH_TRY_FORCED.set(None);
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
