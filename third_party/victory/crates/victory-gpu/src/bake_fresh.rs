//! `VICTORY_BAKE_FRESH=1` (#35, the idea of fxceless/victory-fork `b3d49db` + `082ebaf` +
//! `2983a71` `VICTORY_BAKE_SOURCES`): before a decoration bake draw's own pass, every input slot
//! it picks is checked for freshness, not only the ones still due.
//!
//! `VICTORY_BAKE_INPUTS` serves a bake's inputs that are in the texture queue. A slot that left
//! the queue is taken as current, yet it can be stale in two ways the queue does not see:
//! - its host slot shows another view than the one the title wrote there (null after a stream
//!   out, an older texture, or a `VICTORY_KEEP_SHOWN` stand-in of the same kind);
//! - it is made from the present view, but the title rewrote the texture's memory after the
//!   upload and the look-again (`VICTORY_HEAP_RECHECK`) has not come round yet (its fingerprint
//!   differs now).
//!
//! Batch 5B `05_base` had a black waitress face with no slot due and no `bake why: pending`: the
//! bake read a slot of this kind. With the switch such a slot is made due now, so the bake's own
//! pass uploads it (outside every budget) before the draw. A slot that showed null is shown at
//! once after that pass (before, every list read zeros from it; now zeros until the upload runs,
//! then the texture), which the general `VICTORY_BAKE_PUBLISH` did for every slot and blackened
//! faces by replacing valid old textures. A slot that showed another texture keeps it until its
//! list has run (`VICTORY_SHOW_AFTER`): such a bake, and one whose input is still unmapped or
//! queued, is counted `incomplete` and named in a line (what `VICTORY_BAKE_REPLAY` would redraw).
//!
//! `VICTORY_BAKE_HELD_NULL=1` (with this switch): a slot that was still queued, that the bake's
//! own pass uploaded now, and whose host slot still shows null is shown at once as well, as a
//! stale null one is. Without it `VICTORY_SHOW_AFTER` holds its new view until the list has run,
//! so the bake reads zeros from it: part H/I `bake check` inputs `null+held_here` (b5h 51 of the
//! bad slots read). Nothing valid is replaced: the slot showed nothing.
//!
//! `VICTORY_BAKE_ROOM=1` (the idea of TheSmurphilicious `1e59516195`): a bake's own input that
//! needs a new host copy while the card is past its room line (`crate::resource::room`) is made
//! all the same while the system's budget for the card is not used up. Off: refused (counted as
//! `room_refused` in the same line).

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// Live: read at every bake draw.
pub(crate) static FRESH: victory_core::settings::Live =
    victory_core::settings::Live::flag("VICTORY_BAKE_FRESH");

/// Live: read for a bake's own input refused for room.
pub(crate) static ROOM: victory_core::settings::Live =
    victory_core::settings::Live::flag("VICTORY_BAKE_ROOM");

/// Live: read after each bake draw's passes.
pub(crate) static HELD_NULL: victory_core::settings::Live =
    victory_core::settings::Live::flag("VICTORY_BAKE_HELD_NULL");

pub(crate) fn on() -> bool {
    FRESH.get_bool()
}

/// Whether a bake input's held show is shown before the draw: one that showed null before the
/// pass, and with `held_null` (`VICTORY_BAKE_HELD_NULL`) one taken from the queue whose host slot
/// still shows null after it (`shows_null`).
pub(crate) fn publishes(before: Fresh, held_null: bool, shows_null: bool) -> bool {
    match before {
        Fresh::StaleNull => true,
        Fresh::Queued => held_null && shows_null,
        _ => false,
    }
}

/// What a bake input slot is before the bake's own pass.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Fresh {
    /// Not a texture view (null, buffer): nothing to check.
    NotTexture,
    /// In the queue: the bake's own pass serves it anyway.
    Queued,
    /// Its memory is not mapped: nothing to upload.
    Unmapped,
    /// Its host slot shows null.
    StaleNull,
    /// Its host slot shows another view (an older texture or a stand-in).
    StaleOther,
    /// Made from its present view, but its memory holds other bytes than at its upload.
    OtherBytes,
    /// Current as far as can be told.
    Ok,
}

impl Fresh {
    /// Made due now, so the bake's own pass uploads it.
    pub(crate) fn refresh(self) -> bool {
        matches!(
            self,
            Fresh::StaleNull | Fresh::StaleOther | Fresh::OtherBytes
        )
    }

    fn name(self) -> &'static str {
        match self {
            Fresh::NotTexture => "not_texture",
            Fresh::Queued => "queued",
            Fresh::Unmapped => "unmapped",
            Fresh::StaleNull => "stale_null",
            Fresh::StaleOther => "stale_other",
            Fresh::OtherBytes => "other_bytes",
            Fresh::Ok => "ok",
        }
    }
}

/// What is known of an input slot before the pass.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Look {
    pub(crate) queued: bool,
    pub(crate) texture: bool,
    /// Bytes of its memory mapped (a texture object: 1).
    pub(crate) mapped: u64,
    /// The host slot is made from the view the title holds there now.
    pub(crate) made_from: bool,
    /// The host slot shows null.
    pub(crate) made_null: bool,
    /// Its memory's fingerprint differs from the one at its last whole upload (None: unknown).
    pub(crate) print_changed: Option<bool>,
}

pub(crate) fn classify(l: &Look) -> Fresh {
    if !l.texture {
        Fresh::NotTexture
    } else if l.queued {
        Fresh::Queued
    } else if l.mapped == 0 {
        Fresh::Unmapped
    } else if !l.made_from && l.made_null {
        Fresh::StaleNull
    } else if !l.made_from {
        Fresh::StaleOther
    } else if l.print_changed == Some(true) {
        Fresh::OtherBytes
    } else {
        Fresh::Ok
    }
}

/// What the bake reads from an input slot once its passes are over.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum After {
    /// Current.
    Current,
    /// It showed null and its new view is shown now.
    Published,
    /// Its new view waits for its list to run: the bake reads the old one.
    Held,
    /// Still in the queue (unmapped, partly mapped, or refused for room).
    Left,
    /// Not mapped and not queued.
    Unmapped,
}

impl After {
    fn name(self) -> &'static str {
        match self {
            After::Current => "current",
            After::Published => "published",
            After::Held => "held",
            After::Left => "left",
            After::Unmapped => "unmapped",
        }
    }

    pub(crate) fn incomplete(self) -> bool {
        matches!(self, After::Held | After::Left | After::Unmapped)
    }
}

/// What the bake reads from a slot: `published` (its show was committed now), `held` (a show of
/// it waits in this list), `queued` (still in the queue after the passes), as it was before.
pub(crate) fn after(before: Fresh, published: bool, held: bool, queued: bool) -> After {
    if published {
        After::Published
    } else if held {
        After::Held
    } else if queued {
        After::Left
    } else if before == Fresh::Unmapped {
        After::Unmapped
    } else {
        After::Current
    }
}

/// Whether a bake's own input that needs a new host copy is made with the card past its room
/// line: with the switch, while the card's memory in use (`memory`: budget, used) is under the
/// system's budget.
pub(crate) fn room_open(on: bool, memory: Option<[u64; 4]>) -> bool {
    on && memory.is_some_and(|[budget, used, ..]| used < budget)
}

/// The texture pass is about to refuse a slot for room: whether it goes on with it (a bake's
/// own input, `pinned`, with `VICTORY_BAKE_ROOM=1` and the card under its budget). Counted.
#[cfg(windows)]
pub(crate) fn room_lets(pinned: bool) -> bool {
    if !pinned {
        return false;
    }
    let open = room_open(
        ROOM.get_bool(),
        crate::host::host().and_then(|h| h.video_memory()),
    );
    add(if open { C::RoomMade } else { C::RoomRefused }, 1);
    open
}

#[derive(Clone, Copy)]
#[repr(usize)]
pub(crate) enum C {
    Bakes,
    Inputs,
    Queued,
    Unmapped,
    StaleNull,
    StaleOther,
    OtherBytes,
    Refreshed,
    Published,
    Moved,
    Held,
    Left,
    Incomplete,
    RoomRefused,
    RoomMade,
}
const N: usize = 15;
const NAMES: [&str; N] = [
    "bakes",
    "inputs",
    "queued",
    "unmapped",
    "stale_null",
    "stale_other",
    "other_bytes",
    "refreshed",
    "published",
    "moved",
    "held",
    "left",
    "incomplete",
    "room_refused",
    "room_made",
];
static COUNTS: [AtomicU64; N] = [const { AtomicU64::new(0) }; N];
static FRAME: AtomicU64 = AtomicU64::new(0);
static LINES: AtomicU64 = AtomicU64::new(0);
const MAX_LINES: u64 = 20_000;
/// Lines naming an incomplete bake, at most.
const MAX_BAKE_LINES: u64 = 256;
static BAKE_LINES: AtomicU64 = AtomicU64::new(0);

pub(crate) fn add(c: C, n: u64) {
    roll();
    COUNTS[c as usize].fetch_add(n, Relaxed);
}

/// The counter of a slot's state before the pass.
pub(crate) fn count(f: Fresh) {
    let c = match f {
        Fresh::Queued => C::Queued,
        Fresh::Unmapped => C::Unmapped,
        Fresh::StaleNull => C::StaleNull,
        Fresh::StaleOther => C::StaleOther,
        Fresh::OtherBytes => C::OtherBytes,
        Fresh::NotTexture | Fresh::Ok => return,
    };
    add(c, 1);
}

/// The line for a frame with bake draws (or room counts), at the first count of the next.
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
    tracing::info!(frame = prev, counts = %line(&c), "bake fresh frame");
}

fn line(c: &[u64; N]) -> String {
    NAMES
        .iter()
        .zip(c)
        .map(|(name, n)| format!("{name}={n}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// The slots of a bake, each as `slot:before>after`.
pub(crate) fn slots_text(slots: &[(u32, Fresh, After)]) -> String {
    slots
        .iter()
        .map(|&(slot, f, a)| format!("{slot}:{}>{}", f.name(), a.name()))
        .collect::<Vec<_>>()
        .join(" ")
}

/// A bake ran with an input not current: counted, and named (the first [`MAX_BAKE_LINES`]).
pub(crate) fn incomplete(frame: u64, prog: &str, slots: &[(u32, Fresh, After)]) {
    add(C::Incomplete, 1);
    if BAKE_LINES.fetch_add(1, Relaxed) < MAX_BAKE_LINES {
        tracing::info!(
            "bake fresh: frame {frame} bake {prog} incomplete inputs=[{}]",
            slots_text(slots)
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn look() -> Look {
        Look {
            texture: true,
            mapped: 4096,
            made_from: true,
            ..Look::default()
        }
    }

    /// Batch 5B `05_base`: a black face with nothing due. A slot out of the queue that shows
    /// another view, or whose memory changed since its upload, is not current: it is refreshed.
    #[test]
    fn a_slot_out_of_the_queue_can_still_be_stale() {
        assert_eq!(classify(&look()), Fresh::Ok);
        let stale = Look {
            made_from: false,
            ..look()
        };
        assert_eq!(classify(&stale), Fresh::StaleOther);
        assert!(classify(&stale).refresh());
        let null = Look {
            made_null: true,
            ..stale
        };
        assert_eq!(classify(&null), Fresh::StaleNull);
        assert!(classify(&null).refresh());
        let rewritten = Look {
            print_changed: Some(true),
            ..look()
        };
        assert_eq!(classify(&rewritten), Fresh::OtherBytes);
        assert!(classify(&rewritten).refresh());
        let same = Look {
            print_changed: Some(false),
            ..look()
        };
        assert_eq!(classify(&same), Fresh::Ok);
        assert!(!classify(&same).refresh());
    }

    /// Queued slots are the pass's already; unmapped ones and non-textures have nothing to upload.
    #[test]
    fn queued_unmapped_and_non_textures_are_not_refreshed() {
        let queued = Look {
            queued: true,
            made_from: false,
            ..look()
        };
        assert_eq!(classify(&queued), Fresh::Queued);
        let away = Look {
            mapped: 0,
            made_from: false,
            ..look()
        };
        assert_eq!(classify(&away), Fresh::Unmapped);
        assert_eq!(classify(&Look::default()), Fresh::NotTexture);
        for f in [Fresh::Queued, Fresh::Unmapped, Fresh::NotTexture, Fresh::Ok] {
            assert!(!f.refresh(), "{f:?}");
        }
    }

    /// A slot whose show is held back, still queued or unmapped makes the bake incomplete.
    #[test]
    fn what_the_bake_reads_after_its_passes() {
        assert_eq!(
            after(Fresh::StaleNull, true, false, false),
            After::Published
        );
        assert_eq!(after(Fresh::StaleOther, false, true, false), After::Held);
        assert_eq!(after(Fresh::Queued, false, false, true), After::Left);
        assert_eq!(after(Fresh::Unmapped, false, false, false), After::Unmapped);
        assert_eq!(
            after(Fresh::OtherBytes, false, false, false),
            After::Current
        );
        assert!(!After::Current.incomplete());
        assert!(!After::Published.incomplete());
        for a in [After::Held, After::Left, After::Unmapped] {
            assert!(a.incomplete(), "{a:?}");
        }
    }

    /// Part H/I: `null+held_here` inputs were queued slots the bake's own pass uploaded, shown only
    /// after the list ran. With the switch they are shown before the draw, like stale nulls;
    /// a queued slot showing a texture keeps it (`VICTORY_BAKE_PUBLISH` blackened faces that way).
    #[test]
    fn a_held_null_input_is_shown_before_the_bake_only_with_the_switch() {
        assert!(publishes(Fresh::StaleNull, false, false));
        assert!(!publishes(Fresh::Queued, false, true), "off: held until the list ran");
        assert!(publishes(Fresh::Queued, true, true));
        assert!(!publishes(Fresh::Queued, true, false), "it shows a texture: kept");
        for f in [Fresh::StaleOther, Fresh::OtherBytes, Fresh::Ok, Fresh::Unmapped, Fresh::NotTexture] {
            assert!(!publishes(f, true, true), "{f:?}");
        }
    }

    #[test]
    fn held_null_is_live() {
        victory_core::settings::check_live("VICTORY_BAKE_HELD_NULL", "1", || HELD_NULL.get_bool(), false, true);
    }

    /// `VICTORY_BAKE_ROOM`: past the room line only while the card is under the system's budget.
    #[test]
    fn room_for_a_bake_only_under_the_budget() {
        let gib = 1u64 << 30;
        assert!(!room_open(false, Some([32 * gib, 20 * gib, 0, 0])));
        assert!(room_open(true, Some([32 * gib, 20 * gib, 0, 0])));
        assert!(!room_open(true, Some([32 * gib, 32 * gib, 0, 0])));
        assert!(!room_open(true, None));
    }

    #[test]
    fn the_lines_name_every_count_and_slot() {
        let mut c = [0u64; N];
        c[C::Bakes as usize] = 2;
        c[C::RoomMade as usize] = 1;
        assert_eq!(
            line(&c),
            "bakes=2 inputs=0 queued=0 unmapped=0 stale_null=0 stale_other=0 other_bytes=0 refreshed=0 published=0 moved=0 held=0 left=0 incomplete=0 room_refused=0 room_made=1"
        );
        assert_eq!(
            slots_text(&[
                (7, Fresh::StaleOther, After::Held),
                (9, Fresh::StaleNull, After::Published)
            ]),
            "7:stale_other>held 9:stale_null>published"
        );
    }
}
