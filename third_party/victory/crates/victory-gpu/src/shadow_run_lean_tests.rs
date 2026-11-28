use super::*;
use crate::shadow::lazy_and_simd_tests::make_test_shadow;

fn next(seed: &mut u64) -> usize {
    *seed ^= *seed << 13;
    *seed ^= *seed >> 7;
    *seed ^= *seed << 17;
    *seed as usize
}

/// Title-like memory of `bytes`, never freed (tests).
fn memory(bytes: usize) -> usize {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc,
    };
    // SAFETY: a fresh allocation of our own.
    let at = unsafe {
        VirtualAlloc(
            std::ptr::null(),
            bytes,
            MEM_COMMIT | MEM_RESERVE,
            PAGE_READWRITE,
        )
    };
    assert!(!at.is_null());
    at as usize
}

/// The runs the sub-page plan makes of a written stretch and a few lone 4 KiB pages in
/// `[base, base + span)`: each 64 KiB page's mask cut into runs of its own (`plan_stale_subpages`),
/// now and then a partial page's piece from an odd byte.
fn planned(seed: &mut u64, base: usize, span: usize) -> Vec<(usize, usize)> {
    const SUB: usize = 4096;
    let subs = span / SUB;
    let mut written = vec![false; subs];
    let at = next(seed) % subs;
    let len = 1 + next(seed) % 48;
    for w in written.iter_mut().skip(at).take(len) {
        *w = true;
    }
    for _ in 0..next(seed) % 4 {
        written[next(seed) % subs] = true;
    }
    let mut runs: Vec<(usize, usize)> = Vec::new();
    for page in 0..span / PAGE {
        let mut k = 0;
        while k < PAGE / SUB {
            let i = page * (PAGE / SUB) + k;
            if !written[i] {
                k += 1;
                continue;
            }
            let a = k;
            while k < PAGE / SUB && written[page * (PAGE / SUB) + k] {
                k += 1;
            }
            let mut lo = base + page * PAGE + a * SUB;
            if next(seed) % 16 == 0 {
                lo += 1 + next(seed) % 200;
            }
            runs.push((lo, base + page * PAGE + k * SUB));
        }
    }
    runs
}

fn union(ranges: &[(usize, usize)]) -> Vec<(usize, usize)> {
    let mut v: Vec<(usize, usize)> = ranges.iter().copied().filter(|r| r.0 < r.1).collect();
    v.sort_unstable();
    let mut out: Vec<(usize, usize)> = Vec::new();
    for (a, b) in v {
        match out.last_mut() {
            Some(last) if a <= last.1 => last.1 = last.1.max(b),
            _ => out.push((a, b)),
        }
    }
    out
}

#[test]
fn joined_runs_cover_the_same_bytes_and_keep_the_lanes() {
    let mut seed = 0x88u64;
    for _ in 0..2000 {
        let base = 0x4000_0000 + (next(&mut seed) % 64) * PAGE;
        let runs = planned(&mut seed, base, 32 * PAGE);
        let joined = join(runs.clone());
        assert_eq!(union(&joined), union(&runs), "the same bytes");
        assert!(joined.len() <= runs.len());
        // Every joint of a joined run is a whole number of lanes from its start, and every run
        // left apart either does not touch the one before or would break that.
        let mut j = 0;
        for &(a, b) in &joined {
            assert!(b - a <= MOST);
            while j < runs.len() && runs[j].1 <= b {
                assert!(runs[j].0 >= a);
                assert_eq!((runs[j].0 - a) % LANE, 0, "a joint off the lanes");
                j += 1;
            }
        }
        for w in joined.windows(2) {
            assert!(w[0].1 != w[1].0 || (w[1].0 - w[0].0) % LANE != 0 || w[1].1 - w[0].0 > MOST);
        }
    }
    let many: Vec<(usize, usize)> = (0..600).map(|i| (i * PAGE, (i + 1) * PAGE)).collect();
    let joined = join(many);
    assert!(
        joined.iter().all(|&(a, b)| b - a <= MOST) && joined.len() == 3,
        "{joined:?}"
    );
}

/// The plan's page counts kept on the stack add up as `watch::note_4k` counted them page by page.
#[test]
fn a_plan_tally_counts_as_note_4k() {
    let mut t = Tally::default();
    for m in [Some(0b1011u16), None, Some(u16::MAX), None] {
        t.page(m);
    }
    assert_eq!(t.0, [(3 + 16) * 4096, 2, 32 * 4096, 2]);
}

/// Two copies of the same title memory with the same CPU-side bytes, one refreshed run by run
/// as before and one with the switch: the same bytes, the same ranges for the card, over random
/// plans, a copy in the card's memory with the compare (`VICTORY_COPY_CHANGED`) and one in host memory.
#[test]
fn lean_runs_leave_the_same_copy_and_send_the_same_bytes() {
    let Some(h) = crate::host::host() else {
        return;
    };
    let _lazy = crate::shadow::lazy::test_disable_lazy_copyback();
    COPY_CHANGED_IN_TEST.set(Some(true));
    const SPAN: usize = 32 * PAGE;
    let title = memory(SPAN + PAGE);
    let title = title.next_multiple_of(PAGE);
    let mut seed = 0x5eed_88u64;
    let mut differed = 0;
    for round in 0..24 {
        let carded = round % 3 != 2;
        for i in (0..SPAN).step_by(8) {
            // SAFETY: inside our allocation.
            unsafe { *((title + i) as *mut u64) = next(&mut seed) as u64 };
        }
        let runs = planned(&mut seed, title, SPAN);
        let (Some(old), Some(new)) = (
            make_test_shadow(h, title as u64, SPAN as u64, carded),
            make_test_shadow(h, title as u64, SPAN as u64, carded),
        ) else {
            return;
        };
        // The CPU sides hold the title's bytes but for scattered differences.
        // SAFETY: both CPU sides are mapped for `SPAN` bytes.
        let (a, b) = unsafe {
            (
                std::slice::from_raw_parts_mut(old.data, SPAN),
                std::slice::from_raw_parts_mut(new.data, SPAN),
            )
        };
        a.copy_from_slice(unsafe { std::slice::from_raw_parts(title as *const u8, SPAN) });
        for _ in 0..400 {
            let at = next(&mut seed) % SPAN;
            a[at] ^= 1 + (next(&mut seed) % 255) as u8;
        }
        b.copy_from_slice(a);
        let n_old = old.copy_runs_here(runs.iter().copied());
        FORCED.set(Some(true));
        let n_new = new.copy_runs_lean(&join(runs.clone()));
        FORCED.set(None);
        assert_eq!(n_old, n_new, "round {round}");
        assert!(a == b, "round {round}: the CPU sides differ");
        if carded {
            let sent = |s: &Shadow| union(&s.card.as_ref().unwrap().dirty.lock().unwrap());
            let (so, sn) = (sent(&old), sent(&new));
            assert_eq!(so, sn, "round {round}: the ranges for the card");
            differed += so.len();
        }
    }
    COPY_CHANGED_IN_TEST.set(None);
    assert!(differed > 20, "few differences were sent ({differed})");
    eprintln!("lean runs: {differed} ranges sent alike");
}

/// (#88) A frame of the stale copy's sub-page path on a copy in the card's memory: 400 refreshes
/// over 256 MiB, each a written stretch and a few lone 4 KiB pages cut by page as the plan does
/// (about 1,000 runs, a third of the 4 KiB pages differing), memory out of the caches before each
/// frame, run by run as before and with the switch, on 1 and 16 threads: runs and the threads'
/// busy time a frame.
/// `cargo test --release -p victory-gpu --lib bench_lean_runs -- --ignored --nocapture`
#[test]
#[ignore]
fn bench_lean_runs() {
    let Some(h) = crate::host::host() else {
        eprintln!("no host device");
        return;
    };
    let _lazy = crate::shadow::lazy::test_disable_lazy_copyback();
    const SPAN: usize = 256 << 20;
    const CALLS: usize = 400;
    let title = memory(SPAN + PAGE).next_multiple_of(PAGE);
    let junk = memory(256 << 20);
    let Some(copy) = make_test_shadow(h, title as u64, SPAN as u64, true) else {
        eprintln!("no copy");
        return;
    };
    let mut seed = 0x88u64;
    for frame in 0..8 {
        // Refreshes of their own stretches, as one flush's tasks are.
        let mut slots: Vec<usize> = (0..SPAN / (8 * PAGE)).collect();
        for i in (1..slots.len()).rev() {
            slots.swap(i, next(&mut seed) % (i + 1));
        }
        let calls: Vec<Vec<(usize, usize)>> = slots[..CALLS]
            .iter()
            .map(|&slot| planned(&mut seed, title + slot * 8 * PAGE, 8 * PAGE))
            .collect();
        let runs: usize = calls.iter().map(Vec::len).sum();
        for threads in [1usize, 16] {
            for lean in [false, true] {
                // The title writes its stretches again, a third of them with new bytes.
                for call in &calls {
                    for &(a, b) in call {
                        for i in (a & !31..b).step_by(1024) {
                            // SAFETY: inside the title allocation.
                            unsafe {
                                *(i as *mut u64) = if i / 4096 % 3 == 0 {
                                    next(&mut seed) as u64
                                } else {
                                    7
                                }
                            };
                        }
                    }
                }
                for i in (0..256usize << 20).step_by(64) {
                    // SAFETY: inside our allocation.
                    unsafe { *((junk + i) as *mut u8) = i as u8 };
                }
                COPY_CHANGED_IN_TEST.set(Some(true));
                // Each thread's time in its refreshes, from a common start (thread starts left out).
                let go = std::sync::Barrier::new(threads);
                let busy = std::sync::atomic::AtomicU64::new(0);
                std::thread::scope(|s| {
                    for k in 0..threads {
                        let (calls, copy, go, busy) = (&calls, &copy, &go, &busy);
                        s.spawn(move || {
                            COPY_CHANGED_IN_TEST.set(Some(true));
                            FORCED.set(Some(lean));
                            go.wait();
                            let t = std::time::Instant::now();
                            for call in calls.iter().skip(k).step_by(threads) {
                                if lean {
                                    copy.copy_runs_lean(&join(call.clone()));
                                } else {
                                    copy.copy_runs_here(call.iter().copied());
                                }
                            }
                            busy.fetch_add(t.elapsed().as_nanos() as u64, Ordering::Relaxed);
                        });
                    }
                });
                let us = busy.into_inner() as f64 / 1e3;
                let copied = if lean {
                    calls.iter().map(|c| join(c.clone()).len()).sum()
                } else {
                    runs
                };
                eprintln!(
                    "frame {frame} threads={threads} lean={lean}: {copied} runs copied of {runs}, {us:.0} us busy over the threads"
                );
            }
        }
    }
}
