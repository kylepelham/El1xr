//! (heapcap) `VICTORY_HEAP_CAP=5`: packed views follow what shaders read.
//!
//! A whole-heap table entry with `FAULT_FLAG` set makes a shader that reads
//! it note the title slot in the fault buffer (a raw UAV at the heap's
//! fault slot, `heap_cap::fault_slot_of`; the shader still reads the host
//! slot in the entry's other bits). The host flags:
//! - title slots whose view is live but has no packed slot (overflow, or
//!   evicted): a note asks for one ([`Packed::noted`]: wanted, packed first
//!   next frame by [`Packed::take_wanted`]);
//! - shown packed views not known read for [`ARM_AFTER`] frames ("armed",
//!   [`Packed::arm`]): a note says a shader still reads it (touched; the
//!   flag comes off).
//!
//! When a wanted slot finds no free packed slot, a packed view no shader is
//! known to have read for [`EVICT_AFTER`] frames, armed for [`ARM_WAIT`]
//! frames with no note, gives its slot back ([`Packed::evict`]); the slot
//! is reused [`super::REUSE_AFTER`] frames later. A view whose read the host
//! saw within [`EVICT_AFTER`] frames, or that a shader read at all within
//! [`ARM_WAIT`] frames, is never evicted. A note of a dead slot takes the
//! flag off its entry.

use super::{Owner, Packed, Placed, Stage};
use windows::Win32::Graphics::Direct3D12::{
    D3D12_BUFFER_UAV, D3D12_BUFFER_UAV_FLAG_RAW, D3D12_CPU_DESCRIPTOR_HANDLE,
    D3D12_UAV_DIMENSION_BUFFER, D3D12_UNORDERED_ACCESS_VIEW_DESC,
    D3D12_UNORDERED_ACCESS_VIEW_DESC_0, ID3D12Resource,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R32_TYPELESS;

/// Frames a packed view is kept after a shader was last known to read it.
pub(crate) const EVICT_AFTER: u64 = 360;
/// Frames with no known read after which a shown view is armed.
pub(crate) const ARM_AFTER: u64 = 120;
/// Frames an armed view waits for a note before it may be evicted: a view a
/// shader read in that time is never evicted (a note takes 2-3 frames to
/// come back).
pub(crate) const ARM_WAIT: u64 = 120;
/// Views armed, wanted slots packed and notes read at most per frame.
const ARM_BUDGET: usize = 4096;
const WANT_BUDGET: usize = 4096;
/// Owners an arming or eviction look passes at most per call.
const LOOK: usize = 1 << 16;

/// What a fault note of a title slot meant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Noted {
    /// An armed packed view a shader read: point the entry at host slot `at`
    /// unflagged.
    Touched {
        entry: u32,
        at: u32,
    },
    /// Live with no packed view: queued to pack.
    Wanted,
    /// Its view is gone: the entry, null and unflagged.
    Dead {
        entry: u32,
    },
    Nothing,
}

impl Packed {
    /// A shader noted title slot `slot` (its record names a live view:
    /// `key`) at `frame`.
    pub(crate) fn noted(&mut self, slot: u32, key: Option<[u32; 3]>, frame: u64) -> Noted {
        // the host's own slots are never flagged
        let Some(entry) = self.entry(slot).filter(|_| slot >= self.title_null) else {
            return Noted::Nothing;
        };
        if let Some(&k) = self.map.get(&slot) {
            let base = self.base;
            let Some(owner) = self.owner[k as usize].as_mut() else {
                return Noted::Nothing;
            };
            owner.touched = frame;
            if owner.armed.take().is_some() && owner.stage == Stage::Shown {
                self.counts.touched += 1;
                return Noted::Touched {
                    entry,
                    at: base + k,
                };
            }
            return Noted::Nothing;
        }
        match key {
            Some(key) => {
                if self.wanted_set.insert(slot) {
                    self.wanted.push_back((slot, key));
                    self.counts.wanted += 1;
                }
                Noted::Wanted
            }
            None => {
                self.counts.dead += 1;
                Noted::Dead { entry }
            }
        }
    }

    /// Wanted slots packed at `frame`, at most `most`, oldest first: each
    /// make, or an eviction that frees a slot for later frames. With no slot
    /// free and none to evict, the rest wait.
    pub(crate) fn take_wanted(&mut self, frame: u64, most: usize) -> Vec<(u32, Placed)> {
        let mut out = Vec::new();
        while out.len() < most
            && let Some(&(slot, key)) = self.wanted.front()
        {
            match self.place(slot, Some(key), false, frame) {
                Placed::Overflow => {
                    // as many evicted as slots are wanted beyond those freed already; a
                    // freed slot waits `REUSE_AFTER`: packed in a later frame
                    while out.len() < most && self.wanted.len() > self.free.len() {
                        let Some((title, at)) = self.evict(frame) else {
                            break;
                        };
                        out.push((title, Placed::Drop(at)));
                    }
                    break;
                }
                placed => {
                    self.wanted.pop_front();
                    self.wanted_set.remove(&slot);
                    if let Some(&k) = self.map.get(&slot)
                        && let Some(owner) = self.owner[k as usize].as_mut()
                    {
                        owner.touched = frame;
                    }
                    out.push((slot, placed));
                }
            }
        }
        out
    }

    /// Whether `owner` may be evicted at `frame`.
    fn evictable(owner: &Owner, frame: u64) -> bool {
        match owner.stage {
            Stage::Failed => frame.saturating_sub(owner.touched) > EVICT_AFTER,
            Stage::Shown => {
                frame.saturating_sub(owner.touched) > EVICT_AFTER
                    && owner.armed.is_some_and(|at| frame >= at + ARM_WAIT)
            }
            Stage::Queued | Stage::Made => false,
        }
    }

    /// A packed view evicted at `frame`: its title slot and host slot, the
    /// slot free again [`super::REUSE_AFTER`] frames later.
    pub(crate) fn evict(&mut self, frame: u64) -> Option<(u32, u32)> {
        let n = self.owner.len();
        for _ in 0..n.min(LOOK) {
            let k = self.evict_hand;
            self.evict_hand = (self.evict_hand + 1) % n;
            let Some(owner) = self.owner[k] else { continue };
            if !Self::evictable(&owner, frame) {
                continue;
            }
            self.owner[k] = None;
            self.map.remove(&owner.title);
            self.free.push_back((k as u32, frame));
            self.counts.evicted += 1;
            return Some((owner.title, self.base + k as u32));
        }
        None
    }

    /// Shown views not known read for [`ARM_AFTER`] frames, armed at
    /// `frame`, at most `most`: their entries and host slots, to flag.
    pub(crate) fn arm(&mut self, frame: u64, most: usize) -> Vec<(u32, u32)> {
        let n = self.owner.len();
        let mut out = Vec::new();
        for _ in 0..n.min(LOOK) {
            if out.len() >= most {
                break;
            }
            let k = self.arm_hand;
            self.arm_hand = (self.arm_hand + 1) % n;
            let base = self.base;
            let Some(owner) = self.owner[k].as_mut() else {
                continue;
            };
            if owner.stage != Stage::Shown
                || owner.armed.is_some()
                || frame.saturating_sub(owner.touched) < ARM_AFTER
            {
                continue;
            }
            owner.armed = Some(frame);
            let title = owner.title;
            if let Some(entry) = self.entry(title) {
                out.push((entry, base + k as u32));
            }
        }
        self.counts.armed += out.len() as u64;
        out
    }
}

/// The fault buffer of a packing heap: a buffer shaders write and the CPU
/// reads (`FAULT_DWORDS` dwords, zeroed) and its mapping.
pub(super) fn fault_buffer() -> Option<(ID3D12Resource, usize)> {
    let host = crate::host::host()?;
    let bytes = u64::from(victory_shader::emit::FAULT_DWORDS) * 4;
    let (buffer, data) = host.shared_buffer(bytes)?;
    crate::host::name(&buffer, || {
        "heap cap: title view slots shaders asked for".to_owned()
    });
    // SAFETY: a fresh mapping of `bytes` bytes.
    unsafe { std::ptr::write_bytes(data, 0, bytes as usize) };
    Some((buffer, data as usize))
}

/// Makes `at` the raw UAV of `buffer` (`FAULT_DWORDS` dwords), or a null
/// raw UAV (stores dropped) without one.
pub(super) fn fault_view(buffer: Option<&ID3D12Resource>, at: D3D12_CPU_DESCRIPTOR_HANDLE) {
    let Some(host) = crate::host::host() else {
        return;
    };
    let desc = D3D12_UNORDERED_ACCESS_VIEW_DESC {
        Format: DXGI_FORMAT_R32_TYPELESS,
        ViewDimension: D3D12_UAV_DIMENSION_BUFFER,
        Anonymous: D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
            Buffer: D3D12_BUFFER_UAV {
                NumElements: victory_shader::emit::FAULT_DWORDS,
                Flags: D3D12_BUFFER_UAV_FLAG_RAW,
                ..Default::default()
            },
        },
    };
    // SAFETY: a slot of our heap; a null resource is allowed with a desc.
    unsafe {
        host.device
            .CreateUnorderedAccessView(buffer, None::<&ID3D12Resource>, Some(&desc), at)
    };
}

/// The title slots noted since the last call (each once), the buffer
/// cleared behind them.
fn take_notes(faults: usize) -> Vec<u32> {
    let mut out = Vec::new();
    let data = faults as *mut u32;
    for i in 1..victory_shader::emit::FAULT_DWORDS as usize {
        // SAFETY: inside the mapping; the GPU may be writing: a note missed
        // now is written again by the next read of a still flagged entry.
        let at = unsafe { data.add(i) };
        // SAFETY: as above.
        let slot = unsafe { at.read_volatile() };
        if slot != 0 {
            out.push(slot);
            // SAFETY: as above.
            unsafe { at.write_volatile(0) };
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// Once a frame from `refresh`, before its looks at written slots: the
/// notes taken (touched views unflagged, dead slots unflagged, live ones
/// wanted), wanted slots packed first, and views not known read armed.
pub(super) fn step(
    packed: &mut Packed,
    host: &super::super::HostHeap,
    start: u64,
    step: u64,
    frame: u64,
) {
    let notes = take_notes(packed.faults);
    packed.counts.notes += notes.len() as u64;
    let mut record = [0u8; 12];
    for slot in notes {
        let key =
            crate::host::read_into((start + u64::from(slot) * step) as *const u8, &mut record)
                .then(|| {
                    let word = |w: usize| {
                        u32::from_le_bytes(record[w * 4..w * 4 + 4].try_into().unwrap_or_default())
                    };
                    [word(0), word(1), word(2)]
                })
                .filter(|key| {
                    key[0] == super::super::super::VIEW_TAG
                        && super::super::super::view_at(key[1], key[2]).is_some()
                });
        match packed.noted(slot, key, frame) {
            Noted::Touched { entry, at } => packed.put(entry, at),
            Noted::Dead { entry } => packed.put(entry, packed.title_null),
            Noted::Wanted | Noted::Nothing => {}
        }
    }
    for (slot, placed) in packed.take_wanted(frame, WANT_BUDGET) {
        let Some(entry) = packed.entry(slot) else {
            continue;
        };
        match placed {
            Placed::Make { .. } => {
                let view = packed
                    .map
                    .get(&slot)
                    .and_then(|&k| packed.owner[k as usize])
                    .and_then(|o| super::super::super::view_at(o.key[1], o.key[2]));
                super::apply(packed, host, entry, placed, view);
            }
            Placed::Drop(at) => {
                // evicted, still live: asks again when read
                super::apply(packed, host, entry, placed, None);
                packed.put(entry, packed.unpacked(true));
                let _ = at;
            }
            Placed::Same | Placed::Overflow | Placed::Nothing => {}
        }
    }
    for (entry, at) in packed.arm(frame, ARM_BUDGET) {
        packed.put(entry, at | victory_shader::emit::FAULT_FLAG);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NULL_SLOT: u32 = 500_000;

    fn packed(cap: u32) -> Packed {
        Packed::new(1, NULL_SLOT, cap)
            .with_table(0, 0, victory_shader::emit::FULL_REMAP - 1)
            .with_demand(0)
    }

    /// Makes and shows every queued make, as the worker and the ready pass do.
    fn settle(p: &mut Packed, frame: u64) -> Vec<(u32, u32)> {
        let mut shown = Vec::new();
        let queued: Vec<(u32, u64)> = p
            .owner
            .iter()
            .enumerate()
            .filter_map(|(k, o)| {
                o.filter(|o| o.stage == Stage::Queued)
                    .map(|o| (p.base + k as u32, o.generation))
            })
            .collect();
        for (at, generation) in queued {
            p.made(at, generation, true);
        }
        for due in p.take_made(usize::MAX) {
            if let Some(entry) = p.show(&due) {
                shown.push((entry, due.at));
            }
        }
        let _ = frame;
        shown
    }

    #[test]
    fn a_noted_live_slot_is_packed_and_shown_by_the_next_frame() {
        let mut p = packed(4);
        let slot = 900_000;
        // frame 10: a shader read it (noted), the host packs it at once
        assert_eq!(p.noted(slot, Some([1, 9, 1]), 10), Noted::Wanted);
        assert_eq!(
            p.noted(slot, Some([1, 9, 1]), 10),
            Noted::Wanted,
            "noted twice, wanted once"
        );
        let placed = p.take_wanted(10, 16);
        assert_eq!(placed.len(), 1);
        assert!(matches!(placed[0], (900_000, Placed::Make { .. })));
        assert!(p.wanted.is_empty());
        // frame 11: made on the worker, readied, shown
        let shown = settle(&mut p, 11);
        assert_eq!(
            shown,
            [(slot, NULL_SLOT + 1)],
            "its entry points at its packed slot"
        );
    }

    #[test]
    fn a_dead_note_unflags_and_an_unpacked_live_slot_stays_flagged() {
        let mut p = packed(1);
        assert_eq!(p.noted(900_001, None, 3), Noted::Dead { entry: 900_001 });
        assert_eq!(
            p.unpacked(true),
            NULL_SLOT | victory_shader::emit::FAULT_FLAG
        );
        assert_eq!(p.unpacked(false), NULL_SLOT);
        assert_eq!(
            p.noted(NULL_SLOT - 1, Some([1, 1, 1]), 3),
            Noted::Nothing,
            "an own slot"
        );
    }

    #[test]
    fn an_armed_view_a_shader_reads_is_touched_and_kept() {
        let mut p = packed(1);
        let slot = 900_002;
        p.noted(slot, Some([1, 2, 1]), 0);
        p.take_wanted(0, 16);
        settle(&mut p, 1);
        assert!(p.arm(50, 16).is_empty(), "read lately: not armed");
        let armed = p.arm(ARM_AFTER, 16);
        assert_eq!(armed, [(slot, NULL_SLOT + 1)]);
        // read while armed: the note says so, the flag comes off
        assert_eq!(
            p.noted(slot, Some([1, 2, 1]), ARM_AFTER + 3),
            Noted::Touched {
                entry: slot,
                at: NULL_SLOT + 1
            }
        );
        // another live slot wants a packed slot: none to evict (touched lately)
        p.noted(900_003, Some([1, 3, 1]), ARM_AFTER + 4);
        assert!(p.take_wanted(ARM_AFTER + 4 + ARM_WAIT, 16).is_empty());
        assert_eq!(p.counts.evicted, 0);
    }

    #[test]
    fn an_unread_armed_view_is_evicted_for_a_wanted_slot_then_its_slot_reused() {
        let mut p = packed(1);
        let (old, new) = (900_010, 900_011);
        p.noted(old, Some([1, 10, 1]), 0);
        p.take_wanted(0, 16);
        settle(&mut p, 1);
        let t = EVICT_AFTER + 1;
        assert_eq!(p.arm(t, 16).len(), 1);
        p.noted(new, Some([1, 11, 1]), t + 1);
        assert!(
            p.take_wanted(t + 1, 16).is_empty(),
            "armed too lately to evict"
        );
        let at = t + ARM_WAIT;
        assert_eq!(p.take_wanted(at, 16), [(old, Placed::Drop(NULL_SLOT + 1))]);
        assert!(!p.map.contains_key(&old));
        assert!(
            p.take_wanted(at + 1, 16).is_empty(),
            "the slot waits before reuse"
        );
        let placed = p.take_wanted(at + super::super::REUSE_AFTER, 16);
        assert!(matches!(
            placed[..],
            [(900_011, Placed::Make { at: 500_001, .. })]
        ));
    }

    #[test]
    fn eviction_never_removes_a_view_touched_in_the_last_evict_after_frames() {
        // A busy heap: 64 packed views, three new live slots read every frame, packed views read
        // at pseudo-random frames. A read is seen only while its view is armed (its entry
        // flagged), as on the card; eviction never takes a view whose last read the host saw
        // within EVICT_AFTER frames, nor one read at all within ARM_WAIT frames.
        let mut p = packed(64);
        let mut seed = 0x9e37_79b9_u32;
        let mut rand = || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed
        };
        let (mut read, mut seen) = (
            std::collections::HashMap::new(),
            std::collections::HashMap::new(),
        );
        let mut next = 600_000u32;
        for frame in 1..4000u64 {
            let mapped: Vec<u32> = p.map.keys().copied().collect();
            for slot in mapped {
                // a third of the views are hot, the rest cold
                let hot = slot % 3 == 0;
                if rand() % if hot { 4 } else { 400 } == 0 {
                    read.insert(slot, frame);
                    let armed = p.map.get(&slot).and_then(|&k| p.owner[k as usize]).is_some_and(|o| o.armed.is_some());
                    if armed {
                        p.noted(slot, Some([1, slot, 1]), frame);
                        seen.insert(slot, frame);
                    }
                }
            }
            for _ in 0..3 {
                next += 1;
                p.noted(next, Some([1, next, 1]), frame);
                read.insert(next, frame);
                seen.insert(next, frame);
            }
            for (slot, placed) in p.take_wanted(frame, 8) {
                if let Placed::Drop(_) = placed {
                    let r = read.get(&slot).copied().unwrap_or(0);
                    let s = seen.get(&slot).copied().unwrap_or(0);
                    assert!(frame - s > EVICT_AFTER, "slot {slot} evicted at {frame}, seen read at {s}");
                    assert!(frame - r > ARM_WAIT, "slot {slot} evicted at {frame}, read at {r}");
                }
            }
            settle(&mut p, frame);
            p.arm(frame, 16);
        }
        assert!(p.counts.evicted > 0, "the test evicted something");
        // the hot views stay
        let hot = p.map.keys().filter(|&&s| s % 3 == 0).count();
        assert!(hot * 3 >= p.map.len(), "hot {hot} of {}", p.map.len());
    }
}
