//! (feedcard, #88 R25) The guard walk's cuts: `VICTORY_GUARD_ALIAS_SKIP`, `VICTORY_GUARD_CLAIM_NARROW`
//! and `VICTORY_GUARD_4K_RUNS` tell the same pages as without them; microbenches.

use super::super::super::{BLOCK, BUSY, Claims, PAGE, busy_try};
use super::super::super::{alias_skip, claim_narrow, guard4k};
use super::{Reset, View, clear_stamps};

/// Two views of one committed pagefile section, both known to the guards (every block aliased).
struct Pair {
    section: windows_sys::Win32::Foundation::HANDLE,
    a: usize,
    b: usize,
    len: usize,
}

impl Pair {
    fn new(blocks: usize) -> Self {
        use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, FILE_MAP_WRITE, MapViewOfFile, PAGE_READWRITE,
        };
        let len = blocks * BLOCK;
        // SAFETY: a fresh pagefile section and two views of it, closed by `drop`.
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
            let a = MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, len).Value as usize;
            let b = MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, len).Value as usize;
            assert!(a != 0 && b != 0);
            crate::inplace::title_memory_view(a, len, section as usize, 0);
            crate::inplace::title_memory_view(b, len, section as usize, 0);
            Self { section, a, b, len }
        }
    }

    fn write(&self, through_b: bool, block: usize, value: u8) {
        let at = if through_b { self.b } else { self.a };
        // SAFETY: inside our live view; the guards' handler opens it.
        unsafe { ((at + block * BLOCK + 32) as *mut u8).write_volatile(value) };
    }

    fn refresh(&self, through_b: bool, since: u64) -> (u64, Vec<usize>) {
        let at = if through_b { self.b } else { self.a };
        let (asked, pages) = super::super::super::writes(at, at + self.len, true).expect("a view");
        let first = at / PAGE;
        (
            asked,
            pages
                .into_iter()
                .filter(|&(_, w)| w > since)
                .map(|(p, _)| p - first)
                .collect(),
        )
    }

    /// The second view unmapped and forgotten, as the runtime does.
    fn drop_b(&mut self) {
        use windows_sys::Win32::System::Memory::{MEMORY_MAPPED_VIEW_ADDRESS, UnmapViewOfFile};
        if self.b == 0 {
            return;
        }
        clear_stamps(self.b, self.len);
        super::super::super::title_memory_remapped(self.b, self.len);
        // SAFETY: our view.
        unsafe {
            UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                Value: self.b as *mut _,
            })
        };
        self.b = 0;
    }
}

impl Drop for Pair {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Memory::{MEMORY_MAPPED_VIEW_ADDRESS, UnmapViewOfFile};
        self.drop_b();
        clear_stamps(self.a, self.len);
        super::super::super::title_memory_remapped(self.a, self.len);
        // SAFETY: our view and section.
        unsafe {
            UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                Value: self.a as *mut _,
            });
            CloseHandle(self.section);
        }
    }
}

#[test]
fn alias_bits_follow_the_views() {
    let lone = View::new(2);
    assert!(
        !alias_skip::marked(lone.at / BLOCK),
        "a block with one view"
    );
    let mut pair = Pair::new(3);
    for k in 0..3 {
        assert!(
            alias_skip::marked(pair.a / BLOCK + k) && alias_skip::marked(pair.b / BLOCK + k),
            "both views marked"
        );
    }
    let a = pair.a / BLOCK;
    pair.drop_b();
    assert!(
        !(a..a + 3).any(alias_skip::marked),
        "one view left: not aliased"
    );
}

/// The same writes, through one view and through another of the same pages, tell the same pages
/// with the alias skip off and on; a write through the other view is told at the first.
#[test]
fn alias_skip_tells_the_same_pages() {
    let _reset = Reset;
    super::super::super::PORT_GUARD_IN_TEST.set(true);
    let mut told = Vec::new();
    for (four_k, skip) in [(false, false), (false, true), (true, false), (true, true)] {
        guard4k::FORCED.with(|f| f.set(four_k));
        alias_skip::FORCED.set(Some(skip));
        let lone = View::new(6);
        let pair = Pair::new(4);
        let (mut l, _) = lone.refresh(0);
        let (mut a, _) = pair.refresh(false, 0);
        let mut rounds = Vec::new();
        for round in 0..12u8 {
            for b in 0..6 {
                if (b + round as usize) % 3 == 0 {
                    lone.write(b, round);
                }
            }
            pair.write(round % 2 == 1, round as usize % 4, round);
            let (asked, p) = lone.refresh(l);
            l = asked;
            let (asked, q) = pair.refresh(false, a);
            a = asked;
            assert!(
                q.contains(&((round as usize % 4) * BLOCK / PAGE)),
                "4k={four_k} skip={skip} round {round}: {q:?}"
            );
            rounds.push((p, q));
        }
        told.push(rounds);
    }
    assert_eq!(told[0], told[1], "block guards: the same pages");
    assert_eq!(told[2], told[3], "4 KiB guards: the same pages");
}

#[test]
fn a_shared_claim_keeps_exclusive_ones_out_and_waits_for_them() {
    let pair = View::new(1);
    let block = pair.at / BLOCK;
    claim_narrow::ready(block, block + 1);
    let busy = move || BUSY.any(block, block + 1);
    let one = claim_narrow::SharedClaim::wait(block, busy).expect("room made");
    let two = claim_narrow::SharedClaim::wait(block, busy).expect("shared again");
    assert!(!busy_try(block), "no exclusive claim beside a shared one");
    assert!(!Claims::none().try_one(block));
    drop(one);
    assert!(!busy_try(block));
    drop(two);
    assert!(busy_try(block), "free once the shared holders go");
    // Held exclusive: a shared claim waits for it.
    let got = std::sync::atomic::AtomicBool::new(false);
    std::thread::scope(|s| {
        s.spawn(|| {
            let _c = claim_narrow::SharedClaim::wait(block, busy).expect("room made");
            got.store(true, std::sync::atomic::Ordering::SeqCst);
        });
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(
            !got.load(std::sync::atomic::Ordering::SeqCst),
            "the shared claim did not wait"
        );
        BUSY.clear(block);
    });
    assert!(got.into_inner());
}

/// (#88) 8 writers each writing their own 4 KiB page of two blocks while a refresher closes them
/// again and again, as the title's threads and the final flush do: the writers' time per write
/// (faults included) and the claim waits, `VICTORY_GUARD_CLAIM_NARROW` off and on, alternated.
/// `cargo test --release -p victory-gpu --lib bench_claim_narrow -- --ignored --nocapture`
#[test]
#[ignore]
fn bench_claim_narrow() {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
    const WRITERS: usize = 8;
    const WRITES: usize = 3000;
    let _reset = Reset;
    let view = View::new(2);
    for round in 0..4 {
        for narrow in [false, true] {
            let busy = AtomicU64::new(0);
            let done = AtomicBool::new(false);
            let start = std::sync::Barrier::new(WRITERS + 1);
            let setup = || {
                super::super::super::PORT_GUARD_IN_TEST.set(true);
                guard4k::FORCED.with(|f| f.set(true));
                claim_narrow::FORCED.set(Some(narrow));
            };
            setup();
            view.refresh(0);
            let before = claim_narrow::STATS[1].load(Relaxed);
            std::thread::scope(|s| {
                let (view, done, start, busy) = (&view, &done, &start, &busy);
                s.spawn(move || {
                    setup();
                    start.wait();
                    while !done.load(Relaxed) {
                        view.refresh(0);
                    }
                });
                std::thread::scope(|s| {
                    for t in 0..WRITERS {
                        s.spawn(move || {
                            setup();
                            start.wait();
                            let at = view.at + (t % 2) * BLOCK + (t / 2) * 4096 + 64;
                            let t0 = std::time::Instant::now();
                            for i in 0..WRITES {
                                // SAFETY: inside our live view; the guards' handler opens it.
                                unsafe { (at as *mut u64).write_volatile(i as u64) };
                                if i % 4 == 0 {
                                    std::thread::yield_now();
                                }
                            }
                            busy.fetch_add(t0.elapsed().as_nanos() as u64, Relaxed);
                        });
                    }
                });
                done.store(true, Relaxed);
            });
            eprintln!(
                "round {round} narrow={narrow}: {:.2} us a write (faults and yields), {} shared claims found another holder",
                busy.load(Relaxed) as f64 / ((WRITERS * WRITES) as f64 * 1000.0),
                claim_narrow::STATS[1].load(Relaxed) - before,
            );
        }
    }
}

/// (#88) 16 threads checking views whose blocks are written now and then (so the walk closes
/// blocks and asks for their aliases) while another thread maps and unmaps views (taking the
/// views' lock), as the runtime does: a check's time, `VICTORY_GUARD_ALIAS_SKIP` off and on.
/// `cargo test --release -p victory-gpu --lib bench_alias_skip -- --ignored --nocapture`
#[test]
#[ignore]
fn bench_alias_skip() {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
    const THREADS: usize = 16;
    const CHECKS: usize = 1500;
    let _reset = Reset;
    let views: Vec<View> = (0..THREADS).map(|_| View::new(16)).collect();
    // Many views in the map, as the world has, a few of them aliased.
    let others: Vec<View> = (0..400).map(|_| View::new(1)).collect();
    let _pair = Pair::new(2);
    for round in 0..4 {
        for skip in [false, true] {
            let busy = AtomicU64::new(0);
            let done = AtomicBool::new(false);
            let start = std::sync::Barrier::new(THREADS + 1);
            let alias_before = super::super::STATS[super::super::S::AliasUs as usize].load(Relaxed);
            let skipped = alias_skip::STATS.each_ref().map(|a| a.load(Relaxed));
            std::thread::scope(|s| {
                let (done, start, busy, views, others) = (&done, &start, &busy, &views, &others);
                s.spawn(move || {
                    start.wait();
                    let mut i = 0;
                    while !done.load(Relaxed) {
                        let o = &others[i % others.len()];
                        crate::inplace::views_gone(o.at, o.len);
                        crate::inplace::title_memory_view(o.at, o.len, o.section as usize, 0);
                        i += 1;
                        std::thread::yield_now();
                    }
                });
                std::thread::scope(|s| {
                    for (t, view) in views.iter().enumerate() {
                        s.spawn(move || {
                            super::super::super::PORT_GUARD_IN_TEST.set(true);
                            alias_skip::FORCED.set(Some(skip));
                            start.wait();
                            let t0 = std::time::Instant::now();
                            for i in 0..CHECKS {
                                view.write((i + t) % 16, i as u8);
                                view.write((i * 7 + t) % 16, i as u8);
                                let _ =
                                    super::super::super::writes(view.at, view.at + view.len, true);
                            }
                            busy.fetch_add(t0.elapsed().as_nanos() as u64, Relaxed);
                        });
                    }
                });
                done.store(true, Relaxed);
            });
            let checks = (THREADS * CHECKS) as f64;
            eprintln!(
                "round {round} skip={skip}: {:.2} us a check (two writes and a walk); alias lookups {:.3} us a check \
                 (VICTORY_FLUSH_PROF=1 to time), {} answered by the bits, {} took the lock",
                busy.load(Relaxed) as f64 / (checks * 1000.0),
                (super::super::STATS[super::super::S::AliasUs as usize].load(Relaxed) - alias_before) as f64 / checks,
                alias_skip::STATS[0].load(Relaxed) - skipped[0],
                alias_skip::STATS[1].load(Relaxed) - skipped[1],
            );
        }
    }
}

/// Pokes 4 KiB page `page` of block `block` of `view`.
fn poke(view: &View, block: usize, page: usize, value: u8) {
    // SAFETY: inside our live view; the guards' handler opens the page.
    unsafe { ((view.at + block * BLOCK + page * 4096 + 8) as *mut u8).write_volatile(value) };
}

/// Read-only or not, every 4 KiB page of `view`.
fn read_only(view: &View) -> Vec<bool> {
    (0..view.len / 4096).map(|k| !super::super::super::writable(view.at + k * 4096)).collect()
}

/// The same writes over neighbouring blocks tell the same pages, leave the same pages read-only
/// and fault again after, with `VICTORY_GUARD_4K_RUNS` off and on; on, the closes are joined.
#[test]
fn runs_of_4k_closes_tell_the_same_pages() {
    use std::sync::atomic::Ordering::Relaxed;
    let _reset = Reset;
    super::super::super::PORT_GUARD_IN_TEST.set(true);
    guard4k::FORCED.with(|f| f.set(true));
    let script: [&[(usize, usize)]; 5] = [
        &[(1, 15), (2, 0), (2, 7), (3, 3), (5, 9)],
        &[(0, 0), (0, 15), (1, 0)],
        &[(4, 4)],
        &[(1, 1), (2, 2), (3, 3), (4, 4), (5, 5), (0, 6)],
        &[(5, 15), (3, 0)],
    ];
    let mut seen = Vec::new();
    for runs in [false, true] {
        guard4k::RUNS_FORCED.set(Some(runs));
        let joined = guard4k::RUN_STATS[0].load(Relaxed);
        let view = View::new(6);
        let (asked, _) = view.refresh(0);
        let (mut since, _) = view.refresh(asked);
        let mut rounds = Vec::new();
        for (i, writes) in script.iter().enumerate() {
            for &(b, p) in *writes {
                poke(&view, b, p, i as u8);
            }
            let (asked, told) = view.refresh(since);
            since = asked;
            rounds.push((told, read_only(&view)));
        }
        // Every page read-only again: the next write faults and is told.
        poke(&view, 2, 7, 99);
        let (_, told) = view.refresh(since);
        assert!(told.contains(&(2 * BLOCK / PAGE)), "runs={runs}: {told:?}");
        rounds.push((told, read_only(&view)));
        if runs {
            assert!(guard4k::RUN_STATS[0].load(Relaxed) > joined, "no joined close");
        }
        seen.push(rounds);
    }
    assert_eq!(seen[0], seen[1], "the same pages told and read-only");
}

/// (#88) A walk over 16 neighbouring blocks with one or two pages written in each of 12:
/// `VirtualProtect` calls and walk time, `VICTORY_GUARD_4K_RUNS` off and on, alternated.
/// `cargo test --release -p victory-gpu --lib bench_4k_runs -- --ignored --nocapture`
#[test]
#[ignore]
fn bench_4k_runs() {
    const ROUNDS: usize = 400;
    let _reset = Reset;
    super::super::super::PORT_GUARD_IN_TEST.set(true);
    guard4k::FORCED.with(|f| f.set(true));
    let view = View::new(16);
    for round in 0..4 {
        for runs in [false, true] {
            guard4k::RUNS_FORCED.set(Some(runs));
            view.refresh(0);
            let calls = guard4k::CALLS.with(std::cell::Cell::get);
            let mut walk = std::time::Duration::ZERO;
            for i in 0..ROUNDS {
                for b in 2..14 {
                    poke(&view, b, (i + b) % 16, i as u8);
                    if b % 3 == 0 {
                        poke(&view, b, (i * 5 + b) % 16, i as u8);
                    }
                }
                let t = std::time::Instant::now();
                view.refresh(0);
                walk += t.elapsed();
            }
            eprintln!(
                "round {round} runs={runs}: {:.1} us a walk, {:.1} VirtualProtect calls a walk (faults' opens included)",
                walk.as_secs_f64() * 1e6 / ROUNDS as f64,
                (guard4k::CALLS.with(std::cell::Cell::get) - calls) as f64 / ROUNDS as f64,
            );
        }
    }
}
