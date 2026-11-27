//! `xmem.dll`: console memory, served from host memory. The GPU side works
//! from CPU addresses (unified memory), so memory types and bandwidth
//! classes change nothing here; what constrains addresses (alignment,
//! 2 MiB pages, address-bit limits) is honoured.
#![allow(non_snake_case)]
// Title code calls these through its import table, passing pointers the real
// API would trust.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::alloc::{Layout, alloc, dealloc};
use std::collections::{BTreeMap, HashMap};
use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_COMMITMENT_LIMIT, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, MEM_ADDRESS_REQUIREMENTS, MEM_COMMIT, MEM_EXTENDED_PARAMETER,
    MEM_PRESERVE_PLACEHOLDER, MEM_RELEASE, MEM_REPLACE_PLACEHOLDER, MEM_RESERVE,
    MEM_RESERVE_PLACEHOLDER, MEMORY_BASIC_INFORMATION, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile3,
    MemExtendedParameterAddressRequirements, PAGE_NOACCESS, PAGE_READWRITE, SEC_COMMIT,
    SEC_RESERVE, UnmapViewOfFile2, VirtualAlloc, VirtualAlloc2, VirtualFree, VirtualQueryEx,
};
use windows_sys::Win32::System::ProcessStatus::{
    K32GetPerformanceInfo, K32GetProcessMemoryInfo, PERFORMANCE_INFORMATION,
    PROCESS_MEMORY_COUNTERS,
};
use windows_sys::Win32::System::Threading::GetCurrentProcess;
use windows_sys::core::BOOL;

use crate::{HleModule, hle_export};

mod probe;
mod section_mb;
mod side_by_side;

const MIB: usize = 1 << 20;

/// Whether we run under Wine (its ntdll exports `wine_get_version`).
pub(crate) fn on_wine() -> bool {
    static WINE: LazyLock<bool> = LazyLock::new(|| {
        use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};
        // SAFETY: plain lookups in ntdll, always loaded.
        unsafe {
            let ntdll = GetModuleHandleA(c"ntdll.dll".as_ptr().cast());
            !ntdll.is_null() && GetProcAddress(ntdll, c"wine_get_version".as_ptr().cast()).is_some()
        }
    });
    *WINE
}

/// Standard memory the title has handed to the system.
static TRANSFERRED: AtomicUsize = AtomicUsize::new(0);

// Console additions to the host's flags.
/// 2 MiB pages; the host reads this bit as large pages, which need a
/// privilege.
const PAGES_2M: u32 = 0x2000_0000;
/// With `PAGES_2M`: 64 KiB pages; the host reads it as AWE memory.
const PAGES_64K_BIT: u32 = 0x40_0000;
const PAGES_64K: u32 = PAGES_2M | PAGES_64K_BIT;
/// The host's protection constants and `PAGE_GUARD`. Above them the console
/// adds GPU access bits (0x800-0x40000). Write-combining and uncached are
/// dropped too: our GPU side reads this memory through the CPU.
const HOST_PROTECTION: u32 = 0x1ff;
/// XMem flags of memory both processors reach: all of ours.
const GRAPHICS: u64 = 3;
/// XMem flag bit of 32-bit graphics memory.
const ADDRESS_32BIT: u64 = 4;
/// The host's write watching (not in windows-sys' memory module).
const MEM_WRITE_WATCH: u32 = 0x20_0000;

/// The addresses XMem `flags` allow, first and last: the low 4 GiB for
/// 32-bit memory; else from 4 GiB (kept for 32-bit requests, which the
/// title's big pools would otherwise crowd out) up to 46 down to 40 bits
/// from the field at bit 12, or 40 bits without one: the console keeps
/// title memory there, and its ray-tracing structures must be (
/// the title's BVH builder refuses one above; the title asks for its GPU heap
/// with no field). Decision 0001.
fn address_range(flags: u64) -> (usize, usize) {
    let bits = match (flags & ADDRESS_32BIT, flags >> 12 & 7) {
        (0, 0) => 40,
        (0, field) => 47 - field,
        _ => return (0, u32::MAX as usize),
    };
    (1 << 32, (1 << bits) - 1)
}

pub extern "system" fn XMemVirtualAlloc(
    base: *mut c_void,
    size: usize,
    kind: u32,
    flags: u64,
    protect: u32,
) -> *mut c_void {
    let (lowest, highest) = address_range(flags);
    let asked_protect = protect;
    let mut requirements = MEM_ADDRESS_REQUIREMENTS {
        LowestStartingAddress: lowest as *mut c_void,
        HighestEndingAddress: highest as *mut c_void,
        Alignment: if kind & (PAGES_2M | PAGES_64K_BIT) == PAGES_2M {
            2 * MIB
        } else {
            0
        },
    };
    let mut protect = match protect & HOST_PROTECTION {
        0 => PAGE_NOACCESS,
        p => p,
    };
    // A reservation of 64 KiB pages takes physical pages later
    // (`XMemMapPhysicalPages`); on the host only a placeholder can.
    // shortcut: committing part of such a reservation later would fail
    // (the title's 23 calls never do); replace that part of the
    // placeholder then.
    let placeholder = kind & PAGES_64K == PAGES_64K && kind & MEM_COMMIT == 0;
    if placeholder {
        if kind & MEM_REPLACE_PLACEHOLDER != 0 {
            // Part of a placeholder the title split: it stays one.
            return base;
        }
        protect = PAGE_NOACCESS;
    }
    let mut kind = kind & !(PAGES_2M | PAGES_64K_BIT);
    if placeholder {
        kind |= MEM_RESERVE | MEM_RESERVE_PLACEHOLDER;
    }
    let mut parameter = MEM_EXTENDED_PARAMETER::default();
    parameter.Anonymous1._bitfield = MemExtendedParameterAddressRequirements as u64;
    parameter.Anonymous2.Pointer = (&raw mut requirements).cast();
    // Address requirements apply to reservations at an address of our choice.
    let constrained = base.is_null();
    if constrained {
        kind |= MEM_RESERVE;
        // The GPU side copies only the pages the title wrote since its last
        // copy (`victory_gpu`, decision 0007); placeholders cannot be
        // watched (their physical-page views are copied whole).
        if kind & MEM_RESERVE_PLACEHOLDER == 0 {
            kind |= MEM_WRITE_WATCH;
        }
    }
    // SAFETY: the title's own arguments, cleaned of console bits; the
    // parameter points at `requirements`, which outlives the call.
    let at = unsafe {
        VirtualAlloc2(
            GetCurrentProcess(),
            base,
            size,
            kind,
            protect,
            if constrained {
                &mut parameter
            } else {
                std::ptr::null_mut()
            },
            constrained.into(),
        )
    };
    if base.is_null() && !at.is_null() {
        victory_gpu::title_memory_reserved(at as usize, size, asked_protect);
    }
    if base.is_null() && !at.is_null() && kind & MEM_RESERVE_PLACEHOLDER == 0 {
        victory_gpu::titlemem::title_range(at as usize, size, victory_gpu::titlemem::VIRTUAL);
    }
    // (Grand Theft Auto VI asks for its 32-bit heap with kind 0x42000,
    // without the 64 KiB page bits: not `placeholder` above, though the host
    // makes a placeholder of it all the same.)
    if !at.is_null() && constrained && kind & MEM_COMMIT == 0 && flags & ADDRESS_32BIT != 0 {
        LOW_RESERVATIONS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((at as usize, size));
    }
    if base.is_null() {
        tracing::debug!(
            size,
            kind = format_args!("{kind:#x}"),
            flags = format_args!("{flags:#x}"),
            at = format_args!("{at:p}"),
            "XMemVirtualAlloc"
        );
    }
    at
}

/// `MEMORY_BASIC_INFORMATION` (the console's first 0x30 bytes are the
/// same), then the XMem flags and tag.
#[repr(C)]
pub struct BasicInformation {
    host: MEMORY_BASIC_INFORMATION,
    flags: u64,
    tag_code: u32,
    tag: [u8; 4],
}

// shortcut: every committed page reports graphics memory and no tag, since
// all of it is GPU-reachable here (the title's BVH builder, the
// only caller, wants graphics memory); remember the flags of
// `XMemVirtualAlloc` regions if a title tells CPU-only memory apart.
pub extern "system" fn XMemVirtualQuery(
    process: HANDLE,
    address: *const c_void,
    out: *mut BasicInformation,
) -> BOOL {
    let size = size_of::<MEMORY_BASIC_INFORMATION>();
    // SAFETY: the title passes room for the whole structure; the host
    // fails on a null one.
    unsafe {
        if VirtualQueryEx(process, address, out.cast(), size) == 0 {
            return 0;
        }
        let info = &mut *out;
        info.flags = if info.host.State == MEM_COMMIT {
            GRAPHICS
        } else {
            0
        };
        info.tag_code = 0;
        info.tag = [0; 4];
    }
    1
}

// `XMemAlloc` attributes: alignment as log2 (5 bits at 24, 4 bytes and up
// from 2), then the memory type, then bit 33: a 32-bit address.
const ALIGNMENT_SHIFT: u64 = 24;
const ALLOC_32BIT: u64 = 1 << 33;
/// Our block header: the block's size and alignment, just below the
/// pointer handed out.
const HEADER: usize = 2 * size_of::<usize>();

fn layout(size: usize, attributes: u64) -> Option<Layout> {
    // An alignment of at least 16 leaves room for the header.
    let align = 1usize << (attributes >> ALIGNMENT_SHIFT & 0x1f).max(4);
    Layout::from_size_align(size.checked_add(align)?, align).ok()
}

type AllocRoutine = extern "system" fn(usize, u64) -> *mut c_void;
type FreeRoutine = extern "system" fn(*mut c_void, u64);

/// The allocation and free routines the title's exe exports as
/// `__XMemAllocationHooks__` (the title does): `XMemAlloc` and
/// `XMemFree` call them, for the title's own calls and its middleware's;
/// the `…Default` exports do not.
fn hooks() -> [usize; 2] {
    static AT: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    let at = match AT.get() {
        Some(&at) => at,
        // Looked up once the title is mapped, which is before it runs.
        None => match crate::runtime::main_image() {
            Some(base) => *AT.get_or_init(|| {
                use windows_sys::Win32::System::LibraryLoader::GetProcAddress;
                // SAFETY: a mapped image and a NUL-terminated name.
                let at = unsafe {
                    GetProcAddress(base as _, c"__XMemAllocationHooks__".as_ptr().cast())
                }
                .map_or(0, |f| f as usize);
                tracing::info!(found = at != 0, "xmem allocation hooks");
                at
            }),
            None => 0,
        },
    };
    if at == 0 {
        return [0; 2];
    }
    // SAFETY: the export is two routine pointers.
    unsafe { (at as *const [usize; 2]).read() }
}

/// The title's general allocator, for middleware too: its hook if it has
/// one, else [`XMemAllocDefault`].
pub extern "system" fn XMemAlloc(size: usize, attributes: u64) -> *mut c_void {
    match hooks()[0] {
        0 => XMemAllocDefault(size, attributes),
        hook => {
            // SAFETY: the title's own routine of this type.
            let alloc: AllocRoutine = unsafe { std::mem::transmute(hook) };
            alloc(size, attributes)
        }
    }
}

/// See [`XMemAlloc`].
pub extern "system" fn XMemFree(address: *mut c_void, attributes: u64) {
    match hooks()[1] {
        0 => XMemFreeDefault(address, attributes),
        hook => {
            // SAFETY: the title's own routine of this type.
            let free: FreeRoutine = unsafe { std::mem::transmute(hook) };
            free(address, attributes)
        }
    }
}

/// Smallest `XMemAlloc` block the resident line lists.
const HEAP_LISTED: usize = 256 << 10;

/// The system allocator behind [`XMemAlloc`].
pub extern "system" fn XMemAllocDefault(size: usize, attributes: u64) -> *mut c_void {
    if attributes & ALLOC_32BIT != 0 {
        return XMemVirtualAlloc(
            std::ptr::null_mut(),
            size,
            MEM_RESERVE | MEM_COMMIT,
            GRAPHICS | ADDRESS_32BIT,
            PAGE_READWRITE,
        );
    }
    let Some(layout) = layout(size, attributes) else {
        return std::ptr::null_mut();
    };
    // shortcut: the host heap's addresses lie above 2^40, unlike the
    // console's; nothing seen builds ray-tracing structures in this memory
    // (the title uses its GPU heap). Sub-allocate from a
    // reservation below 2^40 if one does.
    // SAFETY: the layout has a non-zero size.
    let block = unsafe { alloc(layout) };
    if block.is_null() {
        return block.cast();
    }
    // Only blocks that get their own pages: small ones are many and live in the host heap.
    if layout.size() >= HEAP_LISTED {
        victory_gpu::titlemem::title_range(
            block as usize,
            layout.size(),
            victory_gpu::titlemem::HEAP,
        );
    }
    // SAFETY: the block holds the alignment (at least the header) and then
    // `size` bytes.
    unsafe {
        let user = block.add(layout.align());
        user.sub(HEADER)
            .cast::<[usize; 2]>()
            .write([layout.size(), layout.align()]);
        user.cast()
    }
}

/// Frees what [`XMemAllocDefault`] returned, given the same attributes.
pub extern "system" fn XMemFreeDefault(address: *mut c_void, attributes: u64) {
    if address.is_null() {
        return;
    }
    // SAFETY: the address came from `XMemAlloc` with these attributes: a
    // whole reservation, or a header-prefixed block.
    unsafe {
        if attributes & ALLOC_32BIT != 0 {
            victory_gpu::titlemem::title_range_free(address as usize);
            VirtualFree(address, 0, MEM_RELEASE);
            return;
        }
        let user = address.cast::<u8>();
        let [size, align] = user.sub(HEADER).cast::<[usize; 2]>().read();
        if size >= HEAP_LISTED {
            victory_gpu::titlemem::title_range_free(user as usize - align);
        }
        dealloc(
            user.sub(align),
            Layout::from_size_align_unchecked(size, align),
        );
    }
}

/// A retail console has no tools memory.
pub extern "system" fn XMemIsToolsMemAvailable() -> BOOL {
    0
}

// Physical pages: on the console, 64 KiB frames a title maps into its own
// reservations, at more than one address if it likes. Here each allocation
// is a pagefile-backed section, its pages are 64 KiB slices of it, and a
// mapping is a view of consecutive slices in place of a host placeholder.

const PAGE: usize = 64 << 10;

/// Whether [`View::map`] reuses a lazy persistent whole-section RW host alias
/// view (`VICTORY_XMEM_PERSISTENT_VIEW=0` or `false` disables it and falls
/// back to per-call `MapViewOfFile` / `UnmapViewOfFile`).
fn persistent_view_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        !std::env::var("VICTORY_XMEM_PERSISTENT_VIEW")
            .ok()
            .is_some_and(|v| {
                let v = v.trim();
                v == "0" || v.eq_ignore_ascii_case("false")
            })
    })
}

struct Section {
    handle: HANDLE,
    pages: usize,
    /// One bit per 64 KiB page: set once the slice has been mapped or written.
    touched: Vec<AtomicU64>,
    /// One bit per 64 KiB page: set once committed via `VirtualAlloc(MEM_COMMIT)`
    /// through `host_view`. Separate from `touched` because `runs(..., true)`
    /// marks `touched` before `View::map` runs.
    committed: Vec<AtomicU64>,
    /// Lazy persistent whole-section RW host alias view (`MapViewOfFile` base
    /// address as `usize` so `Section` stays `Send + Sync`, or `None` if
    /// mapping failed).
    host_view: std::sync::OnceLock<Option<usize>>,
}

impl Section {
    fn new(handle: HANDLE, pages: usize) -> Self {
        let words = pages.div_ceil(64);
        let touched = (0..words).map(|_| AtomicU64::new(0)).collect();
        let committed = (0..words).map(|_| AtomicU64::new(0)).collect();
        Self {
            handle,
            pages,
            touched,
            committed,
            host_view: std::sync::OnceLock::new(),
        }
    }

    fn mark_touched(&self, start: usize, n: usize) {
        for index in start..start.saturating_add(n).min(self.pages) {
            self.touched[index / 64].fetch_or(1u64 << (index % 64), Ordering::Release);
        }
    }

    fn is_fresh(&self, index: usize) -> bool {
        index < self.pages
            && (self.touched[index / 64].load(Ordering::Acquire) >> (index % 64)) & 1 == 0
    }

    fn all_committed(&self, start: usize, n: usize) -> bool {
        let end = start.saturating_add(n);
        if end > self.pages {
            return false;
        }
        let mut idx = start;
        while idx < end {
            let word_idx = idx / 64;
            let bit_lo = idx % 64;
            let count = (64 - bit_lo).min(end - idx);
            let mask = if count == 64 {
                u64::MAX
            } else {
                ((1u64 << count) - 1) << bit_lo
            };
            if self.committed[word_idx].load(Ordering::Acquire) & mask != mask {
                return false;
            }
            idx += count;
        }
        true
    }

    fn mark_committed(&self, start: usize, n: usize) {
        let end = start.saturating_add(n).min(self.pages);
        let mut idx = start;
        while idx < end {
            let word_idx = idx / 64;
            let bit_lo = idx % 64;
            let count = (64 - bit_lo).min(end - idx);
            let mask = if count == 64 {
                u64::MAX
            } else {
                ((1u64 << count) - 1) << bit_lo
            };
            self.committed[word_idx].fetch_or(mask, Ordering::Release);
            idx += count;
        }
    }

    /// Lazily maps the entire section once as a persistent RW host alias view.
    /// `SEC_RESERVE` consumes zero physical RAM / commit charge when mapped;
    /// pages are committed on demand via `VirtualAlloc(MEM_COMMIT)`.
    fn persistent_base(&self) -> Option<*mut u8> {
        let base = *self.host_view.get_or_init(|| {
            use windows_sys::Win32::System::Memory::{FILE_MAP_WRITE, MapViewOfFile};
            let len = self.pages.checked_mul(PAGE)?;
            // SAFETY: `self.handle` is a valid pagefile-backed section of `len`
            // bytes owned by `self`; the mapping is unmapped once in `Drop`.
            let at = unsafe { MapViewOfFile(self.handle, FILE_MAP_WRITE, 0, 0, len) };
            if at.Value.is_null() {
                let error = std::io::Error::last_os_error();
                tracing::warn!(
                    pages = self.pages,
                    len,
                    %error,
                    "persistent host_view MapViewOfFile failed; falling back to transient views"
                );
                None
            } else {
                Some(at.Value as usize)
            }
        });
        base.map(|addr| addr as *mut u8)
    }
}

// SAFETY: a section handle may be used from any thread.
unsafe impl Send for Section {}
// SAFETY: as above.
unsafe impl Sync for Section {}

impl Drop for Section {
    fn drop(&mut self) {
        if let Some(Some(base)) = self.host_view.get() {
            use windows_sys::Win32::System::Memory::UnmapViewOfFile;
            // SAFETY: `base` came from `MapViewOfFile` in `persistent_base`,
            // and every persistent `View` holds an `Arc<Section>`, so no
            // reference into this mapping can outlive `Drop`.
            unsafe {
                UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                    Value: *base as *mut c_void,
                });
            }
        }
        // SAFETY: our handle; views keep the section alive on their own.
        unsafe { CloseHandle(self.handle) };
    }
}

/// Where a page lives, or what a view starts with: a section and a page
/// index in it.
#[derive(Clone)]
struct Slice {
    section: Arc<Section>,
    index: usize,
}

impl Slice {
    /// The key in `Physical::shown` of the page `k` pages after this one.
    fn key(&self, k: usize) -> (usize, usize) {
        (Arc::as_ptr(&self.section) as usize, self.index + k)
    }
}

/// The first page number given out; 0 stays unused.
const FIRST_PAGE: usize = 0x10_0000;
/// Numbers from here on do not survive the title's 21-bit `number >> 4`
/// ([`FRAMES`]).
const PAGE_LIMIT: usize = 1 << 25;

#[derive(Default)]
struct Physical {
    next_page: usize,
    pages: HashMap<usize, Slice>,
    /// Mapped views by address: their length and what they show.
    views: BTreeMap<usize, (usize, Slice)>,
    /// Where each page is shown, by section and index in it.
    shown: HashMap<(usize, usize), Vec<usize>>,
    /// Placeholders left by [`unmap`], address and length, each exactly
    /// what a view took ([`lean`]). The title may have joined or freed one
    /// since: mapping over it then fails and the placeholder is split first.
    holes: BTreeMap<usize, usize>,
}

impl Physical {
    /// The first of `n` numbers, [`FRAMES`] apart, for new pages: after the
    /// last given while they fit the title's 21 bits (as before), then the
    /// lowest run of `n` freed ones. `None` if there is no such run.
    fn take(&mut self, n: usize) -> Option<usize> {
        let len = n.checked_mul(FRAMES)?;
        let end = self.next_page.checked_add(len);
        if let Some(end) = end.filter(|&end| end <= PAGE_LIMIT) {
            let first = self.next_page;
            self.next_page = end;
            return Some(first);
        }
        static REUSING: std::sync::Once = std::sync::Once::new();
        REUSING.call_once(|| {
            tracing::info!("page numbers reach the title's 21 bits: freed ones given again");
        });
        let mut used: Vec<usize> = self.pages.keys().copied().collect();
        used.sort_unstable();
        free_run(used, len)
    }
}

/// The lowest `start` from [`FIRST_PAGE`] where `len` (a multiple of
/// [`FRAMES`]) numbers up to [`PAGE_LIMIT`] are all outside `used`
/// (sorted).
fn free_run(used: impl IntoIterator<Item = usize>, len: usize) -> Option<usize> {
    let mut start = FIRST_PAGE;
    for at in used.into_iter().chain([PAGE_LIMIT]) {
        if at.checked_sub(start).is_some_and(|gap| gap >= len) {
            return Some(start);
        }
        start = start.max(at.checked_add(FRAMES)?);
    }
    None
}

static PHYSICAL: LazyLock<Mutex<Physical>> = LazyLock::new(|| {
    Mutex::new(Physical {
        // Page numbers are opaque to titles.
        next_page: FIRST_PAGE,
        ..Default::default()
    })
});

fn physical() -> MutexGuard<'static, Physical> {
    PHYSICAL.lock().unwrap_or_else(|e| e.into_inner())
}

/// `count` is in and out (pages wanted, pages given); all or nothing here.
pub extern "system" fn XMemAllocatePhysicalPages(
    flags: u64,
    count: *mut usize,
    pages: *mut usize,
) -> BOOL {
    if count.is_null() || pages.is_null() {
        return 0;
    }
    // SAFETY: checked non-null.
    let n = unsafe { *count };
    if n.checked_mul(PAGE).is_none_or(|size| size == 0) {
        return 0;
    }
    // One section for the whole request unless `VICTORY_SECTION_MB` asks
    // for smaller ones ([`section_mb`]).
    let first = match section_mb::allocate_physical(n, section_mb::section_pages_for(n)) {
        Ok(first) => first,
        Err(error) => {
            tracing::warn!(n, %error, "XMemAllocatePhysicalPages failed");
            // SAFETY: checked non-null.
            unsafe { count.write(0) };
            return 0;
        }
    };
    tracing::debug!(
        n,
        flags = format_args!("{flags:#x}"),
        "XMemAllocatePhysicalPages"
    );
    for index in 0..n {
        // SAFETY: the title passes room for `n` page numbers.
        unsafe { pages.add(index).write(first + index * FRAMES) };
    }
    1
}

/// Page numbers count 4 KiB frames, so a 64 KiB page's is a multiple of
/// 16: the title keeps them as 21-bit `number >> 4` and rebuilds
/// them with `<< 4` (128 GiB of pages at most).
const FRAMES: usize = PAGE >> 12;

/// Pages still mapped stay readable through their views until unmapped.
pub extern "system" fn XMemFreePhysicalPages(n: usize, pages: *const usize) -> BOOL {
    if pages.is_null() {
        return 0;
    }
    let mut physical = physical();
    for i in 0..n {
        // SAFETY: the title passes `n` page numbers.
        physical.pages.remove(&unsafe { *pages.add(i) });
    }
    1
}

enum ViewBacking {
    /// Sub-range of `Section::host_view`; holding `Arc<Section>` prevents
    /// `Section::drop` from unmapping `host_view` while `View` lives, with
    /// zero `UnmapViewOfFile` / TLB shootdown on drop.
    Persistent { _section: Arc<Section> },
    /// Fallback transient `MapViewOfFile` mapping; unmapped on drop.
    Transient(MEMORY_MAPPED_VIEW_ADDRESS),
}

/// A view of our own on consecutive slices of one section, committed and
/// writable.
struct View {
    ptr: *mut u8,
    len: usize,
    _backing: ViewBacking,
}

impl View {
    /// Maps `n` slices from `slice` on.
    fn map(slice: &Slice, n: usize) -> Option<View> {
        let len = n.checked_mul(PAGE)?;
        if slice
            .index
            .checked_add(n)
            .is_none_or(|end| end > slice.section.pages)
        {
            return None;
        }
        if persistent_view_enabled() {
            if let Some(base) = slice.section.persistent_base() {
                // SAFETY: `slice.index + n <= slice.section.pages`, so
                // `[base + slice.index * PAGE, + len)` lies inside `host_view`.
                let ptr = unsafe { base.add(slice.index * PAGE) };
                if !slice.section.all_committed(slice.index, n) {
                    if !commit(ptr as usize, len) {
                        return None;
                    }
                    slice.section.mark_committed(slice.index, n);
                }
                slice.section.mark_touched(slice.index, n);
                return Some(View {
                    ptr,
                    len,
                    _backing: ViewBacking::Persistent {
                        _section: slice.section.clone(),
                    },
                });
            }
        }
        use windows_sys::Win32::System::Memory::{FILE_MAP_WRITE, MapViewOfFile};
        let offset = (slice.index * PAGE) as u64;
        // SAFETY: slices of a section we own; the view is ours until dropped.
        let at = unsafe {
            MapViewOfFile(
                slice.section.handle,
                FILE_MAP_WRITE,
                (offset >> 32) as u32,
                offset as u32,
                len,
            )
        };
        if at.Value.is_null() {
            return None;
        }
        let view = View {
            ptr: at.Value.cast(),
            len,
            _backing: ViewBacking::Transient(at),
        };
        if !commit(at.Value as usize, len) {
            return None;
        }
        slice.section.mark_touched(slice.index, n);
        Some(view)
    }

    fn bytes_range(&mut self, within: usize, len: usize) -> &mut [u8] {
        assert!(within.checked_add(len).is_some_and(|end| end <= self.len));
        // SAFETY: `[within, within + len)` lies within `self.len` committed
        // read-write bytes backed by `_backing` while `self` lives.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.add(within), len) }
    }

    #[cfg(test)]
    fn bytes(&mut self) -> &mut [u8] {
        self.bytes_range(0, self.len)
    }
}

impl Drop for View {
    fn drop(&mut self) {
        if let ViewBacking::Transient(at) = self._backing {
            use windows_sys::Win32::System::Memory::UnmapViewOfFile;
            // SAFETY: our transient view.
            unsafe { UnmapViewOfFile(at) };
        }
    }
}

/// The pages behind bytes `[offset, offset + len)` of `pages` (64 KiB
/// each), as runs of consecutive slices of one section: each run's first
/// slice and its length in pages. One look at the page table, which the
/// title's own mapping calls share with every DirectStorage worker. `None`
/// if a page is unknown or `pages` ends early.
fn runs(pages: &[usize], offset: usize, len: usize, touch: bool) -> Option<Vec<(Slice, usize)>> {
    let end = offset.checked_add(len)?;
    let pages = pages.get(offset / PAGE..end.div_ceil(PAGE))?;
    let physical = physical();
    let mut runs: Vec<(Slice, usize)> = Vec::new();
    for page in pages {
        let slice = physical.pages.get(page)?;
        match runs.last_mut() {
            Some((first, n))
                if Arc::ptr_eq(&first.section, &slice.section)
                    && slice.index == first.index + *n =>
            {
                *n += 1;
            }
            _ => runs.push((slice.clone(), 1)),
        }
    }
    if touch {
        for (slice, n) in &runs {
            slice.section.mark_touched(slice.index, *n);
        }
    }
    Some(runs)
}

/// Whether every page under `[offset, offset + len)` is known (a write that
/// fails with them known is a host failure).
pub(crate) fn pages_known(pages: &[usize], offset: usize, len: usize) -> bool {
    runs(pages, offset, len, false).is_some()
}

/// Flushes and holds any lazy GPU-resident pages currently mapped to `n`
/// physical pages from `slice` on BEFORE writing through an alias view,
/// strictly outside the `physical()` lock: preserves partial-page GPU
/// complements and blocks concurrent relay re-arming across both the physical
/// write and the subsequent `written()` notification.
fn flush_and_hold_shown(slice: &Slice, n: usize) -> Vec<victory_gpu::LazyAccessHold> {
    let mut shown: Vec<usize> = {
        let physical = physical();
        (0..n)
            .filter_map(|k| physical.shown.get(&slice.key(k)))
            .flatten()
            .copied()
            .collect()
    };
    shown.sort_unstable();
    shown.dedup();
    shown
        .chunk_by(|a, b| a + PAGE == *b)
        .map(|run| victory_gpu::kernel_will_write_hold(run[0], run.len() * PAGE))
        .collect()
}

/// Copies `data` into physical pages, from `offset` bytes into the first,
/// whether or not they are mapped anywhere (DirectStorage destinations).
/// One view per run of consecutive pages, not one per page: each view
/// costs a map, a commit and an unmap (which flushes every core's TLB).
/// False if a page is unknown or the host fails.
pub(crate) fn write_physical(pages: &[usize], offset: usize, data: &[u8]) -> bool {
    let Some(runs) = runs(pages, offset, data.len(), true) else {
        return false;
    };
    let (mut within, mut done) = (offset % PAGE, 0);
    for (slice, n) in &runs {
        let _holds = flush_and_hold_shown(slice, *n);
        let Some(mut view) = View::map(slice, *n) else {
            return false;
        };
        let k = (n * PAGE - within).min(data.len() - done);
        view.bytes_range(within, k)
            .copy_from_slice(&data[done..done + k]);
        slice.section.mark_touched(slice.index, *n);
        (within, done) = (0, done + k);
        written(slice, *n);
    }
    true
}

/// After writes into `n` pages from `slice` on through a view of our own:
/// where the title sees those pages, the GPU side has to copy them again (it
/// does not see such writes).
fn written(slice: &Slice, n: usize) {
    let mut shown: Vec<usize> = {
        let physical = physical();
        (0..n)
            .filter_map(|k| physical.shown.get(&slice.key(k)))
            .flatten()
            .copied()
            .collect()
    };
    shown.sort_unstable();
    for run in shown.chunk_by(|a, b| a + PAGE == *b) {
        victory_gpu::kernel_will_write(run[0], run.len() * PAGE);
        if victory_gpu::notify_view_writes() {
            victory_gpu::written_through_view(run[0], run.len() * PAGE);
        }
    }
}

/// Bytes `[offset, offset + len)` of physical pages, writable in place
/// until dropped: DirectStorage decodes into it instead of a buffer it
/// would then copy.
pub(crate) struct PageWindow {
    view: View,
    within: usize,
    len: usize,
    slice: Slice,
    _holds: Vec<victory_gpu::LazyAccessHold>,
}

impl Drop for PageWindow {
    fn drop(&mut self) {
        written(&self.slice, self.view.len / PAGE);
    }
}

impl PageWindow {
    pub(crate) fn bytes(&mut self) -> &mut [u8] {
        self.view.bytes_range(self.within, self.len)
    }
}

/// A window on bytes `[offset, offset + len)` of `pages` when they are
/// consecutive pages of one allocation (how titles usually hand them out);
/// `None` otherwise (pages of several allocations, out of order, or
/// unknown, or a host failure), which `write_physical` then handles or
/// reports.
pub(crate) fn page_window(pages: &[usize], offset: usize, len: usize) -> Option<PageWindow> {
    let runs = runs(pages, offset, len, true)?;
    let [(slice, n)] = runs.as_slice() else {
        return None;
    };
    let holds = flush_and_hold_shown(slice, *n);
    Some(PageWindow {
        view: View::map(slice, *n)?,
        within: offset % PAGE,
        len,
        slice: slice.clone(),
        _holds: holds,
    })
}

/// The title addresses that show bytes `[offset, offset + len)` of `pages`
/// now, byte for byte, as runs (address, bytes) in address order: a write
/// to the pages through a view of our own ([`page_window`],
/// [`write_physical`]) lands there too, and nothing that watches those
/// addresses sees it (the GPU side's guards fault only on a write at the
/// guarded address itself). Empty if no view shows them. Byte for byte,
/// not by whole pages: the title packs several small textures into one
/// page and fills each with a request of its own.
pub(crate) fn shown_at(pages: &[usize], offset: usize, len: usize) -> Vec<(usize, usize)> {
    let Some(end) = offset.checked_add(len) else {
        return Vec::new();
    };
    let first = offset / PAGE;
    let Some(touched) = pages.get(first..end.div_ceil(PAGE)) else {
        return Vec::new();
    };
    let mut pieces: Vec<(usize, usize)> = Vec::new();
    {
        let physical = physical();
        for (i, page) in touched.iter().enumerate() {
            let Some(addresses) = physical
                .pages
                .get(page)
                .and_then(|slice| physical.shown.get(&slice.key(0)))
            else {
                continue;
            };
            // The bytes of this page that the write covers.
            let lo = if i == 0 { offset % PAGE } else { 0 };
            let hi = (end - (first + i) * PAGE).min(PAGE);
            pieces.extend(addresses.iter().map(|&at| (at + lo, hi - lo)));
        }
    }
    pieces.sort_unstable();
    let mut runs: Vec<(usize, usize)> = Vec::new();
    for (at, bytes) in pieces {
        match runs.last_mut() {
            Some((start, n)) if *start + *n == at => *n += bytes,
            _ => runs.push((at, bytes)),
        }
    }
    runs
}

/// Bytes `[offset, offset + len)` of `pages` have been written through a
/// view of our own: the GPU side is told at every title address that
/// shows them (`victory_gpu::title_memory_written`; what it does with it
/// is `VICTORY_PAGE_WRITES`' to say, and it counts either way). Call when
/// the bytes are there, and without the page table's lock ([`shown_at`]
/// takes and leaves it).
pub(crate) fn pages_written(pages: &[usize], offset: usize, len: usize) {
    for (at, bytes) in shown_at(pages, offset, len) {
        victory_gpu::title_memory_written(at, bytes, true);
    }
}

/// Renderer port 1 (`VICTORY_PORT_CONSTANTS=1`; the GPU side's half is
/// `victory_gpu`'s `port`): read here for xmem's part, [`arrange`].
fn port_on() -> bool {
    static ON: LazyLock<bool> =
        LazyLock::new(|| std::env::var("VICTORY_PORT_CONSTANTS").is_ok_and(|v| v.trim() == "1"));
    *ON
}

/// The title's reservations of 64 KiB pages for 32-bit graphics memory:
/// base and bytes (one, of 185 MB, in Grand Theft Auto VI).
static LOW_RESERVATIONS: Mutex<Vec<(usize, usize)>> = Mutex::new(Vec::new());

/// Pages given another place in their section so far by [`arrange`], and
/// the times it found a page that was not fresh.
static ARRANGED: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

/// Before `pages` are mapped at `base`: if `base` lies in a 32-bit graphics
/// reservation ([`LOW_RESERVATIONS`]) whose pages are all one section at
/// least as large, the page numbers are given the slices of that section in
/// the reservation's address order (each page at its offset in the
/// reservation), so that the reservation becomes one stretch of the section
/// and the GPU side can open it as one heap.
///
/// Grand Theft Auto VI takes all the pages of its 185 MB heap in one call
/// and maps its five pieces in another order than their addresses (run
/// port1-1: "Scattered"). Only fresh slices are exchanged: never mapped,
/// never written (no touched bit), so every page still reads as zeros and
/// which slice a number names is not observable by the title.
fn arrange(physical: &mut Physical, base: usize, pages: &[usize]) {
    if pages.is_empty() {
        return;
    }
    let Some(end) = base.checked_add(pages.len().saturating_mul(PAGE)) else {
        return;
    };
    let Some((first, bytes)) = LOW_RESERVATIONS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .copied()
        .find(|&(at, len)| at <= base && end <= at.saturating_add(len))
    else {
        return;
    };
    let Some(section) = pages
        .first()
        .and_then(|p| physical.pages.get(p))
        .map(|s| s.section.clone())
    else {
        return;
    };
    if section.pages * PAGE < bytes
        || !pages.iter().all(|p| {
            physical
                .pages
                .get(p)
                .is_some_and(|s| Arc::ptr_eq(&s.section, &section))
        })
    {
        static SAID: AtomicU64 = AtomicU64::new(0);
        if SAID.fetch_add(1, Ordering::Relaxed) < 4 {
            tracing::warn!(
                at = format_args!("{base:#x}"),
                pages = pages.len(),
                section_pages = section.pages,
                reservation = bytes / PAGE,
                "renderer port 1: pages mapped into the 32-bit heap are not of one section as big as it: left where they are"
            );
        }
        return;
    }
    // Which number has which slice of the section.
    let mut owner: HashMap<usize, usize> = physical
        .pages
        .iter()
        .filter(|(_, s)| Arc::ptr_eq(&s.section, &section))
        .map(|(&number, s)| (s.index, number))
        .collect();
    let mut moved = 0u64;
    for (i, number) in pages.iter().enumerate() {
        let want = (base - first) / PAGE + i;
        let Some(have) = physical.pages.get(number).map(|s| s.index) else {
            continue;
        };
        if have == want {
            continue;
        }
        if !section.is_fresh(have) || !section.is_fresh(want) {
            ARRANGED[1].fetch_add(1, Ordering::Relaxed);
            continue;
        }
        if let Some(other) = owner.get(&want).copied() {
            if let Some(slice) = physical.pages.get_mut(&other) {
                slice.index = have;
                owner.insert(have, other);
            }
        } else {
            owner.remove(&have);
        }
        if let Some(slice) = physical.pages.get_mut(number) {
            slice.index = want;
        }
        owner.insert(want, *number);
        moved += 1;
    }
    if moved > 0 {
        let all = ARRANGED[0].fetch_add(moved, Ordering::Relaxed) + moved;
        tracing::info!(
            at = format_args!("{base:#x}"),
            pages = pages.len(),
            moved,
            all,
            not_fresh = ARRANGED[1].load(Ordering::Relaxed),
            "renderer port 1: the title's 32-bit graphics pages given their section's slices in address order"
        );
    }
}

/// Maps `n` pages at `address`, in place of what is mapped there; null
/// `pages` unmaps. Returns `address`, or null if a page or the host fails.
pub extern "system" fn XMemMapPhysicalPages(
    address: *mut c_void,
    n: usize,
    pages: *const usize,
) -> *mut c_void {
    let _call = Call::begin();
    let mut physical = physical();
    let pages = (!pages.is_null()).then(|| {
        // SAFETY: the title passes `n` page numbers.
        unsafe { std::slice::from_raw_parts(pages, n) }
    });
    if port_on() {
        if let Some(pages) = pages {
            arrange(&mut physical, address as usize, pages);
        }
    }
    if map_pages(&mut physical, address as usize, n, pages) {
        address
    } else {
        std::ptr::null_mut()
    }
}

/// [`XMemMapPhysicalPages`] with the physical state held: unmaps
/// `[base, base + n pages)`, then maps `pages` there (none: only unmaps).
/// False if a page or the host fails.
///
/// Until the pages are mapped again: `unmap` takes whole views away and
/// maps what stays of them again, and a mapped view is a reservation until
/// it is committed. A read of title memory in place must not fall into that
/// gap: the remapping lock is held throughout, for the addresses the change
/// touches ([`changing`]).
#[track_caller]
fn map_pages(physical: &mut Physical, base: usize, n: usize, pages: Option<&[usize]>) -> bool {
    use std::sync::atomic::Ordering::Relaxed;
    WORK[7].fetch_add(n as u64, Relaxed);
    // (Neptune) The pages are there already, each where it is asked for, or nothing is
    // there and nothing is asked for: on the console that costs the title next to nothing,
    // and it asks all the time (a thread of its own stood in these calls for 84% of its
    // time in play, a view taken away and one made for every 64 KiB page, each a call into
    // the kernel that stops every processor for a moment). No view is taken away or made.
    // The GPU side is told the bytes may have changed, as a mapping told it
    // (`VICTORY_REMAP_SAME=2`: it is told nothing; `=0`: mapped again, as before).
    let mode = remap_same();
    if mode != 0 {
        match pages {
            Some(pages) if n <= pages.len() && shows(physical, base, &pages[..n]) => {
                WORK[8].fetch_add(n as u64, Relaxed);
                if mode == 1 {
                    victory_gpu::kernel_will_write(base, n * PAGE);
                }
                return true;
            }
            None if shows_nothing(physical, base, n * PAGE) => {
                WORK[9].fetch_add(n as u64, Relaxed);
                return true;
            }
            _ => {}
        }
    }
    if let Some(pages) = pages.filter(|_| remap_keep()) {
        if let Some(unknown) = pages[..n].iter().find(|page| !physical.pages.contains_key(page)) {
            tracing::warn!(page = unknown, "XMemMapPhysicalPages: unknown page, nothing changed");
            return false;
        }
    }
    let _changing = changing(physical, base, n * PAGE);
    if side_by_side::all_on() {
        // The slices up to an unknown page, which fails the call after
        // those before it are mapped, as below.
        let slices: Vec<Option<Slice>> = pages
            .map(|p| p[..n].iter().map_while(|page| physical.pages.get(page).cloned().map(Some)).collect())
            .unwrap_or_default();
        let maps = side_by_side::runs(base, &slices);
        let done = side_by_side::change(physical, base, n * PAGE, maps, map);
        if let Some(pages) = pages.filter(|_| slices.len() < n) {
            tracing::warn!(page = pages[slices.len()], "XMemMapPhysicalPages: unknown page");
            return false;
        }
        return done;
    }
    unmap(physical, base, n * PAGE);
    let Some(pages) = pages else {
        return true;
    };
    let mut i = 0;
    while i < n {
        let Some(first) = physical.pages.get(&pages[i]).cloned() else {
            tracing::warn!(page = pages[i], "XMemMapPhysicalPages: unknown page");
            return false;
        };
        // One view per run of consecutive slices of one section.
        let run = 1 + pages[i + 1..]
            .iter()
            .zip(1..)
            .take_while(|&(page, k)| {
                physical.pages.get(page).is_some_and(|s| {
                    Arc::ptr_eq(&s.section, &first.section) && s.index == first.index + k
                })
            })
            .count();
        if !map(physical, base + i * PAGE, run * PAGE, first) {
            return false;
        }
        i += run;
    }
    true
}

/// One page at each address; null `pages`, or a page number 0, unmaps
/// (the title passes 0 for addresses to clear).
///
/// The physical state is taken once for the call; the remapping lock per
/// run of addresses that follow one another, for that run alone
/// ([`changing`]). Taken once for the whole call it was held up to 1.95 s
/// (run mem-W3a), and every texture refresh waited; taken per page when it
/// covered all title memory, each page waited for every in-place read under
/// way and the refreshes queued behind each (run amd-F).
///
/// Such a run is unmapped at once ([`map_scattered`]) and unchanged pages
/// inside it are left untouched. Each page is mapped as a view of its own:
/// the title scatters pages it streams one by one, so a view of several
/// would be split again by a later scatter, and while the split's surviving
/// pages are mapped again title threads reading them fault (about 125 a run
/// when runs were mapped as one view).
pub extern "system" fn XMemMapPhysicalPagesScatter(
    addresses: *const *mut c_void,
    n: usize,
    pages: *const usize,
) -> BOOL {
    if addresses.is_null() {
        return 0;
    }
    // SAFETY: the title passes `n` addresses and, unless null, `n` pages.
    let addresses = unsafe { std::slice::from_raw_parts(addresses, n) };
    let pages = (!pages.is_null()).then(|| unsafe { std::slice::from_raw_parts(pages, n) });
    let want = |i: usize| pages.map(|p| p[i]).filter(|&p| p != 0);
    lift_thread();
    let _call = Call::begin();
    let mut physical = physical();
    if !scatter_runs() && side_by_side::threads() > 1 {
        return side_by_side::scatter(&mut physical, addresses, want).into();
    }
    let mut i = 0;
    while i < n {
        let base = addresses[i] as usize;
        let run = if scatter_runs() {
            scatter_run(&physical, addresses, &want, i)
        } else {
            1
        };
        let done = if run == 1 {
            let page = want(i);
            map_pages(
                &mut physical,
                base,
                1,
                page.as_ref().map(std::slice::from_ref),
            )
        } else {
            let wanted: Vec<Option<usize>> = (i..i + run).map(want).collect();
            map_scattered(&mut physical, base, &wanted)
        };
        if !done {
            return 0;
        }
        i += run;
    }
    1
}

/// How many entries of a scatter from `i` on are one run: addresses that
/// follow one another page by page, each with a known page or none. An
/// unknown page ends a run (alone, it fails as before, after what precedes
/// it is done).
fn scatter_run(
    physical: &Physical,
    addresses: &[*mut c_void],
    want: &impl Fn(usize) -> Option<usize>,
    i: usize,
) -> usize {
    let known = |k: usize| want(k).is_none_or(|p| physical.pages.contains_key(&p));
    if !known(i) {
        return 1;
    }
    let base = addresses[i] as usize;
    let mut run = 1;
    while i + run < addresses.len()
        && run
            .checked_mul(PAGE)
            .and_then(|off| base.checked_add(off))
            .is_some_and(|at| at == addresses[i + run] as usize)
        && known(i + run)
    {
        run += 1;
    }
    run
}

/// [`map_pages`] for a run of a scatter: `want[k]` (a page, or none to
/// unmap) at `base + k` pages. Any page already showing what is wanted is
/// left untouched; each contiguous sub-run of pages that actually change is
/// unmapped once under one remapping lock, then mapped a view per page.
fn map_scattered(physical: &mut Physical, base: usize, want: &[Option<usize>]) -> bool {
    use std::sync::atomic::Ordering::Relaxed;
    WORK[7].fetch_add(want.len() as u64, Relaxed);
    let mode = remap_same();
    let there = |physical: &Physical, k: usize| {
        let at = base + k * PAGE;
        mode != 0
            && match want[k] {
                Some(page) => shows(physical, at, &[page]),
                None => shows_nothing(physical, at, PAGE),
            }
    };
    let mut i = 0;
    while i < want.len() {
        if there(physical, i) {
            if want[i].is_some() {
                WORK[8].fetch_add(1, Relaxed);
                if mode == 1 {
                    victory_gpu::kernel_will_write(base + i * PAGE, PAGE);
                }
            } else {
                WORK[9].fetch_add(1, Relaxed);
            }
            i += 1;
            continue;
        }
        let lo = i;
        i += 1;
        while i < want.len() && !there(physical, i) {
            i += 1;
        }
        let hi = i;
        SCATTER[0].fetch_add((hi - lo) as u64, Relaxed);
        SCATTER[1].fetch_add(1, Relaxed);
        let (start, len) = (base + lo * PAGE, (hi - lo) * PAGE);
        let _changing = changing(physical, start, len);
        if side_by_side::threads() > 1 {
            // A view per page, as below; every page of a run is known.
            let maps = (lo..hi)
                .filter_map(|k| Some((base + k * PAGE, PAGE, physical.pages.get(&want[k]?)?.clone())))
                .collect();
            if !side_by_side::change(physical, start, len, maps, map) {
                return false;
            }
            continue;
        }
        unmap(physical, start, len);
        let mut k = lo;
        while k < hi {
            let Some(page) = want[k] else {
                k += 1;
                continue;
            };
            let Some(slice) = physical.pages.get(&page).cloned() else {
                tracing::warn!(page, "XMemMapPhysicalPagesScatter: unknown page");
                return false;
            };
            if !map(physical, base + k * PAGE, PAGE, slice) {
                return false;
            }
            k += 1;
        }
    }
    true
}

/// Maps `n` slices from `first` on at `base`: one view where the run lies
/// in one placeholder, else (error 487, nothing changed) one
/// view per page.
fn map_run(physical: &mut Physical, base: usize, n: usize, first: Slice) -> bool {
    if n > 1 {
        match map_view(physical, base, n * PAGE, first.clone(), true) {
            Ok(()) => return true,
            Err(None) => return false,
            Err(Some(_)) => {}
        }
    }
    (0..n).all(|k| {
        let slice = Slice {
            index: first.index + k,
            section: first.section.clone(),
        };
        map(physical, base + k * PAGE, PAGE, slice)
    })
}

/// (Neptune) Whether a scatter unmaps a run of addresses at once
/// (`VICTORY_SCATTER_RUNS=0`: page by page, as before).
fn scatter_runs() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_SCATTER_RUNS").is_some_and(|v| v != "0"))
}

/// Since the last line ([`work_line`]): pages of scatters handled as runs,
/// those runs, views mapped without splitting a placeholder, views
/// mapped without committing (their pages committed already), and maps
/// over a placeholder that failed (it was split, and mapped again).
static SCATTER: [AtomicU64; 5] = [const { AtomicU64::new(0) }; 5];

/// Maps the physical pages shown at `[from, from + len)` at `to` as well,
/// in place of what `to` shows; `from` 0 unmaps. The GPU's page-mapping
/// copies (`victory_gpu`, queue `CopyPageMappingsBatchX`):
/// the title maps pages into a pool view, then has the GPU map them into its tiled
/// heap. Page by page; false if a source page is not mapped physical memory.
pub fn map_alias(to: u64, from: u64, len: u64) -> bool {
    let (to, from, len) = (to as usize, from as usize, len as usize);
    lift_thread();
    let _making = probe::Making::begin(probe::Kind::Copy);
    let _call = Call::begin();
    let mut physical = physical();
    WORK[4].fetch_add((len / PAGE) as u64, std::sync::atomic::Ordering::Relaxed);
    // (Neptune) As in `map_pages`: what is asked for is there already.
    let mode = remap_same();
    if mode != 0 {
        let slice_at = |at: usize| {
            physical
                .views
                .range(..=at)
                .next_back()
                .filter(|(base, (size, _))| at < *base + size)
                .map(|(base, (_, s))| {
                    (
                        Arc::as_ptr(&s.section) as usize,
                        s.index + (at - base) / PAGE,
                    )
                })
        };
        let same = if from == 0 {
            shows_nothing(&physical, to, len)
        } else {
            (0..len).step_by(PAGE).all(|k| {
                let there = slice_at(from + k);
                there.is_some() && there == slice_at(to + k)
            })
        };
        if same {
            WORK[10].fetch_add((len / PAGE) as u64, std::sync::atomic::Ordering::Relaxed);
            if mode == 1 && from != 0 {
                victory_gpu::kernel_will_write(to, len);
            }
            return true;
        }
    }
    // As in `map_pages`: no read in place meets the gap.
    let _changing = changing(&physical, to, len);
    let apart = from.saturating_add(len) <= to || to.saturating_add(len) <= from;
    if side_by_side::all_on() && (from == 0 || apart && alias_runs()) {
        // The source's slices up to one that is not mapped, which fails
        // the call after those before it are mapped, as below.
        let slices: Vec<Option<Slice>> = if from == 0 {
            Vec::new()
        } else {
            (0..len / PAGE)
                .map_while(|k| {
                    let at = from + k * PAGE;
                    physical
                        .views
                        .range(..=at)
                        .next_back()
                        .filter(|(base, (size, _))| at < *base + size)
                        .map(|(base, (_, s))| {
                            Some(Slice {
                                index: s.index + (at - base) / PAGE,
                                section: s.section.clone(),
                            })
                        })
                })
                .collect()
        };
        let maps = side_by_side::runs(to, &slices);
        let again = |physical: &mut Physical, at: usize, len: usize, first: Slice| {
            map_run(physical, at, len / PAGE, first)
        };
        let done = side_by_side::change(&mut physical, to, len, maps, again);
        return done && (from == 0 || slices.len() == len / PAGE);
    }
    unmap(&mut physical, to, len);
    // (Neptune) One view for each run of pages that follow one another in their section, as
    // `map_pages` makes them: page by page, a texture level of 16 MiB was 256 views, each
    // made by a call into the kernel and each taken away by another when the level goes,
    // and in a load two threads of the title stood in those calls for most of the frames
    // that took seconds. (Not where the two ranges overlap: there each page is looked up
    // after the ones before it are mapped, as before.)
    if from != 0 && apart && alias_runs() {
        let slice_at = |physical: &Physical, at: usize| {
            physical
                .views
                .range(..=at)
                .next_back()
                .filter(|(base, (size, _))| at < *base + size)
                .map(|(base, (_, s))| Slice {
                    index: s.index + (at - base) / PAGE,
                    section: s.section.clone(),
                })
        };
        let pages = len / PAGE;
        let mut k = 0;
        while k < pages {
            let Some(first) = slice_at(&physical, from + k * PAGE) else {
                return false;
            };
            let mut run = 1;
            while k + run < pages
                && slice_at(&physical, from + (k + run) * PAGE).is_some_and(|s| {
                    Arc::ptr_eq(&s.section, &first.section) && s.index == first.index + run
                })
            {
                run += 1;
            }
            if !map_run(&mut physical, to + k * PAGE, run, first) {
                return false;
            }
            k += run;
        }
        return true;
    }
    for k in (0..len).step_by(PAGE) {
        if from == 0 {
            break;
        }
        let at = from + k;
        let Some(slice) = physical
            .views
            .range(..=at)
            .next_back()
            .filter(|(base, (size, _))| at < *base + size)
            .map(|(base, (_, s))| Slice {
                index: s.index + (at - base) / PAGE,
                section: s.section.clone(),
            })
        else {
            return false;
        };
        if !map(&mut physical, to + k, PAGE, slice) {
            return false;
        }
    }
    true
}

/// Maps the single physical page shown at `from` across every page of
/// `[to, to + len)` (`CopyPageMappingsBatchX` range kind 1: constant page).
/// Pages already showing that slice are left untouched; each contiguous
/// sub-run that changes is unmapped once under one remapping lock so
/// multi-page views are torn down once instead of once per page.
pub fn map_alias_constant(to: u64, from: u64, len: u64) -> bool {
    use std::sync::atomic::Ordering::Relaxed;
    let (to, from, len) = (to as usize, from as usize, len as usize);
    let pages = len / PAGE;
    if pages == 0 {
        return true;
    }
    let _making = probe::Making::begin(probe::Kind::Constant);
    let _call = Call::begin();
    let mut physical = physical();
    WORK[4].fetch_add(pages as u64, Relaxed);
    let Some(source) = physical
        .views
        .range(..=from)
        .next_back()
        .filter(|(base, (size, _))| from < *base + size)
        .map(|(base, (_, s))| Slice {
            index: s.index + (from - base) / PAGE,
            section: s.section.clone(),
        })
    else {
        return false;
    };
    let mode = remap_same();
    let shows_source = |physical: &Physical, at: usize| {
        mode != 0
            && physical
                .views
                .range(..=at)
                .next_back()
                .is_some_and(|(base, (size, s))| {
                    at < base + size
                        && Arc::ptr_eq(&s.section, &source.section)
                        && s.index + (at - base) / PAGE == source.index
                })
    };
    let mut i = 0;
    while i < pages {
        let at = to + i * PAGE;
        if shows_source(&physical, at) {
            WORK[10].fetch_add(1, Relaxed);
            if mode == 1 {
                victory_gpu::kernel_will_write(at, PAGE);
            }
            i += 1;
            continue;
        }
        let lo = i;
        i += 1;
        while i < pages && !shows_source(&physical, to + i * PAGE) {
            i += 1;
        }
        let (start, sub_len) = (to + lo * PAGE, (i - lo) * PAGE);
        let _changing = changing(&physical, start, sub_len);
        if side_by_side::all_on() {
            let maps = (0..i - lo).map(|k| (start + k * PAGE, PAGE, source.clone())).collect();
            if !side_by_side::change(&mut physical, start, sub_len, maps, map) {
                return false;
            }
            continue;
        }
        unmap(&mut physical, start, sub_len);
        for k in 0..(i - lo) {
            if !map(&mut physical, start + k * PAGE, PAGE, source.clone()) {
                return false;
            }
        }
    }
    true
}

/// (Neptune) What a mapping of pages that are shown there already does
/// (`VICTORY_REMAP_SAME`): 1, the default: no view is taken away or made, and the GPU side
/// is told the bytes may have changed; 2: nothing; 0: the pages are mapped again.
fn remap_same() -> u8 {
    static MODE: std::sync::OnceLock<u8> = std::sync::OnceLock::new();
    *MODE.get_or_init(|| {
        std::env::var("VICTORY_REMAP_SAME")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(1)
    })
}

/// Whether a view is mapped straight over a placeholder only where
/// [`unmap`] left one of exactly its size, else after splitting one
/// (`VICTORY_MAP_LEAN=1`; off: a map is tried first everywhere, and where it
/// fails, the placeholder is split and the map tried again). Committed pages
/// are not committed again either way.
fn lean() -> bool {
    #[cfg(test)]
    if let Some(on) = tests::FORCE.with(|f| f.get()[0]) {
        return on;
    }
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_MAP_LEAN").is_some_and(|v| v == "1"))
}

/// Whether [`XMemMapPhysicalPages`] looks at every page number before it
/// takes away what is mapped (`VICTORY_REMAP_KEEP=1`, after fxceless #1460):
/// one it does not know fails the call with the old pages still mapped, as
/// the title expects of a remap that failed. Off: what was mapped is gone,
/// and the pages before the unknown one are mapped.
fn remap_keep() -> bool {
    #[cfg(test)]
    if let Some(on) = tests::FORCE.with(|f| f.get()[1]) {
        return on;
    }
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_REMAP_KEEP").is_some_and(|v| v == "1"))
}

/// Lifts the calling thread above normal priority if it is below normal,
/// once per thread (`VICTORY_STREAM_PRIORITY=1`, after cool_fox 49d71266b2).
/// Called where the title's streaming thread scatters pages and copies page
/// mappings: that thread runs below normal, and every mapping call it makes
/// waits for the process's address-space lock behind our guards' and the
/// driver's protection changes, so loading waits on it.
fn lift_thread() {
    use windows_sys::Win32::System::Threading::{
        GetCurrentThread, GetThreadPriority, SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL,
    };
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if !*ON.get_or_init(|| std::env::var_os("VICTORY_STREAM_PRIORITY").is_some_and(|v| v == "1")) {
        return;
    }
    thread_local!(static LOOKED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) });
    if LOOKED.with(|l| l.replace(true)) {
        return;
    }
    // SAFETY: plain calls on this thread's pseudo handle.
    unsafe {
        let thread = GetCurrentThread();
        let was = GetThreadPriority(thread);
        if matches!(was, -2..0) && SetThreadPriority(thread, THREAD_PRIORITY_ABOVE_NORMAL) != 0 {
            tracing::info!(was, "a title thread that maps pages lifted above normal priority (VICTORY_STREAM_PRIORITY=1)");
        }
    }
}

/// Whether `[base, base + pages)` shows `pages` already, page for page.
fn shows(physical: &Physical, base: usize, pages: &[usize]) -> bool {
    pages.iter().enumerate().all(|(i, page)| {
        let at = base + i * PAGE;
        let Some(slice) = physical.pages.get(page) else {
            return false;
        };
        physical
            .views
            .range(..=at)
            .next_back()
            .is_some_and(|(start, (size, shown))| {
                at < start + size
                    && Arc::ptr_eq(&shown.section, &slice.section)
                    && shown.index + (at - start) / PAGE == slice.index
            })
    })
}

/// Whether no view shows any of `[base, base + len)`.
fn shows_nothing(physical: &Physical, base: usize, len: usize) -> bool {
    let before = physical
        .views
        .range(..=base)
        .next_back()
        .is_some_and(|(at, (size, _))| at + size > base);
    !before
        && physical
            .views
            .range(base..base.saturating_add(len))
            .next()
            .is_none()
}

/// (Neptune) Whether a copy of page mappings makes one view for a run of pages
/// (`VICTORY_ALIAS_RUNS=0`: one for every page, as before).
fn alias_runs() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_ALIAS_RUNS").is_none_or(|v| v != "0"))
}

/// (Neptune) Since the last line: views taken away, the microseconds of it, views made, the
/// microseconds of it, pages whose mappings were copied (`map_alias`), calls of the title
/// that map or unmap pages, and the microseconds spent in those calls in all. A line every
/// five seconds in which any were made ([`work_line`]).
static WORK: [std::sync::atomic::AtomicU64; 12] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 12];

fn work_line() {
    use std::sync::atomic::Ordering::Relaxed;
    static LAST: Mutex<Option<std::time::Instant>> = Mutex::new(None);
    let Ok(mut last) = LAST.try_lock() else {
        return;
    };
    let now = std::time::Instant::now();
    let since = *last.get_or_insert(now);
    let secs = now.duration_since(since).as_secs_f64();
    if secs < 5.0 {
        return;
    }
    *last = Some(now);
    let w = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11].map(|i| WORK[i].swap(0, Relaxed));
    let s = [0, 1, 2, 3, 4].map(|i| SCATTER[i].swap(0, Relaxed));
    let split = SPLITS.swap(0, Relaxed);
    tracing::info!(
        "title memory remapped, a second over {secs:.0} s: {:.0} views taken away in {:.1} ms, {:.0} made in {:.1} ms; {:.0} calls of the title, {:.1} ms in them; pages asked for: {:.0} mapped or unmapped ({:.0} of them there already, {:.0} of nothing unmapped: nothing done), {:.0} of copied mappings ({:.0} there already); scatters: {:.0} pages in {:.0} runs; {:.0} views mapped without placeholder split, {:.0} without commit, {:.0} failed first maps, {split} views split; {:.1} ms in the calls side by side (VICTORY_MAP_THREADS, summed over the threads; the views' times above are what the title waited)",
        w[0] as f64 / secs,
        w[1] as f64 / 1e3 / secs,
        w[2] as f64 / secs,
        w[3] as f64 / 1e3 / secs,
        w[5] as f64 / secs,
        w[6] as f64 / 1e3 / secs,
        w[7] as f64 / secs,
        w[8] as f64 / secs,
        w[9] as f64 / secs,
        w[4] as f64 / secs,
        w[10] as f64 / secs,
        s[0] as f64 / secs,
        s[1] as f64 / secs,
        s[2] as f64 / secs,
        s[3] as f64 / secs,
        s[4] as f64 / secs,
        w[11] as f64 / 1e3 / secs,
    );
    if let Some(line) = probe::line(secs) {
        tracing::info!("{line}");
    }
}

/// Counts a view made, when the making ends.
struct Made(std::time::Instant);

impl Drop for Made {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering::Relaxed;
        WORK[2].fetch_add(1, Relaxed);
        WORK[3].fetch_add(self.0.elapsed().as_micros() as u64, Relaxed);
    }
}

/// Counts a call of the title that maps or unmaps pages, when it ends.
struct Call(std::time::Instant, #[allow(dead_code)] victory_gpu::hitch::Span);

impl Call {
    fn begin() -> Call {
        Call(std::time::Instant::now(), victory_gpu::hitch::span(victory_gpu::hitch::Phase::Remap))
    }
}

impl Drop for Call {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering::Relaxed;
        WORK[5].fetch_add(1, Relaxed);
        WORK[6].fetch_add(self.0.elapsed().as_micros() as u64, Relaxed);
        work_line();
    }
}

/// The remapping lock (`victory_gpu::title_memory_remapping_range`) for a
/// change of `[base, base + len)`: those addresses and the whole of the
/// views reaching past them, which `unmap` takes away and maps again in
/// part. Reads of title memory elsewhere go on meanwhile.
#[track_caller]
fn changing(physical: &Physical, base: usize, len: usize) -> victory_gpu::Remapping {
    let (lo, hi) = touched(physical, base, len);
    victory_gpu::title_memory_remapping_range(lo, (hi - lo).max(1))
}

/// The addresses a change of `[base, base + len)` touches: those, and the
/// whole of the views reaching past them.
fn touched(physical: &Physical, base: usize, len: usize) -> (usize, usize) {
    let end = base.saturating_add(len);
    let holding = |at: usize| {
        physical
            .views
            .range(..=at)
            .next_back()
            .filter(|(start, (size, _))| at < *start + size)
            .map(|(start, (size, _))| (*start, *start + size))
    };
    let lo = holding(base).map_or(base, |(start, _)| start.min(base));
    let hi = if len == 0 {
        end
    } else {
        holding(end - 1).map_or(end, |(_, stop)| stop.max(end))
    };
    (lo, hi)
}

/// Diagnostics: where the time under the remapping lock goes, since the last
/// line: [unmaps, their µs, kernel maps (placeholder split, view, commit),
/// their µs, µs of our own bookkeeping after a map (the GPU side's guards and
/// census, the shown pages)]. The texture refreshes wait for all of it.
#[cfg(feature = "diagnostics")]
static REMAP_TIME: [std::sync::atomic::AtomicU64; 5] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 5];

#[cfg(feature = "diagnostics")]
fn remap_time(k: usize, since: std::time::Instant) {
    use std::sync::atomic::Ordering::Relaxed;
    REMAP_TIME[k].fetch_add(since.elapsed().as_micros() as u64, Relaxed);
    if k != 4 {
        REMAP_TIME[k - 1].fetch_add(1, Relaxed);
    }
}

/// Logs [`REMAP_TIME`] at most every 5 s (diagnostics).
#[cfg(feature = "diagnostics")]
fn remap_time_line() {
    use std::sync::atomic::Ordering::Relaxed;
    static LAST: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    let now = std::time::Instant::now();
    if last.is_some_and(|t| now.duration_since(t) < std::time::Duration::from_secs(5)) {
        return;
    }
    let secs = last.map_or(5.0, |t| now.duration_since(t).as_secs_f64());
    *last = Some(now);
    let [unmaps, unmap_us, maps, map_us, books] =
        [0, 1, 2, 3, 4].map(|i| REMAP_TIME[i].swap(0, Relaxed));
    tracing::info!(
        "remapping work per second over {secs:.0} s: {:.0} unmaps {:.1} ms, {:.0} kernel maps {:.1} ms,          {:.1} ms of our bookkeeping after them",
        unmaps as f64 / secs,
        unmap_us as f64 / 1e3 / secs,
        maps as f64 / secs,
        map_us as f64 / 1e3 / secs,
        books as f64 / 1e3 / secs,
    );
}

/// Maps `slice` and the slices after it over `[base, base + len)`, which
/// must lie in one placeholder.
fn map(physical: &mut Physical, base: usize, len: usize, slice: Slice) -> bool {
    match map_view(physical, base, len, slice, true) {
        Ok(()) => true,
        Err(error) => {
            if let Some(error) = error {
                tracing::warn!(
                    address = format_args!("{base:#x}"),
                    len,
                    %error,
                    "mapping physical pages failed (not a placeholder?)"
                );
            }
            false
        }
    }
}

/// [`map`], saying nothing: the host's error if no view could be made
/// there (nothing changed then), none if committing failed (said already).
/// `fresh`: the slices are new at these addresses; else they are what
/// [`unmap`] took away there a moment ago, which the shown pages still list.
fn map_view(
    physical: &mut Physical,
    base: usize,
    len: usize,
    slice: Slice,
    fresh: bool,
) -> Result<(), Option<std::io::Error>> {
    let made = std::time::Instant::now();
    #[cfg(feature = "diagnostics")]
    let began = std::time::Instant::now();
    // SAFETY: maps the section directly when `[base, base + len)` is already
    // an exact placeholder (recycled slot after `UnmapViewOfFile2`), skipping
    // the `VirtualFree` split syscall. Otherwise splits the placeholder
    // around it and retries once. Adjacent placeholders are never coalesced:
    // views that grew across them were split again page by page by the
    // title's unmaps, the whole view each time (bench w_on1/w_off1: unmaps
    // of 0.1-0.5 ms each where the baseline's took 8-16 us).
    let map_once = || unsafe {
        MapViewOfFile3(
            slice.section.handle,
            GetCurrentProcess(),
            base as *const c_void,
            (slice.index * PAGE) as u64,
            len,
            MEM_REPLACE_PLACEHOLDER,
            PAGE_READWRITE,
            std::ptr::null_mut(),
            0,
        )
    };
    // `lean`: a map over a placeholder that is not exactly the range is
    // a kernel call that fails, so it is tried only over one [`unmap`] left.
    let own = physical.holes.remove(&base) == Some(len);
    // Those inside are mapped over now.
    let inside: Vec<usize> = physical.holes.range(base + 1..base + len).map(|(&at, _)| at).collect();
    for at in inside {
        physical.holes.remove(&at);
    }
    let mut view = if own || !lean() {
        map_once()
    } else {
        MEMORY_MAPPED_VIEW_ADDRESS { Value: std::ptr::null_mut() }
    };
    if view.Value as usize == base {
        SCATTER[2].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    } else {
        if own || !lean() {
            SCATTER[4].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        // SAFETY: acts only on placeholders; fails harmlessly when the
        // range is a whole placeholder or spans several.
        unsafe {
            VirtualFree(
                base as *mut c_void,
                len,
                MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER,
            );
        }
        view = map_once();
    }
    if view.Value as usize != base {
        return Err(Some(std::io::Error::last_os_error()));
    }
    let n = len / PAGE;
    // A section's pages committed through one view are committed in every
    // view of it (test `a_page_committed_through_one_view_is_committed_in_another`).
    if slice.section.all_committed(slice.index, n) {
        SCATTER[3].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    } else {
        if !commit(base, len) {
            // SAFETY: the view just mapped, back to a placeholder.
            unsafe { UnmapViewOfFile2(GetCurrentProcess(), view, MEM_PRESERVE_PLACEHOLDER) };
            return Err(None);
        }
        slice.section.mark_committed(slice.index, n);
    }
    #[cfg(feature = "diagnostics")]
    remap_time(3, began);
    noted(physical, base, len, slice, fresh, made);
    Ok(())
}

/// What [`map_view`] notes of a view the host has just mapped (and
/// committed) at `[base, base + len)`, begun at `made`.
fn noted(
    physical: &mut Physical,
    base: usize,
    len: usize,
    slice: Slice,
    fresh: bool,
    made: std::time::Instant,
) {
    let _made = Made(made);
    let n = len / PAGE;
    slice.section.mark_touched(slice.index, n);
    #[cfg(feature = "diagnostics")]
    let books = std::time::Instant::now();
    // New contents, read-write: the GPU side's copies no longer guard it
    // (what `unmap` maps again too: its protection is the view's, anew).
    victory_gpu::title_memory_remapped(base, len);
    // What the range shows now, for the GPU side's in-place census.
    victory_gpu::title_memory_view(
        base,
        len,
        slice.section.handle as usize,
        (slice.index * PAGE) as u64,
    );
    if fresh {
        for k in 0..n {
            let shown = physical.shown.entry(slice.key(k)).or_default();
            shown.push(base + k * PAGE);
        }
    }
    victory_gpu::titlemem::title_range(base, len, victory_gpu::titlemem::VIEW);
    probe::made(base);
    physical.views.insert(base, (len, slice));
    #[cfg(feature = "diagnostics")]
    remap_time(4, books);
}

/// Commits the section pages a view shows (once per page, whichever view).
fn commit(base: usize, len: usize) -> bool {
    if on_wine() {
        return true; // sections are committed whole there
    }
    // SAFETY: `[base, base + len)` is a view of a reserved section.
    if !unsafe { VirtualAlloc(base as *const c_void, len, MEM_COMMIT, PAGE_READWRITE) }.is_null() {
        return true;
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(ERROR_COMMITMENT_LIMIT as i32) {
        let mut info = PERFORMANCE_INFORMATION {
            cb: size_of::<PERFORMANCE_INFORMATION>() as u32,
            ..Default::default()
        };
        // SAFETY: the buffer and size describe a writable PERFORMANCE_INFORMATION.
        let queried = unsafe { K32GetPerformanceInfo(&mut info, info.cb) } != 0;
        victory_loader::log_crash(&commit_limit_message(len, &error, queried.then_some(&info)));
    } else {
        tracing::warn!(len, %error, "committing physical pages failed");
    }
    false
}

fn commit_limit_message(
    len: usize,
    error: &std::io::Error,
    info: Option<&PERFORMANCE_INFORMATION>,
) -> String {
    let mut message = format!(
        "VIctory: Windows commit limit reached while committing {len} bytes of physical pages: {error}.\n"
    );
    if let Some(info) = info {
        use std::fmt::Write;

        let committed = info.CommitTotal * info.PageSize / MIB;
        let limit = info.CommitLimit * info.PageSize / MIB;
        let _ = writeln!(
            message,
            "System commit: {committed} MiB of {limit} MiB in use."
        );
    }
    message.push_str(
        "Close other applications or increase the Windows paging file size, then retry.\n",
    );
    message
}

/// Returns `[base, base + len)` to placeholders; views reaching past it
/// are mapped again outside it.
fn unmap(physical: &mut Physical, base: usize, len: usize) {
    #[cfg(feature = "diagnostics")]
    let began = std::time::Instant::now();
    #[cfg(feature = "diagnostics")]
    let _timed = Defer(move || {
        remap_time(1, began);
        remap_time_line();
    });
    // No bytes: no view to return. The search below would still find the
    // view holding `base` and cut it in two there, and its range of views
    // starting inside would run backwards (which `BTreeMap` panics on).
    if len == 0 {
        return;
    }
    let end = base + len;
    // Views do not overlap: the one holding `base`, then those starting
    // inside (the GPU's page mappings make one view per page).
    let first = physical
        .views
        .range(..=base)
        .next_back()
        .filter(|(at, (size, _))| *at + size > base)
        .map(|(at, _)| *at);
    let hit: Vec<usize> = first
        .into_iter()
        .chain(physical.views.range(base + 1..end).map(|(at, _)| *at))
        .collect();
    for at in hit {
        let Some((size, slice)) = physical.views.remove(&at) else {
            continue;
        };
        probe::gone(at, size);
        // Only the pages taken away stop being shown; what stays of the
        // view is mapped again where it was, showing the same.
        let (lo, hi) = (base.max(at) - at, end.min(at + size) - at);
        for k in lo / PAGE..hi.div_ceil(PAGE) {
            unshow(physical, &slice, k, at + k * PAGE);
        }
        victory_gpu::titlemem::title_range_free(at);
        // Only the unmapped intersection is invalidated here; surviving tails
        // are invalidated in `map_again` when their views are mapped anew.
        victory_gpu::title_memory_remapped(at + lo, hi - lo);
        if at < base || at + size > end {
            SPLITS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        let taken = std::time::Instant::now();
        // SAFETY: a view we mapped; it becomes a placeholder again.
        let gone = unsafe {
            UnmapViewOfFile2(
                GetCurrentProcess(),
                MEMORY_MAPPED_VIEW_ADDRESS {
                    Value: at as *mut c_void,
                },
                MEM_PRESERVE_PLACEHOLDER,
            )
        };
        WORK[0].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        WORK[1].fetch_add(
            taken.elapsed().as_micros() as u64,
            std::sync::atomic::Ordering::Relaxed,
        );
        if at < base {
            map_again(physical, at, base - at, slice.clone());
        }
        if at + size > end {
            let index = slice.index + (end - at) / PAGE;
            map_again(physical, end, at + size - end, Slice { index, ..slice });
        }
        // What is taken away is a placeholder of its own now: cut off the
        // view's placeholder by mapping what stays of it again.
        if gone != 0 && lean() {
            let (lo, hi) = (base.max(at), end.min(at + size));
            physical.holes.insert(lo, hi - lo);
        }
    }
}

/// `slice`'s page `k` is no longer shown at `address`.
fn unshow(physical: &mut Physical, slice: &Slice, k: usize, address: usize) {
    if let std::collections::hash_map::Entry::Occupied(mut o) = physical.shown.entry(slice.key(k)) {
        o.get_mut().retain(|&shown| shown != address);
        if o.get().is_empty() {
            o.remove();
        }
    }
}

/// What stays of a view [`unmap`] took away, mapped again where it was. If
/// that fails, its pages are shown there no more.
fn map_again(physical: &mut Physical, base: usize, len: usize, slice: Slice) {
    if let Err(error) = map_view(physical, base, len, slice.clone(), false) {
        if let Some(error) = error {
            tracing::warn!(
                address = format_args!("{base:#x}"),
                len,
                %error,
                "mapping physical pages failed (not a placeholder?)"
            );
        }
        for k in 0..len / PAGE {
            unshow(physical, &slice, k, base + k * PAGE);
        }
    }
}

/// Views [`unmap`] took away only in part, mapping what stays of them again
/// (title threads reading those pages meanwhile fault), since the last line.
static SPLITS: AtomicU64 = AtomicU64::new(0);

/// The oldest layout we know; newer titles pass other flag values (Minecraft
/// 0x20000001) and read the same leading fields.
#[repr(C)]
#[derive(Debug, Default)]
pub struct WorkingSet {
    pub title_limit: usize,
    pub title_used: usize,
    pub tools_limit: usize,
    pub tools_used: usize,
    pub stack_used: u32,
    pub physical_pages: u32,
    pub gpu_optimal_limit: usize,
    pub gpu_optimal_used: usize,
    pub tools_gpu_optimal_limit: usize,
    pub tools_gpu_optimal_used: usize,
    pub kernel_pool_used: u32,
    pub kernel_pool_limit: u32,
}

/// The title's budget and our process's commit as what it uses. No tools
/// memory: a retail console.
// shortcut: all fields are filled whatever the flags ask for, and use is
// not split by bandwidth class (GPU-optimal use reads 0); track
// allocations per class if a title budgets from that field.
pub extern "system" fn XMemGetWorkingSetStatistics(_which: u32, out: *mut WorkingSet) -> BOOL {
    if out.is_null() {
        return 0;
    }
    let mut counters = PROCESS_MEMORY_COUNTERS {
        cb: size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        ..Default::default()
    };
    // SAFETY: our own process and a sized structure.
    unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, counters.cb) };
    let (gpu_optimal, standard) = victory_gpu::console::current().title_memory();
    let stats = WorkingSet {
        title_limit: gpu_optimal + standard - TRANSFERRED.load(Ordering::Relaxed),
        title_used: counters.PagefileUsage,
        gpu_optimal_limit: gpu_optimal,
        ..Default::default()
    };
    // What the title is told, logged at the first asking and at every
    // 1,024th: "used" is all the process has committed, the layer's own
    // memory and the card's included (27 GB in run 60's gameplay against a
    // limit of 13.5 GB). A title that budgets its streaming from these two
    // figures believes it is far over its limit.
    // shortcut: the figure is not yet the title's own memory alone (its
    // heaps and the pages it mapped); count those if this line shows the
    // title asking.
    static ASKED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let asked = ASKED.fetch_add(1, Ordering::Relaxed);
    if asked.is_multiple_of(1024) {
        tracing::info!(
            asked = asked + 1,
            which = _which,
            limit_mb = stats.title_limit >> 20,
            used_mb = stats.title_used >> 20,
            "the title asks how much memory it uses"
        );
    }
    // SAFETY: checked non-null; the title passes room for the structure.
    unsafe { out.write(stats) };
    1
}

/// Hands title memory (standard, in 2 MiB pages) to the system. The other
/// transfers involve tools memory, which a retail console has none of.
pub extern "system" fn XMemTransferMemory(kind: u32, size: usize) -> BOOL {
    const TITLE_TO_SYSTEM: u32 = 0;
    tracing::info!(kind, size, "XMemTransferMemory");
    if size == 0 {
        return 1;
    }
    let Some(size) = size.checked_next_multiple_of(2 * MIB) else {
        return 0;
    };
    let standard = victory_gpu::console::current().title_memory().1;
    let moved = kind == TITLE_TO_SYSTEM
        && TRANSFERRED
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |t| {
                t.checked_add(size).filter(|&t| t <= standard)
            })
            .is_ok();
    moved.into()
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "xmem",
        dlls: &["xmem.dll"],
        exports: vec![
            hle_export!(XMemAlloc),
            hle_export!(XMemFree),
            hle_export!(XMemAllocDefault),
            hle_export!(XMemFreeDefault),
            hle_export!(XMemIsToolsMemAvailable),
            hle_export!(XMemAllocatePhysicalPages),
            hle_export!(XMemFreePhysicalPages),
            hle_export!(XMemMapPhysicalPages),
            hle_export!(XMemMapPhysicalPagesScatter),
            hle_export!(XMemVirtualAlloc),
            hle_export!(XMemVirtualQuery),
            hle_export!(XMemGetWorkingSetStatistics),
            hle_export!(XMemTransferMemory),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    thread_local! {
        /// This thread's [`lean`] and [`remap_keep`], when set.
        pub(super) static FORCE: std::cell::Cell<[Option<bool>; 2]> =
            const { std::cell::Cell::new([None; 2]) };
        /// This thread's `side_by_side::threads`, when set.
        pub(super) static THREADS: std::cell::Cell<Option<usize>> =
            const { std::cell::Cell::new(None) };
        /// This thread's `side_by_side::all_on` switch, when set.
        pub(super) static THREADS_ALL: std::cell::Cell<Option<bool>> =
            const { std::cell::Cell::new(None) };
    }

    /// A scatter with `VICTORY_SCATTER_RUNS` off ends as page by page,
    /// with `VICTORY_MAP_THREADS=8` or without: pages over two pages of one
    /// view of four, an address given twice, pages shown already, unmaps of
    /// nothing, then a page that is not the title's (what came before it is
    /// done, and the call fails).
    #[test]
    fn a_scatter_page_by_page_side_by_side_ends_as_one_after_the_other() {
        let run = |threads: Option<usize>| {
            THREADS.with(|t| t.set(threads));
            let pages = allocate_whole(12);
            let a = XMemVirtualAlloc(std::ptr::null_mut(), 16 * PAGE, MEM_RESERVE | PAGES_64K, 0, PAGE_READWRITE);
            assert!(!a.is_null());
            let at = |k: usize| a.wrapping_byte_add(k * PAGE);
            assert_eq!(XMemMapPhysicalPages(a, 4, pages.as_ptr()), a);
            assert_eq!(XMemMapPhysicalPages(at(6), 1, &pages[6]), at(6));
            for k in [0, 1, 2, 3, 6] {
                // SAFETY: mapped read-write.
                unsafe { at(k).cast::<u64>().write(100 + k as u64) };
            }
            let to = [at(1), at(2), at(8), at(6), at(9), at(8), at(12), at(10)];
            let what = [pages[9], 0, pages[7], pages[6], 0, pages[11], 0, pages[10]];
            assert_eq!(XMemMapPhysicalPagesScatter(to.as_ptr(), to.len(), what.as_ptr()), 1);
            // SAFETY: the pages are mapped read-write.
            unsafe { at(8).cast::<u64>().write(808) };
            let other = allocate_whole(1);
            assert_eq!(XMemFreePhysicalPages(1, other.as_ptr()), 1);
            let to = [at(13), at(14), at(15)];
            let what = [pages[4], other[0], pages[5]];
            assert_eq!(XMemMapPhysicalPagesScatter(to.as_ptr(), 3, what.as_ptr()), 0);
            let views = views_in(a, 16);
            let state: Vec<u32> = (0..16).map(|k| query(at(k)).host.State).collect();
            // SAFETY: read where mapped only.
            let held: Vec<u64> = (0..16)
                .map(|k| if state[k] == MEM_COMMIT { unsafe { at(k).cast::<u64>().read() } } else { 0 })
                .collect();
            assert_eq!(XMemMapPhysicalPages(a, 16, std::ptr::null()), a);
            assert_eq!(XMemFreePhysicalPages(12, pages.as_ptr()), 1);
            THREADS.with(|t| t.set(None));
            (views, state, held)
        };
        let serial = run(None);
        assert_eq!(serial.2[0], 100);
        assert_eq!(serial.2[3], 103);
        assert_eq!(serial.2[8], 808);
        assert_eq!(serial.1[13], MEM_COMMIT);
        assert_eq!(serial.1[15], MEM_RESERVE);
        assert_eq!(run(Some(8)), serial);
    }

    /// With `VICTORY_MAP_THREADS=8 VICTORY_MAP_THREADS_ALL=1` (lean on, then
    /// off), the tests of maps of several pages, page-mapping copies and
    /// scatters hold as they do one after the other: the same views, the
    /// same bytes, the same failures.
    #[test]
    fn maps_and_copies_side_by_side_are_what_one_after_the_other_are() {
        THREADS.with(|t| t.set(Some(8)));
        THREADS_ALL.with(|t| t.set(Some(true)));
        for lean in [true, false] {
            FORCE.with(|f| f.set([Some(lean), None]));
            physical_pages_map_alias_and_unmap();
            a_scatter_of_many_pages_maps_each_address();
            mapping_no_pages_changes_nothing();
            writes_cross_runs_and_allocations();
            pages_know_where_they_are_shown();
            pages_say_where_they_are_mapped();
            a_page_committed_through_one_view_is_committed_in_another();
            a_view_of_pages_committed_already_is_readable_and_writable();
            a_scatter_unmaps_a_run_once_and_keeps_pages_independent();
            map_alias_constant_unmaps_once_and_never_coalesces_placeholders();
            many_pages_and_a_copy_of_them_side_by_side();
        }
        FORCE.with(|f| f.set([None; 2]));
        THREADS_ALL.with(|t| t.set(None));
        THREADS.with(|t| t.set(None));
    }

    /// 96 pages in 2 runs mapped at once, then 3 other runs over the views
    /// (a map must lie in one placeholder, as ever), copied page-mapping-wise
    /// into another range (a view per run), half unmapped by a copy from 0,
    /// and a copy whose source is mapped only in part.
    fn many_pages_and_a_copy_of_them_side_by_side() {
        let pages = allocate_whole(96);
        let a = XMemVirtualAlloc(std::ptr::null_mut(), 96 * PAGE, MEM_RESERVE | PAGES_64K, 0, PAGE_READWRITE);
        let b = XMemVirtualAlloc(std::ptr::null_mut(), 96 * PAGE, MEM_RESERVE | PAGES_64K, 0, PAGE_READWRITE);
        assert!(!a.is_null() && !b.is_null());
        let at = |base: *mut c_void, k: usize| base.wrapping_byte_add(k * PAGE).cast::<u32>();
        let first: Vec<usize> = (64..96).chain(0..64).collect();
        let order: Vec<usize> = first.iter().map(|&k| pages[k]).collect();
        assert_eq!(XMemMapPhysicalPages(a, 96, order.as_ptr()), a);
        assert_eq!(views_in(a, 96), [(0, 32), (32, 64)]);
        for k in 0..96 {
            // SAFETY: mapped read-write.
            unsafe { at(a, k).write(k as u32) };
        }
        // Each view's pages moved: page `p` holds where it was first.
        let then: Vec<usize> = (0..32).chain(64..96).chain(32..64).collect();
        let order: Vec<usize> = then.iter().map(|&k| pages[k]).collect();
        assert_eq!(XMemMapPhysicalPages(a, 96, order.as_ptr()), a);
        let held = |p: usize| first.iter().position(|&q| q == p).unwrap() as u32;
        // SAFETY: mapped read-write.
        assert!((0..96).all(|k| unsafe { at(a, k).read() } == held(then[k])));
        assert!(map_alias(b as u64, a as u64, (96 * PAGE) as u64));
        assert_eq!(views_in(b, 96), [(0, 32), (32, 32), (64, 32)]);
        // SAFETY: mapped read-write, the same pages as at `a`.
        unsafe {
            at(b, 5).write(555);
            assert_eq!(at(a, 5).read(), 555);
        }
        assert!(map_alias(b as u64, 0, (48 * PAGE) as u64));
        assert!((0..48).all(|k| query(at(b, k).cast()).host.State == MEM_RESERVE));
        // SAFETY: still mapped.
        assert_eq!(unsafe { at(b, 50).read() }, held(then[50]));
        // A source mapped up to its middle: that half is copied, then it fails.
        assert_eq!(XMemMapPhysicalPages(a.wrapping_byte_add(48 * PAGE), 48, std::ptr::null()), a.wrapping_byte_add(48 * PAGE));
        assert!(!map_alias(b as u64, a.wrapping_byte_add(40 * PAGE) as u64, (16 * PAGE) as u64));
        // SAFETY: the copied half is mapped.
        assert!((0..8).all(|k| unsafe { at(b, k).read() } == held(then[40 + k])));
        assert_eq!(XMemMapPhysicalPages(a, 96, std::ptr::null()), a);
        assert!(map_alias(b as u64, 0, (96 * PAGE) as u64));
        assert!(physical().views.range(b as usize..b as usize + 96 * PAGE).next().is_none());
        assert_eq!(XMemFreePhysicalPages(96, pages.as_ptr()), 1);
    }

    /// The same, one after the other.
    #[test]
    fn many_pages_and_a_copy_of_them() {
        many_pages_and_a_copy_of_them_side_by_side();
    }

    #[test]
    fn a_scatter_side_by_side_is_what_one_after_the_other_is() {
        THREADS.with(|t| t.set(Some(8)));
        for lean in [true, false] {
            FORCE.with(|f| f.set([Some(lean), None]));
            const N: usize = 200;
            let pages = allocate(N);
            let a = XMemVirtualAlloc(std::ptr::null_mut(), 2 * N * PAGE, MEM_RESERVE | PAGES_64K, 0, PAGE_READWRITE) as usize;
            assert_ne!(a, 0);
            let at = |k: usize| (a + k * PAGE) as *mut c_void;
            let first = |k: usize| at(k).cast::<u64>();
            for round in 0..4_usize {
                // Every address shows another page each round.
                let to: Vec<*mut c_void> = (0..N).map(|i| at((i + 7 * round) % N)).collect();
                assert_eq!(XMemMapPhysicalPagesScatter(to.as_ptr(), N, pages.as_ptr()), 1);
                for i in 0..N {
                    // SAFETY: mapped read-write.
                    unsafe {
                        if round > 0 {
                            assert_eq!(first((i + 7 * round) % N).read(), (1000 * (round - 1) + i) as u64);
                        }
                        first((i + 7 * round) % N).write((1000 * round + i) as u64);
                    }
                }
            }
            // Half of them unmapped by one scatter (null pages), then mapped
            // again by another.
            let to: Vec<*mut c_void> = (0..N / 2).map(at).collect();
            assert_eq!(XMemMapPhysicalPagesScatter(to.as_ptr(), N / 2, [0usize; N / 2].as_ptr()), 1);
            assert!((0..N / 2).all(|k| query(at(k)).host.State == MEM_RESERVE));
            assert_eq!(XMemMapPhysicalPagesScatter(to.as_ptr(), N / 2, pages.as_ptr()), 1);
            // SAFETY (below): mapped read-write.
            // Pages keep what they held: page k is at k now (and elsewhere).
            assert!((0..N / 2).all(|k| unsafe { first(k).read() } == (3000 + k) as u64));
            // An address twice: the last one counts; page 0 unmaps.
            let to = [at(0), at(1), at(0), at(2), at(3), at(0)];
            let what = [pages[5], pages[6], 0, 0, pages[7], pages[8]];
            assert_eq!(XMemMapPhysicalPagesScatter(to.as_ptr(), 6, what.as_ptr()), 1);
            assert_eq!(query(at(2)).host.State, MEM_RESERVE);
            // SAFETY: mapped read-write; the same page at three addresses.
            unsafe {
                first(0).write(77);
                let to = [at(10), at(11), at(12)];
                assert_eq!(XMemMapPhysicalPagesScatter(to.as_ptr(), 3, [pages[8]; 3].as_ptr()), 1);
                assert_eq!((first(10).read(), first(11).read(), first(12).read()), (77, 77, 77));
            }
            // A view of several pages, a run of a scatter over its middle:
            // both ends stay.
            let b = at(N + 8);
            assert_eq!(XMemMapPhysicalPages(b, 6, pages[20..26].as_ptr()), b);
            // SAFETY: mapped read-write.
            unsafe { first(N + 8 + 5).write(55) };
            let to: Vec<*mut c_void> = (N + 9..N + 13).map(at).collect();
            assert_eq!(XMemMapPhysicalPagesScatter(to.as_ptr(), 4, pages[30..34].as_ptr()), 1);
            // SAFETY: mapped read-write.
            unsafe {
                assert_eq!(first(N + 8 + 5).read(), 55);
                first(N + 9).write(9);
                assert_eq!(first(30).read(), 9);
            }
            assert_eq!(query(at(N + 8)).host.State, MEM_COMMIT);
            // A page the title does not own fails the call.
            assert_eq!(XMemMapPhysicalPagesScatter([at(40), at(41)].as_ptr(), 2, [pages[0], 1].as_ptr()), 0);
            // All gone.
            let to: Vec<*mut c_void> = (0..2 * N).map(at).collect();
            assert_eq!(XMemMapPhysicalPagesScatter(to.as_ptr(), 2 * N, std::ptr::null()), 1);
            assert!((0..2 * N).all(|k| query(at(k)).host.State == MEM_RESERVE));
            assert!(physical().views.range(a..a + 2 * N * PAGE).next().is_none());
            assert_eq!(XMemFreePhysicalPages(N, pages.as_ptr()), 1);
        }
        FORCE.with(|f| f.set([None; 2]));
        THREADS.with(|t| t.set(None));
    }

    /// Pages keep what they hold however often they are mapped, moved and
    /// unmapped with `VICTORY_MAP_LEAN=1`, also after the title joins the
    /// placeholders that unmapping left (the next map over one fails and is
    /// tried again after a split), and in runs of several pages.
    #[test]
    fn lean_maps_keep_what_pages_hold() {
        FORCE.with(|f| f.set([Some(true), None]));
        let pages = allocate(3);
        let a = XMemVirtualAlloc(std::ptr::null_mut(), 8 * PAGE, MEM_RESERVE | PAGES_64K, 0, PAGE_READWRITE);
        assert!(!a.is_null());
        let at = |k: usize| a.wrapping_byte_add(k * PAGE);
        for round in 0..6_usize {
            for (k, page) in pages.iter().enumerate() {
                let to = at((round + 2 * k) % 8);
                assert_eq!(XMemMapPhysicalPages(to, 1, page), to);
                assert!(!physical().holes.contains_key(&(to as usize)));
                let first = to.cast::<u64>();
                // SAFETY: mapped read-write; the far end of the page too.
                unsafe {
                    if round > 0 {
                        assert_eq!(first.read(), (round * 16 + k - 16) as u64);
                        assert_eq!(first.add(PAGE / 8 - 1).read(), k as u64);
                    }
                    first.write((round * 16 + k) as u64);
                    first.add(PAGE / 8 - 1).write(k as u64);
                }
            }
            for k in 0..pages.len() {
                let to = at((round + 2 * k) % 8);
                assert_eq!(XMemMapPhysicalPages(to, 1, std::ptr::null()), to);
                assert_eq!(query(to).host.State, MEM_RESERVE);
                assert_eq!(physical().holes.get(&(to as usize)), Some(&PAGE));
            }
            if round == 3 {
                // SAFETY: placeholders of ours, nothing mapped in them.
                unsafe { VirtualFree(a, 8 * PAGE, MEM_RELEASE | 1 /* MEM_COALESCE_PLACEHOLDERS */) };
            }
        }
        // A run of three as one view, over placeholders the title joined;
        // its middle page taken away leaves a hole of one page, and both
        // ends stay.
        // SAFETY: placeholders of ours, nothing mapped in them.
        unsafe { VirtualFree(a, 8 * PAGE, MEM_RELEASE | 1 /* MEM_COALESCE_PLACEHOLDERS */) };
        assert_eq!(XMemMapPhysicalPages(at(0), 3, pages.as_ptr()), at(0));
        // SAFETY: mapped read-write.
        unsafe { at(2).cast::<u64>().write(99) };
        assert_eq!(XMemMapPhysicalPages(at(1), 1, std::ptr::null()), at(1));
        assert_eq!(physical().holes.get(&(at(1) as usize)), Some(&PAGE));
        assert_eq!(query(at(0)).host.State, MEM_COMMIT);
        // SAFETY: still mapped read-write.
        assert_eq!(unsafe { at(2).cast::<u64>().read() }, 99);
        assert_eq!(XMemMapPhysicalPages(at(1), 1, &pages[1]), at(1));
        assert_eq!(XMemMapPhysicalPages(at(0), 3, std::ptr::null()), at(0));
        // Each page a view of its own then, and a placeholder of its own now.
        assert!((0..3).all(|k| physical().holes.get(&(at(k) as usize)) == Some(&PAGE)));
        assert_eq!(XMemFreePhysicalPages(3, pages.as_ptr()), 1);
        FORCE.with(|f| f.set([None; 2]));
    }

    /// With `VICTORY_REMAP_KEEP=1` a remap that cannot serve every page
    /// number leaves the title's memory as it was: the pages it showed stay
    /// mapped, with their bytes (fxceless #1460).
    #[test]
    fn a_remap_with_an_unknown_page_leaves_the_pages_it_showed() {
        FORCE.with(|f| f.set([None, Some(true)]));
        let pages = allocate(2);
        let a = XMemVirtualAlloc(std::ptr::null_mut(), 2 * PAGE, MEM_RESERVE | PAGES_64K, 0, PAGE_READWRITE);
        assert!(!a.is_null());
        assert_eq!(XMemMapPhysicalPages(a, 2, pages.as_ptr()), a);
        // SAFETY: both pages are mapped read-write.
        unsafe {
            a.cast::<u8>().write(0x5a);
            a.wrapping_byte_add(PAGE).cast::<u8>().write(0xa5);
        }
        // Another page in the list that is no physical page.
        let other = allocate(1);
        assert_eq!(XMemFreePhysicalPages(1, other.as_ptr()), 1);
        assert!(XMemMapPhysicalPages(a, 2, [pages[1], other[0]].as_ptr()).is_null());
        assert_eq!(query(a).host.State, MEM_COMMIT);
        assert_eq!(query(a.wrapping_byte_add(PAGE)).host.State, MEM_COMMIT);
        // SAFETY: both pages are still the ones mapped above.
        unsafe {
            assert_eq!(a.cast::<u8>().read(), 0x5a);
            assert_eq!(a.wrapping_byte_add(PAGE).cast::<u8>().read(), 0xa5);
        }
        assert_eq!(XMemMapPhysicalPages(a, 2, std::ptr::null()), a);
        assert_eq!(XMemFreePhysicalPages(2, pages.as_ptr()), 1);
        FORCE.with(|f| f.set([None; 2]));
    }

    fn query(address: *const c_void) -> BasicInformation {
        // SAFETY: plain data; zero is a valid value.
        let mut info: BasicInformation = unsafe { std::mem::zeroed() };
        // SAFETY: a pseudo handle needs no closing.
        let process = unsafe { GetCurrentProcess() };
        assert_eq!(XMemVirtualQuery(process, address, &mut info), 1);
        info
    }

    #[test]
    fn alloc_honours_alignment_and_frees() {
        for (field, align) in [(0, 16), (4, 16), (0xc, 4096), (0x10, 64 * 1024)] {
            let attributes = field << ALIGNMENT_SHIFT | 1 << 29; // a graphics type
            let p = XMemAlloc(100, attributes);
            assert!(!p.is_null());
            assert_eq!(p as usize % align, 0, "field {field:#x}");
            // SAFETY: 100 bytes were allocated.
            unsafe { p.cast::<u8>().write_bytes(0xab, 100) };
            XMemFree(p, attributes);
        }
        let p = XMemAlloc(4096, ALLOC_32BIT);
        assert!(!p.is_null() && (p as usize) < 1 << 32);
        XMemFree(p, ALLOC_32BIT);
    }

    #[test]
    fn virtual_alloc_meets_console_constraints() {
        const GPU_READ_WRITE: u32 = 0x2000;
        let p = XMemVirtualAlloc(
            std::ptr::null_mut(),
            4 * MIB,
            PAGES_2M | MEM_RESERVE | MEM_COMMIT,
            GRAPHICS | 7 << 12, // 40-bit addresses
            PAGE_READWRITE | GPU_READ_WRITE,
        );
        assert!(!p.is_null());
        assert_eq!(p as usize % (2 * MIB), 0);
        assert!((p as usize) < 1 << 40);
        let info = query(p);
        assert_eq!(info.host.State, MEM_COMMIT);
        assert_eq!(info.host.Protect, PAGE_READWRITE);
        assert_eq!(info.flags & GRAPHICS, GRAPHICS);
        // Write-watched, for the GPU side's copies.
        let (mut found, mut count, mut granularity) = ([0usize; 4], 4, 0);
        // SAFETY: room for `count` addresses.
        let watched = unsafe {
            windows_sys::Win32::System::Memory::GetWriteWatch(
                0,
                p,
                4 * MIB,
                found.as_mut_ptr().cast(),
                &mut count,
                &mut granularity,
            )
        };
        assert_eq!(watched, 0);
        // SAFETY: our own reservation.
        assert_ne!(unsafe { VirtualFree(p, 0, MEM_RELEASE) }, 0);
        assert_eq!(query(p).flags, 0);
        // No address field: above the 32-bit range and below 2^40, where
        // the console keeps it.
        let p = XMemVirtualAlloc(
            std::ptr::null_mut(),
            16 << 30,
            MEM_RESERVE,
            GRAPHICS,
            PAGE_NOACCESS,
        );
        assert!(!p.is_null() && (p as usize) >= 1 << 32, "{p:p}");
        assert!((p as usize) + (16 << 30) <= 1 << 40, "{p:p}");
        // SAFETY: our own reservation.
        assert_ne!(unsafe { VirtualFree(p, 0, MEM_RELEASE) }, 0);
    }

    #[test]
    fn transfers_shrink_the_title_budget() {
        let mut before = WorkingSet::default();
        assert_eq!(XMemGetWorkingSetStatistics(0x2000_0001, &mut before), 1);
        assert!(before.title_used > 0);
        assert_eq!(XMemTransferMemory(0, 128 * MIB - 1), 1);
        assert_eq!(XMemTransferMemory(2, MIB), 0, "no tools memory");
        let standard = victory_gpu::console::current().title_memory().1;
        assert_eq!(XMemTransferMemory(0, standard), 0, "over budget");
        let mut after = WorkingSet::default();
        XMemGetWorkingSetStatistics(0x2000_0001, &mut after);
        assert_eq!(before.title_limit - after.title_limit, 128 * MIB);
    }

    #[test]
    fn statistics_have_the_console_layout() {
        assert_eq!(size_of::<WorkingSet>(), 0x50);
        assert_eq!(size_of::<BasicInformation>(), 0x40);
        assert_eq!(std::mem::offset_of!(WorkingSet, gpu_optimal_limit), 0x28);
    }

    #[test]
    fn commit_limit_report_includes_request_and_system_commit() {
        let error = std::io::Error::from_raw_os_error(ERROR_COMMITMENT_LIMIT as i32);
        let info = PERFORMANCE_INFORMATION {
            CommitTotal: 512,
            CommitLimit: 768,
            PageSize: 4096,
            ..Default::default()
        };
        let report = commit_limit_message(4 * MIB, &error, Some(&info));
        assert!(report.contains("4194304 bytes"));
        assert!(report.contains("2 MiB of 3 MiB in use"));
        let fallback = commit_limit_message(4 * MIB, &error, None);
        for report in [&report, &fallback] {
            assert!(report.contains("os error 1455"));
            assert!(report.contains("Windows paging file size"));
        }
        assert!(!fallback.contains("System commit:"));
    }

    #[test]
    fn the_32_bit_heap_gets_its_slices_in_address_order() {
        // As the title's heap: a 32-bit reservation of 64 KiB pages, all of
        // its pages from one call, mapped piece by piece in another order.
        // The title's own kind: reserve and 0x40000, no 64 KiB page bits.
        let base = XMemVirtualAlloc(
            std::ptr::null_mut(),
            4 * PAGE,
            MEM_RESERVE | MEM_RESERVE_PLACEHOLDER,
            GRAPHICS | ADDRESS_32BIT,
            PAGE_NOACCESS,
        );
        assert!(!base.is_null() && (base as usize) < 1 << 32);
        assert!(
            LOW_RESERVATIONS
                .lock()
                .unwrap()
                .iter()
                .any(|r| r.0 == base as usize),
            "the reservation is known"
        );
        let pages = allocate(4);
        let index = |number: usize| physical().pages.get(&number).map(|s| s.index);
        let at = |page: usize| base.wrapping_byte_add(page * PAGE);
        // The last two pages' place first, from the array's start.
        arrange(&mut physical(), at(2) as usize, &pages[..2]);
        assert_eq!(XMemMapPhysicalPages(at(2), 2, pages.as_ptr()), at(2));
        arrange(&mut physical(), base as usize, &pages[2..]);
        assert_eq!(XMemMapPhysicalPages(base, 2, pages[2..].as_ptr()), base);
        assert_eq!(
            [
                index(pages[0]),
                index(pages[1]),
                index(pages[2]),
                index(pages[3])
            ],
            [Some(2), Some(3), Some(0), Some(1)]
        );
        // Mapped already: nothing moves any more.
        arrange(&mut physical(), base as usize, &pages[..2]);
        assert_eq!(index(pages[0]), Some(2));
        // SAFETY: mapped read-write.
        unsafe { at(3).cast::<u32>().write(7) };
        assert_eq!(XMemMapPhysicalPages(base, 4, std::ptr::null()), base);
        assert_eq!(XMemFreePhysicalPages(4, pages.as_ptr()), 1);
    }

    #[test]
    fn physical_pages_map_alias_and_unmap() {
        let mut pages = [0usize; 3];
        let mut n = 3;
        assert_eq!(XMemAllocatePhysicalPages(0, &mut n, pages.as_mut_ptr()), 1);
        assert_eq!(n, 3);
        // Packed into 21 bits as `number >> 4`, as the title keeps them.
        for p in pages {
            assert_eq!((p >> 4 & 0x1f_ffff) << 4, p);
        }
        // Reservations of 64 KiB pages, as titles make them for mapping.
        let reserve = || {
            XMemVirtualAlloc(
                std::ptr::null_mut(),
                3 * PAGE,
                MEM_RESERVE | PAGES_64K,
                0,
                PAGE_READWRITE,
            )
        };
        let (a, b) = (reserve(), reserve());
        assert!(!a.is_null() && !b.is_null());
        let at = |p: *mut c_void, page: usize| p.wrapping_byte_add(page * PAGE);
        assert_eq!(XMemMapPhysicalPages(a, 3, pages.as_ptr()), a);

        // The same page at a second address shows the same bytes.
        assert_eq!(
            XMemMapPhysicalPages(at(b, 1), 1, pages[2..].as_ptr()),
            at(b, 1)
        );
        // SAFETY: both pages are mapped read-write.
        unsafe {
            at(a, 2).cast::<u8>().write(7);
            assert_eq!(at(b, 1).cast::<u8>().read(), 7);
            a.cast::<u8>().write(1);
        }
        // Writes into pages (DirectStorage's) show through their views.
        assert!(write_physical(&pages[2..], 1, &[9]));
        // SAFETY: mapped read-write.
        assert_eq!(unsafe { at(b, 1).cast::<u8>().add(1).read() }, 9);

        // Unmapping the middle page keeps its neighbours.
        assert_eq!(
            XMemMapPhysicalPages(at(a, 1), 1, std::ptr::null()),
            at(a, 1)
        );
        assert_eq!(query(at(a, 1)).host.State, MEM_RESERVE);
        assert_eq!(query(a).host.State, MEM_COMMIT);
        assert_eq!(query(at(a, 2)).host.State, MEM_COMMIT);
        // SAFETY: still mapped.
        assert_eq!(unsafe { at(a, 2).cast::<u8>().read() }, 7);

        // A scattered page lands in the hole.
        let addresses = [at(a, 1)];
        assert_eq!(
            XMemMapPhysicalPagesScatter(addresses.as_ptr(), 1, pages.as_ptr()),
            1
        );
        // SAFETY: page 0 is now mapped at both `a` and `a + PAGE`.
        assert_eq!(unsafe { at(a, 1).cast::<u8>().read() }, 1);
        // Page number 0 unmaps.
        assert_eq!(
            XMemMapPhysicalPagesScatter(addresses.as_ptr(), 1, [0usize].as_ptr()),
            1
        );
        assert_eq!(query(at(a, 1)).host.State, MEM_RESERVE);
        // The GPU's page-mapping copy: what `a + 2 pages` shows, at `b`.
        assert!(map_alias(b as u64, at(a, 2) as u64, PAGE as u64));
        // SAFETY: mapped read-write.
        assert_eq!(unsafe { b.cast::<u8>().read() }, 7);
        assert!(map_alias(b as u64, 0, PAGE as u64));
        assert_eq!(query(b).host.State, MEM_RESERVE);
        assert!(!map_alias(b as u64, at(a, 1) as u64, PAGE as u64));
        assert_eq!(XMemFreePhysicalPages(3, pages.as_ptr()), 1);
        assert_eq!(XMemIsToolsMemAvailable(), 0);
    }

    /// One scatter call over consecutive addresses: pages out of
    /// order, an unmapped address (page number 0) between mapped ones, and an address
    /// apart from the others each show what the title asked for.
    #[test]
    fn a_scatter_of_many_pages_maps_each_address() {
        let mut pages = [0usize; 4];
        let mut n = 4;
        assert_eq!(XMemAllocatePhysicalPages(0, &mut n, pages.as_mut_ptr()), 1);
        let reserve = || {
            XMemVirtualAlloc(
                std::ptr::null_mut(),
                6 * PAGE,
                MEM_RESERVE | PAGES_64K,
                0,
                PAGE_READWRITE,
            )
        };
        let (a, b) = (reserve(), reserve());
        assert!(!a.is_null() && !b.is_null());
        let at = |p: *mut c_void, page: usize| p.wrapping_byte_add(page * PAGE);
        // Each page marked with its own byte, through a view of all four.
        assert_eq!(XMemMapPhysicalPages(b, 4, pages.as_ptr()), b);
        for k in 0..4 {
            // SAFETY: mapped read-write.
            unsafe { at(b, k).cast::<u8>().write(10 + k as u8) };
        }
        // a+0..a+2: pages 2, 0, 1 (out of order); a+3 unmapped; a+5: page 3.
        let addresses = [at(a, 0), at(a, 1), at(a, 2), at(a, 3), at(a, 5)];
        let scattered = [pages[2], pages[0], pages[1], 0, pages[3]];
        assert_eq!(
            XMemMapPhysicalPagesScatter(addresses.as_ptr(), 5, scattered.as_ptr()),
            1
        );
        // SAFETY: the mapped addresses are read-write.
        let read = |p: *mut c_void| unsafe { p.cast::<u8>().read() };
        assert_eq!(
            [
                read(at(a, 0)),
                read(at(a, 1)),
                read(at(a, 2)),
                read(at(a, 5))
            ],
            [12, 10, 11, 13]
        );
        assert_eq!(query(at(a, 3)).host.State, MEM_RESERVE);
        assert_eq!(query(at(a, 4)).host.State, MEM_RESERVE);
        // A write through the scatter's run shows at the page's other view.
        // SAFETY: mapped read-write.
        unsafe { at(a, 1).cast::<u8>().write(77) };
        assert_eq!(read(b), 77);
        let zeros = [0usize; 3];
        // Addresses that follow one another across two reservations (two
        // placeholders, which one view cannot span), in one call.
        let c = reserve();
        let d = XMemVirtualAlloc(at(c, 6), PAGE, MEM_RESERVE | PAGES_64K, 0, PAGE_READWRITE);
        if d == at(c, 6) {
            let across = [at(c, 5), d];
            assert_eq!(
                XMemMapPhysicalPagesScatter(across.as_ptr(), 2, pages[..2].as_ptr()),
                1
            );
            assert_eq!([read(at(c, 5)), read(d)], [77, 11]);
            assert_eq!(
                XMemMapPhysicalPagesScatter(across.as_ptr(), 2, zeros.as_ptr()),
                1
            );
        }
        // Unmapping in one call.
        assert_eq!(
            XMemMapPhysicalPagesScatter(addresses.as_ptr(), 3, zeros.as_ptr()),
            1
        );
        for k in 0..3 {
            assert_eq!(query(at(a, k)).host.State, MEM_RESERVE);
        }
        assert_eq!(read(at(a, 5)), 13, "the address apart keeps its page");
        assert_eq!(XMemMapPhysicalPages(b, 4, std::ptr::null()), b);
        assert_eq!(
            XMemMapPhysicalPagesScatter(addresses[4..].as_ptr(), 1, zeros.as_ptr()),
            1
        );
        assert_eq!(XMemFreePhysicalPages(4, pages.as_ptr()), 1);
    }

    /// A thread that reads a mapped page in place, as the GPU side reads a
    /// texture, never meets the moment in which the runtime has taken the
    /// page's view away to map it again: mapping the same pages at the same
    /// address unmaps the view and maps a new one, and without the lock the
    /// reader faults on a page "reserved, never committed" (the crashes of
    /// runs 71 and 74).
    #[test]
    fn a_read_in_place_never_meets_the_gap_of_a_remapping() {
        use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
        let mut pages = [0usize; 3];
        let mut n = 3;
        assert_eq!(XMemAllocatePhysicalPages(0, &mut n, pages.as_mut_ptr()), 1);
        let a = XMemVirtualAlloc(
            std::ptr::null_mut(),
            3 * PAGE,
            MEM_RESERVE | PAGES_64K,
            0,
            PAGE_READWRITE,
        );
        assert!(!a.is_null());
        assert_eq!(XMemMapPhysicalPages(a, 3, pages.as_ptr()), a);
        // SAFETY: mapped read-write.
        unsafe { a.cast::<u8>().write(1) };
        let first = a as usize;
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let reads = std::sync::Arc::new(AtomicU64::new(0));
        let reader = {
            let (stop, reads) = (stop.clone(), reads.clone());
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let _reading = victory_gpu::title_memory_reading();
                    // SAFETY: the page is mapped whenever no change of
                    // mapping is under way, and none is while `_reading`
                    // is held.
                    assert_eq!(unsafe { (first as *const u8).read_volatile() }, 1);
                    reads.fetch_add(1, Ordering::Relaxed);
                }
            })
        };
        // The reader is under way before the remappings begin (on a busy
        // machine it may otherwise first run after the last of them).
        while reads.load(Ordering::Relaxed) == 0 {
            std::thread::yield_now();
        }
        for _ in 0..2000 {
            // The same pages at the same address: the view goes and comes
            // back, with the same bytes.
            assert_eq!(XMemMapPhysicalPages(a, 3, pages.as_ptr()), a);
        }
        stop.store(true, Ordering::Relaxed);
        reader.join().expect("the reader never faulted");
        assert!(reads.load(Ordering::Relaxed) > 0);
        assert_eq!(XMemMapPhysicalPages(a, 3, std::ptr::null()), a);
        assert_eq!(XMemFreePhysicalPages(3, pages.as_ptr()), 1);
    }

    #[test]
    fn mapping_no_pages_changes_nothing() {
        let pages = allocate(2);
        let a = XMemVirtualAlloc(
            std::ptr::null_mut(),
            2 * PAGE,
            MEM_RESERVE | PAGES_64K,
            0,
            PAGE_READWRITE,
        );
        assert!(!a.is_null());
        assert_eq!(XMemMapPhysicalPages(a, 2, pages.as_ptr()), a);
        // SAFETY: both pages are mapped read-write.
        unsafe { a.cast::<u8>().write(5) };
        let second = a.wrapping_byte_add(PAGE);

        // A count of 0 at the start of a view, inside it and past it: with
        // pages, without (an unmap), and as the GPU's page-mapping copy.
        for at in [a, second, a.wrapping_byte_add(2 * PAGE)] {
            assert_eq!(XMemMapPhysicalPages(at, 0, pages.as_ptr()), at);
            assert_eq!(XMemMapPhysicalPages(at, 0, std::ptr::null()), at);
            assert!(map_alias(at as u64, a as u64, 0));
            assert!(map_alias(at as u64, 0, 0));
        }

        // The view is still the one view it was.
        let view = physical().views.get(&(a as usize)).map(|v| v.0);
        assert_eq!(view, Some(2 * PAGE));
        assert_eq!(query(a).host.State, MEM_COMMIT);
        assert_eq!(query(second).host.State, MEM_COMMIT);
        // SAFETY: still mapped.
        assert_eq!(unsafe { a.cast::<u8>().read() }, 5);

        // A count of 1 still unmaps that page alone.
        assert_eq!(XMemMapPhysicalPages(second, 1, std::ptr::null()), second);
        assert_eq!(query(second).host.State, MEM_RESERVE);
        assert_eq!(query(a).host.State, MEM_COMMIT);
        assert_eq!(XMemMapPhysicalPages(a, 1, std::ptr::null()), a);
        assert_eq!(XMemFreePhysicalPages(2, pages.as_ptr()), 1);
    }

    /// `n` pages of one new allocation.
    fn allocate(n: usize) -> Vec<usize> {
        let (mut pages, mut given) = (vec![0; n], n);
        assert_eq!(
            XMemAllocatePhysicalPages(0, &mut given, pages.as_mut_ptr()),
            1
        );
        assert_eq!(given, n);
        pages
    }

    /// `n` pages of one new allocation in one section, whatever
    /// `VICTORY_SECTION_MB` says.
    fn allocate_whole(n: usize) -> Vec<usize> {
        let first = section_mb::allocate_physical(n, usize::MAX).expect("allocated");
        (0..n).map(|k| first + k * FRAMES).collect()
    }

    /// The bytes of one page, through a view of our own.
    fn page_bytes(page: usize) -> Vec<u8> {
        let slice = physical().pages.get(&page).cloned().expect("known page");
        View::map(&slice, 1).expect("mapped").bytes().to_vec()
    }

    #[test]
    fn runs_group_consecutive_pages_of_one_allocation() {
        let (a, b) = (allocate(3), allocate(1));
        let lengths = |pages: &[usize], offset, len| {
            runs(pages, offset, len, false).map(|r| r.iter().map(|&(_, n)| n).collect::<Vec<_>>())
        };
        assert_eq!(lengths(&a, 0, 3 * PAGE), Some(vec![3]));
        // Only the pages the bytes reach.
        assert_eq!(lengths(&a, PAGE + 5, 10), Some(vec![1]));
        assert_eq!(lengths(&a, PAGE - 1, 2), Some(vec![2]));
        assert_eq!(lengths(&a, 0, 0), Some(vec![]));
        // Out of order, or across allocations: one run each.
        assert_eq!(
            lengths(&[a[0], a[2], a[1]], 0, 3 * PAGE),
            Some(vec![1, 1, 1])
        );
        assert_eq!(lengths(&[a[1], a[2], b[0]], 0, 3 * PAGE), Some(vec![2, 1]));
        // A slice that follows on in another section starts a run.
        assert_eq!(lengths(&[b[0], a[1]], 0, 2 * PAGE), Some(vec![1, 1]));
        // Too few pages for the bytes, or an unknown one.
        assert_eq!(lengths(&a, 0, 3 * PAGE + 1), None);
        assert_eq!(lengths(&[a[0], 0x10], 0, 2 * PAGE), None);
        assert_eq!(lengths(&a, usize::MAX, 2), None);
        assert_eq!(XMemFreePhysicalPages(3, a.as_ptr()), 1);
        assert_eq!(XMemFreePhysicalPages(1, b.as_ptr()), 1);
    }

    #[test]
    fn writes_cross_runs_and_allocations() {
        let (a, b) = (allocate(2), allocate(1));
        let pages = [a[0], a[1], b[0]];
        let data: Vec<u8> = (0..2 * PAGE + 64).map(|i| (i * 7 % 251) as u8).collect();
        let offset = PAGE - 32;
        // Past the third page: the second run is too short.
        assert!(!write_physical(&pages, offset, &data));
        assert!(write_physical(&pages, offset, &data[..2 * PAGE]));
        let seen: Vec<u8> = pages.iter().flat_map(|&p| page_bytes(p)).collect();
        assert!(seen[..offset].iter().all(|&b| b == 0), "before the offset");
        assert_eq!(seen[offset..offset + 2 * PAGE], data[..2 * PAGE]);
        assert!(seen[offset + 2 * PAGE..].iter().all(|&b| b == 0), "after");
        assert!(
            !write_physical(&[a[0], 0x10], 0, &data[..PAGE + 1]),
            "unknown page"
        );
        assert_eq!(XMemFreePhysicalPages(2, a.as_ptr()), 1);
        assert_eq!(XMemFreePhysicalPages(1, b.as_ptr()), 1);
    }

    /// Pages know where they are shown, as views come, are cut and go.
    #[test]
    fn pages_know_where_they_are_shown() {
        let (mut count, mut pages) = (4usize, [0usize; 4]);
        assert_eq!(
            XMemAllocatePhysicalPages(0, &mut count, pages.as_mut_ptr()),
            1
        );
        let kind = MEM_RESERVE | PAGES_64K;
        let base = XMemVirtualAlloc(std::ptr::null_mut(), 8 * PAGE, kind, 3, PAGE_READWRITE);
        assert!(!base.is_null());
        let at = base as usize;
        let shown = |page: usize| {
            let physical = physical();
            let key = physical.pages[&page].key(0);
            let mut shown = physical.shown.get(&key).cloned().unwrap_or_default();
            shown.sort_unstable();
            shown
        };
        assert_eq!(XMemMapPhysicalPages(base, 4, pages.as_ptr()), base);
        assert_eq!(shown(pages[2]), [at + 2 * PAGE]);
        // Shown twice; the first view cut in two.
        let second = (at + 5 * PAGE) as *mut c_void;
        assert_eq!(XMemMapPhysicalPages(second, 1, &pages[2]), second);
        assert_eq!(shown(pages[2]), [at + 2 * PAGE, at + 5 * PAGE]);
        let gone = (at + 2 * PAGE) as *mut c_void;
        XMemMapPhysicalPages(gone, 1, std::ptr::null());
        assert_eq!(shown(pages[2]), [at + 5 * PAGE]);
        assert_eq!(shown(pages[1]), [at + PAGE]);
        assert_eq!(shown(pages[3]), [at + 3 * PAGE]);
        XMemMapPhysicalPages(base, 8, std::ptr::null());
        assert!(pages.iter().all(|&p| shown(p).is_empty()));
    }

    /// Pages say where the title has them mapped, byte for byte (after
    /// fork PR #571's test; its runs are whole pages).
    #[test]
    fn pages_say_where_they_are_mapped() {
        let pages = allocate(3);
        let reserve = |n: usize| {
            let at = XMemVirtualAlloc(
                std::ptr::null_mut(),
                n * PAGE,
                MEM_RESERVE | PAGES_64K,
                0,
                PAGE_READWRITE,
            );
            assert!(!at.is_null());
            at
        };
        // `b` keeps a page unmapped at either end, so that no run at `b`
        // ever touches one at `a`, wherever the host puts the two.
        let (a, b) = (reserve(3), reserve(4));
        assert!(shown_at(&pages, 0, 3 * PAGE).is_empty(), "mapped nowhere");
        // All three at `a`; the last two at `b`, from its second page on.
        assert_eq!(XMemMapPhysicalPages(a, 3, pages.as_ptr()), a);
        let second = b.wrapping_byte_add(PAGE);
        assert_eq!(XMemMapPhysicalPages(second, 2, pages[1..].as_ptr()), second);
        let (a, second) = (a as usize, second as usize);
        let sorted = |mut runs: Vec<(usize, usize)>| {
            runs.sort_unstable();
            runs
        };
        assert_eq!(shown_at(&pages, 0, PAGE), vec![(a, PAGE)]);
        // A request for part of a page names those bytes alone.
        assert_eq!(shown_at(&pages, 0x4000, 0x3000), vec![(a + 0x4000, 0x3000)]);
        // Across two pages, shown at two addresses each: one run at each.
        assert_eq!(
            shown_at(&pages, PAGE + 1, PAGE),
            sorted(vec![(a + PAGE + 1, PAGE), (second + 1, PAGE)])
        );
        // A page unmapped in the middle of a view: the rest still shows.
        let middle = (a + PAGE) as *mut c_void;
        assert_eq!(XMemMapPhysicalPages(middle, 1, std::ptr::null()), middle);
        assert_eq!(
            shown_at(&pages, 0, 3 * PAGE),
            sorted(vec![(a, PAGE), (a + 2 * PAGE, PAGE), (second, 2 * PAGE)])
        );
        // Too few pages for the bytes: nothing (the write fails too).
        assert!(shown_at(&pages, 0, 3 * PAGE + 1).is_empty());
        for (at, n) in [(a, 3), (second, 2)] {
            let at = at as *mut c_void;
            assert_eq!(XMemMapPhysicalPages(at, n, std::ptr::null()), at);
        }
        assert!(shown_at(&pages, 0, 3 * PAGE).is_empty());
        assert_eq!(XMemFreePhysicalPages(3, pages.as_ptr()), 1);
    }

    #[test]
    fn page_windows_cover_one_run_in_place() {
        let (a, b) = (allocate(3), allocate(1));
        let mut window = page_window(&a, PAGE + 3, PAGE).expect("one run");
        assert_eq!(window.bytes().len(), PAGE);
        window.bytes().fill(0x5a);
        drop(window);
        let (second, third) = (page_bytes(a[1]), page_bytes(a[2]));
        assert!(second[..3].iter().all(|&b| b == 0));
        assert!(second[3..].iter().all(|&b| b == 0x5a));
        assert!(third[..3].iter().all(|&b| b == 0x5a));
        assert!(third[3..].iter().all(|&b| b == 0));
        // Several runs, or an unknown page: no window.
        assert!(page_window(&[a[0], b[0]], 0, PAGE + 1).is_none());
        assert!(page_window(&[a[1], a[0]], 0, PAGE + 1).is_none());
        assert!(page_window(&[0x10], 0, 1).is_none());
        // One page of several is still one run.
        assert!(page_window(&[a[0], b[0]], 0, PAGE).is_some());
        assert_eq!(XMemFreePhysicalPages(3, a.as_ptr()), 1);
        assert_eq!(XMemFreePhysicalPages(1, b.as_ptr()), 1);
    }

    /// A reservation of `n` 64 KiB pages, as titles make them for mapping.
    fn reserve(n: usize) -> *mut c_void {
        let at = XMemVirtualAlloc(
            std::ptr::null_mut(),
            n * PAGE,
            MEM_RESERVE | PAGES_64K,
            0,
            PAGE_READWRITE,
        );
        assert!(!at.is_null());
        at
    }

    /// The views over `[base, base + n pages)`: first page and pages.
    fn views_in(base: *mut c_void, n: usize) -> Vec<(usize, usize)> {
        let base = base as usize;
        physical()
            .views
            .range(base..base + n * PAGE)
            .map(|(at, (len, _))| ((at - base) / PAGE, len / PAGE))
            .collect()
    }

    /// The test `map_view` counts on to map a section's pages without a
    /// commit of their own: what one view of a `SEC_RESERVE` section
    /// commits, every other view of it shows committed, a view mapped over
    /// a placeholder as `map_view` maps them included.
    #[test]
    fn a_page_committed_through_one_view_is_committed_in_another() {
        use windows_sys::Win32::System::Memory::{FILE_MAP_WRITE, MapViewOfFile, UnmapViewOfFile};
        if on_wine() {
            return; // sections are committed whole there
        }
        // SAFETY: a section of two pages, views of it and a placeholder of
        // our own, written only where committed; all released at the end.
        unsafe {
            let handle = CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                std::ptr::null(),
                PAGE_READWRITE | SEC_RESERVE,
                0,
                (2 * PAGE) as u32,
                std::ptr::null(),
            );
            assert!(!handle.is_null());
            let view = || MapViewOfFile(handle, FILE_MAP_WRITE, 0, 0, 2 * PAGE).Value;
            let (a, b) = (view(), view());
            assert!(!a.is_null() && !b.is_null() && a != b);
            assert_eq!(query(b).host.State, MEM_RESERVE);
            assert!(commit(a as usize, PAGE));
            a.cast::<u8>().write(0x5a);
            assert_eq!(query(b).host.State, MEM_COMMIT);
            assert_eq!(query(b.byte_add(PAGE)).host.State, MEM_RESERVE);
            assert_eq!(b.cast::<u8>().read(), 0x5a);
            b.cast::<u8>().add(1).write(7);
            assert_eq!(a.cast::<u8>().add(1).read(), 7);
            let placeholder = VirtualAlloc2(
                GetCurrentProcess(),
                std::ptr::null(),
                PAGE,
                MEM_RESERVE | MEM_RESERVE_PLACEHOLDER,
                PAGE_NOACCESS,
                std::ptr::null_mut(),
                0,
            );
            assert!(!placeholder.is_null());
            let c = MapViewOfFile3(
                handle,
                GetCurrentProcess(),
                placeholder,
                0,
                PAGE,
                MEM_REPLACE_PLACEHOLDER,
                PAGE_READWRITE,
                std::ptr::null_mut(),
                0,
            )
            .Value;
            assert_eq!(c, placeholder);
            assert_eq!(query(c).host.State, MEM_COMMIT);
            assert_eq!(c.cast::<u8>().read(), 0x5a);
            c.cast::<u8>().add(2).write(9);
            assert_eq!(b.cast::<u8>().add(2).read(), 9);
            for at in [a, b, c] {
                UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: at });
            }
            CloseHandle(handle);
        }
    }

    /// Pages committed once (by a title view, or by a write of our own)
    /// show committed and writable in a view mapped later, with no commit.
    #[test]
    fn a_view_of_pages_committed_already_is_readable_and_writable() {
        let (pages, written) = (allocate(2), allocate(1));
        let section = physical().pages[&pages[0]].section.clone();
        let (a, b) = (reserve(2), reserve(3));
        assert_eq!(XMemMapPhysicalPages(a, 2, pages.as_ptr()), a);
        assert!(section.all_committed(0, 2));
        // SAFETY: mapped read-write.
        unsafe {
            a.cast::<u8>().write(3);
            a.byte_add(PAGE).cast::<u8>().write(4);
        }
        assert_eq!(XMemMapPhysicalPages(a, 2, std::ptr::null()), a);
        assert_eq!(XMemMapPhysicalPages(b, 2, pages.as_ptr()), b);
        assert!(write_physical(&written, 0, &[1, 2, 3]));
        let third = b.wrapping_byte_add(2 * PAGE);
        assert_eq!(XMemMapPhysicalPages(third, 1, written.as_ptr()), third);
        for k in 0..3 {
            assert_eq!(query(b.wrapping_byte_add(k * PAGE)).host.State, MEM_COMMIT);
        }
        // SAFETY: mapped read-write.
        unsafe {
            assert_eq!(b.cast::<u8>().read(), 3);
            assert_eq!(b.byte_add(PAGE).cast::<u8>().read(), 4);
            assert_eq!(third.cast::<u8>().add(2).read(), 3);
            b.cast::<u8>().add(5).write(9);
            third.cast::<u8>().add(7).write(8);
        }
        assert_eq!(page_bytes(pages[0])[5], 9);
        assert_eq!(page_bytes(written[0])[7], 8);
        assert_eq!(XMemMapPhysicalPages(b, 3, std::ptr::null()), b);
        assert_eq!(XMemFreePhysicalPages(2, pages.as_ptr()), 1);
        assert_eq!(XMemFreePhysicalPages(1, written.as_ptr()), 1);
    }

    /// Unmapping the middle of a multi-page view via scatter tears down the
    /// view once, keeping both ends' bytes and `shown` entries; slices
    /// scattered back are a view per page; unchanged pages in the middle of a
    /// run are untouched.
    #[test]
    fn a_scatter_unmaps_a_run_once_and_keeps_pages_independent() {
        let pages = allocate(8);
        let a = reserve(8);
        let at = |k: usize| a.wrapping_byte_add(k * PAGE);
        let addresses: Vec<*mut c_void> = (0..8).map(at).collect();
        let scatter = |from: usize, wanted: &[usize]| {
            XMemMapPhysicalPagesScatter(addresses[from..].as_ptr(), wanted.len(), wanted.as_ptr())
        };
        assert_eq!(XMemMapPhysicalPages(a, 8, pages.as_ptr()), a);
        assert_eq!(views_in(a, 8), [(0, 8)]);
        for k in 0..8 {
            // SAFETY: mapped read-write.
            unsafe { at(k).cast::<u8>().write(10 + k as u8) };
        }
        // SAFETY: read only where mapped.
        let read = |k: usize| unsafe { at(k).cast::<u8>().read() };
        let shown = |page: usize| {
            let physical = physical();
            let key = physical.pages[&page].key(0);
            physical.shown.get(&key).cloned().unwrap_or_default()
        };

        // Unmapping 4 pages in the middle of the 8-page view tears it down
        // once and keeps both 2-page tails intact (bytes + shown).
        assert_eq!(scatter(2, &[0; 4]), 1);
        assert_eq!(views_in(a, 8), [(0, 2), (6, 2)]);
        for k in 2..6 {
            assert_eq!(query(at(k)).host.State, MEM_RESERVE);
            assert!(shown(pages[k]).is_empty());
        }
        assert_eq!([read(0), read(1), read(6), read(7)], [10, 11, 16, 17]);
        for k in [0, 1, 6, 7] {
            assert_eq!(shown(pages[k]), [at(k) as usize], "page {k}");
        }

        // Scatter mapping consecutive slices into the hole maps a view per
        // page, which a later scatter never splits.
        assert_eq!(scatter(2, &pages[2..6]), 1);
        assert_eq!(views_in(a, 8), [(0, 2), (2, 1), (3, 1), (4, 1), (5, 1), (6, 2)]);
        assert_eq!((2..6).map(read).collect::<Vec<_>>(), [12, 13, 14, 15]);

        // Unmapping all 8 pages leaves 3 placeholders, which are never
        // coalesced: the run across them is mapped a page at a time.
        assert_eq!(scatter(0, &[0; 8]), 1);
        assert!(views_in(a, 8).is_empty());
        assert_eq!(scatter(0, &pages), 1);
        assert_eq!(views_in(a, 8), (0..8).map(|k| (k, 1)).collect::<Vec<_>>());
        assert_eq!(
            (0..8).map(read).collect::<Vec<_>>(),
            (10..18).collect::<Vec<_>>()
        );

        // Unchanged page in the middle of a scatter run (`pages[3]` at slot 3)
        // is left untouched while slots 2 and 4 are swapped.
        assert_eq!(scatter(2, &[pages[4], pages[3], pages[2]]), 1);
        assert_eq!((2..5).map(read).collect::<Vec<_>>(), [14, 13, 12]);
        assert_eq!(shown(pages[4]), [at(2) as usize]);
        assert_eq!(shown(pages[3]), [at(3) as usize]);
        assert_eq!(shown(pages[2]), [at(4) as usize]);

        // An unknown page fails the call after what comes before it.
        assert_eq!(scatter(2, &[0, 0, 0x10, 0]), 0);
        assert_eq!(query(at(2)).host.State, MEM_RESERVE);
        assert_eq!(query(at(3)).host.State, MEM_RESERVE);
        assert_eq!(read(5), 15, "after the unknown page: as it was");

        assert_eq!(scatter(0, &[0; 8]), 1);
        assert!(views_in(a, 8).is_empty());
        assert!(pages.iter().all(|&p| shown(p).is_empty()));
        assert_eq!(XMemFreePhysicalPages(8, pages.as_ptr()), 1);
    }

    /// A scatter-mapped run is a view per page, and unchanged middle pages are
    /// never unmapped: a title thread writing a live page never faults while
    /// another thread unmaps or remaps the pages around it (no view is split).
    #[test]
    fn scatter_unmapping_a_neighbor_never_faults_a_concurrent_writer() {
        use std::sync::atomic::AtomicBool;
        let pages = allocate(4);
        let a = reserve(4);
        let at = |k: usize| a.wrapping_byte_add(k * PAGE);
        let addresses = [at(0), at(1), at(2), at(3)];
        assert_eq!(
            XMemMapPhysicalPagesScatter(addresses.as_ptr(), 4, pages.as_ptr()),
            1
        );
        assert_eq!(views_in(a, 4), [(0, 1), (1, 1), (2, 1), (3, 1)]);
        let live_addr = at(1) as usize;
        let stop = Arc::new(AtomicBool::new(false));
        let started = Arc::new(AtomicBool::new(false));
        let stop_w = stop.clone();
        let started_w = started.clone();
        let writer = std::thread::spawn(move || {
            let mut writes = 0u64;
            started_w.store(true, Ordering::Release);
            while !stop_w.load(Ordering::Relaxed) {
                // SAFETY: page 1 stays mapped for the entire loop; neither
                // neighbor unmaps nor unchanged-middle scatters may unmap it.
                unsafe { (live_addr as *mut u64).write_volatile(writes) };
                writes += 1;
            }
            writes
        });
        while !started.load(Ordering::Acquire) {
            std::hint::spin_loop();
        }
        for _ in 0..200 {
            // Unmap neighbors 0 and 2 while passing page 1 unchanged in the middle.
            assert_eq!(
                XMemMapPhysicalPagesScatter(addresses.as_ptr(), 3, [0, pages[1], 0].as_ptr()),
                1
            );
            assert_eq!(
                XMemMapPhysicalPagesScatter(
                    addresses.as_ptr(),
                    3,
                    [pages[0], pages[1], pages[2]].as_ptr()
                ),
                1
            );
        }
        stop.store(true, Ordering::Relaxed);
        assert!(writer.join().expect("writer never faulted") > 0);
        assert_eq!(
            XMemMapPhysicalPagesScatter(addresses.as_ptr(), 4, std::ptr::null()),
            1
        );
        assert_eq!(XMemFreePhysicalPages(4, pages.as_ptr()), 1);
    }

    /// `map_alias_constant` unmaps a multi-page view once, leaves surviving
    /// tails and already-matching pages intact; a later `map_alias` over the
    /// 1-page placeholders it left maps a view per page (no coalescing).
    #[test]
    fn map_alias_constant_unmaps_once_and_never_coalesces_placeholders() {
        let pages = allocate(6);
        let pool = reserve(6);
        let dest = reserve(6);
        let at = |base: *mut c_void, k: usize| base.wrapping_byte_add(k * PAGE);
        assert_eq!(XMemMapPhysicalPages(pool, 6, pages.as_ptr()), pool);
        for k in 0..6 {
            // SAFETY: mapped read-write.
            unsafe { at(pool, k).cast::<u8>().write(20 + k as u8) };
        }
        assert!(map_alias(dest as u64, pool as u64, (6 * PAGE) as u64));
        assert_eq!(views_in(dest, 6), [(0, 6)]);

        // Fill middle 4 pages with constant page 0.
        assert!(map_alias_constant(
            at(dest, 1) as u64,
            pool as u64,
            (4 * PAGE) as u64
        ));
        assert_eq!(
            views_in(dest, 6),
            [(0, 1), (1, 1), (2, 1), (3, 1), (4, 1), (5, 1)]
        );
        // SAFETY: all 6 pages are mapped read-write.
        let bytes: Vec<u8> = (0..6)
            .map(|k| unsafe { at(dest, k).cast::<u8>().read() })
            .collect();
        assert_eq!(bytes, [20, 20, 20, 20, 20, 25]);

        // Repeating the same constant fill is a no-op (all pages already match).
        assert!(map_alias_constant(
            at(dest, 1) as u64,
            pool as u64,
            (4 * PAGE) as u64
        ));

        // Mapping the 6 consecutive pool pages back over the 6 1-page
        // placeholders: never coalesced, so a view per page.
        assert!(map_alias(dest as u64, pool as u64, (6 * PAGE) as u64));
        assert_eq!(
            views_in(dest, 6),
            (0..6).map(|k| (k, 1)).collect::<Vec<_>>()
        );
        let restored: Vec<u8> = (0..6)
            .map(|k| unsafe { at(dest, k).cast::<u8>().read() })
            .collect();
        assert_eq!(restored, [20, 21, 22, 23, 24, 25]);

        assert!(map_alias(dest as u64, 0, (6 * PAGE) as u64));
        assert_eq!(XMemMapPhysicalPages(pool, 6, std::ptr::null()), pool);
        assert_eq!(XMemFreePhysicalPages(6, pages.as_ptr()), 1);
    }

    #[test]
    fn persistent_host_view_reuses_mapping_commits_once_and_drops_cleanly() {
        assert!(
            persistent_view_enabled(),
            "persistent view is enabled by default"
        );
        // 70 pages crosses the 64-bit word boundary in `Section::committed`.
        let pages = allocate_whole(70);
        let section = physical().pages[&pages[0]].section.clone();
        let weak = Arc::downgrade(&section);

        // Lazy: `host_view` is not mapped until the first `View::map`.
        assert!(section.host_view.get().is_none());
        assert!(!section.all_committed(0, 70));

        // 1. `write_physical` round-trip through the persistent view.
        let payload: Vec<u8> = (0..PAGE + 137)
            .map(|i| ((i * 13 + 5) & 0xff) as u8)
            .collect();
        assert!(write_physical(&pages, PAGE + 17, &payload));
        let host_base = section
            .host_view
            .get()
            .copied()
            .flatten()
            .expect("persistent host_view mapped on first write");
        assert!(section.all_committed(1, 2));
        assert!(
            !section.all_committed(0, 1),
            "untouched pages stay uncommitted"
        );
        assert!(
            !section.all_committed(3, 1),
            "untouched pages stay uncommitted"
        );

        let p1 = page_bytes(pages[1]);
        let p2 = page_bytes(pages[2]);
        assert!(p1[..17].iter().all(|&b| b == 0));
        assert_eq!(&p1[17..], &payload[..PAGE - 17]);
        assert_eq!(&p2[..payload.len() - (PAGE - 17)], &payload[PAGE - 17..]);
        assert!(p2[payload.len() - (PAGE - 17)..].iter().all(|&b| b == 0));

        // 2. `page_window` round-trip across the 64-page bitmap word boundary.
        let win_data: Vec<u8> = (0..3 * PAGE + 23)
            .map(|i| ((i * 29 + 11) & 0xff) as u8)
            .collect();
        {
            let mut win = page_window(&pages, 62 * PAGE + 11, win_data.len()).expect("one run");
            assert_eq!(win.view.ptr as usize, host_base + 62 * PAGE);
            win.bytes().copy_from_slice(&win_data);
        }
        assert!(section.all_committed(62, 4));
        assert_eq!(section.host_view.get().copied().flatten(), Some(host_base));
        let cross: Vec<u8> = (62..66).flat_map(|i| page_bytes(pages[i])).collect();
        assert!(cross[..11].iter().all(|&b| b == 0));
        assert_eq!(&cross[11..11 + win_data.len()], &win_data[..]);
        assert!(cross[11 + win_data.len()..].iter().all(|&b| b == 0));

        // 3. Repeated writes reuse the exact same `host_view` and hit `all_committed`.
        for iter in 0..4u8 {
            let repeated = vec![0x40 + iter; 256];
            assert!(write_physical(&pages, PAGE + 17, &repeated));
            assert!(section.all_committed(1, 2));
            let slice = physical().pages[&pages[1]].clone();
            let view = View::map(&slice, 2).expect("persistent view");
            assert_eq!(view.ptr as usize, host_base + PAGE);
            assert_eq!(section.host_view.get().copied().flatten(), Some(host_base));
            drop(view);
            assert_eq!(&page_bytes(pages[1])[17..17 + 256], &repeated[..]);
        }

        // 4. Concurrent `page_window` writes across threads to distinct pages
        //    (and disjoint halves of one shared page) of the same section.
        std::thread::scope(|s| {
            for tid in 0..8usize {
                let pages = &pages;
                s.spawn(move || {
                    let base_page = 8 + tid * 4;
                    for pass in 0..4usize {
                        for k in 0..4usize {
                            let page_idx = base_page + k;
                            let fill = ((tid * 17 + k * 31 + pass * 7 + 1) & 0xff) as u8;
                            let mut w = page_window(pages, page_idx * PAGE, PAGE)
                                .expect("concurrent page_window");
                            w.bytes().fill(fill);
                        }
                    }
                });
            }
            // Also verify two threads writing disjoint subranges of page 5 concurrently.
            let pages_ref = &pages;
            s.spawn(move || {
                let mut w = page_window(pages_ref, 5 * PAGE, PAGE / 2).expect("low half");
                w.bytes().fill(0xAA);
            });
            s.spawn(move || {
                let mut w =
                    page_window(pages_ref, 5 * PAGE + PAGE / 2, PAGE / 2).expect("high half");
                w.bytes().fill(0x55);
            });
        });

        assert!(section.all_committed(8, 32));
        for tid in 0..8usize {
            let base_page = 8 + tid * 4;
            for k in 0..4usize {
                let page_idx = base_page + k;
                let expected = ((tid * 17 + k * 31 + 3 * 7 + 1) & 0xff) as u8;
                let bytes = page_bytes(pages[page_idx]);
                assert!(
                    bytes.iter().all(|&b| b == expected),
                    "corruption on thread {tid} page {page_idx}"
                );
            }
        }
        let shared = page_bytes(pages[5]);
        assert!(shared[..PAGE / 2].iter().all(|&b| b == 0xAA));
        assert!(shared[PAGE / 2..].iter().all(|&b| b == 0x55));
        assert_eq!(section.host_view.get().copied().flatten(), Some(host_base));

        // 5. Active `PageWindow` keeps `Section` (and `host_view`) alive across
        //    `XMemFreePhysicalPages`; dropping it drops `Section` and unmaps cleanly.
        let mut live_win = page_window(&pages, 0, PAGE).expect("live window");
        assert_eq!(live_win.view.ptr as usize, host_base);
        drop(section);
        assert_eq!(XMemFreePhysicalPages(pages.len(), pages.as_ptr()), 1);
        assert!(
            weak.upgrade().is_some(),
            "live PageWindow must keep Section and host_view alive"
        );
        live_win.bytes().fill(0xCC);
        drop(live_win);
        assert!(
            weak.upgrade().is_none(),
            "Section dropped and unmapped host_view cleanly once last view dropped"
        );
    }

    #[test]
    fn page_numbers_freed_are_given_again_past_the_title_s_21_bits() {
        let mut table = Physical {
            next_page: PAGE_LIMIT - 2 * FRAMES,
            ..Default::default()
        };
        // The last numbers that fit, as before.
        assert_eq!(table.take(2), Some(PAGE_LIMIT - 2 * FRAMES));
        assert_eq!(table.next_page, PAGE_LIMIT);
        // Then the lowest free run, still packed into 21 bits.
        assert_eq!(table.take(3), Some(FIRST_PAGE));
        assert_eq!(table.take(usize::MAX), None);
        let f = |k: usize| FIRST_PAGE + k * FRAMES;
        assert_eq!(free_run([f(0), f(1), f(3)], FRAMES), Some(f(2)));
        assert_eq!(free_run([f(0), f(1), f(3)], 2 * FRAMES), Some(f(4)));
        assert_eq!(free_run([], PAGE_LIMIT - FIRST_PAGE), Some(FIRST_PAGE));
        assert_eq!(free_run([f(0)], PAGE_LIMIT - FIRST_PAGE), None);
        assert_eq!(free_run([], PAGE_LIMIT - FIRST_PAGE + FRAMES), None);
        let p = free_run([PAGE_LIMIT - FRAMES], FRAMES).unwrap();
        assert_eq!((p >> 4 & 0x1f_ffff) << 4, p);
    }
}

/// Runs its closure when dropped (diagnostics timing of a function with
/// several returns).
#[cfg(feature = "diagnostics")]
struct Defer<F: FnMut()>(F);

#[cfg(feature = "diagnostics")]
impl<F: FnMut()> Drop for Defer<F> {
    fn drop(&mut self) {
        (self.0)();
    }
}
