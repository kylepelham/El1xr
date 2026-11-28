//! `VICTORY_PRELUDE_OWN_COPY=1` (off by default, live) and its diagnostic
//! `VICTORY_CARD_KEPT_WATCH=1` (off by default, live).
//!
//! With `VICTORY_PRELUDE_RECHECK=1` (on in the launcher), a prelude upload of
//! a texture the host wrote after the list was recorded is left out: another
//! list's card copy into it must not be overwritten by stale title memory.
//! But the list's own copy counts too. The first `CopyTextureRegion` into a
//! title texture records an upload of its title memory ahead of itself
//! (`list.rs`, `VICTORY_COPY_OVER_TITLE`), then marks the texture card
//! written; at submission the recheck finds the mark and drops that upload.
//! The host copy then holds only what the copy wrote, and no later list
//! uploads a card-written texture: the rest stays empty (black) for good.
//! A streamer that makes a texture and copies its resident small mips into
//! it on the card is exactly this (issue #55, "goes dark after a load and
//! never comes back"). The prelude runs before the list's own commands, so
//! its upload cannot land over that list's copy.
//!
//! The switch counts each list's host writes of each texture, and a prelude
//! upload is left out only when a write by another list (or by an output
//! with no list: upscaler, external) came after the list's read was
//! recorded. Off, the recheck is as before.
//!
//! The watch logs every 30 frames: `card kept watch:` the card-written
//! textures the sweeps keep (they never let such a copy go: `resource.rs`
//! `let_go_idle`, `slot_evict_with`, `idle_sweep::sweep`) and how many of
//! those were never filled from title memory; the prelude uploads the
//! recheck left out since the last line, by the list's own copy or by
//! another's, and of them those into a copy never filled; and how many this
//! switch let through. Up to [`EXAMPLES`] `card kept watch: texture` lines
//! name such textures (size, format, mips, frame).

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// `card kept watch: texture` lines in a run.
const EXAMPLES: u64 = 48;
/// Frames between `card kept watch:` lines.
const EVERY_FRAMES: u64 = 30;

/// Host writes of a title texture: in all, and by each list that recorded
/// some (by the list's address; lists are pooled, not freed).
#[derive(Default)]
struct Writes {
    total: u64,
    by_list: Vec<(usize, u64)>,
}

impl Writes {
    fn by(&self, list: usize) -> u64 {
        self.by_list
            .iter()
            .find(|(l, _)| *l == list)
            .map_or(0, |(_, n)| *n)
    }
}

static WRITES: Mutex<Option<HashMap<usize, Writes>>> = Mutex::new(None);

/// Prelude uploads the recheck left out by the list's own copy, by
/// another's, of them into copies never filled from title memory, and those
/// this switch let through; then the frame of the last line.
static LEFT_OUT: [AtomicU64; 5] = [const { AtomicU64::new(0) }; 5];
const OWN: usize = 0;
const OTHER: usize = 1;
const NEVER_FILLED: usize = 2;
const LET_THROUGH: usize = 3;
const LAST_LINE: usize = 4;

/// Examples said, and the textures they named.
static SAID: AtomicU64 = AtomicU64::new(0);
static NAMED: Mutex<Option<HashSet<usize>>> = Mutex::new(None);

/// The fix (live).
pub(crate) fn on() -> bool {
    static ON: victory_core::settings::Live =
        victory_core::settings::Live::flag("VICTORY_PRELUDE_OWN_COPY");
    static TOLD: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    let on = ON.get_bool();
    if on && !TOLD.swap(true, Relaxed) {
        tracing::info!(
            "VICTORY_PRELUDE_OWN_COPY=1: a prelude upload is left out only for another list's card write, not the list's own copy"
        );
    }
    on
}

/// The watch (live).
pub(crate) fn watch() -> bool {
    static ON: victory_core::settings::Live =
        victory_core::settings::Live::flag("VICTORY_CARD_KEPT_WATCH");
    ON.get_bool()
}

fn tracked() -> bool {
    on() || watch()
}

/// What a list's read of a texture saw of its host writes when recorded:
/// the list, the writes in all and the list's own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Seen {
    list: usize,
    total: u64,
    own: u64,
}

/// The host wrote title texture `resource` in list `list` (0: no list, an
/// upscaler's or an external output).
pub(crate) fn wrote(resource: usize, list: usize) {
    if tracked() {
        count(resource, list);
    }
}

fn count(resource: usize, list: usize) {
    let mut map = WRITES.lock().unwrap_or_else(|e| e.into_inner());
    let w = map
        .get_or_insert_with(HashMap::new)
        .entry(resource)
        .or_default();
    w.total += 1;
    if list == 0 {
        return;
    }
    match w.by_list.iter_mut().find(|(l, _)| *l == list) {
        Some((_, n)) => *n += 1,
        None => w.by_list.push((list, 1)),
    }
}

/// List `list` records a read of `resource`: what it saw (`None` with
/// neither switch on: the recheck keeps the old rule for it).
pub(crate) fn seen(resource: usize, list: usize) -> Option<Seen> {
    tracked().then(|| snapshot(resource, list))
}

fn snapshot(resource: usize, list: usize) -> Seen {
    let map = WRITES.lock().unwrap_or_else(|e| e.into_inner());
    let w = map.as_ref().and_then(|m| m.get(&resource));
    Seen {
        list,
        total: w.map_or(0, |w| w.total),
        own: w.map_or(0, |w| w.by(list)),
    }
}

/// Host writes of `resource` since `seen` by anything but `seen`'s list.
pub(crate) fn others_since(resource: usize, seen: &Seen) -> u64 {
    let map = WRITES.lock().unwrap_or_else(|e| e.into_inner());
    let Some(w) = map.as_ref().and_then(|m| m.get(&resource)) else {
        return 0;
    };
    let all = w.total.saturating_sub(seen.total);
    let own = w.by(seen.list).saturating_sub(seen.own);
    all.saturating_sub(own)
}

/// Whether the recheck leaves out a prelude upload of a texture the host
/// wrote (`host_wrote`), whose card writes by subresource are still those
/// the read saw (`card_same`), with `others` writes by other lists since
/// the read (`None`: not counted), the switch `own_copy`.
pub(crate) fn leave_out(
    host_wrote: bool,
    card_same: bool,
    others: Option<u64>,
    own_copy: bool,
) -> bool {
    host_wrote && !card_same && !(own_copy && others == Some(0))
}

/// The recheck found a host write after the read of `resource` was
/// recorded: counted by whose (`others`: writes by other lists since,
/// `None` unknown and counted as another's), whether its host copy was
/// ever filled from title memory, and whether the upload was let through;
/// `describe` names the texture for an example.
pub(crate) fn rechecked(
    resource: usize,
    others: Option<u64>,
    never_filled: bool,
    let_through: bool,
    describe: impl FnOnce() -> String,
) {
    let own = others == Some(0);
    if let_through {
        LEFT_OUT[LET_THROUGH].fetch_add(1, Relaxed);
        return;
    }
    LEFT_OUT[if own { OWN } else { OTHER }].fetch_add(1, Relaxed);
    if never_filled {
        LEFT_OUT[NEVER_FILLED].fetch_add(1, Relaxed);
    }
    if never_filled && watch() {
        example(
            resource,
            if own {
                "prelude upload left out by the list's own copy"
            } else {
                "prelude upload left out by another list's write"
            },
            describe,
        );
    }
}

/// One `card kept watch: texture` line for `resource`, once per texture, up
/// to [`EXAMPLES`] in a run.
pub(crate) fn example(resource: usize, why: &str, describe: impl FnOnce() -> String) {
    if SAID.load(Relaxed) >= EXAMPLES {
        return;
    }
    let new = NAMED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_or_insert_with(HashSet::new)
        .insert(resource);
    if !new || SAID.fetch_add(1, Relaxed) >= EXAMPLES {
        return;
    }
    tracing::info!(
        resource = format_args!("{resource:#x}"),
        texture = %describe(),
        frame = crate::frame::current(),
        why,
        "card kept watch: texture (card written, never filled from title memory: what the card did not write stays empty)"
    );
}

/// Whether a `card kept watch:` line is due at `frame` (claims it).
pub(crate) fn line_due(frame: u64) -> bool {
    let last = LEFT_OUT[LAST_LINE].load(Relaxed);
    frame >= last + EVERY_FRAMES
        && LEFT_OUT[LAST_LINE]
            .compare_exchange(last, frame, Relaxed, Relaxed)
            .is_ok()
}

/// The recheck's counts since the last line: own, other, never filled, let
/// through (taken).
pub(crate) fn take_counts() -> [u64; 4] {
    [OWN, OTHER, NEVER_FILLED, LET_THROUGH].map(|i| LEFT_OUT[i].swap(0, Relaxed))
}

/// The title released `resource`.
pub(crate) fn forget(resource: usize) {
    let mut map = WRITES.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(map) = map.as_mut() {
        map.remove(&resource);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Keys no real texture has (the tests share the table).
    const A: usize = 0xC0DE_0101;
    const B: usize = 0xC0DE_0102;
    const L1: usize = 0x1000;
    const L2: usize = 0x2000;

    #[test]
    fn the_lists_own_copy_is_not_another_write() {
        let seen = snapshot(A, L1);
        count(A, L1);
        count(A, L1);
        assert_eq!(others_since(A, &seen), 0, "only the list's own copies");
        count(A, L2);
        assert_eq!(others_since(A, &seen), 1, "another list's copy");
        count(A, 0);
        assert_eq!(
            others_since(A, &seen),
            2,
            "an output with no list is another's"
        );
        forget(A);
        assert_eq!(others_since(A, &seen), 0, "forgotten");
    }

    #[test]
    fn writes_before_the_read_do_not_count() {
        count(B, L2);
        count(B, L1);
        let seen = snapshot(B, L1);
        assert_eq!(others_since(B, &seen), 0);
        count(B, L1);
        assert_eq!(others_since(B, &seen), 0);
        forget(B);
    }

    #[test]
    fn the_recheck_leaves_out_only_anothers_write_with_the_switch() {
        // Off: any host write after the read leaves the upload out, as before.
        assert!(leave_out(true, false, Some(0), false));
        assert!(leave_out(true, false, None, false));
        // On: the list's own copy does not; another's, or one not counted, does.
        assert!(!leave_out(true, false, Some(0), true));
        assert!(leave_out(true, false, Some(1), true));
        assert!(leave_out(true, false, None, true));
        // No host write, or card writes as the read saw them: never left out.
        assert!(!leave_out(false, false, Some(3), false));
        assert!(!leave_out(true, true, Some(3), false));
    }
}
