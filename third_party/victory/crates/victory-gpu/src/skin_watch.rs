//! `VICTORY_SKIN_WATCH=1|N` (diagnostic, live): what the passes that light skin did this frame.
//!
//! Batch 5 part P: skin black at f995, f1066-1108, f1359-1437 and f1908 with every texture of
//! the character material draws `ok` (`crate::face_watch`), eyes and clothes lit. The black
//! comes after the material draw. In this title skin is lit by:
//! - `CS_GenerateMaterialMasks` writing each pixel's material class into the depth plane of a
//!   D16S8 texture (through its colour twin, `crate::resource::Twin`), which a pass of ours draws
//!   into the plane; `CS_MaterialTilesInitial`/`CS_SetMaterialTileIndirectArgs` count the tiles
//!   of each class;
//! - the tiled lighting draws of the skin class (`TL_PS_*PBR_MATERIAL_SHEEN_ANISOTROPY_SSS*`,
//!   indirect rectangles, depth test "equal" against that plane, no stencil), which also read
//!   the screen probes' atlas and layers (`crate::skin_fill`);
//! - the screen-space scattering chain `CS_SSSTileClassification`,
//!   `CS_SSSPrefilterHalf_MaterialTiles`, `CS_SSSDilateHalf_MipMaps`, then `PS_SSSResolve`
//!   (indirect rectangles, depth "equal", additive).
//!
//! Per frame one line says, per pass, how many ran and how many were dropped (and why); for the
//! skin lighting draws the depth target (title and host format, colour twin), the depth
//! function, stencil and whether the rectangles are completed by the geometry stage. The first
//! two skin lighting draws and the first two resolve draws of a frame have a few rows of their
//! first target read back before and after: how many texels the draw changed and how many of
//! those it left black (none changed: the draw lit nothing, as when the depth test fails or the
//! tile count is 0; all changed black: it ran on black inputs). The first skin lighting draw also
//! has its probe inputs read: the share of all-zero texels. `N` reads back every Nth frame only.

use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

pub(crate) static ON: victory_core::settings::Live =
    victory_core::settings::Live::new("VICTORY_SKIN_WATCH", parse);

fn parse(v: Option<&str>) -> u64 {
    v.and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(0)
}

pub(crate) fn on() -> bool {
    ON.get() > 0
}

/// Whether this frame's read-backs are taken (every frame for `1`, every Nth for `N`).
pub(crate) fn reads_this_frame(frame: u64) -> bool {
    let every = ON.get();
    every > 0 && frame.is_multiple_of(every)
}

/// The skin lighting draws (a pixel stage `crate::skin_fill::skin_program` names).
pub(crate) const LIGHTING: &str = "skin_light";
/// The screen-space resolve draws.
pub(crate) const RESOLVE: &str = "sss_resolve";

/// Program names (as their containers spell them) of the passes watched, and their short name.
const PASSES: [(&str, &str); 10] = [
    ("CS_GenerateMaterialMasks", "material_masks"),
    ("CS_MaterialTilesInitial", "material_tiles"),
    ("CS_SetMaterialTileIndirectArgs", "tile_args"),
    ("CS_SSSTileClassification", "sss_tiles"),
    ("CS_SSSPrefilter", "sss_prefilter"),
    ("CS_SSSDilate", "sss_dilate"),
    ("CS_SSSResolve", "sss_cs_resolve"),
    ("CS_SSSCopyHistory", "sss_copy_history"),
    ("SSSHistoryClear", "sss_history_clear"),
    ("PS_SSSResolve", RESOLVE),
];

/// The short name of the watched pass program `name` is, if it is one.
pub(crate) fn pass_of(name: &str) -> Option<&'static str> {
    PASSES
        .iter()
        .find(|(p, _)| name.contains(p))
        .map(|&(_, short)| short)
}

/// Short names by shader hash, each program looked up once.
static KINDS: Mutex<Option<HashMap<String, Option<&'static str>>>> = Mutex::new(None);

/// The watched pass shader `hash` is part of, `name` giving its program name when first asked.
pub(crate) fn kind(hash: &str, name: impl FnOnce() -> String) -> Option<&'static str> {
    let mut kinds = KINDS.lock().unwrap_or_else(|e| e.into_inner());
    let kinds = kinds.get_or_insert_with(HashMap::new);
    if let Some(&k) = kinds.get(hash) {
        return k;
    }
    let k = pass_of(&name());
    kinds.insert(hash.to_owned(), k);
    k
}

/// One pass this frame.
#[derive(Default, Clone, Debug, PartialEq)]
pub(crate) struct Pass {
    pub(crate) ran: u32,
    pub(crate) dropped: u32,
    /// The first reason a drop gave.
    pub(crate) why: &'static str,
    /// What the first draw that ran was bound with (depth target, state).
    pub(crate) bound: Option<String>,
    /// Other bindings draws of the pass ran with this frame, and how many draws each (at most
    /// [`OTHER_BINDINGS`]): batch 5 R2/S logs showed 13-16 frames a run whose first skin
    /// lighting draw tested a 16-bit depth without a twin, drawn as triangles, beside the six
    /// tiled draws against the material plane.
    pub(crate) others: Vec<(String, u32)>,
}

/// Bindings besides the first a frame line names per pass.
pub(crate) const OTHER_BINDINGS: usize = 4;

struct Book {
    frame: u64,
    passes: BTreeMap<&'static str, Pass>,
}

static BOOK: Mutex<Book> = Mutex::new(Book {
    frame: u64::MAX,
    passes: BTreeMap::new(),
});

/// Read-backs taken this frame, by kind.
static TAKEN: Mutex<(u64, Vec<&'static str>)> = Mutex::new((u64::MAX, Vec::new()));
/// Output read-backs a frame at most: every skin lighting draw (6 a frame in the diner: the
/// plain, no-sun, wetness and glints variants each light other tiles), the first resolve draws.
fn outputs_a_frame(kind: &str) -> usize {
    if kind == LIGHTING { 8 } else { 4 }
}
/// Rows read of a target, spread over its height, each its whole width (at most
/// [`ROW_TEXELS`]): batch 5 part R2 read 8 rows of the middle 256 texels and saw skin in 63
/// frames of 1,147.
pub(crate) const ROWS: u32 = 8;
pub(crate) const ROW_TEXELS: u32 = 4096;
/// Slices of a volume read (the light-range volume has 13).
pub(crate) const SLICES: u32 = 16;

/// What the tiled lighting's light-range volume says of the tiles read: each texel holds the
/// first light index (low 16 bits) and the last (high 16 bits) of its tile's lights of one
/// kind, `0xffffffff` for none.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Ranges {
    /// Tiles without lights.
    pub(crate) none: u32,
    /// The most lights one tile loops over.
    pub(crate) widest: u32,
    /// Ranges whose last light comes before the first: not written by the light loop.
    pub(crate) bad: u32,
}

pub(crate) fn light_ranges(bytes: &[u8]) -> Ranges {
    let mut r = Ranges::default();
    for t in bytes.chunks_exact(4) {
        let v = u32::from_le_bytes([t[0], t[1], t[2], t[3]]);
        if v == u32::MAX {
            r.none += 1;
            continue;
        }
        let (first, last) = (v & 0xffff, v >> 16);
        if last < first {
            r.bad += 1;
        } else {
            r.widest = r.widest.max(last - first + 1);
        }
    }
    r
}

/// The rows to read of a `size` target inside `rect` (x, y, w, h; all of it when empty): each
/// as (x, y, texels).
pub(crate) fn rows(rect: [u32; 4], size: [u32; 2]) -> Vec<(u32, u32, u32)> {
    let [x, y, w, h] = crate::bake_check::clip(rect, size);
    if w == 0 || h == 0 {
        return Vec::new();
    }
    let n = ROWS.min(h);
    let texels = w.min(ROW_TEXELS);
    (0..n)
        .map(|k| (x, y + (h * (2 * k + 1)) / (2 * n), texels))
        .collect()
}
/// Read-back lines in all at most.
const MAX_LINES: u64 = 20_000;
static LINES: AtomicU64 = AtomicU64::new(0);

/// Whether a read-back of `kind` is taken now in `frame` (it counts as taken): its index
/// among those of its kind this frame.
pub(crate) fn take(frame: u64, kind: &'static str, most: usize) -> Option<usize> {
    if !reads_this_frame(frame) || LINES.load(Relaxed) >= MAX_LINES {
        return None;
    }
    let mut t = TAKEN.lock().unwrap_or_else(|e| e.into_inner());
    if t.0 != frame {
        *t = (frame, Vec::new());
    }
    let taken = t.1.iter().filter(|&&k| k == kind).count();
    if taken >= most {
        return None;
    }
    t.1.push(kind);
    Some(taken)
}

/// Output read-backs of `kind` a frame: the draw's index among them.
pub(crate) fn take_output(frame: u64, kind: &'static str) -> Option<usize> {
    take(frame, kind, outputs_a_frame(kind))
}

/// A draw or dispatch of pass `kind` ran (`ran`) or was dropped for `why`, in `frame`; `bound`
/// says what it was bound with (asked for the first draw of a frame that ran).
pub(crate) fn note(
    frame: u64,
    kind: &'static str,
    ran: bool,
    why: &'static str,
    bound: impl FnOnce() -> Option<String>,
) {
    let mut b = BOOK.lock().unwrap_or_else(|e| e.into_inner());
    if b.frame != frame {
        if b.frame != u64::MAX && !b.passes.is_empty() {
            tracing::info!(frame = b.frame, passes = %line(&b.passes), "skin watch frame");
        }
        b.frame = frame;
        b.passes.clear();
    }
    let p = b.passes.entry(kind).or_default();
    if ran {
        p.ran += 1;
        if let Some(seen) = bound() {
            note_binding(p, seen);
        }
    } else {
        p.dropped += 1;
        if p.why.is_empty() {
            p.why = if why.is_empty() { "unclassified" } else { why };
        }
    }
}

/// A draw of the pass ran bound with `seen`: the first binding, or counted among the others.
fn note_binding(p: &mut Pass, seen: String) {
    match &p.bound {
        None => p.bound = Some(seen),
        Some(first) if *first == seen => {}
        Some(_) => match p.others.iter().position(|(b, _)| *b == seen) {
            Some(i) => p.others[i].1 += 1,
            None if p.others.len() < OTHER_BINDINGS => p.others.push((seen, 1)),
            None => {}
        },
    }
}

/// The frame line's text: `name=ran/dropped[why]{bound}{other n=draws}...` per pass.
pub(crate) fn line(passes: &BTreeMap<&'static str, Pass>) -> String {
    passes
        .iter()
        .map(|(name, p)| {
            let mut s = format!("{name}={}/{}", p.ran, p.dropped);
            if p.dropped > 0 {
                s.push_str(&format!("[{}]", p.why));
            }
            if let Some(b) = &p.bound {
                s.push_str(&format!("{{{b}}}"));
            }
            for (b, n) in &p.others {
                s.push_str(&format!("{{{b} n={n}}}"));
            }
            s
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The depth function and stencil switch of a pipeline's state summary
/// (`depth true write 0 func 3; stencil false ...`): `func=3 stencil=false`.
pub(crate) fn depth_state(state: &str) -> String {
    let func = state
        .split("; ")
        .next()
        .and_then(|d| d.split_once("func ").map(|(_, f)| f))
        .and_then(|f| f.split_whitespace().next())
        .unwrap_or("?");
    let stencil = state
        .split("; ")
        .find_map(|p| p.strip_prefix("stencil "))
        .and_then(|s| s.split_whitespace().next())
        .unwrap_or("?");
    format!("func={func} stencil={stencil}")
}

/// What a draw changed in the rows read before and after it: texels compared, changed, changed
/// to black, black after. Texels of `texel` bytes; `black` judges one texel's bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Change {
    pub(crate) texels: u32,
    pub(crate) changed: u32,
    pub(crate) changed_black: u32,
    pub(crate) black_after: u32,
}

pub(crate) fn change(
    before: &[u8],
    after: &[u8],
    texel: usize,
    black: impl Fn(&[u8]) -> bool,
) -> Change {
    let mut c = Change::default();
    if texel == 0 {
        return c;
    }
    for (b, a) in before.chunks_exact(texel).zip(after.chunks_exact(texel)) {
        c.texels += 1;
        let dark = black(a);
        c.black_after += u32::from(dark);
        if a != b {
            c.changed += 1;
            c.changed_black += u32::from(dark);
        }
    }
    c
}

/// A texel is black when its colour channels read as 8-bit are at most
/// [`crate::bake_check::BLACK`]; in a format the check cannot read, when every byte is 0.
pub(crate) fn black_texel(format: u32, p: &[u8]) -> bool {
    match crate::bake_check::rgba(format, p) {
        Some([r, g, b, _]) => r.max(g).max(b) <= crate::bake_check::BLACK,
        None => p.iter().all(|&b| b == 0),
    }
}

/// The mean colour (8-bit channels) of the texels a draw changed, in a format
/// `crate::bake_check::rgba` reads: a face lit in one channel (batch 5 part Q, solid red)
/// shows as `r` far above `g` and `b`. None when no changed texel could be read.
pub(crate) fn changed_rgb(before: &[u8], after: &[u8], texel: usize, format: u32) -> Option<[u8; 3]> {
    if texel == 0 {
        return None;
    }
    let (mut sum, mut n) = ([0u64; 3], 0u64);
    for (b, a) in before.chunks_exact(texel).zip(after.chunks_exact(texel)) {
        if a != b
            && let Some([r, g, bl, _]) = crate::bake_check::rgba(format, a)
        {
            for (s, c) in sum.iter_mut().zip([r, g, bl]) {
                *s += u64::from(c);
            }
            n += 1;
        }
    }
    (n > 0).then(|| sum.map(|s| (s / n) as u8))
}

/// Texels of `texel` bytes in `bytes`, and how many of them are all zero.
pub(crate) fn zeros(bytes: &[u8], texel: usize) -> (u32, u32) {
    if texel == 0 {
        return (0, 0);
    }
    bytes.chunks_exact(texel).fold((0, 0), |(n, z), t| {
        (n + 1, z + u32::from(t.iter().all(|&b| b == 0)))
    })
}

/// One output read-back: draw `index` of `kind` in `frame`, pixel shader `pixel`, whether it
/// blends (adds) into its target.
#[derive(Clone, Debug)]
pub(crate) struct Output {
    pub(crate) frame: u64,
    pub(crate) kind: &'static str,
    pub(crate) index: usize,
    pub(crate) pixel: String,
    pub(crate) blended: bool,
}

/// Verdicts by (kind, draw index, pixel shader, verdict) since the last summary, and the frame
/// span they cover.
type Tally = BTreeMap<(&'static str, usize, String, &'static str), u64>;
static TALLY: Mutex<(u64, Tally)> = Mutex::new((u64::MAX, BTreeMap::new()));
/// Frames between `skin watch: outputs` summaries.
const SUMMARY_EVERY: u64 = 300;

/// An output read-back landed: what the draw changed in the rows of its first target. A line
/// for a draw that changed something; the rest counted in a summary every [`SUMMARY_EVERY`]
/// frames.
pub(crate) fn output_landed(o: &Output, target: &str, c: Change, rgb: Option<[u8; 3]>) {
    let verdict = verdict(c, o.blended);
    if let Some(summary) = tally(o, verdict) {
        tracing::info!(counts = %summary, "skin watch: outputs");
    }
    if c.changed == 0 {
        return;
    }
    LINES.fetch_add(1, Relaxed);
    let changed_rgb = rgb.map_or_else(|| "-".to_owned(), |[r, g, b]| format!("{r}/{g}/{b}"));
    tracing::info!(
        changed_rgb,
        frame = o.frame,
        kind = o.kind,
        draw = o.index,
        pixel = %o.pixel,
        blended = o.blended,
        target,
        texels = c.texels,
        changed = c.changed,
        changed_black = c.changed_black,
        black_after = c.black_after,
        verdict,
        "skin watch: output"
    );
}

/// Counts `verdict` for `o`; the summary of what was counted when a new span of
/// [`SUMMARY_EVERY`] frames starts.
fn tally(o: &Output, verdict: &'static str) -> Option<String> {
    let mut t = TALLY.lock().unwrap_or_else(|e| e.into_inner());
    let span = o.frame / SUMMARY_EVERY;
    let summary = (t.0 != span && t.0 != u64::MAX && !t.1.is_empty()).then(|| summary(&t.1));
    if t.0 != span {
        *t = (span, BTreeMap::new());
    }
    *t.1.entry((o.kind, o.index, o.pixel.clone(), verdict)).or_default() += 1;
    summary
}

/// `kind#index/pixel:verdict=count` per entry.
pub(crate) fn summary(t: &Tally) -> String {
    t.iter()
        .map(|((kind, index, pixel, verdict), n)| format!("{kind}#{index}/{pixel}:{verdict}={n}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// `lit_nothing` (no texel changed: the draw covered none of the rows read), `wrote_black`
/// (every changed texel black), `added_dark` (a draw that adds into its target, every changed
/// texel still black: it added little to a dark texel, as in shade or a fade; batch 5 part R2's
/// three resolve `wrote_black` at f680, f1533, f1535), `lit`, or `unread`.
pub(crate) fn verdict(c: Change, blended: bool) -> &'static str {
    if c.texels == 0 {
        "unread"
    } else if c.changed == 0 {
        "lit_nothing"
    } else if c.changed_black == c.changed {
        if blended { "added_dark" } else { "wrote_black" }
    } else {
        "lit"
    }
}

/// An input read-back landed: of the first skin lighting draw of `frame`, its probe `input`
/// (`atlas` or `layers`): format, size, texels read and those all zero; or why it was not read
/// (`no_table`, `no_view`, `no_copy`, `unreadable`).
pub(crate) fn input_landed(frame: u64, input: &str, shape: &str, (texels, zero): (u32, u32)) {
    LINES.fetch_add(1, Relaxed);
    tracing::info!(frame, input, shape, texels, zero, "skin watch: probe input");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passes_are_named_by_program() {
        assert_eq!(
            pass_of("CS_SSSTileClassification_be29fabb_Wrapped"),
            Some("sss_tiles")
        );
        assert_eq!(pass_of("PS_SSSResolve_44ece319_Wrapped"), Some(RESOLVE));
        assert_eq!(
            pass_of("CS_SSSResolve_MaterialClassification_ea1dbf97"),
            Some("sss_cs_resolve")
        );
        assert_eq!(
            pass_of("CS_SSSPrefilterHalf_MaterialTiles_fd35c341"),
            Some("sss_prefilter")
        );
        assert_eq!(
            pass_of("CS_GenerateMaterialMasks_5305082e_Wrapped"),
            Some("material_masks")
        );
        assert_eq!(
            pass_of("TL_PS_PBR_MATERIAL_SHEEN_ANISOTROPY_SSS_42ad1e50"),
            None
        );
        assert_eq!(pass_of("CS_PedStandardSkinning_3"), None);
        assert_eq!(
            kind("00000000000000aa", || "CS_SSSDilateHalf_MipMaps".to_owned()),
            Some("sss_dilate")
        );
        assert_eq!(
            kind("00000000000000aa", || unreachable!()),
            Some("sss_dilate")
        );
    }

    /// The frame line names drops with their reason and the first bound state.
    #[test]
    fn the_frame_line_counts_runs_and_drops() {
        let mut passes = BTreeMap::new();
        passes.insert(
            LIGHTING,
            Pass {
                ran: 4,
                dropped: 1,
                why: "MissingHostPipeline",
                bound: Some("depth=f44 twin=true func=3 stencil=false".to_owned()),
                others: Vec::new(),
            },
        );
        passes.insert(
            "sss_tiles",
            Pass {
                ran: 1,
                ..Pass::default()
            },
        );
        assert_eq!(
            line(&passes),
            "skin_light=4/1[MissingHostPipeline]{depth=f44 twin=true func=3 stencil=false} sss_tiles=1/0"
        );
    }

    /// A pass whose draws ran with other bindings than the first names each, with its draws
    /// (batch 5 R2/S: one skin lighting draw against a 16-bit depth before six tiled ones).
    #[test]
    fn other_bindings_are_named_with_their_draws() {
        let mut p = Pass::default();
        let odd = "depth=f53 twin=false func=3 stencil=false rects=no ps=cdeb13e0";
        let tiled = "depth=f44 twin=true func=3 stencil=false rects=gs ps=677ea27c";
        note_binding(&mut p, odd.to_owned());
        for _ in 0..6 {
            note_binding(&mut p, tiled.to_owned());
        }
        note_binding(&mut p, odd.to_owned());
        assert_eq!(p.bound.as_deref(), Some(odd));
        assert_eq!(p.others, vec![(tiled.to_owned(), 6)]);
        for k in 0..2 * OTHER_BINDINGS {
            note_binding(&mut p, format!("depth=f{k}"));
        }
        assert_eq!(p.others.len(), OTHER_BINDINGS);
        p.ran = 7;
        let mut passes = BTreeMap::new();
        p.others.truncate(1);
        passes.insert(LIGHTING, p);
        assert_eq!(
            line(&passes),
            format!("skin_light=7/0{{{odd}}}{{{tiled} n=6}}")
        );
    }

    #[test]
    fn depth_state_is_read_from_the_summary() {
        let s = "depth true write 0 func 3; stencil false read 0xff write 0x0 front [fail 1 depth-fail 1 pass 1 func 8] back [fail 1 depth-fail 1 pass 1 func 8]; cull 1 front-ccw true; blend false src 2 dst 1 op 1 mask 0xf";
        assert_eq!(depth_state(s), "func=3 stencil=false");
        let b = "depth true write 1 func 7 bounds true; stencil true read 0xff write 0xff front []";
        assert_eq!(depth_state(b), "func=7 stencil=true");
        assert_eq!(depth_state(""), "func=? stencil=?");
    }

    /// A draw that changed nothing lit nothing; one whose changes are all black wrote black.
    #[test]
    fn a_draw_is_judged_by_what_it_changed() {
        let black = |p: &[u8]| p.iter().all(|&b| b == 0);
        let before = [1u8, 1, 0, 0, 5, 5];
        let c = change(&before, &before, 2, black);
        assert_eq!(
            c,
            Change {
                texels: 3,
                changed: 0,
                changed_black: 0,
                black_after: 1
            }
        );
        assert_eq!(verdict(c, false), "lit_nothing");
        let after = [0u8, 0, 0, 0, 5, 5];
        let c = change(&before, &after, 2, black);
        assert_eq!((c.changed, c.changed_black, c.black_after), (1, 1, 2));
        assert_eq!(verdict(c, false), "wrote_black");
        assert_eq!(verdict(c, true), "added_dark", "an additive draw cannot darken");
        let lit = [9u8, 9, 7, 7, 5, 5];
        assert_eq!(verdict(change(&before, &lit, 2, black), true), "lit");
        assert_eq!(verdict(Change::default(), false), "unread");
    }

    /// R8G8B8A8_UNORM (28): the mean of the changed texels only.
    #[test]
    fn the_changed_texels_mean_colour_is_read() {
        let before = [0u8, 0, 0, 255, 9, 9, 9, 255, 1, 2, 3, 255];
        let after = [200u8, 10, 20, 255, 9, 9, 9, 255, 100, 30, 0, 255];
        assert_eq!(changed_rgb(&before, &after, 4, 28), Some([150, 20, 10]));
        assert_eq!(changed_rgb(&before, &before, 4, 28), None);
        assert_eq!(changed_rgb(&before, &after, 0, 28), None);
    }

    /// No lights, one tile with lights 3 to 7, and a range no light loop writes.
    #[test]
    fn light_ranges_are_read() {
        let mut bytes = Vec::new();
        for v in [u32::MAX, 3 | 7 << 16, 9 | 2 << 16, 0] {
            bytes.extend_from_slice(&v.to_le_bytes());
        }
        assert_eq!(light_ranges(&bytes), Ranges { none: 1, widest: 5, bad: 1 });
    }

    /// Whole rows, spread over the height.
    #[test]
    fn rows_span_the_target_width() {
        assert_eq!(
            rows([0; 4], [2560, 1440]),
            (0..8).map(|k| (0, 90 + 180 * k, 2560)).collect::<Vec<_>>()
        );
        assert_eq!(rows([0, 0, 320, 4], [320, 180]).len(), 4);
        assert_eq!(rows([0; 4], [8192, 8]).first(), Some(&(0, 0, ROW_TEXELS)));
    }

    /// The summary counts verdicts per draw and comes out when a new span of frames starts.
    #[test]
    fn verdicts_are_summed_per_span() {
        let o = |frame, index| Output {
            frame,
            kind: LIGHTING,
            index,
            pixel: "677ea27c05f1a634".to_owned(),
            blended: false,
        };
        let base = 9_000 * SUMMARY_EVERY;
        assert_eq!(tally(&o(base, 0), "lit_nothing"), None);
        assert_eq!(tally(&o(base + 1, 0), "lit_nothing"), None);
        assert_eq!(tally(&o(base + 1, 2), "lit"), None);
        assert_eq!(
            tally(&o(base + SUMMARY_EVERY, 0), "lit").as_deref(),
            Some("skin_light#0/677ea27c05f1a634:lit_nothing=2 skin_light#2/677ea27c05f1a634:lit=1")
        );
    }

    #[test]
    fn black_texels_and_zero_inputs_are_counted() {
        // R16G16B16A16_FLOAT: half 0.0 is black, half 1.0 (0x3c00) is not.
        assert!(black_texel(10, &[0, 0, 0, 0, 0, 0, 0, 0x3c]));
        assert!(!black_texel(10, &[0, 0x3c, 0, 0, 0, 0, 0, 0x3c]));
        // R32_UINT (R9G9B9E5 in the atlas): read raw.
        assert!(black_texel(42, &[0, 0, 0, 0]));
        assert!(!black_texel(42, &[1, 0, 0, 0]));
        assert_eq!(zeros(&[0, 0, 0, 0, 1, 0, 0, 0], 4), (2, 1));
        assert_eq!(zeros(&[1, 2], 0), (0, 0));
    }

    /// Read-backs: at most 8 skin lighting and 4 resolve outputs a frame, each with its index;
    /// none on frames `N` skips.
    #[test]
    fn read_backs_are_capped_a_frame() {
        let _ = victory_core::settings::check_live("VICTORY_SKIN_WATCH", "1", on, false, true);
        victory_core::settings::check_live(
            "VICTORY_SKIN_WATCH",
            "1",
            || {
                let lit: Vec<_> = (0..9).map(|_| take_output(7_000_001, LIGHTING)).collect();
                let resolve: Vec<_> = (0..5).map(|_| take_output(7_000_001, RESOLVE)).collect();
                (lit[0], lit[7], lit[8], resolve[3], resolve[4])
            },
            (None, None, None, None, None),
            (Some(0), Some(7), None, Some(3), None),
        );
        victory_core::settings::check_live(
            "VICTORY_SKIN_WATCH",
            "2",
            || (reads_this_frame(4), reads_this_frame(5)),
            (false, false),
            (true, false),
        );
    }
}
