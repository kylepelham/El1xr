//! What the title asks of device method 0x1c8, the console's graphics
//! pipeline made from loose pieces, which we do not make
//! (`VICTORY_PIPELINE_PIECES_DIAG=1`): one log line per frame line while
//! new calls came, with the calls so far, the distinct descriptions, which
//! stage slots they fill, and how often the title then bound a null
//! pipeline or had a draw dropped with none bound.
//!
//! The description (0x108 bytes, layout from trustno1-coder/VIctory-NVIDIA
//! #18 by Kemerd, as ported by RipcuBAN; palmetto's `docs/slot57_wiring.md`
//! by Widmer names the method): a root signature, then pieces as address
//! and length pairs, the packet (+0x08), the state piece (+0x18), the pixel
//! code and metadata (+0x28, +0x38), the vertex code and metadata (+0x48,
//! +0x58), and five more stage slots of four words from +0x68 that the
//! port refuses. Nothing here changes what the title gets.

use std::collections::HashSet;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// Whether the 0x1c8 calls are classified and the line logged. Live; the
/// null binds and null drops are counted either way (rare, one branch).
pub(crate) fn on() -> bool {
    static ON: victory_core::settings::Live =
        victory_core::settings::Live::flag("VICTORY_PIPELINE_PIECES_DIAG");
    ON.get_bool()
}

/// The description's 33 words, as the title wrote them.
pub(crate) const WORDS: usize = 0x108 / 8;

/// Which stage slots a description fills.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Shape {
    pub vertex: bool,
    pub pixel: bool,
    /// The first of the five extra slots (+0x68, +0x88, ..) with a word
    /// set, if any.
    pub extra: Option<usize>,
    pub packet_len: u64,
    pub state_len: u64,
}

/// The shape of a description, from its words.
pub(crate) fn shape(w: &[u64; WORDS]) -> Shape {
    let set = |at: usize, n: usize| w[at..at + n].iter().any(|&v| v != 0);
    Shape {
        pixel: set(5, 4),
        vertex: set(9, 4),
        extra: w[13..].chunks(4).position(|c| c.iter().any(|&v| v != 0)),
        packet_len: w[2],
        state_len: w[4],
    }
}

/// A description's identity: its pieces (addresses and lengths), not the
/// root signature (one object for all of them).
pub(crate) fn key(w: &[u64; WORDS]) -> u64 {
    w[1..].iter().fold(0xcbf2_9ce4_8422_2325_u64, |h, &v| {
        (h ^ v).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

/// Since the start.
#[derive(Default)]
struct Counts {
    calls: u64,
    /// Descriptions not there to read.
    unread: u64,
    keys: HashSet<u64>,
    vertex_codes: HashSet<u64>,
    pixel_codes: HashSet<u64>,
    /// vertex and pixel; vertex alone; pixel alone (plus an extra slot or
    /// not, counted apart); no stage in the first two slots.
    vertex_pixel: u64,
    vertex_only: u64,
    pixel_only: u64,
    neither: u64,
    /// Calls with a word set in each extra slot (the first one set).
    extra: [u64; 5],
    /// Of those, ones with no vertex stage: the extra slot would be the
    /// stage that places vertices.
    extra_no_vertex: u64,
    /// The packet lengths seen, most-seen first when logged.
    packet_lens: Vec<(u64, u64)>,
    nanos: u64,
}

static COUNTS: Mutex<Option<Counts>> = Mutex::new(None);
/// Calls counted at the last line.
static LOGGED_AT: AtomicU64 = AtomicU64::new(0);
/// Since the start: `set_pipeline(null)` calls, and draws or dispatches
/// dropped while no pipeline was bound.
static NULL_BINDS: AtomicU64 = AtomicU64::new(0);
static NULL_DROPS: AtomicU64 = AtomicU64::new(0);
/// Frames presented and the two counts above at the last line.
static LAST: Mutex<[u64; 3]> = Mutex::new([0; 3]);

/// The title bound `pipeline` on a list.
pub(crate) fn bound(pipeline: *const std::ffi::c_void) {
    if pipeline.is_null() {
        NULL_BINDS.fetch_add(1, Ordering::Relaxed);
    }
}

/// A draw or dispatch of title pipeline `this` was dropped.
pub(crate) fn dropped(this: usize) {
    if this == 0 {
        NULL_DROPS.fetch_add(1, Ordering::Relaxed);
    }
}

/// Counts one 0x1c8 call whose description is at `desc`.
pub(crate) fn call(desc: *const u8) {
    if !on() {
        return;
    }
    let start = std::time::Instant::now();
    let words = read(desc);
    let mut counts = COUNTS.lock().unwrap_or_else(|e| e.into_inner());
    let counts = counts.get_or_insert_with(Counts::default);
    counts.calls += 1;
    match words {
        Some(w) => note(counts, &w),
        None => counts.unread += 1,
    }
    counts.nanos += start.elapsed().as_nanos() as u64;
}

/// The description's words, `None` if its first 0x68 bytes (root, packet,
/// state and the two stages) are not there; extra slots not there read as
/// zero.
#[cfg(windows)]
fn read(desc: *const u8) -> Option<[u64; WORDS]> {
    if desc.is_null() {
        return None;
    }
    let mut bytes = [0u8; WORDS * 8];
    if !crate::host::read_into(desc, &mut bytes)
        && !crate::host::read_into(desc, &mut bytes[..0x68])
    {
        return None;
    }
    let mut w = [0u64; WORDS];
    for (to, c) in w.iter_mut().zip(bytes.as_chunks::<8>().0) {
        *to = u64::from_le_bytes(*c);
    }
    Some(w)
}

#[cfg(not(windows))]
fn read(_: *const u8) -> Option<[u64; WORDS]> {
    None
}

fn note(c: &mut Counts, w: &[u64; WORDS]) {
    let s = shape(w);
    c.keys.insert(key(w));
    if s.vertex {
        c.vertex_codes.insert(w[9]);
    }
    if s.pixel {
        c.pixel_codes.insert(w[5]);
    }
    match (s.vertex, s.pixel) {
        (true, true) => c.vertex_pixel += 1,
        (true, false) => c.vertex_only += 1,
        (false, true) => c.pixel_only += 1,
        (false, false) => c.neither += 1,
    }
    if let Some(slot) = s.extra {
        if c.extra[slot.min(4)] == 0 {
            // The first of each slot: what its words hold (a stage is an
            // address and a length, then the metadata's).
            let at = 13 + slot * 4;
            tracing::info!(
                slot = format_args!("+{:#x}", 0x68 + slot * 0x20),
                words = format_args!("{:x?}", &w[at..(at + 4).min(WORDS)]),
                description = format_args!("{:x?}", &w[1..13]),
                "0x1c8 pieces: first description with a word in this extra stage slot (VICTORY_PIPELINE_PIECES_DIAG=1)"
            );
        }
        c.extra[slot.min(4)] += 1;
        if !s.vertex {
            c.extra_no_vertex += 1;
        }
    }
    if let Some((_, n)) = c
        .packet_lens
        .iter_mut()
        .find(|(len, _)| *len == s.packet_len)
    {
        *n += 1;
    } else if c.packet_lens.len() < 64 {
        c.packet_lens.push((s.packet_len, 1));
    }
}

fn text(c: &Counts, null_binds: u64, null_drops: u64, frames: u64) -> String {
    let mut lens = c.packet_lens.clone();
    lens.sort_unstable_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let lens: Vec<String> = lens
        .iter()
        .take(6)
        .map(|(l, n)| format!("{l:#x}x{n}"))
        .collect();
    let per_frame = |n: u64| n as f64 / frames.max(1) as f64;
    format!(
        "0x1c8 pieces (VICTORY_PIPELINE_PIECES_DIAG=1): calls={} distinct={} unread={} vertex+pixel={} vertex_only={} pixel_only={} neither={} extra_slots={:?} extra_without_vertex={} vertex_codes={} pixel_codes={} packet_lens=[{}] call_us={:.1}; null_binds/frame={:.1} null_drops/frame={:.1} over {frames} frames",
        c.calls,
        c.keys.len(),
        c.unread,
        c.vertex_pixel,
        c.vertex_only,
        c.pixel_only,
        c.neither,
        c.extra,
        c.extra_no_vertex,
        c.vertex_codes.len(),
        c.pixel_codes.len(),
        lens.join(" "),
        c.nanos as f64 / 1000.0 / c.calls.max(1) as f64,
        per_frame(null_binds),
        per_frame(null_drops),
    )
}

/// Logs the line at a frame line, if new calls came or null pipelines were
/// bound or dropped since the last one.
pub(crate) fn line(presented: u64) {
    if !on() {
        return;
    }
    let (binds, drops) = (
        NULL_BINDS.load(Ordering::Relaxed),
        NULL_DROPS.load(Ordering::Relaxed),
    );
    let (frames, binds, drops) = {
        let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
        let d = (
            presented.saturating_sub(last[0]),
            binds - last[1],
            drops - last[2],
        );
        *last = [presented, binds, drops];
        d
    };
    let counts = COUNTS.lock().unwrap_or_else(|e| e.into_inner());
    let Some(c) = counts.as_ref() else {
        if binds + drops > 0 {
            tracing::info!(
                "0x1c8 pieces (VICTORY_PIPELINE_PIECES_DIAG=1): calls=0; null_binds/frame={:.1} null_drops/frame={:.1} over {frames} frames",
                binds as f64 / frames.max(1) as f64,
                drops as f64 / frames.max(1) as f64
            );
        }
        return;
    };
    if c.calls == LOGGED_AT.swap(c.calls, Ordering::Relaxed) && binds + drops == 0 {
        return;
    }
    tracing::info!("{}", text(c, binds, drops, frames));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A description like the ones run b5e logged: root, packet, state,
    /// pixel, vertex, and optionally an extra slot.
    fn desc(pixel: bool, vertex: bool, extra: Option<usize>) -> [u64; WORDS] {
        let mut w = [0u64; WORDS];
        w[0] = 0x2b1a_6511_b60;
        w[1..5].copy_from_slice(&[0xd04e_5c7ac, 0x18c, 0xd04e_7b034, 0xbc]);
        if pixel {
            w[5..9].copy_from_slice(&[0xd494_7a600, 0x200, 0xd04d_8efa4, 0xb0]);
        }
        if vertex {
            w[9..13].copy_from_slice(&[0xd494_79e00, 0x240, 0xd04d_8eae4, 0x70]);
        }
        if let Some(slot) = extra {
            let at = 13 + slot * 4;
            w[at..at + 4].copy_from_slice(&[0xd55a_45000, 0xd80, 0xd065_e91d4, 0x5b0]);
        }
        w
    }

    #[test]
    fn shapes_name_the_slots_filled() {
        let s = shape(&desc(true, true, None));
        assert_eq!(
            (s.vertex, s.pixel, s.extra, s.packet_len, s.state_len),
            (true, true, None, 0x18c, 0xbc)
        );
        let s = shape(&desc(true, false, Some(0)));
        assert_eq!((s.vertex, s.pixel, s.extra), (false, true, Some(0)));
        assert_eq!(shape(&desc(false, true, Some(4))).extra, Some(4));
        assert_eq!(shape(&desc(false, false, None)).extra, None);
    }

    #[test]
    fn keys_ignore_the_root_and_tell_pieces_apart() {
        let a = desc(true, true, None);
        let mut b = a;
        b[0] = 0x1234;
        assert_eq!(key(&a), key(&b));
        b[9] += 0x100;
        assert_ne!(key(&a), key(&b));
    }

    #[test]
    fn counts_and_line() {
        let mut c = Counts::default();
        for w in [
            desc(true, true, None),
            desc(true, true, None),
            desc(true, false, Some(0)),
            desc(false, true, None),
        ] {
            c.calls += 1;
            note(&mut c, &w);
        }
        assert_eq!(c.keys.len(), 3);
        assert_eq!(
            (c.vertex_pixel, c.vertex_only, c.pixel_only, c.neither),
            (2, 1, 1, 0)
        );
        assert_eq!(c.extra, [1, 0, 0, 0, 0]);
        assert_eq!(c.extra_no_vertex, 1);
        assert_eq!((c.vertex_codes.len(), c.pixel_codes.len()), (1, 1));
        assert_eq!(
            text(&c, 30, 0, 10),
            "0x1c8 pieces (VICTORY_PIPELINE_PIECES_DIAG=1): calls=4 distinct=3 unread=0 vertex+pixel=2 vertex_only=1 pixel_only=1 neither=0 extra_slots=[1, 0, 0, 0, 0] extra_without_vertex=1 vertex_codes=1 pixel_codes=1 packet_lens=[0x18cx4] call_us=0.0; null_binds/frame=3.0 null_drops/frame=0.0 over 10 frames"
        );
    }

    #[test]
    fn the_switch_is_live() {
        victory_core::settings::check_live("VICTORY_PIPELINE_PIECES_DIAG", "1", on, false, true);
    }
}
