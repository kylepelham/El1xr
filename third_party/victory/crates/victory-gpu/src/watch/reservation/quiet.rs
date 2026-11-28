//! (critpath, #88) `VICTORY_ASK_QUIET_SKIP=<askings>` (live; 0 or unset: off): the
//! submission's write-watch asking (`VICTORY_WATCH_RESERVATION=1`) skips the kernel for a
//! range every 64 KiB page of which the kernel found clean at its last `<askings>` askings,
//! as long as each was asked within the last `VICTORY_ASK_QUIET_EVERY` (live, default 4)
//! submissions and in this frame. The range is answered from the journal, as a range asked
//! ahead is: the kernel's marks of its pages stay and the next asking that walks it finds
//! what was written meanwhile.
//!
//! `VICTORY_ASK_QUIET` is read only by the asking without the reservation grouping, which
//! the launcher's `VICTORY_WATCH_RESERVATION=1` replaces, so it did nothing there. This is
//! the reservation path's own, and a switch of its own so the old one keeps its meaning.
//!
//! No write is lost: only a kernel call with the reset clears a page's mark, and every such
//! call notes what it found in the journal. A write is found late by at most
//! `VICTORY_ASK_QUIET_EVERY - 1` submissions, never past the first asking of the next frame,
//! which walks everything it is given. A page found written is asked at every submission
//! until it is clean `<askings>` times in a row again. Memory remapped, freed or written by
//! DirectStorage or the kernel ([`forget`]) is asked again at once.
//!
//! b5o (quiet run, launcher set): 250 kernel askings a frame walked 361k pages of 4 KiB and
//! found 1.9k written, 1.9 ms of `ask_once`'s 3.4 ms. The askings cannot be spread over
//! threads: a microbenchmark of the same calls took 0.96 ms on one thread, 2.6 ms on four.

use super::*;
use std::sync::atomic::AtomicU64;
use victory_core::settings::Live;

/// Clean askings in a row before a page may be skipped (0: off).
fn need() -> u64 {
    static N: Live = Live::new("VICTORY_ASK_QUIET_SKIP", |v| {
        v.and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(0)
            .min(255)
    });
    N.get()
}

/// Submissions within which a skipped page was walked last (1: never skipped).
fn every() -> u64 {
    static R: Live = Live::new("VICTORY_ASK_QUIET_EVERY", |v| {
        v.and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(4)
            .max(1)
    });
    R.get()
}

/// 64 KiB pages to a block of the table (1 MiB).
const BLOCK_PAGES: usize = 16;
/// More blocks than this (64 GiB) and the table is forgotten (only a shortcut).
const MOST_BLOCKS: usize = 1 << 16;

/// Of each 64 KiB page of a block: the asking that last walked it (0: none known), and
/// the askings in a row that found it clean.
#[derive(Clone, Copy, Default)]
struct Block {
    walked: [u64; BLOCK_PAGES],
    clean: [u8; BLOCK_PAGES],
}

/// FxHash's step for block numbers.
#[derive(Default)]
struct Fx(u64);

impl std::hash::Hasher for Fx {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.write_u64(u64::from(b));
        }
    }

    fn write_usize(&mut self, n: usize) {
        self.write_u64(n as u64);
    }

    fn write_u64(&mut self, n: u64) {
        self.0 = (self.0.rotate_left(5) ^ n).wrapping_mul(0x517c_c1b7_2722_0a95);
    }
}

type Blocks = std::collections::HashMap<usize, Block, std::hash::BuildHasherDefault<Fx>>;

struct Table {
    blocks: Blocks,
    /// The frame the last asking was in, and the first asking of it.
    frame: u64,
    frame_first: u64,
}

#[cfg(not(test))]
static TABLE: Mutex<Option<Table>> = Mutex::new(None);

/// Numbers the askings (never 0).
static ASKINGS: AtomicU64 = AtomicU64::new(0);

/// Since the last line: ranges skipped, their 4 KiB pages, kernel answers judged, ranges
/// forgotten.
static TOLD: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

#[cfg(not(test))]
fn table() -> std::sync::MutexGuard<'static, Option<Table>> {
    TABLE.lock().unwrap_or_else(|e| e.into_inner())
}

/// Tests: a table per thread, so the askings of other tests (switch off: the table
/// dropped) do not touch it.
#[cfg(test)]
fn table() -> std::sync::MutexGuard<'static, Option<Table>> {
    thread_local! {
        static OWN: &'static Mutex<Option<Table>> = Box::leak(Box::new(Mutex::new(None)));
    }
    OWN.with(|own| *own)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// One reservation asking's view of the quiet pages.
pub(super) struct Round {
    /// 0: the switch is off.
    at: u64,
    need: u64,
    every: u64,
    frame_first: u64,
}

impl Round {
    pub(super) fn begin() -> Self {
        let need = need();
        if need == 0 {
            // What a later turn on finds must not be older than its remaps.
            let mut held = table();
            if held.is_some() {
                *held = None;
            }
            return Self::off();
        }
        let at = ASKINGS.fetch_add(1, Relaxed) + 1;
        let frame = crate::queue::presented();
        let mut held = table();
        let t = held.get_or_insert_with(|| Table {
            blocks: Blocks::default(),
            frame,
            frame_first: at,
        });
        if t.frame != frame {
            t.frame = frame;
            t.frame_first = at;
        }
        Self {
            at,
            need,
            every: every(),
            frame_first: t.frame_first,
        }
    }

    fn off() -> Self {
        Self {
            at: 0,
            need: 0,
            every: 1,
            frame_first: 0,
        }
    }

    /// Whether a page walked at `walked`, clean `clean` times in a row, is skipped.
    fn quiet(&self, walked: u64, clean: u8) -> bool {
        walked != 0
            && u64::from(clean) >= self.need
            && walked >= self.frame_first
            && self.at - walked < self.every
    }

    /// Whether `[first, last)` (64 KiB pages) is answered from the journal at this asking,
    /// without the kernel.
    pub(super) fn skipping(&self, first: usize, last: usize) -> bool {
        if self.at == 0 || self.every <= 1 || first >= last {
            return false;
        }
        let held = table();
        let Some(t) = held.as_ref() else {
            return false;
        };
        let mut page = first;
        while page < last {
            let Some(block) = t.blocks.get(&(page / BLOCK_PAGES)) else {
                return false;
            };
            let end = last.min((page / BLOCK_PAGES + 1) * BLOCK_PAGES);
            for p in page..end {
                let i = p % BLOCK_PAGES;
                if !self.quiet(block.walked[i], block.clean[i]) {
                    return false;
                }
            }
            page = end;
        }
        drop(held);
        TOLD[0].fetch_add(1, Relaxed);
        TOLD[1].fetch_add(((last - first) * (PAGE / KERNEL_PAGE)) as u64, Relaxed);
        true
    }

    /// The kernel walked `[first, last)` (64 KiB pages) at this asking and found `found`
    /// (4 KiB page addresses) written.
    pub(super) fn judge(&self, first: usize, last: usize, found: &[usize]) {
        if self.at == 0 || first >= last {
            return;
        }
        let mut written: Vec<usize> = found.iter().map(|&at| at / PAGE).collect();
        written.sort_unstable();
        written.dedup();
        let mut held = table();
        let t = held.get_or_insert_with(|| Table {
            blocks: Blocks::default(),
            frame: 0,
            frame_first: self.frame_first,
        });
        if t.blocks.len() >= MOST_BLOCKS {
            t.blocks.clear();
        }
        let mut w = written.iter().peekable();
        let mut page = first;
        while page < last {
            let block = t.blocks.entry(page / BLOCK_PAGES).or_default();
            let end = last.min((page / BLOCK_PAGES + 1) * BLOCK_PAGES);
            for p in page..end {
                while w.next_if(|&&n| n < p).is_some() {}
                let i = p % BLOCK_PAGES;
                block.walked[i] = self.at;
                block.clean[i] = if w.next_if_eq(&&p).is_some() {
                    0
                } else {
                    block.clean[i].saturating_add(1)
                };
            }
            page = end;
        }
        TOLD[2].fetch_add(1, Relaxed);
    }
}

/// `[at, at + len)` was remapped, freed, or written by DirectStorage or the kernel: its
/// pages are walked at the next asking that is given them.
pub(crate) fn forget(at: usize, len: usize) {
    if len == 0 {
        return;
    }
    let mut held = table();
    let Some(t) = held.as_mut() else {
        return;
    };
    if t.blocks.is_empty() {
        return;
    }
    let (first, last) = (at / PAGE, at.saturating_add(len).div_ceil(PAGE));
    let (b0, b1) = (first / BLOCK_PAGES, last.div_ceil(BLOCK_PAGES));
    let clear = |key: usize, block: &mut Block| {
        let from = first.max(key * BLOCK_PAGES);
        let to = last.min((key + 1) * BLOCK_PAGES);
        for p in from..to {
            block.walked[p % BLOCK_PAGES] = 0;
            block.clean[p % BLOCK_PAGES] = 0;
        }
    };
    if b1 - b0 > t.blocks.len() {
        for (&key, block) in t.blocks.iter_mut().filter(|(k, _)| (b0..b1).contains(*k)) {
            clear(key, block);
        }
    } else {
        for key in b0..b1 {
            if let Some(block) = t.blocks.get_mut(&key) {
                clear(key, block);
            }
        }
    }
    TOLD[3].fetch_add(1, Relaxed);
}

/// One line on the frame-meter cadence while the switch is on.
pub(super) fn line(frames: f64) {
    let need = need();
    if need == 0 || frames <= 0.0 {
        return;
    }
    let [skipped, pages, judged, forgot] =
        TOLD.each_ref().map(|n| n.swap(0, Relaxed) as f64 / frames);
    let known = table().as_ref().map_or(0, |t| t.blocks.len());
    tracing::info!(
        "quiet write-watch ranges per frame (VICTORY_ASK_QUIET_SKIP={need}, VICTORY_ASK_QUIET_EVERY={}): \
         {skipped:.1} answered from the journal without the kernel ({pages:.0} pages of 4 KiB not walked), \
         {judged:.1} kernel answers judged, {forgot:.1} ranges forgotten (remapped, freed, written outside); \
         {known} blocks of 1 MiB known",
        every()
    );
}

#[cfg(test)]
#[path = "quiet_tests.rs"]
mod tests;
