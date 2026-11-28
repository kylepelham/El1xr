//! (worldstream) `VICTORY_ASK_OUTSIDE=1` (live; off by default): the
//! submission's write-watch asking under `VICTORY_WATCH_RESERVATION=1` makes
//! its reservation queries and kernel askings, and notes what they found,
//! without holding the journal's lock; the lock is taken to publish the
//! asking (its number, its ranges) alone, as the asking without the
//! reservation grouping (`ask_once`) already does.
//!
//! Held for the whole asking (`ask_once` in the preparing breakdown, 4-5 ms a
//! frame over 29 submissions), the lock stopped every texture refresh that
//! looked at the journal (`writes`, `asked`) on the recording and prepare
//! threads until the asking was done. Now they find no asking standing (the
//! last one ended when this one began, as before) and ask the kernel for
//! their own range, as they did between askings; what either finds is noted
//! in the same table of pages, so no write is lost to the other's reset.
//!
//! The same groups are asked with the same calls; the pages found are noted
//! with an asking number taken after the kernel calls (as `ask_once` does),
//! and the ranges answered for are the same. An asking ended meanwhile
//! (`ask_once_end`, [`ENDS`]) is published as ended, as the lock made it
//! before (the end waited for the asking, then cleared it).

use super::super::ask_split::{self as split, add, add_us};
use super::*;
use victory_core::settings::Live;

/// Whether the asking is made outside the journal's lock.
pub(in crate::watch) fn on() -> bool {
    static ON: Live = Live::flag("VICTORY_ASK_OUTSIDE");
    ON.get_bool()
}

/// Counts [`ask_once_end`] calls, so an asking made outside the lock knows one
/// came while it ran.
pub(in crate::watch) static ENDS: AtomicU64 = AtomicU64::new(0);

/// The asking of `pieces` (sorted, filtered 64 KiB page ranges), published
/// under the journal's lock at the end.
pub(in crate::watch) fn ask(pieces: &[(usize, usize)]) {
    let ends = ENDS.load(std::sync::atomic::Ordering::Acquire);
    let mut written: Vec<usize> = Vec::new();
    let hot = super::hot::Round::begin();
    let ahead = plan(pieces, |first, last| {
        if let Some(all) = hot.skipping(first, last) {
            written.extend(all);
            return true;
        }
        let before = written.len();
        let answered = asked_kernel(first, last, &mut written);
        if answered {
            hot.judge(first, last, written.len() - before);
        }
        answered
    });
    let now = next_asking();
    let noting = std::time::Instant::now();
    note_kernel_batch(&written, now);
    add_us(split::NOTE_US, noting);
    let locking = std::time::Instant::now();
    let mut j = JOURNAL.write().unwrap_or_else(|e| e.into_inner());
    add_us(split::LOCK_US, locking);
    publish(
        &mut j,
        now,
        pieces,
        ahead,
        ENDS.load(std::sync::atomic::Ordering::Acquire) != ends,
    );
    KEPT.store(j.kept, std::sync::atomic::Ordering::Release);
}

/// What `run` leaves in the journal, followed by `ask_once_end` if `ended`.
fn publish(
    j: &mut Journal,
    now: u64,
    pieces: &[(usize, usize)],
    ahead: Vec<(usize, usize)>,
    ended: bool,
) {
    j.ahead_asked = now;
    if ended {
        j.kept = false;
        j.ahead.clear();
        j.ordered = false;
    } else {
        j.kept = !pieces.is_empty();
        j.ahead = ahead;
        j.ordered = true;
    }
}

/// One kernel asking of `[first, last)` (64 KiB pages), its written 4 KiB
/// pages put into `written`; timed into the split line as `ask_kernel_found`.
fn asked_kernel(first: usize, last: usize, written: &mut Vec<usize>) -> bool {
    let asked = std::time::Instant::now();
    let Some(found) = kernel_written(first, last) else {
        return false;
    };
    add_us(split::KERNEL_US, asked);
    add(split::CALLS, 1);
    add(split::WALKED, ((last - first) * PAGE / KERNEL_PAGE) as u64);
    add(split::FOUND, found.len() as u64);
    written.extend(found);
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree,
    };

    /// Write-watched memory of `pages` 64 KiB pages, the given ones committed.
    struct Memory(usize, usize);

    impl Memory {
        fn new(pages: usize, committed: &[usize]) -> Self {
            // SAFETY: a fresh reservation with the write watch.
            let at = unsafe {
                VirtualAlloc(
                    std::ptr::null(),
                    pages * PAGE,
                    MEM_RESERVE | 0x200000,
                    PAGE_READWRITE,
                )
            } as usize;
            assert_ne!(at, 0);
            for &page in committed {
                // SAFETY: inside our reservation.
                let done = unsafe {
                    VirtualAlloc(
                        (at + page * PAGE) as *const _,
                        PAGE,
                        MEM_COMMIT,
                        PAGE_READWRITE,
                    )
                };
                assert_eq!(done as usize, at + page * PAGE);
            }
            Self(at, pages)
        }

        fn write(&self, page: usize, sub: usize) {
            // SAFETY: a committed page of ours.
            unsafe { ((self.0 + page * PAGE + sub * KERNEL_PAGE) as *mut u8).write_volatile(1) };
        }

        fn page(&self, page: usize) -> usize {
            self.0 / PAGE + page
        }
    }

    impl Drop for Memory {
        fn drop(&mut self) {
            title_memory_remapped(self.0, self.1 * PAGE);
            before_virtual_free(self.0, self.1 * PAGE);
            // SAFETY: our reservation.
            assert_ne!(unsafe { VirtualFree(self.0 as *mut _, 0, MEM_RELEASE) }, 0);
        }
    }

    /// The pages of `memory` found written since `since`, by page.
    fn found(memory: &Memory, since: u64) -> Vec<usize> {
        let first = memory.page(0);
        written_in(first, first + memory.1)
            .into_iter()
            .filter(|&(_, at)| at > since)
            .map(|(page, _)| page - first)
            .collect()
    }

    /// The same writes asked about under the lock (`run`) in one place and
    /// outside it in another, laid out alike: the same ranges answered for,
    /// the same pages found written, the asking standing alike; and a second
    /// asking finds nothing either way (the kernel's marks were reset).
    #[test]
    fn asking_outside_the_lock_answers_as_under_it() {
        let committed = [0, 1, 2, 5, 9, 40, 41];
        let (under, out) = (Memory::new(48, &committed), Memory::new(48, &committed));
        let pieces = |m: &Memory| -> Vec<(usize, usize)> {
            [(0, 1), (1, 3), (5, 6), (9, 10), (40, 42), (44, 45)]
                .iter()
                .map(|&(a, b)| (m.page(a), m.page(b)))
                .collect()
        };
        for m in [&under, &out] {
            for (page, sub) in [(0, 0), (2, 3), (5, 15), (40, 1), (41, 0)] {
                m.write(page, sub);
            }
        }
        let since = next_asking();
        let mut j = Journal {
            found: Vec::new(),
            ahead: Vec::new(),
            ordered: false,
            ahead_asked: 0,
            kept: false,
        };
        run(&mut j, &pieces(&under));
        let mut written = Vec::new();
        let ahead = plan(&pieces(&out), |first, last| {
            asked_kernel(first, last, &mut written)
        });
        let now = next_asking();
        note_kernel_batch(&written, now);
        let mut k = Journal {
            found: Vec::new(),
            ahead: Vec::new(),
            ordered: false,
            ahead_asked: 0,
            kept: false,
        };
        publish(&mut k, now, &pieces(&out), ahead, false);
        let relative = |m: &Memory, ahead: &[(usize, usize)]| -> Vec<(usize, usize)> {
            ahead
                .iter()
                .map(|&(a, b)| (a - m.page(0), b - m.page(0)))
                .collect()
        };
        assert_eq!(relative(&under, &j.ahead), relative(&out, &k.ahead));
        assert!(!k.ahead.is_empty());
        assert_eq!((j.kept, j.ordered), (k.kept, k.ordered));
        assert_eq!(found(&under, since), vec![0, 2, 5, 40, 41]);
        assert_eq!(found(&under, since), found(&out, since));
        for m in [&under, &out] {
            for page in [0, 2, 5, 40, 41] {
                assert!(covered(
                    if std::ptr::eq(m, &under) { &j } else { &k },
                    m.page(page),
                    m.page(page) + 1
                ));
            }
        }
        // Asked again: nothing new either way.
        let again = next_asking();
        run(&mut j, &pieces(&under));
        let mut written = Vec::new();
        plan(&pieces(&out), |first, last| {
            asked_kernel(first, last, &mut written)
        });
        assert!(written.is_empty());
        assert!(found(&under, again).is_empty());
    }

    /// An asking ended while it ran outside the lock is published ended, as the
    /// end waiting for the lock left it; the pages it found stay noted.
    #[test]
    fn an_asking_ended_meanwhile_is_published_ended() {
        let mut j = Journal {
            found: Vec::new(),
            ahead: vec![(1, 2)],
            ordered: true,
            ahead_asked: 0,
            kept: true,
        };
        publish(&mut j, 7, &[(3, 4)], vec![(3, 4)], true);
        assert!(!j.kept && j.ahead.is_empty() && !j.ordered);
        assert_eq!(j.ahead_asked, 7);
        publish(&mut j, 8, &[(3, 4)], vec![(3, 4)], false);
        assert!(j.kept && j.ordered);
        assert_eq!(j.ahead, vec![(3, 4)]);
        let before = ENDS.load(std::sync::atomic::Ordering::Acquire);
        ask_once_end();
        if reservation::enabled() || ask_once_active() {
            assert!(ENDS.load(std::sync::atomic::Ordering::Acquire) > before);
        }
    }
}
