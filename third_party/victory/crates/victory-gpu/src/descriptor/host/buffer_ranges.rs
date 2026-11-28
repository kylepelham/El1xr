//! (`VICTORY_HEAP_BUFFER_ADDRESS=1`) The buffer range of the view at each title slot of a view
//! heap, for shaders that pick buffers from the heap by an index they compute and read them by
//! title address (`victory_shader::emit::SLOT_BUFFER_RANGES`): an upload buffer of
//! `BUFFER_RANGES_COUNT` entries of four dwords:
//! - Direct copy (`BUFFER_RANGE_DIRECT` set in dword 1): byte offset in the copy,
//!   `BUFFER_RANGE_DIRECT | copy_number`, size in bytes, element stride. Skips the 64 KiB page
//!   directory and fault buffer, works for small/unaligned buffers (including shader-written
//!   constant buffers) and eliminates first-frame zero flash.
//! - Fallback (bit 31 clear): first byte's title address (low, high), size in bytes, element
//!   stride (resolved through `SLOT_MEMORY`); all 0 for a slot with no buffer view.
//!
//! The host heap's own title slots are written where the stride table is
//! ([`super::HostHeap::note_buffer_into`]) and upgraded to direct copies by `heap_buffers` and
//! draw-time `ensure_slot`. Title slots past them have no host counterpart: API writes update
//! them immediately ([`write_past`]), raw page writes mark their chunks ([`note_writes`]), and
//! [`refresh`] reads marked chunks on the same submission (and every [`RESCAN_EVERY`] frames all
//! of them, for a view let go without a write), [`READ_BUDGET`] slots a call at most.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};

use victory_shader::emit::BUFFER_RANGE_DIRECT;
use windows::Win32::Graphics::Direct3D12::ID3D12Resource;

use super::super::View;

/// Title slots a chunk covers: one 64 KiB page of 32-byte records.
const CHUNK: u32 = 2048;
/// Frames between looks at every title slot past the host heap.
const RESCAN_EVERY: u64 = 300;
/// Title slots read per [`refresh`] at most (the title rewrites about 95 pages of slots past the
/// host heap a frame, 194k slots).
const READ_BUDGET: u32 = 1 << 18;

/// One view heap's table.
pub(crate) struct Table {
    buffer: ID3D12Resource,
    gpu: u64,
    /// The mapping, `BUFFER_RANGES_COUNT * 4` dwords.
    data: usize,
    past: Mutex<Past>,
}

// SAFETY: the mapping is shared memory written with volatile stores; the rest is behind the lock.
unsafe impl Send for Table {}
// SAFETY: as above.
unsafe impl Sync for Table {}

/// The look at title slots past the host heap's.
#[derive(Default)]
struct Past {
    /// Chunk `k` holds title slots `k * CHUNK..`; whether written since last read.
    chunks: Vec<bool>,
    hand: usize,
    /// The record each slot past the host heap's last had (`generation << 32 | index`, 0:
    /// none), from title slot `count` on.
    keys: Vec<u64>,
    last_rescan: Option<u64>,
}

/// Slots read and entries changed past the host heap, and microseconds spent ([`say`]).
static COUNTS: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];
/// Host CBV views skipped on title slots under `VICTORY_HEAP_SKIP_CBV_VIEWS=1`.
static SKIPPED_CBV_VIEWS: AtomicU64 = AtomicU64::new(0);

/// (`VICTORY_HEAP_SKIP_CBV_VIEWS=1`, requires `VICTORY_HEAP_BUFFER_ADDRESS=1`) Skip creating
/// host CBV / raw-SRV descriptors on title view-heap slots (ranges table serves them directly).
pub(super) fn skip_cbv_views() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    skip_cbv_views_for(
        victory_shader::emit::heap_buffer_address(),
        *ON.get_or_init(|| {
            std::env::var_os("VICTORY_HEAP_SKIP_CBV_VIEWS").is_some_and(|v| v == "1")
        }),
    )
}

fn skip_cbv_views_for(hba: bool, switch: bool) -> bool {
    hba && switch
}

pub(super) fn note_cbv_view_skipped() {
    SKIPPED_CBV_VIEWS.fetch_add(1, Relaxed);
}

/// The table for a new view heap, zeroed; none with the switch off.
pub(super) fn make() -> Option<Table> {
    if !victory_shader::emit::heap_buffer_address() {
        return None;
    }
    let bytes = u64::from(victory_shader::emit::BUFFER_RANGES_COUNT) * 16;
    let (buffer, data) = crate::host::host()?.upload_buffer(bytes)?;
    // SAFETY: the buffer's whole mapping; a live buffer.
    let gpu = unsafe {
        std::ptr::write_bytes(data, 0, bytes as usize);
        buffer.GetGPUVirtualAddress()
    };
    crate::host::name(&buffer, || "buffer ranges by view heap slot".to_owned());
    tracing::info!(
        mib = bytes >> 20,
        "heap buffer address: a view heap's table of buffer ranges by title slot made (upload memory)"
    );
    Some(Table {
        buffer,
        gpu,
        data: data as usize,
        past: Mutex::default(),
    })
}

impl Table {
    /// Its buffer and GPU address, to bind.
    pub(crate) fn root(&self) -> (&ID3D12Resource, u64) {
        (&self.buffer, self.gpu)
    }
}

/// Extracts `(address, len, stride, is_cbv)` from a buffer view.
fn view_parts(view: Option<&View>) -> Option<(u64, u64, u32, bool)> {
    match view {
        Some(View::Constant(at, size)) if *at != 0 => Some((*at, u64::from(*size), 0, true)),
        Some(v) => super::buffer_range(v)
            .filter(|&(at, _)| at != 0)
            .map(|(at, len)| (at, len, super::view_stride(v).unwrap_or(0), false)),
        None => None,
    }
}

/// Pure title-address entry (for unit tests and fallback when no direct copy is available).
pub(crate) fn entry(view: Option<&View>) -> [u32; 4] {
    match view_parts(view) {
        Some((at, len, stride, _)) => [
            at as u32,
            (at >> 32) as u32,
            len.min(u64::from(u32::MAX)) as u32,
            stride,
        ],
        None => [0; 4],
    }
}

/// Encodes a direct-copy entry pointing at `(copy_num, offset_in_copy)`.
pub(crate) fn direct_entry(copy_num: u32, offset: u32, len: u64, stride: u32) -> [u32; 4] {
    [
        offset,
        BUFFER_RANGE_DIRECT | copy_num,
        len.min(u64::from(u32::MAX)) as u32,
        stride,
    ]
}

/// Builds the entry for `view`, registering a direct copy in [`crate::memory`] when the view
/// fits in one host copy (creating the copy only when `create_shadow` is true and memory is
/// committed), or premapping its pages on fallback.
fn entry_for(view: Option<&View>, create_shadow: bool) -> [u32; 4] {
    let Some((at, len, stride, is_cbv)) = view_parts(view) else {
        return [0; 4];
    };
    if len > 0 {
        if let Some((copy_num, off)) = crate::memory::register_buffer(at, len, is_cbv, create_shadow)
        {
            crate::memory::note_buffer_range_result(true);
            return direct_entry(copy_num, off, len, stride);
        }
        crate::memory::note_buffer_range_result(false);
        if create_shadow {
            crate::memory::premap_range(at, len);
        }
    }
    [
        at as u32,
        (at >> 32) as u32,
        len.min(u64::from(u32::MAX)) as u32,
        stride,
    ]
}

/// Writes title slot `index`'s entry (non-creating: upgrades immediately if its shadow exists).
pub(super) fn write(table: &Option<Table>, index: u32, view: Option<&View>) {
    let Some(table) = table else { return };
    put(table, index, entry_for(view, false));
}

/// Updates title slot `index` past `host.count` when an API descriptor call writes `handle`.
pub(super) fn write_past(table: &Table, index: u32, handle: u64) {
    let view = super::super::view(handle);
    put(table, index, entry_for(view.as_ref(), false));
}

/// Upgrades title slot `index`'s entry using the `shadow` already resolved by `heap_buffers`.
pub(super) fn write_shadow(
    table: &Option<Table>,
    index: u32,
    shadow: &Arc<crate::shadow::Shadow>,
    at: u64,
    len: u64,
    stride: u32,
) {
    let Some(table) = table else { return };
    if at == 0 || len == 0 {
        return;
    }
    if let Some(off) = shadow.offset_in(at)
        && shadow.left(at) >= len
        && off.checked_add(len).is_some_and(|e| e <= u64::from(u32::MAX))
        && let Some(copy_num) = crate::memory::register_shadow(shadow, false)
    {
        crate::memory::note_buffer_range_result(true);
        put(table, index, direct_entry(copy_num, off as u32, len, stride));
    }
}

/// Ensures title slot `index`'s entry and host copy are populated before a draw reads it
/// (used for table constant buffers lowered to read by title address).
pub(crate) fn ensure_slot(host: &super::HostHeap, index: u32, view: Option<&View>) {
    let Some(table) = host.buffer_ranges() else {
        return;
    };
    put(table, index, entry_for(view, true));
}

fn put(table: &Table, index: u32, words: [u32; 4]) {
    if index >= victory_shader::emit::BUFFER_RANGES_COUNT {
        return;
    }
    // SAFETY: inside the mapping; shaders read it while we write (each dword is one the slot had
    // or has).
    unsafe {
        let at = (table.data as *mut u32).add(4 * index as usize);
        for (k, w) in words.into_iter().enumerate() {
            at.add(k).write_volatile(w);
        }
    }
}

/// Pages the title wrote in a view heap at `start` (`step` bytes a slot), as byte ranges: the
/// chunks of those past the host heap's `count` title slots (of `title`) are read again.
pub(crate) fn note_writes(
    host: &super::HostHeap,
    ranges: &[(usize, usize)],
    start: u64,
    step: u64,
    title: u32,
) {
    let Some(table) = host.buffer_ranges() else {
        return;
    };
    let count = host.count;
    if count >= title {
        return;
    }
    let mut past = table.past.lock().unwrap_or_else(|e| e.into_inner());
    for &range in ranges {
        let Some((lo, hi)) =
            super::heap_cap::slots_past(range, start as usize, step as usize, count, title)
        else {
            continue;
        };
        mark(&mut past, lo, hi, title);
    }
}

fn mark(past: &mut Past, lo: u32, hi: u32, title: u32) {
    let chunks = title.div_ceil(CHUNK) as usize;
    if past.chunks.len() < chunks {
        past.chunks.resize(chunks, false);
    }
    for k in (lo / CHUNK)..hi.div_ceil(CHUNK) {
        if let Some(c) = past.chunks.get_mut(k as usize) {
            *c = true;
        }
    }
}

/// Reads marked chunks of title slots past the host heap's (all of them every [`RESCAN_EVERY`]
/// frames) and writes their entries; called on each heap sync after [`note_writes`].
pub(crate) fn refresh(start: u64, step: u64, host: &super::HostHeap, title: u32) {
    let Some(table) = host.buffer_ranges() else {
        return;
    };
    let count = host.count;
    if count >= title || step != 32 {
        return;
    }
    let started = std::time::Instant::now();
    let frame = crate::frame::current();
    let mut past = table.past.lock().unwrap_or_else(|e| e.into_inner());
    let past = &mut *past;
    if past.last_rescan.is_none_or(|l| frame >= l + RESCAN_EVERY) {
        past.last_rescan = Some(frame);
        mark(past, count, title, title);
        // every entry written again: a view let go keeps its record
        past.keys.fill(u64::MAX);
    }
    if past.keys.len() < (title - count) as usize {
        past.keys.resize((title - count) as usize, 0);
    }
    static BYTES: Mutex<Vec<u8>> = Mutex::new(Vec::new());
    let mut bytes = BYTES.lock().unwrap_or_else(|e| e.into_inner());
    if bytes.len() < (CHUNK * 32) as usize {
        bytes.resize((CHUNK * 32) as usize, 0);
    }
    let (mut read, mut changed) = (0u32, 0u64);
    let mut held = None;
    let n = past.chunks.len();
    for _ in 0..n {
        if read >= READ_BUDGET {
            break;
        }
        let k = past.hand;
        past.hand = (past.hand + 1) % n;
        if !std::mem::take(&mut past.chunks[k]) {
            continue;
        }
        let lo = (k as u32 * CHUNK).max(count);
        let hi = ((k as u32 + 1) * CHUNK).min(title);
        if lo >= hi {
            continue;
        }
        let buf = &mut bytes[..((hi - lo) * 32) as usize];
        if !crate::host::read_into((start + u64::from(lo) * step) as *const u8, buf) {
            continue;
        }
        read += hi - lo;
        for (j, record) in buf.chunks_exact(32).enumerate() {
            let word = |w: usize| {
                u32::from_le_bytes(record[w * 4..w * 4 + 4].try_into().unwrap_or_default())
            };
            let [tag, i, generation] = [word(0), word(1), word(2)];
            let slot = lo + j as u32;
            let key = if tag == super::super::VIEW_TAG {
                u64::from(generation) << 32 | u64::from(i)
            } else {
                0
            };
            let known = &mut past.keys[(slot - count) as usize];
            if *known == key {
                continue;
            }
            *known = key;
            let view = (key != 0)
                .then(|| super::super::view_with(i, generation, &mut held))
                .flatten();
            put(table, slot, entry_for(view.as_ref(), true));
            changed += 1;
        }
    }
    COUNTS[0].fetch_add(u64::from(read), Relaxed);
    COUNTS[1].fetch_add(changed, Relaxed);
    COUNTS[2].fetch_add(started.elapsed().as_micros() as u64, Relaxed);
    say(frame);
}

/// Every [`RESCAN_EVERY`] frames: what [`refresh`] read and changed since.
fn say(frame: u64) {
    static LAST: AtomicU64 = AtomicU64::new(0);
    let last = LAST.load(Relaxed);
    if frame < last + RESCAN_EVERY
        || LAST
            .compare_exchange(last, frame, Relaxed, Relaxed)
            .is_err()
    {
        return;
    }
    let [read, changed, us] = [0, 1, 2].map(|k| COUNTS[k].swap(0, Relaxed));
    let skipped_cbv_views = SKIPPED_CBV_VIEWS.swap(0, Relaxed);
    tracing::info!(
        frames = frame - last,
        read,
        changed,
        skipped_cbv_views,
        ms = us as f64 / 1000.0,
        "heap buffer address: title slots past the host heap read for their buffer ranges"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_constant_buffer_gives_its_range_and_others_nothing() {
        assert_eq!(
            entry(Some(&View::Constant(0x12_3456_7800, 256))),
            [0x3456_7800, 0x12, 256, 0]
        );
        assert_eq!(entry(Some(&View::Constant(0, 256))), [0; 4]);
        assert_eq!(entry(Some(&View::Shader(0, None))), [0; 4]);
        assert_eq!(entry(None), [0; 4]);
    }

    #[test]
    fn direct_entry_sets_direct_bit_and_preserves_offset_size_and_stride() {
        let e = direct_entry(42, 0x120, 0x340, 16);
        assert_eq!(e, [0x120, BUFFER_RANGE_DIRECT | 42, 0x340, 16]);
        assert_ne!(e[1] & BUFFER_RANGE_DIRECT, 0);
        assert_eq!(e[1] & !BUFFER_RANGE_DIRECT, 42);
    }

    #[test]
    fn written_pages_mark_their_chunks_and_a_rescan_all() {
        let mut past = Past::default();
        mark(&mut past, 4096, 4097, 3 * CHUNK + 5);
        assert_eq!(past.chunks, [false, false, true, false]);
        mark(&mut past, 0, 3 * CHUNK + 5, 3 * CHUNK + 5);
        assert_eq!(past.chunks, [true; 4]);
    }

    #[test]
    fn skip_cbv_views_requires_both_switches() {
        assert!(!skip_cbv_views_for(false, false));
        assert!(!skip_cbv_views_for(false, true));
        assert!(!skip_cbv_views_for(true, false));
        assert!(skip_cbv_views_for(true, true));
    }
}
