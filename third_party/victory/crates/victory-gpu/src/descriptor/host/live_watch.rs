//! (phonescreen, #115) Live textures in memory the card writes.
//!
//! A texture picked by index that changes after its upload is kept live
//! (`VICTORY_LIVE_TEXTURES`): looked at every frame and uploaded again from
//! title memory at each change. In batch 5 the diner's table shot showed
//! the phone screen black with it and lit (blue, no app) with
//! `VICTORY_LIVE_TEXTURES=0`. The suspicion: the screen's memory is one the
//! card writes (a target the phone's UI is drawn into, a texture the host
//! copies into, a buffer view shaders write and whose bytes go back to
//! title memory). Title memory never holds what the card drew there, so
//! every live upload lays the title's bytes over it.
//!
//! `VICTORY_LIVE_TEXTURES_DIAG=1` (live): one `live textures:` line per
//! slot found changed at 1, 2, 4, 8, ... changes (at most 400 lines): its
//! address, bytes, shape, how often it changed and was uploaded, whether
//! the card writes that memory and how, and the share of zero bytes in it.
//! A summary line every 300 frames.
//!
//! `VICTORY_LIVE_TEXTURES_SKIP_GPU_WRITTEN=1` (live): a look that finds
//! such a slot changed in memory the card writes leaves it as it is: not
//! uploaded again from title memory, not live, not looked at again (a
//! descriptor write queues it anew).

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use victory_core::settings::Live;

use super::{Recheck, TextureQueue};

static DIAG: Live = Live::flag("VICTORY_LIVE_TEXTURES_DIAG");
static SKIP: Live = Live::flag("VICTORY_LIVE_TEXTURES_SKIP_GPU_WRITTEN");

/// `VICTORY_LIVE_TEXTURES_DIAG=1`.
pub(crate) fn diag() -> bool {
    DIAG.get_bool()
}

/// `VICTORY_LIVE_TEXTURES_SKIP_GPU_WRITTEN=1`.
pub(crate) fn skip() -> bool {
    SKIP.get_bool()
}

/// How the card writes a span of title memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CardWrote {
    /// A texture the host renders to or writes (target, depth, unordered
    /// access) lies there, starting at this address.
    Target(u64),
    /// A texture the host copied or resolved into, at this address.
    Copied(u64),
    /// Shaders wrote a buffer view there, whose bytes went back to title
    /// memory.
    Buffer,
}

impl std::fmt::Display for CardWrote {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CardWrote::Target(at) => write!(f, "target@{at:#x}"),
            CardWrote::Copied(at) => write!(f, "copied@{at:#x}"),
            CardWrote::Buffer => f.write_str("buffer_uav"),
        }
    }
}

/// Block size of the buffer writes kept.
const BLOCK_SHIFT: u32 = 16;
/// The most blocks kept (64 GiB of memory); the set starts over past it.
const MOST_BLOCKS: usize = 1 << 20;

/// Buffer views shaders wrote, by (address, bytes) seen, and the 64 KiB
/// blocks they cover.
#[derive(Default)]
struct Written {
    ranges: HashSet<(u64, u64)>,
    blocks: HashSet<u64>,
}

static WRITTEN: Mutex<Option<Written>> = Mutex::new(None);

/// Shaders write the buffer view `[address, address + len)` (its bytes go
/// back to title memory): kept while either switch is on.
pub(crate) fn uav_wrote(address: u64, len: u64) {
    if !diag() && !skip() {
        return;
    }
    note_written(address, len);
}

fn note_written(address: u64, len: u64) {
    if len == 0 {
        return;
    }
    let mut w = WRITTEN.lock().unwrap_or_else(|e| e.into_inner());
    let w = w.get_or_insert_with(Written::default);
    if !w.ranges.insert((address, len)) {
        return;
    }
    let (first, last) = (
        address >> BLOCK_SHIFT,
        address.saturating_add(len - 1) >> BLOCK_SHIFT,
    );
    if w.blocks.len() + (last - first + 1) as usize > MOST_BLOCKS {
        w.blocks.clear();
        w.ranges.clear();
        w.ranges.insert((address, len));
    }
    w.blocks.extend(first..=last);
}

/// Whether a buffer view shaders wrote covers part of `[at, at + len)`.
fn buffer_written(at: u64, len: u64) -> bool {
    if len == 0 {
        return false;
    }
    let w = WRITTEN.lock().unwrap_or_else(|e| e.into_inner());
    let Some(w) = w.as_ref() else {
        return false;
    };
    let (first, last) = (at >> BLOCK_SHIFT, at.saturating_add(len - 1) >> BLOCK_SHIFT);
    (first..=last).any(|b| w.blocks.contains(&b))
}

/// How the card writes `[at, at + len)`, if it does.
fn card_wrote(at: u64, len: u64) -> Option<CardWrote> {
    if let Some(start) = crate::resource::placed_inside::target_over(at, len) {
        return Some(CardWrote::Target(start));
    }
    if let Some((start, _)) = crate::texture::in_host_written(at, len, 0) {
        return Some(CardWrote::Copied(start));
    }
    buffer_written(at, len).then_some(CardWrote::Buffer)
}

/// [`card_wrote`] of a slot found changed at `at` (address, bytes), asked
/// only while either switch is on.
pub(crate) fn card_if_asked(at: (u64, u64)) -> Option<CardWrote> {
    if !diag() && !skip() {
        return None;
    }
    card_wrote(at.0, at.1)
}

/// What a look that found a slot changed does with it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Changed {
    /// Due now and uploaded again from title memory (live with
    /// `VICTORY_LIVE_TEXTURES`), as before.
    Upload,
    /// Left as it is: the card writes its memory.
    Leave,
}

/// [`Changed`] of a slot found changed, with `skip` the switch and `card`
/// how the card writes its memory.
pub(crate) fn on_change(skip: bool, card: Option<CardWrote>) -> Changed {
    if skip && card.is_some() {
        Changed::Leave
    } else {
        Changed::Upload
    }
}

impl TextureQueue {
    /// A look found slot `index` (`seen`) changed in memory the card
    /// writes, with `VICTORY_LIVE_TEXTURES_SKIP_GPU_WRITTEN`: it leaves the
    /// looks and is not live. Returns whether it was so (a slot written or
    /// uploaded again since is left alone).
    pub(super) fn card_written(&mut self, index: u32, seen: &Recheck) -> bool {
        if self.rechecks.get(&index) != Some(seen) {
            return false;
        }
        self.rechecks.remove(&index);
        self.live.remove(&index);
        true
    }
}

/// What the diagnostic knows of a slot.
#[derive(Default)]
struct Slot {
    address: u64,
    bytes: u64,
    first: u64,
    changes: u64,
    uploads: u64,
    shape: Option<[u32; 4]>,
    left: u64,
}

static SLOTS: Mutex<Option<HashMap<u32, Slot>>> = Mutex::new(None);
static LINES: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
const MOST_LINES: u32 = 400;

/// Share of zero bytes in `[at, at + len)` (up to 4 MiB of it), read
/// through the kernel (pages not there count as zeros).
fn zero_share(at: u64, len: u64) -> f64 {
    const CHUNK: usize = 64 << 10;
    let len = len.min(4 << 20) as usize;
    if len == 0 {
        return 0.0;
    }
    let mut read = vec![0u8; CHUNK];
    let (mut from, mut zeros) = (0usize, 0usize);
    while from < len {
        let bytes = CHUNK.min(len - from);
        read[..bytes].fill(0);
        crate::host::copy_mapped(
            read.as_mut_ptr(),
            (at as usize + from) as *const u8,
            bytes,
            false,
        );
        zeros += read[..bytes].iter().filter(|&&b| b == 0).count();
        from += bytes;
    }
    zeros as f64 / len as f64
}

/// With the diagnostic: a look in `frame` found slot `index` (`seen`)
/// changed; `card` how the card writes its memory; `left` whether the
/// slot was left as it is ([`Changed::Leave`]).
pub(crate) fn changed(index: u32, seen: &Recheck, frame: u64, card: Option<CardWrote>, left: bool) {
    if !diag() {
        return;
    }
    let (changes, uploads, first, shape, lefts) = {
        let mut slots = SLOTS.lock().unwrap_or_else(|e| e.into_inner());
        let s = slots
            .get_or_insert_with(HashMap::new)
            .entry(index)
            .or_insert_with(|| Slot {
                first: frame,
                ..Slot::default()
            });
        if (s.address, s.bytes) != seen.at {
            *s = Slot {
                address: seen.at.0,
                bytes: seen.at.1,
                first: frame,
                shape: s.shape,
                ..Slot::default()
            };
        }
        s.changes += 1;
        s.left += u64::from(left);
        (s.changes, s.uploads, s.first, s.shape, s.left)
    };
    if !changes.is_power_of_two() {
        return;
    }
    if LINES.fetch_add(1, std::sync::atomic::Ordering::Relaxed) >= MOST_LINES {
        return;
    }
    let [width, height, format, levels] = shape.unwrap_or([0; 4]);
    let span = frame.saturating_sub(first).max(1);
    tracing::warn!(
        slot = index,
        address = format_args!("{:#x}", seen.at.0),
        bytes = seen.at.1,
        width,
        height,
        format,
        levels,
        live = seen.live,
        looks = seen.looks,
        frames_after_upload = frame.saturating_sub(seen.uploaded),
        changes,
        uploads,
        left_alone = lefts,
        per_100_frames = format_args!("{:.1}", changes as f64 * 100.0 / span as f64),
        card_wrote = card.map_or_else(|| "none".to_string(), |c| c.to_string()),
        zero_share = format_args!("{:.3}", zero_share(seen.at.0, seen.at.1)),
        frame,
        "live textures: a texture picked by index changed after its upload (card_wrote: the card writes that memory, so title memory lacks what it drew)"
    );
}

/// With the diagnostic: slot `index`, live, was uploaded again from title
/// texture `resource`.
pub(crate) fn uploaded(index: u32, resource: crate::object::This) {
    if !diag() {
        return;
    }
    let shape = crate::resource::texture_memory(resource).map(|(_, d)| d.shape());
    let mut slots = SLOTS.lock().unwrap_or_else(|e| e.into_inner());
    let s = slots
        .get_or_insert_with(HashMap::new)
        .entry(index)
        .or_default();
    s.uploads += 1;
    if shape.is_some() {
        s.shape = shape;
    }
}

/// With the diagnostic, every 300 frames: live slots now, slots seen
/// changing, and of them left alone.
pub(crate) fn summary(frame: u64, live: usize, waiting: usize) {
    if !diag() || !frame.is_multiple_of(300) {
        return;
    }
    let (seen, changes, uploads, left) = {
        let slots = SLOTS.lock().unwrap_or_else(|e| e.into_inner());
        slots.as_ref().map_or((0, 0, 0, 0), |m| {
            m.values().fold((m.len(), 0, 0, 0), |a, s| {
                (a.0, a.1 + s.changes, a.2 + s.uploads, a.3 + s.left)
            })
        })
    };
    tracing::info!(
        frame,
        live,
        looked_at = waiting,
        slots_changed = seen,
        changes,
        live_uploads = uploads,
        left_alone = left,
        "live textures: summary"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_switches_are_live() {
        victory_core::settings::check_live("VICTORY_LIVE_TEXTURES_DIAG", "1", diag, false, true);
        victory_core::settings::check_live(
            "VICTORY_LIVE_TEXTURES_SKIP_GPU_WRITTEN",
            "1",
            skip,
            false,
            true,
        );
    }

    /// The failure of #115 as the queue sees it: a slot whose memory the
    /// card writes changes after its upload (the title's bytes there move,
    /// what the card drew does not reach them). Without the switch it is
    /// due at once, live, and uploaded from title memory over the host's
    /// contents at every change; with it, it is left as it is.
    #[test]
    fn a_change_in_card_written_memory_is_uploaded_only_without_the_switch() {
        let card = Some(CardWrote::Target(0x1000_0000));
        assert_eq!(on_change(false, card), Changed::Upload);
        assert_eq!(on_change(true, None), Changed::Upload);
        assert_eq!(on_change(true, card), Changed::Leave);
        assert_eq!(on_change(true, Some(CardWrote::Buffer)), Changed::Leave);

        let at = (0x1000_0000, 2 << 20);
        // Off: found changed, it goes live and is due now.
        let mut q = TextureQueue::default();
        q.recheck(5, 10, at, 0xabc, false);
        let (index, seen) = q.rechecks_due(14, 8)[0];
        q.rechecked(index, &seen, true, 14, true, None);
        assert!(q.live.contains(&5));
        assert_eq!(q.due(14, 8), vec![(5, q.writes)]);
        // A live slot found changed again in the next frames: due again
        // each time (an upload of title memory per change).
        let writes = q.writes;
        q.done(5, writes);
        q.recheck(5, 14, at, 0xabd, true);
        let (_, seen) = q.rechecks_due(15, 8)[0];
        assert!(seen.live);
        q.rechecked(5, &seen, true, 15, true, None);
        assert_eq!(q.due(15, 8).len(), 1);

        // On: the same look leaves it alone, not live, not looked at.
        let mut q = TextureQueue::default();
        q.recheck(5, 10, at, 0xabc, true);
        q.live.insert(5);
        let (index, seen) = q.rechecks_due(11, 8)[0];
        assert_eq!(on_change(true, card), Changed::Leave);
        assert!(q.card_written(index, &seen));
        assert!(q.due(11, 8).is_empty());
        assert!(q.rechecks_due(1000, 8).is_empty());
        assert!(!q.live.contains(&5));
        // A write of the slot queues it again as any other.
        q.written(5, 12, 12, false);
        assert_eq!(q.due(12, 8).len(), 1);
    }

    /// A look seen for an older upload does not take out a newer one.
    #[test]
    fn a_stale_look_is_left_alone() {
        let mut q = TextureQueue::default();
        q.recheck(3, 10, (0x2000, 4096), 7, false);
        let (_, seen) = q.rechecks_due(14, 8)[0];
        q.written(3, 12, 12, false);
        let writes = q.writes;
        q.done(3, writes);
        q.recheck(3, 13, (0x2000, 4096), 8, false);
        assert!(!q.card_written(3, &seen));
        assert_eq!(q.rechecks.len(), 1);
    }

    #[test]
    fn buffer_writes_are_found_by_block() {
        note_written(0x7_0000_0000, 0x2_0000);
        assert!(buffer_written(0x7_0001_0000, 16));
        assert!(buffer_written(0x6_ffff_0000, 0x1_0001));
        assert!(!buffer_written(0x7_0002_0000, 0x1_0000));
        assert!(!buffer_written(0x7_0001_0000, 0));
        // The same range again changes nothing.
        note_written(0x7_0000_0000, 0x2_0000);
        assert!(buffer_written(0x7_0000_0000, 1));
    }

    #[test]
    fn the_card_names_read_well() {
        assert_eq!(CardWrote::Target(0x10).to_string(), "target@0x10");
        assert_eq!(CardWrote::Copied(0x20).to_string(), "copied@0x20");
        assert_eq!(CardWrote::Buffer.to_string(), "buffer_uav");
    }
}
