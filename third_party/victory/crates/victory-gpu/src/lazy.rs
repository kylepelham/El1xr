//! Opt-in Lazy GPU-Resident Write-Back (`VICTORY_LAZY_COPYBACK=1`, default OFF).
//! Submodule of [`super`] (`shadow.rs`) keeping the lazy engine and `host.rs`
//! piece-filtering helpers out of `shadow.rs` / `host.rs`.

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
#[cfg(test)]
use std::sync::atomic::AtomicU8;
use std::sync::{Arc, Condvar, Mutex, OnceLock, Weak};

use windows::Win32::Graphics::Direct3D12::ID3D12Resource;

use crate::host::host;
use crate::watch::PAGE;

use super::{Shadow, copy_from_wc};

pub(crate) const LAZY_EAGER: u8 = 0;
pub(crate) const LAZY_DEFERRED: u8 = 1;
pub(crate) const LAZY_FLUSHING: u8 = 2;
pub(crate) const LAZY_RESYNC: u8 = 3;

#[inline]
pub(crate) fn is_lazy_held(state: u8) -> bool {
    state == LAZY_DEFERRED || state == LAZY_FLUSHING
}

thread_local! {
    static IN_LAZY_VEH: Cell<bool> = const { Cell::new(false) };
}

// ============================================================================
// Opt-in Lazy GPU-Resident Write-Back (`VICTORY_LAZY_COPYBACK=1`, default OFF)
// ============================================================================
//
// Strict Acyclic Lock Hierarchy (deadlock-free with Windows VEH):
//   Level 0: `host.submitted`, `CARD_WORK`, `BUFFERS`, `LOOSE`, `EXACT`,
//            `CARDED`, `Shadow::{copied, pending, before, kept, card.*}`,
//            `watch::{JOURNAL, MAPPING, GUARDS}`.
//   Level 1: `LAZY_TABLE` (metadata-only leaf table: armed/busy pages, pinned
//            fence pages, active remap barriers, and flushed 64 KiB baselines).
//            NEVER acquires Level 0 or Level 2 while held, and NEVER calls
//            D3D12 or waits on GPU work.
//   Level 2: `LAZY_GPU` (dedicated D3D12 allocator, command list, fence, event,
//            and persistent 64 KiB `D3D12_HEAP_TYPE_READBACK` buffer). NEVER
//            acquires Level 0 or Level 1 while held.
//   Level 3: `FlushWait` (`Mutex<Option<FlushOutcome>>` + `Condvar`). Leaf only.
//
// VEH (`lazy_on_fault`) ONLY acquires Level 1 -> drops -> Level 2 -> drops ->
// `VirtualProtect` + `memcpy` -> Level 1 -> drops -> Level 3. It NEVER touches
// any Level 0 lock (`watch`, `Shadow`, `CARD_WORK`, `host.submitted`).

/// Whether a `VICTORY_BACK_READS` environment value enables copy-back read
/// diagnostics (`"1"`, `"2"`, `"alt"`, or any non-empty value other than `"0"`).
/// Whether opt-in Lazy GPU-Resident Write-Back is enabled
/// (`VICTORY_LAZY_COPYBACK=1`; default OFF).
pub(crate) fn lazy_copyback() -> bool {
    #[cfg(test)]
    {
        match LAZY_TEST_OVERRIDE.load(Ordering::Acquire) {
            1 => return false,
            2 if lazy_test_scoped() => return true,
            _ => {}
        }
    }
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_LAZY_COPYBACK").is_some_and(|v| v == "1"))
}

#[cfg(test)]
static LAZY_TEST_OVERRIDE: AtomicU8 = AtomicU8::new(0);

// A lazy test's override (`LazyTestGuard::enable`) is for the work of that
// test: its own thread, the threads it starts, the code's own threads, and
// the wide jobs queued from any of them. The other tests libtest runs at the
// same time (on threads named after the test, `module::test`) go on without
// it, as they would without the lazy test: before, they took lazy holds that
// kept the lazy test's pages from arming, and made opted-in shadows whose
// pages armed under them, depending on when they ran (#52).
#[cfg(test)]
thread_local! {
    /// 0: by the thread's name; 1: without the override; 2: with it.
    static LAZY_TEST_SCOPE: std::cell::Cell<u8> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn lazy_test_scoped() -> bool {
    match LAZY_TEST_SCOPE.with(|s| s.get()) {
        1 => false,
        2 => true,
        _ => !std::thread::current().name().is_some_and(|n| n.contains("::")),
    }
}

/// This thread's scope, to run a wide job queued from it in (`shadow::wide`).
#[cfg(test)]
pub(super) fn lazy_test_scope_now() -> u8 {
    if lazy_test_scoped() { 2 } else { 1 }
}

/// Runs this thread in `scope` until dropped.
#[cfg(test)]
pub(super) struct LazyTestScope(u8);

#[cfg(test)]
impl LazyTestScope {
    pub(super) fn enter(scope: u8) -> Self {
        Self(LAZY_TEST_SCOPE.with(|s| s.replace(scope)))
    }
}

#[cfg(test)]
impl Drop for LazyTestScope {
    fn drop(&mut self) {
        LAZY_TEST_SCOPE.with(|s| s.set(self.0));
    }
}

#[cfg(test)]
type PostTransitionHook = Box<dyn FnOnce(usize) + Send>;

#[cfg(test)]
pub(super) static TRY_DEFER_POST_TRANSITION_HOOK: Mutex<Option<PostTransitionHook>> = Mutex::new(None);

#[cfg(test)]
static LAZY_TEST_LOCK: Mutex<()> = Mutex::new(());

#[cfg(test)]
pub(crate) struct LazyTestGuard {
    prev: u8,
    _scope: LazyTestScope,
    _lock: std::sync::MutexGuard<'static, ()>,
}

#[cfg(test)]
#[allow(dead_code)]
pub(crate) fn test_disable_lazy_copyback() -> LazyTestGuard {
    LazyTestGuard::disable()
}

#[cfg(test)]
impl LazyTestGuard {
    pub(super) fn enable() -> Self {
        let lock = LAZY_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = LAZY_TEST_OVERRIDE.swap(2, Ordering::AcqRel);
        Self {
            prev,
            _scope: LazyTestScope::enter(2),
            _lock: lock,
        }
    }

    pub(super) fn disable() -> Self {
        let lock = LAZY_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = LAZY_TEST_OVERRIDE.swap(1, Ordering::AcqRel);
        Self {
            prev,
            _scope: LazyTestScope::enter(2),
            _lock: lock,
        }
    }
}

#[cfg(test)]
impl Drop for LazyTestGuard {
    fn drop(&mut self) {
        LAZY_TEST_OVERRIDE.store(self.prev, Ordering::Release);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FlushOutcome {
    /// Page has been flushed/unprotected (`PAGE_READWRITE`) and is accessible.
    Accessible,
    /// Arming committed (`PAGE_NOACCESS`): waiting faults must retry lookup.
    ArmedRetry,
}

pub(super) struct FlushWait {
    state: Mutex<Option<FlushOutcome>>,
    ready: Condvar,
}

impl FlushWait {
    fn new() -> Self {
        Self {
            state: Mutex::new(None),
            ready: Condvar::new(),
        }
    }

    fn wait(&self) -> FlushOutcome {
        let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(outcome) = *guard {
                return outcome;
            }
            guard = self.ready.wait(guard).unwrap_or_else(|e| e.into_inner());
        }
    }

    fn finish(&self, outcome: FlushOutcome) {
        let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        *guard = Some(outcome);
        self.ready.notify_all();
    }
}

struct ArmedPage {
    shadow: Weak<Shadow>,
    shadow_id: u64,
    offset: usize,
    page_idx: usize,
    /// When the armed page is a single-view `MEM_MAPPED` section (`inplace::Cover::Run`),
    /// stores `(section_handle, section_offset, section_bytes)` so VEH / flush can
    /// write all 64 KiB through our persistent writable `inplace::OWN` alias view
    /// BEFORE flipping the title view from `PAGE_NOACCESS` to `PAGE_READWRITE` —
    /// zero torn-read window and zero per-page `MapViewOfFile`/`UnmapViewOfFile`.
    section_view: Option<(usize, u64, u64)>,
    /// `true` iff this page was armed via Path A (`MEM_MAPPED` clean `watch` view),
    /// `false` for Path B (`MEM_PRIVATE | MEM_WRITE_WATCH`). Used by `watch::known_view`
    /// and `watch::guard` to keep steady-state `MEM_MAPPED` refreshes lock-free without
    /// ever misclassifying `MEM_PRIVATE` pages as view memory.
    is_mapped_view: bool,
}

enum LazyEntry {
    Armed(ArmedPage),
    Busy {
        shadow_id: u64,
        wait: Arc<FlushWait>,
    },
}

/// Per-page baseline flushed from GPU into title memory, paired with the
/// unapplied byte intervals inside `[0, PAGE)` so `lazy_apply_flushed_baseline`
/// applies each byte at most once (idempotent across consecutive relay merges
/// or overlapping ranges) while keeping `LAZY_RESYNC` active until the next
/// `Shadow::update` / `Shadow::writing`.
pub(super) struct FlushedBaseline {
    bytes: Vec<u8>,
    remaining: Vec<(usize, usize)>,
}

impl FlushedBaseline {
    fn new(bytes: Vec<u8>) -> Self {
        Self {
            bytes,
            remaining: vec![(0, PAGE)],
        }
    }

    /// Copies any unapplied subranges intersecting `[src_lo, src_hi)` into `dst`
    /// (where `dst[0]` corresponds to `src_lo`), removing them from `remaining`.
    /// Returns `true` when every byte in `[0, PAGE)` has been applied.
    fn apply_once(&mut self, src_lo: usize, src_hi: usize, dst: &mut [u8]) -> bool {
        let mut next = Vec::with_capacity(self.remaining.len() + 1);
        for &(r_lo, r_hi) in &self.remaining {
            if r_hi <= src_lo || r_lo >= src_hi {
                next.push((r_lo, r_hi));
                continue;
            }
            if r_lo < src_lo {
                next.push((r_lo, src_lo));
            }
            let o_lo = r_lo.max(src_lo);
            let o_hi = r_hi.min(src_hi);
            let dst_off = o_lo - src_lo;
            let n = o_hi - o_lo;
            dst[dst_off..dst_off + n].copy_from_slice(&self.bytes[o_lo..o_hi]);
            if r_hi > src_hi {
                next.push((src_hi, r_hi));
            }
        }
        self.remaining = next;
        self.remaining.is_empty()
    }
}

pub(super) struct LazyTable {
    pages: BTreeMap<usize, LazyEntry>,
    pinned: BTreeSet<usize>,
    /// Active `watch::title_memory_remapping` / `lazy_flush_and_hold` page
    /// intervals `(id, first_page, last_page)`.
    remapping: Vec<(u64, usize, usize)>,
    next_remap_id: u64,
    pub(super) flushed_baselines: BTreeMap<(u64, usize), FlushedBaseline>,
}

impl LazyTable {
    const fn new() -> Self {
        Self {
            pages: BTreeMap::new(),
            pinned: BTreeSet::new(),
            remapping: Vec::new(),
            next_remap_id: 1,
            flushed_baselines: BTreeMap::new(),
        }
    }

    fn is_remapping(&self, page: usize) -> bool {
        self.remapping.iter().any(|&(_, lo, hi)| lo <= page && page < hi)
    }
}

/// Level 1 metadata table for lazy GPU-resident pages.
pub(super) static LAZY_TABLE: Mutex<LazyTable> = Mutex::new(LazyTable::new());

/// Lock-free count of currently armed or busy lazy pages (0 when inactive).
pub(crate) static LAZY_ARMED_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Number of lock-free hash stripes for O(1) contention-free deferred-page
/// queries and Dekker-synchronized host access holds.
const LAZY_STRIPES: usize = 1024;
const LAZY_STRIPE_MASK: usize = LAZY_STRIPES - 1;

/// Exact count of `LAZY_TABLE.pages` entries hashing to `page & LAZY_STRIPE_MASK`.
/// Maintained under `LAZY_TABLE` via [`lazy_table_insert_busy`] and
/// [`lazy_table_remove_page`].
static LAZY_ARMED_STRIPES: [std::sync::atomic::AtomicU32; LAZY_STRIPES] =
    [const { std::sync::atomic::AtomicU32::new(0) }; LAZY_STRIPES];

/// Lock-free host access hold counters (per-stripe for ranges `<= 8` pages,
/// global for larger ranges). Pairs with `LAZY_ARMED_STRIPES` via `SeqCst`
/// Dekker ordering so `host::read_into`/`copy_mapped`/`write_mapped`/`copy_fill`
/// never acquire `LAZY_TABLE` on the common non-deferred path while still
/// strictly blocking concurrent relay `try_defer_piece_relay` arming.
static LAZY_HOLD_STRIPES: [std::sync::atomic::AtomicU32; LAZY_STRIPES] =
    [const { std::sync::atomic::AtomicU32::new(0) }; LAZY_STRIPES];
static LAZY_HOLD_GLOBAL: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Monotonic count of completed lazy flushes (for stale-fault disambiguation).
static LAZY_FLUSH_TOTAL: AtomicU64 = AtomicU64::new(0);

/// Monotonic counters: `[0]=newly_armed, [1]=steady_state_hits,
/// [2]=veh_read_faults, [3]=veh_write_faults, [4]=explicit_flushes,
/// [5]=bytes_flushed`.
pub(super) static LAZY_STATS: [AtomicU64; 6] = [const { AtomicU64::new(0) }; 6];

/// Additional monotonic diagnostic totals: `[0]=self_bypass_hits,
/// [1]=materialize_us, [2]=protect_us, [3]=protect_calls`.
static LAZY_EXTRA_STATS: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];
/// Nanosecond accumulator so sub-microsecond `VirtualProtect` calls are never
/// truncated away from `lazy_protect_ms`.
static LAZY_PROTECT_NS_TOTAL: AtomicU64 = AtomicU64::new(0);

/// Per-frame-window counters swapped in [`take_lazy_interval`]:
/// `[0]=lazy_deferred_pages, [1]=lazy_veh_faults, [2]=lazy_self_bypass_hits,
/// [3]=lazy_bytes_readback, [4]=lazy_materialize_us, [5]=lazy_protect_us`.
static LAZY_METER_STATS: [AtomicU64; 6] = [const { AtomicU64::new(0) }; 6];

/// Per-frame lazy copy-back diagnostic figures for `frametime::Row` and logging.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct LazyTelemetryInterval {
    pub deferred_pages: f64,
    pub veh_faults: f64,
    pub self_bypass_hits: f64,
    pub bytes_readback: f64,
    pub materialize_ms: f64,
    pub protect_ms: f64,
}

fn lazy_record_deferred(pages: u64) {
    if pages == 0 {
        return;
    }
    LAZY_METER_STATS[0].fetch_add(pages, Ordering::Relaxed);
}

fn lazy_record_veh_fault(is_write: bool) {
    LAZY_STATS[if is_write { 3 } else { 2 }].fetch_add(1, Ordering::Relaxed);
    LAZY_METER_STATS[1].fetch_add(1, Ordering::Relaxed);
}

fn lazy_record_self_bypass(pages: u64) {
    if pages == 0 {
        return;
    }
    LAZY_EXTRA_STATS[0].fetch_add(pages, Ordering::Relaxed);
    LAZY_METER_STATS[2].fetch_add(pages, Ordering::Relaxed);
}

fn lazy_record_bytes_readback(bytes: u64) {
    if bytes == 0 {
        return;
    }
    LAZY_STATS[5].fetch_add(bytes, Ordering::Relaxed);
    LAZY_METER_STATS[3].fetch_add(bytes, Ordering::Relaxed);
}

fn lazy_record_materialize_us(us: u64) {
    if us == 0 {
        return;
    }
    LAZY_EXTRA_STATS[1].fetch_add(us, Ordering::Relaxed);
    LAZY_METER_STATS[4].fetch_add(us, Ordering::Relaxed);
    crate::hitch::add(crate::hitch::Phase::Lazy, us);
}

/// Unconditionally records one actual `VirtualProtect` syscall and accumulates
/// its duration in nanoseconds (flushing whole microseconds without sub-us loss).
pub(crate) fn lazy_record_protect_ns(ns: u64) {
    LAZY_EXTRA_STATS[3].fetch_add(1, Ordering::Relaxed);
    let prev = LAZY_PROTECT_NS_TOTAL.fetch_add(ns, Ordering::Relaxed);
    let us_delta = (prev.saturating_add(ns) / 1_000).saturating_sub(prev / 1_000);
    if us_delta > 0 {
        LAZY_EXTRA_STATS[2].fetch_add(us_delta, Ordering::Relaxed);
        LAZY_METER_STATS[5].fetch_add(us_delta, Ordering::Relaxed);
    }
}

/// Takes and resets per-frame lazy copy-back diagnostic figures over `frames`.
pub(crate) fn take_lazy_interval(frames: f64) -> LazyTelemetryInterval {
    let f = frames.max(1.0);
    let s: [u64; 6] = std::array::from_fn(|i| LAZY_METER_STATS[i].swap(0, Ordering::Relaxed));
    LazyTelemetryInterval {
        deferred_pages: s[0] as f64 / f,
        veh_faults: s[1] as f64 / f,
        self_bypass_hits: s[2] as f64 / f,
        bytes_readback: s[3] as f64 / f,
        materialize_ms: s[4] as f64 / 1e3 / f,
        protect_ms: s[5] as f64 / 1e3 / f,
    }
}

/// Logs per-frame lazy copy-back diagnostics (`Meter::frame`) whenever
/// `VICTORY_LAZY_COPYBACK=1` is active or any lazy work occurred in the window.
pub(crate) fn lazy_line(_frames: f64, iv: &LazyTelemetryInterval) {
    let resident = LAZY_ARMED_COUNT.load(Ordering::Relaxed);
    let any = iv.deferred_pages > 0.0
        || iv.veh_faults > 0.0
        || iv.self_bypass_hits > 0.0
        || iv.bytes_readback > 0.0
        || iv.materialize_ms > 0.0
        || iv.protect_ms > 0.0;
    if !lazy_copyback() && resident == 0 && !any {
        return;
    }
    tracing::info!(
        "lazy copy-back per frame: lazy_deferred_pages={:.0}, lazy_veh_faults={:.1}, \
         lazy_self_bypass_hits={:.1}, lazy_bytes_readback={:.0}, lazy_materialize_ms={:.2}, \
         lazy_protect_ms={:.2} (resident_pages={resident})",
        iv.deferred_pages,
        iv.veh_faults,
        iv.self_bypass_hits,
        iv.bytes_readback,
        iv.materialize_ms,
        iv.protect_ms,
    );
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct LazyTelemetrySnapshot {
    pub(super) deferred_pages: u64,
    pub(super) veh_faults: u64,
    pub(super) self_bypass_hits: u64,
    pub(super) materialize_us: u64,
    pub(super) protect_us: u64,
    pub(super) protect_calls: u64,
    pub(super) bytes_flushed: u64,
}

#[cfg(test)]
pub(super) fn lazy_telemetry_snapshot() -> LazyTelemetrySnapshot {
    LazyTelemetrySnapshot {
        deferred_pages: LAZY_STATS[0].load(Ordering::Relaxed) + LAZY_STATS[1].load(Ordering::Relaxed),
        veh_faults: LAZY_STATS[2].load(Ordering::Relaxed) + LAZY_STATS[3].load(Ordering::Relaxed),
        self_bypass_hits: LAZY_EXTRA_STATS[0].load(Ordering::Relaxed),
        materialize_us: LAZY_EXTRA_STATS[1].load(Ordering::Relaxed),
        protect_us: LAZY_EXTRA_STATS[2].load(Ordering::Relaxed),
        protect_calls: LAZY_EXTRA_STATS[3].load(Ordering::Relaxed),
        bytes_flushed: LAZY_STATS[5].load(Ordering::Relaxed),
    }
}

/// Lock-free 2-level bitset of currently armed `MEM_MAPPED` (`is_mapped_view`)
/// 64 KiB pages for O(1) contention-free `watch::known_view` and `watch::guard`
/// steady-state checks (47-bit Windows user VA = `2^31` 64 KiB pages = `2^15`
/// parts of `1024 * 64` bits each; zero heap allocation until a `MEM_MAPPED`
/// page is armed).
const LAZY_MAPPED_PART_WORDS: usize = 1024;
const LAZY_MAPPED_PART_PAGES: usize = LAZY_MAPPED_PART_WORDS * 64;
struct LazyMappedPart([AtomicU64; LAZY_MAPPED_PART_WORDS]);
static LAZY_MAPPED_VIEW: [std::sync::atomic::AtomicPtr<LazyMappedPart>; 1 << 15] =
    [const { std::sync::atomic::AtomicPtr::new(std::ptr::null_mut()) }; 1 << 15];
static LAZY_MAPPED_COUNT: AtomicUsize = AtomicUsize::new(0);

fn lazy_mapped_part(page: usize, make: bool) -> Option<&'static LazyMappedPart> {
    let slot = LAZY_MAPPED_VIEW.get(page / LAZY_MAPPED_PART_PAGES)?;
    let mut ptr = slot.load(Ordering::Acquire);
    if ptr.is_null() {
        if !make {
            return None;
        }
        let layout = std::alloc::Layout::new::<LazyMappedPart>();
        // SAFETY: non-zero layout; zeroed memory is a valid `[AtomicU64; 1024]`.
        let fresh = unsafe { std::alloc::alloc_zeroed(layout) }.cast::<LazyMappedPart>();
        if fresh.is_null() {
            return None;
        }
        match slot.compare_exchange(std::ptr::null_mut(), fresh, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => ptr = fresh,
            Err(other) => {
                // SAFETY: ours alone, just allocated with `layout`.
                unsafe { std::alloc::dealloc(fresh.cast(), layout) };
                ptr = other;
            }
        }
    }
    // SAFETY: parts live for `'static` once published.
    Some(unsafe { &*ptr })
}

fn lazy_mapped_view_set(page: usize, on: bool) {
    let Some(part) = lazy_mapped_part(page, on) else {
        return;
    };
    let word = (page % LAZY_MAPPED_PART_PAGES) / 64;
    let bit = 1u64 << (page & 63);
    if on {
        let prev = part.0[word].fetch_or(bit, Ordering::Release);
        if prev & bit == 0 {
            LAZY_MAPPED_COUNT.fetch_add(1, Ordering::Release);
        }
    } else {
        let prev = part.0[word].fetch_and(!bit, Ordering::Release);
        if prev & bit != 0 {
            LAZY_MAPPED_COUNT.fetch_sub(1, Ordering::Release);
        }
    }
}

/// Lock-free check: whether any `MEM_MAPPED` lazy page is currently armed.
#[inline]
pub(crate) fn has_lazy_mapped_view_pages() -> bool {
    LAZY_MAPPED_COUNT.load(Ordering::Acquire) > 0
}

/// Lock-free O(1) check: whether 64 KiB `page` is currently armed as a
/// `MEM_MAPPED` lazy view page (always `false` for `MEM_PRIVATE` pages).
#[inline]
pub(crate) fn is_lazy_mapped_view_page(page: usize) -> bool {
    if LAZY_MAPPED_COUNT.load(Ordering::Acquire) == 0 {
        return false;
    }
    let Some(part) = lazy_mapped_part(page, false) else {
        return false;
    };
    let word = (page % LAZY_MAPPED_PART_PAGES) / 64;
    let bit = 1u64 << (page & 63);
    part.0[word].load(Ordering::Acquire) & bit != 0
}

fn lazy_table_insert_busy(table: &mut LazyTable, page: usize, shadow_id: u64, wait: Arc<FlushWait>) {
    let prev = table.pages.insert(page, LazyEntry::Busy { shadow_id, wait });
    match prev {
        None => {
            // Both `LAZY_ARMED_COUNT` and `LAZY_ARMED_STRIPES` use `SeqCst` so the
            // Dekker pair with `LAZY_HOLD_GLOBAL`/`LAZY_HOLD_STRIPES` (`SeqCst`) is
            // strictly in the global `SeqCst` total order for all range sizes.
            LAZY_ARMED_COUNT.fetch_add(1, Ordering::SeqCst);
            LAZY_ARMED_STRIPES[page & LAZY_STRIPE_MASK].fetch_add(1, Ordering::SeqCst);
        }
        Some(LazyEntry::Armed(a)) if a.is_mapped_view => {
            lazy_mapped_view_set(page, false);
        }
        Some(_) => {}
    }
}

fn lazy_table_insert_armed(table: &mut LazyTable, page: usize, armed: ArmedPage) {
    let is_mapped = armed.is_mapped_view;
    let prev = table.pages.insert(page, LazyEntry::Armed(armed));
    if prev.is_none() {
        LAZY_ARMED_COUNT.fetch_add(1, Ordering::SeqCst);
        LAZY_ARMED_STRIPES[page & LAZY_STRIPE_MASK].fetch_add(1, Ordering::SeqCst);
    }
    if is_mapped {
        lazy_mapped_view_set(page, true);
    } else if matches!(prev, Some(LazyEntry::Armed(a)) if a.is_mapped_view) {
        lazy_mapped_view_set(page, false);
    }
}

fn lazy_table_remove_page(table: &mut LazyTable, page: usize) -> Option<LazyEntry> {
    let prev = table.pages.remove(&page);
    if let Some(ref entry) = prev {
        if matches!(entry, LazyEntry::Armed(a) if a.is_mapped_view) {
            lazy_mapped_view_set(page, false);
        }
        LAZY_ARMED_STRIPES[page & LAZY_STRIPE_MASK].fetch_sub(1, Ordering::Release);
        LAZY_ARMED_COUNT.fetch_sub(1, Ordering::AcqRel);
    }
    prev
}

#[inline]
fn lazy_is_held(page: usize) -> bool {
    LAZY_HOLD_GLOBAL.load(Ordering::SeqCst) > 0
        || LAZY_HOLD_STRIPES[page & LAZY_STRIPE_MASK].load(Ordering::SeqCst) > 0
}

#[inline]
fn lazy_range_may_have_armed(first: usize, last: usize) -> bool {
    if LAZY_ARMED_COUNT.load(Ordering::SeqCst) == 0 || first >= last {
        return false;
    }
    if last - first <= 16 {
        (first..last).any(|p| LAZY_ARMED_STRIPES[p & LAZY_STRIPE_MASK].load(Ordering::SeqCst) > 0)
    } else {
        true
    }
}

enum HoldKind {
    None,
    Small(usize, usize),
    Global,
}

/// Zero-allocation, lock-free host access hold guard preventing concurrent
/// relay `try_defer_piece_relay` from arming `PAGE_NOACCESS` on pages covered
/// by `[at, at + len)`.
pub struct LazyAccessHold {
    kind: HoldKind,
}

impl Drop for LazyAccessHold {
    fn drop(&mut self) {
        match self.kind {
            HoldKind::None => {}
            HoldKind::Small(first, last) => {
                for p in first..last {
                    LAZY_HOLD_STRIPES[p & LAZY_STRIPE_MASK].fetch_sub(1, Ordering::Release);
                }
            }
            HoldKind::Global => {
                LAZY_HOLD_GLOBAL.fetch_sub(1, Ordering::Release);
            }
        }
    }
}

/// Acquires a lock-free host access hold for `[at, at + len)`. When
/// `VICTORY_LAZY_COPYBACK` is OFF, executes zero atomics and zero locks.
pub(crate) fn lazy_hold_access(at: usize, len: usize) -> LazyAccessHold {
    if len == 0 || !lazy_copyback() {
        return LazyAccessHold { kind: HoldKind::None };
    }
    let first = at / PAGE;
    let last = at.saturating_add(len).div_ceil(PAGE);
    if last - first <= 8 {
        for p in first..last {
            LAZY_HOLD_STRIPES[p & LAZY_STRIPE_MASK].fetch_add(1, Ordering::SeqCst);
        }
        LazyAccessHold {
            kind: HoldKind::Small(first, last),
        }
    } else {
        LAZY_HOLD_GLOBAL.fetch_add(1, Ordering::SeqCst);
        LazyAccessHold { kind: HoldKind::Global }
    }
}

/// Number of 64 KiB pages (4 MiB) batched per single `LazyReadback` GPU submit.
const LAZY_READBACK_BATCH_PAGES: usize = 64;

/// Dedicated GPU readback context (Level 2), pre-allocated before any
/// `PAGE_NOACCESS` page is ever armed so VEH never allocates D3D12 objects.
///
/// Uses a dedicated `D3D12_COMMAND_LIST_TYPE_COPY` queue (falling back to
/// `host.queue` only if the device refuses a copy queue) so on-demand
/// materialization executes immediately on the hardware copy engine instead of
/// stalling behind queued render flights on the direct queue, and batches up to
/// 4 MiB (`LAZY_READBACK_BATCH_PAGES` pages) into a single GPU submit.
struct LazyReadback {
    queue: windows::Win32::Graphics::Direct3D12::ID3D12CommandQueue,
    allocator: windows::Win32::Graphics::Direct3D12::ID3D12CommandAllocator,
    list: windows::Win32::Graphics::Direct3D12::ID3D12GraphicsCommandList,
    fence: windows::Win32::Graphics::Direct3D12::ID3D12Fence,
    event: windows::Win32::Foundation::HANDLE,
    host_wait_event: windows::Win32::Foundation::HANDLE,
    fence_value: u64,
    readback: ID3D12Resource,
    mapped: *const u8,
}

// SAFETY: accessed exclusively under `LAZY_GPU` mutex; D3D12 objects are
// free-threaded and `event`/`host_wait_event` are only waited on under `LAZY_GPU`.
unsafe impl Send for LazyReadback {}

impl Drop for LazyReadback {
    fn drop(&mut self) {
        // SAFETY: `readback` was mapped in `LazyReadback::new` and events are ours.
        unsafe {
            self.readback.Unmap(0, None);
            let _ = windows::Win32::Foundation::CloseHandle(self.event);
            let _ = windows::Win32::Foundation::CloseHandle(self.host_wait_event);
        }
    }
}

impl LazyReadback {
    fn new() -> Option<Self> {
        use windows::Win32::Graphics::Direct3D12::*;
        use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;
        use windows::Win32::System::Threading::CreateEventW;

        let host = host()?;
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_READBACK,
            ..Default::default()
        };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: (LAZY_READBACK_BATCH_PAGES * PAGE) as u64,
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            ..Default::default()
        };
        // SAFETY: plain D3D12 object creation on `host.device`.
        unsafe {
            let copy_desc = D3D12_COMMAND_QUEUE_DESC {
                Type: D3D12_COMMAND_LIST_TYPE_COPY,
                ..Default::default()
            };
            let (queue, list_type) = match host.device.CreateCommandQueue::<ID3D12CommandQueue>(&copy_desc) {
                Ok(q) => (q, D3D12_COMMAND_LIST_TYPE_COPY),
                Err(_) => (host.queue().clone(), D3D12_COMMAND_LIST_TYPE_DIRECT),
            };
            let allocator: ID3D12CommandAllocator = host.device.CreateCommandAllocator(list_type).ok()?;
            crate::alloc_count::track(&allocator);
            let list: ID3D12GraphicsCommandList =
                host.device.CreateCommandList(0, list_type, &allocator, None).ok()?;
            list.Close().ok()?;
            let fence: ID3D12Fence = host.device.CreateFence(0, D3D12_FENCE_FLAG_NONE).ok()?;
            let event = CreateEventW(None, false, false, None).ok()?;
            let host_wait_event = match CreateEventW(None, false, false, None) {
                Ok(ev) => ev,
                Err(_) => {
                    let _ = windows::Win32::Foundation::CloseHandle(event);
                    return None;
                }
            };
            let mut readback: Option<ID3D12Resource> = None;
            if host
                .device
                .CreateCommittedResource(
                    &heap,
                    D3D12_HEAP_FLAG_NONE,
                    &desc,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                    None,
                    &mut readback,
                )
                .is_err()
            {
                let _ = windows::Win32::Foundation::CloseHandle(event);
                let _ = windows::Win32::Foundation::CloseHandle(host_wait_event);
                return None;
            }
            let readback = match readback {
                Some(r) => r,
                None => {
                    let _ = windows::Win32::Foundation::CloseHandle(event);
                    let _ = windows::Win32::Foundation::CloseHandle(host_wait_event);
                    return None;
                }
            };
            let mut mapped = std::ptr::null_mut();
            if readback.Map(0, None, Some(&mut mapped)).is_err() {
                let _ = windows::Win32::Foundation::CloseHandle(event);
                let _ = windows::Win32::Foundation::CloseHandle(host_wait_event);
                return None;
            }
            crate::host::name(&readback, || "lazy copy-back 4 MiB readback".into());
            Some(Self {
                queue,
                allocator,
                list,
                fence,
                event,
                host_wait_event,
                fence_value: 0,
                readback,
                mapped: mapped.cast(),
            })
        }
    }

    /// Reads `reqs.len()` 64 KiB pages into `dst` (`dst.len() == reqs.len() * PAGE`),
    /// strictly synchronized against any in-flight async writes on `host.queue`
    /// (lock-free via `host.submitted_fence()` + `shadow.last_write_fence` +
    /// `host.fence.GetCompletedValue()`, never locking `host.submitted`):
    /// - When `host.fence` has already passed every required write fence: ZERO wait.
    /// - When a non-carded shadow has an in-flight write on `host.queue`: waits on
    ///   `host.fence` before CPU-copying `shadow.data`.
    /// - When a carded shadow has an in-flight write on `host.queue`: issues
    ///   cross-queue `self.queue.Wait(&host.fence, req_fence)` before
    ///   `ExecuteCommandLists` so D3D12 multi-engine ordering and Frame N+1 data
    ///   coherence hold with zero debug layer errors.
    fn read_pages(&mut self, reqs: &[(&Shadow, usize)], dst: &mut [u8]) -> bool {
        use windows::Win32::Graphics::Direct3D12::ID3D12CommandList;
        use windows::Win32::System::Threading::{INFINITE, WaitForSingleObject};
        use windows::core::Interface;

        if reqs.is_empty() {
            return true;
        }
        if dst.len() != reqs.len() * PAGE {
            return false;
        }
        let Some(host) = host() else {
            return false;
        };
        let latest_host_fence = host.submitted_fence();
        let mut non_carded_req = 0u64;
        let mut carded_req = 0u64;
        let mut carded_indices: Vec<usize> = Vec::new();
        let mut non_carded_indices: Vec<usize> = Vec::new();
        for (i, &(shadow, offset)) in reqs.iter().enumerate() {
            if offset.saturating_add(PAGE) > shadow.len as usize {
                return false;
            }
            let wf = shadow.last_write_fence.load(Ordering::Acquire);
            let need = if shadow.writes_pending() {
                wf.max(latest_host_fence)
            } else {
                wf
            };
            if shadow.card.is_some() {
                carded_req = carded_req.max(need);
                carded_indices.push(i);
            } else {
                non_carded_req = non_carded_req.max(need);
                non_carded_indices.push(i);
            }
        }

        if !non_carded_indices.is_empty() {
            // SAFETY: free-threaded query/wait on `host.fence` using our exclusive
            // `host_wait_event` under `LAZY_GPU`; never locks `host.submitted`.
            let completed = unsafe { host.fence.GetCompletedValue() };
            if non_carded_req > completed {
                let waited = unsafe {
                    (|| -> windows::core::Result<()> {
                        host.fence
                            .SetEventOnCompletion(non_carded_req, self.host_wait_event)?;
                        WaitForSingleObject(self.host_wait_event, INFINITE);
                        Ok(())
                    })()
                };
                if waited.is_err() {
                    return false;
                }
            }
            for i in non_carded_indices {
                let (shadow, offset) = reqs[i];
                // SAFETY: `offset + PAGE <= shadow.len` checked above and any
                // in-flight write on `host.queue` has completed.
                unsafe {
                    let dst_ptr = dst.as_mut_ptr().add(i * PAGE);
                    if shadow.data.is_null() {
                        continue;
                    }
                    if shadow.rebar {
                        copy_from_wc(shadow.data.add(offset), dst_ptr, PAGE);
                    } else {
                        std::ptr::copy_nonoverlapping(shadow.data.add(offset), dst_ptr, PAGE);
                    }
                }
            }
        }
        if carded_indices.is_empty() {
            return true;
        }
        let mut waited_cross_queue = false;
        for chunk in carded_indices.chunks(LAZY_READBACK_BATCH_PAGES) {
            // SAFETY: exclusive ownership under `LAZY_GPU`; `self.queue.Wait`
            // strictly orders behind any in-flight `host.queue` write so every
            // `shadow.buffer` is in `COMMON` before `CopyBufferRegion` promotes it.
            let ok = unsafe {
                (|| -> windows::core::Result<()> {
                    if !waited_cross_queue && carded_req > host.fence.GetCompletedValue() {
                        // Its signal on the host queue first (`VICTORY_SEND_THREAD`):
                        // waited for on the host queue itself, it would never come.
                        crate::host::send_thread::wait_idle();
                        self.queue.Wait(&host.fence, carded_req)?;
                        waited_cross_queue = true;
                    }
                    self.allocator.Reset()?;
                    self.list.Reset(&self.allocator, None)?;
                    let mut c = 0usize;
                    while c < chunk.len() {
                        let (shadow, start_off) = reqs[chunk[c]];
                        let mut run_pages = 1usize;
                        while c + run_pages < chunk.len() {
                            let (next_shadow, next_off) = reqs[chunk[c + run_pages]];
                            if std::ptr::eq(shadow, next_shadow)
                                && next_off == start_off + run_pages * PAGE
                            {
                                run_pages += 1;
                            } else {
                                break;
                            }
                        }
                        self.list.CopyBufferRegion(
                            &self.readback,
                            (c * PAGE) as u64,
                            &shadow.buffer,
                            start_off as u64,
                            (run_pages * PAGE) as u64,
                        );
                        c += run_pages;
                    }
                    self.list.Close()?;
                    let cmd: ID3D12CommandList = self.list.cast()?;
                    // The host queue when there is no copy queue (`VICTORY_SEND_THREAD`).
                    crate::host::send_thread::wait_idle();
                    self.queue.ExecuteCommandLists(&[Some(cmd)]);
                    self.fence_value += 1;
                    self.queue.Signal(&self.fence, self.fence_value)?;
                    self.fence.SetEventOnCompletion(self.fence_value, self.event)?;
                    WaitForSingleObject(self.event, INFINITE);
                    for (slot, &req_idx) in chunk.iter().enumerate() {
                        std::ptr::copy_nonoverlapping(
                            self.mapped.add(slot * PAGE),
                            dst.as_mut_ptr().add(req_idx * PAGE),
                            PAGE,
                        );
                    }
                    Ok(())
                })()
            };
            if ok.is_err() {
                return false;
            }
        }
        true
    }

    fn read_page(&mut self, shadow: &Shadow, offset: usize, dst: &mut [u8]) -> bool {
        self.read_pages(&[(shadow, offset)], dst)
    }
}

/// Level 2 GPU readback serializer.
static LAZY_GPU: Mutex<Option<LazyReadback>> = Mutex::new(None);
/// Lock-free readiness flag so `ensure_lazy_readback()` never blocks behind an
/// active GPU readback after one-time initialization.
static LAZY_GPU_READY: AtomicBool = AtomicBool::new(false);
/// Count of threads actively requesting or holding `LAZY_GPU` for a `COPY`
/// queue readback. Pairs via `SeqCst` with [`lazy_submit_gpu_guard`] so
/// `Host::submit` (`DIRECT`) and `LazyReadback::read_pages` (`COPY`) never
/// overlap on `shadow.buffer` or miss a newly-submitted write fence.
static LAZY_READBACK_ACTIVE: AtomicUsize = AtomicUsize::new(0);

struct LazyReadbackGuard {
    guard: std::sync::MutexGuard<'static, Option<LazyReadback>>,
}

impl Drop for LazyReadbackGuard {
    fn drop(&mut self) {
        LAZY_READBACK_ACTIVE.fetch_sub(1, Ordering::Release);
    }
}

fn lock_lazy_gpu_for_readback() -> LazyReadbackGuard {
    LAZY_READBACK_ACTIVE.fetch_add(1, Ordering::SeqCst);
    LazyReadbackGuard {
        guard: LAZY_GPU.lock().unwrap_or_else(|e| e.into_inner()),
    }
}

/// Scoped `LAZY_GPU` guard acquired by `Host::submit` around `ExecuteCommandLists`,
/// `Signal`, `submitted_fence.store`, and `note_write_fence` (dropped before any
/// CPU `WaitForSingleObject`) ONLY when `VICTORY_LAZY_COPYBACK=1` has armed pages
/// or an active readback. Strictly follows Level 0 (`host.submitted`) -> Level 2
/// (`LAZY_GPU`), preventing D3D12 multi-engine `COPY`/`DIRECT` hazards with zero
/// cost when lazy copy-back is inactive.
pub(crate) struct LazySubmitGpuGuard {
    _guard: Option<std::sync::MutexGuard<'static, Option<LazyReadback>>>,
}

pub(crate) fn lazy_submit_gpu_guard() -> LazySubmitGpuGuard {
    if !lazy_copyback()
        || (LAZY_ARMED_COUNT.load(Ordering::SeqCst) == 0
            && LAZY_READBACK_ACTIVE.load(Ordering::SeqCst) == 0)
    {
        return LazySubmitGpuGuard { _guard: None };
    }
    LazySubmitGpuGuard {
        _guard: Some(LAZY_GPU.lock().unwrap_or_else(|e| e.into_inner())),
    }
}

fn ensure_lazy_readback() -> bool {
    if LAZY_GPU_READY.load(Ordering::Acquire) {
        return true;
    }
    let mut gpu = LAZY_GPU.lock().unwrap_or_else(|e| e.into_inner());
    if gpu.is_none() {
        *gpu = LazyReadback::new();
    }
    let ok = gpu.is_some();
    if ok {
        LAZY_GPU_READY.store(true, Ordering::Release);
    }
    ok
}

/// Whether `addr` is currently committed and accessible for a `READ` or `WRITE`.
/// Used in `lazy_on_fault` only after `LAZY_FLUSH_TOTAL > 0` to distinguish a
/// benign concurrent stale fault (already flushed by another thread) from a
/// genuine access violation or a `watch.rs` `PAGE_READONLY` write guard.
fn is_accessible(addr: usize, is_write: bool) -> bool {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_EXECUTE_READ, PAGE_EXECUTE_READWRITE,
        PAGE_READONLY, PAGE_READWRITE, VirtualQuery,
    };
    // SAFETY: plain struct filled by `VirtualQuery`.
    let mut mbi: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: valid out pointer.
    let n = unsafe { VirtualQuery(addr as *const _, &mut mbi, size_of_val(&mbi)) };
    if n == 0 || mbi.State != MEM_COMMIT {
        return false;
    }
    if is_write {
        matches!(mbi.Protect, PAGE_READWRITE | PAGE_EXECUTE_READWRITE)
    } else {
        matches!(
            mbi.Protect,
            PAGE_READWRITE | PAGE_READONLY | PAGE_EXECUTE_READWRITE | PAGE_EXECUTE_READ
        )
    }
}

/// Coalesced `VirtualProtect(..., PAGE_READWRITE)` over a contiguous run of
/// committed `PAGE_NOACCESS` pages `[run_first, run_last)`. Binary-splits on
/// VAD/uncommitted boundaries so raw `VirtualFree(MEM_DECOMMIT)` never crashes
/// and a single view/allocation still takes ONE `VirtualProtect` syscall.
fn restore_rw_run(run_first: usize, run_last: usize, restored: &mut [bool], base_first: usize) {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_NOACCESS, PAGE_READWRITE, VirtualProtect,
        VirtualQuery,
    };
    if run_first >= run_last {
        return;
    }
    let addr = run_first * PAGE;
    let bytes = (run_last - run_first) * PAGE;
    // SAFETY: plain struct filled by `VirtualQuery`.
    let mut mbi: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: valid out pointer.
    let n = unsafe { VirtualQuery(addr as *const _, &mut mbi, size_of_val(&mbi)) };
    if n != 0
        && mbi.State == MEM_COMMIT
        && mbi.Protect == PAGE_NOACCESS
        && (mbi.BaseAddress as usize) <= addr
        && (mbi.BaseAddress as usize).saturating_add(mbi.RegionSize) >= addr.saturating_add(bytes)
    {
        let mut old = 0u32;
        let t0 = std::time::Instant::now();
        // SAFETY: verified committed `PAGE_NOACCESS` contiguous run we armed.
        let ok = unsafe { VirtualProtect(addr as *const _, bytes, PAGE_READWRITE, &mut old) != 0 };
        lazy_record_protect_ns(t0.elapsed().as_nanos() as u64);
        if ok {
            for p in run_first..run_last {
                restored[p - base_first] = true;
            }
            return;
        }
    }
    if run_last - run_first == 1 {
        return;
    }
    let mid = run_first + (run_last - run_first) / 2;
    restore_rw_run(run_first, mid, restored, base_first);
    restore_rw_run(mid, run_last, restored, base_first);
}

/// Restores `PAGE_READWRITE` on `[page_addr, page_addr + PAGE)` only if it is
/// still a single committed `PAGE_NOACCESS` region (defending against raw
/// unnotified `VirtualFree` + `VirtualAlloc` reuse), and writes `data` into it.
///
/// For section-backed `MEM_MAPPED` pages (`section_view = Some((section, off, sec_bytes))`,
/// used by physical console GPU heaps), writes all 64 KiB through our persistent
/// writable `inplace::OWN` alias + `Release` fence WHILE `page_addr` is STILL
/// `PAGE_NOACCESS`, then flips `page_addr` to `PAGE_READWRITE` in one atomic
/// PTE transition (zero torn-read window, zero per-page `MapViewOfFile`/`UnmapViewOfFile`).
fn unprotect_and_write_page(
    page_addr: usize,
    data: Option<&[u8]>,
    section_view: Option<(usize, u64, u64)>,
) -> bool {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_MAPPED, MEMORY_BASIC_INFORMATION, PAGE_NOACCESS, PAGE_READWRITE,
        VirtualProtect, VirtualQuery,
    };
    // SAFETY: plain struct filled by `VirtualQuery`.
    let mut mbi: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: valid out pointer.
    let n = unsafe { VirtualQuery(page_addr as *const _, &mut mbi, size_of_val(&mbi)) };
    if n == 0
        || mbi.State != MEM_COMMIT
        || (mbi.BaseAddress as usize) > page_addr
        || (mbi.BaseAddress as usize).saturating_add(mbi.RegionSize) < page_addr.saturating_add(PAGE)
        || mbi.Protect != PAGE_NOACCESS
    {
        return false;
    }
    if let (Some(bytes), Some((section, off, sec_bytes))) = (data, section_view)
        && mbi.Type == MEM_MAPPED
    {
        debug_assert_eq!(bytes.len(), PAGE);
        if crate::inplace::write_section_own(section, sec_bytes, off, bytes) {
            std::sync::atomic::fence(Ordering::Release);
            let mut old = 0u32;
            let t0 = std::time::Instant::now();
            // SAFETY: atomic PTE flip revealing fully populated physical page.
            let ok = unsafe { VirtualProtect(page_addr as *const _, PAGE, PAGE_READWRITE, &mut old) != 0 };
            lazy_record_protect_ns(t0.elapsed().as_nanos() as u64);
            return ok;
        }
    }
    let mut old = 0u32;
    let t0 = std::time::Instant::now();
    // SAFETY: verified committed `PAGE_NOACCESS` 64 KiB page we armed.
    let ok = unsafe { VirtualProtect(page_addr as *const _, PAGE, PAGE_READWRITE, &mut old) != 0 };
    if !ok {
        lazy_record_protect_ns(t0.elapsed().as_nanos() as u64);
        return false;
    }
    if let Some(bytes) = data {
        debug_assert_eq!(bytes.len(), PAGE);
        // SAFETY: `page_addr` was just verified committed and set `PAGE_READWRITE`
        // for `PAGE` bytes, and `bytes` is our disjoint heap buffer. Copy
        // immediately before querying the clock or recording telemetry to keep
        // the `MEM_PRIVATE` post-`VirtualProtect` window as narrow as possible.
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), page_addr as *mut u8, PAGE) };
        std::sync::atomic::fence(Ordering::Release);
    }
    lazy_record_protect_ns(t0.elapsed().as_nanos() as u64);
    true
}

/// Writes GPU bytes ONLY to the unwritten complement `[0..rel_lo)` and
/// `[rel_hi..PAGE)` of a partially overwritten deferred page `[page_addr, page_addr + PAGE)`
/// (used by [`lazy_release_written_range`] after DirectStorage writes into
/// backing physical pages), then restores `PAGE_READWRITE`. Never touches
/// `[rel_lo..rel_hi)`.
fn unprotect_and_write_complement(
    page_addr: usize,
    gpu_bytes: &[u8],
    rel_lo: usize,
    rel_hi: usize,
    section_view: Option<(usize, u64, u64)>,
) -> bool {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_MAPPED, MEMORY_BASIC_INFORMATION, PAGE_NOACCESS, PAGE_READWRITE,
        VirtualProtect, VirtualQuery,
    };
    debug_assert_eq!(gpu_bytes.len(), PAGE);
    debug_assert!(rel_lo <= rel_hi && rel_hi <= PAGE);
    // SAFETY: plain struct filled by `VirtualQuery`.
    let mut mbi: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: valid out pointer.
    let n = unsafe { VirtualQuery(page_addr as *const _, &mut mbi, size_of_val(&mbi)) };
    if n == 0
        || mbi.State != MEM_COMMIT
        || (mbi.BaseAddress as usize) > page_addr
        || (mbi.BaseAddress as usize).saturating_add(mbi.RegionSize) < page_addr.saturating_add(PAGE)
        || mbi.Protect != PAGE_NOACCESS
    {
        return false;
    }
    if let Some((section, off, sec_bytes)) = section_view
        && mbi.Type == MEM_MAPPED
    {
        let mut own_ok = true;
        if rel_lo > 0 {
            own_ok &= crate::inplace::write_section_own(section, sec_bytes, off, &gpu_bytes[..rel_lo]);
        }
        if own_ok && rel_hi < PAGE {
            own_ok &= crate::inplace::write_section_own(
                section,
                sec_bytes,
                off + rel_hi as u64,
                &gpu_bytes[rel_hi..],
            );
        }
        if own_ok {
            std::sync::atomic::fence(Ordering::Release);
            let mut old = 0u32;
            let t0 = std::time::Instant::now();
            // SAFETY: atomic PTE flip revealing fully coherent page.
            let ok = unsafe { VirtualProtect(page_addr as *const _, PAGE, PAGE_READWRITE, &mut old) != 0 };
            lazy_record_protect_ns(t0.elapsed().as_nanos() as u64);
            return ok;
        }
    }
    let mut old = 0u32;
    let t0 = std::time::Instant::now();
    // SAFETY: verified committed `PAGE_NOACCESS` 64 KiB page we armed.
    let ok = unsafe { VirtualProtect(page_addr as *const _, PAGE, PAGE_READWRITE, &mut old) != 0 };
    if !ok {
        lazy_record_protect_ns(t0.elapsed().as_nanos() as u64);
        return false;
    }
    // SAFETY: `page_addr` is committed and `PAGE_READWRITE`; copy only complement
    // immediately before querying the clock or recording telemetry.
    unsafe {
        let base = page_addr as *mut u8;
        if rel_lo > 0 {
            std::ptr::copy_nonoverlapping(gpu_bytes.as_ptr(), base, rel_lo);
        }
        if rel_hi < PAGE {
            std::ptr::copy_nonoverlapping(gpu_bytes.as_ptr().add(rel_hi), base.add(rel_hi), PAGE - rel_hi);
        }
        std::sync::atomic::fence(Ordering::Release);
    }
    lazy_record_protect_ns(t0.elapsed().as_nanos() as u64);
    true
}

/// Flushes one 64 KiB page that has already been transitioned to `LazyEntry::Busy`
/// in `LAZY_TABLE` and `LAZY_FLUSHING` in `shadow.lazy_state`.
#[allow(clippy::too_many_arguments)]
fn flush_armed_page(
    shadow: &Shadow,
    page: usize,
    offset: usize,
    page_idx: usize,
    section_view: Option<(usize, u64, u64)>,
    wait: &FlushWait,
    is_veh: bool,
    is_write: bool,
    save_baseline: bool,
) {
    // Heap allocation (never 64 KiB on a VEH stack).
    let mut buf = vec![0u8; PAGE];
    let t_mat = std::time::Instant::now();
    let ok = {
        let mut gpu = lock_lazy_gpu_for_readback();
        gpu.guard.as_mut().is_some_and(|rb| rb.read_page(shadow, offset, &mut buf))
    };
    let wrote = unprotect_and_write_page(page * PAGE, ok.then_some(buf.as_slice()), section_view);
    lazy_record_materialize_us(t_mat.elapsed().as_micros() as u64);
    {
        let mut table = LAZY_TABLE.lock().unwrap_or_else(|e| e.into_inner());
        lazy_table_remove_page(&mut table, page);
        LAZY_FLUSH_TOTAL.fetch_add(1, Ordering::Release);
        if save_baseline {
            if ok && wrote {
                table
                    .flushed_baselines
                    .insert((shadow.id, page_idx), FlushedBaseline::new(buf));
            }
            shadow.lazy_state[page_idx].store(LAZY_RESYNC, Ordering::Release);
            shadow.resync_count.fetch_add(1, Ordering::AcqRel);
        } else {
            shadow.lazy_state[page_idx].store(LAZY_EAGER, Ordering::Release);
        }
        shadow.deferred_count.fetch_sub(1, Ordering::AcqRel);
    }
    if is_veh {
        lazy_record_veh_fault(is_write);
    } else {
        LAZY_STATS[4].fetch_add(1, Ordering::Relaxed);
    }
    if ok {
        lazy_record_bytes_readback(PAGE as u64);
    }
    wait.finish(FlushOutcome::Accessible);
}

struct ClaimedFlushPage<'a> {
    page: usize,
    shadow: &'a Shadow,
    offset: usize,
    page_idx: usize,
    section_view: Option<(usize, u64, u64)>,
    wait: Arc<FlushWait>,
}

/// Coalesced batch flush of claimed `Busy` pages: ONE `LazyReadback::read_pages`
/// (up to 4 MiB on the copy queue), persistent `OWN` section writes for
/// `MEM_MAPPED` pages, ONE `VirtualProtect(..., PAGE_READWRITE)` per contiguous
/// run, and ONE `LAZY_TABLE` commit lock.
fn flush_claimed_batch(batch: &[ClaimedFlushPage<'_>], save_baseline: bool) {
    if batch.is_empty() {
        return;
    }
    let n = batch.len();
    let mut bufs = vec![0u8; n * PAGE];
    let t_mat = std::time::Instant::now();
    let reqs: Vec<(&Shadow, usize)> = batch.iter().map(|c| (c.shadow, c.offset)).collect();
    let ok = {
        let mut gpu = lock_lazy_gpu_for_readback();
        gpu.guard.as_mut().is_some_and(|rb| rb.read_pages(&reqs, &mut bufs))
    };

    // Pre-write section-backed pages through persistent `inplace::OWN` ONLY
    // after verifying via region-cached `VirtualQuery` that the VA is still
    // committed `MEM_MAPPED | PAGE_NOACCESS` (never writing into an unnotified
    // unmapped/replaced section).
    let mut prewritten = vec![false; n];
    if ok {
        use windows_sys::Win32::System::Memory::{
            MEM_COMMIT, MEM_MAPPED, MEMORY_BASIC_INFORMATION, PAGE_NOACCESS, VirtualQuery,
        };
        let mut cached: Option<(usize, usize, u32, u32, u32)> = None;
        let mut any_prewritten = false;
        for (i, c) in batch.iter().enumerate() {
            let Some((section, off, sec_bytes)) = c.section_view else {
                continue;
            };
            let page_addr = c.page * PAGE;
            let (base_a, end_a, state, ty, prot) = match cached {
                Some(m) if page_addr >= m.0 && page_addr + PAGE <= m.1 => m,
                _ => {
                    // SAFETY: plain struct filled by `VirtualQuery`.
                    let mut mbi: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
                    // SAFETY: valid out pointer.
                    let qn = unsafe { VirtualQuery(page_addr as *const _, &mut mbi, size_of_val(&mbi)) };
                    if qn == 0 {
                        continue;
                    }
                    let m = (
                        mbi.BaseAddress as usize,
                        (mbi.BaseAddress as usize).saturating_add(mbi.RegionSize),
                        mbi.State,
                        mbi.Type,
                        mbi.Protect,
                    );
                    cached = Some(m);
                    m
                }
            };
            if state == MEM_COMMIT
                && ty == MEM_MAPPED
                && prot == PAGE_NOACCESS
                && page_addr >= base_a
                && page_addr + PAGE <= end_a
                && crate::inplace::write_section_own(
                    section,
                    sec_bytes,
                    off,
                    &bufs[i * PAGE..(i + 1) * PAGE],
                )
            {
                prewritten[i] = true;
                any_prewritten = true;
            }
        }
        if any_prewritten {
            std::sync::atomic::fence(Ordering::Release);
        }
    }

    // Coalesce contiguous pages in `batch` (already sorted by `page`) into runs
    // for `VirtualProtect(..., PAGE_READWRITE)`.
    let mut restored = vec![false; n];
    let mut idx = 0usize;
    while idx < n {
        let run_start_idx = idx;
        let run_first = batch[idx].page;
        let mut run_last = run_first + 1;
        idx += 1;
        while idx < n && batch[idx].page == run_last {
            run_last += 1;
            idx += 1;
        }
        restore_rw_run(run_first, run_last, &mut restored[run_start_idx..idx], run_first);
    }

    // Copy bytes for any restored pages that were not prewritten via `OWN`.
    if ok {
        let mut copied_any = false;
        for (i, c) in batch.iter().enumerate() {
            if restored[i] && !prewritten[i] {
                // SAFETY: `restore_rw_run` verified committed `PAGE_READWRITE`.
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        bufs.as_ptr().add(i * PAGE),
                        (c.page * PAGE) as *mut u8,
                        PAGE,
                    );
                }
                copied_any = true;
            }
        }
        if copied_any {
            std::sync::atomic::fence(Ordering::Release);
        }
    }
    lazy_record_materialize_us(t_mat.elapsed().as_micros() as u64);

    {
        let mut table = LAZY_TABLE.lock().unwrap_or_else(|e| e.into_inner());
        for (i, c) in batch.iter().enumerate() {
            lazy_table_remove_page(&mut table, c.page);
            if save_baseline {
                if ok && restored[i] {
                    table.flushed_baselines.insert(
                        (c.shadow.id, c.page_idx),
                        FlushedBaseline::new(bufs[i * PAGE..(i + 1) * PAGE].to_vec()),
                    );
                }
                c.shadow.lazy_state[c.page_idx].store(LAZY_RESYNC, Ordering::Release);
                c.shadow.resync_count.fetch_add(1, Ordering::AcqRel);
            } else {
                c.shadow.lazy_state[c.page_idx].store(LAZY_EAGER, Ordering::Release);
            }
            c.shadow.deferred_count.fetch_sub(1, Ordering::AcqRel);
        }
        LAZY_FLUSH_TOTAL.fetch_add(n as u64, Ordering::Release);
    }
    LAZY_STATS[4].fetch_add(n as u64, Ordering::Relaxed);
    if ok {
        lazy_record_bytes_readback((n * PAGE) as u64);
    }
    for c in batch {
        c.wait.finish(FlushOutcome::Accessible);
    }
}

/// Permanently pins title pages overlapping `[at, at + len)` (e.g., GPU fence
/// polling and `defer_write` targets) so they are NEVER protected `PAGE_NOACCESS`.
/// Any currently deferred page in the range is flushed first.
pub(crate) fn lazy_pin_range(at: usize, len: usize) {
    if len == 0 || !lazy_copyback() {
        return;
    }
    let first = at / PAGE;
    let last = at.saturating_add(len).div_ceil(PAGE);
    {
        let mut table = LAZY_TABLE.lock().unwrap_or_else(|e| e.into_inner());
        for page in first..last {
            table.pinned.insert(page);
        }
    }
    lazy_flush_range(at, len);
}

/// Returns all currently armed/busy lazy page numbers in `[first, last)` in
/// sorted order under AT MOST ONE `LAZY_TABLE` lock (zero locks when
/// `LAZY_ARMED_COUNT == 0` or small range has empty stripes).
pub(crate) fn lazy_deferred_pages_in_range(first: usize, last: usize) -> Vec<usize> {
    if !lazy_range_may_have_armed(first, last) {
        return Vec::new();
    }
    let table = LAZY_TABLE.lock().unwrap_or_else(|e| e.into_inner());
    table.pages.range(first..last).map(|(&p, _)| p).collect()
}

/// Returns deferred page numbers in `[from, to)` owned by ANY shadow other than
/// `my_id` without flushing or unprotecting them.
pub(crate) fn lazy_other_deferred_pages(my_id: u64, from: usize, to: usize) -> Vec<usize> {
    if from >= to {
        return Vec::new();
    }
    let first = from / PAGE;
    let last = to.div_ceil(PAGE);
    if !lazy_range_may_have_armed(first, last) {
        return Vec::new();
    }
    let table = LAZY_TABLE.lock().unwrap_or_else(|e| e.into_inner());
    table
        .pages
        .range(first..last)
        .filter_map(|(&p, entry)| match entry {
            LazyEntry::Armed(a) if a.shadow_id != my_id => Some(p),
            LazyEntry::Busy { shadow_id, .. } if *shadow_id != my_id => Some(p),
            _ => None,
        })
        .collect()
}

/// Internal layer self-bypass reader: services `[src, src + dst.len())` into
/// host slice `dst` while caller holds [`LazyAccessHold`].
/// - Non-deferred ranges take the lock-free zero-mutex fast path directly to
///   `fallback_read`.
/// - Any `LAZY_DEFERRED` page in the range is read directly from its owning
///   shadow (batched on the copy queue for carded VRAM, direct copy for shared/
///   ReBAR) WITHOUT unprotecting title memory, WITHOUT triggering VEH, and
///   WITHOUT clearing `LAZY_DEFERRED` or setting `LAZY_RESYNC`!
pub(crate) fn lazy_read_bypass(
    src: usize,
    dst: &mut [u8],
    mut fallback_read: impl FnMut(usize, &mut [u8]) -> bool,
) -> bool {
    let len = dst.len();
    if len == 0 {
        return true;
    }
    let end = src.saturating_add(len);
    let first = src / PAGE;
    let last = end.div_ceil(PAGE);
    if !lazy_range_may_have_armed(first, last) {
        return fallback_read(src, dst);
    }

    // Snapshot all `Armed` pages overlapping `[first, last)`, waiting on any
    // transiently `Busy` entry. Because the caller holds `LazyAccessHold`, no
    // new page in `[first, last)` can transition into `LAZY_TABLE` concurrently.
    let armed: Vec<(usize, Arc<Shadow>, usize)> = loop {
        enum Snapshot {
            None,
            Wait(Arc<FlushWait>),
            Yield,
            Pages(Vec<(usize, Arc<Shadow>, usize)>),
        }
        let snap = {
            let table = LAZY_TABLE.lock().unwrap_or_else(|e| e.into_inner());
            let mut found = Vec::new();
            let mut wait = None;
            let mut yield_drop = false;
            for (&page, entry) in table.pages.range(first..last) {
                match entry {
                    LazyEntry::Busy { wait: w, .. } => {
                        wait = Some(w.clone());
                        break;
                    }
                    LazyEntry::Armed(a) => match a.shadow.upgrade() {
                        Some(shadow) => found.push((page, shadow, a.offset)),
                        None => {
                            yield_drop = true;
                            break;
                        }
                    },
                }
            }
            if let Some(w) = wait {
                Snapshot::Wait(w)
            } else if yield_drop {
                Snapshot::Yield
            } else if found.is_empty() {
                Snapshot::None
            } else {
                Snapshot::Pages(found)
            }
        };
        match snap {
            Snapshot::None => return fallback_read(src, dst),
            Snapshot::Wait(w) => {
                let _ = w.wait();
            }
            Snapshot::Yield => std::thread::yield_now(),
            Snapshot::Pages(pages) => break pages,
        }
    };

    // 1. Batch-read all deferred pages directly from their owning shadow(s).
    let t_mat = std::time::Instant::now();
    let mut page_bufs = vec![0u8; armed.len() * PAGE];
    let reqs: Vec<(&Shadow, usize)> = armed.iter().map(|(_, s, off)| (s.as_ref(), *off)).collect();
    let read_ok = {
        let mut gpu = lock_lazy_gpu_for_readback();
        gpu.guard.as_mut().is_some_and(|rb| rb.read_pages(&reqs, &mut page_bufs))
    };
    lazy_record_materialize_us(t_mat.elapsed().as_micros() as u64);
    if !read_ok {
        // Fallback: explicitly flush and read via normal kernel/plain path.
        lazy_flush_range(src, len);
        return fallback_read(src, dst);
    }
    lazy_record_self_bypass(armed.len() as u64);

    // 2. Copy deferred page slices into `dst` and service non-deferred gaps via `fallback_read`.
    let mut cursor = src;
    for (i, &(page, _, _)) in armed.iter().enumerate() {
        let p_lo = (page * PAGE).max(src);
        let p_hi = ((page + 1) * PAGE).min(end);
        if cursor < p_lo {
            let run_dst = &mut dst[cursor - src..p_lo - src];
            if !fallback_read(cursor, run_dst) {
                return false;
            }
        }
        if p_lo < p_hi {
            let in_page_lo = p_lo - page * PAGE;
            let in_page_hi = p_hi - page * PAGE;
            let page_slice = &page_bufs[i * PAGE + in_page_lo..i * PAGE + in_page_hi];
            dst[p_lo - src..p_hi - src].copy_from_slice(page_slice);
        }
        cursor = p_hi;
    }
    cursor >= end || fallback_read(cursor, &mut dst[cursor - src..end - src])
}

/// Deadlock-free VEH demand-fault handler called from `watch.rs::on_fault` on
/// both READ and WRITE access violations before `watch` acquires any lock.
/// Returns `true` iff the fault was handled and execution may resume.
pub(crate) fn lazy_on_fault(addr: usize, is_write: bool) -> bool {
    if LAZY_ARMED_COUNT.load(Ordering::Acquire) == 0 && LAZY_FLUSH_TOTAL.load(Ordering::Acquire) == 0 {
        return false;
    }
    if IN_LAZY_VEH.with(Cell::get) {
        return false;
    }
    struct InVehGuard;
    impl Drop for InVehGuard {
        fn drop(&mut self) {
            IN_LAZY_VEH.with(|c| c.set(false));
        }
    }
    IN_LAZY_VEH.with(|c| c.set(true));
    let _in_veh = InVehGuard;

    let page = addr / PAGE;
    // Fast lock-free exit when this stripe has no armed pages and the fault is
    // not a stale post-flush race.
    if LAZY_ARMED_STRIPES[page & LAZY_STRIPE_MASK].load(Ordering::Acquire) == 0 {
        return LAZY_FLUSH_TOTAL.load(Ordering::Acquire) > 0 && is_accessible(addr, is_write);
    }
    loop {
        let action = {
            let mut table = LAZY_TABLE.lock().unwrap_or_else(|e| e.into_inner());
            match table.pages.get(&page) {
                Some(LazyEntry::Busy { wait, .. }) => Ok(Err(wait.clone())),
                Some(LazyEntry::Armed(armed)) => match armed.shadow.upgrade() {
                    Some(shadow) => {
                        let (shadow_id, offset, page_idx, section_view) =
                            (armed.shadow_id, armed.offset, armed.page_idx, armed.section_view);
                        let wait = Arc::new(FlushWait::new());
                        shadow.lazy_state[page_idx].store(LAZY_FLUSHING, Ordering::Release);
                        lazy_table_insert_busy(&mut table, page, shadow_id, wait.clone());
                        Ok(Ok((shadow, offset, page_idx, section_view, wait)))
                    }
                    // `Shadow::drop` is actively executing and will acquire
                    // `LAZY_TABLE` momentarily to transition to `Busy` and flush.
                    None => Err(true),
                },
                None => Err(false),
            }
        };
        match action {
            Ok(Ok((shadow, offset, page_idx, section_view, wait))) => {
                flush_armed_page(&shadow, page, offset, page_idx, section_view, &wait, true, is_write, true);
                return true;
            }
            Ok(Err(wait)) => {
                if wait.wait() == FlushOutcome::Accessible {
                    return true;
                }
                continue;
            }
            Err(true) => {
                std::thread::yield_now();
                continue;
            }
            Err(false) => {
                return LAZY_FLUSH_TOTAL.load(Ordering::Acquire) > 0 && is_accessible(addr, is_write);
            }
        }
    }
}

/// Flushes every deferred GPU-resident page overlapping `[at, at + len)` into
/// title memory and restores `PAGE_READWRITE` in coalesced batches (up to 4 MiB
/// per copy-queue submit and ONE `VirtualProtect` per contiguous run), waiting
/// on any in-flight flush. Safe to call with zero Level 0 locks held.
pub(crate) fn lazy_flush_range(at: usize, len: usize) {
    if len == 0 {
        return;
    }
    let first = at / PAGE;
    let last = at.saturating_add(len).div_ceil(PAGE);
    if !lazy_range_may_have_armed(first, last) {
        return;
    }
    loop {
        let mut claimed: Vec<(usize, Arc<Shadow>, usize, usize, Option<(usize, u64, u64)>, Arc<FlushWait>)> =
            Vec::new();
        let mut wait_on: Option<Arc<FlushWait>> = None;
        let mut yield_drop = false;
        {
            let mut table = LAZY_TABLE.lock().unwrap_or_else(|e| e.into_inner());
            let keys: Vec<usize> = table
                .pages
                .range(first..last)
                .take(LAZY_READBACK_BATCH_PAGES)
                .map(|(&p, _)| p)
                .collect();
            for page in keys {
                match table.pages.get(&page) {
                    Some(LazyEntry::Busy { wait, .. }) => {
                        if claimed.is_empty() {
                            wait_on = Some(wait.clone());
                            break;
                        }
                    }
                    Some(LazyEntry::Armed(armed)) => match armed.shadow.upgrade() {
                        Some(shadow) => {
                            let (shadow_id, offset, page_idx, section_view) =
                                (armed.shadow_id, armed.offset, armed.page_idx, armed.section_view);
                            let wait = Arc::new(FlushWait::new());
                            shadow.lazy_state[page_idx].store(LAZY_FLUSHING, Ordering::Release);
                            lazy_table_insert_busy(&mut table, page, shadow_id, wait.clone());
                            claimed.push((page, shadow, offset, page_idx, section_view, wait));
                        }
                        None => {
                            if claimed.is_empty() {
                                yield_drop = true;
                                break;
                            }
                        }
                    },
                    None => {}
                }
            }
        }
        if !claimed.is_empty() {
            let batch: Vec<ClaimedFlushPage<'_>> = claimed
                .iter()
                .map(|(page, shadow, offset, page_idx, section_view, wait)| ClaimedFlushPage {
                    page: *page,
                    shadow: shadow.as_ref(),
                    offset: *offset,
                    page_idx: *page_idx,
                    section_view: *section_view,
                    wait: wait.clone(),
                })
                .collect();
            flush_claimed_batch(&batch, true);
            continue;
        }
        if let Some(wait) = wait_on {
            let _ = wait.wait();
            continue;
        }
        if yield_drop {
            std::thread::yield_now();
            continue;
        }
        return;
    }
}

/// Scoped active-remap guard held inside `watch::Remapping` from entry of
/// `watch::title_memory_remapping` until `Remapping::drop`: prevents any
/// concurrent relay/submit thread from re-arming `PAGE_NOACCESS` while views
/// are being unmapped or replaced.
pub(crate) struct LazyRemapGuard {
    id: Option<u64>,
}

impl Drop for LazyRemapGuard {
    fn drop(&mut self) {
        if let Some(id) = self.id.take() {
            let mut table = LAZY_TABLE.lock().unwrap_or_else(|e| e.into_inner());
            table.remapping.retain(|&(rid, _, _)| rid != id);
        }
    }
}

/// Registers an active remap barrier in `LAZY_TABLE` for the duration of
/// `watch::title_memory_remapping()` without flushing unrelated deferred pages
/// (exact remapped ranges are flushed by `watch::title_memory_remapped(at, len)`
/// and `watch::views_mapped` before unmapping/aliasing).
/// Safe to call with zero Level 0 locks held.
pub(crate) fn lazy_begin_remapping_barrier() -> LazyRemapGuard {
    if !lazy_copyback() {
        return LazyRemapGuard { id: None };
    }
    let last = usize::MAX.div_ceil(PAGE);
    let id = {
        let mut table = LAZY_TABLE.lock().unwrap_or_else(|e| e.into_inner());
        let id = table.next_remap_id;
        table.next_remap_id = table.next_remap_id.wrapping_add(1).max(1);
        table.remapping.push((id, 0, last));
        id
    };
    LazyRemapGuard { id: Some(id) }
}

/// Registers a scoped non-arming range barrier for `[at, at + len)` and flushes
/// any currently armed or in-flight-arming deferred pages in that range before
/// returning. Used for title-destination writes and explicit tests; host read
/// paths use lock-free [`lazy_hold_access`] + [`lazy_read_bypass`].
pub(crate) fn lazy_flush_and_hold(at: usize, len: usize) -> LazyAccessHold {
    let hold = lazy_hold_access(at, len);
    if len > 0 && lazy_copyback() {
        lazy_flush_range(at, len);
    }
    hold
}

/// Releases deferred `PAGE_NOACCESS` pages overlapping `[at, at + len)` AFTER
/// external/DirectStorage bytes have ALREADY been written directly into the
/// backing physical pages (`watch::title_memory_written`).
pub(crate) fn lazy_release_written_range(at: usize, len: usize) {
    if len == 0 {
        return;
    }
    let end = at.saturating_add(len);
    let first = at / PAGE;
    let last = end.div_ceil(PAGE);
    if !lazy_range_may_have_armed(first, last) {
        return;
    }
    loop {
        enum Next {
            Done,
            Wait(Arc<FlushWait>),
            Yield,
            Release(usize, Arc<Shadow>, usize, usize, Option<(usize, u64, u64)>, Arc<FlushWait>),
        }
        let next = {
            let mut table = LAZY_TABLE.lock().unwrap_or_else(|e| e.into_inner());
            let found = table.pages.range(first..last).next().map(|(&p, _)| p);
            match found {
                None => Next::Done,
                Some(page) => match table.pages.get(&page) {
                    Some(LazyEntry::Busy { wait, .. }) => Next::Wait(wait.clone()),
                    Some(LazyEntry::Armed(armed)) => match armed.shadow.upgrade() {
                        Some(shadow) => {
                            let (shadow_id, offset, page_idx, section_view) =
                                (armed.shadow_id, armed.offset, armed.page_idx, armed.section_view);
                            let wait = Arc::new(FlushWait::new());
                            shadow.lazy_state[page_idx].store(LAZY_FLUSHING, Ordering::Release);
                            lazy_table_insert_busy(&mut table, page, shadow_id, wait.clone());
                            Next::Release(page, shadow, offset, page_idx, section_view, wait)
                        }
                        None => Next::Yield,
                    },
                    None => Next::Done,
                },
            }
        };
        match next {
            Next::Done => return,
            Next::Wait(wait) => {
                let _ = wait.wait();
            }
            Next::Yield => std::thread::yield_now(),
            Next::Release(page, shadow, offset, page_idx, section_view, wait) => {
                let page_addr = page * PAGE;
                let page_end = page_addr.saturating_add(PAGE);
                let w_lo = at.max(page_addr);
                let w_hi = end.min(page_end);
                let full_cover = w_lo == page_addr && w_hi == page_end;
                let t_mat = std::time::Instant::now();
                let mut baseline = if full_cover {
                    let captured = if shadow.writes_pending() {
                        let mut buf = vec![0u8; PAGE];
                        let ok = {
                            let mut gpu = lock_lazy_gpu_for_readback();
                            gpu.guard.as_mut().is_some_and(|rb| rb.read_page(&shadow, offset, &mut buf))
                        };
                        if ok {
                            lazy_record_bytes_readback(PAGE as u64);
                        }
                        ok.then_some(buf)
                    } else {
                        None
                    };
                    let _ = unprotect_and_write_page(page_addr, None, None);
                    captured
                } else {
                    let mut buf = vec![0u8; PAGE];
                    let ok = {
                        let mut gpu = lock_lazy_gpu_for_readback();
                        gpu.guard.as_mut().is_some_and(|rb| rb.read_page(&shadow, offset, &mut buf))
                    };
                    let wrote = if ok {
                        unprotect_and_write_complement(
                            page_addr,
                            &buf,
                            w_lo - page_addr,
                            w_hi - page_addr,
                            section_view,
                        )
                    } else {
                        unprotect_and_write_page(page_addr, None, None)
                    };
                    if ok {
                        lazy_record_bytes_readback(PAGE as u64);
                    }
                    (ok && wrote).then_some(buf)
                };
                if full_cover && baseline.is_none() && shadow.writes_pending() {
                    let mut buf = vec![0u8; PAGE];
                    let ok = {
                        let mut gpu = lock_lazy_gpu_for_readback();
                        gpu.guard.as_mut().is_some_and(|rb| rb.read_page(&shadow, offset, &mut buf))
                    };
                    if ok {
                        lazy_record_bytes_readback(PAGE as u64);
                        baseline = Some(buf);
                    }
                }
                lazy_record_materialize_us(t_mat.elapsed().as_micros() as u64);
                {
                    let mut table = LAZY_TABLE.lock().unwrap_or_else(|e| e.into_inner());
                    lazy_table_remove_page(&mut table, page);
                    LAZY_FLUSH_TOTAL.fetch_add(1, Ordering::Release);
                    if let Some(buf) = baseline {
                        table
                            .flushed_baselines
                            .insert((shadow.id, page_idx), FlushedBaseline::new(buf));
                    }
                    shadow.lazy_state[page_idx].store(LAZY_RESYNC, Ordering::Release);
                    shadow.resync_count.fetch_add(1, Ordering::AcqRel);
                    shadow.deferred_count.fetch_sub(1, Ordering::AcqRel);
                }
                LAZY_STATS[4].fetch_add(1, Ordering::Relaxed);
                wait.finish(FlushOutcome::Accessible);
            }
        }
    }
}

/// Flushes deferred pages in `[from, to)` owned by ANY shadow other than `my_id`
/// in coalesced batches (used by `Shadow::writing` when another shadow takes
/// GPU write ownership of an overlapping address range).
pub(crate) fn lazy_flush_other_shadows(my_id: u64, from: usize, to: usize) -> Vec<usize> {
    if from >= to {
        return Vec::new();
    }
    let first = from / PAGE;
    let last = to.div_ceil(PAGE);
    if !lazy_range_may_have_armed(first, last) {
        return Vec::new();
    }
    let mut flushed = Vec::new();
    loop {
        let mut claimed: Vec<(usize, Arc<Shadow>, usize, usize, Option<(usize, u64, u64)>, Arc<FlushWait>)> =
            Vec::new();
        let mut wait_on: Option<(usize, Arc<FlushWait>)> = None;
        let mut yield_drop = false;
        {
            let mut table = LAZY_TABLE.lock().unwrap_or_else(|e| e.into_inner());
            let keys: Vec<usize> = table
                .pages
                .range(first..last)
                .filter_map(|(&p, entry)| match entry {
                    LazyEntry::Armed(a) if a.shadow_id != my_id => Some(p),
                    LazyEntry::Busy { shadow_id, .. } if *shadow_id != my_id => Some(p),
                    _ => None,
                })
                .take(LAZY_READBACK_BATCH_PAGES)
                .collect();
            for page in keys {
                match table.pages.get(&page) {
                    Some(LazyEntry::Busy { wait, .. }) => {
                        if claimed.is_empty() {
                            wait_on = Some((page, wait.clone()));
                            break;
                        }
                    }
                    Some(LazyEntry::Armed(armed)) => match armed.shadow.upgrade() {
                        Some(shadow) => {
                            let (shadow_id, offset, page_idx, section_view) =
                                (armed.shadow_id, armed.offset, armed.page_idx, armed.section_view);
                            let wait = Arc::new(FlushWait::new());
                            shadow.lazy_state[page_idx].store(LAZY_FLUSHING, Ordering::Release);
                            lazy_table_insert_busy(&mut table, page, shadow_id, wait.clone());
                            claimed.push((page, shadow, offset, page_idx, section_view, wait));
                        }
                        None => {
                            if claimed.is_empty() {
                                yield_drop = true;
                                break;
                            }
                        }
                    },
                    None => {}
                }
            }
        }
        if !claimed.is_empty() {
            let batch: Vec<ClaimedFlushPage<'_>> = claimed
                .iter()
                .map(|(page, shadow, offset, page_idx, section_view, wait)| ClaimedFlushPage {
                    page: *page,
                    shadow: shadow.as_ref(),
                    offset: *offset,
                    page_idx: *page_idx,
                    section_view: *section_view,
                    wait: wait.clone(),
                })
                .collect();
            flush_claimed_batch(&batch, true);
            flushed.extend(claimed.iter().map(|c| c.0));
            continue;
        }
        if let Some((page, wait)) = wait_on {
            let _ = wait.wait();
            flushed.push(page);
            continue;
        }
        if yield_drop {
            std::thread::yield_now();
            continue;
        }
        return flushed;
    }
}

/// Drops deferred `PAGE_NOACCESS` protections overlapping `[at, at + len)` before
/// `VirtualFree` without wasting a GPU readback on memory being freed, restoring
/// `PAGE_READWRITE` in coalesced contiguous runs and waiting on any in-flight flush.
pub(crate) fn lazy_release_range(at: usize, len: usize) {
    if len == 0 {
        return;
    }
    let first = at / PAGE;
    let last = at.saturating_add(len).div_ceil(PAGE);
    if lazy_copyback() || LAZY_ARMED_COUNT.load(Ordering::Acquire) > 0 {
        let mut table = LAZY_TABLE.lock().unwrap_or_else(|e| e.into_inner());
        if !table.pinned.is_empty() {
            let stale_pins: Vec<usize> = table.pinned.range(first..last).copied().collect();
            for p in stale_pins {
                table.pinned.remove(&p);
            }
        }
    }
    if !lazy_range_may_have_armed(first, last) {
        return;
    }
    loop {
        let mut claimed: Vec<(usize, Weak<Shadow>, usize, Arc<FlushWait>)> = Vec::new();
        let mut wait_on: Option<Arc<FlushWait>> = None;
        {
            let mut table = LAZY_TABLE.lock().unwrap_or_else(|e| e.into_inner());
            let keys: Vec<usize> = table.pages.range(first..last).map(|(&p, _)| p).collect();
            for page in keys {
                match table.pages.get(&page) {
                    Some(LazyEntry::Busy { wait, .. }) => {
                        if claimed.is_empty() {
                            wait_on = Some(wait.clone());
                            break;
                        }
                    }
                    Some(LazyEntry::Armed(armed)) => {
                        let (shadow, shadow_id, page_idx) =
                            (armed.shadow.clone(), armed.shadow_id, armed.page_idx);
                        let wait = Arc::new(FlushWait::new());
                        if let Some(s) = shadow.upgrade() {
                            s.lazy_state[page_idx].store(LAZY_FLUSHING, Ordering::Release);
                        }
                        lazy_table_insert_busy(&mut table, page, shadow_id, wait.clone());
                        claimed.push((page, shadow, page_idx, wait));
                    }
                    None => {}
                }
            }
        }
        if !claimed.is_empty() {
            let n = claimed.len();
            let mut restored = vec![false; n];
            let mut idx = 0usize;
            while idx < n {
                let run_start = idx;
                let run_first = claimed[idx].0;
                let mut run_last = run_first + 1;
                idx += 1;
                while idx < n && claimed[idx].0 == run_last {
                    run_last += 1;
                    idx += 1;
                }
                restore_rw_run(run_first, run_last, &mut restored[run_start..idx], run_first);
            }
            {
                let mut table = LAZY_TABLE.lock().unwrap_or_else(|e| e.into_inner());
                for (page, shadow, page_idx, _) in &claimed {
                    lazy_table_remove_page(&mut table, *page);
                    if let Some(s) = shadow.upgrade() {
                        s.lazy_state[*page_idx].store(LAZY_EAGER, Ordering::Release);
                        s.deferred_count.fetch_sub(1, Ordering::AcqRel);
                    }
                }
                LAZY_FLUSH_TOTAL.fetch_add(n as u64, Ordering::Release);
            }
            for (_, _, _, wait) in claimed {
                wait.finish(FlushOutcome::Accessible);
            }
            continue;
        }
        if let Some(wait) = wait_on {
            let _ = wait.wait();
            continue;
        }
        return;
    }
}

/// Rolls back contiguous runs of pages that were temporarily set to
/// `PAGE_NOACCESS` back to `PAGE_READWRITE` with coalesced `VirtualProtect`.
pub(super) fn rollback_noaccess_pages_coalesced(pages: &[usize]) {
    use windows_sys::Win32::System::Memory::{PAGE_READWRITE, VirtualProtect};
    if pages.is_empty() {
        return;
    }
    fn rollback_subrun(from: usize, to: usize) {
        let count = to - from;
        if count == 0 {
            return;
        }
        let mut _rev = 0u32;
        let t0 = std::time::Instant::now();
        // SAFETY: restoring `PAGE_READWRITE` on pages we just set `PAGE_NOACCESS`.
        let ok = unsafe {
            VirtualProtect((from * PAGE) as *const _, count * PAGE, PAGE_READWRITE, &mut _rev) != 0
        };
        lazy_record_protect_ns(t0.elapsed().as_nanos() as u64);
        if ok || count == 1 {
            return;
        }
        let mid = from + count / 2;
        rollback_subrun(from, mid);
        rollback_subrun(mid, to);
    }
    let mut i = 0;
    while i < pages.len() {
        let run_first = pages[i];
        let mut run_last = run_first + 1;
        i += 1;
        while i < pages.len() && pages[i] == run_last {
            run_last += 1;
            i += 1;
        }
        rollback_subrun(run_first, run_last);
    }
}

/// Coalesced `VirtualProtect(..., PAGE_NOACCESS)` over `[from, to)`: clips each
/// sub-run to a single uniform `VirtualQuery` region (`MEM_COMMIT && MEM_PRIVATE
/// && PAGE_READWRITE`) BEFORE calling multi-page `VirtualProtect` so interior
/// non-matching protections are never clobbered, never overwrites `cur+1..sub_to`
/// with `old` on unexpected prior protection, and records every syscall.
pub(super) fn protect_private_run_noaccess(from: usize, to: usize, protected: &mut Vec<usize>) {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_PRIVATE, MEMORY_BASIC_INFORMATION, PAGE_NOACCESS, PAGE_READWRITE,
        VirtualProtect, VirtualQuery,
    };
    let mut cur = from;
    while cur < to {
        let addr = cur * PAGE;
        // SAFETY: plain struct filled by `VirtualQuery`.
        let mut mbi: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: valid out pointer.
        let n = unsafe { VirtualQuery(addr as *const _, &mut mbi, size_of_val(&mbi)) };
        if n == 0 || mbi.State != MEM_COMMIT || mbi.Type != MEM_PRIVATE || mbi.Protect != PAGE_READWRITE {
            let skip_end = if n != 0 {
                (mbi.BaseAddress as usize).saturating_add(mbi.RegionSize) / PAGE
            } else {
                cur + 1
            };
            cur = skip_end.max(cur + 1);
            continue;
        }
        let region_end = (mbi.BaseAddress as usize).saturating_add(mbi.RegionSize) / PAGE;
        let sub_to = to.min(region_end);
        if sub_to <= cur {
            cur += 1;
            continue;
        }
        let count = sub_to - cur;
        let mut old = 0u32;
        let t0 = std::time::Instant::now();
        // SAFETY: `VirtualQuery` verified `[cur, sub_to)` is one uniform
        // committed `MEM_PRIVATE` `PAGE_READWRITE` region.
        let ok = unsafe {
            VirtualProtect((cur * PAGE) as *const _, count * PAGE, PAGE_NOACCESS, &mut old) != 0
        };
        lazy_record_protect_ns(t0.elapsed().as_nanos() as u64);
        if ok && old == PAGE_READWRITE {
            protected.extend(cur..sub_to);
        } else if ok {
            let mut _rev = 0u32;
            let tr = std::time::Instant::now();
            // SAFETY: restoring page `cur` to its reported prior protection.
            unsafe { VirtualProtect((cur * PAGE) as *const _, PAGE, old, &mut _rev) };
            lazy_record_protect_ns(tr.elapsed().as_nanos() as u64);
            if count > 1 {
                let tr2 = std::time::Instant::now();
                // SAFETY: restoring remaining verified `PAGE_READWRITE` pages.
                unsafe {
                    VirtualProtect(
                        ((cur + 1) * PAGE) as *const _,
                        (count - 1) * PAGE,
                        PAGE_READWRITE,
                        &mut _rev,
                    )
                };
                lazy_record_protect_ns(tr2.elapsed().as_nanos() as u64);
            }
        }
        cur = sub_to;
    }
}

/// Batch-defers candidate `(shadow, lo, hi, fetched_after)` pieces across one
/// or more shadows (used by `host::relay_card_back`, `host::relay_card_inflight`,
/// and `host::card_back`). Groups pieces by `(shadow, fetched_after)` and arms
/// contiguous runs with coalesced `VirtualProtect` syscalls.
pub(crate) fn try_defer_pieces_batch_with_after(
    pieces: &[(Arc<Shadow>, usize, usize, Option<u64>)],
) -> Vec<bool> {
    if pieces.is_empty() || !lazy_copyback() {
        return vec![false; pieces.len()];
    }
    let mut out = vec![false; pieces.len()];
    let mut by_key: Vec<((usize, Option<u64>), Arc<Shadow>, Vec<(usize, usize, usize)>)> = Vec::new();
    for (i, (shadow, lo, hi, after)) in pieces.iter().enumerate() {
        let key = (Arc::as_ptr(shadow) as usize, *after);
        if let Some(entry) = by_key.iter_mut().find(|e| e.0 == key) {
            entry.2.push((i, *lo, *hi));
        } else {
            by_key.push((key, shadow.clone(), vec![(i, *lo, *hi)]));
        }
    }
    for ((_, after), shadow, list) in by_key {
        let offsets: Vec<(usize, usize)> = list.iter().map(|&(_, lo, hi)| (lo, hi)).collect();
        let res = shadow.try_defer_shadow_pieces(&offsets, after);
        for ((orig_idx, _, _), deferred) in list.into_iter().zip(res) {
            out[orig_idx] = deferred;
        }
    }
    out
}

impl Shadow {
    /// Coalesced batch arming for a shadow's GPU-written pieces `offsets` (`(lo, hi)`):
    /// - O(1) lock-free fast path for already-`LAZY_DEFERRED` pieces.
    /// - Amortizes `copied`, `card.dirty`/`card.sent`, `VIEWS`, `JOURNAL`,
    ///   `GUARDS`, and `LAZY_TABLE` locks from O(N) to O(1).
    /// - Zero `VirtualQuery` on `MEM_MAPPED` view pages and cached region-level
    ///   `VirtualQuery` on `MEM_PRIVATE` pages.
    /// - Coalesces contiguous runs into ONE `VirtualProtect(..., PAGE_NOACCESS)`
    ///   syscall (one TLB shootdown) per contiguous run.
    pub(crate) fn try_defer_shadow_pieces(
        self: &Arc<Self>,
        offsets: &[(usize, usize)],
        fetched_after: Option<u64>,
    ) -> Vec<bool> {
        use windows_sys::Win32::System::Memory::{
            GetWriteWatch, MEM_COMMIT, MEM_PRIVATE, MEMORY_BASIC_INFORMATION, PAGE_READWRITE,
            VirtualQuery,
        };

        let mut out = vec![false; offsets.len()];
        if offsets.is_empty() || !lazy_copyback() || self.place.is_some() || self.window || self.rebar || self.transient() || self.data.is_null() {
            return out;
        }
        #[cfg(test)]
        if !self.lazy_opted_in && std::env::var_os("VICTORY_LAZY_COPYBACK").is_none_or(|v| v != "1") {
            return out;
        }

        let start = self.start as usize;
        let len = self.len as usize;
        let base = start / PAGE;
        let watched_ok = self.watched.load(Ordering::Relaxed) == 1;

        struct Cand {
            out_idx: usize,
            lo: usize,
            page: usize,
            page_idx: usize,
            copied_at: u64,
            section_view: Option<(usize, u64, u64)>,
        }

        let mut steady_hits = 0u64;
        let mut cands: Vec<Cand> = Vec::new();
        for (i, &(lo, hi)) in offsets.iter().enumerate() {
            let page_addr = start.saturating_add(lo);
            if hi.saturating_sub(lo) != PAGE || !page_addr.is_multiple_of(PAGE) || hi > len {
                continue;
            }
            let page = page_addr / PAGE;
            let idx = page - base;
            if idx >= self.lazy_state.len() {
                continue;
            }
            let state = self.lazy_state[idx].load(Ordering::Acquire);
            if state == LAZY_DEFERRED {
                out[i] = true;
                steady_hits += 1;
                continue;
            }
            if state == LAZY_EAGER && watched_ok {
                cands.push(Cand {
                    out_idx: i,
                    lo,
                    page,
                    page_idx: idx,
                    copied_at: 0,
                    section_view: None,
                });
            }
        }
        if steady_hits > 0 {
            LAZY_STATS[1].fetch_add(steady_hits, Ordering::Relaxed);
            lazy_record_deferred(steady_hits);
        }
        if cands.is_empty() {
            return out;
        }

        // Filter 1: whole-page initialized (`copied_at != 0`) under ONE `self.copied` lock.
        {
            let copied = self.copied.lock().unwrap_or_else(|e| e.into_inner());
            cands.retain_mut(|c| {
                let at = copied.get(c.page_idx).unwrap_or(0);
                c.copied_at = at;
                at != 0
            });
        }
        if cands.is_empty() {
            return out;
        }

        // Filter 2: no un-uploaded CPU dirty / newer sent bytes under ONE lock.
        if let Some(card) = &self.card
            && (card.has_dirty.load(Ordering::Acquire)
                || (fetched_after.is_some() && card.has_sent.load(Ordering::Acquire)))
        {
            let dirty = card.dirty.lock().unwrap_or_else(|e| e.into_inner());
            let sent = fetched_after.map(|_| card.sent.lock().unwrap_or_else(|e| e.into_inner()));
            cands.retain(|c| {
                let (lo, hi) = (c.lo, c.lo + PAGE);
                if dirty.iter().any(|&(a, b)| a < hi && b > lo) {
                    return false;
                }
                if let (Some(fa), Some(s)) = (fetched_after, sent.as_deref())
                    && s.iter().any(|&(a, b, seq)| seq > fa && a < hi && b > lo)
                {
                    return false;
                }
                true
            });
        }
        if cands.is_empty() {
            return out;
        }

        // Filter 3: journal check under ONE `JOURNAL` lock.
        {
            let queries: Vec<(usize, u64)> = cands.iter().map(|c| (c.page, c.copied_at)).collect();
            let written = crate::watch::was_written_since_batch(&queries);
            let mut k = 0;
            cands.retain(|_| {
                let ok = !written[k];
                k += 1;
                ok
            });
        }
        if cands.is_empty() {
            return out;
        }

        // Install VEH and pre-allocate readback channel ONCE before any `PAGE_NOACCESS`.
        crate::watch::install_veh();
        if !ensure_lazy_readback() {
            return out;
        }

        // Classify `MEM_MAPPED` (clean `watch`-guarded) vs `MEM_PRIVATE` under
        // ONE `GUARDS` lock + ONE `VIEWS` lock (zero `VirtualQuery` syscalls for
        // `MEM_MAPPED` views, whether registered or unregistered, and immediate
        // rejection of aliased or open-guard `MEM_MAPPED` pages!).
        let cand_pages: Vec<usize> = cands.iter().map(|c| c.page).collect();
        let clean_guarded = crate::watch::clean_guarded_mask(&cand_pages);
        let addrs: Vec<usize> = cands.iter().map(|c| c.page * PAGE).collect();
        let views_info = crate::inplace::single_view_pages_batch(&addrs);
        let mut mapped_cands: Vec<Cand> = Vec::new();
        let mut private_cands: Vec<Cand> = Vec::new();
        for ((mut c, is_clean_guarded), vinfo) in cands
            .into_iter()
            .zip(clean_guarded)
            .zip(views_info)
        {
            if is_clean_guarded {
                match vinfo {
                    crate::inplace::PageViewInfo::Aliased => {}
                    crate::inplace::PageViewInfo::Single(s, o, len) => {
                        c.section_view = Some((s, o, len));
                        mapped_cands.push(c);
                    }
                    crate::inplace::PageViewInfo::Unknown => {
                        c.section_view = None;
                        mapped_cands.push(c);
                    }
                }
            } else if vinfo == crate::inplace::PageViewInfo::Unknown {
                private_cands.push(c);
            }
        }

        mapped_cands.sort_unstable_by_key(|c| c.page);
        private_cands.sort_unstable_by_key(|c| c.page);
        // Preserve duplicate `out_idx` entries for identical pages so all
        // duplicate offsets receive `out[idx] = true` if their page arms.
        let mut dup_out: Vec<(usize, usize)> = Vec::new();
        let dedup_preserve = |v: &mut Vec<Cand>, dups: &mut Vec<(usize, usize)>| {
            let mut w = 0usize;
            for r in 0..v.len() {
                if w > 0 && v[w - 1].page == v[r].page {
                    dups.push((v[r].page, v[r].out_idx));
                } else {
                    if w != r {
                        v.swap(w, r);
                    }
                    w += 1;
                }
            }
            v.truncate(w);
        };
        dedup_preserve(&mut mapped_cands, &mut dup_out);
        dedup_preserve(&mut private_cands, &mut dup_out);

        let mut newly_armed = 0u64;
        let mut armed_pages_set: BTreeSet<usize> = BTreeSet::new();

        // ---- Path A: `MEM_MAPPED` single-view clean `watch`-guarded runs ----
        if !mapped_cands.is_empty() {
            let mut reserved: Vec<(Cand, Arc<FlushWait>)> = Vec::with_capacity(mapped_cands.len());
            {
                let mut table = LAZY_TABLE.lock().unwrap_or_else(|e| e.into_inner());
                for c in mapped_cands {
                    if table.pinned.contains(&c.page)
                        || table.is_remapping(c.page)
                        || table.pages.contains_key(&c.page)
                    {
                        continue;
                    }
                    let wait = Arc::new(FlushWait::new());
                    lazy_table_insert_busy(&mut table, c.page, self.id, wait.clone());
                    if lazy_is_held(c.page) {
                        lazy_table_remove_page(&mut table, c.page);
                        continue;
                    }
                    reserved.push((c, wait));
                }
            }
            if !reserved.is_empty() {
                let mut transitioned_set: BTreeSet<usize> = BTreeSet::new();
                let mut i = 0;
                while i < reserved.len() {
                    let run_first = reserved[i].0.page;
                    let mut run_last = run_first + 1;
                    i += 1;
                    while i < reserved.len() && reserved[i].0.page == run_last {
                        run_last += 1;
                        i += 1;
                    }
                    let ok_blocks = crate::watch::transition_clean_guards_run_to_noaccess(run_first, run_last);
                    transitioned_set.extend(ok_blocks);
                }

                #[cfg(test)]
                if !transitioned_set.is_empty()
                    && let Some(hook) = TRY_DEFER_POST_TRANSITION_HOOK
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .take()
                    && let Some(&first_page) = transitioned_set.iter().next()
                {
                    hook(first_page);
                }

                let transitioned_indices: Vec<usize> = reserved
                    .iter()
                    .enumerate()
                    .filter(|(_, (c, _))| transitioned_set.contains(&c.page))
                    .map(|(idx, _)| idx)
                    .collect();
                let post_queries: Vec<(usize, u64)> = transitioned_indices
                    .iter()
                    .map(|&idx| (reserved[idx].0.page, reserved[idx].0.copied_at))
                    .collect();
                let post_written = if post_queries.is_empty() {
                    Vec::new()
                } else {
                    crate::watch::was_written_since_batch(&post_queries)
                };
                let mut surviving = vec![false; reserved.len()];
                for (&r_idx, &w) in transitioned_indices.iter().zip(post_written.iter()) {
                    if !w {
                        surviving[r_idx] = true;
                    }
                }

                let mut to_rollback_vp: Vec<usize> = Vec::new();
                let mut to_remove: Vec<(usize, Arc<FlushWait>)> = Vec::new();
                let mut committed_waits: Vec<Arc<FlushWait>> = Vec::new();
                {
                    let mut table = LAZY_TABLE.lock().unwrap_or_else(|e| e.into_inner());
                    for (r_idx, (c, wait)) in reserved.into_iter().enumerate() {
                        if surviving[r_idx]
                            && !table.pinned.contains(&c.page)
                            && !table.is_remapping(c.page)
                            && !lazy_is_held(c.page)
                        {
                            lazy_table_insert_armed(
                                &mut table,
                                c.page,
                                ArmedPage {
                                    shadow: Arc::downgrade(self),
                                    shadow_id: self.id,
                                    offset: c.lo,
                                    page_idx: c.page_idx,
                                    section_view: c.section_view,
                                    is_mapped_view: true,
                                },
                            );
                            self.lazy_state[c.page_idx].store(LAZY_DEFERRED, Ordering::Release);
                            self.deferred_count.fetch_add(1, Ordering::AcqRel);
                            out[c.out_idx] = true;
                            if !dup_out.is_empty() {
                                armed_pages_set.insert(c.page);
                            }
                            newly_armed += 1;
                            committed_waits.push(wait);
                        } else {
                            if transitioned_set.contains(&c.page) {
                                to_rollback_vp.push(c.page);
                            }
                            to_remove.push((c.page, wait));
                        }
                    }
                }
                if !to_rollback_vp.is_empty() {
                    rollback_noaccess_pages_coalesced(&to_rollback_vp);
                }
                if !to_remove.is_empty() {
                    let mut table = LAZY_TABLE.lock().unwrap_or_else(|e| e.into_inner());
                    for &(page, _) in &to_remove {
                        lazy_table_remove_page(&mut table, page);
                    }
                    drop(table);
                    for (_, wait) in to_remove {
                        wait.finish(FlushOutcome::Accessible);
                    }
                }
                for wait in committed_waits {
                    wait.finish(FlushOutcome::ArmedRetry);
                }
            }
        }

        // ---- Path B: `MEM_PRIVATE | MEM_WRITE_WATCH` runs (cached `VirtualQuery`) ----
        if !private_cands.is_empty() {
            let mut cached: Option<(usize, usize, u32, u32, u32)> = None;
            private_cands.retain(|c| {
                let page_addr = c.page * PAGE;
                let (base_a, end_a, state, ty, prot) = match cached {
                    Some(m) if page_addr >= m.0 && page_addr + PAGE <= m.1 => m,
                    _ => {
                        // SAFETY: plain struct filled by `VirtualQuery`.
                        let mut mbi: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
                        // SAFETY: valid out pointer.
                        let n = unsafe { VirtualQuery(page_addr as *const _, &mut mbi, size_of_val(&mbi)) };
                        if n == 0 {
                            return false;
                        }
                        let m = (
                            mbi.BaseAddress as usize,
                            (mbi.BaseAddress as usize).saturating_add(mbi.RegionSize),
                            mbi.State,
                            mbi.Type,
                            mbi.Protect,
                        );
                        cached = Some(m);
                        m
                    }
                };
                state == MEM_COMMIT
                    && ty == MEM_PRIVATE
                    && prot == PAGE_READWRITE
                    && page_addr >= base_a
                    && page_addr + PAGE <= end_a
            });

            if !private_cands.is_empty() {
                let mut reserved: Vec<(Cand, Arc<FlushWait>)> = Vec::with_capacity(private_cands.len());
                {
                    let mut table = LAZY_TABLE.lock().unwrap_or_else(|e| e.into_inner());
                    for c in private_cands {
                        if table.pinned.contains(&c.page)
                            || table.is_remapping(c.page)
                            || table.pages.contains_key(&c.page)
                        {
                            continue;
                        }
                        let wait = Arc::new(FlushWait::new());
                        lazy_table_insert_busy(&mut table, c.page, self.id, wait.clone());
                        if lazy_is_held(c.page) {
                            lazy_table_remove_page(&mut table, c.page);
                            continue;
                        }
                        reserved.push((c, wait));
                    }
                }

                if !reserved.is_empty() {
                    let mut protected_set: BTreeSet<usize> = BTreeSet::new();
                    let mut ww_clean_set: BTreeSet<usize> = BTreeSet::new();
                    let mut touched_buf: Vec<usize> = Vec::new();
                    let mut i = 0;
                    while i < reserved.len() {
                        let run_first = reserved[i].0.page;
                        let mut run_last = run_first + 1;
                        i += 1;
                        while i < reserved.len() && reserved[i].0.page == run_last {
                            run_last += 1;
                            i += 1;
                        }
                        let mut run_prot: Vec<usize> = Vec::with_capacity(run_last - run_first);
                        protect_private_run_noaccess(run_first, run_last, &mut run_prot);
                        if run_prot.is_empty() {
                            continue;
                        }
                        protected_set.extend(run_prot.iter().copied());

                        let mut j = 0;
                        while j < run_prot.len() {
                            let sub_first = run_prot[j];
                            let mut sub_last = sub_first + 1;
                            j += 1;
                            while j < run_prot.len() && run_prot[j] == sub_last {
                                sub_last += 1;
                                j += 1;
                            }
                            let sub_pages = sub_last - sub_first;
                            let need = sub_pages * (PAGE / 4096);
                            if touched_buf.len() < need {
                                touched_buf.resize(need, 0);
                            }
                            let mut count = need;
                            let mut granularity = 0u32;
                            // SAFETY: non-resetting write-watch query over the contiguous run.
                            let ww_ok = unsafe {
                                GetWriteWatch(
                                    0,
                                    (sub_first * PAGE) as *const _,
                                    sub_pages * PAGE,
                                    touched_buf.as_mut_ptr().cast(),
                                    &mut count,
                                    &mut granularity,
                                ) == 0
                            };
                            if !ww_ok {
                                continue;
                            }
                            if count == 0 {
                                ww_clean_set.extend(sub_first..sub_last);
                            } else {
                                let mut dirty_pages: BTreeSet<usize> = BTreeSet::new();
                                for &addr in &touched_buf[..count] {
                                    dirty_pages.insert(addr / PAGE);
                                }
                                for p in sub_first..sub_last {
                                    if !dirty_pages.contains(&p) {
                                        ww_clean_set.insert(p);
                                    }
                                }
                            }
                        }
                    }

                    let clean_indices: Vec<usize> = reserved
                        .iter()
                        .enumerate()
                        .filter(|(_, (c, _))| ww_clean_set.contains(&c.page))
                        .map(|(idx, _)| idx)
                        .collect();
                    let post_queries: Vec<(usize, u64)> = clean_indices
                        .iter()
                        .map(|&idx| (reserved[idx].0.page, reserved[idx].0.copied_at))
                        .collect();
                    let post_written = if post_queries.is_empty() {
                        Vec::new()
                    } else {
                        crate::watch::was_written_since_batch(&post_queries)
                    };
                    let mut surviving = vec![false; reserved.len()];
                    for (&r_idx, &w) in clean_indices.iter().zip(post_written.iter()) {
                        if !w {
                            surviving[r_idx] = true;
                        }
                    }

                    let mut to_rollback_vp: Vec<usize> = Vec::new();
                    let mut to_remove: Vec<(usize, Arc<FlushWait>)> = Vec::new();
                    let mut committed_waits: Vec<Arc<FlushWait>> = Vec::new();
                    {
                        let mut table = LAZY_TABLE.lock().unwrap_or_else(|e| e.into_inner());
                        for (r_idx, (c, wait)) in reserved.into_iter().enumerate() {
                            if surviving[r_idx]
                                && !table.pinned.contains(&c.page)
                                && !table.is_remapping(c.page)
                                && !lazy_is_held(c.page)
                            {
                                lazy_table_insert_armed(
                                    &mut table,
                                    c.page,
                                    ArmedPage {
                                        shadow: Arc::downgrade(self),
                                        shadow_id: self.id,
                                        offset: c.lo,
                                        page_idx: c.page_idx,
                                        section_view: None,
                                        is_mapped_view: false,
                                    },
                                );
                                self.lazy_state[c.page_idx].store(LAZY_DEFERRED, Ordering::Release);
                                self.deferred_count.fetch_add(1, Ordering::AcqRel);
                                out[c.out_idx] = true;
                                if !dup_out.is_empty() {
                                    armed_pages_set.insert(c.page);
                                }
                                newly_armed += 1;
                                committed_waits.push(wait);
                            } else {
                                if protected_set.contains(&c.page) {
                                    to_rollback_vp.push(c.page);
                                }
                                to_remove.push((c.page, wait));
                            }
                        }
                    }
                    if !to_rollback_vp.is_empty() {
                        rollback_noaccess_pages_coalesced(&to_rollback_vp);
                    }
                    if !to_remove.is_empty() {
                        let mut table = LAZY_TABLE.lock().unwrap_or_else(|e| e.into_inner());
                        for &(page, _) in &to_remove {
                            lazy_table_remove_page(&mut table, page);
                        }
                        drop(table);
                        for (_, wait) in to_remove {
                            wait.finish(FlushOutcome::Accessible);
                        }
                    }
                    for wait in committed_waits {
                        wait.finish(FlushOutcome::ArmedRetry);
                    }
                }
            }
        }

        if !dup_out.is_empty() {
            for (page, extra_idx) in dup_out {
                if armed_pages_set.contains(&page) {
                    out[extra_idx] = true;
                }
            }
        }
        if newly_armed > 0 {
            LAZY_STATS[0].fetch_add(newly_armed, Ordering::Relaxed);
            lazy_record_deferred(newly_armed);
        }
        out
    }

    /// Single-piece wrapper around [`Self::try_defer_shadow_pieces`].
    #[cfg(test)]
    pub(crate) fn try_defer_piece(self: &Arc<Self>, lo: usize, hi: usize) -> bool {
        self.try_defer_shadow_pieces(&[(lo, hi)], None)[0]
    }

    /// Relay variant of [`Self::try_defer_piece`]: also refuses to defer any
    /// newly-armed piece that has pending/later CPU uploads (`dirty` / `sent`).
    #[cfg(test)]
    pub(crate) fn try_defer_piece_relay(self: &Arc<Self>, lo: usize, hi: usize, fetched_after: u64) -> bool {
        self.try_defer_shadow_pieces(&[(lo, hi)], Some(fetched_after))[0]
    }

    /// Applies any unapplied bytes of a VEH-flushed 64 KiB baseline into `dst`
    /// (covering shadow offsets `[at, at + dst.len())`) before eager 3-way merge
    /// so CPU writes made after a mid-flight VEH flush are never overwritten by
    /// stale pre-GPU staging/snapshot bytes. Consumes applied byte subranges so
    /// back-to-back merges (`card_merge_relay` / `relay_merge`) are idempotent
    /// and never rewind `self.data`/`before` to an older baseline.
    pub(super) fn lazy_apply_flushed_baseline(&self, at: usize, dst: &mut [u8]) {
        if dst.is_empty() {
            return;
        }
        let start = self.start as usize;
        let base = start / PAGE;
        let first = (start + at) / PAGE;
        let last = (start + at + dst.len()).div_ceil(PAGE);
        let mut table = LAZY_TABLE.lock().unwrap_or_else(|e| e.into_inner());
        for page in first..last {
            let idx = page - base;
            if idx >= self.lazy_state.len() || self.lazy_state[idx].load(Ordering::Acquire) != LAZY_RESYNC {
                continue;
            }
            let page_addr = page * PAGE;
            let lo = page_addr.max(start + at);
            let hi = (page_addr + PAGE).min(start + at + dst.len());
            if lo >= hi {
                continue;
            }
            let src_lo = lo - page_addr;
            let src_hi = hi - page_addr;
            let dst_off = lo - (start + at);
            let n = hi - lo;
            let remove = if let Some(baseline) = table.flushed_baselines.get_mut(&(self.id, idx)) {
                baseline.apply_once(src_lo, src_hi, &mut dst[dst_off..dst_off + n])
            } else {
                false
            };
            if remove {
                table.flushed_baselines.remove(&(self.id, idx));
            }
        }
    }

    /// Copies specific flushed pages from title memory into this shadow (used by
    /// [`Shadow::writing`] after [`lazy_flush_other_shadows`] materializes another
    /// overlapping shadow's deferred pages), skipping any page intersecting
    /// `skip_busy` (`self.pending`).
    pub(super) fn resync_pages_from_title(&self, pages: &[usize], skip_busy: &[(usize, usize)]) {
        let (start, end) = (self.start as usize, (self.start + self.len) as usize);
        let base = start / PAGE;
        let watched = self.watched.load(Ordering::Relaxed) == 1;
        for &page in pages {
            if page < base {
                continue;
            }
            let idx = page - base;
            if idx >= self.lazy_state.len() {
                continue;
            }
            let page_lo = page * PAGE;
            let page_hi = page_lo.saturating_add(PAGE);
            if skip_busy.iter().any(|&(ba, be)| ba < page_hi && be > page_lo) {
                continue;
            }
            let p_lo = page_lo.max(start);
            let p_hi = page_hi.min(end);
            if p_lo >= p_hi {
                continue;
            }
            let whole = p_lo == page_lo && p_hi == page_hi;
            let now = if watched {
                crate::watch::writes(p_lo, p_hi, true).map_or(0, |(now, _)| now)
            } else {
                0
            };
            self.copy(p_lo, p_hi);
            let mut copied = self.copied.lock().unwrap_or_else(|e| e.into_inner());
            copied.set(idx, if whole { now } else { 0 });
        }
    }

    /// Transitions `LAZY_RESYNC` pages in `[from, to)` back to `LAZY_EAGER`
    /// (skipping any page overlapping `skip_busy`) and clears their `copied`
    /// entry so [`Shadow::update_free`] copies fresh title memory (including
    /// any CPU writes made after VEH flush) into staging and schedules VRAM
    /// upload (`card_changed`).
    pub(super) fn lazy_prepare_update(&self, from: usize, to: usize, skip_busy: &[(usize, usize)]) {
        if self.resync_count.load(Ordering::Acquire) == 0 || from >= to {
            return;
        }
        let base = self.start as usize / PAGE;
        let first = from / PAGE;
        let last = to.div_ceil(PAGE);
        let mut resynced = Vec::new();
        for page in first..last {
            let page_lo = page * PAGE;
            let page_hi = page_lo.saturating_add(PAGE);
            if skip_busy.iter().any(|&(ba, be)| ba < page_hi && be > page_lo) {
                continue;
            }
            let idx = page - base;
            if idx < self.lazy_state.len()
                && self.lazy_state[idx]
                    .compare_exchange(LAZY_RESYNC, LAZY_EAGER, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
            {
                self.resync_count.fetch_sub(1, Ordering::AcqRel);
                resynced.push(idx);
            }
        }
        if resynced.is_empty() {
            return;
        }
        {
            let mut copied = self.copied.lock().unwrap_or_else(|e| e.into_inner());
            for &idx in &resynced {
                copied.set(idx, 0);
            }
        }
        let mut table = LAZY_TABLE.lock().unwrap_or_else(|e| e.into_inner());
        for idx in resynced {
            table.flushed_baselines.remove(&(self.id, idx));
        }
    }

    /// Resyncs `LAZY_RESYNC` pages in `[from, to)` (skipping any page
    /// overlapping `skip_busy`) directly from title memory into `self.data`
    /// (and queues `card_changed`) before a UAV-only submission declared via
    /// [`Shadow::writing`] runs without a preceding `refresh`.
    pub(super) fn lazy_resync_range(&self, from: usize, to: usize, skip_busy: &[(usize, usize)]) {
        let (start, end) = (self.start as usize, (self.start + self.len) as usize);
        let (from, to) = (from.max(start), to.min(end));
        if from >= to {
            return;
        }
        let base = start / PAGE;
        let first = from / PAGE;
        let last = to.div_ceil(PAGE);
        let mut pages = Vec::new();
        for page in first..last {
            let page_lo = page * PAGE;
            let page_hi = page_lo.saturating_add(PAGE);
            if skip_busy.iter().any(|&(ba, be)| ba < page_hi && be > page_lo) {
                continue;
            }
            let idx = page - base;
            if idx < self.lazy_state.len()
                && self.lazy_state[idx]
                    .compare_exchange(LAZY_RESYNC, LAZY_EAGER, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
            {
                self.resync_count.fetch_sub(1, Ordering::AcqRel);
                pages.push((page, idx));
            }
        }
        if pages.is_empty() {
            return;
        }
        {
            let mut table = LAZY_TABLE.lock().unwrap_or_else(|e| e.into_inner());
            for &(_, idx) in &pages {
                table.flushed_baselines.remove(&(self.id, idx));
            }
        }
        let watched = self.watched.load(Ordering::Relaxed) == 1;
        for (page, idx) in pages {
            let p_lo = (page * PAGE).max(start);
            let p_hi = ((page + 1) * PAGE).min(end);
            if p_lo >= p_hi {
                continue;
            }
            let whole = p_lo == page * PAGE && p_hi == (page + 1) * PAGE;
            let now = if watched {
                crate::watch::writes(p_lo, p_hi, true).map_or(0, |(now, _)| now)
            } else {
                u64::from(whole)
            };
            self.copy(p_lo, p_hi);
            let mut copied = self.copied.lock().unwrap_or_else(|e| e.into_inner());
            copied.set(idx, if whole { now } else { 0 });
        }
    }

    /// Unwatched copy of `[from, to)` that skips `LAZY_DEFERRED` / `LAZY_FLUSHING`
    /// pages so GPU-resident pages are never spuriously flushed or overwritten.
    pub(super) fn copy_unwatched_skipping_deferred(&self, from: usize, to: usize) -> usize {
        let (start, end) = (self.start as usize, (self.start + self.len) as usize);
        let (from, to) = (from.max(start), to.min(end));
        if from >= to {
            return 0;
        }
        let base = start / PAGE;
        let first = from / PAGE;
        let last = to.div_ceil(PAGE);
        let mut bytes = 0;
        let mut seg_start: Option<usize> = None;
        for page in first..last {
            let idx = page - base;
            let skip = idx < self.lazy_state.len() && is_lazy_held(self.lazy_state[idx].load(Ordering::Acquire));
            let p_lo = (page * PAGE).max(from);
            let p_hi = ((page + 1) * PAGE).min(to);
            if skip {
                if let Some(s) = seg_start.take() {
                    bytes += self.copy(s, p_lo);
                }
            } else {
                seg_start.get_or_insert(p_lo);
                if page + 1 == last
                    && let Some(s) = seg_start.take()
                {
                    bytes += self.copy(s, p_hi);
                }
            }
        }
        bytes
    }

    /// Flushes every deferred page owned by this `Shadow` in coalesced batches.
    /// Pass `save_baseline = true` from `unregister` (where in-flight lists may
    /// still hold `Arc<Shadow>` and settle via `card_merge_relay`), or `false`
    /// from `Drop::drop` (which also removes any remaining `flushed_baselines`).
    pub(super) fn lazy_flush_all(&self, save_baseline: bool) {
        let first = self.start as usize / PAGE;
        let last = (self.start.saturating_add(self.len) as usize).div_ceil(PAGE);
        if lazy_range_may_have_armed(first, last) {
            loop {
                let mut claimed: Vec<(usize, usize, usize, Option<(usize, u64, u64)>, Arc<FlushWait>)> =
                    Vec::new();
                let mut wait_on: Option<Arc<FlushWait>> = None;
                {
                    let mut table = LAZY_TABLE.lock().unwrap_or_else(|e| e.into_inner());
                    let keys: Vec<usize> = table
                        .pages
                        .range(first..last)
                        .filter_map(|(&page, entry)| match entry {
                            LazyEntry::Armed(a) if a.shadow_id == self.id => Some(page),
                            LazyEntry::Busy { shadow_id, .. } if *shadow_id == self.id => Some(page),
                            _ => None,
                        })
                        .take(LAZY_READBACK_BATCH_PAGES)
                        .collect();
                    for page in keys {
                        match table.pages.get(&page) {
                            Some(LazyEntry::Busy { wait, .. }) => {
                                if claimed.is_empty() {
                                    wait_on = Some(wait.clone());
                                    break;
                                }
                            }
                            Some(LazyEntry::Armed(armed)) => {
                                let (offset, page_idx, section_view) =
                                    (armed.offset, armed.page_idx, armed.section_view);
                                let wait = Arc::new(FlushWait::new());
                                self.lazy_state[page_idx].store(LAZY_FLUSHING, Ordering::Release);
                                lazy_table_insert_busy(&mut table, page, self.id, wait.clone());
                                claimed.push((page, offset, page_idx, section_view, wait));
                            }
                            None => {}
                        }
                    }
                }
                if !claimed.is_empty() {
                    let batch: Vec<ClaimedFlushPage<'_>> = claimed
                        .iter()
                        .map(|(page, offset, page_idx, section_view, wait)| ClaimedFlushPage {
                            page: *page,
                            shadow: self,
                            offset: *offset,
                            page_idx: *page_idx,
                            section_view: *section_view,
                            wait: wait.clone(),
                        })
                        .collect();
                    flush_claimed_batch(&batch, save_baseline);
                    continue;
                }
                if let Some(wait) = wait_on {
                    let _ = wait.wait();
                    continue;
                }
                break;
            }
        }
        if !save_baseline {
            let mut table = LAZY_TABLE.lock().unwrap_or_else(|e| e.into_inner());
            table.flushed_baselines.retain(|&(sid, _), _| sid != self.id);
        }
    }

    /// Flushes every deferred page owned by this `Shadow` and removes all of its
    /// `flushed_baselines` during `Drop::drop` without relying on `Weak::upgrade()`.
    pub(super) fn lazy_flush_all_on_drop(&mut self) {
        self.lazy_flush_all(false);
    }
}

/// Fast lock-free stripe check for byte range `[at, at + len)`: returns `true`
/// if any stripe covering the range currently has an armed or busy lazy page.
/// Used by `host::diff_copy_direct` to fall back to `read_into_fast` (self-bypass)
/// without triggering a VEH fault on another overlapping shadow's deferred page.
#[inline]
pub(crate) fn lazy_range_may_have_armed_bytes(at: usize, len: usize) -> bool {
    if len == 0 || LAZY_ARMED_COUNT.load(Ordering::Acquire) == 0 {
        return false;
    }
    lazy_range_may_have_armed(at / PAGE, at.saturating_add(len).div_ceil(PAGE))
}

/// Filters `relay_hand_back` differing `(at, n)` pieces, deferring eligible
/// full-page pieces when `VICTORY_LAZY_COPYBACK=1`.
pub(crate) fn filter_relay_found(shadow: &Arc<Shadow>, found: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
    if !lazy_copyback() || found.is_empty() {
        return found;
    }
    let pieces: Vec<(usize, usize)> = found.iter().map(|&(at, n)| (at, at + n)).collect();
    let deferred = shadow.try_defer_shadow_pieces(&pieces, None);
    if !deferred.iter().any(|&d| d) {
        return found;
    }
    found
        .into_iter()
        .zip(deferred)
        .filter_map(|(entry, is_def)| (!is_def).then_some(entry))
        .collect()
}

/// Filters `relay_card_back` unkept `pieces` before issuing GPU readback copies.
pub(crate) fn filter_card_pieces(
    pieces: &mut Vec<(Arc<Shadow>, usize, usize, u64)>,
    total: &mut u64,
) {
    if !lazy_copyback() || pieces.is_empty() {
        return;
    }
    let reqs: Vec<(Arc<Shadow>, usize, usize, Option<u64>)> = pieces
        .iter()
        .map(|(s, lo, hi, _)| (s.clone(), *lo, *hi, None))
        .collect();
    let deferred = try_defer_pieces_batch_with_after(&reqs);
    if deferred.iter().any(|&d| d) {
        let mut kept_pieces = Vec::with_capacity(pieces.len());
        *total = 0;
        for (entry, is_def) in std::mem::take(pieces).into_iter().zip(deferred) {
            if !is_def {
                let len = (entry.2 - entry.1) as u64;
                kept_pieces.push((entry.0, entry.1, entry.2, *total));
                *total += len;
            }
        }
        *pieces = kept_pieces;
    }
}

/// Filters `relay_card_back` kept-slot pieces before issuing GPU readback copies.
pub(crate) fn filter_card_kept(
    kept: &mut Vec<(Arc<Shadow>, usize, usize, u64, u64, u64)>,
    kept_total: &mut u64,
) {
    if !lazy_copyback() || kept.is_empty() {
        return;
    }
    let reqs: Vec<(Arc<Shadow>, usize, usize, Option<u64>)> = kept
        .iter()
        .map(|(s, lo, hi, _, _, after)| (s.clone(), *lo, *hi, Some(*after)))
        .collect();
    let deferred = try_defer_pieces_batch_with_after(&reqs);
    if deferred.iter().any(|&d| d) {
        let mut remaining = Vec::with_capacity(kept.len());
        *kept_total = 0;
        for (entry, is_def) in std::mem::take(kept).into_iter().zip(deferred) {
            if !is_def {
                let len = (entry.2 - entry.1) as u64;
                remaining.push((entry.0, entry.1, entry.2, *kept_total, entry.4, entry.5));
                *kept_total += len;
            }
        }
        *kept = remaining;
    }
}

/// Filters inflight-fetched card pieces before `merge_landed`.
pub(crate) fn filter_inflight_pieces(
    pieces: Vec<(Arc<Shadow>, usize, usize, usize, u64)>,
) -> Vec<(Arc<Shadow>, usize, usize, usize, u64)> {
    if !lazy_copyback() || pieces.is_empty() {
        return pieces;
    }
    let reqs: Vec<(Arc<Shadow>, usize, usize, Option<u64>)> = pieces
        .iter()
        .map(|(s, lo, hi, _, after)| (s.clone(), *lo, *hi, Some(*after)))
        .collect();
    let deferred = try_defer_pieces_batch_with_after(&reqs);
    if !deferred.iter().any(|&d| d) {
        return pieces;
    }
    pieces
        .into_iter()
        .zip(deferred)
        .filter_map(|(entry, is_def)| (!is_def).then_some(entry))
        .collect()
}

/// Filters synchronous `card_back` indexed pieces before `CopyBufferRegion` + CPU wait.
pub(crate) fn filter_indexed_pieces(
    pieces: &mut Vec<(usize, usize, usize, u64)>,
    total: &mut u64,
    shadow_of: impl Fn(usize) -> Arc<Shadow>,
) {
    if !lazy_copyback() || pieces.is_empty() {
        return;
    }
    let reqs: Vec<(Arc<Shadow>, usize, usize, Option<u64>)> = pieces
        .iter()
        .map(|&(i, lo, hi, _)| (shadow_of(i), lo, hi, None))
        .collect();
    let deferred = try_defer_pieces_batch_with_after(&reqs);
    if deferred.iter().any(|&d| d) {
        let mut remaining = Vec::with_capacity(pieces.len());
        *total = 0;
        for (entry, is_def) in std::mem::take(pieces).into_iter().zip(deferred) {
            if !is_def {
                let len = (entry.2 - entry.1) as u64;
                remaining.push((entry.0, entry.1, entry.2, *total));
                *total += len;
            }
        }
        *pieces = remaining;
    }
}
