//! `VICTORY_TARGET_REUSE_REREAD=1` (live, off by default): a texture the title fills itself
//! whose memory lies in the memory of a texture the host writes (a render target, a bake
//! result), or of one the title released lately, has its host copy read again from title
//! memory.
//!
//! Batch 5 part O, run 1: a character's 512x512 BC1 skin texture (heap slot 96424) moved
//! through four addresses; at three of them the face was lit, at the fourth (`0xc9f085000`,
//! inside the memory of a bake result, `in_bake_memory`) the face was black for as long as it
//! stayed there. The view never links to the target (a block-compressed view lines up with
//! no target: `crate::resource::alias_of`), so the face reads its own host copy, filled from
//! title memory; what can go wrong there is when that copy was filled: the title streams the
//! texture into memory the target held, and the copy kept what the memory held before (or
//! the bytes landed after the read and no write to them was seen). Here an upload of a texture
//! over a target forgets what the staging buffer last saw and detiles the memory again, as a
//! texture never uploaded is: when it is first found there and then every [`EVERY`] frames
//! while it stays.
//!
//! shortcut: by time, not by what the title wrote: a texture over a target is read again every
//! [`EVERY`] frames whether its bytes changed or not (a few hundred kilobytes each).
//!
//! With `VICTORY_FACE_WATCH` each read again also hashes the title memory it reads and says
//! whether it changed since the texture's last read again (`changed=`), which
//! `crate::face_watch` shows per record slot (`read_again`): batch 5 part Q, a face turned solid
//! red for two seconds in the ON run while textures over target `0xb58000000` were read again.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

pub(crate) static ON: victory_core::settings::Live =
    victory_core::settings::Live::flag("VICTORY_TARGET_REUSE_REREAD");

/// Textures remembered at most (the ones over a target, a few hundred in a scene).
const MOST: usize = 8192;

/// Released targets' memory remembered at most, the newest kept.
const RELEASED: usize = 4096;

/// Title memory of targets the title released: start, bytes; oldest first.
static GONE: Mutex<VecDeque<(u64, u64)>> = Mutex::new(VecDeque::new());

/// Frames between reads again of a texture that stays over a target.
const EVERY: u64 = 16;

/// By texture: the start of the target its memory lay in when it was last read again, and
/// that frame.
static SEEN: Mutex<Option<HashMap<usize, (u64, u64)>>> = Mutex::new(None);
/// Textures read again, in all.
static REREAD: AtomicU64 = AtomicU64::new(0);

/// (`VICTORY_FACE_WATCH`) By texture: the frame it was last read again, the hash of the title
/// memory read then, and whether that hash differed from the one before (None: the first).
type Again = (u64, blake3::Hash, Option<bool>);
static AGAIN: Mutex<Option<HashMap<usize, Again>>> = Mutex::new(None);

/// A read again of `resource` at `frame` whose title memory hashed `hash`: whether that
/// memory changed since the texture's last read again (None: the first).
fn note_again(again: &mut HashMap<usize, Again>, resource: usize, frame: u64, hash: blake3::Hash) -> Option<bool> {
    if again.len() >= MOST && !again.contains_key(&resource) {
        again.clear();
    }
    let changed = again.get(&resource).map(|&(_, was, _)| was != hash);
    again.insert(resource, (frame, hash, changed));
    changed
}

/// (`VICTORY_FACE_WATCH`) When texture `resource` was last read again (frames before
/// `frame`), and whether its title memory had changed since the read again before.
pub(crate) fn read_again(resource: usize, frame: u64) -> Option<(u64, Option<bool>)> {
    let again = AGAIN.lock().unwrap_or_else(|e| e.into_inner());
    again
        .as_ref()?
        .get(&resource)
        .map(|&(at, _, changed)| (frame.saturating_sub(at), changed))
}

/// The hash of title memory `[address, address + len)`, read as a staging refresh reads it.
#[cfg(windows)]
fn title_hash(address: u64, len: u64) -> blake3::Hash {
    let mut bytes = vec![0u8; len as usize];
    crate::host::copy_mapped(bytes.as_mut_ptr(), address as *const u8, bytes.len(), false);
    blake3::hash(&bytes)
}

/// Whether texture `resource`, now (frame `frame`) over the target starting at `over` (None:
/// over none), is to be read again; remembered in `seen`.
fn decide(
    seen: &mut HashMap<usize, (u64, u64)>,
    resource: usize,
    over: Option<u64>,
    frame: u64,
) -> bool {
    let Some(target) = over else {
        seen.remove(&resource);
        return false;
    };
    if let Some(&(was, at)) = seen.get(&resource)
        && was == target
        && frame.wrapping_sub(at) < EVERY
    {
        return false;
    }
    if seen.len() >= MOST && !seen.contains_key(&resource) {
        seen.clear();
    }
    seen.insert(resource, (target, frame));
    true
}

/// The title released a target at `start`; `bytes` says how much memory it held ([`ON`]
/// only).
pub(crate) fn released(start: u64, bytes: impl FnOnce() -> Option<u64>) {
    if !ON.get_bool() {
        return;
    }
    let Some(len) = bytes().filter(|&l| l > 0) else {
        return;
    };
    remember(&mut GONE.lock().unwrap_or_else(|e| e.into_inner()), start, len);
}

fn remember(gone: &mut VecDeque<(u64, u64)>, start: u64, len: u64) {
    gone.retain(|&(s, _)| s != start);
    if gone.len() >= RELEASED {
        gone.pop_front();
    }
    gone.push_back((start, len));
}

/// The start of the released target's memory `[address, address + len)` overlaps, newest
/// first.
fn released_over(gone: &VecDeque<(u64, u64)>, address: u64, len: u64) -> Option<u64> {
    let end = address.saturating_add(len.max(1));
    gone.iter()
        .rev()
        .find(|&&(s, l)| address < s.saturating_add(l) && s < end)
        .map(|&(s, _)| s)
}

/// For an upload of texture `resource` from title memory `[address, address + len)`: whether
/// its staging buffer is to be detiled again ([`ON`] only).
#[cfg(windows)]
pub(crate) fn reread(resource: usize, address: u64, len: u64) -> bool {
    if !ON.get_bool() {
        return false;
    }
    let live = crate::resource::placed_inside::target_over(address, len);
    let over = live.or_else(|| {
        released_over(&GONE.lock().unwrap_or_else(|e| e.into_inner()), address, len)
    });
    let again = {
        let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
        decide(
            seen.get_or_insert_with(HashMap::new),
            resource,
            over,
            crate::frame::current(),
        )
    };
    if again {
        let n = REREAD.fetch_add(1, Relaxed) + 1;
        let changed = crate::face_watch::on().then(|| {
            let hash = title_hash(address, len);
            let mut again = AGAIN.lock().unwrap_or_else(|e| e.into_inner());
            note_again(again.get_or_insert_with(HashMap::new), resource, crate::frame::current(), hash)
        });
        let changed = match changed {
            None => "unhashed",
            Some(None) => "first",
            Some(Some(true)) => "yes",
            Some(Some(false)) => "no",
        };
        if n <= 64 || n.is_power_of_two() || changed == "yes" && n <= 4096 {
            tracing::info!(
                n,
                resource = format_args!("{resource:#x}"),
                address = format_args!("{address:#x}"),
                bytes = len,
                target = format_args!("{:#x}", over.unwrap_or(0)),
                over = if live.is_some() { "live" } else { "released" },
                changed,
                "target reuse: a texture the title fills lies in a target's memory: read again from title memory"
            );
        }
    }
    again
}

/// The title released texture `resource`.
pub(crate) fn forget(resource: usize) {
    if let Some(seen) = SEEN.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        seen.remove(&resource);
    }
    if let Some(again) = AGAIN.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        again.remove(&resource);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Read again the first time it is found over a target, when it is over another one and
    /// every `EVERY` frames while it stays; never while over none.
    #[test]
    fn a_texture_over_a_target_is_read_again_when_found_and_every_few_frames() {
        let mut seen = HashMap::new();
        assert!(!decide(&mut seen, 1, None, 10));
        assert!(decide(&mut seen, 1, Some(0x1000), 10));
        assert!(!decide(&mut seen, 1, Some(0x1000), 11));
        assert!(!decide(&mut seen, 1, Some(0x1000), 10 + EVERY - 1));
        assert!(decide(&mut seen, 1, Some(0x1000), 10 + EVERY));
        assert!(decide(&mut seen, 1, Some(0x2000), 10 + EVERY + 1));
        assert!(!decide(&mut seen, 1, None, 10 + EVERY + 2));
        assert!(decide(&mut seen, 1, Some(0x2000), 10 + EVERY + 3));
        assert!(decide(&mut seen, 2, Some(0x2000), 10 + EVERY + 3));
    }

    /// Batch 5 part O: the skin texture was placed where a bake result had been.
    #[test]
    fn memory_of_a_released_target_is_found_newest_first() {
        let mut gone = VecDeque::new();
        remember(&mut gone, 0x1000, 0x100);
        remember(&mut gone, 0x1080, 0x100);
        assert_eq!(released_over(&gone, 0x1090, 0x10), Some(0x1080));
        assert_eq!(released_over(&gone, 0x1010, 0x10), Some(0x1000));
        assert_eq!(released_over(&gone, 0x1180, 0x10), None);
        remember(&mut gone, 0x1000, 0x100);
        assert_eq!(gone.len(), 2, "the same start is kept once");
        for i in 0..RELEASED as u64 {
            remember(&mut gone, 0x10_0000 + i * 0x1000, 0x1000);
        }
        assert_eq!(gone.len(), RELEASED);
        assert_eq!(released_over(&gone, 0x1000, 0x10), None, "the oldest went");
    }

    /// Whether the title memory changed is told from the second read again on.
    #[test]
    fn a_read_again_says_whether_title_memory_changed() {
        let mut again = HashMap::new();
        let (a, b) = (blake3::hash(b"a"), blake3::hash(b"b"));
        assert_eq!(note_again(&mut again, 1, 10, a), None);
        assert_eq!(note_again(&mut again, 1, 26, a), Some(false));
        assert_eq!(note_again(&mut again, 1, 42, b), Some(true));
        assert_eq!(note_again(&mut again, 2, 42, b), None);
        assert_eq!(again[&1], (42, b, Some(true)));
    }

    #[test]
    fn the_switch_is_live() {
        victory_core::settings::check_live(
            "VICTORY_TARGET_REUSE_REREAD",
            "1",
            || ON.get_bool(),
            false,
            true,
        );
    }
}
