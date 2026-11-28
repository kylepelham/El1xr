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

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use victory_core::settings::Live;

use crate::descriptor::View;
use crate::object::This;

static FIND: Live = Live::new("VICTORY_HEAP_FIND", |v| v.and_then(address).unwrap_or(0));
static WINDOW: Live = Live::new("VICTORY_HEAP_FIND_WINDOW", |v| {
    crate::draw_dump::window(v).map_or(0, |(from, to)| u64::from(from) << 32 | u64::from(to))
});
static EVERY_MS: Live = Live::new("VICTORY_HEAP_FIND_EVERY_MS", |v| {
    v.and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(1000)
});

/// Frames walked in a run at most.
const MOST_FRAMES: u32 = 90;
/// Slots a walk keeps at most.
const MOST_SLOTS: usize = 256;
/// Draw lines of one frame at most.
const MOST_DRAW_LINES: u32 = 400;
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

/// The address looked for, 0 when off.
pub(crate) fn find() -> u64 {
    FIND.get()
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

fn decide(frame: u64, find: u64) {
    let Some(at) = crate::snap::seconds() else {
        return;
    };
    let bits = WINDOW.get();
    let (from, to) = if bits == 0 {
        (0.0, f64::INFINITY)
    } else {
        ((bits >> 32) as f64, (bits & 0xffff_ffff) as f64)
    };
    {
        let mut last = LAST_AT.lock().unwrap_or_else(|e| e.into_inner());
        if !crate::draw_dump::due(at, from, to, *last, EVERY_MS.get())
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
    scan(frame, at, find);
}

/// Where the memory of `view` starts and how many bytes it takes, if it
/// holds `find`.
pub(crate) fn covers(view: &View, find: u64) -> Option<(u64, u64)> {
    let near = |start: u64| start <= find && find - start < FARTHEST;
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
    (find < start.saturating_add(len.max(1))).then_some((start, len))
}

/// A view and what it shows, for a line.
pub(crate) fn view_text(view: &View, find: u64) -> String {
    use crate::resource::find_facts as f;
    let span = covers(view, find).map_or_else(String::new, |(start, len)| {
        format!(
            "memory {start:#x}..{:#x} (+{:#x})",
            start + len,
            find - start
        )
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

fn scan(frame: u64, at: f64, find: u64) {
    let started = std::time::Instant::now();
    let found = crate::descriptor::shader_views_where(|v| covers(v, find).is_some(), MOST_SLOTS);
    let by_index = BY_INDEX.swap(0, Ordering::Relaxed);
    tracing::warn!(
        frame,
        at_s = format_args!("{at:.2}"),
        find = format_args!("{find:#x}"),
        slots = found.len(),
        target = %crate::resource::find_facts::target_text(find),
        binds_by_index_of_the_last_walked_frame = by_index,
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
            "heap find: slot {}",
            view_text(&view, find)
        );
    }
}

/// A walked frame's bind that picks views by index: counted.
pub(crate) fn note_by_index() {
    BY_INDEX.fetch_add(1, Ordering::Relaxed);
}

/// The place of this bind in the walked frame.
pub(crate) fn next_bind() -> u32 {
    BINDS.fetch_add(1, Ordering::Relaxed)
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
    }

    #[test]
    fn the_switches_are_live() {
        victory_core::settings::check_live(
            "VICTORY_HEAP_FIND",
            "0xe3d400000",
            find,
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
        assert_eq!(covers(&view(find, 0x1000), find), Some((find, 0x1000)));
        assert_eq!(
            covers(&view(find - 0x100, 0x200), find),
            Some((find - 0x100, 0x200))
        );
        assert_eq!(covers(&view(find - 0x100, 0x100), find), None);
        assert_eq!(covers(&view(find + 0x100, 0x1000), find), None);
        assert_eq!(covers(&View::Constant(find, 256), find), None);
    }
}
