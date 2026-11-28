//! `VICTORY_TILE_ARGS_WATCH=1|N` (diagnostic, live, off by default): for every skin lighting
//! draw (`crate::skin_watch::LIGHTING`, the tiled `TL_PS_*SSS*` indirect rectangles), the indirect
//! arguments as the card reads them, and the material-class plane the draw tests "equal" against.
//!
//! Batch 5 part R2/S: the skin lighting draws 677ea27c05f1a634 and 5a53c4e2b9144bea often change
//! no texel (`skin watch: output ... lit_nothing`). The class depth test itself is exact on the
//! RTX 5090 (`resource/class_depth_tests.rs`), and the host pipelines never set a depth bias (the
//! title's packet is refused when its bias words are not zero, `victory-shader/src/native/pieces.rs`
//! `G_DEPTH_BIAS`; `D3D12_RASTERIZER_DESC` keeps 0/0/0). Left: a draw with no tiles (its record,
//! written by `CS_SetMaterialTileIndirectArgs`, read as zero, by the title or by our path), or a
//! plane not drawn (or drawn before its twin was written again) when the draw ran.
//!
//! Per draw (every Nth frame for `N`), right before its `ExecuteIndirect`, a copy of ours takes
//! the first record of the buffer the card reads (the title's copy raw, our clamp's scratch, or the
//! list's ahead slot), the record in the title's host copy, and the count; once the list has run a
//! `tile args watch:` line gives them with what title memory held when the draw was recorded, the
//! verdict (`ok`, `zero_written`: the title's copy holds no work either, `zeroed_by_us`: the title's
//! copy has work our path did not pass on, `count_zero`), where the record's copy was written by
//! shaders, the last plane pass and twin write of the bound depth's class plane (this list or
//! which, frames ago, and whether the twin was written after the last plane: `stale=yes`), the last
//! `CS_GenerateMaterialMasks`/`CS_SetMaterialTileIndirectArgs` dispatch, the viewport's depth range
//! and the pipeline's depth bias. A summary of verdicts every 300 frames.

use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

pub(crate) static ON: victory_core::settings::Live =
    victory_core::settings::Live::new("VICTORY_TILE_ARGS_WATCH", parse);

fn parse(v: Option<&str>) -> u64 {
    v.and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(0)
}

pub(crate) fn on() -> bool {
    ON.get() > 0
}

/// Whether `frame`'s draws are watched (every frame for `1`, every Nth for `N`).
pub(crate) fn this_frame(frame: u64) -> bool {
    let every = ON.get();
    every > 0 && frame.is_multiple_of(every)
}

/// One recording event: its order among all events noted, the list (`crate::dred::Ops::id`)
/// and the frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Event {
    pub(crate) seq: u64,
    pub(crate) list: u64,
    pub(crate) frame: u64,
}

static SEQ: AtomicU64 = AtomicU64::new(1);

fn event(list: u64) -> Event {
    Event {
        seq: SEQ.fetch_add(1, Relaxed),
        list,
        frame: crate::frame::current(),
    }
}

/// Per colour twin (by its address): the last plane pass recorded, the last write of the twin.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct TwinLog {
    pub(crate) plane: Option<Event>,
    pub(crate) written: Option<Event>,
}

static TWINS: Mutex<Option<HashMap<usize, TwinLog>>> = Mutex::new(None);

fn with_twin(twin: usize, f: impl FnOnce(&mut TwinLog)) {
    let mut t = TWINS.lock().unwrap_or_else(|e| e.into_inner());
    f(t.get_or_insert_with(HashMap::new).entry(twin).or_default());
}

/// `crate::host::HostList::pass_plane`: list `list` recorded the pass that draws twin `twin`
/// into its depth plane.
pub(crate) fn plane_drawn(twin: usize, list: u64) {
    if on() {
        let e = event(list);
        with_twin(twin, |t| t.plane = Some(e));
    }
}

/// `crate::host::HostList::twins_written`: a draw or dispatch of list `list` writes twin `twin`.
pub(crate) fn twin_written(twin: usize, list: u64) {
    if on() {
        let e = event(list);
        with_twin(twin, |t| t.written = Some(e));
    }
}

fn twin_log(twin: usize) -> TwinLog {
    TWINS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .and_then(|t| t.get(&twin).copied())
        .unwrap_or_default()
}

/// The passes before the skin lighting draws whose last dispatch is told.
const PRODUCERS: [&str; 2] = ["material_masks", "tile_args"];
static LAST: Mutex<[Option<Event>; 2]> = Mutex::new([None; 2]);

/// A dispatch of pass `kind` (`crate::skin_watch::pass_of`) ran in list `list`.
pub(crate) fn producer(kind: &str, list: u64) {
    if !on() {
        return;
    }
    if let Some(i) = PRODUCERS.iter().position(|&p| p == kind) {
        let e = event(list);
        LAST.lock().unwrap_or_else(|e| e.into_inner())[i] = Some(e);
    }
}

/// Where event `e` happened from a draw in list `list`, frame `frame`: `never`, `here` (this
/// list) or `#list`, with the frames since (`here/f-0`).
pub(crate) fn place(e: Option<Event>, list: u64, frame: u64) -> String {
    match e {
        None => "never".to_owned(),
        Some(e) => {
            let at = if e.list == list {
                "here".to_owned()
            } else {
                format!("#{}", e.list)
            };
            format!("{at}/f-{}", frame.saturating_sub(e.frame))
        }
    }
}

/// The plane part of a line: `plane=<place> twin_write=<place> stale=yes|no`, or `plane=none`
/// when the depth has no class plane. Stale: the twin was written after the last plane pass was
/// recorded (or no pass was recorded), so the plane the draw tests may be the one before.
pub(crate) fn plane_text(log: Option<TwinLog>, list: u64, frame: u64) -> String {
    let Some(t) = log else {
        return "plane=none".to_owned();
    };
    let stale = match (t.plane, t.written) {
        (_, None) => false,
        (None, Some(_)) => true,
        (Some(p), Some(w)) => w.seq > p.seq,
    };
    format!(
        "plane={} twin_write={} stale={}",
        place(t.plane, list, frame),
        place(t.written, list, frame),
        if stale { "yes" } else { "no" }
    )
}

/// The plane part of a line for the bound depth's twin (`None`: no twin with a plane).
pub(crate) fn plane_at(twin: Option<usize>, list: u64, frame: u64) -> String {
    plane_text(twin.map(twin_log), list, frame)
}

/// The producers part of a line: `masks=<place> tile_args=<place>`.
pub(crate) fn producers_at(list: u64, frame: u64) -> String {
    let last = *LAST.lock().unwrap_or_else(|e| e.into_inner());
    format!(
        "masks={} tile_args={}",
        place(last[0], list, frame),
        place(last[1], list, frame)
    )
}

/// The work a draw record of `kind` (0 draw, 1 indexed) asks for: its vertices or indices times
/// its instances.
pub(crate) fn work(w: &[u32; 5]) -> u64 {
    u64::from(w[0]) * u64::from(w[1])
}

/// The first record as the card read it (`read`), as the title's host copy held it (`title`,
/// `None` when that is the buffer read), and the count (`None` without a count buffer).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Words {
    pub(crate) read: [u32; 5],
    pub(crate) title: Option<[u32; 5]>,
    pub(crate) count: Option<u32>,
}

/// `count_zero` (a count buffer said 0), `ok` (the record read has work), `zeroed_by_us` (the
/// title's copy has work the buffer read lacks: our clamp or its budget emptied it), or
/// `zero_written` (no work in the title's copy either: the title's pass wrote none, or what it
/// wrote did not reach the copy).
pub(crate) fn verdict(w: &Words) -> &'static str {
    if w.count == Some(0) {
        "count_zero"
    } else if work(&w.read) > 0 {
        "ok"
    } else if w.title.is_some_and(|t| work(&t) > 0) {
        "zeroed_by_us"
    } else {
        "zero_written"
    }
}

/// Little-endian words of a read-back: record read at 0, title's record at [`TITLE_AT`], count
/// at [`COUNT_AT`].
pub(crate) const TITLE_AT: usize = 32;
pub(crate) const COUNT_AT: usize = 56;
pub(crate) const BYTES: u64 = 64;

pub(crate) fn words_of(bytes: &[u8], title: bool, count: bool) -> Words {
    let word = |at: usize| {
        bytes
            .get(at..at + 4)
            .map_or(0, |b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    };
    let record = |at: usize| std::array::from_fn(|i| word(at + 4 * i));
    Words {
        read: record(0),
        title: title.then(|| record(TITLE_AT)),
        count: count.then(|| word(COUNT_AT)),
    }
}

/// What is known of a draw when it is recorded.
#[derive(Clone, Debug, Default)]
pub(crate) struct Draw {
    pub(crate) frame: u64,
    pub(crate) list: u64,
    pub(crate) index: u32,
    pub(crate) pixel: String,
    pub(crate) path: &'static str,
    pub(crate) kind: u32,
    pub(crate) max: u32,
    pub(crate) at: u64,
    /// Title memory's first record words when the draw was recorded.
    pub(crate) cpu: [u32; 5],
    /// The rest of the line (copy, plane, producers, viewport, bias).
    pub(crate) rest: String,
}

/// Skin lighting draws noted this frame (the draw index).
static DRAWS: Mutex<(u64, u32)> = Mutex::new((u64::MAX, 0));

/// The index of the next skin lighting draw of `frame`.
pub(crate) fn next_index(frame: u64) -> u32 {
    let mut d = DRAWS.lock().unwrap_or_else(|e| e.into_inner());
    if d.0 != frame {
        *d = (frame, 0);
    }
    d.1 += 1;
    d.1 - 1
}

fn record_text(w: &[u32; 5], kind: u32) -> String {
    let n = if kind == 1 { 5 } else { 4 };
    w[..n]
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(" ")
}

/// The line's text after the frame, list and draw.
pub(crate) fn line(d: &Draw, w: &Words) -> String {
    let title = w.title.map_or_else(
        || "same".to_owned(),
        |t| format!("[{}]", record_text(&t, d.kind)),
    );
    let count = w
        .count
        .map_or_else(|| "-".to_owned(), |c| format!("{c}/{}", d.max));
    format!(
        "ps={} path={} kind={} at={:#x} read=[{}] title={title} cpu=[{}] count={count} verdict={} {}",
        d.pixel,
        d.path,
        if d.kind == 1 { "indexed" } else { "draw" },
        d.at,
        record_text(&w.read, d.kind),
        record_text(&d.cpu, d.kind),
        verdict(w),
        d.rest
    )
}

/// Lines in all at most.
const MAX_LINES: u64 = 20_000;
static LINES: AtomicU64 = AtomicU64::new(0);
/// Frames between `tile args watch: verdicts` summaries.
const SUMMARY_EVERY: u64 = 300;
type Tally = BTreeMap<(String, &'static str), u64>;
static TALLY: Mutex<(u64, Tally)> = Mutex::new((u64::MAX, BTreeMap::new()));

/// Counts `verdict` for `pixel` in `frame`; the summary of the span before when a new span starts.
fn tally(frame: u64, pixel: &str, verdict: &'static str) -> Option<String> {
    let mut t = TALLY.lock().unwrap_or_else(|e| e.into_inner());
    let span = frame / SUMMARY_EVERY;
    let summary = (t.0 != span && t.0 != u64::MAX && !t.1.is_empty()).then(|| {
        t.1.iter()
            .map(|((p, v), n)| format!("{p}:{v}={n}"))
            .collect::<Vec<_>>()
            .join(" ")
    });
    if t.0 != span {
        *t = (span, BTreeMap::new());
    }
    *t.1.entry((pixel.to_owned(), verdict)).or_default() += 1;
    summary
}

/// A draw's read-back landed (the list has run).
pub(crate) fn landed(d: &Draw, w: &Words) {
    let v = verdict(w);
    if let Some(counts) = tally(d.frame, &d.pixel, v) {
        tracing::info!(counts = %counts, "tile args watch: verdicts");
    }
    if LINES.fetch_add(1, Relaxed) < MAX_LINES {
        tracing::info!(
            "tile args watch: frame={} list=#{} draw={} {}",
            d.frame,
            d.list,
            d.index,
            line(d, w)
        );
    }
}

/// The depth bias part of a pipeline's state summary (`... front-ccw true bias 0 slope 0
/// clamp 0 clip true; ...`): `bias=0/0/0`, `bias=?` when it does not say.
pub(crate) fn bias_of(state: &str) -> String {
    let word = |key: &str| {
        state
            .split_whitespace()
            .skip_while(|w| *w != key)
            .nth(1)
            .map(|v| v.trim_end_matches(';').to_owned())
    };
    match (word("bias"), word("slope"), word("clamp")) {
        (Some(b), Some(s), Some(c)) => format!("bias={b}/{s}/{c}"),
        _ => "bias=?".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verdicts_name_who_left_the_record_empty() {
        let w = |read: [u32; 5], title: Option<[u32; 5]>, count| Words { read, title, count };
        assert_eq!(verdict(&w([3, 24, 0, 0, 0], None, None)), "ok");
        assert_eq!(verdict(&w([3, 0, 0, 0, 0], None, None)), "zero_written");
        assert_eq!(
            verdict(&w([0; 5], Some([3, 12, 0, 0, 0]), None)),
            "zeroed_by_us"
        );
        assert_eq!(
            verdict(&w([0; 5], Some([3, 0, 0, 0, 0]), None)),
            "zero_written"
        );
        assert_eq!(verdict(&w([3, 24, 0, 0, 0], None, Some(0))), "count_zero");
        assert_eq!(verdict(&w([3, 24, 0, 0, 0], None, Some(1))), "ok");
    }

    #[test]
    fn read_backs_are_split_into_records() {
        let mut bytes = vec![0u8; BYTES as usize];
        for (i, v) in [3u32, 40, 0, 7].iter().enumerate() {
            bytes[4 * i..4 * i + 4].copy_from_slice(&v.to_le_bytes());
        }
        bytes[TITLE_AT..TITLE_AT + 4].copy_from_slice(&3u32.to_le_bytes());
        bytes[TITLE_AT + 4..TITLE_AT + 8].copy_from_slice(&41u32.to_le_bytes());
        bytes[COUNT_AT..COUNT_AT + 4].copy_from_slice(&1u32.to_le_bytes());
        let w = words_of(&bytes, true, true);
        assert_eq!(w.read, [3, 40, 0, 7, 0]);
        assert_eq!(w.title, Some([3, 41, 0, 0, 0]));
        assert_eq!(w.count, Some(1));
        assert_eq!(words_of(&bytes, false, false).title, None);
        assert_eq!(words_of(&bytes[..8], false, false).read, [3, 40, 0, 0, 0]);
    }

    #[test]
    fn places_say_list_and_frames_ago() {
        let e = |seq, list, frame| Some(Event { seq, list, frame });
        assert_eq!(place(None, 4, 10), "never");
        assert_eq!(place(e(1, 4, 10), 4, 10), "here/f-0");
        assert_eq!(place(e(1, 3, 9), 4, 10), "#3/f-1");
    }

    /// The twin written after the last plane pass (or with none) is stale; written before is not.
    #[test]
    fn a_twin_written_after_its_plane_is_stale() {
        let e = |seq, list| {
            Some(Event {
                seq,
                list,
                frame: 100,
            })
        };
        let t = |plane, written| Some(TwinLog { plane, written });
        assert_eq!(plane_text(None, 1, 100), "plane=none");
        assert_eq!(
            plane_text(t(e(5, 1), e(4, 1)), 1, 100),
            "plane=here/f-0 twin_write=here/f-0 stale=no"
        );
        assert_eq!(
            plane_text(t(e(5, 2), e(6, 2)), 1, 101),
            "plane=#2/f-1 twin_write=#2/f-1 stale=yes"
        );
        assert_eq!(
            plane_text(t(None, e(6, 2)), 1, 100),
            "plane=never twin_write=#2/f-0 stale=yes"
        );
        assert_eq!(
            plane_text(t(e(5, 2), None), 1, 100),
            "plane=#2/f-0 twin_write=never stale=no"
        );
    }

    /// The hooks record events only with the switch on, in order.
    #[test]
    fn hooks_record_with_the_switch_on() {
        // A fresh twin and list each call: what an earlier call noted stays noted.
        static CALL: AtomicU64 = AtomicU64::new(0);
        victory_core::settings::check_live(
            "VICTORY_TILE_ARGS_WATCH",
            "1",
            || {
                let n = CALL.fetch_add(1, Relaxed);
                let (twin, list) = (0x5eed_0000_usize + n as usize, 7_000_000 + n);
                twin_written(twin, list);
                plane_drawn(twin, list);
                producer("tile_args", list);
                let t = twin_log(twin);
                (
                    t.written.is_some_and(|e| e.list == list),
                    t.plane
                        .is_some_and(|p| t.written.is_some_and(|w| p.seq > w.seq)),
                    producers_at(list, crate::frame::current()).contains("tile_args=here/f-0"),
                )
            },
            (false, false, false),
            (true, true, true),
        );
    }

    #[test]
    fn a_line_tells_records_and_verdict() {
        let d = Draw {
            frame: 1066,
            list: 9,
            index: 0,
            pixel: "677ea27c".to_owned(),
            path: "ahead",
            kind: 0,
            max: 1,
            at: 0x1000,
            cpu: [0; 5],
            rest: "plane=here/f-0".to_owned(),
        };
        let w = Words {
            read: [0; 5],
            title: Some([3, 12, 0, 0, 0]),
            count: None,
        };
        assert_eq!(
            line(&d, &w),
            "ps=677ea27c path=ahead kind=draw at=0x1000 read=[0 0 0 0] title=[3 12 0 0] cpu=[0 0 0 0] count=- verdict=zeroed_by_us plane=here/f-0"
        );
    }

    #[test]
    fn summaries_count_verdicts_per_span() {
        let base = 7_000 * SUMMARY_EVERY;
        assert_eq!(tally(base, "677ea27c", "ok"), None);
        assert_eq!(tally(base + 1, "677ea27c", "zero_written"), None);
        assert_eq!(
            tally(base + SUMMARY_EVERY, "5a53c4e2", "ok").as_deref(),
            Some("677ea27c:ok=1 677ea27c:zero_written=1")
        );
    }

    #[test]
    fn the_bias_is_read_from_the_state() {
        let s = "depth true write 0 func 3; stencil false read 0xff write 0x0 front [] back []; cull 1 front-ccw true bias 0 slope 0 clamp 0 clip true; blend false src 2 dst 1 op 1 mask 0xf";
        assert_eq!(bias_of(s), "bias=0/0/0");
        assert_eq!(
            bias_of("depth true; cull 1 front-ccw true; blend false"),
            "bias=?"
        );
    }
}
