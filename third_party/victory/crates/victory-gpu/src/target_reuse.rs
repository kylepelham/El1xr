//! `VICTORY_TARGET_REUSE_REREAD=1` (live, off by default): a texture the title fills itself
//! whose memory lies in the memory of a texture the host writes (a render target, a bake
//! result) has its host copy read again from title memory, once for each such target.
//!
//! Batch 5 part O, run 1: a character's 512x512 BC1 skin texture (heap slot 96424) moved
//! through four addresses; at three of them the face was lit, at the fourth (`0xc9f085000`,
//! inside the memory of a bake result, `in_bake_memory`) the face was black for as long as it
//! stayed there. The view never links to the target (a block-compressed view lines up with
//! no target: `crate::resource::alias_of`), so the face reads its own host copy, filled from
//! title memory; what can go wrong there is when that copy was filled: the title streams the
//! texture into memory the target held, and the copy kept what the memory held before. Here
//! the first upload after the texture is found over a target forgets what the staging buffer
//! last saw and detiles the memory again, as a texture never uploaded is.
//!
//! shortcut: by the target's start, not by what the title wrote: a texture that stays over
//! one target is read again once, not each time that target is drawn to (nothing the host
//! draws reaches title memory).

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

pub(crate) static ON: victory_core::settings::Live =
    victory_core::settings::Live::flag("VICTORY_TARGET_REUSE_REREAD");

/// Textures remembered at most (the ones over a target, a few hundred in a scene).
const MOST: usize = 8192;

/// By texture: the start of the target its memory lay in when it was last read again.
static SEEN: Mutex<Option<HashMap<usize, u64>>> = Mutex::new(None);
/// Textures read again, in all.
static REREAD: AtomicU64 = AtomicU64::new(0);

/// Whether texture `resource`, now over the target starting at `over` (None: over none),
/// is to be read again; remembered in `seen`.
fn decide(seen: &mut HashMap<usize, u64>, resource: usize, over: Option<u64>) -> bool {
    let Some(target) = over else {
        seen.remove(&resource);
        return false;
    };
    if seen.len() >= MOST && !seen.contains_key(&resource) {
        seen.clear();
    }
    seen.insert(resource, target) != Some(target)
}

/// For an upload of texture `resource` from title memory `[address, address + len)`: whether
/// its staging buffer is to be detiled again ([`ON`] only).
#[cfg(windows)]
pub(crate) fn reread(resource: usize, address: u64, len: u64) -> bool {
    if !ON.get_bool() {
        return false;
    }
    let over = crate::resource::placed_inside::target_over(address, len);
    let again = {
        let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
        decide(seen.get_or_insert_with(HashMap::new), resource, over)
    };
    if again {
        let n = REREAD.fetch_add(1, Relaxed) + 1;
        if n <= 16 || n.is_power_of_two() {
            tracing::info!(
                n,
                resource = format_args!("{resource:#x}"),
                address = format_args!("{address:#x}"),
                bytes = len,
                target = format_args!("{:#x}", over.unwrap_or(0)),
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
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Read again the first time it is found over a target and when it is over another one;
    /// not again over the same one, and again after it left the target's memory.
    #[test]
    fn a_texture_over_a_target_is_read_again_once_per_target() {
        let mut seen = HashMap::new();
        assert!(!decide(&mut seen, 1, None));
        assert!(decide(&mut seen, 1, Some(0x1000)));
        assert!(!decide(&mut seen, 1, Some(0x1000)));
        assert!(decide(&mut seen, 1, Some(0x2000)));
        assert!(!decide(&mut seen, 1, None));
        assert!(decide(&mut seen, 1, Some(0x2000)));
        assert!(decide(&mut seen, 2, Some(0x2000)));
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
