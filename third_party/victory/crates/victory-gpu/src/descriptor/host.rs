//! Host descriptor heaps behind the title's shader-visible heaps (decision
//! 0007, step 3). Translated shaders index the heaps directly at the
//! title's descriptor positions, so each host heap keeps the title heap's
//! order: slot `i` is made from what the title wrote at slot `i`, when it
//! writes it (view creation and descriptor copies) and again when a draw
//! reads it through a table and finds the slot changed.

use std::sync::atomic::AtomicU64;
use std::sync::{Condvar, Mutex};

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;

use super::{VIEW_TAG, View, view_at};
use crate::host::host;

/// [`HostHeap::slot_flags`]: title slot `index` is in the queue of
/// textures picked by index ([`TextureQueue::slots`] or [`TextureQueue::rechecks`]).
const SLOT_IN_TEXTURES: u8 = 1 << 0;
/// [`HostHeap::slot_flags`]: it holds a placed texture view in
/// [`HostHeap::placed`] ([`Spans::by_slot`]).
const SLOT_IN_PLACED: u8 = 1 << 1;
/// [`HostHeap::slot_flags`]: it holds a buffer view in
/// [`HostHeap::buffers`].
const SLOT_IN_BUFFERS: u8 = 1 << 2;

/// Packed bitset of taken dynamic descriptor slots (32 KiB for 262,144 slots,
/// fits in L1 cache and skips 64 occupied slots per cycle).
struct DynamicBits {
    words: Vec<u64>,
    len: usize,
}

impl DynamicBits {
    fn new(len: usize) -> Self {
        Self {
            words: vec![0; len.div_ceil(64)],
            len,
        }
    }

    /// Index of the first set bit in `[start, end)`, or `None` if all are 0.
    fn first_taken(&self, start: usize, end: usize) -> Option<usize> {
        let mut at = start;
        while at < end {
            let w = at / 64;
            let b = at % 64;
            let bits = (end - at).min(64 - b);
            let mask = if bits == 64 {
                !0u64
            } else {
                ((1u64 << bits) - 1) << b
            };
            let hit = self.words[w] & mask;
            if hit != 0 {
                return Some(w * 64 + hit.trailing_zeros() as usize);
            }
            at += bits;
        }
        None
    }

    fn find_in(&self, start: usize, limit: usize, n: usize) -> Option<usize> {
        let mut at = start;
        while at + n <= limit {
            let w = at / 64;
            let b = at % 64;
            let shifted = self.words[w] >> b;
            let ones = shifted.trailing_ones() as usize;
            if ones > 0 {
                at += ones.min(64 - b);
                continue;
            }
            match self.first_taken(at, at + n) {
                None => return Some(at),
                Some(hit) => at = hit + 1,
            }
        }
        None
    }

    fn find_run(&self, cursor: usize, n: usize) -> Option<usize> {
        if n == 0 || n > self.len {
            return None;
        }
        if cursor + n <= self.len && self.first_taken(cursor, cursor + n).is_none() {
            return Some(cursor);
        }
        if let Some(at) = self.find_in(cursor, self.len, n) {
            return Some(at);
        }
        let second_end = cursor.saturating_add(n.saturating_sub(1)).min(self.len);
        self.find_in(0, second_end, n)
    }

    fn set_taken(&mut self, first: usize, n: usize) {
        let end = first.saturating_add(n).min(self.len);
        let mut at = first;
        while at < end {
            let w = at / 64;
            let b = at % 64;
            let bits = (end - at).min(64 - b);
            let mask = if bits == 64 {
                !0u64
            } else {
                ((1u64 << bits) - 1) << b
            };
            self.words[w] |= mask;
            at += bits;
        }
    }

    #[cfg(test)]
    fn clear_taken(&mut self, first: usize, n: usize) -> u32 {
        self.clear_taken_with(first, n, |_| {})
    }

    fn clear_taken_with(&mut self, first: usize, n: usize, mut on_freed: impl FnMut(usize)) -> u32 {
        let end = first.saturating_add(n).min(self.len);
        let mut at = first;
        let mut freed = 0u32;
        while at < end {
            let w = at / 64;
            let b = at % 64;
            let bits = (end - at).min(64 - b);
            let mask = if bits == 64 {
                !0u64
            } else {
                ((1u64 << bits) - 1) << b
            };
            let prev = self.words[w];
            let mut cleared = prev & mask;
            if cleared != 0 {
                freed += cleared.count_ones();
                self.words[w] = prev & !mask;
                while cleared != 0 {
                    let bit = cleared.trailing_zeros() as usize;
                    on_freed(w * 64 + bit);
                    cleared &= cleared - 1;
                }
            }
            at += bits;
        }
        freed
    }
}

/// Distinguishes host heaps even when a title heap address is reused.
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

pub(crate) struct HostHeap {
    id: u64,
    pub(crate) heap: ID3D12DescriptorHeap,
    cpu: usize,
    step: u32,
    /// Slots mirroring the title's; view heaps have `dynamic` more.
    count: u32,
    /// Dynamic slots past `count`: [`DYNAMIC`] in view heaps, none in
    /// sampler heaps. (Upstream falls back to 2^16 on a capped host heap;
    /// ours keeps 2^18 there: 2^16 and 2^17 both ran out in this title.)
    dynamic: u32,
    sampler: bool,
    /// The record (view index and generation) each slot was last made from.
    made: Mutex<Vec<u64>>,
    /// (Neptune) `made` again, read without its lock: a draw's tables name slots that
    /// nearly all show their view already, and the title records on several threads, which
    /// stood in line at that lock for every slot (two fifths of their working time in a
    /// sample). Written under `made`'s lock, with it.
    made_too: Vec<std::sync::atomic::AtomicU64>,
    /// A fingerprint of the record each slot held when the pass over
    /// records copied in as bytes last looked at it
    /// ([`HostHeap::mirror_copied`]; 0: never looked at). Made on first use.
    copied: Mutex<Vec<u64>>,
    /// The host resource each slot views, held while the slot does: shaders
    /// index heaps directly, so a slot the title has not rewritten since it
    /// released the resource may still be read, and a freed host resource
    /// read there is a GPU page fault (the console reads stale memory).
    /// (Neptune) A lock to each slot: the recording threads write different slots.
    kept: Vec<Mutex<Option<ID3D12Resource>>>,
    /// Where the search for free dynamic slots starts, and which are taken
    /// (by lists not yet reset).
    next: Mutex<(u32, DynamicBits)>,
    /// Signalled whenever [`HostHeap::release_many`] returns dynamic slots.
    released: Condvar,
    /// Per-heap frame and shortest table size that timed out waiting for
    /// dynamic slots (accessed under `next`; reset whenever slots return).
    timed_out_frame: std::sync::atomic::AtomicU64,
    timed_out_min: std::sync::atomic::AtomicU32,
    /// How many dynamic slots are free (changed under `next`'s lock): with
    /// fewer than a table needs, the search is not made. Once the world
    /// had taken them all, every later table of the frame searched every
    /// slot before it was refused.
    free: std::sync::atomic::AtomicU32,
    /// The shortest table that found no run of free slots since slots were
    /// last given back (changed under `next`'s lock; `u32::MAX`: none).
    /// Until something is given back the free runs only shrink, so a table
    /// of that length or longer is refused without the search. Enough
    /// slots were free, but in pieces, and every such table walked the
    /// whole ring before it was refused: 0.33 ms each with 131,072 slots,
    /// 1.0 s of the recording threads in a frame that refused 3,143 (run
    /// 45), under the lock all of them need.
    too_long: std::sync::atomic::AtomicU32,
    /// View heaps: the bytes per element of the buffer view at each title
    /// slot, for shaders that pick a buffer by an index they compute
    /// (`victory_shader::emit::SLOT_STRIDES`): an upload buffer of
    /// `SLOT_STRIDES_COUNT` dwords, its GPU address and its mapping.
    strides: Option<(ID3D12Resource, u64, usize)>,
    /// Title slots holding buffer views. Such a view gets its host slot
    /// (and its memory a host copy) only when something reads it: a draw
    /// through a table, or [`HostHeap::buffer_slots`] for shaders that pick
    /// buffers from the whole heap.
    buffers: Mutex<std::collections::BTreeSet<u32>>,
    /// Of those and of the slots that left the set: the ones written since
    /// [`HostHeap::buffer_changes`] last took them.
    changed: Mutex<std::collections::HashSet<u32>>,
    /// Title slots holding a view of a texture the title may fill itself (a
    /// placed texture, a texture object), with the frame from which they
    /// are due: such a texture is uploaded when a draw reads it through a
    /// table; one a shader picks by a computed index is brought up to date
    /// from here ([`HostHeap::texture_slots_due`]).
    textures: Mutex<TextureQueue>,
    /// Per title slot, the frame (plus one; 0: never) in which a draw last
    /// read it through a table, or made a table it lies in (with
    /// [`TABLED_READ`] set if the draw read it). The draw that
    /// reads such a slot brings its texture up to date itself and the slot
    /// then leaves `textures`; a write of a slot that was a table's within
    /// the last [`TABLED_FRAMES`] frames is due only [`TABLED_GRACE`]
    /// frames later, by when its draw has read it. A title that builds a
    /// table per draw writes thousands of texture views a frame into slots
    /// only tables read (the title: some 2,500 a frame in its
    /// opening cutscene, a few hundred textures over and over), and every
    /// one of them stood in the queue of textures picked by index, ahead
    /// of or behind the ones shaders do pick by index.
    tabled: Vec<std::sync::atomic::AtomicU32>,
    /// (Neptune) Per block of [`TABLED_BLOCK`] title slots, the frame (plus one; 0: never) in
    /// which a draw last made a table with a slot in it ([`HostHeap::left_to_draws`]).
    tabled_blocks: Vec<std::sync::atomic::AtomicU32>,
    /// Per title slot, bits saying which sets the slot may be in without
    /// taking their lock to ask ([`SLOT_IN_TEXTURES`],
    /// [`SLOT_IN_PLACED`], [`SLOT_IN_BUFFERS`]): `note_buffer_into` runs
    /// for every descriptor write a frame (some 40,000 in `sync_heaps`),
    /// and most of them take a slot out of sets it is not in, which took
    /// the set's lock for nothing. A bit that is not set says the slot is
    /// not in the set (the slot may be in it with the bit set, which only
    /// costs the lock); dynamic slots and sampler heaps have none.
    slot_flags: Vec<std::sync::atomic::AtomicU8>,
    /// The title memory each placed texture view of this heap describes,
    /// by slot and by address ([`Spans`]): which slots a newer view of
    /// other memory there makes stale, and which to look at again when the
    /// runtime maps, unmaps or writes memory ([`remapped`]).
    placed: std::sync::Arc<Mutex<Spans>>,
}

/// The spans of every view heap made ([`HostHeap::placed`]), for the
/// texture code, which has no heap at hand ([`next_start`],
/// [`first_viewed`]).
static PLACED_SPANS: Mutex<Vec<std::sync::Weak<Mutex<Spans>>>> = Mutex::new(Vec::new());

/// Where the next texture placed inside `[start, end)` starts, past the
/// first bytes of the one at `start` ([`HEAD`]): what is there of the
/// texture at `start` ends there, whatever its full size. `None`: no view
/// in a slot shaders pick by index describes memory in that range.
pub(crate) fn next_start(start: u64, end: u64) -> Option<u64> {
    let heaps = PLACED_SPANS.lock().unwrap_or_else(|e| e.into_inner());
    heaps
        .iter()
        .filter_map(std::sync::Weak::upgrade)
        .filter_map(|spans| {
            spans
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .next_start(start, end)
        })
        .min()
}

/// The lowest level that a view of the placed texture at `start` shows,
/// over the slots shaders pick by index that hold one now. The title
/// views a texture from the first level it holds. `None`: no such slot.
pub(crate) fn first_viewed(start: u64) -> Option<u32> {
    let heaps = PLACED_SPANS.lock().unwrap_or_else(|e| e.into_inner());
    heaps
        .iter()
        .filter_map(std::sync::Weak::upgrade)
        .filter_map(|spans| {
            spans
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .first_viewed(start)
        })
        .min()
}

/// Levels on demand (`crate::tiles`): where the placed textures start
/// whose first level with memory changed since a texture pass last took
/// them ([`HostHeap::texture_views_remade`]).
static RESHOWN: Mutex<std::collections::BTreeSet<u64>> =
    Mutex::new(std::collections::BTreeSet::new());

/// Since the start, of [`HostHeap::texture_views_remade`]: view slots made
/// again, those of them made while a submission was in flight, and passes
/// put off because one was (`VICTORY_VIEWS_REMADE_QUIET=1`).
static VIEWS_REMADE: [std::sync::atomic::AtomicU64; 3] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 3];

/// Whether view slots of textures with levels on demand are made again
/// only while no submission is in flight (`VICTORY_VIEWS_REMADE_QUIET=1`;
/// off by default: at every texture pass, as before). A texture then
/// shows its new level a pass or more later. After fork pull request
/// #1219, whose author lost the device with slots rewritten under lists
/// already sent.
fn views_remade_quiet() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_VIEWS_REMADE_QUIET").is_some_and(|v| v == "1"))
}

/// Whether a title slot shows a texture that is new to it only once the
/// submission with its upload has finished (`VICTORY_SHOW_AFTER`; on,
/// `0` turns it off).
/// Shown at once, the lists already on the card read it by index while it is
/// made: all three losses whose report had the address book (n08, n11, n15)
/// were page faults in a texture made and shown in the last tenth of a
/// second. And a texture cut from a pooled block that was used before
/// shows that block's old bytes until its upload lands: in the opening
/// cutscene the car seat was a green and blue checkerboard for two seconds.
pub(crate) fn show_after() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_SHOW_AFTER").is_none_or(|v| v != "0"))
}

/// Commits a texture view slot whose host descriptor was held back by
/// [`show_after`] until the submission with its upload finished. Only writes
/// the descriptor if the heap is still live and title slot `handle` still
/// carries `expected_key`.
pub(crate) fn commit_show(heap: usize, index: u32, handle: u64, expected_key: u64) {
    let heaps = super::SHADER_HEAPS
        .read()
        .unwrap_or_else(|e| e.into_inner());
    if heaps.iter().any(|&(_, _, h)| h == heap)
        // SAFETY: registered heaps are live (removed when dropped, which takes `.write()`).
        && let Some(Some(host)) = unsafe { &*(heap as *const super::Heap) }.host.get()
    {
        // SAFETY: the handle is a descriptor slot inside the live title heap.
        let [tag, i, generation] = unsafe { (handle as *const [u32; 3]).read_unaligned() };
        let key = u64::from(generation) << 32 | u64::from(i);
        if tag == VIEW_TAG && key == expected_key {
            let _ = host.mirror(index, handle, true);
        }
    }
}

/// Re-queues a deferred texture view slot when a command list is reset
/// without being submitted, so its upload is not permanently orphaned.
pub(crate) fn requeue_show(heap: usize, index: u32) {
    let heaps = super::SHADER_HEAPS
        .read()
        .unwrap_or_else(|e| e.into_inner());
    if heaps.iter().any(|&(_, _, h)| h == heap)
        // SAFETY: registered heaps are live (removed when dropped, which takes `.write()`).
        && let Some(Some(host)) = unsafe { &*(heap as *const super::Heap) }.host.get()
    {
        let mut textures = host.textures.lock().unwrap_or_else(|e| e.into_inner());
        textures.changed(index, 0, 0);
        host.note_slot(index, SLOT_IN_TEXTURES, true);
    }
}

/// The first level with memory of the placed texture at `start` changed:
/// its slots are made again at the next texture pass.
pub(crate) fn levels_changed(start: u64) {
    RESHOWN
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(start);
}

/// Levels on demand (`crate::tiles`): where the placed textures start
/// whose last upload left a held level without tiles (the card had no
/// room, or no heap could be had), and when (`crate::host::moment`). A
/// texture pass makes their slots due again once the card has room
/// ([`HostHeap::texture_slots_waiting`]).
static WAITING: Mutex<std::collections::BTreeMap<u64, u64>> =
    Mutex::new(std::collections::BTreeMap::new());

/// The last upload of the placed texture at `start` left a held level
/// without tiles. A texture picked by index is uploaded once and then
/// only when its memory is found changed, and its staging buffer goes
/// with the upload: without this the level stayed empty (the texture
/// blurred, or black if no level had tiles) for as long as the title left
/// its memory alone, where a texture refused for want of room is tried
/// again.
pub(crate) fn levels_waiting(start: u64) {
    WAITING
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(start, crate::host::moment());
}

/// Title memory by the placed texture views that describe it: per span of
/// memory (its start, its end) the resource description of the texture
/// there and the title slots holding a view of it. A span is the memory of
/// the mips a view shows ([`crate::resource::placed_bytes_from`]). Spans of
/// two textures never overlap: a console title need not clear a descriptor
/// when it streams a texture out, so a view written over memory that older
/// views describe otherwise (another start, or another length and another
/// description: another texture) is all that says the older ones are stale.
/// Views of one span with different descriptions (the formats of one
/// family, say) are one texture's, and so are views of one description at
/// one start that show more and fewer of its mips.
/// No host heap is needed, so it is tested without one.
#[derive(Default)]
pub(crate) struct Spans {
    // (Neptune) By start and end, with the description; upstream's table is by start, with a
    // rule for packed neighbours. Its questions for levels on demand (`first_viewed`,
    // `next_start`, `slots_at`, `slots_holding`) are answered from this one.
    spans: std::collections::BTreeMap<(u64, u64), ([u32; 14], std::collections::BTreeSet<u32>)>,
    by_slot: std::collections::HashMap<u32, (u64, u64)>,
    /// The level each slot's view starts at ([`Spans::shows`]).
    first: std::collections::HashMap<u32, u32>,
    /// The bytes a placed texture takes, by resource description and first
    /// mip: worked out once for each (a title writes the views of a few
    /// thousand descriptions, thousands of times a frame).
    sizes: std::collections::HashMap<([u32; 14], u32), Option<u64>>,
}

/// The first bytes of a texture's memory hold its smallest levels: a texture that starts
/// inside them is another texture at that place, one that starts past them a packed
/// neighbour.
const HEAD: u64 = 4096;

impl Spans {
    /// The bytes of title memory the mips from `first` on of a placed
    /// texture with the resource description `desc` take.
    fn bytes(&mut self, desc: &[u32; 14], first: u32) -> Option<u64> {
        *self
            .sizes
            .entry((*desc, first))
            .or_insert_with(|| crate::resource::placed_bytes_from(desc, first))
    }

    /// The spans that overlap `[start, end)`.
    fn overlapping(&self, start: u64, end: u64) -> Vec<(u64, u64)> {
        // The spans of one start are one texture's, and no other texture's
        // reaches over it: only those of the start before may reach here.
        let before = self
            .spans
            .range(..(start, 0))
            .next_back()
            .map(|(&(at, _), _)| at);
        let before = before
            .into_iter()
            .flat_map(|at| self.spans.range((at, 0)..=(at, u64::MAX)));
        before
            .map(|(&span, _)| span)
            .filter(|span| span.1 > start)
            .chain(
                self.spans
                    .range((start, 0)..(end, 0))
                    .map(|(&span, _)| span),
            )
            .collect()
    }

    /// The spans that say otherwise than that a texture with the resource
    /// description `desc` lies in `[start, end)`.
    fn against(&self, start: u64, end: u64, desc: &[u32; 14]) -> Vec<(u64, u64)> {
        let mut spans = self.overlapping(start, end);
        spans.retain(|span| {
            let same = span.0 == start
                && (span.1 == end || self.spans.get(span).is_some_and(|s| s.0 == *desc));
            !same
        });
        spans
    }

    /// Slot `index` holds a view of a texture with the resource description
    /// `desc` in `[start, end)`. `newer`: the title wrote it now, so the
    /// slots whose views describe that memory otherwise are stale: they are
    /// returned, and forgotten. Otherwise (a draw made the slot from what
    /// it held) a view that overlaps other spans is left out.
    fn note(
        &mut self,
        index: u32,
        start: u64,
        end: u64,
        desc: &[u32; 14],
        newer: bool,
    ) -> Vec<u32> {
        if self.by_slot.get(&index) == Some(&(start, end)) {
            return Vec::new();
        }
        self.forget(index);
        if end <= start {
            return Vec::new();
        }
        let mut stale = Vec::new();
        for span in self.against(start, end, desc) {
            if !newer {
                return stale;
            }
            if let Some((_, slots)) = self.spans.remove(&span) {
                for slot in slots {
                    self.by_slot.remove(&slot);
                    self.first.remove(&slot);
                    stale.push(slot);
                }
            }
        }
        self.spans
            .entry((start, end))
            .or_insert_with(|| (*desc, Default::default()))
            .1
            .insert(index);
        self.by_slot.insert(index, (start, end));
        stale
    }

    /// The view in slot `index` starts at level `first` of its texture.
    fn shows(&mut self, index: u32, first: u32) {
        self.first.insert(index, first);
    }

    /// The lowest level a view of the texture at `start` starts at.
    fn first_viewed(&self, start: u64) -> Option<u32> {
        // (Neptune) The spans here are by start and end: the views of one texture that
        // show more and fewer of its levels are spans of one start.
        self.spans
            .range((start, 0)..=(start, u64::MAX))
            .flat_map(|(_, span)| span.1.iter())
            .filter_map(|slot| self.first.get(slot))
            .min()
            .copied()
    }

    /// The first span that starts inside `[start, end)` past the first
    /// [`HEAD`] bytes of it.
    fn next_start(&self, start: u64, end: u64) -> Option<u64> {
        self.spans
            .range((start.saturating_add(HEAD), 0)..(end, 0))
            .next()
            .map(|(&(at, _), _)| at)
    }

    /// The slots that hold a view of the texture at `start`.
    fn slots_at(&self, start: u64) -> Vec<u32> {
        self.spans
            .range((start, 0)..=(start, u64::MAX))
            .flat_map(|(_, span)| span.1.iter().copied())
            .collect()
    }

    /// Slot `index` holds no placed texture view any more.
    fn forget(&mut self, index: u32) {
        self.first.remove(&index);
        let Some(at) = self.by_slot.remove(&index) else {
            return;
        };
        if let Some(span) = self.spans.get_mut(&at) {
            span.1.remove(&index);
            if span.1.is_empty() {
                self.spans.remove(&at);
            }
        }
    }

    /// What the spans say of a view of a texture with the resource
    /// description `desc` in `[start, end)` in slot `index`, and the first
    /// span that speaks against it.
    fn claim(
        &self,
        index: u32,
        start: u64,
        end: u64,
        desc: &[u32; 14],
    ) -> (Claim, Option<(u64, u64)>) {
        if self.by_slot.get(&index) == Some(&(start, end)) {
            return (Claim::Own, None);
        }
        let spans = self.against(start, end, desc);
        match spans.first() {
            None => (Claim::Free, None),
            Some(&span) if spans.iter().all(|s| s.0 == start) => (Claim::SameStart, Some(span)),
            Some(&span) => (Claim::Other, Some(span)),
        }
    }

    /// The slots whose texture's memory overlaps `[start, end)`.
    fn slots_in(&self, start: u64, end: u64) -> Vec<u32> {
        self.overlapping(start, end)
            .iter()
            .filter_map(|at| self.spans.get(at))
            .flat_map(|span| span.1.iter().copied())
            .collect()
    }

    /// The slots of the textures that hold bytes of `[start, end)`, for a
    /// write there: as [`Spans::slots_in`], but when a texture starts at
    /// `start` itself the one before it is left out (it ends where that
    /// one starts, and a request that fills a texture begins at its first
    /// byte: `slots_in` would name its neighbour with every such write).
    fn slots_holding(&self, start: u64, end: u64) -> Vec<u32> {
        if end <= start {
            return Vec::new();
        }
        let here = self
            .spans
            .range((start, 0)..=(start, u64::MAX))
            .next()
            .is_some();
        // The spans of the start before that reach past `start` (one texture's).
        let before = if here {
            None
        } else {
            self.spans
                .range(..(start, 0))
                .next_back()
                .map(|(&(at, _), _)| at)
        };
        before
            .into_iter()
            .flat_map(|at| self.spans.range((at, 0)..=(at, u64::MAX)))
            .filter(|(span, _)| span.1 > start)
            .chain(self.spans.range((start, 0)..(end, 0)))
            .flat_map(|(_, span)| span.1.iter().copied())
            .collect()
    }
}

/// Title memory written behind the guards' back whose bytes are there
/// (DirectStorage's reads into physical pages, at the addresses the title
/// has them mapped; `crate::watch::title_memory_written`), since a texture
/// pass last took the ranges ([`page_written`]). Kept apart from
/// [`REMAPPED`] so that what such writes do is counted by itself.
static PAGE_WRITTEN: Mutex<Vec<(u64, u64)>> = Mutex::new(Vec::new());

/// Since the start: such ranges kept for a pass; ranges dropped because no
/// pass took them; slots of placed textures that held bytes of them when a
/// pass took them; of those, the slots whose texture had been uploaded
/// before the write (out of the queue, or in it after an upload): they are
/// the ones that kept the bytes from before, and the ones made due when
/// `VICTORY_PAGE_WRITES` asks for it.
pub(crate) static PAGE_WRITES: [std::sync::atomic::AtomicU64; 4] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 4];

/// Frames from the pass that takes a written range to the look at its
/// slots: the levels of one texture arrive in a few requests, and a look
/// two frames on takes those of the next frame with it.
const PAGE_WRITE_AFTER: u64 = 2;

/// The bytes of a write the guards do not see are in title memory `[at,
/// at + len)`: the placed textures there changed with no descriptor
/// written and nothing mapped. Kept for the next texture pass
/// ([`HostHeap::texture_slots_page_written`]), as [`remapped`] keeps its
/// ranges; locks one mutex and calls nothing.
pub(crate) fn page_written(at: usize, len: usize) {
    use std::sync::atomic::Ordering::Relaxed;
    /// Ranges kept until a pass takes them (as many as [`remapped`] keeps).
    const MOST: usize = 1 << 18;
    let (start, end) = (at as u64, at.saturating_add(len) as u64);
    if end <= SPANNED[0].load(Relaxed) || start >= SPANNED[1].load(Relaxed) {
        return;
    }
    let mut ranges = PAGE_WRITTEN.lock().unwrap_or_else(|e| e.into_inner());
    PAGE_WRITES[0].fetch_add(1, Relaxed);
    if let Some(last) = ranges.last_mut()
        && last.1 == start
    {
        last.1 = end;
        return;
    }
    if ranges.len() >= MOST {
        PAGE_WRITES[1].fetch_add(ranges.len() as u64, Relaxed);
        compact(&mut ranges, MOST);
    }
    ranges.push((start, end));
}

/// Whose the memory of a texture view is, by [`Spans`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Claim {
    /// The slot's own.
    Own,
    /// No other texture's.
    Free,
    /// Another texture's that starts at the same address.
    SameStart,
    /// Another texture's.
    Other,
}

/// What the last whole look at the heap's placed texture views found
/// ([`HostHeap::texture_sweep`]): views; of them shown by their host slot;
/// in the queue; read through tables lately; without memory; then the ones
/// with memory that nothing brings up to date, by [`Claim`] (own, free,
/// same start, other), and of all those the ones whose host slot shows
/// another view. And since the start: the ones put back into the queue.
pub(crate) static SWEPT: [std::sync::atomic::AtomicU64; 11] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 11];

/// A look at the heap under way: the next slot, what it counted, and when
/// (milliseconds) it went on last.
static SWEEP: Mutex<(u32, [u64; 10], u64)> = Mutex::new((0, [0; 10], 0));

/// Title memory the runtime mapped, unmapped or is about to write since a
/// texture pass last took the ranges ([`remapped`]).
static REMAPPED: Mutex<Vec<(u64, u64)>> = Mutex::new(Vec::new());

/// The lowest start and the highest end of any span of [`Spans`] so far
/// (the start is `u64::MAX` until there is one): what the runtime does to
/// memory outside is none of the texture pass's business.
static SPANNED: [std::sync::atomic::AtomicU64; 2] = [
    std::sync::atomic::AtomicU64::new(u64::MAX),
    std::sync::atomic::AtomicU64::new(0),
];

/// Since the start: slots whose view a newer view of other memory there
/// made stale; ranges of memory noted as mapped, unmapped or written;
/// ranges dropped because no pass took them; slots made due by such
/// ranges; and made slots whose memory was found unmapped (made null).
pub(crate) static STALE: [std::sync::atomic::AtomicU64; 5] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 5];

/// Whether a view written over memory that older views describe otherwise
/// makes those stale; `VICTORY_STALE_TEXTURES=0` leaves them as they are.
fn stale_out() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_STALE_TEXTURES").is_none_or(|v| v != "0"))
}

/// The runtime mapped or unmapped title memory `[at, at + len)`, or is
/// about to write it through the kernel (`crate::watch`): the placed
/// textures there may have arrived, gone, or changed with no descriptor
/// written. The range is kept for the next texture pass
/// ([`HostHeap::texture_slots_remapped`]). Called with the runtime's own
/// locks held: this locks one mutex of its own and calls nothing.
pub(crate) fn remapped(at: usize, len: usize) {
    use std::sync::atomic::Ordering::Relaxed;
    /// Ranges kept until a pass takes them.
    /// (A world's first minutes note tens of thousands between two passes:
    /// 131,072 were dropped in one run at the 32,768 there were.)
    const MOST: usize = 1 << 18;
    let (start, end) = (at as u64, at.saturating_add(len) as u64);
    if end <= SPANNED[0].load(Relaxed) || start >= SPANNED[1].load(Relaxed) {
        return;
    }
    let mut ranges = REMAPPED.lock().unwrap_or_else(|e| e.into_inner());
    STALE[1].fetch_add(1, Relaxed);
    // Pages are mapped one after the other.
    if let Some(last) = ranges.last_mut()
        && last.1 == start
    {
        last.1 = end;
        return;
    }
    if ranges.len() >= MOST {
        // Compact instead of clearing so streaming bursts never drop texture invalidations.
        STALE[2].fetch_add(ranges.len() as u64, Relaxed);
        compact(&mut ranges, MOST);
    }
    ranges.push((start, end));
}

/// Merges overlapping or touching (within `gap` bytes) ranges in place.
fn join_ranges(ranges: &mut Vec<(u64, u64)>, gap: u64) {
    ranges.retain(|&(start, end)| start < end);
    if ranges.len() < 2 {
        return;
    }
    ranges.sort_unstable();
    let mut write = 0usize;
    for read in 1..ranges.len() {
        let (start, end) = ranges[read];
        if start <= ranges[write].1.saturating_add(gap) {
            ranges[write].1 = ranges[write].1.max(end);
        } else {
            write += 1;
            ranges[write] = (start, end);
        }
    }
    ranges.truncate(write + 1);
}

/// Compacts a full list of changed ranges in place without dropping any
/// invalidation: first merges touching/overlapping ranges, then merges ranges
/// within 64 KiB, and while more than half of `most` remain, merges adjacent
/// pairs until at most `most / 2` ranges are left.
fn compact(ranges: &mut Vec<(u64, u64)>, most: usize) {
    let target = (most / 2).max(1);
    join_ranges(ranges, 0);
    if ranges.len() > target {
        join_ranges(ranges, 64 << 10);
    }
    while ranges.len() > target {
        let mut write = 0usize;
        let mut read = 0usize;
        while read < ranges.len() {
            let start = ranges[read].0;
            let end = ranges[(read + 1).min(ranges.len() - 1)].1;
            ranges[write] = (start, end);
            write += 1;
            read += 2;
        }
        ranges.truncate(write);
    }
}

/// Frames for which a slot a draw read through a table counts as a table's
/// slot ([`HostHeap::tabled`]).
const TABLED_FRAMES: u64 = 240;

/// Frames a write of a table's slot waits in the queue for the draw that
/// reads it, before the slot is taken for one shaders pick by index.
/// Frames as `crate::frame::current` counts them: in its opening cutscene
/// The title begins two or three of them per frame it presents
/// (435 begun in the 353 s of run 52's, for some 140 presented).
const TABLED_GRACE: u64 = 8;

/// In [`HostHeap::tabled`]: a draw read the slot in that frame.
const TABLED_READ: u32 = 1 << 31;
/// (Neptune) The slots of a block of [`HostHeap::tabled_blocks`].
const TABLED_BLOCK: u32 = 64;

/// Since the start: writes of texture views into slots that count as a
/// table's (due later), and slots that left the queue because a draw read
/// them through a table.
pub(crate) static TABLED_WRITES: [std::sync::atomic::AtomicU64; 2] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 2];

/// Whether slots draws read through tables leave the queue of textures
/// picked by index; `VICTORY_TABLED_TEXTURES=1` keeps them in it as
/// before.
fn tabled_out() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_TABLED_TEXTURES").is_none_or(|v| v != "1"))
}

/// What the queue of textures picked by index knows of a title slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Waiting {
    /// The frame from which the slot is due.
    due: u64,
    /// The frame it was written in.
    written: u64,
    /// Which write of the queue put it here: the pass hands it back with
    /// what it did, so a slot written again meanwhile stays as written.
    write: u64,
    /// Looks in a row that found nothing to upload (its memory not mapped,
    /// or no more of it than at the last upload).
    looks: u8,
    /// Bytes of its memory that were mapped when it was last uploaded
    /// without all of it there; 0: not uploaded yet.
    mapped: u64,
}

/// A title slot whose texture was uploaded whole, and is looked at again:
/// the title may fill the texture's memory only after it wrote the view.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Recheck {
    /// The frame from which the next look is due.
    due: u64,
    /// Its memory: address and bytes.
    pub(crate) at: (u64, u64),
    /// The fingerprint of it at the upload (`crate::texture::fingerprint_for`).
    pub(crate) print: u64,
    /// Looks that found it unchanged.
    pub(crate) looks: u8,
    /// The frame of the upload.
    pub(crate) uploaded: u64,
    /// Whether this slot is kept live (rewritten in place by the title,
    /// e.g. a TV or phone video plane): looked at every frame and
    /// fingerprinted whole up to 4 MiB (`VICTORY_LIVE_TEXTURES`).
    pub(crate) live: bool,
}

/// Whether a texture found changed after its upload with nothing to tell
/// of it is kept live: looked at every frame, fingerprinted whole
/// (`VICTORY_LIVE_TEXTURES=0`: backs off along [`RECHECK_FRAMES`] as
/// before). A TV's news video showed a colour ghost of an earlier frame:
/// its luma and chroma planes each waited 12-44 frames on their own count.
fn live_textures() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_LIVE_TEXTURES").is_none_or(|v| v != "0"))
}

/// Unchanged looks in a row after which a live slot goes back to the plain
/// looks: some seconds at the world's frame rate.
const LIVE_LOOKS: u8 = 120;

/// Frames after a whole upload of a texture picked by index at which its
/// memory is looked at again, each counted from the look before.
const RECHECK_FRAMES: [u64; 5] = [4, 12, 40, 120, 400];

/// The queue of title slots whose texture view waits to be brought up to
/// date for shaders that pick textures by index: which are due, and in
/// which order (no host heap needed, so it is tested without one).
#[derive(Default)]
pub(crate) struct TextureQueue {
    slots: std::collections::BTreeMap<u32, Waiting>,
    /// Slots out of the queue (uploaded whole) that are looked at again.
    /// Kept apart: they wait for nothing, and a slot in here is not one
    /// that will be brought up to date anyway.
    rechecks: std::collections::BTreeMap<u32, Recheck>,
    /// Slots found changed after their upload with nothing to tell of it
    /// and currently waiting in `slots` for re-upload: kept live across the
    /// upload (`VICTORY_LIVE_TEXTURES`).
    live: std::collections::HashSet<u32>,
    writes: u64,
    /// Slots held back because the card had no room for the host copy
    /// they need ([`TextureQueue::hold`]), each with the write it was
    /// held as.
    held: std::collections::BTreeMap<u32, u64>,
}

impl TextureQueue {
    /// A texture view was written into slot `index` in `frame`, due from
    /// frame `due`. `seen`: it was not written, a draw or the pass made
    /// the host slot from a view the queue may hold already, which then
    /// stays as it is.
    fn written(&mut self, index: u32, frame: u64, due: u64, seen: bool) {
        if seen && self.slots.contains_key(&index) {
            return;
        }
        self.writes += 1;
        let write = self.writes;
        self.rechecks.remove(&index);
        self.live.remove(&index);
        self.slots.insert(
            index,
            Waiting {
                due,
                written: frame,
                write,
                looks: 0,
                mapped: 0,
            },
        );
    }

    fn remove(&mut self, index: u32) {
        self.slots.remove(&index);
        self.rechecks.remove(&index);
        self.live.remove(&index);
    }

    /// Whether at least one queued slot is due at `frame`.
    fn has_due(&self, frame: u64) -> bool {
        self.slots.values().any(|w| w.due <= frame)
    }

    /// Slot `index` was uploaded whole in `frame` from memory `at`, whose
    /// fingerprint is `print`: looked at again (next frame if `live`, else
    /// along [`RECHECK_FRAMES`]), unless it was written again meanwhile and
    /// waits in the queue.
    fn recheck(&mut self, index: u32, frame: u64, at: (u64, u64), print: u64, live: bool) {
        if !self.slots.contains_key(&index) {
            let due = if live {
                frame + 1
            } else {
                frame + RECHECK_FRAMES[0]
            };
            self.rechecks.insert(
                index,
                Recheck {
                    due,
                    at,
                    print,
                    looks: 0,
                    uploaded: frame,
                    live,
                },
            );
        }
    }

    /// Up to `max` of the slots whose next look is due at `frame`, live
    /// slots first so video planes stay in step, then the ones due longest.
    fn rechecks_due(&self, frame: u64, max: usize) -> Vec<(u32, Recheck)> {
        let mut due: Vec<(u32, Recheck)> = self
            .rechecks
            .iter()
            .filter(|(_, r)| r.due <= frame)
            .map(|(&index, &r)| (index, r))
            .collect();
        due.sort_unstable_by_key(|&(index, r)| (!r.live, r.due, index));
        due.truncate(max);
        due
    }

    /// What a look at slot `index` (as [`TextureQueue::rechecks_due`] gave
    /// it) found: unchanged, and the next look is some frames on (every
    /// frame for a live slot up to [`LIVE_LOOKS`], then back to plain
    /// looks), or none; changed, and the slot is due now (marked live when
    /// `live_enabled`). A slot written or uploaded again since is left alone.
    fn rechecked(
        &mut self,
        index: u32,
        seen: &Recheck,
        changed: bool,
        frame: u64,
        live_enabled: bool,
        plain_print: Option<u64>,
    ) {
        if self.rechecks.get(&index) != Some(seen) {
            return;
        }
        if changed {
            self.changed(index, frame, frame);
            if live_enabled {
                self.live.insert(index);
            }
            return;
        }
        if seen.live {
            if seen.looks < LIVE_LOOKS {
                self.rechecks.insert(
                    index,
                    Recheck {
                        due: frame + 1,
                        looks: seen.looks + 1,
                        ..*seen
                    },
                );
            } else if let Some(print) = plain_print {
                self.rechecks.insert(
                    index,
                    Recheck {
                        due: frame + RECHECK_FRAMES[1],
                        print,
                        looks: 1,
                        live: false,
                        ..*seen
                    },
                );
            } else {
                self.rechecks.remove(&index);
            }
            return;
        }
        match RECHECK_FRAMES.get(usize::from(seen.looks) + 1) {
            Some(wait) => {
                let next = Recheck {
                    due: frame + wait,
                    looks: seen.looks + 1,
                    ..*seen
                };
                self.rechecks.insert(index, next);
            }
            None => {
                self.rechecks.remove(&index);
            }
        }
    }

    /// Up to `max` slots due at `frame`, each with its write number, in
    /// the order to serve them:
    /// - first live slots (in-place video planes found changed) so all
    ///   planes of a video frame upload together;
    /// - next the ones looked at before (their memory was not mapped, or
    ///   only part of it): the look is a query, and memory that has
    ///   arrived since is what the title streamed in last;
    /// - then the others, the newest write first and the oldest next, in
    ///   turns: slot order served the low slots of a backlog of
    ///   25,000-40,000 while the textures of the shot on screen waited
    ///   (run 52: 98% of the drawn pixels had no colour texture in the
    ///   first 3D frames).
    fn due(&self, frame: u64, max: usize) -> Vec<(u32, u64)> {
        let mut live: Vec<(u64, u32, u64)> = Vec::new();
        let mut again: Vec<(u64, u32, u64)> = Vec::new();
        let mut fresh: Vec<(u64, u32, u64)> = Vec::new();
        for (&index, w) in self.slots.iter().filter(|(_, w)| w.due <= frame) {
            let class = if self.live.contains(&index) {
                &mut live
            } else if w.looks > 0 || w.mapped > 0 {
                &mut again
            } else {
                &mut fresh
            };
            class.push((w.written, index, w.write));
        }
        // By the number of the write, counted over the run.
        let newest_first = |a: &(u64, u32, u64), b: &(u64, u32, u64)| b.2.cmp(&a.2);
        live.sort_unstable_by(newest_first);
        again.sort_unstable_by(newest_first);
        fresh.sort_unstable_by(newest_first);
        let mut order: Vec<(u32, u64)> = live
            .iter()
            .chain(again.iter())
            .map(|&(_, index, write)| (index, write))
            .collect();
        let (mut new, mut old) = (0, fresh.len());
        while new < old && order.len() < max {
            order.push((fresh[new].1, fresh[new].2));
            new += 1;
            if new < old && order.len() < max {
                old -= 1;
                order.push((fresh[old].1, fresh[old].2));
            }
        }
        order.truncate(max);
        order
    }

    /// The entry of `index` if it is still the write the pass was handed.
    fn same(&mut self, index: u32, write: u64) -> Option<&mut Waiting> {
        self.slots.get_mut(&index).filter(|w| w.write == write)
    }

    /// Replacing a retry also clears a card-space hold for the same write.
    fn reschedule(&mut self, index: u32, write: u64) -> Option<&mut Waiting> {
        if self.held.get(&index) == Some(&write) {
            self.held.remove(&index);
        }
        self.same(index, write)
    }

    /// Slot `index` is up to date: out of the queue until written again.
    /// Returns whether the slot was marked live.
    fn done(&mut self, index: u32, write: u64) -> bool {
        if self.reschedule(index, write).is_some() {
            self.slots.remove(&index);
            self.live.remove(&index)
        } else {
            false
        }
    }

    /// Nothing to upload at slot `index` now: looked at again after 1, 2,
    /// 4, ... frames, at most `most`.
    fn later(&mut self, index: u32, write: u64, frame: u64, most: u64) {
        if let Some(w) = self.reschedule(index, write) {
            w.due = frame + (1u64 << w.looks.min(16)).min(most.max(1));
            w.looks = w.looks.saturating_add(1);
        }
    }

    /// The card has no room for the host copy slot `index` needs: it is
    /// not handed out again before frame `due`, or before the card has
    /// room again ([`TextureQueue::room`]). Nothing else of it changes: it
    /// is not a slot "waiting for its memory", and when it is due it has
    /// its old place among the slots never looked at. (It stayed due, so
    /// every pass on a full card looked at the same few hundred slots
    /// again and stopped there, and the slots behind them were never
    /// reached: the ones whose host copy exists and only wants bringing up
    /// to date, and the ones whose memory went, which is what lets host
    /// copies go and makes room.)
    fn hold(&mut self, index: u32, write: u64, due: u64) {
        if let Some(w) = self.same(index, write) {
            w.due = w.due.max(due);
            self.held.insert(index, write);
        }
    }

    /// The card has room again at `frame`: the slots held back for lack of
    /// it are due now, as if they had never been held. Returns how many.
    fn room(&mut self, frame: u64) -> usize {
        let held = std::mem::take(&mut self.held);
        let mut back = 0;
        for (index, write) in held {
            if let Some(w) = self.same(index, write) {
                w.due = w.due.min(frame);
                back += 1;
            }
        }
        back
    }

    /// Slot `index` was uploaded with `mapped` bytes of its memory there,
    /// not all of it: looked at again from frame `due`.
    fn partly(&mut self, index: u32, write: u64, due: u64, mapped: u64) {
        if let Some(w) = self.reschedule(index, write) {
            (w.due, w.looks, w.mapped) = (due, 0, mapped.max(1));
        }
    }

    /// The memory slot `index`'s texture lies in was mapped, unmapped or
    /// written in `frame`: the slot is looked at from frame `due` as one
    /// written now, whatever was known of it (a pass at work on it does
    /// not take it out).
    fn changed(&mut self, index: u32, frame: u64, due: u64) {
        self.writes += 1;
        let write = self.writes;
        let due = self.slots.get(&index).map_or(due, |w| w.due.min(due));
        self.rechecks.remove(&index);
        self.slots.insert(
            index,
            Waiting {
                due,
                written: frame,
                write,
                looks: 0,
                mapped: 0,
            },
        );
    }

    /// Bytes of slot `index`'s memory that were mapped at its last upload
    /// (0: not uploaded, or all of it was).
    fn mapped(&self, index: u32) -> u64 {
        self.slots.get(&index).map_or(0, |w| w.mapped)
    }

    /// For the frame line: slots in the queue; of them due at `frame`;
    /// waiting for their memory (looked at, never uploaded); uploaded in
    /// part; and the frames the oldest due one has waited since its write.
    fn census(&self, frame: u64) -> [u64; 5] {
        let mut c = [self.slots.len() as u64, 0, 0, 0, 0];
        for w in self.slots.values() {
            if w.mapped > 0 {
                c[3] += 1;
            } else if w.looks > 0 {
                c[2] += 1;
            }
            if w.due <= frame {
                c[1] += 1;
                c[4] = c[4].max(frame.saturating_sub(w.written));
            }
        }
        c
    }
}

/// Whether `view` is of a texture the title may fill itself: a texture it
/// placed in memory, or a texture object.
pub(crate) fn is_texture_view(view: &View) -> bool {
    match view {
        View::Placed(false, resource, _) => resource[0] != 1,
        View::Shader(r, _) => crate::resource::buffer_bytes(*r as crate::object::This).is_none(),
        _ => false,
    }
}

/// Buffer views left null because the host would refuse them
/// ([`buffer_view_ok`]), since the start.
pub(crate) static REFUSED_VIEWS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Whether the host device is gone, and why (the first time it is asked
/// after that): for passes that make many views, to name the one that did
/// it.
pub(crate) fn device_removed() -> Option<String> {
    let host = host()?;
    // SAFETY: a plain query.
    let reason = unsafe { host.device.GetDeviceRemovedReason() };
    reason.err().map(|e| e.to_string())
}

/// Writes of title view-heap slots with a placed texture view and with a
/// view of a texture object, since the start (diagnostic counts).
pub(crate) static PLACED_TEXTURE_SLOTS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub(crate) static OBJECT_TEXTURE_SLOTS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Bytes per element of a buffer view, as the console's descriptor holds
/// them for indexed reads (structured: the stride; raw: 1, the index is the
/// byte; typed: the format's); `None` if `view` is not a buffer view.
fn view_stride(view: &View) -> Option<u32> {
    const BUFFER: u32 = 1;
    let (words, unordered): (&[u32; 10], bool) = match view {
        View::Shader(_, Some(d)) if d[1] == BUFFER => (d, false),
        View::Unordered(_, _, Some(d)) if d[1] == BUFFER => (d, true),
        View::Placed(unordered, resource, d) if resource[0] == BUFFER => {
            (d[..10].try_into().ok()?, *unordered)
        }
        _ => return None,
    };
    // SAFETY: buffer views' union members.
    let (stride, raw, format) = unsafe {
        if unordered {
            let d = uav_desc(words);
            let b = d.Anonymous.Buffer;
            (
                b.StructureByteStride,
                b.Flags.0 & D3D12_BUFFER_UAV_FLAG_RAW.0 != 0,
                d.Format,
            )
        } else {
            let d = srv_desc(words);
            let b = d.Anonymous.Buffer;
            (
                b.StructureByteStride,
                b.Flags.0 & D3D12_BUFFER_SRV_FLAG_RAW.0 != 0,
                d.Format,
            )
        }
    };
    Some(if stride > 0 {
        stride
    } else if raw {
        1
    } else {
        crate::resource::element(format.0 as u32).map_or(0, |(bytes, _)| bytes as u32)
    })
}

/// Host heaps' GPU ranges (start, bytes, bytes per slot, sampler), for
/// device-removed reports: a shader indexing a heap past its end faults.
static GPU_RANGES: Mutex<Vec<(u64, u64, u64, bool)>> = Mutex::new(Vec::new());

/// For device-removed reports: where host GPU address `va` lies against
/// the host descriptor heaps.
pub(crate) fn near_heaps(va: u64) -> String {
    let ranges = GPU_RANGES.lock().unwrap_or_else(|e| e.into_inner());
    let mut text = String::new();
    for &(start, bytes, step, sampler) in ranges.iter() {
        let kind = if sampler { "sampler" } else { "view" };
        let place = if va < start {
            format!("{:#x} below", start - va)
        } else if va < start + bytes {
            format!("inside, slot {}", (va - start) / step)
        } else {
            format!(
                "{:#x} past its end (slot {})",
                va - start - bytes,
                (va - start) / step
            )
        };
        text.push_str(&format!("; {kind} heap {start:#x}+{bytes:#x}: {place}"));
    }
    text
}

/// The host's limit for shader-visible view heaps on resource binding
/// tiers 1 and 2; tier 3 hardware may take more.
const PORTABLE_VIEWS: u32 = 1_000_000;

/// The size of every host view heap on a host that takes more than
/// [`PORTABLE_VIEWS`] (AMD): the title's heap (a little over 1,000,000
/// views in the retail game) plus [`DYNAMIC`] fits with room to spare, and
/// the last slot is the null slot every view index at or past it reads.
const UNCAPPED_VIEWS: u32 = 1 << 21;

/// Picks, once, the null slot at the end of every host view heap and tells
/// the translator now (`set_view_heap_limit`) instead of when the title's
/// heap is made: a shader keeps the limit it was translated with, and the
/// title makes 25 pipelines (their shaders translated at once) before it
/// makes its view heap. Called before a shader is translated and before a
/// view heap is made.
///
/// Every host clamps. A host that caps view heaps at [`PORTABLE_VIEWS`]
/// (NVIDIA) hung on a view index past the heap; one that does not (AMD
/// RDNA3, RX 7900 XT) took a page fault and removed the device on entering
/// the world: a compute program read view slot 16,777,088 (0xffff80) of a
/// heap of 1,507,328, past its end. Unclamped, that index is undefined on
/// any host.
pub(crate) fn probe_view_limit() {
    if crate::host_facts::assumed() {
        return;
    }
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let Some(host) = host() else { return };
        let fits = |count: u32| {
            // SAFETY: plain heap creation; the heap is released at once.
            let made: windows::core::Result<ID3D12DescriptorHeap> = unsafe {
                host.device
                    .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                        Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                        NumDescriptors: count,
                        Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                        NodeMask: 0,
                    })
            };
            made.is_ok()
        };
        let views = if fits(UNCAPPED_VIEWS) {
            UNCAPPED_VIEWS
        } else {
            PORTABLE_VIEWS
        };
        tracing::info!(
            views,
            "host view heaps hold {views} views; view indices are clamped to the null slot at the end from the first shader on"
        );
        victory_shader::emit::set_view_heap_limit(views - 1);
    });
}

/// Slots past the title's in a view heap, for tables a draw needs made
/// afresh (constant buffers copied per draw). A list holds the slots it
/// took until it is reset (its submissions have run) or released.
/// 2^16 ran out in the world ("no free dynamic descriptor slots", run 11;
/// upstream PR #336 takes 2^18). On a host heap capped at 1,000,000 views
/// (NVIDIA) these come out of the title's slots, whose highest seen is
/// about 720,000: 2^17 left 868,000 of them, and ran out in the opening
/// cutscene (a frame asks for up to 16,000 tables while the frame before
/// still holds its own; a draw refused its table read slots never made
/// for it, and the card stopped within frames). 2^18 leaves 737,000; a
/// table past them is made afresh in these slots like any other.
pub(crate) const DYNAMIC: u32 = 1 << 18;

/// Dynamic slots for a capped view heap: `VICTORY_DYNAMIC_SLOTS`, default
/// [`DYNAMIC`], never more than half of `null` (the heap's last slot). A
/// host without a cap keeps the default.
fn dynamic_slots_on(null: u32, asked: Option<u32>) -> u32 {
    if null == 0 {
        return DYNAMIC;
    }
    let most = (null / 2).max(DYNAMIC);
    asked.unwrap_or(DYNAMIC).clamp(DYNAMIC, most)
}

fn dynamic_slots(null: u32) -> u32 {
    static SLOTS: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *SLOTS.get_or_init(|| {
        let asked = std::env::var("VICTORY_DYNAMIC_SLOTS")
            .ok()
            .and_then(|v| v.trim().parse::<u32>().ok());
        let slots = dynamic_slots_on(null, asked);
        if slots != DYNAMIC {
            tracing::warn!(
                slots,
                title_in_place = null.saturating_sub(slots),
                "dynamic descriptor slots on a capped view heap (VICTORY_DYNAMIC_SLOTS)"
            );
        }
        slots
    })
}

/// (Neptune) Since the last line in the log: tables that waited for slots, the microseconds
/// they waited, and tables given up (their draws not drawn).
pub(crate) static DYNAMIC_WAITS: [std::sync::atomic::AtomicU64; 3] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 3];

impl HostHeap {
    pub(crate) fn new(sampler: bool, count: u32) -> Option<Self> {
        let host = host()?;
        let kind = if sampler {
            D3D12_DESCRIPTOR_HEAP_TYPE_SAMPLER
        } else {
            D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV
        };
        let make = |count: u32| -> windows::core::Result<ID3D12DescriptorHeap> {
            // SAFETY: plain heap creation.
            unsafe {
                host.device
                    .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                        Type: kind,
                        NumDescriptors: count,
                        Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                        NodeMask: 0,
                    })
            }
        };
        // A view heap ends at the null slot shaders clamp view indices to
        // (`probe_view_limit`), whatever the title's own count: the title's
        // slots, then the dynamic ones, then null up to and with that slot.
        // Sampler heaps always back the full 2,048 hardware limit and reserve
        // slot 2,047 as a permanent null point sampler so `umin(index, 2047)`
        // in `victory-shader` never reads past smaller title sampler heaps.
        const SAMPLERS: u32 = 2048;
        if !sampler {
            probe_view_limit();
        }
        let null = if sampler {
            SAMPLERS - 1
        } else {
            victory_shader::emit::view_heap_limit()
        };
        let extra = if sampler { 0 } else { dynamic_slots(null) };
        let (heap, count, size) = if sampler {
            let fitted = count.min(SAMPLERS - 1);
            match make(SAMPLERS) {
                Ok(heap) => (heap, fitted, SAMPLERS),
                Err(e) => {
                    tracing::warn!(error = %e, count, sampler, "no host descriptor heap");
                    return None;
                }
            }
        } else if null == 0 {
            match make(count + extra) {
                Ok(heap) => (heap, count, count + extra),
                Err(e) => {
                    tracing::warn!(error = %e, count, sampler, "no host descriptor heap");
                    return None;
                }
            }
        } else {
            let size = null + 1;
            let fitted = if count + extra <= null {
                count
            } else {
                // shortcut: descriptors past the host's limit are left out; a title that reaches
                // them needs remapping. They read the null slot instead.
                tracing::warn!(
                    count,
                    "host view heap capped at {size}; views past it read a null slot"
                );
                null - extra
            };
            match make(size) {
                Ok(heap) => (heap, fitted, size),
                Err(e) => {
                    tracing::warn!(error = %e, count, size, "no host view heap");
                    return None;
                }
            }
        };
        // SAFETY: a live heap.
        let (cpu, step) = unsafe {
            (
                heap.GetCPUDescriptorHandleForHeapStart().ptr,
                host.device.GetDescriptorHandleIncrementSize(kind),
            )
        };
        tracing::debug!(count, sampler, "host descriptor heap");
        // The null slot every index past the heap reads is the heap's last,
        // past the dynamic slots (`probe_view_limit`): all of them are free.
        let taken = DynamicBits::new(extra as usize);
        // SAFETY: a live shader-visible heap.
        let gpu = unsafe { heap.GetGPUDescriptorHandleForHeapStart().ptr };
        GPU_RANGES.lock().unwrap_or_else(|e| e.into_inner()).push((
            gpu,
            u64::from(size) * u64::from(step),
            u64::from(step),
            sampler,
        ));
        let strides = (!sampler)
            .then(|| {
                let bytes = u64::from(victory_shader::emit::SLOT_STRIDES_COUNT) * 4;
                let (buffer, data) = host.upload_buffer(bytes)?;
                // SAFETY: the buffer's whole mapping; a live buffer.
                let gpu = unsafe {
                    std::ptr::write_bytes(data, 0, bytes as usize);
                    buffer.GetGPUVirtualAddress()
                };
                crate::host::name(&buffer, || "element strides by view heap slot".to_owned());
                Some((buffer, gpu, data as usize))
            })
            .flatten();
        let h = HostHeap {
            id: NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            heap,
            cpu,
            step,
            count,
            dynamic: extra,
            sampler,
            made: Mutex::new(vec![0; count as usize]),
            copied: Mutex::new(Vec::new()),
            made_too: (0..count)
                .map(|_| std::sync::atomic::AtomicU64::new(0))
                .collect(),
            kept: (0..count + extra).map(|_| Mutex::new(None)).collect(),
            next: Mutex::new((0, taken)),
            released: Condvar::new(),
            timed_out_frame: std::sync::atomic::AtomicU64::new(u64::MAX),
            timed_out_min: std::sync::atomic::AtomicU32::new(u32::MAX),
            free: std::sync::atomic::AtomicU32::new(extra),
            too_long: std::sync::atomic::AtomicU32::new(u32::MAX),
            strides,
            buffers: Mutex::default(),
            changed: Mutex::default(),
            textures: Mutex::default(),
            tabled: (0..if sampler { 0 } else { count })
                .map(|_| std::sync::atomic::AtomicU32::new(0))
                .collect(),
            tabled_blocks: (0..if sampler { 0 } else { count / TABLED_BLOCK + 1 })
                .map(|_| std::sync::atomic::AtomicU32::new(0))
                .collect(),
            slot_flags: (0..if sampler { 0 } else { count })
                .map(|_| std::sync::atomic::AtomicU8::new(0))
                .collect(),
            placed: {
                let spans = std::sync::Arc::<Mutex<Spans>>::default();
                if !sampler {
                    PLACED_SPANS
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push(std::sync::Arc::downgrade(&spans));
                }
                spans
            },
        };
        // A slot never written is undefined: shaders reading one (a slot
        // the title wrote in a way we do not know, or a table entry no
        // used-resource entry names) read a wild address. Every slot starts
        // null instead.
        let started = std::time::Instant::now();
        if let Err(e) = h.fill_null(kind, size) {
            tracing::warn!(error = %e, "host descriptor heap not made null");
        }
        tracing::debug!(
            ms = started.elapsed().as_millis(),
            "host descriptor heap made null"
        );
        Some(h)
    }

    /// Makes the first `total` slots null: one null slot in a CPU-side heap,
    /// doubled by copies to a block, the block copied over the heap.
    fn fill_null(&self, kind: D3D12_DESCRIPTOR_HEAP_TYPE, total: u32) -> windows::core::Result<()> {
        const BLOCK: u32 = 1 << 16;
        let Some(host) = host() else { return Ok(()) };
        // SAFETY: plain heap creation and copies inside live heaps.
        unsafe {
            let block: ID3D12DescriptorHeap =
                host.device
                    .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                        Type: kind,
                        NumDescriptors: BLOCK,
                        ..Default::default()
                    })?;
            let start = block.GetCPUDescriptorHandleForHeapStart();
            let at = |i: u32| D3D12_CPU_DESCRIPTOR_HANDLE {
                ptr: start.ptr + (i * self.step) as usize,
            };
            if self.sampler {
                null_sampler(start);
            } else {
                null_view(start);
            }
            let mut filled = 1;
            while filled < BLOCK {
                let n = filled.min(BLOCK - filled);
                host.device
                    .CopyDescriptorsSimple(n, at(filled), start, kind);
                filled += n;
            }
            let mut done = 0;
            while done < total {
                let n = BLOCK.min(total - done);
                host.device
                    .CopyDescriptorsSimple(n, self.cpu(done), start, kind);
                done += n;
            }
        }
        Ok(())
    }

    /// A plain point sampler at slot `index`, for sampler slots without a
    /// view.
    fn null_sampler(&self, index: u32) {
        null_sampler(self.cpu(index));
    }
}

/// A plain point sampler at `at`.
fn null_sampler(at: D3D12_CPU_DESCRIPTOR_HANDLE) {
    let Some(host) = host() else { return };
    let desc = D3D12_SAMPLER_DESC {
        Filter: D3D12_FILTER_MIN_MAG_MIP_POINT,
        AddressU: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
        AddressV: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
        AddressW: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
        MaxAnisotropy: 1,
        ComparisonFunc: D3D12_COMPARISON_FUNC_NEVER,
        ..Default::default()
    };
    // SAFETY: a slot of a sampler heap of ours.
    unsafe { host.device.CreateSampler(&desc, at) };
}

impl HostHeap {
    /// `n` consecutive free dynamic slots (view heaps only), taken until
    /// [`HostHeap::release_many`]: the first's index.
    pub(crate) fn dynamic(&self, n: u32) -> Option<u32> {
        if self.sampler || n == 0 || n > self.dynamic {
            return None;
        }
        #[cfg(feature = "diagnostics")]
        let started = std::time::Instant::now();
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::DynamicAsked, 1);
        // (Neptune) A table that finds no slots is a draw that is not drawn: a piece of the
        // world missing for that frame (55 mesh draws a frame in a view of 7,800 draws, with
        // every pipeline made). The slots come back as the lists in the air land, so the
        // table waits for them, up to `VICTORY_DYNAMIC_WAIT_MS` (30; 0: refused at once, as
        // before), before it is given up.
        static WAIT: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
        let wait = *WAIT.get_or_init(|| {
            std::env::var("VICTORY_DYNAMIC_WAIT_MS")
                .ok()
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(30)
        });
        let began = std::time::Instant::now();
        let deadline = std::time::Duration::from_millis(wait);
        let mut waited = false;
        let mut guard = self.next.lock().unwrap_or_else(|e| e.into_inner());
        let (found, enough) = loop {
            let (next, taken) = &mut *guard;
            let enough = self.free.load(std::sync::atomic::Ordering::Relaxed) >= n
                && n < self.too_long.load(std::sync::atomic::Ordering::Relaxed);
            let found = enough
                .then(|| taken.find_run(*next as usize, n as usize))
                .flatten();
            // (Once a table of length `m` has waited its whole time in a frame,
            // tables of length >= `m` in the same frame give up at once until
            // `HostHeap::release_many` returns slots; smaller tables can still fit.)
            let frame = crate::frame::current();
            let elapsed = began.elapsed();
            let over = elapsed >= deadline;
            let already_timed_out = self
                .timed_out_frame
                .load(std::sync::atomic::Ordering::Relaxed)
                == frame
                && n >= self
                    .timed_out_min
                    .load(std::sync::atomic::Ordering::Relaxed);
            if found.is_none() && over && wait != 0 {
                if self
                    .timed_out_frame
                    .load(std::sync::atomic::Ordering::Relaxed)
                    != frame
                {
                    self.timed_out_frame
                        .store(frame, std::sync::atomic::Ordering::Relaxed);
                    self.timed_out_min
                        .store(n, std::sync::atomic::Ordering::Relaxed);
                } else {
                    self.timed_out_min
                        .fetch_min(n, std::sync::atomic::Ordering::Relaxed);
                }
            }
            if found.is_some() || over || already_timed_out {
                break (found, enough);
            }
            if enough {
                // Free slots, but no run of them: the same answer until some come back.
                self.too_long
                    .fetch_min(n, std::sync::atomic::Ordering::Relaxed);
            }
            waited = true;
            let remaining = deadline.saturating_sub(elapsed);
            guard = self
                .released
                .wait_timeout(guard, remaining)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        };
        if waited {
            DYNAMIC_WAITS[0].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            DYNAMIC_WAITS[1].fetch_add(
                began.elapsed().as_micros() as u64,
                std::sync::atomic::Ordering::Relaxed,
            );
        }
        let (next, taken) = &mut *guard;
        let n = n as usize;
        let Some(first) = found else {
            DYNAMIC_WAITS[2].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if enough {
                self.too_long
                    .fetch_min(n as u32, std::sync::atomic::Ordering::Relaxed);
            }
            static ONCE: std::sync::Once = std::sync::Once::new();
            ONCE.call_once(|| {
                tracing::warn!(n, "no free dynamic descriptor slots: a table is not made")
            });
            #[cfg(feature = "diagnostics")]
            {
                use crate::copystat::{C, add};
                add(C::DynamicRefused, 1);
                add(C::DynamicNs, started.elapsed().as_nanos() as u64);
            }
            return None;
        };
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(
            crate::copystat::C::DynamicNs,
            started.elapsed().as_nanos() as u64,
        );
        taken.set_taken(first, n);
        self.free
            .fetch_sub(n as u32, std::sync::atomic::Ordering::Relaxed);
        *next = ((first + n) % self.dynamic as usize) as u32;
        Some(self.count + first as u32)
    }

    /// The title slots this heap has host slots for; fewer than the title's
    /// when the host caps its size.
    pub(crate) fn count(&self) -> u32 {
        self.count
    }

    /// The GPU address of the table of element strides by title slot (view
    /// heaps).
    pub(crate) fn strides_gpu(&self) -> Option<u64> {
        self.strides.as_ref().map(|s| s.1)
    }

    /// The table's buffer, for a list to hold.
    pub(crate) fn strides_buffer(&self) -> Option<&ID3D12Resource> {
        self.strides.as_ref().map(|s| &s.0)
    }

    /// Up to `max` title slots whose texture view is due at `frame`, each
    /// with its write number, in the order to serve them
    /// ([`TextureQueue::due`]), and the queue's counts
    /// ([`TextureQueue::census`]).
    /// Whether any slot in the queue of textures picked by index is due at `frame`.
    pub(crate) fn texture_slots_has_due(&self, frame: u64) -> bool {
        self.textures
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .has_due(frame)
    }

    pub(crate) fn texture_slots_due(&self, frame: u64, max: usize) -> (Vec<(u32, u64)>, [u64; 5]) {
        let textures = self.textures.lock().unwrap_or_else(|e| e.into_inner());
        (
            if max == 0 {
                Vec::new()
            } else {
                textures.due(frame, max)
            },
            textures.census(frame),
        )
    }

    /// Every title slot of the queue that is due at `frame`, and whether
    /// it was uploaded before (in part, or whole and waiting for another
    /// look): for the card accounting line
    /// (`crate::resource::card_accounting`).
    pub(crate) fn texture_slots_all_due(&self, frame: u64) -> Vec<(u32, bool)> {
        let textures = self.textures.lock().unwrap_or_else(|e| e.into_inner());
        textures
            .slots
            .iter()
            .filter(|(_, w)| w.due <= frame)
            .map(|(&index, w)| {
                (
                    index,
                    w.mapped > 0 || textures.rechecks.contains_key(&index),
                )
            })
            .collect()
    }

    /// The texture view of title slot `index` (as handed out with `write`)
    /// was brought up to date: due again from frame `again` (uploaded with
    /// that many bytes of its memory there, not all), or not until the
    /// slot is written again. Returns whether the slot was marked live.
    pub(crate) fn texture_slot_done(
        &self,
        index: u32,
        write: u64,
        again: Option<(u64, u64)>,
    ) -> bool {
        let mut textures = self.textures.lock().unwrap_or_else(|e| e.into_inner());
        match again {
            Some((frame, mapped)) => {
                textures.partly(index, write, frame, mapped);
                false
            }
            None => textures.done(index, write),
        }
    }

    /// Title slot `index` was uploaded whole: see [`TextureQueue::recheck`].
    pub(crate) fn texture_slot_recheck(
        &self,
        index: u32,
        frame: u64,
        at: (u64, u64),
        print: u64,
        live: bool,
    ) {
        let mut textures = self.textures.lock().unwrap_or_else(|e| e.into_inner());
        textures.recheck(index, frame, at, print, live);
        self.note_slot(index, SLOT_IN_TEXTURES, true);
    }

    /// Looks again at slots uploaded whole: the title writes a texture's
    /// view, and may fill the texture's memory itself only afterwards, with
    /// no descriptor write and nothing from the runtime to tell of it. The
    /// host copy then kept what the memory held at the first look for the
    /// rest of the run. A changed one goes back into the queue (and is kept
    /// live while `VICTORY_LIVE_TEXTURES` is on). Returns how many were
    /// unchanged and changed, and how many wait for a look.
    pub(crate) fn texture_slots_rechecked(&self, frame: u64) -> [u64; 3] {
        /// Looks per pass (each reads 48 KiB, or up to 4 MiB for live slots),
        /// and the time they may take.
        const MOST: usize = 256;
        static SPAN: std::sync::OnceLock<std::time::Duration> = std::sync::OnceLock::new();
        let span = *SPAN.get_or_init(|| {
            let ms = std::env::var("VICTORY_TEXTURE_RECHECK_MS")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .map(|ms| ms.max(1))
                .unwrap_or(4);
            std::time::Duration::from_millis(ms)
        });
        let (due, waiting) = {
            let textures = self.textures.lock().unwrap_or_else(|e| e.into_inner());
            (
                textures.rechecks_due(frame, MOST),
                textures.rechecks.len() as u64,
            )
        };
        let live_on = live_textures();
        let began = std::time::Instant::now();
        let mut found = [0, 0, waiting];
        let mut results: Vec<(u32, Recheck, bool, Option<u64>)> = Vec::with_capacity(due.len());
        for (index, seen) in due {
            if began.elapsed() > span {
                break;
            }
            let changed =
                crate::texture::fingerprint_for(seen.at.0, seen.at.1, seen.live) != seen.print;
            let plain_print = if !changed && seen.live && seen.looks >= LIVE_LOOKS {
                Some(crate::texture::fingerprint(seen.at.0, seen.at.1))
            } else {
                None
            };
            found[usize::from(changed)] += 1;
            if changed {
                static SAID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                let n = SAID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if n < 40 || n.is_multiple_of(1000) {
                    tracing::info!(
                        slot = index,
                        address = format_args!("{:#x}", seen.at.0),
                        bytes = seen.at.1,
                        frames_after_upload = frame.saturating_sub(seen.uploaded),
                        looks = seen.looks,
                        live = seen.live,
                        changed = n + 1,
                        "a texture picked by index changed after its upload with nothing to tell of it: uploaded again"
                    );
                }
            }
            results.push((index, seen, changed, plain_print));
        }
        if !results.is_empty() {
            let mut textures = self.textures.lock().unwrap_or_else(|e| e.into_inner());
            for (index, seen, changed, plain_print) in results {
                textures.rechecked(index, &seen, changed, frame, live_on, plain_print);
                if changed {
                    self.note_slot(index, SLOT_IN_TEXTURES, true);
                }
            }
        }
        found
    }

    /// Nothing to upload at title slot `index` now (its memory is not
    /// mapped, or no more of it than before): looked at again after 1, 2,
    /// 4, ... frames, at most `most`.
    pub(crate) fn texture_slot_later(&self, index: u32, write: u64, frame: u64, most: u64) {
        let mut textures = self.textures.lock().unwrap_or_else(|e| e.into_inner());
        textures.later(index, write, frame, most);
    }

    /// See [`TextureQueue::hold`].
    pub(crate) fn texture_slot_held(&self, index: u32, write: u64, due: u64) {
        self.textures
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .hold(index, write, due);
    }

    /// See [`TextureQueue::room`].
    pub(crate) fn texture_slots_room(&self, frame: u64) -> usize {
        self.textures
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .room(frame)
    }

    /// Bytes of title slot `index`'s memory that were mapped when it was
    /// last uploaded in part (0: not uploaded, or whole).
    pub(crate) fn texture_slot_mapped(&self, index: u32) -> u64 {
        self.textures
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .mapped(index)
    }

    /// Takes the ranges of title memory the runtime mapped, unmapped or
    /// wrote since the last call ([`remapped`]) and makes the slots whose
    /// placed texture lies there due from the next frame (by when a write
    /// under way has landed): memory that arrived is uploaded, memory that
    /// went makes the slot null ([`HostHeap::texture_slot_gone`]), and a
    /// texture streamed into the place of another of the same description
    /// is uploaded although no descriptor says so. Slots tables read are
    /// left to their draws. Returns how many slots it made due.
    /// shortcut: the ranges are kept in one place for all heaps; the heap
    /// whose pass runs first takes them (a title has one view heap).
    pub(crate) fn texture_slots_remapped(&self, frame: u64) -> usize {
        use std::sync::atomic::Ordering::Relaxed;
        self.texture_slots_page_written(frame);
        let mut ranges = std::mem::take(&mut *REMAPPED.lock().unwrap_or_else(|e| e.into_inner()));
        join_ranges(&mut ranges, 0);
        if ranges.is_empty() {
            return 0;
        }
        let mut slots: Vec<u32> = {
            let placed = self.placed.lock().unwrap_or_else(|e| e.into_inner());
            ranges
                .iter()
                .flat_map(|&(start, end)| placed.slots_in(start, end))
                .collect()
        };
        slots.sort_unstable();
        slots.dedup();
        // A table's slot waits for its draw, as a write of it does: the
        // draw takes it out again. Left out altogether, a slot that shaders
        // also pick by index kept what it showed before.
        let lately: Vec<bool> = slots
            .iter()
            .map(|&slot| self.tabled_lately(slot, frame))
            .collect();
        let mut textures = self.textures.lock().unwrap_or_else(|e| e.into_inner());
        for (&slot, lately) in slots.iter().zip(lately) {
            textures.changed(slot, frame, frame + if lately { TABLED_GRACE } else { 1 });
            self.note_slot(slot, SLOT_IN_TEXTURES, true);
        }
        STALE[3].fetch_add(slots.len() as u64, Relaxed);
        slots.len()
    }

    /// Takes the ranges of title memory written behind the guards' back
    /// since the last call ([`page_written`]) and finds the slots whose
    /// placed texture holds bytes of them ([`Spans::slots_holding`]). A
    /// slot whose texture was uploaded before the write shows the bytes
    /// from before: with `VICTORY_PAGE_WRITES` (`crate::watch::page_writes`,
    /// bit 2) it is due again [`PAGE_WRITE_AFTER`] frames on, as memory
    /// mapped under it would make it ([`HostHeap::texture_slots_remapped`]);
    /// its host copy exists, so the upload needs no room on the card. A
    /// slot still waiting for its first upload is left as it is: that
    /// upload takes the bytes that are there by then. Slots tables read
    /// are left to their draws. Counted either way; returns how many slots
    /// it made due.
    fn texture_slots_page_written(&self, frame: u64) -> usize {
        let mut ranges =
            std::mem::take(&mut *PAGE_WRITTEN.lock().unwrap_or_else(|e| e.into_inner()));
        join_ranges(&mut ranges, 0);
        if ranges.is_empty() {
            return 0;
        }
        self.slots_written_under(&ranges, frame, crate::watch::page_writes() & 2 != 0)[2]
    }

    /// [`HostHeap::texture_slots_page_written`] for `ranges` (start, end):
    /// the slots that held bytes of them, those of them uploaded before,
    /// and those made due (the latter, if `due`).
    fn slots_written_under(&self, ranges: &[(u64, u64)], frame: u64, due: bool) -> [usize; 3] {
        use std::sync::atomic::Ordering::Relaxed;
        let mut slots: Vec<u32> = {
            let placed = self.placed.lock().unwrap_or_else(|e| e.into_inner());
            ranges
                .iter()
                .flat_map(|&(start, end)| placed.slots_holding(start, end))
                .collect()
        };
        slots.sort_unstable();
        slots.dedup();
        slots.retain(|&slot| !self.tabled_lately(slot, frame));
        let held = slots.len();
        let mut textures = self.textures.lock().unwrap_or_else(|e| e.into_inner());
        // Uploaded before: out of the queue, or in it after an upload (in
        // part, or whole and being looked at again).
        // (Neptune: the slots uploaded whole and looked at again are kept apart from the
        // queue here, `rechecks`, so they count as out of it.)
        slots.retain(|slot| textures.slots.get(slot).is_none_or(|w| w.mapped > 0));
        PAGE_WRITES[2].fetch_add(held as u64, Relaxed);
        PAGE_WRITES[3].fetch_add(slots.len() as u64, Relaxed);
        if !due {
            return [held, slots.len(), 0];
        }
        for &slot in &slots {
            textures.changed(slot, frame, frame + PAGE_WRITE_AFTER);
            self.note_slot(slot, SLOT_IN_TEXTURES, true);
        }
        [held, slots.len(), slots.len()]
    }

    /// Whether host slot `index` is made from the very record the title
    /// has at `handle` now. Its host texture then exists (a slot keeps what
    /// it views), so bringing that texture up to date takes no memory of
    /// the card.
    pub(crate) fn texture_slot_made_from(&self, index: u32, handle: u64) -> bool {
        if index >= self.count {
            return false;
        }
        // SAFETY: the handle is a descriptor slot in title memory.
        let [tag, i, generation] = unsafe { (handle as *const [u32; 3]).read_unaligned() };
        let key = u64::from(generation) << 32 | u64::from(i);
        if tag != VIEW_TAG {
            return false;
        }
        // And the slot holds a host resource: a slot is also noted as made
        // from a view the host could not make (`make` gave nothing, the
        // slot is null and keeps nothing). The host copy of such a
        // texture is held by no slot, may have been let go, and would be
        // made anew here, with the card full. `made` first, then `kept`,
        // as `mirror` takes them.
        let made = self.made.lock().unwrap_or_else(|e| e.into_inner());
        made.get(index as usize)
            .is_some_and(|&made| made != 0 && made == key)
            && self
                .kept
                .get(index as usize)
                .is_some_and(|slot| slot.lock().unwrap_or_else(|e| e.into_inner()).is_some())
    }

    /// Goes on with a look at all of the heap's placed texture views, a
    /// part at a time: a view whose memory is there, whose host slot shows
    /// nothing (or another view) and which is not in the queue will stay
    /// that way until the title writes it again, and a shader that picks it
    /// reads black. One whose memory no newer view describes otherwise goes
    /// back into the queue; the others are counted ([`SWEPT`]).
    /// `VICTORY_TEXTURE_SWEEP=0` turns it off, `count` only counts.
    pub(crate) fn texture_sweep(&self, heap: &super::Heap, frame: u64) {
        use std::sync::atomic::Ordering::Relaxed;
        /// Slots looked at at a time, and the least time between.
        const SLOTS: u32 = 1 << 15;
        const EVERY_MS: u64 = 100;
        static MODE: std::sync::OnceLock<u8> = std::sync::OnceLock::new();
        let mode = *MODE.get_or_init(|| match std::env::var("VICTORY_TEXTURE_SWEEP").as_deref() {
            Ok("0") => 0,
            Ok("count") => 1,
            _ => 2,
        });
        if mode == 0 || self.sampler {
            return;
        }
        let now = crate::host::now_ms();
        let Ok(mut sweep) = SWEEP.try_lock() else {
            return;
        };
        if now.saturating_sub(sweep.2) < EVERY_MS {
            return;
        }
        sweep.2 = now;
        let (from, mut counts) = (sweep.0.min(self.count), sweep.1);
        let to = from.saturating_add(SLOTS).min(self.count);
        self.sweep_part(
            from..to,
            |index| heap.handle(index),
            frame,
            mode == 2,
            &mut counts,
        );
        sweep.0 = to;
        sweep.1 = counts;
        if to >= self.count {
            for (to, counted) in SWEPT.iter().zip(counts) {
                to.store(counted, Relaxed);
            }
            (sweep.0, sweep.1) = (0, [0; 10]);
        }
    }

    /// [`HostHeap::texture_sweep`] of some slots, `handle` saying where the
    /// title's descriptors lie.
    fn sweep_part(
        &self,
        slots: std::ops::Range<u32>,
        handle: impl Fn(u32) -> Option<u64>,
        frame: u64,
        put_back: bool,
        counts: &mut [u64; 10],
    ) {
        use std::sync::atomic::Ordering::Relaxed;
        /// Put back per part, and only while the queue is shorter than
        /// this: a view the title forgot to clear may be among them, and
        /// the queue serves the newest first.
        const MOST: u32 = 128;
        const SHORT: usize = 4_000;
        let mut back = 0;
        // The slots whose host slot does not show what the title's says.
        let mut unmade = Vec::new();
        {
            let made = self.made.lock().unwrap_or_else(|e| e.into_inner());
            // The memory is the title's, and a heap may be larger than what
            // of it is there: asked once per page.
            let mut page = (u64::MAX, false);
            for index in slots {
                let Some(handle) = handle(index) else {
                    break;
                };
                if page.0 != handle >> 12 {
                    page = (
                        handle >> 12,
                        crate::texture::committed(handle as *const u8, 12),
                    );
                }
                if !page.1 {
                    continue;
                }
                // SAFETY: a descriptor slot of the title's heap, there.
                let [tag, i, generation] = unsafe { (handle as *const [u32; 3]).read_unaligned() };
                if tag != VIEW_TAG || generation >> super::KIND_SHIFT == super::CONSTANT {
                    continue;
                }
                let key = u64::from(generation) << 32 | u64::from(i);
                let shown = made.get(index as usize).copied().unwrap_or(0);
                if shown == key {
                    counts[0] += 1;
                    counts[1] += 1;
                } else {
                    unmade.push((index, i, generation, shown != 0));
                }
            }
        }
        for (index, i, generation, other) in unmade {
            let Some(View::Placed(false, resource, desc)) = view_at(i, generation) else {
                continue;
            };
            if resource[0] == 1 {
                continue;
            }
            counts[0] += 1;
            let queued = {
                let textures = self.textures.lock().unwrap_or_else(|e| e.into_inner());
                textures.slots.contains_key(&index)
            };
            if queued {
                counts[2] += 1;
                continue;
            }
            if self.tabled_lately(index, frame) {
                counts[3] += 1;
                continue;
            }
            let start = u64::from(desc[10]) | u64::from(desc[11]) << 32;
            if !crate::texture::committed(start as *const u8, 1) {
                counts[4] += 1;
                continue;
            }
            let (claim, against) = {
                let mut placed = self.placed.lock().unwrap_or_else(|e| e.into_inner());
                let Some(bytes) = placed.bytes(&resource, crate::resource::first_mip(&desc)) else {
                    continue;
                };
                placed.claim(index, start, start.saturating_add(bytes), &resource)
            };
            counts[5 + claim as usize] += 1;
            counts[9] += u64::from(other);
            static SAID: [std::sync::atomic::AtomicU32; 4] =
                [const { std::sync::atomic::AtomicU32::new(0) }; 4];
            if SAID[claim as usize].fetch_add(1, Relaxed) < 12 {
                let (width, height, format) = (resource[4], resource[6], resource[8]);
                tracing::info!(
                    "a texture view nothing brings up to date: slot {index}, {claim:?}, {width}x{height} f{format} \
                     mips {} at {start:#x}, shows {}; against {against:x?}; its view {:x?}",
                    resource[7] >> 16,
                    if other { "another view" } else { "nothing" },
                    &desc[..10]
                );
            }
            if put_back && back < MOST && matches!(claim, Claim::Own | Claim::Free) {
                let mut textures = self.textures.lock().unwrap_or_else(|e| e.into_inner());
                if textures.slots.len() < SHORT {
                    textures.written(index, frame, frame + 1, false);
                    self.note_slot(index, SLOT_IN_TEXTURES, true);
                    back += 1;
                    SWEPT[10].fetch_add(1, Relaxed);
                }
            }
        }
    }

    /// Levels on demand (`crate::tiles`): makes again the host slots of the
    /// placed textures whose first level with memory changed since the
    /// last call ([`levels_changed`]), each from the view it was made
    /// from, so that it shows the texture from that level ([`make`]). Only
    /// slots that are made; a slot draws read through a table is made by
    /// its draw. Returns how many it made.
    /// shortcut: the starts are kept in one place for all heaps; the heap
    /// whose pass runs first takes them (a title has one view heap).
    /// Levels on demand (`crate::tiles`): the card has room, so the slots
    /// of up to [`Self::WAITING_A_PASS`] placed textures whose upload left
    /// a held level without tiles at least a second ago
    /// ([`levels_waiting`]) are due again from the next frame, as memory
    /// mapped under them would make them
    /// ([`HostHeap::texture_slots_remapped`]): the upload then gives the
    /// level its tiles. To be called only while the card has room: without
    /// it the upload would give nothing again. Slots tables read are left
    /// to their draws (`texture.rs` `upload` asks again for those). Returns
    /// how many slots it made due.
    /// shortcut: kept in one place for all heaps, as the starts below.
    pub(crate) fn texture_slots_waiting(&self, frame: u64) -> usize {
        if self.sampler {
            return 0;
        }
        let starts: Vec<u64> = {
            let mut waiting = WAITING.lock().unwrap_or_else(|e| e.into_inner());
            if waiting.is_empty() {
                return 0;
            }
            let starts: Vec<u64> = waiting
                .iter()
                .filter(|&(_, &since)| crate::host::long_ago(since, 1000, 2))
                .map(|(&start, _)| start)
                .take(Self::WAITING_A_PASS)
                .collect();
            for start in &starts {
                waiting.remove(start);
            }
            starts
        };
        if starts.is_empty() {
            return 0;
        }
        let mut slots: Vec<u32> = {
            let placed = self.placed.lock().unwrap_or_else(|e| e.into_inner());
            starts
                .iter()
                .flat_map(|&start| placed.slots_at(start))
                .collect()
        };
        slots.sort_unstable();
        slots.dedup();
        slots.retain(|&slot| !self.tabled_lately(slot, frame));
        let mut textures = self.textures.lock().unwrap_or_else(|e| e.into_inner());
        for &slot in &slots {
            textures.changed(slot, frame, frame + 1);
            self.note_slot(slot, SLOT_IN_TEXTURES, true);
        }
        crate::tiles::STATS[10].fetch_add(slots.len() as u64, std::sync::atomic::Ordering::Relaxed);
        slots.len()
    }

    /// Textures a pass asks again for their levels
    /// ([`HostHeap::texture_slots_waiting`]): the first ones given tiles
    /// may take the room the others were asked for, and an upload that
    /// gives nothing is a whole texture staged and copied for nothing.
    const WAITING_A_PASS: usize = 128;

    pub(crate) fn texture_views_remade(&self) -> usize {
        use std::sync::atomic::Ordering::Relaxed;
        if self.sampler {
            return 0;
        }
        // The slots made again here are live slots of the heap the card
        // reads, and the making is the layer's own, at a moment the title
        // knows nothing of: a list already sent may be reading a slot while
        // it is written (Direct3D 12 asks that a descriptor a submitted
        // list reads is left alone until the list has run). Counted always;
        // with `VICTORY_VIEWS_REMADE_QUIET=1` the pass is put off while a
        // submission is in flight, and what is due stays due.
        let flying = crate::host::submissions_in_flight();
        if flying > 0 && views_remade_quiet() {
            if !RESHOWN.lock().unwrap_or_else(|e| e.into_inner()).is_empty() {
                VIEWS_REMADE[2].fetch_add(1, Relaxed);
            }
            return 0;
        }
        let starts = std::mem::take(&mut *RESHOWN.lock().unwrap_or_else(|e| e.into_inner()));
        if starts.is_empty() {
            return 0;
        }
        let slots: Vec<u32> = {
            let placed = self.placed.lock().unwrap_or_else(|e| e.into_inner());
            starts
                .iter()
                .flat_map(|&start| placed.slots_at(start))
                .collect()
        };
        let made = self.made.lock().unwrap_or_else(|e| e.into_inner());
        let mut remade = 0;
        for slot in slots {
            let Some(&key) = made.get(slot as usize) else {
                continue;
            };
            if key == 0 {
                continue;
            }
            let Some(view) = view_at(key as u32, (key >> 32) as u32) else {
                continue;
            };
            if let Some((kept, _)) = make(&view, self.cpu(slot)) {
                self.keep(slot, kept);
                remade += 1;
            }
        }
        let before = VIEWS_REMADE[0].fetch_add(remade as u64, Relaxed);
        if flying > 0 {
            VIEWS_REMADE[1].fetch_add(remade as u64, Relaxed);
        }
        if (before + remade as u64) / 1024 != before / 1024 {
            tracing::info!(
                "view slots of textures with levels on demand made again, since the start: {}, {} of them while a submission was in flight                  (a list already sent may have been reading the slot); {} passes put off for that (VICTORY_VIEWS_REMADE_QUIET=1: {})",
                before + remade as u64,
                VIEWS_REMADE[1].load(Relaxed),
                VIEWS_REMADE[2].load(Relaxed),
                if views_remade_quiet() { "on" } else { "off" }
            );
        }
        remade
    }

    /// The memory of the texture title slot `index` views is not mapped
    /// (any more): a host slot made from it becomes null again, so that
    /// the host copy can be let go (`crate::resource::let_go_idle`; a slot
    /// keeps what it views). True if the slot was made.
    pub(crate) fn texture_slot_gone(&self, index: u32) -> bool {
        let mut made = self.made.lock().unwrap_or_else(|e| e.into_inner());
        if made.get(index as usize).is_none_or(|&key| key == 0) {
            return false;
        }
        self.unmake(&mut made, index);
        self.textures
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .rechecks
            .remove(&index);
        STALE[4].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        true
    }

    /// For `crate::resource::slot_evict`: every slot (title or dynamic)
    /// showing one of the host textures `raws` (raw pointers), as (index,
    /// raw, whether letting it go is safe): a title slot a draw read through
    /// a table lately, which that draw makes again by itself
    /// ([`HostHeap::mirror`]). Slots only shaders pick by index (nothing
    /// would make them again) and dynamic ones (a list not yet reset) are
    /// not.
    pub(crate) fn slots_showing(
        &self,
        raws: &std::collections::HashSet<usize>,
        frame: u64,
    ) -> Vec<(u32, usize, bool)> {
        self.kept
            .iter()
            .enumerate()
            .filter_map(|(i, slot)| {
                // (A lock to a slot: see `kept`.)
                let raw = {
                    let r = slot.lock().unwrap_or_else(|e| e.into_inner());
                    windows::core::Interface::as_raw(r.as_ref()?) as usize
                };
                raws.contains(&raw).then(|| {
                    let i = i as u32;
                    (
                        i,
                        raw,
                        i < self.count && !self.sampler && self.tabled_lately(i, frame),
                    )
                })
            })
            .collect()
    }

    /// Makes host slot `index` null again, as it is before its view is
    /// made: a null view of the kind it held.
    fn unmake(&self, made: &mut [u64], index: u32) {
        let Some(key) = made.get_mut(index as usize).map(std::mem::take) else {
            return;
        };
        if let Some(too) = self.made_too.get(index as usize) {
            too.store(0, std::sync::atomic::Ordering::Release);
        }
        if key == 0 {
            return;
        }
        match view_at(key as u32, (key >> 32) as u32) {
            Some(view) => null_for(&view, self.cpu(index)),
            None => null_view(self.cpu(index)),
        }
        self.keep(index, None);
    }

    /// Title slot `index` is a table's ([`HostHeap::tabled`]). `read`: a
    /// draw read it through the table and brought what it views up to
    /// date, so the slot leaves the queue of textures picked by index; the
    /// caller makes the host slot in place ([`HostHeap::mirror`]), as the
    /// pass would have, for a shader that picks it by index all the same.
    pub(crate) fn texture_slot_tabled(&self, index: u32, read: bool) {
        self.tabled_in(index, read, crate::frame::current());
    }

    /// [`HostHeap::texture_slot_tabled`] in frame `frame`.
    fn tabled_in(&self, index: u32, read: bool, frame: u64) {
        use std::sync::atomic::Ordering::Relaxed;
        let Some(stamp) = self.tabled.get(index as usize) else {
            return;
        };
        let now = (frame as u32).wrapping_add(1) & !TABLED_READ;
        if let Some(block) = self.tabled_blocks.get((index / TABLED_BLOCK) as usize)
            && block.load(Relaxed) != now
        {
            block.store(now, Relaxed);
        }
        let old = stamp.load(Relaxed);
        if !read {
            if old & !TABLED_READ != now {
                stamp.store(now, Relaxed);
            }
            return;
        }
        // Once per slot and frame: tables are read by every draw.
        if old != now | TABLED_READ {
            stamp.store(now | TABLED_READ, Relaxed);
            if tabled_out() {
                let mut textures = self.textures.lock().unwrap_or_else(|e| e.into_inner());
                if textures.slots.remove(&index).is_some() {
                    TABLED_WRITES[1].fetch_add(1, Relaxed);
                }
            }
        }
    }

    /// Whether title slot `index` was a table's within the last
    /// [`TABLED_FRAMES`] frames before `frame` (never, if such slots are
    /// to be queued as any other).
    fn tabled_lately(&self, index: u32, frame: u64) -> bool {
        let stamp = self.tabled_stamp(index) & !TABLED_READ;
        let now = (frame as u32).wrapping_add(1) & !TABLED_READ;
        tabled_out() && stamp != 0 && u64::from(now.wrapping_sub(stamp)) <= TABLED_FRAMES
    }

    /// (Neptune) Whether the pass over records copied in as bytes leaves title slot `index`
    /// to its draws: a table's slot lately ([`HostHeap::tabled_lately`]), or a slot that was
    /// a table's at some time in a block of [`TABLED_BLOCK`] slots where tables were made
    /// lately.
    ///
    /// The second are the slots of tables the title writes for draws that are not drawn: the
    /// ray-tracing structure builder's dispatches above all (some 2,000 a frame in the house,
    /// dropped while the builder is off), whose tables lie in the same ring as every other
    /// draw's. No draw made their tables, so their slots were "not a table's lately" and
    /// 8,400 to 10,300 records a frame were mirrored, 8,400 to 8,900 of them in slots a table
    /// had read at some time, all in slots 327,680 to 786,431 (run ve136); the buffer views
    /// among them were then looked at by the pass over buffer views. A slot shaders pick by
    /// index was never a table's, and is mirrored as before.
    /// `VICTORY_HEAP_SYNC_BLOCKS=0`: only a table's slot lately is left, as before.
    #[allow(dead_code)]
    fn left_to_draws(&self, index: u32, frame: u64) -> bool {
        if self.tabled_lately(index, frame) {
            return true;
        }
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if !tabled_out()
            || self.tabled_stamp(index) == 0
            || !*ON.get_or_init(|| {
                std::env::var_os("VICTORY_HEAP_SYNC_BLOCKS").is_none_or(|v| v != "0")
            })
        {
            return false;
        }
        let block = self
            .tabled_blocks
            .get((index / TABLED_BLOCK) as usize)
            .map_or(0, |b| b.load(std::sync::atomic::Ordering::Relaxed));
        let now = (frame as u32).wrapping_add(1) & !TABLED_READ;
        block != 0 && u64::from(now.wrapping_sub(block)) <= TABLED_FRAMES
    }

    /// (Neptune) Of title slots `slots`, those the pass over records copied in as bytes does
    /// not leave to their draws ([`HostHeap::left_to_draws`], the same rule), into `out`;
    /// the others are counted ([`COPIED_TABLED`]). One pass over the stamps: asked slot by
    /// slot, with the frame and the switches looked up for each, the question alone was
    /// most of the 12 to 16 ms a frame the pass took over 310,000 to 365,000 slots (run
    /// ve138, where it then mirrored 450 of them).
    pub(crate) fn not_left_to_draws(&self, slots: std::ops::Range<u64>, out: &mut Vec<u32>) {
        use std::sync::atomic::Ordering::Relaxed;
        out.clear();
        let slots = slots.start.min(u64::from(self.count)) as usize
            ..slots.end.min(u64::from(self.count)) as usize;
        if sync_tables() || !tabled_out() || self.tabled.len() < slots.end {
            out.extend(slots.map(|i| i as u32));
            return;
        }
        static BLOCKS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        let blocks = *BLOCKS
            .get_or_init(|| std::env::var_os("VICTORY_HEAP_SYNC_BLOCKS").is_none_or(|v| v != "0"));
        let now = (crate::frame::current() as u32).wrapping_add(1) & !TABLED_READ;
        let lately = |stamp: u32| stamp != 0 && u64::from(now.wrapping_sub(stamp)) <= TABLED_FRAMES;
        let n = slots.len();
        let (mut block_of, mut block_lately) = (usize::MAX, false);
        for i in slots {
            let stamp = self.tabled[i].load(Relaxed) & !TABLED_READ;
            if lately(stamp) {
                continue;
            }
            if stamp != 0 && blocks {
                let block = i / TABLED_BLOCK as usize;
                if block != block_of {
                    block_of = block;
                    block_lately = self
                        .tabled_blocks
                        .get(block)
                        .is_some_and(|b| lately(b.load(Relaxed)));
                }
                if block_lately {
                    continue;
                }
            }
            out.push(i as u32);
        }
        COPIED_TABLED.fetch_add((n - out.len()) as u64, Relaxed);
    }

    /// What `tabled` holds for title slot `index`; 0: never a table's.
    fn tabled_stamp(&self, index: u32) -> u32 {
        self.tabled
            .get(index as usize)
            .map_or(0, |s| s.load(std::sync::atomic::Ordering::Relaxed))
    }

    /// Whether title slot `index` may be in the set `bit` stands for
    /// ([`HostHeap::slot_flags`]): a slot the flags do not cover (a dynamic
    /// one) is always taken to be, so it takes the set's lock as before.
    fn slot_may(&self, index: u32, bit: u8) -> bool {
        self.slot_flags
            .get(index as usize)
            .is_none_or(|f| f.load(std::sync::atomic::Ordering::Relaxed) & bit != 0)
    }

    /// Notes in [`HostHeap::slot_flags`] that title slot `index` is
    /// (`there`), or is not, in the set `bit` stands for (nothing for
    /// dynamic slots).
    fn note_slot(&self, index: u32, bit: u8, there: bool) {
        use std::sync::atomic::Ordering::Relaxed;
        let Some(flag) = self.slot_flags.get(index as usize) else {
            return;
        };
        if there {
            flag.fetch_or(bit, Relaxed);
        } else {
            flag.fetch_and(!bit, Relaxed);
        }
    }

    /// The queue's part of a write of title slot `index` in `frame`:
    /// `texture`, whether it now holds a view of a texture the title may
    /// fill; `seen` as for [`HostHeap::note_buffer_into`].
    fn queue_texture(&self, index: u32, texture: bool, seen: bool, frame: u64) {
        let stamp = if tabled_out() {
            self.tabled_stamp(index)
        } else {
            0
        };
        let now = (frame as u32).wrapping_add(1) & !TABLED_READ;
        // A table's slot lately: the draw that reads it uploads its
        // texture, so its write waits for that draw.
        let lately =
            stamp != 0 && u64::from(now.wrapping_sub(stamp & !TABLED_READ)) <= TABLED_FRAMES;
        // Made now by a draw of this frame that read it through a table:
        // up to date.
        let read_now = seen && stamp == now | TABLED_READ;
        let mut textures = self.textures.lock().unwrap_or_else(|e| e.into_inner());
        if texture && !read_now {
            let due = if lately && !seen {
                TABLED_WRITES[0].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                frame + TABLED_GRACE
            } else {
                0
            };
            textures.written(index, frame, due, seen);
            self.note_slot(index, SLOT_IN_TEXTURES, true);
        } else {
            textures.remove(index);
            // Out of the queue: nothing to take out here until it is
            // written again ([`HostHeap::note_buffer_into`]).
            self.note_slot(index, SLOT_IN_TEXTURES, false);
        }
    }

    /// The title slots that hold buffer views.
    pub(crate) fn buffer_slots(&self) -> Vec<u32> {
        let buffers = self.buffers.lock().unwrap_or_else(|e| e.into_inner());
        buffers.iter().copied().collect()
    }

    /// The title slots whose buffer view was written, or went, since the
    /// last call.
    pub(crate) fn buffer_changes(&self) -> Vec<u32> {
        let mut changed = self.changed.lock().unwrap_or_else(|e| e.into_inner());
        // The set keeps its allocation across passes. It is emptied under
        // the same lock as before, so writes racing this drain still belong
        // to the next pass.
        changed.drain().collect()
    }

    /// [`HostHeap::note_buffer_into`] without a batch, for tests.
    #[cfg(test)]
    fn note_buffer(
        &self,
        index: u32,
        view: Option<&View>,
        seen: bool,
        record_change: bool,
    ) -> Vec<u32> {
        self.note_buffer_into(index, view, seen, record_change, None)
    }

    /// Records what title slot `index` now holds for shaders that pick
    /// buffers from the heap: its element stride in the table, and whether
    /// it is a buffer view at all.
    /// `seen`: the slot was not written now, a draw or a pass made its
    /// host slot from what it held.
    /// `record_change`: hand a buffer slot whose membership changed to
    /// [`HostHeap::buffer_changes`] (not for the pass that took it from
    /// there: [`HostHeap::mirror_heap_buffer`]), or to `batch` when given
    /// (a page of them, `mirror_copied_page`).
    /// Returns the title slots whose placed texture view the write made
    /// stale ([`Spans`]): out of the queue of textures already, their host
    /// slots are the caller's to make null (it holds `made`).
    fn note_buffer_into(
        &self,
        index: u32,
        view: Option<&View>,
        seen: bool,
        record_change: bool,
        batch: Option<&mut Vec<u32>>,
    ) -> Vec<u32> {
        if self.sampler {
            return Vec::new();
        }
        // Measured for the texture side of the same gap (textures a shader
        // picks by a computed index are neither made, if placed, nor
        // uploaded): slots written with a placed texture view, and with a
        // view of a texture object.
        let texture = match view {
            Some(View::Placed(false, resource, _)) if resource[0] != 1 => {
                PLACED_TEXTURE_SLOTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                true
            }
            Some(View::Shader(r, _))
                if crate::resource::buffer_bytes(*r as crate::object::This).is_none() =>
            {
                OBJECT_TEXTURE_SLOTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                true
            }
            _ => false,
        };
        // A slot taking a view out of the queue is nearly always one that is
        // not in it: asked of the slot's flags first, so that the queue's
        // lock is taken only to take something out.
        if texture || self.slot_may(index, SLOT_IN_TEXTURES) {
            self.queue_texture(index, texture, seen, crate::frame::current());
        }
        // The memory a placed texture view describes. Not for a slot
        // tables read (thousands of writes a frame, each read by its
        // draw): no pass looks at it, and the same texture's view in a
        // slot shaders pick by index says what it says.
        let placed_view = match view {
            Some(View::Placed(false, resource, desc))
                if resource[0] != 1 && !self.tabled_lately(index, crate::frame::current()) =>
            {
                let first = crate::resource::first_mip(desc);
                Some((
                    resource,
                    u64::from(desc[10]) | u64::from(desc[11]) << 32,
                    first,
                ))
            }
            _ => None,
        };
        let stale = if placed_view.is_some() || self.slot_may(index, SLOT_IN_PLACED) {
            use std::sync::atomic::Ordering::Relaxed;
            let mut placed = self.placed.lock().unwrap_or_else(|e| e.into_inner());
            let span = placed_view.and_then(|(resource, start, first)| {
                Some((
                    resource,
                    start,
                    start.saturating_add(placed.bytes(resource, first)?),
                    first,
                ))
            });
            match span {
                Some((resource, start, end, first)) => {
                    SPANNED[0].fetch_min(start, Relaxed);
                    SPANNED[1].fetch_max(end, Relaxed);
                    let stale = placed.note(index, start, end, resource, !seen && stale_out());
                    placed.shows(index, first);
                    let there = placed.by_slot.contains_key(&index);
                    self.note_slot(index, SLOT_IN_PLACED, there);
                    for &slot in &stale {
                        self.note_slot(slot, SLOT_IN_PLACED, false);
                    }
                    stale
                }
                None => {
                    placed.forget(index);
                    self.note_slot(index, SLOT_IN_PLACED, false);
                    Vec::new()
                }
            }
        } else {
            // Not in the spans as far as the flags say: nothing to forget.
            Vec::new()
        };
        if !stale.is_empty() {
            STALE[0].fetch_add(stale.len() as u64, std::sync::atomic::Ordering::Relaxed);
            let mut textures = self.textures.lock().unwrap_or_else(|e| e.into_inner());
            for &slot in &stale {
                textures.remove(slot);
                self.note_slot(slot, SLOT_IN_TEXTURES, false);
            }
        }
        let stride = view.and_then(view_stride);
        if let Some((_, _, data)) = &self.strides
            && index < victory_shader::emit::SLOT_STRIDES_COUNT
        {
            // SAFETY: inside the mapping; shaders read it while we write (a
            // dword, either value is one the slot had).
            unsafe {
                (*data as *mut u32)
                    .add(index as usize)
                    .write_volatile(stride.unwrap_or(0))
            };
        }
        // The set is only written here: a slot with its flag set is in it,
        // and the lock is taken only to put one in or to take one out.
        let flagged = self.slot_flags.get(index as usize);
        let in_buffers = flagged
            .is_some_and(|f| f.load(std::sync::atomic::Ordering::Relaxed) & SLOT_IN_BUFFERS != 0);
        let was = if stride.is_some() {
            if !in_buffers {
                self.buffers
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(index);
                self.note_slot(index, SLOT_IN_BUFFERS, true);
            }
            true
        } else if in_buffers || flagged.is_none() {
            let removed = self
                .buffers
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&index);
            self.note_slot(index, SLOT_IN_BUFFERS, false);
            removed
        } else {
            false
        };
        if was && record_change {
            match batch {
                Some(changed) => changed.push(index),
                None => {
                    self.changed
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(index);
                }
            }
        }
        stale
    }

    /// The identity to store with a range this heap allocated.
    pub(crate) fn id(&self) -> u64 {
        self.id
    }

    /// Gives back `n` dynamic slots from `first` (an index [`dynamic`]
    /// returned) when they came from this heap.
    ///
    /// [`dynamic`]: HostHeap::dynamic
    #[cfg(test)]
    pub(crate) fn release(&self, id: u64, first: u32, n: u32) {
        self.release_many(id, &[(first, n)]);
    }

    /// Gives back a batch of `(first, n)` dynamic slot ranges under one lock,
    /// retiring any host resources those slots kept alive so released dynamic
    /// tables do not pin evicted VRAM/shared memory. Ranges another host heap
    /// took (`id` not this heap's: the title heap's address was reused) are
    /// left alone.
    pub(crate) fn release_many(&self, id: u64, ranges: &[(u32, u32)]) {
        if ranges.is_empty() || id != self.id {
            return;
        }
        let mut retired: Vec<ID3D12Resource> = Vec::new();
        let released_any = {
            let mut guard = self.next.lock().unwrap_or_else(|e| e.into_inner());
            let mut given = 0u32;
            for &(first, n) in ranges {
                let Some(at) = first.checked_sub(self.count) else {
                    continue;
                };
                if (at as usize) < guard.1.len {
                    given += guard
                        .1
                        .clear_taken_with(at as usize, n as usize, |slot_idx| {
                            let index = self.count as usize + slot_idx;
                            if let Some(slot) = self.kept.get(index)
                                && let Some(old) =
                                    slot.lock().unwrap_or_else(|e| e.into_inner()).take()
                            {
                                retired.push(old);
                            }
                        });
                }
            }
            if given != 0 {
                self.free
                    .fetch_add(given, std::sync::atomic::Ordering::Relaxed);
                self.too_long
                    .store(u32::MAX, std::sync::atomic::Ordering::Relaxed);
                self.timed_out_frame
                    .store(u64::MAX, std::sync::atomic::Ordering::Relaxed);
                self.timed_out_min
                    .store(u32::MAX, std::sync::atomic::Ordering::Relaxed);
                true
            } else {
                false
            }
        };
        if released_any {
            self.released.notify_all();
        }
        for old in retired {
            crate::host::retire(old);
        }
    }

    /// Makes slot `index` (a dynamic one) from `view`; a null view if the
    /// host cannot.
    pub(crate) fn make_at(&self, index: u32, view: &View) {
        let made = make(view, self.cpu(index));
        if made.is_none() {
            null_for(view, self.cpu(index));
        }
        self.keep(index, made.and_then(|(kept, _)| kept));
    }

    /// Makes slot `index` read nothing: a null view, or a plain sampler.
    pub(crate) fn make_null(&self, index: u32) {
        if self.sampler {
            self.null_sampler(index);
        } else {
            null_view(self.cpu(index));
        }
        self.keep(index, None);
    }

    /// Makes slot `index` read nothing through a null view shaped like
    /// `view` (its dimension, or a buffer).
    pub(crate) fn make_null_for(&self, index: u32, view: &View) {
        null_for(view, self.cpu(index));
        self.keep(index, None);
    }

    /// Makes slot `index` a null raw buffer view (reads zeros).
    pub(crate) fn make_null_buffer(&self, index: u32) {
        self.keep(index, None);
        null_buffer_view(self.cpu(index));
    }

    /// Slot `index` now views `resource` (or nothing of the host's).
    fn keep(&self, index: u32, resource: Option<ID3D12Resource>) {
        if let Some(slot) = self.kept.get(index as usize) {
            let old = {
                let mut guard = slot.lock().unwrap_or_else(|e| e.into_inner());
                match (guard.as_ref(), resource.as_ref()) {
                    (None, None) => return,
                    (Some(cur), Some(new))
                        if windows::core::Interface::as_raw(cur)
                            == windows::core::Interface::as_raw(new) =>
                    {
                        return;
                    }
                    _ => std::mem::replace(&mut *guard, resource),
                }
            };
            // A running submission may still read the old resource through
            // this slot.
            if let Some(old) = old {
                crate::host::retire(old);
            }
        }
    }

    /// Slot `index` now views `resource`, avoiding a COM clone and retire when
    /// the slot already holds the exact same resource pointer.
    fn keep_ref(&self, index: u32, resource: &ID3D12Resource) {
        if let Some(slot) = self.kept.get(index as usize) {
            let old = {
                let mut guard = slot.lock().unwrap_or_else(|e| e.into_inner());
                if guard.as_ref().is_some_and(|cur| {
                    windows::core::Interface::as_raw(cur)
                        == windows::core::Interface::as_raw(resource)
                }) {
                    return;
                }
                std::mem::replace(&mut *guard, Some(resource.clone()))
            };
            // A running submission may still read the old resource through
            // this slot.
            if let Some(old) = old {
                crate::host::retire(old);
            }
        }
    }

    /// Makes slot `index` (a dynamic one) a raw buffer view of `len` bytes
    /// of `buffer` from byte `offset` (a multiple of 16): a root SRV with a
    /// size, whose reads past it return zeros instead of reaching whatever
    /// the host placed after the buffer.
    pub(crate) fn raw_at(&self, index: u32, buffer: &ID3D12Resource, offset: u64, len: u64) {
        let Some(host) = host() else { return };
        let elements = (len / 4).clamp(1, u64::from(u32::MAX)) as u32;
        let desc = D3D12_SHADER_RESOURCE_VIEW_DESC {
            Format: DXGI_FORMAT_R32_TYPELESS,
            ViewDimension: D3D12_SRV_DIMENSION_BUFFER,
            Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
            Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                Buffer: D3D12_BUFFER_SRV {
                    FirstElement: offset / 4,
                    NumElements: elements,
                    StructureByteStride: 0,
                    Flags: D3D12_BUFFER_SRV_FLAG_RAW,
                },
            },
        };
        // SAFETY: a slot of our heap and a live buffer covering the range.
        unsafe {
            host.device
                .CreateShaderResourceView(buffer, Some(&desc), self.cpu(index))
        };
        self.keep_ref(index, buffer);
    }

    /// Makes slot `index` (a dynamic one) a raw buffer view that reads
    /// zeros.
    pub(crate) fn null_raw_at(&self, index: u32) {
        null_buffer_view(self.cpu(index));
        self.keep(index, None);
    }

    /// Makes slot `index` (a dynamic one) a raw unordered access view of
    /// `len` bytes of `buffer` from byte `offset` (a multiple of 16): a
    /// root UAV with a size, whose accesses past it do nothing instead of
    /// reaching whatever the host placed after the buffer.
    pub(crate) fn raw_uav_at(&self, index: u32, buffer: &ID3D12Resource, offset: u64, len: u64) {
        let Some(host) = host() else { return };
        let elements = (len / 4).clamp(1, u64::from(u32::MAX)) as u32;
        let desc = D3D12_UNORDERED_ACCESS_VIEW_DESC {
            Format: DXGI_FORMAT_R32_TYPELESS,
            ViewDimension: D3D12_UAV_DIMENSION_BUFFER,
            Anonymous: D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
                Buffer: D3D12_BUFFER_UAV {
                    FirstElement: offset / 4,
                    NumElements: elements,
                    StructureByteStride: 0,
                    CounterOffsetInBytes: 0,
                    Flags: D3D12_BUFFER_UAV_FLAG_RAW,
                },
            },
        };
        // SAFETY: a slot of our heap and a live buffer covering the range.
        unsafe {
            host.device.CreateUnorderedAccessView(
                buffer,
                None::<&ID3D12Resource>,
                Some(&desc),
                self.cpu(index),
            )
        };
        self.keep_ref(index, buffer);
    }

    /// Makes slot `index` (a dynamic one) a raw unordered access view of
    /// nothing: reads give zeros, writes are dropped.
    pub(crate) fn null_raw_uav_at(&self, index: u32) {
        self.keep(index, None);
        let Some(host) = host() else { return };
        let desc = D3D12_UNORDERED_ACCESS_VIEW_DESC {
            Format: DXGI_FORMAT_R32_TYPELESS,
            ViewDimension: D3D12_UAV_DIMENSION_BUFFER,
            Anonymous: D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
                Buffer: D3D12_BUFFER_UAV {
                    NumElements: 1,
                    Flags: D3D12_BUFFER_UAV_FLAG_RAW,
                    ..Default::default()
                },
            },
        };
        // SAFETY: a slot of our heap; a null resource is allowed with a desc.
        unsafe {
            host.device.CreateUnorderedAccessView(
                None::<&ID3D12Resource>,
                None::<&ID3D12Resource>,
                Some(&desc),
                self.cpu(index),
            )
        };
    }

    /// The shader-visible handle of slot `index`.
    pub(crate) fn gpu_handle(&self, index: u32) -> D3D12_GPU_DESCRIPTOR_HANDLE {
        // SAFETY: a live shader-visible heap.
        let start = unsafe { self.heap.GetGPUDescriptorHandleForHeapStart().ptr };
        D3D12_GPU_DESCRIPTOR_HANDLE {
            ptr: start + u64::from(index) * u64::from(self.step),
        }
    }

    /// Makes slot `index` (a dynamic one) a constant buffer view of host
    /// memory (the list's own, which it holds).
    pub(crate) fn constant_at(&self, index: u32, gpu: u64, size: u32) {
        self.keep(index, None);
        let Some(host) = host() else { return };
        let desc = D3D12_CONSTANT_BUFFER_VIEW_DESC {
            BufferLocation: gpu,
            SizeInBytes: constant_size(size),
        };
        // SAFETY: a slot of our heap; the memory is the caller's.
        unsafe {
            host.device
                .CreateConstantBufferView(Some(&desc), self.cpu(index))
        };
    }

    /// Renderer port 5 (`crate::port_cbv`): makes slot `index` (a dynamic
    /// one) a raw buffer view of the constant buffer of `size` bytes at `gpu`
    /// of `buffer`, directly in the slot itself. Returns `false` if `gpu` is
    /// not 16-byte aligned inside `buffer`.
    pub(crate) fn constant_raw_at(
        &self,
        index: u32,
        buffer: &ID3D12Resource,
        gpu: u64,
        size: u32,
    ) -> bool {
        if !constant_raw_view(buffer, gpu, size, self.cpu(index)) {
            return false;
        }
        self.keep_ref(index, buffer);
        true
    }

    pub(crate) fn cpu(&self, index: u32) -> D3D12_CPU_DESCRIPTOR_HANDLE {
        D3D12_CPU_DESCRIPTOR_HANDLE {
            ptr: self.cpu + index as usize * self.step as usize,
        }
    }

    /// For a slot on a page of the title's heap that changed with no call
    /// (`super::sync_heaps`: the title copies records into its heap as
    /// bytes): [`HostHeap::mirror`], if the `bytes` bytes of the record at
    /// `handle` are not the ones this last saw there. True if it did.
    ///
    /// The page says only that something on it was written, and `mirror`
    /// by itself would take every buffer or placed-texture record on it for
    /// newly written each time (it leaves such a slot unmade until a draw
    /// or a pass uses it): the texture queue's entry for the slot would
    /// start again, and the buffer pass would look at 2,048 slots for one
    /// that changed.
    /// `record`: the record's bytes as the caller read them (through the
    /// kernel, which fails on a page the title unmapped meanwhile where a
    /// read in place would fault: the title unmaps heap pages too).
    ///
    /// (Neptune) The caller has asked which slots are left to their draws
    /// ([`HostHeap::not_left_to_draws`]) and comes with the others.
    /// Batched [`HostHeap::mirror_copied`] over one page's `asked` slots:
    /// locks `self.copied` once per page instead of once per slot, skips
    /// all-zero never-seen records without hashing, and reuses `mirror`'s
    /// returned view for [`COPIED_CENSUS`]. Returns how many slots mirrored.
    pub(crate) fn mirror_copied_page(
        &self,
        start: u64,
        step: u64,
        from: usize,
        page_bytes: &[u8],
        asked: &[u32],
        changed: &mut Vec<(u32, u64)>,
    ) -> u64 {
        changed.clear();
        let step_usize = step as usize;
        {
            let mut copied = self.copied.lock().unwrap_or_else(|e| e.into_inner());
            if copied.is_empty() {
                copied.resize(self.count as usize, 0);
            }
            for &index in asked {
                if index >= self.count {
                    continue;
                }
                let handle = start + u64::from(index) * step;
                let Some(at) = (handle as usize).checked_sub(from) else {
                    continue;
                };
                let Some(record) = page_bytes.get(at..at + step_usize) else {
                    continue;
                };
                let last = &mut copied[index as usize];
                if *last == 0 && record_is_zero(record) {
                    continue;
                }
                let print = record_print(record) | 1;
                if *last == print {
                    continue;
                }
                *last = print;
                changed.push((index, handle));
            }
        }
        if !changed.is_empty() {
            // The records' lock once for the batch, and the buffer changes'
            // once, not once for each record mirrored (some 270 a page,
            // 42,000 a frame): `note_buffer_into` pushes the slots whose buffer
            // membership changed into `buf_changed` under the records' lock.
            let mut buf_changed = Vec::new();
            {
                let mut made = self.made.lock().unwrap_or_else(|e| e.into_inner());
                for &(index, handle) in changed.iter() {
                    let view = self
                        .mirror_with_made(
                            &mut made,
                            index,
                            handle,
                            false,
                            true,
                            Some(&mut buf_changed),
                        )
                        .map(|(view, _)| view);
                    self.note_copied_census(index, view.as_ref());
                }
            }
            if !buf_changed.is_empty() {
                self.changed
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .extend(buf_changed);
            }
        }
        changed.len() as u64
    }

    fn note_copied_census(&self, index: u32, view: Option<&View>) {
        use std::sync::atomic::Ordering::Relaxed;
        let buffer = |r: usize| {
            usize::from(crate::resource::buffer_bytes(r as crate::object::This).is_some())
        };
        let kind = match view {
            None => 0,
            Some(View::Constant(..)) => 1,
            Some(View::Shader(r, _)) => 2 + buffer(*r),
            Some(View::Unordered(r, _, _)) => 4 + buffer(*r),
            Some(View::Sampler(_) | View::ConsoleSampler(_)) => 6,
            Some(View::Placed(false, ..)) => 7,
            Some(View::Placed(true, ..)) => 8,
        };
        COPIED_CENSUS[kind].fetch_add(1, Relaxed);
        if self.tabled_stamp(index) != 0 {
            COPIED_CENSUS[9].fetch_add(1, Relaxed);
        }
        COPIED_CENSUS[10 + (index as usize >> 16).min(21)].fetch_add(1, Relaxed);
    }

    #[allow(dead_code)]
    pub(crate) fn mirror_copied(&self, index: u32, handle: u64, record: &[u8]) -> bool {
        if index >= self.count {
            return false;
        }
        {
            let mut copied = self.copied.lock().unwrap_or_else(|e| e.into_inner());
            if copied.is_empty() {
                copied.resize(self.count as usize, 0);
            }
            let Some(last) = copied.get_mut(index as usize) else {
                return false;
            };
            if *last == 0 && record_is_zero(record) {
                return false;
            }
            let print = record_print(record) | 1;
            if *last == print {
                return false;
            }
            *last = print;
        }
        let view = self.mirror(index, handle, false);
        self.note_copied_census(index, view.as_ref());
        true
    }

    /// Makes host slot `index` from the record the title has at `handle`
    /// if it changed since, and returns the view recorded there. Buffer
    /// views are made only for draws (`for_draw`), which also copy their
    /// data: a host copy of a buffer is made on first use.
    pub(crate) fn mirror(&self, index: u32, handle: u64, for_draw: bool) -> Option<View> {
        self.mirror_inner(index, handle, for_draw, true)
            .map(|(view, _)| view)
    }

    /// [`HostHeap::mirror`] for the pass over slots whose buffer view was
    /// written (`heap_buffers` in the draw code): a slot it mirrors is not
    /// handed back to the pass as changed, which made it look at every
    /// buffer slot it mirrored again on the next frame. The copy the slot
    /// reads ([`buffer_copy`]) comes with it when making the view resolved
    /// it, so the pass need not look it up again.
    pub(crate) fn mirror_heap_buffer(
        &self,
        index: u32,
        handle: u64,
    ) -> Option<(View, Option<BufferCopy>)> {
        self.mirror_inner(index, handle, true, false)
    }

    fn slot_in_bounds(&self, index: u32, for_draw: bool) -> bool {
        if index < self.count {
            return true;
        }
        // Diagnostic (NVIDIA heap cap): the highest title slot past the
        // host heap seen so far.
        static HIGHEST: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        if HIGHEST.fetch_max(index, std::sync::atomic::Ordering::Relaxed) < index {
            tracing::warn!(
                index,
                count = self.count,
                for_draw,
                "title slot past the host heap"
            );
        }
        false
    }

    /// The view at `handle`, made on slot `index` if it changed since, and
    /// the copy a buffer view was made on when the make resolved it
    /// ([`BufferCopy`]; otherwise `None` and the caller looks it up with
    /// [`buffer_copy`]).
    fn mirror_inner(
        &self,
        index: u32,
        handle: u64,
        for_draw: bool,
        record_change: bool,
    ) -> Option<(View, Option<BufferCopy>)> {
        if !self.slot_in_bounds(index, for_draw) {
            return None;
        }
        // SAFETY: the handle is a descriptor slot in title memory.
        let [tag, i, generation] = unsafe { (handle as *const [u32; 3]).read_unaligned() };
        // Generations start at 1, so a made slot's key is never 0.
        let key = u64::from(generation) << 32 | u64::from(i);
        // (Neptune) The slot shows this view already: nothing to do, and no lock.
        if tag == VIEW_TAG
            && self.made_too[index as usize].load(std::sync::atomic::Ordering::Acquire) == key
            && let Some(view) = view_at(i, generation)
        {
            return Some((view, None));
        }
        let mut made = self.made.lock().unwrap_or_else(|e| e.into_inner());
        self.mirror_with_made(&mut made, index, handle, for_draw, record_change, None)
    }

    /// [`HostHeap::mirror_inner`] with the records already locked, for a
    /// whole page of them (`mirror_copied_page`): `batch`, when given, takes
    /// the buffer slots whose membership changed instead of each locking
    /// [`HostHeap::changed`].
    fn mirror_with_made(
        &self,
        made: &mut [u64],
        index: u32,
        handle: u64,
        for_draw: bool,
        record_change: bool,
        mut batch: Option<&mut Vec<u32>>,
    ) -> Option<(View, Option<BufferCopy>)> {
        if !self.slot_in_bounds(index, for_draw) {
            return None;
        }
        // SAFETY: the handle is a descriptor slot in title memory.
        let [tag, i, generation] = unsafe { (handle as *const [u32; 3]).read_unaligned() };
        // Generations start at 1, so a made slot's key is never 0.
        let key = u64::from(generation) << 32 | u64::from(i);
        let view = (tag == VIEW_TAG).then(|| view_at(i, generation)).flatten();
        let Some(view) = view else {
            // Not a view of ours (never written, or of a resource the
            // title released): nothing to read.
            if made[index as usize] != 0 {
                made[index as usize] = 0;
                self.made_too[index as usize].store(0, std::sync::atomic::Ordering::Release);
                self.make_null(index);
                let _ = self.note_buffer_into(index, None, false, record_change, batch);
            } else if for_draw {
                // Left for later and stale since: out of the sets and the stride table.
                let _ = self.note_buffer_into(index, None, false, record_change, batch);
            }
            return None;
        };
        // The copy the make resolved for a buffer view, if it made one.
        let mut resolved = None;
        if made[index as usize] != key {
            // Views of other textures in the memory this one describes
            // are stale: their host slots read nothing from here on, and
            // let go of the host copies they held.
            let stale = self.note_buffer_into(
                index,
                Some(&view),
                for_draw,
                record_change,
                batch.as_mut().map(|b| &mut **b),
            );
            for stale in stale {
                if stale != index {
                    self.unmake(made, stale);
                }
            }
            // A view of a texture object that has no host copy yet waits
            // for it, as buffer views wait for a draw: the slot is made
            // when the texture is first used (`super::make_waiting`), or
            // here when a draw reads the slot through a table.
            let waiting = if for_draw {
                None
            } else {
                view.waiting_texture()
            };
            if let Some(texture) = waiting {
                super::wait_for(texture, handle);
            }
            let later = !for_draw && (view.is_buffer() || waiting.is_some());
            // Another view of the placed texture the slot shows already (a
            // title that streams a texture in writes its view again with
            // more mips each time some arrive): the slot goes on showing
            // the old view until a draw or the texture pass makes the new
            // one and uploads what it shows. Made null here, the texture
            // read as nothing for as long as it waited in the queue, every
            // time it got sharper. (A slot never made has no view at
            // `(0, 0)` to ask after: the ask took the views' lock for
            // nothing, some 40,000 times a frame in `sync_heaps`.)
            //
            // (Neptune) And a view of another texture: the title streams a material's
            // texture in as a new texture at another address and writes the slot with it
            // (and with the small one again when it lets the large one go). The slot read as
            // nothing from that write until the texture pass had uploaded the new texture,
            // seconds at times: a torso, a wall, a building black, then back
            // (`VICTORY_KEEP_OTHER_VIEWS=0`: null at once, as before). What the slot keeps
            // showing is the host copy it holds; the copy lives as long as the slot does.
            if later
                && keep_views()
                && made[index as usize] != 0
                && let Some(old) = view_at(
                    made[index as usize] as u32,
                    (made[index as usize] >> 32) as u32,
                )
            {
                let same = same_placed_texture(&old, &view);
                let other = !same
                    && keep_other_views()
                    && shows_texture(&view)
                    && shows_texture(&old)
                    && self
                        .kept
                        .get(index as usize)
                        .is_some_and(|k| k.lock().unwrap_or_else(|e| e.into_inner()).is_some());
                if same || other {
                    if same { &KEPT_VIEWS } else { &KEPT_OTHER_VIEWS }
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    return Some((view, None));
                }
            }
            if later {
                // A slot of ours that never made a view is already null
                // (`HostHeap::new` makes every slot null, `made` starts 0
                // and `kept` at `None`): making it null again costs a
                // D3D12 `Create*View` for every buffer slot a frame
                // (`heap_buffers` mirrors thousands).
                if made[index as usize] != 0 {
                    made[index as usize] = 0;
                    self.made_too[index as usize].store(0, std::sync::atomic::Ordering::Release);
                    if !self.sampler {
                        null_for(&view, self.cpu(index));
                    }
                    self.keep(index, None);
                }
            } else {
                made[index as usize] = key;
                match make(&view, self.cpu(index)) {
                    Some((kept, copy)) => {
                        self.keep(index, kept);
                        resolved = copy;
                    }
                    None => {
                        if !self.sampler {
                            null_for(&view, self.cpu(index));
                        }
                        self.keep(index, None);
                    }
                }
                self.made_too[index as usize].store(key, std::sync::atomic::Ordering::Release);
            }
        }
        Some((view, resolved))
    }

    /// Like [`HostHeap::mirror`] with `for_draw = true` for `heap_textures`,
    /// but when [`show_after`] is on and slot `index` does not yet show this
    /// texture view, returns `(Some(view), Some(key))` without writing the
    /// live descriptor into the shader-visible heap yet. The caller records
    /// the upload on the list and later calls [`commit_show`] once the
    /// submission has finished on the GPU.
    pub(crate) fn mirror_deferred(&self, index: u32, handle: u64) -> (Option<View>, Option<u64>) {
        if !show_after() || index >= self.count {
            return (self.mirror(index, handle, true), None);
        }
        // SAFETY: the handle is a descriptor slot in title memory.
        let [tag, i, generation] = unsafe { (handle as *const [u32; 3]).read_unaligned() };
        let key = u64::from(generation) << 32 | u64::from(i);
        if tag == VIEW_TAG
            && self.made_too[index as usize].load(std::sync::atomic::Ordering::Acquire) == key
            && let Some(view) = view_at(i, generation)
        {
            return (Some(view), None);
        }
        if tag == VIEW_TAG
            && let Some(view) = view_at(i, generation)
            && shows_texture(&view)
        {
            return (Some(view), Some(key));
        }
        (self.mirror(index, handle, true), None)
    }

    /// Whether slot `index` is backed by a made host descriptor (tests).
    #[cfg(test)]
    pub(crate) fn made(&self, index: u32) -> bool {
        self.made
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(index as usize)
            .is_some_and(|&key| key != 0)
    }

    /// Whether host slot `index` shows `buffer`; `None` if it shows no
    /// resource (diagnostics: a buffer view's slot must show the host copy
    /// that is brought up to date for it).
    #[cfg(feature = "diagnostics")]
    pub(crate) fn shows(&self, index: u32, buffer: &ID3D12Resource) -> Option<bool> {
        let kept = self
            .kept
            .get(index as usize)?
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let shown = kept.as_ref()?;
        Some(windows::core::Interface::as_raw(shown) == windows::core::Interface::as_raw(buffer))
    }

    /// What the queue of textures picked by index knows of title slot
    /// `index` at `frame`, in a few words (diagnostics).
    #[cfg(feature = "diagnostics")]
    pub(crate) fn texture_slot_state(&self, index: u32, frame: u64) -> String {
        let textures = self.textures.lock().unwrap_or_else(|e| e.into_inner());
        match textures.slots.get(&index) {
            None if textures.rechecks.contains_key(&index) => {
                "uploaded whole, to be looked at again".into()
            }
            None => "not queued".into(),
            Some(w) => format!(
                "queued due={} written@{} looks={} part_mapped={} recheck={}",
                if w.due <= frame {
                    "now".to_string()
                } else {
                    format!("@{}", w.due)
                },
                w.written,
                w.looks,
                w.mapped,
                textures.rechecks.contains_key(&index)
            ),
        }
    }

    /// The view host slot `index` was made from, in a few words, with the
    /// title and host addresses of a buffer view (diagnostics). "null" for
    /// a slot never made or left null.
    #[cfg(feature = "diagnostics")]
    pub(crate) fn describe_slot(&self, index: u32) -> Option<String> {
        let key = *self
            .made
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(index as usize)?;
        if key == 0 {
            return Some("null".into());
        }
        let view = view_at(key as u32, (key >> 32) as u32)?;
        let mut text: String = format!("{view:?}").chars().take(120).collect();
        if let Some((address, len)) = buffer_range(&view) {
            let host = crate::shadow::find(address, len.max(1)).map(|s| s.gpu(address));
            text.push_str(&format!(" title {address:#x}+{len:#x} host {host:#x?}"));
        }
        Some(text)
    }
}

/// Logs `what` the first time it happens.
macro_rules! once {
    ($($arg:tt)*) => {{
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| tracing::warn!($($arg)*));
    }};
}

/// A view description from its words, with the format as the host has it.
fn srv_desc(words: &[u32; 10]) -> D3D12_SHADER_RESOURCE_VIEW_DESC {
    let mut words = *words;
    words[0] = crate::resource::pc_format(words[0]);
    // SAFETY: 40 bytes in the host's layout.
    unsafe {
        std::ptr::from_ref(&words)
            .cast::<D3D12_SHADER_RESOURCE_VIEW_DESC>()
            .read_unaligned()
    }
}

/// The unsigned integer format with the layout of a 16-bit float or
/// signed integer format (one, two or four channels), if `format` is one.
pub(crate) fn half_format(format: u32) -> Option<u32> {
    match format {
        // R16G16B16A16_FLOAT and _SINT: R16G16B16A16_UINT.
        10 | 14 => Some(12),
        // R16G16_FLOAT and _SINT: R16G16_UINT.
        34 | 38 => Some(36),
        // R16_FLOAT and _SINT: R16_UINT.
        54 | 59 => Some(57),
        _ => None,
    }
}

/// `view` as a shader is shown it where it moves a buffer's 16-bit
/// channels as unsigned integers
/// (`victory_shader::emit::HostShader::half_views`): a buffer view in a
/// 16-bit float or signed format, in the unsigned format of the same
/// layout, through which every 16-bit pattern is read and stored as it
/// is. `None` for every other view: the title's own serves (an unsigned
/// one already carries the bits; a texture can only be viewed in the
/// formats it was made for).
pub(crate) fn half_view(view: &View) -> Option<View> {
    // Buffer views: dimension 1 in the title's description of either kind.
    const BUFFER: u32 = 1;
    match view {
        View::Shader(resource, Some(desc)) if desc[1] == BUFFER => {
            let mut desc = *desc;
            desc[0] = half_format(desc[0])?;
            Some(View::Shader(*resource, Some(desc)))
        }
        View::Unordered(resource, counter, Some(desc)) if desc[1] == BUFFER => {
            let mut desc = *desc;
            desc[0] = half_format(desc[0])?;
            Some(View::Unordered(*resource, *counter, Some(desc)))
        }
        View::Placed(unordered, resource, desc) if resource[0] == BUFFER && desc[1] == BUFFER => {
            let mut desc = *desc;
            desc[0] = half_format(desc[0])?;
            Some(View::Placed(*unordered, *resource, desc))
        }
        _ => None,
    }
}

/// The format of `view`, a buffer view through which a shader moves
/// 16-bit channels as unsigned integers
/// (`victory_shader::emit::HostShader::half_views`), if those integers
/// are not what the console gives: a format of wider channels, a
/// normalised one or a signed 8-bit one, whose values the console converts
/// to and from halves. `None` for the formats the integers serve exactly
/// (16 bits a channel of any kind, through [`half_view`] where needed, and
/// unsigned channels of fewer bits; the title's are 16-bit unsigned) and
/// for any other view.
pub(crate) fn half_inexact(view: &View) -> Option<u32> {
    // Buffer views: dimension 1 in the title's description of either kind.
    const BUFFER: u32 = 1;
    // R16G16B16A16, R16G16 and R16 (float, unsigned, signed), then
    // R10G10B10A2, R8G8B8A8, R8G8 and R8 unsigned.
    const EXACT: [u32; 13] = [10, 12, 14, 34, 36, 38, 54, 57, 59, 25, 30, 50, 62];
    let format = match view {
        View::Shader(_, Some(desc)) | View::Unordered(_, _, Some(desc)) if desc[1] == BUFFER => {
            desc[0]
        }
        View::Placed(_, resource, desc) if resource[0] == BUFFER && desc[1] == BUFFER => desc[0],
        _ => return None,
    };
    (!EXACT.contains(&format)).then_some(format)
}

fn uav_desc(words: &[u32; 10]) -> D3D12_UNORDERED_ACCESS_VIEW_DESC {
    let mut words = *words;
    words[0] = crate::resource::pc_format(words[0]);
    // SAFETY: 40 bytes in the host's layout.
    unsafe {
        std::ptr::from_ref(&words)
            .cast::<D3D12_UNORDERED_ACCESS_VIEW_DESC>()
            .read_unaligned()
    }
}

/// A buffer view of title memory at `address` on its host copy: the first
/// element moves by the copy's offset.
/// Bytes per element of a buffer view.
fn element_size(desc: &D3D12_SHADER_RESOURCE_VIEW_DESC) -> u64 {
    // SAFETY: a buffer view's union member.
    let buffer = unsafe { desc.Anonymous.Buffer };
    if buffer.StructureByteStride > 0 {
        u64::from(buffer.StructureByteStride)
    } else if buffer.Flags.0 & D3D12_BUFFER_SRV_FLAG_RAW.0 != 0 {
        4
    } else {
        crate::resource::element(desc.Format.0 as u32).map_or(0, |(bytes, _)| bytes)
    }
}

/// The title memory a buffer shader or unordered access view reads: its
/// address and length.
pub(crate) fn buffer_range(view: &View) -> Option<(u64, u64)> {
    let (base, words): (u64, &[u32; 10]) = match view {
        View::Shader(r, Some(desc)) => (
            crate::resource::buffer_bytes(*r as crate::object::This)?.0 as u64,
            desc,
        ),
        View::Placed(false, resource, desc) if resource[0] == 1 => (
            u64::from(desc[10]) | u64::from(desc[11]) << 32,
            desc[..10].try_into().ok()?,
        ),
        View::Unordered(r, _, Some(desc)) => {
            let base = crate::resource::buffer_bytes(*r as crate::object::This)?.0 as u64;
            return unordered_range(base, desc);
        }
        View::Placed(true, resource, desc) if resource[0] == 1 => {
            let base = u64::from(desc[10]) | u64::from(desc[11]) << 32;
            return unordered_range(base, desc[..10].try_into().ok()?);
        }
        _ => return None,
    };
    let desc = srv_desc(words);
    let element = element_size(&desc);
    // SAFETY: a buffer view's union member.
    let buffer = unsafe { desc.Anonymous.Buffer };
    Some((
        base + buffer.FirstElement * element,
        u64::from(buffer.NumElements) * element,
    ))
}

/// The title memory the host slot of buffer view `view` is made on
/// (address, bytes): the whole buffer the view is of, which is what
/// [`make`] finds the host copy by. [`buffer_range`] is the part of it the
/// view reads.
pub(crate) fn buffer_extent(view: &View) -> Option<(u64, u64)> {
    match view {
        View::Shader(r, Some(_)) | View::Unordered(r, _, Some(_)) => {
            let (data, len) = crate::resource::buffer_bytes(*r as crate::object::This)?;
            Some((data as u64, len))
        }
        View::Placed(_, resource, desc) if resource[0] == 1 => Some((
            u64::from(desc[10]) | u64::from(desc[11]) << 32,
            u64::from(resource[4]) | u64::from(resource[5]) << 32,
        )),
        _ => None,
    }
}

/// Whether a buffer view is brought up to date in the host copy its slot
/// shows ([`buffer_copy`]). On by default; `VICTORY_VIEW_EXTENT=0` looks the
/// copy up by the view's range alone, as before.
fn view_extent() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_VIEW_EXTENT").is_none_or(|v| v != "0"))
}

/// The host copy buffer view `view` reads, and the title range (address,
/// bytes) it reads there; `None` for other views and empty ranges.
///
/// The copy is the one the view's slot shows: [`make`] finds it by the
/// whole buffer the view is of ([`buffer_extent`]). Looked up by the range
/// the view reads, as it was, the copy is another one wherever the two
/// lookups differ: for memory outside buffer objects, a view of a part of a
/// buffer that crosses a window line (the buffer's window is two cells, the
/// part's one), and for buffer objects, a part that also lies in a smaller
/// buffer object. The slot then showed a copy nothing brought up to date,
/// and what shaders wrote through it was looked for in the other copy.
pub(crate) fn buffer_copy(view: &View) -> Option<BufferCopy> {
    let (address, len) = buffer_range(view)?;
    if len == 0 {
        return None;
    }
    // A view off its buffer's elements is made on a copy of its own bytes
    // (`own_bytes`): the same question `buffer_srv` and `buffer_uav` ask,
    // of the copy they find, so that the copy brought up to date is the
    // one the slot shows.
    if let Some((base, width)) = buffer_extent(view)
        && let Some(whole) = crate::shadow::find(base, width)
    {
        if let Some((first, count, element, stride, unknown, raw)) = buffer_fields(view)
            && let Some((start, bytes)) = own_bytes(
                whole.offset(base),
                base,
                first,
                count,
                element,
                stride,
                unknown,
                raw,
            )
            && let Some(own) = crate::shadow::exact(start, bytes)
        {
            return Some((own, start, bytes));
        }
        // (The console is lax: a view may begin past its buffer's end.)
        if view_extent() && address >= base && address - base < width.max(1) {
            return Some((whole, address, len));
        }
    }
    crate::shadow::find(address, len).map(|shadow| (shadow, address, len))
}

/// [`buffer_range`] of an unordered access view of memory at `base`.
fn unordered_range(base: u64, words: &[u32; 10]) -> Option<(u64, u64)> {
    let desc = uav_desc(words);
    // SAFETY: a buffer view's union member.
    let buffer = unsafe { desc.Anonymous.Buffer };
    let element = if buffer.StructureByteStride > 0 {
        u64::from(buffer.StructureByteStride)
    } else if buffer.Flags.0 & D3D12_BUFFER_UAV_FLAG_RAW.0 != 0 {
        4
    } else {
        crate::resource::element(desc.Format.0 as u32)?.0
    };
    Some((
        base + buffer.FirstElement * element,
        u64::from(buffer.NumElements) * element,
    ))
}

/// The host resource a slot made by [`make`] views, if any.
type Kept = Option<ID3D12Resource>;

/// The host copy a buffer view reads, and the title range (address, bytes)
/// it reads there: what [`buffer_copy`] reports for it.
pub(crate) type BufferCopy = (std::sync::Arc<crate::shadow::Shadow>, u64, u64);

/// Whether the pass over records copied in as bytes also mirrors slots
/// that draws read through tables lately (`VICTORY_HEAP_SYNC_TABLES=1`;
/// off by default: [`HostHeap::mirror_copied`]).
fn sync_tables() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_HEAP_SYNC_TABLES").is_some_and(|v| v == "1"))
}

/// (Neptune) Since the last frame line, of the records [`HostHeap::mirror_copied`] mirrored:
/// by kind (0 no view of ours, 1 constant buffer, 2 texture, 3 buffer, 4 unordered texture,
/// 5 unordered buffer, 6 sampler, 7 placed, 8 placed unordered), then (9) those whose slot a
/// table read at some time, then by where in the heap (the slot's number in 65,536s).
pub(crate) static COPIED_CENSUS: [std::sync::atomic::AtomicU64; 32] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 32];

/// Since the last frame line: slots on changed heap pages that
/// [`HostHeap::mirror_copied`] left to their draws (tables' slots).
pub(crate) static COPIED_TABLED: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// A 64-bit fingerprint of a descriptor record's bytes (16 or 32 of them):
/// equal records give equal prints, and two different records the same
/// print about once in 2^64. Not a hash anyone can attack: the records
/// are our own.
fn record_is_zero(record: &[u8]) -> bool {
    let mut acc = 0u64;
    let mut words = record.chunks_exact(8);
    for word in &mut words {
        acc |= u64::from_le_bytes(word.try_into().unwrap_or([0; 8]));
    }
    if acc != 0 {
        return false;
    }
    words.remainder().iter().all(|&b| b == 0)
}

fn record_print(record: &[u8]) -> u64 {
    const K: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut h = K ^ record.len() as u64;
    let mut words = record.chunks_exact(8);
    for word in &mut words {
        let w = u64::from_le_bytes(word.try_into().unwrap_or([0; 8]));
        h = (h ^ w).wrapping_mul(K);
        h ^= h >> 29;
    }
    for &b in words.remainder() {
        h = (h ^ u64::from(b)).wrapping_mul(K);
    }
    h ^ (h >> 32)
}

/// Whether a buffer view off its buffer's elements gets a copy of its own
/// bytes (`VICTORY_VIEW_OWN_COPY=0`: it is refused and reads zeros, as
/// before).
fn own_copy_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_VIEW_OWN_COPY").is_none_or(|v| v != "0"))
}

/// The title memory (start, bytes) of a buffer view that cannot be made on
/// the host copy of its buffer: `count` elements of `element` bytes from
/// element `first` of the buffer at `base`, which lies `offset` bytes into
/// that copy, where `offset` is not a whole number of elements and the view
/// cannot be a raw view of the same bytes either ([`raw_window`]:
/// `unknown`, the view has no format, and `stride` is its structure's).
/// The host counts a view's first element from its buffer's start, so such
/// a view has no host view there; it is made on a copy of its own bytes
/// (`crate::shadow::exact`), where it starts at element 0. `None`: the
/// view is made on its buffer's copy, as every other.
#[allow(clippy::too_many_arguments)]
fn own_bytes(
    offset: u64,
    base: u64,
    first: u64,
    count: u32,
    element: u64,
    stride: u32,
    unknown: bool,
    raw: bool,
) -> Option<(u64, u64)> {
    own_bytes_if(
        own_copy_on(),
        offset,
        base,
        first,
        count,
        element,
        stride,
        unknown,
        raw,
    )
}

#[allow(clippy::too_many_arguments)]
fn own_bytes_if(
    on: bool,
    offset: u64,
    base: u64,
    first: u64,
    count: u32,
    element: u64,
    stride: u32,
    unknown: bool,
    raw: bool,
) -> Option<(u64, u64)> {
    if !on || element == 0 || count == 0 {
        return None;
    }
    if raw && stride == 0 {
        // Raw views are counted in dwords (4 bytes), but D3D12 requires the
        // start in the host buffer (`offset + first * 4`) to be 16-byte
        // aligned (`first.is_multiple_of(4)` in `buffer_view_ok`).
        if first
            .checked_mul(element)
            .and_then(|at| offset.checked_add(at))
            .is_some_and(|at| at.is_multiple_of(16))
        {
            return None;
        }
    } else if offset.is_multiple_of(element) {
        return None;
    }
    if unknown && raw_window(offset, first, count, stride).is_some() {
        return None;
    }
    let start = base.checked_add(first.checked_mul(element)?)?;
    Some((start, u64::from(count) * element))
}

/// Of a buffer view: its first element, its elements, the bytes of one (a
/// raw view's: 4), its structure's stride, whether it has no format, and
/// whether it is a raw view.
/// As [`buffer_srv`] and [`buffer_uav`] read them from the same words.
fn buffer_fields(view: &View) -> Option<(u64, u32, u64, u32, bool, bool)> {
    const BUFFER: u32 = 1;
    let (words, unordered): (&[u32; 10], bool) = match view {
        View::Shader(_, Some(d)) if d[1] == BUFFER => (d, false),
        View::Unordered(_, _, Some(d)) if d[1] == BUFFER => (d, true),
        View::Placed(unordered, resource, d) if resource[0] == BUFFER => {
            (d[..10].try_into().ok()?, *unordered)
        }
        _ => return None,
    };
    if unordered {
        let d = uav_desc(words);
        // SAFETY: a buffer view's union member.
        let b = unsafe { d.Anonymous.Buffer };
        let raw = b.Flags.0 & D3D12_BUFFER_UAV_FLAG_RAW.0 != 0;
        let element = if b.StructureByteStride > 0 {
            u64::from(b.StructureByteStride)
        } else if raw {
            4
        } else {
            crate::resource::element(d.Format.0 as u32).map_or(0, |(bytes, _)| bytes)
        };
        Some((
            b.FirstElement,
            b.NumElements,
            element,
            b.StructureByteStride,
            d.Format == DXGI_FORMAT_UNKNOWN,
            raw,
        ))
    } else {
        let d = srv_desc(words);
        // SAFETY: a buffer view's union member.
        let b = unsafe { d.Anonymous.Buffer };
        let raw = b.Flags.0 & D3D12_BUFFER_SRV_FLAG_RAW.0 != 0;
        Some((
            b.FirstElement,
            b.NumElements,
            element_size(&d),
            b.StructureByteStride,
            d.Format == DXGI_FORMAT_UNKNOWN,
            raw,
        ))
    }
}

/// A structured buffer view (`first`, `count` elements of `stride` bytes,
/// `offset` bytes into its host copy) as a raw view of the same bytes:
/// its first dword and its dwords. Translated shaders read untyped buffers
/// as raw bytes at `element * stride + offset` (the stride from the
/// used-resource entry, or from the table by heap slot for a buffer picked
/// by index), and a raw read through a structured view gives zeros on
/// NVIDIA (reported upstream with PR #193; structured reads through raw
/// views were right there). So the host's view is raw whenever it can be:
/// the bytes must start on 16 (the host's raw view alignment) and be whole
/// dwords. A view that cannot stays structured.
fn raw_window(offset: u64, first: u64, count: u32, stride: u32) -> Option<(u64, u32)> {
    if stride == 0 {
        return None;
    }
    let start = first.checked_mul(u64::from(stride))?.checked_add(offset)?;
    let bytes = u64::from(count) * u64::from(stride);
    (start % 16 == 0 && bytes % 4 == 0)
        .then(|| {
            u32::try_from(bytes / 4)
                .ok()
                .map(|dwords| (start / 4, dwords))
        })
        .flatten()
}

/// Whether the host accepts a buffer view with these fields of a buffer of
/// `width` bytes. It removes the device for one it does not (view creation
/// returns nothing), and the console is laxer: the title's heap holds
/// views past their memory's end, of no elements, of typeless formats
/// without the raw flag, with strides the host has no structured buffers
/// for. Since every buffer view of the heap is made for shaders that pick
/// them by index, one such view ended the run (2026-11-24, run 10).
fn buffer_view_ok(
    format: DXGI_FORMAT,
    first: u64,
    count: u32,
    stride: u32,
    raw: bool,
    width: u64,
) -> bool {
    use crate::resource::family;
    let f = format.0 as u32;
    let element = if raw {
        // raw: R32 typeless, counted in dwords, starting on 16 bytes
        if format != DXGI_FORMAT_R32_TYPELESS || stride != 0 || !first.is_multiple_of(4) {
            return false;
        }
        4
    } else if stride > 0 {
        // structured: no format, a stride the host takes
        if format != DXGI_FORMAT_UNKNOWN || stride > 2048 || !stride.is_multiple_of(4) {
            return false;
        }
        u64::from(stride)
    } else {
        // typed: a format with a size that is not typeless
        let typeless = family(f) == f && family(f + 1) == f;
        match crate::resource::element(f) {
            Some((bytes, 1)) if f != 0 && !typeless && bytes > 0 => bytes,
            _ => return false,
        }
    };
    count > 0
        && first
            .checked_add(u64::from(count))
            .and_then(|end| end.checked_mul(element))
            .is_some_and(|end| end <= width)
}

/// The bytes of a constant buffer view of `size` bytes: whole 256s, and at
/// most the 64 KiB a shader can index. The host removes the device for a
/// larger one, and the title's heap holds old views of any size in slots it
/// does not use.
fn constant_size(size: u32) -> u32 {
    size.min(64 << 10).next_multiple_of(256)
}

/// How many of `count` elements of `element` bytes from `first` on lie in
/// `width` bytes: the console lets a view reach past its buffer's end, the
/// host refuses one. A view with no element inside keeps its count (and is
/// refused).
fn elements_inside(first: u64, count: u32, element: u64, width: u64) -> u32 {
    let room = (width / element.max(1)).saturating_sub(first);
    match u32::try_from(room) {
        Ok(0) | Err(_) => count,
        Ok(room) => count.min(room),
    }
}

/// Diagnostics: a buffer view left null for shaders that pick views by
/// index, and why. Told at each doubling of the distinct views of a kind.
fn left_null(why: &'static str, unordered: bool, address: u64, len: u64, element: u64, first: u64) {
    if !cfg!(feature = "diagnostics") {
        return;
    }
    type Seen = std::collections::BTreeMap<
        (&'static str, bool, u64),
        std::collections::HashSet<(u64, u64)>,
    >;
    static SEEN: Mutex<Seen> = Mutex::new(Seen::new());
    let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    let views = seen.entry((why, unordered, element)).or_default();
    if views.insert((address, len)) && views.len().is_power_of_two() {
        tracing::warn!(
            views = views.len(),
            unordered,
            element,
            first,
            len,
            address = format_args!("{address:#x}"),
            "buffer views left null: {why}"
        );
    }
}

fn buffer_srv(
    address: u64,
    len: u64,
    mut desc: D3D12_SHADER_RESOURCE_VIEW_DESC,
    at: D3D12_CPU_DESCRIPTOR_HANDLE,
    buffer_view: bool,
) -> Option<(Kept, Option<BufferCopy>)> {
    let whole = crate::shadow::find(address, len)?;
    let offset = whole.offset(address);
    // The title range the view reads ([`buffer_range`]): what `buffer_copy`
    // reports the copy of a made slot with.
    // SAFETY: a buffer view's union member.
    let range = {
        let b = unsafe { desc.Anonymous.Buffer };
        let element = element_size(&desc);
        (
            address + b.FirstElement * element,
            u64::from(b.NumElements) * element,
        )
    };
    // Off the elements of that copy: on a copy of its own bytes, from its
    // element 0 (`own_bytes`).
    let own = buffer_view.then_some(()).and_then(|()| {
        // SAFETY: a buffer view's union member.
        let b = unsafe { desc.Anonymous.Buffer };
        let unknown = desc.Format == DXGI_FORMAT_UNKNOWN;
        let raw = b.Flags.0 & D3D12_BUFFER_SRV_FLAG_RAW.0 != 0;
        own_bytes(
            offset,
            address,
            b.FirstElement,
            b.NumElements,
            element_size(&desc),
            b.StructureByteStride,
            unknown,
            raw,
        )
        .and_then(|(start, bytes)| {
            crate::shadow::exact(start, bytes).map(|own| (own, start, bytes))
        })
    });
    let (shadow, offset, own_copy) = match own {
        Some((own, start, bytes)) => {
            desc.Anonymous.Buffer.FirstElement = 0;
            (own, 0, Some((start, bytes)))
        }
        None => (whole, offset, None),
    };
    // A structured view as a raw view of the same bytes (see `raw_window`).
    // SAFETY: a buffer view's union member.
    let structured = unsafe { desc.Anonymous.Buffer };
    let window = (desc.Format == DXGI_FORMAT_UNKNOWN)
        .then(|| {
            raw_window(
                offset,
                structured.FirstElement,
                structured.NumElements,
                structured.StructureByteStride,
            )
        })
        .flatten();
    if let Some((first, count)) = window {
        desc.Format = DXGI_FORMAT_R32_TYPELESS;
        desc.Anonymous.Buffer = D3D12_BUFFER_SRV {
            FirstElement: first,
            NumElements: count,
            StructureByteStride: 0,
            Flags: D3D12_BUFFER_SRV_FLAG_RAW,
        };
    }
    let element = element_size(&desc);
    // SAFETY: a buffer view's union member.
    let buffer = unsafe { &mut desc.Anonymous.Buffer };
    if window.is_none() {
        if element == 0 || offset % element != 0 {
            left_null(
                "not on an element of their host copy",
                false,
                address,
                len,
                element,
                buffer.FirstElement,
            );
            return None;
        }
        buffer.FirstElement += offset / element;
    }
    // SAFETY: a live buffer.
    let width = unsafe { shadow.buffer.GetDesc() }.Width;
    let unit = if window.is_some() { 4 } else { element };
    buffer.NumElements = elements_inside(buffer.FirstElement, buffer.NumElements, unit, width);
    let (first, count, stride) = (
        buffer.FirstElement,
        buffer.NumElements,
        buffer.StructureByteStride,
    );
    let raw = buffer.Flags.0 & D3D12_BUFFER_SRV_FLAG_RAW.0 != 0;
    if !buffer_view_ok(desc.Format, first, count, stride, raw, width) {
        REFUSED_VIEWS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let why = if raw {
            "raw, refused by the host"
        } else {
            "refused by the host"
        };
        left_null(why, false, address, len, element, first);
        once!(
            format = desc.Format.0,
            first,
            count,
            stride,
            raw,
            width,
            "buffer view the host would refuse left null"
        );
        return None;
    }
    // The console has no component mapping; one the host does not know is the default's.
    if desc.Shader4ComponentMapping != D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING {
        desc.Shader4ComponentMapping = D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING;
    }
    // The copy this slot now shows, as `buffer_copy` reports it: a copy of
    // the view's own bytes, else the whole buffer's, which the view lies in
    // (`VICTORY_VIEW_EXTENT=0` looks copies up by the view's range alone).
    let copy = match own_copy {
        Some((start, bytes)) => Some((shadow.clone(), start, bytes)),
        None if view_extent()
            && range.1 > 0
            && range.0 >= address
            && range.0 - address < len.max(1) =>
        {
            Some((shadow.clone(), range.0, range.1))
        }
        None => None,
    };
    let host = host()?;
    // SAFETY: a live buffer and a slot of our heap.
    unsafe {
        host.device
            .CreateShaderResourceView(&shadow.buffer, Some(&desc), at)
    };
    Some((Some(shadow.buffer.clone()), copy))
}

/// An unordered access view of title buffer memory at `address` (`len`
/// bytes) on its host copy, as [`buffer_srv`]; false if the copy cannot be
/// written by shaders (upload memory).
/// shortcut: what shaders write stays in the host copy; title memory (and
/// so the CPU, and copies run on the CPU) does not see it.
fn buffer_uav(
    address: u64,
    len: u64,
    mut desc: D3D12_UNORDERED_ACCESS_VIEW_DESC,
    at: D3D12_CPU_DESCRIPTOR_HANDLE,
    buffer_view: bool,
) -> Option<(Kept, Option<BufferCopy>)> {
    let whole = crate::shadow::find(address, len)?;
    // The title range the view reads ([`buffer_range`]: `None` where it finds
    // none): what `buffer_copy` reports the copy of a made slot with.
    let range = {
        // SAFETY: a buffer view's union member.
        let b = unsafe { desc.Anonymous.Buffer };
        let element = if b.StructureByteStride > 0 {
            Some(u64::from(b.StructureByteStride))
        } else if b.Flags.0 & D3D12_BUFFER_UAV_FLAG_RAW.0 != 0 {
            Some(4)
        } else {
            crate::resource::element(desc.Format.0 as u32).map(|(bytes, _)| bytes)
        };
        element.map(|element| {
            (
                address + b.FirstElement * element,
                u64::from(b.NumElements) * element,
            )
        })
    };
    // Off the elements of that copy: on a copy of its own bytes, from its
    // element 0 (`own_bytes`).
    let own = buffer_view.then_some(()).and_then(|()| {
        // SAFETY: a buffer view's union member.
        let b = unsafe { desc.Anonymous.Buffer };
        let raw = b.Flags.0 & D3D12_BUFFER_UAV_FLAG_RAW.0 != 0;
        let element = if b.StructureByteStride > 0 {
            u64::from(b.StructureByteStride)
        } else if raw {
            4
        } else {
            crate::resource::element(desc.Format.0 as u32).map_or(0, |(bytes, _)| bytes)
        };
        let unknown = desc.Format == DXGI_FORMAT_UNKNOWN;
        own_bytes(
            whole.offset(address),
            address,
            b.FirstElement,
            b.NumElements,
            element,
            b.StructureByteStride,
            unknown,
            raw,
        )
        .and_then(|(start, bytes)| {
            crate::shadow::exact(start, bytes).map(|own| (own, start, bytes))
        })
    });
    let (shadow, own_copy) = match own {
        Some((own, start, bytes)) => {
            desc.Anonymous.Buffer.FirstElement = 0;
            (own, Some((start, bytes)))
        }
        None => (whole, None),
    };
    let own = own_copy.is_some();
    // SAFETY: a live buffer.
    let flags = unsafe { shadow.buffer.GetDesc() }.Flags;
    if flags.0 & D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS.0 == 0 {
        once!("unordered access to a buffer whose host copy shaders cannot write");
        return None;
    }
    // SAFETY: a buffer view's union member.
    let buffer = unsafe { &mut desc.Anonymous.Buffer };
    let element = if buffer.StructureByteStride > 0 {
        u64::from(buffer.StructureByteStride)
    } else if buffer.Flags.0 & D3D12_BUFFER_UAV_FLAG_RAW.0 != 0 {
        4
    } else {
        crate::resource::element(desc.Format.0 as u32).map_or(0, |(bytes, _)| bytes)
    };
    let offset = if own { 0 } else { shadow.offset(address) };
    // A structured view as a raw view of the same bytes (see `raw_window`).
    let window = (desc.Format == DXGI_FORMAT_UNKNOWN)
        .then(|| {
            raw_window(
                offset,
                buffer.FirstElement,
                buffer.NumElements,
                buffer.StructureByteStride,
            )
        })
        .flatten();
    if let Some((first, count)) = window {
        buffer.FirstElement = first;
        buffer.NumElements = count;
        buffer.StructureByteStride = 0;
        buffer.Flags = D3D12_BUFFER_UAV_FLAG_RAW;
    } else {
        if element == 0 || offset % element != 0 {
            left_null(
                "not on an element of their host copy",
                true,
                address,
                len,
                element,
                buffer.FirstElement,
            );
            return None;
        }
        buffer.FirstElement += offset / element;
    }
    buffer.CounterOffsetInBytes = 0;
    // SAFETY: a live buffer.
    let width = unsafe { shadow.buffer.GetDesc() }.Width;
    let unit = if window.is_some() { 4 } else { element };
    buffer.NumElements = elements_inside(buffer.FirstElement, buffer.NumElements, unit, width);
    let (first, count, stride) = (
        buffer.FirstElement,
        buffer.NumElements,
        buffer.StructureByteStride,
    );
    let raw = buffer.Flags.0 & D3D12_BUFFER_UAV_FLAG_RAW.0 != 0;
    if window.is_some() {
        // (Left out at first: every structured unordered access view was then
        // refused by `buffer_view_ok` and left null, runs 15 and 16.)
        desc.Format = DXGI_FORMAT_R32_TYPELESS;
    }
    if !buffer_view_ok(desc.Format, first, count, stride, raw, width) {
        REFUSED_VIEWS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let why = if raw {
            "raw, refused by the host"
        } else {
            "refused by the host"
        };
        left_null(why, true, address, len, element, first);
        once!(
            format = desc.Format.0,
            first,
            count,
            stride,
            raw,
            width,
            "unordered buffer view the host would refuse left null"
        );
        return None;
    }
    let host = host()?;
    // SAFETY: a live buffer and a slot of our heap.
    unsafe {
        host.device.CreateUnorderedAccessView(
            &shadow.buffer,
            None::<&ID3D12Resource>,
            Some(&desc),
            at,
        )
    };
    // The copy this slot now shows, as `buffer_copy` reports it: a copy of
    // the view's own bytes, else the whole buffer's, which the view lies in
    // (`VICTORY_VIEW_EXTENT=0` looks copies up by the view's range alone).
    let copy = match (own_copy, range) {
        (Some((start, bytes)), _) => Some((shadow.clone(), start, bytes)),
        (None, Some((from, bytes)))
            if view_extent() && bytes > 0 && from >= address && from - address < len.max(1) =>
        {
            Some((shadow.clone(), from, bytes))
        }
        _ => None,
    };
    Some((Some(shadow.buffer.clone()), copy))
}

/// Whether the host can make a shader (or unordered access) view of
/// `texture` in `format` (none: the texture's own). The console allows any
/// view; the host refuses (and removes the device for) views without the
/// resource's permission, of typed depth, and in another format than a
/// typed resource's. A view of a typed texture in another format of its
/// family takes the texture's format.
/// shortcut: such a view (sRGB of UNORM, say) reads without its
/// conversion; cast with castable formats if colors come out wrong.
fn fits(texture: &ID3D12Resource, mut format: Option<&mut DXGI_FORMAT>, unordered: bool) -> bool {
    use crate::resource::{DEPTH_FORMATS, family};
    // SAFETY: a live resource.
    let d = unsafe { texture.GetDesc() };
    let allowed = if unordered {
        d.Flags.0 & D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS.0 != 0
    } else {
        d.Flags.0 & D3D12_RESOURCE_FLAG_DENY_SHADER_RESOURCE.0 == 0
    };
    let own = d.Format.0 as u32;
    if !allowed || DEPTH_FORMATS.iter().any(|&(typed, _)| typed == own) {
        if unordered {
            crate::resource::DEPTH_UNORDERED[3].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        once!(
            unordered,
            format = own,
            flags = d.Flags.0,
            "view the host does not allow"
        );
        return false;
    }
    // Every family has more than one format, its typeless one first.
    let typeless = family(own) == own && family(own + 1) == own;
    if let Some(format) = format.as_deref_mut()
        && unordered
        && let Some(&(_, linear)) = crate::resource::SRGB
            .iter()
            .find(|&&(s, _)| s == format.0 as u32)
    {
        format.0 = linear as i32;
    }
    let view = format.as_ref().map_or(0, |f| f.0 as u32);
    if view == 0 {
        return !typeless;
    }
    // A view of a target whose format the host refuses, in the title's
    // format: in the stand-in the host copy has.
    if view != own && crate::resource::target_format(view) == own {
        if let Some(format) = format {
            *format = d.Format;
        }
        return true;
    }
    if family(view) != family(own) {
        once!(view, own, "view format of another family");
        return false;
    }
    if let Some(format) = format
        && !typeless
    {
        *format = d.Format;
    }
    true
}

/// The title's unordered access view of a depth texture, as a view of the
/// texture's colour twin `twin` ([`crate::resource::Twin`]): the same mip
/// and slices, in the colour format of the twin's family for a depth or
/// typeless format (R16_UNORM for 16-bit depth, R32_FLOAT for 32-bit; the
/// bits are the same). Without a description: mip 0 of every slice.
/// The twin of a stencil plane (R8) stands for the views of that plane
/// only, which become R8_UINT views of its one plane; `None` for a view of
/// the depth plane, which the twin does not stand for. The depth plane has
/// a twin of its own, viewed as a depth texture's without stencil: a view
/// in the format of the depth plane of a depth-stencil format (21, 46)
/// takes the twin's colour format too.
fn twin_uav(
    twin: &ID3D12Resource,
    desc: Option<&D3D12_UNORDERED_ACCESS_VIEW_DESC>,
) -> Option<D3D12_UNORDERED_ACCESS_VIEW_DESC> {
    // SAFETY: a live resource.
    let d = unsafe { twin.GetDesc() };
    let mut desc = desc.cloned().unwrap_or_else(|| {
        let mut all = D3D12_UNORDERED_ACCESS_VIEW_DESC {
            ViewDimension: D3D12_UAV_DIMENSION_TEXTURE2D,
            ..Default::default()
        };
        if d.DepthOrArraySize > 1 {
            all.ViewDimension = D3D12_UAV_DIMENSION_TEXTURE2DARRAY;
            all.Anonymous.Texture2DArray = D3D12_TEX2D_ARRAY_UAV {
                ArraySize: u32::from(d.DepthOrArraySize),
                ..Default::default()
            };
        }
        all
    });
    // A stencil plane's twin (R8 typeless, 60): the title's view is of the
    // stencil plane if it names plane 1 or a stencil view format (22, 47).
    if d.Format.0 == 60 {
        let mut plane = 0;
        match desc.ViewDimension {
            D3D12_UAV_DIMENSION_TEXTURE2D => {
                // SAFETY: the member the dimension names.
                let mut view = unsafe { desc.Anonymous.Texture2D };
                plane = std::mem::take(&mut view.PlaneSlice);
                desc.Anonymous.Texture2D = view;
            }
            D3D12_UAV_DIMENSION_TEXTURE2DARRAY => {
                // SAFETY: the member the dimension names.
                let mut view = unsafe { desc.Anonymous.Texture2DArray };
                plane = std::mem::take(&mut view.PlaneSlice);
                desc.Anonymous.Texture2DArray = view;
            }
            _ => {}
        }
        if plane != 1 && !matches!(desc.Format.0, 22 | 47) {
            return None;
        }
        desc.Format.0 = 62;
        return Some(desc);
    }
    // None, typeless and depth formats (R16: 0, 53, 55; R32: 0, 39, 40),
    // and those of a depth-stencil format's depth plane (19 to 21, 44 to
    // 46), for the twin of such a plane.
    desc.Format.0 = match (desc.Format.0, d.Format.0) {
        (0 | 53 | 55 | 19..=21 | 44..=46, 53) => 56,
        (0 | 39 | 40 | 19..=21 | 44..=46, 39) => 41,
        (other, _) => other,
    };
    Some(desc)
}

/// Renderer port 5 (`crate::port_cbv`): makes a raw buffer view at `at`
/// of the constant buffer of `size` bytes at `gpu` of `buffer`.
fn constant_raw_view(
    buffer: &ID3D12Resource,
    gpu: u64,
    size: u32,
    at: D3D12_CPU_DESCRIPTOR_HANDLE,
) -> bool {
    let Some(host) = host() else { return false };
    // SAFETY: a live buffer.
    let (base, width) = unsafe { (buffer.GetGPUVirtualAddress(), buffer.GetDesc().Width) };
    let Some(offset) = gpu
        .checked_sub(base)
        .filter(|&o| o < width && crate::port_cbv::raw_ok(o))
    else {
        return false;
    };
    let bytes = u64::from(constant_size(size)).min(width - offset) & !3;
    if bytes == 0 {
        null_buffer_view(at);
        return true;
    }
    let raw = D3D12_SHADER_RESOURCE_VIEW_DESC {
        Format: DXGI_FORMAT_R32_TYPELESS,
        ViewDimension: D3D12_SRV_DIMENSION_BUFFER,
        Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
        Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
            Buffer: D3D12_BUFFER_SRV {
                FirstElement: offset / 4,
                NumElements: (bytes / 4) as u32,
                StructureByteStride: 0,
                Flags: D3D12_BUFFER_SRV_FLAG_RAW,
            },
        },
    };
    // SAFETY: a slot of our heap; the range lies in the live buffer.
    unsafe { host.device.CreateShaderResourceView(buffer, Some(&raw), at) };
    true
}

/// Makes the host descriptor for `view` at `at`: the host resource it
/// views, or `None` if it cannot. A buffer view made now also reports the
/// host copy it reads ([`BufferCopy`]), so that the pass that mirrors it
/// ([`HostHeap::mirror_heap_buffer`]) need not look the copy up again.
pub(crate) fn make(
    view: &View,
    at: D3D12_CPU_DESCRIPTOR_HANDLE,
) -> Option<(Kept, Option<BufferCopy>)> {
    let host = host()?;
    let device = &host.device;
    match view {
        &View::Constant(address, size) => {
            let shadow = crate::shadow::find(address, u64::from(size))?;
            let gpu = shadow.gpu(address);
            if crate::port_cbv::on() {
                if constant_raw_view(&shadow.buffer, gpu, size, at) {
                    crate::port_cbv::twinned();
                    return Some((Some(shadow.buffer.clone()), None));
                }
                return None;
            }
            if gpu % 256 != 0 {
                once!(
                    address = format_args!("{address:#x}"),
                    "constant buffer not 256-byte aligned on the host"
                );
                return None;
            }
            let desc = D3D12_CONSTANT_BUFFER_VIEW_DESC {
                BufferLocation: gpu,
                SizeInBytes: constant_size(size),
            };
            // SAFETY: a slot of our heap; the range lies in the host copy.
            unsafe { device.CreateConstantBufferView(Some(&desc), at) };
            Some((Some(shadow.buffer.clone()), None))
        }
        View::Shader(resource, desc) => {
            let resource = *resource as crate::object::This;
            if let Some(copy) = crate::resource::host_copy(resource) {
                let texture = &copy.texture;
                // The title's views of textures with a stencil plane, as it
                // describes them (format, dimension, component mapping, a
                // pad, then first level, levels, plane): each distinct one
                // once. They say how its shaders read the stencil plane
                // (the lighting takes each pixel's material class and its
                // "inside a room" bit from it).
                #[cfg(feature = "diagnostics")]
                {
                    // SAFETY: a live resource.
                    let own = unsafe { texture.GetDesc() }.Format.0;
                    if matches!(own, 19..=22 | 44..=47) {
                        static SEEN: Mutex<Vec<(i32, Option<[u32; 10]>)>> = Mutex::new(Vec::new());
                        let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
                        if seen.len() < 64 && !seen.contains(&(own, *desc)) {
                            seen.push((own, *desc));
                            tracing::info!(
                                ?resource,
                                texture_format = own,
                                view = ?desc,
                                "shader view of a depth-stencil texture"
                            );
                        }
                    }
                }
                let mut desc = desc.as_ref().map(srv_desc);
                if !fits(texture, desc.as_mut().map(|d| &mut d.Format), false) {
                    return None;
                }
                // Levels on demand (`crate::tiles`): a texture made level by
                // level is shown from the first level that is the title's
                // and has memory, if the title's view starts before it (a
                // level without memory reads zeros: a black texture). The
                // title's own view mostly starts there already.
                if let Some(first) = crate::resource::first_shown(resource)
                    && first > 0
                {
                    // SAFETY: a live resource.
                    let own = unsafe { texture.GetDesc() };
                    // The texture's own view (the title gave none): all of it.
                    let whole = D3D12_SHADER_RESOURCE_VIEW_DESC {
                        Format: own.Format,
                        ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2D,
                        Shader4ComponentMapping: 5768,
                        Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                            Texture2D: D3D12_TEX2D_SRV {
                                MostDetailedMip: 0,
                                MipLevels: u32::MAX,
                                PlaneSlice: 0,
                                ResourceMinLODClamp: 0.0,
                            },
                        },
                    };
                    crate::tiles::show_from(
                        desc.get_or_insert(whole),
                        first,
                        u32::from(own.MipLevels),
                    );
                }
                // Made whole from the store (`crate::texstore`): all its levels.
                let prefilled = copy.prefilled.load(std::sync::atomic::Ordering::Relaxed);
                if prefilled > 0
                    && let Some(desc) = &mut desc
                {
                    show_prefilled(desc, prefilled);
                }
                // SAFETY: a live texture and a slot of our heap.
                unsafe {
                    device.CreateShaderResourceView(
                        texture,
                        desc.as_ref().map(std::ptr::from_ref),
                        at,
                    )
                };
                Some((Some(texture.clone()), None))
            } else if let (Some((data, len)), Some(desc)) =
                (crate::resource::buffer_bytes(resource), desc)
            {
                // Buffer views: dimension 1 (`buffer_fields`).
                const BUFFER: u32 = 1;
                buffer_srv(data as u64, len, srv_desc(desc), at, desc[1] == BUFFER)
            } else {
                None
            }
        }
        View::Unordered(resource, _counter, desc) => {
            let resource = *resource as crate::object::This;
            let Some(texture) = crate::resource::host_resource(resource) else {
                return match (crate::resource::buffer_bytes(resource), desc) {
                    (Some((data, len)), Some(desc)) => {
                        // Buffer views: dimension 1 (`buffer_fields`).
                        const BUFFER: u32 = 1;
                        buffer_uav(data as u64, len, uav_desc(desc), at, desc[1] == BUFFER)
                    }
                    _ => None,
                };
            };
            // The first such views of depth textures are logged as the
            // title describes them (format, dimension, then mip, slices and
            // plane): they say which planes shaders write, and as what.
            if crate::resource::depth_and_unordered(resource) {
                static LOGGED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
                if LOGGED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 512 {
                    tracing::info!(
                        ?resource,
                        view = ?desc,
                        "unordered access view of a depth texture"
                    );
                }
            }
            let mut desc = desc.as_ref().map(uav_desc);
            // A depth texture's unordered access is its colour twin's: the
            // host gives a depth texture none.
            let twin = crate::resource::twin_resource(resource)
                .and_then(|twin| Some((twin, twin_uav(twin, desc.as_ref())?)))
                // A view of the depth plane of a texture with a stencil
                // plane: of that plane's own twin.
                .or_else(|| {
                    let twin = crate::resource::twin_plane_resource(resource)?;
                    Some((twin, twin_uav(twin, desc.as_ref())?))
                });
            let texture = match twin {
                Some((twin, view)) => {
                    desc = Some(view);
                    twin
                }
                None => texture,
            };
            if !fits(texture, desc.as_mut().map(|d| &mut d.Format), true) {
                return None;
            }
            // SAFETY: a live texture and a slot of our heap.
            unsafe {
                device.CreateUnorderedAccessView(
                    texture,
                    None::<&ID3D12Resource>,
                    desc.as_ref().map(std::ptr::from_ref),
                    at,
                )
            };
            Some((Some(texture.clone()), None))
        }
        View::Sampler(words) => {
            // SAFETY: 13 words in the host's layout.
            let desc = unsafe {
                std::ptr::from_ref(words)
                    .cast::<D3D12_SAMPLER_DESC>()
                    .read_unaligned()
            };
            // SAFETY: a slot of our sampler heap.
            unsafe { device.CreateSampler(&desc, at) };
            Some((None, None))
        }
        View::ConsoleSampler(words) => {
            let desc = crate::sampler::console(words, crate::sampler::aniso_point_mip(device));
            // SAFETY: a slot of our sampler heap.
            unsafe { device.CreateSampler(&desc, at) };
            Some((None, None))
        }
        View::Placed(unordered, resource, desc) => {
            if let Some(view) = placed_texture(view) {
                return make(&view, at);
            }
            const BUFFER: u32 = 1;
            if resource[0] != BUFFER {
                return None;
            }
            // The resource description's width, after the alignment.
            let len = u64::from(resource[4]) | u64::from(resource[5]) << 32;
            let address = u64::from(desc[10]) | u64::from(desc[11]) << 32;
            let words: &[u32; 10] = desc[..10].try_into().unwrap_or(&[0; 10]);
            // An empty title buffer: nothing to view, and no host copy to
            // look for (#70: such views were in flight at device removals).
            if len == 0 {
                return None;
            }
            if *unordered {
                buffer_uav(address, len, uav_desc(words), at, true)
            } else {
                buffer_srv(address, len, srv_desc(words), at, true)
            }
        }
    }
}

/// The view `desc` of a host copy whose levels below `levels` the store
/// filled (`crate::texstore`): from level 0, if it showed from one of them.
fn show_prefilled(desc: &mut D3D12_SHADER_RESOURCE_VIEW_DESC, levels: u32) {
    if desc.ViewDimension != D3D12_SRV_DIMENSION_TEXTURE2D {
        return;
    }
    // SAFETY: the member its dimension says.
    let t = unsafe { &mut desc.Anonymous.Texture2D };
    if t.MostDetailedMip == 0 || t.MostDetailedMip > levels {
        return;
    }
    if t.MipLevels != u32::MAX {
        t.MipLevels += t.MostDetailedMip;
    }
    t.MostDetailedMip = 0;
    t.ResourceMinLODClamp = 0.0;
}

/// Writes of a slot with another view of the placed texture it showed,
/// since the start: the slot kept showing the old view.
pub(crate) static KEPT_VIEWS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Whether a slot written with another view of the placed texture it
/// shows keeps showing the old one until the new is made;
/// `VICTORY_KEEP_VIEWS=0` makes it null at once, as before.
fn keep_views() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_KEEP_VIEWS").is_none_or(|v| v != "0"))
}

/// (Neptune) Writes of a slot with a view of another texture than the one it showed, since
/// the start: the slot kept showing the old texture until the new one was made.
pub(crate) static KEPT_OTHER_VIEWS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// (Neptune) Whether a slot written with a view of another texture keeps showing the one it
/// showed until the new one is made (`VICTORY_KEEP_OTHER_VIEWS=0`: null at once, as before).
fn keep_other_views() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_KEEP_OTHER_VIEWS").is_none_or(|v| v != "0"))
}

/// (Neptune) Whether the view is a shader's view of a texture: one the title placed in
/// memory, or a texture object.
fn shows_texture(view: &View) -> bool {
    const BUFFER: u32 = 1;
    match view {
        View::Placed(false, resource, _) => resource[0] != BUFFER,
        View::Shader(r, _) => crate::resource::buffer_bytes(*r as crate::object::This).is_none(),
        _ => false,
    }
}

/// Whether both are shader views of one texture the title placed in
/// memory: the same address and the same resource description, whatever
/// part of it each shows.
fn same_placed_texture(a: &View, b: &View) -> bool {
    match (a, b) {
        (View::Placed(false, resource, desc), View::Placed(false, other, other_desc)) => {
            resource[0] != 1 && resource == other && desc[10..12] == other_desc[10..12]
        }
        _ => false,
    }
}

/// A shader or unordered access view of a texture the title placed in
/// memory itself, as a view of the resource made for that memory (on first
/// use).
pub(crate) fn placed_texture(view: &View) -> Option<View> {
    const BUFFER: u32 = 1;
    const TARGET_DEPTH_UNORDERED: u32 = 0x7;
    let &View::Placed(unordered, mut resource, ref desc) = view else {
        return None;
    };
    if resource[0] == BUFFER {
        return None;
    }
    let address = u64::from(desc[10]) | u64::from(desc[11]) << 32;
    let words = desc[..10].try_into().ok();
    if unordered {
        let texture = crate::resource::uav_texture_at(address, &resource, desc[0])?;
        Some(View::Unordered(texture as usize, 0, words))
    } else {
        // Shader views placed in memory take their texels either from title
        // memory or from `TARGETS_AT` (`resource::alias_of`): clear target
        // flags the title left on the resource description so this view
        // never replaces that target in `TARGETS_AT`.
        resource[12] &= !TARGET_DEPTH_UNORDERED;
        let texture = crate::resource::texture_at(address, &resource)?;
        Some(View::Shader(texture as usize, words))
    }
}

/// With `VICTORY_NULL_MAGENTA=1`, a 2D colour texture view that shows
/// nothing yet shows magenta: on the screen, a texture that is not there is
/// told from one whose contents are black. False if it is to be null.
#[cfg(feature = "diagnostics")]
fn magenta_for(desc: &[u32; 16], at: D3D12_CPU_DESCRIPTOR_HANDLE) -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if !*ON.get_or_init(|| std::env::var_os("VICTORY_NULL_MAGENTA").is_some_and(|v| v == "1")) {
        return false;
    }
    // Block-compressed colour (BC1 to BC3, BC7) and 8-bit RGBA and BGRA.
    let colour = matches!(desc[0], 27..=29 | 70..=78 | 87..=91 | 97..=99);
    if desc[1] != D3D12_SRV_DIMENSION_TEXTURE2D.0 as u32 || !colour {
        return false;
    }
    let (Some(host), Some(texture)) = (host(), magenta()) else {
        return false;
    };
    // SAFETY: a slot of our heap and a live texture.
    unsafe { host.device.CreateShaderResourceView(texture, None, at) };
    true
}

/// One magenta texel, written by the processor.
#[cfg(feature = "diagnostics")]
fn magenta() -> Option<&'static ID3D12Resource> {
    static TEXTURE: std::sync::OnceLock<Option<ID3D12Resource>> = std::sync::OnceLock::new();
    TEXTURE
        .get_or_init(|| {
            let heap = D3D12_HEAP_PROPERTIES {
                Type: D3D12_HEAP_TYPE_CUSTOM,
                CPUPageProperty: D3D12_CPU_PAGE_PROPERTY_WRITE_BACK,
                MemoryPoolPreference: D3D12_MEMORY_POOL_L0,
                ..Default::default()
            };
            let desc = D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
                Width: 1,
                Height: 1,
                DepthOrArraySize: 1,
                MipLevels: 1,
                Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                ..Default::default()
            };
            let mut texture: Option<ID3D12Resource> = None;
            // SAFETY: valid descriptions and an out pointer; the texel is
            // written while nothing reads the texture.
            unsafe {
                host()?
                    .device
                    .CreateCommittedResource(
                        &heap,
                        D3D12_HEAP_FLAG_NONE,
                        &desc,
                        D3D12_RESOURCE_STATE_COMMON,
                        None,
                        &mut texture,
                    )
                    .ok()?;
                let texture = texture?;
                texture.Map(0, None, None).ok()?;
                let texel = [255u8, 0, 255, 255];
                texture
                    .WriteToSubresource(0, None, texel.as_ptr().cast(), 4, 4)
                    .ok()?;
                texture.Unmap(0, None);
                Some(texture)
            }
        })
        .as_ref()
}

/// A null view of the kind `view` is, for a slot whose view the host
/// cannot make: unordered access (to a buffer or a 2D texture) or a shader
/// view. Writes to it are dropped, reads give zeros.
fn null_for(view: &View, at: D3D12_CPU_DESCRIPTOR_HANDLE) {
    let Some(host) = host() else { return };
    let is_buffer = |r: &usize| crate::resource::buffer_bytes(*r as crate::object::This).is_some();
    // Unordered views: whether a buffer, and the title's view dimension.
    let (buffer, dimension) = match view {
        View::Unordered(r, _, desc) => (
            is_buffer(r) || desc.is_some_and(|d| d[1] == 1),
            desc.map(|d| d[1]),
        ),
        View::Placed(true, resource, desc) => (resource[0] == 1, Some(desc[1])),
        // Shader views of buffers read nothing through a null buffer view:
        // a null 2D view where the shader expects a buffer is a mismatch
        // the hardware does not survive in general (GPU-based validation
        // reports it).
        View::Shader(r, desc) if is_buffer(r) || desc.is_some_and(|d| d[1] == 1) => {
            return null_buffer_view(at);
        }
        View::Placed(false, resource, _) if resource[0] == 1 => return null_buffer_view(at),
        // Textures: a null view of the title's dimension (1D, arrays, 3D,
        // cubes), not always 2D.
        View::Shader(_, Some(desc)) => return null_like(desc, at),
        View::Placed(false, _, desc) => {
            #[cfg(feature = "diagnostics")]
            if magenta_for(desc, at) {
                return;
            }
            return null_like(desc[..10].try_into().unwrap_or(&[0; 10]), at);
        }
        View::Constant(..) => {
            if crate::port_cbv::on() {
                return null_buffer_view(at);
            }
            let desc = D3D12_CONSTANT_BUFFER_VIEW_DESC::default();
            // SAFETY: a slot of our heap; a zero location makes a null view.
            return unsafe { host.device.CreateConstantBufferView(Some(&desc), at) };
        }
        _ => return null_view(at),
    };
    let desc = if buffer {
        D3D12_UNORDERED_ACCESS_VIEW_DESC {
            Format: DXGI_FORMAT_R32_UINT,
            ViewDimension: D3D12_UAV_DIMENSION_BUFFER,
            ..Default::default()
        }
    } else {
        null_uav_desc(dimension)
    };
    // SAFETY: a slot of our heap; a null resource is allowed with a desc.
    unsafe {
        host.device.CreateUnorderedAccessView(
            None::<&ID3D12Resource>,
            None::<&ID3D12Resource>,
            Some(&desc),
            at,
        )
    };
}

/// A null texture UAV of the title's view dimension (1D, 1D or 2D arrays,
/// 3D), 2D if it names none: a 2D null view where the shader writes a 3D
/// texture is a mismatch GPU-based validation reports (the world's wind
/// field, CS_CopyWindFieldBeforeUpdateWithSmooth, 2026-11-24).
fn null_uav_desc(dimension: Option<u32>) -> D3D12_UNORDERED_ACCESS_VIEW_DESC {
    let mut desc = D3D12_UNORDERED_ACCESS_VIEW_DESC {
        Format: DXGI_FORMAT_R32_UINT,
        ViewDimension: D3D12_UAV_DIMENSION_TEXTURE2D,
        ..Default::default()
    };
    match dimension.map(|d| D3D12_UAV_DIMENSION(d as i32)) {
        Some(D3D12_UAV_DIMENSION_TEXTURE1D) => {
            desc.ViewDimension = D3D12_UAV_DIMENSION_TEXTURE1D;
        }
        Some(D3D12_UAV_DIMENSION_TEXTURE1DARRAY) => {
            desc.ViewDimension = D3D12_UAV_DIMENSION_TEXTURE1DARRAY;
            desc.Anonymous.Texture1DArray = D3D12_TEX1D_ARRAY_UAV {
                ArraySize: 1,
                ..Default::default()
            };
        }
        Some(D3D12_UAV_DIMENSION_TEXTURE2DARRAY) => {
            desc.ViewDimension = D3D12_UAV_DIMENSION_TEXTURE2DARRAY;
            desc.Anonymous.Texture2DArray = D3D12_TEX2D_ARRAY_UAV {
                ArraySize: 1,
                ..Default::default()
            };
        }
        Some(D3D12_UAV_DIMENSION_TEXTURE3D) => {
            desc.ViewDimension = D3D12_UAV_DIMENSION_TEXTURE3D;
            desc.Anonymous.Texture3D = D3D12_TEX3D_UAV {
                WSize: u32::MAX,
                ..Default::default()
            };
        }
        _ => {}
    }
    desc
}

/// A buffer view that reads zeros (raw, as translated shaders mostly read
/// buffers).
fn null_buffer_view(at: D3D12_CPU_DESCRIPTOR_HANDLE) {
    let Some(host) = host() else { return };
    let desc = D3D12_SHADER_RESOURCE_VIEW_DESC {
        Format: DXGI_FORMAT_R32_TYPELESS,
        ViewDimension: D3D12_SRV_DIMENSION_BUFFER,
        Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
        Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
            Buffer: D3D12_BUFFER_SRV {
                NumElements: 1,
                Flags: D3D12_BUFFER_SRV_FLAG_RAW,
                ..Default::default()
            },
        },
    };
    // SAFETY: a slot of our heap; a null resource is allowed with a desc.
    unsafe {
        host.device
            .CreateShaderResourceView(None::<&ID3D12Resource>, Some(&desc), at)
    };
}

/// A texture view that reads zeros with the dimension (and mip and array
/// counts) of the title's description `words`; 2D if it names none.
fn null_like(words: &[u32; 10], at: D3D12_CPU_DESCRIPTOR_HANDLE) {
    const TEXTURES: std::ops::RangeInclusive<u32> = 2..=10;
    if !TEXTURES.contains(&words[1]) {
        return null_view(at);
    }
    let Some(host) = host() else { return };
    // One mip, one slice or cube, plane 0: fields a null view still checks.
    let mut w = [0u32; 10];
    w[1] = words[1];
    match words[1] {
        6 => {}
        7 => w[5] = 1,
        3 | 5 | 10 => (w[5], w[7]) = (1, 1),
        _ => w[5] = 1,
    }
    let mut desc = srv_desc(&w);
    desc.Format = DXGI_FORMAT_R8G8B8A8_UNORM;
    desc.Shader4ComponentMapping = D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING;
    // SAFETY: a slot of our heap; a null resource is allowed with a desc.
    unsafe {
        host.device
            .CreateShaderResourceView(None::<&ID3D12Resource>, Some(&desc), at)
    };
}

/// A view that reads zeros, for slots whose view the host cannot make.
/// shortcut: slots with no view at all (kind unknown) get this null 2D
/// view; null buffer views there did not stop the world's low-address
/// page faults (2026-11-24, run w10), so they are not the cause.
fn null_view(at: D3D12_CPU_DESCRIPTOR_HANDLE) {
    let Some(host) = host() else { return };
    let desc = D3D12_SHADER_RESOURCE_VIEW_DESC {
        Format: DXGI_FORMAT_R8G8B8A8_UNORM,
        ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2D,
        Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
        Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
            Texture2D: D3D12_TEX2D_SRV {
                MipLevels: 1,
                ..Default::default()
            },
        },
    };
    // SAFETY: a slot of our heap; a null resource is allowed with a desc.
    unsafe {
        host.device
            .CreateShaderResourceView(None::<&ID3D12Resource>, Some(&desc), at)
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The set of changed buffer slots is emptied, not replaced: it keeps
    /// its allocation from one heap pass to the next.
    #[test]
    fn dirty_descriptor_drain_retains_capacity() {
        let Some(heap) = HostHeap::new(false, 4) else {
            return; // no host GPU
        };
        let capacity = {
            let mut changed = heap.changed.lock().unwrap();
            changed.extend([1, 2, 3]);
            changed.capacity()
        };
        let mut first = heap.buffer_changes();
        first.sort_unstable();
        assert_eq!(first, vec![1, 2, 3]);
        assert!(heap.buffer_changes().is_empty());
        assert_eq!(heap.changed.lock().unwrap().capacity(), capacity);
        heap.changed.lock().unwrap().insert(2);
        assert_eq!(heap.buffer_changes(), vec![2]);
    }

    /// The unsigned twin of a 16-bit float or signed format has its layout,
    /// and only buffer views in such a format are shown as another view.
    #[test]
    fn only_16_bit_float_and_signed_buffer_views_get_an_unsigned_twin() {
        for (format, unsigned) in [(10, 12), (14, 12), (34, 36), (38, 36), (54, 57), (59, 57)] {
            assert_eq!(half_format(format), Some(unsigned));
            assert_eq!(
                crate::resource::element(format),
                crate::resource::element(unsigned),
                "format {format}"
            );
        }
        // The unsigned formats themselves, normalised 16-bit formats, and
        // wider and narrower ones.
        for other in [0, 12, 36, 57, 11, 13, 35, 37, 56, 58, 42, 17, 30, 61] {
            assert_eq!(half_format(other), None, "format {other}");
        }
        const BUFFER: u32 = 1;
        let words = |format: u32, dimension: u32| [format, dimension, 5768, 0, 0, 0, 64, 0, 0, 0];
        // R16G16B16A16_FLOAT: a buffer of halves a program moves as they are.
        assert_eq!(
            half_view(&View::Unordered(7, 0, Some(words(10, BUFFER)))),
            Some(View::Unordered(7, 0, Some(words(12, BUFFER))))
        );
        assert_eq!(
            half_view(&View::Shader(7, Some(words(54, BUFFER)))),
            Some(View::Shader(7, Some(words(57, BUFFER))))
        );
        let mut placed = [0u32; 16];
        placed[..10].copy_from_slice(&words(38, BUFFER));
        placed[10] = 0x1234_0000;
        let mut twin = placed;
        twin[0] = 36;
        let memory = [BUFFER, 0, 0, 0, 512, 0, 1, 0x1_0001, 0, 1, 0, 1, 0, 0];
        assert_eq!(
            half_view(&View::Placed(true, memory, placed)),
            Some(View::Placed(true, memory, twin))
        );
        // The title's kind (R16G16B16A16_UINT and R16_UINT buffers), a
        // texture view (dimension 4: 2D), a view without a description, a
        // constant buffer: the title's own.
        assert_eq!(
            half_view(&View::Unordered(7, 0, Some(words(12, BUFFER)))),
            None
        );
        assert_eq!(half_view(&View::Shader(7, Some(words(57, BUFFER)))), None);
        assert_eq!(half_view(&View::Unordered(7, 0, Some(words(10, 4)))), None);
        assert_eq!(half_view(&View::Shader(7, None)), None);
        assert_eq!(half_view(&View::Constant(0x1000, 256)), None);
    }

    /// The buffer views through which the integers a shader moves halves
    /// as are not what the console gives are named; the title's are not.
    #[test]
    fn views_whose_values_the_console_would_convert_are_named() {
        const BUFFER: u32 = 1;
        let words = |format: u32, dimension: u32| [format, dimension, 5768, 0, 0, 0, 64, 0, 0, 0];
        for exact in [10, 12, 14, 34, 36, 38, 54, 57, 59, 25, 30, 50, 62] {
            let view = View::Unordered(7, 0, Some(words(exact, BUFFER)));
            assert_eq!(half_inexact(&view), None, "format {exact}");
        }
        // R32_FLOAT, R32_UINT, R16G16B16A16_UNORM, R8G8B8A8_UNORM, R8_SINT,
        // and a structured or raw view (no format).
        for inexact in [41, 42, 11, 28, 64, 0] {
            let view = View::Shader(7, Some(words(inexact, BUFFER)));
            assert_eq!(half_inexact(&view), Some(inexact));
        }
        let mut placed = [0u32; 16];
        placed[..10].copy_from_slice(&words(41, BUFFER));
        let memory = [BUFFER, 0, 0, 0, 512, 0, 1, 0x1_0001, 0, 1, 0, 1, 0, 0];
        assert_eq!(half_inexact(&View::Placed(true, memory, placed)), Some(41));
        // Not a buffer view.
        assert_eq!(half_inexact(&View::Shader(7, Some(words(41, 4)))), None);
        assert_eq!(half_inexact(&View::Constant(0x1000, 256)), None);
    }

    /// The two ways a translated `buffer_load_format_d16_xyzw` and
    /// `buffer_store_format_d16_xyzw` have moved a typed buffer's 16-bit
    /// channels, on the host device, over a buffer that holds every 16-bit
    /// pattern. As unsigned integers through an unsigned view (the title's
    /// own R16G16B16A16_UINT, or the twin `half_view` gives a float one)
    /// every pattern comes back as it was. By way of floats, as before
    /// (load as floats, each to its half's bits; each half widened and
    /// stored as a float): through the title's integer view that is
    /// outside what D3D12 defines, and what the device gives is printed,
    /// not checked (1 pattern of 65,536 on the cards tried: the network
    /// that moves the title's faces computed zeros).
    #[test]
    fn halves_cross_an_unsigned_buffer_view_unchanged() {
        const MOVES: &str = r#"
#define RS "DescriptorTable(UAV(u0, numDescriptors=2))"
#ifdef BY_FLOATS
RWBuffer<float4> from : register(u0);
RWBuffer<float4> to : register(u1);
#else
RWBuffer<uint4> from : register(u0);
RWBuffer<uint4> to : register(u1);
#endif
[RootSignature(RS)]
[numthreads(64, 1, 1)]
void BY_FLOATS_OR_NOT(uint3 id : SV_DispatchThreadID) {
#ifdef BY_FLOATS
    uint4 h = f32tof16(from[id.x]);
#else
    uint4 h = from[id.x] & 0xffff;
#endif
    uint low = h.x | h.y << 16;
    uint high = h.z | h.w << 16;
#ifdef BY_FLOATS
    to[id.x] = float4(f16tof32(low), f16tof32(low >> 16), f16tof32(high), f16tof32(high >> 16));
#else
    to[id.x] = uint4(low & 0xffff, low >> 16, high & 0xffff, high >> 16);
#endif
}
"#;
        const ELEMENTS: usize = 1 << 14;
        const BYTES: u64 = ELEMENTS as u64 * 8;
        const TITLES: u32 = 12; // R16G16B16A16_UINT
        let Some(host) = host() else {
            return; // no host GPU
        };
        let heap: ID3D12DescriptorHeap = {
            let desc = D3D12_DESCRIPTOR_HEAP_DESC {
                Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                NumDescriptors: 2,
                Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                NodeMask: 0,
            };
            // SAFETY: plain heap creation.
            unsafe { host.device.CreateDescriptorHeap(&desc) }.expect("a view heap")
        };
        // How many of the 65,536 patterns come back as they were, moved by
        // the program `by_floats` or not, through views in `format`.
        let moved = |by_floats: bool, format: u32| -> usize {
            let source = if by_floats {
                MOVES
                    .replace("BY_FLOATS_OR_NOT", "main")
                    .replace("#ifdef BY_FLOATS", "#if 1")
            } else {
                MOVES
                    .replace("BY_FLOATS_OR_NOT", "main")
                    .replace("#ifdef BY_FLOATS", "#if 0")
            };
            let code = crate::host::compile(
                &source,
                windows::core::s!("main"),
                windows::core::s!("cs_5_1"),
            )
            .expect("the test's shader");
            let (from, filled) = host.shared_buffer(BYTES).expect("a buffer");
            let (to, result) = host.shared_buffer(BYTES).expect("a buffer");
            // SAFETY: the shader's own root signature and plain creation
            // calls; both mappings hold `BYTES` bytes; the views are of
            // live buffers, in slots of the heap made above; an open list.
            unsafe {
                let root: ID3D12RootSignature = host
                    .device
                    .CreateRootSignature(0, &code)
                    .expect("a root signature");
                let pso: ID3D12PipelineState = host
                    .device
                    .CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                        pRootSignature: std::mem::transmute_copy(&root),
                        CS: D3D12_SHADER_BYTECODE {
                            pShaderBytecode: code.as_ptr().cast(),
                            BytecodeLength: code.len(),
                        },
                        ..Default::default()
                    })
                    .expect("a pipeline");
                let filled = std::slice::from_raw_parts_mut(filled.cast::<u16>(), ELEMENTS * 4);
                for (pattern, half) in filled.iter_mut().enumerate() {
                    *half = pattern as u16;
                }
                std::ptr::write_bytes(result, 0xee, BYTES as usize);
                let view = D3D12_UNORDERED_ACCESS_VIEW_DESC {
                    Format: DXGI_FORMAT(format as i32),
                    ViewDimension: D3D12_UAV_DIMENSION_BUFFER,
                    Anonymous: D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
                        Buffer: D3D12_BUFFER_UAV {
                            NumElements: ELEMENTS as u32,
                            ..Default::default()
                        },
                    },
                };
                let start = heap.GetCPUDescriptorHandleForHeapStart();
                let step = host
                    .device
                    .GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV)
                    as usize;
                host.device.CreateUnorderedAccessView(
                    &from,
                    None::<&ID3D12Resource>,
                    Some(&view),
                    start,
                );
                let second = D3D12_CPU_DESCRIPTOR_HANDLE {
                    ptr: start.ptr + step,
                };
                host.device.CreateUnorderedAccessView(
                    &to,
                    None::<&ID3D12Resource>,
                    Some(&view),
                    second,
                );
                let list = host.new_list().expect("a host list");
                list.list.SetDescriptorHeaps(&[Some(heap.clone())]);
                list.list.SetComputeRootSignature(&root);
                list.list.SetPipelineState(&pso);
                list.list
                    .SetComputeRootDescriptorTable(0, heap.GetGPUDescriptorHandleForHeapStart());
                list.list.Dispatch(ELEMENTS as u32 / 64, 1, 1);
                list.close();
                host.execute(&[&list]);
                assert!(
                    host.device.GetDeviceRemovedReason().is_ok(),
                    "the device, after views in format {format}"
                );
                let result = std::slice::from_raw_parts(result.cast::<u16>(), ELEMENTS * 4);
                (0..=u16::MAX)
                    .filter(|&p| result[usize::from(p)] == p)
                    .count()
            }
        };
        // A float view of halves is shown as the title's kind of view.
        assert_eq!(half_format(10), Some(TITLES));
        let unsigned = moved(false, TITLES);
        let by_floats = moved(true, TITLES);
        eprintln!(
            "halves through an R16G16B16A16_UINT buffer view: {unsigned} of 65536 patterns came back unchanged moved as unsigned integers, {by_floats} moved by way of floats"
        );
        assert_eq!(
            unsigned, 65_536,
            "patterns unchanged, moved as unsigned integers"
        );
    }

    /// A null texture UAV keeps the title's dimension (3D for the world's
    /// wind field), and is 2D when the title names none.
    #[test]
    fn null_uavs_keep_the_titles_dimension() {
        let d = null_uav_desc(Some(8));
        assert_eq!(d.ViewDimension, D3D12_UAV_DIMENSION_TEXTURE3D);
        // SAFETY: the member the dimension names.
        assert_eq!(unsafe { d.Anonymous.Texture3D.WSize }, u32::MAX);
        let d = null_uav_desc(Some(5));
        assert_eq!(d.ViewDimension, D3D12_UAV_DIMENSION_TEXTURE2DARRAY);
        // SAFETY: the member the dimension names.
        assert_eq!(unsafe { d.Anonymous.Texture2DArray.ArraySize }, 1);
        let dimension = |d| null_uav_desc(d).ViewDimension;
        assert_eq!(dimension(Some(2)), D3D12_UAV_DIMENSION_TEXTURE1D);
        assert_eq!(dimension(Some(4)), D3D12_UAV_DIMENSION_TEXTURE2D);
        assert_eq!(dimension(None), D3D12_UAV_DIMENSION_TEXTURE2D);
        // Multisampled UAVs need a cap; they stay 2D.
        assert_eq!(dimension(Some(6)), D3D12_UAV_DIMENSION_TEXTURE2D);
    }

    /// What the texture code asks of the spans (levels on demand): where
    /// the next texture starts inside a span, the lowest level a view of a
    /// texture shows, and a texture's slots.
    #[test]
    fn spans_say_the_next_start_and_the_lowest_level_viewed() {
        // One view description for every slot: none is stale against another.
        const DESC: [u32; 14] = [0; 14];
        let mut spans = Spans::default();
        // A 512x512 texture of 0x56000 bytes viewed from levels 3 (0x2000 bytes)
        // and 1 (0x6000 bytes), and the next one packed 0x6000 after it (run 188).
        spans.note(1, 0x1_0000, 0x1_0000 + 0x2000, &DESC, true);
        spans.shows(1, 3);
        spans.note(2, 0x1_0000, 0x1_0000 + 0x6000, &DESC, true);
        spans.shows(2, 1);
        spans.note(3, 0x1_6000, 0x1_6000 + 0x5_6000, &DESC, true);
        assert_eq!(
            spans.next_start(0x1_0000, 0x1_0000 + 0x5_6000),
            Some(0x1_6000)
        );
        assert_eq!(spans.next_start(0x1_6000, 0x1_6000 + 0x5_6000), None);
        assert_eq!(spans.first_viewed(0x1_0000), Some(1));
        assert_eq!(spans.first_viewed(0x1_6000), None);
        assert_eq!(spans.slots_at(0x1_0000), vec![1, 2]);
        // The view from level 1 goes: the texture is viewed from level 3.
        spans.forget(2);
        assert_eq!(spans.first_viewed(0x1_0000), Some(3));
    }

    /// Views of one placed texture are told from views of others by the
    /// address and the resource description, not by what they show.
    #[test]
    fn views_of_one_placed_texture_are_told_by_address_and_description() {
        let texture = [3, 0, 0, 0, 512, 0, 512, 0x8_0001, 98, 1, 0, 0x105, 0, 0];
        let view = |resource: [u32; 14], first_mip: u32, address: u32| {
            let mut desc = [0u32; 16];
            (desc[0], desc[1], desc[4], desc[5], desc[10], desc[11]) =
                (98, 4, first_mip, 8 - first_mip, address, 0xb);
            View::Placed(false, resource, desc)
        };
        let low = view(texture, 3, 0x1000_0000);
        // more mips of the same texture
        assert!(same_placed_texture(&low, &view(texture, 0, 0x1000_0000)));
        // another address, another description, a buffer, an unordered view
        assert!(!same_placed_texture(&low, &view(texture, 3, 0x2000_0000)));
        let mut wider = texture;
        wider[4] = 1024;
        assert!(!same_placed_texture(&low, &view(wider, 3, 0x1000_0000)));
        let mut buffer = texture;
        buffer[0] = 1;
        assert!(!same_placed_texture(
            &view(buffer, 3, 0x1000_0000),
            &view(buffer, 3, 0x1000_0000)
        ));
        let View::Placed(_, resource, desc) = low.clone() else {
            unreachable!()
        };
        assert!(!same_placed_texture(
            &low,
            &View::Placed(true, resource, desc)
        ));
        assert!(!same_placed_texture(&low, &View::Shader(0, None)));
    }

    /// The texture that stands for missing ones can be made and viewed.
    #[cfg(feature = "diagnostics")]
    #[test]
    fn a_magenta_texture_is_made() {
        let Some(host) = host() else {
            return; // no host GPU
        };
        let Some(heap) = HostHeap::new(false, 4) else {
            return;
        };
        let texture = magenta().expect("the texture");
        // SAFETY: a slot of our heap and a live texture.
        unsafe {
            host.device
                .CreateShaderResourceView(texture, None, heap.cpu(0))
        };
        // SAFETY: a plain device query.
        assert!(unsafe { host.device.GetDeviceRemovedReason() }.is_ok());
    }

    /// A null 3D UAV made on the real device leaves it alive.
    #[test]
    fn null_3d_uav_is_accepted_by_the_device() {
        let Some(host) = host() else {
            return; // no host GPU
        };
        let Some(heap) = HostHeap::new(false, 4) else {
            return;
        };
        let view = View::Unordered(0, 0, Some([28, 8, 0, 0, 0, 0, 0, 0, 0, 0]));
        null_for(&view, heap.cpu(0));
        // SAFETY: a plain device query.
        assert!(unsafe { host.device.GetDeviceRemovedReason() }.is_ok());
    }

    /// Dynamic slots stay taken until given back, and runs do not wrap.
    #[test]
    fn a_structured_view_made_raw_passes_the_host_check() {
        // 100 elements of 20 bytes from element 8, in a copy that starts 32
        // bytes before the view's memory: bytes 192..2192.
        let (first, count) = raw_window(32, 8, 100, 20).unwrap();
        assert_eq!((first, count), (48, 500));
        assert!(buffer_view_ok(
            DXGI_FORMAT_R32_TYPELESS,
            first,
            count,
            0,
            true,
            4096
        ));
        // the format a raw view must name; the structured one's is refused
        assert!(!buffer_view_ok(
            DXGI_FORMAT_UNKNOWN,
            first,
            count,
            0,
            true,
            4096
        ));
        // past the buffer, not on 16 bytes, no elements
        assert!(!buffer_view_ok(
            DXGI_FORMAT_R32_TYPELESS,
            first,
            count,
            0,
            true,
            2000
        ));
        assert!(raw_window(4, 0, 100, 20).is_none());
        assert!(!buffer_view_ok(
            DXGI_FORMAT_R32_TYPELESS,
            0,
            0,
            0,
            true,
            4096
        ));
        // a structured view the host takes as it is
        assert!(buffer_view_ok(DXGI_FORMAT_UNKNOWN, 1, 10, 12, false, 4096));
    }

    /// The queue of textures picked by index serves the slots looked at
    /// before first, then the newest write and the oldest in turns; a
    /// slot whose memory is not there is looked at again after
    /// 1, 2, 4 frames up to the limit; and what the pass reports about a
    /// slot written again meanwhile is dropped.
    #[test]
    fn constant_views_are_whole_and_at_most_64_kib() {
        assert_eq!(constant_size(1), 256);
        assert_eq!(constant_size(256), 256);
        assert_eq!(constant_size(65_536), 65_536);
        assert_eq!(constant_size(969_472), 65_536);
        assert_eq!(constant_size(u32::MAX), 65_536);
    }

    #[test]
    fn views_past_the_end_are_cut_to_the_elements_inside() {
        assert_eq!(elements_inside(0, 10, 16, 160), 10);
        assert_eq!(elements_inside(4, 10, 16, 160), 6);
        assert_eq!(elements_inside(4, 10, 16, 167), 6);
        // Nothing inside: as it was, for the host check to refuse.
        assert_eq!(elements_inside(10, 10, 16, 160), 10);
        assert_eq!(elements_inside(0, 7, 4, 1 << 40), 7);
    }

    /// A slot uploaded whole is looked at a few times, further and further
    /// apart; one found changed is due at once; one written again, taken
    /// out or waiting in the queue is not looked at.
    #[test]
    fn uploaded_textures_are_looked_at_again() {
        let mut q = TextureQueue::default();
        let at = (0x1000, 64);
        q.written(7, 1, 1, false);
        let write = q.due(1, 1)[0].1;
        q.recheck(7, 1, at, 5, false);
        assert!(q.rechecks_due(1000, 9).is_empty(), "it waits in the queue");
        assert!(!q.done(7, write));
        q.recheck(7, 1, at, 5, false);
        assert_eq!(q.census(1000)[0], 0, "no slot of the queue");
        let mut frame = 1;
        for wait in RECHECK_FRAMES {
            assert!(q.rechecks_due(frame + wait - 1, 9).is_empty());
            frame += wait;
            let due = q.rechecks_due(frame, 9);
            assert_eq!(due.len(), 1);
            q.rechecked(7, &due[0].1, false, frame, false, None);
        }
        assert!(q.rechecks_due(u64::MAX, 9).is_empty(), "the looks are over");
        q.recheck(7, frame, at, 5, false);
        let due = q.rechecks_due(u64::MAX, 9);
        q.rechecked(7, &due[0].1, true, frame + 4, false, None);
        assert_eq!(q.due(frame + 4, 9)[0].0, 7);
        assert!(q.rechecks_due(u64::MAX, 9).is_empty());
        // A draw makes the host slot anew: the slot is due although a look
        // at it was still to come.
        q.recheck(8, 1, at, 5, false);
        let seen = q.rechecks_due(u64::MAX, 9);
        q.written(8, 2, 2, true);
        assert!(q.due(2, 9).iter().any(|d| d.0 == 8));
        q.rechecked(8, &seen[0].1, false, 5, false, None);
        assert!(q.rechecks_due(u64::MAX, 9).is_empty());
    }

    /// A slot found changed after its upload becomes live: served ahead of
    /// regular slots, looked at every frame up to LIVE_LOOKS unchanged looks,
    /// then downgraded to plain looks; a fresh write or removal clears live.
    #[test]
    fn live_slots_check_every_frame_and_downgrade_after_idle() {
        let mut q = TextureQueue::default();
        let at = (0x1000, 0x2000);
        // Slot 1: regular; slot 7: will become live.
        q.recheck(1, 1, at, 11, false);
        q.recheck(7, 1, at, 5, false);
        let due = q.rechecks_due(5, 9);
        let seen7 = due.iter().find(|(idx, _)| *idx == 7).unwrap().1;
        // Slot 7 changed -> becomes live and re-enters `slots`.
        q.rechecked(7, &seen7, true, 5, true, None);
        assert!(q.live.contains(&7));
        // Even with another `again` slot queued, live slot 7 is served first.
        q.written(2, 5, 5, false);
        let w2 = q.due(5, 9).into_iter().find(|d| d.0 == 2).unwrap().1;
        q.later(2, w2, 4, 16);
        let order = q.due(5, 9);
        assert_eq!(order[0].0, 7, "live slot served first");
        let w7 = order[0].1;
        assert!(q.done(7, w7), "done returns live status and clears pending set");
        assert!(!q.live.contains(&7));

        // Re-entered as live: due on frame + 1 and prioritized ahead of overdue plain rechecks.
        q.recheck(7, 5, at, 6, true);
        assert!(q.rechecks_due(5, 9).iter().all(|(idx, _)| *idx != 7));
        let due6 = q.rechecks_due(6, 9);
        assert_eq!(due6[0].0, 7, "live slot checked ahead of overdue plain slot 1");
        assert!(due6[0].1.live);

        // Stays live across LIVE_LOOKS unchanged frames, then downgrades to plain.
        let mut frame = 6u64;
        for _ in 0..super::LIVE_LOOKS {
            let r = q.rechecks_due(frame, 1)[0].1;
            assert!(r.live);
            q.rechecked(7, &r, false, frame, true, None);
            frame += 1;
        }
        let last_live = q.rechecks_due(frame, 1)[0].1;
        assert!(last_live.live);
        assert_eq!(last_live.looks, super::LIVE_LOOKS);
        q.rechecked(7, &last_live, false, frame, true, Some(99));
        assert!(q.rechecks_due(frame + RECHECK_FRAMES[1] - 1, 9).iter().all(|(idx, _)| *idx != 7));
        let downgraded = q
            .rechecks_due(frame + RECHECK_FRAMES[1], 9)
            .into_iter()
            .find(|(idx, _)| *idx == 7)
            .expect("downgraded slot still checked on plain schedule")
            .1;
        assert!(!downgraded.live);
        assert_eq!(downgraded.print, 99);
        assert_eq!(downgraded.looks, 1);

        // Fresh write clears live flag.
        q.recheck(9, 10, at, 5, true);
        let r9 = q.rechecks_due(11, 1)[0].1;
        q.rechecked(9, &r9, true, 11, true, None);
        assert!(q.live.contains(&9));
        q.written(9, 12, 12, false);
        assert!(!q.live.contains(&9));
    }

    /// `fingerprint_for(..., true)` catches a 1-byte change between the
    /// sampled 4 KiB windows of `fingerprint` for textures up to 4 MiB.
    #[test]
    fn fingerprint_for_live_catches_mid_plane_change() {
        let mut plane = vec![0x42u8; 256 << 10];
        let base = plane.as_ptr() as u64;
        let len = plane.len() as u64;
        let plain_before = crate::texture::fingerprint(base, len);
        let live_before = crate::texture::fingerprint_for(base, len, true);
        assert_eq!(crate::texture::fingerprint_for(base, len, false), plain_before);
        // Byte at 8 KiB falls in the unsampled gap between piece 0 (0..4K) and piece 1 (~20K).
        plane[8 << 10] ^= 0x01;
        assert_eq!(
            crate::texture::fingerprint(base, len),
            plain_before,
            "sampled fingerprint misses gap byte"
        );
        assert_ne!(
            crate::texture::fingerprint_for(base, len, true),
            live_before,
            "live whole-plane fingerprint must catch gap byte"
        );
    }

    /// The fingerprint of a texture's memory sees a change at its start, in
    /// its middle and at its end, and is the same for the same bytes.
    #[test]
    fn a_fingerprint_sees_changes() {
        let mut memory = vec![3u8; 1 << 20];
        let print = |m: &[u8]| crate::texture::fingerprint(m.as_ptr() as u64, m.len() as u64);
        let before = print(&memory);
        assert_eq!(print(&memory), before);
        let last = memory.len() - 1;
        for at in [0, ((memory.len() - 4096) / 11 * 5) & !0xfff, last] {
            memory[at] ^= 1;
            assert_ne!(print(&memory), before, "byte {at}");
            memory[at] ^= 1;
        }
        assert_ne!(print(&memory[..100]), print(&memory[..200]));
    }

    #[test]
    fn card_room_preserves_texture_retries_after_a_hold() {
        let mut q = TextureQueue::default();
        for index in 1..=4 {
            q.written(index, 0, 0, false);
        }
        let due = q.due(0, 4);
        for &(index, write) in &due {
            q.hold(index, write, 4);
        }
        assert!(q.due(3, 4).is_empty());
        assert_eq!(q.due(4, 4), due);

        for (index, write) in due {
            match index {
                1 => q.later(index, write, 4, 8),
                2 => q.partly(index, write, 8, 4),
                3 => {
                    q.done(index, write);
                    q.recheck(index, 4, (16, 8), 1, false);
                }
                _ => q.hold(index, write, 12),
            }
        }

        assert_eq!(q.room(4), 1);
        let due_slots = |frame| {
            q.due(frame, 4)
                .into_iter()
                .map(|(index, _)| index)
                .collect::<std::collections::BTreeSet<_>>()
        };
        assert_eq!(due_slots(4), [4].into());
        assert_eq!(due_slots(5), [1, 4].into());
        assert_eq!(due_slots(8), [1, 2, 4].into());
    }

    #[test]
    fn has_due_reports_whether_any_slot_is_ready_at_frame() {
        let mut q = TextureQueue::default();
        assert!(!q.has_due(0));
        q.written(3, 1, 5, false);
        assert!(!q.has_due(4));
        assert!(q.has_due(5));
        let write = q.due(5, 1)[0].1;
        q.done(3, write);
        assert!(!q.has_due(10));
    }

    #[test]
    fn the_texture_queue_serves_the_newest_writes_first() {
        let mut q = TextureQueue::default();
        // Slots 10..18 written in frames 1..9, slot 5 last.
        for i in 0..8u32 {
            q.written(10 + i, u64::from(i) + 1, 0, false);
        }
        q.written(5, 9, 0, false);
        let order: Vec<u32> = q.due(9, 100).into_iter().map(|d| d.0).collect();
        // newest, oldest, newest, oldest, and so on
        assert_eq!(order, [5, 10, 17, 11, 16, 12, 15, 13, 14]);
        assert_eq!(q.due(9, 2).len(), 2);
        assert_eq!(q.census(9), [9, 9, 0, 0, 8]);
        // Slot 17's memory is not mapped: again next frame, then in 2, 4, 4.
        let write = |q: &TextureQueue, index: u32| {
            q.due(u64::MAX, 100)
                .iter()
                .find(|d| d.0 == index)
                .unwrap()
                .1
        };
        let w17 = write(&q, 17);
        let mut frame = 9;
        for wait in [1, 2, 4, 4] {
            q.later(17, w17, frame, 4);
            assert!(!q.due(frame + wait - 1, 100).iter().any(|d| d.0 == 17));
            frame += wait;
            // Due again, and ahead of every slot never looked at.
            assert_eq!(q.due(frame, 100)[0].0, 17);
        }
        assert_eq!(q.census(9)[2], 1);
        q.done(17, w17);
        assert!(!q.due(u64::MAX, 100).iter().any(|d| d.0 == 17));
        // Uploaded with part of its memory there: kept, with what was there.
        let w16 = write(&q, 16);
        q.partly(16, w16, 12, 4096);
        assert_eq!(q.mapped(16), 4096);
        assert_eq!(q.due(12, 100)[0].0, 16);
        // The title writes slot 16 again while the pass works on it: what
        // the pass then says about the old write changes nothing.
        q.written(16, 13, 0, false);
        q.done(16, w16);
        q.later(16, w16, 13, 4);
        assert_eq!(q.mapped(16), 0);
        assert_eq!(q.due(13, 100)[0].0, 16);
        // A draw that makes the host slot (`seen`) keeps what the queue knows,
        let w15 = write(&q, 15);
        q.partly(15, w15, 20, 8192);
        q.written(15, 14, 0, true);
        assert_eq!(q.mapped(15), 8192);
        // and queues a slot the queue did not hold.
        q.written(99, 14, 0, true);
        assert!(q.due(14, 100).iter().any(|d| d.0 == 99));
        // A write due later (a table's slot) is not handed out before.
        q.written(98, 14, 16, false);
        assert!(!q.due(15, 100).iter().any(|d| d.0 == 98));
        assert!(q.due(16, 100).iter().any(|d| d.0 == 98));
    }

    /// A slot written with another texture leaves its old span; the same
    /// view written again changes nothing.
    #[test]
    fn a_slot_rewritten_moves_to_its_new_span() {
        let mut spans = Spans::default();
        assert!(
            spans
                .note(1, 0x10000, 0x12000, &shape(0x10000, 0x12000), true)
                .is_empty()
        );
        assert!(
            spans
                .note(2, 0x12000, 0x14000, &shape(0x12000, 0x14000), true)
                .is_empty()
        );
        assert!(
            spans
                .note(1, 0x10000, 0x12000, &shape(0x10000, 0x12000), true)
                .is_empty()
        );
        assert_eq!(spans.slots_in(0x10000, 0x10001), [1]);
        // Slot 1 now views a texture far away.
        assert!(
            spans
                .note(1, 0x90000, 0x91000, &shape(0x90000, 0x91000), true)
                .is_empty()
        );
        assert_eq!(spans.slots_in(0x10000, 0x10001), [] as [u32; 0]);
        assert_eq!(spans.slots_in(0x12000, 0x12001), [2]);
        assert_eq!(spans.slots_in(0x90000, 0x90001), [1]);
        spans.forget(1);
        spans.forget(2);
        assert!(spans.spans.is_empty() && spans.by_slot.is_empty());
    }

    /// A view written over memory that older views describe otherwise makes
    /// those stale; views of the same span are one texture's.
    #[test]
    fn a_newer_view_of_other_memory_makes_the_older_stale() {
        let mut spans = Spans::default();
        // Three textures side by side, the first in two slots.
        assert!(
            spans
                .note(1, 0x1000, 0x3000, &shape(0x1000, 0x3000), true)
                .is_empty()
        );
        assert!(
            spans
                .note(2, 0x1000, 0x3000, &shape(0x1000, 0x3000), true)
                .is_empty()
        );
        assert!(
            spans
                .note(3, 0x3000, 0x4000, &shape(0x3000, 0x4000), true)
                .is_empty()
        );
        assert!(
            spans
                .note(4, 0x8000, 0x9000, &shape(0x8000, 0x9000), true)
                .is_empty()
        );
        // The same slot written with the same span again: nothing changes.
        assert!(
            spans
                .note(2, 0x1000, 0x3000, &shape(0x1000, 0x3000), true)
                .is_empty()
        );
        assert_eq!(spans.slots_in(0x2000, 0x3800), [1, 2, 3]);
        assert_eq!(spans.slots_in(0x4000, 0x8000), [] as [u32; 0]);
        // A draw makes a slot whose view overlaps them: left out, nothing stale.
        assert!(
            spans
                .note(9, 0x2000, 0x3800, &shape(0x2000, 0x3800), false)
                .is_empty()
        );
        assert_eq!(spans.slots_in(0x2000, 0x3800), [1, 2, 3]);
        // The title writes a texture over the end of the first and all of
        // the second: their three slots are stale.
        let mut stale = spans.note(5, 0x2000, 0x4000, &shape(0x2000, 0x4000), true);
        stale.sort_unstable();
        assert_eq!(stale, [1, 2, 3]);
        assert_eq!(spans.slots_in(0, u64::MAX), [5, 4]);
        // Another start in the same memory: stale in turn. Another length
        // from the same start too.
        assert_eq!(
            spans.note(6, 0x2800, 0x3000, &shape(0x2800, 0x3000), true),
            [5]
        );
        assert_eq!(
            spans.note(7, 0x2800, 0x2c00, &shape(0x2800, 0x2c00), true),
            [6]
        );
        // A slot that moves to another texture leaves its span behind.
        assert!(
            spans
                .note(7, 0x8000, 0x9000, &shape(0x8000, 0x9000), true)
                .is_empty()
        );
        assert_eq!(spans.slots_in(0x2800, 0x2c00), [] as [u32; 0]);
        assert_eq!(spans.slots_in(0x8000, 0x8001), [4, 7]);
        spans.forget(4);
        spans.forget(7);
        assert!(spans.spans.is_empty() && spans.by_slot.is_empty());
    }

    /// Views of one texture that show more and fewer of its mips are all
    /// its own, and the next texture may begin where the mips that are
    /// there end.
    #[test]
    fn a_texture_of_which_the_smaller_mips_are_there_ends_behind_them() {
        // As the title has them: 512x512 BC7 of which 64x64 is
        // there, and a 1024x512 BC1 texture 8 KiB on.
        let texture = [3, 0, 0, 0, 512, 0, 512, 0x8_0001, 99, 1, 0, 0x105, 0, 0];
        let next = [3, 0, 0, 0, 1024, 0, 512, 0x8_0001, 71, 1, 0, 0x105, 0, 0];
        let mut spans = Spans::default();
        let at = 0xb_5c04_0000u64;
        let end = |spans: &mut Spans, desc: &[u32; 14], at: u64, first| {
            at + spans.bytes(desc, first).unwrap()
        };
        let a = end(&mut spans, &texture, at, 3);
        assert_eq!(a, at + 0x2000);
        assert!(spans.note(1, at, a, &texture, true).is_empty());
        let b = end(&mut spans, &next, a, 3);
        assert!(b <= a + 0x4000);
        assert!(spans.note(2, a, b, &next, true).is_empty());
        assert_eq!(spans.slots_in(at, b), [1, 2]);
        // The smaller mips alone in another slot: the same texture's.
        let small = end(&mut spans, &texture, at, 5);
        assert!(small < a);
        assert!(spans.note(3, at, small, &texture, true).is_empty());
        assert_eq!(spans.slots_in(at, at + 1), [3, 1]);
        // More of it streamed in, over the next texture: that one is stale.
        let more = end(&mut spans, &texture, at, 1);
        assert!(more > b);
        assert_eq!(spans.note(1, at, more, &texture, true), [2]);
        assert_eq!(spans.claim(3, at, small, &texture), (Claim::Own, None));
        assert_eq!(
            spans.claim(2, a, b, &next),
            (Claim::Other, Some((at, more)))
        );
    }

    /// A resource description of its own for every span.
    fn shape(start: u64, end: u64) -> [u32; 14] {
        let mut desc = [0; 14];
        (desc[0], desc[4], desc[6]) = (3, start as u32, end as u32);
        desc
    }

    /// Whose the memory of a view is: the slot's own span, no other's, or
    /// one that says otherwise.
    #[test]
    fn spans_say_whose_memory_a_view_is() {
        let mut spans = Spans::default();
        assert!(
            spans
                .note(1, 0x1000, 0x3000, &shape(0x1000, 0x3000), true)
                .is_empty()
        );
        assert_eq!(
            spans.claim(1, 0x1000, 0x3000, &shape(0x1000, 0x3000)),
            (Claim::Own, None)
        );
        // Another slot's view of the same texture, or of memory nothing describes.
        assert_eq!(
            spans.claim(2, 0x1000, 0x3000, &shape(0x1000, 0x3000)),
            (Claim::Free, None)
        );
        assert_eq!(
            spans.claim(2, 0x3000, 0x4000, &shape(0x3000, 0x4000)),
            (Claim::Free, None)
        );
        // The same start with another length; another start.
        assert_eq!(
            spans.claim(2, 0x1000, 0x2000, &shape(0x1000, 0x2000)),
            (Claim::SameStart, Some((0x1000, 0x3000)))
        );
        assert_eq!(
            spans.claim(2, 0x800, 0x2000, &shape(0x800, 0x2000)),
            (Claim::Other, Some((0x1000, 0x3000)))
        );
    }

    /// A view that fell out of the queue with its memory there and nothing
    /// to show for it goes back; one a newer view speaks against does not.
    #[test]
    fn a_texture_sweep_finds_views_nothing_brings_up_to_date() {
        let Some(host) = HostHeap::new(false, 8) else {
            return; // no host GPU
        };
        // Below 2^40, as a title's memory is.
        // SAFETY: new memory, left to the process.
        let at = unsafe {
            use windows_sys::Win32::System::Memory::{
                MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc,
            };
            VirtualAlloc(
                0x7d_4000_0000usize as *const _,
                1 << 20,
                MEM_RESERVE | MEM_COMMIT,
                PAGE_READWRITE,
            ) as u64
        };
        if at == 0 {
            return; // taken
        }
        let mut slots = vec![[0u32; 8]; 8];
        let base = slots.as_mut_ptr() as u64;
        let handle = |index: u32| Some(base + u64::from(index) * 32);
        let resource = [3, 0, 0, 0, 64, 0, 64, 0x1_0001, 98, 1, 0, 0x105, 0, 0];
        let view = |width: u32, address: u64| {
            let mut desc = [0u32; 16];
            (desc[10], desc[11]) = (address as u32, (address >> 32) as u32);
            let mut resource = resource;
            resource[4] = width;
            View::Placed(false, resource, desc)
        };
        super::super::record(handle(1).unwrap(), view(64, at));
        super::super::record(handle(2).unwrap(), view(64, at + 0x8_0000));
        // A newer, wider texture in the second one's memory.
        host.note_buffer(5, Some(&view(128, at + 0x8_0000)), false, true);
        let queued = || -> Vec<u32> {
            host.texture_slots_due(u64::MAX, 8)
                .0
                .into_iter()
                .map(|d| d.0)
                .collect()
        };
        for slot in queued() {
            host.textures.lock().unwrap().remove(slot);
        }
        let mut counts = [0; 10];
        host.sweep_part(0..8, handle, 50, false, &mut counts);
        assert_eq!(counts, [2, 0, 0, 0, 0, 0, 1, 1, 0, 0]);
        assert_eq!(queued(), []);
        host.sweep_part(0..8, handle, 50, true, &mut counts);
        assert_eq!(queued(), [1]);
    }

    /// Memory the runtime maps under a slot's texture makes the slot due
    /// again from the next frame; a stale slot leaves the queue.
    #[test]
    fn remapped_memory_makes_its_texture_slots_due() {
        let Some(heap) = HostHeap::new(false, 8) else {
            return; // no host GPU
        };
        let slots = |at: u64| -> Vec<u32> {
            heap.texture_slots_due(at, 8)
                .0
                .into_iter()
                .map(|d| d.0)
                .collect()
        };
        // 64x64 BC7, one mip, at an address no test and no title uses.
        let at = 0x7fee_0000_0000u64;
        let view = |address: u64, width: u32| {
            let mut desc = [0u32; 16];
            (desc[10], desc[11]) = (address as u32, (address >> 32) as u32);
            View::Placed(
                false,
                [3, 0, 0, 0, width, 0, 64, 0x1_0001, 98, 1, 0, 0x105, 0, 0],
                desc,
            )
        };
        let len = crate::resource::placed_bytes(&[
            3, 0, 0, 0, 64, 0, 64, 0x1_0001, 98, 1, 0, 0x105, 0, 0,
        ])
        .unwrap();
        assert!(
            heap.note_buffer(1, Some(&view(at, 64)), false, true)
                .is_empty()
        );
        let (queued, write) = heap.texture_slots_due(u64::MAX, 8).0[0];
        assert_eq!(queued, 1);
        heap.texture_slot_done(1, write, None);
        assert_eq!(slots(u64::MAX), []);
        // Memory elsewhere: nothing. Under the texture: due from the next frame.
        remapped((at + 2 * len) as usize, 0x1000);
        remapped(at as usize, 0x1000);
        assert_eq!(heap.texture_slots_remapped(50), 1);
        assert_eq!(slots(50), []);
        assert_eq!(slots(51), [1]);
        // A wider texture written at the same address: slot 1 is stale.
        assert_eq!(heap.note_buffer(2, Some(&view(at, 128)), false, true), [1]);
        assert_eq!(slots(u64::MAX), [2]);
    }

    /// A slot a draw read through a table leaves the queue; the title's
    /// next write of it waits for its draw, and is due after the grace
    /// frames if none reads it.
    #[test]
    fn slots_read_through_tables_leave_the_texture_queue() {
        let Some(heap) = HostHeap::new(false, 8) else {
            return; // no host GPU
        };
        let frame = 100;
        let slots = |at: u64| -> Vec<u32> {
            heap.texture_slots_due(at, 8)
                .0
                .into_iter()
                .map(|d| d.0)
                .collect()
        };
        let texture = View::Placed(
            false,
            [3, 0, 0, 0, 4, 0, 4, 0x1_0001, 28, 1, 0, 0x105, 0, 0],
            [0; 16],
        );
        assert!(is_texture_view(&texture));
        heap.queue_texture(2, true, false, frame);
        heap.queue_texture(3, true, false, frame);
        assert_eq!(slots(frame), [3, 2]);
        heap.tabled_in(3, true, frame);
        assert_eq!(slots(frame), [2]);
        // Made in place by the draw that read it: still out.
        heap.queue_texture(3, true, true, frame);
        assert_eq!(slots(frame), [2]);
        // Written again in the next frame: due once the grace frames have
        // passed without a draw that reads it.
        heap.queue_texture(3, true, false, frame + 1);
        assert_eq!(slots(frame + TABLED_GRACE), [2]);
        assert_eq!(slots(frame + 1 + TABLED_GRACE), [3, 2]);
        // The draw comes after all: out again.
        heap.tabled_in(3, true, frame + 1);
        assert_eq!(slots(u64::MAX), [2]);
        // A slot of a table no draw read stays queued; its next write waits.
        heap.tabled_in(2, false, frame + 1);
        assert_eq!(slots(frame + 1), [2]);
        heap.queue_texture(2, true, false, frame + 2);
        assert_eq!(slots(frame + 2), []);
        // Long after the last table, a write is due at once.
        heap.queue_texture(2, true, false, frame + 3 + TABLED_FRAMES);
        assert_eq!(slots(frame + 3 + TABLED_FRAMES), [2]);
    }

    #[test]
    fn dynamic_slots_stay_within_half_a_capped_heap() {
        assert_eq!(dynamic_slots_on(0, Some(524_288)), DYNAMIC);
        assert_eq!(dynamic_slots_on(2_000_000, None), DYNAMIC);
        assert_eq!(dynamic_slots_on(2_000_000, Some(524_288)), 524_288);
        assert_eq!(dynamic_slots_on(800_000, Some(524_288)), 400_000);
        assert_eq!(dynamic_slots_on(2_000_000, Some(1)), DYNAMIC);
    }

    #[test]
    fn dynamic_slots_are_given_back_only_to_the_heap_that_took_them() {
        let Some(old) = HostHeap::new(false, 4) else {
            return;
        };
        let Some(new) = HostHeap::new(false, 4) else {
            return;
        };
        assert_ne!(old.id(), new.id());
        let first = new.dynamic(2).expect("two dynamic slots");
        let free = new.free.load(std::sync::atomic::Ordering::Relaxed);
        new.release(old.id(), first, 2);
        assert_eq!(new.free.load(std::sync::atomic::Ordering::Relaxed), free);
        new.release(new.id(), first, 2);
        assert_eq!(
            new.free.load(std::sync::atomic::Ordering::Relaxed),
            free + 2
        );
    }

    #[test]
    fn dynamic_slots_are_held_until_released() {
        let Some(heap) = HostHeap::new(false, 4) else {
            return; // no host GPU
        };
        let a = heap.dynamic(heap.dynamic - 101).unwrap(); // the last slot is never given
        let b = heap.dynamic(60).unwrap();
        assert_eq!((a, b), (4, 4 + heap.dynamic - 101));
        // 40 left at the end: 50 do not fit until something is given back.
        assert_eq!(heap.dynamic(50), None);
        heap.release(heap.id(), a, 50);
        assert_eq!(heap.dynamic(50), Some(4));
        assert_eq!(heap.dynamic(40), Some(4 + heap.dynamic - 41));
    }

    /// Mirroring a buffer slot from `heap_buffers` (`record_change = false`)
    /// does not hand the slot back to `buffer_changes`, and `slot_flags` stay
    /// in sync across `textures`, `placed`, and `buffers`.
    #[test]
    fn mirror_heap_buffer_does_not_requeue_and_slot_flags_stay_in_sync() {
        let Some(heap) = HostHeap::new(false, 8) else {
            return; // no host GPU
        };
        assert!(!heap.slot_may(3, SLOT_IN_TEXTURES));
        assert!(!heap.slot_may(3, SLOT_IN_PLACED));
        assert!(!heap.slot_may(3, SLOT_IN_BUFFERS));

        // Placed raw buffer view: sets SLOT_IN_BUFFERS and queues in buffer_changes.
        let mut buf_desc = [0u32; 16];
        // Format R32_TYPELESS (39), dim BUFFER (1), mapping 5768, count 16, flags RAW (1).
        (
            buf_desc[0],
            buf_desc[1],
            buf_desc[2],
            buf_desc[6],
            buf_desc[8],
        ) = (39, 1, 5768, 16, 1);
        (buf_desc[10], buf_desc[11]) = (0x1000, 0);
        let buf_view = View::Placed(
            false,
            [1, 0, 0, 0, 256, 0, 1, 0x1_0001, 0, 1, 0, 1, 0, 0],
            buf_desc,
        );
        assert!(heap.note_buffer(3, Some(&buf_view), false, true).is_empty());
        assert!(heap.slot_may(3, SLOT_IN_BUFFERS));
        assert!(!heap.slot_may(3, SLOT_IN_TEXTURES));
        assert!(!heap.slot_may(3, SLOT_IN_PLACED));
        assert_eq!(heap.buffer_slots(), [3]);
        assert_eq!(heap.buffer_changes(), [3]);

        // Re-noting with record_change = false (as `mirror_heap_buffer` does)
        // keeps slot 3 in `buffers` without re-queuing it in `changed`.
        assert!(heap.note_buffer(3, Some(&buf_view), true, false).is_empty());
        assert!(heap.buffer_changes().is_empty());
        assert_eq!(heap.buffer_slots(), [3]);
        assert!(heap.slot_may(3, SLOT_IN_BUFFERS));

        // Clearing slot 3 removes it from `buffers`, clears SLOT_IN_BUFFERS,
        // and queues one removal change; clearing it again queues nothing.
        assert!(heap.note_buffer(3, None, false, true).is_empty());
        assert!(!heap.slot_may(3, SLOT_IN_BUFFERS));
        assert!(heap.buffer_slots().is_empty());
        assert_eq!(heap.buffer_changes(), [3]);
        assert!(heap.note_buffer(3, None, false, true).is_empty());
        assert!(heap.buffer_changes().is_empty());

        // `mirror_heap_buffer` on a descriptor record written via `mirror(..., false)`
        // does not re-queue the slot in `buffer_changes`.
        let mut slots = [[0u32; 8]; 8];
        let handle = slots[4].as_mut_ptr() as u64;
        super::super::record(handle, buf_view.clone());
        let _ = heap.mirror(4, handle, false);
        assert_eq!(heap.buffer_changes(), [4]);
        let _ = heap.mirror_heap_buffer(4, handle);
        assert!(heap.buffer_changes().is_empty());

        // Placed texture view: sets SLOT_IN_TEXTURES and SLOT_IN_PLACED, and
        // recheck / overwrite keep both flags consistent.
        let at = 0x7fed_0000_0000u64;
        let tex_view = |width: u32| {
            let mut desc = [0u32; 16];
            (desc[10], desc[11]) = (at as u32, (at >> 32) as u32);
            View::Placed(
                false,
                [3, 0, 0, 0, width, 0, 64, 0x1_0001, 98, 1, 0, 0x105, 0, 0],
                desc,
            )
        };
        assert!(
            heap.note_buffer(1, Some(&tex_view(64)), false, true)
                .is_empty()
        );
        assert!(heap.slot_may(1, SLOT_IN_TEXTURES));
        assert!(heap.slot_may(1, SLOT_IN_PLACED));
        let (_, write) = heap.texture_slots_due(u64::MAX, 8).0[0];
        heap.texture_slot_done(1, write, None);
        heap.texture_slot_recheck(1, 10, (at, 64), 0x1234, false);
        assert!(heap.slot_may(1, SLOT_IN_TEXTURES));
        // A wider texture in slot 2 makes slot 1 stale and clears both flags on slot 1.
        assert_eq!(heap.note_buffer(2, Some(&tex_view(128)), false, true), [1]);
        assert!(!heap.slot_may(1, SLOT_IN_TEXTURES));
        assert!(!heap.slot_may(1, SLOT_IN_PLACED));
        assert!(heap.slot_may(2, SLOT_IN_TEXTURES));
        assert!(heap.slot_may(2, SLOT_IN_PLACED));
        // Overwriting slot 2 with None clears both flags and removes it from both sets.
        assert!(heap.note_buffer(2, None, false, true).is_empty());
        assert!(!heap.slot_may(2, SLOT_IN_TEXTURES));
        assert!(!heap.slot_may(2, SLOT_IN_PLACED));
        assert!(heap.texture_slots_due(u64::MAX, 8).0.is_empty());
    }
}

#[cfg(test)]
mod record_print_tests {
    use super::record_print;

    /// Equal records print alike; a record that differs in any one byte,
    /// or in its length, prints otherwise.
    #[test]
    fn a_record_that_changed_prints_otherwise() {
        let record: Vec<u8> = (0..32u8)
            .map(|i| i.wrapping_mul(37).wrapping_add(5))
            .collect();
        let print = record_print(&record);
        assert_eq!(print, record_print(&record.clone()));
        for at in 0..32 {
            for bit in 0..8 {
                let mut other = record.clone();
                other[at] ^= 1 << bit;
                assert_ne!(record_print(&other), print, "byte {at} bit {bit}");
            }
        }
        assert_ne!(record_print(&record[..16]), print);
        assert_ne!(record_print(&[0u8; 32]), record_print(&[0u8; 16]));
        // A record of zeros is told from "never looked at" by the caller's `| 1`.
        assert_ne!(record_print(&[0u8; 32]) | 1, 0);
    }
}

#[cfg(test)]
mod own_copy_tests {
    use super::{compact, join_ranges, own_bytes_if};

    /// A buffer view gets a copy of its own bytes only when it has no host
    /// view on its buffer's copy: off that copy's elements, and not a
    /// structured view that can be a raw view of the same bytes.
    #[test]
    fn a_view_off_its_copys_elements_gets_its_own_bytes() {
        let base = 0xd_9000_0000u64;
        // On an element: the buffer's copy (the two cases of every run's
        // log are the ones below).
        assert_eq!(
            own_bytes_if(true, 24, base, 3, 10, 12, 0, false, false),
            None
        );
        assert_eq!(
            own_bytes_if(true, 0, base, 0, 10, 12, 0, false, false),
            None
        );
        // A typed view of 12-byte elements, 5,222,240 bytes into its copy
        // (8 past an element): its own bytes, from its first element.
        assert_eq!(
            own_bytes_if(true, 5_222_240, base, 3, 10, 12, 0, false, false),
            Some((base + 36, 120))
        );
        // A structured view of 32-byte elements 8 bytes in: not on 16, so
        // no raw view of the copy either.
        assert_eq!(
            own_bytes_if(true, 8, base, 2, 4, 32, 32, true, false),
            Some((base + 64, 128))
        );
        // The same 16 bytes in: off its elements, but a raw view of the
        // copy's bytes does (as before).
        assert_eq!(
            own_bytes_if(true, 16, base, 2, 4, 32, 32, true, false),
            None
        );
        // Raw buffer views require 16-byte alignment (`(offset + first * 4) % 16 == 0`).
        // 8-byte-aligned raw views (`offset=8, first=0` or `offset=0, first=2`) need their own copy.
        assert_eq!(
            own_bytes_if(true, 8, base, 0, 16, 4, 0, false, true),
            Some((base, 64))
        );
        assert_eq!(
            own_bytes_if(true, 0, base, 2, 16, 4, 0, false, true),
            Some((base + 8, 64))
        );
        assert_eq!(
            own_bytes_if(true, 16, base, 2, 32, 4, 0, false, true),
            Some((base + 8, 128))
        );
        // 16-byte-aligned raw views fit on the existing host copy.
        assert_eq!(own_bytes_if(true, 16, base, 0, 16, 4, 0, false, true), None);
        assert_eq!(own_bytes_if(true, 8, base, 2, 16, 4, 0, false, true), None);
        // No elements, no size, or the switch off: as before.
        assert_eq!(own_bytes_if(true, 8, base, 2, 0, 32, 32, true, false), None);
        assert_eq!(own_bytes_if(true, 8, base, 2, 4, 0, 0, false, false), None);
        assert_eq!(
            own_bytes_if(false, 8, base, 2, 4, 32, 32, true, false),
            None
        );
        assert_eq!(own_bytes_if(false, 8, base, 0, 16, 4, 0, false, true), None);
    }

    /// A full list of changed ranges is compacted in place, never emptied:
    /// every original invalidation remains covered.
    #[test]
    fn a_full_change_list_keeps_every_change() {
        let mut touching = vec![
            (40, 50),
            (30, 40),
            (35, 40),
            (10, 20),
            (45, 60),
            (60, 61),
            (1, 1),
        ];
        join_ranges(&mut touching, 0);
        assert_eq!(touching, [(10, 20), (30, 61)]);

        // Nearby ranges within 64 KiB collapse into one range.
        let mut nearby: Vec<(u64, u64)> = (0..10u64).map(|i| (i * 4096, i * 4096 + 1024)).collect();
        compact(&mut nearby, 8);
        assert!(nearby.len() <= 4);
        for i in 0..10u64 {
            assert!(
                nearby
                    .iter()
                    .any(|r| r.0 <= i * 4096 && i * 4096 + 1024 <= r.1),
                "nearby change {i} lost"
            );
        }

        // Sparse ranges > 64 KiB apart pair-merge until <= most / 2, keeping every change covered.
        let mut sparse: Vec<(u64, u64)> = (0..10u64)
            .map(|i| (i * (1 << 20), i * (1 << 20) + 4096))
            .collect();
        compact(&mut sparse, 8);
        assert!(sparse.len() <= 4);
        for i in 0..10u64 {
            let start = i * (1 << 20);
            assert!(
                sparse.iter().any(|r| r.0 <= start && start + 4096 <= r.1),
                "sparse change {i} lost"
            );
        }
    }
}

#[cfg(test)]
mod dynamic_bits_tests {
    use super::DynamicBits;

    #[test]
    fn find_set_and_clear_across_word_boundaries_and_wrap() {
        let mut bits = DynamicBits::new(200);
        assert_eq!(bits.find_run(0, 0), None);
        assert_eq!(bits.find_run(0, 201), None);
        assert_eq!(bits.find_run(0, 130), Some(0));
        bits.set_taken(0, 130);
        assert_eq!(bits.find_run(130, 70), Some(130));
        bits.set_taken(130, 65);
        // Only 5 free at the end (195..200).
        assert_eq!(bits.find_run(195, 10), None);
        // Free a run crossing the 64-bit word boundary (60..70) and check wrap.
        assert_eq!(bits.clear_taken(60, 10), 10);
        assert_eq!(bits.clear_taken(60, 10), 0);
        assert_eq!(bits.find_run(195, 10), Some(60));
        bits.set_taken(60, 10);
        // Free a run spanning cursor=65 (62..68) and verify wrap-around finds it.
        assert_eq!(bits.clear_taken(62, 6), 6);
        assert_eq!(bits.find_run(65, 6), Some(62));
    }

    #[test]
    fn release_clears_kept_resources_and_wakes_blocked_dynamic_waiter() {
        let Some(h) = super::host() else {
            return;
        };
        let Some(heap) = super::HostHeap::new(false, 4) else {
            return;
        };
        let Some((buf, _)) = h.upload_buffer(256) else {
            return;
        };
        let slot = heap.dynamic(1).unwrap();
        heap.raw_at(slot, &buf, 0, 256);
        assert!(
            heap.kept[slot as usize]
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_some()
        );
        heap.release(heap.id(), slot, 1);
        assert!(
            heap.kept[slot as usize]
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_none(),
            "releasing a dynamic slot must not leave stale ID3D12Resource refs pinned in kept"
        );

        // Fill the ring and verify `release` wakes a blocked `dynamic()` waiter via Condvar.
        let heap = std::sync::Arc::new(heap);
        let fill_a = heap.dynamic(heap.dynamic - 17).unwrap();
        let fill_b = heap.dynamic(16).unwrap();
        let waiter_heap = std::sync::Arc::clone(&heap);
        let handle = std::thread::spawn(move || waiter_heap.dynamic(16));
        std::thread::sleep(std::time::Duration::from_millis(5));
        heap.release(heap.id(), fill_b, 16);
        let got = handle.join().unwrap();
        assert_eq!(got, Some(fill_b));
        heap.release(heap.id(), fill_a, heap.dynamic - 17);
        heap.release(heap.id(), fill_b, 16);
    }

    /// Every host sampler heap backs all 2,048 hardware descriptors and
    /// reserves slot 2,047 as a null sentinel so `umin(index, 2047)` in
    /// `victory-shader` never reads past smaller title sampler heaps.
    #[test]
    fn sampler_heaps_back_2048_slots_and_reserve_null_sentinel() {
        let Some(small) = super::HostHeap::new(true, 16) else {
            return;
        };
        // SAFETY: live heap query.
        assert_eq!(unsafe { small.heap.GetDesc().NumDescriptors }, 2048);
        assert_eq!(small.count, 16);
        let Some(full) = super::HostHeap::new(true, 2048) else {
            return;
        };
        // SAFETY: live heap query.
        assert_eq!(unsafe { full.heap.GetDesc().NumDescriptors }, 2048);
        assert_eq!(full.count, 2047);
    }

    #[test]
    fn show_prefilled_widens_texture2d_view_to_level_zero() {
        use windows::Win32::Graphics::Direct3D12::{
            D3D12_SHADER_RESOURCE_VIEW_DESC, D3D12_SHADER_RESOURCE_VIEW_DESC_0,
            D3D12_SRV_DIMENSION_TEXTURE2D, D3D12_TEX2D_SRV,
        };
        let mut desc = D3D12_SHADER_RESOURCE_VIEW_DESC {
            ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2D,
            Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                Texture2D: D3D12_TEX2D_SRV {
                    MostDetailedMip: 3,
                    MipLevels: 6,
                    PlaneSlice: 0,
                    ResourceMinLODClamp: 3.0,
                },
            },
            ..Default::default()
        };
        super::show_prefilled(&mut desc, 3);
        // SAFETY: Texture2D variant set above.
        let t = unsafe { desc.Anonymous.Texture2D };
        assert_eq!(t.MostDetailedMip, 0);
        assert_eq!(t.MipLevels, 9);
        assert_eq!(t.ResourceMinLODClamp, 0.0);

        // A view starting past the prefilled range is left unchanged.
        let mut untouched = D3D12_SHADER_RESOURCE_VIEW_DESC {
            ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2D,
            Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                Texture2D: D3D12_TEX2D_SRV {
                    MostDetailedMip: 4,
                    MipLevels: 5,
                    PlaneSlice: 0,
                    ResourceMinLODClamp: 4.0,
                },
            },
            ..Default::default()
        };
        super::show_prefilled(&mut untouched, 3);
        // SAFETY: Texture2D variant set above.
        let t2 = unsafe { untouched.Anonymous.Texture2D };
        assert_eq!(t2.MostDetailedMip, 4);
        assert_eq!(t2.MipLevels, 5);
    }

    /// Placed unordered texture views (2D and 3D) mirror onto host unordered
    /// access textures when written into a shader-visible heap, reuse an
    /// existing target at the same address, and feed shader views placed at
    /// that address in another format through `texture::alias`.
    #[test]
    fn placed_unordered_texture_view_mirrors_and_aliases_to_shader_view() {
        use super::*;

        let Some(host) = host() else {
            return; // no host GPU
        };
        let Some(heap) = HostHeap::new(false, 8) else {
            return;
        };

        // 1. 3D volume UAV (4x4x4 R32_UINT, resource flags = 0) mirrored via
        // `HostHeap::mirror(..., for_draw = false)` as a shader-visible heap
        // copy does.
        let vol_at = 0x7fed_1000_0000u64;
        let vol_res = [4, 0, 0, 0, 4, 0, 4, 0x1_0004, 42, 1, 0, 0, 0, 0];
        let mut vol_uav = [0u32; 16];
        (vol_uav[0], vol_uav[1], vol_uav[4]) = (42, 8, 4);
        (vol_uav[10], vol_uav[11]) = (vol_at as u32, (vol_at >> 32) as u32);
        let slot_mem = [0u64; 4];
        let slot_handle = slot_mem.as_ptr() as u64;
        super::super::create_placed_uav(std::ptr::null(), &vol_res, &vol_uav, slot_handle);
        assert!(heap.mirror(0, slot_handle, false).is_some());
        {
            let kept = heap.kept[0].lock().unwrap();
            let tex = kept
                .as_ref()
                .expect("3D placed UAV must mirror immediately");
            // SAFETY: live D3D12 resource.
            let d = unsafe { tex.GetDesc() };
            assert_eq!(d.Dimension, D3D12_RESOURCE_DIMENSION_TEXTURE3D);
            assert_ne!(d.Flags.0 & D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS.0, 0);
        }
        // SAFETY: plain device query.
        assert!(unsafe { host.device.GetDeviceRemovedReason() }.is_ok());

        // Another placed UAV at the same address with compatible format reuses
        // the resource already recorded in `TARGETS_AT`.
        let Some(View::Unordered(first_vol, _, _)) =
            placed_texture(&View::Placed(true, vol_res, vol_uav))
        else {
            panic!("expected View::Unordered");
        };
        let mut explicit_flags = vol_res;
        explicit_flags[12] = 0x4;
        let Some(View::Unordered(second_vol, _, _)) =
            placed_texture(&View::Placed(true, explicit_flags, vol_uav))
        else {
            panic!("expected View::Unordered");
        };
        assert_eq!(
            first_vol, second_vol,
            "must reuse existing UAV texture at address"
        );

        // 2. Cross-format aliasing: a 4x4 R9G9B9E5_SHAREDEXP (67) description
        // written through an R32_UINT (42) placed UAV and then read through an
        // R9G9B9E5_SHAREDEXP placed SRV (even if `ALLOW_UNORDERED_ACCESS` was
        // left on the SRV's resource description) copies the GPU texels across.
        let alias_at = 0x7fed_2000_0000u64;
        let rgb9e5_res = [3, 0, 0, 0, 4, 0, 4, 0x1_0001, 67, 1, 0, 0, 0x4, 0];
        let mut uav_desc = [0u32; 16];
        (uav_desc[0], uav_desc[1]) = (42, 4);
        (uav_desc[10], uav_desc[11]) = (alias_at as u32, (alias_at >> 32) as u32);
        let uav_placed = View::Placed(true, rgb9e5_res, uav_desc);
        let Some(View::Unordered(uav_obj, _, _)) = placed_texture(&uav_placed) else {
            panic!("expected placed UAV texture");
        };
        assert!(make(&uav_placed, heap.cpu(1)).and_then(|k| k.0).is_some());

        let mut srv_words = [0u32; 16];
        (srv_words[0], srv_words[1], srv_words[2], srv_words[5]) =
            (67, 4, D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING, 1);
        (srv_words[10], srv_words[11]) = (alias_at as u32, (alias_at >> 32) as u32);
        let srv_placed = View::Placed(false, rgb9e5_res, srv_words);
        let Some(View::Shader(srv_obj, _)) = placed_texture(&srv_placed) else {
            panic!("expected placed SRV texture");
        };
        assert!(make(&srv_placed, heap.cpu(2)).and_then(|k| k.0).is_some());
        assert_eq!(
            crate::resource::alias_of(srv_obj as crate::object::This).map(|h| h.get()),
            Some(uav_obj as crate::object::This),
            "placed SRV must find the placed UAV texture in TARGETS_AT"
        );

        // Upload a known R32_UINT pattern into `uav_obj` on the GPU, run
        // `texture::alias` into `srv_obj`, and read `srv_obj` back.
        let uav_tex = crate::resource::host_resource(uav_obj as crate::object::This).unwrap();
        let srv_tex = crate::resource::host_resource(srv_obj as crate::object::This).unwrap();
        let pattern = 0x7c40_8210u32;
        let make_buf = |heap_type, state| {
            let hp = D3D12_HEAP_PROPERTIES {
                Type: heap_type,
                ..Default::default()
            };
            let rd = D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                Width: 1024,
                Height: 1,
                DepthOrArraySize: 1,
                MipLevels: 1,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                ..Default::default()
            };
            let mut out: Option<ID3D12Resource> = None;
            // SAFETY: valid buffer description.
            unsafe {
                host.device
                    .CreateCommittedResource(&hp, D3D12_HEAP_FLAG_NONE, &rd, state, None, &mut out)
                    .unwrap();
            }
            out.unwrap()
        };
        let upload = make_buf(D3D12_HEAP_TYPE_UPLOAD, D3D12_RESOURCE_STATE_GENERIC_READ);
        let readback = make_buf(D3D12_HEAP_TYPE_READBACK, D3D12_RESOURCE_STATE_COPY_DEST);
        // SAFETY: 1024-byte upload buffer mapped on CPU.
        unsafe {
            let mut ptr = std::ptr::null_mut();
            upload.Map(0, None, Some(&mut ptr)).unwrap();
            std::slice::from_raw_parts_mut(ptr.cast::<u32>(), 256).fill(pattern);
            upload.Unmap(0, None);
        }

        let list = host.new_list().unwrap();
        crate::list::ensure_state(&list, uav_obj as crate::object::This, 0x400); // COPY_DEST
        let buf_loc = |buf: &ID3D12Resource, format| D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: borrowed pointer not released by the copy location.
            pResource: unsafe { std::mem::transmute_copy(buf) },
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                    Offset: 0,
                    Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                        Format: format,
                        Width: 4,
                        Height: 4,
                        Depth: 1,
                        RowPitch: 256,
                    },
                },
            },
        };
        let tex_loc = |tex: &ID3D12Resource| D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: borrowed pointer not released by the copy location.
            pResource: unsafe { std::mem::transmute_copy(tex) },
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                SubresourceIndex: 0,
            },
        };
        // SAFETY: open list and live resources in matching copy states.
        unsafe {
            list.list.CopyTextureRegion(
                &tex_loc(&uav_tex),
                0,
                0,
                0,
                &buf_loc(&upload, DXGI_FORMAT_R32_UINT),
                None,
            );
        }
        crate::list::ensure_state(&list, uav_obj as crate::object::This, 0x8); // UNORDERED_ACCESS
        assert!(crate::texture::alias(
            &list,
            srv_obj as crate::object::This,
            uav_obj as crate::object::This
        ));
        crate::list::ensure_state(&list, srv_obj as crate::object::This, 0x800); // COPY_SOURCE
        // SAFETY: open list and `srv_tex` transitioned to `COPY_SOURCE`.
        unsafe {
            list.list.CopyTextureRegion(
                &buf_loc(&readback, DXGI_FORMAT_R9G9B9E5_SHAREDEXP),
                0,
                0,
                0,
                &tex_loc(&srv_tex),
                None,
            );
        }
        list.close();
        host.execute(&[&list]);

        // SAFETY: GPU execution finished; readback holds 1024 bytes.
        let got = unsafe {
            let mut ptr = std::ptr::null_mut();
            readback.Map(0, None, Some(&mut ptr)).unwrap();
            let word = *ptr.cast::<u32>();
            readback.Unmap(0, None);
            word
        };
        assert_eq!(got, pattern);
        // SAFETY: plain device query.
        assert!(unsafe { host.device.GetDeviceRemovedReason() }.is_ok());
    }
}
