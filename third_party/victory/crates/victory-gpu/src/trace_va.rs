//! (diagnostics) `VICTORY_TRACE_VA`: logs every host action on chosen title
//! memory, one line each, to line up what the host did to a buffer with the
//! dispatches that write and read it.
//!
//! - `auto`: the buffer views bound by dispatches of the shaders in
//!   `VICTORY_TRACE_VA_HASHES` (default: the face blendshape chain,
//!   `CS_BlendshapeTranslationsHSSGraphOutputExtract`, `CS_ComputeStickyLips`
//!   and `CS_ReconstructBlendshapeNormals`), learnt as they are bound.
//! - `<hex address>[:<hex length>]`, comma separated: those ranges (a
//!   length of 64 KiB if none is given), plus the learnt ones above.
//!
//! `VICTORY_TRACE_VA_FRAMES=<first>-<last>` limits the lines to those frames.
//! Off, every hook is one cached boolean test.

use std::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::{Mutex, OnceLock};

const DEFAULT_HASHES: [&str; 3] = ["970c4fbb3b017c0e", "068c90badc86f1fd", "e4a7e088c7780ce8"];
const DEFAULT_LEN: u64 = 64 << 10;
/// Lines logged at most, so a long run cannot fill the disk.
const MAX_LINES: u64 = 400_000;

#[derive(Debug, PartialEq)]
struct Config {
    ranges: Vec<(u64, u64)>,
    hashes: Vec<String>,
    frames: (u64, u64),
}

fn parse(value: Option<&str>, hashes: Option<&str>, frames: Option<&str>) -> Option<Config> {
    let value = value?.trim();
    if value.is_empty() || value == "0" {
        return None;
    }
    let hex = |s: &str| u64::from_str_radix(s.trim().trim_start_matches("0x"), 16).ok();
    let mut ranges = Vec::new();
    if value != "auto" {
        for part in value.split(',').filter(|p| !p.trim().is_empty()) {
            let (at, len) = match part.split_once(':') {
                Some((a, l)) => (hex(a)?, hex(l)?),
                None => (hex(part)?, DEFAULT_LEN),
            };
            ranges.push((at, at.saturating_add(len)));
        }
    }
    let hashes = match hashes {
        Some(h) => h
            .split(',')
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty())
            .collect(),
        None => DEFAULT_HASHES.iter().map(|&h| h.to_owned()).collect(),
    };
    let frames = frames
        .and_then(|f| f.split_once('-'))
        .and_then(|(a, b)| Some((a.trim().parse().ok()?, b.trim().parse().ok()?)))
        .unwrap_or((0, u64::MAX));
    Some(Config {
        ranges,
        hashes,
        frames,
    })
}

fn config() -> Option<&'static Config> {
    static CFG: OnceLock<Option<Config>> = OnceLock::new();
    CFG.get_or_init(|| {
        let var = |n: &str| std::env::var(n).ok();
        let c = parse(
            var("VICTORY_TRACE_VA").as_deref(),
            var("VICTORY_TRACE_VA_HASHES").as_deref(),
            var("VICTORY_TRACE_VA_FRAMES").as_deref(),
        );
        if let Some(c) = &c {
            let ranges: Vec<String> = c
                .ranges
                .iter()
                .map(|r| format!("{:#x}..{:#x}", r.0, r.1))
                .collect();
            tracing::info!(ranges = ?ranges, hashes = ?c.hashes, frames = ?c.frames, "trace_va on");
            RANGES
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .extend(c.ranges.iter().copied());
            ANY.store(!c.ranges.is_empty(), Relaxed);
        }
        c
    })
    .as_ref()
}

static RANGES: Mutex<Vec<(u64, u64)>> = Mutex::new(Vec::new());
static ANY: AtomicBool = AtomicBool::new(false);
static LINES: AtomicU64 = AtomicU64::new(0);

thread_local! {
    /// The watched shader of the dispatch this thread is recording, if any.
    static TAG: Cell<Option<&'static str>> = const { Cell::new(None) };
}

pub(crate) fn on() -> bool {
    config().is_some()
}

/// Adds `[from, to)` to `ranges`, joined with any it touches.
fn learn(ranges: &mut Vec<(u64, u64)>, from: u64, to: u64) -> bool {
    if ranges.iter().any(|r| r.0 <= from && to <= r.1) {
        return false;
    }
    let (mut lo, mut hi) = (from, to);
    ranges.retain(|r| {
        let touches = r.0 <= hi && lo <= r.1;
        if touches {
            lo = lo.min(r.0);
            hi = hi.max(r.1);
        }
        !touches
    });
    ranges.push((lo, hi));
    true
}

fn overlaps(from: u64, to: u64) -> bool {
    ANY.load(Relaxed)
        && RANGES
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .any(|r| r.0 < to && from < r.1)
}

fn line(what: &str, from: u64, to: u64, extra: u64) {
    let Some(c) = config() else { return };
    let frame = crate::queue::frame_count();
    if frame < c.frames.0 || frame > c.frames.1 || LINES.fetch_add(1, Relaxed) >= MAX_LINES {
        return;
    }
    tracing::info!(
        frame,
        submission = crate::shadow::prepared(),
        from = format_args!("{from:#x}"),
        len = to - from,
        extra,
        tag = TAG.get().unwrap_or("-"),
        "trace_va {what}"
    );
}

/// A host action on title memory `[address, address + len)`; `extra` is its
/// count (bytes copied, written...).
pub(crate) fn event(what: &str, address: u64, len: u64, extra: u64) {
    if !on() {
        return;
    }
    let to = address.saturating_add(len);
    if overlaps(address, to) {
        line(what, address, to, extra);
    }
}

/// Clears the dispatch tag when the dispatch is recorded.
pub(crate) struct Tagged;

impl Drop for Tagged {
    fn drop(&mut self) {
        TAG.set(None);
    }
}

/// A dispatch of title pipeline `this` into host list `list` begins.
pub(crate) fn dispatch(this: crate::object::This, list: usize) -> Option<Tagged> {
    let c = config()?;
    let shaders = crate::pipeline::shaders_of(this);
    let hash = c.hashes.iter().find(|h| shaders.contains(h.as_str()))?;
    let hash: &'static str = DEFAULT_HASHES
        .iter()
        .copied()
        .find(|d| d == hash)
        .unwrap_or_else(|| Box::leak(hash.clone().into_boxed_str()));
    TAG.set(Some(hash));
    line("dispatch", list as u64, list as u64, 0);
    Some(Tagged)
}

/// A buffer view `[address, address + len)` is bound for the draw or
/// dispatch being recorded (`uav`: shaders may write it).
pub(crate) fn view(uav: bool, address: u64, len: u64) {
    if !on() {
        return;
    }
    let to = address.saturating_add(len);
    if TAG.get().is_some() {
        let learnt = learn(
            &mut RANGES.lock().unwrap_or_else(|e| e.into_inner()),
            address,
            to,
        );
        ANY.store(true, Relaxed);
        if learnt {
            line("learnt", address, to, u64::from(uav));
        }
    }
    if overlaps(address, to) {
        line(if uav { "bind uav" } else { "bind srv" }, address, to, 0);
    }
}

#[cfg(test)]
mod tests {
    use super::{Config, learn, parse};

    #[test]
    fn parses_auto_ranges_hashes_and_frames() {
        assert_eq!(parse(None, None, None), None);
        assert_eq!(parse(Some("0"), None, None), None);
        let auto = parse(Some("auto"), None, None).unwrap();
        assert!(auto.ranges.is_empty());
        assert_eq!(auto.hashes.len(), 3);
        assert_eq!(auto.frames, (0, u64::MAX));
        assert_eq!(
            parse(
                Some("0xd71fc6330:2c0000,1000"),
                Some("AB, cd"),
                Some("900-1200")
            ),
            Some(Config {
                ranges: vec![(0xd71fc6330, 0xd71fc6330 + 0x2c0000), (0x1000, 0x11000)],
                hashes: vec!["ab".into(), "cd".into()],
                frames: (900, 1200),
            })
        );
        assert_eq!(parse(Some("zz"), None, None), None);
    }

    #[test]
    fn learnt_ranges_join_where_they_touch() {
        let mut r = Vec::new();
        assert!(learn(&mut r, 10, 20));
        assert!(!learn(&mut r, 12, 18));
        assert!(learn(&mut r, 20, 30));
        assert_eq!(r, vec![(10, 30)]);
        assert!(learn(&mut r, 40, 50));
        assert!(learn(&mut r, 5, 45));
        assert_eq!(r, vec![(5, 50)]);
    }
}
