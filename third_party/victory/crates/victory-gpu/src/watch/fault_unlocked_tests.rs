//! (feedcard, #88) `VICTORY_FAULT_UNLOCKED`: a write fault beside a walk closing its block does
//! not wait for the walk's claim, and loses no write; microbench.

use super::super::super::{BLOCK, BUSY, busy_try, fault_unlocked, guard4k};
use super::{Reset, View, racing_refreshes_lose_nothing_all};

fn setup(unlocked: bool) {
    super::super::super::PORT_GUARD_IN_TEST.set(true);
    guard4k::FORCED.with(|f| f.set(true));
    guard4k::STAMP_FRESH_FORCED.set(Some(true));
    fault_unlocked::FORCED.set(Some(unlocked));
}

/// A walk holds the block's claim and is closing it; another thread writes a page of the block.
/// Whether the write was done before the walk let go of the claim, and what the walk's close
/// end said; then whether the next refresh is told the page.
fn write_beside_a_close(unlocked: bool) -> (bool, bool, bool) {
    use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
    let _reset = Reset;
    setup(unlocked);
    let view = View::new(1);
    let (asked, _) = view.refresh(0);
    let block = view.at / BLOCK;
    // Not clean, as a block a walk closes: a page opened under the claim as before.
    view.write(0, 1);
    assert!(busy_try(block), "nobody else holds a fresh block");
    let closing = fault_unlocked::Closing::begin(block);
    let done = AtomicBool::new(false);
    let (written_early, beside) = std::thread::scope(|s| {
        let (view, done) = (&view, &done);
        s.spawn(move || {
            setup(unlocked);
            // SAFETY: inside our live view; the guards' handler opens it.
            unsafe { ((view.at + 3 * 4096 + 8) as *mut u64).write_volatile(7) };
            done.store(true, SeqCst);
        });
        let until = std::time::Instant::now() + std::time::Duration::from_millis(300);
        while !done.load(SeqCst) && std::time::Instant::now() < until {
            std::thread::yield_now();
        }
        let early = done.load(SeqCst);
        let beside = closing.end();
        BUSY.clear(block);
        (early, beside)
    });
    let (_, pages) = view.refresh(asked);
    (written_early, beside, pages == [0])
}

#[test]
fn a_fault_beside_a_closing_walk_does_not_wait() {
    let (early, beside, told) = write_beside_a_close(true);
    assert!(early, "the write waited for the walk's claim");
    assert!(
        beside,
        "the close end did not see the page opened beside it"
    );
    assert!(told, "the next refresh was not told the page");
}

#[test]
fn without_the_switch_the_fault_waits_for_the_claim() {
    let (early, beside, told) = write_beside_a_close(false);
    assert!(!early, "the write was done while the walk held the claim");
    assert!(!beside);
    assert!(told, "the next refresh was not told the page");
}

/// Writers and refreshers racing on the same view (4 KiB guards), faults beside closes: no write
/// lost, as without the switch.
#[test]
fn racing_refreshes_lose_nothing_unlocked() {
    let before = fault_unlocked::STATS[0].load(std::sync::atomic::Ordering::Relaxed);
    for publish_try in [false, true] {
        let missed = racing_refreshes_lose_nothing_all(publish_try, true, true, false, true);
        assert_eq!(
            missed, 0,
            "VICTORY_FAULT_UNLOCKED=1, PUBLISH_TRY={publish_try}"
        );
    }
    eprintln!(
        "faults opened beside a close: {}",
        fault_unlocked::STATS[0].load(std::sync::atomic::Ordering::Relaxed) - before
    );
}

/// 8 writers each writing their own 4 KiB page of two blocks while a refresher closes them again
/// and again, as the title's threads and the final flush do: the time of a write that faulted, `VICTORY_FAULT_UNLOCKED` off and on, alternated, with `VICTORY_GUARD_CLAIM_NARROW` on
/// (faults do not wait for each other either way).
/// `cargo test --release -p victory-gpu --lib bench_fault_unlocked -- --ignored --nocapture`
#[test]
#[ignore]
fn bench_fault_unlocked() {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
    const WRITERS: usize = 8;
    const WRITES: usize = 3000;
    let _reset = Reset;
    let view = View::new(2);
    for round in 0..4 {
        for unlocked in [false, true] {
            // Writes that took 1 us or more (they faulted): how many, nanoseconds, the longest.
            let slow = [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)];
            let done = AtomicBool::new(false);
            let start = std::sync::Barrier::new(WRITERS + 1);
            let setup = || {
                setup(unlocked);
                super::super::super::claim_narrow::FORCED.set(Some(true));
            };
            setup();
            view.refresh(0);
            let before = fault_unlocked::STATS.each_ref().map(|a| a.load(Relaxed));
            std::thread::scope(|s| {
                let (view, done, start, slow) = (&view, &done, &start, &slow);
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
                            for i in 0..WRITES {
                                let t0 = std::time::Instant::now();
                                // SAFETY: inside our live view; the guards' handler opens it.
                                unsafe { (at as *mut u64).write_volatile(i as u64) };
                                let ns = t0.elapsed().as_nanos() as u64;
                                if ns >= 1000 {
                                    slow[0].fetch_add(1, Relaxed);
                                    slow[1].fetch_add(ns, Relaxed);
                                    slow[2].fetch_max(ns, Relaxed);
                                }
                                // Time for the refresher to close the page again.
                                let pause = std::time::Instant::now();
                                while pause.elapsed() < std::time::Duration::from_micros(20) {
                                    std::hint::spin_loop();
                                }
                            }
                        });
                    }
                });
                done.store(true, Relaxed);
            });
            let after = fault_unlocked::STATS.each_ref().map(|a| a.load(Relaxed));
            eprintln!(
                "round {round} unlocked={unlocked}: {} faulting writes, {:.2} us each, longest {:.1} us; {} faults beside \
                 a close, {} closes waited for them, {} left not clean",
                slow[0].load(Relaxed),
                slow[1].load(Relaxed) as f64 / (slow[0].load(Relaxed).max(1) as f64 * 1000.0),
                slow[2].load(Relaxed) as f64 / 1000.0,
                after[0] - before[0],
                after[3] - before[3],
                after[5] - before[5],
            );
        }
    }
}
