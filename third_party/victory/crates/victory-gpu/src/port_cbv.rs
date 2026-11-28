//! Renderer port #5: the title's constant buffers read by the card where the
//! title has them (`VICTORY_PORT_CBV=1`, off by default; read in [`on`]).
//!
//! What the console allows: a constant buffer anywhere on 16 bytes. What
//! D3D12 asks: a constant buffer view on 256. So the layer made every
//! constant buffer a draw's tables name afresh at each submission: a copy of
//! its bytes (out of title memory, on the CPU) into upload memory of its own
//! on 256, and a view of that ([`crate::draw`]'s tables, `defer_constants`).
//! With renderer port 1 (`VICTORY_PORT_CONSTANTS=1`) the memory they are
//! copied out of is the card's to read in place already.
//!
//! What is done here instead (NVIDIA-clean, no raw twin heap): shaders are
//! translated to read constant buffers as raw bytes
//! (`victory_shader::emit::constants_raw`): a table's directly through its
//! descriptor slot, a root one as a root SRV. A raw view starts on 16 bytes,
//! a root SRV on 4. So a constant buffer of a table in title memory in place
//! is made a raw view of the title's bytes directly in its slot, and nothing
//! is copied or done at submission; a root one is bound at its address as
//! root SRVs are. Constant buffers anywhere else are copied as before and
//! viewed as raw bytes directly in their slot, unless `VICTORY_PORT_CBV_COPIES=1`
//! is also set (off by default): then a table's constant buffer in any host copy
//! is viewed directly as raw bytes too, with a normal [`crate::shadow::Shadow::refresh`]
//! for CPU writes, avoiding scratch fills and flight waits. A title slot's constant
//! buffer view is likewise made a raw view of its bytes.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// Whether renderer port #5 is on: the shaders' side
/// (`victory_shader::emit::constants_raw`) and the host's views follow the
/// same switch.
pub(crate) fn on() -> bool {
    victory_shader::emit::constants_raw()
}

/// Whether a table's constant buffer in title memory in place is given to the
/// card where it is (on with the port; `VICTORY_PORT_CBV_PLACE=0`: copied as
/// before, and read as raw bytes all the same).
pub(crate) fn in_place_on() -> bool {
    static PLACE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    on() && *PLACE
        .get_or_init(|| std::env::var_os("VICTORY_PORT_CBV_PLACE").is_none_or(|v| v != "0"))
}

/// Whether a table's constant buffer in a host copy (not in place) is viewed
/// directly as raw bytes instead of copied into scratch upload memory
/// (`VICTORY_PORT_CBV_COPIES=1`, off by default; requires `VICTORY_PORT_CBV=1`).
pub(crate) fn copies_on() -> bool {
    if !on() {
        return false;
    }
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| copies_value(std::env::var("VICTORY_PORT_CBV_COPIES").ok().as_deref()))
}

fn copies_value(value: Option<&str>) -> bool {
    value.is_some_and(|v| v.trim() == "1")
}

/// Whether, with [`copies_on`], a title buffer copy into a host copy that
/// draws' constant buffers are viewed in goes on the card at its place in the
/// list (`crate::list::card_copy`) instead of on the CPU at submission
/// (`VICTORY_PORT_CBV_COPY_ORDER=0`: on the CPU, as before).
///
/// A constant buffer copied into upload memory takes its bytes at its place
/// among the list's steps; one viewed in its host copy is read when the card
/// runs the draw. A copy on the CPU into those bytes, recorded after the
/// draw (in its list, a later list of the submission, or a later submission
/// while the draw's is still in the air), had the draw read the bytes after
/// the copy. On the card the copy runs in the title's order. Host copies are
/// known by a bit each once a constant buffer has been viewed in them (a few
/// more copies than needed go on the card where two share a bit); the first
/// copy into one before any view of it is not.
pub(crate) fn copy_order_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    (copies_on() || victory_shader::emit::heap_buffer_address())
        && *ON.get_or_init(|| {
            std::env::var_os("VICTORY_PORT_CBV_COPY_ORDER").is_none_or(|v| v != "0")
        })
}

/// Host copies constant buffers were viewed in, a bit each by address.
static VIEWED: [AtomicU64; VIEWED_WORDS] = [const { AtomicU64::new(0) }; VIEWED_WORDS];
const VIEWED_WORDS: usize = 256;

fn viewed_bit(at: usize) -> (usize, u64) {
    let h = ((at >> 4) as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15) >> 50;
    ((h >> 6) as usize % VIEWED_WORDS, 1 << (h & 63))
}

/// A table's constant buffer was viewed in `shadow` ([`copy_order_on`]).
pub(crate) fn viewed(shadow: &std::sync::Arc<crate::shadow::Shadow>) {
    if !copy_order_on() {
        return;
    }
    let (word, bit) = viewed_bit(std::sync::Arc::as_ptr(shadow) as usize);
    if VIEWED[word].load(Relaxed) & bit == 0 {
        VIEWED[word].fetch_or(bit, Relaxed);
    }
}

/// Whether a constant buffer may have been viewed in `shadow`.
pub(crate) fn was_viewed(shadow: &std::sync::Arc<crate::shadow::Shadow>) -> bool {
    let (word, bit) = viewed_bit(std::sync::Arc::as_ptr(shadow) as usize);
    VIEWED[word].load(Relaxed) & bit != 0
}

/// Since the last line: copies put on the card for [`copy_order_on`], and
/// those left on the CPU as `card_copy` could not take them.
static ORDERED: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

/// A copy into a host copy constant buffers are viewed in (`for_order`) went
/// on the card (`done`) or not.
pub(crate) fn ordered(for_order: bool, done: bool) {
    if for_order {
        ORDERED[usize::from(!done)].fetch_add(1, Relaxed);
    }
}

/// How a table's constant buffer is bound under renderer port 5.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TableUse {
    /// View title memory in place directly as raw bytes (no refresh).
    InPlace,
    /// View a host copy directly as raw bytes and defer a normal refresh.
    HostCopy,
    /// Copy into scratch upload memory as before.
    Copy,
}

/// Chooses how to bind a table's constant buffer given the switch states and
/// whether a host copy exists and is in place.
pub(crate) fn table_use(
    in_place_enabled: bool,
    copies_enabled: bool,
    has_shadow: bool,
    shadow_in_place: bool,
) -> TableUse {
    if !has_shadow {
        return TableUse::Copy;
    }
    if shadow_in_place && in_place_enabled {
        TableUse::InPlace
    } else if !shadow_in_place && copies_enabled {
        TableUse::HostCopy
    } else {
        TableUse::Copy
    }
}

/// Since the last line: tables' constant buffers given in place, their bytes;
/// copied as before because not in place (or no copy), because not on 16 bytes;
/// title slots' constant buffers made as raw views; tables' constant buffers
/// viewed directly in host copies, their bytes.
static STATS: [AtomicU64; 7] = [const { AtomicU64::new(0) }; 7];

/// (feedcard, #88; ported from MAXAWER/victory-merged-amd `8aa43c301a`, GPL-3.0)
/// `VICTORY_PORT_CBV_WRITTEN` (Live; 0, off, by default; needs `VICTORY_PORT_CBV=1`): a table's
/// constant buffer in the host copy of a small title buffer that shaders write is a raw view of
/// that copy, read by the card when the draw runs, instead of bytes copied out of title memory on
/// the CPU when the submission is prepared. 1: the constant's bytes are refreshed from title memory
/// first (a refresh leaves out ranges flights still write); 2: no refresh.
///
/// Why: the CPU copy reads title memory, so it must wait for the flights that write the buffer to
/// land and be copied back. Upstream (AMD) measured about four such waits a frame, 22-30 ms of a
/// 57 ms frame, for five buffers of 0x190-0x340 bytes the title's shaders fill, and 20 to 23 fps
/// at level 2. Their level 1 flickered trees (40-43% of picture changes): the refresh laid what
/// title memory held (a frame old) over what the shaders had just written on the card. Ours skips
/// ranges with flights out, so 1 may be clean here; 2 is upstream's.
///
/// What it assumes: the shaders that fill the constants write this host copy (the same lookup
/// finds it for their views), and the title's CPU does not write those bytes between (at 2).
/// Not in the cache key: the shaders already read constants raw with the port.
static WRITTEN: victory_core::settings::Live =
    victory_core::settings::Live::new("VICTORY_PORT_CBV_WRITTEN", |v| match v.map(str::trim) {
        Some("1") => 1,
        Some("2") => 2,
        _ => 0,
    });

/// Title buffers up to this size that shaders write are read in their host copy (upstream's
/// buffers were 0x190-0x340 bytes; a large host copy is rewritten under flights in the air).
pub(crate) const SMALL_WRITTEN: u64 = 64 << 10;

/// [`WRITTEN`] with the port on: 0, 1 or 2.
pub(crate) fn written_level() -> u64 {
    if on() { WRITTEN.get() } else { 0 }
}

/// Whether a table's constant buffer in a host copy is read in that copy at [`written_level`]:
/// a copy, not title memory in place nor a window, that shaders write, of at most
/// [`SMALL_WRITTEN`] bytes.
pub(crate) fn written_fits(in_place: bool, window: bool, shaders_wrote: bool, len: u64) -> bool {
    !in_place && !window && shaders_wrote && len <= SMALL_WRITTEN
}

/// Since the last line: tables' constant buffers read in the host copy of a buffer shaders write,
/// and those copied all the same (the view could not be made).
static WRITTEN_STATS: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

pub(crate) fn from_written() {
    WRITTEN_STATS[0].fetch_add(1, Relaxed);
}

pub(crate) fn not_from_written() {
    WRITTEN_STATS[1].fetch_add(1, Relaxed);
}

/// A table's constant buffer of `size` bytes was given in place.
pub(crate) fn in_place(size: u32) {
    STATS[0].fetch_add(1, Relaxed);
    STATS[1].fetch_add(u64::from(size), Relaxed);
}

/// A table's constant buffer of `size` bytes was viewed directly in its host copy.
pub(crate) fn in_copy(size: u32) {
    STATS[5].fetch_add(1, Relaxed);
    STATS[6].fetch_add(u64::from(size), Relaxed);
}

/// A table's constant buffer was copied as before: `aligned` false when it
/// is in place or a host copy but not on 16 bytes.
pub(crate) fn copied(aligned: bool) {
    STATS[if aligned { 2 } else { 3 }].fetch_add(1, Relaxed);
}

/// A title slot's constant buffer view was made as a raw view.
pub(crate) fn twinned() {
    STATS[4].fetch_add(1, Relaxed);
}

/// Whether a constant buffer at host address `gpu` can be a raw view of
/// those bytes: on 16 bytes.
pub(crate) fn raw_ok(gpu: u64) -> bool {
    gpu.is_multiple_of(16)
}

/// For the frame-rate line.
pub(crate) fn log(frames: f64) {
    if !on() {
        return;
    }
    let s: Vec<f64> = STATS
        .iter()
        .map(|s| s.swap(0, Relaxed) as f64 / frames.max(1e-9))
        .collect();
    let [read, not] = WRITTEN_STATS.each_ref().map(|a| a.swap(0, Relaxed) as f64 / frames.max(1e-9));
    let level = written_level();
    if level > 0 {
        tracing::info!(
            "renderer port 5 per frame (VICTORY_PORT_CBV_WRITTEN={level}): {read:.0} tables' constant buffers read in the host copy \
             of a small buffer shaders write, {not:.0} of those copied all the same"
        );
    }
    if copy_order_on() {
        let [done, left] = ORDERED.each_ref().map(|a| a.swap(0, Relaxed) as f64 / frames.max(1e-9));
        tracing::info!(
            "renderer port 5 copies into host copies constant buffers are viewed in, per frame (VICTORY_PORT_CBV_COPY_ORDER=1): \
             {done:.1} on the card at their place, {left:.1} left on the CPU (draws before them may read them after)"
        );
    }
    if copies_on() {
        tracing::info!(
            "renderer port 5 (VICTORY_PORT_CBV=1, VICTORY_PORT_CBV_COPIES=1) per frame: {:.0} in place ({:.1} KB), {:.0} in host copies ({:.1} KB not copied); \
             copied as before: {:.0} unshadowed, {:.0} not on 16 bytes; {:.0} title slots' constant buffers as raw views",
            s[0],
            s[1] / 1024.0,
            s[5],
            s[6] / 1024.0,
            s[2],
            s[3],
            s[4],
        );
    } else {
        tracing::info!(
            "renderer port 5 (VICTORY_PORT_CBV=1) per frame: {:.0} tables' constant buffers read by the card in place ({:.1} KB not copied, \
             no step at submission); copied as before: {:.0} not in place, {:.0} not on 16 bytes; {:.0} title slots' constant buffers as raw views",
            s[0],
            s[1] / 1024.0,
            s[2],
            s[3],
            s[4],
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_small_shader_written_copies_are_read_in_place_of_a_copy() {
        assert!(written_fits(false, false, true, 0x340));
        assert!(written_fits(false, false, true, SMALL_WRITTEN));
        assert!(!written_fits(false, false, true, SMALL_WRITTEN + 1), "large");
        assert!(!written_fits(false, false, false, 0x340), "never written by shaders");
        assert!(!written_fits(true, false, true, 0x340), "in place: read where it is already");
        assert!(!written_fits(false, true, true, 0x340), "a window, not a buffer's copy");
    }

    #[test]
    fn the_written_level_parses_and_is_live() {
        if std::env::var_os("VICTORY_PORT_CBV_WRITTEN").is_some() {
            return;
        }
        for (v, want) in [("2", 2), ("1", 1), ("0", 0), ("", 0)] {
            victory_core::settings::set("VICTORY_PORT_CBV_WRITTEN", v).unwrap();
            assert_eq!(WRITTEN.get(), want, "{v:?}");
        }
    }

    #[test]
    fn a_viewed_copy_is_known_by_its_bit() {
        for at in [0x1000usize, 0x7ff6_1234_5670, usize::MAX & !15] {
            let (word, bit) = viewed_bit(at);
            assert!(word < VIEWED_WORDS);
            assert_eq!(bit.count_ones(), 1);
            assert_eq!(viewed_bit(at), (word, bit));
        }
        // Neighbouring copies spread over the words.
        let words: std::collections::HashSet<usize> = (0..64usize).map(|i| viewed_bit(0x2_0000_0000 + i * 0x140).0).collect();
        assert!(words.len() > 32);
    }

    #[test]
    fn raw_views_need_sixteen_bytes() {
        assert!(raw_ok(0x3500_0010));
        assert!(raw_ok(0x3500_0100));
        assert!(!raw_ok(0x3500_0008));
        assert!(!raw_ok(0x3500_0004));
    }

    #[test]
    fn copies_switch_is_opt_in() {
        assert!(!copies_value(None));
        assert!(!copies_value(Some("")));
        assert!(!copies_value(Some("0")));
        assert!(!copies_value(Some("true")));
        assert!(copies_value(Some("1")));
        assert!(copies_value(Some(" 1 ")));
    }

    #[test]
    fn table_use_picks_in_place_host_copy_or_copy() {
        assert_eq!(table_use(true, true, false, false), TableUse::Copy);
        assert_eq!(table_use(true, true, false, true), TableUse::Copy);
        assert_eq!(table_use(true, false, true, true), TableUse::InPlace);
        assert_eq!(table_use(true, true, true, true), TableUse::InPlace);
        assert_eq!(table_use(false, true, true, true), TableUse::Copy);
        assert_eq!(table_use(true, false, true, false), TableUse::Copy);
        assert_eq!(table_use(true, true, true, false), TableUse::HostCopy);
        assert_eq!(table_use(false, true, true, false), TableUse::HostCopy);
    }
}
