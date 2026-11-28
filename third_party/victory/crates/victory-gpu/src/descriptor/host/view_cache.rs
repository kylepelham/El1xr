//! (feedcard, #88) `VICTORY_TABLE_VIEW_CACHE=1` (Live, off by default): a
//! texture view our fresh descriptor tables hold is made once, into a heap
//! the shaders do not see, and copied into every table that names the same
//! record after, across lists and frames (`VICTORY_LIST_VIEWS` keeps them
//! for one recording only).
//!
//! A table holding a constant buffer view is made afresh for each list that
//! binds it (`crate::draw::State::tables`), and each texture view in it was
//! made again: the description checked, the view created. b5s 04_cpu: 26.3
//! ms a frame of view making over the title's recording threads. Xenia makes
//! each texture view once, keyed by texture and view, and copies it.
//!
//! An entry is keyed by the record (`(tag, index, generation)`: the same
//! record is the same title view) and copied only while what its view shows
//! is what it was made from: the title texture the record (or its placed
//! memory) names, its host copy (told by its shared allocation, which the
//! entry keeps from reuse with a weak reference, not the copy itself, so an
//! idle copy can still be let go), the first level shown (levels on demand)
//! and the levels the store filled. Anything else is made again in place.
//! Only views that show the host copy itself are kept; buffer views,
//! unordered access, stand-ins and views the host cannot make are made in
//! the table's slot as before.
//!
//! After fxceless/victory-fork PR #1484 (`VICTORY_TABLE_VIEW_CACHE`,
//! KainanP), written for our `make`. Line: table views per frame.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Mutex, Weak};

use windows::Win32::Graphics::Direct3D12::*;
use windows::core::Interface;

use super::{HostHeap, View, make, placed_texture};
use crate::resource::HostTexture;

static ON: victory_core::settings::Live =
    victory_core::settings::Live::flag("VICTORY_TABLE_VIEW_CACHE");

pub(crate) fn on() -> bool {
    ON.get_bool()
}

/// Locks the entries are spread over, by record.
const SHARDS: usize = 64;
/// Slots of the heap each shard owns: 65,536 in all (2 MB).
const PER_SHARD: u32 = 1024;

/// Copied from the cache, made into it, left to the table's slot, since
/// the last line.
static COUNTS: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

/// What a view shows besides its record: the title texture, its host copy,
/// the first level shown and the levels the store filled.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Shows {
    title: usize,
    copy: *const HostTexture,
    first: Option<u32>,
    prefilled: u32,
}

/// What a slot of the cache was made from.
struct Entry {
    slot: u32,
    shows: Shows,
    /// Keeps `shows.copy`'s allocation from reuse while the entry lives.
    _copy: Weak<HostTexture>,
}

#[derive(Default)]
struct Shard {
    entries: HashMap<[u32; 3], Entry>,
    /// The record each of the shard's slots holds (`None`: free), and the
    /// slot to take next when every slot holds one.
    owners: Vec<Option<[u32; 3]>>,
    next: u32,
}

impl Shard {
    /// The slot (of the shard's) for `key` made anew: the one it had, else
    /// the next in turn, whose record is forgotten (made again when asked).
    fn take(&mut self, key: [u32; 3]) -> u32 {
        if let Some(old) = self.entries.remove(&key) {
            return old.slot;
        }
        if self.owners.is_empty() {
            self.owners = vec![None; PER_SHARD as usize];
        }
        let at = self.next;
        self.next = (at + 1) % PER_SHARD;
        if let Some(owner) = self.owners[at as usize].replace(key) {
            self.entries.remove(&owner);
        }
        at
    }

    /// The slot `key`'s entry has, if it was made from `shows`.
    fn found(&self, key: &[u32; 3], shows: &Shows) -> Option<u32> {
        self.entries
            .get(key)
            .filter(|e| e.shows == *shows)
            .map(|e| e.slot)
    }
}

struct ViewCache {
    /// Held for its slots.
    _heap: ID3D12DescriptorHeap,
    cpu: usize,
    step: usize,
    shards: Vec<Mutex<Shard>>,
}

// SAFETY: the heap is written only through the device (free-threaded), a
// shard's slots only under its lock.
unsafe impl Send for ViewCache {}
// SAFETY: as above.
unsafe impl Sync for ViewCache {}

fn cache() -> Option<&'static ViewCache> {
    static CACHE: std::sync::OnceLock<Option<ViewCache>> = std::sync::OnceLock::new();
    CACHE
        .get_or_init(|| {
            let host = crate::host::host()?;
            // SAFETY: plain object creation and queries; a heap the card does not see.
            unsafe {
                let heap: ID3D12DescriptorHeap = host
                    .device
                    .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                        Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                        NumDescriptors: SHARDS as u32 * PER_SHARD,
                        ..Default::default()
                    })
                    .ok()?;
                let cpu = heap.GetCPUDescriptorHandleForHeapStart().ptr;
                let step = host
                    .device
                    .GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV)
                    as usize;
                Some(ViewCache {
                    _heap: heap,
                    cpu,
                    step,
                    shards: (0..SHARDS).map(|_| Mutex::new(Shard::default())).collect(),
                })
            }
        })
        .as_ref()
}

fn shard_of(key: &[u32; 3]) -> usize {
    let mix = (u64::from(key[1]) | u64::from(key[2]) << 32).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    (mix >> 58) as usize % SHARDS
}

impl ViewCache {
    fn slot(&self, slot: u32) -> D3D12_CPU_DESCRIPTOR_HANDLE {
        D3D12_CPU_DESCRIPTOR_HANDLE {
            ptr: self.cpu + slot as usize * self.step,
        }
    }

    /// Writes record `key`'s view `view` to `to` from the cache, made there
    /// first if it is not or no longer shows what it shows now: the host
    /// texture the slot is to hold. `None`: a view the cache does not keep
    /// (nothing written).
    fn copy(
        &self,
        view: &View,
        key: [u32; 3],
        to: D3D12_CPU_DESCRIPTOR_HANDLE,
    ) -> Option<ID3D12Resource> {
        let placed;
        let shown = match view {
            View::Shader(..) => view,
            View::Placed(false, ..) => {
                placed = placed_texture(view)?;
                &placed
            }
            _ => return None,
        };
        let &View::Shader(title, _) = shown else {
            return None;
        };
        let resource = title as crate::object::This;
        let copy = crate::resource::host_copy(resource)?;
        let shows = Shows {
            title,
            copy: std::sync::Arc::as_ptr(&copy),
            first: crate::resource::first_shown(resource),
            prefilled: copy.prefilled.load(Relaxed),
        };
        let device = &crate::host::host()?.device;
        let s = shard_of(&key);
        let base = s as u32 * PER_SHARD;
        let mut shard = self.shards[s].lock().unwrap_or_else(|e| e.into_inner());
        if let Some(slot) = shard.found(&key, &shows) {
            // `make`'s note of a view with no level uploaded, as for a view made.
            super::flicker::made_view(&copy, shows.first);
            // SAFETY: a slot of the cache made from this record and these; `to` a heap's slot.
            unsafe {
                device.CopyDescriptorsSimple(
                    1,
                    to,
                    self.slot(base + slot),
                    D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                )
            };
            COUNTS[0].fetch_add(1, Relaxed);
            return Some(copy.texture.clone());
        }
        let slot = shard.take(key);
        let made = make(shown, self.slot(base + slot)).and_then(|(kept, _)| kept);
        // A view of anything else than this host copy is not kept (its slot
        // is free again for the next record).
        let Some(texture) = made.filter(|t| t.as_raw() == copy.texture.as_raw()) else {
            shard.owners[slot as usize] = None;
            return None;
        };
        shard.entries.insert(
            key,
            Entry {
                slot,
                shows,
                _copy: std::sync::Arc::downgrade(&copy),
            },
        );
        // SAFETY: as above.
        unsafe {
            device.CopyDescriptorsSimple(
                1,
                to,
                self.slot(base + slot),
                D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
            )
        };
        COUNTS[1].fetch_add(1, Relaxed);
        Some(texture)
    }
}

impl HostHeap {
    /// Makes slot `index` (a dynamic one) the view the title's record
    /// `words` names, from the cache of views made (the view read from the
    /// record words, as the key is). False: not one the cache keeps
    /// (nothing written); the caller makes it as before.
    pub(crate) fn make_cached(&self, words: [u32; 3], index: u32) -> bool {
        if self.sampler || words[0] != super::VIEW_TAG {
            return false;
        }
        let Some(view) = super::view_at(words[1], words[2]) else {
            COUNTS[2].fetch_add(1, Relaxed);
            return false;
        };
        match cache().and_then(|c| c.copy(&view, words, self.cpu(index))) {
            Some(texture) => {
                self.keep(index, Some(texture));
                true
            }
            None => {
                COUNTS[2].fetch_add(1, Relaxed);
                false
            }
        }
    }
}

/// One line a frame, once a table asked.
pub(crate) fn line(frames: f64) {
    let c = COUNTS.each_ref().map(|a| a.swap(0, Relaxed));
    if frames <= 0.0 || c.iter().all(|&n| n == 0) {
        return;
    }
    tracing::info!(
        "table views per frame (VICTORY_TABLE_VIEW_CACHE={}): {:.0} copied from the cache, \
         {:.0} made into it, {:.0} made in the table as before",
        u8::from(on()),
        c[0] as f64 / frames,
        c[1] as f64 / frames,
        c[2] as f64 / frames,
    );
}

#[cfg(test)]
#[path = "view_cache_tests.rs"]
mod tests;
