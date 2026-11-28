//! `VICTORY_CARD_MIPS_TRACK=1` (off by default, live): a title texture the
//! card wrote only some subresources of (a `CopyTextureRegion` or resolve
//! into those mips or array slices) keeps the card's contents in those, and
//! takes the others from title memory again when that memory changes.
//!
//! Without it, any card write makes the whole texture the card's for good
//! (`upload_from`'s shortcut): its title memory is never read again. A
//! streamer that makes a texture, copies its resident small mips into it on
//! the card and then writes the large mips into title memory left those as
//! they were at the copy (zeros or stale) for the rest of the run: a zero
//! normal map turns a sunlit surface dark with its albedo intact (Kyle's
//! prison window sill, issue #55).
//!
//! Every card write is noted by subresource here, switch on or off (a lock
//! per copy, a few dozen a frame). With the switch on, a texture whose card
//! writes cover some of its subresources is uploaded as any title texture
//! is (staged, checked for changes by write-watch or guard at submission,
//! never hashed on the recording thread), and its prelude copies skip the
//! subresources the card had written when the read was recorded. A whole
//! write (`CopyResource`, an upscaler's or an external output), or one this
//! cannot name the subresource of, keeps the old rule for that texture.
//!
//! Logged: `card mips: uploads` once a second while any happen (uploads,
//! subresources copied and skipped, textures), and up to [`EXAMPLES`]
//! `card mips: texture` lines (size, format, mips, layers, the subresources
//! the card owns).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};

/// Subresources past this are not tracked: a write there makes the texture
/// the card's whole.
const MOST_SUBRESOURCES: u32 = 64 * 32;
/// `card mips: texture` lines in a run.
const EXAMPLES: u64 = 48;

/// The subresources a texture's card writes covered, a bit each; `None`:
/// all of them, or a write that did not say which.
type Owned = Option<Arc<[u64]>>;

static OWNED: Mutex<Option<HashMap<usize, Owned>>> = Mutex::new(None);

/// Uploads recorded with subresources skipped, subresources copied,
/// subresources skipped, and the frame of the last `card mips: uploads` line.
static COUNTS: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];
/// `card mips: texture` lines said.
static SAID: AtomicU64 = AtomicU64::new(0);

/// The switch (live).
pub(crate) fn on() -> bool {
    static ON: victory_core::settings::Live =
        victory_core::settings::Live::flag("VICTORY_CARD_MIPS_TRACK");
    static TOLD: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    let on = ON.get_bool();
    if on && !TOLD.swap(true, Relaxed) {
        tracing::info!(
            "VICTORY_CARD_MIPS_TRACK=1: a texture the card wrote some mips of takes the others from title memory"
        );
    }
    on
}

/// The card wrote subresource `sub` of title texture `resource` (`None`:
/// every subresource, or one this cannot name).
pub(crate) fn wrote(resource: usize, sub: Option<u32>) {
    let mut map = OWNED.lock().unwrap_or_else(|e| e.into_inner());
    let map = map.get_or_insert_with(HashMap::new);
    match sub.filter(|&s| s < MOST_SUBRESOURCES) {
        None => {
            map.insert(resource, None);
        }
        Some(sub) => {
            let owned = map
                .entry(resource)
                .or_insert_with(|| Some(Arc::from(Vec::new())));
            if let Some(bits) = owned
                && !owns(bits, sub)
            {
                *owned = Some(with(bits, sub));
            }
        }
    }
}

/// `bits` and `sub`: a new set (sets handed out are never changed, so a
/// snapshot is a reference count).
fn with(bits: &[u64], sub: u32) -> Arc<[u64]> {
    let word = (sub / 64) as usize;
    let mut v = bits.to_vec();
    if v.len() <= word {
        v.resize(word + 1, 0);
    }
    v[word] |= 1 << (sub % 64);
    Arc::from(v)
}

/// Whether `bits` holds subresource `sub`.
pub(crate) fn owns(bits: &[u64], sub: u32) -> bool {
    bits.get((sub / 64) as usize)
        .is_some_and(|w| w & 1 << (sub % 64) != 0)
}

/// Of `bits`, the subresources below `subresources`.
fn held(bits: &[u64], subresources: u32) -> u32 {
    (0..subresources).filter(|&s| owns(bits, s)).count() as u32
}

/// The subresources the card wrote of `resource` (`subresources` of them
/// in all), when it wrote some and not all; `None`: none noted, all, or
/// unknown which (the texture keeps the card's contents).
pub(crate) fn partly(resource: usize, subresources: u32) -> Option<Arc<[u64]>> {
    let map = OWNED.lock().unwrap_or_else(|e| e.into_inner());
    let bits = map.as_ref()?.get(&resource)?.as_ref()?;
    let n = held(bits, subresources);
    (n > 0 && n < subresources).then(|| Arc::clone(bits))
}

/// Whether `resource`'s card writes are still `seen` (no subresource
/// written since that snapshot was taken).
pub(crate) fn same(resource: usize, seen: &Arc<[u64]>) -> bool {
    let map = OWNED.lock().unwrap_or_else(|e| e.into_inner());
    map.as_ref()
        .and_then(|m| m.get(&resource))
        .and_then(Option::as_ref)
        .is_some_and(|bits| Arc::ptr_eq(bits, seen))
}

/// The title released `resource`.
pub(crate) fn forget(resource: usize) {
    let mut map = OWNED.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(map) = map.as_mut() {
        map.remove(&resource);
    }
}

/// A prelude recorded the upload of `resource` with `copied` subresources
/// from title memory and `skipped` the card owns (`owned`); `describe`
/// names the texture for an example line.
pub(crate) fn uploaded(
    resource: usize,
    owned: &[u64],
    copied: u64,
    skipped: u64,
    describe: impl FnOnce() -> String,
) {
    COUNTS[0].fetch_add(1, Relaxed);
    COUNTS[1].fetch_add(copied, Relaxed);
    COUNTS[2].fetch_add(skipped, Relaxed);
    if SAID.load(Relaxed) < EXAMPLES && SAID.fetch_add(1, Relaxed) < EXAMPLES {
        let subs: Vec<u32> = (0..MOST_SUBRESOURCES).filter(|&s| owns(owned, s)).collect();
        tracing::info!(
            resource = format_args!("{resource:#x}"),
            texture = %describe(),
            copied,
            skipped,
            card_owned = ?subs,
            "card mips: texture (card-written subresources kept, the others uploaded from title memory)"
        );
    }
    let frame = crate::frame::current();
    let last = COUNTS[3].load(Relaxed);
    if frame >= last + 30
        && COUNTS[3]
            .compare_exchange(last, frame, Relaxed, Relaxed)
            .is_ok()
    {
        let textures = OWNED
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map_or(0, |m| m.values().filter(|o| o.is_some()).count());
        tracing::info!(
            frame,
            uploads = COUNTS[0].swap(0, Relaxed),
            copied = COUNTS[1].swap(0, Relaxed),
            skipped = COUNTS[2].swap(0, Relaxed),
            partly_written = textures,
            "card mips: uploads (since the last line, of textures the card wrote some subresources of)"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Resource keys no real texture has (the tests share the table).
    const A: usize = 0xC0DE_0001;
    const B: usize = 0xC0DE_0002;
    const C: usize = 0xC0DE_0003;
    const D: usize = 0xC0DE_0004;

    #[test]
    fn some_mips_written_leave_the_others_to_title_memory() {
        // A streamer copies the small mips 8..11 of a 12-mip texture on the card.
        for mip in 8..12 {
            wrote(A, Some(mip));
        }
        let owned = partly(A, 12).expect("partly the card's");
        assert!((0..8).all(|m| !owns(&owned, m)));
        assert!((8..12).all(|m| owns(&owned, m)));
        forget(A);
        assert!(partly(A, 12).is_none());
    }

    #[test]
    fn a_whole_or_unnamed_write_keeps_the_old_rule() {
        wrote(B, Some(0));
        wrote(B, None);
        wrote(B, Some(3));
        assert!(partly(B, 12).is_none(), "a whole write stays whole");
        wrote(C, Some(MOST_SUBRESOURCES));
        assert!(
            partly(C, 12).is_none(),
            "an untracked subresource is a whole write"
        );
        forget(B);
        forget(C);
    }

    #[test]
    fn every_subresource_written_is_the_cards_and_snapshots_stay() {
        wrote(D, Some(0));
        let first = partly(D, 2).expect("one of two");
        assert!(same(D, &first));
        wrote(D, Some(0));
        assert!(same(D, &first), "an old subresource again changes nothing");
        wrote(D, Some(1));
        assert!(!same(D, &first), "a new subresource is a new set");
        assert!(
            owns(&first, 0) && !owns(&first, 1),
            "the snapshot is as it was"
        );
        assert!(partly(D, 2).is_none(), "all of it: the card's");
        // An array texture: slice 1's mip 0 of 2 slices x 3 mips is subresource 3.
        assert!(partly(D, 6).is_some());
        forget(D);
    }
}
