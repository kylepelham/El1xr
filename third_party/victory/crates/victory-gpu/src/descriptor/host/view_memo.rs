//! (entryhitch, #16) `VICTORY_TABLE_VIEW_MEMO=1` (Live, off by default):
//! within one recording of a command list, memoizes the descriptors of fresh
//! descriptor table slots (`record -> (local slot, kind, epoch, kept)`) in a
//! per-thread CPU-only descriptor heap, and coalesces adjacent cached slots
//! into batched `CopyDescriptorsSimple(run_len)` calls.
//!
//! `view_cache::bench` measured `CopyDescriptorsSimple(8)` at 0.4 ns/slot
//! (`CopyDescriptorsSimple(1)` at 2.5 ns/slot, faster than scattered
//! `CopyDescriptors` at 3.7 ns/slot), while a cross-list
//! `VICTORY_TABLE_VIEW_CACHE` hit costs ~94 ns (texture) and ~119 ns (placed
//! buffer) because finding the title resource / host copies
//! (`resource::host_copy`, `first_shown`, `shadow::find`, `shadow::exact`) and
//! locking a cache shard dominates the hit. Here the first occurrence of a
//! record in a list resolves through the global view cache (when on, else
//! `make`) into the thread's private CPU heap and remembers it with the current
//! monotonic resource/shadow epoch. Repeated draws in the same list skip every
//! resource lookup, global lock, and per-slot atomic RMW; jointly contiguous
//! runs copy together in one `CopyDescriptorsSimple(run_len)` call at the end
//! of the table build.
//!
//! Safety:
//! - Texture views (SRV and UAV, object and placed) carry `resource::view_epoch()`,
//!   bumped after any texture host copy eviction/drop, texture release, placed
//!   texture removal, `TARGETS_AT` insert, `first_shown` change, or `prefilled`
//!   change.
//! - Buffer views (SRV and UAV, object and placed) carry `shadow::view_epoch()`,
//!   bumped after any `BUFFERS` register/unregister, `LOOSE` insert/eviction,
//!   `in_place_let_go`, or `EXACT` eviction.
//! - The epoch is sampled before resolving a miss, and mutations bump the epoch
//!   after updating their map (under the same lock), so no concurrent change can
//!   slip between a lookup and the epoch recorded with it.
//! - Each thread owns its private CPU heap exclusively (zero concurrent read/write
//!   race with global cache eviction). Before overwriting a stale slot or wrapping
//!   a full heap, any pending `CopyBatch` is flushed so `HostHeap::kept` always
//!   matches the descriptor written to every dynamic slot.
//! - Empty tiled textures (`tiled.is_some() && first_shown.is_none()`) are not
//!   memoized so `flicker::made_view` keeps reporting them and level uploads show
//!   at once.
//! - Cleared when the list resets (`forget`) or the thread starts a new recording.

use std::cell::RefCell;
use std::collections::HashMap;
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use windows::Win32::Graphics::Direct3D12::*;

use super::{HostHeap, View, make, placed_texture, view_cache};

static ON: victory_core::settings::Live =
    victory_core::settings::Live::flag("VICTORY_TABLE_VIEW_MEMO");

/// Whether fresh table views are memoized per list (`VICTORY_TABLE_VIEW_MEMO`).
pub(crate) fn on() -> bool {
    ON.get_bool()
}

/// Slots in each recording thread's private CPU-only descriptor heap.
const SLOTS: u32 = 8192;

/// Maximum distinct contiguous `(dst, src)` runs held inline in [`CopyBatch`]
/// before flushing to D3D12.
const BATCH_CAP: usize = 32;

/// Flush thread-local hit/copy stats to [`COUNTS`] every N hits/slots so the
/// hot path performs zero atomic RMWs (`lock xadd`) per slot or per table.
const STAT_FLUSH: u64 = 64;

/// Memo hits, misses, stale rebuilds, batched copy calls, slots copied in batches.
static COUNTS: [AtomicU64; 5] = [const { AtomicU64::new(0) }; 5];

/// Single-instruction multiply-rotate hasher for 64-bit record keys `(idx | gen << 32)`.
#[derive(Default, Clone)]
struct FastU64State;

struct FastU64Hasher(u64);

impl BuildHasher for FastU64State {
    type Hasher = FastU64Hasher;

    #[inline]
    fn build_hasher(&self) -> FastU64Hasher {
        FastU64Hasher(0)
    }
}

impl Hasher for FastU64Hasher {
    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }

    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = self
                .0
                .wrapping_add(u64::from(b))
                .wrapping_mul(0xf135_7aea_2e62_a9c5);
        }
        self.0 = self.0.rotate_left(26);
    }

    #[inline]
    fn write_u64(&mut self, n: u64) {
        self.0 = n.wrapping_mul(0xf135_7aea_2e62_a9c5).rotate_left(26);
    }
}

/// Which monotonic invalidation epoch guards a memoized view.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Kind {
    Texture,
    Buffer,
}

impl Kind {
    #[inline]
    fn epoch(self) -> u64 {
        match self {
            Kind::Texture => crate::resource::view_epoch(),
            Kind::Buffer => crate::shadow::view_epoch(),
        }
    }
}

/// Classifies a view into the epoch kind that guards its host resource(s).
/// `None` for constant buffers and samplers (not memoized here).
pub(super) fn kind_of(view: &View) -> Option<Kind> {
    const BUFFER: u32 = 1;
    match view {
        View::Constant(..) | View::Sampler(_) | View::ConsoleSampler(_) => None,
        View::Placed(_, resource, _) => Some(if resource[0] == BUFFER {
            Kind::Buffer
        } else {
            Kind::Texture
        }),
        View::Shader(r, _) | View::Unordered(r, _, _) => {
            if crate::resource::buffer_bytes(*r as crate::object::This).is_some() {
                Some(Kind::Buffer)
            } else {
                Some(Kind::Texture)
            }
        }
    }
}

/// Whether `view` is a shader view of a tiled texture with no level uploaded yet
/// (kept out of the memo so `flicker::made_view` fires and uploads show at once).
fn is_empty_tiled(view: &View) -> bool {
    let placed;
    let shown = match view {
        View::Shader(..) => view,
        View::Placed(false, ..) => match placed_texture(view) {
            Some(v) => {
                placed = v;
                &placed
            }
            None => return false,
        },
        _ => return false,
    };
    let &View::Shader(title, _) = shown else {
        return false;
    };
    let resource = title as crate::object::This;
    let Some(copy) = crate::resource::host_copy(resource) else {
        return false;
    };
    copy.tiled.is_some() && crate::resource::first_shown(resource).is_none()
}

/// Packs record words `[VIEW_TAG, idx, gen]` into a 64-bit map key.
#[inline]
fn record_key(words: [u32; 3]) -> u64 {
    u64::from(words[1]) | (u64::from(words[2]) << 32)
}

#[derive(Clone, Copy, Default)]
struct CopyRun {
    dst: u32,
    src: u32,
    len: u32,
}

/// Batched descriptor copies accumulated over one fresh table build, coalescing
/// jointly adjacent `(dst, src)` slots into `CopyDescriptorsSimple(run.len)`
/// calls (measured at 0.4 ns/slot for runs of 8 and 2.5 ns/slot for singletons,
/// both faster than scattered `CopyDescriptors` at 3.7 ns/slot on RTX 5090).
pub(crate) struct CopyBatch {
    runs: [CopyRun; BATCH_CAP],
    len: usize,
    total: u32,
    /// Accumulated copy calls and slots copied on this thread (flushed periodically).
    local_calls: u64,
    local_slots: u64,
}

impl CopyBatch {
    #[inline]
    pub(crate) const fn new() -> Self {
        CopyBatch {
            runs: [CopyRun {
                dst: 0,
                src: 0,
                len: 0,
            }; BATCH_CAP],
            len: 0,
            total: 0,
            local_calls: 0,
            local_slots: 0,
        }
    }

    #[inline]
    pub(crate) fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Appends `(dst, src)` to the batch, extending the last run when both
    /// `dst` and `src` advance by 1. Returns `false` if inline capacity is full
    /// (caller must flush and retry).
    #[inline]
    pub(super) fn push_raw(&mut self, dst: u32, src: u32) -> bool {
        if self.len > 0 {
            let last = &mut self.runs[self.len - 1];
            if last.dst + last.len == dst && last.src + last.len == src {
                last.len += 1;
                self.total += 1;
                return true;
            }
        }
        if self.len == BATCH_CAP {
            return false;
        }
        self.runs[self.len] = CopyRun { dst, src, len: 1 };
        self.len += 1;
        self.total += 1;
        true
    }

    /// Flushes all queued runs from CPU heap `(src_base, src_step)` into
    /// destination heap `(dst_base, dst_step)` via `CopyDescriptorsSimple`.
    #[inline]
    pub(super) fn flush_raw(
        &mut self,
        device: &ID3D12Device,
        dst_base: usize,
        dst_step: usize,
        src_base: usize,
        src_step: usize,
    ) {
        if self.len == 0 {
            return;
        }
        for i in 0..self.len {
            let run = self.runs[i];
            let dst = D3D12_CPU_DESCRIPTOR_HANDLE {
                ptr: dst_base + run.dst as usize * dst_step,
            };
            let src = D3D12_CPU_DESCRIPTOR_HANDLE {
                ptr: src_base + run.src as usize * src_step,
            };
            // SAFETY: valid CPU handles in live heaps; `run.len` fits both ranges.
            unsafe {
                device.CopyDescriptorsSimple(
                    run.len,
                    dst,
                    src,
                    D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                );
            }
        }
        self.local_calls += self.len as u64;
        self.local_slots += u64::from(self.total);
        if self.local_slots >= STAT_FLUSH {
            COUNTS[3].fetch_add(self.local_calls, Relaxed);
            COUNTS[4].fetch_add(self.local_slots, Relaxed);
            self.local_calls = 0;
            self.local_slots = 0;
        }
        self.len = 0;
        self.total = 0;
    }

    fn flush_host(&mut self, host: &HostHeap, src_base: usize, src_step: usize) {
        if self.len == 0 {
            return;
        }
        let Some(h) = crate::host::host() else {
            self.len = 0;
            self.total = 0;
            return;
        };
        self.flush_raw(&h.device, host.cpu, host.step as usize, src_base, src_step);
    }

    /// Flushes any pending copies queued against this thread's memo into `host`.
    pub(crate) fn flush(&mut self, host: &HostHeap) {
        if self.len == 0 {
            return;
        }
        let Some((src_base, src_step)) = MEMO.with(|cell| {
            let memo = cell.borrow();
            memo.as_ref().map(|m| (m.start, m.step))
        }) else {
            self.len = 0;
            self.total = 0;
            return;
        };
        self.flush_host(host, src_base, src_step);
        if self.local_calls > 0 {
            COUNTS[3].fetch_add(self.local_calls, Relaxed);
            COUNTS[4].fetch_add(self.local_slots, Relaxed);
            self.local_calls = 0;
            self.local_slots = 0;
        }
    }
}

struct MemoEntry {
    slot: u32,
    kind: Kind,
    epoch: u64,
    kept: ID3D12Resource,
}

/// Per-thread private CPU-only descriptor heap and record memo for one
/// command-list recording.
pub(super) struct ViewMemo {
    recording: u64,
    _heap: ID3D12DescriptorHeap,
    pub(super) start: usize,
    pub(super) step: usize,
    next: u32,
    local_hits: u64,
    entries: HashMap<u64, MemoEntry, FastU64State>,
}

impl ViewMemo {
    pub(super) fn new() -> Option<ViewMemo> {
        let host = crate::host::host()?;
        // SAFETY: plain object creation and queries; a heap the shaders do not see.
        unsafe {
            let heap: ID3D12DescriptorHeap = host
                .device
                .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                    NumDescriptors: SLOTS,
                    ..Default::default()
                })
                .ok()?;
            let step = host
                .device
                .GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV)
                as usize;
            let start = heap.GetCPUDescriptorHandleForHeapStart().ptr;
            Some(ViewMemo {
                recording: 0,
                _heap: heap,
                start,
                step,
                next: 0,
                local_hits: 0,
                entries: HashMap::with_capacity_and_hasher(256, FastU64State),
            })
        }
    }

    #[inline]
    pub(super) fn slot(&self, i: u32) -> D3D12_CPU_DESCRIPTOR_HANDLE {
        debug_assert!(i < SLOTS);
        D3D12_CPU_DESCRIPTOR_HANDLE {
            ptr: self.start + i as usize * self.step,
        }
    }

    #[inline]
    fn flush_local_hits(&mut self) {
        if self.local_hits > 0 {
            COUNTS[0].fetch_add(self.local_hits, Relaxed);
            self.local_hits = 0;
        }
    }

    pub(super) fn clear(&mut self) {
        self.flush_local_hits();
        self.recording = 0;
        self.next = 0;
        self.entries.clear();
    }

    /// Resolves record `words` (`view`) in `recording`, pushing the copy into
    /// `batch` (or copying straight to `direct_to` if not memoizable).
    /// Calls `flush(batch, src_base, src_step)` if `batch` must flush first,
    /// and `keep(&kept)` with the host resource for `dst_index`.
    #[inline]
    #[allow(clippy::too_many_arguments)]
    pub(super) fn resolve_slot(
        &mut self,
        recording: u64,
        words: [u32; 3],
        view: &View,
        dst_index: u32,
        direct_to: D3D12_CPU_DESCRIPTOR_HANDLE,
        batch: &mut CopyBatch,
        use_cache_tex: bool,
        use_cache_buf: bool,
        mut flush: impl FnMut(&mut CopyBatch, usize, usize),
        keep: impl FnOnce(&ID3D12Resource),
    ) -> bool {
        if self.recording != recording {
            if !batch.is_empty() {
                flush(batch, self.start, self.step);
            }
            self.flush_local_hits();
            self.recording = recording;
            self.next = 0;
            self.entries.clear();
        }
        let key = record_key(words);
        // Fast path: memo hit with matching resource/shadow epoch (zero locks, zero atomic RMW).
        if let Some(entry) = self.entries.get(&key)
            && entry.epoch == entry.kind.epoch()
        {
            if !batch.push_raw(dst_index, entry.slot) {
                flush(batch, self.start, self.step);
                let pushed = batch.push_raw(dst_index, entry.slot);
                debug_assert!(pushed);
            }
            keep(&entry.kept);
            self.local_hits += 1;
            if self.local_hits >= STAT_FLUSH {
                self.flush_local_hits();
            }
            return true;
        }
        self.resolve_miss(
            key,
            words,
            view,
            dst_index,
            direct_to,
            batch,
            use_cache_tex,
            use_cache_buf,
            flush,
            keep,
        )
    }

    #[cold]
    #[inline(never)]
    #[allow(clippy::too_many_arguments)]
    fn resolve_miss(
        &mut self,
        key: u64,
        words: [u32; 3],
        view: &View,
        dst_index: u32,
        direct_to: D3D12_CPU_DESCRIPTOR_HANDLE,
        batch: &mut CopyBatch,
        use_cache_tex: bool,
        use_cache_buf: bool,
        mut flush: impl FnMut(&mut CopyBatch, usize, usize),
        keep: impl FnOnce(&ID3D12Resource),
    ) -> bool {
        let old = self.entries.remove(&key);
        if old.is_some() {
            // Flush before overwriting the reused slot so no earlier slot in
            // this table can read the new descriptor with the old `kept` resource.
            if !batch.is_empty() {
                flush(batch, self.start, self.step);
            }
            COUNTS[2].fetch_add(1, Relaxed);
        }
        let Some(kind) = kind_of(view) else {
            return false;
        };
        // Sample the epoch *before* resolving: any concurrent mutation bumps it
        // *after* updating its map, so a miss that saw pre-mutation state stores
        // the old epoch and misses again on the next draw.
        let epoch_before = kind.epoch();
        let local_slot = match old {
            Some(ref e) => e.slot,
            None => {
                if self.next >= SLOTS {
                    if !batch.is_empty() {
                        flush(batch, self.start, self.step);
                    }
                    self.entries.clear();
                    self.next = 0;
                }
                self.next
            }
        };
        let dst = self.slot(local_slot);
        let filled = fill_slot(kind, view, words, dst, use_cache_tex, use_cache_buf);
        let Some((kept, memoizable)) = filled else {
            return false;
        };
        if !memoizable {
            // Empty tiled view: copy once straight to the table slot and do not
            // memoize so `flicker::made_view` fires and level uploads show at once.
            if let Some(host) = crate::host::host() {
                // SAFETY: `dst` is a slot of our private CPU heap, `direct_to` is the table slot.
                unsafe {
                    host.device.CopyDescriptorsSimple(
                        1,
                        direct_to,
                        dst,
                        D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                    );
                }
            }
            keep(&kept);
            COUNTS[1].fetch_add(1, Relaxed);
            return true;
        }
        if old.is_none() {
            self.next += 1;
        }
        keep(&kept);
        self.entries.insert(
            key,
            MemoEntry {
                slot: local_slot,
                kind,
                epoch: epoch_before,
                kept,
            },
        );
        if !batch.push_raw(dst_index, local_slot) {
            flush(batch, self.start, self.step);
            let pushed = batch.push_raw(dst_index, local_slot);
            debug_assert!(pushed);
        }
        COUNTS[1].fetch_add(1, Relaxed);
        true
    }
}

impl Drop for ViewMemo {
    fn drop(&mut self) {
        self.flush_local_hits();
    }
}

/// Populates private CPU descriptor slot `dst` for `view` (using the global
/// cross-list `ViewCache` when enabled for that kind, else `make`).
/// Returns `(kept_resource, memoizable)`.
fn fill_slot(
    kind: Kind,
    view: &View,
    words: [u32; 3],
    dst: D3D12_CPU_DESCRIPTOR_HANDLE,
    use_cache_tex: bool,
    use_cache_buf: bool,
) -> Option<(ID3D12Resource, bool)> {
    match kind {
        Kind::Texture => {
            if use_cache_tex && matches!(view, View::Shader(..) | View::Placed(false, ..)) {
                return view_cache::cache()?.copy_texture_memo(view, words, dst);
            }
            let kept = make(view, dst).and_then(|(k, _)| k)?;
            Some((kept, !is_empty_tiled(view)))
        }
        Kind::Buffer => {
            if use_cache_buf && super::buffer_srv_args(view).is_some() {
                let kept = view_cache::cache()?.copy_buffer(view, words, dst)?;
                return Some((kept, true));
            }
            let kept = make(view, dst).and_then(|(k, _)| k)?;
            Some((kept, true))
        }
    }
}

thread_local! {
    static MEMO: RefCell<Option<ViewMemo>> = const { RefCell::new(None) };
}

/// Drops this thread's memoized entries if they belong to `recording`
/// (called when a command list resets or drops so held `ID3D12Resource`s
/// are released right away).
pub(crate) fn forget(recording: u64) {
    if recording == 0 {
        return;
    }
    MEMO.with(|cell| {
        if let Ok(mut memo) = cell.try_borrow_mut()
            && let Some(m) = memo.as_mut()
            && m.recording == recording
        {
            m.clear();
        }
    });
}

impl HostHeap {
    /// Makes dynamic slot `index` from this thread's per-list view memo for
    /// `recording` (populating through the global view cache when on, else
    /// `make`), queueing the descriptor copy into `batch`. Returns `false` if
    /// the host cannot make this view (nothing queued; caller falls back to
    /// `make_at`).
    pub(crate) fn make_memoized(
        &self,
        recording: u64,
        words: [u32; 3],
        view: &View,
        index: u32,
        batch: &mut CopyBatch,
    ) -> bool {
        if self.sampler || recording == 0 || words[0] != super::VIEW_TAG {
            return false;
        }
        let use_cache_tex = view_cache::on();
        let use_cache_buf = view_cache::buffers_on();
        MEMO.with(|cell| {
            let mut slot = cell.borrow_mut();
            if slot.is_none() {
                *slot = ViewMemo::new();
            }
            let Some(memo) = slot.as_mut() else {
                return false;
            };
            memo.resolve_slot(
                recording,
                words,
                view,
                index,
                self.cpu(index),
                batch,
                use_cache_tex,
                use_cache_buf,
                |b, base, step| b.flush_host(self, base, step),
                |res| self.keep_ref(index, res),
            )
        })
    }
}

/// One line a second, once the memo was asked.
pub(crate) fn line(frames: f64) {
    let c = COUNTS.each_ref().map(|a| a.swap(0, Relaxed));
    if frames <= 0.0 || c.iter().all(|&n| n == 0) {
        return;
    }
    let avg = if c[3] > 0 {
        c[4] as f64 / c[3] as f64
    } else {
        0.0
    };
    tracing::info!(
        "table view memo per frame (VICTORY_TABLE_VIEW_MEMO={}): {:.0} hits, {:.0} misses ({:.0} stale), \
         {:.0} copy calls for {:.0} slots ({:.1} slots/call)",
        u8::from(on()),
        c[0] as f64 / frames,
        c[1] as f64 / frames,
        c[2] as f64 / frames,
        c[3] as f64 / frames,
        c[4] as f64 / frames,
        avg,
    );
}

#[cfg(test)]
#[path = "view_memo_tests.rs"]
mod tests;
