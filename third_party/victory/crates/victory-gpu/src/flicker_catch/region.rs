//! (phonescreen, #115) The flicker catcher's regional mode
//! (`VICTORY_FLICKER_CATCH_REGION=<tiles>`, live, with
//! `VICTORY_FLICKER_CATCH=1`): a frame where that many tiles of the 16x9
//! grid, joined by their sides, go darker by two thirds and are back the
//! frame after is caught like a frame's flicker (the cell wall beside the
//! bunk going black for one snap: about a tenth of the frame, under the
//! quarter a frame's flicker needs).
//!
//! While it is on, every draw into a target shaped as the presented
//! picture (16:9) and every dispatch is noted per (title pipeline, render
//! target 0) for the frame: draws, the tiles its viewport and scissor
//! cover (a dispatch writing a 16:9 texture covers them all), and, the
//! first time a frame meets the pair with a set of tables (at most
//! [`SIGS`] sets), the textures its tables bind: memory, size, format,
//! heap slot, what the host shows in that slot, the view's generation and
//! the frame the catcher first saw the texture. A region event's line then
//! names the pairs that drew into the region in the frame before, the dark
//! frame and the one after, those that drew before and not in the dark
//! frame first, with each texture flagged `new` (first seen in the frame
//! before or the dark one), `remade` (another view or memory in that slot
//! than in the other frame) or by a host slot not `made`, and the
//! pipelines first bound (the `first bind` line) in the two frames before
//! the dark one and in it.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

use super::Summary;
use super::detect::{ACROSS, DOWN, TILES};

static SET: victory_core::settings::Live =
    victory_core::settings::Live::new("VICTORY_FLICKER_CATCH_REGION", parse);

fn parse(v: Option<&str>) -> u64 {
    v.and_then(|v| v.trim().parse::<u64>().ok())
        .map_or(0, |n| n.min(TILES as u64))
}

/// Adjacent tiles that make a region (0: regions not caught).
pub(crate) fn tiles() -> usize {
    if super::on() { SET.get() as usize } else { 0 }
}

pub(crate) fn on() -> bool {
    tiles() > 0
}

/// Frames whose notes are kept (the read-back and the frame after come a
/// few frames after the dark frame's present).
const KEEP: usize = 12;
/// Table sets walked per pair and frame.
const SIGS: usize = 3;
/// Textures kept per pair.
const MOST_TEXTURES: usize = 12;
/// Pairs and textures per pair named in a line.
const NAMED: usize = 16;
const NAMED_TEXTURES: usize = 6;

/// Tiles of the 16x9 grid as bits.
pub(crate) type Mask = [u64; 3];

pub(crate) const ALL: Mask = [u64::MAX, u64::MAX, (1 << (TILES - 128)) - 1];

pub(crate) fn mask_of_tiles(tiles: &[usize]) -> Mask {
    let mut m = [0u64; 3];
    for &t in tiles.iter().filter(|&&t| t < TILES) {
        m[t / 64] |= 1 << (t % 64);
    }
    m
}

fn meets(a: &Mask, b: &Mask) -> bool {
    a.iter().zip(b).any(|(x, y)| x & y != 0)
}

/// Whether a target `w` x `h` is shaped as the presented picture: 16:9
/// within 3%, 320 or more wide.
pub(crate) fn screen_shaped(w: u64, h: u64) -> bool {
    w >= 320 && h > 0 && ((w as f64 / h as f64) / (16.0 / 9.0) - 1.0).abs() < 0.03
}

/// What a draw covers of a target `w` x `h`: its viewport (x, y, width,
/// height) cut by its scissor (left, top, right, bottom), as the share of
/// the target from left/top to right/bottom; `None` if nothing.
pub(crate) fn footprint(
    viewport: Option<[f32; 4]>,
    scissor: Option<[i32; 4]>,
    w: u64,
    h: u64,
) -> Option<[f32; 4]> {
    let (w, h) = (w as f32, h as f32);
    let mut r = [0.0, 0.0, w, h];
    if let Some([x, y, vw, vh]) = viewport {
        r = [r[0].max(x), r[1].max(y), r[2].min(x + vw), r[3].min(y + vh)];
    }
    if let Some([l, t, rr, b]) = scissor {
        r = [
            r[0].max(l as f32),
            r[1].max(t as f32),
            r[2].min(rr as f32),
            r[3].min(b as f32),
        ];
    }
    (r[2] > r[0] && r[3] > r[1]).then(|| [r[0] / w, r[1] / h, r[2] / w, r[3] / h])
}

/// The tiles a footprint ([`footprint`]) touches.
pub(crate) fn mask_of_share(share: [f32; 4]) -> Mask {
    let cell = |v: f32, n: usize| ((v * n as f32).floor().max(0.0) as usize).min(n - 1);
    let (x0, y0) = (cell(share[0], ACROSS), cell(share[1], DOWN));
    // The right and bottom edges are open: a footprint ending on a tile's
    // edge does not touch the tile past it.
    let (x1, y1) = (
        cell((share[2] - 1e-4).max(share[0]), ACROSS),
        cell((share[3] - 1e-4).max(share[1]), DOWN),
    );
    let mut tiles = Vec::new();
    for y in y0..=y1 {
        for x in x0..=x1 {
            tiles.push(y * ACROSS + x);
        }
    }
    mask_of_tiles(&tiles)
}

/// A texture a pair's tables bind.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Texture {
    /// `t` or `u`, and the register.
    pub(crate) kind: u8,
    pub(crate) register: u16,
    /// The title resource (0: a view of memory without one).
    pub(crate) resource: usize,
    pub(crate) memory: u64,
    pub(crate) width: u64,
    pub(crate) height: u32,
    pub(crate) format: u32,
    /// The slot in the view heap, if the table lies in it.
    pub(crate) slot: Option<u32>,
    /// What the host shows in that slot (`made`, `null`, `other_view`, ...).
    pub(crate) host: &'static str,
    /// The view's record: its view number and generation.
    pub(crate) view: (u32, u32),
    /// The frame the catcher first saw the texture bound.
    pub(crate) first: u64,
}

/// A pair's notes in a frame.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Use {
    pub(crate) draws: u32,
    pub(crate) mask: Mask,
    /// Render target 0: memory, width, height (0s for a dispatch).
    pub(crate) target: (u64, u64, u32),
    pub(crate) compute: bool,
    /// Whether a shader of it picks views from the heap by index (those
    /// are not listed).
    pub(crate) heap_indexed: bool,
    pub(crate) textures: Vec<Texture>,
    pub(crate) sigs: Vec<u64>,
}

pub(crate) type Notes = HashMap<(usize, usize), Use>;

static NOW: Mutex<Option<Notes>> = Mutex::new(None);
/// What the title recorded up to its present call (with
/// `VICTORY_FLICKER_CATCH_TITLE_CUT`).
static STAGED: Mutex<Option<Notes>> = Mutex::new(None);
static KEPT: Mutex<VecDeque<(u64, Notes)>> = Mutex::new(VecDeque::new());
/// Textures by resource (or memory): the frame first seen bound.
static FIRST_SEEN: Mutex<Option<HashMap<u64, u64>>> = Mutex::new(None);
/// Pipelines by the frame they were first bound.
static FIRST_BOUND: Mutex<Vec<(u64, usize)>> = Mutex::new(Vec::new());

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// A draw or dispatch of title pipeline `pipeline` into render target `rt`
/// (0: a dispatch) covering `mask`, its tables' set `sig`: whether its
/// textures are to be walked ([`walked`]).
pub(crate) fn note(pipeline: usize, rt: usize, sig: u64, mask: Mask, compute: bool) -> bool {
    let mut now = lock(&NOW);
    let u = now
        .get_or_insert_with(HashMap::new)
        .entry((pipeline, rt))
        .or_default();
    u.draws += 1;
    u.compute = compute;
    for (a, b) in u.mask.iter_mut().zip(mask) {
        *a |= b;
    }
    !u.sigs.contains(&sig) && u.sigs.len() < SIGS
}

/// The frame texture `key` (resource or memory) was first seen bound,
/// noting `frame` if never.
pub(crate) fn first_seen(key: u64, frame: u64) -> u64 {
    let mut seen = lock(&FIRST_SEEN);
    let seen = seen.get_or_insert_with(HashMap::new);
    if seen.len() > 1 << 20 {
        seen.clear();
    }
    *seen.entry(key).or_insert(frame)
}

/// The pair's textures walked for tables' set `sig`; `target`: render
/// target 0 (memory, size); `wide`: the dispatch writes a 16:9 texture (it
/// covers every tile).
pub(crate) fn walked(
    pipeline: usize,
    rt: usize,
    sig: u64,
    target: (u64, u64, u32),
    textures: Vec<Texture>,
    wide: bool,
    heap_indexed: bool,
) {
    let mut now = lock(&NOW);
    let u = now
        .get_or_insert_with(HashMap::new)
        .entry((pipeline, rt))
        .or_default();
    u.sigs.push(sig);
    u.target = target;
    u.heap_indexed |= heap_indexed;
    if wide {
        u.mask = ALL;
    }
    for t in textures {
        if u.textures.len() >= MOST_TEXTURES {
            break;
        }
        if !u
            .textures
            .iter()
            .any(|k| k.kind == t.kind && k.register == t.register && k.memory == t.memory)
        {
            u.textures.push(t);
        }
    }
}

/// Pipeline `pipeline` is bound for the first time (`first bind`).
pub(crate) fn first_bound(pipeline: usize) {
    if on() {
        let mut v = lock(&FIRST_BOUND);
        if v.len() >= 4096 {
            v.drain(..2048);
        }
        v.push((crate::frame::current(), pipeline));
    }
}

/// The title asks for its present ([`super::title_presents`]).
pub(crate) fn title_presents() {
    let taken = lock(&NOW).take().unwrap_or_default();
    let mut staged = lock(&STAGED);
    let s = staged.get_or_insert_with(HashMap::new);
    for (k, u) in taken {
        merge(s.entry(k).or_default(), u);
    }
}

fn merge(into: &mut Use, u: Use) {
    into.draws += u.draws;
    for (a, b) in into.mask.iter_mut().zip(u.mask) {
        *a |= b;
    }
    if into.target == (0, 0, 0) {
        into.target = u.target;
    }
    into.compute |= u.compute;
    into.heap_indexed |= u.heap_indexed;
    for t in u.textures {
        if into.textures.len() < MOST_TEXTURES && !into.textures.contains(&t) {
            into.textures.push(t);
        }
    }
}

/// Frame `frame` is presented ([`super::frame_done`]).
pub(crate) fn frame_done(frame: u64) {
    let notes = match lock(&STAGED).take() {
        Some(mut s) => {
            for (k, u) in lock(&NOW).take().unwrap_or_default() {
                merge(s.entry(k).or_default(), u);
            }
            s
        }
        None => lock(&NOW).take().unwrap_or_default(),
    };
    if notes.is_empty() && !on() {
        return;
    }
    let mut kept = lock(&KEPT);
    kept.push_back((frame, notes));
    while kept.len() > KEEP {
        kept.pop_front();
    }
}

/// Kept notes of frame `frame`.
fn notes_of(frame: u64) -> Option<Notes> {
    lock(&KEPT)
        .iter()
        .find(|(f, _)| *f == frame)
        .map(|(_, n)| n.clone())
}

fn texture_text(t: &Texture, other: Option<&Use>, frame: u64) -> (bool, String) {
    let new = t.first + 1 >= frame;
    let remade = other.is_some_and(|o| {
        o.textures
            .iter()
            .find(|k| k.kind == t.kind && k.register == t.register)
            .is_some_and(|k| k.memory != t.memory || k.view != t.view)
    });
    let bad = t.host != "made" && t.slot.is_some();
    let slot = t.slot.map_or_else(|| "-".to_owned(), |s| s.to_string());
    let flags = format!(
        "{}{}{}",
        if new { " new" } else { "" },
        if remade { " remade" } else { "" },
        if bad { " not_made" } else { "" }
    );
    (
        new || remade || bad,
        format!(
            "{}{} {:#x} {}x{} fmt {} slot {slot} host={} view {}/{} first f{}{flags}",
            t.kind as char,
            t.register,
            t.memory,
            t.width,
            t.height,
            t.format,
            t.host,
            t.view.0,
            t.view.1,
            t.first
        ),
    )
}

/// The region part of a region event's line: frame `frame` dark over
/// `tiles`; `before`/`dark`/`after`: the catcher's summaries of the frame
/// before, the dark one and the one after.
pub(crate) fn line(
    frame: u64,
    tiles: &[usize],
    before: Option<&Summary>,
    dark: Option<&Summary>,
    after: Option<&Summary>,
    name: &dyn Fn(usize) -> String,
) -> String {
    let first_bound: Vec<(u64, usize)> = lock(&FIRST_BOUND)
        .iter()
        .filter(|&&(f, _)| f + 2 >= frame && f <= frame)
        .copied()
        .collect();
    line_of(
        frame,
        tiles,
        [notes_of(frame - 1), notes_of(frame), notes_of(frame + 1)],
        &first_bound,
        [before, dark, after],
        name,
    )
}

/// [`line`] from the notes of the frame before, the dark one and the one
/// after, and the pipelines first bound near it (frame, pipeline).
pub(crate) fn line_of(
    frame: u64,
    tiles: &[usize],
    notes: [Option<Notes>; 3],
    first_bound: &[(u64, usize)],
    summaries: [Option<&Summary>; 3],
    name: &dyn Fn(usize) -> String,
) -> String {
    let [before, dark, after] = summaries;
    let region = mask_of_tiles(tiles);
    let [b, d, a] = notes;
    let into = |n: &Option<Notes>| -> HashMap<(usize, usize), Use> {
        n.iter()
            .flatten()
            .filter(|(_, u)| meets(&u.mask, &region))
            .map(|(k, u)| (*k, u.clone()))
            .collect()
    };
    let (bi, di, ai) = (into(&b), into(&d), into(&a));
    let mut keys: Vec<(usize, usize)> = bi.keys().chain(di.keys()).copied().collect();
    keys.sort_unstable();
    keys.dedup();
    let draws = |m: &HashMap<(usize, usize), Use>, k| m.get(&k).map_or(0, |u| u.draws);
    // Before and not dark first, then fewer than half, then new in the
    // dark frame, then by draws.
    let rank = |&k: &(usize, usize)| {
        let (db, dd) = (draws(&bi, k), draws(&di, k));
        let class = if dd == 0 {
            0
        } else if dd * 2 < db {
            1
        } else if db == 0 {
            2
        } else {
            3
        };
        (class, std::cmp::Reverse(db.max(dd)), k)
    };
    keys.sort_unstable_by_key(rank);
    let missing = keys.iter().filter(|&&k| draws(&di, k) == 0).count();
    let fewer = keys
        .iter()
        .filter(|&&k| {
            let (db, dd) = (draws(&bi, k), draws(&di, k));
            dd > 0 && dd * 2 < db
        })
        .count();
    let extra = keys.iter().filter(|&&k| draws(&bi, k) == 0).count();
    let ran =
        |s: Option<&Summary>, p: usize| s.map_or_else(|| "-".to_owned(), |s| s.of(p).0.to_string());
    let dropped =
        |s: Option<&Summary>, p: usize| s.map_or_else(|| "-".to_owned(), |s| s.of(p).1.to_string());
    let mut flagged = 0usize;
    let shown: Vec<String> = keys
        .iter()
        .take(NAMED)
        .map(|&(p, rt)| {
            let (now, other) = match di.get(&(p, rt)) {
                Some(u) => (u, bi.get(&(p, rt))),
                None => (&bi[&(p, rt)], None),
            };
            let target = if now.compute {
                "cs".to_owned()
            } else {
                format!("rt {:#x} {}x{}", now.target.0, now.target.1, now.target.2)
            };
            let mut texts: Vec<(bool, String)> = now
                .textures
                .iter()
                .map(|t| texture_text(t, other, frame))
                .collect();
            texts.sort_by_key(|(f, _)| !*f);
            let n = texts.len();
            let why = match dark {
                Some(s) if s.of(p).1 > 0 => format!(" ({})", s.why_of(p)),
                _ => String::new(),
            };
            let bound = first_bound
                .iter()
                .find(|&&(_, q)| q == p)
                .map_or_else(String::new, |(f, _)| format!(" first_bind=f{f}"));
            format!(
                "{} {target} draws {}/{}/{} ran {}/{}/{} dropped {}/{}/{}{why}{bound}{} tex {}[{}{}]",
                name(p),
                draws(&di, (p, rt)),
                draws(&bi, (p, rt)),
                draws(&ai, (p, rt)),
                ran(dark, p),
                ran(before, p),
                ran(after, p),
                dropped(dark, p),
                dropped(before, p),
                dropped(after, p),
                if now.heap_indexed { " heap_indexed" } else { "" },
                n,
                texts
                    .iter()
                    .take(NAMED_TEXTURES)
                    .map(|(_, t)| t.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
                if n > NAMED_TEXTURES {
                    format!(", +{} more", n - NAMED_TEXTURES)
                } else {
                    String::new()
                }
            )
        })
        .collect();
    for (k, u) in &di {
        let other = bi.get(k);
        flagged += u
            .textures
            .iter()
            .filter(|t| texture_text(t, other, frame).0)
            .count();
    }
    let bound_text: Vec<String> = first_bound
        .iter()
        .take(NAMED)
        .map(|&(f, p)| format!("{} f{f}", name(p)))
        .collect();
    format!(
        "region pairs {}/{}/{} (dark/before/after) missing={missing} fewer={fewer} extra={extra} textures_flagged={flagged} first_bound={}[{}]{}: {}",
        di.len(),
        bi.len(),
        ai.len(),
        first_bound.len(),
        bound_text.join("; "),
        if d.is_none() || b.is_none() {
            " (frames not kept)"
        } else {
            ""
        },
        shown.join(" | ")
    )
}

#[cfg(test)]
mod tests;
