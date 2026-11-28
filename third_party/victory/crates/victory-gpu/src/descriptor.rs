//! Descriptor heaps: zeroed memory, with handles that are plain addresses
//! (CPU and GPU alike, as on the console).
//!
//! Descriptor sizes: views 32 bytes and samplers 16, the size of RDNA 2's
//! image and sampler descriptors, so a title that copies descriptors itself
//! moves whole ones. Render target and depth views are ours alone; 128 bytes
//! leaves room for whatever stride a title assumes.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use crate::object::{self, DESCRIPTOR_HEAP, E_INVALIDARG, HRESULT, Iid, Object, Table, This, guid};
use crate::resource::{Memory, PAGEABLE_IIDS};

#[cfg(windows)]
mod busy_later;
#[cfg(windows)]
pub(crate) mod copies;
#[cfg(windows)]
pub(crate) mod host;
#[cfg(windows)]
mod sync_in_place;
#[cfg(windows)]
mod tabled_all;

const HEAP_IIDS: [Iid; 4] = [
    guid(
        0x8efb471d,
        0x616c,
        0x4f49,
        [0x90, 0xf7, 0x12, 0x7b, 0xb7, 0x63, 0xfa, 0x51],
    ),
    PAGEABLE_IIDS[0],
    PAGEABLE_IIDS[1],
    PAGEABLE_IIDS[2],
];

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(crate) struct HeapDesc {
    kind: u32,
    count: u32,
    flags: u32,
    nodes: u32,
}

/// Newer titles (the title) pass the description followed by
/// memory of their own for the descriptors: an address and its size in
/// bytes, or a null address to have the heap allocate (inferred from the
/// title's call sites; older titles pass the plain description).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct Placement {
    address: u64,
    size: u64,
}

#[repr(C)]
pub(crate) struct Heap {
    object: Object,
    desc: HeapDesc,
    /// The first descriptor: in `memory`, or in the title's own memory.
    pub(crate) start: u64,
    memory: Option<Memory>,
    /// Shader-visible view and sampler heaps get a host heap with the same
    /// descriptor order when a draw first uses them (decision 0007, step
    /// 3); slots written before wait in `pending`.
    #[cfg(windows)]
    host: std::sync::OnceLock<Option<host::HostHeap>>,
    #[cfg(windows)]
    pending: std::sync::Mutex<Vec<u32>>,
    /// Changed pages of a placed shader-visible heap. Some titles copy
    /// descriptor records into it as bytes, without a descriptor API call.
    #[cfg(windows)]
    written: Option<crate::watch::Tracked>,
    /// Separate write-watch cursor for `draw::State::heap_buffers` so its
    /// dirty-slot scans do not consume `sync_heaps`'s `written` cursor.
    #[cfg(windows)]
    buffer_written: Option<crate::watch::Tracked>,
    /// Pages to read again when what `VICTORY_TABLED_ALL` left on them is
    /// left no longer ([`tabled_all`]; none without the switch).
    #[cfg(windows)]
    table_retries: Option<std::sync::Mutex<tabled_all::Retries>>,
}

pub(crate) fn heap<'a>(this: This) -> &'a Heap {
    // SAFETY: called from descriptor heap methods only.
    unsafe { &*this.cast::<Heap>() }
}

impl Heap {
    /// Whether this is a sampler heap.
    pub(crate) fn samplers(&self) -> bool {
        self.desc.kind == SAMPLERS
    }

    /// The host heap, made on first use from what the title wrote so far.
    /// Made late because a host view heap the size of the title's
    /// costs over 100 MB of commit, which its boot needs for its page pool.
    #[cfg(windows)]
    pub(crate) fn host(&self) -> Option<&host::HostHeap> {
        if let Some(h) = self.host.get() {
            return h.as_ref();
        }
        if self.desc.flags & SHADER_VISIBLE == 0 || !matches!(self.desc.kind, VIEWS_KIND | SAMPLERS)
        {
            return None;
        }
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        // A recorder can take the fast path as soon as the OnceLock is
        // initialized. Publish only after replaying the queued writes.
        self.host
            .get_or_init(|| {
                let step = u64::from(increment(self.desc.kind)?);
                let h = host::HostHeap::new(self.samplers(), self.desc.count)?;
                for i in pending.drain(..) {
                    h.mirror(i, self.start + u64::from(i) * step, false);
                }
                Some(h)
            })
            .as_ref()
    }

    /// The handle of descriptor `index` of this heap.
    #[cfg(windows)]
    pub(crate) fn handle(&self, index: u32) -> Option<u64> {
        let step = u64::from(increment(self.desc.kind)?);
        (index < self.desc.count).then(|| self.start + u64::from(index) * step)
    }

    /// Number of descriptor slots in this heap.
    #[cfg(windows)]
    pub(crate) fn count(&self) -> u32 {
        self.desc.count
    }

    /// Byte stride per descriptor slot in this heap.
    #[cfg(windows)]
    pub(crate) fn slot_stride(&self) -> usize {
        increment(self.desc.kind).unwrap_or(0) as usize
    }

    /// Byte size of the heap's descriptor storage.
    #[cfg(windows)]
    pub(crate) fn bytes(&self) -> usize {
        self.slot_stride()
            .saturating_mul(self.desc.count.max(1) as usize)
    }

    /// Changed byte ranges of a placed shader-visible heap since the last
    /// buffer-pass check, independent of [`sync_heaps`]'s `written` cursor.
    #[cfg(windows)]
    pub(crate) fn dirty_buffer_pages(&self) -> Option<Vec<(usize, usize)>> {
        let end = (self.start as usize).checked_add(self.bytes())?;
        Some(self.buffer_written.as_ref()?.take(self.start as usize, end))
    }

    /// The descriptor index of `handle` in this heap, if it lies in it.
    pub(crate) fn index(&self, handle: u64) -> Option<u32> {
        let step = u64::from(increment(self.desc.kind)?);
        let at = handle.checked_sub(self.start)?;
        let index = at / step;
        (index < u64::from(self.desc.count)).then_some(index as u32)
    }
}

const VIEWS_KIND: u32 = 0;
const SAMPLERS: u32 = 1;
const SHADER_VISIBLE: u32 = 1;

/// Shader-visible heaps by address range, for mirroring descriptor writes:
/// start, end, heap.
#[cfg(windows)]
static SHADER_HEAPS: std::sync::RwLock<Vec<(u64, u64, usize)>> = std::sync::RwLock::new(Vec::new());

/// The shader-visible heap holding `handle`, and the descriptor's index.
#[cfg(windows)]
fn shader_slot<'a>(handle: u64) -> Option<(&'a Heap, u32)> {
    let heaps = SHADER_HEAPS.read().unwrap_or_else(|e| e.into_inner());
    let &(_, _, at) = heaps
        .iter()
        .find(|&&(start, end, _)| (start..end).contains(&handle))?;
    // SAFETY: registered heaps are live (removed when dropped).
    let heap = unsafe { &*(at as *const Heap) };
    Some((heap, heap.index(handle)?))
}

/// Makes the host descriptor for what the title wrote at `handle`, if it
/// is in a shader-visible heap.
#[cfg(windows)]
fn mirror(handle: u64) {
    let Some((heap, index)) = shader_slot(handle) else {
        return;
    };
    let mut pending = heap.pending.lock().unwrap_or_else(|e| e.into_inner());
    match heap.host.get() {
        None => pending.push(index),
        Some(host) => {
            drop(pending);
            if let Some(host) = host {
                host.mirror(index, handle, false);
            }
        }
    }
}

/// Mirrors records copied directly into placed shader-visible heaps. Calls
/// to `Create*View` are already mirrored by [`mirror`]; raw CPU copies leave
/// only write-watch pages to reveal which slots changed.
///
/// Upstream VIctory 86a5ed6 does the same and counts 1,300 to 14,400 such
/// slots a frame in the world of the title this fork works on, against a
/// few hundred views made by calls; without it those host slots stayed
/// null, or went on showing what they showed before.
/// `VICTORY_HEAP_SYNC=0`: not done, as before. [`SYNCED`] counts.
#[cfg(windows)]
pub(crate) fn sync_heaps() {
    use std::sync::atomic::Ordering::Relaxed;
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if !*ON.get_or_init(|| std::env::var_os("VICTORY_HEAP_SYNC").is_none_or(|v| v != "0")) {
        return;
    }
    let started = std::time::Instant::now();
    /// Maximum contiguous dirty-page span read in one `read_into` call.
    const MAX_BATCH: usize = 256 << 10;
    let (mut pages, mut looked, mut mirrored) = (0u64, 0u64, 0u64);
    let mut page_bytes: Vec<u8> = Vec::new();
    let mut asked: Vec<u32> = Vec::new();
    let mut batch_asked: Vec<u32> = Vec::new();
    let mut batch_pages: Vec<(usize, usize, usize, usize)> = Vec::new();
    let mut changed: Vec<(u32, u64)> = Vec::new();
    // `VICTORY_SYNC_IN_PLACE=1`: view heaps' records compared where they are.
    let mut all_held = sync_in_place::on().then(sync_in_place::all);
    let mut live: Vec<u64> = Vec::new();
    let mut scratch = sync_in_place::Scratch::default();
    let heaps = SHADER_HEAPS.read().unwrap_or_else(|e| e.into_inner());
    for &(start, end, at) in heaps.iter() {
        // SAFETY: registered heaps are live (removed when dropped, which takes `.write()`).
        let heap = unsafe { &*(at as *const Heap) };
        let (Some(Some(host)), Some(written), Some(step)) =
            (heap.host.get(), &heap.written, increment(heap.desc.kind))
        else {
            continue;
        };
        let step = u64::from(step);
        // Capped host heaps have no counterparts for the title slots reserved
        // for per-draw tables; raw writes there must not be mirrored.
        let mirrored_count = heap.desc.count.min(host.count());
        live.push(host.id());
        let mut held = all_held
            .as_mut()
            .filter(|_| step == 32)
            .map(|all| all.of(host.id(), mirrored_count));
        let mut ranges = written.take(start as usize, end as usize);
        let mut retries = heap
            .table_retries
            .as_ref()
            .map(|r| r.lock().unwrap_or_else(|e| e.into_inner()));
        if let Some(retries) = &mut retries {
            retries.include_due(crate::frame::current(), start as usize, end as usize, &mut ranges);
        }
        for (range_from, range_end) in ranges {
            let mut page = range_from & !(crate::watch::PAGE - 1);
            while page < range_end {
                let from = page.max(range_from);
                let to = (page + crate::watch::PAGE).min(range_end);
                // The page's records are read through the kernel, into a
                // copy: a page the title unmaps meanwhile fails the read
                // where 2,048 reads in place would fault.
                // (Neptune) First which of its slots are not left to their draws: a page
                // of per-draw tables alone is not read at all.
                let slots = changed_slots(from, to, start as usize, step, mirrored_count);
                looked += slots.end - slots.start;
                let mut retry_at = u64::MAX;
                host.not_left_to_draws(slots, &mut asked, retries.is_some().then_some(&mut retry_at));
                if let Some(retries) = &mut retries {
                    retries.after_read(page, retry_at);
                }
                pages += 1;
                if let Some(held) = held.as_deref_mut() {
                    mirrored += sync_in_place::page(
                        host,
                        held,
                        start,
                        from,
                        to,
                        mirrored_count,
                        &asked,
                        &mut scratch,
                    );
                    page += crate::watch::PAGE;
                    continue;
                }
                if asked.is_empty() {
                    mirrored += flush_sync_batch(
                        host,
                        start,
                        step,
                        &mut batch_pages,
                        &mut batch_asked,
                        &mut page_bytes,
                        &mut changed,
                    );
                    page += crate::watch::PAGE;
                    continue;
                }
                if let Some(&(first_from, ..)) = batch_pages.first()
                    && (batch_pages.last().unwrap().1 != from || to - first_from > MAX_BATCH)
                {
                    mirrored += flush_sync_batch(
                        host,
                        start,
                        step,
                        &mut batch_pages,
                        &mut batch_asked,
                        &mut page_bytes,
                        &mut changed,
                    );
                }
                let a_start = batch_asked.len();
                batch_asked.extend_from_slice(&asked);
                let a_end = batch_asked.len();
                batch_pages.push((from, to, a_start, a_end));
                page += crate::watch::PAGE;
            }
            mirrored += flush_sync_batch(
                host,
                start,
                step,
                &mut batch_pages,
                &mut batch_asked,
                &mut page_bytes,
                &mut changed,
            );
        }
    }
    drop(heaps);
    if let Some(all) = all_held.as_mut() {
        all.keep_only(&live);
    }
    drop(all_held);
    for (count, n) in SYNCED.iter().zip([
        pages,
        looked,
        mirrored,
        started.elapsed().as_micros() as u64,
    ]) {
        count.fetch_add(n, Relaxed);
    }
}

/// Whether [`sync_heaps`] runs on a background worker thread overlapped with
/// submission preparation (`VICTORY_ASYNC_HEAP_SYNC=0`: synchronous, as before).
#[cfg(windows)]
pub(crate) fn async_sync_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_ASYNC_HEAP_SYNC").is_none_or(|v| v != "0"))
}

#[cfg(windows)]
struct HeapSyncWorker {
    /// `(requested, completed)` monotonic tickets.
    state: std::sync::Mutex<(u64, u64)>,
    cv: std::sync::Condvar,
}

#[cfg(windows)]
fn heap_sync_loop(worker: &'static HeapSyncWorker) {
    struct CompleteGuard {
        worker: &'static HeapSyncWorker,
        target: u64,
    }
    impl Drop for CompleteGuard {
        fn drop(&mut self) {
            {
                let mut state = self.worker.state.lock().unwrap_or_else(|e| e.into_inner());
                if state.1 < self.target {
                    state.1 = self.target;
                }
            }
            self.worker.cv.notify_all();
        }
    }
    loop {
        let target = {
            let mut state = worker.state.lock().unwrap_or_else(|e| e.into_inner());
            while state.0 <= state.1 {
                state = worker.cv.wait(state).unwrap_or_else(|e| e.into_inner());
            }
            state.0
        };
        let _done = CompleteGuard { worker, target };
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(sync_heaps)).is_err() {
            tracing::error!(
                ticket = target,
                "victory-heap-sync panicked during sync_heaps"
            );
        }
    }
}

#[cfg(windows)]
fn heap_sync_worker() -> (&'static HeapSyncWorker, bool) {
    static WORKER: std::sync::OnceLock<HeapSyncWorker> = std::sync::OnceLock::new();
    static STARTED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let worker = WORKER.get_or_init(|| HeapSyncWorker {
        state: std::sync::Mutex::new((0, 0)),
        cv: std::sync::Condvar::new(),
    });
    let spawned = *STARTED.get_or_init(|| {
        std::thread::Builder::new()
            .name("victory-heap-sync".into())
            .spawn(move || {
                if crate::topology::vcache_pin_enabled() {
                    crate::topology::pin_hot("victory-heap-sync");
                } else {
                    crate::cores::keep_helper_on_fast_cores();
                }
                heap_sync_loop(worker);
            })
            .is_ok()
    });
    (worker, spawned)
}

/// Schedules a [`sync_heaps`] pass on the persistent `victory-heap-sync` worker
/// thread and returns its completion ticket (or runs synchronously and returns
/// `0` if the worker thread could not be spawned).
#[cfg(windows)]
pub(crate) fn sync_heaps_start() -> u64 {
    let (worker, spawned) = heap_sync_worker();
    if !spawned {
        sync_heaps();
        return 0;
    }
    let ticket = {
        let mut state = worker.state.lock().unwrap_or_else(|e| e.into_inner());
        state.0 += 1;
        state.0
    };
    worker.cv.notify_one();
    ticket
}

/// Blocks until the [`sync_heaps_start`] pass for `ticket` has completed.
#[cfg(windows)]
pub(crate) fn sync_heaps_wait(ticket: u64) {
    if ticket == 0 {
        return;
    }
    let (worker, spawned) = heap_sync_worker();
    if !spawned {
        return;
    }
    let mut state = worker.state.lock().unwrap_or_else(|e| e.into_inner());
    while state.1 < ticket {
        state = worker.cv.wait(state).unwrap_or_else(|e| e.into_inner());
    }
}

/// The ticket the next [`sync_heaps_start`] would get: a round for it, or
/// any later one, begins after this call (`VICTORY_HEAP_ROUND_SHARE`'s
/// stamps, `crate::heap_rounds`). `0` when there is no worker.
#[cfg(windows)]
pub(crate) fn heap_sync_next() -> u64 {
    let (worker, spawned) = heap_sync_worker();
    if !spawned {
        return 0;
    }
    worker.state.lock().unwrap_or_else(|e| e.into_inner()).0 + 1
}

/// A round for ticket `need` or later: the ticket to wait for, and how it
/// was had ([`crate::round_share::decide`]: done already, asked already by
/// someone else, or asked now).
#[cfg(windows)]
pub(crate) fn sync_heaps_share(need: u64) -> (u64, crate::round_share::Decision) {
    use crate::round_share::Decision;
    let (worker, spawned) = heap_sync_worker();
    if !spawned {
        sync_heaps();
        return (0, Decision::Ask);
    }
    let (ticket, decision) = {
        let mut state = worker.state.lock().unwrap_or_else(|e| e.into_inner());
        let decision = crate::round_share::decide(state.0, state.1, need);
        if decision == Decision::Ask {
            state.0 += 1;
            (state.0, decision)
        } else {
            (need, decision)
        }
    };
    if decision == Decision::Ask {
        worker.cv.notify_one();
    }
    (ticket, decision)
}

/// Whether any of `ranges` (`[from, to)`) overlaps a registered
/// shader-visible descriptor heap (one look at the heaps for all).
#[cfg(windows)]
pub(crate) fn any_in_shader_heap(ranges: impl IntoIterator<Item = (usize, usize)>) -> bool {
    let heaps = SHADER_HEAPS.read().unwrap_or_else(|e| e.into_inner());
    if heaps.is_empty() {
        return false;
    }
    ranges.into_iter().any(|(from, to)| {
        let (from, to) = (from as u64, to as u64);
        from < to && heaps.iter().any(|&(hs, he, _)| from < he && hs < to)
    })
}

/// Whether `[start, start + len)` overlaps any registered shader-visible
/// descriptor heap.
#[cfg(windows)]
pub(crate) fn overlaps_shader_heap(start: usize, len: usize) -> bool {
    if len == 0 {
        return false;
    }
    let from = start as u64;
    let to = from.saturating_add(len as u64);
    let heaps = SHADER_HEAPS.read().unwrap_or_else(|e| e.into_inner());
    heaps.iter().any(|&(hs, he, _)| from < he && hs < to)
}

#[cfg(windows)]
#[allow(clippy::too_many_arguments)]
fn flush_sync_batch(
    host: &host::HostHeap,
    start: u64,
    step: u64,
    batch_pages: &mut Vec<(usize, usize, usize, usize)>,
    batch_asked: &mut Vec<u32>,
    page_bytes: &mut Vec<u8>,
    changed: &mut Vec<(u32, u64)>,
) -> u64 {
    if batch_pages.is_empty() {
        return 0;
    }
    let began = sync_in_place::now();
    let mirrored = flush_sync_pages(host, start, step, batch_pages, batch_asked, page_bytes, changed);
    sync_in_place::batch_since(began);
    mirrored
}

#[cfg(windows)]
#[allow(clippy::too_many_arguments)]
fn flush_sync_pages(
    host: &host::HostHeap,
    start: u64,
    step: u64,
    batch_pages: &mut Vec<(usize, usize, usize, usize)>,
    batch_asked: &mut Vec<u32>,
    page_bytes: &mut Vec<u8>,
    changed: &mut Vec<(u32, u64)>,
) -> u64 {
    let batch_from = batch_pages[0].0;
    let batch_to = batch_pages.last().unwrap().1;
    let mut mirrored = 0u64;
    let len = batch_to - batch_from;
    if page_bytes.len() < len {
        page_bytes.resize(len, 0);
    }
    let slice = &mut page_bytes[..len];
    if crate::host::read_into(batch_from as *const u8, slice) {
        mirrored += host.mirror_copied_page(start, step, batch_from, slice, batch_asked, changed);
    } else if batch_pages.len() > 1 {
        // Fall back to per-page reads so one unmapped page does not skip valid neighbors.
        for &(p_from, p_to, a_start, a_end) in batch_pages.iter() {
            let len = p_to - p_from;
            if page_bytes.len() < len {
                page_bytes.resize(len, 0);
            }
            let slice = &mut page_bytes[..len];
            if crate::host::read_into(p_from as *const u8, slice) {
                mirrored += host.mirror_copied_page(
                    start,
                    step,
                    p_from,
                    slice,
                    &batch_asked[a_start..a_end],
                    changed,
                );
            }
        }
    }
    batch_pages.clear();
    batch_asked.clear();
    mirrored
}

/// Since the last frame line, of [`sync_heaps`]: changed pages of placed
/// shader-visible heaps looked at, slots on them, slots whose record was
/// not the one last seen (mirrored), and the microseconds it took.
#[cfg(windows)]
pub(crate) static SYNCED: [std::sync::atomic::AtomicU64; 4] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 4];

/// With the frame line: what [`sync_heaps`] did per frame over `frames`.
/// Nothing while no heap page changed.
#[cfg(windows)]
pub(crate) fn sync_line(frames: f64) {
    let [pages, looked, mirrored, micros] =
        [0, 1, 2, 3].map(|i| SYNCED[i].swap(0, std::sync::atomic::Ordering::Relaxed));
    if pages == 0 {
        return;
    }
    tracing::info!(
        "descriptor records the title copied into its heap as bytes (VICTORY_HEAP_SYNC=0: not looked for): per frame \
         {:.1} changed heap pages, {:.0} slots looked at, {:.1} whose record was new and was mirrored, {:.0} left to the \n         draws that read them through tables (VICTORY_HEAP_SYNC_TABLES=1 mirrors those too), {:.2} ms",
        pages as f64 / frames,
        looked as f64 / frames,
        mirrored as f64 / frames,
        host::COPIED_TABLED.swap(0, std::sync::atomic::Ordering::Relaxed) as f64 / frames,
        micros as f64 / 1000.0 / frames
    );
    // (Neptune) What the mirrored ones were.
    let c: Vec<f64> = host::COPIED_CENSUS
        .iter()
        .map(|c| c.swap(0, std::sync::atomic::Ordering::Relaxed) as f64 / frames)
        .collect();
    let place: Vec<String> = c[10..]
        .iter()
        .enumerate()
        .filter(|(_, n)| **n >= 0.5)
        .map(|(i, n)| format!("{i}: {n:.0}"))
        .collect();
    tracing::info!(
        "of the mirrored records per frame: {:.0} no view of ours, {:.0} constant buffers, {:.0} textures, {:.0} buffers, \
         {:.0} unordered textures, {:.0} unordered buffers, {:.0} samplers, {:.0} placed, {:.0} placed unordered; {:.0} in \
         slots a table read at some time; by the slot's number in 65,536s: {}",
        c[0],
        c[1],
        c[2],
        c[3],
        c[4],
        c[5],
        c[6],
        c[7],
        c[8],
        c[9],
        place.join(", ")
    );
    sync_in_place::line(frames);
    busy_later::line(frames);
}

/// Title descriptor slots in a changed byte range that have host counterparts.
/// `range_end` is exclusive.
#[cfg(windows)]
fn changed_slots(
    range_from: usize,
    range_end: usize,
    heap_start: usize,
    step: u64,
    mirrored_count: u32,
) -> std::ops::Range<u64> {
    let from = (range_from - heap_start) as u64;
    let to = (range_end - heap_start) as u64;
    let first = from / step;
    let last = to.div_ceil(step).min(u64::from(mirrored_count));
    first.min(last)..last
}

/// Gives back a list's dynamic slots in batches by heap, taking `SHADER_HEAPS`
/// and each host heap's dynamic lock once per contiguous run of the same heap.
#[cfg(windows)]
pub(crate) fn release_dynamic_many(entries: &[(usize, u64, u32, u32)]) {
    if entries.is_empty() {
        return;
    }
    let heaps = SHADER_HEAPS.read().unwrap_or_else(|e| e.into_inner());
    let mut batch: Vec<(u32, u32)> = Vec::new();
    let mut i = 0;
    while i < entries.len() {
        let (heap, id) = (entries[i].0, entries[i].1);
        batch.clear();
        while i < entries.len() && (entries[i].0, entries[i].1) == (heap, id) {
            batch.push((entries[i].2, entries[i].3));
            i += 1;
        }
        // A heap whose address a new one took since keeps the new host
        // heap's identity: `release_many` leaves its slots alone.
        if heaps.iter().any(|&(_, _, h)| h == heap)
            // SAFETY: registered heaps are live (removed when dropped, which takes `.write()`).
            && let Some(Some(host)) = unsafe { &*(heap as *const Heap) }.host.get()
        {
            host.release_many(id, &batch);
        }
    }
}

#[cfg(windows)]
impl Drop for Heap {
    fn drop(&mut self) {
        let at = std::ptr::from_ref::<Heap>(self) as usize;
        {
            let mut heaps = SHADER_HEAPS.write().unwrap_or_else(|e| e.into_inner());
            heaps.retain(|&(_, _, h)| h != at);
        }
        copies::unregister(at);
        if let Some(Some(host)) = self.host.get() {
            crate::draw::forget_heap_ranges(host.id());
        }
    }
}

/// Bytes per descriptor, by heap type (views, samplers, render targets,
/// depth).
fn increment(kind: u32) -> Option<u32> {
    [32, 16, 128, 128].get(kind as usize).copied()
}

/// Device method.
pub(crate) extern "system" fn increment_size(_: This, kind: u32) -> u32 {
    increment(kind).unwrap_or(0)
}

/// What our render target views hold: the resource and the view
/// description, turned into a host view when a command uses them.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct RtvRecord {
    tag: u32,
    described: u32,
    pub(crate) resource: This,
    desc: [u32; 6],
}
const RTV_TAG: u32 = u32::from_le_bytes(*b"vRTV");
const _: () = assert!(size_of::<RtvRecord>() <= 112);

/// A render target or depth view's values, kept for replaying a list after
/// a cut: the resource as an integer (like `draw::State`), never the address
/// of descriptor storage the title may rewrite. The caller still owns the
/// resource's lifetime, as with the command first recorded.
#[cfg(windows)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RtvSnapshot {
    tag: u32,
    described: u32,
    resource: usize,
    desc: [u32; 6],
}

#[cfg(windows)]
impl RtvSnapshot {
    pub(crate) fn restore(self) -> RtvRecord {
        RtvRecord {
            tag: self.tag,
            described: self.described,
            resource: self.resource as This,
            desc: self.desc,
        }
    }
}

impl RtvRecord {
    /// The values of this view ([`RtvSnapshot`]).
    #[cfg(windows)]
    pub(crate) fn snapshot(self) -> RtvSnapshot {
        RtvSnapshot {
            tag: self.tag,
            described: self.described,
            resource: self.resource as usize,
            desc: self.desc,
        }
    }

    /// The view description in the host's type (the same layout).
    #[cfg(windows)]
    pub(crate) fn desc(
        &self,
    ) -> Option<windows::Win32::Graphics::Direct3D12::D3D12_RENDER_TARGET_VIEW_DESC> {
        const TEXTURE_2D: u32 = 4;
        self.host_desc(TEXTURE_2D)
    }

    /// The depth view description in the host's type (the same layout).
    #[cfg(windows)]
    pub(crate) fn depth_desc(
        &self,
    ) -> Option<windows::Win32::Graphics::Direct3D12::D3D12_DEPTH_STENCIL_VIEW_DESC> {
        const TEXTURE_2D: u32 = 3;
        self.host_desc(TEXTURE_2D)
    }

    /// The description words as a host type, the format as the host has it.
    /// Without one, a typeless host copy gets a 2D view (`dimension`) in
    /// the title's format, which the host needs spelled out.
    #[cfg(windows)]
    fn host_desc<T>(&self, dimension: u32) -> Option<T> {
        assert!(size_of::<T>() <= size_of::<[u32; 6]>());
        let mut words = if self.described != 0 {
            self.desc
        } else {
            [
                crate::resource::view_format(self.resource)?,
                dimension,
                0,
                0,
                0,
                0,
            ]
        };
        // A target or depth view: of a target, in its stand-in format.
        words[0] = crate::resource::target_format(crate::resource::pc_format(words[0]));
        // SAFETY: both host view descriptions are 24 bytes in this layout.
        Some(unsafe { (&raw const words).cast::<T>().read_unaligned() })
    }
}

/// Device method: records the view at `handle` (descriptor memory).
pub(crate) extern "system" fn create_rtv(
    _: This,
    resource: This,
    desc: *const [u32; 6],
    handle: u64,
) {
    if handle == 0 {
        return;
    }
    // SAFETY: the title passes a description or null.
    let described = unsafe { desc.as_ref() };
    let record = RtvRecord {
        tag: RTV_TAG,
        described: u32::from(described.is_some()),
        resource,
        desc: described.copied().unwrap_or_default(),
    };
    // SAFETY: the handle is a descriptor slot, at least 112 bytes.
    unsafe { (handle as *mut RtvRecord).write_unaligned(record) };
}

/// Device method: a depth view, recorded as render target views are.
pub(crate) extern "system" fn create_dsv(
    this: This,
    resource: This,
    desc: *const [u32; 6],
    handle: u64,
) {
    create_rtv(this, resource, desc, handle);
    if handle != 0 {
        // SAFETY: just written by `create_rtv`.
        unsafe { (handle as *mut u32).write_unaligned(DSV_TAG) };
    }
}
const DSV_TAG: u32 = u32::from_le_bytes(*b"vDSV");

/// A shader view or sampler as the title described it (descriptions in the
/// host's layout, kept as words), for the host to build its own from.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum View {
    /// Constant buffer: GPU address and size.
    Constant(u64, u32),
    /// Shader resource: resource, description (none: the resource's own).
    Shader(usize, Option<[u32; 10]>),
    /// Unordered access: resource, counter resource, description.
    Unordered(usize, usize, Option<[u32; 10]>),
    Sampler([u32; 13]),
    /// The console's own sampler description: flags, filters per axis,
    /// addressing, LOD and anisotropy settings, border color, then
    /// console-only tuning words.
    ConsoleSampler([u32; 27]),
    /// A view of memory without a resource object: whether it is unordered
    /// access, the resource description, then the view's (the plain one
    /// followed by the memory address and console fields).
    Placed(bool, [u32; 14], [u32; 16]),
}

impl View {
    /// Whether the view reads buffer memory (constant buffers, buffer views
    /// of resources or of memory) or memory the title placed a texture in
    /// to sample: such views get host slots only for draws. Placed
    /// unordered texture views live on the host alone, as texture objects'
    /// unordered views do, so shader-visible heap writes mirror them.
    #[cfg(windows)]
    fn is_buffer(&self) -> bool {
        const BUFFER: u32 = 1;
        match self {
            View::Constant(..) | View::Placed(false, ..) => true,
            View::Placed(true, resource, _) => resource[0] == BUFFER,
            View::Shader(r, _) | View::Unordered(r, _, _) => {
                crate::resource::buffer_bytes(*r as This).is_some()
            }
            View::Sampler(_) | View::ConsoleSampler(_) => false,
        }
    }
}

/// Every distinct view in use; a descriptor holds a tag, an index into it
/// and the entry's generation, which survive titles copying descriptors as
/// bytes. A view of a resource lives as long as the resource: when the
/// title releases it, its entries are freed for reuse under a new
/// generation, so descriptors (and host slots made from them) naming the
/// old one go stale instead of naming whatever is made at the same address
/// next.
///
/// Views without a resource object have nothing that releases them, and a
/// title copies descriptors as bytes, so nothing says when the last
/// descriptor naming an entry is gone: such entries were kept for the run.
/// The title writes some 14,000 constant buffer views a frame,
/// each at the address its constants have that frame: 2,450,139 entries
/// after four minutes of play, 1,047 MB with the index, and 11,000 more
/// every second (run 65). So the two kinds that carry an address are not
/// entries any more: their records name them whole ([`constant_words`],
/// [`placed_words`]), and the table holds only what is bounded: views of
/// resource objects (freed with the resource), samplers (as many as there
/// are sampler states), and the shapes of placed views (their descriptions
/// without the address: as many as the title has kinds of textures and
/// buffers, however often it moves them).
/// `VICTORY_VIEW_INLINE=0`: every view is an entry, as before.
#[cfg_attr(test, derive(Default))]
struct Views {
    /// Each entry's view and generation.
    list: Vec<(View, u32)>,
    /// How many entries of [`list`](Self::list) hold a constant buffer view
    /// and how many a view of placed memory (freed ones included, as they
    /// still hold their view until reused), kept as entries change rather
    /// than counted for the memory line: that walk over every entry took
    /// up to 49 ms at 30 million entries.
    #[cfg(all(windows, feature = "diagnostics"))]
    address_entries: [u64; 2],
    index: std::collections::BTreeMap<View, u32>,
    /// Entries naming each resource.
    by_resource: std::collections::BTreeMap<usize, Vec<u32>>,
    free: Vec<u32>,
    /// The shapes of views of placed memory, and where each is.
    shapes: Vec<Shape>,
    shape_index: std::collections::BTreeMap<Shape, u32>,
    /// Entries of views that no resource releases (constant buffer views
    /// and views of placed memory that did not fit a record), oldest first,
    /// with the generation each was made at: past [`ownerless_cap`] the
    /// oldest is freed, so they cannot grow for the whole run.
    ownerless: std::collections::VecDeque<(u32, u32)>,
}

/// Read far more often than written: every slot a draw reads through a
/// table is looked up here, on several recording threads at once in the
/// world. Readers share it; only a view not seen before (a new table entry
/// or shape) and a released resource take it alone.
static VIEWS: std::sync::RwLock<Views> = std::sync::RwLock::new(Views {
    list: Vec::new(),
    #[cfg(all(windows, feature = "diagnostics"))]
    address_entries: [0, 0],
    index: std::collections::BTreeMap::new(),
    by_resource: std::collections::BTreeMap::new(),
    free: Vec::new(),
    shapes: Vec::new(),
    shape_index: std::collections::BTreeMap::new(),
    ownerless: std::collections::VecDeque::new(),
});

/// How many table entries of views without a resource are kept
/// ([`Views::ownerless`]). Once the shapes of placed views are full (the
/// title makes far more of them than it has kinds of textures: 4,194,304
/// after half an hour of play), every placed view it writes becomes such an
/// entry, some 15,000 a second: kept for the run they took 16.7 GB after 70
/// minutes and the frame rate fell by half. A descriptor names the view it
/// was written with for a frame or a few, so the oldest of a million are
/// long out of use; one naming a freed entry reads as stale (no view).
/// `VICTORY_OWNERLESS_VIEWS=<n>` keeps `n` (0: all, as before).
fn ownerless_cap() -> usize {
    static CAP: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *CAP.get_or_init(|| {
        std::env::var("VICTORY_OWNERLESS_VIEWS")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(1 << 20)
    })
}
const VIEW_TAG: u32 = u32::from_le_bytes(*b"vVIE");

/// A record is the tag and two words, `i` and `generation` (the host keeps
/// them as one 64-bit key per slot). The top two bits of `generation` say
/// what the other 62 bits are: 0, entry `i` of the table at that
/// generation; [`CONSTANT`], a constant buffer view, whole; [`PLACED`], a
/// view of placed memory: its address and the number of its shape.
const KIND_SHIFT: u32 = 30;
/// Table generations stay below the kind bits (and above 0).
const GENERATIONS: u32 = (1 << KIND_SHIFT) - 1;
const CONSTANT: u32 = 1;
const PLACED: u32 = 2;
/// Bits of a constant buffer view's address in its record: any address a
/// program can have. The 15 bits above them are its size.
const CONSTANT_ADDRESS_BITS: u32 = 47;
/// Bits of a placed view's address in its record: placed memory is title
/// memory, which lies below 1 TiB. The 22 bits above them number its shape.
const PLACED_ADDRESS_BITS: u32 = 40;
const SHAPES: usize = 1 << (62 - PLACED_ADDRESS_BITS);

/// A view of placed memory without its address: whether it is unordered
/// access, the resource description, and the view's words but the two that
/// hold the address (10 and 11).
type Shape = (bool, [u32; 14], [u32; 14]);

/// Whether constant buffer views and views of placed memory are named whole
/// in their records ([`Views`]). On by default; `VICTORY_VIEW_INLINE=0`
/// makes every view a table entry, as before.
fn inline() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_VIEW_INLINE").is_none_or(|v| v != "0"))
}

/// Views named whole in their records since the start: constant buffer
/// views, views of placed memory (for the memory line).
static NAMED_WHOLE: [std::sync::atomic::AtomicU64; 2] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 2];

/// The two record words of the 62 bits `payload` of kind `kind`.
fn kind_words(kind: u32, payload: u64) -> [u32; 2] {
    [payload as u32, kind << KIND_SHIFT | (payload >> 32) as u32]
}

/// The 62 bits two record words carry beside their kind.
fn payload(i: u32, generation: u32) -> u64 {
    u64::from(generation & GENERATIONS) << 32 | u64::from(i)
}

/// The record words that name the constant buffer view of `size` bytes at
/// `address` whole; `None` if they cannot: an address past 47 bits, or a
/// size of 16 KiB or more that is no multiple of 256 or reaches 4 MiB (the
/// size has 15 bits: bytes below 16 KiB, else units of 256 bytes). Such a
/// view is a table entry.
fn constant_words(address: u64, size: u32) -> Option<[u32; 2]> {
    if address >> CONSTANT_ADDRESS_BITS != 0 {
        return None;
    }
    let code = if size < 1 << 14 {
        u64::from(size)
    } else if size.is_multiple_of(256) && size >> 8 < 1 << 14 {
        1 << 14 | u64::from(size >> 8)
    } else {
        return None;
    };
    Some(kind_words(
        CONSTANT,
        code << CONSTANT_ADDRESS_BITS | address,
    ))
}

/// The constant buffer view [`constant_words`] named.
fn constant_of(i: u32, generation: u32) -> View {
    let payload = payload(i, generation);
    let code = (payload >> CONSTANT_ADDRESS_BITS) as u32;
    let size = if code >> 14 == 0 {
        code
    } else {
        (code & 0x3fff) << 8
    };
    View::Constant(payload & ((1 << CONSTANT_ADDRESS_BITS) - 1), size)
}

/// A placed view's shape and its address.
fn shape_of(unordered: bool, resource: &[u32; 14], desc: &[u32; 16]) -> (Shape, u64) {
    let mut rest = [0u32; 14];
    rest[..10].copy_from_slice(&desc[..10]);
    rest[10..].copy_from_slice(&desc[12..]);
    (
        (unordered, *resource, rest),
        u64::from(desc[10]) | u64::from(desc[11]) << 32,
    )
}

/// The record words that name the view of placed memory at `address` with
/// shape number `shape`.
fn placed_words(shape: u32, address: u64) -> [u32; 2] {
    kind_words(PLACED, u64::from(shape) << PLACED_ADDRESS_BITS | address)
}

impl Views {
    /// The number of `shape`, a new one if it is not there yet; `None` when
    /// the shapes are full (the view is then a table entry).
    fn shape(&mut self, shape: Shape) -> Option<u32> {
        if let Some(&at) = self.shape_index.get(&shape) {
            return Some(at);
        }
        if self.shapes.len() >= SHAPES {
            return None;
        }
        let at = self.shapes.len() as u32;
        self.shapes.push(shape);
        self.shape_index.insert(shape, at);
        #[cfg(feature = "diagnostics")]
        if matches!(at + 1, 1024 | 16384 | 131_072 | 1_048_576) {
            self.log_shape_words();
        }
        Some(at)
    }

    /// How many distinct values each word of the shapes kept takes: the
    /// words that tell shapes apart. Logged as the count of shapes passes a
    /// few marks, to see which fields make shapes many.
    #[cfg(feature = "diagnostics")]
    fn log_shape_words(&self) {
        let distinct = |word: &dyn Fn(&Shape) -> u32| {
            self.shapes
                .iter()
                .map(word)
                .collect::<std::collections::BTreeSet<_>>()
                .len()
        };
        let resource: Vec<usize> = (0..14).map(|w| distinct(&|s: &Shape| s.1[w])).collect();
        let view: Vec<usize> = (0..14).map(|w| distinct(&|s: &Shape| s.2[w])).collect();
        let unordered = distinct(&|s: &Shape| u32::from(s.0));
        tracing::info!(
            shapes = self.shapes.len(),
            unordered,
            ?resource,
            ?view,
            "shapes of placed views: distinct values of each word (resource description words 0-13; view description words 0-9 and 12-15)"
        );
    }

    /// The view of placed memory [`placed_words`] named.
    fn placed_of(&self, i: u32, generation: u32) -> Option<View> {
        let payload = payload(i, generation);
        let address = payload & ((1 << PLACED_ADDRESS_BITS) - 1);
        let &(unordered, resource, rest) =
            self.shapes.get((payload >> PLACED_ADDRESS_BITS) as usize)?;
        let mut desc = [0u32; 16];
        desc[..10].copy_from_slice(&rest[..10]);
        (desc[10], desc[11]) = (address as u32, (address >> 32) as u32);
        desc[12..].copy_from_slice(&rest[10..]);
        Some(View::Placed(unordered, resource, desc))
    }

    /// The record words of `view` as a table entry, if it has one.
    fn entry_of(&self, view: &View) -> Option<[u32; 2]> {
        let &i = self.index.get(view)?;
        Some([i, self.list[i as usize].1])
    }

    /// The record words of `view` as a table entry: its entry (a new or a
    /// freed one if the view is not there yet) and that entry's generation.
    fn entry(&mut self, view: View) -> [u32; 2] {
        self.entry_kept(view, ownerless_cap())
    }

    /// [`entry`](Self::entry), keeping at most `cap` entries of views
    /// without a resource (0: all).
    fn entry_kept(&mut self, view: View, cap: usize) -> [u32; 2] {
        let ownerless =
            view.resources() == [0, 0] && matches!(view, View::Constant(..) | View::Placed(..));
        let fresh = !self.index.contains_key(&view);
        let words = self.entry_made(view);
        if ownerless && fresh {
            self.ownerless.push_back((words[0], words[1]));
            while cap != 0 && self.ownerless.len() > cap {
                let (i, generation) = self.ownerless.pop_front().expect("longer than cap");
                self.free_ownerless(i, generation);
            }
        }
        words
    }

    /// Frees entry `i` if it still holds the view it had at `generation`
    /// (it may have been freed and made again for another view since).
    fn free_ownerless(&mut self, i: u32, generation: u32) {
        let Some((view, current)) = self.list.get(i as usize).cloned() else {
            return;
        };
        if current != generation || self.index.get(&view) != Some(&i) {
            return;
        }
        self.index.remove(&view);
        self.list[i as usize].1 = ((generation + 1) & GENERATIONS).max(1);
        self.free.push(i);
        note_forgotten(i);
    }

    /// Counts `view` in or out of [`address_entries`](Self::address_entries).
    #[cfg(all(windows, feature = "diagnostics"))]
    fn count_address(&mut self, view: &View, added: bool) {
        let kind = match view {
            View::Constant(..) => 0,
            View::Placed(..) => 1,
            _ => return,
        };
        let n = &mut self.address_entries[kind];
        *n = if added { *n + 1 } else { n.saturating_sub(1) };
    }

    fn entry_made(&mut self, view: View) -> [u32; 2] {
        let i = match self.index.get(&view) {
            Some(&i) => i,
            None => {
                let i = match self.free.pop() {
                    Some(i) => {
                        #[cfg(all(windows, feature = "diagnostics"))]
                        self.count_address(&self.list[i as usize].0.clone(), false);
                        self.list[i as usize].0 = view.clone();
                        i
                    }
                    None => {
                        self.list.push((view.clone(), 1));
                        self.list.len() as u32 - 1
                    }
                };
                #[cfg(all(windows, feature = "diagnostics"))]
                self.count_address(&view, true);
                for r in view.resources().into_iter().filter(|&r| r != 0) {
                    self.by_resource.entry(r).or_default().push(i);
                }
                self.index.insert(view, i);
                i
            }
        };
        [i, self.list[i as usize].1]
    }
}

/// The two words that name `view` in a record: the view itself where its
/// kind allows ([`Views`]), else its table entry.
fn words_for(view: View) -> [u32; 2] {
    use std::sync::atomic::Ordering::Relaxed;
    if inline() {
        match &view {
            View::Constant(address, size) => {
                if let Some(words) = constant_words(*address, *size) {
                    NAMED_WHOLE[0].fetch_add(1, Relaxed);
                    return words;
                }
            }
            View::Placed(unordered, resource, desc) => {
                let (shape, address) = shape_of(*unordered, resource, desc);
                if address >> PLACED_ADDRESS_BITS == 0 {
                    // Nearly every placed view has a shape seen before.
                    let known = views_read().shape_index.get(&shape).copied();
                    let shape = known.or_else(|| views_write().shape(shape));
                    if let Some(shape) = shape {
                        NAMED_WHOLE[1].fetch_add(1, Relaxed);
                        return placed_words(shape, address);
                    }
                }
            }
            _ => {}
        }
    }
    // A view written before keeps its entry and generation.
    if let Some(words) = views_read().entry_of(&view) {
        return words;
    }
    views_write().entry(view)
}

fn views_read() -> std::sync::RwLockReadGuard<'static, Views> {
    VIEWS.read().unwrap_or_else(|e| e.into_inner())
}

fn views_write() -> std::sync::RwLockWriteGuard<'static, Views> {
    VIEWS.write().unwrap_or_else(|e| e.into_inner())
}

/// For the memory line: the distinct views kept ([`Views`]), those of them
/// free for reuse, and the bytes the table and its index hold for them (an
/// estimate: an entry of the table, and one of the index with its share of
/// a tree node; the shapes likewise); then the entries that are constant
/// buffer views and views of placed memory (those that did not fit a
/// record, or all of them with `VICTORY_VIEW_INLINE=0`), the shapes of
/// placed views kept, and the views named whole in their records since the
/// start: constant buffer views, views of placed memory.
#[cfg(all(windows, feature = "diagnostics"))]
pub(crate) fn views_kept() -> [u64; 8] {
    use std::sync::atomic::Ordering::Relaxed;
    let views = views_read();
    let each = size_of::<(View, u32)>() as u64;
    let shape = size_of::<Shape>() as u64;
    let [constants, placed] = views.address_entries;
    [
        views.list.len() as u64,
        views.free.len() as u64,
        views.list.capacity() as u64 * each
            + views.index.len() as u64 * each * 3 / 2
            + views.shapes.capacity() as u64 * shape
            + views.shape_index.len() as u64 * shape * 3 / 2,
        constants,
        placed,
        views.shapes.len() as u64,
        NAMED_WHOLE[0].load(Relaxed),
        NAMED_WHOLE[1].load(Relaxed),
    ]
}

impl View {
    /// The resource objects the view names.
    fn resources(&self) -> [usize; 2] {
        match *self {
            View::Shader(r, _) => [r, 0],
            View::Unordered(r, counter, _) => [r, counter],
            _ => [0, 0],
        }
    }
}

/// Title slots (their handles) written with a view of a texture object
/// that had no host copy then, by texture. A title texture gets its host
/// copy on first use (`resource.rs` `Hosted`), and the title
/// writes each texture's views into its view heap when it creates the
/// texture: making those host slots would make every host copy at once.
/// They are made when the texture is: translated shaders index the heap
/// directly, so a slot is not always made again by the draw that reads it.
#[cfg(windows)]
static WAITING: std::sync::Mutex<std::collections::BTreeMap<usize, Vec<u64>>> =
    std::sync::Mutex::new(std::collections::BTreeMap::new());

/// Textures given their host copy since [`make_waiting`] last ran.
#[cfg(windows)]
static MADE_LATE: std::sync::Mutex<Vec<usize>> = std::sync::Mutex::new(Vec::new());

/// Title texture `resource` was given its host copy (called under that
/// texture's own lock: nothing else is locked here).
#[cfg(windows)]
pub(crate) fn texture_made(resource: usize) {
    MADE_LATE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(resource);
}

/// The slot at `handle` was written with a view of texture `resource`,
/// which has no host copy: it waits for one.
#[cfg(windows)]
fn wait_for(resource: usize, handle: u64) {
    /// A texture with more slots waiting than this is in use for all we
    /// know (its views are being copied about): its host copy is made.
    const MOST: usize = 64;
    let many = {
        let mut waiting = WAITING.lock().unwrap_or_else(|e| e.into_inner());
        let slots = waiting.entry(resource).or_default();
        if !slots.contains(&handle) {
            slots.push(handle);
        }
        slots.len() > MOST
    };
    if many {
        // Makes it, and notes it made.
        let _ = crate::resource::host_copy(resource as This);
    } else if !crate::resource::host_waits(resource as This) {
        // Made by another thread since the slot was looked at: that making
        // may have been taken by `make_waiting` before this slot was noted.
        texture_made(resource);
    }
}

/// For `crate::resource::slot_evict`: of the host textures `raws` (raw
/// pointer to title texture), those shown only by title slots a draw makes
/// again by itself (`HostHeap::slots_showing`), with those slots as (heap,
/// index). A texture any other slot shows is left out.
#[cfg(windows)]
pub(crate) fn evictable_slots(
    raws: &std::collections::HashMap<usize, usize>,
    frame: u64,
) -> std::collections::HashMap<usize, Vec<(usize, u32)>> {
    let set: std::collections::HashSet<usize> = raws.keys().copied().collect();
    let heaps = SHADER_HEAPS.read().unwrap_or_else(|e| e.into_inner());
    let mut by_texture: std::collections::HashMap<usize, Vec<(usize, u32)>> =
        std::collections::HashMap::new();
    let mut blocked = std::collections::HashSet::new();
    for &(_, _, at) in heaps.iter() {
        // SAFETY: registered heaps are live (removed when dropped, which takes `.write()`).
        let Some(Some(host)) = unsafe { &*(at as *const Heap) }.host.get() else {
            continue;
        };
        for (index, raw, ok) in host.slots_showing(&set, frame) {
            if ok {
                by_texture.entry(raw).or_default().push((at, index));
            } else {
                blocked.insert(raw);
            }
        }
    }
    drop(heaps);
    by_texture.retain(|raw, _| !blocked.contains(raw));
    by_texture
        .into_iter()
        .filter_map(|(raw, slots)| Some((*raws.get(&raw)?, slots)))
        .collect()
}

/// Puts the slots of title texture `resource` back to waiting for its host
/// copy ([`evictable_slots`]): null now, made again when the texture is
/// (`make_waiting`) or by the draw that reads them. Returns how many were
/// made. Called before the host copy is let go.
#[cfg(windows)]
pub(crate) fn evict_slots(resource: usize, slots: &[(usize, u32)]) -> u64 {
    let mut n = 0;
    for &(at, index) in slots {
        let heaps = SHADER_HEAPS.read().unwrap_or_else(|e| e.into_inner());
        if !heaps.iter().any(|&(_, _, h)| h == at) {
            continue;
        }
        // SAFETY: registered heaps are live (removed when dropped, which takes `.write()`).
        let heap = unsafe { &*(at as *const Heap) };
        let (Some(handle), Some(Some(host))) = (heap.handle(index), heap.host.get()) else {
            continue;
        };
        let gone = host.texture_slot_gone(index);
        drop(heaps);
        if gone {
            let mut waiting = WAITING.lock().unwrap_or_else(|e| e.into_inner());
            let handles = waiting.entry(resource).or_default();
            if !handles.contains(&handle) {
                handles.push(handle);
            }
            n += 1;
        }
    }
    n
}

/// Makes the host slots that waited for a texture's host copy, for the
/// textures made since the last call. Called before a submission runs, by
/// the thread that submits: a slot is read when a list runs.
#[cfg(windows)]
pub(crate) fn make_waiting() {
    let mut made = std::mem::take(&mut *MADE_LATE.lock().unwrap_or_else(|e| e.into_inner()));
    // `VICTORY_HEAP_SYNC_BUSY_LATER`: textures a mirroring found held.
    made.extend(busy_later::ready());
    if made.is_empty() {
        return;
    }
    let work: Vec<(usize, Vec<u64>)> = {
        let mut waiting = WAITING.lock().unwrap_or_else(|e| e.into_inner());
        made.into_iter()
            .filter_map(|resource| waiting.remove(&resource).map(|handles| (resource, handles)))
            .collect()
    };
    for (resource, handles) in work {
        for handle in handles {
            // Only if the slot still holds a view of that texture: the
            // title may have written it since (and a view of another
            // texture without a host copy must go on waiting).
            // (Looked up first: the handle is read only while its heap is
            // there.)
            let Some((heap, index)) = shader_slot(handle) else {
                continue;
            };
            let same = match view(handle) {
                Some(View::Shader(r, _) | View::Unordered(r, _, _)) => r == resource,
                _ => false,
            };
            if same && let Some(Some(host)) = heap.host.get() {
                host.mirror(index, handle, true);
            }
        }
    }
}

fn record(handle: u64, view: View) {
    if handle == 0 {
        return;
    }
    let [i, generation] = words_for(view);
    // SAFETY: the handle is a descriptor slot, at least 16 bytes.
    unsafe { (handle as *mut [u32; 3]).write_unaligned([VIEW_TAG, i, generation]) };
    #[cfg(windows)]
    mirror(handle);
}

/// The title released `resource`: the views naming it go stale. With
/// `VICTORY_HEAP_FIND` on, the memory it lies in is kept for the records
/// of those views ([`copies::note_former`]).
pub(crate) fn forget(resource: usize) {
    #[cfg(windows)]
    let former = crate::heap_find::on()
        .then(|| crate::resource::find_facts::object_span(resource as This))
        .flatten();
    #[cfg(windows)]
    WAITING
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&resource);
    let mut views = views_write();
    let Some(entries) = views.by_resource.remove(&resource) else {
        return;
    };
    for i in entries {
        let (view, generation) = views.list[i as usize].clone();
        // Entries freed through a view's other resource may hold another
        // view by now.
        if views.index.get(&view) != Some(&i) || !view.resources().contains(&resource) {
            continue;
        }
        views.index.remove(&view);
        // (Below the bits that say a record is no table entry.)
        views.list[i as usize].1 = ((generation + 1) & GENERATIONS).max(1);
        #[cfg(windows)]
        if let Some(span) = former {
            copies::note_former(i, generation, span);
        }
        // The view's other resource (a counter) keeps a stale entry, which
        // `forget` of that resource skips.
        views.free.push(i);
        note_forgotten(i);
    }
}

/// Table entries freed since the heap pass over the buffer views last asked
/// ([`take_forgotten`]): records that name them read nothing now, though no
/// slot was written. Kept only once a pass has asked. (asked, overflowed,
/// entries)
static FORGOTTEN: std::sync::Mutex<(bool, bool, Vec<u32>)> =
    std::sync::Mutex::new((false, false, Vec::new()));

/// More entries than this between two asks: the list is dropped, and the
/// next ask is told it does not know.
const FORGOTTEN_MOST: usize = 1 << 20;

fn note_forgotten(i: u32) {
    let mut forgotten = FORGOTTEN.lock().unwrap_or_else(|e| e.into_inner());
    let (asked, overflowed, list) = &mut *forgotten;
    if !*asked || *overflowed {
        return;
    }
    if list.len() >= FORGOTTEN_MOST {
        *overflowed = true;
        *list = Vec::new();
        return;
    }
    list.push(i);
}

/// The table entries freed since the last call (in any order, maybe twice);
/// `None` on the first call and when too many were to be kept.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn take_forgotten() -> Option<Vec<u32>> {
    let mut forgotten = FORGOTTEN.lock().unwrap_or_else(|e| e.into_inner());
    let (asked, overflowed, list) = &mut *forgotten;
    let known = *asked && !*overflowed;
    *asked = true;
    *overflowed = false;
    let list = std::mem::take(list);
    known.then_some(list)
}

/// The record at `handle` (a descriptor slot in title memory): its tag and
/// two words, read as they are.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn record_at(handle: u64) -> [u32; 3] {
    // SAFETY: the handle is a descriptor slot in title memory, at least 16
    // bytes.
    unsafe { (handle as *const [u32; 3]).read_unaligned() }
}

/// The view table entry a record names, if it names one (not a view named
/// whole in its record, not a record of no view).
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn record_entry(record: [u32; 3]) -> Option<u32> {
    let [tag, i, generation] = record;
    (tag == VIEW_TAG && generation >> KIND_SHIFT == 0).then_some(i)
}

/// The view the record words `i` and `generation` name: the one they hold
/// whole, or the view of entry `i` if `generation` is still its own.
fn view_at(i: u32, generation: u32) -> Option<View> {
    match generation >> KIND_SHIFT {
        0 => {
            let views = views_read();
            let (view, current) = views.list.get(i as usize)?;
            (*current == generation).then(|| view.clone())
        }
        CONSTANT => Some(constant_of(i, generation)),
        PLACED => views_read().placed_of(i, generation),
        _ => None,
    }
}

/// The view recorded at `handle`, if one is (and its resource is alive).
pub(crate) fn view(handle: u64) -> Option<View> {
    if handle == 0 {
        return None;
    }
    // SAFETY: the handle is a descriptor slot the title passes back.
    let [tag, i, generation] = unsafe { (handle as *const [u32; 3]).read_unaligned() };
    if tag != VIEW_TAG {
        return None;
    }
    view_at(i, generation)
}

/// (#115, `crate::heap_find`) The views in the shader-visible view heaps
/// that `keep` takes: (heap start, slot index, the slot's record words,
/// view). Reads the records as they are; the view table's lock is taken a
/// chunk of slots at a time, the heaps' list for the whole walk. `keep`
/// takes no lock of this module.
#[cfg(windows)]
pub(crate) fn shader_views_where(
    mut keep: impl FnMut(&View) -> bool,
    most: usize,
) -> Vec<(u64, u32, [u32; 3], View)> {
    const CHUNK: u32 = 4096;
    let mut found = Vec::new();
    let heaps = SHADER_HEAPS.read().unwrap_or_else(|e| e.into_inner());
    for &(start, _, at) in heaps.iter() {
        // SAFETY: registered heaps are live (removed when dropped, which
        // waits for this list's lock).
        let heap = unsafe { &*(at as *const Heap) };
        let Some(step) = increment(heap.desc.kind).filter(|_| heap.desc.kind == VIEWS_KIND) else {
            continue;
        };
        let mut first = 0;
        while first < heap.desc.count && found.len() < most {
            let last = first.saturating_add(CHUNK).min(heap.desc.count);
            let views = views_read();
            for index in first..last {
                let record = record_at(start + u64::from(index) * u64::from(step));
                let [tag, i, generation] = record;
                if tag != VIEW_TAG {
                    continue;
                }
                let view = match generation >> KIND_SHIFT {
                    0 => views
                        .list
                        .get(i as usize)
                        .filter(|(_, g)| *g == generation)
                        .map(|(v, _)| v.clone()),
                    CONSTANT => Some(constant_of(i, generation)),
                    PLACED => views.placed_of(i, generation),
                    _ => None,
                };
                if let Some(view) = view.filter(|v| keep(v))
                    && found.len() < most
                {
                    found.push((start, index, record, view));
                }
            }
            drop(views);
            first = last;
        }
    }
    found
}

/// (#115, `crate::heap_find`) The slots of the shader-visible view heaps
/// that hold no record of ours (descriptors the title wrote itself) whose
/// first eight words `keep` takes: (heap start, slot index, words).
#[cfg(windows)]
pub(crate) fn raw_slots_where(
    mut keep: impl FnMut(&[u32; 8]) -> bool,
    most: usize,
) -> Vec<(u64, u32, [u32; 8])> {
    let mut found = Vec::new();
    let heaps = SHADER_HEAPS.read().unwrap_or_else(|e| e.into_inner());
    for &(start, _, at) in heaps.iter() {
        // SAFETY: as in `shader_views_where`.
        let heap = unsafe { &*(at as *const Heap) };
        let Some(step) = increment(heap.desc.kind).filter(|_| heap.desc.kind == VIEWS_KIND) else {
            continue;
        };
        for index in 0..heap.desc.count {
            if found.len() >= most {
                return found;
            }
            let slot = start + u64::from(index) * u64::from(step);
            // SAFETY: a view slot in title memory is 32 bytes.
            let words = unsafe { (slot as *const [u32; 8]).read_unaligned() };
            if words[0] != VIEW_TAG && keep(&words) {
                found.push((start, index, words));
            }
        }
    }
    found
}

/// (#115) The shader-visible heap slot of `handle`: heap start and index.
#[cfg(windows)]
pub(crate) fn shader_heap_slot(handle: u64) -> Option<(u64, u32)> {
    shader_slot(handle).map(|(heap, index)| (heap.start, index))
}

/// (#115, `crate::heap_find`) What the host slot of title slot `index` of
/// the shader-visible heap starting at `heap` shows against the title's
/// record there ([`host::HostHeap::made_state`]).
#[cfg(windows)]
pub(crate) fn host_slot_state(heap: u64, index: u32, record: [u32; 3]) -> &'static str {
    let heaps = SHADER_HEAPS.read().unwrap_or_else(|e| e.into_inner());
    let Some(&(_, _, at)) = heaps.iter().find(|h| h.0 == heap) else {
        return "no_heap";
    };
    // SAFETY: registered heaps are live (removed when dropped, which waits
    // for this list's lock).
    let heap = unsafe { &*(at as *const Heap) };
    match heap.host.get() {
        Some(Some(host)) => host.made_state(index, record),
        _ => "no_host_heap",
    }
}

/// A description of `N` words, if the title passes one.
fn words<const N: usize>(desc: *const [u32; N]) -> Option<[u32; N]> {
    // SAFETY: the title passes a description or null.
    (!desc.is_null()).then(|| unsafe { desc.read_unaligned() })
}

/// Device method.
///
/// A null `desc` writes a null CBV (`Constant(0, 0)`), matching D3D12's
/// `CreateConstantBufferView` with a null description. Leaving the slot
/// untouched would keep a stale view active.
pub(crate) extern "system" fn create_cbv(_: This, desc: *const [u32; 4], handle: u64) {
    let view = match words(desc) {
        Some([lo, hi, size, _]) => {
            let address = u64::from(lo) | u64::from(hi) << 32;
            View::Constant(address, size)
        }
        None => View::Constant(0, 0),
    };
    record(handle, view);
}

/// Device method.
pub(crate) extern "system" fn create_srv(
    _: This,
    resource: This,
    desc: *const [u32; 10],
    handle: u64,
) {
    record(handle, View::Shader(resource as usize, words(desc)));
}

/// Device method.
pub(crate) extern "system" fn create_uav(
    _: This,
    resource: This,
    counter: This,
    desc: *const [u32; 10],
    handle: u64,
) {
    let view = View::Unordered(resource as usize, counter as usize, words(desc));
    record(handle, view);
}

/// Copy only the fields of a placed resource description. Padding can differ
/// between two views of the same texture and the caller may leave it unwritten.
fn placed_resource_words(desc: *const [u32; 14]) -> Option<[u32; 14]> {
    if desc.is_null() {
        return None;
    }
    let src = desc.cast::<u32>();
    let mut fields = [0; 14];
    // SAFETY: the caller supplies a 56-byte ResourceDesc, possibly unaligned.
    // Dimension occupies word 0. Alignment through Flags occupy words 2..13.
    unsafe {
        fields[0] = src.read_unaligned();
        fields[2..13].copy_from_slice(&src.add(2).cast::<[u32; 11]>().read_unaligned());
    }
    Some(fields)
}

/// The words of a placed view's description with those its kind does not
/// use set to zero. The first ten words are the PC's view description: for a
/// shader resource view, the format, the dimension, the component mapping,
/// a word of padding (the union that follows starts on 8 bytes) and the
/// union's six words; for an unordered access view, the format, the
/// dimension and the union's eight words. Each dimension uses only the head
/// of the union, and the title leaves the rest unwritten: run vt-shapes saw
/// 131,392 values in word 3 and 36,413 in word 9 of a million shapes, each
/// a new shape for the same view. Words 10 and on (the address and the
/// console's own fields) are kept; an unknown dimension keeps all.
fn placed_view_words(desc: *const [u32; 16], unordered: bool) -> Option<[u32; 16]> {
    let mut d = words(desc)?;
    // Words of the union each dimension uses (D3D12_BUFFER_SRV and the like).
    let (start, used) = if unordered {
        // D3D12_UAV_DIMENSION: buffer, 1D, 1D array, 2D, 2D array, 2D MS,
        // 2D MS array, 3D.
        const USED: [usize; 9] = [0, 7, 1, 3, 2, 4, 0, 2, 3];
        (2, USED.get(d[1] as usize).copied())
    } else {
        // D3D12_SRV_DIMENSION: buffer, 1D, 1D array, 2D, 2D array, 2D MS,
        // 2D MS array, 3D, cube, cube array, acceleration structure.
        const USED: [usize; 12] = [0, 5, 3, 5, 4, 6, 0, 2, 3, 3, 5, 2];
        (4, USED.get(d[1] as usize).copied())
    };
    if let Some(used) = used.filter(|_| d[1] != 0) {
        if !unordered {
            d[3] = 0;
        }
        d[start + used..10].fill(0);
    }
    Some(d)
}

/// Device methods 0x230 and 0x238: shader resource and unordered access
/// views of memory the title placed itself, `(resource desc, view desc,
/// handle)`.
pub(crate) extern "system" fn create_placed_srv(
    _: This,
    resource: *const [u32; 14],
    desc: *const [u32; 16],
    handle: u64,
) {
    if let (Some(resource), Some(desc)) = (
        placed_resource_words(resource),
        placed_view_words(desc, false),
    ) {
        record(handle, View::Placed(false, resource, desc));
    }
}

pub(crate) extern "system" fn create_placed_uav(
    _: This,
    resource: *const [u32; 14],
    desc: *const [u32; 16],
    handle: u64,
) {
    if let (Some(resource), Some(desc)) = (
        placed_resource_words(resource),
        placed_view_words(desc, true),
    ) {
        record(handle, View::Placed(true, resource, desc));
    }
}

/// Device method.
pub(crate) extern "system" fn create_sampler(_: This, desc: *const [u32; 13], handle: u64) {
    if let Some(desc) = words(desc) {
        record(handle, View::Sampler(desc));
    }
}

/// Device method 0x1e8.
pub(crate) extern "system" fn create_sampler_x(_: This, desc: *const [u32; 27], handle: u64) {
    if let Some(desc) = words(desc) {
        record(handle, View::ConsoleSampler(desc));
    }
}

/// Device method: `count` descriptors of `kind` from `src` to `dst`.
pub(crate) extern "system" fn copy_simple(_: This, count: u32, dst: u64, src: u64, kind: u32) {
    let step = u64::from(increment(kind).unwrap_or(0));
    let len = u64::from(count) * step;
    if dst != 0 && src != 0 {
        // SAFETY: both ranges are descriptor slots the title owns.
        unsafe { std::ptr::copy(src as *const u8, dst as *mut u8, len as usize) };
        #[cfg(windows)]
        for i in 0..u64::from(count) {
            mirror(dst + i * step);
        }
    }
}

/// Device method: ranges of descriptors (start and count each) gathered
/// from the sources and scattered to the destinations, in order.
#[allow(clippy::too_many_arguments)] // the method's own arguments
pub(crate) extern "system" fn copy(
    _: This,
    dst_ranges: u32,
    dst_starts: *const u64,
    dst_sizes: *const u32,
    src_ranges: u32,
    src_starts: *const u64,
    src_sizes: *const u32,
    kind: u32,
) {
    let Some(step) = increment(kind) else {
        return;
    };
    // Each range as (start, count); a null size array means one descriptor
    // per range.
    let ranges = |n: u32, starts: *const u64, sizes: *const u32| -> Vec<(u64, u32)> {
        (0..n as usize)
            .map(|i| {
                // SAFETY: the title passes `n` starts and sizes (or null sizes).
                unsafe {
                    (
                        *starts.add(i),
                        if sizes.is_null() { 1 } else { *sizes.add(i) },
                    )
                }
            })
            .collect()
    };
    if dst_starts.is_null() || src_starts.is_null() {
        return;
    }
    let src = ranges(src_ranges, src_starts, src_sizes);
    let mut dst = ranges(dst_ranges, dst_starts, dst_sizes).into_iter();
    let (mut to, mut room) = dst.next().unwrap_or((0, 0));
    for (mut from, mut left) in src {
        while left > 0 {
            while room == 0 {
                let Some(next) = dst.next() else { return };
                (to, room) = next;
            }
            let n = left.min(room);
            copy_simple(std::ptr::null(), n, to, from, kind);
            let bytes = u64::from(n) * u64::from(step);
            (to, from, room, left) = (to + bytes, from + bytes, room - n, left - n);
        }
    }
}

/// The view recorded at `handle`, if one is.
pub(crate) fn rtv_record(handle: u64) -> Option<RtvRecord> {
    tagged_record(handle, RTV_TAG)
}

/// The depth view recorded at `handle`, if one is.
pub(crate) fn dsv_record(handle: u64) -> Option<RtvRecord> {
    tagged_record(handle, DSV_TAG)
}

fn tagged_record(handle: u64, tag: u32) -> Option<RtvRecord> {
    if handle == 0 {
        return None;
    }
    // SAFETY: the handle is a descriptor slot the title passes back.
    let record = unsafe { (handle as *const RtvRecord).read_unaligned() };
    (record.tag == tag).then_some(record)
}

/// Device method.
pub(crate) extern "system" fn create_heap(
    _: This,
    desc_ptr: *const HeapDesc,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    // SAFETY: the title passes a description (null is tolerated).
    let Some(&desc) = (unsafe { desc_ptr.as_ref() }) else {
        return E_INVALIDARG;
    };
    let Some(size) = increment(desc.kind).map(|i| u64::from(i) * u64::from(desc.count.max(1)))
    else {
        return E_INVALIDARG;
    };
    // SAFETY: read only as a hint: a title that passes the plain
    // description has its own stack or data after it.
    let placement = unsafe { desc_ptr.add(1).cast::<Placement>().read_unaligned() };
    // shortcut: a placement is recognised by its size matching the count,
    // since the plain and extended descriptions share the slot and the
    // interface version; find the flag that tells them apart if a title
    // trips over this.
    let placed = placement.address != 0 && placement.size == size;
    tracing::debug!(?desc, placed, "descriptor heap");
    let (start, memory) = if placed {
        (placement.address, None)
    } else {
        match Memory::zeroed(size, 64) {
            Ok(memory) => (memory.address(), Some(memory)),
            Err(hr) => return hr,
        }
    };
    let visible = desc.flags & SHADER_VISIBLE != 0 && matches!(desc.kind, VIEWS_KIND | SAMPLERS);
    let heap = Box::new(Heap {
        object: Object::new(&FUNCTIONS),
        desc,
        start,
        memory,
        #[cfg(windows)]
        host: std::sync::OnceLock::new(),
        #[cfg(windows)]
        pending: Default::default(),
        #[cfg(windows)]
        written: (placed && visible)
            .then(|| crate::watch::track(start as usize, size as usize, true))
            .flatten(),
        #[cfg(windows)]
        buffer_written: (placed && visible)
            .then(|| crate::watch::track(start as usize, size as usize, true))
            .flatten(),
        #[cfg(windows)]
        table_retries: (placed && visible && desc.kind == VIEWS_KIND && tabled_all::on())
            .then(std::sync::Mutex::default),
    });
    #[cfg(windows)]
    if visible {
        let at = std::ptr::from_ref::<Heap>(&heap) as usize;
        let mut heaps = SHADER_HEAPS.write().unwrap_or_else(|e| e.into_inner());
        heaps.push((start, start + size, at));
    }
    #[cfg(windows)]
    if desc.kind == VIEWS_KIND {
        copies::register(std::ptr::from_ref::<Heap>(&heap) as usize);
    }
    #[cfg(not(windows))]
    let _ = visible;
    object::hand_out(DESCRIPTOR_HEAP, heap, iid, &HEAP_IIDS, out)
}

extern "system" fn query(this: This, iid: *const Iid, out: *mut This) -> HRESULT {
    let found = object::is(iid, &HEAP_IIDS).then_some(this);
    object::answer(DESCRIPTOR_HEAP, iid, found, out)
}
extern "system" fn release(this: This) -> u32 {
    object::release_boxed::<Heap>(this)
}
extern "system" fn desc(this: This, out: *mut HeapDesc) {
    if !out.is_null() {
        // SAFETY: checked non-null; the title passes room for a description.
        unsafe { out.write(heap(this).desc) };
    }
}
/// The same address for the CPU and the GPU start.
extern "system" fn start(this: This) -> u64 {
    heap(this).start
}

/// IUnknown 3, object 4, device child 1, heap 3.
static FUNCTIONS: Table<24> = {
    let mut f = object::d3d12_object(object::missing_table::<DESCRIPTOR_HEAP, 24>());
    f[0] = query as *const ();
    f[2] = release as *const ();
    f[7] = object::parent_device as *const (); // 0x38
    f[8] = desc as *const (); // 0x40
    f[9] = start as *const (); // 0x48 CPU
    f[10] = start as *const (); // 0x50 GPU
    Table(f)
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::{S_OK, slot};

    /// A view's snapshot keeps the values it had when taken, whatever is
    /// written to the record afterwards.
    #[cfg(windows)]
    #[test]
    fn render_target_snapshot_preserves_original_descriptor_words() {
        let mut record = RtvRecord {
            tag: RTV_TAG,
            described: 1,
            resource: 0x1000usize as This,
            desc: [24, 4, 0, 0, 0, 0],
        };
        let snapshot = record.snapshot();
        record.desc[0] = 26;
        record.resource = 0x2000usize as This;
        let restored = snapshot.restore();
        assert_eq!(restored.tag, record.tag);
        assert_eq!(restored.described, record.described);
        assert_eq!(restored.desc[0], 24);
        assert_eq!(restored.resource as usize, 0x1000);
        assert_eq!(record.desc[0], 26);
        assert_eq!(record.resource as usize, 0x2000);
    }

    /// What newer titles pass; tests use it throughout, so that reading the
    /// placement stays inside the value.
    #[repr(C)]
    struct Extended(HeapDesc, Placement);

    fn create(desc: HeapDesc, placement: Placement, out: &mut This) -> HRESULT {
        let extended = Extended(desc, placement);
        create_heap(std::ptr::null(), &extended.0, &HEAP_IIDS[0], out)
    }

    #[cfg(windows)]
    #[test]
    fn raw_heap_sync_stops_before_dynamic_host_slots() {
        let start = 0x1000usize;
        let step = 32;
        let capped = 5;
        assert_eq!(
            changed_slots(
                start + 3 * step + 4,
                start + 4 * step + 8,
                start,
                step as u64,
                capped
            ),
            3..5
        );
        assert_eq!(
            changed_slots(
                start + 6 * step,
                start + 9 * step,
                start,
                step as u64,
                capped
            ),
            5..5
        );
    }

    #[test]
    fn heaps_hold_count_descriptors_from_their_start() {
        let null = std::ptr::null();
        let desc = HeapDesc {
            kind: 0,
            count: 1000,
            flags: 1,
            nodes: 0,
        };
        let none = Placement {
            address: 0,
            size: 0,
        };
        let mut h = null;
        assert_eq!(create(desc, none, &mut h), S_OK);
        let cpu = slot::<extern "system" fn(This) -> u64>(h, 0x48)(h);
        assert_eq!(cpu % 64, 0);
        assert_eq!(slot::<extern "system" fn(This) -> u64>(h, 0x50)(h), cpu);
        assert_eq!(increment_size(null, 0), 32);
        // SAFETY: the heap holds 1000 descriptors of 32 bytes.
        unsafe { (cpu as *mut u8).add(999 * 32 + 31).write(1) };
        assert_eq!(increment_size(null, 9), 0);
        let bad = HeapDesc { kind: 9, ..desc };
        assert_eq!(create(bad, none, &mut h), E_INVALIDARG);
    }

    /// A descriptor copied into a placed heap as bytes is mirrored before
    /// the next submission, even though no descriptor method saw the copy.
    #[cfg(windows)]
    #[test]
    fn records_copied_as_bytes_are_made_at_the_next_sync() {
        use windows_sys::Win32::System::Memory::{
            MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree,
        };
        const MEM_WRITE_WATCH: u32 = 0x20_0000;
        let memory = unsafe {
            VirtualAlloc(
                std::ptr::null(),
                crate::watch::PAGE,
                MEM_RESERVE | MEM_COMMIT | MEM_WRITE_WATCH,
                PAGE_READWRITE,
            )
        };
        assert!(!memory.is_null());
        let desc = HeapDesc {
            kind: SAMPLERS,
            count: 64,
            flags: SHADER_VISIBLE,
            nodes: 1,
        };
        let placement = Placement {
            address: memory as u64,
            size: 64 * 16,
        };
        let mut h: This = std::ptr::null();
        assert_eq!(create(desc, placement, &mut h), S_OK);
        if let Some(host) = heap(h).host() {
            let mut elsewhere = [0u64; 2];
            let source = elsewhere.as_mut_ptr() as u64;
            let sampler = [21, 3, 3, 3, 0, 1, 0, 0, 0, 0, 0, 0x7f7f_ffff, 0];
            create_sampler(std::ptr::null(), &sampler, source);
            // SAFETY: the heap slot and source both hold 16 descriptor bytes.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    source as *const u8,
                    memory.cast::<u8>().add(5 * 16),
                    16,
                )
            };
            assert!(!host.made(5));
            sync_heaps();
            assert!(host.made(5));
        }
        slot::<extern "system" fn(This) -> u32>(h, 0x10)(h);
        // SAFETY: this allocation belongs to the test.
        unsafe { VirtualFree(memory, 0, MEM_RELEASE) };
    }

    /// A null CBV description overwrites the slot (D3D12 null CBV), so a
    /// later read does not see the previous view.
    #[test]
    fn null_cbv_clears_a_stale_view() {
        let null = std::ptr::null();
        let slots = [0u64; 2 * 4];
        let a = slots.as_ptr() as u64;
        create_cbv(null, &[0x1000, 1, 256, 0], a);
        assert_eq!(view(a), Some(View::Constant(0x1_0000_1000, 256)));
        create_cbv(null, std::ptr::null(), a);
        assert_eq!(view(a), Some(View::Constant(0, 0)));
        create_srv(null, null, std::ptr::null(), a);
        assert_eq!(view(a), Some(View::Shader(0, None)));
        create_cbv(null, std::ptr::null(), a);
        assert_eq!(view(a), Some(View::Constant(0, 0)));
    }

    /// Two placed views alike but for the words their kind does not use
    /// (the padding after the component mapping, the union's unused tail)
    /// are one record; a used word still tells them apart.
    #[test]
    fn placed_views_ignore_unused_view_words() {
        let mut slot = [0u32; 8];
        let handle = slot.as_mut_ptr() as u64;
        let null = std::ptr::null();
        let resource = [1, 0, 0, 0, 0x4000, 0, 1, 0x10001, 0, 1, 0, 1, 0, 0];
        // A structured buffer view: words 4-5 first element, 6 count, 7
        // stride, 8 flags; 3 and 9 are not used.
        let srv = [
            0, 1, 0x1688, 0, 16, 0, 64, 16, 0, 0, 0x1000, 0x6, 0, 0, 0, 0,
        ];
        // An unordered buffer view: words 2-3 first element, 4 count, 5
        // stride, 6-7 counter offset, 8 flags; 9 is not used.
        let uav = [0, 1, 16, 0, 64, 16, 0, 0, 0, 0, 0x1000, 0x6, 0, 0, 0, 0];
        for (unordered, create, desc, unused, used) in [
            (
                false,
                create_placed_srv as extern "system" fn(_, _, _, _),
                srv,
                [3usize, 9],
                6usize,
            ),
            (true, create_placed_uav, uav, [9, 9], 4),
        ] {
            create(null, &resource, &desc, handle);
            let first = slot;
            assert_eq!(view(handle), Some(View::Placed(unordered, resource, desc)));
            let mut noisy = desc;
            for w in unused {
                noisy[w] = 0xdead_beef;
            }
            create(null, &resource, &noisy, handle);
            assert_eq!(slot, first, "unused words left out");
            let mut other = desc;
            other[used] += 1;
            create(null, &resource, &other, handle);
            assert_ne!(slot, first, "a used word counts");
        }
        // A 2D texture view uses four words of the union (4-7): 8 and 9 go.
        let mut tex = [
            28, 4, 0x1688, 0x77, 0, 1, 0, 0, 0x55, 0x66, 0x2000, 0x6, 0, 0, 0, 0,
        ];
        create_placed_srv(null, &resource, &tex, handle);
        tex[3] = 0;
        (tex[8], tex[9]) = (0, 0);
        assert_eq!(view(handle), Some(View::Placed(false, resource, tex)));
    }

    #[test]
    fn placed_views_share_resources_across_description_padding() {
        let mut slot = [0u32; 8];
        let handle = slot.as_mut_ptr() as u64;
        let view_desc = [0u32; 16];
        let null = std::ptr::null();
        let mut resource = [3, 0, 0, 0, 8, 0, 8, 0x10001, 28, 1, 0, 0, 0, 0];
        let expected = resource;
        for (unordered, create) in [
            (false, create_placed_srv as extern "system" fn(_, _, _, _)),
            (true, create_placed_uav),
        ] {
            create(null, &expected, &view_desc, handle);
            let first = slot;
            resource[1] = 0xaaaa_aaaa;
            resource[13] = 0xbbbb_bbbb;
            create(null, &resource, &view_desc, handle);
            assert_eq!(slot, first);
            assert_eq!(
                view(handle),
                Some(View::Placed(unordered, expected, view_desc))
            );
            let mut other = resource;
            other[12] = 4;
            create(null, &other, &view_desc, handle);
            assert_ne!(slot, first);
        }
    }

    #[test]
    fn views_survive_copies() {
        let null = std::ptr::null();
        let mut heap = vec![0u64; 4 * 8]; // four 32-byte slots
        let at = |i: usize| heap.as_ptr() as u64 + 32 * i as u64;
        let (s0, s1, s2, s3) = (at(0), at(1), at(2), at(3));
        create_cbv(null, &[0x1000, 1, 256, 0], s0);
        assert_eq!(view(s0), Some(View::Constant(0x1_0000_1000, 256)));
        create_srv(null, null, std::ptr::null(), s1);
        assert_eq!(view(s1), Some(View::Shader(0, None)));
        // The same view twice is one entry.
        create_srv(null, null, std::ptr::null(), s2);
        // SAFETY: both slots are in `heap`.
        assert_eq!(unsafe { *(s1 as *const [u32; 3]) }, unsafe {
            *(s2 as *const [u32; 3])
        });
        // Two ranges of one into one range of two.
        let (dst, dst_n, src, src_n) = ([s2], [2u32], [s0, s1], [1u32, 1]);
        copy(
            null,
            1,
            dst.as_ptr(),
            dst_n.as_ptr(),
            2,
            src.as_ptr(),
            src_n.as_ptr(),
            0,
        );
        assert_eq!(view(s2), view(s0));
        assert_eq!(view(s3), view(s1));
        assert_eq!(view(at(0) + 8), None);
        heap[0] = 0;
    }

    /// Constant buffer views are named whole in their records: any address
    /// a program can have, any size below 16 KiB, multiples of 256 bytes
    /// up to 4 MiB; what does not fit says so.
    #[test]
    fn constant_views_fit_their_records() {
        let top = (1u64 << CONSTANT_ADDRESS_BITS) - 1;
        for address in [
            0u64,
            0x100,
            0x2602_0000,
            0xd_6b60_0000,
            0x7ffd_a34f_0000,
            top,
        ] {
            for size in [
                0u32,
                1,
                255,
                256,
                4000,
                (1 << 14) - 1,
                1 << 14,
                65536,
                (1 << 22) - 256,
            ] {
                let [i, generation] = constant_words(address, size).expect("fits");
                assert_eq!(generation >> KIND_SHIFT, CONSTANT);
                assert_eq!(
                    view_at(i, generation),
                    Some(View::Constant(address, size)),
                    "{address:#x} {size}"
                );
            }
        }
        assert_eq!(
            constant_words(top + 1, 256),
            None,
            "an address past 47 bits"
        );
        assert_eq!(
            constant_words(0x1000, (1 << 14) + 1),
            None,
            "16 KiB or more, no multiple of 256"
        );
        assert_eq!(constant_words(0x1000, 1 << 22), None, "4 MiB");
        // Different views never share their words.
        assert_ne!(constant_words(0x1000, 256), constant_words(0x1000, 512));
        assert_ne!(constant_words(0x1000, 256), constant_words(0x1100, 256));
        assert_ne!(constant_words(0x1000, 1 << 14), constant_words(0x1000, 64));
    }

    /// The counts of constant buffer and placed entries the memory line
    /// reads follow the table as a walk over it would count: a freed entry
    /// reused for another kind is counted out and in.
    #[cfg(all(windows, feature = "diagnostics"))]
    #[test]
    fn diagnostic_counts_follow_distinct_entries_including_reused_slots() {
        let walk = |views: &Views| {
            let count =
                |f: fn(&View) -> bool| views.list.iter().filter(|(v, _)| f(v)).count() as u64;
            [
                count(|v| matches!(v, View::Constant(..))),
                count(|v| matches!(v, View::Placed(..))),
            ]
        };
        let mut views = Views::default();
        // A slot freed by forget still holds the old resource view, with its
        // new generation; it can be reused for a view without a resource.
        views.list.push((View::Shader(1, None), 2));
        views.free.push(0);
        let constant = View::Constant(0x1234, 256);
        assert_eq!(views.entry(constant.clone()), [0, 2]);
        assert_eq!(views.entry(constant), [0, 2]);
        let placed = View::Placed(false, [0; 14], [0; 16]);
        let index = views.entry(placed.clone());
        assert_eq!(views.entry(placed), index);
        views.entry(View::Unordered(1, 2, None));
        assert_eq!(views.address_entries, [1, 1]);
        assert_eq!(views.address_entries, walk(&views));
        // Under a cap of 2, placed views push the constant one out; its
        // freed entry, reused, changes kind.
        for n in 1..6u32 {
            views.entry_kept(View::Placed(false, [n; 14], [n; 16]), 2);
            assert_eq!(views.address_entries, walk(&views), "after placed view {n}");
        }
        views.entry_kept(View::Constant(0x5678, 512), 2);
        assert_eq!(
            views.address_entries,
            walk(&views),
            "a constant view in a freed entry"
        );
    }

    /// Entries of views no resource releases stay under their cap: the
    /// oldest go stale, the newest still name their views, entries are
    /// reused rather than added, and views of resources are never dropped.
    #[test]
    fn ownerless_entries_are_capped() {
        let mut views = Views::default();
        let placed = |n: u32| View::Placed(false, [n; 14], [n; 16]);
        let named = |views: &Views, words: [u32; 2], view: &View| {
            views
                .list
                .get(words[0] as usize)
                .is_some_and(|(v, g)| *g == words[1] && v == view)
        };
        let words: Vec<[u32; 2]> = (0..10).map(|n| views.entry_kept(placed(n), 4)).collect();
        assert_eq!(views.index.len(), 4, "four kept");
        assert!(
            views.list.len() <= 5,
            "entries reused, not added: {}",
            views.list.len()
        );
        for n in 0..6 {
            assert!(
                !named(&views, words[n], &placed(n as u32)),
                "entry {n} stale"
            );
        }
        for n in 6..10 {
            assert!(named(&views, words[n], &placed(n as u32)), "entry {n} kept");
        }
        // The same view written again keeps its words.
        assert_eq!(views.entry_kept(placed(9), 4), words[9]);
        // A view of a resource is freed with the resource only.
        let shader = View::Shader(0x1234, None);
        let kept = views.entry_kept(shader.clone(), 4);
        for n in 10..40 {
            views.entry_kept(placed(n), 4);
        }
        assert!(named(&views, kept, &shader));
        assert!(views.list.len() <= 6);
        // 0 keeps them all.
        for n in 40..60 {
            views.entry_kept(placed(n), 0);
        }
        assert_eq!(views.index.len(), 1 + 4 + 20);
    }

    /// The leak of run 65: a title that writes constant buffer views and
    /// views of placed memory at ever new addresses adds nothing to the
    /// table; one shape stands for all the placed views alike.
    #[test]
    fn views_at_new_addresses_add_no_entries() {
        if !inline() {
            return;
        }
        let null = std::ptr::null();
        let slots = [0u64; 4];
        let at = slots.as_ptr() as u64;
        // A resource description and a view no other test uses.
        // (Its padding words, 1 and 13, are zero: placed views leave them out.)
        let resource: [u32; 14] = std::array::from_fn(|k| {
            if k == 1 || k == 13 {
                0
            } else {
                0x5ee0_0000 + k as u32
            }
        });
        let mut desc: [u32; 16] = std::array::from_fn(|k| 0x7a70_0000 + k as u32);
        let mine = |v: &View| match v {
            View::Constant(address, _) => (0x6_0000_0000..0x6_1000_0000).contains(address),
            View::Placed(_, r, _) => *r == resource,
            _ => false,
        };
        let entries = || {
            VIEWS
                .read()
                .unwrap()
                .list
                .iter()
                .filter(|(v, _)| mine(v))
                .count()
        };
        let shapes = || {
            VIEWS
                .read()
                .unwrap()
                .shapes
                .iter()
                .filter(|s| s.1 == resource)
                .count()
        };
        for n in 0..20_000u64 {
            let address = 0x6_0000_0000 + n * 0x100;
            create_cbv(null, &[address as u32, (address >> 32) as u32, 512, 0], at);
            assert_eq!(view(at), Some(View::Constant(address, 512)));
            let placed = 0xd_4940_0000 + n * 0x1_0000;
            (desc[10], desc[11]) = (placed as u32, (placed >> 32) as u32);
            create_placed_srv(null, &resource, &desc, at);
            assert_eq!(view(at), Some(View::Placed(false, resource, desc)));
        }
        assert_eq!(entries(), 0, "no table entries");
        assert_eq!(shapes(), 1, "one shape for 20,000 placed views");
        // The same shape, unordered: another shape.
        create_placed_uav(null, &resource, &desc, at);
        assert_eq!(view(at), Some(View::Placed(true, resource, desc)));
        assert_eq!(shapes(), 2);
        // Placed memory past 1 TiB (not title memory) is a table entry.
        (desc[10], desc[11]) = (0, 0x100);
        create_placed_srv(null, &resource, &desc, at);
        assert_eq!(view(at), Some(View::Placed(false, resource, desc)));
        assert_eq!(entries(), 1);
        // A record's words are the same for the same view (the host knows
        // a slot's view by them), and differ with the address.
        // SAFETY: both handles are slots of 32 bytes above.
        let words = |handle: u64| unsafe { *(handle as *const [u32; 3]) };
        let other = [0u64; 4];
        create_cbv(null, &[0x40, 6, 512, 0], at);
        create_cbv(null, &[0x40, 6, 512, 0], other.as_ptr() as u64);
        assert_eq!(words(at), words(other.as_ptr() as u64));
        create_cbv(null, &[0x140, 6, 512, 0], other.as_ptr() as u64);
        assert_ne!(words(at), words(other.as_ptr() as u64));
    }

    /// A released resource's views go stale; the same view of a new
    /// resource at the same address is a new record.
    #[test]
    fn views_die_with_their_resource() {
        let null = std::ptr::null();
        let slots = [0u64; 2 * 4];
        let (a, b) = (slots.as_ptr() as u64, slots.as_ptr() as u64 + 32);
        let resource = 0x7777_0000usize as This;
        create_srv(null, resource, std::ptr::null(), a);
        assert_eq!(view(a), Some(View::Shader(0x7777_0000, None)));
        forget(0x7777_0000);
        assert_eq!(view(a), None, "stale after release");
        create_srv(null, resource, std::ptr::null(), b);
        assert_eq!(view(b), Some(View::Shader(0x7777_0000, None)));
        // SAFETY: both slots are in `slots`.
        let (old, new) = unsafe { (*(a as *const [u32; 3]), *(b as *const [u32; 3])) };
        assert_ne!(old, new, "a new generation");
    }

    /// The title places its shader-visible heap in its own memory
    /// and checks that the heap starts there.
    #[test]
    fn placed_heaps_start_at_the_title_memory() {
        let mut memory = vec![0u8; 64 * 32];
        let address = memory.as_mut_ptr() as u64;
        let desc = HeapDesc {
            kind: 0,
            count: 64,
            flags: 1,
            nodes: 1,
        };
        let mut h = std::ptr::null();
        assert_eq!(
            create(
                desc,
                Placement {
                    address,
                    size: 64 * 32
                },
                &mut h
            ),
            S_OK
        );
        assert_eq!(slot::<extern "system" fn(This) -> u64>(h, 0x48)(h), address);
        assert_eq!(slot::<extern "system" fn(This) -> u64>(h, 0x50)(h), address);
        // A size that does not match the count is not a placement.
        assert_eq!(create(desc, Placement { address, size: 7 }, &mut h), S_OK);
        assert_ne!(slot::<extern "system" fn(This) -> u64>(h, 0x48)(h), address);
    }

    #[cfg(windows)]
    #[test]
    fn async_heap_sync_completes_sequential_tickets() {
        let mut prev = 0u64;
        for _ in 0..8 {
            let ticket = sync_heaps_start();
            assert!(ticket > prev, "tickets advance monotonically");
            sync_heaps_wait(ticket);
            prev = ticket;
        }
        // Already-completed tickets and 0 return immediately.
        sync_heaps_wait(0);
        sync_heaps_wait(prev);
        // Back-to-back starts before waiting complete cleanly without deadlock.
        let t1 = sync_heaps_start();
        let t2 = sync_heaps_start();
        assert!(t2 > t1);
        sync_heaps_wait(t1);
        sync_heaps_wait(t2);
        assert!(!overlaps_shader_heap(0, 0));
    }

}
