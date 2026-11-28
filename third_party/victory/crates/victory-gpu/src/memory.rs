//! Title memory by address, for translated shaders that read and write
//! through raw 40-bit addresses (`victory_shader::emit::SLOT_MEMORY`: the
//! console's ray-tracing structure builder and the programs that walk its
//! structures; the structures themselves lie in plain title memory, in no
//! buffer object).
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
//! there are at most [`COPIES`] of them (4 MiB windows mostly: 4 GiB at
//! most). Every dispatch of such a shader brings all numbered copies up to
//! date and, if it can store, declares all of them written (the bytes are
//! compared on the CPU afterwards, as for any unordered access view): a
//! dirty-page mask written by the shader would bound both.
//! `VICTORY_BVH_DECLARE_OUTPUTS=1` (`crate::bvh_declare`) declares written
//! only the builds' outputs and what writing draws' root arguments name.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use victory_shader::emit::{MEMORY_FAULTS, MEMORY_PAGES};
use windows::Win32::Graphics::Direct3D12::*;

use crate::descriptor::host::HostHeap;
use crate::host::{HostList, host};
use crate::shadow::Shadow;

/// Copies that can be numbered (the run of heap slots is one longer). A
/// listed frame of the opening cutscene names about 50 windows.
const COPIES: u32 = 1023;
/// A window of memory outside buffers (`shadow.rs`).
const CELL: u64 = 4 << 20;
/// A directory page.
const PAGE: u64 = 1 << 16;

struct Memory {
    /// Buffer, its mapping, its GPU address.
    directory: (ID3D12Resource, *mut u32, u64),
    faults: (ID3D12Resource, *mut u32, u64),
    /// Copy `n` is `copies[n - 1]`.
    copies: Vec<Arc<Shadow>>,
    /// Per host view heap (by address): the first slot of its run, and how
    /// many copies have their view there.
    heaps: HashMap<usize, (u32, u32)>,
    /// The frame the faults were last read in.
    frame: u64,
    /// Pages asked for this frame that are not the title's memory.
    refused: std::collections::HashSet<u64>,
    /// The first copy numbered by [`premap`] since the last [`ready`]
    /// (`usize::MAX`: none): the next draw brings it up to date.
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
        tracing::info!("title memory by address: a shader reads or writes through raw addresses");
        Some(Memory {
            directory: make(u64::from(MEMORY_PAGES) * 2, "directory")?,
            faults: make(u64::from(MEMORY_FAULTS), "faults")?,
            copies: Vec::new(),
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
                crate::shadow::REBAR[7].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            return;
        }
        let n = match self.copies.iter().position(|c| Arc::ptr_eq(c, &shadow)) {
            Some(n) => n,
            None if self.copies.len() < COPIES as usize => {
                self.copies.push(shadow.clone());
                let n = self.copies.len() - 1;
                let (s, l) = shadow.span();
                let written = crate::bvh_declare::on() && crate::bvh_declare::cells().span_written(s, l);
                self.generation += u64::from(written);
                self.written_at.push(if written { self.generation } else { 0 });
                // Every page the copy holds whole, not only the one asked
                // for: a structure is more than 64 KiB.
                // shortcut: a page of a title buffer inside this one that
                // has a copy of its own is given this copy too.
                let (first, len) = shadow.span();
                let mut at = first.next_multiple_of(PAGE);
                while at + PAGE <= first + len && at / PAGE < u64::from(MEMORY_PAGES) - 1 {
                    // SAFETY: inside the directory's mapping.
                    let other = unsafe { self.directory.1.add((at / PAGE) as usize * 2) };
                    // SAFETY: as above; the offset first, as below.
                    unsafe {
                        if other.read() == 0 {
                            other.add(1).write_volatile((at - first) as u32);
                            other.write_volatile(n as u32 + 1);
                        }
                    }
                    at += PAGE;
                }
                n
            }
            None => {
                static ONCE: std::sync::Once = std::sync::Once::new();
                ONCE.call_once(|| {
                    tracing::warn!(COPIES, "title memory by address: no number left for another copy; its pages read zeros")
                });
                return;
            }
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
            // Each is an access that read 0 or was dropped.
            static SAID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            if SAID.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 50 {
                tracing::warn!(
                    pages = pages.len(),
                    first = format_args!("{:#x}", u64::from(pages[0]) * PAGE),
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
    let key = std::ptr::from_ref(heap) as usize;
    let (base, made) = match m.heaps.get(&key) {
        Some(&run) => run,
        None => {
            // Held for the heap's life: not given back with a list's slots.
            let base = heap.dynamic(COPIES + 1)?;
            heap.null_raw_uav_at(base);
            (base, 0)
        }
    };
    for (n, shadow) in m.copies.iter().enumerate().skip(made as usize) {
        let (_, len) = shadow.span();
        heap.raw_uav_at(base + 1 + n as u32, &shadow.buffer, 0, len);
    }
    m.heaps.insert(key, (base, m.copies.len() as u32));
    // Once per list, and for copies numbered since: the builder alone
    // dispatches hundreds of times a frame.
    // shortcut: a copy another list numbers meanwhile is brought up to date
    // by that list's submission, which may run after this one's.
    const READ: usize = usize::MAX - 0x6d;
    const WRITE: usize = usize::MAX - 0x6e;
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
    for (n, shadow) in m.copies.iter().enumerate() {
        let (start, len) = shadow.span();
        if first_read || n >= numbered {
            shadow.refresh(list, start, len);
        }
        if writes && !outputs && (first_write || n >= numbered) {
            shadow.gpu_writes(list, start, len);
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
static DECLARED: [std::sync::atomic::AtomicU64; 5] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 5];

/// A writing list's first draw: what it declares written against what is
/// numbered.
fn count(m: &Memory) {
    use std::sync::atomic::Ordering::Relaxed;
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
        total.fetch_add(add, Relaxed);
    }
}

/// The declare summary since the last one.
fn say(m: &Memory) {
    use std::sync::atomic::Ordering::Relaxed;
    let d: Vec<u64> = DECLARED.iter().map(|c| c.swap(0, Relaxed)).collect();
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
        no_card[0].load(Relaxed),
        no_card[1].load(Relaxed) >> 20,
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
