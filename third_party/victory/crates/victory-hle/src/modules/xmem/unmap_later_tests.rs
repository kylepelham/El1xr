use std::ffi::c_void;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::time::Instant;

use windows_sys::Win32::System::Memory::{
    MEM_COMMIT, MEM_RESERVE, MEMORY_BASIC_INFORMATION, PAGE_READWRITE, VirtualQuery,
};

use super::super::tests::allocate_whole;
use super::super::{
    PAGES_64K, XMemFreePhysicalPages, XMemMapPhysicalPages, XMemMapPhysicalPagesScatter,
    XMemVirtualAlloc, physical,
};
use super::*;

/// The host's state of the page at `at`, without draining (unlike `tests::query`).
fn state(at: *const c_void) -> u32 {
    // SAFETY: plain data; zero is a valid value.
    let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: a sized structure of ours.
    assert_ne!(
        unsafe { VirtualQuery(at, &mut info, size_of::<MEMORY_BASIC_INFORMATION>()) },
        0
    );
    info.State
}

/// Holds the thread back while it lives.
struct Pause;

impl Pause {
    fn new() -> Pause {
        PAUSED.fetch_add(1, Relaxed);
        Pause
    }
}

impl Drop for Pause {
    fn drop(&mut self) {
        PAUSED.fetch_sub(1, Relaxed);
    }
}

fn reserve(pages: usize) -> *mut c_void {
    let a = XMemVirtualAlloc(
        std::ptr::null_mut(),
        pages * PAGE,
        MEM_RESERVE | PAGES_64K,
        0,
        PAGE_READWRITE,
    );
    assert!(!a.is_null());
    a
}

/// A pure unmap returns with the views out of the page table, the holes
/// noted and the kernel's unmaps not done; a map over one of them unmaps it
/// first (the caller takes it back), and the thread does the rest once let.
#[test]
fn a_pure_unmap_is_left_to_the_thread_and_taken_back_by_a_map() {
    FORCE.with(|f| f.set(Some(true)));
    super::super::tests::FORCE.with(|f| f.set([Some(true), None]));
    let pages = allocate_whole(4);
    let a = reserve(4);
    let at = |k: usize| a.wrapping_byte_add(k * PAGE);
    for k in 0..4 {
        assert_eq!(XMemMapPhysicalPages(at(k), 1, &pages[k]), at(k));
        // SAFETY: mapped read-write.
        unsafe { at(k).cast::<u64>().write(10 + k as u64) };
    }
    {
        let _pause = Pause::new();
        let to: Vec<*mut c_void> = (0..4).map(at).collect();
        assert_eq!(
            XMemMapPhysicalPagesScatter(to.as_ptr(), 4, [0usize; 4].as_ptr()),
            1
        );
        let base = a as usize;
        assert!(
            physical()
                .views
                .range(base..base + 4 * PAGE)
                .next()
                .is_none()
        );
        assert!((0..4).all(|k| physical().holes.get(&(at(k) as usize)) == Some(&PAGE)));
        // Not unmapped yet: what the old page holds is still there.
        assert_eq!(state(at(1)), MEM_COMMIT);
        // Page 3 mapped over address 1: its view handed on goes first.
        assert_eq!(XMemMapPhysicalPages(at(1), 1, &pages[3]), at(1));
        // SAFETY: mapped read-write.
        assert_eq!(unsafe { at(1).cast::<u64>().read() }, 13);
        assert_eq!(state(at(2)), MEM_COMMIT);
    }
    drain();
    assert!([0, 2, 3].iter().all(|&k| state(at(k)) == MEM_RESERVE));
    assert_eq!(state(at(1)), MEM_COMMIT);
    assert_eq!(XMemMapPhysicalPages(a, 4, std::ptr::null()), a);
    drain();
    assert!((0..4).all(|k| state(at(k)) == MEM_RESERVE));
    assert_eq!(XMemFreePhysicalPages(4, pages.as_ptr()), 1);
    super::super::tests::FORCE.with(|f| f.set([None; 2]));
    FORCE.with(|f| f.set(None));
}

/// The thread waits for a remapping lock held by a reader of title memory
/// in place, and unmaps once it is let go.
#[test]
fn the_thread_waits_for_reads_in_place() {
    FORCE.with(|f| f.set(Some(true)));
    let pages = allocate_whole(1);
    let a = reserve(1);
    assert_eq!(XMemMapPhysicalPages(a, 1, pages.as_ptr()), a);
    let pause = Pause::new();
    assert_eq!(XMemMapPhysicalPages(a, 1, std::ptr::null()), a);
    let reading = victory_gpu::title_memory_reading_range(a as usize, PAGE);
    drop(pause);
    std::thread::sleep(Duration::from_millis(20));
    // SAFETY: still mapped: the read under way may go on.
    assert_eq!(unsafe { a.cast::<u64>().read() }, 0);
    assert_eq!(state(a), MEM_COMMIT);
    drop(reading);
    drain();
    assert_eq!(state(a), MEM_RESERVE);
    assert_eq!(XMemFreePhysicalPages(1, pages.as_ptr()), 1);
    FORCE.with(|f| f.set(None));
}

/// Microbenchmark (`cargo test ... unmap_later_bench -- --ignored --nocapture`): a map/unmap
/// loop of single pages, the title-side µs of each pure unmap and of the map after it, with
/// the switch off and on. The section has 4,096 other views of a page each (the title maps its
/// pool 64 KiB at a time). Loaded: 30 threads of this process writing memory (every unmap's TLB
/// flush reaches their cores) and one more mapping and unmapping pages of the same section.
#[test]
#[ignore]
fn unmap_later_bench() {
    use super::super::{CALL_SPLIT, WORK};
    const N: usize = 256;
    const OTHER: usize = 4096;
    const CHURN: usize = 64;
    const ROUNDS: usize = 20;
    super::super::tests::FORCE.with(|f| f.set([Some(true), None]));
    let pages = allocate_whole(N + OTHER + CHURN);
    let a = reserve(N);
    let at = |k: usize| a.wrapping_byte_add(k * PAGE);
    let b = reserve(OTHER);
    for k in 0..OTHER {
        let to = b.wrapping_byte_add(k * PAGE);
        assert_eq!(XMemMapPhysicalPages(to, 1, &pages[N + k]), to);
    }
    let churn = reserve(CHURN) as usize;
    let churn_pages: Vec<usize> = pages[N + OTHER..].to_vec();
    for loaded in [false, true] {
        let stop = Arc::new(AtomicBool::new(false));
        let mut load: Vec<_> = (0..if loaded { 30 } else { 0 })
            .map(|_| {
                let stop = stop.clone();
                std::thread::spawn(move || {
                    let mut buf = vec![0u64; 1 << 16];
                    let mut i = 0usize;
                    while !stop.load(Relaxed) {
                        buf[i & 0xffff] = buf[i & 0xffff].wrapping_add(i as u64);
                        i = i.wrapping_add(4099);
                    }
                    std::hint::black_box(&buf);
                })
            })
            .collect();
        if loaded {
            let stop = stop.clone();
            let churn_pages = churn_pages.clone();
            load.push(std::thread::spawn(move || {
                super::super::tests::FORCE.with(|f| f.set([Some(true), None]));
                while !stop.load(Relaxed) {
                    for (k, page) in churn_pages.iter().enumerate() {
                        let to = (churn + k * PAGE) as *mut c_void;
                        XMemMapPhysicalPages(to, 1, page);
                        XMemMapPhysicalPages(to, 1, std::ptr::null());
                    }
                }
            }));
        }
        for on in [false, true] {
            FORCE.with(|f| f.set(Some(on)));
            let (mut unmap_us, mut map_us, mut worst) = (0u128, 0u128, 0u128);
            let mut split = [0u64; 3];
            let mut kernel = [0u64; 2];
            for round in 0..ROUNDS {
                for k in 0..N {
                    let page = pages[(k + round) % N];
                    let t = Instant::now();
                    assert_eq!(XMemMapPhysicalPages(at(k), 1, &page), at(k));
                    map_us += t.elapsed().as_micros();
                }
                // The pure unmaps' own parts (the churn thread's land here too when loaded).
                for (i, s) in [2, 3, 4].into_iter().zip(split.iter_mut()) {
                    CALL_SPLIT[i].swap(0, Relaxed);
                    *s += 0;
                }
                let before = [WORK[0].load(Relaxed), WORK[1].load(Relaxed)];
                for k in 0..N {
                    let t = Instant::now();
                    assert_eq!(XMemMapPhysicalPages(at(k), 1, std::ptr::null()), at(k));
                    let us = t.elapsed().as_micros();
                    unmap_us += us;
                    worst = worst.max(us);
                }
                kernel[0] += WORK[0].load(Relaxed) - before[0];
                kernel[1] += WORK[1].load(Relaxed) - before[1];
                for (i, s) in [2, 3, 4].into_iter().zip(split.iter_mut()) {
                    *s += CALL_SPLIT[i].swap(0, Relaxed);
                }
                // Half the rounds map again at once (the caller takes views back),
                // half after the thread is done.
                if round % 2 == 0 {
                    drain();
                }
            }
            drain();
            let calls = (N * ROUNDS) as f64;
            println!(
                "unmap_later bench: loaded={loaded} on={on}: pure unmap {:.1} us a call (worst {worst} us; kernel unmap waited {:.1} us a view; page table {:.1}, remapping lock {:.1}, GPU side {:.1} us a call), map after it {:.1} us a call",
                unmap_us as f64 / calls,
                kernel[1] as f64 / kernel[0].max(1) as f64,
                split[0] as f64 / calls,
                split[1] as f64 / calls,
                split[2] as f64 / calls,
                map_us as f64 / calls,
            );
            println!("unmap_later bench:   {}", line(1.0));
        }
        stop.store(true, Relaxed);
        for t in load {
            t.join().unwrap();
        }
    }
    assert_eq!(XMemMapPhysicalPages(b, OTHER, std::ptr::null()), b);
    drain();
    assert_eq!(XMemFreePhysicalPages(N + OTHER + CHURN, pages.as_ptr()), 1);
    FORCE.with(|f| f.set(None));
    super::super::tests::FORCE.with(|f| f.set([None; 2]));
}
