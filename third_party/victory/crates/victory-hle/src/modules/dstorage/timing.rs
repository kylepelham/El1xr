//! (bootfast) `VICTORY_DSTORAGE_TIMING=1`: where each DirectStorage
//! request's time goes between the title's Submit and the end of its run on
//! a worker, summed every 5 s into one line ("dstorage timing"): per phase
//! the total, the median, the 99th percentile and the most over the
//! requests of the window, and the phases of the slowest request.
//!
//! Phases, in a request's order:
//! - `queue`: from the Submit that handed it to the workers to a worker
//!   taking it (the scheduler's lock, staging room, workers busy);
//! - `read`: reading its bytes from the file (or copying a memory source);
//! - `zlib`, `bcpack`: decompressing;
//! - `dest`: making the destination writable before the bytes go in: the
//!   page table's lock and the page runs, the lazy GPU pages flushed and
//!   held, the view, or the guards opened (`kernel_will_write`);
//! - `notify`: telling the GPU side the bytes are there after the write
//!   (the views that show the pages, `title_memory_written`);
//! - `copy`: a copy of bytes already decoded (a bounce buffer into its
//!   address, or `write_physical` into pages that are not one run, with its
//!   retries);
//! - `other`: what falls between (allocations, zeroing, the request's checks);
//! - `finish`: the scheduler's lock taken again after the run, and the
//!   request's status entries written (`Lanes::finish`, `count`).
//!
//! Off: one relaxed load per mark.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Phase {
    Queue = 0,
    Read,
    Zlib,
    Bcpack,
    Dest,
    Notify,
    Copy,
    Other,
    Finish,
}

const PHASES: usize = 9;
const NAMES: [&str; PHASES] = [
    "queue", "read", "zlib", "bcpack", "dest", "notify", "copy", "other", "finish",
];

pub(super) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    static CHECKED: AtomicBool = AtomicBool::new(false);
    let on =
        *ON.get_or_init(|| std::env::var_os("VICTORY_DSTORAGE_TIMING").is_some_and(|v| v == "1"));
    if on && !CHECKED.swap(true, Relaxed) {
        tracing::info!("dstorage: per-request timing on (VICTORY_DSTORAGE_TIMING)");
    }
    on
}

/// When each read was handed to the workers, by sequence number.
static SUBMITTED: Mutex<Option<HashMap<u64, Instant>>> = Mutex::new(None);

/// At Submit: reads `seqs` go to the workers now.
pub(super) fn submitted(seqs: impl IntoIterator<Item = u64>) {
    let now = Instant::now();
    let mut all = SUBMITTED.lock().unwrap_or_else(|e| e.into_inner());
    let all = all.get_or_insert_with(HashMap::new);
    for seq in seqs {
        all.insert(seq, now);
    }
}

/// A request's phases so far on this worker, and where the last mark was.
struct Current {
    at: Instant,
    us: [u64; PHASES],
}

thread_local! {
    static CURRENT: RefCell<Option<Current>> = const { RefCell::new(None) };
}

/// A worker took read `seq`.
pub(super) fn begin(seq: u64) {
    if !on() {
        return;
    }
    let now = Instant::now();
    let queued = SUBMITTED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_mut()
        .and_then(|all| all.remove(&seq))
        .map_or(0, |at| now.duration_since(at).as_micros() as u64);
    let mut us = [0; PHASES];
    us[Phase::Queue as usize] = queued;
    CURRENT.with_borrow_mut(|c| *c = Some(Current { at: now, us }));
}

/// The time since the last mark went to `phase`.
pub(super) fn mark(phase: Phase) {
    if !on() {
        return;
    }
    CURRENT.with_borrow_mut(|c| {
        if let Some(c) = c {
            let now = Instant::now();
            c.us[phase as usize] += now.duration_since(c.at).as_micros() as u64;
            c.at = now;
        }
    });
}

/// The request is done (its status written if it was the last before one):
/// its phases go into the window.
pub(super) fn end(bytes: u32, options: u64) {
    if !on() {
        return;
    }
    let Some(current) = CURRENT.with_borrow_mut(Option::take) else {
        return;
    };
    let line = {
        let mut w = WINDOW.lock().unwrap_or_else(|e| e.into_inner());
        let w = w.get_or_insert_with(Window::new);
        w.add(current.us, bytes, options);
        (w.since.elapsed().as_secs() >= 5).then(|| std::mem::replace(w, Window::new()))
    };
    if let Some(w) = line {
        w.log();
    }
}

struct Window {
    since: Instant,
    requests: Vec<[u64; PHASES]>,
    slowest: Option<([u64; PHASES], u32, u64)>,
}

static WINDOW: Mutex<Option<Window>> = Mutex::new(None);

fn total(us: &[u64; PHASES]) -> u64 {
    us.iter().sum()
}

impl Window {
    fn new() -> Window {
        Window {
            since: Instant::now(),
            requests: Vec::new(),
            slowest: None,
        }
    }

    fn add(&mut self, us: [u64; PHASES], bytes: u32, options: u64) {
        if self.slowest.is_none_or(|(s, _, _)| total(&us) > total(&s)) {
            self.slowest = Some((us, bytes, options));
        }
        self.requests.push(us);
    }

    /// Per phase: total ms, median, 99th percentile and most in µs.
    fn summary(&self) -> [(u64, u64, u64, u64); PHASES] {
        let mut out = [(0, 0, 0, 0); PHASES];
        let n = self.requests.len();
        if n == 0 {
            return out;
        }
        let mut column = Vec::with_capacity(n);
        for (p, slot) in out.iter_mut().enumerate() {
            column.clear();
            column.extend(self.requests.iter().map(|r| r[p]));
            column.sort_unstable();
            let sum: u64 = column.iter().sum();
            *slot = (
                sum / 1000,
                column[n / 2],
                column[(n * 99 / 100).min(n - 1)],
                column[n - 1],
            );
        }
        out
    }

    fn text(&self) -> String {
        let s = self.summary();
        let phases: Vec<String> = NAMES
            .iter()
            .zip(s)
            .map(|(name, (ms, p50, p99, max))| {
                format!("{name} {ms}ms p50={p50}us p99={p99}us max={max}us")
            })
            .collect();
        let slowest = self
            .slowest
            .map_or_else(String::new, |(us, bytes, options)| {
                let parts: Vec<String> = NAMES
                    .iter()
                    .zip(us)
                    .filter(|(_, v)| *v > 0)
                    .map(|(n, v)| format!("{n} {v}"))
                    .collect();
                format!(
                    "; slowest {}us ({bytes} bytes, options {options:#x}): {}",
                    total(&us),
                    parts.join(", ")
                )
            });
        let totals: Vec<u64> = {
            let mut t: Vec<u64> = self.requests.iter().map(total).collect();
            t.sort_unstable();
            t
        };
        let (p50, p99) = match totals.len() {
            0 => (0, 0),
            n => (totals[n / 2], totals[(n * 99 / 100).min(n - 1)]),
        };
        format!(
            "dstorage timing: last {:.1} s, {} requests, submit to done p50={p50}us p99={p99}us; {}{slowest}",
            self.since.elapsed().as_secs_f64(),
            self.requests.len(),
            phases.join("; ")
        )
    }

    fn log(&self) {
        tracing::info!("{}", self.text());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_window_sums_and_ranks_its_requests() {
        let mut w = Window::new();
        for i in 1..=100u64 {
            let mut us = [0; PHASES];
            us[Phase::Read as usize] = i;
            us[Phase::Queue as usize] = 1000;
            w.add(us, 64, 1);
        }
        let mut slow = [0; PHASES];
        slow[Phase::Notify as usize] = 90_000;
        w.add(slow, 4096, 0x200000);
        let s = w.summary();
        assert_eq!(s[Phase::Queue as usize], (100, 1000, 1000, 1000));
        let read = s[Phase::Read as usize];
        assert_eq!((read.0, read.3), (5, 100));
        assert_eq!(s[Phase::Notify as usize].3, 90_000);
        let text = w.text();
        assert!(text.starts_with("dstorage timing: last "), "{text}");
        assert!(text.contains("101 requests"), "{text}");
        assert!(
            text.contains("; slowest 90000us (4096 bytes, options 0x200000): notify 90000"),
            "{text}"
        );
        assert!(
            text.contains("notify 90ms p50=0us p99=0us max=90000us"),
            "{text}"
        );
    }

    #[test]
    fn an_empty_window_says_so() {
        let w = Window::new();
        assert!(
            w.text()
                .contains(" 0 requests, submit to done p50=0us p99=0us")
        );
    }
}
