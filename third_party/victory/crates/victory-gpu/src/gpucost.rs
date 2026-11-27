//! Diagnostics (`VICTORY_GPU_COST=1`): where the card's time goes. A GPU
//! timestamp is written before each timed command of a host list (the
//! title's draws, dispatches, copies, clears and barriers, named by kind and
//! pipeline, so by shader hash) and before each piece of work of ours (draw
//! setup, indirect clamps, twin copies, the end-of-list copies), and at the
//! start and end of our own lists (preludes, the card copies' sync, compare
//! and fetch). The time from one stamp to the next goes to the first one's
//! label. A list's stamps are read when it is next reset (its run is over
//! then). Every five seconds the sums per frame are logged with the top 15
//! labels; "ours:" labels are the layer's own work.
//!
//! Timestamps are written at the end of the pipe: draws that overlap share
//! their time with their neighbours, so a label's time is approximate, and
//! the sums are the card's time in the timed lists.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;
use windows::core::Interface;

/// What a stretch of the card's time is summed under.
pub(crate) type Label = Cow<'static, str>;

/// Timestamps per list; marks past them are not timed (their time goes to
/// the last label).
const STAMPS: u32 = 8192;

/// Written over the read stamps: a list closed and reset without running
/// leaves it there, and its stamps are not counted.
const UNWRITTEN: u64 = u64::MAX;

/// Lines of the top table.
const TOP: usize = 15;

/// `VICTORY_GPU_COST`: 0 (or unset) off; 1 the coarse split (the title's
/// passes as one label, our own work by kind, the idle gaps between timed
/// lists), few stamps so the card runs as without them; 2 also every
/// title command by kind and shader hashes.
fn level() -> u8 {
    static LEVEL: OnceLock<u8> = OnceLock::new();
    *LEVEL.get_or_init(|| {
        std::env::var("VICTORY_GPU_COST")
            .ok()
            .and_then(|v| v.parse::<u8>().ok())
            .unwrap_or(0)
    })
}

/// Whether the breakdown is on.
pub(crate) fn on() -> bool {
    level() > 0
}

/// Whether title commands get their own labels (level 2).
pub(crate) fn fine() -> bool {
    level() > 1
}

/// The title's work in coarse mode, and the first label of a host list.
pub(crate) const TITLE: &str = "title passes";

/// Labels of our own work, for the hooks in `host.rs`.
pub(crate) const CARD_SYNC: &str = "ours: card sync copies (into the card's copies)";
pub(crate) const CARD_PATCH: &str = "ours: card CARD_PATCH";
pub(crate) const CARD_REHASH: &str = "ours: card rehash (CARD_COMPARE hashing of the before copies)";
pub(crate) const CARD_CHECK: &str = "ours: card check (CARD_COMPARE of shader-written ranges)";
pub(crate) const FETCH_RELAY: &str = "ours: copy-back fetch (relay)";
pub(crate) const FETCH_WAITED: &str = "ours: copy-back fetch (waited)";
pub(crate) const LIST_END: &str = "ours: list end (twins to depth, textures back, indirect args back)";
pub(crate) const TRANSITIONS: &str = "ours: texture state transitions (and the setup after)";
pub(crate) const TWIN_INLINE: &str = "ours: twin copy inline (and the setup after)";
pub(crate) const PRELUDE_TWINS: &str = "ours: prelude twin copies (and their transitions)";
pub(crate) const PRELUDE_UPLOADS: &str = "ours: prelude texture uploads";
pub(crate) const PRELUDE_AHEAD: &str = "ours: prelude indirect clamps ahead";

/// [`mark`] under a fixed label.
pub(crate) fn at(list: &ID3D12GraphicsCommandList, label: &'static str) {
    mark(list, || label);
}

/// A list's timestamp heap, where it is resolved, and the labels of the
/// stamps written since the list's last reset.
struct Stamps {
    heap: ID3D12QueryHeap,
    back: ID3D12Resource,
    labels: Vec<Label>,
    /// Resolved at the list's close since its last reset.
    closed: bool,
}

// SAFETY: free-threaded D3D12 objects behind a lock.
unsafe impl Send for Stamps {}

/// By host command list (its interface pointer).
static LISTS: Mutex<Option<HashMap<usize, Stamps>>> = Mutex::new(None);

/// Since the last line, by label: stretches, ticks, and the longest one.
static TOTALS: Mutex<Option<HashMap<Label, (u64, u64, u64)>>> = Mutex::new(None);

/// Since the last line: each counted list run's first and last stamp, and
/// its first label (what kind of list it is).
static INTERVALS: Mutex<Vec<(u64, u64, Label)>> = Mutex::new(Vec::new());

/// Since the last line: list runs not counted, by why and list kind.
static SKIPS: Mutex<Option<HashMap<String, u64>>> = Mutex::new(None);

/// Since the last line: runs of lists counted, and runs not counted (not
/// run, or stamps out of order).
static RUNS: [std::sync::atomic::AtomicU64; 2] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 2];

fn key(list: &ID3D12GraphicsCommandList) -> usize {
    list.as_raw() as usize
}

fn make() -> Option<Stamps> {
    let device = &crate::host::host()?.device;
    let mut heap: Option<ID3D12QueryHeap> = None;
    let mut back: Option<ID3D12Resource> = None;
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: u64::from(STAMPS) * 8,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        ..Default::default()
    };
    // SAFETY: plain object creation.
    unsafe {
        device
            .CreateQueryHeap(
                &D3D12_QUERY_HEAP_DESC {
                    Type: D3D12_QUERY_HEAP_TYPE_TIMESTAMP,
                    Count: STAMPS,
                    NodeMask: 0,
                },
                &mut heap,
            )
            .ok()?;
        device
            .CreateCommittedResource(
                &D3D12_HEAP_PROPERTIES {
                    Type: D3D12_HEAP_TYPE_READBACK,
                    ..Default::default()
                },
                D3D12_HEAP_FLAG_NONE,
                &desc,
                D3D12_RESOURCE_STATE_COPY_DEST,
                None,
                &mut back,
            )
            .ok()?;
    }
    let back = back?;
    fill_unwritten(&back, STAMPS as usize);
    Some(Stamps {
        heap: heap?,
        back,
        labels: Vec::new(),
        closed: false,
    })
}

/// Writes [`UNWRITTEN`] over the first `n` stamps of `back`.
fn fill_unwritten(back: &ID3D12Resource, n: usize) {
    let mut data = std::ptr::null_mut();
    // SAFETY: a read-back buffer of at least `n` stamps no list resolves
    // into now.
    unsafe {
        if back
            .Map(0, Some(&D3D12_RANGE { Begin: 0, End: 0 }), Some(&mut data))
            .is_ok()
        {
            std::slice::from_raw_parts_mut(data.cast::<u64>(), n).fill(UNWRITTEN);
            back.Unmap(
                0,
                Some(&D3D12_RANGE {
                    Begin: 0,
                    End: n * 8,
                }),
            );
        }
    }
}

/// The ticks of the card's timestamps per second.
fn frequency() -> u64 {
    static FREQUENCY: OnceLock<u64> = OnceLock::new();
    *FREQUENCY.get_or_init(|| {
        crate::host::host()
            // SAFETY: a plain query.
            .and_then(|h| unsafe { h.queue().GetTimestampFrequency() }.ok())
            .unwrap_or(1)
            .max(1)
    })
}

/// Adds the last run's stamps of `s` to [`TOTALS`] (its run is over).
fn collect(s: &mut Stamps) {
    let labels = std::mem::take(&mut s.labels);
    let closed = std::mem::replace(&mut s.closed, false);
    if labels.is_empty() || !closed {
        return;
    }
    let n = labels.len() + 1;
    let mut data = std::ptr::null_mut();
    // SAFETY: the list's resolve into the read-back buffer has finished.
    let stamps: Vec<u64> = unsafe {
        if s.back
            .Map(
                0,
                Some(&D3D12_RANGE {
                    Begin: 0,
                    End: n * 8,
                }),
                Some(&mut data),
            )
            .is_err()
        {
            return;
        }
        let read = std::slice::from_raw_parts(data.cast::<u64>(), n).to_vec();
        std::slice::from_raw_parts_mut(data.cast::<u64>(), n).fill(UNWRITTEN);
        s.back.Unmap(
            0,
            Some(&D3D12_RANGE {
                Begin: 0,
                End: n * 8,
            }),
        );
        read
    };
    let ordered = stamps.windows(2).all(|w| w[0] <= w[1]);
    let span = stamps[n - 1].wrapping_sub(stamps[0]);
    let why = if stamps.contains(&UNWRITTEN) {
        Some(if stamps.iter().all(|&t| t == UNWRITTEN) {
            "not run"
        } else {
            "partly written"
        })
    } else if !ordered {
        Some("out of order")
    } else if span > frequency() {
        Some("over a second")
    } else {
        None
    };
    if let Some(why) = why {
        RUNS[1].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let kind = labels[0].split(" (").next().unwrap_or(&labels[0]).to_owned();
        *SKIPS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_or_insert_with(HashMap::new)
            .entry(format!("{why}: {kind}"))
            .or_insert(0) += 1;
        return;
    }
    RUNS[0].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    INTERVALS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push((stamps[0], stamps[n - 1], labels[0].clone()));
    let mut totals = TOTALS.lock().unwrap_or_else(|e| e.into_inner());
    let totals = totals.get_or_insert_with(HashMap::new);
    for (label, w) in labels.into_iter().zip(stamps.windows(2)) {
        let ticks = w[1] - w[0];
        let e = totals.entry(label).or_insert((0, 0, 0));
        e.0 += 1;
        e.1 += ticks;
        e.2 = e.2.max(ticks);
    }
}

/// `list` was just reset: its last run's stamps are counted, and its first
/// stamp, under `label`, is written.
pub(crate) fn begin(list: &ID3D12GraphicsCommandList, label: impl Into<Label>) {
    if !on() {
        return;
    }
    let mut lists = LISTS.lock().unwrap_or_else(|e| e.into_inner());
    let lists = lists.get_or_insert_with(HashMap::new);
    let s = match lists.entry(key(list)) {
        std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
        std::collections::hash_map::Entry::Vacant(e) => match make() {
            Some(s) => e.insert(s),
            None => return,
        },
    };
    collect(s);
    // SAFETY: an open list and its own heap.
    unsafe { list.EndQuery(&s.heap, D3D12_QUERY_TYPE_TIMESTAMP, 0) };
    s.labels.push(label.into());
}

/// A stamp in `list` before what is about to be recorded, under the label
/// `label` makes (only if the list was begun and has room).
pub(crate) fn mark<L: Into<Label>>(list: &ID3D12GraphicsCommandList, label: impl FnOnce() -> L) {
    if !on() {
        return;
    }
    let mut lists = LISTS.lock().unwrap_or_else(|e| e.into_inner());
    let Some(s) = lists.as_mut().and_then(|l| l.get_mut(&key(list))) else {
        return;
    };
    let at = s.labels.len() as u32;
    if at == 0 || s.closed || at >= STAMPS - 1 {
        return;
    }
    let mut label = label().into();
    if !fine() && !label.starts_with("ours") {
        label = Cow::Borrowed(TITLE);
    }
    if s.labels.last() == Some(&label) {
        return;
    }
    // SAFETY: an open list and its own heap.
    unsafe { list.EndQuery(&s.heap, D3D12_QUERY_TYPE_TIMESTAMP, at) };
    s.labels.push(label);
}

/// `list` is about to close: its last stamp, and the resolve of them all.
pub(crate) fn end(list: &ID3D12GraphicsCommandList) {
    if !on() {
        return;
    }
    let mut lists = LISTS.lock().unwrap_or_else(|e| e.into_inner());
    let Some(s) = lists.as_mut().and_then(|l| l.get_mut(&key(list))) else {
        return;
    };
    let at = s.labels.len() as u32;
    if at == 0 || s.closed {
        return;
    }
    // SAFETY: an open list, its own heap and read-back buffer.
    unsafe {
        list.EndQuery(&s.heap, D3D12_QUERY_TYPE_TIMESTAMP, at);
        list.ResolveQueryData(&s.heap, D3D12_QUERY_TYPE_TIMESTAMP, 0, at + 1, &s.back, 0);
    }
    s.closed = true;
}

/// The label of a title command: its kind and pipeline (its shaders'
/// hashes).
pub(crate) fn title(kind: &str, pipeline: &str) -> Label {
    Cow::Owned(format!("{kind} {pipeline}"))
}

/// Logs the sums per frame since the last line, `frames` frames.
pub(crate) fn log(frames: f64, frame_ms: f64) {
    if !on() {
        return;
    }
    let frames = frames.max(1.0);
    let totals = TOTALS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
        .unwrap_or_default();
    let [counted, skipped] = [0, 1].map(|i| RUNS[i].swap(0, std::sync::atomic::Ordering::Relaxed));
    let ms = |ticks: u64| ticks as f64 * 1e3 / frequency() as f64 / frames;
    let mut rows: Vec<(Label, (u64, u64, u64))> = totals.into_iter().collect();
    let all: u64 = rows.iter().map(|r| r.1.1).sum();
    let (gaps, long, span, busy) = gaps();
    // The gaps are of the runs up to a second before the last one seen (a
    // list's stamps are read at its next reset): per frame of that span.
    let per_span = |ticks: u64| {
        let f = span as f64 * 1e3 / frequency() as f64 / frame_ms.max(1.0);
        ticks as f64 * frames / f.max(1.0)
    };
    let gaps: Vec<(Label, (u64, u64, u64))> = gaps
        .into_iter()
        .map(|(l, (n, t, m))| (l, (n, per_span(t) as u64, m)))
        .collect();
    let idle: u64 = gaps.iter().map(|g| g.1.1).sum();
    let busy = per_span(busy) as u64;
    let mut skips: Vec<(String, u64)> = SKIPS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
        .unwrap_or_default()
        .into_iter()
        .collect();
    skips.sort_by_key(|s| std::cmp::Reverse(s.1));
    tracing::info!(
        "gpu cost: card busy in timed lists {:.2} ms/frame (their union), idle between them {:.2} ms; \
         list runs not counted per frame: {}",
        ms(busy),
        ms(idle),
        skips
            .iter()
            .take(8)
            .map(|(k, n)| format!("{k} {:.1}", *n as f64 / frames))
            .collect::<Vec<_>>()
            .join(", ")
    );
    rows.extend(gaps);
    rows.sort_by_key(|r| std::cmp::Reverse(r.1.1));
    let ours: u64 = rows
        .iter()
        .filter(|r| r.0.starts_with("ours:"))
        .map(|r| r.1.1)
        .sum();
    // Ours by kind (the label up to its first parenthesis).
    let mut kinds: Vec<(String, u64)> = Vec::new();
    for (label, (_, ticks, _)) in rows.iter().filter(|r| r.0.starts_with("ours:")) {
        let kind = label.split(" (").next().unwrap_or(label).to_owned();
        match kinds.iter_mut().find(|k| k.0 == kind) {
            Some(k) => k.1 += ticks,
            None => kinds.push((kind, *ticks)),
        }
    }
    kinds.sort_by_key(|k| std::cmp::Reverse(k.1));
    // The title's by command kind (level 2: the label's first word, or two
    // for "title ..." labels).
    let mut title: Vec<(String, u64, u64)> = Vec::new();
    for (label, (n, ticks, _)) in rows
        .iter()
        .filter(|r| !r.0.starts_with("ours") && !r.0.starts_with("idle"))
    {
        let words = if label.starts_with("title") { 2 } else { 1 };
        let kind = label.split(' ').take(words).collect::<Vec<_>>().join(" ");
        match title.iter_mut().find(|k| k.0 == kind) {
            Some(k) => {
                k.1 += ticks;
                k.2 += n;
            }
            None => title.push((kind, *ticks, *n)),
        }
    }
    title.sort_by_key(|k| std::cmp::Reverse(k.1));
    tracing::info!(
        "gpu cost, the title's by kind per frame: {}",
        title
            .iter()
            .map(|(k, t, n)| format!("{k} {:.2} ms in {:.0}", ms(*t), *n as f64 / frames))
            .collect::<Vec<_>>()
            .join(", ")
    );
    tracing::info!(
        "gpu cost per frame (VICTORY_GPU_COST): {:.2} ms timed, ours {:.2} ms, the title's {:.2} ms, \
         idle between timed lists {:.2} ms ({:.1} gaps over 100 ms a frame left out); \
         {:.1} list runs a frame counted, {:.1} not ({} labels); ours by kind: {}",
        ms(all),
        ms(ours),
        ms(all - ours),
        ms(idle),
        long as f64 / frames,
        counted as f64 / frames,
        skipped as f64 / frames,
        rows.len(),
        kinds
            .iter()
            .map(|(k, t)| format!("{k} {:.2} ms", ms(*t)))
            .collect::<Vec<_>>()
            .join(", ")
    );
    for (i, (label, (n, ticks, most))) in rows.iter().take(TOP).enumerate() {
        tracing::info!(
            "gpu cost top {:>2}: {:6.2} ms/frame {:5.1}% in {:7.1}/frame (longest {:.2} ms): {}",
            i + 1,
            ms(*ticks),
            if all + idle > 0 {
                100.0 * *ticks as f64 / (all + idle) as f64
            } else {
                0.0
            },
            *n as f64 / frames,
            ms(*most) * frames,
            label.chars().take(300).collect::<String>()
        );
    }
}

/// The card's time between the timed list runs since the last line, by the
/// kinds of the lists before and after (stretches, ticks, longest), and how
/// many gaps of over 100 ms were left out (lists counted late, or the title
/// not submitting).
fn gaps() -> (HashMap<Label, (u64, u64, u64)>, u64, u64, u64) {
    let mut runs = {
        let mut all = INTERVALS.lock().unwrap_or_else(|e| e.into_inner());
        let Some(cut) = all.iter().map(|r| r.1).max().map(|m| m.saturating_sub(frequency()))
        else {
            return (HashMap::new(), 0, 0, 0);
        };
        let (old, keep): (Vec<_>, Vec<_>) = all.drain(..).partition(|r| r.1 <= cut);
        *all = keep;
        old
    };
    runs.sort_by_key(|r| r.0);
    let span = match (runs.first(), runs.iter().map(|r| r.1).max()) {
        (Some(f), Some(l)) => l - f.0,
        _ => 0,
    };
    let mut busy = 0u64;
    let most = frequency() / 10;
    let kind = |l: &Label| l.split(" (").next().unwrap_or(l).to_owned();
    let mut out: HashMap<Label, (u64, u64, u64)> = HashMap::new();
    let mut long = 0;
    let mut end = None::<(u64, &Label)>;
    for (from, to, label) in &runs {
        if let Some((last, before)) = end {
            if *from > last {
                let gap = from - last;
                if gap > most {
                    long += 1;
                } else {
                    let key = Cow::Owned(format!(
                        "idle: after {} before {}",
                        kind(before),
                        kind(label)
                    ));
                    let e = out.entry(key).or_insert((0, 0, 0));
                    e.0 += 1;
                    e.1 += gap;
                    e.2 = e.2.max(gap);
                }
            }
        }
        let start = end.map_or(*from, |(last, _)| (*from).max(last));
        busy += to.saturating_sub(start);
        if end.is_none_or(|(last, _)| *to > last) {
            end = Some((*to, label));
        }
    }
    (out, long, span, busy)
}
