//! Texture change notices kept for each view heap
//! (`VICTORY_HEAP_NOTICES=1`; off by default, read once).
//!
//! Ported from TaynanGT/Vice-Tay #371 `bc44a9f`. The ranges of title memory
//! the runtime maps, unmaps or writes behind the guards' back
//! ([`super::remapped`], [`super::page_written`]) are kept in one list for
//! every heap, and the first heap whose texture pass runs takes them all
//! ("a title has one view heap"). A second shader-visible view heap then
//! never sees them: its placed-texture slots keep the bytes from before
//! (stale or black textures, pop-in) until a draw or the periodic recheck
//! makes them again. With the switch each view heap is subscribed when it
//! is made, every range goes to every subscribed heap, and each heap's pass
//! takes only its own.
//!
//! Heaps made while the switch is off (and, in tests, heaps never
//! subscribed) go on reading the shared lists: the shared lists are fed
//! while any such heap lives, so nothing changes for them.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};

/// The two kinds of notice, as the shared lists keep them apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    /// [`super::page_written`].
    Written,
    /// [`super::remapped`].
    Remapped,
}

#[derive(Default)]
struct Changes {
    written: Vec<(u64, u64)>,
    remapped: Vec<(u64, u64)>,
}

impl Changes {
    fn of(&mut self, kind: Kind) -> &mut Vec<(u64, u64)> {
        match kind {
            Kind::Written => &mut self.written,
            Kind::Remapped => &mut self.remapped,
        }
    }
}

/// Subscribed view heaps by id.
static HEAPS: Mutex<BTreeMap<u64, Changes>> = Mutex::new(BTreeMap::new());
/// A heap was ever subscribed: until then nothing here locks [`HEAPS`].
static ANY: AtomicBool = AtomicBool::new(false);
/// Live view heaps not subscribed: the shared lists are fed while any is.
static SHARED_READERS: AtomicUsize = AtomicUsize::new(0);

fn parse(value: Option<&str>) -> bool {
    value.is_some_and(|v| v.trim() == "1")
}

fn on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        let on = parse(std::env::var("VICTORY_HEAP_NOTICES").ok().as_deref());
        if on {
            tracing::info!(
                "texture change notices kept for each view heap (VICTORY_HEAP_NOTICES=1; Vice-Tay #371)"
            );
        }
        on
    })
}

/// A heap was made: a view heap is subscribed with the switch, and reads
/// the shared lists without it. Sampler heaps have no textures.
pub(super) fn born(id: u64, sampler: bool) {
    if sampler {
        return;
    }
    if on() {
        subscribe(id);
    } else {
        SHARED_READERS.fetch_add(1, Relaxed);
    }
}

/// A heap went: out of the subscribers, or one shared reader fewer.
pub(super) fn died(id: u64, sampler: bool) {
    if sampler {
        return;
    }
    if HEAPS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&id)
        .is_none()
    {
        let _ = SHARED_READERS.fetch_update(Relaxed, Relaxed, |n| n.checked_sub(1));
    }
}

/// Subscribes the view heap `id` (made as a shared reader, if it was).
fn subscribe(id: u64) {
    ANY.store(true, Relaxed);
    HEAPS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(id)
        .or_default();
}

/// Whether the heap `id` takes its own notices.
pub(super) fn subscribed(id: u64) -> bool {
    ANY.load(Relaxed)
        && HEAPS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(&id)
}

/// Whether the shared lists are still read by a heap: always without the
/// switch.
pub(super) fn shared_read() -> bool {
    !on() || SHARED_READERS.load(Relaxed) != 0
}

/// Notes `[start, end)` for every subscribed heap, kept as `push` keeps it
/// for the shared list; returns how many ranges it compacted.
pub(super) fn note(
    kind: Kind,
    start: u64,
    end: u64,
    push: impl Fn(&mut Vec<(u64, u64)>, u64, u64) -> usize,
) -> usize {
    if !ANY.load(Relaxed) {
        return 0;
    }
    let mut heaps = HEAPS.lock().unwrap_or_else(|e| e.into_inner());
    heaps
        .values_mut()
        .map(|changes| push(changes.of(kind), start, end))
        .sum()
}

/// Takes the heap `id`'s ranges of `kind` since its last take.
pub(super) fn take(id: u64, kind: Kind) -> Vec<(u64, u64)> {
    HEAPS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_mut(&id)
        .map(|changes| std::mem::take(changes.of(kind)))
        .unwrap_or_default()
}

impl Drop for super::HostHeap {
    fn drop(&mut self) {
        died(self.id, self.sampler);
    }
}

#[cfg(test)]
pub(super) fn subscribe_for_test(id: u64) {
    if !subscribed(id) {
        let _ = SHARED_READERS.fetch_update(Relaxed, Relaxed, |n| n.checked_sub(1));
        subscribe(id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn push(ranges: &mut Vec<(u64, u64)>, start: u64, end: u64) -> usize {
        ranges.push((start, end));
        0
    }

    #[test]
    fn switch_reads_one_only() {
        assert!(parse(Some("1")));
        assert!(!parse(Some("0")));
        assert!(!parse(None));
    }

    /// Every subscribed heap gets every range, and a take empties only
    /// the taker's own list of that kind.
    #[test]
    fn each_subscribed_heap_takes_its_own() {
        let (a, b) = (u64::MAX - 10, u64::MAX - 11);
        subscribe(a);
        subscribe(b);
        let at = 0x7fec_5000_0000u64;
        note(Kind::Remapped, at, at + 0x1000, push);
        note(Kind::Written, at + 0x2000, at + 0x3000, push);
        assert!(take(a, Kind::Remapped).contains(&(at, at + 0x1000)));
        assert!(!take(a, Kind::Remapped).contains(&(at, at + 0x1000)));
        assert!(take(b, Kind::Remapped).contains(&(at, at + 0x1000)));
        assert!(take(a, Kind::Written).contains(&(at + 0x2000, at + 0x3000)));
        assert!(take(b, Kind::Written).contains(&(at + 0x2000, at + 0x3000)));
        assert_eq!(take(u64::MAX - 12, Kind::Written), []);
        HEAPS.lock().unwrap().remove(&a);
        HEAPS.lock().unwrap().remove(&b);
    }

    /// Two view heaps with an uploaded slot over the same memory: memory
    /// mapped under it makes the slot due in both. Shared, the first heap's
    /// pass took the range and the second heap's slot stayed as it was.
    #[test]
    fn remaps_reach_each_view_heap() {
        use crate::descriptor::View;
        let at = 0x7fec_6000_0000u64;
        let heap_with_view = |slot: u32| {
            let heap = super::super::HostHeap::new(false, 8)?;
            subscribe_for_test(heap.id);
            let mut desc = [0u32; 16];
            (desc[10], desc[11]) = (at as u32, (at >> 32) as u32);
            let view = View::Placed(
                false,
                [3, 0, 0, 0, 64, 0, 64, 0x1_0001, 98, 1, 0, 0x105, 0, 0],
                desc,
            );
            assert!(heap.note_buffer(slot, Some(&view), false, true).is_empty());
            let (queued, write) = heap.texture_slots_due(u64::MAX, 8).0[0];
            assert_eq!(queued, slot);
            heap.texture_slot_done(slot, write, None);
            Some(heap)
        };
        let (Some(first), Some(second)) = (heap_with_view(1), heap_with_view(3)) else {
            return; // no host GPU
        };
        super::super::remapped(at as usize, 0x1000);
        for (heap, slot) in [(&first, 1u32), (&second, 3)] {
            assert_eq!(heap.texture_slots_remapped(50), 1);
            let due: Vec<u32> = heap.texture_slots_due(51, 8).0.into_iter().map(|d| d.0).collect();
            assert_eq!(due, [slot]);
        }
        let (first_id, second_id) = (first.id, second.id);
        drop(first);
        drop(second);
        assert!(!subscribed(first_id) && !subscribed(second_id));
    }
}
