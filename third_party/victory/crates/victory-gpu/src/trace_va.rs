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
/// Host lists a watched shader was dispatched into.
static LISTS: Mutex<Vec<usize>> = Mutex::new(Vec::new());
/// Host lists recorded since their last submission, with the role they
/// play for the trace: a watched shader ("chain") or one of
/// `VICTORY_TRACE_VA_ORDER` (default the face network's last op,
/// `CS_GatherOperation`).
static ROLES: Mutex<Vec<(usize, &'static str)>> = Mutex::new(Vec::new());

fn order_hashes() -> &'static [String] {
    static H: OnceLock<Vec<String>> = OnceLock::new();
    H.get_or_init(|| {
        std::env::var("VICTORY_TRACE_VA_ORDER")
            .unwrap_or_else(|_| "aaf7da7a94dc72a9".to_owned())
            .split(',')
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty())
            .collect()
    })
}

fn note_role(list: usize, role: &'static str) {
    let mut roles = ROLES.lock().unwrap_or_else(|e| e.into_inner());
    if !roles.iter().any(|&(l, r)| l == list && r == role) {
        roles.push((list, role));
    }
}

/// The positions in `lists` of the lists with a role, as `role@index`.
fn roles_in(roles: &[(usize, &'static str)], lists: &[usize]) -> Vec<String> {
    lists
        .iter()
        .enumerate()
        .flat_map(|(i, l)| {
            roles
                .iter()
                .filter(move |&&(r, _)| r == *l)
                .map(move |&(_, role)| format!("{role}@{i}"))
        })
        .collect()
}

/// A title submission of host lists `lists` (in order) to a queue of
/// `kind`: logged when it holds a list with a role; those lists' roles are
/// then forgotten (the title records them again).
pub(crate) fn submit(queue: usize, kind: u32, lists: &[usize]) {
    if !on() {
        return;
    }
    let mut roles = ROLES.lock().unwrap_or_else(|e| e.into_inner());
    let found = roles_in(&roles, lists);
    if found.is_empty() {
        return;
    }
    roles.retain(|(l, _)| !lists.contains(l));
    drop(roles);
    let Some(c) = config() else { return };
    let frame = crate::queue::frame_count();
    if frame < c.frames.0 || frame > c.frames.1 {
        return;
    }
    tracing::info!(
        frame,
        queue = format_args!("{queue:#x}"),
        kind,
        count = lists.len(),
        roles = found.join(" "),
        "trace_va submit"
    );
}

thread_local! {
    /// The watched shader of the dispatch this thread is recording, if any.
    static TAG: Cell<Option<&'static str>> = const { Cell::new(None) };
    /// The first shader of any other dispatch this thread is recording, so
    /// the binds of the passes around the watched ones are named too.
    static OTHER: Cell<Option<&'static str>> = const { Cell::new(None) };
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
        tag = TAG.get().or(OTHER.get()).unwrap_or("-"),
        "trace_va {what}"
    );
}

/// A list-wide command (`what`: a sync, a barrier) recorded into host list
/// `list`; logged for the lists watched shaders were dispatched into.
pub(crate) fn list_op(what: &str, list: usize, extra: u64) {
    if !on() || !LISTS.lock().unwrap_or_else(|e| e.into_inner()).contains(&list) {
        return;
    }
    line(what, list as u64, list as u64, extra);
}

/// (diagnostics) `VICTORY_DISPATCH_UAV=1`: a UAV barrier on every resource
/// before each dispatch, to tell a race between dispatches the title left
/// unordered from other faults.
pub(crate) fn dispatch_barrier() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_DISPATCH_UAV").is_some_and(|v| v == "1"))
}

/// Whether `[address, address + len)` touches a traced range.
pub(crate) fn watched(address: u64, len: u64) -> bool {
    on() && overlaps(address, address.saturating_add(len))
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
        OTHER.set(None);
    }
}

/// `hash` as a string that lives for the run (one per shader).
fn interned(hash: &str) -> &'static str {
    static NAMES: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());
    let mut names = NAMES.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(&n) = names.iter().find(|&&n| n == hash) {
        return n;
    }
    let n: &'static str = Box::leak(hash.to_owned().into_boxed_str());
    names.push(n);
    n
}

/// A dispatch of title pipeline `this` into host list `list` begins.
pub(crate) fn dispatch(this: crate::object::This, list: usize) -> Option<Tagged> {
    let c = config()?;
    let shaders = crate::pipeline::shaders_of(this);
    let Some(hash) = c.hashes.iter().find(|h| shaders.contains(h.as_str())) else {
        // `[compute 0123456789abcdef]`: the first shader's hash.
        let first = shaders.split_whitespace().nth(1).map(|h| h.trim_end_matches([']', ',']));
        let first = first.filter(|h| h.len() == 16).map(interned);
        if let Some(h) = first
            && order_hashes().iter().any(|o| o == h)
        {
            note_role(list, "ml");
            line("ml dispatch", list as u64, list as u64, 0);
        }
        OTHER.set(first);
        return Some(Tagged);
    };
    let hash: &'static str = DEFAULT_HASHES
        .iter()
        .copied()
        .find(|d| d == hash)
        .unwrap_or_else(|| Box::leak(hash.clone().into_boxed_str()));
    TAG.set(Some(hash));
    {
        let mut lists = LISTS.lock().unwrap_or_else(|e| e.into_inner());
        if !lists.contains(&list) {
            lists.push(list);
        }
    }
    note_role(list, "chain");
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
    use super::{Config, learn, parse, roles_in};

    #[test]
    fn roles_are_named_by_their_place_in_the_submission() {
        let roles = [(7, "ml"), (9, "chain"), (7, "chain")];
        assert_eq!(roles_in(&roles, &[1, 9, 7]), vec!["chain@1", "ml@2", "chain@2"]);
        assert!(roles_in(&roles, &[1, 2]).is_empty());
    }

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
