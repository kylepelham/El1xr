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
//!
//! `VICTORY_FRAME_PHASES=1` also writes every frame's phases, all of them,
//! on three sets of threads: ours that presents, the title's that called
//! the present, and all others ([`frame_line`]).

#[cfg(windows)]
mod cpu;
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
    /// Our thread that runs the queues' lines, waiting for work.
    Idle,
    /// The title's present waiting for the one before it to be sent.
    PresentTurn,
    /// Binding a draw or dispatch as its list is recorded.
    Bind,
}

pub(crate) const PHASES: usize = 15;

/// Banks of [`MICROS`]: the presenting thread, the title's thread that
/// called the present (if another), all other threads.
const BANKS: usize = 3;

pub(crate) const NAMES: [&str; PHASES] = [
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
    "waiting for work",
    "present turn",
    "binding",
];

/// Microseconds and calls per phase since the last present, per bank
/// ([`BANKS`]).
static MICROS: [[AtomicU64; PHASES]; BANKS] = [const { [const { AtomicU64::new(0) }; PHASES] }; BANKS];
static CALLS: [[AtomicU64; PHASES]; BANKS] = [const { [const { AtomicU64::new(0) }; PHASES] }; BANKS];

/// The thread that presented last (its id as `thread_id` gives it).
static PRESENTER: AtomicU32 = AtomicU32::new(0);
/// The title's thread that called the present last.
static TITLE: AtomicU32 = AtomicU32::new(0);

/// Hitch lines written since the start, and frames over the threshold.
pub(crate) static COUNTS: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

thread_local! {
    /// The innermost open span on this thread and when its current stretch began.
    static OPEN: Cell<Option<(Phase, Instant)>> = const { Cell::new(None) };
    static THREAD_ID: Cell<u32> = const { Cell::new(0) };
}

pub(crate) fn thread_id() -> u32 {
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
    let me = thread_id();
    if PRESENTER.load(Ordering::Relaxed) == me {
        0
    } else if TITLE.load(Ordering::Relaxed) == me {
        1
    } else {
        2
    }
}

/// The title calls a present on this thread.
pub fn title_presents() {
    TITLE.store(thread_id(), Ordering::Relaxed);
    #[cfg(windows)]
    stacks::title_presents(thread_id());
    #[cfg(windows)]
    if every_frame() {
        TITLE_CPU_US.store(cpu::lap().unwrap_or(u64::MAX), Ordering::Relaxed);
    }
}

/// The CPU time of the title's presenting thread between its last two
/// presents (`u64::MAX`: not known).
static TITLE_CPU_US: AtomicU64 = AtomicU64::new(u64::MAX);

/// CPU microseconds of a frame: our presenting thread, the title's, the
/// whole process (`None`: not known).
type Cpu = [Option<u64>; 3];

fn cpu_now() -> Cpu {
    #[cfg(windows)]
    {
        let title = TITLE_CPU_US.load(Ordering::Relaxed);
        [cpu::lap(), (title != u64::MAX).then_some(title), cpu::process_lap()]
    }
    #[cfg(not(windows))]
    [None; 3]
}

/// Whether every frame's phases are written (`VICTORY_FRAME_PHASES=1`).
fn every_frame() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_FRAME_PHASES").is_some_and(|v| v == "1"))
}

fn credit(phase: Phase, micros: u64, call: bool) {
    let bank = bank();
    MICROS[bank][phase as usize].fetch_add(micros, Ordering::Relaxed);
    crate::boot_prof::phase(phase as usize, micros);
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
    #[cfg(windows)]
    stacks::opened(phase, parent.is_none(), now);
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
        #[cfg(windows)]
        stacks::closed(self.parent);
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
    let taken: [[(u64, u64); PHASES]; BANKS] = std::array::from_fn(|b| {
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
    if every_frame() {
        tracing::info!("{}", frame_line(frame, frame_us, &taken, cpu_now()));
    }
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
pub(crate) fn line(frame: u64, frame_us: u64, taken: &[[(u64, u64); PHASES]; BANKS]) -> String {
    let presenter = top(&taken[0], 3);
    let merged: [(u64, u64); PHASES] =
        std::array::from_fn(|i| (taken[1][i].0 + taken[2][i].0, taken[1][i].1 + taken[2][i].1));
    let others = top(&merged, 3);
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

/// Every phase of a bank that ran, in microseconds and calls, and for a
/// single thread what the frame spent outside them.
fn all(bank: &[(u64, u64); PHASES], frame_us: Option<u64>) -> String {
    let mut parts: Vec<String> = bank
        .iter()
        .enumerate()
        .filter(|(_, (us, n))| *us > 0 || *n > 0)
        .map(|(i, (us, n))| format!("{} {us}/{n}", NAMES[i]))
        .collect();
    if let Some(frame_us) = frame_us {
        let ours: u64 = bank.iter().map(|&(us, _)| us).sum();
        parts.push(format!("outside {}", frame_us.saturating_sub(ours)));
    }
    parts.join(", ")
}

/// The line of every frame (`VICTORY_FRAME_PHASES=1`): each bank's phases
/// as microseconds/calls, and the CPU time of our presenting thread, the
/// title's and the process.
pub(crate) fn frame_line(frame: u64, frame_us: u64, taken: &[[(u64, u64); PHASES]; BANKS], cpu: Cpu) -> String {
    let title = TITLE.load(Ordering::Relaxed);
    let own = title != 0 && title != PRESENTER.load(Ordering::Relaxed);
    let cpu = cpu.map(|us| us.map_or("?".to_owned(), |us| us.to_string()));
    format!(
        "frame phases: frame {frame} took {frame_us} us; ours: {}; title: {}; others: {}; cpu us: ours {}, title {}, process {}",
        all(&taken[0], Some(frame_us)),
        all(&taken[1], own.then_some(frame_us)),
        all(&taken[2], None),
        cpu[0],
        cpu[1],
        cpu[2]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_spans_leave_out_their_inner_time() {
        let started = std::time::Instant::now();
        let outer = span(Phase::Submit);
        std::thread::sleep(std::time::Duration::from_millis(20));
        {
            let _inner = span(Phase::HeapBuffers);
            std::thread::sleep(std::time::Duration::from_millis(40));
        }
        drop(outer);
        let elapsed = started.elapsed().as_micros() as u64;
        let bank = bank();
        let submit = MICROS[bank][Phase::Submit as usize].swap(0, Ordering::Relaxed);
        let heap = MICROS[bank][Phase::HeapBuffers as usize].swap(0, Ordering::Relaxed);
        assert!(heap >= 40_000, "{heap}");
        // The outer span leaves out the inner one: what is left is the outer sleep, however long a
        // loaded machine made either sleep.
        assert!(submit >= 20_000 && submit + heap <= elapsed + 1_000, "{submit} + {heap} vs {elapsed}");
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
        let mut taken = [[(0u64, 0u64); PHASES]; BANKS];
        taken[0][Phase::HeapBuffers as usize] = (900_000, 1);
        taken[0][Phase::Submit as usize] = (50_000, 12);
        taken[0][Phase::Present as usize] = (500, 1);
        taken[1][Phase::Remap as usize] = (300_000, 400);
        taken[2][Phase::Remap as usize] = (400_000, 1000);
        let text = line(7, 1_200_000, &taken);
        assert_eq!(
            text,
            "hitch: frame 7 took 1200.0 ms; presenting thread: heap buffers 900.0 ms (1x), submit 50.0 ms (12x), \
             outside our calls 249.5 ms; other threads: memory remap 700.0 ms (1400x)"
        );
    }

    #[test]
    fn the_frame_line_names_every_phase_of_each_bank() {
        let mut taken = [[(0u64, 0u64); PHASES]; BANKS];
        taken[0][Phase::Submit as usize] = (20_000, 24);
        taken[0][Phase::Idle as usize] = (9_000, 30);
        taken[0][Phase::Present as usize] = (500, 1);
        taken[1][Phase::Bind as usize] = (6_000, 4000);
        taken[1][Phase::PresentTurn as usize] = (2_000, 1);
        taken[2][Phase::Remap as usize] = (300, 7);
        let ours = all(&taken[0], Some(40_000));
        assert_eq!(ours, "submit 20000/24, present 500/1, waiting for work 9000/30, outside 10500");
        assert_eq!(all(&taken[1], None), "present turn 2000/1, binding 6000/4000");
        assert_eq!(all(&taken[2], None), "memory remap 300/7");
        let line = frame_line(3, 40_000, &taken, [Some(30_000), None, Some(200_000)]);
        assert!(line.starts_with("frame phases: frame 3 took 40000 us; ours: submit 20000/24"));
        assert!(line.ends_with("cpu us: ours 30000, title ?, process 200000"), "{line}");
    }

    #[test]
    fn short_hitches_are_limited_to_four_a_second() {
        let now = Instant::now();
        let n = (0..10).filter(|_| allowed(now, 120_000)).count();
        assert!(n <= 4);
        assert!(allowed(now, 300_000));
    }
}
