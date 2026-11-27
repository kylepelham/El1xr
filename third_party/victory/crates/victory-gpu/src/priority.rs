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
//! Source: readmet3xt-lab/foxhound (`dstorage.rs`, `cores.rs`:
//! `VICTORY_DSTORAGE_PRIORITY`, `VICTORY_DSTORAGE_WORKERS`,
//! `VICTORY_HELPER_PRIORITY=<n>`) and TaynanGT/Vice-Tay PR #213 (`5c803d236b`,
//! `cores::Priority`: the helpers the submitting thread waits for above
//! normal). Measured upstream on a cutscene with `VICTORY_HELPER_PRIORITY=1`
//! (foxhound, 4070S, one run each): mean 37.2 → 36.6 ms a frame, worst
//! second 74.8 → 63.8 ms. Vice-Tay (i5-12400F, RTX 3060): the waits for
//! helpers shrank (refreshes 19.1 → 16.9 ms a frame), fps did not move.

use std::sync::OnceLock;

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
    use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadPriority};
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
    let Some(priority) = priority else {
        return;
    };
    let current = std::thread::current();
    if !ours(current.name()) || DONE.with(|done| done.replace(true)) {
        return;
    }
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
    if let Some(priority) = dstorage_priority()
        && !set(priority)
    {
        tracing::warn!(priority, error = %std::io::Error::last_os_error(), "dstorage: worker priority refused");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::System::Threading::{GetCurrentThread, GetThreadPriority};

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
}
