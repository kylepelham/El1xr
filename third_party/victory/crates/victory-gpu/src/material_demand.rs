//! `VICTORY_MATERIAL_TEXTURE_DEMAND=1`: the textures the title's deferred character material
//! shaders pick by index go to the front of the next texture pass.
//!
//! Port of DexNom/VIctory PR #49 (Artking175). Two pixel shaders draw characters into the
//! deferred buffers (`a402e650d0acbc92`, its packed form `b15ba4896d650511`; both bound in
//! our runs, mesh pipelines with `d84e50dad3dc0a3e`). Each finds its textures in a material
//! record: user data word 8 is the byte offset in the view heap of the table whose dword 688
//! is the material buffer, user data word 14 the record's byte offset in it, and the record's
//! dwords at [`fields`] hold heap indices (the low 27 bits: the shaders shift the word left
//! five bits). Without the switch those textures wait in the queue with the 6,000-24,000
//! others the world brings in, and a character is drawn with what is not uploaded yet as
//! zeros until its turn comes (a dark or wrong-coloured outfit that settles later).
//!
//! DexNom reads the record back from the card a frame later and reorders the queue; here the
//! record is read from title memory at bind time, as `crate::bake_inputs` reads the bake
//! records, and the slots found are put first in the next pass, within its usual budgets.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// The user data words holding the table's and the record's byte offsets.
pub(crate) const TABLE_USER_WORD: u32 = 8;
pub(crate) const RECORD_USER_WORD: u32 = 14;
/// Bytes from the table to the material buffer's descriptor (dword 688).
pub(crate) const TABLE_BYTES: u32 = 688 * 4;
const INDEX_BITS: u32 = (1 << 27) - 1;
/// Slots kept for the next pass at most (each draw's few; a frame draws the same characters
/// many times).
const MOST: usize = 512;

/// Live: each character draw reads its record when it is bound.
pub(crate) static ON: victory_core::settings::Live =
    victory_core::settings::Live::new("VICTORY_MATERIAL_TEXTURE_DEMAND", |v| {
        u64::from(v == Some("1"))
    });

pub(crate) fn on() -> bool {
    ON.get_bool()
}

/// The record's byte offsets of the texture indices of pixel shader `hash`; None for another.
pub(crate) fn fields(hash: &str) -> Option<&'static [u32]> {
    match hash {
        "a402e650d0acbc92" => Some(&[0, 4, 8, 12, 16, 20, 24]),
        "b15ba4896d650511" => Some(&[0, 4, 8, 12, 16, 20, 24, 28, 32]),
        _ => None,
    }
}

/// Where the record's fields lie in a buffer of `len` bytes: its offset and the bytes up to
/// the last field. None if the record is not dword aligned or runs past the buffer.
pub(crate) fn byte_range(record: u32, fields: &[u32], len: u64) -> Option<(u64, usize)> {
    let bytes = fields.last()?.checked_add(4)?;
    let end = u64::from(record) + u64::from(bytes);
    (record.is_multiple_of(4) && end <= len).then_some((u64::from(record), bytes as usize))
}

/// The heap slots the record's `words` (from its first field on) name at `fields`. Distinct,
/// in the order found; slot 0 left out (a null index).
pub(crate) fn indices(words: &[u32], fields: &[u32]) -> Vec<u32> {
    let mut out = Vec::new();
    for field in fields {
        if let Some(&word) = words.get((field / 4) as usize) {
            let slot = word & INDEX_BITS;
            if slot != 0 && !out.contains(&slot) {
                out.push(slot);
            }
        }
    }
    out
}

static WANTED: Mutex<Vec<u32>> = Mutex::new(Vec::new());

/// Slots a character draw reads, for the next pass.
pub(crate) fn want(slots: &[u32]) {
    let mut wanted = WANTED.lock().unwrap_or_else(|e| e.into_inner());
    for &s in slots {
        if wanted.len() >= MOST {
            break;
        }
        if !wanted.contains(&s) {
            wanted.push(s);
        }
    }
}

/// The slots wanted since the last pass took them.
pub(crate) fn take() -> Vec<u32> {
    std::mem::take(&mut *WANTED.lock().unwrap_or_else(|e| e.into_inner()))
}

#[derive(Clone, Copy)]
#[repr(usize)]
pub(crate) enum C {
    /// Character material draws looked at.
    Draws,
    /// Of those, the record could not be read (offsets, view or memory).
    Unread,
    /// Slots their records named (summed over the draws).
    Slots,
    /// Queue entries a pass put first.
    Promoted,
}
const N: usize = 4;
const NAMES: [&str; N] = ["draws", "unread", "slots", "promoted"];
static COUNTS: [AtomicU64; N] = [const { AtomicU64::new(0) }; N];
static LAST: AtomicU64 = AtomicU64::new(0);
/// A line every this many frames with draws.
const EVERY: u64 = 300;

pub(crate) fn add(c: C, n: u64) {
    COUNTS[c as usize].fetch_add(n, Relaxed);
    let now = crate::frame::current();
    let last = LAST.load(Relaxed);
    if now < last + EVERY || LAST.compare_exchange(last, now, Relaxed, Relaxed).is_err() {
        return;
    }
    let c: [u64; N] = std::array::from_fn(|i| COUNTS[i].swap(0, Relaxed));
    tracing::info!(frame = now, counts = %line(&c), "material demand since the last line");
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

    #[test]
    fn the_two_character_shaders_have_fields() {
        let regular = fields("a402e650d0acbc92").expect("deferred character material");
        let packed = fields("b15ba4896d650511").expect("packed deferred character material");
        assert_eq!(regular, [0, 4, 8, 12, 16, 20, 24]);
        assert_eq!(packed, [0, 4, 8, 12, 16, 20, 24, 28, 32]);
        assert_eq!(fields("0000000000000000"), None);
    }

    #[test]
    fn indices_drop_the_shifted_bits_repeats_and_nulls() {
        let regular = fields("a402e650d0acbc92").unwrap();
        let words = [0xf800_0003u32, 0, 7, 0, 5, 6, 3, 9, 10];
        assert_eq!(indices(&words, regular), vec![3, 7, 5, 6]);
        let packed = fields("b15ba4896d650511").unwrap();
        assert_eq!(indices(&words, packed), vec![3, 7, 5, 6, 9, 10]);
        // fewer words than fields: what is there
        assert_eq!(indices(&words[..3], packed), vec![3, 7]);
    }

    #[test]
    fn a_record_must_fit_its_buffer() {
        let regular = fields("a402e650d0acbc92").unwrap();
        let packed = fields("b15ba4896d650511").unwrap();
        // 28 bytes after offset 64: enough for the regular form, too short for the packed
        assert_eq!(byte_range(64, regular, 92), Some((64, 28)));
        assert_eq!(byte_range(64, packed, 92), None);
        assert_eq!(byte_range(64, packed, 100), Some((64, 36)));
        // unaligned
        assert_eq!(byte_range(66, regular, 1024), None);
        // past the buffer, and the largest offset without overflow
        assert_eq!(byte_range(1024, regular, 1024), None);
        assert_eq!(
            byte_range(u32::MAX - 3, regular, u64::MAX),
            Some((u64::from(u32::MAX - 3), 28))
        );
    }

    #[test]
    fn wanted_slots_are_distinct_and_bounded() {
        let _ = take();
        want(&[4, 5, 4]);
        want(&[5, 6]);
        assert_eq!(take(), vec![4, 5, 6]);
        assert!(take().is_empty());
        let many: Vec<u32> = (1..=2 * MOST as u32).collect();
        want(&many);
        assert_eq!(take().len(), MOST);
    }

    #[test]
    fn the_line_names_every_count() {
        assert_eq!(
            line(&[10, 1, 40, 12]),
            "draws=10 unread=1 slots=40 promoted=12"
        );
    }

    #[test]
    fn the_switch_is_live() {
        assert_eq!(
            on(),
            std::env::var_os("VICTORY_MATERIAL_TEXTURE_DEMAND").is_some_and(|v| v == "1")
        );
        if std::env::var_os("VICTORY_MATERIAL_TEXTURE_DEMAND").is_some() {
            return;
        }
        victory_core::settings::set("VICTORY_MATERIAL_TEXTURE_DEMAND", "1").unwrap();
        let now = on();
        victory_core::settings::set("VICTORY_MATERIAL_TEXTURE_DEMAND", "").unwrap();
        assert!(now, "a change takes effect while running");
        assert!(!on(), "and clears back to the launch value");
    }
}
