//! Title memory by address, for translated shaders that read and write
//! through raw 40-bit addresses (`victory_shader::emit::SLOT_MEMORY`: the
//! console's ray-tracing structure builder and the programs that walk its
//! structures; the structures themselves lie in plain title memory, in no
//! buffer object) and, under `VICTORY_HEAP_BUFFER_ADDRESS=1`, heap-picked
//! buffers and table constant buffers (`victory_shader::emit::SLOT_BUFFER_RANGES`).
//!
//! The copies are the ones every other access uses ([`crate::shadow`]: a
//! title buffer's copy, or the 4 MiB window of memory outside buffers), so
//! a root view and a raw address reach the same host bytes. This module
//! keeps what a shader needs to find them:
//!
//! - the **directory**: per 64 KiB page of title address space the number
//!   of the copy that holds the page (0: none) and the page's byte offset
//!   in it; one buffer for all lists, entries only ever go from empty to
//!   filled;
//! - per view heap a run of slots with a raw unordered access view of each
//!   numbered copy (slot 0 of the run: a null view, for pages without a
//!   copy);
//! - the **faults**: the page numbers shaders found empty, read back here
//!   once a frame, so the copies exist from the next frame on. A pointer a
//!   shader takes out of memory cannot be known before it runs; the
//!   addresses a dispatch is given as root views are made ready before it.
//!
//! **A page that is not there when a shader touches it is a lost access**:
//! the read gives 0, the store is dropped, and the fault note only helps
//! the next frame. For the structure builder that is the same as a dropped
//! stage, so before a dispatch the host makes ready everything it can see
//! the dispatch name: the whole copy around each root view, around each
//! pair of root constants that is a title address and around each root
//! constant that is one in 256-byte units (the builder passes structures
//! all three ways), and the 4 MiB after each (a structure may cross a
//! window's end).
//! shortcut: what a stage reaches only through pointers it reads out of
//! memory (InitHeaders' table of structure addresses, LoadInstances'
//! instance records) is not made ready unless something else named it;
//! the fault count in the log says whether that happens. The whole answer
//! is the extent of the title's allocation around an address, which the
//! memory module of the layer knows and this one does not ask yet.
//!
//! shortcut: a copy that is numbered is kept for the rest of the run, and
//! there are at most [`COPIES`] of them by default ([`COPIES_BUFFER_ADDRESS`]
//! under `VICTORY_HEAP_BUFFER_ADDRESS=1`, where the title has ~5,200+ live
//! buffer host copies). Every dispatch of a raw-address shader brings
//! per-list copies up to date and, if it can store, declares all of them
//! written (the bytes are compared on the CPU afterwards, as for any
//! unordered access view): a dirty-page mask written by the shader would
//! bound both. `VICTORY_BVH_DECLARE_OUTPUTS=1` (`crate::bvh_declare`)
//! declares written only the builds' outputs and what writing draws' root
//! arguments name.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use victory_shader::emit::{MEMORY_FAULTS, MEMORY_PAGES};
use windows::Win32::Graphics::Direct3D12::*;

use crate::descriptor::host::HostHeap;
use crate::host::{HostList, host};
use crate::shadow::Shadow;

/// Copies that can be numbered without `VICTORY_HEAP_BUFFER_ADDRESS=1` (the
/// run of heap slots is one longer).
const COPIES: u32 = 1023;
/// Copies that can be numbered with `VICTORY_HEAP_BUFFER_ADDRESS=1`: the
/// retail title warms ~5,192 distinct buffer copies on the loading screen
/// alone plus ~500-900 loose windows. 16,384 dynamic slots (~6-9% of a view
/// heap's dynamic pool) gives >3x headroom so pages never silently read 0.
const COPIES_BUFFER_ADDRESS: u32 = 16_383;
/// Bit 31 of [`Shadow::memory_copy`]: this copy is refreshed on every list's
/// first use (raw-address copies and copies backing constant buffers).
const PER_LIST_BIT: u32 = 1 << 31;
/// Maximum 64 KiB pages premapped for one fallback multi-window buffer range.
const PREMAP_MAX_PAGES: u64 = 256;
/// A window of memory outside buffers (`shadow.rs`).
const CELL: u64 = 4 << 20;
/// A directory page.
const PAGE: u64 = 1 << 16;

fn max_copies() -> u32 {
    max_copies_for(victory_shader::emit::heap_buffer_address())
}

fn max_copies_for(hba: bool) -> u32 {
    if hba {
        COPIES_BUFFER_ADDRESS
    } else {
        COPIES
    }
}

/// Cached GPU addresses for lock-free [`ready_buffers`] fast path.
static DIR_GPU: AtomicU64 = AtomicU64::new(0);
static FAULTS_GPU: AtomicU64 = AtomicU64::new(0);
/// Incremented whenever a copy is added or promoted to `per_list`; starts at 1
/// so 0 (initial [`ListReady`] state) never matches.
static GEN: AtomicU64 = AtomicU64::new(1);

/// (`VICTORY_HEAP_BUFFER_ADDRESS=1`) Counters since last summary:
/// `[direct_ranges, fallback_ranges, faults_total, cap_misses, uav_refused]`.
static HBA_STATS: [AtomicU64; 5] = [const { AtomicU64::new(0) }; 5];

/// Per-[`HostList`] fast-path cache for [`ready_buffers`]: avoids locking the
/// global `MEMORY` mutex on draws once the list is synced at the current
/// `(frame, GEN, heap)`.
#[derive(Default)]
pub(crate) struct ListReady {
    epoch: AtomicU64,
    frame: AtomicU64,
    heap: AtomicUsize,
    base: AtomicU32,
    seen: AtomicU32,
}

impl ListReady {
    pub(crate) fn reset(&self) {
        self.epoch.store(0, Ordering::Relaxed);
        self.frame.store(0, Ordering::Relaxed);
        self.heap.store(0, Ordering::Relaxed);
        self.base.store(0, Ordering::Relaxed);
        self.seen.store(0, Ordering::Relaxed);
    }
}

struct Memory {
    /// Buffer, its mapping, its GPU address.
    directory: (ID3D12Resource, *mut u32, u64),
    faults: (ID3D12Resource, *mut u32, u64),
    /// Copy `n` is `copies[n - 1]`.
    copies: Vec<Arc<Shadow>>,
    /// Frame when each copy was numbered (new copies are refreshed on every
    /// list of their birth frame to avoid first-frame zero flash under parallel
    /// list recording).
    added_frame: Vec<u64>,
    /// Indices into `copies` that must be refreshed on every list's first use
    /// (raw-address copies and copies backing constant buffers).
    per_list: Vec<usize>,
    /// Parallel to `copies`: whether index `n` is in `per_list`.
    per_list_flag: Vec<bool>,
    /// Last frame on which all non-`per_list` copies were refreshed.
    per_frame_refreshed: u64,
    /// Last frame on which the `VICTORY_HEAP_BUFFER_ADDRESS=1` summary logged.
    hba_last_log: u64,
    /// Per host view heap (by address): the first slot of its run, and how
    /// many copies have their view there.
    heaps: HashMap<usize, (u32, u32)>,
    /// The frame the faults were last read in.
    frame: u64,
    /// Pages asked for this frame that are not the title's memory.
    refused: std::collections::HashSet<u64>,
    /// The first copy numbered by [`premap`] or [`register_shadow`] since the
    /// last [`ready`] (`usize::MAX`: none): the next draw brings it up to date.
    premapped: usize,
    /// (`VICTORY_BVH_DECLARE_OUTPUTS`, `crate::bvh_declare`) Per copy, the
    /// generation it became written in (0: read only), the last generation,
    /// per list the generation it last declared at, the check's cursor and
    /// frame.
    written_at: Vec<u64>,
    generation: u64,
    lists: HashMap<usize, u64>,
    check: (usize, u64),
}

// SAFETY: the mappings are of buffers the struct owns; all access is under
// the mutex.
unsafe impl Send for Memory {}

static MEMORY: Mutex<Option<Memory>> = Mutex::new(None);

/// What a draw binds.
pub(crate) struct Ready {
    pub(crate) directory: u64,
    pub(crate) faults: u64,
    /// The view heap slot of the null view; copy `n`'s view is `n` further.
    pub(crate) base: u32,
}

impl Memory {
    fn new() -> Option<Memory> {
        let host = host()?;
        let make = |dwords: u64, what: &str| {
            let (buffer, data) = host.shared_buffer(dwords * 4)?;
            crate::host::name(&buffer, || format!("title memory by address: {what}"));
            // SAFETY: a fresh mapping of at least that many bytes.
            unsafe { std::ptr::write_bytes(data, 0, (dwords * 4) as usize) };
            // SAFETY: a live buffer.
            let gpu = unsafe { buffer.GetGPUVirtualAddress() };
            Some((buffer, data.cast::<u32>(), gpu))
        };
        let directory = make(u64::from(MEMORY_PAGES) * 2, "directory")?;
        let faults = make(u64::from(MEMORY_FAULTS), "faults")?;
        DIR_GPU.store(directory.2, Ordering::Release);
        FAULTS_GPU.store(faults.2, Ordering::Release);
        tracing::info!(
            cap = max_copies(),
            "title memory by address: a shader reads or writes through raw addresses"
        );
        Some(Memory {
            directory,
            faults,
            copies: Vec::new(),
            added_frame: Vec::new(),
            per_list: Vec::new(),
            per_list_flag: Vec::new(),
            per_frame_refreshed: u64::MAX,
            hba_last_log: 0,
            heaps: HashMap::new(),
            frame: u64::MAX,
            refused: Default::default(),
            premapped: usize::MAX,
            written_at: Vec::new(),
            generation: 0,
            lists: HashMap::new(),
            check: (0, u64::MAX),
        })
    }

    /// The copy (its index) that holds `address`'s page.
    fn copy_at(&self, address: u64) -> Option<usize> {
        let page = address / PAGE;
        if page >= u64::from(MEMORY_PAGES) - 1 {
            return None;
        }
        // SAFETY: inside the directory's mapping.
        let n = unsafe { self.directory.1.add(page as usize * 2).read() };
        (n != 0).then(|| n as usize - 1)
    }

    /// (`crate::bvh_declare`) The copies around `address` and the 4 MiB
    /// after become written if their cells now are.
    fn rejudge(&mut self, address: u64) {
        let cells = crate::bvh_declare::cells();
        for a in [address, (address & !(CELL - 1)) + CELL] {
            if let Some(n) = self.copy_at(a)
                && self.written_at[n] == 0
                && {
                    let (start, len) = self.copies[n].span();
                    cells.span_written(start, len)
                }
            {
                self.generation += 1;
                self.written_at[n] = self.generation;
            }
        }
    }

    /// Ensures `shadow` has a copy index `0..max_copies()`, maps all whole
    /// 64 KiB pages it covers in the directory, and marks it `per_list` if asked.
    fn insert_copy(&mut self, shadow: &Arc<Shadow>, per_list: bool, frame: u64) -> Option<usize> {
        let raw = shadow.memory_copy.load(Ordering::Acquire);
        let existing = raw & !PER_LIST_BIT;
        if existing != 0 {
            let n = existing as usize - 1;
            if per_list && !self.per_list_flag[n] {
                self.per_list_flag[n] = true;
                self.per_list.push(n);
                shadow
                    .memory_copy
                    .store(existing | PER_LIST_BIT, Ordering::Release);
                GEN.fetch_add(1, Ordering::Release);
            }
            return Some(n);
        }
        let cap = max_copies() as usize;
        if self.copies.len() >= cap {
            HBA_STATS[3].fetch_add(1, Ordering::Relaxed);
            static ONCE: std::sync::Once = std::sync::Once::new();
            ONCE.call_once(|| {
                tracing::warn!(
                    cap,
                    "title memory by address: no number left for another copy; its pages read zeros"
                )
            });
            return None;
        }
        self.copies.push(shadow.clone());
        let n = self.copies.len() - 1;
        self.added_frame.push(frame);
        self.per_list_flag.push(per_list);
        if per_list {
            self.per_list.push(n);
        }
        let (s, l) = shadow.span();
        let written = crate::bvh_declare::on() && crate::bvh_declare::cells().span_written(s, l);
        self.generation += u64::from(written);
        self.written_at.push(if written { self.generation } else { 0 });
        let num = n as u32 + 1;
        shadow.memory_copy.store(
            num | if per_list { PER_LIST_BIT } else { 0 },
            Ordering::Release,
        );
        self.premapped = self.premapped.min(n);
        // Every page the copy holds whole, not only the one asked for: a
        // structure is more than 64 KiB.
        let mut at = s.next_multiple_of(PAGE);
        while at + PAGE <= s + l && at / PAGE < u64::from(MEMORY_PAGES) - 1 {
            // SAFETY: inside the directory's mapping.
            let other = unsafe { self.directory.1.add((at / PAGE) as usize * 2) };
            // SAFETY: as above; the offset first, as below.
            unsafe {
                if other.read() == 0 {
                    other.add(1).write_volatile((at - s) as u32);
                    other.write_volatile(num);
                }
            }
            at += PAGE;
        }
        GEN.fetch_add(1, Ordering::Release);
        Some(n)
    }

    /// Gives the page of `address` its copy, if it has none and the memory
    /// is the title's.
    fn map(&mut self, address: u64) {
        let page = address / PAGE;
        // The last entry stays empty: addresses past the directory read it.
        if page >= u64::from(MEMORY_PAGES) - 1 {
            return;
        }
        // SAFETY: inside the directory's mapping.
        let entry = unsafe { self.directory.1.add(page as usize * 2) };
        // SAFETY: as above.
        if unsafe { entry.read() } != 0 {
            return;
        }
        let start = page * PAGE;
        // A shader's pointer, or a constant taken for an address, may be
        // anything: only memory the title has.
        if self.refused.contains(&page) {
            return;
        }
        if !crate::texture::committed(start as *const u8, PAGE as usize) {
            self.refused.insert(page);
            return;
        }
        // The copy a root view at that address would get; a buffer that
        // ends inside the page is passed over for what holds all of it.
        let whole = |s: &Arc<Shadow>| s.offset_in(start).is_some_and(|_| s.left(start) >= PAGE);
        let Some(shadow) = crate::shadow::find(start, 1)
            .filter(whole)
            .or_else(|| crate::shadow::find(start, PAGE).filter(whole))
        else {
            return;
        };
        // SAFETY: a live buffer.
        let flags = unsafe { shadow.buffer.GetDesc() }.Flags;
        if flags.0 & D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS.0 == 0 {
            // (Neptune) A rebar window has none: refused for unordered access.
            if shadow.is_rebar() {
                crate::shadow::REBAR[7].fetch_add(1, Ordering::Relaxed);
            }
            HBA_STATS[4].fetch_add(1, Ordering::Relaxed);
            return;
        }
        let frame = crate::frame::current();
        let Some(n) = self.insert_copy(&shadow, true, frame) else {
            return;
        };
        let offset = shadow.offset_in(start).unwrap_or(0);
        // The offset first: a shader that sees the number sees it too.
        // SAFETY: inside the directory's mapping.
        unsafe {
            entry.add(1).write_volatile(offset as u32);
            entry.write_volatile(n as u32 + 1);
        }
    }

    /// The pages shaders found empty since the last frame.
    fn take_faults(&mut self) {
        let mut pages = Vec::new();
        for i in 1..MEMORY_FAULTS as usize {
            // SAFETY: inside the faults' mapping; the GPU may be writing,
            // a number missed now is written again next frame.
            let at = unsafe { self.faults.1.add(i) };
            // SAFETY: as above.
            let page = unsafe { at.read_volatile() };
            if page != 0 {
                pages.push(page);
                // SAFETY: as above.
                unsafe { at.write_volatile(0) };
            }
        }
        if !pages.is_empty() {
            HBA_STATS[2].fetch_add(pages.len() as u64, Ordering::Relaxed);
            // Each is an access that read 0 or was dropped.
            static SAID: AtomicU32 = AtomicU32::new(0);
            if SAID.fetch_add(1, Ordering::Relaxed) < 50 {
                tracing::warn!(
                    pages = pages.len(),
                    first = format_args!("{:#x}", u64::from(pages[0]) * PAGE),
                    copies = self.copies.len(),
                    cap = max_copies(),
                    "title memory by address: shaders touched pages that had no copy (those accesses read 0 or were dropped); the copies are made now"
                );
            }
        }
        for page in pages {
            self.map(u64::from(page) * PAGE);
        }
    }

    /// What a dispatch names: the copy around `address` and the 4 MiB
    /// after it.
    fn name(&mut self, address: u64) {
        self.map(address);
        self.map((address & !(CELL - 1)) + CELL);
    }

    /// Ensures `heap` has its run of `max_copies() + 1` dynamic slots and a
    /// raw UAV for every numbered copy so far.
    fn sync_heap(&mut self, heap: &HostHeap) -> Option<u32> {
        let key = std::ptr::from_ref(heap) as usize;
        let (base, made) = match self.heaps.get(&key) {
            Some(&run) => run,
            None => {
                // Held for the heap's life: not given back with a list's slots.
                let base = heap.dynamic(max_copies() + 1)?;
                heap.null_raw_uav_at(base);
                (base, 0)
            }
        };
        for (n, shadow) in self.copies.iter().enumerate().skip(made as usize) {
            let (_, len) = shadow.span();
            heap.raw_uav_at(base + 1 + n as u32, &shadow.buffer, 0, len);
        }
        self.heaps.insert(key, (base, self.copies.len() as u32));
        Some(base)
    }

    /// Refreshes copies needed on `list` under `VICTORY_HEAP_BUFFER_ADDRESS=1`:
    /// per-list copies (CBVs and raw-address copies) and copies born this frame
    /// on every list's first read; all other copies once per frame; any copies
    /// newly added mid-list immediately.
    fn refresh_for_list(&mut self, list: &HostList, frame: u64, first_read: bool, prev_seen: usize) {
        if first_read {
            for &n in &self.per_list {
                let (start, len) = self.copies[n].span();
                self.copies[n].refresh(list, start, len);
            }
            if self.per_frame_refreshed != frame {
                self.per_frame_refreshed = frame;
                for shadow in &self.copies {
                    let (start, len) = shadow.span();
                    shadow.refresh(list, start, len);
                }
            } else {
                for (n, shadow) in self.copies.iter().enumerate() {
                    if !self.per_list_flag[n] && self.added_frame[n] == frame {
                        let (start, len) = shadow.span();
                        shadow.refresh(list, start, len);
                    }
                }
            }
        } else if prev_seen < self.copies.len() {
            for shadow in &self.copies[prev_seen..] {
                let (start, len) = shadow.span();
                shadow.refresh(list, start, len);
            }
        }
    }

    /// Periodic summary when `VICTORY_HEAP_BUFFER_ADDRESS=1`.
    fn maybe_say_hba(&mut self, frame: u64) {
        if !victory_shader::emit::heap_buffer_address() || frame < self.hba_last_log + SAY_EVERY {
            return;
        }
        let frames = frame - self.hba_last_log;
        self.hba_last_log = frame;
        let [direct, fallback, faults, cap_misses, uav_refused] =
            [0, 1, 2, 3, 4].map(|k| HBA_STATS[k].swap(0, Ordering::Relaxed));
        tracing::info!(
            frames,
            copies = self.copies.len(),
            cap = max_copies(),
            per_list = self.per_list.len(),
            direct,
            fallback,
            faults,
            cap_misses,
            uav_refused,
            "heap buffer address: memory directory copies and range lookups"
        );
    }
}

/// Counts direct vs fallback buffer range writes for the periodic summary.
pub(crate) fn note_buffer_range_result(direct: bool) {
    HBA_STATS[usize::from(!direct)].fetch_add(1, Ordering::Relaxed);
}

/// Registers `shadow` in the memory directory and returns its 1-based copy
/// number (`1..=max_copies()`). Lock-free once registered (and promoted if
/// `per_list`).
pub(crate) fn register_shadow(shadow: &Arc<Shadow>, per_list: bool) -> Option<u32> {
    let raw = shadow.memory_copy.load(Ordering::Acquire);
    let num = raw & !PER_LIST_BIT;
    if num != 0 && (!per_list || raw & PER_LIST_BIT != 0) {
        return Some(num);
    }
    // SAFETY: a live buffer.
    let flags = unsafe { shadow.buffer.GetDesc() }.Flags;
    if flags.0 & D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS.0 == 0 {
        if shadow.is_rebar() {
            crate::shadow::REBAR[7].fetch_add(1, Ordering::Relaxed);
        }
        HBA_STATS[4].fetch_add(1, Ordering::Relaxed);
        return None;
    }
    let mut guard = MEMORY.lock().unwrap_or_else(|e| e.into_inner());
    if guard.is_none() {
        *guard = Memory::new();
    }
    let m = guard.as_mut()?;
    let frame = crate::frame::current();
    let n = m.insert_copy(shadow, per_list, frame)?;
    Some(n as u32 + 1)
}

/// (`VICTORY_HEAP_BUFFER_ADDRESS=1`) Finds (or creates if `create` and
/// committed) the single [`Shadow`] covering `[at, at + len)`, registers it in
/// the memory directory, and returns `(copy_number, byte_offset_in_copy)` for
/// a `BUFFER_RANGE_DIRECT` entry. When `is_cbv`, marks the copy for per-list
/// refresh and notifies [`crate::port_cbv::viewed`] so `CopyBufferRegion`
/// keeps GPU copy order.
pub(crate) fn register_buffer(at: u64, len: u64, is_cbv: bool, create: bool) -> Option<(u32, u32)> {
    let shadow = crate::shadow::find_existing(at, len).or_else(|| {
        if create && crate::texture::committed(at as *const u8, 1) {
            crate::shadow::find(at, len)
        } else {
            None
        }
    })?;
    let off = shadow.offset_in(at)?;
    if shadow.left(at) < len {
        return None;
    }
    if off.checked_add(len).is_none_or(|end| end > u64::from(u32::MAX)) {
        return None;
    }
    let copy = register_shadow(&shadow, is_cbv)?;
    if is_cbv {
        crate::port_cbv::viewed(&shadow);
    }
    Some((copy, off as u32))
}

/// Fallback for a buffer range that spans multiple loose windows (cannot fit
/// in a single direct copy): maps its 64 KiB pages into the memory directory
/// ahead of draws so shaders do not flash zeros on frame 1.
pub(crate) fn premap_range(at: u64, len: u64) {
    if at == 0 || len == 0 {
        return;
    }
    let first_page = at / PAGE;
    let last_page = at.saturating_add(len - 1) / PAGE;
    let pages = (last_page - first_page + 1).min(PREMAP_MAX_PAGES);
    let mut guard = MEMORY.lock().unwrap_or_else(|e| e.into_inner());
    if guard.is_none() {
        *guard = Memory::new();
    }
    let Some(m) = guard.as_mut() else {
        return;
    };
    for p in first_page..first_page + pages {
        m.map(p * PAGE);
    }
}

/// `VICTORY_BVH_PREMAP` (`crate::bvh_premap`): the copies around the
/// structure builder's ranges (`addresses`: destinations, sources, scratch,
/// instances, vertices and indices of the builds the title asks for), made
/// before the builder's first dispatch rather than from its faults a frame
/// later. Brought up to date (and declared written) by the next draw that
/// reads title memory by address.
pub(crate) fn premap(addresses: &[u64]) {
    let mut guard = MEMORY.lock().unwrap_or_else(|e| e.into_inner());
    if guard.is_none() {
        *guard = Memory::new();
    }
    let Some(m) = guard.as_mut() else {
        return;
    };
    let before = m.copies.len();
    for &address in addresses.iter().filter(|&&a| a != 0) {
        m.name(address);
    }
    if m.copies.len() > before {
        m.premapped = m.premapped.min(before);
    }
}

/// (`VICTORY_BVH_DECLARE_OUTPUTS`, `crate::bvh_declare`) The builder hook
/// names every range of a build (`named`) and which of them it writes
/// (`outputs`: destination, scratch); copies already numbered around an
/// output become written.
pub(crate) fn hook(outputs: &[u64], named: &[u64]) {
    if !crate::bvh_declare::on() {
        return;
    }
    crate::bvh_declare::cells().hook(outputs, named);
    let mut guard = MEMORY.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(m) = guard.as_mut() {
        for &a in outputs.iter().filter(|&&a| a != 0) {
            m.rejudge(a);
        }
    }
}

const READ: usize = usize::MAX - 0x6d;
const WRITE: usize = usize::MAX - 0x6e;

/// (`VICTORY_HEAP_BUFFER_ADDRESS=1`) Fast path for draws whose shader only
/// uses `SLOT_MEMORY` via `SLOT_BUFFER_RANGES` (read-only, no raw-address
/// root constants to scan): lock-free once `list` is synced at `(frame, GEN)`.
pub(crate) fn ready_buffers(list: &HostList, heap: &HostHeap) -> Option<Ready> {
    let frame = crate::frame::current();
    let epoch = GEN.load(Ordering::Acquire);
    let key = std::ptr::from_ref(heap) as usize;
    if list.memory_ready.epoch.load(Ordering::Acquire) == epoch
        && list.memory_ready.frame.load(Ordering::Relaxed) == frame
        && list.memory_ready.heap.load(Ordering::Relaxed) == key
    {
        let directory = DIR_GPU.load(Ordering::Relaxed);
        let faults = FAULTS_GPU.load(Ordering::Relaxed);
        let base = list.memory_ready.base.load(Ordering::Relaxed);
        if directory != 0 {
            return Some(Ready {
                directory,
                faults,
                base,
            });
        }
    }
    let mut guard = MEMORY.lock().unwrap_or_else(|e| e.into_inner());
    if guard.is_none() {
        *guard = Memory::new();
    }
    let m = guard.as_mut()?;
    if m.frame != frame {
        m.frame = frame;
        m.refused.clear();
        m.take_faults();
        if crate::bvh_declare::on() && frame.is_multiple_of(SAY_EVERY) {
            say(m);
        }
        m.maybe_say_hba(frame);
    }
    let base = m.sync_heap(heap)?;
    let first_read = list.first_use(READ);
    let prev_seen = list.memory_ready.seen.load(Ordering::Relaxed) as usize;
    m.refresh_for_list(list, frame, first_read, prev_seen);
    if first_read {
        list.keep(&m.directory.0);
        list.keep(&m.faults.0);
    }
    let cur_gen = GEN.load(Ordering::Acquire);
    list.memory_ready
        .seen
        .store(m.copies.len() as u32, Ordering::Relaxed);
    list.memory_ready.heap.store(key, Ordering::Relaxed);
    list.memory_ready.base.store(base, Ordering::Relaxed);
    list.memory_ready.frame.store(frame, Ordering::Relaxed);
    list.memory_ready.epoch.store(cur_gen, Ordering::Release);
    Some(Ready {
        directory: m.directory.2,
        faults: m.faults.2,
        base,
    })
}

/// Before a draw whose shader reads title memory by address: what its
/// root arguments name gets its copies (`addresses`: its root views;
/// `constants`: its root constants, see the module), every numbered copy
/// has its view in `heap`'s run and is brought up to date when `list` is
/// submitted (and, with `writes`, copied back after). None if the host
/// cannot hold it; the draw is then dropped.
pub(crate) fn ready(
    list: &HostList,
    heap: &HostHeap,
    addresses: &[u64],
    constants: &[u32],
    writes: bool,
) -> Option<Ready> {
    let mut guard = MEMORY.lock().unwrap_or_else(|e| e.into_inner());
    if guard.is_none() {
        *guard = Memory::new();
    }
    let m = guard.as_mut()?;
    let frame = crate::frame::current();
    if m.frame != frame {
        m.frame = frame;
        m.refused.clear();
        m.take_faults();
        if crate::bvh_declare::on() && frame.is_multiple_of(SAY_EVERY) {
            say(m);
        }
        m.maybe_say_hba(frame);
    }
    let numbered = m.copies.len().min(std::mem::replace(&mut m.premapped, usize::MAX));
    // Only the builder's outputs are declared written (`crate::bvh_declare`).
    let outputs = writes && crate::bvh_declare::on();
    for address in named(addresses, constants) {
        if outputs {
            crate::bvh_declare::cells().root_output(address);
        }
        m.name(address);
        if outputs {
            m.rejudge(address);
        }
    }
    let base = m.sync_heap(heap)?;
    // Once per list, and for copies numbered since: the builder alone
    // dispatches hundreds of times a frame.
    // shortcut: a copy another list numbers meanwhile is brought up to date
    // by that list's submission, which may run after this one's.
    let first_read = list.first_use(READ);
    let first_write = writes && list.first_use(WRITE);
    // With only the outputs declared: the written copies the list has not
    // declared yet (all of them at its first writing draw).
    let declare = if outputs {
        let at = std::ptr::from_ref(list) as usize;
        let from = if first_write { None } else { m.lists.get(&at).copied() };
        let fresh = from.is_none_or(|g| g < m.generation);
        m.lists.insert(at, m.generation);
        if first_write {
            count(m);
        }
        if fresh {
            crate::bvh_declare::to_declare(&m.written_at, from)
        } else {
            Vec::new()
        }
    } else {
        Vec::new()
    };
    if victory_shader::emit::heap_buffer_address() && !writes {
        let prev_seen = list.memory_ready.seen.load(Ordering::Relaxed) as usize;
        m.refresh_for_list(list, frame, first_read, prev_seen.min(numbered));
    } else {
        for (n, shadow) in m.copies.iter().enumerate() {
            let (start, len) = shadow.span();
            if first_read || n >= numbered {
                shadow.refresh(list, start, len);
            }
            if writes && !outputs && (first_write || n >= numbered) {
                shadow.gpu_writes(list, start, len);
            }
        }
    }
    for n in declare {
        let (start, len) = m.copies[n].span();
        m.copies[n].gpu_writes(list, start, len);
    }
    if outputs {
        check(m, list, frame);
    }
    if first_read {
        list.keep(&m.directory.0);
        list.keep(&m.faults.0);
    }
    let key = std::ptr::from_ref(heap) as usize;
    list.memory_ready
        .seen
        .store(m.copies.len() as u32, Ordering::Relaxed);
    list.memory_ready.heap.store(key, Ordering::Relaxed);
    list.memory_ready.base.store(base, Ordering::Relaxed);
    list.memory_ready.frame.store(frame, Ordering::Relaxed);
    list.memory_ready
        .epoch
        .store(GEN.load(Ordering::Acquire), Ordering::Release);
    Some(Ready {
        directory: m.directory.2,
        faults: m.faults.2,
        base,
    })
}

/// The title addresses a draw's root arguments name: each root view, a
/// title address in two constants (low dword, then bits 32 to 39), and one
/// in 256-byte units. A constant that only looks like one names memory
/// nothing reads; one that is not the title's memory is refused.
pub(crate) fn named(addresses: &[u64], constants: &[u32]) -> Vec<u64> {
    let mut out = addresses.to_vec();
    for (i, &c) in constants.iter().enumerate() {
        if let Some(&high) = constants.get(i + 1)
            && (1..=0xFF).contains(&high)
            && c >= 0x1_0000
        {
            out.push(u64::from(c) | u64::from(high) << 32);
        }
        if c >= 0x0100_0000 {
            out.push(u64::from(c) << 8);
        }
    }
    out
}

/// Frames between the declare summaries.
const SAY_EVERY: u64 = 300;

/// Writing lists, copies they declared written, bytes, copies numbered,
/// bytes, since the last summary.
static DECLARED: [AtomicU64; 5] = [const { AtomicU64::new(0) }; 5];

/// A writing list's first draw: what it declares written against what is
/// numbered.
fn count(m: &Memory) {
    let mut d = [1, 0, 0, m.copies.len() as u64, 0];
    for (shadow, &at) in m.copies.iter().zip(&m.written_at) {
        let len = shadow.span().1;
        d[4] += len;
        if at > 0 {
            d[1] += 1;
            d[2] += len;
        }
    }
    for (total, add) in DECLARED.iter().zip(d) {
        total.fetch_add(add, Ordering::Relaxed);
    }
}

/// The declare summary since the last one.
fn say(m: &Memory) {
    let d: Vec<u64> = DECLARED
        .iter()
        .map(|c| c.swap(0, Ordering::Relaxed))
        .collect();
    let lists = d[0].max(1);
    let [checks, found] = crate::bvh_declare::checks();
    let no_card = &crate::bvh_declare::READ_ONLY_NO_CARD;
    tracing::info!(
        "bvh declare outputs: per writing list {} of {} copies declared written ({} of {} MB), {} lists in {SAY_EVERY} frames; \
         {} copies written of {} numbered; read-only windows given no extra card side {} ({} MB); checks {checks}, written though undeclared {found}",
        d[1] / lists,
        d[3] / lists,
        d[2] / lists >> 20,
        d[4] / lists >> 20,
        d[0],
        m.written_at.iter().filter(|&&at| at > 0).count(),
        m.copies.len(),
        no_card[0].load(Ordering::Relaxed),
        no_card[1].load(Ordering::Relaxed) >> 20,
    );
}

/// `VICTORY_BVH_DECLARE_CHECK=N`: in every Nth frame, the first writing
/// list also declares one copy that is not written (round robin), and its
/// landing is watched (`crate::bvh_declare::landed`).
fn check(m: &mut Memory, list: &HostList, frame: u64) {
    let every = crate::bvh_declare::check_every();
    if every == 0 || !frame.is_multiple_of(every) || m.check.1 == frame {
        return;
    }
    m.check.1 = frame;
    let Some(n) = crate::bvh_declare::pick_check(&m.written_at, m.check.0) else {
        return;
    };
    m.check.0 = n;
    let shadow = &m.copies[n];
    crate::bvh_declare::arm(shadow.id(), frame, shadow.shaders_wrote());
    let (start, len) = shadow.span();
    shadow.gpu_writes(list, start, len);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_cap_expands_under_heap_buffer_address_and_fits_direct_encoding() {
        assert_eq!(max_copies_for(false), 1023);
        assert_eq!(max_copies_for(true), 16_383);
        assert!(max_copies_for(true) < victory_shader::emit::BUFFER_RANGE_DIRECT);
        assert!(max_copies_for(true) < PER_LIST_BIT);
    }

    #[test]
    fn list_ready_reset_clears_cached_state() {
        let lr = ListReady::default();
        lr.epoch.store(42, Ordering::Relaxed);
        lr.frame.store(100, Ordering::Relaxed);
        lr.heap.store(0x1000, Ordering::Relaxed);
        lr.base.store(500, Ordering::Relaxed);
        lr.seen.store(12, Ordering::Relaxed);
        lr.reset();
        assert_eq!(lr.epoch.load(Ordering::Relaxed), 0);
        assert_eq!(lr.frame.load(Ordering::Relaxed), 0);
        assert_eq!(lr.heap.load(Ordering::Relaxed), 0);
        assert_eq!(lr.base.load(Ordering::Relaxed), 0);
        assert_eq!(lr.seen.load(Ordering::Relaxed), 0);
    }
}
