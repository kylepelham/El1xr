//! (El1xr) `VICTORY_ALIAS_SKIP=1`: an alias copy ([`super::alias`]) whose
//! source and destination nobody wrote since the last copy between them ran
//! is skipped on the card.
//!
//! Each list copies an alias again at its first read of it (some 77 copies
//! a frame, most of them of lighting textures written once and read by many
//! lists). Lists are recorded on many threads, so whether the source changed
//! since the last copy is only known at submission, in the order of the
//! queue. Each copy is therefore recorded under predication on a slot of the
//! list's own: the slot is 1 (run) when recorded, and at submission
//! ([`submitted`], in queue order) it becomes 0 (skip) only when the last
//! copy into the destination that ran was from the same source and no list
//! submitted since, nor this one before the copy, used either texture in a
//! state that writes it ([`used`]). A list sent again sets every slot to 1.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use windows::Win32::Graphics::Direct3D12::*;

use crate::host::HostList;
use crate::resource::HostTexture;

/// Whether the skip is on (`VICTORY_ALIAS_SKIP=1`; default off).
pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_ALIAS_SKIP").is_some_and(|v| v == "1"))
}

/// Predicate slots of one list (8 bytes each); copies past them run always.
const SLOTS: u32 = 512;

/// Host states that only read: vertex and constant, index, depth read,
/// shader resources, indirect argument (predication), copy source, shading
/// rate source. Every other state, and common, may write.
const READS: u32 = 0x1 | 0x2 | 0x20 | 0x40 | 0x80 | 0x200 | 0x800 | 0x0100_0000;

fn writes(state: u32) -> bool {
    state == 0 || state & !READS != 0
}

/// What the skip asks of a texture.
trait Texture {
    /// Whether the title's CPU writes it: its uploads are not seen here.
    fn title_written(&self) -> bool;
}

impl Texture for HostTexture {
    fn title_written(&self) -> bool {
        HostTexture::title_written(self)
    }
}

enum Event<T = HostTexture> {
    /// A host texture used in a state that may write it.
    Wrote(usize),
    /// An alias copy, under the slot (none: it always runs).
    Copy {
        slot: Option<u32>,
        placed: Weak<T>,
        source: Weak<T>,
    },
}

/// What a list did that [`submitted`] needs, in the order it was recorded.
#[derive(Default)]
pub(crate) struct Log {
    /// Host textures the list's prelude may write ([`uploaded`]).
    prelude: Vec<usize>,
    events: Vec<Event>,
    /// The list's predicate slots: an upload buffer and its mapping.
    slots: Option<(ID3D12Resource, usize)>,
    used: u32,
    /// Sent since its last reset.
    sent: bool,
}

// SAFETY: the buffer is only recorded and written by the list's holder, and
// its mapping is plain upload memory.
unsafe impl Send for Log {}

fn key<T>(copy: &Arc<T>) -> usize {
    Arc::as_ptr(copy) as usize
}

/// A list uses `copy` in `state` (the host state asked for).
pub(crate) fn used(log: &Mutex<Log>, copy: &Arc<HostTexture>, state: u32) {
    if !on() || !writes(state) {
        return;
    }
    let mut log = log.lock().unwrap_or_else(|e| e.into_inner());
    log.events.push(Event::Wrote(key(copy)));
}

/// `resource`'s host copy may get title memory in the list's prelude
/// (`super::upload`): a write before everything the list does.
pub(crate) fn uploaded(list: &HostList, resource: crate::object::This) {
    if !on() {
        return;
    }
    if let Some(copy) = crate::resource::host_copy(resource) {
        let mut log = list.alias_log.lock().unwrap_or_else(|e| e.into_inner());
        log.prelude.push(key(&copy));
    }
}

/// The list was reset: its earlier runs are over.
pub(crate) fn reset(log: &Mutex<Log>) {
    if !on() {
        return;
    }
    let mut log = log.lock().unwrap_or_else(|e| e.into_inner());
    log.prelude.clear();
    log.events.clear();
    log.used = 0;
    log.sent = false;
}

/// The predicate an alias copy from `source` into `placed` is recorded
/// under (skipped while its value is 0), or `None`: it runs always. The
/// caller moved `placed` to the copy destination state just before; that
/// use is the copy's own, not a write of someone else's.
pub(crate) fn predicate(
    list: &HostList,
    placed: &Arc<HostTexture>,
    source: &Arc<HostTexture>,
    always: bool,
) -> Option<(ID3D12Resource, u64)> {
    let mut log = list.alias_log.lock().unwrap_or_else(|e| e.into_inner());
    if matches!(log.events.last(), Some(Event::Wrote(k)) if *k == key(placed)) {
        log.events.pop();
    }
    if log.slots.is_none() && !always {
        log.slots = crate::host::host()
            .and_then(|host| host.upload_buffer(u64::from(SLOTS) * 8))
            .map(|(buffer, data)| {
                crate::host::name(&buffer, || "alias copy predicates".to_owned());
                (buffer, data as usize)
            });
    }
    let slot = match &log.slots {
        Some((buffer, data)) if !always && log.used < SLOTS => {
            let slot = log.used;
            // SAFETY: the mapping holds `SLOTS` slots; no run of the list
            // reads them (it was reset, or not sent yet).
            unsafe { ((data + slot as usize * 8) as *mut u64).write_unaligned(1) };
            Some((slot, buffer.clone()))
        }
        _ => None,
    };
    if slot.is_some() {
        log.used += 1;
    }
    log.events.push(Event::Copy {
        slot: slot.as_ref().map(|(s, _)| *s),
        placed: Arc::downgrade(placed),
        source: Arc::downgrade(source),
    });
    slot.map(|(s, buffer)| (buffer, u64::from(s) * 8))
}

/// The copies that ran last into a destination, by destination: their
/// source; and the destinations by source.
struct Fresh<T = HostTexture> {
    into: HashMap<usize, (Weak<T>, Weak<T>)>,
    from: HashMap<usize, Vec<usize>>,
}

impl<T> Default for Fresh<T> {
    fn default() -> Self {
        Fresh {
            into: HashMap::new(),
            from: HashMap::new(),
        }
    }
}

impl<T: Texture> Fresh<T> {
    const MOST: usize = 4096;

    fn wrote(&mut self, texture: usize) {
        self.into.remove(&texture);
        if let Some(placed) = self.from.remove(&texture) {
            for p in placed {
                self.into.remove(&p);
            }
        }
    }

    fn can_skip(&self, placed: &Weak<T>, source: &Weak<T>) -> bool {
        let (Some(p), Some(s)) = (placed.upgrade(), source.upgrade()) else {
            return false;
        };
        !s.title_written()
            && self
                .into
                .get(&key(&p))
                .is_some_and(|(_, from)| Weak::ptr_eq(from, source))
    }

    fn ran(&mut self, placed: &Weak<T>, source: &Weak<T>) {
        if self.into.len() >= Self::MOST || self.from.len() >= Self::MOST {
            self.into.clear();
            self.from.clear();
        }
        let (p, s) = (placed.as_ptr() as usize, source.as_ptr() as usize);
        self.into.insert(p, (placed.clone(), source.clone()));
        let into = self.from.entry(s).or_default();
        if !into.contains(&p) {
            into.push(p);
        }
    }
}

static FRESH: Mutex<Option<Fresh>> = Mutex::new(None);

/// Copies recorded under a predicate, and of them skipped.
static COUNTS: [AtomicU64; 2] = [AtomicU64::new(0), AtomicU64::new(0)];

/// `runs` go to the queue now, in this order, and nothing else of the
/// queue's goes between them and the next call: decides each copy's slot.
pub(crate) fn submitted(runs: &[&HostList]) {
    if !on() {
        return;
    }
    let mut fresh = FRESH.lock().unwrap_or_else(|e| e.into_inner());
    let fresh = fresh.get_or_insert_with(Fresh::default);
    for list in runs {
        let mut log = list.alias_log.lock().unwrap_or_else(|e| e.into_inner());
        let again = std::mem::replace(&mut log.sent, true);
        let data = log.slots.as_ref().map(|(_, data)| *data);
        for &texture in &log.prelude {
            fresh.wrote(texture);
        }
        decide(fresh, &log.events, again, |slot, run| {
            if let Some(data) = data {
                // SAFETY: a slot of the list's mapping; its runs before
                // read it already or run it whatever it holds (see `again`).
                unsafe { ((data + slot as usize * 8) as *mut u64).write_unaligned(u64::from(run)) };
            }
        });
    }
}

/// [`submitted`] for one list's events: `set(slot, run)` for each slot.
fn decide<T: Texture>(
    fresh: &mut Fresh<T>,
    events: &[Event<T>],
    again: bool,
    mut set: impl FnMut(u32, bool),
) {
    for event in events {
        match event {
            Event::Wrote(texture) => fresh.wrote(*texture),
            Event::Copy {
                slot,
                placed,
                source,
            } => {
                let skip = slot.is_some() && !again && fresh.can_skip(placed, source);
                if let Some(slot) = slot {
                    set(*slot, !skip);
                    let n = COUNTS[0].fetch_add(1, Ordering::Relaxed) + 1;
                    if skip {
                        COUNTS[1].fetch_add(1, Ordering::Relaxed);
                    }
                    if n.is_multiple_of(10_000) {
                        tracing::info!(
                            "alias copies skipped (VICTORY_ALIAS_SKIP=1): {} of the last {n}",
                            COUNTS[1].swap(0, Ordering::Relaxed)
                        );
                        COUNTS[0].store(0, Ordering::Relaxed);
                    }
                }
                if skip {
                    continue;
                }
                if slot.is_some() {
                    fresh.ran(placed, source);
                } else {
                    fresh.wrote(placed.as_ptr() as usize);
                }
            }
        }
    }
}

/// Records `copy` under the predicate (if any): skipped on the card while
/// the slot holds 0.
pub(crate) fn under(
    list: &ID3D12GraphicsCommandList,
    predicate: Option<&(ID3D12Resource, u64)>,
    copy: impl FnOnce(),
) {
    let Some((buffer, at)) = predicate else {
        return copy();
    };
    // SAFETY: an open direct list; an upload buffer (always in the
    // predication state) the list holds while it may run.
    unsafe { list.SetPredication(buffer, *at, D3D12_PREDICATION_OP_EQUAL_ZERO) };
    copy();
    // SAFETY: as above.
    unsafe { list.SetPredication(None, 0, D3D12_PREDICATION_OP_EQUAL_ZERO) };
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A texture of the tests: none is written by the title.
    struct Tex;

    impl Texture for Tex {
        fn title_written(&self) -> bool {
            false
        }
    }

    /// A copy log decided on one `Fresh`: the slots skipped.
    fn skipped(fresh: &mut Fresh<Tex>, events: &[Event<Tex>], again: bool) -> Vec<u32> {
        let mut out = Vec::new();
        decide(fresh, events, again, |slot, run| {
            if !run {
                out.push(slot);
            }
        });
        out
    }

    fn copy(slot: u32, placed: &Arc<Tex>, source: &Arc<Tex>) -> Event<Tex> {
        Event::Copy {
            slot: Some(slot),
            placed: Arc::downgrade(placed),
            source: Arc::downgrade(source),
        }
    }

    #[test]
    fn only_untouched_pairs_are_skipped() {
        let (placed, source, other) = (Arc::new(Tex), Arc::new(Tex), Arc::new(Tex));
        let mut fresh = Fresh::default();
        // The first copy runs; the next list's copy of the same pair is skipped.
        assert!(skipped(&mut fresh, &[copy(0, &placed, &source)], false).is_empty());
        assert_eq!(
            skipped(&mut fresh, &[copy(0, &placed, &source)], false),
            [0]
        );
        // A list sent again runs every copy.
        assert!(skipped(&mut fresh, &[copy(0, &placed, &source)], true).is_empty());
        // A write of the source in a list between, or before the copy in the
        // same list, makes it run; the one after in that list is skipped.
        assert!(skipped(&mut fresh, &[Event::Wrote(key(&source))], false).is_empty());
        assert!(skipped(&mut fresh, &[copy(0, &placed, &source)], false).is_empty());
        let events = [
            copy(0, &placed, &source),
            Event::Wrote(key(&source)),
            copy(1, &placed, &source),
            copy(2, &placed, &source),
        ];
        assert_eq!(skipped(&mut fresh, &events, false), [0, 2]);
        // A write of the destination makes it run.
        let events = [Event::Wrote(key(&placed)), copy(0, &placed, &source)];
        assert!(skipped(&mut fresh, &events, false).is_empty());
        // Another source into the same destination runs, and then the first again.
        assert!(skipped(&mut fresh, &[copy(0, &placed, &other)], false).is_empty());
        assert!(skipped(&mut fresh, &[copy(0, &placed, &source)], false).is_empty());
        // A copy that always runs (no slot) leaves the next one to run too.
        let events = [
            Event::Copy {
                slot: None,
                placed: Arc::downgrade(&placed),
                source: Arc::downgrade(&source),
            },
            copy(0, &placed, &source),
        ];
        assert!(skipped(&mut fresh, &events, false).is_empty());
        // A texture let go: its copy runs.
        let gone = Arc::new(Tex);
        assert!(skipped(&mut fresh, &[copy(0, &placed, &gone)], false).is_empty());
        let events = [copy(0, &placed, &gone)];
        drop(gone);
        assert!(skipped(&mut fresh, &events, false).is_empty());
    }

    #[test]
    fn write_states() {
        assert!(writes(0));
        assert!(writes(0x4) && writes(0x8) && writes(0x10) && writes(0x400) && writes(0x1000));
        assert!(writes(0x8 | 0x40));
        assert!(!writes(0x40 | 0x80) && !writes(0x800) && !writes(0x20) && !writes(0x200));
    }
}
