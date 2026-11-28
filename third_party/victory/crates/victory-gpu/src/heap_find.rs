//! (phonescreen, #115) `VICTORY_HEAP_FIND=<address>` (live, off by
//! default; hex with or without `0x`): names every view the title's
//! shader-visible heaps hold whose memory holds that address, and every
//! draw that binds one, so the draw that samples a texture the host
//! renders (the phone's video target at 0xe3d400000 in batch 5 part E) and
//! the view it samples it through are known even when no draw names that
//! memory as its own texture.
//!
//! One frame every `VICTORY_HEAP_FIND_EVERY_MS` (default 1000) inside
//! `VICTORY_HEAP_FIND_WINDOW=<from>-<to>` (seconds as the snaps count them;
//! default the whole run), at most [`MOST_FRAMES`] frames:
//! - the heaps are walked: `heap find: scan` (frame, seconds, slots found),
//!   then `heap find: slot` for each slot found for the first time or
//!   holding another record since the last walk: heap and index, the view
//!   (format, dimension, levels; for a view of placed memory the resource
//!   description too), where its memory starts and ends, the texture made
//!   for it (if one is) and what a draw reading it is copied from
//!   (`link`: the texture the host writes it is linked to, or `none`:
//!   filled from title memory), the texture the host writes whose memory
//!   holds the address (`target`), and the frames of the walks that first
//!   saw the slot and saw its record change (a write between two walks
//!   shows at the later one);
//! - every draw and dispatch of the frame that renders to, writes or reads
//!   through its tables memory holding the address logs `heap find: draw`
//!   (programs, the bind's place in the frame, each binding that holds it
//!   with its heap slot). Draws that pick views by index are counted in
//!   the scan line of the next frame walked (they cannot be resolved).
//!
//! (round 6) The video target moves every run (0xe3d400000 in part E,
//! 0xb9d000000 in part G), so `VICTORY_HEAP_FIND=bink` finds it itself: the
//! render target 0 of every draw of the title's Bink pixel program
//! ([`BINK_PIXEL`], or a program named `*Bink*`) narrower than 1920 (the
//! phone's 736x1280 video, not the whole-screen ones) is followed, up to
//! [`MOST_FOLLOWED`] targets, each over its whole memory, not one address:
//! - `heap find: follow` when a target is first followed and `heap find:
//!   write` at each later Bink draw into it (frame, seconds, the target);
//!   the frames right after a write are walked whatever the 1 s rate;
//! - the walk also reads the slots holding no view of ours (descriptors the
//!   title wrote itself, raw) as image and buffer descriptors and logs
//!   `heap find: raw slot` for those whose address lies in a followed
//!   target;
//! - a draw that picks views by index from a heap that holds a slot found
//!   logs `heap find: draw ... by_index` with those slots (it may read
//!   them), and root views into the memory are named as `root<slot>`;
//! - copies, resolves and memory copies from or into the memory log `heap
//!   find: copy` in any frame (at most [`MOST_COPY_LINES`]).
//!
//! (round 7) Translated shaders read every view through the heap: the slot
//! is (table's byte offset + the load's offset + a register offset) / 32,
//! so a draw may read a slot past its tables' first views, or one picked
//! by a number in its constants, which the lines above do not see. The scan
//! line now says what each found slot's host slot shows (`host_slots`:
//! `made` = the found view, `null`, `other_view`), and
//! `VICTORY_HEAP_FIND_REFS=1` (live) also looks, for each bind of a walked
//! frame, at:
//! - its tables: `heap find: draw ... table<n> at slot <t> reaches found
//!   slot <s> at +<s - t>` when a found slot lies less than [`NEAR`] slots
//!   past a table's first;
//! - its root constants and the first [`MOST_SCAN`] bytes of every
//!   constant buffer, root view and buffer view it binds: `heap find: draw
//!   ... ref <where> dword <n> = <value> (<what>)` when a word holds a
//!   found slot as an index or a byte offset in its heap, from the heap's
//!   start or from one of the draw's tables, or the slot's handle (low
//!   half). Each buffer is read once a frame per table set, at most
//!   256 MiB a frame (`refs_bytes`, `refs_full` on the scan line; round 8
//!   below).
//!
//! (round 8) Part K: no reader in the frames of the table shot, with the
//! search whole there (`refs_full=false` at frames 866-980), so the reader
//! names the view some other way, or reads a copy of it. Now:
//! - `VICTORY_HEAP_FIND_REFS=1` searches only the draws that render to the
//!   scene colour or a UI target ([`scene_or_ui`]: rt0 at least 1280 wide,
//!   or at least 512 tall and not wider than tall, and not the followed
//!   target itself) in the
//!   [`REFS_AFTER`] frames after a Bink write; `=2` searches every bind as
//!   before. Small buffers are read whole and large ones up to 1 MiB, from
//!   budgets of their own, and buffers that hash as before are not
//!   searched again ([`refs`]); the scan line says what each walked frame
//!   searched (`refs_searched_small`/`_large`, `refs_unchanged`,
//!   `refs_buffers`, `refs_binds`, `refs_full`).
//! - Draws and dispatches not run (no host pipeline: part K's frame line
//!   says about 1400 draws a frame, which never reach the bind's walk) are
//!   searched too, by their tables and root constants and root views:
//!   `heap find: draw ... dropped draw|dispatch pipeline=<title pipeline>
//!   [<stages and names>] made=<host pipeline now>`.
//! - (`bink`) every view heap, shader-visible or not, is walked for slots
//!   that name the found view another way
//!   (`crate::descriptor::copies`): its very record at another index
//!   (`same_record`), a view of the memory in a heap the card does not see
//!   (`same_memory`), a record of a view freed with its resource whose
//!   memory was the target's (`stale`: reads as no view, its host slot is
//!   null), a record copied off a slot's start (`shifted`), a raw
//!   descriptor in a heap the card does not see (`raw`): `heap find: copy
//!   slot` lines, and `copies`, `stale_visible`, `view_heaps` on the scan
//!   line.
//!
//! (round 9) Part N: `refs_binds` 424-2354 on the walked frames 859-901 of
//! the Bink window and 0 past it, as meant, but the line showed the frame
//! before (now `refs_of_frame`, and `refs_window` says whether this frame
//! is in the window); the large budget ran out on every frame (fixed in
//! [`refs`]: a buffer is read once a frame, not once per table set). Now:
//! - `VICTORY_HEAP_FIND_REFS=1` searches every draw in the window whose
//!   rt0 is not the followed target and not tiny ([`searched_target`]:
//!   both sides from [`TINY`]; the scene at 2560x1440 under the upscaler,
//!   a UI at 1024x512, a phone's 736x1280 all pass) and every dispatch;
//!   the binds left out are counted (`refs_passed_over`), and the draws
//!   and dispatches not run searched (`refs_dropped`; part N's window
//!   dropped only ray-tracing builder and ray-only indirect dispatches,
//!   which are left out before a bind, so 0 `dropped` lines was right);
//! - `stale_visible` counts every record of a freed view in a
//!   shader-visible heap; a `copy slot ... kind=stale` line needs the
//!   freed view's memory to meet the target. The scan line adds
//!   `stale_former_known` and `stale_target`, and the first
//!   [`MOST_STALE_LINES`] such records a run that show other memory log
//!   `heap find: stale slot` with that memory.

mod refs;

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use victory_core::settings::Live;

use crate::descriptor::View;
use crate::object::This;

static FIND: Live = Live::new("VICTORY_HEAP_FIND", |v| v.and_then(parse_find).unwrap_or(0));
static WINDOW: Live = Live::new("VICTORY_HEAP_FIND_WINDOW", |v| {
    crate::draw_dump::window(v).map_or(0, |(from, to)| u64::from(from) << 32 | u64::from(to))
});
static EVERY_MS: Live = Live::new("VICTORY_HEAP_FIND_EVERY_MS", |v| {
    v.and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(1000)
});
static REFS: Live = Live::new("VICTORY_HEAP_FIND_REFS", |v| {
    match v.map(str::trim) {
        Some("1") => 1,
        Some("2") => 2,
        _ => 0,
    }
});

/// Frames walked in a run at most.
const MOST_FRAMES: u32 = 120;
/// [`FIND`]'s value for `bink`: follow the video targets (no address is 1).
const FOLLOW: u64 = 1;
/// The title's Bink YUV-to-RGB pixel program (`PS_BinkNoAlpha`).
const BINK_PIXEL: &str = "3bfe730f7e24ee1a";
/// Targets followed at once at most (the newest kept).
const MOST_FOLLOWED: usize = 4;
/// Frames walked after each Bink write, whatever the rate.
const AFTER_WRITE: u32 = 3;
/// Copy lines of a run at most.
const MOST_COPY_LINES: u32 = 300;
/// Write lines of a run at most.
const MOST_WRITE_LINES: u32 = 200;
/// Slots a walk keeps at most.
const MOST_SLOTS: usize = 256;
/// Draw lines of one frame at most.
const MOST_DRAW_LINES: u32 = 400;
/// Slots past a table's first named as within its reach.
pub(crate) const NEAR: u32 = 1024;
/// Bytes of a view slot as translated shaders count them (slot = byte
/// offset >> 5).
const SLOT_BYTES: u64 = 32;
/// Bytes of a root view looked at, from its address (its size is not
/// known; buffer views are read by their own size: [`refs::taken`]).
pub(crate) const MOST_SCAN: u64 = 16 << 10;
/// Frames after a Bink write whose scene and UI draws are searched.
const REFS_AFTER: u64 = 30;
/// A render target 0 narrower or shorter than this is tiny (a reduction,
/// a lookup table): its draws are not searched.
const TINY: u64 = 128;
/// Values below this are too common in constants to be told apart.
pub(crate) const SMALLEST: u32 = 256;
/// `heap find: stale slot` lines of a run at most.
const MOST_STALE_LINES: u32 = 64;
/// Found slots named on the scan line.
const MOST_HOST_SLOTS: usize = 8;
/// A view starting further than this before the address is not looked at
/// closer (its size is not worked out): no texture is so big.
const FARTHEST: u64 = 1 << 32;

/// `"0xe3d400000"` or `"e3d400000"`; `None` for 0 or not hex.
fn address(value: &str) -> Option<u64> {
    let v = value.trim();
    let v = v
        .strip_prefix("0x")
        .or_else(|| v.strip_prefix("0X"))
        .unwrap_or(v);
    u64::from_str_radix(v, 16).ok().filter(|&a| a != 0)
}

fn parse_find(value: &str) -> Option<u64> {
    if value.trim().eq_ignore_ascii_case("bink") {
        return Some(FOLLOW);
    }
    address(value)
}

/// Whether the switch is on.
pub(crate) fn on() -> bool {
    FIND.get() != 0
}

/// Whether binds of walked frames are searched for the found slots
/// (`VICTORY_HEAP_FIND_REFS=1`).
pub(crate) fn refs() -> bool {
    REFS.get() != 0
}

/// Whether every bind is searched (`VICTORY_HEAP_FIND_REFS=2`), not only
/// the draws and dispatches after a Bink write.
pub(crate) fn refs_all() -> bool {
    REFS.get() == 2
}

/// Whether a draw whose render target 0 is `width` x `height` is searched
/// (round 9): any not [`TINY`] (the scene at any internal size, a UI of any
/// shape); the followed target itself is left out by its memory.
pub(crate) fn searched_target(width: u64, height: u64) -> bool {
    width >= TINY && height >= TINY
}

/// Whether this frame is one of the [`REFS_AFTER`] after a Bink write
/// (always, when an address is looked for).
pub(crate) fn after_write() -> bool {
    if !follows() {
        return true;
    }
    let frame = crate::frame::current();
    FOLLOWED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .any(|f| frame.saturating_sub(f.last) <= REFS_AFTER)
}

/// Whether the video targets are followed (`bink`).
pub(crate) fn follows() -> bool {
    FIND.get() == FOLLOW
}

/// A target followed: its memory, description, the frames of its first and
/// last Bink write.
struct Followed {
    start: u64,
    len: u64,
    text: String,
    first: u64,
    last: u64,
    writes: u32,
}

static FOLLOWED: Mutex<Vec<Followed>> = Mutex::new(Vec::new());
/// Frames still to walk whatever the rate (set at a Bink write).
static FORCED: AtomicU32 = AtomicU32::new(0);
static COPY_LINES: AtomicU32 = AtomicU32::new(0);
static STALE_LINES: AtomicU32 = AtomicU32::new(0);
/// The stale records logged (heap, index, record), each once a run.
static STALE_SEEN: Mutex<Option<HashSet<(u64, u32, [u32; 3])>>> = Mutex::new(None);
static WRITE_LINES: AtomicU32 = AtomicU32::new(0);

/// The memory looked for: half-open ranges (one byte for an address).
pub(crate) fn spans() -> Vec<(u64, u64)> {
    match FIND.get() {
        0 => Vec::new(),
        FOLLOW => FOLLOWED
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|f| (f.start, f.start + f.len.max(1)))
            .collect(),
        a => vec![(a, a + 1)],
    }
}

/// Whether pixel program `hash` named `name` is a Bink pass.
pub(crate) fn may_be_bink(hash: &str, name: &str) -> bool {
    hash == BINK_PIXEL || name.contains("Bink")
}

/// Whether a draw of pixel program `hash` named `name` writing a target of
/// `width` is a phone-sized Bink pass.
fn is_bink(hash: &str, name: &str, width: u64) -> bool {
    may_be_bink(hash, name) && width < 1920
}

/// A draw of a Bink program into render target 0 (`start`, `len` bytes,
/// `width`, `text` its description): the target is followed and the frames
/// after it walked.
pub(crate) fn bink_draw(hash: &str, name: &str, start: u64, len: u64, width: u64, text: String) {
    if !is_bink(hash, name, width) {
        return;
    }
    let frame = crate::frame::current();
    let at = crate::snap::seconds().unwrap_or(0.0);
    let mut followed = FOLLOWED.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(f) = followed.iter_mut().find(|f| f.start == start && f.len == len) {
        if f.last == frame {
            return;
        }
        f.last = frame;
        f.writes += 1;
        if WRITE_LINES.fetch_add(1, Ordering::Relaxed) < MOST_WRITE_LINES {
            tracing::warn!(
                frame,
                at_s = format_args!("{at:.2}"),
                writes = f.writes,
                first_frame = f.first,
                ps = format_args!("{hash} ({name})"),
                "heap find: write rt0 [{}]",
                f.text
            );
        }
    } else {
        tracing::warn!(
            frame,
            at_s = format_args!("{at:.2}"),
            ps = format_args!("{hash} ({name})"),
            "heap find: follow rt0 [{text}]"
        );
        followed.insert(
            0,
            Followed {
                start,
                len,
                text,
                first: frame,
                last: frame,
                writes: 1,
            },
        );
        followed.truncate(MOST_FOLLOWED);
    }
    FORCED.store(AFTER_WRITE, Ordering::Relaxed);
}

/// The frame being walked (`u64::MAX`: none).
static FINDING: AtomicU64 = AtomicU64::new(u64::MAX);
static DECIDED: AtomicU64 = AtomicU64::new(u64::MAX);
static FRAMES: AtomicU32 = AtomicU32::new(0);
static BINDS: AtomicU32 = AtomicU32::new(0);
static LINES: AtomicU32 = AtomicU32::new(0);
/// Binds of walked frames that pick views by index (not resolved).
static BY_INDEX: AtomicU32 = AtomicU32::new(0);
static LAST_AT: Mutex<Option<f64>> = Mutex::new(None);

/// Whether this frame's binds are looked at; the frame's first bind
/// decides, and walks the heaps when it is.
pub(crate) fn recording() -> bool {
    let find = FIND.get();
    if find == 0 {
        return false;
    }
    let frame = crate::frame::current();
    if DECIDED.load(Ordering::Relaxed) != frame && DECIDED.swap(frame, Ordering::Relaxed) != frame {
        decide(frame, find);
    }
    FINDING.load(Ordering::Relaxed) == frame
}

fn decide(frame: u64, _find: u64) {
    let Some(at) = crate::snap::seconds() else {
        return;
    };
    let spans = spans();
    if spans.is_empty() {
        return;
    }
    let bits = WINDOW.get();
    let (from, to) = if bits == 0 {
        (0.0, f64::INFINITY)
    } else {
        ((bits >> 32) as f64, (bits & 0xffff_ffff) as f64)
    };
    {
        let mut last = LAST_AT.lock().unwrap_or_else(|e| e.into_inner());
        let forced = FORCED.load(Ordering::Relaxed) > 0 && at >= from && at < to;
        if forced {
            FORCED.fetch_sub(1, Ordering::Relaxed);
        }
        if !(forced || crate::draw_dump::due(at, from, to, *last, EVERY_MS.get()))
            || FRAMES.load(Ordering::Relaxed) >= MOST_FRAMES
        {
            return;
        }
        *last = Some(at);
    }
    FRAMES.fetch_add(1, Ordering::Relaxed);
    BINDS.store(0, Ordering::Relaxed);
    LINES.store(0, Ordering::Relaxed);
    FINDING.store(frame, Ordering::Relaxed);
    scan(frame, at, &spans);
}

/// Whether `[start, start + len)` meets one of `spans`.
pub(crate) fn meets(spans: &[(u64, u64)], start: u64, len: u64) -> bool {
    spans
        .iter()
        .any(|&(lo, hi)| start < hi && lo < start.saturating_add(len.max(1)))
}

/// Where the memory of `view` starts and how many bytes it takes, if it
/// meets `[lo, hi)`.
pub(crate) fn covers(view: &View, lo: u64, hi: u64) -> Option<(u64, u64)> {
    let near = |start: u64| start < hi && lo.saturating_sub(start) < FARTHEST;
    let (start, len) = match view {
        View::Placed(_, resource, desc) => {
            let start = u64::from(desc[10]) | u64::from(desc[11]) << 32;
            if !near(start) {
                return None;
            }
            (start, crate::resource::find_facts::words_bytes(resource)?)
        }
        View::Shader(r, _) | View::Unordered(r, _, _) => {
            let (start, len) = crate::resource::find_facts::object_span(*r as This)?;
            if !near(start) {
                return None;
            }
            (start, len)
        }
        _ => return None,
    };
    (lo < start.saturating_add(len.max(1))).then_some((start, len))
}

/// The first of `spans` the memory of `view` meets: (lo, hi, view start,
/// view bytes).
pub(crate) fn covers_any(view: &View, spans: &[(u64, u64)]) -> Option<(u64, u64, u64, u64)> {
    spans
        .iter()
        .find_map(|&(lo, hi)| covers(view, lo, hi).map(|(s, l)| (lo, hi, s, l)))
}

/// A view and what it shows, for a line.
pub(crate) fn view_text(view: &View, spans: &[(u64, u64)]) -> String {
    use crate::resource::find_facts as f;
    let span = covers_any(view, spans).map_or_else(String::new, |(lo, _, start, len)| {
        let at = if lo >= start {
            format!("found at +{:#x}", lo - start)
        } else {
            format!("found starts {:#x} in", start - lo)
        };
        format!("memory {start:#x}..{:#x} ({at})", start + len)
    });
    let made = match view {
        View::Placed(false, resource, desc) if resource[0] != 1 => {
            crate::resource::placed_known(
                u64::from(desc[10]) | u64::from(desc[11]) << 32,
                resource,
            )
        }
        View::Shader(r, _) => Some(*r as This),
        _ => None,
    };
    let resource = match view {
        View::Placed(_, resource, _) => f::words_text(resource),
        View::Shader(r, _) | View::Unordered(r, _, _) => f::object_text(*r as This),
        _ => String::new(),
    };
    let made_text = made.map_or_else(|| "none".into(), f::object_text);
    let link = made.map_or_else(|| "none (no texture made yet)".into(), f::link_text);
    format!(
        "view [{}] {span} resource [{resource}] texture [{made_text}] link [{link}]",
        crate::descriptor::host::view_ages::text(Some(view))
    )
}

/// What the walks saw of a slot: its record, the frame of the walk that
/// first saw it and of the one that saw its record change.
struct Seen {
    record: [u32; 3],
    first: u64,
    changed: u64,
}

static SEEN: Mutex<Option<HashMap<(u64, u32), Seen>>> = Mutex::new(None);

/// The slots of the last walk, by heap start: what a draw picking views by
/// index from that heap may read.
static SLOTS_NOW: Mutex<Vec<(u64, u32)>> = Mutex::new(Vec::new());

/// The slots of the last walk in the heap starting at `heap`.
pub(crate) fn slots_in(heap: u64) -> Vec<u32> {
    SLOTS_NOW
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .filter(|s| s.0 == heap)
        .map(|s| s.1)
        .collect()
}

/// The addresses a raw descriptor's first words name: as an image
/// descriptor (address >> 8 in word 0, bits 40-47 in word 1's low byte)
/// and as a buffer descriptor (address bits 0-31 in word 0, 32-47 in word
/// 1's low half).
fn raw_addresses(words: &[u32; 8]) -> [u64; 2] {
    let (w0, w1) = (u64::from(words[0]), u64::from(words[1]));
    [((w1 & 0xff) << 32 | w0) << 8, (w1 & 0xffff) << 32 | w0]
}

/// Which reading of a raw descriptor lies in `spans`, if one does.
fn raw_hit(words: &[u32; 8], spans: &[(u64, u64)]) -> Option<&'static str> {
    let [image, buffer] = raw_addresses(words);
    let inside = |a: u64| a != 0 && spans.iter().any(|&(lo, hi)| lo <= a && a < hi);
    if inside(image) {
        Some("image")
    } else if inside(buffer) {
        Some("buffer")
    } else {
        None
    }
}

fn scan(frame: u64, at: f64, spans: &[(u64, u64)]) {
    let started = std::time::Instant::now();
    let found =
        crate::descriptor::shader_views_where(|v| covers_any(v, spans).is_some(), MOST_SLOTS);
    let raw = if follows() {
        crate::descriptor::raw_slots_where(|w| raw_hit(w, spans).is_some(), MOST_SLOTS)
    } else {
        Vec::new()
    };
    *SLOTS_NOW.lock().unwrap_or_else(|e| e.into_inner()) = found
        .iter()
        .map(|f| (f.0, f.1))
        .chain(raw.iter().map(|r| (r.0, r.1)))
        .collect();
    let by_index = BY_INDEX.swap(0, Ordering::Relaxed);
    let find = spans.first().map_or(0, |s| s.0);
    let host_slots: Vec<String> = found
        .iter()
        .take(MOST_HOST_SLOTS)
        .map(|f| {
            format!(
                "{:#x}[{}]:{}",
                f.0,
                f.1,
                crate::descriptor::host_slot_state(f.0, f.1, f.2)
            )
        })
        .collect();
    let spent = RefsState::with(|r| r.start(frame));
    let window = after_write();
    let (copies, stale, walked) = if follows() {
        let records: Vec<(u64, u32, [u32; 3])> = found.iter().map(|f| (f.0, f.1, f.2)).collect();
        crate::descriptor::copies::copies_where(
            &records,
            |v| covers_any(v, spans).is_some(),
            |start, len| meets(spans, start, len),
            |w| raw_hit(w, spans).is_some(),
            MOST_SLOTS,
        )
    } else {
        Default::default()
    };
    tracing::warn!(
        frame,
        at_s = format_args!("{at:.2}"),
        find = format_args!("{find:#x}"),
        followed = spans.len(),
        slots = found.len(),
        raw_slots = raw.len(),
        target = %crate::resource::find_facts::target_text(find),
        binds_by_index_of_the_last_walked_frame = by_index,
        host_slots = format_args!("[{}]", host_slots.join(", ")),
        refs_of_frame = format_args!("{}", spent.frame.map_or_else(|| "none".into(), |f| f.to_string())),
        refs_window = window,
        refs_searched_small = spent.searched[0],
        refs_searched_large = spent.searched[1],
        refs_unchanged = spent.unchanged,
        refs_buffers = spent.buffers,
        refs_binds = spent.binds,
        refs_passed_over = spent.passed_over,
        refs_dropped = spent.dropped,
        refs_full = spent.full,
        copies = copies.len(),
        stale_visible = walked.stale_visible,
        stale_former_known = walked.stale_former_known,
        stale_target = walked.stale_target,
        view_heaps = format_args!("{}/{}_visible", walked.heaps, walked.visible_heaps),
        walk_ms = format_args!("{:.1}", started.elapsed().as_secs_f64() * 1000.0),
        "heap find: scan"
    );
    let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    let seen = seen.get_or_insert_with(HashMap::new);
    for (heap, index, record, view) in found {
        let mut new = false;
        let s = seen.entry((heap, index)).or_insert_with(|| {
            new = true;
            Seen {
                record,
                first: frame,
                changed: frame,
            }
        });
        if !new && s.record == record {
            continue;
        }
        s.record = record;
        s.changed = frame;
        tracing::warn!(
            frame,
            heap = format_args!("{heap:#x}"),
            index,
            first_seen_frame = s.first,
            changed_frame = s.changed,
            host = crate::descriptor::host_slot_state(heap, index, record),
            "heap find: slot {}",
            view_text(&view, spans)
        );
    }
    for (heap, index, words) in raw {
        let record = [words[0], words[1], words[2]];
        let mut new = false;
        let s = seen.entry((heap, index)).or_insert_with(|| {
            new = true;
            Seen {
                record,
                first: frame,
                changed: frame,
            }
        });
        if !new && s.record == record {
            continue;
        }
        s.record = record;
        s.changed = frame;
        let [image, buffer] = raw_addresses(&words);
        tracing::warn!(
            frame,
            heap = format_args!("{heap:#x}"),
            index,
            read_as = raw_hit(&words, spans).unwrap_or("?"),
            image_address = format_args!("{image:#x}"),
            buffer_address = format_args!("{buffer:#x}"),
            words = format_args!("{words:08x?}"),
            first_seen_frame = s.first,
            changed_frame = s.changed,
            "heap find: raw slot (a descriptor the title wrote itself)"
        );
    }
    let mut stale_seen = STALE_SEEN.lock().unwrap_or_else(|e| e.into_inner());
    let stale_seen = stale_seen.get_or_insert_with(HashSet::new);
    for c in &stale {
        if !stale_seen.insert((c.heap, c.index, [c.words[0], c.words[1], c.words[2]])) {
            continue;
        }
        if STALE_LINES.fetch_add(1, Ordering::Relaxed) >= MOST_STALE_LINES {
            break;
        }
        let former = c.former.map_or_else(
            || "unknown (freed before the switch, or no memory)".to_string(),
            |(start, len)| format!("{start:#x}..{:#x}", start + len),
        );
        tracing::warn!(
            frame,
            heap = format_args!("{:#x}", c.heap),
            index = c.index,
            words = format_args!("{:08x?}", c.words),
            former = %former,
            "heap find: stale slot (a record of a freed view; its memory is not the target)"
        );
    }
    for c in copies {
        let record = [c.words[0], c.words[1], c.words[2]];
        let mut new = false;
        let s = seen.entry((c.heap, c.index)).or_insert_with(|| {
            new = true;
            Seen {
                record,
                first: frame,
                changed: frame,
            }
        });
        if !new && s.record == record {
            continue;
        }
        s.record = record;
        s.changed = frame;
        let host = if c.visible {
            crate::descriptor::host_slot_state(c.heap, c.index, record)
        } else {
            "not_shader_visible"
        };
        let former = c.former.map_or_else(
            || "-".to_string(),
            |(start, len)| format!("{start:#x}..{:#x}", start + len),
        );
        tracing::warn!(
            frame,
            heap = format_args!("{:#x}", c.heap),
            visible = c.visible,
            index = c.index,
            kind = c.kind.text(),
            words = format_args!("{:08x?}", c.words),
            former = %former,
            first_seen_frame = s.first,
            changed_frame = s.changed,
            host,
            "heap find: copy slot (the found view named at another slot)"
        );
    }
}

/// A copy (`kind`) on list `list` from `from` into `to`, each a memory
/// range if known: logged if either meets the memory looked for.
pub(crate) fn copy(kind: &str, list: usize, to: Option<(u64, u64)>, from: Option<(u64, u64)>) {
    let spans = spans();
    let hits = |r: Option<(u64, u64)>| r.is_some_and(|(s, l)| meets(&spans, s, l));
    if !(hits(to) || hits(from)) || COPY_LINES.fetch_add(1, Ordering::Relaxed) >= MOST_COPY_LINES {
        return;
    }
    let text = |r: Option<(u64, u64)>| {
        r.map_or_else(|| "?".into(), |(s, l)| format!("{s:#x}..{:#x}", s + l))
    };
    tracing::warn!(
        frame = crate::frame::current(),
        at_s = format_args!("{:.2}", crate::snap::seconds().unwrap_or(0.0)),
        list = format_args!("{list:#x}"),
        reads_found = hits(from),
        writes_found = hits(to),
        "heap find: copy {kind} from {} into {}",
        text(from),
        text(to)
    );
}

/// [`copy`] of title resources.
pub(crate) fn copy_resources(kind: &str, list: usize, to: This, from: This) {
    use crate::resource::find_facts::object_span;
    copy(kind, list, object_span(to), object_span(from));
}

/// A walked frame's bind that picks views by index: counted.
pub(crate) fn note_by_index() {
    BY_INDEX.fetch_add(1, Ordering::Relaxed);
}

/// The place of this bind in the walked frame.
pub(crate) fn next_bind() -> u32 {
    BINDS.fetch_add(1, Ordering::Relaxed)
}

/// A number a bind may hold that names a found slot, and what it is.
pub(crate) type Wanted = (u32, String);

/// The numbers that name slot `slots` of the heap starting at `heap` (a
/// handle's low half, an index or a byte offset from the heap's start)
/// and, for each of a bind's `tables` (root slot, first slot) in that
/// heap, an index or byte offset from the table's first; those under
/// [`SMALLEST`] left out, each number once.
pub(crate) fn wanted(heap: u64, slots: &[u32], tables: &[(u8, u32)]) -> Vec<Wanted> {
    let mut out: Vec<Wanted> = Vec::new();
    let mut push = |value: u64, what: String| {
        if let Ok(v) = u32::try_from(value)
            && v >= SMALLEST
            && !out.iter().any(|w| w.0 == v)
        {
            out.push((v, what));
        }
    };
    for &s in slots {
        let s64 = u64::from(s);
        push(s64, format!("slot {s} as an index"));
        push(s64 * SLOT_BYTES, format!("slot {s} as a byte offset"));
        push(
            heap.wrapping_add(s64 * SLOT_BYTES) & 0xffff_ffff,
            format!("slot {s} handle low half"),
        );
        for &(root, t) in tables {
            if let Some(rel) = s.checked_sub(t) {
                let rel = u64::from(rel);
                push(rel, format!("slot {s} as an index from table{root} at {t}"));
                push(
                    rel * SLOT_BYTES,
                    format!("slot {s} as a byte offset from table{root} at {t}"),
                );
            }
        }
    }
    out
}

/// The first word of `words` that is a `wanted` number: its place and
/// what it names.
pub(crate) fn search<'a>(words: &[u32], wanted: &'a [Wanted]) -> Option<(usize, u32, &'a str)> {
    let mut values: Vec<(u32, usize)> = wanted.iter().enumerate().map(|(n, x)| (x.0, n)).collect();
    values.sort_unstable();
    words.iter().enumerate().find_map(|(i, &w)| {
        values
            .binary_search_by_key(&w, |v| v.0)
            .ok()
            .map(|at| (i, w, wanted[values[at].1].1.as_str()))
    })
}

/// The tables of a bind within [`NEAR`] slots before a found slot:
/// (root slot, table's first slot, found slot).
pub(crate) fn near(slots: &[u32], tables: &[(u8, u32)]) -> Vec<(u8, u32, u32)> {
    let mut out = Vec::new();
    for &(root, t) in tables {
        for &s in slots {
            if s >= t && s - t < NEAR {
                out.push((root, t, s));
            }
        }
    }
    out
}

static REFS_STATE: Mutex<Option<refs::Refs>> = Mutex::new(None);

/// What the buffer searches of the walked frame spend ([`refs::Refs`]).
struct RefsState;

impl RefsState {
    fn with<T>(f: impl FnOnce(&mut refs::Refs) -> T) -> T {
        f(REFS_STATE
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_or_insert_with(Default::default))
    }
}

/// A bind of a walked frame has its buffers searched.
pub(crate) fn refs_bind() {
    RefsState::with(refs::Refs::bind);
}

/// A bind of the Bink window was left out (rt0 the target, tiny or none).
pub(crate) fn refs_passed_over() {
    RefsState::with(refs::Refs::passed_over);
}

/// A draw or dispatch not run had its roots searched.
pub(crate) fn refs_dropped() {
    RefsState::with(refs::Refs::dropped);
}

pub(crate) use refs::Filter;

/// The words that may name one of `slots` of the heap starting at `heap`
/// in any encoding [`wanted`] makes, whatever the bind's tables.
pub(crate) fn filter(heap: u64, slots: &[u32]) -> Filter {
    let bound = slots.iter().map(|&s| u64::from(s) * SLOT_BYTES).max().unwrap_or(0);
    let extra = slots
        .iter()
        .map(|&s| (heap.wrapping_add(u64::from(s) * SLOT_BYTES) & 0xffff_ffff) as u32)
        .collect();
    Filter {
        bound: u32::try_from(bound).unwrap_or(u32::MAX),
        extra,
    }
}

/// [`search`] of title memory at `start` (`len` bytes; small buffers whole,
/// large ones their first [`refs::MOST_LARGE`]): read once a walked frame
/// whatever binds it, not searched again while its bytes hash the same
/// ([`refs`]); `None` if nothing is found or it cannot be read.
pub(crate) fn search_memory(
    start: u64,
    len: u64,
    filter: &Filter,
    wanted: &[Wanted],
) -> Option<(usize, u32, String)> {
    let len = refs::taken(len);
    if start == 0 || len == 0 || start % 4 != 0 {
        return None;
    }
    let k = (start, len, filter.id());
    let known = RefsState::with(|r| r.known(k).ok_or_else(|| r.may_read(len)));
    let candidates = match known {
        Ok(c) => c,
        Err(false) => return None,
        Err(true) => {
            let mut bytes = vec![0u8; len as usize];
            if !crate::host::read_into(start as *const u8, &mut bytes) {
                return None;
            }
            RefsState::with(|r| r.take(k, &bytes, filter))
        }
    };
    refs::lookup(&candidates?, wanted)
}

/// A bind of a walked frame that holds the address: its line.
pub(crate) fn log_draw(bind: u32, head: &str, hits: &[String]) {
    if LINES.fetch_add(1, Ordering::Relaxed) >= MOST_DRAW_LINES {
        return;
    }
    tracing::warn!(
        "heap find: draw frame={} bind={bind} {head} | {}",
        crate::frame::current(),
        hits.join("; ")
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_parse() {
        assert_eq!(address("0xe3d400000"), Some(0xe_3d40_0000));
        assert_eq!(address(" e3d400000 "), Some(0xe_3d40_0000));
        assert_eq!(address("0"), None);
        assert_eq!(address("zz"), None);
        assert_eq!(parse_find(" Bink "), Some(FOLLOW));
        assert_eq!(parse_find("0x10"), Some(0x10));
    }

    /// The phone's Bink pass is followed; the whole-screen video drawn by
    /// the same program and other programs are not.
    #[test]
    fn only_narrow_bink_targets_are_followed() {
        assert!(is_bink(BINK_PIXEL, "", 736));
        assert!(is_bink("0000000000000000", "PS_BinkNoAlpha_e9aa19fd", 736));
        assert!(!is_bink(BINK_PIXEL, "PS_BinkNoAlpha_e9aa19fd", 1920));
        assert!(!is_bink("2df9f97cd6586068", "TL_PS_PBR_MATERIAL_EMISSIVE", 736));
    }

    /// A raw image descriptor of the target's memory is found, as is a raw
    /// buffer descriptor inside it; zeros and other memory are not.
    #[test]
    fn raw_descriptors_read_as_image_or_buffer_addresses() {
        let target = (0xb_9d00_0000u64, 0xb_9d00_0000u64 + 736 * 1280 * 4);
        let mut image = [0u32; 8];
        image[0] = (0xb_9d00_0000u64 >> 8) as u32;
        image[1] = 0x1234_5600;
        assert_eq!(raw_addresses(&image)[0], 0xb_9d00_0000);
        assert_eq!(raw_hit(&image, &[target]), Some("image"));
        let mut buffer = [0u32; 8];
        buffer[0] = 0x9d01_0000;
        buffer[1] = 0x000b;
        assert_eq!(raw_hit(&buffer, &[target]), Some("buffer"));
        assert_eq!(raw_hit(&[0; 8], &[target]), None);
        image[0] = (0xa_0000_0000u64 >> 8) as u32;
        assert_eq!(raw_hit(&image, &[target]), None);
    }

    /// Memory meets a span when the two overlap at all.
    #[test]
    fn spans_meet_overlapping_memory() {
        let spans = [(0x1000, 0x2000)];
        assert!(meets(&spans, 0x1800, 0x1000));
        assert!(meets(&spans, 0x800, 0x900));
        assert!(!meets(&spans, 0x2000, 0x100));
        assert!(!meets(&spans, 0x800, 0x800));
    }

    /// A found slot is named by its index, byte offset and handle from
    /// the heap's start, and by its index or byte offset from a table's
    /// first; small numbers and tables past the slot are left out.
    #[test]
    fn found_slots_are_wanted_in_every_encoding() {
        let heap = 0x2234_0000;
        let w = wanted(heap, &[114_106], &[(2, 114_000), (3, 200_000), (4, 114_100)]);
        let values: Vec<u32> = w.iter().map(|x| x.0).collect();
        assert!(values.contains(&114_106));
        assert!(values.contains(&(114_106 * 32)));
        assert!(values.contains(&(0x2234_0000 + 114_106 * 32)));
        assert!(values.contains(&(106 * 32)));
        assert!(!values.contains(&106), "under the smallest number searched");
        assert!(!values.contains(&6), "too small");
        assert!(w.iter().all(|x| !x.1.contains("table3")), "a table past the slot");
        let words = [7, 0, 106 * 32, 114_106];
        assert_eq!(
            search(&words, &w),
            Some((2, 106 * 32, "slot 114106 as a byte offset from table2 at 114000"))
        );
        assert_eq!(search(&[1, 2, 3], &w), None);
    }

    /// A table within reach of a found slot is named; one past it or too
    /// far before it is not.
    #[test]
    fn tables_near_a_found_slot_are_named() {
        let tables = [(1, 114_000), (2, 114_107), (3, 100_000)];
        assert_eq!(near(&[114_106], &tables), vec![(1, 114_000, 114_106)]);
        assert!(near(&[114_106], &[(0, 114_106 - NEAR)]).is_empty());
        assert_eq!(near(&[5], &[(0, 0)]), vec![(0, 0, 5)]);
    }

    /// Buffers are read once a walked frame per table set; a new frame
    /// starts over and reports what the last one searched.
    #[test]
    fn buffers_are_searched_once_a_frame() {
        let data: Vec<u32> = vec![1, 2, 114_106, 4];
        let start = data.as_ptr() as u64;
        let w = wanted(0x1000_0000, &[114_106], &[]);
        let f = filter(0x1000_0000, &[114_106]);
        let _ = RefsState::with(|r| r.start(1));
        assert_eq!(
            search_memory(start, 16, &f, &w),
            Some((2, 114_106, "slot 114106 as an index".to_string()))
        );
        assert_eq!(
            search_memory(start, 16, &f, &w).map(|f| f.0),
            Some(2),
            "kept for the frame"
        );
        let spent = RefsState::with(|r| r.start(2));
        assert_eq!((spent.searched, spent.buffers, spent.full), ([16, 0], 1, false));
        assert_eq!(search_memory(0, 16, &f, &w), None);
        assert_eq!(search_memory(start + 2, 16, &f, &w), None, "unaligned");
        // A material table of 4 MiB naming part N's slot 109463 as a byte
        // offset from a table at 109000, 3 MiB in (past round 8's 1 MiB).
        let heap = 0x0b69_0000;
        let mut table = vec![0x3f80_0000u32; (4 << 20) / 4];
        table[(3 << 20) / 4] = 463 * 32;
        let at = table.as_ptr() as u64;
        let f = filter(heap, &[109_463]);
        let w = wanted(heap, &[109_463], &[(2, 109_000)]);
        assert_eq!(
            search_memory(at, 4 << 20, &f, &w),
            Some((
                (3 << 20) / 4,
                463 * 32,
                "slot 109463 as a byte offset from table2 at 109000".to_string()
            ))
        );
        let w_other = wanted(heap, &[109_463], &[(1, 100_000)]);
        assert_eq!(search_memory(at, 4 << 20, &f, &w_other), None, "another table's numbers");
        let spent = RefsState::with(|r| r.start(3));
        assert_eq!((spent.searched, spent.buffers), ([0, 4 << 20], 1), "read once for both binds");
    }

    /// Every target but tiny ones is searched: the scene at the title's
    /// internal sizes under the upscaler, landscape UI and effect targets
    /// (round 8 left 1024x512 and 256x256 out), a phone's portrait one.
    #[test]
    fn every_target_but_tiny_ones_is_searched() {
        for (w, h) in [
            (3840, 2160),
            (2560, 1440),
            (1920, 1080),
            (1280, 720),
            (960, 540),
            (736, 1280),
            (1024, 512),
            (512, 256),
            (256, 256),
            (128, 128),
        ] {
            assert!(searched_target(w, h), "{w}x{h}");
        }
        for (w, h) in [(64, 64), (1, 1), (4096, 1), (16, 1024)] {
            assert!(!searched_target(w, h), "{w}x{h}");
        }
    }

    /// The filter keeps every number [`wanted`] makes for any table, so a
    /// buffer read once answers every bind.
    #[test]
    fn the_filter_takes_every_wanted_number() {
        let heap = 0x0b69_0000;
        let slots = [109_463, 91_711];
        let f = filter(heap, &slots);
        assert_eq!(f.bound, 109_463 * 32);
        for tables in [&[][..], &[(2, 109_000)][..], &[(0, 0), (5, 91_000)][..]] {
            for (v, what) in wanted(heap, &slots, tables) {
                let bytes = v.to_le_bytes();
                assert_eq!(refs::candidates(&bytes, &f), vec![(v, 0)], "{what}");
            }
        }
        assert!(refs::candidates(&1.0f32.to_bits().to_le_bytes(), &f).is_empty());
    }

    #[test]
    fn the_switches_are_live() {
        victory_core::settings::check_live("VICTORY_HEAP_FIND_REFS", "1", refs, false, true);
        victory_core::settings::check_live("VICTORY_HEAP_FIND_REFS", "2", refs_all, false, true);
        victory_core::settings::check_live("VICTORY_HEAP_FIND_REFS", "1", refs_all, false, false);
        victory_core::settings::check_live(
            "VICTORY_HEAP_FIND",
            "0xe3d400000",
            || FIND.get(),
            0,
            0xe_3d40_0000,
        );
        victory_core::settings::check_live(
            "VICTORY_HEAP_FIND_WINDOW",
            "40-65",
            || WINDOW.get(),
            0,
            40 << 32 | 65,
        );
        victory_core::settings::check_live(
            "VICTORY_HEAP_FIND_EVERY_MS",
            "500",
            || EVERY_MS.get(),
            1000,
            500,
        );
    }

    /// A buffer view of placed memory holds the address when it reaches it;
    /// one before or past it does not.
    #[test]
    fn placed_buffer_views_cover_by_their_width() {
        let view = |start: u64, len: u64| {
            let mut resource = [0u32; 14];
            resource[0] = 1;
            (resource[4], resource[5]) = (len as u32, (len >> 32) as u32);
            let mut desc = [0u32; 16];
            (desc[10], desc[11]) = (start as u32, (start >> 32) as u32);
            View::Placed(false, resource, desc)
        };
        let find = 0xe_3d40_0000;
        let end = find + 1;
        assert_eq!(covers(&view(find, 0x1000), find, end), Some((find, 0x1000)));
        assert_eq!(
            covers(&view(find - 0x100, 0x200), find, end),
            Some((find - 0x100, 0x200))
        );
        assert_eq!(covers(&view(find - 0x100, 0x100), find, end), None);
        assert_eq!(covers(&view(find + 0x100, 0x1000), find, end), None);
        assert_eq!(covers(&View::Constant(find, 256), find, end), None);
        // A view lying inside a followed target, not at its start.
        assert_eq!(
            covers(&view(find + 0x100, 0x100), find, find + 0x1000),
            Some((find + 0x100, 0x100))
        );
    }
}
