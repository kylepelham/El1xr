//! Opt-in per-shadow and per-reason copy-volume profiler (`VICTORY_COPY_PROFILE=1`).
//!
//! Zero cost when unset (single cached boolean check). When enabled, logs a
//! breakdown of every byte counted in `PREPARING[1]` alongside the frame line:
//! - split by source (`Shadow::update` by shadow kind, `Batch::flush_fills`, `Step::Copy`)
//! - split by dirty reason (`first_ever`, `mask_4k`, `whole_64k_view`, `whole_64k_other`,
//!   `unwatched`, plus `partial_unmarked` pages whose `copied` stamp was not advanced)
//! - `copy_changed` scan bytes vs actually differing bytes
//! - top 15 individual shadows by MB/frame copied.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Mutex, OnceLock};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub(crate) enum ShadowKind {
    BufCard = 0,
    BufHost = 1,
    WinTransient = 2,
    WinCard = 3,
    WinRebar = 4,
    WinHost = 5,
}

impl ShadowKind {
    const COUNT: usize = 6;

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

/// Exact reason `stale_mask` fell back to a whole 64 KiB page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub(crate) enum WholeWhy {
    WholeStamp = 0,
    NoSubsPart = 1,
    ZeroMask = 2,
    NoWatch4k = 3,
}

/// Whether watched refreshes expand partial edge pages to whole 64 KiB page
/// boundaries (clamped to `[start, end)` and skipping any edge page that
/// overlaps pending shader writes) so `copied` timestamps advance instead of
/// re-copying the same partial pages on every submission (`VICTORY_EXPAND_PARTIAL=0`
/// restores partial-only clipping).
pub(crate) fn expand_partial_pages() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_EXPAND_PARTIAL").is_none_or(|v| v != "0"))
}

/// Computes `(eff_from, eff_to)` for a watched refresh of `[from, to)` inside
/// a shadow covering `[start, end)`. Expands partial first/last 64 KiB pages to
/// whole page boundaries (clamped to `[start, end)`) whenever `allow_expand` is
/// true, `VICTORY_EXPAND_PARTIAL` is not `"0"`, and `skip_busy` does not overlap
/// that edge page.
pub(crate) fn expand_bounds(
    from: usize,
    to: usize,
    start: usize,
    end: usize,
    skip_busy: &[(usize, usize)],
    allow_expand: bool,
) -> (usize, usize) {
    if !allow_expand || from >= to || !expand_partial_pages() {
        return (from, to);
    }
    const PAGE: usize = crate::watch::PAGE;
    let first_page = from / PAGE;
    let last_page = (to - 1) / PAGE;
    let first_lo = (first_page * PAGE).max(start);
    let first_hi = ((first_page + 1) * PAGE).min(end);
    let last_lo = (last_page * PAGE).max(start);
    let last_hi = ((last_page + 1) * PAGE).min(end);
    let can_first =
        from > first_lo && !skip_busy.iter().any(|&(a, e)| a < first_hi && e > first_lo);
    let can_last = to < last_hi && !skip_busy.iter().any(|&(a, e)| a < last_hi && e > last_lo);
    (
        if can_first { first_lo } else { from },
        if can_last { last_hi } else { to },
    )
}

pub(crate) fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_COPY_PROFILE").is_some_and(|v| v == "1"))
}

static FILL: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];
static STEP_COPY: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

static KIND_BYTES: [AtomicU64; ShadowKind::COUNT] =
    [const { AtomicU64::new(0) }; ShadowKind::COUNT];
static KIND_CALLS: [AtomicU64; ShadowKind::COUNT] =
    [const { AtomicU64::new(0) }; ShadowKind::COUNT];
static KIND_US: [AtomicU64; ShadowKind::COUNT] = [const { AtomicU64::new(0) }; ShadowKind::COUNT];

// Reason counters: [bytes, pages_or_calls]
// 0: first_ever, 1: mask_4k, 2: whole_view, 3: whole_other, 4: unwatched, 5: partial_unmarked
static REASON_BYTES: [AtomicU64; 6] = [const { AtomicU64::new(0) }; 6];
static REASON_COUNT: [AtomicU64; 6] = [const { AtomicU64::new(0) }; 6];

// Whole-page fallback sub-reasons (pages)
static WHOLE_WHY: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

// copy_changed: [read_bytes, changed_bytes]
static DIFF: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

#[derive(Default, Clone, Copy)]
pub(crate) struct UpdateCounts {
    pub(crate) first_bytes: u64,
    pub(crate) first_pages: u64,
    pub(crate) mask4k_bytes: u64,
    pub(crate) mask4k_pages: u64,
    pub(crate) whole_view_bytes: u64,
    pub(crate) whole_view_pages: u64,
    pub(crate) whole_other_bytes: u64,
    pub(crate) whole_other_pages: u64,
    pub(crate) unwatched_bytes: u64,
    pub(crate) partial_unmarked_bytes: u64,
    pub(crate) partial_unmarked_pages: u64,
}

#[derive(Clone, Copy)]
struct ShadowEntry {
    start: u64,
    len: u64,
    kind: ShadowKind,
    is_view: bool,
    calls: u64,
    bytes: u64,
    us: u64,
    first_bytes: u64,
    mask4k_bytes: u64,
    whole_view_bytes: u64,
    whole_other_bytes: u64,
    unwatched_bytes: u64,
    partial_unmarked_bytes: u64,
    diff_read: u64,
    diff_changed: u64,
    sum_ask: u64,
}

static SHADOWS: Mutex<Option<HashMap<u64, ShadowEntry>>> = Mutex::new(None);

pub(crate) fn note_fill(bytes: usize, us: u64) {
    if !enabled() || bytes == 0 {
        return;
    }
    FILL[0].fetch_add(1, Relaxed);
    FILL[1].fetch_add(bytes as u64, Relaxed);
    FILL[2].fetch_add(us, Relaxed);
}

pub(crate) fn note_step_copy(bytes: usize, us: u64) {
    if !enabled() || bytes == 0 {
        return;
    }
    STEP_COPY[0].fetch_add(1, Relaxed);
    STEP_COPY[1].fetch_add(bytes as u64, Relaxed);
    STEP_COPY[2].fetch_add(us, Relaxed);
}

pub(crate) fn note_whole_why(why: WholeWhy) {
    if enabled() {
        WHOLE_WHY[why as usize].fetch_add(1, Relaxed);
    }
}

thread_local! {
    static TL_DIFF: std::cell::Cell<(u64, u64)> = const { std::cell::Cell::new((0, 0)) };
}

pub(crate) fn begin_diff() {
    if enabled() {
        TL_DIFF.with(|c| c.set((0, 0)));
    }
}

pub(crate) fn note_diff(read: usize, changed: usize) {
    if !enabled() || read == 0 {
        return;
    }
    DIFF[0].fetch_add(read as u64, Relaxed);
    DIFF[1].fetch_add(changed as u64, Relaxed);
    TL_DIFF.with(|c| {
        let (r, ch) = c.get();
        c.set((r + read as u64, ch + changed as u64));
    });
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn note_shadow_update(
    id: u64,
    start: u64,
    len: u64,
    kind: ShadowKind,
    is_view: bool,
    ask_len: usize,
    copied: usize,
    us: u64,
    counts: UpdateCounts,
) {
    if !enabled() || copied == 0 {
        return;
    }
    let k = kind as usize;
    KIND_BYTES[k].fetch_add(copied as u64, Relaxed);
    KIND_CALLS[k].fetch_add(1, Relaxed);
    KIND_US[k].fetch_add(us, Relaxed);

    REASON_BYTES[0].fetch_add(counts.first_bytes, Relaxed);
    REASON_COUNT[0].fetch_add(counts.first_pages, Relaxed);
    REASON_BYTES[1].fetch_add(counts.mask4k_bytes, Relaxed);
    REASON_COUNT[1].fetch_add(counts.mask4k_pages, Relaxed);
    REASON_BYTES[2].fetch_add(counts.whole_view_bytes, Relaxed);
    REASON_COUNT[2].fetch_add(counts.whole_view_pages, Relaxed);
    REASON_BYTES[3].fetch_add(counts.whole_other_bytes, Relaxed);
    REASON_COUNT[3].fetch_add(counts.whole_other_pages, Relaxed);
    REASON_BYTES[4].fetch_add(counts.unwatched_bytes, Relaxed);
    if counts.unwatched_bytes > 0 {
        REASON_COUNT[4].fetch_add(1, Relaxed);
    }
    REASON_BYTES[5].fetch_add(counts.partial_unmarked_bytes, Relaxed);
    REASON_COUNT[5].fetch_add(counts.partial_unmarked_pages, Relaxed);

    let mut guard = SHADOWS.lock().unwrap_or_else(|e| e.into_inner());
    let map = guard.get_or_insert_with(HashMap::new);
    let e = map.entry(id).or_insert(ShadowEntry {
        start,
        len,
        kind,
        is_view,
        calls: 0,
        bytes: 0,
        us: 0,
        first_bytes: 0,
        mask4k_bytes: 0,
        whole_view_bytes: 0,
        whole_other_bytes: 0,
        unwatched_bytes: 0,
        partial_unmarked_bytes: 0,
        diff_read: 0,
        diff_changed: 0,
        sum_ask: 0,
    });
    let (diff_r, diff_c) = TL_DIFF.with(|c| c.replace((0, 0)));
    e.calls += 1;
    e.bytes += copied as u64;
    e.us += us;
    e.first_bytes += counts.first_bytes;
    e.mask4k_bytes += counts.mask4k_bytes;
    e.whole_view_bytes += counts.whole_view_bytes;
    e.whole_other_bytes += counts.whole_other_bytes;
    e.unwatched_bytes += counts.unwatched_bytes;
    e.partial_unmarked_bytes += counts.partial_unmarked_bytes;
    e.diff_read += diff_r;
    e.diff_changed += diff_c;
    e.sum_ask += ask_len as u64;
}

pub(crate) fn line(frames: f64) {
    if !enabled() || frames <= 0.0 {
        return;
    }
    let mb = |b: u64| b as f64 / 1e6 / frames;
    let ms = |u: u64| u as f64 / 1e3 / frames;
    let per = |n: u64| n as f64 / frames;

    let fill_n = FILL[0].swap(0, Relaxed);
    let fill_b = FILL[1].swap(0, Relaxed);
    let fill_u = FILL[2].swap(0, Relaxed);
    let sc_n = STEP_COPY[0].swap(0, Relaxed);
    let sc_b = STEP_COPY[1].swap(0, Relaxed);
    let sc_u = STEP_COPY[2].swap(0, Relaxed);

    let kinds = [
        ShadowKind::BufCard,
        ShadowKind::BufHost,
        ShadowKind::WinTransient,
        ShadowKind::WinCard,
        ShadowKind::WinRebar,
        ShadowKind::WinHost,
    ];
    let mut kind_parts = Vec::with_capacity(ShadowKind::COUNT);
    let mut shadow_total_b = 0u64;
    for k in kinds {
        let b = KIND_BYTES[k as usize].swap(0, Relaxed);
        let n = KIND_CALLS[k as usize].swap(0, Relaxed);
        let u = KIND_US[k as usize].swap(0, Relaxed);
        shadow_total_b += b;
        if b > 0 {
            kind_parts.push(format!(
                "{} {:.1} MB ({:.0}x, {:.2} ms)",
                k.label(),
                mb(b),
                per(n),
                ms(u)
            ));
        }
    }
    tracing::info!(
        "copy_profile/frame: total {:.1} MB (shadows {:.1} MB [{}]; fills {:.1} MB {:.0}x {:.2} ms; step_copy {:.1} MB {:.0}x {:.2} ms)",
        mb(shadow_total_b + fill_b + sc_b),
        mb(shadow_total_b),
        kind_parts.join(", "),
        mb(fill_b),
        per(fill_n),
        ms(fill_u),
        mb(sc_b),
        per(sc_n),
        ms(sc_u),
    );

    let rb = [0, 1, 2, 3, 4, 5].map(|i| REASON_BYTES[i].swap(0, Relaxed));
    let rc = [0, 1, 2, 3, 4, 5].map(|i| REASON_COUNT[i].swap(0, Relaxed));
    let ww = [0, 1, 2, 3].map(|i| WHOLE_WHY[i].swap(0, Relaxed));
    let diff_r = DIFF[0].swap(0, Relaxed);
    let diff_c = DIFF[1].swap(0, Relaxed);
    tracing::info!(
        "copy_profile reasons/frame: first_ever {:.1} MB ({:.0} pg), mask_4k {:.1} MB ({:.0} pg), \
         whole_64k_view {:.1} MB ({:.0} pg), whole_64k_other {:.1} MB ({:.0} pg), \
         unwatched {:.1} MB ({:.0}x), partial_unmarked {:.1} MB ({:.0} pg); \
         whole_why(pg): whole_stamp {:.0}, no_subs {:.0}, zero_mask {:.0}, no_watch4k {:.0}; \
         copy_changed: read {:.1} MB -> diff {:.1} MB",
        mb(rb[0]),
        per(rc[0]),
        mb(rb[1]),
        per(rc[1]),
        mb(rb[2]),
        per(rc[2]),
        mb(rb[3]),
        per(rc[3]),
        mb(rb[4]),
        per(rc[4]),
        mb(rb[5]),
        per(rc[5]),
        per(ww[0]),
        per(ww[1]),
        per(ww[2]),
        per(ww[3]),
        mb(diff_r),
        mb(diff_c),
    );

    let entries = SHADOWS.lock().unwrap_or_else(|e| e.into_inner()).take();
    let mut list: Vec<(u64, ShadowEntry)> = entries.into_iter().flatten().collect();
    list.sort_unstable_by_key(|&(_, e)| std::cmp::Reverse(e.bytes));
    for (id, e) in list.into_iter().take(15) {
        let avg_ask_kb = if e.calls > 0 {
            e.sum_ask as f64 / e.calls as f64 / 1024.0
        } else {
            0.0
        };
        tracing::info!(
            "  copy_profile top: id={id} {:#x}+{:#x} ({:.2} MB) {} view={} | {:.1} MB/f ({:.1}x/f, {:.2} ms/f, avg_ask {:.0} KB) \
             [first {:.1}, 4k {:.1}, w_view {:.1}, w_oth {:.1}, unw {:.1}, part_unm {:.1}; diff {:.1}/{:.1} MB]",
            e.start,
            e.len,
            e.len as f64 / (1 << 20) as f64,
            e.kind.label(),
            u8::from(e.is_view),
            mb(e.bytes),
            per(e.calls),
            ms(e.us),
            avg_ask_kb,
            mb(e.first_bytes),
            mb(e.mask4k_bytes),
            mb(e.whole_view_bytes),
            mb(e.whole_other_bytes),
            mb(e.unwatched_bytes),
            mb(e.partial_unmarked_bytes),
            mb(e.diff_changed),
            mb(e.diff_read),
        );
    }
}
