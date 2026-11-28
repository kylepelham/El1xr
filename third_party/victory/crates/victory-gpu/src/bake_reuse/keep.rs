//! `VICTORY_BAKE_REUSE_KEEP=1` (#115, live, off by default): retains a bounded,
//! short-lived cache of released GPU-written (`host_written`) texture host copies
//! keyed by `(title_addr, size/layout)` so when the title recreates a texture at
//! the same address with a matching layout (and title CPU memory was not written
//! or remapped in between), the new texture adopts the retained GPU-written host
//! copy instead of uploading stale or zero title memory.
//!
//! Caps: [`MAX_BYTES`] (96 MiB), [`MAX_ENTRIES`] (512), [`MAX_AGE_FRAMES`] (120).
//! Eviction: TTL expiry after [`MAX_AGE_FRAMES`] frames, then LRU (oldest released
//! first). Immediate invalidation on remap or runtime write
//! (`watch::title_memory_remapped`, `writing`, `title_memory_written`,
//! `written_through_view`) and on any newer GPU write covering the range; at
//! adoption time, title CPU memory is checked against both the write-watch
//! journal (when write-watched) and a `blake3` hash taken at release so direct
//! CPU overwrites drop the retained copy and upload fresh title memory.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};

/// Live switch: `0` off, `1` on.
pub(crate) static ON: victory_core::settings::Live =
    victory_core::settings::Live::flag("VICTORY_BAKE_REUSE_KEEP");

#[cfg(test)]
thread_local! {
    /// Per-thread override so tests can exercise OFF/ON without racing global `ON`.
    pub(crate) static TEST_ON: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

/// Whether `VICTORY_BAKE_REUSE_KEEP=1` is active.
pub(crate) fn on() -> bool {
    #[cfg(test)]
    if let Some(v) = TEST_ON.get() {
        return v;
    }
    static TOLD: AtomicBool = AtomicBool::new(false);
    let active = ON.get_bool();
    if active && !TOLD.swap(true, Relaxed) {
        tracing::info!(
            max_mib = MAX_BYTES >> 20,
            max_entries = MAX_ENTRIES,
            max_age_frames = MAX_AGE_FRAMES,
            "VICTORY_BAKE_REUSE_KEEP=1: retaining released GPU-written host copies for matching recreation"
        );
    }
    active
}

/// Total VRAM bytes retained at most across the cache (96 MiB).
pub(crate) const MAX_BYTES: u64 = 96 << 20;
/// Largest single texture retained (half the cache cap, 48 MiB).
pub(crate) const MAX_ENTRY_BYTES: u64 = MAX_BYTES / 2;
/// Entries retained at most.
pub(crate) const MAX_ENTRIES: usize = 512;
/// Frames a retained entry lives before expiry.
pub(crate) const MAX_AGE_FRAMES: u64 = 120;

/// Layout key that guarantees host `ID3D12Resource` compatibility between the
/// released texture and a newly created texture at the same address.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct LayoutKey {
    pub(crate) bytes: u64,
    pub(crate) dimension: u32,
    pub(crate) width: u64,
    pub(crate) height: u32,
    pub(crate) depth_or_array_size: u16,
    pub(crate) mip_levels: u16,
    pub(crate) format: u32,
    pub(crate) sample_count: u32,
    pub(crate) sample_quality: u32,
    pub(crate) flags: u32,
}

/// A released GPU-written host texture held in the short-lived reuse cache.
#[derive(Debug)]
pub(crate) struct Retained<T> {
    pub(crate) addr: u64,
    pub(crate) bytes: u64,
    pub(crate) cost: u64,
    pub(crate) layout: LayoutKey,
    pub(crate) title_hash: blake3::Hash,
    pub(crate) watch_asking: Option<u64>,
    pub(crate) released_frame: u64,
    pub(crate) seq: u64,
    pub(crate) card_mips: Option<Arc<[u64]>>,
    pub(crate) host: T,
}

/// Bounded, TTL + LRU cache of released GPU-written host copies keyed by title address.
pub(crate) struct Cache<T> {
    entries: BTreeMap<u64, Retained<T>>,
    total_bytes: u64,
    next_seq: u64,
}

impl<T> Default for Cache<T> {
    fn default() -> Self {
        Self {
            entries: BTreeMap::new(),
            total_bytes: 0,
            next_seq: 1,
        }
    }
}

impl<T> Cache<T> {
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(crate) fn total_bytes(&self) -> u64 {
        self.total_bytes
    }

    /// Inserts `entry`, pruning expired and overlapping entries and evicting LRU
    /// entries until within `max_bytes` and `max_entries`. Returns every displaced
    /// or rejected `T` so the caller can retire them outside the lock.
    pub(crate) fn retain(
        &mut self,
        mut entry: Retained<T>,
        now_frame: u64,
        max_bytes: u64,
        max_entries: usize,
        max_age: u64,
    ) -> (bool, Vec<T>) {
        let mut retired = self.prune(now_frame, max_age);
        retired.extend(self.invalidate(entry.addr, entry.bytes));
        if entry.bytes == 0 || entry.cost > max_bytes / 2 || max_entries == 0 {
            retired.push(entry.host);
            return (false, retired);
        }
        entry.seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1).max(1);
        self.total_bytes = self.total_bytes.saturating_add(entry.cost);
        self.entries.insert(entry.addr, entry);
        while self.total_bytes > max_bytes || self.entries.len() > max_entries {
            let Some(oldest_addr) = self
                .entries
                .iter()
                .min_by_key(|(_, e)| e.seq)
                .map(|(&a, _)| a)
            else {
                break;
            };
            if let Some(evicted) = self.entries.remove(&oldest_addr) {
                self.total_bytes = self.total_bytes.saturating_sub(evicted.cost);
                retired.push(evicted.host);
            }
        }
        (true, retired)
    }

    /// Takes a non-expired entry at `addr` with exact matching `layout`, pruning
    /// expired entries first. Returns `(matched, expired_hosts)`.
    pub(crate) fn take_matching(
        &mut self,
        addr: u64,
        layout: LayoutKey,
        now_frame: u64,
        max_age: u64,
    ) -> (Option<Retained<T>>, Vec<T>) {
        let retired = self.prune(now_frame, max_age);
        let matches = self.entries.get(&addr).is_some_and(|e| {
            e.layout == layout && now_frame.saturating_sub(e.released_frame) <= max_age
        });
        if !matches {
            return (None, retired);
        }
        let Some(entry) = self.entries.remove(&addr) else {
            return (None, retired);
        };
        self.total_bytes = self.total_bytes.saturating_sub(entry.cost);
        (Some(entry), retired)
    }

    /// Removes every entry whose `[addr, addr + bytes)` overlaps `[at, at + len)`.
    pub(crate) fn invalidate(&mut self, at: u64, len: u64) -> Vec<T> {
        if self.entries.is_empty() || len == 0 {
            return Vec::new();
        }
        let end = at.saturating_add(len);
        let mut hits = Vec::new();
        // Check entries starting at or before `at` that might extend past `at`.
        // Since entries are non-overlapping (enforced on insert), at most one entry
        // starting `<= at` can overlap `at`.
        if let Some((&a, e)) = self.entries.range(..=at).next_back()
            && a < end
            && at < a.saturating_add(e.bytes)
        {
            hits.push(a);
        }
        for (&a, e) in self.entries.range(at..end) {
            if a < end && at < a.saturating_add(e.bytes) && hits.last() != Some(&a) {
                hits.push(a);
            }
        }
        let mut retired = Vec::with_capacity(hits.len());
        for a in hits {
            if let Some(e) = self.entries.remove(&a) {
                self.total_bytes = self.total_bytes.saturating_sub(e.cost);
                retired.push(e.host);
            }
        }
        retired
    }

    /// Removes entries older than `max_age` frames at `now_frame`.
    pub(crate) fn prune(&mut self, now_frame: u64, max_age: u64) -> Vec<T> {
        if self.entries.is_empty() {
            return Vec::new();
        }
        let stale: Vec<u64> = self
            .entries
            .iter()
            .filter(|(_, e)| now_frame.saturating_sub(e.released_frame) > max_age)
            .map(|(&a, _)| a)
            .collect();
        let mut retired = Vec::with_capacity(stale.len());
        for a in stale {
            if let Some(e) = self.entries.remove(&a) {
                self.total_bytes = self.total_bytes.saturating_sub(e.cost);
                retired.push(e.host);
            }
        }
        retired
    }

    /// Removes all entries.
    pub(crate) fn drain(&mut self) -> Vec<T> {
        self.total_bytes = 0;
        std::mem::take(&mut self.entries)
            .into_values()
            .map(|e| e.host)
            .collect()
    }
}

#[cfg(windows)]
mod live {
    use super::*;
    use crate::resource::HostTexture;

    static CACHE: Mutex<Option<Cache<Arc<HostTexture>>>> = Mutex::new(None);
    static KEPT_COUNT: AtomicU64 = AtomicU64::new(0);
    static KEPT_BYTES: AtomicU64 = AtomicU64::new(0);

    pub(crate) fn kept_count() -> u64 {
        KEPT_COUNT.load(Relaxed)
    }

    pub(crate) fn kept_bytes() -> u64 {
        KEPT_BYTES.load(Relaxed)
    }

    fn sync_stats(cache: &Cache<Arc<HostTexture>>) {
        KEPT_COUNT.store(cache.len() as u64, Relaxed);
        KEPT_BYTES.store(cache.total_bytes(), Relaxed);
    }

    fn retire_all(hosts: Vec<Arc<HostTexture>>) {
        for h in hosts {
            crate::host::retire(h);
        }
    }

    /// Reads title CPU memory `[at, at + bytes)` safely (non-blocking against an
    /// active remap) and returns its `blake3` digest. Returns `None` if a remap
    /// is actively in flight on that range or the span exceeds the cap.
    fn hash_title_memory(at: u64, bytes: u64) -> Option<blake3::Hash> {
        if at == 0 || bytes == 0 || bytes > MAX_ENTRY_BYTES {
            return None;
        }
        let len = usize::try_from(bytes).ok()?;
        let _reading = crate::watch::title_memory_reading_now(at as usize, len)?;
        let mut buf = vec![0u8; len];
        crate::host::copy_mapped(buf.as_mut_ptr(), at as *const u8, len, false);
        Some(blake3::hash(&buf))
    }

    /// Checks whether title CPU memory `[at, at + bytes)` is untouched since the
    /// retained entry was recorded: neither written via write-watch nor modified
    /// in content.
    fn title_memory_unchanged(
        at: u64,
        bytes: u64,
        saved_hash: blake3::Hash,
        saved_asking: Option<u64>,
    ) -> bool {
        if let Some(asked) = saved_asking {
            let start = at as usize;
            let end = start.saturating_add(bytes as usize);
            match crate::watch::writes(start, end, false) {
                Some((_, written)) if written.iter().any(|&(_, w)| w > asked) => return false,
                None => return false,
                Some(_) => {}
            }
        }
        hash_title_memory(at, bytes).is_some_and(|now| now == saved_hash)
    }

    /// Retains a released GPU-written `host` copy at `[at, at + bytes)` with
    /// `layout`, or retires it if not eligible or rejected by caps.
    pub(crate) fn retain(
        at: u64,
        bytes: u64,
        layout: LayoutKey,
        resource: usize,
        host: Arc<HostTexture>,
    ) {
        if !on() || at == 0 || bytes == 0 || bytes > MAX_ENTRY_BYTES {
            crate::host::retire(host);
            return;
        }
        let cost = host.host_bytes().max(bytes);
        if cost > MAX_ENTRY_BYTES {
            crate::host::retire(host);
            return;
        }
        let watch_asking = crate::watch::writes(
            at as usize,
            (at as usize).saturating_add(bytes as usize),
            false,
        )
        .map(|(now, _)| now);
        let Some(title_hash) = hash_title_memory(at, bytes) else {
            crate::host::retire(host);
            return;
        };
        let card_mips = crate::texture::card_mips_snapshot(resource);
        let released_frame = crate::bake_reuse::frame_line();
        let entry = Retained {
            addr: at,
            bytes,
            cost,
            layout,
            title_hash,
            watch_asking,
            released_frame,
            seq: 0,
            card_mips,
            host,
        };
        let (_, retired) = {
            let mut guard = CACHE.lock().unwrap_or_else(|e| e.into_inner());
            let cache = guard.get_or_insert_with(Cache::default);
            let res = cache.retain(
                entry,
                released_frame,
                MAX_BYTES,
                MAX_ENTRIES,
                MAX_AGE_FRAMES,
            );
            sync_stats(cache);
            res
        };
        retire_all(retired);
    }

    /// Attempts to adopt a retained GPU-written `HostTexture` for a texture at
    /// `at` with `layout`. Verifies title CPU memory has not been written or
    /// remapped since release; drops and retires the cached entry if it changed.
    pub(crate) fn adopt(
        at: u64,
        bytes: u64,
        layout: LayoutKey,
    ) -> Option<(Arc<HostTexture>, Option<Arc<[u64]>>)> {
        if !on() || KEPT_COUNT.load(Relaxed) == 0 || at == 0 || bytes == 0 {
            return None;
        }
        let now_frame = crate::bake_reuse::frame_line();
        let (found, retired) = {
            let mut guard = CACHE.lock().unwrap_or_else(|e| e.into_inner());
            let Some(cache) = guard.as_mut() else {
                return None;
            };
            let res = cache.take_matching(at, layout, now_frame, MAX_AGE_FRAMES);
            sync_stats(cache);
            res
        };
        retire_all(retired);
        let entry = found?;
        if !title_memory_unchanged(at, bytes, entry.title_hash, entry.watch_asking) {
            crate::host::retire(entry.host);
            crate::bake_reuse::note_invalidated(1);
            return None;
        }
        let age_frames = now_frame.saturating_sub(entry.released_frame);
        crate::bake_reuse::note_adopted(at, layout, age_frames, entry.cost);
        Some((entry.host, entry.card_mips))
    }

    /// Invalidates any retained entries overlapping `[at, at + len)` (called on
    /// title memory remap, runtime/DirectStorage write, or newer GPU write).
    pub(crate) fn invalidate(at: u64, len: u64) {
        if KEPT_COUNT.load(Relaxed) == 0 || len == 0 {
            return;
        }
        let retired = {
            let mut guard = CACHE.lock().unwrap_or_else(|e| e.into_inner());
            let Some(cache) = guard.as_mut() else {
                return;
            };
            let gone = cache.invalidate(at, len);
            sync_stats(cache);
            gone
        };
        let n = retired.len() as u64;
        if n > 0 {
            retire_all(retired);
            crate::bake_reuse::note_invalidated(n);
        }
    }

    /// Periodic maintenance (called from `resource::let_go_idle`): drains the
    /// cache if the switch was turned off live, or prunes entries past
    /// [`MAX_AGE_FRAMES`].
    pub(crate) fn tick() {
        let active = on();
        if KEPT_COUNT.load(Relaxed) > 0 {
            let now_frame = crate::frame::current();
            let retired = {
                let mut guard = CACHE.lock().unwrap_or_else(|e| e.into_inner());
                let Some(cache) = guard.as_mut() else {
                    return;
                };
                let gone = if active {
                    cache.prune(now_frame, MAX_AGE_FRAMES)
                } else {
                    cache.drain()
                };
                sync_stats(cache);
                gone
            };
            retire_all(retired);
        }
        if active || crate::bake_reuse::on() {
            crate::bake_reuse::frame_line();
        }
    }
}

#[cfg(not(windows))]
mod live {
    use super::*;

    pub(crate) fn kept_count() -> u64 {
        0
    }

    pub(crate) fn kept_bytes() -> u64 {
        0
    }

    pub(crate) fn invalidate(_at: u64, _len: u64) {}

    pub(crate) fn tick() {}
}

pub(crate) use live::*;

#[cfg(test)]
mod tests {
    use super::*;

    fn layout(bytes: u64, width: u64, height: u32, format: u32) -> LayoutKey {
        LayoutKey {
            bytes,
            dimension: 3,
            width,
            height,
            depth_or_array_size: 1,
            mip_levels: 1,
            format,
            sample_count: 1,
            sample_quality: 0,
            flags: 0,
        }
    }

    fn entry(addr: u64, bytes: u64, cost: u64, frame: u64, id: u32) -> Retained<u32> {
        Retained {
            addr,
            bytes,
            cost,
            layout: layout(bytes, 256, 256, 78),
            title_hash: blake3::hash(&addr.to_le_bytes()),
            watch_asking: None,
            released_frame: frame,
            seq: 0,
            card_mips: None,
            host: id,
        }
    }

    #[test]
    fn adopts_matching_layout_and_rejects_mismatch() {
        let mut c = Cache::default();
        let (kept, retired) = c.retain(entry(0x1000, 0x1000, 0x1000, 10, 1), 10, 1 << 20, 16, 120);
        assert!(kept);
        assert!(retired.is_empty());
        assert_eq!(c.len(), 1);
        assert_eq!(c.total_bytes(), 0x1000);

        // Wrong format does not adopt and leaves the entry in place.
        let (hit, retired) = c.take_matching(0x1000, layout(0x1000, 256, 256, 71), 15, 120);
        assert!(hit.is_none());
        assert!(retired.is_empty());
        assert_eq!(c.len(), 1);

        // Exact layout adopts and clears the cache slot.
        let (hit, retired) = c.take_matching(0x1000, layout(0x1000, 256, 256, 78), 15, 120);
        assert_eq!(hit.map(|e| e.host), Some(1));
        assert!(retired.is_empty());
        assert_eq!(c.len(), 0);
        assert_eq!(c.total_bytes(), 0);
    }

    #[test]
    fn expires_after_max_age_frames() {
        let mut c = Cache::default();
        c.retain(entry(0x2000, 0x1000, 0x1000, 100, 7), 100, 1 << 20, 16, 120);
        // At frame 220 (age 120): still valid.
        let retired = c.prune(220, 120);
        assert!(retired.is_empty());
        assert_eq!(c.len(), 1);
        // At frame 221 (age 121): expired and retired.
        let (hit, retired) = c.take_matching(0x2000, layout(0x1000, 256, 256, 78), 221, 120);
        assert!(hit.is_none());
        assert_eq!(retired, vec![7]);
        assert_eq!(c.len(), 0);
        assert_eq!(c.total_bytes(), 0);
    }

    #[test]
    fn evicts_lru_on_byte_and_entry_caps() {
        let mut c = Cache::default();
        // Byte cap: 4096 bytes, single entry max = 2048.
        let (k1, r1) = c.retain(entry(0x1000, 1024, 1500, 1, 1), 1, 4096, 8, 120);
        assert!(k1 && r1.is_empty());
        let (k2, r2) = c.retain(entry(0x2000, 1024, 1500, 2, 2), 2, 4096, 8, 120);
        assert!(k2 && r2.is_empty());
        // Adding a third 1500-byte entry (4500 > 4096) evicts LRU id=1.
        let (k3, r3) = c.retain(entry(0x3000, 1024, 1500, 3, 3), 3, 4096, 8, 120);
        assert!(k3);
        assert_eq!(r3, vec![1]);
        assert_eq!(c.len(), 2);
        assert_eq!(c.total_bytes(), 3000);

        // Oversized single entry (> max_bytes/2 = 2048) is rejected immediately.
        let (k4, r4) = c.retain(entry(0x4000, 1024, 3000, 4, 4), 4, 4096, 8, 120);
        assert!(!k4);
        assert_eq!(r4, vec![4]);
        assert_eq!(c.len(), 2);

        // Entry cap = 2: inserting another evicts oldest remaining (id=2).
        let (k5, r5) = c.retain(entry(0x5000, 1024, 1000, 5, 5), 5, 4096, 2, 120);
        assert!(k5);
        assert_eq!(r5, vec![2]);
        assert_eq!(c.len(), 2);
        assert_eq!(c.total_bytes(), 2500);
    }

    #[test]
    fn invalidate_removes_only_overlapping_ranges() {
        let mut c = Cache::default();
        c.retain(entry(0x1000, 0x1000, 0x1000, 1, 1), 1, 1 << 20, 16, 120);
        c.retain(entry(0x2000, 0x1000, 0x1000, 2, 2), 2, 1 << 20, 16, 120);
        c.retain(entry(0x4000, 0x1000, 0x1000, 3, 3), 3, 1 << 20, 16, 120);
        // Overlap tail of 0x1000 and head of 0x2000.
        let mut gone = c.invalidate(0x1800, 0x1000);
        gone.sort_unstable();
        assert_eq!(gone, vec![1, 2]);
        assert_eq!(c.len(), 1);
        assert_eq!(c.total_bytes(), 0x1000);
        // 0x4000 untouched.
        let (hit, _) = c.take_matching(0x4000, layout(0x1000, 256, 256, 78), 5, 120);
        assert_eq!(hit.map(|e| e.host), Some(3));
    }

    #[test]
    fn the_switch_is_live() {
        victory_core::settings::check_live("VICTORY_BAKE_REUSE_KEEP", "1", on, false, true);
    }
}
