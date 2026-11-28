//! (mandy 6e80eb8648) `VICTORY_ALIAS_LATER=1` (live; off by default; only with
//! `VICTORY_ASYNC_SUBMIT=1`, which gives the queues their line): the title's
//! page-mapping copies (`CopyPageMappingsBatchX`) are done by a thread of ours
//! behind its call.
//!
//! The call looks up which physical pages each range is to show (the pool's
//! pages as they are at the call: the title maps other pages into its pool
//! right after) and returns; the mapping thread unmaps and maps them, batch
//! after batch in the order asked; the queue's line gets an entry
//! (`queue::Held::Alias`) that every later submission, signal and present of
//! that queue stands behind until that batch is done, as on the console's
//! queue. Without it the title's thread stood in the unmaps and maps (mandy:
//! 69 ms a frame while streaming or driving, the longest call 188 ms, the card
//! idle 79-122 ms a frame behind it; p90 -28 ms with it).
//!
//! What the title does to the same addresses outside the line (its own
//! `XMemMapPhysicalPages`, a copy whose source is a destination still pending)
//! first waits for the batches that touch them ([`wait_touching`]), so every
//! address ends as it would have with the mapping done in the call.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use victory_core::settings::Live;

/// One range of a page-mapping copy: `[to, to + len)` shows the pages at
/// `[from, from + len)` (`from` 0: nothing, unmapped), or with `constant` the
/// one page at `from` on every page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AliasOp {
    pub to: u64,
    pub from: u64,
    pub len: u64,
    pub constant: bool,
}

/// The mapping itself, with the pages already looked up: true if every page
/// was mapped.
pub type LaterWork = Box<dyn FnOnce() -> bool + Send>;

/// Looks up what the ranges are to show and hands back the mapping, to be done
/// once, later, on another thread; `None`: it cannot be split so (the ranges
/// read what an earlier range of the call maps), and the call maps them itself.
pub type LaterPageMapper = fn(&[AliasOp]) -> Option<LaterWork>;

static MAPPER: OnceLock<LaterPageMapper> = OnceLock::new();

/// Sets what maps pages behind the title's call. Called once, by `victory-hle`.
pub fn use_later_page_mapper(mapper: LaterPageMapper) {
    let _ = MAPPER.set(mapper);
}

pub(crate) fn mapper() -> Option<LaterPageMapper> {
    MAPPER.get().copied()
}

/// The switch (the line is checked by the caller).
pub(crate) fn on() -> bool {
    static ON: Live = Live::flag("VICTORY_ALIAS_LATER");
    ON.get_bool()
}

/// Batches waiting for the mapping thread, and the addresses each changes.
struct Line {
    work: VecDeque<(u64, LaterWork)>,
    /// Batches sent and not done: their number and the ranges they map at,
    /// sorted and joined.
    pending: Vec<(u64, Vec<(u64, u64)>)>,
    started: bool,
}

static LINE: Mutex<Line> = Mutex::new(Line {
    work: VecDeque::new(),
    pending: Vec::new(),
    started: false,
});
static MORE: Condvar = Condvar::new();
/// How many batches were sent, and the highest number done (they are done in order).
static SENT: AtomicU64 = AtomicU64::new(0);
static DONE: AtomicU64 = AtomicU64::new(0);
static DONE_WAKE: (Mutex<()>, Condvar) = (Mutex::new(()), Condvar::new());

/// Since the last line: batches, pages-worth bytes / 64 KiB, µs mapping, µs the
/// line stood waiting, µs the title waited for pending batches, those waits,
/// batches that failed.
static STATS: [AtomicU64; 7] = [const { AtomicU64::new(0) }; 7];
const BATCHES: usize = 0;
const PAGES: usize = 1;
const MAP_US: usize = 2;
const LINE_US: usize = 3;
const TITLE_US: usize = 4;
const TITLE_WAITS: usize = 5;
const FAILED: usize = 6;

/// The ranges of `ops` that change, sorted and joined.
pub fn touched(ops: &[AliasOp]) -> Vec<(u64, u64)> {
    let mut ranges: Vec<(u64, u64)> = ops
        .iter()
        .filter(|op| op.len > 0)
        .map(|op| (op.to, op.to.saturating_add(op.len)))
        .collect();
    join(&mut ranges);
    ranges
}

fn join(ranges: &mut Vec<(u64, u64)>) {
    ranges.sort_unstable();
    let mut out: Vec<(u64, u64)> = Vec::with_capacity(ranges.len());
    for &(lo, hi) in ranges.iter() {
        match out.last_mut() {
            Some(last) if lo <= last.1 => last.1 = last.1.max(hi),
            _ => out.push((lo, hi)),
        }
    }
    *ranges = out;
}

/// Whether sorted, joined `ranges` meet `[lo, hi)`.
fn meets(ranges: &[(u64, u64)], lo: u64, hi: u64) -> bool {
    let i = ranges.partition_point(|r| r.1 <= lo);
    ranges.get(i).is_some_and(|r| r.0 < hi)
}

/// Gives `work` (mapping `touched`) to the mapping thread: its number, which
/// the line waits for. `Err` if there is no such thread: the caller maps.
pub fn send(work: LaterWork, touched: Vec<(u64, u64)>) -> Result<u64, LaterWork> {
    let number = {
        let mut line = LINE.lock().unwrap_or_else(|e| e.into_inner());
        if !line.started {
            if std::thread::Builder::new()
                .name("victory-alias".into())
                .spawn(worker)
                .is_err()
            {
                return Err(work);
            }
            line.started = true;
        }
        let number = SENT.fetch_add(1, Relaxed) + 1;
        line.work.push_back((number, work));
        line.pending.push((number, touched));
        number
    };
    MORE.notify_one();
    Ok(number)
}

fn worker() {
    crate::priority::stream_thread();
    loop {
        let (number, work) = {
            let mut line = LINE.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                match line.work.pop_front() {
                    Some(next) => break next,
                    None => line = MORE.wait(line).unwrap_or_else(|e| e.into_inner()),
                }
            }
        };
        let began = Instant::now();
        let ok = work();
        STATS[BATCHES].fetch_add(1, Relaxed);
        STATS[MAP_US].fetch_add(began.elapsed().as_micros() as u64, Relaxed);
        if !ok {
            STATS[FAILED].fetch_add(1, Relaxed);
            static ONCE: std::sync::Once = std::sync::Once::new();
            ONCE.call_once(|| {
                tracing::warn!(
                    "a page mapping done behind the title's call failed (VICTORY_ALIAS_LATER)"
                )
            });
        }
        finish(number);
    }
}

/// Batch `number` is done (they finish in order).
fn finish(number: u64) {
    LINE.lock()
        .unwrap_or_else(|e| e.into_inner())
        .pending
        .retain(|(n, _)| *n != number);
    {
        let _held = DONE_WAKE.0.lock().unwrap_or_else(|e| e.into_inner());
        DONE.store(number, Relaxed);
    }
    DONE_WAKE.1.notify_all();
    crate::queue::alias_done();
}

/// Whether batch `number` is done.
pub fn done(number: u64) -> bool {
    DONE.load(Relaxed) >= number
}

/// Until batch `number` is done; the µs it took.
fn wait_for(number: u64) -> u64 {
    if done(number) {
        return 0;
    }
    let began = Instant::now();
    let mut held = DONE_WAKE.0.lock().unwrap_or_else(|e| e.into_inner());
    while !done(number) {
        held = DONE_WAKE
            .1
            .wait_timeout(held, Duration::from_millis(2))
            .unwrap_or_else(|e| e.into_inner())
            .0;
    }
    began.elapsed().as_micros() as u64
}

/// For the line: until batch `number` is done (a present that runs the lines out).
pub(crate) fn wait(number: u64) {
    STATS[LINE_US].fetch_add(wait_for(number), Relaxed);
}

/// Whether batches are sent and not done.
pub fn pending() -> bool {
    DONE.load(Relaxed) < SENT.load(Relaxed)
}

/// Until every batch sent so far is done: for a page-mapping copy done in the
/// call while some are pending (the switch turned off, or one that cannot be
/// split), so that it lands after them.
pub fn drain() {
    if pending() {
        STATS[TITLE_US].fetch_add(wait_for(SENT.load(Relaxed)), Relaxed);
        STATS[TITLE_WAITS].fetch_add(1, Relaxed);
    }
}

/// Until no pending batch maps any address of `ranges` (`(start, len)`): for
/// the runtime's other changes of mapping, and for the sources of a copy,
/// before they look at what is mapped there.
pub fn wait_touching(ranges: &[(u64, u64)]) {
    if !pending() {
        return;
    }
    let last = {
        let line = LINE.lock().unwrap_or_else(|e| e.into_inner());
        line.pending
            .iter()
            .filter(|(_, at)| {
                ranges
                    .iter()
                    .any(|&(lo, len)| len > 0 && meets(at, lo, lo.saturating_add(len)))
            })
            .map(|(n, _)| *n)
            .max()
    };
    if let Some(last) = last {
        STATS[TITLE_US].fetch_add(wait_for(last), Relaxed);
        STATS[TITLE_WAITS].fetch_add(1, Relaxed);
    }
}

/// Counts a batch's 64 KiB pages when it is sent.
pub(crate) fn count_pages(ops: &[AliasOp]) {
    STATS[PAGES].fetch_add(ops.iter().map(|op| op.len >> 16).sum(), Relaxed);
}

/// The per-frame line, when any batch was done since the last.
pub(crate) fn line(frames: f64) {
    if frames <= 0.0 {
        return;
    }
    let s = STATS.each_ref().map(|n| n.swap(0, Relaxed) as f64 / frames);
    if s[BATCHES] == 0.0 && s[TITLE_WAITS] == 0.0 {
        return;
    }
    tracing::info!(
        "page mappings done behind the title's call, per frame (VICTORY_ALIAS_LATER=1): {:.1} batches of {:.0} pages of 64 KiB, \
         {:.1} ms of the mapping thread; the line stood waiting for them {:.1} ms; the title waited {:.1} ms in {:.1} waits for \
         addresses still being mapped; {:.1} batches failed",
        s[BATCHES],
        s[PAGES],
        s[MAP_US] / 1000.0,
        s[LINE_US] / 1000.0,
        s[TITLE_US] / 1000.0,
        s[TITLE_WAITS],
        s[FAILED],
    );
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// One test at a time drives the mapping thread (its numbers are global).
    pub(crate) static ONE: Mutex<()> = Mutex::new(());

    #[test]
    fn touched_ranges_join_and_meet() {
        let op = |to: u64, len: u64| AliasOp {
            to,
            from: 0,
            len,
            constant: false,
        };
        let t = touched(&[
            op(0x30000, 0x10000),
            op(0x10000, 0x10000),
            op(0x20000, 0x10000),
            op(0x80000, 0),
            op(0x60000, 0x20000),
        ]);
        assert_eq!(t, vec![(0x10000, 0x40000), (0x60000, 0x80000)]);
        assert!(meets(&t, 0x3ffff, 0x40000));
        assert!(!meets(&t, 0x40000, 0x60000));
        assert!(meets(&t, 0x50000, 0x60001));
        assert!(!meets(&t, 0x80000, 0x90000));
        assert!(!meets(&t, 0, 0x10000));
    }

    /// Batches run in the order sent; `wait_touching` waits only for batches that
    /// map one of its addresses, `drain` for all.
    #[test]
    fn batches_run_in_order_and_waits_wait_for_what_touches() {
        let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
        let order = std::sync::Arc::new(Mutex::new(Vec::new()));
        let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
        let first = {
            let order = order.clone();
            send(
                Box::new(move || {
                    go_rx.recv().unwrap();
                    order.lock().unwrap().push(1);
                    true
                }),
                vec![(0x100000, 0x110000)],
            )
            .unwrap_or_else(|_| panic!("no mapping thread"))
        };
        let second = {
            let order = order.clone();
            send(
                Box::new(move || {
                    order.lock().unwrap().push(2);
                    true
                }),
                vec![(0x200000, 0x210000)],
            )
            .unwrap_or_else(|_| panic!("no mapping thread"))
        };
        assert!(second > first);
        assert!(pending() && !done(first) && !done(second));
        // Addresses no pending batch maps: no wait.
        wait_touching(&[(0x300000, 0x10000)]);
        assert!(!done(first));
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            go_tx.send(()).unwrap();
        });
        // The second batch's addresses: waits for it, so for the first too.
        wait_touching(&[(0x20f000, 0x1000)]);
        assert!(done(first) && done(second));
        assert_eq!(*order.lock().unwrap(), vec![1, 2]);
        releaser.join().unwrap();
        drain();
        assert!(!pending());
    }
}
