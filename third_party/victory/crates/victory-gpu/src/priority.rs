//! Our threads' priorities, beside their placement (`crate::cores`,
//! `crate::topology`). Two switches, both off by default:
//!
//! - `VICTORY_HELPER_PRIORITY=<-2..2>`: the threads a frame waits for that
//!   are ours (named `victory-…`: the submitting thread, the relay, the heap
//!   sync, the `shadow::wide` workers) run at that thread priority, 1 being
//!   above normal. They are the threads that ask for the fast cores
//!   (`cores::keep_on_fast_cores`, `cores::keep_helper_on_fast_cores`) or
//!   the hot sets (`topology::pin_hot`), so the switch applies with
//!   `VICTORY_VCACHE_PIN` on or off. The title's own threads that ask too
//!   (a list's recording, a submission on a title thread) keep their
//!   priority: the title set it.
//! - `VICTORY_DSTORAGE_PRIORITY=<-2..2>` (-1: below normal) for the
//!   DirectStorage unpack workers (`victory_hle`'s `dstorage`), and
//!   `VICTORY_DSTORAGE_WORKERS=<n>`: that many of them (default: one per
//!   logical processor, as before).
//!
//! With both on (1 and -1) a frame's threads come before the unpacking when
//! the processor is full, and the unpacking takes what is left. With
//! `VICTORY_STREAM_PRIORITY=1` (the title's mapping thread lifted to above
//! normal) that thread and our helpers are at the same level, both above
//! the unpacking it waits on less than the frame does. Placement is not
//! touched: `VICTORY_VCACHE_PIN` still sends the workers to the other cache
//! and the hot threads to the large one.
//!
//! All three are live (`victory_core::settings`): the threads they apply to
//! are kept (a duplicated handle each, also with the switch off), and a
//! change sets every kept thread again (0: back to normal, or for the
//! title's mapping threads to their own priority).
//!
//! Source: readmet3xt-lab/foxhound (`dstorage.rs`, `cores.rs`:
//! `VICTORY_DSTORAGE_PRIORITY`, `VICTORY_DSTORAGE_WORKERS`,
//! `VICTORY_HELPER_PRIORITY=<n>`) and TaynanGT/Vice-Tay PR #213 (`5c803d236b`,
//! `cores::Priority`: the helpers the submitting thread waits for above
//! normal). Measured upstream on a cutscene with `VICTORY_HELPER_PRIORITY=1`
//! (foxhound, 4070S, one run each): mean 37.2 → 36.6 ms a frame, worst
//! second 74.8 → 63.8 ms. Vice-Tay (i5-12400F, RTX 3060): the waits for
//! helpers shrank (refreshes 19.1 → 16.9 ms a frame), fps did not move.

use std::sync::{Mutex, Once, OnceLock};

use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::System::Threading::{GetThreadPriority, SetThreadPriority};

/// Threads a live change applies to: a duplicated handle each (as `usize`,
/// for `Send`) and, for the title's mapping threads, the priority they had.
static HELPERS: Mutex<Vec<usize>> = Mutex::new(Vec::new());
static WORKERS: Mutex<Vec<usize>> = Mutex::new(Vec::new());
static STREAMERS: Mutex<Vec<(usize, i32)>> = Mutex::new(Vec::new());

/// A handle to the calling thread that other threads can use (0 if refused).
fn keep_current() -> usize {
    use windows_sys::Win32::Foundation::{DUPLICATE_SAME_ACCESS, DuplicateHandle};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetCurrentThread};
    let mut out: HANDLE = std::ptr::null_mut();
    // SAFETY: duplicates this thread's pseudo handle into a real one in this process.
    let ok = unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            GetCurrentThread(),
            GetCurrentProcess(),
            &mut out,
            0,
            0,
            DUPLICATE_SAME_ACCESS,
        )
    };
    if ok != 0 { out as usize } else { 0 }
}

fn keep(list: &Mutex<Vec<usize>>) {
    let handle = keep_current();
    if handle != 0 {
        list.lock().unwrap_or_else(|e| e.into_inner()).push(handle);
    }
}

/// Sets every thread in `list` to `priority`; how many took it (a thread
/// that ended refuses).
fn set_all(list: &Mutex<Vec<usize>>, priority: i32) -> usize {
    let list = list.lock().unwrap_or_else(|e| e.into_inner());
    // SAFETY: handles duplicated by `keep_current`, never closed.
    list.iter()
        .filter(|&&h| unsafe { SetThreadPriority(h as HANDLE, priority) } != 0)
        .count()
}

/// Registers the live hooks once (from the first thread that asks).
fn hook() {
    static HOOKED: Once = Once::new();
    HOOKED.call_once(|| {
        victory_core::settings::on_change("VICTORY_HELPER_PRIORITY", |v| {
            let priority = parse_priority(Some(v)).unwrap_or(0);
            let threads = set_all(&HELPERS, priority);
            tracing::info!(priority, threads, "helper threads' priority changed (VICTORY_HELPER_PRIORITY)");
        });
        victory_core::settings::on_change("VICTORY_DSTORAGE_PRIORITY", |v| {
            let priority = parse_priority(Some(v)).unwrap_or(0);
            let threads = set_all(&WORKERS, priority);
            tracing::info!(priority, threads, "dstorage workers' priority changed (VICTORY_DSTORAGE_PRIORITY)");
        });
        victory_core::settings::on_change("VICTORY_STREAM_PRIORITY", |v| {
            let on = v == "1";
            let list = STREAMERS.lock().unwrap_or_else(|e| e.into_inner());
            // SAFETY: handles duplicated by `keep_current`, never closed.
            let threads = list
                .iter()
                .filter(|&&(h, was)| unsafe { SetThreadPriority(h as HANDLE, if on { 1 } else { was }) } != 0)
                .count();
            tracing::info!(on, threads, "title mapping threads' priority changed (VICTORY_STREAM_PRIORITY)");
        });
    });
}

/// A thread priority from `-2` to `2` in `value`; `None` (leave the thread
/// alone) for 0, nothing, or anything else.
fn parse_priority(value: Option<&str>) -> Option<i32> {
    let p = value?.trim().parse::<i32>().ok()?;
    (p != 0).then_some(p.clamp(-2, 2))
}

/// `VICTORY_HELPER_PRIORITY`, read once.
fn helper_priority() -> Option<i32> {
    static P: OnceLock<Option<i32>> = OnceLock::new();
    *P.get_or_init(|| parse_priority(std::env::var("VICTORY_HELPER_PRIORITY").ok().as_deref()))
}

/// Whether a thread of this name is one of ours (the title's threads have
/// no Rust name).
fn ours(name: Option<&str>) -> bool {
    name.is_some_and(|n| n.starts_with("victory-"))
}

/// Sets the calling thread's priority; false if the system refused.
fn set(priority: i32) -> bool {
    use windows_sys::Win32::System::Threading::GetCurrentThread;
    // SAFETY: the calling thread's pseudo handle.
    unsafe { SetThreadPriority(GetCurrentThread(), priority) != 0 }
}

/// `VICTORY_HELPER_PRIORITY` for the calling thread if it is one of ours,
/// once per thread. Called where our threads a frame waits for ask for
/// their cores.
pub(crate) fn helper() {
    helper_with(helper_priority());
}

fn helper_with(priority: Option<i32>) {
    thread_local! {
        static DONE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }
    if DONE.with(|done| done.replace(true)) {
        return;
    }
    let current = std::thread::current();
    if !ours(current.name()) {
        return;
    }
    hook();
    keep(&HELPERS);
    let Some(priority) = priority else {
        return;
    };
    let ok = set(priority);
    tracing::info!(
        what = current.name().unwrap_or("thread"),
        priority,
        ok,
        "the thread's priority (VICTORY_HELPER_PRIORITY)"
    );
}

/// How many DirectStorage unpack workers to start (`VICTORY_DSTORAGE_WORKERS`,
/// else one per logical processor).
pub fn dstorage_workers() -> usize {
    workers_from(
        std::env::var("VICTORY_DSTORAGE_WORKERS").ok().as_deref(),
        std::thread::available_parallelism().map_or(4, |n| n.get()),
    )
}

fn workers_from(value: Option<&str>, processors: usize) -> usize {
    match value.and_then(|v| v.trim().parse::<usize>().ok()) {
        Some(n) if n > 0 => n,
        _ => processors,
    }
}

/// `VICTORY_DSTORAGE_PRIORITY` (`None`: as made), read once.
pub fn dstorage_priority() -> Option<i32> {
    static P: OnceLock<Option<i32>> = OnceLock::new();
    *P.get_or_init(|| parse_priority(std::env::var("VICTORY_DSTORAGE_PRIORITY").ok().as_deref()))
}

/// Called by each DirectStorage unpack worker as it starts.
pub fn dstorage_worker() {
    hook();
    keep(&WORKERS);
    if let Some(priority) = dstorage_priority()
        && !set(priority)
    {
        tracing::warn!(priority, error = %std::io::Error::last_os_error(), "dstorage: worker priority refused");
    }
}

/// Lifts the calling title thread above normal priority if it runs below
/// normal (`VICTORY_STREAM_PRIORITY=1`, after cool_fox 49d71266b2), once per
/// thread. Called where the title's streaming thread scatters pages and
/// copies page mappings (`victory_hle`'s `xmem`): that thread runs below
/// normal, and every mapping call it makes waits for the process's
/// address-space lock behind our guards' and the driver's protection
/// changes, so loading waits on it. Below-normal threads are kept with the
/// switch off too, for a live change.
pub fn stream_thread() {
    use windows_sys::Win32::System::Threading::{GetCurrentThread, THREAD_PRIORITY_ABOVE_NORMAL};
    thread_local!(static LOOKED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) });
    if LOOKED.with(|l| l.replace(true)) {
        return;
    }
    // SAFETY: plain call on this thread's pseudo handle.
    let was = unsafe { GetThreadPriority(GetCurrentThread()) };
    if !matches!(was, -2..0) {
        return;
    }
    hook();
    let handle = keep_current();
    if handle != 0 {
        STREAMERS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((handle, was));
    }
    if std::env::var_os("VICTORY_STREAM_PRIORITY").is_some_and(|v| v == "1")
        // SAFETY: this thread's pseudo handle.
        && unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL) } != 0
    {
        tracing::info!(
            was,
            "a title thread that maps pages lifted above normal priority (VICTORY_STREAM_PRIORITY=1)"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::System::Threading::GetCurrentThread;

    fn priority_now() -> i32 {
        // SAFETY: the calling thread's pseudo handle.
        unsafe { GetThreadPriority(GetCurrentThread()) }
    }

    #[test]
    fn priorities_parse_within_two_steps_and_zero_leaves_alone() {
        assert_eq!(parse_priority(None), None);
        assert_eq!(parse_priority(Some("")), None);
        assert_eq!(parse_priority(Some("0")), None);
        assert_eq!(parse_priority(Some("on")), None);
        assert_eq!(parse_priority(Some(" 1 ")), Some(1));
        assert_eq!(parse_priority(Some("-1")), Some(-1));
        assert_eq!(parse_priority(Some("15")), Some(2));
        assert_eq!(parse_priority(Some("-7")), Some(-2));
    }

    #[test]
    fn workers_default_to_the_processors() {
        assert_eq!(workers_from(None, 32), 32);
        assert_eq!(workers_from(Some("0"), 32), 32);
        assert_eq!(workers_from(Some("x"), 32), 32);
        assert_eq!(workers_from(Some("8"), 32), 8);
    }

    #[test]
    fn only_our_named_threads_are_raised_and_once() {
        assert!(ours(Some("victory-submit")));
        assert!(ours(Some("victory-wide-0-3")));
        assert!(!ours(Some("title-record")));
        assert!(!ours(None));
        let raised = std::thread::Builder::new()
            .name("victory-test-helper".into())
            .spawn(|| {
                let before = priority_now();
                helper_with(Some(1));
                let after = priority_now();
                // Once per thread: a later call does not set it again.
                set(0);
                helper_with(Some(1));
                (before, after, priority_now())
            })
            .unwrap()
            .join()
            .unwrap();
        assert_eq!(raised, (0, 1, 0));
        let unnamed = std::thread::spawn(|| {
            helper_with(Some(1));
            priority_now()
        })
        .join()
        .unwrap();
        assert_eq!(unnamed, 0, "a thread without our name keeps its priority");
        let off = std::thread::Builder::new()
            .name("victory-test-off".into())
            .spawn(|| {
                helper_with(None);
                priority_now()
            })
            .unwrap()
            .join()
            .unwrap();
        assert_eq!(off, 0, "switch off: as made");
    }

    #[test]
    fn a_live_change_sets_the_kept_threads_again() {
        // A list of its own: other tests' threads are in `HELPERS`.
        static KEPT: Mutex<Vec<usize>> = Mutex::new(Vec::new());
        let (go, wait) = std::sync::mpsc::channel::<()>();
        let (back, seen) = std::sync::mpsc::channel::<i32>();
        let thread = std::thread::spawn(move || {
            keep(&KEPT);
            back.send(priority_now()).unwrap();
            wait.recv().unwrap();
            back.send(priority_now()).unwrap();
        });
        assert_eq!(seen.recv().unwrap(), 0, "kept, as made");
        assert_eq!(set_all(&KEPT, 1), 1);
        go.send(()).unwrap();
        assert_eq!(seen.recv().unwrap(), 1);
        thread.join().unwrap();
    }
}
