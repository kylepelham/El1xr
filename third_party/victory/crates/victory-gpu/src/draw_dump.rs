//! (phonescreen, #115) `VICTORY_DRAW_DUMP=<from>-<to>` (live, off by
//! default): within that window of seconds (counted as the snaps are, from
//! the first present: `snap-0046s-...` is second 46), one whole frame every
//! `VICTORY_DRAW_DUMP_EVERY_MS` (default 2000) has every draw and dispatch
//! logged, one `draw dump:` line per bind: its programs (hash and name),
//! whether it picks views from the heap by index, its render targets and
//! depth, and the textures and unordered views its tables bind, each with
//! the title memory it lies in (the address `live textures:` and
//! `placed inside:` lines name), format, size and levels. So one run names
//! the draw that puts a surface on screen (the phone screen in the diner)
//! and the texture it samples. At most [`MOST_FRAMES`] frames and
//! [`MOST_LINES_PER_FRAME`] lines a frame.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use victory_core::settings::Live;

static WINDOW: Live = Live::new("VICTORY_DRAW_DUMP", window_bits);
static EVERY_MS: Live = Live::new("VICTORY_DRAW_DUMP_EVERY_MS", |v| {
    v.and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(2000)
});

/// Frames dumped in a run at most.
const MOST_FRAMES: u32 = 40;
/// Lines of one frame at most.
const MOST_LINES_PER_FRAME: u32 = 8000;

/// `"40-65"` as `from << 32 | to` (seconds); 0 (off) when not a window.
fn window_bits(value: Option<&str>) -> u64 {
    window(value).map_or(0, |(from, to)| u64::from(from) << 32 | u64::from(to))
}

/// `"<from>-<to>"` seconds, `from < to`.
pub(crate) fn window(value: Option<&str>) -> Option<(u32, u32)> {
    let (from, to) = value?.trim().split_once('-')?;
    let (from, to) = (from.trim().parse().ok()?, to.trim().parse().ok()?);
    (from < to).then_some((from, to))
}

/// The frame being dumped (`u64::MAX`: none).
static DUMPING: AtomicU64 = AtomicU64::new(u64::MAX);
/// The last frame [`recording`] decided about.
static DECIDED: AtomicU64 = AtomicU64::new(u64::MAX);
static BINDS: AtomicU32 = AtomicU32::new(0);
static FRAMES: AtomicU32 = AtomicU32::new(0);
/// When the last dumped frame began, in seconds since the first present.
static LAST_AT: Mutex<Option<f64>> = Mutex::new(None);

/// Whether this frame's binds are dumped. Decided at the frame's first bind.
pub(crate) fn recording() -> bool {
    let bits = WINDOW.get();
    if bits == 0 {
        return false;
    }
    let frame = crate::frame::current();
    if DECIDED.load(Ordering::Relaxed) != frame && DECIDED.swap(frame, Ordering::Relaxed) != frame {
        decide(frame, bits);
    }
    DUMPING.load(Ordering::Relaxed) == frame
}

fn decide(frame: u64, bits: u64) {
    let (from, to) = ((bits >> 32) as f64, (bits & 0xffff_ffff) as f64);
    let Some(at) = crate::snap::seconds() else {
        return;
    };
    let mut last = LAST_AT.lock().unwrap_or_else(|e| e.into_inner());
    if !due(at, from, to, *last, EVERY_MS.get()) || FRAMES.load(Ordering::Relaxed) >= MOST_FRAMES {
        return;
    }
    *last = Some(at);
    let n = FRAMES.fetch_add(1, Ordering::Relaxed) + 1;
    let before = BINDS.swap(0, Ordering::Relaxed);
    DUMPING.store(frame, Ordering::Relaxed);
    tracing::info!(
        frame,
        at_s = format_args!("{at:.2}"),
        dump = n,
        binds_of_the_dump_before = before,
        "draw dump: a frame's binds follow (one line each)"
    );
}

/// Whether a frame beginning `at` seconds is dumped: inside `[from, to)`
/// and `every_ms` after the last dumped one.
pub(crate) fn due(at: f64, from: f64, to: f64, last: Option<f64>, every_ms: u64) -> bool {
    at >= from && at < to && last.is_none_or(|l| (at - l) * 1000.0 >= every_ms as f64)
}

/// A program's name, made once per shader object (`key`): finding it
/// scans the container.
pub(crate) fn name(key: usize, make: impl FnOnce() -> String) -> String {
    static NAMES: Mutex<Option<std::collections::HashMap<usize, String>>> = Mutex::new(None);
    let mut names = NAMES.lock().unwrap_or_else(|e| e.into_inner());
    names
        .get_or_insert_with(Default::default)
        .entry(key)
        .or_insert_with(make)
        .clone()
}

/// One binding of a dumped bind: `rt0`, `dsv`, `t3`, `u0`...
pub(crate) struct Bound {
    pub(crate) slot: String,
    /// Title memory, 0 if unknown.
    pub(crate) memory: u64,
    /// Width, height, format (DXGI), levels; `None` for a view placed in
    /// memory with no resource made for it yet.
    pub(crate) shape: Option<[u32; 4]>,
}

/// The binding of title texture `this` in `slot`.
pub(crate) fn texture(slot: String, this: crate::object::This) -> Option<Bound> {
    let (memory, desc) = crate::resource::texture_memory(this)?;
    Some(Bound {
        slot,
        memory: memory as u64,
        shape: Some(desc.shape()),
    })
}

/// The line of a bind, if the frame still has room for one.
pub(crate) fn log(
    kind: &str,
    stages: &[(&str, &str, String)],
    heap: bool,
    list: usize,
    bound: &[Bound],
) {
    let bind = BINDS.fetch_add(1, Ordering::Relaxed);
    if bind >= MOST_LINES_PER_FRAME {
        return;
    }
    tracing::info!(
        "{}",
        line(
            crate::frame::current(),
            bind,
            kind,
            stages,
            heap,
            list,
            bound
        )
    );
}

fn line(
    frame: u64,
    bind: u32,
    kind: &str,
    stages: &[(&str, &str, String)],
    heap: bool,
    list: usize,
    bound: &[Bound],
) -> String {
    use std::fmt::Write;
    let mut text = format!("draw dump: frame={frame} bind={bind} list={list:#x} {kind}");
    for (stage, hash, name) in stages {
        let short = match *stage {
            "vertex" => "vs",
            "pixel" => "ps",
            "compute" => "cs",
            other => other,
        };
        let _ = write!(text, " {short}={hash}");
        if !name.is_empty() {
            let _ = write!(text, " ({name})");
        }
    }
    let _ = write!(text, " heap_indexed={heap} |");
    for b in bound {
        let _ = write!(text, " {} {:#x}", b.slot, b.memory);
        match b.shape {
            Some([w, h, f, l]) => {
                let _ = write!(text, " fmt {f} {w}x{h} lv {l};");
            }
            None => text.push_str(" placed;"),
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_switches_are_live() {
        victory_core::settings::check_live(
            "VICTORY_DRAW_DUMP",
            "40-65",
            || WINDOW.get(),
            0,
            40 << 32 | 65,
        );
        victory_core::settings::check_live(
            "VICTORY_DRAW_DUMP_EVERY_MS",
            "500",
            || EVERY_MS.get(),
            2000,
            500,
        );
    }

    #[test]
    fn windows_parse() {
        assert_eq!(window(Some("40-65")), Some((40, 65)));
        assert_eq!(window(Some(" 46 - 48 ")), Some((46, 48)));
        assert_eq!(window(Some("65-40")), None);
        assert_eq!(window(Some("1")), None);
        assert_eq!(window(Some("0")), None);
        assert_eq!(window(None), None);
        assert_eq!(window_bits(Some("0")), 0);
    }

    #[test]
    fn frames_are_dumped_inside_the_window_every_so_often() {
        assert!(!due(39.9, 40.0, 65.0, None, 2000));
        assert!(due(40.0, 40.0, 65.0, None, 2000));
        assert!(!due(41.5, 40.0, 65.0, Some(40.0), 2000));
        assert!(due(42.0, 40.0, 65.0, Some(40.0), 2000));
        assert!(due(40.05, 40.0, 65.0, Some(40.0), 0));
        assert!(!due(65.0, 40.0, 65.0, Some(40.0), 2000));
    }

    #[test]
    fn a_line_names_programs_and_memory() {
        let stages = [
            ("vertex", "aaaa", String::new()),
            ("pixel", "bbbb", "FYUV_Wrapped".to_string()),
        ];
        let bound = [
            Bound {
                slot: "rt0".into(),
                memory: 0xc000_0000,
                shape: Some([736, 1280, 28, 1]),
            },
            Bound {
                slot: "t0".into(),
                memory: 0xcbed0_0000,
                shape: Some([736, 1280, 61, 1]),
            },
            Bound {
                slot: "t1".into(),
                memory: 0x7414_00000,
                shape: None,
            },
        ];
        assert_eq!(
            line(755, 3, "draw", &stages, true, 0x10, &bound),
            "draw dump: frame=755 bind=3 list=0x10 draw vs=aaaa ps=bbbb (FYUV_Wrapped) heap_indexed=true | \
             rt0 0xc0000000 fmt 28 736x1280 lv 1; t0 0xcbed00000 fmt 61 736x1280 lv 1; t1 0x741400000 placed;"
        );
    }
}
