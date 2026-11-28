//! (feedcard, #88) `VICTORY_DEFAULT_NO_BACK=1` (Live; off by default): what
//! shaders write into a committed DEFAULT-heap buffer of 1 MiB or more stays
//! on the card, as `VICTORY_SKIP_WC_BACK` does for write-combining title
//! memory: a declared range wholly inside one such buffer and larger than
//! `VICTORY_SKIP_WC_KEEP` (1 MiB by default) is not fetched or landed. A
//! DEFAULT heap is not the CPU's under the D3D12 contract (no Map).
//!
//! After TheSmurphilicious/victorious-foxwoodhound-lite `5f9cd50a9b`
//! (`DEFAULT_NO_BACK`; there 21 of 33 MB a frame fetched and landed was one
//! buffer). Their `DEFAULT_WATCH` (`37c46b5308`), the write-watched memory
//! it needs there, is not ported: our DEFAULT buffers already come from the
//! title's write-watched memory (copy profile: unwatched 0.0 MB).
//!
//! What it risks: a CPU step that reads such a buffer after the shaders
//! wrote it (a buffer copy into a read-back buffer, done on the CPU with
//! `VICTORY_CARD_COPIES=0`; the title's CPU at its GPU address) reads what
//! title memory held before. Ranges of at most the keep size still come back.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

static ON: victory_core::settings::Live =
    victory_core::settings::Live::flag("VICTORY_DEFAULT_NO_BACK");

/// The smallest buffer kept on the card.
const MIN: usize = 1 << 20;

/// Committed DEFAULT-heap buffers of [`MIN`] or more: start to length.
/// Recorded with the switch off too, for a live change.
static BUFFERS: Mutex<BTreeMap<usize, usize>> = Mutex::new(BTreeMap::new());

/// Pieces left on the card: count and bytes.
static LEFT: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

/// A committed resource was given `[at, at + len)` in a heap of `heap_kind`.
pub(crate) fn committed(at: usize, len: usize, heap_kind: u32, buffer: bool) {
    const DEFAULT_HEAP: u32 = 1;
    if !buffer || heap_kind != DEFAULT_HEAP || len < MIN || at == 0 {
        return;
    }
    let mut buffers = BUFFERS.lock().unwrap_or_else(|e| e.into_inner());
    insert(&mut buffers, at, len);
}

/// A buffer whose memory began at `at` went away.
pub(crate) fn freed(at: usize) {
    BUFFERS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&at);
}

fn insert(buffers: &mut BTreeMap<usize, usize>, at: usize, len: usize) {
    let Some(end) = at.checked_add(len) else {
        return;
    };
    // A buffer whose memory is now another's was not told freed: forget it.
    let stale: Vec<usize> = buffers
        .range(..end)
        .rev()
        .take_while(|&(&base, &n)| base.saturating_add(n) > at)
        .map(|(&base, _)| base)
        .collect();
    for base in stale {
        buffers.remove(&base);
    }
    buffers.insert(at, len);
}

fn inside(buffers: &BTreeMap<usize, usize>, (from, to): (usize, usize)) -> bool {
    from < to
        && buffers
            .range(..to)
            .next_back()
            .is_some_and(|(&base, &len)| base <= from && to <= base.saturating_add(len))
}

/// Whether a card-copy piece of `bytes` of the declared range `declared`
/// stays on the card (`crate::wc_skip::skip_back`).
pub(crate) fn skip(declared: (usize, usize), bytes: u64) -> bool {
    if !ON.get_bool() || declared.1.saturating_sub(declared.0) <= crate::wc_skip::keep_bytes() {
        return false;
    }
    let left = inside(&BUFFERS.lock().unwrap_or_else(|e| e.into_inner()), declared);
    if left {
        let n = LEFT[0].fetch_add(1, Ordering::Relaxed) + 1;
        let b = LEFT[1].fetch_add(bytes, Ordering::Relaxed) + bytes;
        if n.is_power_of_two() {
            tracing::info!(
                "VICTORY_DEFAULT_NO_BACK: {n} pieces ({} MB) of DEFAULT-heap buffers left on the card so far",
                b >> 20
            );
        }
    }
    left
}

#[cfg(test)]
mod tests {
    use super::{BTreeMap, MIN, insert, inside};

    #[test]
    fn a_declaration_must_fit_one_buffer() {
        let mut buffers = BTreeMap::new();
        insert(&mut buffers, 0x100_0000, 2 * MIN);
        insert(&mut buffers, 0x100_0000 + 2 * MIN, MIN);
        assert!(inside(&buffers, (0x100_0000, 0x100_0000 + 2 * MIN)));
        assert!(inside(&buffers, (0x100_0000 + 4096, 0x100_0000 + 8192)));
        // Across two neighbouring buffers: not one buffer's.
        assert!(!inside(&buffers, (0x100_0000 + MIN, 0x100_0000 + 3 * MIN)));
        assert!(!inside(&buffers, (0x0ff_f000, 0x100_1000)));
        assert!(!inside(&buffers, (0x100_0000, 0x100_0000)));
    }

    #[test]
    fn memory_given_to_another_buffer_forgets_the_old_one() {
        let mut buffers = BTreeMap::new();
        insert(&mut buffers, 0x200_0000, 4 * MIN);
        insert(&mut buffers, 0x200_0000 + MIN, MIN);
        assert_eq!(buffers.len(), 1);
        assert!(!inside(&buffers, (0x200_0000, 0x200_0000 + 4096)));
        assert!(inside(&buffers, (0x200_0000 + MIN, 0x200_0000 + 2 * MIN)));
    }
}
