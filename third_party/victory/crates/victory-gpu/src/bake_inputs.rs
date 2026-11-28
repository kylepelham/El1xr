//! `VICTORY_BAKE_INPUTS=1`: a decoration bake draw gets the textures it reads uploaded before it,
//! and only those, whatever is left of the frame's bake budget.
//!
//! The title's decoration bake (`PS_BakeDecorations_*`, `PS_DecorationBlend_*`,
//! [`crate::draw::BAKE_PIXEL_SHADERS`]) composes a character's skin into a tile of a target
//! once, from textures it picks by index. Without the switch each bake draw runs the general
//! pass over every due texture until the frame's bake budget (500 ms by default during play) is
//! spent: with 6,000-24,000 textures due as the world comes in, the bake reads textures not
//! uploaded yet (zeros) and the face stays black (batch 3: a black face after every run of short
//! bake passes; `VICTORY_BAKE_TEXTURES_MS=10000` ended the black faces at the cost of an 11 s
//! hitch).
//!
//! The bake shaders all find their textures the same way (their DXIL, see the tests): user data
//! word 9 is the byte offset in the view heap of the draw's records buffer, and the slot
//! [`params_offset`] bytes after it holds its parameter constants. The parameters name up to
//! four textures (dwords 8, 12, 16 and 24) and the run of records the draw composes (dword 30
//! up to dword 31, at most dword 0); each 304-byte record names up to four (bytes 0, 16, 32 and
//! 64). Every index is the low 27 bits. These are read from title memory on the CPU at bind time
//! ([`slots`]) and served first in a pass of their own, so the draw waits for a few textures, not
//! for the queue. One the title has not streamed in yet (no memory) still cannot be uploaded:
//! the per-frame line counts those.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// The record stride and the byte offsets in a record of its texture indices.
const RECORD: u64 = 304;
const RECORD_TEXTURES: [u64; 4] = [0, 16, 32, 64];
/// The parameter dwords holding texture indices, and the records' first, end and count.
const PARAM_TEXTURES: [usize; 4] = [8, 12, 16, 24];
const FIRST: usize = 30;
const END: usize = 31;
const COUNT: usize = 0;
/// Dwords of the parameters read (up to dword 31).
pub(crate) const PARAM_DWORDS: usize = 32;
/// Records looked at per draw at most (the shaders' loop stops after 128).
const MAX_RECORDS: u32 = 128;
const INDEX_BITS: u32 = (1 << 27) - 1;
/// The user data word holding the records buffer's byte offset in the view heap.
pub(crate) const USER_WORD: u32 = 9;

/// Live: each bake draw picks its upload pass when it is bound.
pub(crate) static ON: victory_core::settings::Live =
    victory_core::settings::Live::new("VICTORY_BAKE_INPUTS", |v| u64::from(v == Some("1")));

pub(crate) fn on() -> bool {
    ON.get_bool()
}

/// Bytes from the records buffer's descriptor to the parameters' for the bake pixel shader
/// `hash` (`PS_BakeDecorations_0000`: 96, the blends: 32); None for another shader.
pub(crate) fn params_offset(hash: &str) -> Option<u32> {
    match hash {
        "c0a7fe77b1862e10" => Some(96),
        "0d1575a396dfe513" | "b7cfd8dda91ebb6b" | "280967ad158cee0b" | "d126bcae5d5d8d26"
        | "fcee612943c008e0" | "fa455c8a3c32577b" => Some(32),
        _ => None,
    }
}

/// The view heap slots a bake draw reads textures from, given its parameter dwords and a
/// reader of its records buffer's dwords by byte offset (None past what can be read).
/// Distinct, in the order found; slot 0 left out (a null index).
pub(crate) fn slots(params: &[u32; PARAM_DWORDS], record: impl Fn(u64) -> Option<u32>) -> Vec<u32> {
    let mut out = Vec::new();
    let mut add = |v: u32| {
        let slot = v & INDEX_BITS;
        if slot != 0 && !out.contains(&slot) {
            out.push(slot);
        }
    };
    for d in PARAM_TEXTURES {
        add(params[d]);
    }
    let (first, end) = (params[FIRST], params[END].min(params[COUNT]));
    for i in first..end.min(first.saturating_add(MAX_RECORDS)) {
        for off in RECORD_TEXTURES {
            match record(u64::from(i) * RECORD + off) {
                Some(v) => add(v),
                None => return out,
            }
        }
    }
    out
}

/// The byte offset and length in the records buffer of the run of records the draw composes.
pub(crate) fn records_bytes(params: &[u32; PARAM_DWORDS]) -> (u64, u64) {
    let (first, end) = (params[FIRST], params[END].min(params[COUNT]));
    let end = end.min(first.saturating_add(MAX_RECORDS));
    (
        u64::from(first) * RECORD,
        u64::from(end.saturating_sub(first)) * RECORD,
    )
}

#[derive(Clone, Copy)]
#[repr(usize)]
pub(crate) enum C {
    /// Bake draws given their own pass.
    Bakes,
    /// Their texture slots (summed over the draws).
    Inputs,
    /// Of those, uploaded by the bake's own pass.
    Uploaded,
    /// Bake draws that ran with one of their textures still not uploaded: cut short.
    Short,
    /// Their slots still waiting (summed): no memory yet, part of it, or no room on the card.
    Pending,
    /// Bake draws whose parameters or records could not be read.
    Unread,
    /// Microseconds of the passes run for a bake draw's textures alone (after the frame's bake
    /// budget was spent).
    OwnUs,
}
const N: usize = 7;
const NAMES: [&str; N] = ["bakes", "inputs", "uploaded", "short", "pending", "unread", "own_us"];
static COUNTS: [AtomicU64; N] = [const { AtomicU64::new(0) }; N];
static FRAME: AtomicU64 = AtomicU64::new(0);
static LINES: AtomicU64 = AtomicU64::new(0);
const MAX_LINES: u64 = 20_000;

pub(crate) fn add(c: C, n: u64) {
    roll();
    COUNTS[c as usize].fetch_add(n, Relaxed);
}

/// The line for a frame with bake draws, at the first count of the next.
fn roll() {
    let now = crate::frame::current();
    let prev = FRAME.load(Relaxed);
    if prev == now || FRAME.compare_exchange(prev, now, Relaxed, Relaxed).is_err() {
        return;
    }
    let c: [u64; N] = std::array::from_fn(|i| COUNTS[i].swap(0, Relaxed));
    if c[C::Bakes as usize] == 0 && c[C::Unread as usize] == 0 {
        return;
    }
    if LINES.fetch_add(1, Relaxed) >= MAX_LINES {
        return;
    }
    tracing::info!(frame = prev, counts = %line(&c), "bake inputs frame");
}

fn line(c: &[u64; N]) -> String {
    NAMES
        .iter()
        .zip(c)
        .map(|(name, n)| format!("{name}={n}"))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `VICTORY_BAKE_REPLAY` hashes the run of records the draw composes: dword 30 up to the
    /// lesser of dword 31 and dword 0, at most 128 records of 304 bytes.
    #[test]
    fn the_run_of_records_a_draw_composes() {
        let mut p = [0u32; PARAM_DWORDS];
        p[FIRST] = 2;
        p[END] = 5;
        p[COUNT] = 4;
        assert_eq!(records_bytes(&p), (2 * RECORD, 2 * RECORD));
        p[COUNT] = 1;
        assert_eq!(records_bytes(&p), (2 * RECORD, 0));
        p[COUNT] = 1000;
        p[END] = 1000;
        assert_eq!(records_bytes(&p).1, u64::from(MAX_RECORDS) * RECORD);
    }

    #[test]
    fn the_switch_is_live() {
        // Unchanged, the switch reads what the environment said, as before.
        assert_eq!(on(), std::env::var_os("VICTORY_BAKE_INPUTS").is_some_and(|v| v == "1"));
        if std::env::var_os("VICTORY_BAKE_INPUTS").is_some() {
            return;
        }
        victory_core::settings::set("VICTORY_BAKE_INPUTS", "1").unwrap();
        let now = on();
        victory_core::settings::set("VICTORY_BAKE_INPUTS", "").unwrap();
        assert!(now, "a change takes effect while running");
        assert!(!on(), "and clears back to the launch value");
    }

    fn params(textures: [u32; 4], first: u32, end: u32, count: u32) -> [u32; PARAM_DWORDS] {
        let mut p = [0u32; PARAM_DWORDS];
        for (d, t) in PARAM_TEXTURES.iter().zip(textures) {
            p[*d] = t;
        }
        p[FIRST] = first;
        p[END] = end;
        p[COUNT] = count;
        p
    }

    /// Records whose texture fields hold `100 * record + field`, `n` of them.
    fn records(n: u64) -> impl Fn(u64) -> Option<u32> {
        move |at| {
            let (i, off) = (at / RECORD, at % RECORD);
            let field = RECORD_TEXTURES.iter().position(|&o| o == off)? as u64;
            (i < n).then_some((100 * i + field + 1) as u32)
        }
    }

    #[test]
    fn parameters_and_records_name_the_slots() {
        // The high five bits are not the index (the shaders shift them out).
        let p = params([7 | 0xF800_0000, 0, 9, 7], 2, 4, 10);
        assert_eq!(
            slots(&p, records(10)),
            vec![7, 9, 201, 202, 203, 204, 301, 302, 303, 304]
        );
    }

    #[test]
    fn the_run_of_records_stops_at_the_count_and_at_what_can_be_read() {
        // end past the count: the count bounds it (the shaders' `icmp uge %i, r0.x`)
        assert_eq!(
            slots(&params([0; 4], 0, 5, 1), records(10)),
            vec![1, 2, 3, 4]
        );
        // an empty run
        assert!(slots(&params([0; 4], 3, 3, 10), records(10)).is_empty());
        // first past end
        assert!(slots(&params([0; 4], 4, 2, 10), records(10)).is_empty());
        // the buffer ends after one record: what was read is kept
        assert_eq!(
            slots(&params([5, 0, 0, 0], 0, 3, 10), records(1)),
            vec![5, 1, 2, 3, 4]
        );
        // a wild run is capped
        let wild = slots(&params([0; 4], 0, u32::MAX, u32::MAX), |_| Some(1));
        assert_eq!(wild, vec![1]);
    }

    #[test]
    fn only_the_seven_bake_shaders_have_parameters() {
        assert_eq!(params_offset("c0a7fe77b1862e10"), Some(96));
        for h in crate::draw::BAKE_PIXEL_SHADERS
            .iter()
            .filter(|&&h| h != "c0a7fe77b1862e10")
        {
            assert_eq!(params_offset(h), Some(32), "{h}");
        }
        assert_eq!(params_offset("0000000000000000"), None);
    }

    #[test]
    fn the_frame_line_names_every_count() {
        assert_eq!(
            line(&[3, 12, 9, 1, 3, 0, 4100]),
            "bakes=3 inputs=12 uploaded=9 short=1 pending=3 unread=0 own_us=4100"
        );
    }
}
