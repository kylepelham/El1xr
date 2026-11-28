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
}

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
/// Output read-backs per kind a frame at most.
const OUTPUTS_A_FRAME: usize = 2;
/// Read-back lines in all at most.
const MAX_LINES: u64 = 20_000;
static LINES: AtomicU64 = AtomicU64::new(0);

/// Whether a read-back of `kind` is taken now in `frame` (it counts as taken).
pub(crate) fn take(frame: u64, kind: &'static str, most: usize) -> bool {
    if !reads_this_frame(frame) || LINES.load(Relaxed) >= MAX_LINES {
        return false;
    }
    let mut t = TAKEN.lock().unwrap_or_else(|e| e.into_inner());
    if t.0 != frame {
        *t = (frame, Vec::new());
    }
    if t.1.iter().filter(|&&k| k == kind).count() >= most {
        return false;
    }
    t.1.push(kind);
    true
}

/// Output read-backs of `kind` a frame.
pub(crate) fn take_output(frame: u64, kind: &'static str) -> bool {
    take(frame, kind, OUTPUTS_A_FRAME)
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
        if p.bound.is_none() {
            p.bound = bound();
        }
    } else {
        p.dropped += 1;
        if p.why.is_empty() {
            p.why = if why.is_empty() { "unclassified" } else { why };
        }
    }
}

/// The frame line's text: `name=ran/dropped[why]{bound}` per pass.
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

/// Texels of `texel` bytes in `bytes`, and how many of them are all zero.
pub(crate) fn zeros(bytes: &[u8], texel: usize) -> (u32, u32) {
    if texel == 0 {
        return (0, 0);
    }
    bytes.chunks_exact(texel).fold((0, 0), |(n, z), t| {
        (n + 1, z + u32::from(t.iter().all(|&b| b == 0)))
    })
}

/// An output read-back landed: what draw `kind` (pixel shader `pixel`) of `frame` changed in
/// the rows of its first target.
pub(crate) fn output_landed(frame: u64, kind: &str, pixel: &str, target: &str, c: Change) {
    LINES.fetch_add(1, Relaxed);
    tracing::info!(
        frame,
        kind,
        pixel,
        target,
        texels = c.texels,
        changed = c.changed,
        changed_black = c.changed_black,
        black_after = c.black_after,
        verdict = verdict(c),
        "skin watch: output"
    );
}

/// `lit_nothing` (no texel changed), `wrote_black` (every changed texel black), `lit`, or
/// `unread`.
pub(crate) fn verdict(c: Change) -> &'static str {
    if c.texels == 0 {
        "unread"
    } else if c.changed == 0 {
        "lit_nothing"
    } else if c.changed_black == c.changed {
        "wrote_black"
    } else {
        "lit"
    }
}

/// An input read-back landed: of the first skin lighting draw of `frame`, its probe `input`
/// (`atlas` or `layers`): format, size, texels read and those all zero.
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
        assert_eq!(verdict(c), "lit_nothing");
        let after = [0u8, 0, 0, 0, 5, 5];
        let c = change(&before, &after, 2, black);
        assert_eq!((c.changed, c.changed_black, c.black_after), (1, 1, 2));
        assert_eq!(verdict(c), "wrote_black");
        let lit = [9u8, 9, 7, 7, 5, 5];
        assert_eq!(verdict(change(&before, &lit, 2, black)), "lit");
        assert_eq!(verdict(Change::default()), "unread");
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

    /// Read-backs: at most two outputs of a kind a frame, none on frames `N` skips.
    #[test]
    fn read_backs_are_capped_a_frame() {
        let _ = victory_core::settings::check_live("VICTORY_SKIN_WATCH", "1", on, false, true);
        victory_core::settings::check_live(
            "VICTORY_SKIN_WATCH",
            "1",
            || {
                (
                    take_output(7_000_001, LIGHTING),
                    take_output(7_000_001, LIGHTING),
                    take_output(7_000_001, LIGHTING),
                    take_output(7_000_001, RESOLVE),
                )
            },
            (false, false, false, false),
            (true, true, false, true),
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
