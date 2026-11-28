//! `VICTORY_BAKE_PUBLISH` (#36; the idea of trustno1-coder/VIctory-NVIDIA `545f890`, codec_d's
//! bake-slot publication): a decoration bake draw's texture slots whose new descriptor
//! `VICTORY_SHOW_AFTER` still holds back are shown before the draw.
//!
//! With `VICTORY_SHOW_AFTER` (on in the launcher) the texture pass records a slot's upload into
//! the list but writes the slot's live descriptor only once that list's submission has run
//! (`draw::State::pending_shows`, committed by `tables_done` or the list's drop). A bake draw
//! (`PS_BakeDecorations_*`, `PS_DecorationBlend_*`) picks its textures by index from the
//! shader-visible heap when the card runs it. Recorded in the same list right after its texture
//! passes, it reads each newly uploaded slot's old descriptor: null (zeros: a black face) or the
//! texture the slot showed before (a wrong outfit). The title keeps what it baked until it bakes
//! again. The slot has left the texture queue, so `bake inputs frame` counts it as uploaded and
//! not as short. With the relay (`VICTORY_RELAY=1|2`), a list's shows are committed only when
//! its flight lands and the landed lists are recycled (`host_late::land`), so bakes in later
//! lists read the old descriptor until then too.
//!
//! `1`: the bake's input slots held back in its own list are shown at once. Their uploads are
//! recorded earlier in the same list, so on the card they run before the draw. `2`: also those
//! held back by another list (its upload was recorded in a list that is in the air, or not sent
//! yet if the title sends its lists out of order). Off: nothing is shown before its list has run,
//! as before.
//!
//! `VICTORY_BAKE_KEEP_GONE=1` (`keeps_gone`): a bake input unmapped at record time keeps its slot.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};

/// Live: read at every bake draw and every held-back show.
pub(crate) static MODE: victory_core::settings::Live =
    victory_core::settings::Live::new("VICTORY_BAKE_PUBLISH", parse);

fn parse(v: Option<&str>) -> u64 {
    match v.map(str::trim) {
        Some("1") => 1,
        Some("2") => 2,
        _ => 0,
    }
}

/// 0 off, 1 the bake's own list, 2 every list.
pub(crate) fn mode() -> u64 {
    MODE.get()
}

/// `VICTORY_BAKE_KEEP_GONE=1` (live): a bake draw's own input (pinned by `VICTORY_BAKE_INPUTS`)
/// whose memory the title has not mapped when the draw is recorded keeps what its slot showed.
/// The texture pass lets go of a texture streamed out (`HostHeap::texture_slot_gone`), and that
/// writes a null view into the shader-visible slot at once: the bake, run on the card later,
/// reads zeros, a black face the title keeps until it bakes again. The title may map the pages
/// after it records the draw and before it submits; the slot stays due either way.
pub(crate) static KEEP_GONE: victory_core::settings::Live =
    victory_core::settings::Live::flag("VICTORY_BAKE_KEEP_GONE");

/// Whether the texture pass keeps the slot of an unmapped texture (`pinned`: a bake's own input).
pub(crate) fn keeps_gone(pinned: bool) -> bool {
    if !pinned || !KEEP_GONE.get_bool() {
        return false;
    }
    add(C::KeptGone, 1);
    true
}

/// `2`: the shows held back by any list, by (title heap, slot): the slot's descriptor handle and
/// the record key its show waits for.
static HELD: Mutex<BTreeMap<(usize, u32), (u64, u64)>> = Mutex::new(BTreeMap::new());
static HELD_LEN: AtomicUsize = AtomicUsize::new(0);

/// A list holds back the show of `index` (`VICTORY_SHOW_AFTER`). Kept only with `2` (or with
/// `VICTORY_BAKE_CHECK`, which only looks).
pub(crate) fn held(heap: usize, index: u32, handle: u64, key: u64) {
    if mode() != 2 && !crate::bake_check::on() {
        return;
    }
    let mut held = HELD.lock().unwrap_or_else(|e| e.into_inner());
    held.insert((heap, index), (handle, key));
    HELD_LEN.store(held.len(), Relaxed);
}

/// The list's show of `index` was committed, or given back to the queue (a list reset unsent).
pub(crate) fn released(heap: usize, index: u32) {
    if HELD_LEN.load(Relaxed) == 0 {
        return;
    }
    let mut held = HELD.lock().unwrap_or_else(|e| e.into_inner());
    held.remove(&(heap, index));
    HELD_LEN.store(held.len(), Relaxed);
}

/// (`VICTORY_BAKE_CHECK`) Whether a list holds back the show of `index`.
pub(crate) fn is_held(heap: usize, index: u32) -> bool {
    HELD_LEN.load(Relaxed) != 0
        && HELD
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(&(heap, index))
}

/// The show of `index` another list holds back, taken to be done now.
pub(crate) fn take_held(heap: usize, index: u32) -> Option<(u64, u64)> {
    if HELD_LEN.load(Relaxed) == 0 {
        return None;
    }
    let mut held = HELD.lock().unwrap_or_else(|e| e.into_inner());
    let taken = held.remove(&(heap, index));
    HELD_LEN.store(held.len(), Relaxed);
    taken
}

#[derive(Clone, Copy)]
#[repr(usize)]
pub(crate) enum C {
    /// Bake draws looked at.
    Bakes,
    /// Their texture slots (summed over the draws).
    Inputs,
    /// Slots held back in the bake's own list, shown before it.
    SameList,
    /// Slots held back by another list, shown before it (`2`).
    OtherList,
    /// Held-back slots the title had rewritten since (nothing shown).
    Moved,
    /// Bake draws whose parameters or records could not be read.
    Unread,
    /// Unmapped bake inputs whose slot was kept, not nulled (`VICTORY_BAKE_KEEP_GONE=1`).
    KeptGone,
}
const N: usize = 7;
const NAMES: [&str; N] = [
    "bakes",
    "inputs",
    "same_list",
    "other_list",
    "moved",
    "unread",
    "kept_gone",
];
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
    if c[C::Bakes as usize] == 0 && c[C::Unread as usize] == 0 && c[C::KeptGone as usize] == 0 {
        return;
    }
    if LINES.fetch_add(1, Relaxed) >= MAX_LINES {
        return;
    }
    tracing::info!(frame = prev, counts = %line(&c), "bake publish frame");
}

fn line(c: &[u64; N]) -> String {
    NAMES
        .iter()
        .zip(c)
        .map(|(name, n)| format!("{name}={n}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Of `pending` (a list's held-back shows: title heap, slot, handle, key), takes out those of
/// heap `heap` whose slot is in `slots` (sorted), in order.
pub(crate) fn take_mine(
    pending: &mut Vec<(usize, u32, u64, u64)>,
    heap: usize,
    slots: &[u32],
) -> Vec<(usize, u32, u64, u64)> {
    let mut mine = Vec::new();
    pending.retain(|&show| {
        let hit = show.0 == heap && slots.binary_search(&show.1).is_ok();
        if hit {
            mine.push(show);
        }
        !hit
    });
    mine
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keep_gone_only_for_a_bakes_own_input() {
        assert!(!keeps_gone(false));
        if std::env::var_os("VICTORY_BAKE_KEEP_GONE").is_some() {
            return;
        }
        assert!(!keeps_gone(true), "off");
        victory_core::settings::set("VICTORY_BAKE_KEEP_GONE", "1").unwrap();
        let (own, other) = (keeps_gone(true), keeps_gone(false));
        victory_core::settings::set("VICTORY_BAKE_KEEP_GONE", "").unwrap();
        assert!(own && !other);
    }

    #[test]
    fn values() {
        assert_eq!(parse(None), 0);
        assert_eq!(parse(Some("0")), 0);
        assert_eq!(parse(Some("1")), 1);
        assert_eq!(parse(Some(" 2 ")), 2);
        assert_eq!(parse(Some("3")), 0);
    }

    /// Live, and off (and `1`) keep no registry: `held` is a no-op, so the lists' commits are
    /// untouched. (One test: the setting is shared by the test threads.)
    #[test]
    fn the_switch_is_live_and_only_two_keeps_other_lists_shows() {
        assert_eq!(
            mode(),
            parse(std::env::var("VICTORY_BAKE_PUBLISH").ok().as_deref())
        );
        if std::env::var_os("VICTORY_BAKE_PUBLISH").is_some() {
            return;
        }
        let heap = 0x5eed_0000_usize;
        held(heap, 7, 0x10, 0x20);
        assert_eq!(take_held(heap, 7), None, "off: nothing kept");
        victory_core::settings::set("VICTORY_BAKE_PUBLISH", "1").unwrap();
        let now = mode();
        held(heap, 7, 0x10, 0x20);
        assert_eq!(take_held(heap, 7), None, "1: nothing kept");
        assert_eq!(now, 1, "a change takes effect while running");
        victory_core::settings::set("VICTORY_BAKE_PUBLISH", "2").unwrap();
        held(heap, 7, 0x10, 0x20);
        held(heap, 8, 0x11, 0x21);
        held(heap, 8, 0x12, 0x22);
        victory_core::settings::set("VICTORY_BAKE_PUBLISH", "").unwrap();
        assert_eq!(mode(), 0, "and clears back to the launch value");
        released(heap, 7);
        assert_eq!(take_held(heap, 7), None, "a committed show is gone");
        assert_eq!(
            take_held(heap, 8),
            Some((0x12, 0x22)),
            "the newest write of a slot"
        );
        assert_eq!(take_held(heap, 8), None, "taken once");
    }

    #[test]
    fn a_bakes_own_shows_are_taken_out_of_its_list() {
        let (a, b) = (1usize, 2usize);
        let mut pending = vec![
            (a, 5, 50, 500),
            (b, 5, 51, 501),
            (a, 9, 90, 900),
            (a, 3, 30, 300),
            (a, 5, 52, 502),
        ];
        let mine = take_mine(&mut pending, a, &[3, 5]);
        assert_eq!(
            mine,
            vec![(a, 5, 50, 500), (a, 3, 30, 300), (a, 5, 52, 502)]
        );
        assert_eq!(pending, vec![(b, 5, 51, 501), (a, 9, 90, 900)]);
        assert!(take_mine(&mut pending, a, &[]).is_empty());
        assert_eq!(pending.len(), 2);
    }

    #[test]
    fn a_frame_line_names_every_count() {
        let mut c = [0u64; N];
        c[C::Bakes as usize] = 3;
        c[C::SameList as usize] = 2;
        assert_eq!(
            line(&c),
            "bakes=3 inputs=0 same_list=2 other_list=0 moved=0 unread=0 kept_gone=0"
        );
    }
}
