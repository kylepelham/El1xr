//! Copy-volume profile of shadow refreshes (`VICTORY_COPY_PROFILE=1`, off by default).
//!
//! Splits the bytes the submissions copy out of title memory per frame by where
//! they go (shadow kind, batch fills, `Step::Copy`), by why a page was taken
//! (never copied, 4 KiB sub-pages, a whole 64 KiB page of view memory or of
//! other memory, an unwatched copy), how much of a `copy_changed` compare
//! really differed, and the shadows that copy the most. Off, every hook is one
//! cached boolean test.

use std::cell::Cell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Mutex, OnceLock};

const KERNEL_PAGE: u64 = 4096;
const PAGE: u64 = 64 * 1024;
const TOP: usize = 15;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ShadowKind {
    BufCard = 0,
    BufHost = 1,
    WinTransient = 2,
    WinCard = 3,
    WinRebar = 4,
    WinHost = 5,
}

const KINDS: [ShadowKind; 6] = [
    ShadowKind::BufCard,
    ShadowKind::BufHost,
    ShadowKind::WinTransient,
    ShadowKind::WinCard,
    ShadowKind::WinRebar,
    ShadowKind::WinHost,
];

impl ShadowKind {
    const fn label(self) -> &'static str {
        match self {
            Self::BufCard => "buf_card",
            Self::BufHost => "buf_host",
            Self::WinTransient => "win_transient",
            Self::WinCard => "win_card",
            Self::WinRebar => "win_rebar",
            Self::WinHost => "win_host",
        }
    }
}

/// Why a page was taken by a watched refresh.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Reason {
    First = 0,
    Mask4k = 1,
    WholeView = 2,
    WholeOther = 3,
    Unwatched = 4,
}

const REASONS: [&str; 5] = [
    "first_ever",
    "mask_4k",
    "whole_64k_view",
    "whole_64k_other",
    "unwatched",
];

/// Why `watch::stale_mask` fell back to the whole 64 KiB page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WholeWhy {
    WholeStamp = 0,
    NoSubsPart = 1,
    ZeroMask = 2,
    NoWatch4k = 3,
}

pub(crate) fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_COPY_PROFILE").is_some_and(|v| v == "1"))
}

static FILL: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];
static STEP_COPY: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];
static KIND: [[AtomicU64; 3]; 6] = [const { [const { AtomicU64::new(0) }; 3] }; 6];
static REASON: [[AtomicU64; 2]; 5] = [const { [const { AtomicU64::new(0) }; 2] }; 5];
static WHOLE_WHY: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];
static DIFF: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

/// One refresh on this thread: reason bytes (upper bound: whole 64 KiB page or
/// its 4 KiB sub-pages), `copy_changed` bytes read and differing, view memory.
#[derive(Clone, Copy, Default)]
struct Refresh {
    reason: [u64; 5],
    diff: [u64; 2],
    view: bool,
}

thread_local! {
    static NOW: Cell<Refresh> = const { Cell::new(Refresh { reason: [0; 5], diff: [0; 2], view: false }) };
}

#[derive(Clone, Copy, Default)]
struct Entry {
    start: u64,
    len: u64,
    kind: Option<ShadowKind>,
    view: bool,
    calls: u64,
    asked: u64,
    bytes: u64,
    us: u64,
    reason: [u64; 5],
    diff: [u64; 2],
}

static SHADOWS: Mutex<Option<HashMap<u64, Entry>>> = Mutex::new(None);

/// A refresh starts on this thread; `view` when it is view memory.
pub(crate) fn begin(view: bool) {
    NOW.with(|c| {
        c.set(Refresh {
            view,
            ..Refresh::default()
        })
    });
}

/// Why a page with last copy `prev` (`0`: never) and sub-page `mask` was
/// taken, and its bytes at most.
fn classify(prev: u64, mask: Option<u16>, view: bool) -> (Reason, u64) {
    match mask {
        _ if prev == 0 => (Reason::First, PAGE),
        Some(m) => (Reason::Mask4k, u64::from(m.count_ones()) * KERNEL_PAGE),
        None if view => (Reason::WholeView, PAGE),
        None => (Reason::WholeOther, PAGE),
    }
}

/// A watched refresh takes `page` (last copied at `prev`, `0` never) with
/// `mask` 4 KiB sub-pages (`None`: the whole page).
pub(crate) fn page(prev: u64, mask: Option<u16>) {
    if !enabled() {
        return;
    }
    NOW.with(|c| {
        let mut r = c.get();
        let (reason, bytes) = classify(prev, mask, r.view);
        r.reason[reason as usize] += bytes;
        c.set(r);
    });
}

pub(crate) fn whole_why(why: WholeWhy) {
    if enabled() {
        WHOLE_WHY[why as usize].fetch_add(1, Relaxed);
    }
}

/// `copy_changed` read `read` bytes, of which `changed` differed.
pub(crate) fn diff(read: usize, changed: usize) {
    if !enabled() || read == 0 {
        return;
    }
    DIFF[0].fetch_add(read as u64, Relaxed);
    DIFF[1].fetch_add(changed as u64, Relaxed);
    NOW.with(|c| {
        let mut r = c.get();
        r.diff[0] += read as u64;
        r.diff[1] += changed as u64;
        c.set(r);
    });
}

pub(crate) fn fill(bytes: usize, us: u64) {
    if enabled() && bytes > 0 {
        add3(&FILL, bytes as u64, us);
    }
}

pub(crate) fn step_copy(bytes: usize, us: u64) {
    if enabled() && bytes > 0 {
        add3(&STEP_COPY, bytes as u64, us);
    }
}

fn add3(at: &[AtomicU64; 3], bytes: u64, us: u64) {
    at[0].fetch_add(1, Relaxed);
    at[1].fetch_add(bytes, Relaxed);
    at[2].fetch_add(us, Relaxed);
}

/// The refresh begun by [`begin`] ends: `copied` bytes of the `asked` ones,
/// `unwatched` when copied whole without the write watch.
#[allow(clippy::too_many_arguments)]
pub(crate) fn end(
    id: u64,
    start: u64,
    len: u64,
    kind: ShadowKind,
    asked: usize,
    copied: usize,
    us: u64,
    unwatched: bool,
) {
    let mut r = NOW.with(|c| c.take());
    if copied == 0 {
        return;
    }
    if unwatched {
        r.reason[Reason::Unwatched as usize] += copied as u64;
    }
    add3(&KIND[kind as usize], copied as u64, us);
    for (i, &b) in r.reason.iter().enumerate() {
        if b > 0 {
            REASON[i][0].fetch_add(b, Relaxed);
            REASON[i][1].fetch_add(1, Relaxed);
        }
    }
    let mut map = SHADOWS.lock().unwrap_or_else(|e| e.into_inner());
    let e = map.get_or_insert_with(HashMap::new).entry(id).or_default();
    e.start = start;
    e.len = len;
    e.kind = Some(kind);
    e.view |= r.view;
    e.calls += 1;
    e.asked += asked as u64;
    e.bytes += copied as u64;
    e.us += us;
    for i in 0..5 {
        e.reason[i] += r.reason[i];
    }
    e.diff[0] += r.diff[0];
    e.diff[1] += r.diff[1];
}

fn take3(at: &[AtomicU64; 3]) -> [u64; 3] {
    [
        at[0].swap(0, Relaxed),
        at[1].swap(0, Relaxed),
        at[2].swap(0, Relaxed),
    ]
}

/// The profile lines, every meter period of `frames` frames.
pub(crate) fn line(frames: f64) {
    if !enabled() || frames <= 0.0 {
        return;
    }
    let mb = |b: u64| b as f64 / 1e6 / frames;
    let ms = |us: u64| us as f64 / 1e3 / frames;
    let per = |n: u64| n as f64 / frames;
    let [fill_n, fill_b, fill_us] = take3(&FILL);
    let [step_n, step_b, step_us] = take3(&STEP_COPY);
    let mut kinds = Vec::new();
    let mut shadows = 0;
    for kind in KINDS {
        let [n, b, us] = take3(&KIND[kind as usize]);
        shadows += b;
        if b > 0 {
            kinds.push(format!(
                "{} {:.1} MB ({:.0}x, {:.2} ms)",
                kind.label(),
                mb(b),
                per(n),
                ms(us)
            ));
        }
    }
    tracing::info!(
        "copy profile per frame (VICTORY_COPY_PROFILE=1): {:.1} MB copied; shadows {:.1} MB [{}]; fills {:.1} MB ({:.0}x, {:.2} ms); step copies {:.1} MB ({:.0}x, {:.2} ms)",
        mb(shadows + fill_b + step_b),
        mb(shadows),
        kinds.join(", "),
        mb(fill_b),
        per(fill_n),
        ms(fill_us),
        mb(step_b),
        per(step_n),
        ms(step_us),
    );
    let reasons: Vec<String> = REASONS
        .iter()
        .enumerate()
        .map(|(i, name)| {
            let b = REASON[i][0].swap(0, Relaxed);
            let n = REASON[i][1].swap(0, Relaxed);
            format!("{name} {:.1} MB ({:.0}x)", mb(b), per(n))
        })
        .collect();
    let why = [0, 1, 2, 3].map(|i| per(WHOLE_WHY[i].swap(0, Relaxed)));
    let (read, changed) = (DIFF[0].swap(0, Relaxed), DIFF[1].swap(0, Relaxed));
    tracing::info!(
        "copy profile reasons per frame (page bound): {}; whole pages from the 4 KiB watch: newer whole stamp {:.0}, no sub-page stamps {:.0}, empty mask {:.0}, watch off {:.0}; copy_changed read {:.1} MB, differing {:.1} MB",
        reasons.join(", "),
        why[0],
        why[1],
        why[2],
        why[3],
        mb(read),
        mb(changed),
    );
    let taken = SHADOWS.lock().unwrap_or_else(|e| e.into_inner()).take();
    let mut list: Vec<(u64, Entry)> = taken.into_iter().flatten().collect();
    list.sort_unstable_by_key(|&(_, e)| std::cmp::Reverse(e.bytes));
    for (id, e) in list.into_iter().take(TOP) {
        tracing::info!(
            "  copy profile top: copy {id} {:#x}+{:#x} ({:.2} MB) {} view={} | {:.1} MB/frame in {:.1} refreshes ({:.2} ms, {:.0} KB asked each) [first {:.1}, 4k {:.1}, whole view {:.1}, whole other {:.1}, unwatched {:.1}; differing {:.1} of {:.1} MB compared]",
            e.start,
            e.len,
            e.len as f64 / f64::from(1 << 20),
            e.kind.map_or("?", ShadowKind::label),
            u8::from(e.view),
            mb(e.bytes),
            per(e.calls),
            ms(e.us),
            e.asked as f64 / e.calls.max(1) as f64 / 1024.0,
            mb(e.reason[0]),
            mb(e.reason[1]),
            mb(e.reason[2]),
            mb(e.reason[3]),
            mb(e.reason[4]),
            mb(e.diff[1]),
            mb(e.diff[0]),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pages_are_sorted_by_reason_and_sized_by_mask() {
        assert_eq!(classify(0, Some(1), true), (Reason::First, PAGE));
        assert_eq!(classify(5, Some(0b101), false), (Reason::Mask4k, 2 * KERNEL_PAGE));
        assert_eq!(classify(5, None, true), (Reason::WholeView, PAGE));
        assert_eq!(classify(5, None, false), (Reason::WholeOther, PAGE));
    }
}
