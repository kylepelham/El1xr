//! `VICTORY_BAKE_CHECK=1|N` (#36/#35, live, diagnostic): the failure made visible at the bake.
//!
//! Batch 5 had a waitress's face black with no `bake why: pending` and no `short>0` line: the
//! counters before it look at the texture queue, not at what the bake read or wrote. With the
//! switch, each decoration bake draw (`crate::draw::BAKE_PIXEL_SHADERS`) is looked at twice:
//! - when it is recorded, every input slot it picks by index (`crate::bake_inputs::slots`): the
//!   view the title slot holds (placed memory, a texture object, none), whether the host slot the
//!   card will read was made from that same view (`current`), from an older one (`stale`), or is
//!   null (`null`), whether `VICTORY_SHOW_AFTER` still holds its new descriptor back in this list
//!   or another (`held_here`, `held_other`), whether the queue still has it due (`due_now`,
//!   `due_later`), and how much of its memory is mapped (`unmapped`, `partly`);
//! - once its list has run, a few rows of texels of each target it drew into, inside the
//!   viewport (copied to a read-back buffer right after the draw): the share of near-black
//!   texels, whether they are all alike, their mean colour.
//!
//! A bake whose output is black, dark or uniform gives a `bake check: ... bake` line with both;
//! so does one in N of the others (`1`: one in 16), for comparison. A bake draw dropped before it
//! ran gives a `dropped` line.
//!
//! Its result is followed too: a texture a copy or resolve writes in the list of a bake draw
//! (the title copies the baked tile into the character's texture) is a bake result; a later
//! write over it from elsewhere gives a `bake check: ... replaced` line naming what wrote it
//! (`copy` from a list with no bake, a non-bake `draw` into it, a `title_upload` of its title
//! memory, an `alias_upload` of another texture over its memory, `zeros` of levels on demand).
//! One `bake check frame` line per frame with any of these counts.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// Live: `0` off, `1` on (one good bake in 16 logged), `N` one in N.
pub(crate) static ON: victory_core::settings::Live =
    victory_core::settings::Live::new("VICTORY_BAKE_CHECK", parse);

fn parse(v: Option<&str>) -> u64 {
    v.and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(0)
}

pub(crate) fn on() -> bool {
    ON.get() > 0
}

/// One good bake in so many gets its line.
fn sample_every() -> u64 {
    match ON.get() {
        0 | 1 => 16,
        n => n,
    }
}

/// Bake draws whose targets are read back per frame at most (the others are counted).
pub(crate) const CHECKS_PER_FRAME: u64 = 48;
/// Rows of texels read back per target, and texels per row at most.
pub(crate) const ROWS: u32 = 8;
pub(crate) const ROW_TEXELS: u32 = 256;
/// Targets read back per bake draw at most.
pub(crate) const TARGETS: usize = 4;
const MAX_LINES: u64 = 20_000;
/// Results followed at most (the oldest go first).
const MAX_RESULTS: usize = 8192;

/// What a title slot holds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum ViewKind {
    /// No view of ours (never written, or its resource released).
    #[default]
    Empty,
    /// A texture placed in title memory.
    Placed,
    /// A texture object (the host's own contents when the host writes it).
    Object,
    /// Any other view.
    Other,
}

/// Whether `VICTORY_SHOW_AFTER` holds the slot's new descriptor back.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Held {
    #[default]
    No,
    /// In the bake draw's own list (shown once that list has run).
    Here,
    /// In another list not yet run (`VICTORY_BAKE_PUBLISH=2` registry).
    Other,
}

/// One input slot of a bake draw as it stood when the draw was recorded.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Slot {
    pub(crate) slot: u32,
    pub(crate) view: ViewKind,
    /// The title slot's view key (0: none).
    pub(crate) title_key: u64,
    /// The view key the host slot was made from (0: null).
    pub(crate) made: u64,
    /// In the queue of textures picked by index: due now (true) or later (false).
    pub(crate) queued: Option<bool>,
    pub(crate) held: Held,
    /// A placed texture's address, the bytes of it mapped and its length.
    pub(crate) address: u64,
    pub(crate) mapped: u64,
    pub(crate) len: u64,
    /// Width, height, mips, format (`crate::bake_why::shape`); a texture object's resource in
    /// `address`.
    pub(crate) shape: [u32; 4],
}

pub(crate) const EMPTY: u16 = 1;
pub(crate) const NULL: u16 = 2;
pub(crate) const STALE: u16 = 4;
pub(crate) const HELD_HERE: u16 = 8;
pub(crate) const HELD_OTHER: u16 = 16;
pub(crate) const UNMAPPED: u16 = 32;
pub(crate) const PARTLY: u16 = 64;
pub(crate) const DUE_NOW: u16 = 128;
pub(crate) const DUE_LATER: u16 = 256;
const FLAG_NAMES: [(u16, &str); 9] = [
    (EMPTY, "empty"),
    (NULL, "null"),
    (STALE, "stale"),
    (HELD_HERE, "held_here"),
    (HELD_OTHER, "held_other"),
    (UNMAPPED, "unmapped"),
    (PARTLY, "partly"),
    (DUE_NOW, "due_now"),
    (DUE_LATER, "due_later"),
];

/// What is wrong with an input slot (0: the card reads the title's current view, made, with
/// all its memory and nothing due).
pub(crate) fn flags(s: &Slot) -> u16 {
    let mut f = 0;
    if s.view == ViewKind::Empty || s.title_key == 0 {
        f |= EMPTY;
    }
    if s.made == 0 {
        f |= NULL;
    } else if s.title_key != 0 && s.made != s.title_key {
        f |= STALE;
    }
    match s.held {
        Held::No => {}
        Held::Here => f |= HELD_HERE,
        Held::Other => f |= HELD_OTHER,
    }
    if s.view == ViewKind::Placed && s.len > 0 {
        if s.mapped == 0 {
            f |= UNMAPPED;
        } else if s.mapped < s.len {
            f |= PARTLY;
        }
    }
    match s.queued {
        Some(true) => f |= DUE_NOW,
        Some(false) => f |= DUE_LATER,
        None => {}
    }
    f
}

/// The card reads zeros through the slot: no view, or a null host slot.
pub(crate) fn reads_black(f: u16) -> bool {
    f & (EMPTY | NULL) != 0
}

/// The card reads something other than the title's current texture (zeros, an older
/// texture, or a texture whose bytes are not all there).
pub(crate) fn bad(f: u16) -> bool {
    f & (EMPTY | NULL | STALE | HELD_HERE | HELD_OTHER | UNMAPPED | PARTLY | DUE_NOW) != 0
}

/// `ok` or the flags joined by `+`.
pub(crate) fn state_words(f: u16) -> String {
    if f == 0 {
        return "ok".into();
    }
    FLAG_NAMES
        .iter()
        .filter(|(bit, _)| f & bit != 0)
        .map(|(_, name)| *name)
        .collect::<Vec<_>>()
        .join("+")
}

/// `slot:state` and what the slot shows, for the line.
pub(crate) fn slot_text(s: &Slot) -> String {
    let f = flags(s);
    let mut t = format!("{}:{}", s.slot, state_words(f));
    match s.view {
        ViewKind::Placed => {
            let [w, h, mips, format] = s.shape;
            t.push_str(&format!("@{:#x} f{format} {w}x{h}m{mips}", s.address));
            if s.len > 0 && s.mapped < s.len {
                t.push_str(&format!(" mapped={}/{}", s.mapped, s.len));
            }
        }
        ViewKind::Object => t.push_str(&format!("@obj{:#x}", s.address)),
        ViewKind::Other => t.push_str("@other"),
        ViewKind::Empty => {}
    }
    t
}

/// Bytes per texel of the target formats read; None for another.
pub(crate) fn texel_bytes(format: u32) -> Option<u32> {
    match format {
        2 => Some(16),
        9..=11 => Some(8),
        23..=26 | 27..=30 | 41 | 87..=88 | 90..=93 => Some(4),
        48 | 49 | 54 | 56 => Some(2),
        60 | 61 => Some(1),
        _ => None,
    }
}

fn half(h: u16) -> u8 {
    unit(crate::screenshot::f32_from_half(h))
}

fn unit(v: f32) -> u8 {
    (if v.is_nan() { 0.0 } else { v.clamp(0.0, 1.0) } * 255.0 + 0.5) as u8
}

/// A small float of `bits` mantissa bits under a 5-bit exponent (R11G11B10).
fn small_float(v: u32, mantissa: u32) -> u8 {
    let m = v & ((1 << mantissa) - 1);
    let e = (v >> mantissa) & 31;
    let f = if e == 0 {
        m as f32 / (1 << mantissa) as f32 * 2f32.powi(-14)
    } else if e == 31 {
        0.0
    } else {
        (1.0 + m as f32 / (1 << mantissa) as f32) * 2f32.powi(e as i32 - 15)
    };
    unit(f)
}

/// One texel of DXGI format `format` as 8-bit RGBA (alpha 255 where there is none).
pub(crate) fn rgba(format: u32, p: &[u8]) -> Option<[u8; 4]> {
    let n = texel_bytes(format)? as usize;
    if p.len() < n {
        return None;
    }
    let w = |i: usize| u32::from_le_bytes([p[i], p[i + 1], p[i + 2], p[i + 3]]);
    let h = |i: usize| u16::from_le_bytes([p[i], p[i + 1]]);
    Some(match format {
        2 => {
            let f = |i: usize| unit(f32::from_bits(w(i)));
            [f(0), f(4), f(8), f(12)]
        }
        9 | 10 => [half(h(0)), half(h(2)), half(h(4)), half(h(6))],
        11 => [p[1], p[3], p[5], p[7]],
        23..=25 => {
            let v = w(0);
            let c = |s: u32| (((v >> s & 0x3ff) * 255 + 511) / 1023) as u8;
            [c(0), c(10), c(20), ((v >> 30) * 85) as u8]
        }
        26 => {
            let v = w(0);
            [
                small_float(v & 0x7ff, 6),
                small_float(v >> 11 & 0x7ff, 6),
                small_float(v >> 22, 5),
                255,
            ]
        }
        27..=30 => [p[0], p[1], p[2], p[3]],
        41 => {
            let g = unit(f32::from_bits(w(0)));
            [g, g, g, 255]
        }
        87 | 90 | 91 => [p[2], p[1], p[0], p[3]],
        88 | 92 | 93 => [p[2], p[1], p[0], 255],
        48 | 49 => [p[0], p[1], 0, 255],
        54 => {
            let g = half(h(0));
            [g, g, g, 255]
        }
        56 => [p[1], p[1], p[1], 255],
        _ => [p[0], p[0], p[0], 255],
    })
}

/// What the texels read back of one target show.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Out {
    pub(crate) texels: u32,
    /// Texels whose largest colour channel is at most [`BLACK`].
    pub(crate) black: u32,
    /// Every texel within [`ALIKE`] of the first, in each channel.
    pub(crate) uniform: bool,
    /// Mean colour and alpha.
    pub(crate) mean: [u8; 4],
}

pub(crate) const BLACK: u8 = 10;
pub(crate) const ALIKE: u8 = 3;
/// A mean luma at most this is dark.
pub(crate) const DARK: u32 = 40;

pub(crate) fn judge(texels: &[[u8; 4]]) -> Out {
    let Some(first) = texels.first() else {
        return Out::default();
    };
    let mut sum = [0u64; 4];
    let mut black = 0;
    let mut uniform = true;
    for t in texels {
        if t[0].max(t[1]).max(t[2]) <= BLACK {
            black += 1;
        }
        uniform &= (0..4).all(|c| t[c].abs_diff(first[c]) <= ALIKE);
        for c in 0..4 {
            sum[c] += u64::from(t[c]);
        }
    }
    let n = texels.len() as u64;
    Out {
        texels: n as u32,
        black,
        uniform,
        mean: sum.map(|s| (s / n) as u8),
    }
}

impl Out {
    pub(crate) fn black_share(&self) -> f32 {
        if self.texels == 0 {
            0.0
        } else {
            self.black as f32 / self.texels as f32
        }
    }

    fn luma(&self) -> u32 {
        let [r, g, b, _] = self.mean.map(u32::from);
        (r * 299 + g * 587 + b * 114) / 1000
    }

    /// `black` (nine texels in ten near-black), `dark` (mean luma at most [`DARK`]), `uniform`
    /// (all alike), or None.
    pub(crate) fn flag(&self) -> Option<&'static str> {
        if self.texels == 0 {
            return None;
        }
        if self.black * 10 >= self.texels * 9 {
            Some("black")
        } else if self.luma() <= DARK {
            Some("dark")
        } else if self.uniform {
            Some("uniform")
        } else {
            None
        }
    }
}

/// The worst flag of a bake's targets (black before dark before uniform).
pub(crate) fn worst(outs: &[Out]) -> Option<&'static str> {
    let rank = |f: &str| match f {
        "black" => 3,
        "dark" => 2,
        _ => 1,
    };
    outs.iter().filter_map(Out::flag).max_by_key(|f| rank(f))
}

/// One read-back target, for its line: index among the draw's targets, resource, size, format,
/// the rectangle read (x, y, w, h) and what it showed.
pub(crate) struct Target {
    pub(crate) index: usize,
    pub(crate) resource: usize,
    pub(crate) size: [u32; 2],
    pub(crate) format: u32,
    pub(crate) rect: [u32; 4],
    pub(crate) out: Option<Out>,
}

pub(crate) fn target_text(t: &Target) -> String {
    let [x, y, w, h] = t.rect;
    let mut s = format!(
        "{}:res={:#x} {}x{} f{} vp={x},{y},{w},{h}",
        t.index, t.resource, t.size[0], t.size[1], t.format
    );
    match &t.out {
        Some(o) => {
            let [r, g, b, a] = o.mean;
            s.push_str(&format!(
                " black={:.2} uniform={} mean={r},{g},{b},{a} n={}",
                o.black_share(),
                u8::from(o.uniform),
                o.texels
            ));
        }
        None => s.push_str(" unread_format"),
    }
    s
}

/// The rows to read inside rectangle `rect` (x, y, w, h) of a `size` target: each as (x, y,
/// texels), [`ROWS`] spread over its height, at most [`ROW_TEXELS`] wide, centred.
pub(crate) fn rows(rect: [u32; 4], size: [u32; 2]) -> Vec<(u32, u32, u32)> {
    let [x, y, w, h] = clip(rect, size);
    if w == 0 || h == 0 {
        return Vec::new();
    }
    let n = ROWS.min(h);
    let texels = w.min(ROW_TEXELS);
    let left = x + (w - texels) / 2;
    (0..n)
        .map(|k| (left, y + (h * (2 * k + 1)) / (2 * n), texels))
        .collect()
}

/// `rect` (x, y, w, h) inside a `size` target; the whole target for an empty one.
pub(crate) fn clip(rect: [u32; 4], size: [u32; 2]) -> [u32; 4] {
    let [x, y, w, h] = rect;
    if w == 0 || h == 0 {
        return [0, 0, size[0], size[1]];
    }
    let x = x.min(size[0]);
    let y = y.min(size[1]);
    [x, y, w.min(size[0] - x), h.min(size[1] - y)]
}

#[derive(Clone, Copy)]
#[repr(usize)]
pub(crate) enum C {
    /// Bake draws recorded.
    Bakes,
    /// Of them, read back.
    Checked,
    /// Not read back: past [`CHECKS_PER_FRAME`], no target, or no read-back buffer.
    Unchecked,
    /// Landed read-backs by worst flag.
    Black,
    Dark,
    Uniform,
    /// Bake draws with an input the card does not read as the title's current texture.
    InputsBad,
    /// Input slots read as zeros (no view or a null host slot), summed.
    InputsBlack,
    /// Input slots held back by `VICTORY_SHOW_AFTER`, summed.
    InputsHeld,
    /// Bake draws whose parameters could not be read.
    Unread,
    /// Bake draws dropped before they ran.
    Dropped,
    /// Textures written in a bake's list (results), and results written over from elsewhere.
    Results,
    Replaced,
}
const N: usize = 13;
const NAMES: [&str; N] = [
    "bakes",
    "checked",
    "unchecked",
    "black",
    "dark",
    "uniform",
    "inputs_bad",
    "inputs_black",
    "inputs_held",
    "unread",
    "dropped",
    "results",
    "replaced",
];
static COUNTS: [AtomicU64; N] = [const { AtomicU64::new(0) }; N];
static FRAME: AtomicU64 = AtomicU64::new(0);
static LINES: AtomicU64 = AtomicU64::new(0);
static EVENT_LINES: AtomicU64 = AtomicU64::new(0);
static GOOD: AtomicU64 = AtomicU64::new(0);
static CHECKED_FRAME: AtomicU64 = AtomicU64::new(u64::MAX);
static CHECKED_IN_FRAME: AtomicU64 = AtomicU64::new(0);
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

pub(crate) fn add(c: C, n: u64) {
    roll();
    COUNTS[c as usize].fetch_add(n, Relaxed);
}

/// The line for a frame with counts, at the first count of the next.
fn roll() {
    let now = crate::frame::current();
    let prev = FRAME.load(Relaxed);
    if prev == now || FRAME.compare_exchange(prev, now, Relaxed, Relaxed).is_err() {
        return;
    }
    let c: [u64; N] = std::array::from_fn(|i| COUNTS[i].swap(0, Relaxed));
    if c.iter().all(|&n| n == 0) || LINES.fetch_add(1, Relaxed) >= MAX_LINES {
        return;
    }
    tracing::info!(frame = prev, counts = %line(&c), "bake check frame");
}

fn line(c: &[u64; N]) -> String {
    NAMES
        .iter()
        .zip(c)
        .map(|(name, n)| format!("{name}={n}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn say() -> bool {
    EVENT_LINES.fetch_add(1, Relaxed) < MAX_LINES
}

/// A bake draw is being recorded at `frame`: its id, and whether its targets are read back
/// (at most [`CHECKS_PER_FRAME`] a frame).
pub(crate) fn begin(frame: u64) -> (u64, bool) {
    add(C::Bakes, 1);
    let id = NEXT_ID.fetch_add(1, Relaxed);
    if CHECKED_FRAME.swap(frame, Relaxed) != frame {
        CHECKED_IN_FRAME.store(0, Relaxed);
    }
    (
        id,
        CHECKED_IN_FRAME.fetch_add(1, Relaxed) < CHECKS_PER_FRAME,
    )
}

/// Counts of a bake draw's inputs.
pub(crate) fn inputs_seen(slots: &[Slot]) {
    let f: Vec<u16> = slots.iter().map(flags).collect();
    if f.iter().any(|&f| bad(f)) {
        add(C::InputsBad, 1);
    }
    add(
        C::InputsBlack,
        f.iter().filter(|&&f| reads_black(f)).count() as u64,
    );
    add(
        C::InputsHeld,
        f.iter()
            .filter(|&&f| f & (HELD_HERE | HELD_OTHER) != 0)
            .count() as u64,
    );
}

/// What a bake draw was, for its lines.
pub(crate) struct Bake {
    pub(crate) id: u64,
    pub(crate) frame: u64,
    pub(crate) prog: &'static str,
    /// None: its parameters could not be read.
    pub(crate) inputs: Option<Vec<Slot>>,
}

fn inputs_text(inputs: &Option<Vec<Slot>>) -> String {
    match inputs {
        None => "unread".into(),
        Some(slots) => slots.iter().map(slot_text).collect::<Vec<_>>().join(" "),
    }
}

/// Whether a landed bake gets its line: flagged, any input read as zeros, or one good in N.
pub(crate) fn tells(flag: Option<&str>, inputs_black: bool, nth_good: u64, every: u64) -> bool {
    flag.is_some() || inputs_black || nth_good.is_multiple_of(every.max(1))
}

/// The bake's list has run: `targets` hold what its read-backs showed.
pub(crate) fn landed(bake: &Bake, targets: &[Target]) {
    let outs: Vec<Out> = targets.iter().filter_map(|t| t.out).collect();
    let flag = worst(&outs);
    match flag {
        Some("black") => add(C::Black, 1),
        Some("dark") => add(C::Dark, 1),
        Some(_) => add(C::Uniform, 1),
        None => {}
    }
    let inputs_black = bake
        .inputs
        .as_ref()
        .is_some_and(|s| s.iter().any(|s| reads_black(flags(s))));
    let nth = if flag.is_none() {
        GOOD.fetch_add(1, Relaxed)
    } else {
        1
    };
    if !tells(flag, inputs_black, nth, sample_every()) || !say() {
        return;
    }
    let landed = crate::frame::current();
    tracing::info!(
        "bake check: frame {} bake {} id={} flag={} landed@{} out=[{}] inputs=[{}] results=[{}]",
        bake.frame,
        bake.prog,
        bake.id,
        flag.unwrap_or("ok"),
        landed,
        targets
            .iter()
            .map(target_text)
            .collect::<Vec<_>>()
            .join(" "),
        inputs_text(&bake.inputs),
        results_of(bake.id)
            .iter()
            .map(|r| format!("{r:#x}"))
            .collect::<Vec<_>>()
            .join(" "),
    );
    outcome(bake.id, flag.unwrap_or("ok"));
}

/// A bake draw recorded without a read-back (past the frame's checks): its line only when an
/// input reads zeros.
pub(crate) fn unchecked(bake: &Bake) {
    add(C::Unchecked, 1);
    let black = bake
        .inputs
        .as_ref()
        .is_some_and(|s| s.iter().any(|s| reads_black(flags(s))));
    if black && say() {
        tracing::info!(
            "bake check: frame {} bake {} id={} flag=unchecked out=[] inputs=[{}]",
            bake.frame,
            bake.prog,
            bake.id,
            inputs_text(&bake.inputs)
        );
    }
}

/// A bake draw was dropped before it ran (its target keeps what it held).
pub(crate) fn dropped(frame: u64, prog: &str, why: &str) {
    add(C::Dropped, 1);
    if say() {
        tracing::info!("bake check: frame {frame} dropped {prog} why={why}");
    }
}

/// A bake draw's last record of where it came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Baked {
    id: u64,
    frame: u64,
    prog: &'static str,
    /// Replacement kinds already told since it was baked (bits of [`KINDS`]).
    told: u8,
}

const KINDS: [&str; 5] = ["copy", "draw", "title_upload", "alias_upload", "zeros"];

#[derive(Default)]
struct Book {
    /// By list: its last bake draw.
    lists: HashMap<usize, Baked>,
    /// By resource: the bake its contents came from, and the order it was entered in.
    results: HashMap<usize, (Baked, u64)>,
    entered: u64,
    /// The worst flag of landed bakes, by id (the newest few thousand).
    outcomes: HashMap<u64, &'static str>,
}

/// The resources of [`Book::results`] (perhaps a few more), for every draw to look in cheaply.
fn result_set() -> &'static std::sync::RwLock<std::collections::HashSet<usize>> {
    static SET: std::sync::OnceLock<std::sync::RwLock<std::collections::HashSet<usize>>> =
        std::sync::OnceLock::new();
    SET.get_or_init(Default::default)
}

fn book() -> std::sync::MutexGuard<'static, Book> {
    static BOOK: std::sync::OnceLock<Mutex<Book>> = std::sync::OnceLock::new();
    BOOK.get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// A bake draw was recorded in `list`.
pub(crate) fn bake_in_list(list: usize, id: u64, frame: u64, prog: &'static str) {
    book().lists.insert(
        list,
        Baked {
            id,
            frame,
            prog,
            told: 0,
        },
    );
}

fn outcome(id: u64, flag: &'static str) {
    let mut b = book();
    if b.outcomes.len() >= MAX_RESULTS {
        let cut = id.saturating_sub(MAX_RESULTS as u64 / 2);
        b.outcomes.retain(|&k, _| k >= cut);
    }
    b.outcomes.insert(id, flag);
}

/// (`VICTORY_FACE_WATCH`) What bake texture `resource` is a result of: `prog@frame flag`.
pub(crate) fn result_of(resource: usize) -> Option<String> {
    if !result_set()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .contains(&resource)
    {
        return None;
    }
    let b = book();
    let (baked, _) = b.results.get(&resource)?;
    let flag = b.outcomes.get(&baked.id).copied().unwrap_or("?");
    Some(format!("{}@{} id={} {flag}", baked.prog, baked.frame, baked.id))
}

fn results_of(id: u64) -> Vec<usize> {
    let b = book();
    let mut r: Vec<usize> = b
        .results
        .iter()
        .filter(|(_, (baked, _))| baked.id == id)
        .map(|(&r, _)| r)
        .collect();
    r.sort_unstable();
    r.truncate(8);
    r
}

/// What [`Book::write`] found.
#[derive(Debug, PartialEq, Eq)]
enum Wrote {
    /// Written in a bake's list in the same or the next frame: a result of that bake.
    Result,
    /// A result written over from elsewhere, first time for this kind since its bake.
    Replaced(Baked),
    Nothing,
}

impl Book {
    /// `resource` is written at `frame` by `kind` (an index of [`KINDS`]), in `list` (0: not a
    /// list's copy).
    fn write(&mut self, list: usize, resource: usize, kind: usize, frame: u64) -> Wrote {
        if kind == 0
            && let Some(&baked) = self.lists.get(&list)
            && frame <= baked.frame + 1
        {
            if self.results.len() >= MAX_RESULTS {
                let cut = self.entered.saturating_sub(MAX_RESULTS as u64 / 2);
                self.results.retain(|_, (_, at)| *at >= cut);
            }
            self.entered += 1;
            self.results.insert(resource, (baked, self.entered));
            return Wrote::Result;
        }
        let Some((baked, _)) = self.results.get_mut(&resource) else {
            return Wrote::Nothing;
        };
        let bit = 1u8 << kind;
        if baked.told & bit != 0 {
            return Wrote::Nothing;
        }
        baked.told |= bit;
        Wrote::Replaced(*baked)
    }
}

/// `resource` is written by `kind` (`copy`, `draw`, `title_upload`, `alias_upload`, `zeros`);
/// `list` for a copy or resolve; `by`: the other texture (the alias, the copy's source).
pub(crate) fn written(list: usize, resource: usize, kind: &str, by: usize) {
    let Some(k) = KINDS.iter().position(|&n| n == kind) else {
        return;
    };
    let frame = crate::frame::current();
    let (wrote, flag) = {
        let mut b = book();
        let wrote = b.write(list, resource, k, frame);
        let flag = match &wrote {
            Wrote::Replaced(baked) => b.outcomes.get(&baked.id).copied(),
            _ => None,
        };
        (wrote, flag)
    };
    match wrote {
        Wrote::Result => {
            add(C::Results, 1);
            #[cfg(windows)]
            if crate::face_watch::on()
                && let Some((at, desc)) = crate::resource::texture_memory(resource as crate::object::This)
                && let Some(layout) = crate::resource::texture_layout(&desc)
            {
                crate::face_watch::note_result(at as u64, layout.size, resource);
            }
            let mut set = result_set().write().unwrap_or_else(|e| e.into_inner());
            set.insert(resource);
            if set.len() > 2 * MAX_RESULTS {
                *set = book().results.keys().copied().collect();
            }
        }
        Wrote::Replaced(baked) => {
            add(C::Replaced, 1);
            if say() {
                tracing::info!(
                    "bake check: frame {frame} replaced res={resource:#x} by={kind} other={by:#x} baked@{} bake {} id={} bake_flag={}",
                    baked.frame,
                    baked.prog,
                    baked.id,
                    flag.unwrap_or("?")
                );
            }
        }
        Wrote::Nothing => {}
    }
}

/// A non-bake draw renders into `targets`: any that is a bake result is written over.
pub(crate) fn drawn(targets: &[usize]) {
    let set = result_set().read().unwrap_or_else(|e| e.into_inner());
    if set.is_empty() {
        return;
    }
    let hits: Vec<usize> = targets.iter().copied().filter(|t| set.contains(t)).collect();
    drop(set);
    for t in hits {
        if t != 0 {
            written(0, t, "draw", 0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn current(slot: u32) -> Slot {
        Slot {
            slot,
            view: ViewKind::Placed,
            title_key: 0x5_0000_0007,
            made: 0x5_0000_0007,
            address: 0x1000,
            mapped: 64,
            len: 64,
            shape: [1024, 1024, 9, 98],
            ..Slot::default()
        }
    }

    #[test]
    fn values() {
        assert_eq!(parse(None), 0);
        assert_eq!(parse(Some("0")), 0);
        assert_eq!(parse(Some("1")), 1);
        assert_eq!(parse(Some(" 8 ")), 8);
        assert_eq!(parse(Some("x")), 0);
    }

    #[test]
    fn the_switch_is_live() {
        assert_eq!(
            on(),
            parse(std::env::var("VICTORY_BAKE_CHECK").ok().as_deref()) > 0
        );
        if std::env::var_os("VICTORY_BAKE_CHECK").is_some() {
            return;
        }
        victory_core::settings::set("VICTORY_BAKE_CHECK", "1").unwrap();
        let (now, every) = (on(), sample_every());
        victory_core::settings::set("VICTORY_BAKE_CHECK", "").unwrap();
        assert!(now && every == 16, "a change takes effect while running");
        assert!(!on(), "and clears back to the launch value");
    }

    #[test]
    fn a_current_made_input_is_ok() {
        let s = current(12);
        assert_eq!(flags(&s), 0);
        assert!(!bad(0));
        assert_eq!(slot_text(&s), "12:ok@0x1000 f98 1024x1024m9");
    }

    /// Batch 5 part B 05_base: the face black with nothing pending or short. A slot whose new
    /// descriptor `VICTORY_SHOW_AFTER` holds back reads its old host slot: null (black) or the
    /// texture it showed before (stale), and the queue no longer has it.
    #[test]
    fn a_held_back_slot_reads_null_or_the_old_texture() {
        let mut s = current(3);
        s.made = 0;
        s.held = Held::Here;
        let f = flags(&s);
        assert_eq!(f, NULL | HELD_HERE);
        assert!(reads_black(f) && bad(f));
        assert_eq!(state_words(f), "null+held_here");
        s.made = 0x4_0000_0007;
        s.held = Held::Other;
        let f = flags(&s);
        assert_eq!(f, STALE | HELD_OTHER);
        assert!(!reads_black(f) && bad(f));
    }

    #[test]
    fn unmapped_partly_and_queued_inputs() {
        let mut s = current(4);
        s.mapped = 0;
        s.made = 0;
        s.queued = Some(true);
        let f = flags(&s);
        assert_eq!(state_words(f), "null+unmapped+due_now");
        assert!(slot_text(&s).ends_with("mapped=0/64"));
        s.mapped = 10;
        s.made = s.title_key;
        s.queued = Some(false);
        assert_eq!(flags(&s), PARTLY | DUE_LATER);
        // due later alone: uploaded, to be looked at again: not bad
        s.mapped = 64;
        assert!(!bad(flags(&s)));
    }

    #[test]
    fn an_empty_slot_reads_zeros() {
        let s = Slot {
            slot: 9,
            ..Slot::default()
        };
        let f = flags(&s);
        assert_eq!(f, EMPTY | NULL);
        assert!(reads_black(f));
        assert_eq!(slot_text(&s), "9:empty+null");
        let o = Slot {
            slot: 5,
            view: ViewKind::Object,
            title_key: 7,
            made: 7,
            address: 0xabc,
            ..Slot::default()
        };
        assert_eq!(slot_text(&o), "5:ok@obj0xabc");
    }

    #[test]
    fn formats_are_read_as_colour() {
        assert_eq!(rgba(28, &[1, 2, 3, 4]), Some([1, 2, 3, 4]));
        assert_eq!(rgba(87, &[1, 2, 3, 4]), Some([3, 2, 1, 4]));
        assert_eq!(rgba(88, &[1, 2, 3, 4]), Some([3, 2, 1, 255]));
        // R10G10B10A2: all ones
        assert_eq!(
            rgba(24, &u32::MAX.to_le_bytes()),
            Some([255, 255, 255, 255])
        );
        // R16G16B16A16_FLOAT 1.0, 0.0, 0.5, 1.0
        let h = [0x3c00u16, 0, 0x3800, 0x3c00];
        let b: Vec<u8> = h.iter().flat_map(|v| v.to_le_bytes()).collect();
        assert_eq!(rgba(10, &b), Some([255, 0, 128, 255]));
        // R11G11B10_FLOAT 1.0 in each: exponent 15, mantissa 0
        let one = (15 << 6) | (15 << 6) << 11 | (15 << 5) << 22;
        assert_eq!(
            rgba(26, &(one as u32).to_le_bytes()),
            Some([255, 255, 255, 255])
        );
        assert_eq!(rgba(61, &[77]), Some([77, 77, 77, 255]));
        assert_eq!(rgba(71, &[0; 16]), None, "block-compressed: not a target");
        assert_eq!(rgba(28, &[1, 2]), None, "short");
    }

    #[test]
    fn outputs_are_judged() {
        let black = vec![[2, 3, 1, 255]; 100];
        let o = judge(&black);
        assert_eq!(o.flag(), Some("black"));
        assert_eq!(o.black_share(), 1.0);
        // a few lit texels in a black tile: still black
        let mut mostly = black.clone();
        mostly[..9].fill([200, 120, 150, 255]);
        assert_eq!(judge(&mostly).flag(), Some("black"));
        // a dark charcoal shirt instead of pink
        let charcoal = vec![[30, 28, 32, 255], [24, 22, 26, 255]];
        let o = judge(&charcoal);
        assert_eq!((o.flag(), o.uniform), (Some("dark"), false));
        let pink = vec![[220, 150, 170, 255]; 4];
        let o = judge(&pink);
        assert_eq!(o.flag(), Some("uniform"));
        assert_eq!(o.mean, [220, 150, 170, 255]);
        let lit = vec![[220, 150, 170, 255], [90, 60, 40, 255]];
        assert_eq!(judge(&lit).flag(), None);
        assert_eq!(judge(&[]).flag(), None);
        assert_eq!(
            worst(&[judge(&pink), judge(&black), judge(&charcoal)]),
            Some("black")
        );
        assert_eq!(worst(&[judge(&lit)]), None);
    }

    #[test]
    fn rows_lie_inside_the_viewport() {
        // the bake's 256x256 corner of a 2048x2048 target
        let r = rows([256, 512, 256, 256], [2048, 2048]);
        assert_eq!(r.len(), ROWS as usize);
        assert_eq!(r[0], (256, 512 + 16, 256));
        assert_eq!(r[7], (256, 512 + 240, 256));
        // wider than a row: centred
        assert_eq!(rows([0, 0, 1024, 4], [2048, 2048])[0], (384, 0, 256));
        // fewer lines than rows
        assert_eq!(rows([0, 0, 8, 2], [8, 8]).len(), 2);
        // past the edge: clipped; no viewport: the whole target
        assert_eq!(clip([2000, 0, 256, 256], [2048, 2048]), [2000, 0, 48, 256]);
        assert_eq!(clip([0, 0, 0, 0], [64, 32]), [0, 0, 64, 32]);
        assert!(rows([4096, 0, 16, 16], [2048, 2048]).is_empty());
    }

    #[test]
    fn a_landed_bake_is_told_when_flagged_or_one_good_in_n() {
        assert!(tells(Some("black"), false, 5, 16));
        assert!(tells(None, true, 5, 16), "an input read as zeros");
        assert!(!tells(None, false, 5, 16));
        assert!(tells(None, false, 32, 16));
        assert!(tells(None, false, 3, 0), "never divides by zero");
    }

    #[test]
    fn results_are_what_a_bakes_list_copies_and_later_writes_replace_them() {
        let mut b = Book::default();
        let (list, other, tex) = (0x10, 0x20, 0x900);
        b.lists.insert(
            list,
            Baked {
                id: 7,
                frame: 100,
                prog: "c0a7fe77b1862e10",
                told: 0,
            },
        );
        assert_eq!(
            b.write(other, tex, 0, 100),
            Wrote::Nothing,
            "not a result yet"
        );
        assert_eq!(b.write(list, tex, 0, 101), Wrote::Result);
        // a copy from a list without a bake, later
        match b.write(other, tex, 0, 400) {
            Wrote::Replaced(baked) => assert_eq!((baked.id, baked.frame), (7, 100)),
            w => panic!("{w:?}"),
        }
        assert_eq!(
            b.write(other, tex, 0, 401),
            Wrote::Nothing,
            "told once per kind"
        );
        assert!(
            matches!(b.write(0, tex, 2, 402), Wrote::Replaced(_)),
            "a title upload"
        );
        // the bake's list two frames later: no longer this bake's
        assert!(matches!(b.write(list, tex, 3, 402), Wrote::Replaced(_)));
        assert_eq!(b.write(list, 0x901, 0, 102), Wrote::Nothing);
        // baked again: told again
        b.lists.get_mut(&list).unwrap().frame = 500;
        assert_eq!(b.write(list, tex, 0, 500), Wrote::Result);
        assert!(
            matches!(b.write(0, tex, 1, 501), Wrote::Replaced(_)),
            "drawn over"
        );
    }

    #[test]
    fn the_frame_line_names_every_count() {
        let mut c = [0u64; N];
        c[C::Bakes as usize] = 3;
        c[C::Black as usize] = 1;
        assert_eq!(
            line(&c),
            "bakes=3 checked=0 unchecked=0 black=1 dark=0 uniform=0 inputs_bad=0 inputs_black=0 inputs_held=0 unread=0 dropped=0 results=0 replaced=0"
        );
    }
}
