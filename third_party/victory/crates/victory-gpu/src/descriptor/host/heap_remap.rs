//! (heapcap) `VICTORY_HEAP_CAP=3`: the title's view slots past a capped
//! host heap's get their own descriptors, packed into host slots the
//! dynamic range gives up.
//!
//! Layout on the RTX 5090 (null slot 999,999, title heap 1,245,184): title
//! slots 0..737,855 are the host's own, slot 737,855 is null, the next
//! [`compact_slots`] (`VICTORY_HEAP_CAP_COMPACT`, default 65,536) hold the
//! live views of title slots 737,855.., the rest up to 999,999 are the
//! dynamic slots. Translated shaders read title view indices at or past
//! 737,855 through a table (`victory_shader::emit::SLOT_REMAP`: dword `k`
//! the host slot of title slot 737,855 + k, the null slot when it has
//! none), kept in the heap's strides buffer from `REMAP_BASE` on.
//!
//! The title writes records past the host heap by copying them into its
//! heap's pages and by making views there; both mark those title slots
//! (`heap_cap::made_past`, `heap_cap::note_writes`), and every
//! [`RESCAN_EVERY`] frames all of them are looked at again (a view let go
//! without a write goes null). A record
//! naming a live view gets a packed slot and the view is made there; a slot
//! whose record no longer does gives its packed slot back (reused
//! [`REUSE_AFTER`] frames later). A view whose host resource is not there
//! yet stays null and is tried again.
//!
//! shortcut: a packed slot is not in the passes that make textures and
//! buffers picked by index ready (`draw::State::heap_textures`,
//! `heap_buffers`); it shows what the host copy holds when made from other
//! reads of the same resource.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// Frames between looks at every title slot past the host heap.
pub(crate) const RESCAN_EVERY: u64 = 300;
/// Frames a packed slot given back waits before another title slot gets it.
pub(crate) const REUSE_AFTER: u64 = 4;
/// Title slots read, and views made, at most per call of [`refresh`].
const READ_BUDGET: u32 = 1 << 16;
const MAKE_BUDGET: u32 = 4096;
/// Not-made packed views tried again per call, at most.
const RETRY_BUDGET: usize = 1024;

/// Host slots packed title views get (`VICTORY_HEAP_CAP_COMPACT`, 0 to
/// 131,072; default 65,536), taken from the dynamic slots.
pub(crate) fn compact_slots() -> u32 {
    static SLOTS: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *SLOTS.get_or_init(|| {
        std::env::var("VICTORY_HEAP_CAP_COMPACT")
            .ok()
            .and_then(|v| v.trim().parse::<u32>().ok())
            .unwrap_or(1 << 16)
            .min(1 << 17)
    })
}

/// The packed slots of a heap of `dynamic` dynamic slots whose title null
/// slot is dynamic slot `at`: `compact_slots()`, never more than leaves
/// half the dynamic slots.
pub(crate) fn compact_of(wanted: u32, at: u32, dynamic: u32) -> u32 {
    wanted.min((dynamic / 2).saturating_sub(at + 1))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Owner {
    title: u32,
    key: [u32; 3],
    made: bool,
}

/// What [`Packed::place`] says to do for a title slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Placed {
    /// Its packed slot shows its view already.
    Same,
    /// Make its view in this host slot and point its table entry there.
    Make(u32),
    /// Its view is gone: point its table entry at the null slot, make this
    /// host slot null.
    Drop(u32),
    /// No packed slot left: it reads the null slot.
    Overflow,
    /// No view, none before.
    Nothing,
}

/// The packed slots of one host heap: which title slot holds each.
pub(crate) struct Packed {
    pub id: u64,
    pub title_null: u32,
    pub base: u32,
    pub cap: u32,
    map: HashMap<u32, u32>,
    owner: Vec<Option<Owner>>,
    free: VecDeque<(u32, u64)>,
    next: u32,
    /// Title slot ranges to look at, and whether to make their views again
    /// even when the record is the same.
    dirty: Vec<(u32, u32, bool)>,
    seeded: bool,
    last_rescan: u64,
    pub counts: Counts,
}

/// Since the last line.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Counts {
    pub made: u64,
    pub unready: u64,
    pub dropped: u64,
    pub overflow: u64,
    pub unread: u64,
    pub looked: u64,
}

impl Packed {
    pub(crate) fn new(id: u64, title_null: u32, cap: u32) -> Self {
        Self {
            id,
            title_null,
            base: title_null + 1,
            cap,
            map: HashMap::new(),
            owner: vec![None; cap as usize],
            free: VecDeque::new(),
            next: 0,
            dirty: Vec::new(),
            seeded: false,
            last_rescan: 0,
            counts: Counts::default(),
        }
    }

    /// Title slots with a packed slot.
    pub(crate) fn mapped(&self) -> usize {
        self.map.len()
    }

    /// The table entry of title slot `slot`, if the table covers it.
    pub(crate) fn entry(&self, slot: u32) -> Option<u32> {
        slot.checked_sub(self.title_null)
            .filter(|&k| k < victory_shader::emit::REMAP_LEN)
    }

    /// Title slot `slot` holds a record naming a live view (`key`: the
    /// record's words) or not (`None`), at `frame`; `again`: make the view
    /// again even if the record is the same.
    pub(crate) fn place(
        &mut self,
        slot: u32,
        key: Option<[u32; 3]>,
        again: bool,
        frame: u64,
    ) -> Placed {
        match (key, self.map.get(&slot).copied()) {
            (Some(key), Some(k)) => {
                let Some(owner) = self.owner[k as usize].as_mut() else {
                    return Placed::Nothing;
                };
                if !again && owner.made && owner.key == key {
                    return Placed::Same;
                }
                owner.key = key;
                Placed::Make(self.base + k)
            }
            (Some(key), None) => {
                let k = match self.free.front() {
                    Some(&(k, at)) if frame >= at + REUSE_AFTER => {
                        self.free.pop_front();
                        k
                    }
                    _ if self.next < self.cap => {
                        self.next += 1;
                        self.next - 1
                    }
                    _ => {
                        self.counts.overflow += 1;
                        return Placed::Overflow;
                    }
                };
                self.map.insert(slot, k);
                self.owner[k as usize] = Some(Owner {
                    title: slot,
                    key,
                    made: false,
                });
                Placed::Make(self.base + k)
            }
            (None, Some(k)) => {
                self.map.remove(&slot);
                self.owner[k as usize] = None;
                self.free.push_back((k, frame));
                self.counts.dropped += 1;
                Placed::Drop(self.base + k)
            }
            (None, None) => Placed::Nothing,
        }
    }

    /// The view of host slot `host_slot` was made (`ok`) or left null for now.
    pub(crate) fn made(&mut self, host_slot: u32, ok: bool) {
        if let Some(Some(owner)) = host_slot
            .checked_sub(self.base)
            .and_then(|k| self.owner.get_mut(k as usize))
        {
            owner.made = ok;
        }
        if ok {
            self.counts.made += 1;
        } else {
            self.counts.unready += 1;
        }
    }

    /// Title slots whose packed view is not made yet, at most `most`.
    fn unmade(&self, most: usize) -> Vec<u32> {
        self.owner
            .iter()
            .flatten()
            .filter(|o| !o.made)
            .map(|o| o.title)
            .take(most)
            .collect()
    }
}

static STATE: Mutex<Vec<Packed>> = Mutex::new(Vec::new());
/// Title slot ranges marked by writes, until [`refresh`] takes them.
static PENDING: Mutex<Vec<(u32, u32)>> = Mutex::new(Vec::new());
/// The most dynamic slots taken at once since the last line, and ever.
pub(crate) static PEAK_TAKEN: [AtomicU64; 2] = [AtomicU64::new(0), AtomicU64::new(0)];

/// Title slots `[lo, hi)` were written (`heap_cap::made_past`, `note_writes`).
pub(crate) fn dirty(lo: u32, hi: u32) {
    if lo < hi {
        PENDING
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((lo, hi));
    }
}

/// The table entries of `host`'s strides buffer: `REMAP_LEN + 1` dwords.
fn table(host: &super::HostHeap) -> Option<*mut u32> {
    let (_, _, data) = host.strides.as_ref()?;
    // SAFETY: the buffer is SLOT_STRIDES_COUNT dwords; REMAP_BASE + REMAP_LEN + 1 of them.
    Some(unsafe { (*data as *mut u32).add(victory_shader::emit::REMAP_BASE as usize) })
}

fn put(table: *mut u32, entry: u32, value: u32) {
    if entry <= victory_shader::emit::REMAP_LEN {
        // SAFETY: inside the mapping; shaders read it while we write (a
        // dword, either value is one the entry had).
        unsafe { table.add(entry as usize).write_volatile(value) };
    }
}

/// A new view heap (`HostHeap::new`): with the switch, its table starts all
/// null and its packed slots free.
pub(super) fn born(host: &super::HostHeap) {
    if !super::heap_cap::remap_on() || host.sampler {
        return;
    }
    let title_null = victory_shader::emit::title_view_limit();
    if title_null == 0 {
        return;
    }
    let Some(table) = table(host) else {
        tracing::warn!(
            "heap cap: no strides buffer; title view slots past the host heap read null"
        );
        return;
    };
    // Every heap's table, packed or not: a zero entry would read slot 0.
    for entry in 0..=victory_shader::emit::REMAP_LEN {
        put(table, entry, title_null);
    }
    let Some(at) = super::heap_cap::reserved_of(title_null, host.count, host.dynamic) else {
        return;
    };
    let cap = compact_of(compact_slots(), at, host.dynamic);
    tracing::info!(
        title_null,
        packed_first = title_null + 1,
        packed = cap,
        dynamic_left = host.dynamic - cap - 1,
        "heap cap: title view slots past the host heap get packed host slots {} to {}",
        title_null + 1,
        title_null + 1 + cap
    );
    let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());
    // the newest few heaps (the title makes one)
    if state.len() >= 4 {
        state.remove(0);
    }
    state.push(Packed::new(host.id, title_null, cap));
}

/// The root SRV of the remap table for a list that draws with `host`'s
/// heap (`draw.rs`, `SLOT_REMAP`).
pub(crate) fn root(list: &crate::host::HostList, host: &super::HostHeap) -> Option<u64> {
    let gpu = host.strides_gpu()?;
    const ONCE: usize = usize::MAX - 0x5f;
    if list.first_use(ONCE)
        && let Some(buffer) = host.strides_buffer()
    {
        list.keep(buffer);
    }
    Some(gpu + u64::from(victory_shader::emit::REMAP_BASE) * 4)
}

/// Once a frame or more from `sync_heaps`, for the title view heap at
/// `start` of `title` slots: the marked title slots past the host heap
/// looked at and their packed views made or let go.
pub(super) fn refresh(start: u64, step: u64, host: &super::HostHeap, title: u32) {
    if step != 32 {
        return;
    }
    let mut guard = STATE.lock().unwrap_or_else(|e| e.into_inner());
    let Some(packed) = guard.iter_mut().find(|p| p.id == host.id) else {
        return;
    };
    let Some(table) = table(host) else { return };
    let frame = crate::frame::current();
    let end = title.min(
        packed
            .title_null
            .saturating_add(victory_shader::emit::REMAP_LEN),
    );
    if packed.title_null >= end {
        return;
    }
    let rescan = !packed.seeded || frame >= packed.last_rescan + RESCAN_EVERY;
    if rescan {
        if packed.seeded {
            line(packed, frame, title);
        }
        packed.dirty.push((packed.title_null, end, false));
        packed.seeded = true;
        packed.last_rescan = frame;
    }
    for (lo, hi) in std::mem::take(&mut *PENDING.lock().unwrap_or_else(|e| e.into_inner())) {
        let (lo, hi) = (lo.max(packed.title_null), hi.min(end));
        if lo < hi {
            packed.dirty.push((lo, hi, false));
        }
    }
    if frame.is_multiple_of(8) {
        for slot in packed.unmade(RETRY_BUDGET) {
            packed.dirty.push((slot, slot + 1, false));
        }
    }
    let (mut read, mut makes) = (0u32, 0u32);
    if packed.dirty.is_empty() {
        return;
    }
    let mut bytes = vec![0u8; 1 << 19];
    while let Some((lo, hi, again)) = packed.dirty.pop() {
        if read >= READ_BUDGET || makes >= MAKE_BUDGET {
            packed.dirty.push((lo, hi, again));
            break;
        }
        let n = (hi - lo)
            .min((bytes.len() / 32) as u32)
            .min(READ_BUDGET - read);
        if lo + n < hi {
            packed.dirty.push((lo + n, hi, again));
        }
        read += n;
        let buf = &mut bytes[..(n * 32) as usize];
        if !crate::host::read_into((start + u64::from(lo) * step) as *const u8, buf) {
            packed.counts.unread += u64::from(n);
            continue;
        }
        for k in 0..n {
            let slot = lo + k;
            let record = &buf[(k * 32) as usize..(k * 32 + 12) as usize];
            let word = |w: usize| {
                u32::from_le_bytes(record[w * 4..w * 4 + 4].try_into().unwrap_or_default())
            };
            let key = [word(0), word(1), word(2)];
            let view = (key[0] == super::super::VIEW_TAG)
                .then(|| super::super::view_at(key[1], key[2]))
                .flatten();
            packed.counts.looked += 1;
            let Some(entry) = packed.entry(slot) else {
                continue;
            };
            match packed.place(slot, view.as_ref().map(|_| key), again, frame) {
                Placed::Make(at) => {
                    let Some(view) = view else { continue };
                    makes += 1;
                    let ok = match super::make(&view, host.cpu(at)) {
                        Some((kept, _)) => {
                            host.keep(at, kept);
                            true
                        }
                        None => {
                            super::null_for(&view, host.cpu(at));
                            host.keep(at, None);
                            false
                        }
                    };
                    stride(host, at, super::view_stride(&view).unwrap_or(0));
                    put(table, entry, at);
                    packed.made(at, ok);
                }
                Placed::Drop(at) => {
                    put(table, entry, packed.title_null);
                    host.make_null(at);
                    stride(host, at, 0);
                }
                Placed::Same | Placed::Overflow | Placed::Nothing => {}
            }
        }
    }
}

/// The element stride shaders read for host slot `slot` (the strides buffer).
fn stride(host: &super::HostHeap, slot: u32, value: u32) {
    if let Some((_, _, data)) = &host.strides
        && slot < victory_shader::emit::SLOT_STRIDES_COUNT
    {
        // SAFETY: inside the mapping; as `HostHeap` writes strides.
        unsafe { (*data as *mut u32).add(slot as usize).write_volatile(value) };
    }
}

fn line(packed: &mut Packed, frame: u64, title: u32) {
    let c = std::mem::take(&mut packed.counts);
    tracing::warn!(
        frame,
        title_slots = title,
        title_null = packed.title_null,
        mapped = packed.mapped(),
        packed = packed.cap,
        made = c.made,
        unready = c.unready,
        dropped = c.dropped,
        overflow = c.overflow,
        unread = c.unread,
        looked = c.looked,
        backlog = packed.dirty.len(),
        dynamic_peak = PEAK_TAKEN[0].swap(0, Relaxed),
        dynamic_peak_ever = PEAK_TAKEN[1].load(Relaxed),
        "heap cap: packed title views"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    const NULL_SLOT: u32 = 737_855;

    #[test]
    fn every_live_title_slot_gets_its_own_packed_slot_and_dead_ones_none() {
        let mut p = Packed::new(1, NULL_SLOT, 1000);
        let live = |s: u32| Some([1, s, 1]);
        let mut seen = std::collections::HashSet::new();
        for s in (NULL_SLOT..NULL_SLOT + 2000).step_by(3) {
            match p.place(s, live(s), false, 0) {
                Placed::Make(at) => {
                    assert!((NULL_SLOT + 1..NULL_SLOT + 1001).contains(&at), "{at}");
                    assert!(seen.insert(at), "host slot {at} given twice");
                    p.made(at, true);
                }
                Placed::Overflow => assert!(seen.len() == 1000),
                other => panic!("{other:?}"),
            }
        }
        assert_eq!(p.mapped(), 667.min(1000));
        // the same record again: nothing to do; a new one: made again in the same slot
        let s = NULL_SLOT + 3;
        assert_eq!(p.place(s, live(s), false, 1), Placed::Same);
        let Placed::Make(at) = p.place(s, Some([1, 9, 9]), false, 1) else {
            panic!()
        };
        assert_eq!(at, p.base + 1);
        // dead: dropped, then nothing
        assert_eq!(p.place(s, None, false, 2), Placed::Drop(at));
        assert_eq!(p.place(s, None, false, 2), Placed::Nothing);
        assert_eq!(
            p.place(NULL_SLOT + 1, None, false, 2),
            Placed::Nothing,
            "never live"
        );
    }

    #[test]
    fn a_packed_slot_given_back_waits_before_reuse_and_overflow_reads_null() {
        let mut p = Packed::new(1, NULL_SLOT, 2);
        let (a, b, c) = (NULL_SLOT, NULL_SLOT + 10, NULL_SLOT + 20);
        assert_eq!(
            p.place(a, Some([1, 1, 1]), false, 0),
            Placed::Make(NULL_SLOT + 1)
        );
        assert_eq!(
            p.place(b, Some([1, 2, 1]), false, 0),
            Placed::Make(NULL_SLOT + 2)
        );
        assert_eq!(p.place(c, Some([1, 3, 1]), false, 0), Placed::Overflow);
        assert_eq!(p.counts.overflow, 1);
        assert_eq!(p.place(a, None, false, 10), Placed::Drop(NULL_SLOT + 1));
        assert_eq!(
            p.place(c, Some([1, 3, 1]), false, 11),
            Placed::Overflow,
            "not reused yet"
        );
        assert_eq!(
            p.place(c, Some([1, 3, 1]), false, 10 + REUSE_AFTER),
            Placed::Make(NULL_SLOT + 1)
        );
    }

    #[test]
    fn a_view_not_made_is_tried_again_and_a_rescan_makes_all_again() {
        let mut p = Packed::new(1, NULL_SLOT, 8);
        let key = Some([1, 5, 1]);
        let Placed::Make(at) = p.place(NULL_SLOT + 4, key, false, 0) else {
            panic!()
        };
        p.made(at, false);
        assert_eq!(p.unmade(10), [NULL_SLOT + 4]);
        assert_eq!(
            p.place(NULL_SLOT + 4, key, false, 1),
            Placed::Make(at),
            "not made: again"
        );
        p.made(at, true);
        assert!(p.unmade(10).is_empty());
        assert_eq!(p.place(NULL_SLOT + 4, key, false, 2), Placed::Same);
        assert_eq!(
            p.place(NULL_SLOT + 4, key, true, 2),
            Placed::Make(at),
            "a rescan"
        );
    }

    #[test]
    fn table_entries_cover_title_slots_from_the_null_slot() {
        let p = Packed::new(1, NULL_SLOT, 8);
        assert_eq!(p.entry(NULL_SLOT - 1), None);
        assert_eq!(p.entry(NULL_SLOT), Some(0));
        assert_eq!(p.entry(1_245_183), Some(1_245_183 - NULL_SLOT));
        assert_eq!(p.entry(NULL_SLOT + victory_shader::emit::REMAP_LEN), None);
        // the retail title fits the table
        assert!(1_245_184 - NULL_SLOT <= victory_shader::emit::REMAP_LEN);
    }

    #[test]
    fn packed_slots_leave_half_the_dynamic_slots() {
        assert_eq!(compact_of(1 << 16, 0, 1 << 18), 1 << 16);
        assert_eq!(compact_of(1 << 17, 0, 1 << 18), (1 << 17) - 1);
        assert_eq!(compact_of(1 << 16, 200_000, 1 << 18), 0);
    }
}
