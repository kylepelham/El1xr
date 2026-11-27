//! The ray-tracing structure builder's `DispatchX` calls.
//!
//! `VICTORY_RT_DIAG=1` (off by default): every 5 s, one line per
//! (program, flags) of the `DispatchX` calls since, with the program's
//! thread-group size and the range of `x`, and how many builder dispatches
//! ran or were dropped (`crate::pipeline::builder_runs`). Nothing changes.
//!
//! `VICTORY_DISPATCHX_THREADS` (off by default) reads the dimensions of a
//! `DispatchX` as thread counts and dispatches the groups that hold them,
//! rounded up. The flags look like the console's
//! `COMPUTE_DISPATCH_INITIATOR` (`0x20` and `0x8020` in the builder's
//! calls): bit 5 `USE_THREAD_DIMENSIONS`, bit 15 `CS_W32_EN` (Mesa `sid.h`,
//! AMD's register names). An offline replay of captured builder
//! submissions on NVIDIA cards (Marvisak/VIctory issue #3486, comment by
//! Slay326) found that the calls with flags `0x20` count threads in `x`:
//! launched as `x` groups, the surplus lanes wrote outside the build's
//! ranges and a rehydrated bottom level's root became `ffffffff`. `=1`
//! counts threads where bit 5 is set and bit 15 is not (what that replay
//! measured); `=2` wherever bit 5 is set (the register's meaning).
//! The lanes past the count in the last group (up to one group less one
//! lane) still run unless `VICTORY_DISPATCHX_MASK=1`: then a call read as a
//! thread count hands its `x` to the program as a host word
//! (`victory_shader::emit::USER_THREAD_LIMIT`, [`thread_limit`]), and the
//! builder's programs make their stores only below it, as the replay
//! masked them.

use std::cell::Cell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use crate::object::This;

/// `COMPUTE_DISPATCH_INITIATOR.USE_THREAD_DIMENSIONS`.
const THREAD_DIMENSIONS: u32 = 1 << 5;
/// `COMPUTE_DISPATCH_INITIATOR.CS_W32_EN`.
const WAVE32: u32 = 1 << 15;

/// Whether `VICTORY_RT_DIAG=1`.
pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_RT_DIAG").is_some_and(|v| v == "1"))
}

/// `VICTORY_DISPATCHX_THREADS`: 0 off, 1 or 2 (see the module).
fn threads_mode() -> u8 {
    static MODE: OnceLock<u8> = OnceLock::new();
    *MODE.get_or_init(|| parse_mode(std::env::var("VICTORY_DISPATCHX_THREADS").ok().as_deref()))
}

fn parse_mode(value: Option<&str>) -> u8 {
    match value.map(str::trim) {
        Some("1") => 1,
        Some("2") => 2,
        _ => 0,
    }
}

thread_local! {
    /// The thread count in x of the `DispatchX` this thread is recording
    /// ([`with_thread_limit`]); `u32::MAX` outside one.
    static LIMIT: Cell<u32> = const { Cell::new(u32::MAX) };
}

/// Whether `VICTORY_DISPATCHX_MASK=1` (the translator reads it too, for its
/// programs and the shader cache key).
fn mask_on() -> bool {
    victory_shader::emit::dispatchx_mask()
}

/// The host word `victory_shader::emit::USER_THREAD_LIMIT`: the thread
/// count in x of the `DispatchX` being recorded on this thread, `u32::MAX`
/// for any other dispatch.
pub(crate) fn thread_limit() -> u32 {
    LIMIT.get()
}

/// Runs `f` (the dispatch) with `limit` as [`thread_limit`] when it is
/// `Some`.
pub(crate) fn with_thread_limit<R>(limit: Option<u32>, f: impl FnOnce() -> R) -> R {
    let Some(limit) = limit else {
        return f();
    };
    let before = LIMIT.replace(limit);
    let r = f();
    LIMIT.set(before);
    r
}

/// Whether a `DispatchX` with `flags` counts threads under `mode`.
fn counts_threads(mode: u8, flags: u32) -> bool {
    let threads = flags & THREAD_DIMENSIONS != 0;
    match mode {
        1 => threads && flags & WAVE32 == 0,
        2 => threads,
        _ => false,
    }
}

/// The groups that hold `dims` threads in groups of `group`.
fn groups_for(dims: [u32; 3], group: [u32; 3]) -> [u32; 3] {
    std::array::from_fn(|i| dims[i].div_ceil(group[i].max(1)))
}

/// What the calls of one (program, flags) did since the last line.
#[derive(Default)]
struct Calls {
    hash: String,
    group: [u32; 3],
    count: u64,
    x_min: u32,
    x_max: u32,
    yz_max: u32,
    /// `x` not a multiple of the group's width.
    ragged: u64,
    /// Read as thread counts (`VICTORY_DISPATCHX_THREADS`).
    as_threads: u64,
}

static CALLS: Mutex<Option<HashMap<(usize, u32), Calls>>> = Mutex::new(None);
static BUILDER_RAN: AtomicU64 = AtomicU64::new(0);
static BUILDER_DROPPED: AtomicU64 = AtomicU64::new(0);

/// A builder dispatch ran (`true`) or was dropped.
pub(crate) fn builder(ran: bool) {
    if on() {
        if ran { &BUILDER_RAN } else { &BUILDER_DROPPED }.fetch_add(1, Ordering::Relaxed);
    }
}

/// At a `DispatchX(x, y, z, flags)` of the title's pipeline `this`: the
/// group counts to dispatch, and the thread limit in x for its programs
/// (`VICTORY_DISPATCHX_MASK=1`, [`with_thread_limit`]).
pub(crate) fn dispatch_x(this: This, dims: [u32; 3], flags: u32) -> ([u32; 3], Option<u32>) {
    let threads = counts_threads(threads_mode(), flags);
    if !threads && !on() {
        return (dims, None);
    }
    let Some((hash, group)) = group_of(this) else {
        return (dims, None);
    };
    let limit = (threads && mask_on()).then_some(dims[0]);
    let out = if threads {
        groups_for(dims, group)
    } else {
        dims
    };
    if on() {
        note(this as usize, &hash, group, dims, flags, threads);
        summary();
    }
    (out, limit)
}

/// `this`'s shader hash and thread-group size, the size parsed once per
/// shader.
fn group_of(this: This) -> Option<(String, [u32; 3])> {
    static GROUPS: Mutex<Option<HashMap<String, Option<[u32; 3]>>>> = Mutex::new(None);
    let (hash, _) = crate::pipeline::compute_group(this, false)?;
    if let Some(&known) = GROUPS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .and_then(|g| g.get(&hash))
    {
        return known.map(|g| (hash, g));
    }
    let group = crate::pipeline::compute_group(this, true).and_then(|(_, g)| g);
    GROUPS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_or_insert_with(HashMap::new)
        .insert(hash.clone(), group);
    group.map(|g| (hash, g))
}

fn note(pipeline: usize, hash: &str, group: [u32; 3], dims: [u32; 3], flags: u32, threads: bool) {
    let mut calls = CALLS.lock().unwrap_or_else(|e| e.into_inner());
    let c = calls
        .get_or_insert_with(HashMap::new)
        .entry((pipeline, flags))
        .or_insert_with(|| Calls {
            hash: hash.to_owned(),
            group,
            x_min: u32::MAX,
            ..Calls::default()
        });
    c.count += 1;
    c.x_min = c.x_min.min(dims[0]);
    c.x_max = c.x_max.max(dims[0]);
    c.yz_max = c.yz_max.max(dims[1]).max(dims[2]);
    c.ragged += u64::from(dims[0] % group[0].max(1) != 0);
    c.as_threads += u64::from(threads);
}

/// Every 5 s: the calls since.
fn summary() {
    static LAST: Mutex<Option<std::time::Instant>> = Mutex::new(None);
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    let now = std::time::Instant::now();
    if last.is_some_and(|t| now.duration_since(t).as_secs() < 5) {
        return;
    }
    let first = last.is_none();
    *last = Some(now);
    drop(last);
    let calls = CALLS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
        .unwrap_or_default();
    if first {
        return;
    }
    tracing::info!(
        ran = BUILDER_RAN.swap(0, Ordering::Relaxed),
        dropped = BUILDER_DROPPED.swap(0, Ordering::Relaxed),
        programs = calls.len(),
        mode = threads_mode(),
        "rt diag: structure builder dispatches in the last 5 s (VICTORY_BVH_BUILDER=1 runs them)"
    );
    let mut calls: Vec<_> = calls.into_iter().collect();
    calls.sort_by(|a, b| b.1.count.cmp(&a.1.count));
    for ((_, flags), c) in calls {
        tracing::info!(
            shader = c.hash,
            flags = format_args!("{flags:#x}"),
            group = format_args!("{}x{}x{}", c.group[0], c.group[1], c.group[2]),
            calls = c.count,
            x = format_args!("{}..{}", c.x_min, c.x_max),
            yz_max = c.yz_max,
            ragged = c.ragged,
            as_threads = c.as_threads,
            "rt diag: DispatchX (ragged: x not a multiple of the group width; as_threads: dispatched as thread counts, VICTORY_DISPATCHX_THREADS)"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn off_unless_asked() {
        assert_eq!(parse_mode(None), 0);
        assert_eq!(parse_mode(Some("0")), 0);
        assert_eq!(parse_mode(Some("yes")), 0);
        assert_eq!(parse_mode(Some("1")), 1);
        assert_eq!(parse_mode(Some(" 2 ")), 2);
    }

    #[test]
    fn mode_1_counts_threads_only_for_wave64_thread_dimension_calls() {
        assert!(counts_threads(1, 0x20));
        assert!(!counts_threads(1, 0x8020));
        assert!(!counts_threads(1, 0x8000));
        assert!(!counts_threads(1, 0));
        assert!(counts_threads(2, 0x20));
        assert!(counts_threads(2, 0x8020));
        assert!(!counts_threads(2, 0x8000));
        for flags in [0, 0x20, 0x8020] {
            assert!(!counts_threads(0, flags));
        }
    }

    #[test]
    fn the_thread_limit_holds_only_inside_its_dispatch() {
        assert_eq!(thread_limit(), u32::MAX);
        assert_eq!(with_thread_limit(Some(100), thread_limit), 100);
        assert_eq!(thread_limit(), u32::MAX);
        assert_eq!(with_thread_limit(None, thread_limit), u32::MAX);
        assert!(!mask_on());
    }

    #[test]
    fn thread_counts_round_up_to_groups() {
        assert_eq!(groups_for([1, 1, 1], [64, 1, 1]), [1, 1, 1]);
        assert_eq!(groups_for([64, 1, 1], [64, 1, 1]), [1, 1, 1]);
        assert_eq!(groups_for([65, 1, 1], [64, 1, 1]), [2, 1, 1]);
        assert_eq!(groups_for([11632, 1, 1], [64, 1, 1]), [182, 1, 1]);
        assert_eq!(groups_for([1038, 3, 1], [32, 2, 1]), [33, 2, 1]);
        assert_eq!(groups_for([0, 1, 1], [64, 1, 1]), [0, 1, 1]);
        assert_eq!(groups_for([5, 1, 1], [0, 1, 1]), [5, 1, 1]);
    }
}
