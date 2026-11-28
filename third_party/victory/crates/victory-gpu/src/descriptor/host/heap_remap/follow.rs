//! (heapcap) Packed views follow the title memory of their textures.
//!
//! A title slot's view can change in two ways. A new record (another view,
//! the higher-resolution one a stream brings in, or the same view made
//! again) is a write of the slot: its chunk is looked at again and the
//! packed view made again from the new record (`Packed::place`). The same
//! view over memory the title maps or writes later (levels of a placed
//! texture streaming in) writes no slot at all: own slots learn of it from
//! the memory's remap and page-write notices
//! (`HostHeap::texture_slots_remapped`), packed ones from [`memory_changed`]
//! here. A packed placed texture whose memory changed is made again in
//! place: its view from its first level with memory now, readied (uploaded)
//! again, its entry pointing at it all along.

use super::{Packed, View};
use std::collections::BTreeMap;
use std::sync::Mutex;

/// Memory ranges changed since the last [`changed`], at most this many kept.
const PENDING_MOST: usize = 8192;
/// Packed views made again for memory changes per frame, at most.
const FOLLOW_BUDGET: usize = 4096;

static PENDING: Mutex<Vec<(u64, u64)>> = Mutex::new(Vec::new());

/// The title memory a placed texture view shows, from its first level.
pub(crate) fn span_of(view: &View) -> Option<(u64, u64)> {
    const BUFFER: u32 = 1;
    let View::Placed(false, resource, desc) = view else {
        return None;
    };
    if resource[0] == BUFFER {
        return None;
    }
    let start = u64::from(desc[10]) | u64::from(desc[11]) << 32;
    let bytes = crate::resource::placed_bytes_from(resource, crate::resource::first_mip(desc))?;
    Some((start, start.saturating_add(bytes)))
}

/// The memory of the packed placed textures: by start and packed slot, the
/// end and the make that noted it.
#[derive(Default)]
pub(crate) struct Spans {
    by_start: BTreeMap<(u64, u32), (u64, u64)>,
    longest: u64,
}

impl Spans {
    /// Packed slot `k` shows `view` from make `generation`.
    pub(crate) fn note(&mut self, view: &View, k: u32, generation: u64) {
        if let Some((start, end)) = span_of(view) {
            self.add(start, end, k, generation);
        }
    }

    pub(crate) fn add(&mut self, start: u64, end: u64, k: u32, generation: u64) {
        if end > start {
            self.longest = self.longest.max(end - start);
            self.by_start.insert((start, k), (end, generation));
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.by_start.len()
    }

    /// The packed slots whose span overlaps `[start, end)` and whose make is
    /// still `current(k, generation)`; spans of earlier makes are let go.
    pub(crate) fn hits(
        &mut self,
        start: u64,
        end: u64,
        current: impl Fn(u32, u64) -> bool,
    ) -> Vec<u32> {
        let from = (start.saturating_sub(self.longest), 0);
        let mut out = Vec::new();
        let mut gone = Vec::new();
        for (&(at, k), &(span_end, generation)) in self.by_start.range(from..(end, 0)) {
            if span_end <= start {
                continue;
            }
            if current(k, generation) {
                out.push(k);
            } else {
                gone.push((at, k));
            }
        }
        for key in gone {
            self.by_start.remove(&key);
        }
        out
    }
}

/// Title memory `ranges` was mapped or written (from the own slots' remap
/// and page-write notices).
pub(crate) fn memory_changed(ranges: &[(u64, u64)]) {
    if ranges.is_empty() || !super::super::heap_cap::remap_on() {
        return;
    }
    let mut pending = PENDING.lock().unwrap_or_else(|e| e.into_inner());
    let room = PENDING_MOST.saturating_sub(pending.len());
    pending.extend(ranges.iter().copied().take(room));
}

/// The title slots of `packed` whose packed placed texture's memory changed
/// since the last call, at most [`FOLLOW_BUDGET`].
pub(super) fn changed(packed: &mut Packed) -> Vec<u32> {
    let ranges = std::mem::take(&mut *PENDING.lock().unwrap_or_else(|e| e.into_inner()));
    if ranges.is_empty() || packed.spans.len() == 0 {
        return Vec::new();
    }
    let owner = &packed.owner;
    let mut ks = Vec::new();
    for (start, end) in ranges {
        ks.extend(packed.spans.hits(start, end, |k, generation| {
            owner
                .get(k as usize)
                .and_then(|o| o.as_ref())
                .is_some_and(|o| o.generation == generation)
        }));
        if ks.len() >= FOLLOW_BUDGET {
            break;
        }
    }
    ks.sort_unstable();
    ks.dedup();
    ks.truncate(FOLLOW_BUDGET);
    ks.into_iter()
        .filter_map(|k| packed.owner[k as usize].map(|o| o.title))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::super::{Placed, Stage};
    use super::*;

    const NULL_SLOT: u32 = 500_000;

    fn made_and_shown(p: &mut Packed, slot: u32, key: [u32; 3], again: bool, frame: u64) -> u32 {
        let Placed::Make { at, generation } = p.place(slot, Some(key), again, frame) else {
            panic!("not made");
        };
        p.spans
            .add(0x1000_0000, 0x1010_0000, at - p.base, generation);
        assert!(p.made(at, generation, true));
        for due in p.take_made(usize::MAX) {
            p.show(&due);
        }
        at
    }

    #[test]
    fn a_packed_texture_whose_memory_changes_is_made_again_in_place() {
        let mut p = Packed::new(1, NULL_SLOT, 8);
        let slot = 900_000;
        let at = made_and_shown(&mut p, slot, [1, 7, 1], false, 0);
        assert!(p.shows(slot, [1, 7, 1]));
        // a level of its texture is mapped: the slot is due again, in place
        p.spans.longest = p.spans.longest.max(1);
        let hits = p.spans.hits(0x1008_0000, 0x1009_0000, |_, _| true);
        assert_eq!(hits, [at - p.base]);
        // `refresh`: again, in place (its entry keeps pointing at `at`), made, readied, shown
        assert!(
            p.shows(slot, [1, 7, 1]),
            "in place: the entry is left as it is"
        );
        let Placed::Make {
            at: again,
            generation,
        } = p.place(slot, Some([1, 7, 1]), true, 1)
        else {
            panic!("not made again");
        };
        assert_eq!(again, at);
        assert!(p.made(at, generation, true));
        let due = p.take_made(usize::MAX);
        assert_eq!(due.len(), 1);
        assert!(p.show(&due[0]).is_some());
        assert!(p.shows(slot, [1, 7, 1]));
        // memory elsewhere: nothing
        assert!(
            p.spans
                .hits(0x2000_0000, 0x2001_0000, |_, _| true)
                .is_empty()
        );
    }

    #[test]
    fn a_span_of_an_earlier_make_is_let_go() {
        let mut spans = Spans::default();
        spans.add(100, 200, 3, 1);
        spans.add(150, 400, 4, 2);
        assert_eq!(spans.hits(190, 191, |_, _| true), [3, 4]);
        // slot 3 was made again since (generation 5): its old span goes
        assert_eq!(spans.hits(190, 191, |k, g| k != 3 || g == 5), [4]);
        assert_eq!(spans.len(), 1);
        assert!(
            spans.hits(0, 100, |_, _| true).is_empty(),
            "ends are exclusive"
        );
        assert_eq!(spans.hits(399, 1000, |_, _| true), [4]);
    }

    #[test]
    fn a_new_record_in_a_packed_slot_shows_the_new_view_and_never_stays_null() {
        // a stream replaces the low-resolution view with the high-resolution one in the same
        // slot: the slot reads null only while the new view is made and readied
        let mut p = Packed::new(1, NULL_SLOT, 8);
        let slot = 910_000;
        let at = made_and_shown(&mut p, slot, [1, 10, 1], false, 0);
        let Placed::Make {
            at: again,
            generation,
        } = p.place(slot, Some([1, 11, 1]), false, 1)
        else {
            panic!("a new record is made again");
        };
        assert_eq!(again, at);
        assert!(!p.shows(slot, [1, 11, 1]));
        // its make fails (no room): tried again later, then shown
        assert!(p.made(at, generation, false));
        let mut frame = 1;
        let shown = loop {
            frame += 1;
            p.now = frame;
            if let Placed::Make { at, generation } = p.place(slot, Some([1, 11, 1]), false, frame) {
                assert!(p.made(at, generation, true));
                for due in p.take_made(usize::MAX) {
                    p.show(&due);
                }
            }
            if p.shows(slot, [1, 11, 1]) || frame > 100 {
                break frame;
            }
        };
        assert!(shown <= 10, "shown at {shown}");
        let owner = p.owner[(at - p.base) as usize].unwrap();
        assert_eq!(owner.stage, Stage::Shown);
        // and in a new slot: the old one is dropped when its record dies, the new one packed
        assert_eq!(p.place(slot, None, false, 200), Placed::Drop(at));
        assert!(matches!(
            p.place(slot + 1, Some([1, 11, 1]), false, 200),
            Placed::Make { .. }
        ));
    }

    #[test]
    fn written_chunks_are_bounded_and_each_looked_at_once() {
        let mut p = Packed::new(1, NULL_SLOT, 8);
        for _ in 0..100 {
            p.mark(737_855, 1_245_184);
        }
        let due = p.chunks_due();
        assert_eq!(
            due,
            ((1_245_184 - 1) / super::super::CHUNK - 737_855 / super::super::CHUNK + 1) as usize
        );
        let mut seen = 0;
        while let Some((lo, hi)) = p.next_chunk() {
            assert_eq!(hi - lo, super::super::CHUNK);
            seen += 1;
        }
        assert_eq!(seen, due);
        assert_eq!(p.chunks_due(), 0);
    }
}
