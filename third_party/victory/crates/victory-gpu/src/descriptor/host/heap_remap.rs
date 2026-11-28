//! (heapcap) `VICTORY_HEAP_CAP=3`: the title's view slots past a capped
//! host heap's get their own descriptors, packed into host slots the
//! dynamic range gives up.
//!
//! Layout on the RTX 5090 (null slot 999,999, title heap 1,245,184): title
//! slots 0..737,855 are the host's own, slot 737,855 is null, the next
//! [`compact_slots`] (`VICTORY_HEAP_CAP_COMPACT`, default 65,536) hold the
//! live views of title slots 737,855.., the rest up to 999,999 are the
//! dynamic slots. Translated shaders read title view indices at or past
//! 737,855 through a table (`victory_shader::emit::SLOT_REMAP`: dword `k`
//! the host slot of title slot 737,855 + k, the null slot when it has
//! none), kept in the heap's strides buffer from `REMAP_BASE` on.
//!
//! The title writes records past the host heap by copying them into its
//! heap's pages and by making views there; both mark those title slots
//! (`heap_cap::made_past`, `heap_cap::note_writes`), and every
//! [`RESCAN_EVERY`] frames all of them are looked at again (a view let go
//! without a write goes null). A record naming a live view gets a packed
//! slot; a slot whose record no longer does gives its packed slot back
//! (reused [`REUSE_AFTER`] frames later).
//!
//! A packed view goes through four stages ([`Stage`]): queued, its view
//! made on the `victory-heap-remap` thread (off the submit thread: host
//! copies and descriptors are made there, in order with every other make or
//! null of packed slots), readied on a command list by the pass that
//! readies textures and buffers picked by index (`draw.rs` `heap_buffers`:
//! [`ready_pass`], the upload of the texture or the copy of the buffer
//! range, as for any other slot), and only then shown: its table entry
//! points at it. Until then the entry is the null slot, so a shader never
//! reads a view not made or not uploaded. A view made with no host
//! resource (no room, or refused) is made again later. Shown views are
//! readied again in turn ([`RECHECK_BUDGET`] a pass), as draws ready what
//! their tables name, so what the title writes into them later arrives.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Condvar, Mutex};
use windows::Win32::Graphics::Direct3D12::{D3D12_CPU_DESCRIPTOR_HANDLE, ID3D12DescriptorHeap};

use super::super::View;

mod commit;
mod demand;

/// Frames between looks at every title slot past the host heap.
pub(crate) const RESCAN_EVERY: u64 = 300;
/// Frames a packed slot given back waits before another title slot gets it.
pub(crate) const REUSE_AFTER: u64 = 4;
/// Title slots read, and views queued, at most per call of [`refresh`].
const READ_BUDGET: u32 = 1 << 16;
const MAKE_BUDGET: u32 = 4096;
/// Failed makes queued again per call, at most.
const RETRY_BUDGET: usize = 1024;
/// Made views readied and shown per [`ready_pass`], at most; shown ones
/// readied again.
pub(crate) const READY_BUDGET: usize = 256;
pub(crate) const RECHECK_BUDGET: usize = 64;
/// Time and staged bytes a [`ready_pass`] spends at most.
const READY_SPAN: std::time::Duration = std::time::Duration::from_micros(1500);
const READY_BYTES: u64 = 16 << 20;
/// Made views waiting past which a [`ready_pass`] catches up: 1,024 views,
/// 3 ms and 48 MB a pass (Part S: 8 ms and 192 MB a pass put ready_ms at
/// 473 a window and staging on a box out of commit).
const CATCH_UP: usize = 4096;

/// Views, time and bytes a [`ready_pass`] spends with `backlog` made views
/// waiting.
pub(crate) fn ready_budget(backlog: usize) -> (usize, std::time::Duration, u64) {
    if backlog > CATCH_UP {
        (1024, std::time::Duration::from_millis(3), 48 << 20)
    } else {
        (READY_BUDGET, READY_SPAN, READY_BYTES)
    }
}

/// Host slots packed title views get (`VICTORY_HEAP_CAP_COMPACT`, 0 to
/// 131,072; default 65,536), taken from the dynamic slots.
pub(crate) fn compact_slots() -> u32 {
    static SLOTS: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *SLOTS.get_or_init(|| {
        std::env::var("VICTORY_HEAP_CAP_COMPACT")
            .ok()
            .and_then(|v| v.trim().parse::<u32>().ok())
            .unwrap_or(1 << 16)
            .min(1 << 17)
    })
}

/// (`=4`, `=5`) The packed slots of a heap of `dynamic` dynamic slots whose
/// title null slot is dynamic slot `at`: all after it but the `keep` last.
pub(crate) fn packed_full(at: u32, dynamic: u32, keep: u32) -> u32 {
    dynamic.saturating_sub(at + 1 + keep)
}

/// The packed slots of a heap of `dynamic` dynamic slots whose title null
/// slot is dynamic slot `at`: `compact_slots()`, never more than leaves
/// half the dynamic slots.
pub(crate) fn compact_of(wanted: u32, at: u32, dynamic: u32) -> u32 {
    wanted.min((dynamic / 2).saturating_sub(at + 1))
}

/// Where a packed view is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stage {
    /// Its view is being made (`victory-heap-remap`).
    Queued,
    /// Made with no host resource: queued again later.
    Failed,
    /// Made: readied next by [`ready_pass`], then shown.
    Made,
    /// Its table entry points at it.
    Shown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Owner {
    title: u32,
    key: [u32; 3],
    /// Bumped at every make, so a late result of an earlier one is told apart.
    generation: u64,
    stage: Stage,
    /// (`=5`) The last frame a shader is known to have read it (0: never
    /// known), and the frame its entry was flagged to ask whether one does.
    touched: u64,
    armed: Option<u64>,
    /// Makes that found no host resource in a row, and the frame the next
    /// may be tried (waits of 8 frames doubling to 1,024).
    tries: u8,
    retry_at: u64,
}

/// The frame a view whose make failed `tries` times in a row, the last at
/// `frame`, is made again.
pub(crate) fn retry_at(frame: u64, tries: u8) -> u64 {
    frame + (8u64 << tries.saturating_sub(1).min(7))
}

/// What [`Packed::place`] says to do for a title slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Placed {
    /// Its packed slot shows (or is making) its view already.
    Same,
    /// Point its table entry at the null slot, make its view in host slot
    /// `at` (as make `generation`).
    Make { at: u32, generation: u64 },
    /// Its view is gone: point its table entry at the null slot, make this
    /// host slot null.
    Drop(u32),
    /// No packed slot left: it reads the null slot.
    Overflow,
    /// No view, none before.
    Nothing,
}

/// A made or shown view to ready ([`Packed::take_made`], [`Packed::take_shown`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Due {
    pub at: u32,
    pub title: u32,
    pub key: [u32; 3],
    pub generation: u64,
}

/// The packed slots of one host heap: which title slot holds each.
pub(crate) struct Packed {
    pub id: u64,
    pub title_null: u32,
    pub base: u32,
    pub cap: u32,
    map: HashMap<u32, u32>,
    owner: Vec<Option<Owner>>,
    free: VecDeque<(u32, u64)>,
    next: u32,
    generation: u64,
    /// Where the next look at shown views starts.
    cursor: usize,
    /// Title slot ranges to look at, and whether to make their views again
    /// even when the record is the same.
    dirty: Vec<(u32, u32, bool)>,
    seeded: bool,
    last_rescan: u64,
    /// The frame of the last [`refresh`].
    now: u64,
    pub counts: Counts,
    /// Table entry `k` names title slot `entry_base + k`, `entry_len` of
    /// them and a last one, null; the table's first dword (0: none).
    entry_base: u32,
    entry_len: u32,
    table: usize,
    /// Packed slots (index, make) made since, and those whose make found no
    /// host resource: to ready and show, or to make again.
    made_queue: VecDeque<(u32, u64)>,
    failed_queue: VecDeque<(u32, u64)>,
    /// (`=5`) Packed on demand; the fault buffer's mapping (0: none); title
    /// slots a shader read with no packed view (and their records), first to
    /// pack; where the arming and eviction looks go on.
    demand: bool,
    faults: usize,
    wanted: VecDeque<(u32, [u32; 3])>,
    wanted_set: std::collections::HashSet<u32>,
    arm_hand: usize,
    evict_hand: usize,
}

/// Since the last line.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Counts {
    pub queued: u64,
    pub made: u64,
    pub unready: u64,
    pub stale: u64,
    pub readied: u64,
    pub shown: u64,
    pub rechecked: u64,
    pub dropped: u64,
    pub overflow: u64,
    pub unread: u64,
    pub looked: u64,
    pub worker_us: u64,
    pub ready_us: u64,
    pub ready_bytes: u64,
    /// (`=5`) Fault notes: touched (armed and read), wanted (live, no packed
    /// view), dead; entries armed; packed views evicted; notes read.
    pub touched: u64,
    pub wanted: u64,
    pub dead: u64,
    pub armed: u64,
    pub evicted: u64,
    pub notes: u64,
    /// The longest ready pass, and passes skipped for commit.
    pub ready_max_us: u64,
    pub ready_waits: u64,
}

impl Packed {
    pub(crate) fn new(id: u64, title_null: u32, cap: u32) -> Self {
        Self {
            id,
            title_null,
            base: title_null + 1,
            cap,
            map: HashMap::new(),
            owner: vec![None; cap as usize],
            free: VecDeque::new(),
            next: 0,
            generation: 0,
            cursor: 0,
            dirty: Vec::new(),
            seeded: false,
            last_rescan: 0,
            now: 0,
            counts: Counts::default(),
            entry_base: title_null,
            entry_len: victory_shader::emit::REMAP_LEN,
            table: 0,
            made_queue: VecDeque::new(),
            failed_queue: VecDeque::new(),
            demand: false,
            faults: 0,
            wanted: VecDeque::new(),
            wanted_set: Default::default(),
            arm_hand: 0,
            evict_hand: 0,
        }
    }

    /// (`=5`) Packed on demand, its fault buffer mapped at `faults`.
    pub(crate) fn with_demand(mut self, faults: usize) -> Self {
        (self.demand, self.faults) = (true, faults);
        self
    }

    /// The table value of a title slot with no packed view: the null slot,
    /// flagged under `=5` when its view is `live` (a shader that reads it
    /// says so).
    fn unpacked(&self, live: bool) -> u32 {
        if self.demand && live {
            self.title_null | victory_shader::emit::FAULT_FLAG
        } else {
            self.title_null
        }
    }

    /// Its table: entry `k` names title slot `base + k`, `len` of them.
    pub(crate) fn with_table(mut self, table: usize, base: u32, len: u32) -> Self {
        (self.table, self.entry_base, self.entry_len) = (table, base, len);
        self
    }

    /// Points table entry `entry` at host slot `value`.
    fn put(&self, entry: u32, value: u32) {
        if self.table != 0 {
            put(self.table as *mut u32, self.entry_len, entry, value);
        }
    }

    /// Whether no packed slot can be given at `frame`.
    pub(crate) fn full(&self, frame: u64) -> bool {
        self.next >= self.cap
            && self
                .free
                .front()
                .is_none_or(|&(_, at)| frame < at + REUSE_AFTER)
    }

    /// Title slots with a packed slot.
    pub(crate) fn mapped(&self) -> usize {
        self.map.len()
    }

    /// Packed views by stage: queued, failed, made, shown.
    pub(crate) fn stages(&self) -> [usize; 4] {
        let mut n = [0; 4];
        for o in self.owner.iter().flatten() {
            n[o.stage as usize] += 1;
        }
        n
    }

    /// The table entry of title slot `slot`, if the table covers it.
    pub(crate) fn entry(&self, slot: u32) -> Option<u32> {
        slot.checked_sub(self.entry_base)
            .filter(|&k| k < self.entry_len)
    }

    fn owner_at(&mut self, at: u32) -> Option<&mut Owner> {
        let k = at.checked_sub(self.base)?;
        self.owner.get_mut(k as usize)?.as_mut()
    }

    /// Title slot `slot` holds a record naming a live view (`key`: the
    /// record's words) or not (`None`), at `frame`; `again`: make the view
    /// again even if the record is the same.
    pub(crate) fn place(
        &mut self,
        slot: u32,
        key: Option<[u32; 3]>,
        again: bool,
        frame: u64,
    ) -> Placed {
        let k = match (key, self.map.get(&slot).copied()) {
            (Some(key), Some(k)) => {
                let Some(owner) = self.owner[k as usize].as_ref() else {
                    return Placed::Nothing;
                };
                if !again
                    && owner.key == key
                    && (owner.stage != Stage::Failed || frame < owner.retry_at)
                {
                    return Placed::Same;
                }
                k
            }
            (Some(_), None) => {
                let k = match self.free.front() {
                    Some(&(k, at)) if frame >= at + REUSE_AFTER => {
                        self.free.pop_front();
                        k
                    }
                    _ if self.next < self.cap => {
                        self.next += 1;
                        self.next - 1
                    }
                    _ => {
                        self.counts.overflow += 1;
                        return Placed::Overflow;
                    }
                };
                self.map.insert(slot, k);
                k
            }
            (None, Some(k)) => {
                self.map.remove(&slot);
                self.owner[k as usize] = None;
                self.free.push_back((k, frame));
                self.counts.dropped += 1;
                return Placed::Drop(self.base + k);
            }
            (None, None) => return Placed::Nothing,
        };
        self.generation += 1;
        let (touched, tries) = self.owner[k as usize]
            .filter(|o| o.title == slot)
            .map_or((0, 0), |o| (o.touched, o.tries));
        self.owner[k as usize] = key.map(|key| Owner {
            title: slot,
            key,
            generation: self.generation,
            stage: Stage::Queued,
            touched,
            armed: None,
            tries,
            retry_at: 0,
        });
        self.counts.queued += 1;
        Placed::Make {
            at: self.base + k,
            generation: self.generation,
        }
    }

    /// Make `generation` of host slot `at` ended: with a host resource
    /// (`ok`) or not. Whether it is still that slot's latest make.
    pub(crate) fn made(&mut self, at: u32, generation: u64, ok: bool) -> bool {
        let now = self.now;
        let current = match self.owner_at(at) {
            Some(owner) if owner.generation == generation && owner.stage == Stage::Queued => {
                owner.stage = if ok { Stage::Made } else { Stage::Failed };
                if ok {
                    owner.tries = 0;
                } else {
                    owner.tries = owner.tries.saturating_add(1);
                    owner.retry_at = retry_at(now, owner.tries);
                }
                true
            }
            _ => false,
        };
        if current {
            let k = at - self.base;
            if ok {
                self.made_queue.push_back((k, generation));
            } else {
                self.failed_queue.push_back((k, generation));
            }
        }
        if !current {
            self.counts.stale += 1;
        } else if ok {
            self.counts.made += 1;
        } else {
            self.counts.unready += 1;
        }
        current
    }

    fn due(&self, k: usize, stage: Stage) -> Option<Due> {
        let o = self.owner.get(k)?.as_ref().filter(|o| o.stage == stage)?;
        Some(Due {
            at: self.base + k as u32,
            title: o.title,
            key: o.key,
            generation: o.generation,
        })
    }

    /// Made views to ready, at most `most`, oldest first; those not readied
    /// go back with [`Packed::untake`].
    pub(crate) fn take_made(&mut self, most: usize) -> Vec<Due> {
        let mut out = Vec::new();
        while out.len() < most
            && let Some((k, generation)) = self.made_queue.pop_front()
        {
            out.extend(
                self.due(k as usize, Stage::Made)
                    .filter(|d| d.generation == generation),
            );
        }
        out
    }

    /// Made views [`Packed::take_made`] gave that were not readied: first
    /// again next time.
    pub(crate) fn untake(&mut self, dues: &[Due]) {
        for d in dues.iter().rev() {
            self.made_queue.push_front((d.at - self.base, d.generation));
        }
    }

    /// Made views waiting to be readied (some may be stale).
    pub(crate) fn made_backlog(&self) -> usize {
        self.made_queue.len()
    }

    /// Shown views to ready again, `most` of them from where the last look
    /// ended, in turn (looking at no more than 64 slots for each).
    pub(crate) fn take_shown(&mut self, most: usize) -> Vec<Due> {
        let n = self.owner.len();
        let mut out = Vec::new();
        for _ in 0..n.min(most.saturating_mul(64)) {
            if out.len() >= most {
                break;
            }
            let k = self.cursor;
            self.cursor = (self.cursor + 1) % n;
            out.extend(self.due(k, Stage::Shown));
        }
        out
    }

    /// `due` was readied on a list: shown now if it is still the latest
    /// make of its slot. The table entry to point at `due.at`, then.
    pub(crate) fn show(&mut self, due: &Due) -> Option<u32> {
        let entry = self.entry(due.title)?;
        let owner = self.owner_at(due.at)?;
        if owner.generation != due.generation || owner.stage != Stage::Made {
            return None;
        }
        owner.stage = Stage::Shown;
        self.counts.shown += 1;
        Some(entry)
    }

    /// Title slots whose make found no host resource, at most `most`,
    /// oldest first.
    fn failed(&mut self, most: usize) -> Vec<u32> {
        let mut out = Vec::new();
        for _ in 0..self.failed_queue.len() {
            if out.len() >= most {
                break;
            }
            let Some((k, generation)) = self.failed_queue.pop_front() else {
                break;
            };
            let Some(o) = self.owner[k as usize].filter(|o| {
                o.stage == Stage::Failed && o.generation == generation
            }) else {
                continue;
            };
            if self.now < o.retry_at {
                self.failed_queue.push_back((k, generation));
                continue;
            }
            out.push(o.title);
        }
        out
    }
}

static STATE: Mutex<Vec<Packed>> = Mutex::new(Vec::new());
/// Title slot ranges marked by writes, until [`refresh`] takes them.
static PENDING: Mutex<Vec<(u32, u32)>> = Mutex::new(Vec::new());
/// The most dynamic slots taken at once since the last line, and ever.
pub(crate) static PEAK_TAKEN: [AtomicU64; 2] = [AtomicU64::new(0), AtomicU64::new(0)];

/// Title slots `[lo, hi)` were written (`heap_cap::made_past`, `note_writes`).
pub(crate) fn dirty(lo: u32, hi: u32) {
    if lo < hi {
        PENDING
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((lo, hi));
    }
}

/// The `=3` table entries of `host`'s strides buffer: `REMAP_LEN + 1` dwords.
fn table(host: &super::HostHeap) -> Option<*mut u32> {
    let (_, _, data) = host.strides.as_ref()?;
    // SAFETY: the buffer is SLOT_STRIDES_COUNT dwords; REMAP_BASE + REMAP_LEN + 1 of them.
    Some(unsafe { (*data as *mut u32).add(victory_shader::emit::REMAP_BASE as usize) })
}

/// (`=4`) The whole-heap tables, by host heap: the buffer (held for the
/// lists that bind it) and its GPU address; its mapping
/// (`victory_shader::emit::FULL_REMAP` dwords) is the packed state's.
struct FullTable {
    id: u64,
    buffer: windows::Win32::Graphics::Direct3D12::ID3D12Resource,
    gpu: u64,
    /// (`=5`) The fault buffer, held as the table is.
    faults: Option<windows::Win32::Graphics::Direct3D12::ID3D12Resource>,
}

// SAFETY: a buffer and its mapping, written under the state's locks or once at birth.
unsafe impl Send for FullTable {}

static FULL_TABLES: Mutex<Vec<FullTable>> = Mutex::new(Vec::new());

/// A whole-heap table for `host`: dword `i` the host slot of title view
/// slot `i`: its own below `own` (all below `null` when `own` is 0), else
/// `title_null`; the last dword `title_null`.
fn full_table(
    host: &super::HostHeap,
    own: u32,
    title_null: u32,
    null: u32,
    faults: Option<(windows::Win32::Graphics::Direct3D12::ID3D12Resource, usize)>,
) -> Option<usize> {
    let gpu_host = crate::host::host()?;
    let dwords = victory_shader::emit::FULL_REMAP;
    // a heap that does not pack reads the same table as every other such heap: one for all
    // (Part S: 8 MB of upload memory per title view heap)
    static SHARED: Mutex<Option<(windows::Win32::Graphics::Direct3D12::ID3D12Resource, u64, usize)>> =
        Mutex::new(None);
    let shared = own == 0 && faults.is_none();
    if shared
        && let Some((buffer, gpu, data)) = SHARED.lock().unwrap_or_else(|e| e.into_inner()).clone()
    {
        let mut tables = FULL_TABLES.lock().unwrap_or_else(|e| e.into_inner());
        if tables.len() >= 16 {
            tables.remove(0);
        }
        tables.push(FullTable {
            id: host.id,
            buffer,
            gpu,
            faults: None,
        });
        return Some(data);
    }
    let (buffer, data) = gpu_host.upload_buffer(u64::from(dwords) * 4)?;
    crate::host::name(&buffer, || "heap cap: title view slot to host slot".to_owned());
    // SAFETY: a live buffer.
    let gpu = unsafe { buffer.GetGPUVirtualAddress() };
    let table = data.cast::<u32>();
    commit::held(u64::from(dwords) * 4);
    // (`=5`) slots past the own ones may be live: a shader that reads one says so
    let past = if faults.is_some() {
        title_null | victory_shader::emit::FAULT_FLAG
    } else {
        title_null
    };
    for i in 0..dwords {
        let slot = if own == 0 {
            i.min(null)
        } else if i < own {
            i
        } else {
            past
        };
        // SAFETY: inside the fresh mapping of `dwords` dwords.
        unsafe { table.add(i as usize).write(slot) };
    }
    // SAFETY: as above.
    unsafe { table.add(dwords as usize - 1).write(if own == 0 { null } else { title_null }) };
    let mut tables = FULL_TABLES.lock().unwrap_or_else(|e| e.into_inner());
    // the newest heaps (the title makes a few); a list binding an older one holds it
    if tables.len() >= 16 {
        tables.remove(0);
    }
    if shared {
        *SHARED.lock().unwrap_or_else(|e| e.into_inner()) =
            Some((buffer.clone(), gpu, table as usize));
    }
    tables.push(FullTable {
        id: host.id,
        buffer,
        gpu,
        faults: faults.map(|f| f.0),
    });
    Some(table as usize)
}

fn put(table: *mut u32, len: u32, entry: u32, value: u32) {
    if entry <= len {
        // SAFETY: inside the mapping; shaders read it while we write (a
        // dword, either value is one the entry had).
        unsafe { table.add(entry as usize).write_volatile(value) };
    }
}

/// A make or null of a packed slot, for `victory-heap-remap`.
struct Op {
    /// The host heap's, held so the slot's memory stays while queued.
    heap: ID3D12DescriptorHeap,
    id: u64,
    at: u32,
    cpu: D3D12_CPU_DESCRIPTOR_HANDLE,
    generation: u64,
    /// `None`: make the slot null.
    view: Option<View>,
}

/// What `victory-heap-remap` did with an [`Op`]: taken by [`refresh`].
struct Done {
    id: u64,
    at: u32,
    generation: u64,
    /// For a make: whether it found a host resource, the resource the slot
    /// now views (kept by the heap) and the element stride.
    made: Option<(bool, super::Kept, u32)>,
}

// SAFETY: descriptor heaps, resources and views are free-threaded in D3D12;
// a slot is written by one op at a time, in queue order.
unsafe impl Send for Op {}
// SAFETY: as for `Op`.
unsafe impl Send for Done {}

struct Worker {
    ops: Mutex<VecDeque<Op>>,
    cv: Condvar,
    done: Mutex<Vec<Done>>,
    us: AtomicU64,
}

fn worker() -> Option<&'static Worker> {
    static WORKER: std::sync::OnceLock<Option<&'static Worker>> = std::sync::OnceLock::new();
    *WORKER.get_or_init(|| {
        let w: &'static Worker = Box::leak(Box::new(Worker {
            ops: Mutex::new(VecDeque::new()),
            cv: Condvar::new(),
            done: Mutex::new(Vec::new()),
            us: AtomicU64::new(0),
        }));
        std::thread::Builder::new()
            .name("victory-heap-remap".into())
            .spawn(move || work(w))
            .map_err(|e| tracing::warn!(error = %e, "heap cap: no victory-heap-remap thread; packed views are made where they are found"))
            .ok()
            .map(|_| w)
    })
}

fn work(w: &'static Worker) {
    loop {
        let op = {
            let mut ops = w.ops.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if let Some(op) = ops.pop_front() {
                    break op;
                }
                ops = w.cv.wait(ops).unwrap_or_else(|e| e.into_inner());
            }
        };
        let started = std::time::Instant::now();
        let done = run(op);
        w.us.fetch_add(started.elapsed().as_micros() as u64, Relaxed);
        w.done.lock().unwrap_or_else(|e| e.into_inner()).push(done);
    }
}

/// Does `op`: the descriptor written, nothing else of the heap's touched.
fn run(op: Op) -> Done {
    // a texture whose host copy is not there: made only as the commit gate says
    let texture = match &op.view {
        Some(View::Shader(resource, _)) => Some(*resource as crate::object::This)
            .filter(|&r| !crate::resource::host_copy_made(r)),
        _ => None,
    };
    let gated = texture.is_some_and(|r| !commit::copy_allowed(r));
    let made = match &op.view {
        Some(view) if gated => {
            super::null_for(view, op.cpu);
            Some((false, None, super::view_stride(view).unwrap_or(0)))
        }
        Some(view) => Some(match super::make(view, op.cpu) {
            Some((kept, _)) => {
                if let Some(r) = texture
                    && crate::resource::host_copy_made(r)
                {
                    commit::made_copy(r);
                }
                (true, kept, super::view_stride(view).unwrap_or(0))
            }
            None => {
                super::null_for(view, op.cpu);
                (false, None, super::view_stride(view).unwrap_or(0))
            }
        }),
        None => {
            super::null_view(op.cpu);
            None
        }
    };
    drop(op.heap);
    Done {
        id: op.id,
        at: op.at,
        generation: op.generation,
        made,
    }
}

/// Queues `op` on `victory-heap-remap`, or does it now without the thread.
fn send(op: Op) {
    match worker() {
        Some(w) => {
            w.ops
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push_back(op);
            w.cv.notify_one();
        }
        None => {
            let done = run(op);
            DONE_HERE
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(done);
        }
    }
}

/// Results of ops done without the thread.
static DONE_HERE: Mutex<Vec<Done>> = Mutex::new(Vec::new());

/// The results of `host`'s ops: slots kept, strides written, stages moved.
fn take_done(packed: &mut Packed, host: &super::HostHeap) {
    let mut done = std::mem::take(&mut *DONE_HERE.lock().unwrap_or_else(|e| e.into_inner()));
    if let Some(w) = worker() {
        done.append(&mut w.done.lock().unwrap_or_else(|e| e.into_inner()));
        packed.counts.worker_us += w.us.swap(0, Relaxed);
    }
    let mut others = Vec::new();
    for d in done {
        if d.id != host.id {
            others.push(d);
            continue;
        }
        match d.made {
            Some((ok, kept, element)) => {
                if packed.made(d.at, d.generation, ok) {
                    host.keep(d.at, kept);
                    stride(host, d.at, element);
                }
            }
            None => {
                host.keep(d.at, None);
                stride(host, d.at, 0);
            }
        }
    }
    // another heap's: kept for its own refresh
    if !others.is_empty() {
        DONE_HERE
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .append(&mut others);
    }
}

/// A new view heap (`HostHeap::new`): with the switch, its table starts
/// with no packed view (`=3`: every entry null; `=4`: the host's own slots
/// their own, the rest null) and its packed slots free.
pub(super) fn born(host: &super::HostHeap) {
    if !super::heap_cap::remap_on() || host.sampler {
        return;
    }
    let title_null = super::heap_cap::title_null();
    if title_null == 0 {
        return;
    }
    let full = super::heap_cap::full_on();
    // the heap whose own slots end at the title null slot packs; others only get a table
    let packs = !full || host.count == title_null;
    let reserved = super::heap_cap::reserved_of(title_null, host.count, host.dynamic)
        .filter(|_| packs);
    let demand = super::heap_cap::demand_on();
    let null = victory_shader::emit::view_heap_limit();
    // (`=5`) every heap's fault slot views a buffer (the packing heap's) or nothing
    let faults = if demand {
        let fault = super::heap_cap::fault_slot_of(null);
        let buffer = reserved.and_then(|_| demand::fault_buffer());
        demand::fault_view(buffer.as_ref().map(|b| &b.0), host.cpu(fault));
        buffer
    } else {
        None
    };
    let fault_map = faults.as_ref().map_or(0, |f| f.1);
    let table = if full {
        let own = if reserved.is_some() { title_null } else { 0 };
        full_table(host, own, title_null, null, faults)
    } else {
        table(host).map(|table| {
            // a zero entry would read slot 0
            for entry in 0..=victory_shader::emit::REMAP_LEN {
                put(table, victory_shader::emit::REMAP_LEN, entry, title_null);
            }
            table as usize
        })
    };
    let Some(table) = table else {
        tracing::warn!("heap cap: no remap table; title view indices read null or their own slot");
        return;
    };
    let Some(at) = reserved else {
        return;
    };
    let cap = if full {
        packed_full(at, host.dynamic, super::heap_cap::dynamic_kept())
    } else {
        compact_of(compact_slots(), at, host.dynamic)
    };
    let (base, len) = if full {
        (0, victory_shader::emit::FULL_REMAP - 1)
    } else {
        (title_null, victory_shader::emit::REMAP_LEN)
    };
    tracing::info!(
        title_null,
        packed_first = title_null + 1,
        packed = cap,
        dynamic_left = host.dynamic - cap - 1,
        whole_heap_table = full,
        "heap cap: title view slots past the host heap get packed host slots {} to {}",
        title_null + 1,
        title_null + 1 + cap
    );
    let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());
    // the newest few heaps (the title makes one)
    if state.len() >= 4 {
        state.remove(0);
    }
    // its lists: an owner per packed slot
    commit::held(u64::from(cap) * size_of::<Option<Owner>>() as u64);
    let packed = Packed::new(host.id, title_null, cap).with_table(table, base, len);
    state.push(if demand && fault_map != 0 {
        packed.with_demand(fault_map)
    } else {
        packed
    });
}

/// The root SRV of the remap table for a list that draws with `host`'s
/// heap (`draw.rs`, `SLOT_REMAP`).
pub(crate) fn root(list: &crate::host::HostList, host: &super::HostHeap) -> Option<u64> {
    const ONCE: usize = usize::MAX - 0x5f;
    if super::heap_cap::full_on() {
        let tables = FULL_TABLES.lock().unwrap_or_else(|e| e.into_inner());
        let table = tables.iter().find(|t| t.id == host.id)?;
        if list.first_use(ONCE) {
            list.keep(&table.buffer);
            if let Some(faults) = &table.faults {
                list.keep(faults);
            }
        }
        return Some(table.gpu);
    }
    let gpu = host.strides_gpu()?;
    if list.first_use(ONCE)
        && let Some(buffer) = host.strides_buffer()
    {
        list.keep(buffer);
    }
    Some(gpu + u64::from(victory_shader::emit::REMAP_BASE) * 4)
}

/// Once a frame or more from `sync_heaps`, for the title view heap at
/// `start` of `title` slots: results of the ops taken, the marked title
/// slots past the host heap looked at, their packed views queued or let go.
pub(super) fn refresh(start: u64, step: u64, host: &super::HostHeap, title: u32) {
    if step != 32 {
        return;
    }
    let mut guard = STATE.lock().unwrap_or_else(|e| e.into_inner());
    let Some(packed) = guard.iter_mut().find(|p| p.id == host.id) else {
        return;
    };
    let frame = crate::frame::current();
    packed.now = frame;
    commit::new_frame();
    take_done(packed, host);
    if packed.demand {
        demand::step(packed, host, start, step, frame);
    }
    let end = title.min(packed.entry_base.saturating_add(packed.entry_len));
    if packed.title_null >= end {
        return;
    }
    let rescan = !packed.seeded || frame >= packed.last_rescan + RESCAN_EVERY;
    if rescan {
        if packed.seeded {
            line(packed, frame, title);
        }
        packed.dirty.push((packed.title_null, end, false));
        packed.seeded = true;
        packed.last_rescan = frame;
    }
    for (lo, hi) in std::mem::take(&mut *PENDING.lock().unwrap_or_else(|e| e.into_inner())) {
        let (lo, hi) = (lo.max(packed.title_null), hi.min(end));
        if lo < hi {
            packed.dirty.push((lo, hi, false));
        }
    }
    if frame.is_multiple_of(8) {
        for slot in packed.failed(RETRY_BUDGET) {
            packed.dirty.push((slot, slot + 1, false));
        }
    }
    if packed.dirty.is_empty() {
        return;
    }
    let (mut read, mut makes) = (0u32, 0u32);
    // one buffer for the run, not one a frame
    static BYTES: Mutex<Vec<u8>> = Mutex::new(Vec::new());
    let mut bytes = BYTES.lock().unwrap_or_else(|e| e.into_inner());
    if bytes.len() < 1 << 19 {
        bytes.resize(1 << 19, 0);
    }
    while let Some((lo, hi, again)) = packed.dirty.pop() {
        if read >= READ_BUDGET || makes >= MAKE_BUDGET {
            packed.dirty.push((lo, hi, again));
            break;
        }
        let n = (hi - lo)
            .min((bytes.len() / 32) as u32)
            .min(READ_BUDGET - read);
        if lo + n < hi {
            packed.dirty.push((lo + n, hi, again));
        }
        read += n;
        let buf = &mut bytes[..(n * 32) as usize];
        if !crate::host::read_into((start + u64::from(lo) * step) as *const u8, buf) {
            packed.counts.unread += u64::from(n);
            continue;
        }
        for k in 0..n {
            let slot = lo + k;
            let record = &buf[(k * 32) as usize..(k * 32 + 12) as usize];
            let word = |w: usize| {
                u32::from_le_bytes(record[w * 4..w * 4 + 4].try_into().unwrap_or_default())
            };
            let key = [word(0), word(1), word(2)];
            // (`=3`, `=4`) no packed slot free: a slot without one has nothing to do (Part S:
            // 28.3M overflowing looks by f1202, each a view lookup)
            if !packed.demand && packed.full(frame) && !packed.map.contains_key(&slot) {
                packed.counts.overflow += u64::from(key[0] == super::super::VIEW_TAG);
                continue;
            }
            let view = (key[0] == super::super::VIEW_TAG)
                .then(|| super::super::view_at(key[1], key[2]))
                .flatten();
            packed.counts.looked += 1;
            let Some(entry) = packed.entry(slot) else {
                continue;
            };
            // (`=5`) packed only when a shader asks: a slot with no packed view is
            // flagged while live, so the first read asks for it
            if packed.demand && !packed.map.contains_key(&slot) {
                packed.put(entry, packed.unpacked(view.is_some()));
                continue;
            }
            let placed = packed.place(slot, view.as_ref().map(|_| key), again, frame);
            if let Placed::Make { .. } = placed {
                makes += 1;
            }
            apply(packed, host, entry, placed, view);
        }
    }
}

/// Does what [`Packed::place`] said for title slot entry `entry` (`view`
/// its view): off the table first (shown again once made and readied), the
/// make or null queued; an overflow left unpacked (flagged under `=5`).
fn apply(packed: &Packed, host: &super::HostHeap, entry: u32, placed: Placed, view: Option<View>) {
    let (at, generation, view) = match placed {
        Placed::Make { at, generation } => (at, generation, view),
        Placed::Drop(at) => (at, 0, None),
        Placed::Overflow => {
            packed.put(entry, packed.unpacked(true));
            return;
        }
        Placed::Same | Placed::Nothing => return,
    };
    packed.put(entry, packed.title_null);
    send(Op {
        heap: host.heap.clone(),
        id: host.id,
        at,
        cpu: host.cpu(at),
        generation,
        view,
    });
}

/// Once a frame from `draw.rs` `heap_buffers`, with the list it records:
/// packed views made since are readied by `ready` (a texture's upload, a
/// buffer range's copy) and then shown in the table; some shown ones are
/// readied again. `ready` gives the bytes it staged.
pub(crate) fn ready_pass(host: &super::HostHeap, mut ready: impl FnMut(&View) -> u64) {
    if !super::heap_cap::remap_on() {
        return;
    }
    let started = std::time::Instant::now();
    let pressed = commit::pressed();
    let (made, shown, budget) = {
        let mut guard = STATE.lock().unwrap_or_else(|e| e.into_inner());
        let Some(packed) = guard.iter_mut().find(|p| p.id == host.id) else {
            return;
        };
        // no staging while commit is short: made views wait to be shown
        if pressed {
            packed.counts.ready_waits += 1;
            return;
        }
        let budget = ready_budget(packed.made_backlog());
        (
            packed.take_made(budget.0),
            packed.take_shown(RECHECK_BUDGET),
            budget,
        )
    };
    if made.is_empty() && shown.is_empty() {
        return;
    }
    let (mut bytes, mut readied, mut rechecked) = (0u64, Vec::new(), 0u64);
    let mut left = made.len();
    for (n, (due, again)) in made
        .iter()
        .map(|d| (d, false))
        .chain(shown.iter().map(|d| (d, true)))
        .enumerate()
    {
        if started.elapsed() >= budget.1 || bytes >= budget.2 {
            left = n.min(made.len());
            break;
        }
        if !again {
            left = made.len() - n - 1;
        }
        // a view let go since: the next look at its slot drops it
        let Some(view) = super::super::view_at(due.key[1], due.key[2]) else {
            continue;
        };
        bytes += ready(&view);
        if again {
            rechecked += 1;
        } else {
            readied.push(*due);
        }
    }
    let mut guard = STATE.lock().unwrap_or_else(|e| e.into_inner());
    let Some(packed) = guard.iter_mut().find(|p| p.id == host.id) else {
        return;
    };
    // not reached in time: first next time
    packed.untake(&made[made.len() - left..]);
    for due in &readied {
        if let Some(entry) = packed.show(due) {
            packed.put(entry, due.at);
        }
    }
    packed.counts.readied += readied.len() as u64;
    packed.counts.rechecked += rechecked;
    packed.counts.ready_bytes += bytes;
    let us = started.elapsed().as_micros() as u64;
    packed.counts.ready_us += us;
    packed.counts.ready_max_us = packed.counts.ready_max_us.max(us);
}

/// The element stride shaders read for host slot `slot` (the strides buffer).
fn stride(host: &super::HostHeap, slot: u32, value: u32) {
    if let Some((_, _, data)) = &host.strides
        && slot < victory_shader::emit::SLOT_STRIDES_COUNT
    {
        // SAFETY: inside the mapping; as `HostHeap` writes strides.
        unsafe { (*data as *mut u32).add(slot as usize).write_volatile(value) };
    }
}

fn line(packed: &mut Packed, frame: u64, title: u32) {
    let c = std::mem::take(&mut packed.counts);
    let [commit_mb, new_copies, new_copy_mb, new_copy_mb_total, gated, free_commit_mb] =
        commit::fields();
    let [queued, failed, made, shown] = packed.stages();
    tracing::warn!(
        frame,
        title_slots = title,
        title_null = packed.title_null,
        mapped = packed.mapped(),
        packed = packed.cap,
        now_queued = queued,
        now_failed = failed,
        now_made = made,
        now_shown = shown,
        queued_since = c.queued,
        made_since = c.made,
        unready = c.unready,
        stale = c.stale,
        readied = c.readied,
        shown_since = c.shown,
        rechecked = c.rechecked,
        ready_mb = c.ready_bytes >> 20,
        ready_ms = c.ready_us / 1000,
        ready_max_ms = c.ready_max_us / 1000,
        ready_waits = c.ready_waits,
        worker_ms = c.worker_us / 1000,
        dropped = c.dropped,
        demand = packed.demand,
        notes = c.notes,
        touched = c.touched,
        wanted = c.wanted,
        wanted_now = packed.wanted.len(),
        dead = c.dead,
        armed = c.armed,
        evicted = c.evicted,
        overflow = c.overflow,
        unread = c.unread,
        looked = c.looked,
        backlog = packed.dirty.len(),
        dynamic_peak = PEAK_TAKEN[0].swap(0, Relaxed),
        dynamic_peak_ever = PEAK_TAKEN[1].load(Relaxed),
        commit_mb,
        new_copies,
        new_copy_mb,
        new_copy_mb_total,
        gated,
        free_commit_mb,
        "heap cap: packed title views"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    const NULL_SLOT: u32 = 737_855;

    fn make_at(p: Placed) -> (u32, u64) {
        match p {
            Placed::Make { at, generation } => (at, generation),
            other => panic!("{other:?}"),
        }
    }

    /// The remap table as `refresh` and `ready_pass` keep it, for title
    /// slots from `NULL_SLOT` on: off the table at every make or drop,
    /// pointed at the packed slot only once shown.
    struct Table(Vec<u32>);

    impl Table {
        fn new() -> Self {
            Self(vec![NULL_SLOT; 4096])
        }
        fn placed(&mut self, p: &Packed, slot: u32, placed: Placed) -> Placed {
            if matches!(placed, Placed::Make { .. } | Placed::Drop(_)) {
                self.0[p.entry(slot).unwrap() as usize] = NULL_SLOT;
            }
            placed
        }
        fn ready(&mut self, p: &mut Packed) {
            for due in p.take_made(usize::MAX) {
                if let Some(entry) = p.show(&due) {
                    self.0[entry as usize] = due.at;
                }
            }
        }
        fn reads(&self, slot: u32) -> u32 {
            self.0[(slot - NULL_SLOT) as usize]
        }
    }

    #[test]
    fn every_live_title_slot_gets_its_own_packed_slot_and_dead_ones_none() {
        let mut p = Packed::new(1, NULL_SLOT, 1000);
        let live = |s: u32| Some([1, s, 1]);
        let mut seen = std::collections::HashSet::new();
        for s in (NULL_SLOT..NULL_SLOT + 2000).step_by(3) {
            match p.place(s, live(s), false, 0) {
                Placed::Make { at, generation } => {
                    assert!((NULL_SLOT + 1..NULL_SLOT + 1001).contains(&at), "{at}");
                    assert!(seen.insert(at), "host slot {at} given twice");
                    assert!(p.made(at, generation, true));
                }
                Placed::Overflow => assert!(seen.len() == 1000),
                other => panic!("{other:?}"),
            }
        }
        assert_eq!(p.mapped(), 667);
        // the same record again: nothing to do; a new one: made again in the same slot
        let s = NULL_SLOT + 3;
        assert_eq!(p.place(s, live(s), false, 1), Placed::Same);
        let (at, _) = make_at(p.place(s, Some([1, 9, 9]), false, 1));
        assert_eq!(at, p.base + 1);
        // dead: dropped, then nothing
        assert_eq!(p.place(s, None, false, 2), Placed::Drop(at));
        assert_eq!(p.place(s, None, false, 2), Placed::Nothing);
        assert_eq!(
            p.place(NULL_SLOT + 1, None, false, 2),
            Placed::Nothing,
            "never live"
        );
    }

    #[test]
    fn a_title_slot_reads_its_packed_view_only_once_made_and_readied() {
        let mut p = Packed::new(1, NULL_SLOT, 8);
        let mut t = Table::new();
        let s = NULL_SLOT + 40;
        let placed = p.place(s, Some([1, 7, 1]), false, 0);
        let (at, generation) = make_at(t.placed(&p, s, placed));
        assert_eq!(t.reads(s), NULL_SLOT, "queued: null");
        t.ready(&mut p);
        assert_eq!(t.reads(s), NULL_SLOT, "not made yet: nothing to ready");
        assert!(p.made(at, generation, true));
        assert_eq!(p.stages(), [0, 0, 1, 0]);
        t.ready(&mut p);
        assert_eq!(t.reads(s), at, "made and readied: its own view");
        assert_eq!(p.stages(), [0, 0, 0, 1]);
        // its record changes: off the table until the new view is made and readied
        let placed = p.place(s, Some([1, 8, 1]), false, 1);
        let (again, generation) = make_at(t.placed(&p, s, placed));
        assert_eq!(again, at);
        assert_eq!(t.reads(s), NULL_SLOT);
        assert!(p.made(at, generation, true));
        t.ready(&mut p);
        assert_eq!(t.reads(s), at);
        // let go: null
        let placed = p.place(s, None, false, 2);
        t.placed(&p, s, placed);
        assert_eq!(t.reads(s), NULL_SLOT);
        t.ready(&mut p);
        assert_eq!(t.reads(s), NULL_SLOT);
    }

    #[test]
    fn a_late_result_of_an_earlier_make_is_not_shown() {
        let mut p = Packed::new(1, NULL_SLOT, 8);
        let mut t = Table::new();
        let s = NULL_SLOT + 2;
        let placed = p.place(s, Some([1, 1, 1]), false, 0);
        let (at, first) = make_at(t.placed(&p, s, placed));
        let placed = p.place(s, Some([1, 2, 1]), false, 0);
        let (_, second) = make_at(t.placed(&p, s, placed));
        assert!(!p.made(at, first, true), "the first make's result is stale");
        assert_eq!(p.counts.stale, 1);
        t.ready(&mut p);
        assert_eq!(t.reads(s), NULL_SLOT);
        assert!(p.made(at, second, true));
        t.ready(&mut p);
        assert_eq!(t.reads(s), at);
        // a slot given back and taken by another title slot: the old due is not shown
        let due = Due {
            at,
            title: s,
            key: [1, 2, 1],
            generation: second,
        };
        let placed = p.place(s, None, false, 1);
        t.placed(&p, s, placed);
        let other = NULL_SLOT + 3;
        let placed = p.place(other, Some([1, 3, 1]), false, 1 + REUSE_AFTER);
        let (reused, _) = make_at(t.placed(&p, other, placed));
        assert_eq!(reused, at);
        assert_eq!(p.show(&due), None);
        assert_eq!(t.reads(s), NULL_SLOT);
        assert_eq!(t.reads(other), NULL_SLOT, "not made yet");
    }

    #[test]
    fn a_packed_slot_given_back_waits_before_reuse_and_overflow_reads_null() {
        let mut p = Packed::new(1, NULL_SLOT, 2);
        let (a, b, c) = (NULL_SLOT, NULL_SLOT + 10, NULL_SLOT + 20);
        assert_eq!(
            make_at(p.place(a, Some([1, 1, 1]), false, 0)).0,
            NULL_SLOT + 1
        );
        assert_eq!(
            make_at(p.place(b, Some([1, 2, 1]), false, 0)).0,
            NULL_SLOT + 2
        );
        assert_eq!(p.place(c, Some([1, 3, 1]), false, 0), Placed::Overflow);
        assert_eq!(p.counts.overflow, 1);
        assert_eq!(p.place(a, None, false, 10), Placed::Drop(NULL_SLOT + 1));
        assert_eq!(
            p.place(c, Some([1, 3, 1]), false, 11),
            Placed::Overflow,
            "not reused yet"
        );
        assert_eq!(
            make_at(p.place(c, Some([1, 3, 1]), false, 10 + REUSE_AFTER)).0,
            NULL_SLOT + 1
        );
    }

    #[test]
    fn a_view_not_made_is_tried_again_and_a_rescan_makes_all_again() {
        let mut p = Packed::new(1, NULL_SLOT, 8);
        let key = Some([1, 5, 1]);
        let (at, generation) = make_at(p.place(NULL_SLOT + 4, key, false, 0));
        assert_eq!(
            p.place(NULL_SLOT + 4, key, false, 0),
            Placed::Same,
            "queued"
        );
        assert!(p.made(at, generation, false));
        assert!(p.failed(10).is_empty(), "not due yet");
        assert_eq!(p.place(NULL_SLOT + 4, key, false, 1), Placed::Same, "a look before it is due");
        p.now = retry_at(0, 1);
        assert_eq!(p.failed(10), [NULL_SLOT + 4]);
        assert!(p.take_made(10).is_empty(), "nothing to show");
        let (again, generation) = make_at(p.place(NULL_SLOT + 4, key, false, p.now));
        assert_eq!(again, at, "not made: again");
        assert!(p.made(at, generation, true));
        assert!(p.failed(10).is_empty());
        assert_eq!(p.place(NULL_SLOT + 4, key, false, 2), Placed::Same);
        assert_eq!(
            make_at(p.place(NULL_SLOT + 4, key, true, 2)).0,
            at,
            "a rescan"
        );
    }

    #[test]
    fn shown_views_are_readied_again_in_turn() {
        let mut p = Packed::new(1, NULL_SLOT, 6);
        for s in 0..5 {
            let (at, generation) = make_at(p.place(NULL_SLOT + s, Some([1, s, 1]), false, 0));
            p.made(at, generation, true);
        }
        for due in p.take_made(usize::MAX) {
            p.show(&due);
        }
        let first: Vec<u32> = p.take_shown(3).iter().map(|d| d.title).collect();
        let next: Vec<u32> = p.take_shown(3).iter().map(|d| d.title).collect();
        assert_eq!(first, [NULL_SLOT, NULL_SLOT + 1, NULL_SLOT + 2]);
        assert_eq!(next, [NULL_SLOT + 3, NULL_SLOT + 4, NULL_SLOT]);
    }

    #[test]
    fn the_whole_heap_table_names_every_title_slot_and_starts_identity_then_null() {
        // (`=4`) entries from title slot 0
        let full = victory_shader::emit::FULL_REMAP;
        let p = Packed::new(1, 500_000, 8).with_table(0, 0, full - 1);
        assert_eq!(p.entry(5), Some(5));
        assert_eq!(p.entry(500_000), Some(500_000));
        assert_eq!(p.entry(1_245_183), Some(1_245_183));
        assert_eq!(p.entry(full - 1), None);
        // the table a heap starts with: its own slots their own, the rest null
        let Some(heap) = super::super::HostHeap::new(false, 8) else {
            return; // no host GPU
        };
        let null = 999_999;
        let check = |own: u32, title_null: u32, want: &dyn Fn(u32) -> u32| {
            let table = full_table(&heap, own, title_null, null, None).expect("a table") as *const u32;
            for i in [0, 1, 7, 8, 499_999, 500_000, 999_999, 1_000_000, 1_245_183, full - 2, full - 1] {
                // SAFETY: inside the table's mapping.
                let got = unsafe { table.add(i as usize).read() };
                assert_eq!(got, want(i), "title slot {i}");
            }
        };
        check(500_000, 500_000, &|i| if i < 500_000 && i != full - 1 { i } else { 500_000 });
        // a heap that does not pack: as before the table, its index up to the null slot
        check(0, 500_000, &|i| if i == full - 1 { null } else { i.min(null) });
    }

    #[test]
    fn made_views_are_readied_oldest_first_and_those_not_reached_come_first_again() {
        let mut p = Packed::new(1, NULL_SLOT, 8);
        for s in 0..4 {
            let (at, generation) = make_at(p.place(NULL_SLOT + s, Some([1, s, 1]), false, 0));
            assert!(p.made(at, generation, true));
        }
        let got = p.take_made(3);
        assert_eq!(got.iter().map(|d| d.title - NULL_SLOT).collect::<Vec<_>>(), [0, 1, 2]);
        // only the first was readied in time
        p.untake(&got[1..]);
        assert!(p.show(&got[0]).is_some());
        assert_eq!(p.made_backlog(), 3);
        let next = p.take_made(10);
        assert_eq!(next.iter().map(|d| d.title - NULL_SLOT).collect::<Vec<_>>(), [1, 2, 3]);
        // a remake since makes the queued one stale: not given
        let (at, generation) = make_at(p.place(NULL_SLOT + 1, Some([1, 9, 1]), false, 1));
        p.untake(&next);
        let again = p.take_made(10);
        assert_eq!(again.iter().map(|d| d.title - NULL_SLOT).collect::<Vec<_>>(), [2, 3]);
        assert!(p.made(at, generation, false));
        p.now = 100;
        assert_eq!(p.failed(10), [NULL_SLOT + 1]);
        assert!(p.failed(10).is_empty(), "taken once");
    }

    #[test]
    fn a_failed_make_waits_longer_each_time_and_a_success_starts_over() {
        assert_eq!(retry_at(100, 1), 108);
        assert_eq!(retry_at(100, 2), 116);
        assert_eq!(retry_at(100, 8), 100 + 1024);
        assert_eq!(retry_at(100, 200), 100 + 1024, "at most 1,024 frames");
        let mut p = Packed::new(1, NULL_SLOT, 8);
        let key = Some([1, 5, 1]);
        let slot = NULL_SLOT + 4;
        let mut tries = 0;
        // Part S: 1.59M failed makes by f902; now a slot that keeps failing is made 9 times in
        // 3,000 frames, not at every look
        for frame in 0..3000u64 {
            p.now = frame;
            if let Placed::Make { at, generation } = p.place(slot, key, false, frame) {
                tries += 1;
                assert!(p.made(at, generation, false));
            }
        }
        assert!(tries <= 12, "{tries} makes");
        p.now = 5000;
        let (at, generation) = make_at(p.place(slot, key, false, 5000));
        assert!(p.made(at, generation, true));
        p.now = 5001;
        let (at, generation) = make_at(p.place(slot, Some([1, 6, 1]), false, 5001));
        assert!(p.made(at, generation, false));
        assert_eq!(p.place(slot, Some([1, 6, 1]), false, 5001 + 7), Placed::Same);
        assert!(matches!(p.place(slot, Some([1, 6, 1]), false, 5001 + 8), Placed::Make { .. }), "the wait starts over");
    }

    #[test]
    fn table_entries_cover_title_slots_from_the_null_slot() {
        let p = Packed::new(1, NULL_SLOT, 8);
        assert_eq!(p.entry(NULL_SLOT - 1), None);
        assert_eq!(p.entry(NULL_SLOT), Some(0));
        assert_eq!(p.entry(1_245_183), Some(1_245_183 - NULL_SLOT));
        assert_eq!(p.entry(NULL_SLOT + victory_shader::emit::REMAP_LEN), None);
        // the retail title fits the table
        assert!(1_245_184 - NULL_SLOT <= victory_shader::emit::REMAP_LEN);
    }

    #[test]
    fn packed_slots_leave_half_the_dynamic_slots() {
        assert_eq!(compact_of(1 << 16, 0, 1 << 18), 1 << 16);
        assert_eq!(compact_of(1 << 17, 0, 1 << 18), (1 << 17) - 1);
        assert_eq!(compact_of(1 << 16, 200_000, 1 << 18), 0);
    }
}
