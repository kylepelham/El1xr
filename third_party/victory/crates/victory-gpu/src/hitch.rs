//! Frames that take 100 ms or more ("hitches"), always on: at the present
//! that ends one, a line naming the phases that took most of it, on the
//! presenting thread and on all others.
//!
//! A phase is timed by a [`span`] around a call that can block (a
//! submission, a present, the pass over the heap, a texture refresh, a wait
//! for a pipeline, a call of the title that remaps memory, ...). Spans nest:
//! a span's time leaves out the spans opened inside it on the same thread,
//! so the phases of a thread add up to no more than the frame. What the
//! presenting thread spent outside every span (the title's own work, or
//! waits of its own on its other threads) is named as such.
//!
//! `VICTORY_HITCH_MS` sets the threshold (default 100; 0 turns the line
//! off). With `VICTORY_HITCH_STACKS=1` (on by default in the diagnostics
//! build) a thread of ours also samples the presenting thread's stack while
//! a frame runs late, and names its most frequent stacks for frames of
//! [`stacks::SAMPLE_FROM_MS`] or more ([`stacks`]).

#[cfg(windows)]
pub(crate) mod stacks;

use std::cell::Cell;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Instant;

/// What a thread was doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Phase {
    /// The title's `ExecuteCommandLists` (preparing and sending a submission).
    Submit,
    /// A present (what queues hold is run out first).
    Present,
    /// The pass over the heap's buffer views picked by index.
    HeapBuffers,
    /// The pass over textures picked by index.
    HeapTextures,
    /// The early and bake uploads of textures picked by index.
    EarlyTextures,
    /// A texture brought up to date from title memory.
    TextureRefresh,
    /// Host copies of title buffers brought up to date (a batch's flush).
    Copies,
    /// Waiting for the card to land submissions sent ahead.
    CardWait,
    /// Waiting for a host pipeline being made.
    PipelineWait,
    /// Translating a shader on first use.
    ShaderTranslate,
    /// A call of the title that maps or unmaps memory (`victory-hle` xmem).
    Remap,
    /// Bringing lazily kept host copies in on a fault.
    Lazy,
}

const PHASES: usize = 12;

const NAMES: [&str; PHASES] = [
    "submit",
    "present",
    "heap buffers",
    "heap textures",
    "early/bake textures",
    "texture refresh",
    "buffer copies",
    "card wait",
    "pipeline wait",
    "shader translate",
    "memory remap",
    "lazy copies",
];

/// Microseconds and calls per phase since the last present: `[0]` on the
/// presenting thread, `[1]` on all others.
static MICROS: [[AtomicU64; PHASES]; 2] = [const { [const { AtomicU64::new(0) }; PHASES] }; 2];
static CALLS: [[AtomicU64; PHASES]; 2] = [const { [const { AtomicU64::new(0) }; PHASES] }; 2];

/// The thread that presented last (its id as `thread_id` gives it).
static PRESENTER: AtomicU32 = AtomicU32::new(0);

/// Hitch lines written since the start, and frames over the threshold.
pub(crate) static COUNTS: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

thread_local! {
    /// The innermost open span on this thread and when its current stretch began.
    static OPEN: Cell<Option<(Phase, Instant)>> = const { Cell::new(None) };
    static THREAD_ID: Cell<u32> = const { Cell::new(0) };
}

fn thread_id() -> u32 {
    THREAD_ID.with(|id| {
        if id.get() == 0 {
            id.set(os_thread_id());
        }
        id.get()
    })
}

#[cfg(windows)]
fn os_thread_id() -> u32 {
    // SAFETY: a plain query.
    unsafe { windows_sys::Win32::System::Threading::GetCurrentThreadId() }
}

#[cfg(not(windows))]
fn os_thread_id() -> u32 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    std::thread::current().id().hash(&mut h);
    (h.finish() as u32) | 1
}

fn bank() -> usize {
    usize::from(PRESENTER.load(Ordering::Relaxed) != thread_id())
}

fn credit(phase: Phase, micros: u64, call: bool) {
    let bank = bank();
    MICROS[bank][phase as usize].fetch_add(micros, Ordering::Relaxed);
    if call {
        CALLS[bank][phase as usize].fetch_add(1, Ordering::Relaxed);
    }
}

/// The threshold in milliseconds (`VICTORY_HITCH_MS`, default 100; 0: off).
pub(crate) fn threshold_ms() -> u64 {
    static MS: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *MS.get_or_init(|| {
        std::env::var("VICTORY_HITCH_MS")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(100)
    })
}

/// An open phase; its time is counted when it is dropped.
#[must_use]
pub struct Span {
    phase: Phase,
    parent: Option<Phase>,
}

/// Opens `phase` on this thread until the span is dropped.
pub fn span(phase: Phase) -> Span {
    let now = Instant::now();
    let parent = OPEN.with(|open| {
        let parent = open.get().map(|(p, since)| {
            credit(p, (now - since).as_micros() as u64, false);
            p
        });
        open.set(Some((phase, now)));
        parent
    });
    Span { phase, parent }
}

impl Drop for Span {
    fn drop(&mut self) {
        let now = Instant::now();
        OPEN.with(|open| {
            if let Some((p, since)) = open.get() {
                credit(p, (now - since).as_micros() as u64, p == self.phase);
            }
            open.set(self.parent.map(|p| (p, now)));
        });
    }
}

/// Adds `micros` of `phase` timed elsewhere (a leaf: nothing is taken off
/// an open span).
pub fn add(phase: Phase, micros: u64) {
    credit(phase, micros, true);
}

/// The previous present, for the frame's time.
static LAST: Mutex<Option<Instant>> = Mutex::new(None);

/// Ends a frame: called by the thread that presents, once per present.
/// Writes the hitch line if the frame took the threshold or more.
pub(crate) fn presented(frame: u64) {
    let now = Instant::now();
    let me = thread_id();
    PRESENTER.store(me, Ordering::Relaxed);
    // The span this thread is in (the present) counts up to now in this frame.
    OPEN.with(|open| {
        if let Some((p, since)) = open.get() {
            credit(p, (now - since).as_micros() as u64, false);
            open.set(Some((p, now)));
        }
    });
    let taken: [[(u64, u64); PHASES]; 2] = std::array::from_fn(|b| {
        std::array::from_fn(|i| {
            (
                MICROS[b][i].swap(0, Ordering::Relaxed),
                CALLS[b][i].swap(0, Ordering::Relaxed),
            )
        })
    });
    let last = LAST.lock().unwrap_or_else(|e| e.into_inner()).replace(now);
    let Some(last) = last else {
        return;
    };
    let frame_us = (now - last).as_micros() as u64;
    #[cfg(windows)]
    stacks::frame_ended(frame, frame_us, me);
    let threshold = threshold_ms();
    if threshold == 0 || frame_us < threshold * 1000 {
        return;
    }
    COUNTS[1].fetch_add(1, Ordering::Relaxed);
    if !allowed(now, frame_us) {
        return;
    }
    COUNTS[0].fetch_add(1, Ordering::Relaxed);
    tracing::info!("{}", line(frame, frame_us, &taken));
}

/// Every frame of 250 ms or more is named; shorter hitches at most four
/// times a second (the rest are counted in the next line).
fn allowed(now: Instant, frame_us: u64) -> bool {
    static WINDOW: Mutex<Option<(Instant, u32)>> = Mutex::new(None);
    if frame_us >= 250_000 {
        return true;
    }
    let mut window = WINDOW.lock().unwrap_or_else(|e| e.into_inner());
    let (since, n) = window.get_or_insert((now, 0));
    if now.duration_since(*since).as_secs_f64() >= 1.0 {
        *since = now;
        *n = 0;
    }
    *n += 1;
    *n <= 4
}

/// The phases of `bank` over 1 ms, longest first, at most `most`.
fn top(bank: &[(u64, u64); PHASES], most: usize) -> Vec<(usize, u64, u64)> {
    let mut all: Vec<(usize, u64, u64)> = bank
        .iter()
        .enumerate()
        .filter(|(_, (us, _))| *us >= 1000)
        .map(|(i, &(us, n))| (i, us, n))
        .collect();
    all.sort_by(|a, b| b.1.cmp(&a.1));
    all.truncate(most);
    all
}

fn named(phases: &[(usize, u64, u64)]) -> String {
    phases
        .iter()
        .map(|&(i, us, n)| format!("{} {:.1} ms ({n}x)", NAMES[i], us as f64 / 1e3))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The hitch line for a frame of `frame_us`.
pub(crate) fn line(frame: u64, frame_us: u64, taken: &[[(u64, u64); PHASES]; 2]) -> String {
    let presenter = top(&taken[0], 3);
    let others = top(&taken[1], 3);
    let ours: u64 = taken[0].iter().map(|&(us, _)| us).sum();
    let outside = frame_us.saturating_sub(ours);
    format!(
        "hitch: frame {frame} took {:.1} ms; presenting thread: {}{}outside our calls {:.1} ms; other threads: {}",
        frame_us as f64 / 1e3,
        named(&presenter),
        if presenter.is_empty() { "" } else { ", " },
        outside as f64 / 1e3,
        if others.is_empty() {
            "nothing over 1 ms".to_owned()
        } else {
            named(&others)
        }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_spans_leave_out_their_inner_time() {
        let outer = span(Phase::Submit);
        std::thread::sleep(std::time::Duration::from_millis(20));
        {
            let _inner = span(Phase::HeapBuffers);
            std::thread::sleep(std::time::Duration::from_millis(40));
        }
        drop(outer);
        let bank = bank();
        let submit = MICROS[bank][Phase::Submit as usize].swap(0, Ordering::Relaxed);
        let heap = MICROS[bank][Phase::HeapBuffers as usize].swap(0, Ordering::Relaxed);
        assert!(heap >= 40_000, "{heap}");
        assert!((20_000..40_000).contains(&submit), "{submit}");
        assert_eq!(
            CALLS[bank][Phase::Submit as usize].swap(0, Ordering::Relaxed),
            1
        );
        assert_eq!(
            CALLS[bank][Phase::HeapBuffers as usize].swap(0, Ordering::Relaxed),
            1
        );
        assert!(OPEN.with(Cell::get).is_none());
    }

    #[test]
    fn the_line_names_the_longest_phases_and_the_rest() {
        let mut taken = [[(0u64, 0u64); PHASES]; 2];
        taken[0][Phase::HeapBuffers as usize] = (900_000, 1);
        taken[0][Phase::Submit as usize] = (50_000, 12);
        taken[0][Phase::Present as usize] = (500, 1);
        taken[1][Phase::Remap as usize] = (700_000, 1400);
        let text = line(7, 1_200_000, &taken);
        assert_eq!(
            text,
            "hitch: frame 7 took 1200.0 ms; presenting thread: heap buffers 900.0 ms (1x), submit 50.0 ms (12x), \
             outside our calls 249.5 ms; other threads: memory remap 700.0 ms (1400x)"
        );
    }

    #[test]
    fn short_hitches_are_limited_to_four_a_second() {
        let now = Instant::now();
        let n = (0..10).filter(|_| allowed(now, 120_000)).count();
        assert!(n <= 4);
        assert!(allowed(now, 300_000));
    }
}
