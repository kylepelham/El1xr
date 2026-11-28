//! (feedcard, #88) `VICTORY_GUARD_HOLE_MARKS=1|2` (Live, off by default): the uncommitted runs a
//! guard check finds are marked block by block in a lock-free table for the rest of the frame,
//! so the checks of the other refresh threads, and the walks, take them with one atomic load
//! instead of a `VirtualQuery`.
//!
//! Why: the final flush's refreshes are the submitting thread's longest wait (b5 critpath r9:
//! 4.1-4.6 ms a frame behind `victory-flush-beside`), and the checks before the guard walk are a
//! third of their time: part Q (`train_b5q_cpu/02_cpu.log`) 2,110 checks a frame, 1,900 settled
//! there, with about 9,000 hole queries in 12-14 ms of all threads. `VICTORY_GUARD_HOLES_KEPT`
//! kept the same runs in one `RwLock<BTreeMap>`, and every commit of the title's host heap
//! (`xmem` `title_pages_committed`, thousands a frame) took its write lock and threw away the
//! inserts in flight: the hole queries went from 12 ms to 65-90 ms and the refreshes from 6.1 to
//! 9-10 ms. Here nothing is shared but per-block words: a forget touches only the blocks it
//! names, and a lookup is one load.
//!
//! Each block's word holds a version (high 32 bits, bumped by every forget of the block) and a
//! frame tag (low 32 bits, the frame + 1 it was found uncommitted in; 0: not known). A check reads
//! the words of the blocks it may mark before it asks Windows, and marks them after with a
//! compare-exchange against what it read: a forget in between (the runtime committed or mapped
//! there) changed the version, so the stale answer is not kept. A forget after the mark clears
//! it. `=1` marks reserved runs only (as `HOLES_KEPT`); `=2` free runs too (only the runtime maps
//! or allocates title memory there, and it tells us; the host's own allocations are above the
//! title's addresses). Not in the cache key: no shader or pipeline output.

use std::sync::atomic::Ordering::{AcqRel, Acquire, Relaxed, SeqCst};
use std::sync::atomic::{AtomicPtr, AtomicU64};

use super::{PART, Part, part_of};

/// The marks, by block (`super::BLOCK` is a journal page), in parts made on first mark.
static MARKS: [AtomicPtr<Part>; 1 << 16] =
    [const { AtomicPtr::new(std::ptr::null_mut()) }; 1 << 16];

/// The most blocks one answer marks: the checks ask about a refresh's blocks (a 1 MiB piece is
/// 16), so a longer run is marked a piece at a time, one query each.
pub(super) const MAX_RUN: usize = 64;

const TAG: u64 = u32::MAX as u64;
const VERSION: u64 = 1 << 32;

static MODE: victory_core::settings::Live =
    victory_core::settings::Live::new("VICTORY_GUARD_HOLE_MARKS", |v| match v {
        Some("1") => 1,
        Some("2") => 2,
        _ => 0,
    });

#[cfg(test)]
thread_local! {
    pub(super) static FORCED: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
}

/// The frame the marks are for: [`crate::frame::current`] (in tests, plus this thread's shift).
pub(super) fn frame() -> u64 {
    #[cfg(test)]
    return crate::frame::current() + SHIFT.with(std::cell::Cell::get);
    #[cfg(not(test))]
    crate::frame::current()
}

#[cfg(test)]
thread_local! {
    static SHIFT: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// (Tests) This thread's checks take the next frame as begun.
#[cfg(test)]
pub(super) fn next_frame_for_test() {
    SHIFT.with(|s| s.set(s.get() + 1));
}

/// (Tests) This thread's checks take `shift` frames as begun since the real frame.
#[cfg(test)]
pub(super) fn frames_on_for_test(shift: u64) {
    SHIFT.with(|s| s.set(shift));
}

/// 0 off, 1 reserved runs, 2 free runs too.
pub(super) fn mode() -> u64 {
    #[cfg(test)]
    if let Some(forced) = FORCED.with(std::cell::Cell::get) {
        return forced;
    }
    MODE.get()
}

/// Since the last frame line: answers from the marks, queries made, runs marked, runs found
/// free and not marked (`=1`), marks lost to a forget while asking.
pub(super) static STATS: [AtomicU64; 5] = [const { AtomicU64::new(0) }; 5];
pub(super) const HITS: usize = 0;
pub(super) const QUERIES: usize = 1;
pub(super) const MARKED: usize = 2;
pub(super) const FREE: usize = 3;
pub(super) const LOST: usize = 4;

fn tag(frame: u64) -> u64 {
    (frame.wrapping_add(1) & TAG).max(1)
}

fn word(block: usize, make: bool) -> Option<&'static AtomicU64> {
    part_of(&MARKS, block, make).map(|part| &part.0[block % PART])
}

/// The end of the marked run at `block` in `frame` (`block` itself if it is not marked), looked
/// at up to [`MAX_RUN`] blocks on.
fn marked_end(block: usize, frame: u64) -> usize {
    let t = tag(frame);
    let mut b = block;
    while b < block + MAX_RUN && word(b, false).is_some_and(|w| w.load(Acquire) & TAG == t) {
        b += 1;
    }
    b
}

/// [`super::uncommitted_end`] of `block` for `frame`, from the marks or by `span` (the end of the
/// uncommitted run at `block` and whether it is reserved), marking what it found. `mode` as
/// [`mode`] (1 or 2).
pub(super) fn end(
    block: usize,
    frame: u64,
    mode: u64,
    span: impl FnOnce(usize) -> (usize, bool),
) -> usize {
    let known = marked_end(block, frame);
    if known > block {
        STATS[HITS].fetch_add(1, Relaxed);
        return known;
    }
    // The words are made before they are read, so a forget from here on finds them and bumps
    // them; one before saw no word, and the commit it tells of is before this asking.
    let mut before = [0u64; MAX_RUN];
    for (i, seen) in before.iter_mut().enumerate() {
        *seen = word(block + i, true).map_or(0, |w| w.load(Acquire));
    }
    std::sync::atomic::fence(SeqCst);
    let (end, reserved) = span(block);
    STATS[QUERIES].fetch_add(1, Relaxed);
    if end <= block {
        return end;
    }
    if !reserved && mode < 2 {
        STATS[FREE].fetch_add(1, Relaxed);
        return end;
    }
    let t = tag(frame);
    let stop = end.min(block + MAX_RUN);
    for b in block..stop {
        let seen = before[b - block];
        let Some(w) = word(b, true) else {
            return end;
        };
        let mark = (seen & !TAG) | t;
        if w.compare_exchange(seen, mark, AcqRel, Acquire).is_err() {
            // A forget since it was read (or another check marked it): leave it.
            if w.load(Acquire) & TAG != t {
                STATS[LOST].fetch_add(1, Relaxed);
            }
        }
    }
    STATS[MARKED].fetch_add(1, Relaxed);
    end
}

/// The runtime committed, mapped or unmapped `[at, at + len)`: its blocks' marks are dropped and
/// their versions bumped, so an answer asked before is not marked after.
pub(super) fn forget(at: usize, len: usize) {
    use super::BLOCK;
    let (first, last) = (at / BLOCK, at.saturating_add(len).div_ceil(BLOCK));
    std::sync::atomic::fence(SeqCst);
    let mut block = first;
    while block < last {
        let stop = ((block / PART + 1) * PART).min(last);
        if let Some(part) = part_of(&MARKS, block, false) {
            for b in block..stop {
                let w = &part.0[b % PART];
                let _ = w.fetch_update(AcqRel, Acquire, |v| Some((v & !TAG).wrapping_add(VERSION)));
            }
        }
        block = stop;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Blocks no other test marks: a reservation of our own.
    fn blocks(n: usize) -> usize {
        use windows_sys::Win32::System::Memory::{MEM_RESERVE, PAGE_NOACCESS, VirtualAlloc};
        // SAFETY: a fresh reservation, never committed nor freed (a few blocks per test).
        let at = unsafe {
            VirtualAlloc(
                std::ptr::null(),
                n * super::super::BLOCK,
                MEM_RESERVE,
                PAGE_NOACCESS,
            )
        };
        assert!(!at.is_null());
        at as usize / super::super::BLOCK
    }

    #[test]
    fn marks_hold_for_their_frame_and_where_nothing_was_told() {
        let b = blocks(MAX_RUN * 3);
        let mut asked = 0;
        let mut ask = |end: usize, reserved: bool| {
            asked += 1;
            (end, reserved)
        };
        assert_eq!(end(b, 7, 1, |_| ask(b + 4, true)), b + 4);
        assert_eq!(end(b + 2, 7, 1, |_| ask(b + 9, true)), b + 4, "marked");
        assert_eq!(
            end(b + 2, 8, 1, |_| ask(b + 3, true)),
            b + 3,
            "a new frame asks again"
        );
        assert_eq!(
            end(b + 2, 7, 1, |_| ask(b + 9, true)),
            b + 9,
            "an older frame's marks are gone"
        );
        forget((b + 5) * super::super::BLOCK, 1);
        assert_eq!(
            end(b + 5, 7, 1, |_| ask(b + 5, false)),
            b + 5,
            "forgotten where told"
        );
        assert_eq!(
            end(b + 6, 7, 1, |_| ask(b + 99, true)),
            b + 9,
            "the rest is kept, without a query"
        );
        assert_eq!(end(b + 20, 7, 1, |_| ask(b + 22, false)), b + 22);
        assert_eq!(
            end(b + 20, 7, 1, |_| ask(b + 22, false)),
            b + 22,
            "=1: a free run is not marked"
        );
        assert_eq!(end(b + 20, 7, 2, |_| ask(b + 22, false)), b + 22);
        assert_eq!(
            end(b + 20, 7, 2, |_| ask(b + 99, false)),
            b + 22,
            "=2: a free run is marked"
        );
        // A long run is marked MAX_RUN blocks at a time.
        let far = b + 2 * MAX_RUN + 10;
        assert_eq!(end(b + 30, 7, 1, |_| ask(far, true)), far);
        assert_eq!(end(b + 31, 7, 1, |_| ask(far, true)), b + 30 + MAX_RUN);
        assert_eq!(end(b + 30 + MAX_RUN, 7, 1, |_| ask(far, true)), far);
        assert_eq!(asked, 9);
    }

    #[test]
    fn an_answer_told_of_while_asked_is_not_marked() {
        let b = blocks(16);
        let at = (b + 3) * super::super::BLOCK;
        assert_eq!(
            end(b, 3, 1, |_| {
                forget(at, 1);
                (b + 8, true)
            }),
            b + 8
        );
        assert_eq!(
            marked_end(b, 3),
            b + 3,
            "the blocks before the told one are marked"
        );
        assert_eq!(marked_end(b + 3, 3), b + 3, "the told block is not");
        assert_eq!(marked_end(b + 4, 3), b + 8, "the ones after it are");
    }

    /// Commits and checks racing: once a forget has returned, no check marks the block as a hole
    /// on an answer from before it.
    #[test]
    fn racing_forgets_leave_no_stale_mark() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst};
        let b = blocks(MAX_RUN);
        // The truth: blocks [b, b + committed) are committed, the rest of the run is a hole.
        let committed = AtomicUsize::new(0);
        let stop = AtomicBool::new(false);
        let stale = AtomicUsize::new(0);
        std::thread::scope(|s| {
            for _ in 0..6 {
                s.spawn(|| {
                    while !stop.load(SeqCst) {
                        let at = b + (committed.load(SeqCst) % 8);
                        end(at, 1, 1, |blk| {
                            let c = committed.load(SeqCst);
                            if blk >= b + c {
                                (b + MAX_RUN, true)
                            } else {
                                (blk, false)
                            }
                        });
                    }
                });
            }
            s.spawn(|| {
                for c in 1..=MAX_RUN {
                    // Commit block c - 1 (the truth first, as the kernel does), then tell.
                    committed.store(c, SeqCst);
                    forget((b + c - 1) * super::super::BLOCK, super::super::BLOCK);
                    std::thread::yield_now();
                    if marked_end(b, 1) > b + c - 1 || (0..c).any(|i| marked_end(b + i, 1) > b + i)
                    {
                        stale.fetch_add(1, SeqCst);
                    }
                }
                stop.store(true, SeqCst);
            });
        });
        assert_eq!(
            stale.load(SeqCst),
            0,
            "a committed block was left marked as a hole"
        );
    }

    #[test]
    fn the_switch_is_live() {
        if std::env::var_os("VICTORY_GUARD_HOLE_MARKS").is_some() {
            return;
        }
        assert_eq!(mode(), 0, "off by default");
        victory_core::settings::set("VICTORY_GUARD_HOLE_MARKS", "2").unwrap();
        let now = mode();
        victory_core::settings::set("VICTORY_GUARD_HOLE_MARKS", "").unwrap();
        assert_eq!(now, 2, "a change takes effect while running");
        assert_eq!(mode(), 0);
    }
}
