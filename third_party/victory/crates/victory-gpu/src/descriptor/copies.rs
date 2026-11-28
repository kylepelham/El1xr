//! (phonescreen, #115, round 8; `crate::heap_find`) Copies of a found
//! view's descriptor anywhere in the title's view heaps, shader-visible or
//! not.
//!
//! The heap walk of `crate::heap_find` reads the shader-visible heaps for
//! records whose live view holds the memory looked for. It does not see:
//! - a copy in a heap the card does not see (`CopyDescriptors` sources, or
//!   the title's own staging heaps), from which the title copies into a
//!   shader-visible slot later;
//! - a record that names the view's entry at an older generation (stale:
//!   the resource it was written for was released, so the record reads as
//!   no view and its host slot is null), though on the console a
//!   descriptor names memory, not an object, and shows whatever lies there
//!   now;
//! - a record copied to an offset that is not a slot's start.
//!
//! [`copies_where`] walks every view heap for those. The memory of the
//! views freed with their resource is kept ([`note_former`], only while
//! `VICTORY_HEAP_FIND` is on), so a stale record can be told to have shown
//! the memory looked for.

use std::collections::HashMap;
use std::sync::{Mutex, RwLock};

use super::{Heap, KIND_SHIFT, VIEW_TAG, VIEWS_KIND, View, increment, views_read};

/// Every view heap, shader-visible or not: its heap object.
static VIEW_HEAPS: RwLock<Vec<usize>> = RwLock::new(Vec::new());

/// A view heap was made (`at`: the heap object).
pub(super) fn register(at: usize) {
    VIEW_HEAPS
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .push(at);
}

/// The heap object `at` is going away.
pub(super) fn unregister(at: usize) {
    VIEW_HEAPS
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|&h| h != at);
}

/// The memory (start, bytes) of the resource whose release freed entry
/// `i` at generation `g`, by (i, g).
static FORMER: Mutex<Option<HashMap<(u32, u32), (u64, u64)>>> = Mutex::new(None);

/// Freed entries kept at most (the map starts over past it).
const MOST_FORMER: usize = 1 << 18;

/// Entry `i`, at generation `generation`, was freed with a resource lying
/// at `span`.
pub(super) fn note_former(i: u32, generation: u32, span: (u64, u64)) {
    let mut former = FORMER.lock().unwrap_or_else(|e| e.into_inner());
    let map = former.get_or_insert_with(HashMap::new);
    if map.len() >= MOST_FORMER {
        map.clear();
    }
    map.insert((i, generation), span);
}

fn former_of(i: u32, generation: u32) -> Option<(u64, u64)> {
    FORMER
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()?
        .get(&(i, generation))
        .copied()
}

/// What a slot of a walk holds that names the view looked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    /// The very record of a found slot, in another slot.
    SameRecord,
    /// A live view whose memory holds what is looked for, in a heap the
    /// card does not see (those of shader-visible heaps are the walk's own).
    SameMemory,
    /// A record of a freed view that showed that memory.
    Stale,
    /// A found record copied at an offset that is not a slot's start.
    Shifted,
    /// A descriptor the title wrote itself, in a heap the card does not see.
    Raw,
}

impl Kind {
    pub(crate) fn text(self) -> &'static str {
        match self {
            Kind::SameRecord => "same_record",
            Kind::SameMemory => "same_memory",
            Kind::Stale => "stale",
            Kind::Shifted => "shifted",
            Kind::Raw => "raw",
        }
    }
}

/// One slot found: heap start, whether the card sees the heap, index,
/// what it holds, its first words, the memory it showed (a stale one).
pub(crate) struct Copy {
    pub(crate) heap: u64,
    pub(crate) visible: bool,
    pub(crate) index: u32,
    pub(crate) kind: Kind,
    pub(crate) words: [u32; 8],
    pub(crate) former: Option<(u64, u64)>,
}

/// The counts of a walk: heaps (shader-visible ones), slots read, records
/// of freed views (in shader-visible heaps).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Walked {
    pub(crate) heaps: u32,
    pub(crate) visible_heaps: u32,
    pub(crate) slots: u64,
    pub(crate) stale_visible: u64,
}

/// Whether one of `records` lies in `words` past its first word.
pub(crate) fn shifted(words: &[u32; 8], records: &[[u32; 3]]) -> bool {
    (1..=5).any(|k| records.iter().any(|r| words[k..k + 3] == r[..]))
}

/// What the slot `words` names of the view looked for, if anything.
/// `visible`: in a shader-visible heap. `found`: the records of the found
/// slots. `live`: the record's live view, if it names one, taken by
/// `keep`. `former`: the memory a stale record's view showed.
pub(crate) fn classify(
    words: &[u32; 8],
    visible: bool,
    found: &[[u32; 3]],
    live: Option<bool>,
    former: Option<(u64, u64)>,
    meets: &dyn Fn(u64, u64) -> bool,
    raw: &dyn Fn(&[u32; 8]) -> bool,
) -> Option<Kind> {
    if words[0] != VIEW_TAG {
        if shifted(words, found) {
            return Some(Kind::Shifted);
        }
        return (!visible && raw(words)).then_some(Kind::Raw);
    }
    if found.iter().any(|r| words[..3] == r[..]) {
        return Some(Kind::SameRecord);
    }
    match live {
        Some(true) if !visible => Some(Kind::SameMemory),
        Some(_) => None,
        None => former
            .is_some_and(|(start, len)| meets(start, len))
            .then_some(Kind::Stale),
    }
}

/// Every view heap walked for slots naming the view looked for. `found`:
/// the found slots (heap start, index, record), not named again; `keep`:
/// whether a live view holds the memory; `meets`: whether memory (start,
/// bytes) meets it; `raw`: whether a descriptor the title wrote itself
/// names it. At most `most` slots.
pub(crate) fn copies_where(
    found: &[(u64, u32, [u32; 3])],
    mut keep: impl FnMut(&View) -> bool,
    meets: impl Fn(u64, u64) -> bool,
    raw: impl Fn(&[u32; 8]) -> bool,
    most: usize,
) -> (Vec<Copy>, Walked) {
    const CHUNK: u32 = 4096;
    let records: Vec<[u32; 3]> = found.iter().map(|f| f.2).collect();
    let mut out = Vec::new();
    let mut walked = Walked::default();
    let heaps = VIEW_HEAPS.read().unwrap_or_else(|e| e.into_inner());
    for &at in heaps.iter() {
        // SAFETY: registered heaps are live (removed when dropped, which
        // waits for this list's lock).
        let heap = unsafe { &*(at as *const Heap) };
        let Some(step) = increment(heap.desc.kind).filter(|_| heap.desc.kind == VIEWS_KIND) else {
            continue;
        };
        let visible = heap.desc.flags & super::SHADER_VISIBLE != 0;
        walked.heaps += 1;
        walked.visible_heaps += u32::from(visible);
        let mut first = 0;
        while first < heap.desc.count && out.len() < most {
            let last = first.saturating_add(CHUNK).min(heap.desc.count);
            let views = views_read();
            for index in first..last {
                if found.iter().any(|f| f.0 == heap.start && f.1 == index) {
                    continue;
                }
                let slot = heap.start + u64::from(index) * u64::from(step);
                // SAFETY: a view slot is 32 bytes of the heap's memory.
                let words = unsafe { (slot as *const [u32; 8]).read_unaligned() };
                walked.slots += 1;
                let (i, generation) = (words[1], words[2]);
                let entry = (words[0] == VIEW_TAG && generation >> KIND_SHIFT == 0)
                    .then(|| views.list.get(i as usize))
                    .flatten();
                let stale = entry.is_some_and(|(_, current)| *current != generation);
                let live = if words[0] != VIEW_TAG {
                    None
                } else if let Some((view, current)) = entry {
                    (*current == generation).then(|| keep(view))
                } else {
                    match generation >> KIND_SHIFT {
                        super::CONSTANT => Some(keep(&super::constant_of(i, generation))),
                        super::PLACED => views.placed_of(i, generation).map(|v| keep(&v)),
                        _ => None,
                    }
                };
                let former = stale.then(|| former_of(i, generation)).flatten();
                if stale && visible {
                    walked.stale_visible += 1;
                }
                let kind = classify(&words, visible, &records, live, former, &meets, &raw);
                if let Some(kind) = kind
                    && out.len() < most
                {
                    out.push(Copy {
                        heap: heap.start,
                        visible,
                        index,
                        kind,
                        words,
                        former,
                    });
                }
            }
            drop(views);
            first = last;
        }
    }
    (out, walked)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RECORD: [u32; 3] = [VIEW_TAG, 91_711, 7];

    fn slot(first: [u32; 3]) -> [u32; 8] {
        let mut w = [0u32; 8];
        w[..3].copy_from_slice(&first);
        w
    }

    fn never(_: u64, _: u64) -> bool {
        false
    }

    fn no_raw(_: &[u32; 8]) -> bool {
        false
    }

    /// The found record elsewhere is a copy wherever it lies; a live view
    /// of the memory counts only in a heap the card does not see (the
    /// walk of the shader-visible heaps names those itself).
    #[test]
    fn copies_and_views_of_the_memory_are_named() {
        let found = [RECORD];
        let c =
            |w: &[u32; 8], visible, live| classify(w, visible, &found, live, None, &never, &no_raw);
        assert_eq!(c(&slot(RECORD), true, Some(true)), Some(Kind::SameRecord));
        assert_eq!(c(&slot(RECORD), false, Some(true)), Some(Kind::SameRecord));
        let other = slot([VIEW_TAG, 5, 1]);
        assert_eq!(c(&other, false, Some(true)), Some(Kind::SameMemory));
        assert_eq!(c(&other, true, Some(true)), None);
        assert_eq!(c(&other, false, Some(false)), None);
    }

    /// A stale record is named when the memory its view showed meets what
    /// is looked for, not otherwise, and not without a memory kept.
    #[test]
    fn stale_records_are_named_by_the_memory_they_showed() {
        let found = [RECORD];
        let meets = |start: u64, len: u64| start < 0x2000 && 0x1000 < start + len;
        let stale = slot([VIEW_TAG, 5, 1]);
        let c = |former| classify(&stale, true, &found, None, former, &meets, &no_raw);
        assert_eq!(c(Some((0x1000, 0x100))), Some(Kind::Stale));
        assert_eq!(c(Some((0x3000, 0x100))), None);
        assert_eq!(c(None), None);
    }

    /// A found record copied at another offset is named; raw descriptors
    /// count only in heaps the card does not see.
    #[test]
    fn shifted_and_raw_copies_are_named() {
        let found = [RECORD];
        let mut w = [0u32; 8];
        w[2..5].copy_from_slice(&RECORD);
        assert!(shifted(&w, &found));
        assert_eq!(
            classify(&w, true, &found, None, None, &never, &no_raw),
            Some(Kind::Shifted)
        );
        assert!(!shifted(&[0; 8], &found));
        let yes = |_: &[u32; 8]| true;
        assert_eq!(
            classify(&[1; 8], false, &found, None, None, &never, &yes),
            Some(Kind::Raw)
        );
        assert_eq!(
            classify(&[1; 8], true, &found, None, None, &never, &yes),
            None
        );
    }

    /// The memory of freed views is kept by entry and generation.
    #[test]
    fn freed_views_keep_their_memory() {
        note_former(0xdead_0001, 3, (0x5000, 0x40));
        assert_eq!(former_of(0xdead_0001, 3), Some((0x5000, 0x40)));
        assert_eq!(former_of(0xdead_0001, 4), None);
    }
}
