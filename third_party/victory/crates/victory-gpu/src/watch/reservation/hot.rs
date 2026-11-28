//! (worldstream) `VICTORY_HOT_SKIP=1` (live; off by default): a range the
//! submission's write-watch asking (`VICTORY_WATCH_RESERVATION=1`) found
//! wholly written, every 4 KiB page of it, in two askings in a row is
//! answered "all written" for the next [`SKIPS`] askings without the kernel
//! (no walk, no reset of its pages), then asked again. The first kernel
//! answer after the skips holds the writes of all of them, so it is not
//! judged; the asking after it is.
//!
//! The answer is a superset of the truth: every 4 KiB page of the range is
//! noted written at that asking, as if the kernel had found it so, and the
//! copies of it take all of it, as they did when the title rewrote it whole.
//! A call that resets written pages changes and flushes each page's entry
//! on every core: a range rewritten whole costs the most to ask about, and
//! asking buys nothing there.
//!
//! Ported from sixth-star d130c100 (ihaterockstargames), which keys the
//! skips on the ranges `writes` asks about one by one; here they are the
//! groups the reservation asking makes, and only ranges of up to
//! [`MOST_PAGES`] are judged (a skip notes each of their 4 KiB pages).

use super::*;
use victory_core::settings::Live;

/// Whether ranges rewritten whole skip the kernel.
fn on() -> bool {
    static ON: Live = Live::flag("VICTORY_HOT_SKIP");
    ON.get_bool()
}

/// A range the title rewrites whole.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Hot {
    /// Found wholly written at this round; once more in a row and it is
    /// skipped.
    Warm(u64),
    /// Answered without the kernel this many more times.
    Skip(u32),
    /// The skips are over; the kernel's answer at this round took the writes
    /// of all of them, so it says nothing about one round. The next decides.
    Probe(u64),
}

/// Askings of a hot range answered without the kernel before it is asked
/// again.
const SKIPS: u32 = 8;
/// More hot ranges than this and they are all forgotten (only a shortcut).
const MAX: usize = 4096;
/// The longest range judged, in 64 KiB pages (16 MiB: 4,096 pages of 4 KiB
/// noted per skip).
const MOST_PAGES: usize = (16 << 20) / PAGE;

static TABLE: Mutex<Option<std::collections::HashMap<(usize, usize), Hot>>> = Mutex::new(None);

/// Numbers the reservation askings for [`Hot`].
static ROUNDS: AtomicU64 = AtomicU64::new(0);

/// Since the last line: ranges answered without the kernel, their 4 KiB
/// pages, ranges judged.
static TOLD: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

/// What becomes of a range's state after a kernel answer at `round` that
/// found it wholly written (`whole`) or not (sixth-star `next_hot`).
fn next_hot(state: Option<Hot>, whole: bool, round: u64) -> Option<Hot> {
    match state {
        Some(Hot::Skip(n)) if n > 0 => Some(Hot::Skip(n)),
        Some(Hot::Skip(_)) => Some(Hot::Probe(round)),
        Some(Hot::Probe(at)) | Some(Hot::Warm(at)) if at == round => state,
        Some(Hot::Probe(_)) | Some(Hot::Warm(_)) => whole.then_some(Hot::Skip(SKIPS)),
        None => whole.then_some(Hot::Warm(round)),
    }
}

/// One reservation asking's view of the hot ranges.
pub(super) struct Round {
    /// 0: the switch is off.
    round: u64,
}

impl Round {
    pub(super) fn begin() -> Self {
        let round = if on() {
            ROUNDS.fetch_add(1, Relaxed) + 1
        } else {
            0
        };
        Self { round }
    }

    /// If `[first, last)` (64 KiB pages) is answered without the kernel at
    /// this asking: every 4 KiB page address of it, to be noted as the
    /// kernel's find.
    pub(super) fn skipping(&self, first: usize, last: usize) -> Option<Vec<usize>> {
        if self.round == 0 || last - first > MOST_PAGES {
            return None;
        }
        {
            let mut table = TABLE.lock().unwrap_or_else(|e| e.into_inner());
            let Some(Hot::Skip(n)) = table.as_mut()?.get_mut(&(first, last)) else {
                return None;
            };
            if *n == 0 {
                return None;
            }
            *n -= 1;
        }
        let all: Vec<usize> = (first * PAGE..last * PAGE).step_by(KERNEL_PAGE).collect();
        TOLD[0].fetch_add(1, Relaxed);
        TOLD[1].fetch_add(all.len() as u64, Relaxed);
        Some(all)
    }

    /// The kernel answered for `[first, last)` with `found` written 4 KiB
    /// pages.
    pub(super) fn judge(&self, first: usize, last: usize, found: usize) {
        if self.round == 0 || last - first > MOST_PAGES {
            return;
        }
        let whole = found >= (last - first) * (PAGE / KERNEL_PAGE);
        let mut held = TABLE.lock().unwrap_or_else(|e| e.into_inner());
        let table = held.get_or_insert_with(Default::default);
        let state = table.get(&(first, last)).copied();
        if state.is_none() && !whole {
            return;
        }
        TOLD[2].fetch_add(1, Relaxed);
        match next_hot(state, whole, self.round) {
            Some(next) => {
                if state.is_none() && table.len() >= MAX {
                    table.clear();
                }
                table.insert((first, last), next);
            }
            None => {
                table.remove(&(first, last));
            }
        }
    }
}

/// One line on the frame-meter cadence while the switch is on.
pub(super) fn line(frames: f64) {
    if !on() || frames <= 0.0 {
        return;
    }
    let [skipped, pages, judged] = TOLD.each_ref().map(|n| n.swap(0, Relaxed) as f64 / frames);
    let known = TABLE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map_or(0, |t| t.len());
    tracing::info!(
        "write-watch ranges rewritten whole per frame (VICTORY_HOT_SKIP=1): {skipped:.1} answered without the kernel ({pages:.0} pages of 4 KiB noted), {judged:.1} judged; {known} ranges known"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree,
    };

    #[test]
    fn hot_states_follow_whole_writes() {
        assert_eq!(next_hot(None, true, 5), Some(Hot::Warm(5)));
        assert_eq!(next_hot(None, false, 5), None);
        assert_eq!(next_hot(Some(Hot::Warm(5)), true, 5), Some(Hot::Warm(5)));
        assert_eq!(
            next_hot(Some(Hot::Warm(5)), true, 6),
            Some(Hot::Skip(SKIPS))
        );
        assert_eq!(next_hot(Some(Hot::Warm(5)), false, 6), None);
        assert_eq!(next_hot(Some(Hot::Skip(0)), true, 9), Some(Hot::Probe(9)));
        assert_eq!(next_hot(Some(Hot::Skip(0)), false, 9), Some(Hot::Probe(9)));
        assert_eq!(next_hot(Some(Hot::Probe(9)), true, 9), Some(Hot::Probe(9)));
        assert_eq!(
            next_hot(Some(Hot::Probe(9)), true, 10),
            Some(Hot::Skip(SKIPS))
        );
        assert_eq!(next_hot(Some(Hot::Probe(9)), false, 10), None);
    }

    /// The reservation asking's kernel call for one range, through a round
    /// as `run` and `outside::ask` make it: the 4 KiB pages it reports.
    fn ask(round: &Round, first: usize, last: usize) -> Vec<usize> {
        if let Some(all) = round.skipping(first, last) {
            return all;
        }
        let found = kernel_written(first, last).expect("write-watched");
        round.judge(first, last, found.len());
        found
    }

    fn round(n: u64) -> Round {
        Round { round: n }
    }

    /// Rewritten whole twice: the next askings report every page without the
    /// kernel, whose marks stay; after the skips the kernel holds a write
    /// made during them; a range then written in part is asked each time.
    /// With the switch off (round 0) the kernel answers every time.
    #[test]
    fn a_range_written_whole_skips_the_kernel_and_is_asked_again() {
        const PAGES: usize = 4;
        // SAFETY: a fresh write-watched allocation, freed at the end.
        let p = unsafe {
            VirtualAlloc(
                std::ptr::null(),
                PAGES * PAGE,
                MEM_RESERVE | MEM_COMMIT | 0x20_0000,
                PAGE_READWRITE,
            )
        } as usize;
        assert_ne!(p, 0);
        let (first, last) = (p / PAGE, p / PAGE + PAGES);
        let every: Vec<usize> = (p..p + PAGES * PAGE).step_by(KERNEL_PAGE).collect();
        let write_all = || {
            for &at in &every {
                // SAFETY: committed pages of ours.
                unsafe { (at as *mut u8).write_volatile(1) };
            }
        };
        let mut n = 1_000_000;
        let mut next = || {
            n += 1;
            round(n)
        };
        // Off: always the kernel.
        write_all();
        assert_eq!(ask(&round(0), first, last), every);
        write_all();
        assert_eq!(ask(&round(0), first, last), every);
        assert!(ask(&round(0), first, last).is_empty());
        // Whole twice in a row.
        write_all();
        assert_eq!(ask(&next(), first, last), every);
        write_all();
        assert_eq!(ask(&next(), first, last), every);
        // SAFETY: committed page of ours.
        unsafe { ((p + PAGE + 8) as *mut u8).write_volatile(2) };
        for _ in 0..SKIPS {
            assert_eq!(ask(&next(), first, last), every);
        }
        // The kernel still holds the write made during the skips.
        let probe = ask(&next(), first, last);
        assert_eq!(probe, vec![p + PAGE]);
        // Written in part: asked, and no longer hot.
        // SAFETY: committed page of ours.
        unsafe { ((p + 2 * PAGE) as *mut u8).write_volatile(3) };
        assert_eq!(ask(&next(), first, last), vec![p + 2 * PAGE]);
        assert!(ask(&next(), first, last).is_empty());
        assert!(
            !TABLE
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                .is_some_and(|t| t.contains_key(&(first, last)))
        );
        // SAFETY: our allocation.
        unsafe { VirtualFree(p as *mut _, 0, MEM_RELEASE) };
    }
}
