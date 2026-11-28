//! `VICTORY_BAKE_ZERO_WAIT=<ms>` (#35, live): a decoration bake draw whose input texture's
//! memory still reads all zeros is held until the title writes it, at most `<ms>`.
//!
//! Part I flick: of the bakes `bake check` flagged black, d126bcae 27 of 33 had every input `ok`
//! (the slot shows the view the title wrote, made from its memory as it is). The title had mapped
//! that memory but not streamed the texture's bytes into it yet: the bake read zeros from a
//! current texture, and neither `VICTORY_BAKE_FRESH` nor `VICTORY_BAKE_REPLAY` saw anything wrong.
//!
//! With the switch, before a bake draw (after its own pass, `VICTORY_BAKE_INPUTS=1`), each input
//! texture placed in mapped memory is sampled: [`SAMPLES`] runs of [`SAMPLE_BYTES`] spread over its
//! bytes from the first mip shown. One reading zeros in every run holds the bake: the memory is
//! looked at every ms until each such input has a non-zero byte or the bound passes
//! (at most [`FRAME_TIMES`] times `<ms>` per frame). The inputs that filled are made due and
//! uploaded alone before the draw. An input still zero at the bound is remembered for
//! [`KNOWN_FRAMES`] frames (a texture that is black on purpose holds no later bake).
//!
//! With `VICTORY_BAKE_REPLAY` (and `VICTORY_BAKE_FRESH`) a bake drawn on zeros (held to the bound,
//! or no wait left this frame) is kept as an incomplete one, and drawn again only once none of
//! its inputs reads zeros and each is current.
//!
//! One `bake zero wait: frame` line per frame with a zero input: bakes, inputs checked, zero
//! inputs, held, drawn (held, then drawn on filled inputs), expired (drawn on zeros at the bound),
//! no_time, known, refreshed, ms, and held/drawn/expired/skipped per bake program. The first
//! [`MAX_BAKE_LINES`] held bakes get a `bake zero wait: bake` line each.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// Live: read at every bake draw.
pub(crate) static WAIT: victory_core::settings::Live =
    victory_core::settings::Live::new("VICTORY_BAKE_ZERO_WAIT", parse);

/// The longest a bake is held, whatever the value.
const MAX_MS: u64 = 1_000;
/// A frame's bake draws are held this many times the value at most, in all.
pub(crate) const FRAME_TIMES: u64 = 4;
/// Runs read of each input.
pub(crate) const SAMPLES: u64 = 8;
/// Bytes of each run.
pub(crate) const SAMPLE_BYTES: u64 = 512;
/// Frames an input found zero at the bound holds no bake.
pub(crate) const KNOWN_FRAMES: u64 = 600;
/// Inputs remembered as zero at most.
const MAX_KNOWN: usize = 4_096;
/// `bake zero wait: bake` lines at most.
pub(crate) const MAX_BAKE_LINES: u64 = 256;
/// Frame lines at most.
const MAX_LINES: u64 = 20_000;

fn parse(v: Option<&str>) -> u64 {
    v.and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(0)
        .min(MAX_MS)
}

/// The bound per bake draw in ms (0: off).
pub(crate) fn wait_ms() -> u64 {
    WAIT.get()
}

/// The runs read of a texture of `len` bytes: offset and length of each. A small texture is read
/// whole; a larger one in [`SAMPLES`] runs from its first byte to its last, 16-byte aligned.
pub(crate) fn sample_spans(len: u64) -> Vec<(u64, u64)> {
    if len == 0 {
        return Vec::new();
    }
    if len <= SAMPLES * SAMPLE_BYTES {
        return vec![(0, len)];
    }
    let last = len - SAMPLE_BYTES;
    (0..SAMPLES)
        .map(|i| {
            let at = if i + 1 == SAMPLES {
                last
            } else {
                (last * i / (SAMPLES - 1)) & !15
            };
            (at, SAMPLE_BYTES)
        })
        .collect()
}

/// Whether every byte is zero.
pub(crate) fn all_zero(bytes: &[u8]) -> bool {
    // SAFETY: any eight bytes are a valid u64.
    let (head, words, tail) = unsafe { bytes.align_to::<u64>() };
    head.iter().all(|&b| b == 0) && words.iter().all(|&w| w == 0) && tail.iter().all(|&b| b == 0)
}

/// Whether a texture of `len` bytes reads zeros in every run: `read(offset, len)` says whether
/// that run is all zeros (None: it cannot be read, nor then the texture).
pub(crate) fn reads_zero(len: u64, mut read: impl FnMut(u64, u64) -> Option<bool>) -> Option<bool> {
    let spans = sample_spans(len);
    if spans.is_empty() {
        return None;
    }
    for (at, n) in spans {
        if !read(at, n)? {
            return Some(false);
        }
    }
    Some(true)
}

static WAIT_FRAME: AtomicU64 = AtomicU64::new(u64::MAX);
static WAIT_SPENT: AtomicU64 = AtomicU64::new(0);

/// How long this bake draw may still be held in `frame`, given `ms` per draw.
pub(crate) fn wait_left(frame: u64, ms: u64) -> u64 {
    if WAIT_FRAME.swap(frame, Relaxed) != frame {
        WAIT_SPENT.store(0, Relaxed);
    }
    ms.min((ms * FRAME_TIMES).saturating_sub(WAIT_SPENT.load(Relaxed)))
}

/// `ms` of this frame's holding were spent.
pub(crate) fn waited(ms: u64) {
    WAIT_SPENT.fetch_add(ms, Relaxed);
}

static KNOWN: Mutex<Option<HashMap<(u64, u64), u64>>> = Mutex::new(None);

/// Whether the input at `address` (`len` bytes) was found zero at the bound within
/// [`KNOWN_FRAMES`] of `frame`.
pub(crate) fn known(address: u64, len: u64, frame: u64) -> bool {
    let k = KNOWN.lock().unwrap_or_else(|e| e.into_inner());
    k.as_ref()
        .and_then(|k| k.get(&(address, len)))
        .is_some_and(|&at| frame < at.saturating_add(KNOWN_FRAMES))
}

/// The input at `address` (`len` bytes) still read zeros at the bound in `frame`.
pub(crate) fn expired(address: u64, len: u64, frame: u64) {
    let mut k = KNOWN.lock().unwrap_or_else(|e| e.into_inner());
    let k = k.get_or_insert_with(HashMap::new);
    if k.len() >= MAX_KNOWN {
        k.retain(|_, &mut at| frame < at.saturating_add(KNOWN_FRAMES));
        if k.len() >= MAX_KNOWN {
            k.clear();
        }
    }
    k.insert((address, len), frame);
}

/// What became of a bake draw.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// No input read zeros.
    Clean,
    /// Held, and every input that read zeros (and was not known) filled: drawn on them.
    Drawn,
    /// Held to the bound with an input still reading zeros: drawn on it.
    Expired,
    /// An input read zeros and the frame's holding was spent: drawn on it.
    NoTime,
    /// Every input reading zeros was found zero at the bound before: drawn without holding.
    Known,
}

impl Outcome {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Outcome::Clean => "clean",
            Outcome::Drawn => "drawn",
            Outcome::Expired => "expired",
            Outcome::NoTime => "no_time",
            Outcome::Known => "known",
        }
    }

    /// Kept by `VICTORY_BAKE_REPLAY` as an incomplete bake: it read zeros that may still fill.
    pub(crate) fn replays(self) -> bool {
        matches!(self, Outcome::Expired | Outcome::NoTime)
    }

    pub(crate) fn held(self) -> bool {
        matches!(self, Outcome::Drawn | Outcome::Expired)
    }
}

/// What the zeros held a bake draw for: its outcome given whether every zero input is known
/// (`all_known`), the ms left to hold it this frame (`left`), and, once held, whether every
/// unknown zero input filled (`filled`, asked only when held).
pub(crate) fn decide(
    zero: usize,
    all_known: bool,
    left: u64,
    filled: impl FnOnce(u64) -> bool,
) -> Outcome {
    if zero == 0 {
        Outcome::Clean
    } else if all_known {
        Outcome::Known
    } else if left == 0 {
        Outcome::NoTime
    } else if filled(left) {
        Outcome::Drawn
    } else {
        Outcome::Expired
    }
}

/// A bake's counts for its frame line.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub(crate) struct Seen {
    pub(crate) checked: u64,
    pub(crate) zero: u64,
    pub(crate) refreshed: u64,
    pub(crate) ms: u64,
}

#[derive(Default)]
struct Frame {
    frame: u64,
    bakes: u64,
    checked: u64,
    zero: u64,
    refreshed: u64,
    ms: u64,
    /// held, drawn, expired, no_time, known.
    outcomes: [u64; 5],
    /// Per bake program: held, drawn, expired, skipped (no_time or known).
    by_bake: Vec<(&'static str, [u64; 4])>,
}

static FRAME: Mutex<Option<Frame>> = Mutex::new(None);
static LINES: AtomicU64 = AtomicU64::new(0);
static BAKE_LINES: AtomicU64 = AtomicU64::new(0);

fn frame_line(f: &Frame) -> String {
    let by: Vec<String> = f
        .by_bake
        .iter()
        .map(|(prog, [h, d, e, s])| format!("{prog}:{h}/{d}/{e}/{s}"))
        .collect();
    format!(
        "bake zero wait: frame {} bakes={} checked={} zero={} held={} drawn={} expired={} no_time={} known={} refreshed={} ms={} by_bake(held/drawn/expired/skipped)=[{}]",
        f.frame,
        f.bakes,
        f.checked,
        f.zero,
        f.outcomes[0],
        f.outcomes[1],
        f.outcomes[2],
        f.outcomes[3],
        f.outcomes[4],
        f.refreshed,
        f.ms,
        by.join(" ")
    )
}

fn add_to(f: &mut Frame, prog: &'static str, outcome: Outcome, seen: Seen) {
    f.bakes += 1;
    f.checked += seen.checked;
    f.zero += seen.zero;
    f.refreshed += seen.refreshed;
    f.ms += seen.ms;
    let column = match outcome {
        Outcome::Clean => return,
        Outcome::Drawn => 1,
        Outcome::Expired => 2,
        Outcome::NoTime | Outcome::Known => 3,
    };
    if outcome.held() {
        f.outcomes[0] += 1;
    }
    f.outcomes[match outcome {
        Outcome::Drawn => 1,
        Outcome::Expired => 2,
        Outcome::NoTime => 3,
        _ => 4,
    }] += 1;
    let i = match f.by_bake.iter().position(|(p, _)| *p == prog) {
        Some(i) => i,
        None => {
            f.by_bake.push((prog, [0; 4]));
            f.by_bake.len() - 1
        }
    };
    let row = &mut f.by_bake[i].1;
    if outcome.held() {
        row[0] += 1;
    }
    row[column] += 1;
}

/// A bake draw of program `prog` was looked at in `frame`: counted, and the frame before's line
/// written at the first of a new frame (only for a frame with a zero input).
pub(crate) fn record(frame: u64, prog: &'static str, outcome: Outcome, seen: Seen) {
    let mut f = FRAME.lock().unwrap_or_else(|e| e.into_inner());
    let cur = f.get_or_insert_with(Frame::default);
    if cur.frame != frame {
        let done = std::mem::replace(
            cur,
            Frame {
                frame,
                ..Frame::default()
            },
        );
        if done.zero > 0 && LINES.fetch_add(1, Relaxed) < MAX_LINES {
            tracing::info!("{}", frame_line(&done));
        }
    }
    add_to(cur, prog, outcome, seen);
}

/// A held bake's line: its program, outcome, ms held and zero inputs (`slot:address+len`).
pub(crate) fn bake_line(
    frame: u64,
    prog: &str,
    outcome: Outcome,
    ms: u64,
    zero: &[(u32, u64, u64)],
) {
    if BAKE_LINES.fetch_add(1, Relaxed) >= MAX_BAKE_LINES {
        return;
    }
    let slots: Vec<String> = zero
        .iter()
        .map(|&(slot, at, len)| format!("{slot}:{at:#x}+{len}"))
        .collect();
    tracing::info!(
        "bake zero wait: frame {frame} bake {prog} outcome={} ms={ms} zero=[{}]",
        outcome.name(),
        slots.join(" ")
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_runs_cover_the_texture_from_its_first_byte_to_its_last() {
        assert!(sample_spans(0).is_empty());
        assert_eq!(sample_spans(100), [(0, 100)]);
        assert_eq!(
            sample_spans(SAMPLES * SAMPLE_BYTES),
            [(0, SAMPLES * SAMPLE_BYTES)]
        );
        let len = 4 << 20;
        let s = sample_spans(len);
        assert_eq!(s.len() as u64, SAMPLES);
        assert_eq!(s[0], (0, SAMPLE_BYTES));
        assert_eq!(s.last().copied(), Some((len - SAMPLE_BYTES, SAMPLE_BYTES)));
        for w in s.windows(2) {
            assert!(w[0].0 + w[0].1 <= w[1].0, "{w:?} overlap or out of order");
        }
        assert!(s.iter().all(|&(at, n)| at + n <= len));
        let odd = sample_spans(SAMPLES * SAMPLE_BYTES + 7);
        assert!(
            odd.iter()
                .all(|&(at, n)| at + n <= SAMPLES * SAMPLE_BYTES + 7)
        );
    }

    #[test]
    fn zeros_are_zeros_at_any_alignment() {
        let mut b = vec![0u8; 1031];
        assert!(all_zero(&b));
        assert!(all_zero(&b[3..]));
        b[1030] = 1;
        assert!(!all_zero(&b[3..]));
        b[1030] = 0;
        b[1] = 0x80;
        assert!(!all_zero(&b));
        assert!(all_zero(&b[2..]));
        assert!(all_zero(&[]));
    }

    /// Part I flick: d126bcae baked black with every input `ok`; its texture's memory was mapped
    /// and still zeros. One non-zero run anywhere is content; an unreadable run says nothing.
    #[test]
    fn a_texture_reads_zero_only_when_every_run_does() {
        let len = 1 << 20;
        assert_eq!(reads_zero(len, |_, _| Some(true)), Some(true));
        let last = sample_spans(len).last().unwrap().0;
        assert_eq!(reads_zero(len, |at, _| Some(at != last)), Some(false));
        let mut runs = 0;
        assert_eq!(
            reads_zero(len, |_, _| {
                runs += 1;
                Some(false)
            }),
            Some(false)
        );
        assert_eq!(runs, 1, "stops at the first run with content");
        assert_eq!(reads_zero(len, |at, _| (at == 0).then_some(true)), None);
        assert_eq!(reads_zero(0, |_, _| Some(true)), None);
    }

    #[test]
    fn a_bake_is_held_only_for_unknown_zeros_and_while_the_frame_has_time() {
        let never = |_: u64| -> bool { panic!("not held") };
        assert_eq!(decide(0, false, 10, never), Outcome::Clean);
        assert_eq!(decide(2, true, 10, never), Outcome::Known);
        assert_eq!(decide(1, false, 0, never), Outcome::NoTime);
        let mut asked = 0;
        assert_eq!(
            decide(1, false, 7, |ms| {
                asked = ms;
                true
            }),
            Outcome::Drawn
        );
        assert_eq!(asked, 7);
        assert_eq!(decide(1, false, 7, |_| false), Outcome::Expired);
        assert!(Outcome::Expired.replays() && Outcome::NoTime.replays());
        for o in [Outcome::Clean, Outcome::Drawn, Outcome::Known] {
            assert!(!o.replays(), "{o:?}");
        }
        assert!(Outcome::Drawn.held() && Outcome::Expired.held());
        assert!(!Outcome::NoTime.held() && !Outcome::Known.held());
    }

    #[test]
    fn a_frame_holds_at_most_four_bakes_worth() {
        let frame = u64::MAX - 11;
        assert_eq!(wait_left(frame, 10), 10);
        waited(35);
        assert_eq!(wait_left(frame, 10), 5);
        waited(5);
        assert_eq!(wait_left(frame, 10), 0);
        assert_eq!(wait_left(frame + 1, 10), 10);
    }

    #[test]
    fn an_input_zero_at_the_bound_holds_no_bake_for_a_while() {
        let (at, len) = (0xdead_0000_u64, 65_536);
        assert!(!known(at, len, 100));
        expired(at, len, 100);
        assert!(known(at, len, 100));
        assert!(known(at, len, 100 + KNOWN_FRAMES - 1));
        assert!(!known(at, len, 100 + KNOWN_FRAMES));
        assert!(
            !known(at, len + 1, 100),
            "another texture at the same address"
        );
    }

    #[test]
    fn the_value_is_ms_capped_and_live() {
        assert_eq!(parse(None), 0);
        assert_eq!(parse(Some(" 20 ")), 20);
        assert_eq!(parse(Some("x")), 0);
        assert_eq!(parse(Some("99999")), MAX_MS);
        victory_core::settings::check_live("VICTORY_BAKE_ZERO_WAIT", "20", wait_ms, 0, 20);
        victory_core::settings::set("VICTORY_BAKE_ZERO_WAIT", "").unwrap();
    }

    #[test]
    fn the_frame_line_names_held_drawn_and_expired_per_bake() {
        let mut f = Frame {
            frame: 7,
            ..Frame::default()
        };
        let seen = |zero| Seen {
            checked: 4,
            zero,
            refreshed: 0,
            ms: 3,
        };
        add_to(
            &mut f,
            "d126bcae",
            Outcome::Drawn,
            Seen {
                refreshed: 1,
                ..seen(1)
            },
        );
        add_to(&mut f, "d126bcae", Outcome::Expired, seen(2));
        add_to(&mut f, "280967ad", Outcome::Known, seen(1));
        add_to(&mut f, "280967ad", Outcome::Clean, seen(0));
        assert_eq!(
            frame_line(&f),
            "bake zero wait: frame 7 bakes=4 checked=16 zero=4 held=2 drawn=1 expired=1 no_time=0 known=1 refreshed=1 ms=12 by_bake(held/drawn/expired/skipped)=[d126bcae:2/1/1/0 280967ad:0/0/0/1]"
        );
    }
}
