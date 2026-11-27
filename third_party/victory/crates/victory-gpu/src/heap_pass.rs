//! Bookkeeping of the pass over the view heap's buffer views for shaders
//! that pick them by a computed index (`draw::State::heap_buffers`): which
//! slots it looks at again, and the span of each host copy their views
//! read. Kept apart from D3D12 so that it can be tested alone.
//!
//! The pass used to look again at every buffer view of the heap (81,000 in
//! the world) every 16 passes, and to rebuild the spans of all host copies
//! from all views every pass. Now a slot is looked at again only when
//! - its buffer view was written or went (the heap's own set of changes);
//! - the view table entry its record names was freed (the title released
//!   the resource: `descriptor::forget`), which leaves the record as it was;
//! - its record in title memory is not the one seen at the last look (a
//!   record copied by the title itself, past the API); a compare of 12
//!   bytes per slot, no lock and no lookup;
//! - it found no host copy at its last look: every 16 passes, as before;
//! - every [`FULL_EVERY`] passes, all of them, as a safety net.
//!
//! The span of a copy is worked out again only when one of its slots went.
//! Off by default: the old pass (all slots every 16 passes, all spans every
//! pass) stays. `VICTORY_HEAP_PASS_DIRTY=1` turns the marked pass on.

use std::collections::{BTreeMap, HashMap, HashSet};

/// Passes between two looks at every slot, besides those marked.
pub(crate) const FULL_EVERY: u64 = 256;
/// Passes between two looks at slots that found no host copy (and between
/// two looks at every slot with the old pass).
pub(crate) const ORPHANS_EVERY: u64 = 16;

/// Whether the old pass is used: unless `VICTORY_HEAP_PASS_DIRTY=1` asks for the marked one.
pub(crate) fn full_pass() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| !std::env::var_os("VICTORY_HEAP_PASS_DIRTY").is_some_and(|v| v == "1"))
}

/// A slot's record in title memory: the tag and two words
/// (`descriptor::record_at`).
pub(crate) type Record = [u32; 3];

struct Slot {
    copy: usize,
    from: u64,
    to: u64,
    record: Record,
}

struct Copy<C> {
    copy: C,
    slots: HashSet<u32>,
    from: u64,
    to: u64,
    /// A slot went: the span is worked out again from those left.
    stale: bool,
}

/// Passes between two scans of all kept slots' raw records in title memory
/// for past-the-API copies (`VICTORY_HEAP_SCAN_EVERY`, default 1).
pub(crate) fn scan_every() -> u64 {
    static N: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *N.get_or_init(|| {
        std::env::var("VICTORY_HEAP_SCAN_EVERY")
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|&n| n >= 1)
            .unwrap_or(1)
    })
}

/// How many passes a full heap buffer rescan is spread across
/// (`VICTORY_HEAP_RESCAN_SLICES`, default 16; 1: single-frame full rescan).
pub(crate) fn rescan_slices() -> u64 {
    static N: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *N.get_or_init(|| {
        std::env::var("VICTORY_HEAP_RESCAN_SLICES")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(16)
            .clamp(1, 1024)
    })
}

/// What a pass looks at (see the module).
pub(crate) struct Picked<C> {
    passes: u64,
    slots: BTreeMap<u32, Slot>,
    copies: HashMap<usize, Copy<C>>,
    /// Slots that found no host copy, with the record seen.
    orphans: BTreeMap<u32, Record>,
    /// View table entry -> slots whose record named it at their last look
    /// (some may name another by now: checked when used).
    by_entry: HashMap<u32, Vec<u32>>,
    /// Table entries freed since the last pass; `None`: not known.
    forgotten: Option<Vec<u32>>,
    /// The old pass (the default; `VICTORY_HEAP_PASS_DIRTY=1` turns it off).
    full: bool,
    /// Interval for scanning all kept slots for past-the-API record changes.
    scan_every: u64,
    /// Number of passes across which periodic full rescans are sliced (1: all at once).
    rescan_slices: u64,
}

impl<C: Clone> Picked<C> {
    pub(crate) fn new(full: bool) -> Self {
        Self::with_options(
            full,
            if cfg!(test) { 1 } else { scan_every() },
            if cfg!(test) { 1 } else { rescan_slices() },
        )
    }

    pub(crate) fn with_options(full: bool, scan_every: u64, rescan_slices: u64) -> Self {
        Picked {
            passes: 0,
            slots: BTreeMap::new(),
            copies: HashMap::new(),
            orphans: BTreeMap::new(),
            by_entry: HashMap::new(),
            forgotten: None,
            full,
            scan_every: scan_every.max(1),
            rescan_slices: rescan_slices.clamp(1, 1024),
        }
    }

    /// View table entries were freed (`None`: some were, which is not
    /// known): their slots are looked at next pass. Every heap's pass is
    /// told, though one of them asks.
    pub(crate) fn note_forgotten(&mut self, entries: Option<&[u32]>) {
        match (entries, &mut self.forgotten) {
            (Some(new), Some(kept)) => kept.extend_from_slice(new),
            _ => self.forgotten = None,
        }
    }

    /// Whether the next `plan_in_ranges` call will scan kept/orphan slots (or
    /// run pass 0 to establish baseline write-watch stamps).
    pub(crate) fn will_scan(&self) -> bool {
        self.forgotten.is_none() || self.scan_every == 1 || (self.passes + 1).is_multiple_of(self.scan_every)
    }

    /// The slots this pass looks at again, in order. `changed`: the slots
    /// whose buffer view was written or went since the last pass (and the
    /// entries [`Picked::note_forgotten`] was told of: if it was told it
    /// does not know, all slots are looked at); `all`: every slot that holds a
    /// buffer view; `record`: a slot's record now; `entry`: the table
    /// entry a record names, if it names one.
    #[cfg(test)]
    pub(crate) fn plan(
        &mut self,
        changed: Vec<u32>,
        all: impl FnOnce() -> Vec<u32>,
        record: impl Fn(u32) -> Option<Record>,
        entry: impl Fn(Record) -> Option<u32>,
    ) -> Vec<u32> {
        self.plan_in_ranges(changed, all, record, entry, None)
    }

    /// Like [`Picked::plan`], but when `dirty_slots` is `Some(ranges)` only
    /// kept slots and orphans inside those half-open `[lo, hi)` slot ranges
    /// are scanned for past-the-API record changes.
    pub(crate) fn plan_in_ranges(
        &mut self,
        changed: Vec<u32>,
        all: impl FnOnce() -> Vec<u32>,
        mut record: impl FnMut(u32) -> Option<Record>,
        entry: impl Fn(Record) -> Option<u32>,
        dirty_slots: Option<&[(u32, u32)]>,
    ) -> Vec<u32> {
        let pass = self.passes;
        self.passes += 1;
        let forgotten = self.forgotten.replace(Vec::new());
        if self.full {
            if self.rescan_slices <= 1 {
                if pass.is_multiple_of(ORPHANS_EVERY) {
                    self.clear();
                    return all();
                }
                return changed;
            }
            if pass == 0 {
                self.clear();
                return all();
            }
            // Spread the full rescan across `rescan_slices` passes (by slot
            // index) so every slot is revisited every `rescan_slices` passes
            // without stalling a single frame on ~80,000 views.
            let mut due = changed;
            let slices = self.rescan_slices as u32;
            let part = (pass % self.rescan_slices) as u32;
            self.rescan_slice(all(), part, slices, &mut due);
            if due.len() > 1 {
                due.sort_unstable();
                due.dedup();
            }
            return due;
        }
        let Some(forgotten) = forgotten else {
            self.clear();
            return all();
        };
        if self.rescan_slices <= 1 && pass.is_multiple_of(FULL_EVERY) {
            self.clear();
            return all();
        }
        let mut due = changed;
        if self.rescan_slices > 1 && pass > 0 {
            let stride = (FULL_EVERY / self.rescan_slices).max(1);
            if pass.is_multiple_of(stride) {
                let slices = self.rescan_slices as u32;
                let part = ((pass / stride) % self.rescan_slices) as u32;
                self.rescan_slice(all(), part, slices, &mut due);
            }
        }
        for e in forgotten {
            if let Some(slots) = self.by_entry.remove(&e) {
                // Only those whose record still names the entry.
                due.extend(
                    slots
                        .into_iter()
                        .filter(|s| self.record_of(*s).is_some_and(|r| entry(r) == Some(e))),
                );
            }
        }
        let scan = self.scan_every == 1 || pass.is_multiple_of(self.scan_every);
        if scan {
            match dirty_slots {
                Some(ranges) => {
                    for &(lo, hi) in ranges {
                        if lo >= hi {
                            continue;
                        }
                        for (&slot, kept) in self.slots.range(lo..hi) {
                            if record(slot) != Some(kept.record) {
                                due.push(slot);
                            }
                        }
                    }
                }
                None => {
                    for (&slot, kept) in &self.slots {
                        if record(slot) != Some(kept.record) {
                            due.push(slot);
                        }
                    }
                }
            }
        }
        if pass.is_multiple_of(ORPHANS_EVERY) {
            due.extend(self.orphans.keys().copied());
        } else if scan {
            match dirty_slots {
                Some(ranges) => {
                    for &(lo, hi) in ranges {
                        if lo >= hi {
                            continue;
                        }
                        for (&slot, &seen) in self.orphans.range(lo..hi) {
                            if record(slot) != Some(seen) {
                                due.push(slot);
                            }
                        }
                    }
                }
                None => {
                    for (&slot, &seen) in &self.orphans {
                        if record(slot) != Some(seen) {
                            due.push(slot);
                        }
                    }
                }
            }
        }
        if due.len() > 1 {
            due.sort_unstable();
            due.dedup();
        }
        due
    }

    fn record_of(&self, slot: u32) -> Option<Record> {
        self.slots
            .get(&slot)
            .map(|s| s.record)
            .or_else(|| self.orphans.get(&slot).copied())
    }

    fn clear(&mut self) {
        self.slots.clear();
        self.copies.clear();
        self.orphans.clear();
        self.by_entry.clear();
    }

    /// Rescans slots whose index modulo `slices` equals `part`: forgets any
    /// kept or orphan slot in that slice absent from sorted `all`, and appends
    /// the live slots in that slice to `due`.
    fn rescan_slice(&mut self, all: Vec<u32>, part: u32, slices: u32, due: &mut Vec<u32>) {
        let stale: Vec<u32> = self
            .slots
            .keys()
            .chain(self.orphans.keys())
            .copied()
            .filter(|&index| index % slices == part && all.binary_search(&index).is_err())
            .collect();
        for index in stale {
            self.forget(index);
        }
        due.extend(all.into_iter().filter(|&index| index % slices == part));
    }

    /// What slot `slot` held is let go (it is looked at now).
    pub(crate) fn forget(&mut self, slot: u32) {
        self.orphans.remove(&slot);
        let Some(old) = self.slots.remove(&slot) else {
            return;
        };
        if let Some(copy) = self.copies.get_mut(&old.copy) {
            copy.slots.remove(&slot);
            if copy.slots.is_empty() {
                self.copies.remove(&old.copy);
            } else if old.from <= copy.from || old.to >= copy.to {
                copy.stale = true;
            }
        }
    }

    /// Slot `slot` (with `record`, naming table entry `entry`) reads title
    /// memory `[from, to)` through host copy `copy` (`key`: its identity);
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn keep(
        &mut self,
        slot: u32,
        key: usize,
        copy: &C,
        [from, to]: [u64; 2],
        record: Record,
        entry: Option<u32>,
    ) {
        if let Some(old) = self.slots.get_mut(&slot)
            && old.copy == key
        {
            let record_changed = old.record != record;
            let (old_from, old_to) = (old.from, old.to);
            old.from = from;
            old.to = to;
            old.record = record;
            if let Some(c) = self.copies.get_mut(&key) {
                if (old_from <= c.from && from > old_from) || (old_to >= c.to && to < old_to) {
                    c.stale = true;
                }
                c.from = c.from.min(from);
                c.to = c.to.max(to);
            }
            if record_changed {
                self.note_entry(slot, entry);
            }
            return;
        }
        self.forget(slot);
        self.note_entry(slot, entry);
        self.slots.insert(slot, Slot { copy: key, from, to, record });
        let c = self.copies.entry(key).or_insert_with(|| Copy {
            copy: copy.clone(),
            slots: HashSet::new(),
            from,
            to,
            stale: false,
        });
        c.slots.insert(slot);
        c.from = c.from.min(from);
        c.to = c.to.max(to);
    }

    /// Slot `slot` holds a buffer view whose memory has no host copy yet.
    pub(crate) fn orphan(&mut self, slot: u32, record: Record, entry: Option<u32>) {
        if self.orphans.get(&slot) == Some(&record) {
            return;
        }
        self.forget(slot);
        self.note_entry(slot, entry);
        self.orphans.insert(slot, record);
    }

    fn note_entry(&mut self, slot: u32, entry: Option<u32>) {
        // Only kept with the incremental pass (the old one does not use it).
        if let Some(e) = entry
            && !self.full
        {
            // (Maybe twice: the pass's set of due slots takes it once.)
            self.by_entry.entry(e).or_default().push(slot);
        }
    }

    /// The views kept.
    pub(crate) fn views(&self) -> usize {
        self.slots.len()
    }

    /// Visits each host copy and the span its views read: `(&copy, from, to)`,
    /// recomputing stale spans in place without allocating a `Vec` or cloning `C`.
    pub(crate) fn for_each_span(&mut self, mut f: impl FnMut(&C, u64, u64)) {
        let slots = &self.slots;
        for c in self.copies.values_mut() {
            if c.stale {
                c.stale = false;
                let (mut from, mut to) = (u64::MAX, 0);
                for s in c.slots.iter().filter_map(|s| slots.get(s)) {
                    from = from.min(s.from);
                    to = to.max(s.to);
                }
                (c.from, c.to) = (from, to);
            }
            f(&c.copy, c.from, c.to);
        }
    }

    /// Per host copy, the span its views read: `(copy, from, to)`.
    #[cfg(test)]
    pub(crate) fn spans(&mut self) -> Vec<(C, u64, u64)> {
        if self.full {
            // As the old pass did: from every view, every pass.
            let mut spans: HashMap<usize, (u64, u64)> = HashMap::new();
            for s in self.slots.values() {
                let span = spans.entry(s.copy).or_insert((s.from, s.to));
                span.0 = span.0.min(s.from);
                span.1 = span.1.max(s.to);
            }
            return spans
                .into_iter()
                .filter_map(|(key, (from, to))| Some((self.copies.get(&key)?.copy.clone(), from, to)))
                .collect();
        }
        let slots = &self.slots;
        self.copies
            .values_mut()
            .map(|c| {
                if c.stale {
                    c.stale = false;
                    let (mut from, mut to) = (u64::MAX, 0);
                    for s in c.slots.iter().filter_map(|s| slots.get(s)) {
                        from = from.min(s.from);
                        to = to.max(s.to);
                    }
                    (c.from, c.to) = (from, to);
                }
                (c.copy.clone(), c.from, c.to)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A heap of slots with records, as title memory holds them.
    struct Heap {
        records: HashMap<u32, Record>,
    }

    impl Heap {
        fn record(&self, slot: u32) -> Option<Record> {
            self.records.get(&slot).copied()
        }
    }

    fn entry(r: Record) -> Option<u32> {
        Some(r[1])
    }

    /// One pass: plans, then "looks" at each due slot: a slot reads 16
    /// bytes from `slot * 16` of copy `slot / 4`.
    fn pass(p: &mut Picked<u32>, heap: &Heap, changed: Vec<u32>, forgotten: Vec<u32>) -> Vec<u32> {
        p.note_forgotten(Some(&forgotten));
        let due = p.plan(
            changed,
            || {
                let mut all: Vec<u32> = heap.records.keys().copied().collect();
                all.sort_unstable();
                all
            },
            |s| heap.record(s),
            entry,
        );
        for &slot in &due {
            let Some(record) = heap.record(slot) else {
                p.forget(slot);
                continue;
            };
            let copy = slot / 4;
            let from = u64::from(slot) * 16;
            p.keep(slot, copy as usize, &copy, [from, from + 16], record, entry(record));
        }
        due
    }

    fn heap(n: u32) -> Heap {
        Heap {
            records: (0..n).map(|s| (s, [1, 100 + s, 1])).collect(),
        }
    }

    #[test]
    fn only_slots_changed_since_the_last_pass_are_looked_at_again() {
        let mut heap = heap(64);
        let mut p = Picked::new(false);
        // The first pass looks at all of them.
        assert_eq!(pass(&mut p, &heap, vec![], vec![]).len(), 64);
        assert_eq!(p.views(), 64);
        // Nothing changed: nothing is looked at.
        assert!(pass(&mut p, &heap, vec![], vec![]).is_empty());
        // A view written through the API.
        heap.records.insert(5, [1, 500, 1]);
        assert_eq!(pass(&mut p, &heap, vec![5], vec![]), vec![5]);
        // A record copied past the API: found by its words alone.
        heap.records.insert(9, [1, 900, 1]);
        assert_eq!(pass(&mut p, &heap, vec![], vec![]), vec![9]);
        // The resource of entry 117 (slot 17) released: the record is as
        // it was, the slot is looked at all the same; slot 5 no longer
        // names its old entry 105 and is not.
        assert_eq!(pass(&mut p, &heap, vec![], vec![117, 105]), vec![17]);
        // And the next pass leaves them alone again.
        assert!(pass(&mut p, &heap, vec![], vec![]).is_empty());
    }

    #[test]
    fn a_slot_that_went_shrinks_its_copy_span() {
        let mut heap = heap(8);
        let mut p = Picked::new(false);
        pass(&mut p, &heap, vec![], vec![]);
        let span = |p: &mut Picked<u32>, copy: u32| p.spans().into_iter().find(|s| s.0 == copy).map(|s| (s.1, s.2));
        assert_eq!(span(&mut p, 1), Some((64, 128)));
        heap.records.remove(&7);
        pass(&mut p, &heap, vec![7], vec![]);
        assert_eq!(span(&mut p, 1), Some((64, 112)));
        for s in 4..7 {
            heap.records.remove(&s);
        }
        pass(&mut p, &heap, vec![4, 5, 6], vec![]);
        assert_eq!(span(&mut p, 1), None);
        assert_eq!(span(&mut p, 0), Some((0, 64)));
    }

    #[test]
    fn orphans_are_tried_again_every_16_passes_and_all_slots_now_and_then() {
        let heap = heap(4);
        let mut p: Picked<u32> = Picked::new(false);
        let _ = p.plan(vec![], || vec![0, 1, 2, 3], |s| heap.record(s), entry);
        for s in 0..3 {
            p.keep(s, 0, &0, [0, 16], heap.record(s).unwrap(), None);
        }
        p.orphan(3, heap.record(3).unwrap(), None);
        for n in 1..FULL_EVERY {
            let due = p.plan(vec![], || vec![0, 1, 2, 3], |s| heap.record(s), entry);
            let want: Vec<u32> = if n.is_multiple_of(ORPHANS_EVERY) { vec![3] } else { vec![] };
            assert_eq!(due, want, "pass {n}");
        }
        let due = p.plan(vec![], || vec![0, 1, 2, 3], |s| heap.record(s), entry);
        assert_eq!(due, vec![0, 1, 2, 3]);
    }

    #[test]
    fn the_old_pass_looks_at_all_every_16_passes() {
        let heap = heap(4);
        let mut p: Picked<u32> = Picked::new(true);
        for n in 0..40u64 {
            let due = p.plan(vec![2], || vec![0, 1, 2, 3], |s| heap.record(s), entry);
            let want: Vec<u32> = if n.is_multiple_of(ORPHANS_EVERY) { vec![0, 1, 2, 3] } else { vec![2] };
            assert_eq!(due, want, "pass {n}");
        }
    }

    #[test]
    fn updating_a_slot_in_place_keeps_copy_clean_unless_span_shrinks() {
        let mut p: Picked<u32> = Picked::new(false);
        p.keep(0, 1, &1, [0, 32], [1, 10, 1], Some(10));
        p.keep(1, 1, &1, [32, 64], [1, 11, 1], Some(11));
        assert!(!p.copies[&1].stale);
        // Same span with a new record does not mark the copy stale.
        p.keep(0, 1, &1, [0, 32], [1, 20, 1], Some(20));
        assert!(!p.copies[&1].stale);
        assert_eq!(p.spans(), vec![(1, 0, 64)]);
        // Expanding the span keeps it clean and updates the bounds.
        p.keep(1, 1, &1, [32, 96], [1, 21, 1], Some(21));
        assert!(!p.copies[&1].stale);
        assert_eq!(p.spans(), vec![(1, 0, 96)]);
        // Shrinking an endpoint marks it stale and recomputes on spans().
        p.keep(1, 1, &1, [32, 48], [1, 22, 1], Some(22));
        assert!(p.copies[&1].stale);
        assert_eq!(p.spans(), vec![(1, 0, 48)]);
    }

    #[test]
    fn dirty_ranges_scan_only_slots_within_ranges_and_none_scans_all() {
        let mut heap = heap(512);
        let mut p = Picked::new(false);
        assert_eq!(pass(&mut p, &heap, vec![], vec![]).len(), 512);
        heap.records.insert(15, [1, 1500, 1]);
        heap.records.insert(500, [1, 5000, 1]);
        let mut probed = Vec::new();
        let due = p.plan_in_ranges(
            vec![],
            || vec![],
            |s| {
                probed.push(s);
                heap.record(s)
            },
            entry,
            Some(&[(10, 20)]),
        );
        assert_eq!(due, vec![15]);
        assert!(probed.iter().all(|&s| (10..20).contains(&s)));
        assert!(!probed.contains(&500));
        let r15 = heap.record(15).unwrap();
        p.keep(15, 15 / 4, &(15 / 4), [15 * 16, 16 * 16], r15, entry(r15));
        let due = p.plan_in_ranges(vec![], || vec![], |s| heap.record(s), entry, None);
        assert_eq!(due, vec![500]);
    }

    #[test]
    fn for_each_span_matches_spans_for_both_modes_and_recomputes_stale() {
        let collect = |p: &mut Picked<u32>| {
            let mut out = Vec::new();
            p.for_each_span(|copy, from, to| out.push((*copy, from, to)));
            out.sort_unstable();
            out
        };
        let collect_spans = |p: &mut Picked<u32>| {
            let mut out = p.spans();
            out.sort_unstable();
            out
        };
        for full in [false, true] {
            let mut p: Picked<u32> = Picked::new(full);
            p.keep(0, 1, &1, [16, 48], [1, 10, 1], Some(10));
            p.keep(1, 1, &1, [32, 96], [1, 11, 1], Some(11));
            p.keep(2, 2, &2, [100, 200], [1, 12, 1], Some(12));
            assert_eq!(collect(&mut p), vec![(1, 16, 96), (2, 100, 200)]);
            assert_eq!(collect(&mut p), collect_spans(&mut p));
            // Shrink an endpoint in place -> marks stale, recomputed on visit.
            p.keep(1, 1, &1, [32, 64], [1, 13, 1], Some(13));
            assert!(p.copies[&1].stale);
            assert_eq!(collect(&mut p), vec![(1, 16, 64), (2, 100, 200)]);
            assert!(!p.copies[&1].stale);
            assert_eq!(collect(&mut p), collect_spans(&mut p));
            // Forgetting an endpoint shrinks copy 1; forgetting copy 2 removes it.
            p.forget(0);
            p.forget(2);
            assert_eq!(collect(&mut p), vec![(1, 32, 64)]);
            assert_eq!(collect(&mut p), collect_spans(&mut p));
        }
    }

    #[test]
    fn sliced_full_rescan_spreads_work_and_drops_removed_slots() {
        let mut heap = heap(16);
        let mut p: Picked<u32> = Picked::with_options(true, 1, 4);
        // Pass 0 populates all slots immediately.
        assert_eq!(pass(&mut p, &heap, vec![], vec![]).len(), 16);
        assert_eq!(p.views(), 16);
        // Remove slot 5 (5 % 4 == 1) without reporting it in `changed`.
        heap.records.remove(&5);
        // Passes 1..=4 visit slices 1, 2, 3, 0 — each at most 4 slots, covering
        // every live slot once across the 4 passes and dropping stale slot 5 on pass 1.
        let mut seen = Vec::new();
        for round in 1..=4u32 {
            let due = pass(&mut p, &heap, vec![], vec![]);
            assert!(
                due.len() <= 4,
                "round {round} scanned too many slots: {due:?}"
            );
            seen.extend(due);
            if round == 1 {
                assert_eq!(p.views(), 15, "stale slot 5 forgotten on slice 1");
            }
        }
        seen.sort_unstable();
        let expected: Vec<u32> = (0..16).filter(|&s| s != 5).collect();
        assert_eq!(seen, expected);
    }

    #[test]
    fn sliced_dirty_safety_net_avoids_single_frame_full_clear() {
        let mut heap = heap(32);
        // Set scan_every > FULL_EVERY so the periodic sliced safety net is what
        // visits every slot and drops the silently-removed slot 18.
        let mut p: Picked<u32> = Picked::with_options(false, FULL_EVERY + 1, 16);
        // Pass 0 initializes everything.
        assert_eq!(pass(&mut p, &heap, vec![], vec![]).len(), 32);
        assert_eq!(p.views(), 32);
        // Remove slot 18 (18 % 16 == 2) silently.
        heap.records.remove(&18);
        let mut rescanned = Vec::new();
        for n in 1..=FULL_EVERY {
            let due = pass(&mut p, &heap, vec![], vec![]);
            assert!(
                due.len() <= 2,
                "pass {n} rescanned more than one slice: {due:?}"
            );
            rescanned.extend(due);
        }
        rescanned.sort_unstable();
        rescanned.dedup();
        let expected: Vec<u32> = (0..32).filter(|&s| s != 18).collect();
        assert_eq!(rescanned, expected);
        assert_eq!(p.views(), 31);
    }
}
