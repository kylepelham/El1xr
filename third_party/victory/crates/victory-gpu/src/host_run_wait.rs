//! (feedcard, #88) The two waits inside `run_other` on the "preparing
//! breakdown" line, each with its own switch (both Live, off by default),
//! and a line in both settings that splits them per frame.
//!
//! - `VICTORY_HOST_ALONE=1`: a host step (`Step::Host`) first waits for every
//!   texture check pending before it (`Batch::flush_textures`), because a
//!   host callback may use a check's result (the mip mask that maps a
//!   reserved texture, `crate::texture`'s card writes into levels on demand).
//!   Most host steps read none: the clamp's budget words read back
//!   (`HostList::budget`) and the rectangles expanded into scratch
//!   (`crate::draw`) write only host memory of the list's own and read no
//!   texture. Those are recorded "alone" and, with the switch, skip the
//!   wait: the checks pending before them are joined at the next place that
//!   joins them anyway (a host step that is not alone, a title write, the
//!   preludes, the flush), always before any prelude records an upload.
//!   The same textures are checked, from the same title memory (a host step
//!   writes none), so the uploads recorded are the same. b3 `18_diag`: 38
//!   host steps a frame, 3.6-7.0 ms, most of it this wait.
//! - `VICTORY_PRELUDE_AHEAD=1`: a list's prelude (`HostList::prelude`) is
//!   reset, allocator and list, when the list itself is reset (the title's
//!   reset on its recording thread, or the landed list's recycle), not on
//!   the submitting thread right before it records. Every submission of the
//!   list has run then, the prelude's with it (it runs in the same
//!   submission, just before the list). The prelude is left open and empty;
//!   the next submission records into it and closes it, or leaves it open
//!   if the list needs no prelude. Same commands on the card.
//!
//! Not in the cache key: no shader or pipeline output.

use super::{HostList, Step};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::Instant;
use windows::Win32::Graphics::Direct3D12::{ID3D12CommandAllocator, ID3D12GraphicsCommandList};

static ALONE: victory_core::settings::Live =
    victory_core::settings::Live::flag("VICTORY_HOST_ALONE");
static AHEAD: victory_core::settings::Live =
    victory_core::settings::Live::flag("VICTORY_PRELUDE_AHEAD");

#[cfg(test)]
thread_local! {
    /// Tests: the switches (alone, ahead) for calls made on this thread.
    pub(super) static FORCED: std::cell::Cell<Option<(bool, bool)>> = const { std::cell::Cell::new(None) };
}

pub(super) fn alone_on() -> bool {
    #[cfg(test)]
    if let Some((on, _)) = FORCED.with(std::cell::Cell::get) {
        return on;
    }
    ALONE.get_bool()
}

pub(super) fn ahead_on() -> bool {
    #[cfg(test)]
    if let Some((_, on)) = FORCED.with(std::cell::Cell::get) {
        return on;
    }
    AHEAD.get_bool()
}

/// Since the last line, by place.
const HOST_STEPS: usize = 0;
/// Host steps that waited for pending checks, and the nanoseconds.
const JOINED: usize = 1;
const JOIN_NS: usize = 2;
/// Alone host steps that skipped a wait (checks were pending).
const SKIPPED: usize = 3;
/// The host steps' own work.
const WORK_NS: usize = 4;
/// Preludes asked for, recorded, and their nanoseconds (all of `prelude`).
const PRELUDES: usize = 5;
const RECORDED: usize = 6;
const PRELUDE_NS: usize = 7;
/// Of those: resets on the submitting thread, closes.
const RESETS: usize = 8;
const RESET_NS: usize = 9;
const CLOSE_NS: usize = 10;
/// Preludes found open (reset ahead), and resets ahead with their time
/// (on the thread that reset the list).
const FOUND_OPEN: usize = 11;
const AHEAD_RESETS: usize = 12;
const AHEAD_NS: usize = 13;
/// Host steps recorded alone (whether checks were pending or not).
const ALONE_STEPS: usize = 14;
/// Texture uploads recorded on preludes, and their nanoseconds.
const UPLOADS: usize = 15;
const UPLOAD_NS: usize = 16;
/// Preludes with clamps ahead (`crate::indirect::record_ahead`), and
/// their nanoseconds.
const CLAMPS: usize = 17;
const CLAMP_NS: usize = 18;
const SLOTS: usize = 19;

static COUNTS: [AtomicU64; SLOTS] = [const { AtomicU64::new(0) }; SLOTS];

fn add(slot: usize, n: u64) {
    COUNTS[slot].fetch_add(n, Relaxed);
}

fn ns(since: Instant) -> u64 {
    since.elapsed().as_nanos() as u64
}

/// Whether `step`, a `Step::Run` or `Step::Host`, waits for the texture
/// checks pending before it. A `Run` writes title memory and joins them in
/// `Batch::title_written`, which is not this.
pub(super) fn host_joins(step: &Step) -> bool {
    match step {
        Step::Host(_, alone) => !(*alone && alone_on()),
        _ => false,
    }
}

/// A host step: `join` waits for the pending checks if it must (`pending`:
/// there are some), then `work` runs; both timed.
pub(super) fn host_step(step: &Step, pending: bool, join: impl FnOnce(), work: impl FnOnce()) {
    add(HOST_STEPS, 1);
    if let Step::Host(_, true) = step {
        add(ALONE_STEPS, 1);
    }
    if pending {
        if host_joins(step) {
            let at = Instant::now();
            join();
            add(JOIN_NS, ns(at));
            add(JOINED, 1);
        } else {
            add(SKIPPED, 1);
        }
    }
    let at = Instant::now();
    work();
    add(WORK_NS, ns(at));
}

/// Times one `HostList::prelude` call made through `prelude`.
pub(super) fn prelude<T>(prelude: impl FnOnce() -> Option<T>) -> Option<T> {
    let at = Instant::now();
    let made = prelude();
    add(PRELUDE_NS, ns(at));
    add(PRELUDES, 1);
    add(RECORDED, u64::from(made.is_some()));
    made
}

/// A prelude's `n` texture uploads recorded by `record`, timed.
pub(super) fn uploads(n: usize, record: impl FnOnce()) {
    if n == 0 {
        return;
    }
    let at = Instant::now();
    record();
    add(UPLOAD_NS, ns(at));
    add(UPLOADS, n as u64);
}

/// A prelude's clamps ahead recorded by `record`, timed.
pub(super) fn ahead(record: impl FnOnce()) {
    let at = Instant::now();
    record();
    add(CLAMP_NS, ns(at));
    add(CLAMPS, 1);
}

/// The prelude made open for recording: as the list's reset left it (with
/// the switch), or reset now.
///
/// # Safety
/// The prelude is not executing.
pub(super) unsafe fn open(
    list: &HostList,
    allocator: &ID3D12CommandAllocator,
    prelude: &ID3D12GraphicsCommandList,
) -> windows::core::Result<()> {
    if list.prelude_open.swap(false, Relaxed) {
        add(FOUND_OPEN, 1);
        return Ok(());
    }
    let at = Instant::now();
    // SAFETY: as the caller says.
    let reset = unsafe {
        allocator
            .Reset()
            .and_then(|()| prelude.Reset(allocator, None))
    };
    add(RESET_NS, ns(at));
    add(RESETS, 1);
    reset
}

/// Closes the recorded prelude, timed.
///
/// # Safety
/// An open list.
pub(super) unsafe fn close(prelude: &ID3D12GraphicsCommandList) -> windows::core::Result<()> {
    let at = Instant::now();
    // SAFETY: as the caller says.
    let closed = unsafe { prelude.Close() };
    add(CLOSE_NS, ns(at));
    closed
}

/// At the list's reset (every submission of it has run): its prelude, if it
/// has one and it is closed, is reset and left open, with the switch.
pub(super) fn reset_ahead(list: &HostList) {
    if !ahead_on() || list.prelude_open.load(Relaxed) {
        return;
    }
    let Some(Some((allocator, prelude))) = list.prelude.get() else {
        return;
    };
    let at = Instant::now();
    // SAFETY: the list's submissions have all run, its prelude's with them.
    let reset = unsafe {
        allocator
            .Reset()
            .and_then(|()| prelude.Reset(allocator, None))
    };
    if let Err(e) = reset {
        super::fail(format_args!("a host state prelude did not reset ({e})"));
    }
    list.prelude_open.store(true, Relaxed);
    add(AHEAD_NS, ns(at));
    add(AHEAD_RESETS, 1);
}

/// Tests: the counts since the last line, taken.
#[cfg(test)]
pub(super) fn taken() -> [u64; SLOTS] {
    std::array::from_fn(|i| COUNTS[i].swap(0, Relaxed))
}

/// The frame line's, in both settings.
pub(super) fn line(frames: f64) {
    let c: Vec<f64> = COUNTS.iter().map(|c| c.swap(0, Relaxed) as f64).collect();
    let per = |slot: usize| c[slot] / frames;
    let ms = |slot: usize| c[slot] / 1e6 / frames;
    tracing::info!(
        "run_other waits per frame: host steps {:.1} ({:.1} recorded alone), {:.1} waited for pending texture \
         checks {:.2} ms, {:.1} skipped the wait (VICTORY_HOST_ALONE={}), their work {:.2} ms; preludes {:.1} \
         ({:.1} recorded) {:.2} ms, of which {:.1} list resets {:.2} ms, closes {:.2} ms, {:.1} uploads {:.2} ms, \
         {:.1} with clamps ahead {:.2} ms; {:.1} found open, {:.1} reset ahead at the list's reset {:.2} ms \
         (VICTORY_PRELUDE_AHEAD={})",
        per(HOST_STEPS),
        per(ALONE_STEPS),
        per(JOINED),
        ms(JOIN_NS),
        per(SKIPPED),
        u8::from(alone_on()),
        ms(WORK_NS),
        per(PRELUDES),
        per(RECORDED),
        ms(PRELUDE_NS),
        per(RESETS),
        ms(RESET_NS),
        ms(CLOSE_NS),
        per(UPLOADS),
        ms(UPLOAD_NS),
        per(CLAMPS),
        ms(CLAMP_NS),
        per(FOUND_OPEN),
        per(AHEAD_RESETS),
        ms(AHEAD_NS),
        u8::from(ahead_on()),
    );
}
