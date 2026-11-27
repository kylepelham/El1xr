//! Which title memory the title wrote since a host copy took it (decision
//! 0007, "Copies of title memory"). The runtime reserves title GPU memory
//! with write watching (`victory-hle` xmem), so the kernel keeps a dirty bit
//! per 4 KiB page; asking for a range's written pages also clears them. Host
//! copies of the same memory can overlap (nested buffers, textures in
//! buffers), so what one asks is kept here for the others: a journal of
//! each page's last write, numbered by the asking.
//!
//! Physical-page views (xmem's sections) cannot be write-watched. There a
//! copy leaves each 64 KiB block it took read-only, and our vectored handler
//! takes the first write to it: it notes the block as written and gives the
//! write access back. The runtime tells us when it maps or unmaps views
//! ([`title_memory_remapped`]), which resets their protection. Blocks found
//! wholly uncommitted are skipped while they remain so; one query can
//! confirm a whole region, including ordinary reservations between views.
//!
//! Writes into physical pages through another view are seen too: the
//! runtime tells us the views of the pages it wrote itself ([`writing`],
//! xmem's `write_physical` and DirectStorage's page destinations), and a
//! write by the title through any view of a guarded page faults like one
//! at the guarded address (the guard makes every view of the page
//! read-only, [`open`] gives them all back; not a page of more than
//! [`MANY_VIEWS`] views). A
//! system call writing into a guarded block fails (none is known to).

use std::collections::BTreeMap;
use std::sync::Mutex;

/// The unit tracked and copied: 64 KiB, the console's page (the kernel's
/// 4 KiB pages fold into it). Finer units made the checks of the world's
/// big buffers cost more than the copies they saved.
pub(crate) const PAGE: usize = 64 << 10;
/// The kernel's page, the unit of its answers.
const KERNEL_PAGE: usize = 4096;

/// A title-memory range whose changed pages another subsystem needs to read.
pub(crate) struct Tracked {
    start: usize,
    end: usize,
    seen: Mutex<std::collections::BTreeMap<usize, u64>>,
}

/// Tracks a runtime GPU-memory range, including physical-page views when
/// `views` is true. Other process memory is not tracked.
pub(crate) fn track(start: usize, len: usize, views: bool) -> Option<Tracked> {
    let end = start.checked_add(len)?;
    if len == 0 || writes(start, end, views).is_none() {
        return None;
    }
    Some(Tracked {
        start,
        end,
        seen: Mutex::default(),
    })
}

mod reservation;
pub(crate) use reservation::line as reservation_line;

struct Journal {
    /// Room for the kernel's answer.
    found: Vec<usize>,
    /// Page ranges asked ahead for a submission's refreshes ([`ask_ahead`])
    /// and the asking that did: `writes` answers for a range inside one
    /// from the journal, without another kernel call.
    ahead: Vec<(usize, usize)>,
    ordered: bool,
    ahead_asked: u64,
    /// The ahead asking stands for a whole submission ([`ask_once`]): no
    /// other ahead asking is made, and `ahead` stays, until
    /// [`ask_once_end`].
    kept: bool,
}

// (Neptune) Read by many at once: the refreshes of a submission run on several threads, and
// each asks the journal (`writes`); behind one lock they stood in line (a third of the
// submitting thread's time in a sample).
static JOURNAL: std::sync::RwLock<Journal> = std::sync::RwLock::new(Journal {
    found: Vec::new(),
    ahead: Vec::new(),
    ordered: false,
    ahead_asked: 0,
    kept: false,
});

/// Lock-free mirror of [`Journal::kept`]: `true` while a submission-wide
/// [`ask_once`] asking stands, updated under `JOURNAL.write()`.
static KEPT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// (Neptune) Numbers the askings; 0 is "never". Outside the journal's lock, with the pages
/// below: the refreshes of a submission run on several threads and each asks what was
/// written in its range, the thread that copies back notes its own writes, and behind one
/// lock they stood in line (two fifths of the submitting thread's time in two samples).
static ASKED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The next asking's number.
fn next_asking() -> u64 {
    ASKED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1
}

/// Journal pages to a part of the table: 4 GiB of addresses.
const PART: usize = 1 << 16;

/// Page number → the asking that last found it written (0: never), for one part.
struct Part([std::sync::atomic::AtomicU64; PART]);

/// The parts by `page / PART`, made when a page of theirs is first noted written and
/// kept for good.
static PARTS: [std::sync::atomic::AtomicPtr<Part>; 1 << 16] =
    [const { std::sync::atomic::AtomicPtr::new(std::ptr::null_mut()) }; 1 << 16];

fn part(page: usize, make: bool) -> Option<&'static Part> {
    part_of(&PARTS, page, make)
}

/// The clean blocks of the guards again (`Guards::clean`), 1 for a clean block: a table read
/// without the guards' lock. It is written under that lock, with the set. A refresh of view
/// memory asks whether its blocks are all clean, nearly all are, and the refreshes of a
/// submission run on several threads: behind the lock they waited for every thread that
/// was setting or opening a guard (an eighth of the submitting thread's time in a sample).
static CLEAN: [std::sync::atomic::AtomicPtr<Part>; 1 << 16] =
    [const { std::sync::atomic::AtomicPtr::new(std::ptr::null_mut()) }; 1 << 16];

/// Blocks of view memory that are BOTH read-only (`Guards::clean`) AND have all
/// earlier writes published into [`PARTS`] (`VICTORY_LIGHT_REFRESH`, default on
/// unless `=0`). Cleared with `SeqCst` *before* `VirtualProtect` restores write
/// access; set with `SeqCst` *after* `written_set` writes `PARTS`.
static PUBLISHED: [std::sync::atomic::AtomicPtr<Part>; 1 << 16] =
    [const { std::sync::atomic::AtomicPtr::new(std::ptr::null_mut()) }; 1 << 16];

/// Whether `VICTORY_LIGHT_REFRESH` is enabled (on by default; `=0` turns it off).
pub(crate) fn light() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_LIGHT_REFRESH").is_none_or(|v| v != "0"))
}

/// A bit per [`BLOCK`] of the 48-bit address space, in 8 KiB pieces (4 GiB of
/// blocks each) allocated on first use and never freed.
struct Bits([std::sync::atomic::AtomicPtr<BitsPiece>; Bits::PIECES]);

type BitsPiece = [std::sync::atomic::AtomicU64; Bits::WORDS];

/// Bitmask of the blocks in `[first, last)` that fall inside the 64-block word
/// starting at `base`.
fn in_word(base: usize, first: usize, last: usize) -> u64 {
    let lo = first.saturating_sub(base);
    let hi = last.saturating_sub(base).min(64);
    if lo >= hi {
        return 0;
    }
    (!0u64 << lo) & (!0u64 >> (64 - hi))
}

impl Bits {
    /// Words per piece: 1024 * 64 = 65,536 blocks = 4 GiB.
    const WORDS: usize = 1 << 10;
    /// Pieces covering the 48-bit address space (matching [`PARTS`]).
    const PIECES: usize = 1 << 16;
    const MAX_BLOCKS: usize = Bits::PIECES * Bits::WORDS * 64;

    const fn new() -> Bits {
        Bits([const { std::sync::atomic::AtomicPtr::new(std::ptr::null_mut()) }; Bits::PIECES])
    }

    fn piece(&self, word: usize) -> Option<&BitsPiece> {
        let ptr = self
            .0
            .get(word / Bits::WORDS)?
            .load(std::sync::atomic::Ordering::Acquire);
        // SAFETY: null, or a piece allocated by `piece_or_make` which is never freed.
        unsafe { ptr.as_ref() }
    }

    fn piece_or_make(&self, word: usize) -> Option<&BitsPiece> {
        use std::sync::atomic::Ordering::{AcqRel, Acquire};
        let place = self.0.get(word / Bits::WORDS)?;
        let mut ptr = place.load(Acquire);
        if ptr.is_null() {
            let layout = std::alloc::Layout::new::<BitsPiece>();
            // SAFETY: non-zero layout; zeroed memory is a valid `BitsPiece`.
            let fresh = unsafe { std::alloc::alloc_zeroed(layout) }.cast::<BitsPiece>();
            if fresh.is_null() {
                return None;
            }
            match place.compare_exchange(std::ptr::null_mut(), fresh, AcqRel, Acquire) {
                Ok(_) => ptr = fresh,
                Err(other) => {
                    // SAFETY: allocated just above with this layout, never shared.
                    unsafe { std::alloc::dealloc(fresh.cast(), layout) };
                    ptr = other;
                }
            }
        }
        // SAFETY: a piece once made is never freed.
        Some(unsafe { &*ptr })
    }

    #[cfg(test)]
    fn set(&self, block: usize) {
        let word = block / 64;
        if let Some(piece) = self.piece_or_make(word) {
            piece[word % Bits::WORDS]
                .fetch_or(1u64 << (block % 64), std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// Sets `block` if it was not set; returns whether it was newly set.
    fn try_set(&self, block: usize) -> bool {
        self.set_was(block) == Some(false)
    }

    /// Sets `block`; returns whether it was set before (`None`: out of address space).
    fn set_was(&self, block: usize) -> Option<bool> {
        let word = block / 64;
        let bit = 1u64 << (block % 64);
        let piece = self.piece_or_make(word)?;
        Some(
            piece[word % Bits::WORDS].fetch_or(bit, std::sync::atomic::Ordering::SeqCst) & bit != 0,
        )
    }

    /// Clears `block`; returns whether it was set before.
    fn clear_was(&self, block: usize) -> bool {
        let word = block / 64;
        let bit = 1u64 << (block % 64);
        self.piece(word).is_some_and(|piece| {
            piece[word % Bits::WORDS].fetch_and(!bit, std::sync::atomic::Ordering::SeqCst) & bit
                != 0
        })
    }

    /// Blocks set in `[first, last)`, in ascending order.
    #[cfg_attr(not(test), allow(dead_code))]
    fn set_in(&self, first: usize, last: usize) -> impl Iterator<Item = usize> + '_ {
        let last = last.min(Bits::MAX_BLOCKS).max(first);
        let mut word = first / 64;
        let end = last.div_ceil(64);
        if first >= last {
            word = end;
        }
        let mut bits = 0u64;
        std::iter::from_fn(move || {
            loop {
                if bits != 0 {
                    let bit = bits.trailing_zeros() as usize;
                    bits &= bits - 1;
                    return Some((word - 1) * 64 + bit);
                }
                if word >= end {
                    return None;
                }
                match self.piece(word) {
                    Some(piece) => {
                        bits = piece[word % Bits::WORDS].load(std::sync::atomic::Ordering::SeqCst)
                            & in_word(word * 64, first, last);
                        word += 1;
                    }
                    None => word = (word / Bits::WORDS + 1) * Bits::WORDS,
                }
            }
        })
    }

    /// Total set bits across all allocated pieces.
    #[cfg_attr(not(test), allow(dead_code))]
    fn count(&self) -> usize {
        self.0
            .iter()
            .filter_map(|p| {
                // SAFETY: null, or a piece allocated by `piece_or_make` which is never freed.
                unsafe { p.load(std::sync::atomic::Ordering::Acquire).as_ref() }
            })
            .map(|piece| {
                piece
                    .iter()
                    .map(|w| w.load(std::sync::atomic::Ordering::SeqCst).count_ones() as usize)
                    .sum::<usize>()
            })
            .sum()
    }

    fn set_range_counted(&self, first: usize, last: usize) -> usize {
        if first >= last {
            return 0;
        }
        let last = last.min(Bits::MAX_BLOCKS);
        if first >= last {
            return 0;
        }
        let mut added = 0usize;
        let mut word = first / 64;
        let end = last.div_ceil(64);
        while word < end {
            let Some(piece) = self.piece_or_make(word) else {
                return added;
            };
            let stop = ((word / Bits::WORDS + 1) * Bits::WORDS).min(end);
            while word < stop {
                let mask = in_word(word * 64, first, last);
                let cell = &piece[word % Bits::WORDS];
                let prev = cell.fetch_or(mask, std::sync::atomic::Ordering::SeqCst);
                added += (mask & !prev).count_ones() as usize;
                word += 1;
            }
        }
        added
    }

    fn clear_range_counted(&self, first: usize, last: usize) -> usize {
        if first >= last {
            return 0;
        }
        let last = last.min(Bits::MAX_BLOCKS);
        if first >= last {
            return 0;
        }
        let mut removed = 0usize;
        let mut word = first / 64;
        let end = last.div_ceil(64);
        while word < end {
            match self.piece(word) {
                Some(piece) => {
                    let stop = ((word / Bits::WORDS + 1) * Bits::WORDS).min(end);
                    while word < stop {
                        let mask = in_word(word * 64, first, last);
                        let cell = &piece[word % Bits::WORDS];
                        if cell.load(std::sync::atomic::Ordering::Acquire) & mask != 0 {
                            let prev = cell.fetch_and(!mask, std::sync::atomic::Ordering::SeqCst);
                            removed += (prev & mask).count_ones() as usize;
                        }
                        word += 1;
                    }
                }
                None => word = (word / Bits::WORDS + 1) * Bits::WORDS,
            }
        }
        removed
    }

    fn take_range_runs(&self, first: usize, last: usize) -> (Vec<(usize, usize)>, usize) {
        let mut runs: Vec<(usize, usize)> = Vec::new();
        if first >= last {
            return (runs, 0);
        }
        let last = last.min(Bits::MAX_BLOCKS);
        if first >= last {
            return (runs, 0);
        }
        let mut removed = 0usize;
        let mut word = first / 64;
        let end = last.div_ceil(64);
        while word < end {
            match self.piece(word) {
                Some(piece) => {
                    let stop = ((word / Bits::WORDS + 1) * Bits::WORDS).min(end);
                    while word < stop {
                        let base = word * 64;
                        let mask = in_word(base, first, last);
                        let cell = &piece[word % Bits::WORDS];
                        if cell.load(std::sync::atomic::Ordering::Acquire) & mask != 0 {
                            let mut cleared =
                                cell.fetch_and(!mask, std::sync::atomic::Ordering::SeqCst) & mask;
                            if cleared == !0u64 {
                                removed += 64;
                                match runs.last_mut() {
                                    Some((_, e)) if *e == base => *e = base + 64,
                                    _ => runs.push((base, base + 64)),
                                }
                            } else if cleared != 0 {
                                removed += cleared.count_ones() as usize;
                                while cleared != 0 {
                                    let lo = cleared.trailing_zeros() as usize;
                                    let run_len = (cleared >> lo).trailing_ones() as usize;
                                    let a = base + lo;
                                    let b = a + run_len;
                                    let run_mask = if run_len == 64 {
                                        !0u64
                                    } else {
                                        ((1u64 << run_len) - 1) << lo
                                    };
                                    cleared &= !run_mask;
                                    match runs.last_mut() {
                                        Some((_, e)) if *e == a => *e = b,
                                        _ => runs.push((a, b)),
                                    }
                                }
                            }
                        }
                        word += 1;
                    }
                }
                None => word = (word / Bits::WORDS + 1) * Bits::WORDS,
            }
        }
        (runs, removed)
    }

    fn count_in(&self, first: usize, last: usize) -> usize {
        if first >= last {
            return 0;
        }
        let last = last.min(Bits::MAX_BLOCKS);
        if first >= last {
            return 0;
        }
        let mut total = 0usize;
        let mut word = first / 64;
        let end = last.div_ceil(64);
        while word < end {
            match self.piece(word) {
                Some(piece) => {
                    let stop = ((word / Bits::WORDS + 1) * Bits::WORDS).min(end);
                    while word < stop {
                        let mask = in_word(word * 64, first, last);
                        total += (piece[word % Bits::WORDS]
                            .load(std::sync::atomic::Ordering::Acquire)
                            & mask)
                            .count_ones() as usize;
                        word += 1;
                    }
                }
                None => word = (word / Bits::WORDS + 1) * Bits::WORDS,
            }
        }
        total
    }

    /// If `block` is set (with `block < limit`), returns the exclusive end of
    /// its contiguous run of set bits clamped to `limit`.
    fn run_end_in(&self, block: usize, limit: usize) -> Option<usize> {
        let limit = limit.min(Bits::MAX_BLOCKS);
        if block >= limit {
            return None;
        }
        let mut word = block / 64;
        let bit = block % 64;
        let first_piece = self.piece(word)?;
        let w = first_piece[word % Bits::WORDS].load(std::sync::atomic::Ordering::Acquire);
        if w & (1u64 << bit) == 0 {
            return None;
        }
        let ones = (w >> bit).trailing_ones() as usize;
        if bit + ones < 64 {
            return Some((block + ones).min(limit));
        }
        let mut cur = (word + 1) * 64;
        word += 1;
        while cur < limit {
            let Some(piece) = self.piece(word) else {
                return Some(cur);
            };
            let w = piece[word % Bits::WORDS].load(std::sync::atomic::Ordering::Acquire);
            if w == !0u64 {
                cur = cur.saturating_add(64);
                word += 1;
            } else {
                return Some((cur + w.trailing_ones() as usize).min(limit));
            }
        }
        Some(limit)
    }

    /// First set bit in `[first, last)`, or `None` if all bits are 0.
    fn first_in(&self, first: usize, last: usize) -> Option<usize> {
        let last = last.min(Bits::MAX_BLOCKS);
        if first >= last {
            return None;
        }
        let mut word = first / 64;
        let end = last.div_ceil(64);
        while word < end {
            match self.piece(word) {
                Some(piece) => {
                    let stop = ((word / Bits::WORDS + 1) * Bits::WORDS).min(end);
                    while word < stop {
                        let base = word * 64;
                        let m = piece[word % Bits::WORDS]
                            .load(std::sync::atomic::Ordering::Acquire)
                            & in_word(base, first, last);
                        if m != 0 {
                            return Some(base + m.trailing_zeros() as usize);
                        }
                        word += 1;
                    }
                }
                None => word = (word / Bits::WORDS + 1) * Bits::WORDS,
            }
        }
        None
    }

    fn set_range(&self, first: usize, last: usize) {
        if first >= last {
            return;
        }
        let last = last.min(Bits::MAX_BLOCKS);
        if first >= last {
            return;
        }
        let mut word = first / 64;
        let end = last.div_ceil(64);
        while word < end {
            let Some(piece) = self.piece_or_make(word) else {
                return;
            };
            let stop = ((word / Bits::WORDS + 1) * Bits::WORDS).min(end);
            while word < stop {
                let mask = in_word(word * 64, first, last);
                let cell = &piece[word % Bits::WORDS];
                if cell.load(std::sync::atomic::Ordering::Acquire) & mask != mask {
                    cell.fetch_or(mask, std::sync::atomic::Ordering::AcqRel);
                }
                word += 1;
            }
        }
    }

    fn clear(&self, block: usize) {
        let word = block / 64;
        if let Some(piece) = self.piece(word) {
            piece[word % Bits::WORDS]
                .fetch_and(!(1u64 << (block % 64)), std::sync::atomic::Ordering::AcqRel);
        }
    }

    fn clear_range(&self, first: usize, last: usize) {
        if first >= last {
            return;
        }
        let last = last.min(Bits::MAX_BLOCKS);
        if first >= last {
            return;
        }
        let mut word = first / 64;
        let end = last.div_ceil(64);
        while word < end {
            match self.piece(word) {
                Some(piece) => {
                    let mask = in_word(word * 64, first, last);
                    let cell = &piece[word % Bits::WORDS];
                    if cell.load(std::sync::atomic::Ordering::Acquire) & mask != 0 {
                        cell.fetch_and(!mask, std::sync::atomic::Ordering::AcqRel);
                    }
                    word += 1;
                }
                None => {
                    word = (word / Bits::WORDS + 1) * Bits::WORDS;
                }
            }
        }
    }

    fn any(&self, first: usize, last: usize) -> bool {
        if first >= last {
            return false;
        }
        let last = last.min(Bits::MAX_BLOCKS);
        if first >= last {
            return false;
        }
        let mut word = first / 64;
        let end = last.div_ceil(64);
        while word < end {
            match self.piece(word) {
                Some(piece) => {
                    let mask = in_word(word * 64, first, last);
                    if piece[word % Bits::WORDS].load(std::sync::atomic::Ordering::Acquire) & mask
                        != 0
                    {
                        return true;
                    }
                    word += 1;
                }
                None => {
                    word = (word / Bits::WORDS + 1) * Bits::WORDS;
                }
            }
        }
        false
    }

    fn all(&self, first: usize, last: usize) -> bool {
        if first >= last {
            return true;
        }
        if last > Bits::MAX_BLOCKS {
            return false;
        }
        let mut word = first / 64;
        let end = last.div_ceil(64);
        while word < end {
            let Some(piece) = self.piece(word) else {
                return false;
            };
            let mask = in_word(word * 64, first, last);
            if piece[word % Bits::WORDS].load(std::sync::atomic::Ordering::Acquire) & mask != mask {
                return false;
            }
            word += 1;
        }
        true
    }
}

/// Lock-free mirror of [`Guards::protected`] (1 bit per [`BLOCK`]): blocks
/// currently read-only by our hand.
static PROTECTED: Bits = Bits::new();

/// Blocks known to be view memory (1 bit per [`BLOCK`]): guarded by us once or
/// found a view by [`guard_with_epochs`]. The kernel refuses `GetWriteWatch` on
/// views; forgotten on [`title_memory_remapped`] or if a fallback check finds
/// non-view memory at the address.
static VIEWS: Bits = Bits::new();

/// Whether known view memory skips `GetWriteWatch` (`VICTORY_WATCH_SKIP_VIEWS`,
/// on by default when [`light`] is on; `=0` turns it off).
fn skip_views() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_WATCH_SKIP_VIEWS").is_none_or(|v| v != "0"))
}

/// Renderer port #6 (`VICTORY_PORT_GUARD=1`, off by default): the guards'
/// lock is taken shared by write faults, refreshes guarding view memory, and
/// `publish`, while per-block claims ([`BUSY`] / [`Claims`]) serialize state
/// transitions on the same block without blocking unrelated faults or refreshes.
pub(crate) fn port_guard() -> bool {
    #[cfg(test)]
    if PORT_GUARD_IN_TEST.get() {
        return true;
    }
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("VICTORY_PORT_GUARD").is_ok_and(|v| v.trim() == "1"))
}

#[cfg(test)]
thread_local! {
    /// [`port_guard`], for what this thread does.
    pub(crate) static PORT_GUARD_IN_TEST: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// The blocks a thread holding [`GUARDS`] shared is changing the state of
/// (`VICTORY_PORT_GUARD=1`, [`port_guard`]): a bit each, set by whoever holds
/// the block's claim ([`Claims`] / [`ClaimOne`]).
static BUSY: Bits = Bits::new();

/// Single-block claim on [`BUSY`] with zero heap allocation (used on the
/// unaliased write-fault fast path and [`publish`]).
struct ClaimOne(usize);

impl ClaimOne {
    fn wait(block: usize) -> Self {
        let mut spun = 0u64;
        let mut tries = 0u32;
        while !BUSY.try_set(block) {
            tries += 1;
            spun += 1;
            if tries < 64 {
                std::hint::spin_loop();
            } else {
                std::thread::yield_now();
            }
        }
        if spun != 0 {
            port6_count(P6_CLAIM_SPINS, spun);
        }
        Self(block)
    }
}

impl Drop for ClaimOne {
    fn drop(&mut self) {
        BUSY.clear(self.0);
    }
}

/// Claims of blocks ([`BUSY`]), released when dropped.
struct Claims(Vec<usize>);

impl Claims {
    /// Claims of `blocks` (strictly ascending, each once), waited for in order:
    /// whoever waits holds only claims of strictly lower blocks.
    fn wait_for(blocks: &[usize]) -> Claims {
        debug_assert!(blocks.windows(2).all(|w| w[0] < w[1]));
        let mut spun = 0u64;
        for &block in blocks {
            let mut tries = 0u32;
            while !BUSY.try_set(block) {
                tries += 1;
                spun += 1;
                if tries < 64 {
                    std::hint::spin_loop();
                } else {
                    // Another thread's `VirtualProtect` of the block.
                    std::thread::yield_now();
                }
            }
        }
        if spun != 0 {
            port6_count(P6_CLAIM_SPINS, spun);
        }
        Claims(blocks.to_vec())
    }

    /// The claim of `block` if it is free; never waits.
    fn try_one(&mut self, block: usize) -> bool {
        let got = BUSY.try_set(block);
        if got {
            self.0.push(block);
        } else {
            port6_count(P6_CLAIMS_MISSED, 1);
        }
        got
    }

    fn none() -> Claims {
        Claims(Vec::new())
    }

    fn holds(&self, block: usize) -> bool {
        self.0.contains(&block)
    }
}

impl Drop for Claims {
    fn drop(&mut self) {
        for &block in &self.0 {
            BUSY.clear(block);
        }
    }
}

/// Renderer port #6 counts since the last frame line:
/// 0: faults opened under the shared lock;
/// 1: nanoseconds faults waited for the shared lock;
/// 2: refreshes' guards done shared;
/// 3: blocks a refresh could not claim (left written);
/// 4: spins for claims;
/// 5: kernel askings left out for view memory.
static PORT6: [std::sync::atomic::AtomicU64; 6] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 6];
const P6_FAULTS: usize = 0;
const P6_FAULT_WAIT_NS: usize = 1;
const P6_GUARDS: usize = 2;
const P6_CLAIMS_MISSED: usize = 3;
const P6_CLAIM_SPINS: usize = 4;
const P6_NOT_ASKED: usize = 5;

fn port6_count(what: usize, n: u64) {
    PORT6[what].fetch_add(n, std::sync::atomic::Ordering::Relaxed);
}

/// Renderer port #6's line of the frame lines, per frame.
pub(crate) fn port_guard_line(frames: f64) {
    if frames <= 0.0 {
        return;
    }
    let n: Vec<f64> = PORT6
        .iter()
        .map(|c| c.swap(0, std::sync::atomic::Ordering::Relaxed) as f64 / frames)
        .collect();
    if !port_guard() && n.iter().all(|&v| v == 0.0) {
        return;
    }
    tracing::info!(
        "renderer port 6 (VICTORY_PORT_GUARD=1) per frame: faults opened under the shared lock {:.1} ({:.2} ms waiting for it), \
         refreshes' guards done shared {:.1}, blocks left written for want of their claim {:.1}, spins for claims {:.0}; \
         kernel askings left out for view memory {:.1}",
        n[P6_FAULTS],
        n[P6_FAULT_WAIT_NS] / 1e6,
        n[P6_GUARDS],
        n[P6_CLAIMS_MISSED],
        n[P6_CLAIM_SPINS],
        n[P6_NOT_ASKED],
    );
}

/// Whether the kernel would refuse to ask about `[start, end)` for its view
/// memory: read-only by our hand ([`PROTECTED`]), or known a view ([`VIEWS`]).
/// Such a range is left to [`writes`], which goes to the guards at once; asked
/// with others in a batch it failed their asking too, forcing bisection retries.
fn refused_as_view(start: usize, end: usize) -> bool {
    if !light() || !skip_views() || start >= end {
        return false;
    }
    let (first, last) = (start / BLOCK, end.div_ceil(BLOCK));
    let refused = PROTECTED.any(first, last) || VIEWS.any(first, last);
    if refused && port_guard() {
        port6_count(P6_NOT_ASKED, 1);
    }
    refused
}

/// Monotonic epoch stamped into `CLEAN` whenever a block is guarded or
/// marked clean, so `publish` can never publish a newer guard epoch whose
/// `written_set` stamps have not yet landed in `PARTS`.
static GUARD_EPOCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn published_set(block: usize, epoch: u64) {
    if let Some(part) = part_of(&PUBLISHED, block, epoch != 0) {
        part.0[block % PART].store(epoch, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Clears lock-free [`PUBLISHED`] entries for `[from, to)` with `SeqCst`,
/// chunked by [`PART`].
fn published_clear_range(from: usize, to: usize) {
    let mut block = from;
    while block < to {
        let stop = ((block / PART + 1) * PART).min(to);
        if let Some(part) = part_of(&PUBLISHED, block, false) {
            for b in block..stop {
                part.0[b % PART].store(0, std::sync::atomic::Ordering::SeqCst);
            }
        }
        block = stop;
    }
}

fn clean_epoch(block: usize) -> u64 {
    part_of(&CLEAN, block, false).map_or(0, |part| {
        part.0[block % PART].load(std::sync::atomic::Ordering::Acquire)
    })
}

fn published_all(first_block: usize, last_block: usize) -> bool {
    let mut b = first_block;
    while b < last_block {
        let Some(part) = part_of(&PUBLISHED, b, false) else {
            return false;
        };
        let end = last_block.min((b / PART + 1) * PART);
        while b < end {
            if part.0[b % PART].load(std::sync::atomic::Ordering::SeqCst) == 0 {
                return false;
            }
            b += 1;
        }
    }
    true
}

fn publish(epochs: &[(usize, usize, u64)]) {
    if epochs.is_empty() || !light() {
        return;
    }
    if port_guard() {
        // Hold each block's claim while setting `PUBLISHED`: a concurrent fault
        // opening it clears `PUBLISHED` after this sets it, never before.
        let g = guards_read();
        for &(from, to, epoch) in epochs {
            if epoch == 0 || from >= to {
                continue;
            }
            for block in from..to {
                let _claim = ClaimOne::wait(block);
                if g.clean.contains(block) && clean_epoch(block) == epoch {
                    published_set(block, epoch);
                }
            }
        }
        return;
    }
    let g = guards_read();
    for &(from, to, epoch) in epochs {
        if epoch == 0 || from >= to {
            continue;
        }
        if g.clean.covers(from, to) {
            for block in from..to {
                if clean_epoch(block) == epoch {
                    published_set(block, epoch);
                }
            }
        } else {
            for block in from..to {
                if g.clean.contains(block) && clean_epoch(block) == epoch {
                    published_set(block, epoch);
                }
            }
        }
    }
}

/// Lock-free atomic version of [`nothing_new`] for [`crate::copied_pages::CopiedPages`].
/// Checks `published_all` first; skips `JOURNAL.read()` on known view memory (`VIEWS.all`);
/// and when `all_copied` is true skips scanning `copied` for zeros and only loads
/// `copied[i]` for pages with a nonzero write stamp in `PARTS`.
pub(crate) fn nothing_new_atomic(
    start: usize,
    end: usize,
    base: usize,
    copied: &[std::sync::atomic::AtomicU64],
    all_copied: bool,
) -> bool {
    use std::sync::atomic::Ordering::Acquire;
    if !light() || start >= end {
        return false;
    }
    let (first, last) = (start / PAGE, end.div_ceil(PAGE));
    let Some(off) = first.checked_sub(base) else {
        return false;
    };
    let len = last - first;
    let Some(taken) = copied.get(off..off + len) else {
        return false;
    };
    if !all_copied && taken.iter().any(|a| a.load(Acquire) == 0) {
        return false;
    }
    let (fb, lb) = (start / BLOCK, end.div_ceil(BLOCK));
    if !published_all(fb, lb) {
        if skip_views() && VIEWS.all(fb, lb) {
            return false;
        }
        let ahead = {
            let j = JOURNAL.read().unwrap_or_else(|e| e.into_inner());
            reservation::covered(&j, first, last)
        };
        if !ahead {
            return false;
        }
    }
    let mut page = first;
    while page < last {
        let end_p = last.min((page / PART + 1) * PART);
        if let Some(part) = part_of(&PARTS, page, false) {
            for p in page..end_p {
                let stamp = part.0[p % PART].load(Acquire);
                if stamp != 0 && stamp > taken[p - first].load(Acquire) {
                    return false;
                }
            }
        }
        page = end_p;
    }
    true
}

/// (Neptune) `VICTORY_GUARD_HOT=<frames>` (0 or unset: off): a block of view memory that was
/// found written at three checks running, frame after frame, is left without its guard for
/// that many frames, and counts as written at every check meanwhile.
///
/// A block the title writes every frame cost a fault in the title's thread, the guard's
/// opening there, and the guard's setting again at the next refresh, each a change of
/// protection behind the guards' one lock: the refreshes of a submission waited for one
/// another there (a tenth of the submitting thread's time in a sample). Left open, such a
/// block is read and compared at each refresh of it, on the refreshes' own threads, and
/// nothing waits. Not a block shown in other views too: a write through this one could not
/// open theirs.
fn guard_hot() -> u64 {
    static FRAMES: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *FRAMES.get_or_init(|| {
        std::env::var("VICTORY_GUARD_HOT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    })
}

/// Of a block, for [`guard_hot`]: the frame it was last found written in (40 bits), how
/// many checks running found it so (16 bits), and whether it is left open since that frame.
static HOT: [std::sync::atomic::AtomicPtr<Part>; 1 << 16] =
    [const { std::sync::atomic::AtomicPtr::new(std::ptr::null_mut()) }; 1 << 16];
const HOT_FRAME: u64 = (1 << 40) - 1;
const HOT_OPEN: u64 = 1 << 63;

fn hot_get(block: usize) -> u64 {
    part_of(&HOT, block, false).map_or(0, |part| {
        part.0[block % PART].load(std::sync::atomic::Ordering::Acquire)
    })
}

fn hot_set(block: usize, value: u64) {
    if let Some(part) = part_of(&HOT, block, value != 0) {
        part.0[block % PART].store(value, std::sync::atomic::Ordering::Release);
    }
}

/// Whether `block` is left open now ([`guard_hot`]).
fn left_open(block: usize, frame: u64, keep: u64) -> bool {
    let v = hot_get(block);
    v & HOT_OPEN != 0 && frame.wrapping_sub(v & HOT_FRAME) <= keep
}

/// A check found `block` written and without a guard: whether it is left so ([`guard_hot`]).
/// Under the guards' lock.
fn stays_open(g: &Guards, block: usize, frame: u64, keep: u64) -> bool {
    if g.aliased.contains(block) {
        return false;
    }
    let v = hot_get(block);
    let (was, streak) = (v & HOT_FRAME, (v >> 40) & 0xffff);
    if v & HOT_OPEN != 0 {
        if frame.wrapping_sub(was) <= keep {
            return true;
        }
        // Its time is over: guarded again, and counted from nothing.
        hot_set(block, frame & HOT_FRAME);
        return false;
    }
    let streak = match frame.wrapping_sub(was) {
        0 if v != 0 => streak,
        1 | 2 if v != 0 => streak + 1,
        _ => 1,
    };
    if streak >= 3 && crate::inplace::pages_of(block * BLOCK).len() < 2 {
        hot_set(block, HOT_OPEN | (frame & HOT_FRAME));
        HOT_LEFT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return true;
    }
    hot_set(block, (streak.min(0xffff) << 40) | (frame & HOT_FRAME));
    false
}

/// Blocks left open since the start ([`guard_hot`]).
static HOT_LEFT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// (Neptune) Since the last line in the log: write faults that opened a guard, changes of
/// protection that set guards, the blocks they set, checks that took the guards' lock to
/// change something, checks answered without it; kernel askings of [`ask_once`], the 4 KiB
/// pages they walked, the written pages they found.
static COUNTS: [std::sync::atomic::AtomicU64; 8] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 8];

fn count(which: usize, n: u64) {
    COUNTS[which].fetch_add(n, std::sync::atomic::Ordering::Relaxed);
}

/// (Neptune) The absent blocks of the guards again (`Guards::absent`), as [`CLEAN`] is of the
/// clean ones: a range with blocks that are not committed never was "all clean", so every
/// check of it took the guards' lock to find nothing to do (a thousand checks a frame under
/// the lock for three hundred that set a guard).
/// Encodes 0 = not absent, 1 = ordinary reservation/free (must probe `VirtualQuery`),
/// 2 = confirmed `MEM_RESERVE_PLACEHOLDER` (skipped without `VirtualQuery` until remap).
static ABSENT: [std::sync::atomic::AtomicPtr<Part>; 1 << 16] =
    [const { std::sync::atomic::AtomicPtr::new(std::ptr::null_mut()) }; 1 << 16];

const ABSENT_NONE: u64 = 0;
const ABSENT_ORDINARY: u64 = 1;
const ABSENT_PLACEHOLDER: u64 = 2;

fn parse_bool_flag(raw: Option<&std::ffi::OsStr>, default: bool) -> bool {
    match raw.and_then(|v| v.to_str()) {
        Some("1") => true,
        Some("0") => false,
        _ => default,
    }
}

/// Whether confirmed placeholder holes skip per-asking `VirtualQuery` probes
/// until `title_memory_remapped` (`VICTORY_PLACEHOLDER_SKIP=1`, off by default).
fn placeholder_skip_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        parse_bool_flag(
            std::env::var_os("VICTORY_PLACEHOLDER_SKIP").as_deref(),
            false,
        )
    })
}

/// Whether contiguous guard/unguard runs coalesce into batched `VirtualProtect`
/// syscalls (`VICTORY_WATCH_COALESCE=0|1`, default `1`).
fn watch_coalesce() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| parse_bool_flag(std::env::var_os("VICTORY_WATCH_COALESCE").as_deref(), true))
}

/// Whether alias tracking uses batched section lookups (`aliases_in_range` /
/// `aliases_for_blocks`) instead of per-block `pages_of` (`VICTORY_WATCH_ALIAS_BATCH=0|1`, default `1`).
fn watch_alias_batch() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        parse_bool_flag(
            std::env::var_os("VICTORY_WATCH_ALIAS_BATCH").as_deref(),
            true,
        )
    })
}

/// Whether `guard` uses the fast `INSTALLED` + known-view single re-check path
/// instead of calling `install()` unconditionally (`VICTORY_WATCH_SINGLE_LOCK=0|1`, default `1`).
fn watch_single_lock() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        parse_bool_flag(
            std::env::var_os("VICTORY_WATCH_SINGLE_LOCK").as_deref(),
            true,
        )
    })
}

/// Notes `[from, to)` absent kind in lock-free [`ABSENT`], chunked by [`PART`].
fn absent_set_range(from: usize, to: usize, kind: u64) {
    let make = kind != ABSENT_NONE;
    let mut block = from;
    while block < to {
        let stop = ((block / PART + 1) * PART).min(to);
        if let Some(part) = part_of(&ABSENT, block, make) {
            for b in block..stop {
                part.0[b % PART].store(kind, std::sync::atomic::Ordering::Release);
            }
        }
        block = stop;
    }
}

fn absent_kind(block: usize) -> u64 {
    part_of(&ABSENT, block, false).map_or(ABSENT_NONE, |part| {
        part.0[block % PART].load(std::sync::atomic::Ordering::Acquire)
    })
}

/// Notes `block` clean or not, with `Guards::clean`.
fn clean_set(block: usize, clean: bool) -> u64 {
    let val = if clean {
        GUARD_EPOCH.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    } else {
        0
    };
    if let Some(part) = part_of(&CLEAN, block, clean) {
        part.0[block % PART].store(val, std::sync::atomic::Ordering::Release);
    }
    val
}

/// Notes `[from, to)` clean or not in lock-free [`CLEAN`], chunked by [`PART`].
/// Returns the monotonic [`GUARD_EPOCH`] stamped when `clean` is true (0 otherwise).
fn clean_set_range(from: usize, to: usize, clean: bool) -> u64 {
    if from >= to {
        return 0;
    }
    let val = if clean {
        GUARD_EPOCH.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    } else {
        0
    };
    let mut block = from;
    while block < to {
        let stop = ((block / PART + 1) * PART).min(to);
        if let Some(part) = part_of(&CLEAN, block, clean) {
            for b in block..stop {
                part.0[b % PART].store(val, std::sync::atomic::Ordering::Release);
            }
        }
        block = stop;
    }
    val
}

/// Whether the blocks `[first, last)` are all clean.
fn clean_all(first: usize, last: usize) -> bool {
    let mut block = first;
    while block < last {
        let stop = ((block / PART + 1) * PART).min(last);
        let Some(part) = part_of(&CLEAN, block, false) else {
            return false;
        };
        if (block..stop).any(|b| part.0[b % PART].load(std::sync::atomic::Ordering::Acquire) == 0) {
            return false;
        }
        block = stop;
    }
    true
}

fn part_of(
    table: &'static [std::sync::atomic::AtomicPtr<Part>; 1 << 16],
    page: usize,
    make: bool,
) -> Option<&'static Part> {
    use std::sync::atomic::Ordering::{AcqRel, Acquire};
    let slot = table.get(page / PART)?;
    let mut at = slot.load(Acquire);
    if at.is_null() {
        if !make {
            return None;
        }
        let layout = std::alloc::Layout::new::<Part>();
        // SAFETY: a layout of non-zero size; zeroed memory is a valid `Part`.
        let fresh = unsafe { std::alloc::alloc_zeroed(layout) }.cast::<Part>();
        if fresh.is_null() {
            return None;
        }
        match slot.compare_exchange(std::ptr::null_mut(), fresh, AcqRel, Acquire) {
            Ok(_) => at = fresh,
            Err(other) => {
                // SAFETY: ours alone, allocated with this layout just above.
                unsafe { std::alloc::dealloc(fresh.cast(), layout) };
                at = other;
            }
        }
    }
    // SAFETY: a part once made is never freed.
    Some(unsafe { &*at })
}

/// (Neptune) Whether written-ness is also kept per 4 KiB (`VICTORY_WATCH_4K=1`; off by
/// default): where the kernel says which 4 KiB pages were written, [`stale_mask`] tells a
/// refresh which sub-pages of a stale 64 KiB page it need copy.
pub(crate) fn watch_4k() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_WATCH_4K").is_some_and(|v| v == "1"))
}

/// Per 64 KiB page: the asking that last noted it written without saying which 4 KiB pages
/// (guards, our own writes, kernel writes we were told of): such a page counts written whole.
static WHOLE: [std::sync::atomic::AtomicPtr<Part>; 1 << 16] =
    [const { std::sync::atomic::AtomicPtr::new(std::ptr::null_mut()) }; 1 << 16];

/// Per 4 KiB page (`address / 4096`): the asking that last found it written by the kernel.
static SUBS: [std::sync::atomic::AtomicPtr<Part>; 1 << 16] =
    [const { std::sync::atomic::AtomicPtr::new(std::ptr::null_mut()) }; 1 << 16];

/// With `VICTORY_DIRTY_4K` ([`dirty4k`]): page number → the asking of `PARTS`
/// for it, the asking before that which had found it written (0: never), and
/// the 4 KiB kernel pages of it that asking found written (bit `k`: bytes
/// `k * 4096..(k + 1) * 4096` of the page). Only where the kernel's write watch
/// answered: a page noted written any other way (the guards, our own writes)
/// has no entry, and counts as written whole. Stored in its own `RwLock` so
/// readers never contend with [`JOURNAL`].
static SUB_PAGES: std::sync::RwLock<BTreeMap<usize, (u64, u64, u16)>> =
    std::sync::RwLock::new(BTreeMap::new());

/// Whether refreshes of write-watched memory copy only the 4 KiB pages written
/// (`VICTORY_DIRTY_4K=1`; off by default; `alt`: twenty seconds with, twenty
/// without, in turn, each change said in the log).
pub(crate) fn dirty4k() -> bool {
    match dirty4k_mode() {
        0 => false,
        1 => true,
        _ => {
            static EPOCH: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
            let on = (EPOCH
                .get_or_init(std::time::Instant::now)
                .elapsed()
                .as_secs()
                / 20)
                % 2
                == 1;
            static LAST: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(2);
            if LAST.swap(u8::from(on), std::sync::atomic::Ordering::Relaxed) != u8::from(on) {
                tracing::info!(
                    "refreshes of write-watched memory copy only the 4 KiB pages written from now (VICTORY_DIRTY_4K=alt): {}",
                    if on {
                        "ON"
                    } else {
                        "off, whole 64 KiB pages as usual"
                    }
                );
            }
            on
        }
    }
}

#[cfg(test)]
thread_local! {
    /// Overrides `VICTORY_DIRTY_4K` mode for this test thread (`u8::MAX`: use env).
    pub(crate) static DIRTY4K_IN_TEST: std::cell::Cell<u8> = const { std::cell::Cell::new(u8::MAX) };
}

/// `VICTORY_DIRTY_4K`: 0 off, 1 on, 2 in turn (`alt`).
fn dirty4k_mode() -> u8 {
    #[cfg(test)]
    {
        let m = DIRTY4K_IN_TEST.get();
        if m != u8::MAX {
            return m;
        }
    }
    static MODE: std::sync::OnceLock<u8> = std::sync::OnceLock::new();
    *MODE.get_or_init(|| {
        match std::env::var_os("VICTORY_DIRTY_4K")
            .as_deref()
            .and_then(|v| v.to_str())
            .map(str::trim)
        {
            Some("1") => 1,
            Some("alt") => 2,
            _ => 0,
        }
    })
}

/// Whether the journal keeps the 4 KiB pages found ([`SUB_PAGES`]): with the
/// switch on, and in both phases of `alt` (the phase without counts what it
/// would have saved).
pub(crate) fn dirty4k_kept() -> bool {
    !watch_4k() && dirty4k_mode() != 0
}

/// With [`dirty4k_kept`]: the entries of [`SUB_PAGES`] for the pages of
/// `[start, end)`, in page order, as `(page, asking, the asking before, 4 KiB
/// pages found)`. Empty without the switch. Read once the refresh's asking is
/// done; an entry whose asking is not the one the refresh was given for its
/// page (a later asking came between) is not used.
pub(crate) fn subpages(start: usize, end: usize) -> Vec<(usize, u64, u64, u16)> {
    if !dirty4k_kept() || start >= end {
        return Vec::new();
    }
    let (first, last) = (start / PAGE, end.div_ceil(PAGE));
    let sub = SUB_PAGES.read().unwrap_or_else(|e| e.into_inner());
    sub.range(first..last)
        .map(|(&p, &(asked, before, bits))| (p, asked, before, bits))
        .collect()
}

/// Since the start, with `VICTORY_DIRTY_4K`: stale pages a refresh could narrow
/// to their 4 KiB pages, the 4 KiB pages of them, those narrowed in fact (the
/// switch on), and stale pages copied whole (no entry, or taken too long ago).
pub(crate) static DIRTY4K: [std::sync::atomic::AtomicU64; 4] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 4];

/// Counts a stale page of a refresh ([`DIRTY4K`]): `found`, its 4 KiB pages if
/// it could be narrowed; `narrowed`, whether it was. Says the counts at each
/// power of two of the pages that could be.
pub(crate) fn dirty4k_count(found: Option<u16>, narrowed: bool) {
    use std::sync::atomic::Ordering::Relaxed;
    let Some(bits) = found else {
        DIRTY4K[3].fetch_add(1, Relaxed);
        return;
    };
    DIRTY4K[1].fetch_add(u64::from(bits.count_ones()), Relaxed);
    DIRTY4K[2].fetch_add(u64::from(narrowed), Relaxed);
    let n = DIRTY4K[0].fetch_add(1, Relaxed) + 1;
    if n.is_power_of_two() {
        let pieces = DIRTY4K[1].load(Relaxed);
        tracing::info!(
            "stale 64 KiB pages of write-watched memory (VICTORY_DIRTY_4K), since the start: {n} could be copied as their {pieces} written 4 KiB pages ({:.1} MB instead of {:.1} MB), {} were; {} copied whole (written another way, or taken too long ago)",
            pieces as f64 * 4096.0 / 1e6,
            n as f64 * 65536.0 / 1e6,
            DIRTY4K[2].load(Relaxed),
            DIRTY4K[3].load(Relaxed)
        );
    }
}

fn clear_subpages_range(first: usize, last: usize) {
    if !dirty4k_kept() || first >= last {
        return;
    }
    let mut sub = SUB_PAGES.write().unwrap_or_else(|e| e.into_inner());
    let keys: Vec<usize> = sub.range(first..last).map(|(&k, _)| k).collect();
    for k in keys {
        sub.remove(&k);
    }
}

/// Notes `page` written at asking `now`; what it held before (0: nothing). Says nothing of
/// which 4 KiB pages, so with [`watch_4k`] or [`dirty4k_kept`] the whole page counts.
fn written_set(page: usize, now: u64) -> u64 {
    if watch_4k() {
        // Before the page's stamp, which a reader looks at first.
        match part_of(&WHOLE, page, true) {
            Some(part) => {
                part.0[page % PART].fetch_max(now, std::sync::atomic::Ordering::AcqRel);
            }
            None => {}
        }
    }
    if dirty4k_kept() {
        let mut sub = SUB_PAGES.write().unwrap_or_else(|e| e.into_inner());
        let before = stamp_set(page, now);
        sub.remove(&page);
        return before;
    }
    stamp_set(page, now)
}

/// The page's stamp alone: what it held before.
fn stamp_set(page: usize, now: u64) -> u64 {
    match part(page, true) {
        Some(part) => part.0[page % PART].fetch_max(now, std::sync::atomic::Ordering::AcqRel),
        None => 0,
    }
}

/// Notes a batch of 4 KiB pages found written by the kernel at asking `now`.
/// Updates `PARTS` (plus `SUBS` under `watch_4k()`, plus `SUB_PAGES` under
/// `dirty4k_kept()`).
fn note_kernel_batch(addrs: &[usize], now: u64) {
    if addrs.is_empty() {
        return;
    }
    if !dirty4k_kept() {
        for &at in addrs {
            found_set(at, now);
        }
        return;
    }
    let mut sub = SUB_PAGES.write().unwrap_or_else(|e| e.into_inner());
    for &at in addrs {
        note_kernel(&mut sub, at, now);
    }
}

/// Notes in [`PARTS`] and [`SUB_PAGES`] that the kernel's asking `now` found
/// the 4 KiB page at `at` written.
fn note_kernel(sub: &mut BTreeMap<usize, (u64, u64, u16)>, at: usize, now: u64) {
    let page = at / PAGE;
    let bit = 1u16 << ((at % PAGE) / KERNEL_PAGE);
    if watch_4k() {
        let sub_idx = at / KERNEL_PAGE;
        if let Some(part) = part_of(&SUBS, sub_idx, true) {
            part.0[sub_idx % PART].fetch_max(now, std::sync::atomic::Ordering::AcqRel);
        } else if let Some(part) = part_of(&WHOLE, page, true) {
            part.0[page % PART].fetch_max(now, std::sync::atomic::Ordering::AcqRel);
        }
    }
    let before = stamp_set(page, now);
    if before < now {
        sub.insert(page, (now, before, bit));
    } else if before == now {
        // Found again in the same asking: one more 4 KiB page of it. A page
        // this asking already noted whole (our own writes under a standing
        // asking) stays whole: it has no entry.
        if let Some(entry) = sub.get_mut(&page)
            && entry.0 == now
        {
            entry.2 |= bit;
        }
    } else {
        // Noted with a later asking concurrently: advance the stamp past it
        // and fall back to whole-page copy.
        let later = next_asking();
        if watch_4k() {
            let sub_idx = at / KERNEL_PAGE;
            if let Some(part) = part_of(&SUBS, sub_idx, true) {
                part.0[sub_idx % PART].fetch_max(later, std::sync::atomic::Ordering::AcqRel);
            }
        }
        stamp_set(page, later);
        sub.remove(&page);
    }
}

/// Notes the 4 KiB page at `address` found written by the kernel at asking `now` (as
/// [`written_set`], but only that sub-page). Where the sub-page table cannot be made the
/// whole page counts.
fn found_set(address: usize, now: u64) {
    let (page, sub) = (address / PAGE, address / KERNEL_PAGE);
    if !watch_4k() {
        written_set(page, now);
        return;
    }
    let Some(part) = part_of(&SUBS, sub, true) else {
        written_set(page, now);
        return;
    };
    let cell = &part.0[sub % PART];
    cell.fetch_max(now, std::sync::atomic::Ordering::AcqRel);
    // Noted with a later asking meanwhile (the copy-back's own write): what the kernel found
    // is newer than that, and gets a number that says so.
    if stamp_set(page, now) > now {
        let later = next_asking();
        cell.fetch_max(later, std::sync::atomic::Ordering::AcqRel);
        stamp_set(page, later);
    }
}

/// With [`watch_4k`]: which 4 KiB pages (bit k: the k-th) of `page` were written after
/// asking `at`, and `None` where that is not known, and the whole page is to be taken: never
/// copied (`at == 0`), written without a 4 KiB answer, or nothing found.
pub(crate) fn stale_mask(page: usize, at: u64) -> Option<u16> {
    use std::sync::atomic::Ordering::Acquire;
    if !watch_4k() || at == 0 {
        return None;
    }
    if part_of(&WHOLE, page, false).is_some_and(|p| p.0[page % PART].load(Acquire) > at) {
        return None;
    }
    let sub = page * (PAGE / KERNEL_PAGE);
    let part = part_of(&SUBS, sub, false)?;
    let stamps: [u64; PAGE / KERNEL_PAGE] =
        std::array::from_fn(|k| part.0[(sub + k) % PART].load(Acquire));
    mask_of(&stamps, at)
}

/// The sub-pages stamped after `at`; `None` if there are none (the page's stamp then rests on
/// something this cannot see: copy it whole).
fn mask_of(stamps: &[u64; PAGE / KERNEL_PAGE], at: u64) -> Option<u16> {
    let mask = stamps
        .iter()
        .enumerate()
        .fold(0u16, |m, (k, &s)| m | (u16::from(s > at) << k));
    (mask != 0).then_some(mask)
}

/// Bytes refreshed through 4 KiB masks and the pages so, then bytes and pages that fell back
/// to the whole 64 KiB.
static COPY_4K: [std::sync::atomic::AtomicU64; 4] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 4];

/// Notes stale pages refreshed, batched per `copy_stale` call to avoid per-page
/// atomic contention across worker threads.
pub(crate) fn note_4k_batch(sub_4k_count: u64, sub_64k_pages: u64, whole_64k_pages: u64) {
    use std::sync::atomic::Ordering::Relaxed;
    if sub_64k_pages > 0 {
        COPY_4K[0].fetch_add(sub_4k_count * KERNEL_PAGE as u64, Relaxed);
        COPY_4K[1].fetch_add(sub_64k_pages, Relaxed);
    }
    if whole_64k_pages > 0 {
        COPY_4K[2].fetch_add(whole_64k_pages * PAGE as u64, Relaxed);
        COPY_4K[3].fetch_add(whole_64k_pages, Relaxed);
    }
}

/// [`COPY_4K`], taken.
pub(crate) fn copy_4k_stats() -> [u64; 4] {
    [0, 1, 2, 3].map(|i| COPY_4K[i].swap(0, std::sync::atomic::Ordering::Relaxed))
}

/// The pages of `[first, last)` ever noted written, in order, each with the asking that
/// last found it so.
fn written_in(first: usize, last: usize) -> Vec<(usize, u64)> {
    let mut out = Vec::new();
    let mut page = first;
    while page < last {
        let stop = ((page / PART + 1) * PART).min(last);
        if let Some(part) = part(page, false) {
            for p in page..stop {
                let w = part.0[p % PART].load(std::sync::atomic::Ordering::Acquire);
                if w != 0 {
                    out.push((p, w));
                }
            }
        }
        page = stop;
    }
    out
}

/// Ranges of one submission closer than this are asked about together:
/// walking the pages between them costs less than another kernel call (a
/// call that resets written pages costs 30-70 microseconds whatever the
/// range).
const AHEAD_GAP: usize = (8 << 20) / PAGE;
/// The most one asking covers (its answer needs 8 bytes per 4 KiB page).
const AHEAD_MAX: usize = (1 << 30) / PAGE;

/// (Neptune) `VICTORY_ASK_WIDE_MB=<n>`: a submission's ranges are asked about together when
/// they lie within `n` MiB of one another, and one asking covers up to `n` MiB (at most
/// 16384), in place of 8 MiB and 1 GiB. A frame made about 3,000 askings, each resetting
/// pages and with that flushing every core's address cache (36 ms a frame); a reservation
/// asked about whole is one such call per submission. Unset or 0: as before.
fn ahead_limits() -> (usize, usize) {
    static WIDE: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    let mb = *WIDE.get_or_init(|| {
        std::env::var("VICTORY_ASK_WIDE_MB")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(0)
            .min(16384)
    });
    if mb == 0 {
        (AHEAD_GAP, AHEAD_MAX)
    } else {
        ((mb << 20) / PAGE, ((mb.max(1024)) << 20) / PAGE)
    }
}

/// Whether a submission-wide [`ask_once`] journal is active (so [`ask_ahead`] is a no-op).
pub(crate) fn ask_once_active() -> bool {
    KEPT.load(std::sync::atomic::Ordering::Acquire)
}

/// Asks the kernel about many ranges `(start, end)` at once, before the
/// [`writes`] of each: ranges close together are one asking, and `writes`
/// then answers from the journal until [`ask_ahead_done`]. The world's
/// submissions refresh thousands of host copies (6,664 askings a frame,
/// 456 ms, in run 14). Ranges that are not write-watched, or lie alone, are
/// left to `writes`.
fn prepare_flush_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| !std::env::var_os("VICTORY_PREPARE_FLUSH").is_some_and(|v| v == "0"))
}

/// Coalesces view-memory re-guarding (`VirtualProtect`) and any uncovered
/// private `GetWriteWatch` ranges across all copies in a `Batch::flush` ONCE
/// on the submitting thread before `wide_chunks` runs.
///
/// This prevents 16 worker threads from issuing hundreds of overlapping 1 MiB
/// `VirtualProtect` / `GetWriteWatch` calls concurrently (which contend on the
/// Windows kernel VAD lock and fire TLB shootdown IPIs into parallel
/// `diff_copy_direct` workers).
pub(crate) fn prepare_flush(ranges: &[(usize, usize)]) {
    if !prepare_flush_enabled() || !light() || ranges.is_empty() {
        return;
    }
    let mut pages: Vec<(usize, usize)> = ranges
        .iter()
        .filter(|&&(a, b)| a < b)
        .map(|&(a, b)| (a / PAGE, b.div_ceil(PAGE)))
        .collect();
    if pages.is_empty() {
        return;
    }
    pages.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(pages.len());
    for (a, b) in pages {
        match merged.last_mut() {
            Some((_, last)) if a <= *last => *last = (*last).max(b),
            _ => merged.push((a, b)),
        }
    }
    const { assert!(BLOCK == PAGE) };
    let mut views: Vec<(usize, usize)> = Vec::new();
    let mut priv_cand: Vec<(usize, usize)> = Vec::new();
    let skip_v = skip_views();
    for (first, last) in merged {
        if published_all(first, last) {
            continue;
        }
        let is_v = (skip_v && (PROTECTED.any(first, last) || VIEWS.any(first, last)))
            || known_view(first * PAGE);
        if is_v {
            views.push((first, last));
        } else {
            priv_cand.push((first, last));
        }
    }
    if !views.is_empty() {
        let t = crate::refresh_prof::start();
        let mut called = 0u64;
        for (first, last) in views {
            if published_all(first, last) {
                continue;
            }
            if let Some((guarded_pages, epochs)) = guard_with_epochs(first * PAGE, last * PAGE) {
                called += 1;
                if !guarded_pages.is_empty() || !epochs.is_empty() {
                    let now = next_asking();
                    for &page in &guarded_pages {
                        written_set(page, now);
                    }
                    publish(&epochs);
                }
            }
        }
        crate::refresh_prof::add(crate::refresh_prof::P::PrepareViewsCalls, called);
        crate::refresh_prof::finish(crate::refresh_prof::P::PrepareViewsUs, t);
    }
    if !priv_cand.is_empty() && reservation::enabled() && ask_once_active() {
        let uncovered: Vec<(usize, usize)> = {
            let j = JOURNAL.read().unwrap_or_else(|e| e.into_inner());
            if !j.kept {
                Vec::new()
            } else {
                priv_cand
                    .into_iter()
                    .filter(|&(a, b)| !reservation::covered(&j, a, b))
                    .collect()
            }
        };
        if !uncovered.is_empty() {
            let t = crate::refresh_prof::start();
            let n = uncovered.len() as u64;
            reservation::ask_more(&uncovered);
            crate::refresh_prof::add(crate::refresh_prof::P::PreparePrivCalls, n);
            crate::refresh_prof::finish(crate::refresh_prof::P::PreparePrivUs, t);
        }
    }
}

pub(crate) fn ask_ahead(ranges: &[(usize, usize)]) {
    // The submission's one asking stands ([`ask_once`]): what it does not
    // hold is left to `writes`. Check before allocating/scanning/sorting.
    if ask_once_active() {
        return;
    }
    let mut pieces: Vec<(usize, usize)> = ranges
        .iter()
        .filter(|r| r.0 < r.1 && !refused_as_view(r.0, r.1))
        .map(|&(start, end)| (start / PAGE, end.div_ceil(PAGE)))
        .collect();
    if pieces.len() < 2 {
        return;
    }
    pieces.sort_unstable();
    let mut j = JOURNAL.write().unwrap_or_else(|e| e.into_inner());
    let j = &mut *j;
    // The submission's one asking stands ([`ask_once`]): what it does not
    // hold is left to `writes`.
    if j.kept {
        return;
    }
    j.ahead_asked = next_asking();
    j.ahead.clear();
    j.ordered = false;
    let (gap, most) = ahead_limits();
    let mut i = 0;
    while i < pieces.len() {
        let first = pieces[i].0;
        let mut last = pieces[i].1;
        let mut k = i + 1;
        while k < pieces.len() && pieces[k].0 <= last + gap && last.max(pieces[k].1) - first <= most
        {
            last = last.max(pieces[k].1);
            k += 1;
        }
        ask_run(j, &pieces[i..k]);
        i = k;
    }
}

/// Since the last frame line: the kernel askings that took a millisecond
/// or more, their microseconds, and the longest asking's. An asking walks
/// page tables, some ten microseconds for most ranges; in run 75 the 2,644
/// askings of a frame took 219 ms in the lines where the pass over the
/// heap's views ran long, 25-50 ms in the others. These numbers say
/// whether a few askings wait for something or all of them are slow.
#[cfg(feature = "diagnostics")]
static SLOW: [std::sync::atomic::AtomicU64; 3] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 3];

#[cfg(feature = "diagnostics")]
fn slow(us: u64) {
    use std::sync::atomic::Ordering::Relaxed;
    if us >= 1000 {
        SLOW[0].fetch_add(1, Relaxed);
        SLOW[1].fetch_add(us, Relaxed);
    }
    SLOW[2].fetch_max(us, Relaxed);
}

/// [`SLOW`], taken.
#[cfg(feature = "diagnostics")]
pub(crate) fn slow_asks() -> [u64; 3] {
    [0, 1, 2].map(|i| SLOW[i].swap(0, std::sync::atomic::Ordering::Relaxed))
}

/// The submission's refreshes are done: `writes` asks the kernel again.
pub(crate) fn ask_ahead_done() {
    if ask_once_active() {
        return;
    }
    let mut j = JOURNAL.write().unwrap_or_else(|e| e.into_inner());
    if !j.kept {
        j.ahead.clear();
        j.ordered = false;
    }
}

/// Whether a submission asks the kernel once, before its first refresh,
/// about every range its lists refresh (`VICTORY_ASK_ONCE=1`; off by
/// default).
///
/// A submission's refreshes are done in groups, between the title's own
/// buffer copies (each copy flushes the refreshes recorded before it), and
/// every group asked the kernel again: 2,644 askings a frame, 70 a
/// submission. With the switch the journal answers all of them from one
/// asking made when the submission starts.
///
/// What changes: memory the title's other threads write while the
/// submission is being prepared is seen by the next submission's asking,
/// not by this one's later groups. What this submission's lists read was
/// written before they were submitted, so before the asking. Our own
/// writes into title memory while preparing are told to the journal
/// ([`wrote`]) or end the asking ([`ask_once_end`]).
pub(crate) fn once() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    // On by default since run 81 (with card buffers: 375-450 ms a frame
    // against 573-647, the picture unchanged over four minutes);
    // `VICTORY_ASK_ONCE=0` asks per range, as before.
    *ON.get_or_init(|| std::env::var_os("VICTORY_ASK_ONCE").is_none_or(|v| v != "0"))
}

/// Whether [`ask_once`] spreads its write-watch kernel askings across the
/// kept worker threads (`VICTORY_ASK_SPREAD`, on by default; `=0` runs them
/// sequentially on the submitting thread).
fn spread() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_ASK_SPREAD").is_none_or(|v| v != "0"))
}

/// Runs `each(i)` for every `i` below `n`, on the threads kept for refreshes
/// (`crate::shadow::wide_chunks`) if [`spread`] is on and `n >= 2`; returns
/// what each produces, in order.
fn spread_each<T: Send + 'static>(
    n: usize,
    each: impl Fn(usize) -> T + Send + Sync + 'static,
) -> Vec<T> {
    if !spread() || n < 2 {
        return (0..n).map(each).collect();
    }
    let out: std::sync::Arc<Vec<Mutex<Option<T>>>> =
        std::sync::Arc::new((0..n).map(|_| Mutex::new(None)).collect());
    let out_w = out.clone();
    crate::shadow::wide_chunks(n, 1, move |start, end| {
        for i in start..end {
            let value = each(i);
            *out_w[i].lock().unwrap_or_else(|e| e.into_inner()) = Some(value);
        }
    });
    match std::sync::Arc::try_unwrap(out) {
        Ok(slots) => slots
            .into_iter()
            .map(|m| {
                m.into_inner()
                    .unwrap_or_else(|e| e.into_inner())
                    .expect("spread_each slot")
            })
            .collect(),
        Err(slots) => slots
            .iter()
            .map(|m| {
                m.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .take()
                    .expect("spread_each slot")
            })
            .collect(),
    }
}

/// Asks the kernel for the written 4 KiB pages of `[first, last)` (64 KiB
/// journal pages) using a thread-local answer buffer, without touching
/// [`JOURNAL`]; returns `None` if the range is not write-watched memory of one
/// reservation.
fn kernel_written(first: usize, last: usize) -> Option<Vec<usize>> {
    use windows_sys::Win32::System::Memory::GetWriteWatch;
    const WRITE_WATCH_FLAG_RESET: u32 = 1;
    if known_view(first * PAGE) {
        return None;
    }
    thread_local! {
        static ASK_FOUND: std::cell::RefCell<Vec<usize>> = const { std::cell::RefCell::new(Vec::new()) };
    }
    ASK_FOUND.with(|buf| {
        let mut found = buf.borrow_mut();
        let mut count = (last - first) * PAGE / KERNEL_PAGE;
        if found.len() < count {
            found.resize(count, 0);
        }
        let mut granularity = 0;
        #[cfg(feature = "diagnostics")]
        let started = std::time::Instant::now();
        reservation::count_kernel(count);
        // SAFETY: the answer buffer holds `count` entries.
        let failed = unsafe {
            GetWriteWatch(
                WRITE_WATCH_FLAG_RESET,
                (first * PAGE) as *const _,
                (last - first) * PAGE,
                found.as_mut_ptr().cast(),
                &mut count,
                &mut granularity,
            )
        } != 0;
        if failed {
            return None;
        }
        #[cfg(feature = "diagnostics")]
        {
            use crate::copystat::{C, add};
            add(C::Watches, 1);
            add(C::WatchUs, started.elapsed().as_micros() as u64);
            slow(started.elapsed().as_micros() as u64);
        }
        COUNTS[5].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        COUNTS[6].fetch_add(
            ((last - first) * PAGE / KERNEL_PAGE) as u64,
            std::sync::atomic::Ordering::Relaxed,
        );
        COUNTS[7].fetch_add(count as u64, std::sync::atomic::Ordering::Relaxed);
        Some(found[..count].to_vec())
    })
}

/// Like [`ask_run`], but also asks leaf ranges of length 1 directly so
/// [`ask_once`] completes in a single pass with no follow-up scan.
fn ask_run_all(
    pieces: &[(usize, usize)],
    asked: &mut Vec<(usize, usize)>,
    written: &mut Vec<usize>,
) {
    if pieces.is_empty() {
        return;
    }
    if pieces.len() == 1 {
        let (first, last) = pieces[0];
        if let Some(pages) = kernel_written(first, last) {
            written.extend(pages);
            asked.push((first, last));
        }
        return;
    }
    let first = pieces[0].0;
    let last = pieces.iter().map(|p| p.1).max().unwrap_or(first);
    if let Some(pages) = kernel_written(first, last) {
        written.extend(pages);
        asked.push((first, last));
        return;
    }
    let (a, b) = pieces.split_at(pieces.len() / 2);
    ask_run_all(a, asked, written);
    ask_run_all(b, asked, written);
}

/// With [`once`]: asks about `ranges` (everything a submission's lists
/// refresh) as [`ask_ahead`] does, and lets that asking stand until
/// [`ask_once_end`].
pub(crate) fn ask_once(ranges: &[(usize, usize)]) {
    if !once() {
        return;
    }
    ask_once_end();
    if reservation::enabled() {
        reservation::ask(ranges);
        return;
    }
    if quiet_frames() > 0 {
        ask_once_quiet(ranges);
        return;
    }
    // Filter, sort, and drop contained ranges in O(N log N) before taking
    // `JOURNAL`, then ask both grouped and solo runs in a single parallel
    // pass and commit them under one `JOURNAL.write()` lock.
    let mut pieces: Vec<(usize, usize)> = ranges
        .iter()
        .filter(|r| {
            r.0 < r.1
                && !(light() && published_all(r.0 / BLOCK, r.1.div_ceil(BLOCK)))
                && !refused_as_view(r.0, r.1)
        })
        .map(|&(start, end)| (start / PAGE, end.div_ceil(PAGE)))
        .collect();
    if pieces.is_empty() {
        return;
    }
    pieces.sort_unstable_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
    let mut max_end = 0;
    pieces.retain(|&(_, end)| {
        if end <= max_end {
            false
        } else {
            max_end = end;
            true
        }
    });
    let (gap, most) = ahead_limits();
    let mut runs = Vec::new();
    let mut i = 0;
    while i < pieces.len() {
        let first = pieces[i].0;
        let mut last = pieces[i].1;
        let mut k = i + 1;
        while k < pieces.len() && pieces[k].0 <= last + gap && last.max(pieces[k].1) - first <= most
        {
            last = last.max(pieces[k].1);
            k += 1;
        }
        runs.push(i..k);
        i = k;
    }
    let pieces = std::sync::Arc::new(pieces);
    let runs = std::sync::Arc::new(runs);
    let (mut asked, mut written) = (Vec::new(), Vec::new());
    let n = runs.len();
    for (a, w) in spread_each(n, move |r| {
        let (mut a, mut w) = (Vec::new(), Vec::new());
        ask_run_all(&pieces[runs[r].clone()], &mut a, &mut w);
        (a, w)
    }) {
        asked.extend(a);
        written.extend(w);
    }
    let mut j = JOURNAL.write().unwrap_or_else(|e| e.into_inner());
    let now = next_asking();
    j.ahead_asked = now;
    note_kernel_batch(&written, now);
    j.kept = !asked.is_empty();
    KEPT.store(j.kept, std::sync::atomic::Ordering::Release);
    j.ahead = asked;
    j.ordered = false;
}

/// (Neptune) `VICTORY_ASK_QUIET=<frames>` (0 or unset: off): memory nothing was found written
/// in for that many frames is asked about once a frame, not once a submission.
///
/// The kernel walks every page of a range it is asked about, and each of a frame's thirty
/// submissions asked about the ranges its lists refresh, mostly the same ones: some 3,000
/// askings over 11 GB, a third of the submitting thread's time. Title memory is taken in
/// blocks of 1 MiB here, and a block is asked about whole. One in which a written page was
/// found within the last `frames` frames is asked about at every submission that refreshes
/// part of it, as before; any other at the first such submission of a frame, and the
/// journal answers for it until the next frame.
///
/// What changes: a write into a quiet block that arrives after the first asking of the
/// frame is seen by the next frame, so a list later in the same frame draws from the bytes
/// of before, once. From then on the block is asked about at every submission.
fn quiet_frames() -> u64 {
    static FRAMES: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *FRAMES.get_or_init(|| {
        std::env::var("VICTORY_ASK_QUIET")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    })
}

/// Blocks that may lie between two blocks of one asking (`VICTORY_ASK_QUIET_GAP_MB`, 4: a
/// clean page walked is 4 nanoseconds, a call that resets pages some 50 microseconds).
fn quiet_gap() -> usize {
    static GAP: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *GAP.get_or_init(|| {
        let mb = std::env::var("VICTORY_ASK_QUIET_GAP_MB")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(4usize);
        mb * ((1 << 20) / (QUIET_BLOCK * PAGE))
    })
}

/// Journal pages to a block of [`quiet_frames`].
const QUIET_BLOCK: usize = (256 << 10) / PAGE;

/// Of a block: the frame it was last asked about whole (`u64::MAX`: never), the last frame
/// a written page was found in it (`u64::MAX`: never), and whether asking about it whole
/// fails (it is not all inside one write-watched reservation).
#[derive(Clone, Copy)]
struct Quiet {
    asked: u64,
    written: u64,
    partial: bool,
}

const NEVER: Quiet = Quiet {
    asked: u64::MAX,
    written: u64::MAX,
    partial: false,
};

/// FxHash's multiply-rotate step for integer block keys (avoids SipHash on
/// ~44,000 block lookups/frame in [`ask_once_quiet`]).
#[derive(Default)]
struct Fx(u64);

impl std::hash::Hasher for Fx {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.write_u64(u64::from(b));
        }
    }

    fn write_u32(&mut self, n: u32) {
        self.write_u64(u64::from(n));
    }

    fn write_u64(&mut self, n: u64) {
        self.0 = (self.0.rotate_left(5) ^ n).wrapping_mul(0x517c_c1b7_2722_0a95);
    }

    fn write_usize(&mut self, n: usize) {
        self.write_u64(n as u64);
    }
}

struct Blocks {
    blocks: std::collections::HashMap<usize, Quiet, std::hash::BuildHasherDefault<Fx>>,
    /// Since the last line in the log: frames, blocks asked about because they were
    /// written lately, blocks asked about as the first of a frame, ranges answered by the
    /// journal alone, ranges asked about on their own.
    told: [u64; 5],
    frame: u64,
}

static BLOCKS: Mutex<Option<Blocks>> = Mutex::new(None);

/// [`ask_once`] with [`quiet_frames`].
fn ask_once_quiet(ranges: &[(usize, usize)]) {
    let frame = crate::queue::presented();
    let keep = quiet_frames();
    let mut held = BLOCKS.lock().unwrap_or_else(|e| e.into_inner());
    let state = held.get_or_insert_with(|| Blocks {
        blocks: Default::default(),
        told: [0; 5],
        frame,
    });
    let mut j = JOURNAL.write().unwrap_or_else(|e| e.into_inner());
    let j = &mut *j;
    j.ahead_asked = next_asking();
    j.ahead.clear();
    j.ordered = false;
    if state.frame != frame {
        state.told[0] += frame.wrapping_sub(state.frame).min(1 << 20);
        state.frame = frame;
        if state.told[0] >= 512 {
            let n = state.told[0] as f64;
            tracing::info!(
                "title memory asked about per frame (VICTORY_ASK_QUIET={keep}): {:.0} blocks of 256 KiB written lately, {:.0} quiet ones as the first of a frame, {:.0} ranges answered from the journal, {:.0} ranges on their own; {} blocks known",
                state.told[1] as f64 / n,
                state.told[2] as f64 / n,
                state.told[3] as f64 / n,
                state.told[4] as f64 / n,
                state.blocks.len()
            );
            state.told = [0; 5];
            let a = crate::fence::asked_taken();
            tracing::info!(
                "fence events per frame: {:.2} asked for a value already reached, {:.2} for one not yet reached, set {:.1} ms after they were asked for (in all, per frame)",
                a[0] as f64 / n,
                a[1] as f64 / n,
                a[2] as f64 / n / 1000.0
            );
            let w = crate::frame::waits_taken();
            tracing::info!(
                "frame waits per frame: on a tick {:.2} origin ({:.1} ms slept) and {:.2} other ({:.1} ms); timed out {:.2} origin ({:.1} ms) and {:.2} other ({:.1} ms); longest time-out asked {} ms",
                w[0] as f64 / n,
                w[4] as f64 / n / 1000.0,
                w[1] as f64 / n,
                w[5] as f64 / n / 1000.0,
                w[2] as f64 / n,
                w[6] as f64 / n / 1000.0,
                w[3] as f64 / n,
                w[7] as f64 / n / 1000.0,
                w[8]
            );
            let c: Vec<f64> = COUNTS
                .iter()
                .map(|c| c.swap(0, std::sync::atomic::Ordering::Relaxed) as f64 / n)
                .collect();
            tracing::info!(
                "guards per frame: {:.0} write faults, {:.0} settings of {:.0} blocks, {:.0} checks under the lock, {:.0} without; askings per frame: {:.0} over {:.0} pages of 4 KiB, {:.0} found written",
                c[0],
                c[1],
                c[2],
                c[3],
                c[4],
                c[5],
                c[6],
                c[7]
            );
        }
        if guard_hot() > 0 && frame % 512 == 0 {
            tracing::info!(
                "blocks of view memory left without their guard so far (VICTORY_GUARD_HOT={}): {}",
                guard_hot(),
                HOT_LEFT.load(std::sync::atomic::Ordering::Relaxed)
            );
        }
        if crate::shadow::copy_changed() && frame % 512 == 0 {
            let [read, differed] = crate::shadow::copy_changed_stats();
            let [quiet, through] = crate::shadow::quiet_back_stats();
            if watch_4k() {
                let [part_b, part_n, whole_b, whole_n] = copy_4k_stats();
                tracing::info!(
                    "stale pages refreshed (VICTORY_WATCH_4K=1): {} MB by 4 KiB masks ({} pages), {} MB whole ({} pages fell back to 64 KiB)",
                    part_b >> 20,
                    part_n,
                    whole_b >> 20,
                    whole_n
                );
            }
            tracing::info!(
                "refreshes of copies in the card's memory since the last such line (VICTORY_COPY_CHANGED=1): {} MB of title memory read, {} MB of it differed and went up; {} pages the copy-back kept quiet, {} of its writes through a view of our own (VICTORY_QUIET_BACK)",
                read >> 20,
                differed >> 20,
                quiet,
                through
            );
        }
    }
    // The blocks to ask about now.
    let mut wanted: Vec<usize> = Vec::new();
    for &(start, end) in ranges {
        if start >= end || refused_as_view(start, end) {
            continue;
        }
        let (first, last) = (start / PAGE, end.div_ceil(PAGE));
        for block in first / QUIET_BLOCK..=(last - 1) / QUIET_BLOCK {
            let q = *state.blocks.entry(block).or_insert(NEVER);
            if q.partial {
                continue;
            }
            let lately = q.written != u64::MAX && frame.wrapping_sub(q.written) <= keep;
            if q.asked != frame || lately {
                wanted.push(block);
            }
        }
    }
    wanted.sort_unstable();
    wanted.dedup();
    let mut i = 0;
    while i < wanted.len() {
        let mut k = i + 1;
        // Blocks up to 16 MiB apart are one asking: the kernel walks the pages between
        // them in less time than another call takes (and they count as asked about).
        while k < wanted.len()
            && wanted[k] <= wanted[k - 1] + 1 + quiet_gap()
            && wanted[k] - wanted[i] < 4096
        {
            k += 1;
        }
        let run = &wanted[i..k];
        let whole = ask_blocks(j, state, run[0], run[run.len() - 1] + 1, frame, keep);
        if !whole && run.len() > 1 {
            // One of them is not all write-watched memory: each on its own.
            for &block in run {
                if !ask_blocks(j, state, block, block + 1, frame, keep)
                    && let Some(q) = state.blocks.get_mut(&block)
                {
                    q.partial = true;
                }
            }
        } else if !whole && let Some(q) = state.blocks.get_mut(&run[0]) {
            q.partial = true;
        }
        i = k;
    }
    // What the journal now answers for: a range whose blocks were all asked about this
    // frame. Any other is asked about on its own, as without the switch.
    for &(start, end) in ranges {
        if start >= end || refused_as_view(start, end) {
            continue;
        }
        let (first, last) = (start / PAGE, end.div_ceil(PAGE));
        let covered = (first / QUIET_BLOCK..=(last - 1) / QUIET_BLOCK).all(|block| {
            state
                .blocks
                .get(&block)
                .is_some_and(|q| !q.partial && q.asked == frame)
        });
        if covered {
            state.told[3] += 1;
            j.ahead.push((first, last));
        } else if !j.ahead.iter().any(|&(a, b)| a <= first && last <= b)
            && ask_kernel(j, first, last)
        {
            state.told[4] += 1;
            j.ahead.push((first, last));
        }
    }
    j.kept = !j.ahead.is_empty();
    KEPT.store(j.kept, std::sync::atomic::Ordering::Release);
}

/// Asks about blocks `[first, last)` whole and notes the answer in them; false if the kernel
/// does not answer for all of it.
fn ask_blocks(
    j: &mut Journal,
    state: &mut Blocks,
    first: usize,
    last: usize,
    frame: u64,
    keep: u64,
) -> bool {
    let Some(count) = ask_kernel_found(j, first * QUIET_BLOCK, last * QUIET_BLOCK) else {
        return false;
    };
    for block in first..last {
        let q = state.blocks.entry(block).or_insert(NEVER);
        let lately = q.written != u64::MAX && frame.wrapping_sub(q.written) <= keep;
        state.told[if q.asked == frame || lately { 1 } else { 2 }] += 1;
        q.asked = frame;
    }
    for k in 0..count {
        let block = j.found[k] / PAGE / QUIET_BLOCK;
        if let Some(q) = state.blocks.get_mut(&block) {
            q.written = frame;
        }
    }
    true
}

/// (Neptune) Whether what the copy-back writes into title memory is kept from counting as the
/// title's writing for the copy it came from (`VICTORY_QUIET_BACK=1`; off by default).
///
/// The card's bytes go into title memory through the kernel, and the kernel then reports
/// those pages written like any other. The copy the bytes came from read them again at the
/// next submission, compared them, found nothing, and a range shaders write every frame
/// was looked at this way at every submission (an eighth of the submitting thread's time in
/// a sample, beside the refreshes' own reading). With the switch the thread that copies
/// back asks about the pages it is about to write (what the title wrote there is noted in
/// the journal as before), writes, resets the kernel's note of its own write, and notes the
/// pages in the journal itself: other copies of the same memory still see them written,
/// and the copy they came from is told it holds them ([`ours_after`]).
pub(crate) fn quiet_back() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_QUIET_BACK").is_some_and(|v| v == "1"))
}

/// Whether [`quiet_back`] also takes write-watched memory (`VICTORY_QUIET_BACK_WATCHED=1`; off
/// by default: it can lose a write of the title's, see `Shadow::card_merge_relay`).
pub(crate) fn quiet_back_watched() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_QUIET_BACK_WATCHED").is_some_and(|v| v == "1"))
}

/// Before our own write of `[at, at + len)` ([`quiet_back`]): asks the kernel about its 4 KiB
/// pages and notes what the title wrote there in the journal. The journal pages that held
/// such a write; `None` if the memory is not write-watched (nothing is to be done then).
pub(crate) fn ours_before(at: usize, len: usize) -> Option<Vec<usize>> {
    use windows_sys::Win32::System::Memory::GetWriteWatch;
    const WRITE_WATCH_FLAG_RESET: u32 = 1;
    let first = at & !(KERNEL_PAGE - 1);
    let last = at.saturating_add(len).div_ceil(KERNEL_PAGE) * KERNEL_PAGE;
    let mut found = [0usize; 40];
    if len == 0 || (last - first) / KERNEL_PAGE > found.len() {
        return None;
    }
    let mut count = found.len();
    let mut granularity = 0;
    // SAFETY: the answer buffer holds `count` entries.
    let failed = unsafe {
        GetWriteWatch(
            WRITE_WATCH_FLAG_RESET,
            first as *const _,
            last - first,
            found.as_mut_ptr().cast(),
            &mut count,
            &mut granularity,
        )
    } != 0;
    if failed {
        return None;
    }
    let mut pages: Vec<usize> = found[..count].iter().map(|&at| at / PAGE).collect();
    pages.dedup();
    if !pages.is_empty() {
        let now = next_asking();
        note_kernel_batch(&found[..count], now);
    }
    Some(pages)
}

/// After our own write of `[at, at + len)`, which [`ours_before`] answered for: the kernel
/// forgets it, and the journal takes the pages as written now. Per journal page: the asking
/// that had last found it written (0: none) and this one.
pub(crate) fn ours_after(at: usize, len: usize) -> Vec<(usize, u64, u64)> {
    use windows_sys::Win32::System::Memory::ResetWriteWatch;
    let first = at & !(KERNEL_PAGE - 1);
    let last = at.saturating_add(len).div_ceil(KERNEL_PAGE) * KERNEL_PAGE;
    if ours_together() {
        // Forgotten with the landing's other writes ([`ours_flush`]).
        OURS.lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((first, last));
    } else {
        // SAFETY: a range the kernel just answered for.
        unsafe { ResetWriteWatch(first as *const _, last - first) };
    }
    let now = next_asking();
    (at / PAGE..at.saturating_add(len).div_ceil(PAGE))
        .map(|page| (page, written_set(page, now), now))
        .collect()
}

/// (Neptune) Whether the kernel forgets the copy-back's writes of a landing together
/// (`VICTORY_QUIET_BACK_TOGETHER=0`: each as it is made, as before).
///
/// A call that resets written pages has every core drop what it knows of them, and the
/// caller waits for all of them: some 50 microseconds here, whatever the range (the walk
/// of a clean page is 4 nanoseconds). The copy-back made such a call for each of the 650
/// pieces it wrote in a frame. Now the ranges are kept, and when the landing's merges are
/// done one asking takes each run of them that lie near one another: what it finds outside
/// our own ranges the title wrote, and the journal is told so.
fn ours_together() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_QUIET_BACK_TOGETHER").is_none_or(|v| v != "0"))
}

/// Ranges of whole 4 KiB pages the copy-back wrote and the kernel has not forgotten yet.
static OURS: Mutex<Vec<(usize, usize)>> = Mutex::new(Vec::new());

/// The kernel forgets the writes [`ours_after`] kept ([`ours_together`]). Called when a
/// landing's merges are done.
pub(crate) fn ours_flush() {
    use windows_sys::Win32::System::Memory::{GetWriteWatch, ResetWriteWatch};
    const WRITE_WATCH_FLAG_RESET: u32 = 1;
    // Ranges up to 32 MiB apart are one asking, of 1 GiB at most.
    const GAP: usize = 32 << 20;
    const MOST: usize = 1 << 30;
    let mut ours = std::mem::take(&mut *OURS.lock().unwrap_or_else(|e| e.into_inner()));
    if ours.is_empty() {
        return;
    }
    ours.sort_unstable();
    thread_local! {
        static FOUND: std::cell::RefCell<Vec<usize>> = const { std::cell::RefCell::new(Vec::new()) };
    }
    FOUND.with(|found| {
        let mut found = found.borrow_mut();
        let mut i = 0;
        while i < ours.len() {
            let first = ours[i].0;
            let mut last = ours[i].1;
            let mut k = i + 1;
            while k < ours.len() && ours[k].0 <= last + GAP && ours[k].1.max(last) - first <= MOST {
                last = last.max(ours[k].1);
                k += 1;
            }
            let run = &ours[i..k];
            let mut count = (last - first) / KERNEL_PAGE;
            if found.len() < count {
                found.resize(count, 0);
            }
            let mut granularity = 0;
            // SAFETY: the answer buffer holds `count` entries.
            let failed = unsafe {
                GetWriteWatch(
                    WRITE_WATCH_FLAG_RESET,
                    first as *const _,
                    last - first,
                    found.as_mut_ptr().cast(),
                    &mut count,
                    &mut granularity,
                )
            } != 0;
            COUNTS[5].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if failed {
                // Not all one reservation: each on its own, as before.
                for &(a, b) in run {
                    // SAFETY: a range the kernel answered for before the write.
                    unsafe { ResetWriteWatch(a as *const _, b - a) };
                }
            } else {
                // What lies outside our own ranges the title wrote.
                let mut now = 0;
                for &at in &found[..count] {
                    let before = run.partition_point(|r| r.0 <= at);
                    // The ranges are in order of their starts and short: the few before
                    // `at` are looked at.
                    let ours_too = run[before.saturating_sub(8)..before]
                        .iter()
                        .any(|r| at >= r.0 && at < r.1);
                    if !ours_too {
                        if now == 0 {
                            now = next_asking();
                        }
                        written_set(at / PAGE, now);
                    }
                }
            }
            i = k;
        }
    });
}

/// (Neptune) Whether view memory `[at, at + len)` is all guarded and clean: nothing was written
/// there since a copy last took it ([`quiet_back`]).
pub(crate) fn guarded_clean(at: usize, len: usize) -> bool {
    let (first, last) = (at / BLOCK, at.saturating_add(len).div_ceil(BLOCK));
    first < last && clean_all(first, last)
}

/// After our own write of view memory `[at, at + len)` through a view of our own
/// (`crate::inplace::host_alias`), which no guard saw: the journal takes its pages as
/// written now, at every title view that shows them. Per journal page of the range: the
/// asking that had last found it written (0: none) and this one.
pub(crate) fn ours_through(at: usize, len: usize) -> Vec<(usize, u64, u64)> {
    let now = next_asking();
    let mut marks = Vec::new();
    for page in at / PAGE..at.saturating_add(len).div_ceil(PAGE) {
        marks.push((page, written_set(page, now), now));
        for alias in crate::inplace::pages_of(page * PAGE) {
            if alias / PAGE != page {
                written_set(alias / PAGE, now);
            }
        }
    }
    marks
}

/// The submission's one asking stands no longer: its refreshes are done,
/// or title memory was written in a way the journal was not told.
pub(crate) fn ask_once_end() {
    if !ask_once_active() && !reservation::enabled() {
        return;
    }
    let mut j = JOURNAL.write().unwrap_or_else(|e| e.into_inner());
    if j.kept || reservation::enabled() {
        j.kept = false;
        KEPT.store(false, std::sync::atomic::Ordering::Release);
        j.ahead.clear();
        j.ordered = false;
    }
}

/// We wrote title memory `[at, at + len)` while a submission's one asking
/// stands (the title's buffer copies, done at submission): its pages count
/// as written after that asking, so the refreshes that follow copy them.
pub(crate) fn wrote(at: usize, len: usize) {
    if len == 0 || !ask_once_active() {
        return;
    }
    let now = next_asking();
    // The asking that stands moves with it: `asked` and `writes` give it as
    // "now", and a page noted written after "now" stays stale for every
    // copy that takes it (copied at `now`, written at `now + 1`): copied
    // again at every refresh, and a reason to settle flights
    // (fxceless/victory-fork#410).
    JOURNAL
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .ahead_asked = now;
    for page in at / PAGE..at.saturating_add(len).div_ceil(PAGE) {
        written_set(page, now);
    }
}

/// Bytes [`written_through_view`] was told of since the last ram line.
pub(crate) static NOTIFIED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// `VICTORY_DS_NOTIFY=1` (default off): the runtime's writes into physical pages through a view
/// of its own are told to the journal at the title's addresses that show them.
pub fn notify_view_writes() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_DS_NOTIFY").is_some_and(|v| v == "1"))
}

/// The runtime wrote the physical pages shown at title memory `[at, at + len)` through a view of
/// its own (DirectStorage, `write_physical`): the pages count as written now, so host copies
/// and textures taken before refresh. Unlike [`wrote`] with no asking standing too: nothing
/// else (no guard fault, no kernel watch of the title's view) sees such a write.
pub fn written_through_view(at: usize, len: usize) {
    if len == 0 {
        return;
    }
    // Release any lazy GPU-resident pages without clobbering bytes already
    // written into the backing physical section through an alias view.
    crate::shadow::lazy_release_written_range(at, len);
    let now = next_asking();
    for page in at / PAGE..at.saturating_add(len).div_ceil(PAGE) {
        written_set(page, now);
    }
    NOTIFIED.fetch_add(len as u64, std::sync::atomic::Ordering::Relaxed);
}

#[cfg(test)]
pub(crate) fn note_written_for_test(page: usize) {
    let now = next_asking();
    written_set(page, now);
}

/// One kernel asking for `pieces` (page ranges in order); by halves if it
/// fails, as a run across two reservations or into memory that is not
/// write-watched does. A piece alone is left to [`writes`].
fn ask_run(j: &mut Journal, pieces: &[(usize, usize)]) {
    if pieces.len() < 2 {
        return;
    }
    let first = pieces[0].0;
    let last = pieces.iter().map(|p| p.1).max().unwrap_or(first);
    if ask_kernel(j, first, last) {
        j.ahead.push((first, last));
        return;
    }
    let (a, b) = pieces.split_at(pieces.len() / 2);
    ask_run(j, a);
    ask_run(j, b);
}

/// Asks the kernel for the written pages of pages `[first, last)` and notes
/// them in the journal under the ahead asking; false (and nothing changed)
/// if the range is not write-watched memory of one reservation.
fn ask_kernel(j: &mut Journal, first: usize, last: usize) -> bool {
    ask_kernel_found(j, first, last).is_some()
}

/// [`ask_kernel`], saying how many written 4 KiB pages the kernel reported (they are the
/// first of `j.found`); `None` where it says false.
fn ask_kernel_found(j: &mut Journal, first: usize, last: usize) -> Option<usize> {
    use windows_sys::Win32::System::Memory::GetWriteWatch;
    const WRITE_WATCH_FLAG_RESET: u32 = 1;
    let mut count = (last - first) * PAGE / KERNEL_PAGE;
    j.found.resize(count.max(j.found.len()), 0);
    let mut granularity = 0;
    #[cfg(feature = "diagnostics")]
    let started = std::time::Instant::now();
    // (Neptune) View memory the guards already hold cannot be write-watched, and a range that
    // starts in it fails as a whole: no asking (fxceless/victory-fork 9376fd6).
    let known = known_view(first * PAGE);
    if !known {
        reservation::count_kernel(count);
    }
    // SAFETY: the answer buffer holds `count` entries.
    let failed = known
        || unsafe {
            GetWriteWatch(
                WRITE_WATCH_FLAG_RESET,
                (first * PAGE) as *const _,
                (last - first) * PAGE,
                j.found.as_mut_ptr().cast(),
                &mut count,
                &mut granularity,
            )
        } != 0;
    if failed {
        return None;
    }
    #[cfg(feature = "diagnostics")]
    {
        use crate::copystat::{C, add};
        add(C::Watches, 1);
        add(C::WatchUs, started.elapsed().as_micros() as u64);
        slow(started.elapsed().as_micros() as u64);
    }
    COUNTS[5].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    COUNTS[6].fetch_add(
        ((last - first) * PAGE / KERNEL_PAGE) as u64,
        std::sync::atomic::Ordering::Relaxed,
    );
    COUNTS[7].fetch_add(count as u64, std::sync::atomic::Ordering::Relaxed);
    let now = j.ahead_asked;
    // Noted with a later asking meanwhile (the copy-back's own write) it gets a number
    // that says the kernel's find is newer (`note_kernel_batch` / `found_set`).
    note_kernel_batch(&j.found[..count], now);
    Some(count)
}

/// What [`writes`] would say of `[start, end)` from the journal alone, if
/// the range lies in one asked ahead ([`ask_ahead`], [`ask_once`]): the
/// pages written since tracking began and the asking that last found each
/// written, with the number of the asking that stands (as [`writes`] gives
/// it). `None` if the kernel would have to be asked.
pub(crate) fn asked(start: usize, end: usize) -> Option<(u64, Vec<(usize, u64)>)> {
    let (first, last) = (start / PAGE, end.div_ceil(PAGE));
    let standing = {
        let j = JOURNAL.read().unwrap_or_else(|e| e.into_inner());
        reservation::covered(&j, first, last).then_some(j.ahead_asked)
    };
    standing.map(|asked| (asked, written_in(first, last)))
}

/// The pages of `[start, end)` (whole pages) as last written, for pages
/// written since tracking began: `(page number, asking)` in page order, and
/// this asking's number, which is higher than any in the list but those the
/// kernel just reported. `None` if the memory is not write-watched (nor,
/// with `guarded`, view memory, which is then guarded).
pub(crate) fn writes(start: usize, end: usize, guarded: bool) -> Option<(u64, Vec<(usize, u64)>)> {
    use windows_sys::Win32::System::Memory::GetWriteWatch;
    const WRITE_WATCH_FLAG_RESET: u32 = 1;
    let (first, last) = (start / PAGE, end.div_ceil(PAGE));
    let (first_b, last_b) = (start / BLOCK, end.div_ceil(BLOCK));
    let t_path = crate::refresh_prof::start();
    if guarded && light() && start < end && published_all(first_b, last_b) {
        let out = Some((next_asking(), written_in(first, last)));
        crate::refresh_prof::add(crate::refresh_prof::P::PublishedCalls, 1);
        crate::refresh_prof::finish(crate::refresh_prof::P::PublishedUs, t_path);
        return out;
    }
    // Asked ahead with the submission's other ranges: the journal has it. (Neptune) The lock
    // is held for the look alone, and the kernel is asked without it: the pages are
    // noted in a table of their own (`written_set`). Known view memory is never covered by
    // GetWriteWatch reservations, so skip the shared RwLock on `VIEWS.all`.
    let skip_journal = guarded && light() && skip_views() && start < end && VIEWS.all(first_b, last_b);
    let standing = if skip_journal {
        None
    } else {
        let j = JOURNAL.read().unwrap_or_else(|e| e.into_inner());
        reservation::covered(&j, first, last).then_some(j.ahead_asked)
    };
    if let Some(asked) = standing {
        let out = Some((asked, written_in(first, last)));
        crate::refresh_prof::add(crate::refresh_prof::P::StandingCalls, 1);
        crate::refresh_prof::finish(crate::refresh_prof::P::StandingUs, t_path);
        return out;
    }
    // View memory the kernel would refuse, and the caller does not ask the
    // guards (textures, which then fall back to guarded/hash tracking): no
    // failing kernel call.
    if !guarded && refused_as_view(start, end) {
        return None;
    }
    let skip_enabled = light() && skip_views() && start < end;
    let protected = guarded && skip_enabled && PROTECTED.any(first_b, last_b);
    let viewed = guarded && skip_enabled && !protected && VIEWS.any(first_b, last_b);
    if viewed && port_guard() {
        port6_count(P6_NOT_ASKED, 1);
    }
    let refused = protected || viewed;
    // View memory the guards already hold cannot be write-watched: skip
    // the failing kernel GetWriteWatch call and go straight to `guard`.
    let known = guarded && !refused && known_view(first * PAGE);
    thread_local! {
        /// Room for the kernel's answer.
        static FOUND: std::cell::RefCell<Vec<usize>> = const { std::cell::RefCell::new(Vec::new()) };
    }
    FOUND.with(|found| {
        let mut found = found.borrow_mut();
        let ask = |found: &mut Vec<usize>, count: &mut usize| -> bool {
            let need = (last - first) * PAGE / KERNEL_PAGE;
            if found.len() < need {
                found.resize(need, 0);
            }
            *count = need;
            let mut granularity = 0;
            reservation::count_kernel(need);
            // SAFETY: the answer buffer holds `*count` entries.
            unsafe {
                GetWriteWatch(
                    WRITE_WATCH_FLAG_RESET,
                    (first * PAGE) as *const _,
                    (last - first) * PAGE,
                    found.as_mut_ptr().cast(),
                    count,
                    &mut granularity,
                ) == 0
            }
        };
        let mut count = 0usize;
        #[cfg(feature = "diagnostics")]
        let started = std::time::Instant::now();
        let mut failed = if refused || known {
            true
        } else {
            !ask(&mut found, &mut count)
        };
        let (guarded_pages, epochs) = if failed {
            count = 0;
            if !guarded {
                return None;
            }
            #[cfg(feature = "diagnostics")]
            let guarding = std::time::Instant::now();
            let res = guard_with_epochs(first * PAGE, last * PAGE);
            #[cfg(feature = "diagnostics")]
            crate::copystat::add(
                crate::copystat::C::GuardUs,
                guarding.elapsed().as_micros() as u64,
            );
            match res {
                Some(pair) => pair,
                None if refused => {
                    // Not a view after all (regular memory at this address now,
                    // or remapped concurrently): clear stale `VIEWS` bits and
                    // fall back to `GetWriteWatch`.
                    if viewed {
                        VIEWS.clear_range(first_b, last_b);
                    }
                    if ask(&mut found, &mut count) {
                        failed = false;
                        (Vec::new(), Vec::new())
                    } else {
                        return None;
                    }
                }
                None => return None,
            }
        } else {
            (Vec::new(), Vec::new())
        };
        #[cfg(feature = "diagnostics")]
        {
            use crate::copystat::{C, add};
            add(C::Watches, 1);
            add(C::WatchUs, started.elapsed().as_micros() as u64);
            slow(started.elapsed().as_micros() as u64);
        }
        let now = next_asking();
        note_kernel_batch(&found[..count], now);
        for &page in &guarded_pages {
            written_set(page, now);
        }
        if failed {
            publish(&epochs);
            crate::refresh_prof::add(crate::refresh_prof::P::GuardCalls, 1);
            crate::refresh_prof::finish(crate::refresh_prof::P::GuardUs, t_path);
        } else {
            crate::refresh_prof::add(crate::refresh_prof::P::KernelCalls, 1);
            crate::refresh_prof::finish(crate::refresh_prof::P::KernelUs, t_path);
        }
        Some((now, written_in(first, last)))
    })
}

impl Tracked {
    /// Takes changed pages of `[from, to)`, cut to the tracked range. A
    /// partial page stays changed so a later take can read its other bytes.
    pub(crate) fn take(&self, from: usize, to: usize) -> Vec<(usize, usize)> {
        let (from, to) = (from.max(self.start), to.min(self.end));
        if from >= to {
            return Vec::new();
        }
        let Some((_, pages)) = writes(from, to, true) else {
            return Vec::new();
        };
        let mut seen = self.seen.lock().unwrap_or_else(|e| e.into_inner());
        let mut ranges: Vec<(usize, usize)> = Vec::new();
        for (page, write) in pages {
            if seen.get(&page).is_some_and(|&last| last >= write) {
                continue;
            }
            let (start, end) = ((page * PAGE).max(from), ((page + 1) * PAGE).min(to));
            if start >= end {
                continue;
            }
            // Keep partial pages dirty: the caller has not taken the rest.
            if (page * PAGE).max(self.start) >= from && ((page + 1) * PAGE).min(self.end) <= to {
                seen.insert(page, write);
            }
            match ranges.last_mut() {
                Some((_, last)) if *last == start => *last = end,
                _ => ranges.push((start, end)),
            }
        }
        ranges
    }
}

/// Guarded blocks of view memory.
const BLOCK: usize = PAGE;

/// Whether the block holding `start` is view memory the guards already hold (read-only by our
/// hand, or noted absent): asking the kernel's write watch about it can only fail.
/// `VICTORY_VIEW_ASK=1` (or `VICTORY_GUARD_ASK=1`) asks anyway, as before.
fn known_view(start: usize) -> bool {
    static ASK: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let ask = *ASK.get_or_init(|| {
        ["VICTORY_VIEW_ASK", "VICTORY_GUARD_ASK"]
            .iter()
            .any(|n| std::env::var_os(n).is_some_and(|v| v == "1"))
    });
    if ask {
        return false;
    }
    let block = start / BLOCK;
    if crate::shadow::is_lazy_mapped_view_page(block) {
        return true;
    }
    if light() && published_all(block, block + 1) {
        return true;
    }
    if light() && skip_views() && PROTECTED.any(block, block + 1) {
        return true;
    }
    if clean_all(block, block + 1) || absent_kind(block) != ABSENT_NONE {
        return true;
    }
    let g = guards_read();
    g.is_known_view_block(block)
}

/// Disjoint coalesced half-open block intervals `[start, end)`, stored as
/// `start -> end` with invariant `start < end` and `prev.end < next.start`.
#[cfg(test)]
struct BlockSet(BTreeMap<usize, usize>);

#[cfg(test)]
impl BlockSet {
    const fn new() -> Self {
        Self(BTreeMap::new())
    }

    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn contains(&self, block: usize) -> bool {
        self.0
            .range(..=block)
            .next_back()
            .is_some_and(|(_, &end)| block < end)
    }

    /// True if every block in `[from, to)` is contained in a single coalesced run.
    fn covers(&self, from: usize, to: usize) -> bool {
        from < to
            && self
                .0
                .range(..=from)
                .next_back()
                .is_some_and(|(_, &end)| end >= to)
    }

    /// If `block` is present, returns the exclusive end of its contiguous run.
    fn run_end_at(&self, block: usize) -> Option<usize> {
        self.0
            .range(..=block)
            .next_back()
            .and_then(|(_, &end)| (block < end).then_some(end))
    }

    /// Start of the first interval at or after `block`.
    fn next_start_from(&self, block: usize) -> Option<usize> {
        self.0.range(block..).next().map(|(&start, _)| start)
    }

    /// True if any block in `[from, to)` is present.
    fn intersects(&self, from: usize, to: usize) -> bool {
        if from >= to {
            return false;
        }
        if self
            .0
            .range(..=from)
            .next_back()
            .is_some_and(|(_, &end)| end > from)
        {
            return true;
        }
        self.0.range(from..to).next().is_some()
    }

    /// Total number of blocks in `[from, to)` present in this set.
    fn count_in(&self, from: usize, to: usize) -> usize {
        if from >= to {
            return 0;
        }
        let mut total = 0usize;
        if let Some((_, &end)) = self.0.range(..from).next_back()
            && end > from
        {
            total += end.min(to) - from;
        }
        for (&start, &end) in self.0.range(from..to) {
            total += end.min(to) - start;
        }
        total
    }

    #[cfg(test)]
    fn insert(&mut self, block: usize) {
        if let Some(next) = block.checked_add(1) {
            self.insert_range(block, next);
        }
    }

    fn insert_range(&mut self, mut from: usize, mut to: usize) {
        if from >= to {
            return;
        }
        if let Some((&prev_start, &prev_end)) = self.0.range(..=from).next_back()
            && prev_end >= from
        {
            if prev_end >= to {
                return;
            }
            from = prev_start;
            to = to.max(prev_end);
        }
        while let Some((&s, &e)) = self.0.range(from..=to).next() {
            self.0.remove(&s);
            if e > to {
                to = e;
            }
        }
        self.0.insert(from, to);
    }

    fn insert_sorted_blocks(&mut self, blocks: &[usize]) {
        let mut i = 0;
        while i < blocks.len() {
            let start = blocks[i];
            let mut end = start.saturating_add(1);
            i += 1;
            while i < blocks.len() && blocks[i] <= end {
                if blocks[i] == end {
                    end = end.saturating_add(1);
                }
                i += 1;
            }
            self.insert_range(start, end);
        }
    }

    /// Removes a single `block`; returns true if it was present.
    fn remove(&mut self, block: usize) -> bool {
        let Some((&s, &e)) = self.0.range(..=block).next_back() else {
            return false;
        };
        if block >= e {
            return false;
        }
        let next = block + 1;
        if s == block {
            self.0.remove(&s);
            if next < e {
                self.0.insert(next, e);
            }
        } else {
            self.0.insert(s, block);
            if next < e {
                self.0.insert(next, e);
            }
        }
        true
    }

    /// Removes all blocks in `[from, to)` and returns the removed sub-intervals
    /// in ascending order.
    fn take_range(&mut self, from: usize, to: usize) -> Vec<(usize, usize)> {
        let mut taken = Vec::new();
        if from >= to {
            return taken;
        }
        if let Some((&s, &e)) = self.0.range(..from).next_back()
            && e > from
        {
            self.0.insert(s, from);
            let cut_end = e.min(to);
            taken.push((from, cut_end));
            if e > to {
                self.0.insert(to, e);
                return taken;
            }
        }
        while let Some((&s, &e)) = self.0.range(from..to).next() {
            self.0.remove(&s);
            let cut_end = e.min(to);
            taken.push((s, cut_end));
            if e > to {
                self.0.insert(to, e);
                break;
            }
        }
        taken
    }
}

#[link(name = "ntdll", kind = "raw-dylib")]
unsafe extern "system" {
    /// The kernel's question about a process's memory, `(process, address,
    /// class of the answer, answer out, its room, bytes answered out)`: an
    /// NTSTATUS, negative when it cannot answer. It changes nothing.
    fn NtQueryVirtualMemory(
        process: *mut std::ffi::c_void,
        at: *const std::ffi::c_void,
        class: u32,
        out: *mut std::ffi::c_void,
        len: usize,
        returned: *mut usize,
    ) -> i32;
}

/// Queries `NtQueryVirtualMemory(MemoryRegionInformationEx)` at `block`.
/// Returns `(is_placeholder, region_end_block)` where `region_end_block` is
/// at least `block + 1` so callers can advance past an entire OS region
/// (placeholder or ordinary reservation) in one syscall.
fn placeholder_region_info(block: usize) -> (bool, usize) {
    #[repr(C)]
    #[derive(Default)]
    #[allow(dead_code)]
    struct Region {
        allocation_base: usize,
        allocation_protect: u32,
        region_type: u32,
        region_size: usize,
        commit_size: usize,
        partition_id: usize,
        node_preference: usize,
    }
    const MEMORY_REGION_INFORMATION_EX: u32 = 7;
    const PLACEHOLDER_RESERVATION: u32 = 1 << 8;
    let Some(addr) = block.checked_mul(BLOCK) else {
        return (false, block.saturating_add(1));
    };
    let mut region = Region::default();
    // SAFETY: the answer's room and its size; the kernel checks the address.
    let status = unsafe {
        NtQueryVirtualMemory(
            windows_sys::Win32::System::Threading::GetCurrentProcess(),
            addr as *const _,
            MEMORY_REGION_INFORMATION_EX,
            (&raw mut region).cast(),
            size_of::<Region>(),
            std::ptr::null_mut(),
        )
    };
    if status >= 0 {
        let end = region.allocation_base.saturating_add(region.region_size) / BLOCK;
        let is_ph = region.region_type & PLACEHOLDER_RESERVATION != 0 && end > block;
        (is_ph, end.max(block.saturating_add(1)))
    } else {
        (false, block.saturating_add(1))
    }
}

/// Atomic bitset of 64 KiB guard blocks with an O(1) population counter.
/// Under `GUARDS.write()` (`VICTORY_PORT_GUARD=0` and remap/open paths) it is
/// accessed exclusively; under `GUARDS.read()` (`VICTORY_PORT_GUARD=1`) threads
/// mutate disjoint blocks concurrently while holding each block's [`BUSY`] claim.
struct GuardBlocks {
    bits: Bits,
    count: std::sync::atomic::AtomicUsize,
}

impl GuardBlocks {
    const fn new() -> Self {
        Self {
            bits: Bits::new(),
            count: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    fn is_empty(&self) -> bool {
        self.count.load(std::sync::atomic::Ordering::Acquire) == 0
    }

    fn contains(&self, block: usize) -> bool {
        self.bits.any(block, block.saturating_add(1))
    }

    fn covers(&self, from: usize, to: usize) -> bool {
        from < to && self.bits.all(from, to)
    }

    fn run_end_in(&self, block: usize, limit: usize) -> Option<usize> {
        self.bits.run_end_in(block, limit)
    }

    fn first_in(&self, from: usize, to: usize) -> Option<usize> {
        self.bits.first_in(from, to)
    }

    fn intersects(&self, from: usize, to: usize) -> bool {
        self.bits.any(from, to)
    }

    fn count_in(&self, from: usize, to: usize) -> usize {
        self.bits.count_in(from, to)
    }

    fn insert(&self, block: usize) -> bool {
        if self.bits.set_was(block) == Some(false) {
            self.count
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            true
        } else {
            false
        }
    }

    fn insert_range(&self, from: usize, to: usize) {
        let added = self.bits.set_range_counted(from, to);
        if added != 0 {
            self.count
                .fetch_add(added, std::sync::atomic::Ordering::Relaxed);
        }
    }

    fn insert_sorted_blocks(&self, blocks: &[usize]) {
        let mut i = 0;
        while i < blocks.len() {
            let start = blocks[i];
            let mut end = start.saturating_add(1);
            i += 1;
            while i < blocks.len() && blocks[i] <= end {
                if blocks[i] == end {
                    end = end.saturating_add(1);
                }
                i += 1;
            }
            self.insert_range(start, end);
        }
    }

    fn remove(&self, block: usize) -> bool {
        if self.bits.clear_was(block) {
            self.count
                .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
            true
        } else {
            false
        }
    }

    fn remove_range(&self, from: usize, to: usize) {
        let removed = self.bits.clear_range_counted(from, to);
        if removed != 0 {
            self.count
                .fetch_sub(removed, std::sync::atomic::Ordering::Relaxed);
        }
    }

    fn take_range(&self, from: usize, to: usize) -> Vec<(usize, usize)> {
        let (runs, removed) = self.bits.take_range_runs(from, to);
        if removed != 0 {
            self.count
                .fetch_sub(removed, std::sync::atomic::Ordering::Relaxed);
        }
        runs
    }
}

struct Guards {
    /// Blocks we made read-only.
    protected: GuardBlocks,
    /// Of those, the ones not written since a copy took them.
    clean: GuardBlocks,
    /// Blocks already copied as zeros. While they remain wholly uncommitted,
    /// the copy can skip hundreds of MB of failing kernel reads per world frame.
    absent: GuardBlocks,
    /// Subset of `absent` confirmed to lie in `MEM_RESERVE_PLACEHOLDER`
    /// reservations, where pages can only appear through runtime remaps
    /// ([`title_memory_remapped`]).
    placeholder_absent: GuardBlocks,
    /// Blocks whose physical page was guarded together with its other views
    /// (those views' blocks are in it too): a write to one opens them all.
    /// A write range can be huge (`fread`), so only these are looked up.
    aliased: GuardBlocks,
}

impl Guards {
    fn is_known_view_block(&self, block: usize) -> bool {
        self.protected.contains(block) || self.absent.contains(block)
    }
}

// (Neptune) Read by many at once: a refresh of view memory asks whether its blocks are all
// clean, which nearly all are, and the refreshes of a submission run on several threads;
// behind one lock they stood in line with one another and with the thread that copies back
// (a sixth of the submitting thread's time in a sample). With `VICTORY_PORT_GUARD=1`
// write faults and refreshes also mutate disjoint blocks under `.read()` + `BUSY` claims.
static GUARDS: std::sync::RwLock<Guards> = std::sync::RwLock::new(Guards {
    protected: GuardBlocks::new(),
    clean: GuardBlocks::new(),
    absent: GuardBlocks::new(),
    placeholder_absent: GuardBlocks::new(),
    aliased: GuardBlocks::new(),
});

fn guards() -> std::sync::RwLockWriteGuard<'static, Guards> {
    GUARDS.write().unwrap_or_else(|e| e.into_inner())
}

/// The guards, to look at only.
fn guards_read() -> std::sync::RwLockReadGuard<'static, Guards> {
    GUARDS.read().unwrap_or_else(|e| e.into_inner())
}

/// For view memory: the pages of `[start, end)`'s blocks written since
/// they were last guarded (all of a block's pages), guarding them again
/// before the caller copies them. `None` if the memory is not a view.
#[cfg(test)]
fn guard(start: usize, end: usize) -> Option<Vec<usize>> {
    guard_with_epochs(start, end).map(|(w, _)| w)
}

#[cfg(test)]
fn guard_with_flags(
    start: usize,
    end: usize,
    skip_placeholders: bool,
    single_lock: bool,
    coalesce: bool,
    alias_batch: bool,
) -> Option<Vec<usize>> {
    guard_with_epochs_flags(
        start,
        end,
        skip_placeholders,
        single_lock,
        coalesce,
        alias_batch,
    )
    .map(|(w, _)| w)
}

fn guard_with_epochs(start: usize, end: usize) -> Option<(Vec<usize>, Vec<(usize, usize, u64)>)> {
    guard_with_epochs_flags(
        start,
        end,
        placeholder_skip_enabled(),
        watch_single_lock(),
        watch_coalesce(),
        watch_alias_batch(),
    )
}

/// [`guard_with_epochs`] parameterized over placeholder-skipping and diagnostic bisect flags.
fn guard_with_epochs_flags(
    start: usize,
    end: usize,
    skip_placeholders: bool,
    single_lock: bool,
    coalesce: bool,
    alias_batch: bool,
) -> Option<(Vec<usize>, Vec<(usize, usize, u64)>)> {
    // A range whose first block the guards hold already (read-only by our
    // hand, or noted absent) is a view: no question to Windows.
    // `VICTORY_GUARD_ASK=1` asks every time, as before.
    static ASK: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let ask = *ASK.get_or_init(|| std::env::var_os("VICTORY_GUARD_ASK").is_some_and(|v| v == "1"));
    let (first, last) = (start / BLOCK, end.div_ceil(BLOCK));
    if !ask && first < last && clean_all(first, last) {
        // (Neptune) All clean, so all guarded and a view: said without the lock.
        count(4, 1);
        return Some((Vec::new(), Vec::new()));
    }
    let hot = guard_hot();
    let frame = crate::queue::presented();
    if !ask && first < last {
        // (Neptune) Clean blocks, lazy-deferred MEM_MAPPED blocks, blocks left open, and
        // blocks that are still not committed (confirmed placeholder holes skip VirtualQuery
        // altogether when enabled), nothing else: answered without the lock, reporting open
        // hot blocks as written.
        const { assert!(BLOCK == PAGE) };
        let mut written = Vec::new();
        let mut settled = true;
        let mut uncommitted_until = first;
        let has_lazy_mapped = crate::shadow::has_lazy_mapped_view_pages();
        for block in first..last {
            if clean_all(block, block + 1) {
                continue;
            }
            if has_lazy_mapped && crate::shadow::is_lazy_mapped_view_page(block) {
                continue;
            }
            if hot > 0 && left_open(block, frame, hot) {
                written.push(block);
                continue;
            }
            match absent_kind(block) {
                ABSENT_PLACEHOLDER if skip_placeholders => continue,
                ABSENT_ORDINARY | ABSENT_PLACEHOLDER => {
                    // Ordinary reservations (or when placeholder skip is disabled):
                    // reuse one VirtualQuery across the contiguous uncommitted region.
                    if block >= uncommitted_until {
                        uncommitted_until = uncommitted_end(block);
                    }
                    if block < uncommitted_until {
                        continue;
                    }
                }
                _ => {}
            }
            settled = false;
            break;
        }
        if settled {
            count(4, 1);
            return Some((written, Vec::new()));
        }
    }
    let known = {
        let g = guards_read();
        let known = !ask
            && (g.is_known_view_block(first)
                || crate::shadow::is_lazy_mapped_view_page(first)
                || (hot > 0 && left_open(first, frame, hot)));
        if known && (first >= last || g.clean.covers(first, last)) {
            // (Neptune) Nothing written, mapped or new since the range was last taken: said
            // beside the other readers in O(log K).
            return Some((Vec::new(), Vec::new()));
        }
        known
    };
    if port_guard() {
        if !known && !is_view(start, end) {
            return None;
        }
        if !INSTALLED.load(std::sync::atomic::Ordering::Acquire) {
            install();
        }
        if first < last && !VIEWS.all(first, last) {
            VIEWS.set_range(first, last);
        }
        return Some(guard_shared(
            first,
            last,
            hot,
            frame,
            skip_placeholders,
            coalesce,
            alias_batch,
        ));
    }
    let g = if single_lock && known {
        if !INSTALLED.load(std::sync::atomic::Ordering::Acquire) {
            install();
        }
        let g = guards();
        if g.is_known_view_block(first)
            || crate::shadow::is_lazy_mapped_view_page(first)
            || (hot > 0 && left_open(first, frame, hot))
        {
            g
        } else {
            drop(g);
            if !is_view(start, end) {
                return None;
            }
            install();
            guards()
        }
    } else {
        if !known && !is_view(start, end) {
            return None;
        }
        install();
        guards()
    };
    count(3, 1);
    if first < last && !VIEWS.all(first, last) {
        VIEWS.set_range(first, last);
    }
    Some(guard_locked(
        &g,
        first,
        last,
        hot,
        frame,
        skip_placeholders,
        coalesce,
        alias_batch,
    ))
}

/// Locked guard walk for blocks `[first, last)` under exclusive `GUARDS.write()`:
/// - Checks `g.clean.covers(first, last)` in word-stride bitmask scan.
/// - Skips contiguous `clean`, lazy-deferred `PAGE_NOACCESS`, and confirmed
///   `placeholder_absent` intervals without per-block `VirtualQuery`.
/// - Marks already-`protected` blocks `clean` in `O(dirty)` without redundant
///   `VirtualProtect` syscalls, preserving `VICTORY_GUARD_HOT` (`hot > 0`).
/// - Coalesces contiguous unprotected runs into batched `VirtualProtect` calls.
#[allow(clippy::too_many_arguments)]
fn guard_locked(
    g: &Guards,
    first: usize,
    last: usize,
    hot: u64,
    frame: u64,
    skip_placeholders: bool,
    coalesce: bool,
    alias_batch: bool,
) -> (Vec<usize>, Vec<(usize, usize, u64)>) {
    const { assert!(BLOCK == PAGE) };
    if first >= last || g.clean.covers(first, last) {
        return (Vec::new(), Vec::new());
    }
    let lazy_pages = crate::shadow::lazy_deferred_pages_in_range(first, last);
    let mut written = Vec::new();
    let mut epochs: Vec<(usize, usize, u64)> = Vec::new();
    let mut run: Option<(usize, usize)> = None;
    let mut uncommitted_until = first;
    let mut next_lazy = 0usize;
    let mut cursor = first;

    while cursor < last {
        // 1. Skip an entire contiguous clean interval in one step.
        if let Some(clean_end) = g.clean.run_end_in(cursor, last) {
            if let Some((rf, rt)) = run.take() {
                protect(g, rf, rt, coalesce, alias_batch, &mut epochs);
            }
            cursor = clean_end;
            continue;
        }

        // 1b. Skip any contiguous lazy-deferred `PAGE_NOACCESS` interval.
        while next_lazy < lazy_pages.len() && lazy_pages[next_lazy] < cursor {
            next_lazy += 1;
        }
        if next_lazy < lazy_pages.len() && lazy_pages[next_lazy] == cursor {
            if let Some((rf, rt)) = run.take() {
                protect(g, rf, rt, coalesce, alias_batch, &mut epochs);
            }
            let mut lazy_end = cursor + 1;
            next_lazy += 1;
            while next_lazy < lazy_pages.len() && lazy_pages[next_lazy] == lazy_end {
                lazy_end += 1;
                next_lazy += 1;
            }
            cursor = lazy_end.min(last);
            continue;
        }

        // 2. Skip an entire contiguous absent interval where valid.
        if let Some(absent_end) = g.absent.run_end_in(cursor, last) {
            let mut seg_end = absent_end;
            if next_lazy < lazy_pages.len() {
                seg_end = seg_end.min(lazy_pages[next_lazy]);
            }
            if skip_placeholders
                && let Some(ph_end) = g.placeholder_absent.run_end_in(cursor, seg_end)
            {
                if let Some((rf, rt)) = run.take() {
                    protect(g, rf, rt, coalesce, alias_batch, &mut epochs);
                }
                cursor = ph_end;
                continue;
            }
            // Ordinary commits need no runtime mapping notification. Reuse
            // this asking's query for the following blocks in the same region.
            if cursor >= uncommitted_until {
                uncommitted_until = uncommitted_end(cursor);
            }
            if cursor < uncommitted_until {
                if let Some((rf, rt)) = run.take() {
                    protect(g, rf, rt, coalesce, alias_batch, &mut epochs);
                }
                cursor = uncommitted_until.min(seg_end);
                continue;
            }
            // Pages appeared at `cursor` without a runtime mapping notification.
            g.absent.remove(cursor);
            g.placeholder_absent.remove(cursor);
            absent_set_range(cursor, cursor + 1, ABSENT_NONE);
        }

        // 3. Dirty/unguarded span starting at `cursor` up to the next clean, lazy, or absent boundary.
        let mut dirty_end = last;
        if let Some(s) = g.clean.first_in(cursor, dirty_end) {
            dirty_end = dirty_end.min(s);
        }
        if next_lazy < lazy_pages.len() {
            dirty_end = dirty_end.min(lazy_pages[next_lazy]);
        }
        if let Some(s) = g.absent.first_in(cursor, dirty_end) {
            dirty_end = dirty_end.min(s);
        }
        written.extend(cursor..dirty_end);

        let mut d = cursor;
        while d < dirty_end {
            if let Some(prot_end) = g.protected.run_end_in(d, dirty_end) {
                // Written through the runtime or guarded via an alias view, still read-only:
                // mark clean in O(dirty) without re-calling VirtualProtect.
                let clean_to = prot_end;
                if let Some((rf, rt)) = run.take() {
                    protect(g, rf, rt, coalesce, alias_batch, &mut epochs);
                }
                g.clean.insert_range(d, clean_to);
                let ep = clean_set_range(d, clean_to, true);
                if ep != 0 {
                    epochs.push((d, clean_to, ep));
                }
                d = clean_to;
            } else {
                let unprot_to = g.protected.first_in(d, dirty_end).unwrap_or(dirty_end);
                if hot > 0 {
                    for blk in d..unprot_to {
                        if stays_open(g, blk, frame, hot) {
                            if let Some((rf, rt)) = run.take() {
                                protect(g, rf, rt, coalesce, alias_batch, &mut epochs);
                            }
                        } else {
                            match &mut run {
                                Some((_, r_to)) if *r_to == blk => *r_to = blk + 1,
                                Some((r_from, r_to)) => {
                                    let (rf, rt) = (*r_from, *r_to);
                                    run = Some((blk, blk + 1));
                                    protect(g, rf, rt, coalesce, alias_batch, &mut epochs);
                                }
                                None => run = Some((blk, blk + 1)),
                            }
                        }
                    }
                } else {
                    match &mut run {
                        Some((_, r_to)) if *r_to == d => *r_to = unprot_to,
                        Some((r_from, r_to)) => {
                            let (rf, rt) = (*r_from, *r_to);
                            run = Some((d, unprot_to));
                            protect(g, rf, rt, coalesce, alias_batch, &mut epochs);
                        }
                        None => run = Some((d, unprot_to)),
                    }
                }
                d = unprot_to;
            }
        }
        cursor = dirty_end;
    }

    if let Some((rf, rt)) = run.take() {
        protect(g, rf, rt, coalesce, alias_batch, &mut epochs);
    }
    (written, epochs)
}

/// Guard walk for blocks `[first, last)` under shared `GUARDS.read()`
/// (`VICTORY_PORT_GUARD=1`): per-block [`Claims`] serialize mutations on the
/// same block without blocking unrelated write faults or refreshes. Any block
/// whose claim cannot be acquired immediately is left unguarded (already
/// reported written, retried on the next asking).
#[allow(clippy::too_many_arguments)]
fn guard_shared(
    first: usize,
    last: usize,
    hot: u64,
    frame: u64,
    skip_placeholders: bool,
    coalesce: bool,
    alias_batch: bool,
) -> (Vec<usize>, Vec<(usize, usize, u64)>) {
    const { assert!(BLOCK == PAGE) };
    port6_count(P6_GUARDS, 1);
    count(3, 1);
    let g = guards_read();
    if first >= last || g.clean.covers(first, last) {
        return (Vec::new(), Vec::new());
    }
    let lazy_pages = crate::shadow::lazy_deferred_pages_in_range(first, last);
    let mut written = Vec::new();
    let mut epochs: Vec<(usize, usize, u64)> = Vec::new();
    let mut run: Option<(usize, usize)> = None;
    let mut uncommitted_until = first;
    let mut next_lazy = 0usize;
    let mut cursor = first;

    while cursor < last {
        if let Some(clean_end) = g.clean.run_end_in(cursor, last) {
            if let Some((rf, rt)) = run.take() {
                protect_claimed(&g, rf, rt, coalesce, alias_batch, &mut epochs);
            }
            cursor = clean_end;
            continue;
        }

        while next_lazy < lazy_pages.len() && lazy_pages[next_lazy] < cursor {
            next_lazy += 1;
        }
        if next_lazy < lazy_pages.len() && lazy_pages[next_lazy] == cursor {
            if let Some((rf, rt)) = run.take() {
                protect_claimed(&g, rf, rt, coalesce, alias_batch, &mut epochs);
            }
            let mut lazy_end = cursor + 1;
            next_lazy += 1;
            while next_lazy < lazy_pages.len() && lazy_pages[next_lazy] == lazy_end {
                lazy_end += 1;
                next_lazy += 1;
            }
            cursor = lazy_end.min(last);
            continue;
        }

        if let Some(absent_end) = g.absent.run_end_in(cursor, last) {
            let mut seg_end = absent_end;
            if next_lazy < lazy_pages.len() {
                seg_end = seg_end.min(lazy_pages[next_lazy]);
            }
            if skip_placeholders
                && let Some(ph_end) = g.placeholder_absent.run_end_in(cursor, seg_end)
            {
                if let Some((rf, rt)) = run.take() {
                    protect_claimed(&g, rf, rt, coalesce, alias_batch, &mut epochs);
                }
                cursor = ph_end;
                continue;
            }
            if cursor >= uncommitted_until {
                uncommitted_until = uncommitted_end(cursor);
            }
            if cursor < uncommitted_until {
                if let Some((rf, rt)) = run.take() {
                    protect_claimed(&g, rf, rt, coalesce, alias_batch, &mut epochs);
                }
                cursor = uncommitted_until.min(seg_end);
                continue;
            }
            if let Some((rf, rt)) = run.take() {
                protect_claimed(&g, rf, rt, coalesce, alias_batch, &mut epochs);
            }
            let mut claim = Claims::none();
            if claim.try_one(cursor) {
                g.absent.remove(cursor);
                g.placeholder_absent.remove(cursor);
                absent_set_range(cursor, cursor + 1, ABSENT_NONE);
            } else {
                written.push(cursor);
                cursor += 1;
                continue;
            }
        }

        let mut dirty_end = last;
        if let Some(s) = g.clean.first_in(cursor, dirty_end) {
            dirty_end = dirty_end.min(s);
        }
        if next_lazy < lazy_pages.len() {
            dirty_end = dirty_end.min(lazy_pages[next_lazy]);
        }
        if let Some(s) = g.absent.first_in(cursor, dirty_end) {
            dirty_end = dirty_end.min(s);
        }
        written.extend(cursor..dirty_end);

        let mut d = cursor;
        while d < dirty_end {
            if let Some(prot_end) = g.protected.run_end_in(d, dirty_end) {
                if let Some((rf, rt)) = run.take() {
                    protect_claimed(&g, rf, rt, coalesce, alias_batch, &mut epochs);
                }
                for blk in d..prot_end {
                    let mut claims = Claims::none();
                    if claims.try_one(blk) && g.protected.contains(blk) && !g.clean.contains(blk) {
                        let uncovered = if view_guard() {
                            cover_aliases_claimed(
                                &g,
                                blk,
                                blk + 1,
                                coalesce,
                                alias_batch,
                                &mut claims,
                            )
                        } else {
                            Vec::new()
                        };
                        if uncovered.is_empty() {
                            g.clean.insert(blk);
                            let ep = clean_set(blk, true);
                            if ep != 0 {
                                epochs.push((blk, blk + 1, ep));
                            }
                        }
                    }
                }
                d = prot_end;
            } else {
                let unprot_to = g.protected.first_in(d, dirty_end).unwrap_or(dirty_end);
                if hot > 0 {
                    for blk in d..unprot_to {
                        if stays_open(&g, blk, frame, hot) {
                            if let Some((rf, rt)) = run.take() {
                                protect_claimed(&g, rf, rt, coalesce, alias_batch, &mut epochs);
                            }
                        } else {
                            match &mut run {
                                Some((_, r_to)) if *r_to == blk => *r_to = blk + 1,
                                Some((r_from, r_to)) => {
                                    let (rf, rt) = (*r_from, *r_to);
                                    run = Some((blk, blk + 1));
                                    protect_claimed(&g, rf, rt, coalesce, alias_batch, &mut epochs);
                                }
                                None => run = Some((blk, blk + 1)),
                            }
                        }
                    }
                } else {
                    match &mut run {
                        Some((_, r_to)) if *r_to == d => *r_to = unprot_to,
                        Some((r_from, r_to)) => {
                            let (rf, rt) = (*r_from, *r_to);
                            run = Some((d, unprot_to));
                            protect_claimed(&g, rf, rt, coalesce, alias_batch, &mut epochs);
                        }
                        None => run = Some((d, unprot_to)),
                    }
                }
                d = unprot_to;
            }
        }
        cursor = dirty_end;
    }

    if let Some((rf, rt)) = run.take() {
        protect_claimed(&g, rf, rt, coalesce, alias_batch, &mut epochs);
    }
    (written, epochs)
}

/// [`protect`] of `[from, to)` under shared `GUARDS.read()`: claims every
/// free block via `try_one` and protects each claimed run together with its
/// alias views (leaving any contended block unguarded for the next asking).
fn protect_claimed(
    g: &Guards,
    from: usize,
    to: usize,
    coalesce: bool,
    alias_batch: bool,
    epochs: &mut Vec<(usize, usize, u64)>,
) {
    let mut claims = Claims::none();
    let mut start = None;
    for block in from..to {
        if claims.try_one(block) {
            if g.clean.contains(block) || g.absent.contains(block) {
                if let Some(s) = start.take() {
                    protect_run_claimed(g, s, block, coalesce, alias_batch, epochs, &mut claims);
                }
            } else {
                start.get_or_insert(block);
            }
        } else if let Some(s) = start.take() {
            protect_run_claimed(g, s, block, coalesce, alias_batch, epochs, &mut claims);
        }
    }
    if let Some(s) = start {
        protect_run_claimed(g, s, to, coalesce, alias_batch, epochs, &mut claims);
    }
}

/// Protects claimed blocks `[from, to)` under shared `GUARDS.read()`, covers
/// their alias views while holding claims, and marks only fully covered runs
/// clean (so no concurrent lock-free reader ever sees `CLEAN` set before all
/// alias views are read-only).
fn protect_run_claimed(
    g: &Guards,
    from: usize,
    to: usize,
    coalesce: bool,
    alias_batch: bool,
    epochs: &mut Vec<(usize, usize, u64)>,
    claims: &mut Claims,
) {
    let prot_runs = protect_here_shared(g, from, to, coalesce);
    for (pf, pt) in prot_runs {
        let mut uncovered = if view_guard() {
            cover_aliases_claimed(g, pf, pt, coalesce, alias_batch, claims)
        } else {
            Vec::new()
        };
        if uncovered.is_empty() {
            g.clean.insert_range(pf, pt);
            let ep = clean_set_range(pf, pt, true);
            if ep != 0 {
                epochs.push((pf, pt, ep));
            }
        } else {
            uncovered.sort_unstable();
            uncovered.dedup();
            let mut s = pf;
            for u in uncovered {
                if u >= pf && u < pt {
                    if s < u {
                        g.clean.insert_range(s, u);
                        let ep = clean_set_range(s, u, true);
                        if ep != 0 {
                            epochs.push((s, u, ep));
                        }
                    }
                    s = u + 1;
                }
            }
            if s < pt {
                g.clean.insert_range(s, pt);
                let ep = clean_set_range(s, pt, true);
                if ep != 0 {
                    epochs.push((s, pt, ep));
                }
            }
        }
    }
}

/// Covers alias views for protected blocks `[from, to)` under shared
/// `GUARDS.read()` + `claims`. Returns the blocks in `[from, to)` whose
/// aliases could not all be claimed/protected and must remain non-clean.
fn cover_aliases_claimed(
    g: &Guards,
    from: usize,
    to: usize,
    coalesce: bool,
    alias_batch: bool,
    claims: &mut Claims,
) -> Vec<usize> {
    if alias_batch {
        let (sources, all_aliased, peers) =
            crate::inplace::aliases_in_range(from.saturating_mul(BLOCK), to.saturating_mul(BLOCK));
        if sources == 0 {
            return Vec::new();
        }
        let mut all_claimed = true;
        for &p in &peers {
            if !claims.holds(p) && !claims.try_one(p) {
                all_claimed = false;
            }
        }
        if all_claimed {
            #[cfg(feature = "diagnostics")]
            ALIASES[0].fetch_add(sources, std::sync::atomic::Ordering::Relaxed);
            g.aliased.insert_sorted_blocks(&all_aliased);
            let mut i = 0;
            while i < peers.len() {
                let b = peers[i];
                if g.protected.contains(b) {
                    i += 1;
                    continue;
                }
                let run_start = b;
                let mut run_end = b + 1;
                i += 1;
                while i < peers.len() && peers[i] == run_end && !g.protected.contains(peers[i]) {
                    run_end += 1;
                    i += 1;
                }
                protect_alias_run(g, run_start, run_end, coalesce);
            }
            if peers.iter().all(|&p| g.protected.contains(p)) {
                return Vec::new();
            }
        }
    }
    let mut uncovered = Vec::new();
    for (base, bytes) in
        crate::inplace::views_between(from.saturating_mul(BLOCK), to.saturating_mul(BLOCK))
    {
        let first_block = (base / BLOCK).max(from);
        let last_block = base.saturating_add(bytes).div_ceil(BLOCK).min(to);
        for block in first_block..last_block {
            let pages = crate::inplace::pages_of(block * BLOCK);
            if pages.len() < 2 || pages.len() > MANY_VIEWS {
                continue;
            }
            #[cfg(feature = "diagnostics")]
            ALIASES[0].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            g.aliased.insert(block);
            let mut covered = true;
            for alias in pages {
                let ab = alias / BLOCK;
                if ab == block {
                    continue;
                }
                if !claims.holds(ab) && !claims.try_one(ab) {
                    covered = false;
                    continue;
                }
                g.aliased.insert(ab);
                if !g.protected.contains(ab) {
                    protect_alias_run(g, ab, ab + 1, coalesce);
                }
                if !g.protected.contains(ab) {
                    covered = false;
                }
            }
            if !covered {
                uncovered.push(block);
            }
        }
    }
    uncovered
}

/// [`protect_here`] under shared `GUARDS.read()`: makes `[from, to)`
/// `PAGE_READONLY` and records `g.protected` / `PROTECTED` / `VIEWS` (and
/// uncommitted holes in `g.absent`), returning the committed sub-runs that
/// succeeded WITHOUT marking them clean yet.
fn protect_here_shared(g: &Guards, from: usize, to: usize, coalesce: bool) -> Vec<(usize, usize)> {
    use windows_sys::Win32::System::Memory::{
        MEM_FREE, MEM_RESERVE, MEMORY_BASIC_INFORMATION, PAGE_READONLY,
    };
    let mut out = Vec::new();
    let record = |g: &Guards, a: usize, b: usize, out: &mut Vec<(usize, usize)>| {
        note_protected(g, a, b, false);
        out.push((a, b));
    };
    count(1, 1);
    count(2, (to - from) as u64);
    if !coalesce {
        let mut block = from;
        while block < to {
            if os_virtual_protect(block, block + 1, PAGE_READONLY) {
                record(g, block, block + 1, &mut out);
                block += 1;
            } else {
                block = handle_protect_failure(g, block, to).max(block + 1);
            }
        }
        return out;
    }
    if os_virtual_protect(from, to, PAGE_READONLY) {
        record(g, from, to, &mut out);
        return out;
    }
    if to <= from + 1 {
        let _ = handle_protect_failure(g, from, to);
        return out;
    }
    let mut at = from;
    while at < to {
        let Some(addr) = at.checked_mul(BLOCK) else {
            break;
        };
        // SAFETY: plain data filled in by the query.
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        let n = os_virtual_query(addr, &mut info);
        if n == 0 {
            at += 1;
            continue;
        }
        let region_end =
            ((info.BaseAddress as usize).saturating_add(info.RegionSize) / BLOCK).min(to);
        if matches!(info.State, MEM_RESERVE | MEM_FREE) {
            if region_end > at {
                note_absent_span(g, at, region_end);
                at = region_end;
            } else {
                at += 1;
            }
            continue;
        }
        let sub_to = region_end.max(at + 1);
        if os_virtual_protect(at, sub_to, PAGE_READONLY) {
            record(g, at, sub_to, &mut out);
            at = sub_to;
        } else if sub_to > at + 1 {
            let mut block = at;
            while block < sub_to {
                if os_virtual_protect(block, block + 1, PAGE_READONLY) {
                    record(g, block, block + 1, &mut out);
                    block += 1;
                } else {
                    block = handle_protect_failure(g, block, sub_to).max(block + 1);
                }
            }
            at = sub_to;
        } else {
            at = handle_protect_failure(g, at, to).max(at + 1);
        }
    }
    out
}

/// Whether `[start, end)` is view memory. The title's views lie in
/// placeholders, mapped and unmapped block by block as it streams, so a
/// range can begin in a block that is not mapped right now: the first
/// pages of the range that are there decide. A range with nothing there
/// counts as a view (its blocks are noted absent, and looked at again when
/// the runtime maps them).
///
/// Looking at the first address only took such a range for "not a view":
/// its copy was marked not watched for good and copied whole at every
/// refresh, around its unmapped pages (run 54: 300 such ranges, 570 MB
/// copied a frame for them, 255 copies a frame reading around holes).
/// `VICTORY_GUARD_HOLES=0` looks at the first address only, as before.
fn is_view(start: usize, end: usize) -> bool {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_MAPPED, MEMORY_BASIC_INFORMATION, VirtualQuery,
    };
    static HOLES: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let holes =
        *HOLES.get_or_init(|| std::env::var_os("VICTORY_GUARD_HOLES").is_none_or(|v| v != "0"));
    let mut at = start;
    loop {
        // SAFETY: plain data filled in by the query.
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: a valid out pointer of the right size.
        let n = unsafe { VirtualQuery(at as *const _, &mut info, size_of_val(&info)) };
        if n == 0 {
            return false;
        }
        if info.Type == MEM_MAPPED {
            return true;
        }
        // Committed memory that is no view (the title's heaps, ours), or
        // the answer as it was before.
        if info.State == MEM_COMMIT || !holes {
            return false;
        }
        // A hole (a placeholder, or free): on to what follows it.
        let next = (info.BaseAddress as usize).saturating_add(info.RegionSize);
        if next <= at || next >= end {
            return true;
        }
        at = next;
    }
}

#[cfg(test)]
thread_local! {
    static OS_VIRTUAL_PROTECT_CALLS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static OS_VIRTUAL_QUERY_CALLS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Changes page protection for contiguous blocks `[from, to)` in one syscall.
fn os_virtual_protect(from: usize, to: usize, protect_flags: u32) -> bool {
    use windows_sys::Win32::System::Memory::VirtualProtect;
    if from >= to {
        return true;
    }
    let Some(addr) = from.checked_mul(BLOCK) else {
        return false;
    };
    let Some(len) = (to - from).checked_mul(BLOCK) else {
        return false;
    };
    #[cfg(test)]
    OS_VIRTUAL_PROTECT_CALLS.with(|c| c.set(c.get() + 1));
    let mut old = 0;
    // SAFETY: title view memory; only its page protection changes.
    unsafe { VirtualProtect(addr as *const _, len, protect_flags, &mut old) != 0 }
}

/// Queries OS virtual memory metadata for `addr`.
fn os_virtual_query(
    addr: usize,
    info: &mut windows_sys::Win32::System::Memory::MEMORY_BASIC_INFORMATION,
) -> usize {
    use windows_sys::Win32::System::Memory::VirtualQuery;
    #[cfg(test)]
    OS_VIRTUAL_QUERY_CALLS.with(|c| c.set(c.get() + 1));
    // SAFETY: a valid out pointer of the right size.
    unsafe { VirtualQuery(addr as *const _, info, size_of_val(info)) }
}

/// Makes blocks `[from, to)` read-only and notes them guarded and clean;
/// splits along OS VM regions if they span views or uncommitted holes,
/// noting uncommitted spans absent without per-block syscalls. The same
/// physical pages at every other view of them are coalesced into contiguous
/// runs and made read-only too.
/// `VICTORY_VIEW_GUARD=0` leaves the other views as they are, as before.
fn protect(
    g: &Guards,
    from: usize,
    to: usize,
    coalesce: bool,
    alias_batch: bool,
    epochs: &mut Vec<(usize, usize, u64)>,
) {
    protect_here(g, from, to, coalesce, epochs);
    if !view_guard() {
        return;
    }
    if !alias_batch {
        for (base, bytes) in
            crate::inplace::views_between(from.saturating_mul(BLOCK), to.saturating_mul(BLOCK))
        {
            let first_block = (base / BLOCK).max(from);
            let last_block = base.saturating_add(bytes).div_ceil(BLOCK).min(to);
            for block in first_block..last_block {
                let pages = crate::inplace::pages_of(block * BLOCK);
                if pages.len() < 2 || pages.len() > MANY_VIEWS {
                    continue;
                }
                #[cfg(feature = "diagnostics")]
                ALIASES[0].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                for alias in pages {
                    let ab = alias / BLOCK;
                    g.aliased.insert_range(ab, ab + 1);
                    if ab != block && !g.protected.contains(ab) {
                        protect_alias_run(g, ab, ab + 1, coalesce);
                    }
                }
            }
        }
        return;
    }
    let (sources, all_aliased, peers) =
        crate::inplace::aliases_in_range(from.saturating_mul(BLOCK), to.saturating_mul(BLOCK));
    if sources == 0 {
        return;
    }
    #[cfg(feature = "diagnostics")]
    ALIASES[0].fetch_add(sources, std::sync::atomic::Ordering::Relaxed);
    g.aliased.insert_sorted_blocks(&all_aliased);
    // Coalesce unprotected peer alias blocks into contiguous runs.
    let mut i = 0;
    while i < peers.len() {
        let b = peers[i];
        if g.protected.contains(b) {
            i += 1;
            continue;
        }
        let run_start = b;
        let mut run_end = b + 1;
        i += 1;
        while i < peers.len() && peers[i] == run_end && !g.protected.contains(peers[i]) {
            run_end += 1;
            i += 1;
        }
        protect_alias_run(g, run_start, run_end, coalesce);
    }
}

/// Views of one page past which its views are not guarded together.
pub(crate) const MANY_VIEWS: usize = 64;

/// Whether all the views of a guarded page are guarded together
/// (`VICTORY_VIEW_GUARD`).
fn view_guard() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_VIEW_GUARD").is_none_or(|v| v != "0"))
}

/// Diagnostics: blocks guarded together with their other views, blocks
/// opened because another view of theirs was written, and blocks opened
/// because a view of theirs was mapped; since the start.
#[cfg(feature = "diagnostics")]
pub(crate) static ALIASES: [std::sync::atomic::AtomicU64; 3] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 3];

/// Records `[from, to)` as guarded in `g`, lock-free [`PROTECTED`] and [`VIEWS`]
/// (and `clean` + lock-free [`CLEAN`] when `mark_clean`), clearing any
/// absent/placeholder state so `protected ∩ absent == ∅` holds by construction.
/// Returns the monotonic [`GUARD_EPOCH`] stamped when `mark_clean` is true (0 otherwise).
fn note_protected(g: &Guards, from: usize, to: usize, mark_clean: bool) -> u64 {
    if g.absent.intersects(from, to) {
        g.absent.remove_range(from, to);
        g.placeholder_absent.remove_range(from, to);
        absent_set_range(from, to, ABSENT_NONE);
    }
    g.protected.insert_range(from, to);
    PROTECTED.set_range(from, to);
    VIEWS.set_range(from, to);
    if mark_clean {
        g.clean.insert_range(from, to);
        clean_set_range(from, to, true)
    } else {
        0
    }
}

/// Makes contiguous alias blocks `[from, to)` read-only in a single coalesced
/// `VirtualProtect` (splitting along OS VM regions if they span multiple views).
/// Does NOT mark them clean: copies at the alias view have not refreshed yet.
fn protect_alias_run(g: &Guards, from: usize, to: usize, coalesce: bool) {
    use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_READONLY};
    if !coalesce {
        for block in from..to {
            if os_virtual_protect(block, block + 1, PAGE_READONLY) {
                note_protected(g, block, block + 1, false);
            }
        }
        return;
    }
    if os_virtual_protect(from, to, PAGE_READONLY) {
        note_protected(g, from, to, false);
        return;
    }
    if to <= from + 1 {
        return;
    }
    let mut at = from;
    while at < to {
        let Some(addr) = at.checked_mul(BLOCK) else {
            break;
        };
        // SAFETY: plain data filled in by the query.
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        let n = os_virtual_query(addr, &mut info);
        if n == 0 {
            at += 1;
            continue;
        }
        let region_end = (info.BaseAddress as usize).saturating_add(info.RegionSize) / BLOCK;
        if info.State != MEM_COMMIT {
            at = region_end.min(to).max(at + 1);
            continue;
        }
        let sub_to = region_end.min(to).max(at + 1);
        if os_virtual_protect(at, sub_to, PAGE_READONLY) {
            note_protected(g, at, sub_to, false);
        } else if sub_to > at + 1 {
            for block in at..sub_to {
                if os_virtual_protect(block, block + 1, PAGE_READONLY) {
                    note_protected(g, block, block + 1, false);
                }
            }
        }
        at = sub_to;
    }
}

/// Records an uncommitted span `[from, to)` as absent (and placeholder-absent
/// for every sub-region where `NtQueryVirtualMemory` confirms a placeholder
/// reservation, advancing by region size and updating lock-free [`ABSENT`]).
fn note_absent_span(g: &Guards, from: usize, to: usize) {
    if from >= to {
        return;
    }
    g.absent.insert_range(from, to);
    let mut b = from;
    while b < to {
        let (is_ph, region_end) = placeholder_region_info(b);
        let next = region_end.min(to).max(b + 1);
        if is_ph {
            g.placeholder_absent.insert_range(b, next);
            absent_set_range(b, next, ABSENT_PLACEHOLDER);
        } else {
            absent_set_range(b, next, ABSENT_ORDINARY);
        }
        b = next;
    }
}

/// Handles a single block that failed `VirtualProtect(..., PAGE_READONLY)`.
/// Returns the exclusive end block handled (at least `block + 1`) so callers
/// advance past any whole uncommitted span recorded in `g.absent`.
fn handle_protect_failure(g: &Guards, block: usize, limit: usize) -> usize {
    let end = uncommitted_end(block).min(limit);
    if block < end {
        note_absent_span(g, block, end);
        end
    } else {
        block + 1
    }
}

/// [`protect`] for one range, without the other views of its pages.
/// Tries a single coalesced `VirtualProtect` first; on failure (VAD boundary
/// or uncommitted hole inside `[from, to)`), walks by OS VM regions so each
/// committed view is protected in one syscall and each uncommitted span is
/// skipped without failing `VirtualProtect` calls.
fn protect_here(
    g: &Guards,
    from: usize,
    to: usize,
    coalesce: bool,
    epochs: &mut Vec<(usize, usize, u64)>,
) {
    use windows_sys::Win32::System::Memory::{
        MEM_FREE, MEM_RESERVE, MEMORY_BASIC_INFORMATION, PAGE_READONLY,
    };
    let record = |g: &Guards, a: usize, b: usize, epochs: &mut Vec<(usize, usize, u64)>| {
        let ep = note_protected(g, a, b, true);
        if ep != 0 {
            epochs.push((a, b, ep));
        }
    };
    count(1, 1);
    count(2, (to - from) as u64);
    if !coalesce {
        let mut block = from;
        while block < to {
            if os_virtual_protect(block, block + 1, PAGE_READONLY) {
                record(g, block, block + 1, epochs);
                block += 1;
            } else {
                block = handle_protect_failure(g, block, to).max(block + 1);
            }
        }
        return;
    }
    if os_virtual_protect(from, to, PAGE_READONLY) {
        record(g, from, to, epochs);
        return;
    }
    if to <= from + 1 {
        let _ = handle_protect_failure(g, from, to);
        return;
    }
    let mut at = from;
    while at < to {
        let Some(addr) = at.checked_mul(BLOCK) else {
            break;
        };
        // SAFETY: plain data filled in by the query.
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        let n = os_virtual_query(addr, &mut info);
        if n == 0 {
            at += 1;
            continue;
        }
        let region_end =
            ((info.BaseAddress as usize).saturating_add(info.RegionSize) / BLOCK).min(to);
        if matches!(info.State, MEM_RESERVE | MEM_FREE) {
            if region_end > at {
                note_absent_span(g, at, region_end);
                at = region_end;
            } else {
                // Partially committed block (starts reserved, later 4 KiB page committed):
                // cannot be guarded and is not wholly absent.
                at += 1;
            }
            continue;
        }
        let sub_to = region_end.max(at + 1);
        if os_virtual_protect(at, sub_to, PAGE_READONLY) {
            record(g, at, sub_to, epochs);
            at = sub_to;
        } else if sub_to > at + 1 {
            let mut block = at;
            while block < sub_to {
                if os_virtual_protect(block, block + 1, PAGE_READONLY) {
                    record(g, block, block + 1, epochs);
                    block += 1;
                } else {
                    block = handle_protect_failure(g, block, sub_to).max(block + 1);
                }
            }
            at = sub_to;
        } else {
            at = handle_protect_failure(g, at, to).max(at + 1);
        }
    }
}

/// The end (exclusive) of the whole uncommitted blocks starting at `block`.
fn uncommitted_end(block: usize) -> usize {
    use windows_sys::Win32::System::Memory::{MEM_FREE, MEM_RESERVE, MEMORY_BASIC_INFORMATION};
    let Some(addr) = block.checked_mul(BLOCK) else {
        return block;
    };
    // SAFETY: plain data filled in by the query.
    let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    let n = os_virtual_query(addr, &mut info);
    if n != 0 && matches!(info.State, MEM_RESERVE | MEM_FREE) {
        (info.BaseAddress as usize).saturating_add(info.RegionSize) / BLOCK
    } else {
        block
    }
}

/// Restores `PAGE_READWRITE` for contiguous blocks `[from, to)` in a single
/// coalesced `VirtualProtect`, splitting along OS VM regions if the run spans
/// multiple views and skipping uncommitted gaps without per-block failures.
#[cfg_attr(not(test), allow(dead_code))]
fn unprotect_run_os(from: usize, to: usize) {
    unprotect_run_os_with(from, to, watch_coalesce());
}

fn unprotect_run_os_with(from: usize, to: usize, coalesce: bool) {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_READWRITE,
    };
    if !coalesce {
        for block in from..to {
            os_virtual_protect(block, block + 1, PAGE_READWRITE);
        }
        return;
    }
    if os_virtual_protect(from, to, PAGE_READWRITE) || to <= from + 1 {
        return;
    }
    let mut at = from;
    while at < to {
        let Some(addr) = at.checked_mul(BLOCK) else {
            break;
        };
        // SAFETY: plain data filled in by the query.
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        let n = os_virtual_query(addr, &mut info);
        if n == 0 {
            at += 1;
            continue;
        }
        let region_end = (info.BaseAddress as usize).saturating_add(info.RegionSize) / BLOCK;
        let sub_to = region_end.min(to).max(at + 1);
        if info.State != MEM_COMMIT {
            at = sub_to;
            continue;
        }
        if !os_virtual_protect(at, sub_to, PAGE_READWRITE) && sub_to > at + 1 {
            for block in at..sub_to {
                os_virtual_protect(block, block + 1, PAGE_READWRITE);
            }
        }
        at = sub_to;
    }
}

/// Gives write access back to the guarded blocks of `[start, end)` and
/// notes them written; true if any was guarded. The same physical pages at
/// every other view of them are opened too: the write that faulted (or is
/// ours, `crate::host::copy_mapped`) goes through one view, and every
/// guard over the pages must copy them again.
fn open(g: &Guards, start: usize, end: usize) -> bool {
    open_with(g, start, end, watch_coalesce(), watch_alias_batch())
}

fn open_with(g: &Guards, start: usize, end: usize, coalesce: bool, alias_batch: bool) -> bool {
    if start >= end {
        return false;
    }
    let (first, last) = (start / BLOCK, end.div_ceil(BLOCK));
    // Zero-allocation fast path for the common VEH single-block unaliased fault.
    if last == first + 1 && !g.aliased.contains(first) {
        if !g.protected.remove(first) {
            return false;
        }
        PROTECTED.clear(first);
        g.clean.remove(first);
        clean_set(first, false);
        published_set(first, 0);
        os_virtual_protect(
            first,
            last,
            windows_sys::Win32::System::Memory::PAGE_READWRITE,
        );
        return true;
    }
    let aliased_runs = if g.aliased.is_empty() {
        Vec::new()
    } else {
        g.aliased.take_range(first, last)
    };
    if !alias_batch {
        let mut alias_blocks = Vec::new();
        for (rf, rt) in aliased_runs {
            for block in rf..rt {
                for alias in crate::inplace::pages_of(block * BLOCK) {
                    alias_blocks.push(alias / BLOCK);
                }
            }
        }
        alias_blocks.sort_unstable();
        alias_blocks.dedup();
        // Clear lock-free CLEAN and PUBLISHED bits for ALL alias blocks BEFORE making
        // primary blocks writable.
        for &ab in &alias_blocks {
            clean_set(ab, false);
            published_set(ab, 0);
            g.aliased.remove(ab);
        }
        let any = open_here_with(g, start, end, coalesce);
        for ab in alias_blocks {
            if !(first..last).contains(&ab)
                && open_here_with(g, ab * BLOCK, (ab + 1) * BLOCK, coalesce)
            {
                #[cfg(feature = "diagnostics")]
                ALIASES[1].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
        return any;
    }
    let peer_blocks = if aliased_runs.is_empty() {
        Vec::new()
    } else {
        crate::inplace::aliases_for_blocks(&aliased_runs)
    };
    let primary_runs = g.protected.take_range(first, last);
    if !primary_runs.is_empty() {
        g.clean.remove_range(first, last);
    }
    // Clear lock-free PROTECTED, CLEAN, and PUBLISHED entries for BOTH primary and
    // all alias blocks BEFORE making any of them writable.
    for &(rf, rt) in &primary_runs {
        PROTECTED.clear_range(rf, rt);
        clean_set_range(rf, rt, false);
        published_clear_range(rf, rt);
    }
    let mut alias_to_unprotect = Vec::new();
    for &ab in &peer_blocks {
        g.aliased.remove(ab);
        if g.protected.remove(ab) {
            PROTECTED.clear(ab);
            g.clean.remove(ab);
            clean_set(ab, false);
            published_set(ab, 0);
            alias_to_unprotect.push(ab);
            #[cfg(feature = "diagnostics")]
            ALIASES[1].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }
    for &(rf, rt) in &primary_runs {
        unprotect_run_os_with(rf, rt, coalesce);
    }
    // Coalesce sorted alias blocks into contiguous runs.
    let mut i = 0;
    while i < alias_to_unprotect.len() {
        let run_start = alias_to_unprotect[i];
        let mut run_end = run_start + 1;
        i += 1;
        while i < alias_to_unprotect.len() && alias_to_unprotect[i] == run_end {
            run_end += 1;
            i += 1;
        }
        unprotect_run_os_with(run_start, run_end, coalesce);
    }
    !primary_runs.is_empty()
}

/// [`open`] for one range, without the other views of its pages.
fn open_here(g: &Guards, start: usize, end: usize) -> bool {
    open_here_with(g, start, end, watch_coalesce())
}

fn open_here_with(g: &Guards, start: usize, end: usize, coalesce: bool) -> bool {
    if start >= end {
        return false;
    }
    let (first, last) = (start / BLOCK, end.div_ceil(BLOCK));
    let runs = g.protected.take_range(first, last);
    if runs.is_empty() {
        return false;
    }
    g.clean.remove_range(first, last);
    for &(rf, rt) in &runs {
        PROTECTED.clear_range(rf, rt);
        clean_set_range(rf, rt, false);
        published_clear_range(rf, rt);
    }
    for &(rf, rt) in &runs {
        unprotect_run_os_with(rf, rt, coalesce);
    }
    true
}

/// Opens the guarded block at `at` for a write fault under shared
/// `GUARDS.read()` + sorted [`Claims`] (`VICTORY_PORT_GUARD=1`).
/// Returns `(opened_blocks, already_writable)`: when `opened_blocks == 0`,
/// `already_writable` is evaluated while still holding the block's claim so a
/// concurrent refresh cannot re-protect it before the fault handler checks
/// `writable(at)`.
fn open_faulted(at: usize) -> (usize, bool) {
    let block = at / BLOCK;
    let began = std::time::Instant::now();
    let g = guards_read();
    let waited = began.elapsed();
    port6_count(P6_FAULT_WAIT_NS, waited.as_nanos() as u64);
    // Zero-allocation fast path for the common single-block unaliased fault.
    {
        let _claim = ClaimOne::wait(block);
        if !g.protected.contains(block) {
            return (0, writable(at));
        }
        if !g.aliased.contains(block) {
            if !g.protected.remove(block) {
                return (0, writable(at));
            }
            PROTECTED.clear(block);
            g.clean.remove(block);
            clean_set(block, false);
            published_set(block, 0);
            os_virtual_protect(
                block,
                block + 1,
                windows_sys::Win32::System::Memory::PAGE_READWRITE,
            );
            port6_count(P6_FAULTS, 1);
            return (1, true);
        }
    }
    // Aliased block: release the single claim first and acquire all alias views'
    // claims in strictly ascending block order so no two faults can deadlock.
    let mut views: Vec<usize> = vec![block];
    loop {
        let _claims = Claims::wait_for(&views);
        if !g.protected.contains(block) {
            return (0, writable(at));
        }
        let given = if g.aliased.contains(block) {
            let mut now: Vec<usize> = crate::inplace::pages_of(block * BLOCK)
                .into_iter()
                .map(|p| p / BLOCK)
                .collect();
            now.push(block);
            now.sort_unstable();
            now.dedup();
            if now.iter().any(|b| views.binary_search(b).is_err()) {
                views = now;
                continue;
            }
            Some(now)
        } else {
            None
        };
        let opened = open_given(&g, block, given.as_deref());
        if opened != 0 {
            port6_count(P6_FAULTS, 1);
            return (opened, true);
        }
        return (0, writable(at));
    }
}

/// Opens `block` (and its sorted alias views `given`, if any) while holding
/// claims on all of them under shared `GUARDS.read()`.
fn open_given(g: &Guards, block: usize, given: Option<&[usize]>) -> usize {
    let coalesce = watch_coalesce();
    let Some(others) = given else {
        if !g.protected.remove(block) {
            return 0;
        }
        PROTECTED.clear(block);
        g.clean.remove(block);
        clean_set(block, false);
        published_set(block, 0);
        os_virtual_protect(
            block,
            block + 1,
            windows_sys::Win32::System::Memory::PAGE_READWRITE,
        );
        return 1;
    };
    let mut to_unprotect = Vec::with_capacity(others.len());
    let mut primary_opened = false;
    // Clear all lock-free bits across every view BEFORE making any view writable.
    for &b in others {
        g.aliased.remove(b);
        if g.protected.remove(b) {
            PROTECTED.clear(b);
            g.clean.remove(b);
            clean_set(b, false);
            published_set(b, 0);
            to_unprotect.push(b);
            if b == block {
                primary_opened = true;
            } else {
                #[cfg(feature = "diagnostics")]
                ALIASES[1].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
    }
    if !primary_opened && to_unprotect.is_empty() {
        return 0;
    }
    let mut i = 0;
    while i < to_unprotect.len() {
        let run_start = to_unprotect[i];
        let mut run_end = run_start + 1;
        i += 1;
        while i < to_unprotect.len() && to_unprotect[i] == run_end {
            run_end += 1;
            i += 1;
        }
        unprotect_run_os_with(run_start, run_end, coalesce);
    }
    to_unprotect.len()
}

/// Best-effort open-ahead for unaliased guarded blocks `[from, to)` under
/// shared `GUARDS.read()` (`VICTORY_PORT_GUARD=1` + `VICTORY_GUARD_OPEN_AHEAD>1`).
fn open_ahead_shared(from: usize, to: usize) {
    if from >= to {
        return;
    }
    let g = guards_read();
    let mut claims = Claims::none();
    let mut opened = Vec::new();
    for b in from..to {
        if !g.protected.contains(b) || g.aliased.contains(b) {
            continue;
        }
        if !claims.try_one(b) {
            continue;
        }
        if !g.aliased.contains(b) && g.protected.remove(b) {
            PROTECTED.clear(b);
            g.clean.remove(b);
            clean_set(b, false);
            published_set(b, 0);
            opened.push(b);
        }
    }
    let coalesce = watch_coalesce();
    let mut i = 0;
    while i < opened.len() {
        let run_start = opened[i];
        let mut run_end = run_start + 1;
        i += 1;
        while i < opened.len() && opened[i] == run_end {
            run_end += 1;
            i += 1;
        }
        unprotect_run_os_with(run_start, run_end, coalesce);
    }
}

/// Before an external physical-page write (e.g. DirectStorage `write_physical`
/// / `page_window`) mutates backing section pages shown at `[at, at + len)`:
/// flushes any lazy GPU-resident pages and holds a [`crate::shadow::LazyAccessHold`]
/// blocking concurrent relay re-arming until dropped.
pub fn writing_hold(at: usize, len: usize) -> crate::shadow::LazyAccessHold {
    crate::shadow::lazy_flush_and_hold(at, len)
}

/// Before our own write into title memory `[at, at + len)` through the
/// kernel (which does not fault into our handler): opens its guarded blocks.
/// The runtime calls it (as `kernel_will_write`) before the title's kernel
/// reads into its own memory.
pub fn writing(at: usize, len: usize) {
    // Flush any lazy GPU-resident deferred pages before a kernel write lands.
    crate::shadow::lazy_flush_range(at, len);
    // The length is the title's (`fread`'s size times count, saturated).
    let end = at.saturating_add(len);
    // (Neptune) Nothing guarded there (most of the copy-back's writes): looked at beside the
    // other readers in O(log K), and left at that.
    let guarded = if at < end {
        let g = guards_read();
        let (first, last) = (at / BLOCK, end.div_ceil(BLOCK));
        g.protected.intersects(first, last) || g.aliased.intersects(first, last)
    } else {
        false
    };
    if guarded {
        open(&guards(), at, end);
    }
    // A placed texture there changes with no descriptor written.
    crate::descriptor::host::remapped(at, len);
}

/// What `VICTORY_PAGE_WRITES` asks to be done with a write the guards do
/// not see once its bytes are there ([`title_memory_written`]), as bits:
/// 1, the guarded blocks under it are opened and count as written (host
/// copies of buffers and textures read through tables take them again);
/// 2, the placed textures picked by index that lie under it are due again;
/// 4, both also for reads to plain addresses when they have finished;
/// 8, for reads to plain addresses the textures alone (bit 2's look).
/// Unset: bits 1, 2 and 8. `guards`: bit 1 alone (the default until
/// 2026-11-26). `0`: nothing, counted only (the default until run 199).
/// `1`: bits 1 and 2. `slots`: bit 2 alone. `all`: everything.
///
/// Bits 2 and 8 by default (gfx41, the black faces of the opening
/// cutscene): a read makes the placed textures under it due before it
/// begins ([`writing`]), and the texture pass of its own thread
/// (`VICTORY_HEAP_WORKER=1`) uploads them while the read is under way,
/// from memory that does not hold the bytes yet; nothing asked again once
/// they were there. Run 345 at the cutscene: 282 slots of placed textures
/// uploaded before a DirectStorage write into their pages, "left as they
/// were", and 55,836 reads to plain addresses finished, "counted only".
/// Only slots uploaded before the bytes landed are made due again
/// (`HostHeap::texture_slots_page_written`): a slot still waiting for its
/// first upload takes the bytes that are there by then.
/// (Bit 1 by default: in every run 10,000 to 50,000 guarded blocks of host
/// copies lay under such writes and kept the bytes from before; runs 197
/// and 198 opened them, 9,766 and 10,796 blocks, with nothing worse for
/// it. Bit 2 makes texture slots due and is the one that flooded the
/// queue in an earlier form: still asked for by name.)
pub(crate) fn page_writes() -> u8 {
    static MODE: std::sync::OnceLock<u8> = std::sync::OnceLock::new();
    *MODE.get_or_init(|| {
        match std::env::var("VICTORY_PAGE_WRITES")
            .as_deref()
            .map(str::trim)
        {
            Ok("0") => 0,
            Ok("1") => 3,
            Ok("slots") => 2,
            Ok("guards") => 1,
            Ok("all") => 15,
            _ => 1 | 2 | 8,
        }
    })
}

/// Since the start: writes into physical pages through the runtime's own
/// views that landed at addresses the title has mapped (one for each run
/// of addresses); their bytes; guarded blocks that lay under them (host
/// copies that would otherwise keep the bytes from before); reads to
/// plain addresses that finished; of those, the ones passed to the texture
/// pass (bit 8 or 4 of [`page_writes`]).
pub(crate) static WRITTEN: [std::sync::atomic::AtomicU64; 5] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 5];

/// The bytes of a write the guards do not see are there: title memory
/// `[at, at + len)`. `pages`: DirectStorage wrote them into physical pages
/// through a view of the runtime's own, and this is an address where the
/// title has those pages mapped (no title address faulted, nothing was
/// mapped or unmapped; the shortcut this file's header names). Otherwise a
/// read to a plain address has finished, of which [`writing`] was told
/// before it began.
///
/// Upstream's c75fdba does the same for its trackers (its `Writing` marks
/// the pages once more when dropped; page destinations are reported at
/// each address showing them). Here it is counted always and acted on as
/// [`page_writes`] says. Called without the runtime's page-table lock;
/// takes the guards' lock, then, alone, the lock of the ranges kept for
/// the texture pass.
pub fn title_memory_written(at: usize, len: usize, pages: bool) {
    use std::sync::atomic::Ordering::Relaxed;
    if len == 0 {
        return;
    }
    reservation::completed_write(at, len);
    // Preserve DirectStorage/runtime physical-page writes over any deferred
    // GPU pages, writing back only the unwritten complement.
    crate::shadow::lazy_release_written_range(at, len);
    let mode = page_writes();
    let end = at.saturating_add(len);
    if pages {
        WRITTEN[0].fetch_add(1, Relaxed);
        WRITTEN[1].fetch_add(len as u64, Relaxed);
    } else {
        WRITTEN[3].fetch_add(1, Relaxed);
        if mode & (4 | 8) == 0 {
            return;
        }
        WRITTEN[4].fetch_add(1, Relaxed);
        if mode & 4 == 0 {
            // (Bit 8 alone: the placed textures under it, not the guards.)
            crate::descriptor::host::page_written(at, len);
            return;
        }
    }
    {
        let g = guards();
        let guarded = g.protected.count_in(at / BLOCK, end.div_ceil(BLOCK)) as u64;
        if pages {
            WRITTEN[2].fetch_add(guarded, Relaxed);
        }
        if mode & 1 != 0 && guarded != 0 {
            open(&g, at, end);
        }
    }
    // A placed texture there changed with no descriptor written.
    crate::descriptor::host::page_written(at, len);
}

/// The runtime mapped a view of the physical bytes `[offset, offset + len)`
/// of `section` (`crate::inplace::title_memory_view`): the view is
/// writable, so the same bytes guarded at another view must be checked
/// again — and are, the pages noted written, which also has the next check
/// guard the new view (`protect`).
pub(crate) fn views_mapped(section: usize, offset: u64, len: usize) {
    let ranges = crate::inplace::views_of_section(section, offset, len);
    if ranges.is_empty() {
        return;
    }
    // Always flush any lazy GPU-resident pages mapped to the same physical
    // section bytes BEFORE acquiring `guards()` (even when `!view_guard()`),
    // so newly mapped alias views immediately observe GPU bytes and cannot diverge.
    for &(at, n) in &ranges {
        crate::shadow::lazy_flush_range(at, n);
    }
    if !view_guard() {
        return;
    }
    let g = guards();
    if light() {
        for &(at, n) in &ranges {
            let (first, last) = (at / BLOCK, at.saturating_add(n).div_ceil(BLOCK));
            if g.protected.intersects(first, last) {
                published_clear_range(first, last);
            }
        }
    }
    for &(at, n) in &ranges {
        if open_here(&g, at, at.saturating_add(n)) {
            #[cfg(feature = "diagnostics")]
            ALIASES[2].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

/// Held shared by a read of title memory in place, and alone by the runtime
/// while it changes what title memory shows.
///
/// The runtime does not change a mapping in one step. To unmap one page of
/// a view it unmaps the whole view and maps what stays again
/// (`xmem.rs` `unmap`), and a view it maps is a reservation until the next
/// call commits it (`map`, then `commit`). For that moment pages the title
/// never gave up are "reserved, never committed", and a thread reading
/// them in place takes an access violation: run 71 in `Layout::detile`,
/// run 74 in the hash before it, both under `Staging::refresh_now`, which
/// had asked whether the texture was all mapped and been told yes. Asking
/// cannot help; not changing the mapping while such a read is under way
/// does.
///
/// One lock per 16 MB of addresses (taken modulo [`SHARDS`]), not one for
/// all title memory: a read holds the locks of the addresses it reads, a
/// change those of the addresses it changes. With one lock, every texture
/// refresh waited for every remap anywhere in title memory, and the remaps
/// are kernel calls of 0.1-0.8 ms each, some 1,000-2,700 a second while the
/// world streams in (run mem-W3a): up to 1.1 s of a frame's texture
/// preparation was waiting for it (mem-W0). Several locks are always taken
/// in ascending order, by readers and changes alike, so none waits on
/// another in a circle.
static MAPPING: [std::sync::RwLock<()>; SHARDS] = [const { std::sync::RwLock::new(()) }; SHARDS];

/// Locks in [`MAPPING`], and the bits of address each covers (16 MB).
const SHARDS: usize = 256;
const SHARD_BITS: u32 = 24;

/// The locks of [`MAPPING`] covering `[at, at + len)`, ascending; every
/// one for `len` 0 (the whole of title memory) or a range of [`SHARDS`]
/// spans or more.
fn shards(at: usize, len: usize) -> Vec<usize> {
    if len == 0 {
        return (0..SHARDS).collect();
    }
    let first = at >> SHARD_BITS;
    let last = at.saturating_add(len - 1) >> SHARD_BITS;
    if last - first + 1 >= SHARDS {
        return (0..SHARDS).collect();
    }
    let mut all: Vec<usize> = (first..=last).map(|s| s % SHARDS).collect();
    all.sort_unstable();
    all
}

/// What the runtime holds while it changes what title memory shows
/// ([`title_memory_remapping`]).
pub struct Remapping {
    _guards: Vec<std::sync::RwLockWriteGuard<'static, ()>>,
    _lazy: crate::shadow::LazyRemapGuard,
    /// When it was had, how long it was waited for (µs), and where.
    #[cfg(feature = "diagnostics")]
    taken: (
        std::time::Instant,
        u64,
        &'static std::panic::Location<'static>,
    ),
}

/// The runtime is about to map, unmap or move physical pages of title
/// memory: until what it returns is dropped, no read of title memory in
/// place is under way and none begins ([`title_memory_reading`]). Taken
/// around the whole change, the unmapping and the mapping again. All of
/// title memory; [`title_memory_remapping_range`] for part of it.
#[track_caller]
pub fn title_memory_remapping() -> Remapping {
    title_memory_remapping_range(0, 0)
}

/// [`title_memory_remapping`] for `[at, at + len)` only: every address
/// whose mapping the change touches, the parts of views it takes away and
/// maps again included. Reads elsewhere go on.
#[track_caller]
pub fn title_memory_remapping_range(at: usize, len: usize) -> Remapping {
    #[cfg(feature = "diagnostics")]
    let asked = std::time::Instant::now();
    let guards = shards(at, len)
        .into_iter()
        .map(|s| MAPPING[s].write().unwrap_or_else(|e| e.into_inner()))
        .collect();
    let lazy = crate::shadow::lazy_begin_remapping_barrier();
    Remapping {
        _guards: guards,
        _lazy: lazy,
        #[cfg(feature = "diagnostics")]
        taken: (
            std::time::Instant::now(),
            asked.elapsed().as_micros() as u64,
            std::panic::Location::caller(),
        ),
    }
}

/// Diagnostics: the changes of the mapping by where the runtime made them since the
/// last asking: file, line, and [changes, µs waited for in-place reads to end, µs held,
/// longest µs held]. The texture refreshes wait on these ("waiting for title memory to
/// hold still").
#[cfg(feature = "diagnostics")]
static REMAPS: std::sync::Mutex<Vec<(&'static str, u32, [u64; 4])>> =
    std::sync::Mutex::new(Vec::new());

#[cfg(feature = "diagnostics")]
impl Drop for Remapping {
    fn drop(&mut self) {
        let (at, waited, place) = self.taken;
        let held = at.elapsed().as_micros() as u64;
        let mut all = REMAPS.lock().unwrap_or_else(|e| e.into_inner());
        let i = match all
            .iter()
            .position(|r| r.0 == place.file() && r.1 == place.line())
        {
            Some(i) => i,
            None => {
                all.push((place.file(), place.line(), [0; 4]));
                all.len() - 1
            }
        };
        let k = &mut all[i].2;
        k[0] += 1;
        k[1] += waited;
        k[2] += held;
        k[3] = k[3].max(held);
    }
}

/// [`REMAPS`], taken (and reset).
#[cfg(feature = "diagnostics")]
pub(crate) fn remaps_taken() -> Vec<(&'static str, u32, [u64; 4])> {
    std::mem::take(&mut *REMAPS.lock().unwrap_or_else(|e| e.into_inner()))
}

/// What a read of title memory in place holds ([`title_memory_reading`]).
pub struct Reading(#[allow(dead_code)] Vec<std::sync::RwLockReadGuard<'static, ()>>);

/// For reading title memory in place: until what it returns is dropped,
/// the runtime changes no mapping, so memory found mapped stays mapped.
/// Not to be taken twice by one thread, nor held across anything that
/// maps title memory. All of title memory; [`title_memory_reading_range`]
/// for part of it.
pub fn title_memory_reading() -> Reading {
    title_memory_reading_range(0, 0)
}

/// [`title_memory_reading`] for `[at, at + len)` only: changes of mapping
/// elsewhere go on meanwhile.
pub fn title_memory_reading_range(at: usize, len: usize) -> Reading {
    Reading(
        shards(at, len)
            .into_iter()
            .map(|s| MAPPING[s].read().unwrap_or_else(|e| e.into_inner()))
            .collect(),
    )
}

/// [`title_memory_reading_range`] if it can be had at once: `None` while
/// the runtime changes a mapping there (or waits to), for a caller that has
/// another way to read and would rather not wait.
pub fn title_memory_reading_now(at: usize, len: usize) -> Option<Reading> {
    let mut guards = Vec::new();
    for s in shards(at, len) {
        match MAPPING[s].try_read() {
            Ok(guard) => guards.push(guard),
            Err(std::sync::TryLockError::Poisoned(e)) => guards.push(e.into_inner()),
            Err(std::sync::TryLockError::WouldBlock) => return None,
        }
    }
    Some(Reading(guards))
}

/// The runtime mapped or unmapped title memory `[at, at + len)`: what it
/// shows is new and read-write, so it is no longer guarded (nor known
/// unmapped).
pub fn title_memory_remapped(at: usize, len: usize) {
    crate::texback::remapped(at, len);
    if reservation::enabled() {
        ask_once_end();
    }
    // Flush any lazy deferred pages BEFORE views/sections are unmapped or closed.
    crate::shadow::lazy_flush_range(at, len);
    // A placed texture there arrives, goes or changes with no descriptor
    // written.
    crate::descriptor::host::remapped(at, len);
    // The views the runtime told us of there are no longer what it shows.
    crate::inplace::views_gone(at, len);
    // And nothing stays in place over it.
    crate::inplace::title_memory_freeing(at, len);
    if len == 0 {
        return;
    }
    let (first, last) = (at / BLOCK, at.saturating_add(len).div_ceil(BLOCK));
    clear_subpages_range(first, last);
    let g = guards();
    VIEWS.clear_range(first, last);
    if guard_hot() > 0 {
        for block in first..last {
            if hot_get(block) != 0 {
                hot_set(block, 0);
            }
        }
    }
    let absent_runs = g.absent.take_range(first, last);
    if !absent_runs.is_empty() {
        g.placeholder_absent.remove_range(first, last);
        for (af, at_end) in absent_runs {
            absent_set_range(af, at_end, ABSENT_NONE);
        }
    }
    let prot_runs = g.protected.take_range(first, last);
    if !prot_runs.is_empty() {
        g.clean.remove_range(first, last);
        let coalesce = watch_coalesce();
        for (pf, pt) in prot_runs {
            PROTECTED.clear_range(pf, pt);
            clean_set_range(pf, pt, false);
            published_clear_range(pf, pt);
            unprotect_run_os_with(pf, pt, coalesce);
        }
    }
    g.aliased.remove_range(first, last);
}

/// Fast-path flag indicating [`install`] has registered the VEH write handler.
static INSTALLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn guard_open_ahead() -> usize {
    static AHEAD: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *AHEAD.get_or_init(|| {
        std::env::var("VICTORY_GUARD_OPEN_AHEAD")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1)
            .clamp(1, 64)
    })
}

/// Registers the handler that takes writes to guarded blocks, once, ahead
/// of the crash reporter's.
fn install() {
    use windows_sys::Win32::Foundation::{EXCEPTION_ACCESS_VIOLATION, NTSTATUS};
    use windows_sys::Win32::System::Diagnostics::Debug::{
        AddVectoredExceptionHandler, EXCEPTION_CONTINUE_EXECUTION, EXCEPTION_CONTINUE_SEARCH,
        EXCEPTION_POINTERS,
    };
    unsafe extern "system" fn on_write(info: *mut EXCEPTION_POINTERS) -> i32 {
        // SAFETY: the system passes the exception being dispatched.
        let r = unsafe { &*(*info).ExceptionRecord };
        const READ: usize = 0;
        const WRITE: usize = 1;
        if r.ExceptionCode != EXCEPTION_ACCESS_VIOLATION as NTSTATUS || r.NumberParameters < 2 {
            return EXCEPTION_CONTINUE_SEARCH;
        }
        let kind = r.ExceptionInformation[0];
        let at = r.ExceptionInformation[1];
        if (kind == READ || kind == WRITE) && crate::shadow::lazy_on_fault(at, kind == WRITE) {
            return EXCEPTION_CONTINUE_EXECUTION;
        }
        if kind != WRITE {
            return EXCEPTION_CONTINUE_SEARCH;
        }
        let ahead = guard_open_ahead();
        if port_guard() {
            let (opened, already_writable) = open_faulted(at);
            if opened != 0 {
                if ahead > 1 {
                    let block = at / BLOCK;
                    open_ahead_shared(block + 1, block + ahead);
                }
                count(0, 1);
                #[cfg(feature = "diagnostics")]
                crate::copystat::add(crate::copystat::C::GuardFaults, 1);
                return EXCEPTION_CONTINUE_EXECUTION;
            }
            return if already_writable {
                EXCEPTION_CONTINUE_EXECUTION
            } else {
                EXCEPTION_CONTINUE_SEARCH
            };
        }
        // Held across both checks: another thread may have opened the block
        // since this one faulted, and a copy may have guarded it again since
        // (then it is ours again, not read-only for some other reason).
        let g = guards();
        let block = at / BLOCK * BLOCK;
        if open(&g, at, at + 1) {
            if ahead > 1 {
                open(&g, block + BLOCK, block + ahead * BLOCK);
            }
            count(0, 1);
            #[cfg(feature = "diagnostics")]
            crate::copystat::add(crate::copystat::C::GuardFaults, 1);
            return EXCEPTION_CONTINUE_EXECUTION;
        }
        if writable(at) {
            EXCEPTION_CONTINUE_EXECUTION
        } else {
            EXCEPTION_CONTINUE_SEARCH
        }
    }
    static ONCE: std::sync::Once = std::sync::Once::new();
    // Pre-warm switches read inside the handler (`open` / `open_faulted` read them) so
    // an early write fault never reads the environment inside the handler.
    let _ = (
        light(),
        watch_coalesce(),
        watch_alias_batch(),
        port_guard(),
        guard_open_ahead(),
    );
    // SAFETY: the handler only touches our guard state and protections.
    ONCE.call_once(|| {
        unsafe {
            AddVectoredExceptionHandler(1, Some(on_write));
        }
        INSTALLED.store(true, std::sync::atomic::Ordering::Release);
    });
}

/// Ensures our vectored exception handler is installed (also used by lazy
/// copy-back before arming any `PAGE_NOACCESS` protection).
pub(crate) fn install_veh() {
    install();
}

#[cfg(test)]
pub(crate) fn is_guarded(block: usize) -> bool {
    guards_read().protected.contains(block)
}

/// Before a caller invokes `VirtualFree` on title memory `[at, at + len)`:
/// waits for any in-flight lazy flush, drops deferred `PAGE_NOACCESS`
/// protections, and removes `watch` guard metadata so the pages release cleanly.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn before_virtual_free(at: usize, len: usize) {
    crate::shadow::lazy_release_range(at, len);
    if len == 0 {
        return;
    }
    let first_p = at / PAGE;
    let last_p = at.saturating_add(len).div_ceil(PAGE);
    clear_subpages_range(first_p, last_p);
    for p in first_p..last_p {
        if let Some(part) = part(p, false) {
            part.0[p % PART].store(0, std::sync::atomic::Ordering::Release);
        }
    }
    let g = guards();
    let (first, last) = (at / BLOCK, at.saturating_add(len).div_ceil(BLOCK));
    VIEWS.clear_range(first, last);
    let absent_runs = g.absent.take_range(first, last);
    if !absent_runs.is_empty() {
        g.placeholder_absent.remove_range(first, last);
        for (af, at_end) in absent_runs {
            absent_set_range(af, at_end, ABSENT_NONE);
        }
    }
    let prot_runs = g.protected.take_range(first, last);
    if !prot_runs.is_empty() {
        g.clean.remove_range(first, last);
        for (pf, pt) in prot_runs {
            PROTECTED.clear_range(pf, pt);
            clean_set_range(pf, pt, false);
            published_clear_range(pf, pt);
        }
    }
    g.aliased.remove_range(first, last);
}

/// Before a caller invokes `VirtualProtect` on title memory `[at, at + len)`:
/// flushes any lazy deferred pages and opens `watch` guards first.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn before_virtual_protect(at: usize, len: usize) {
    crate::shadow::lazy_flush_range(at, len);
    open(&guards(), at, at.saturating_add(len));
}

/// Lock-free batch query: for each `(page, since)`, whether `PARTS` records a
/// write strictly newer than `since`.
pub(crate) fn was_written_since_batch(queries: &[(usize, u64)]) -> Vec<bool> {
    queries
        .iter()
        .map(|&(page, since)| {
            part(page, false).is_some_and(|p| {
                p.0[page % PART].load(std::sync::atomic::Ordering::Acquire) > since
            })
        })
        .collect()
}

/// Batch query under ONE `GUARDS` read lock: for each 64 KiB `page`, whether it
/// is currently clean-guarded (`protected && clean && !aliased`).
pub(crate) fn clean_guarded_mask(pages: &[usize]) -> Vec<bool> {
    let g = guards_read();
    pages
        .iter()
        .map(|&p| g.protected.contains(p) && g.clean.contains(p) && !g.aliased.contains(p))
        .collect()
}

/// Atomically transitions every clean, non-aliased `watch`-guarded 64 KiB block
/// in `[run_first, run_last)` from `PAGE_READONLY` to `PAGE_NOACCESS` under ONE
/// `GUARDS` write lock, coalescing contiguous runs clipped to uniform
/// `VirtualQuery` (`MEM_COMMIT && PAGE_READONLY`) regions so adjacent pages with
/// differing protection are never clobbered. Removes transitioned blocks from
/// `g.protected`, `PROTECTED`, `g.clean`, `CLEAN`, and `PUBLISHED` (keeping `VIEWS` set).
pub(crate) fn transition_clean_guards_run_to_noaccess(
    run_first: usize,
    run_last: usize,
) -> Vec<usize> {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_NOACCESS, PAGE_READONLY, VirtualProtect,
        VirtualQuery,
    };
    if run_first >= run_last {
        return Vec::new();
    }
    let g = guards();
    let mut eligible: Vec<usize> = Vec::with_capacity(run_last - run_first);
    for block in run_first..run_last {
        if g.protected.contains(block) && g.clean.contains(block) && !g.aliased.contains(block) {
            eligible.push(block);
        }
    }
    if eligible.is_empty() {
        return Vec::new();
    }

    let mut out: Vec<usize> = Vec::with_capacity(eligible.len());
    let protect_subrun = |from: usize, to: usize, out: &mut Vec<usize>, g: &Guards| {
        let mut cur = from;
        while cur < to {
            let cur_addr = cur * BLOCK;
            // SAFETY: plain struct filled by `VirtualQuery`.
            let mut mbi: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
            // SAFETY: valid out pointer.
            let qn = unsafe { VirtualQuery(cur_addr as *const _, &mut mbi, size_of_val(&mbi)) };
            if qn == 0 || mbi.State != MEM_COMMIT || mbi.Protect != PAGE_READONLY {
                cur += 1;
                continue;
            }
            let reg_end_addr = (mbi.BaseAddress as usize).saturating_add(mbi.RegionSize);
            let sub_to = (reg_end_addr / BLOCK).min(to);
            if sub_to <= cur {
                cur += 1;
                continue;
            }
            let sub_len = (sub_to - cur) * BLOCK;
            let mut old = 0u32;
            let t0 = std::time::Instant::now();
            // SAFETY: `VirtualQuery` verified `[cur_addr, cur_addr + sub_len)` is a
            // uniform committed `PAGE_READONLY` region under `GUARDS`.
            let ok = unsafe {
                VirtualProtect(cur_addr as *const _, sub_len, PAGE_NOACCESS, &mut old) != 0
            };
            crate::shadow::lazy_record_protect_ns(t0.elapsed().as_nanos() as u64);
            if ok && old == PAGE_READONLY {
                g.protected.remove_range(cur, sub_to);
                PROTECTED.clear_range(cur, sub_to);
                g.clean.remove_range(cur, sub_to);
                clean_set_range(cur, sub_to, false);
                published_clear_range(cur, sub_to);
                out.extend(cur..sub_to);
            } else if ok {
                // Unexpected protection appeared concurrently at `cur`: restore `cur`
                // to `old` and the remaining tail to `PAGE_READONLY` without recursing.
                let mut tmp = 0u32;
                let t1 = std::time::Instant::now();
                // SAFETY: restoring the exact `old` protection on page `cur`.
                unsafe { VirtualProtect(cur_addr as *const _, BLOCK, old, &mut tmp) };
                crate::shadow::lazy_record_protect_ns(t1.elapsed().as_nanos() as u64);
                if sub_to > cur + 1 {
                    let t2 = std::time::Instant::now();
                    // SAFETY: restoring `PAGE_READONLY` on the remaining uniform tail.
                    unsafe {
                        VirtualProtect(
                            ((cur + 1) * BLOCK) as *const _,
                            (sub_to - cur - 1) * BLOCK,
                            PAGE_READONLY,
                            &mut tmp,
                        );
                    }
                    crate::shadow::lazy_record_protect_ns(t2.elapsed().as_nanos() as u64);
                }
            }
            cur = sub_to;
        }
    };

    let mut i = 0;
    while i < eligible.len() {
        let start = eligible[i];
        let mut end = start + 1;
        i += 1;
        while i < eligible.len() && eligible[i] == end {
            end += 1;
            i += 1;
        }
        protect_subrun(start, end, &mut out, &g);
    }
    out
}

/// Whether `at` is committed read-write memory now.
fn writable(at: usize) -> bool {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_READWRITE, VirtualQuery,
    };
    // SAFETY: plain data filled in by the query.
    let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: a valid out pointer of the right size.
    let n = unsafe { VirtualQuery(at as *const _, &mut info, size_of_val(&info)) };
    n != 0 && info.State == MEM_COMMIT && info.Protect == PAGE_READWRITE
}

#[cfg(test)]
mod tests {
    #[test]
    fn sub_page_masks_follow_the_stamps() {
        let mut stamps = [0u64; 16];
        assert_eq!(mask_of(&stamps, 5), None);
        stamps[3] = 6;
        stamps[9] = 5;
        stamps[15] = 9;
        assert_eq!(mask_of(&stamps, 5), Some(1 << 3 | 1 << 15));
        assert_eq!(mask_of(&stamps, 9), None);
    }

    use super::*;
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree,
    };
    const MEM_WRITE_WATCH: u32 = 0x20_0000;

    #[test]
    fn written_pages_are_reported_to_every_asker() {
        // SAFETY: a fresh write-watched allocation, freed at the end.
        let p = unsafe {
            VirtualAlloc(
                std::ptr::null(),
                4 * PAGE,
                MEM_RESERVE | MEM_COMMIT | MEM_WRITE_WATCH,
                PAGE_READWRITE,
            )
        } as usize;
        assert_ne!(p, 0);
        let (start, end) = (p, p + 4 * PAGE);
        let (_, before) = writes(start, end, false).expect("watched");
        assert!(before.is_empty());
        // SAFETY: committed pages of ours.
        unsafe { ((p + PAGE + 8) as *mut u8).write(1) };
        let (now, after) = writes(start, end, false).expect("watched");
        assert_eq!(after, [(p / PAGE + 1, now)]);
        // Another asker after the kernel's bit was cleared still sees it.
        let (later, again) = writes(p + PAGE, p + 2 * PAGE, false).expect("watched");
        assert!(later > now);
        assert_eq!(again, [(p / PAGE + 1, now)]);
        let mut plain = vec![0u8; PAGE];
        assert!(
            writes(
                plain.as_mut_ptr() as usize,
                plain.as_ptr() as usize + PAGE,
                true
            )
            .is_none()
        );
        before_virtual_free(p, 4 * PAGE);
        // SAFETY: our allocation.
        unsafe { VirtualFree(p as *mut _, 0, MEM_RELEASE) };
    }

    /// A kernel read into a guarded view block fails; opened first, it
    /// lands and the block counts as written.
    #[test]
    fn kernel_reads_into_guarded_blocks_after_opening() {
        use std::io::{Read, Seek, Write};
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, FILE_MAP_WRITE, MapViewOfFile, UnmapViewOfFile,
        };
        // SAFETY: a pagefile section of two blocks, mapped read-write.
        let view = unsafe {
            let section = CreateFileMappingW(
                -1isize as _,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                (2 * BLOCK) as u32,
                std::ptr::null(),
            );
            assert!(!section.is_null());
            MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, 2 * BLOCK).Value as usize
        };
        assert_ne!(view, 0);
        assert!(guard(view, view + 2 * BLOCK).is_some()); // now read-only
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(b"from the disk").unwrap();
        // `ReadFile` inside: the kernel writes the view.
        let read = |file: &mut std::fs::File| {
            file.rewind().unwrap();
            // SAFETY: 13 bytes of our view, nothing else refers to them.
            let into = unsafe { std::slice::from_raw_parts_mut((view + 100) as *mut u8, 13) };
            file.read(into).is_ok_and(|n| n == 13)
        };
        assert!(
            !read(&mut file),
            "the kernel does not fault into our handler"
        );
        writing(view + 100, 13);
        assert!(read(&mut file));
        // SAFETY: the bytes just read.
        let landed = unsafe { std::slice::from_raw_parts((view + 100) as *const u8, 13) };
        assert_eq!(landed, b"from the disk");
        assert_eq!(
            guard(view, view + 2 * BLOCK),
            Some((view / PAGE..view / PAGE + 1).collect())
        );
        title_memory_remapped(view, 2 * BLOCK);
        // SAFETY: our view.
        unsafe {
            UnmapViewOfFile(
                windows_sys::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS {
                    Value: view as *mut _,
                },
            )
        };
    }

    #[test]
    fn writes_in_partly_committed_view_blocks_are_reported() {
        use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, FILE_MAP_WRITE, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile,
            SEC_RESERVE, UnmapViewOfFile,
        };

        const SIZE: usize = 3 * BLOCK;
        struct View(MEMORY_MAPPED_VIEW_ADDRESS);
        impl Drop for View {
            fn drop(&mut self) {
                title_memory_remapped(self.0.Value as usize, SIZE);
                // SAFETY: our view is still mapped.
                unsafe { UnmapViewOfFile(self.0) };
            }
        }
        // SAFETY: a new reserved pagefile section. The view holds its own
        // reference after we close the handle, including on assertion failure.
        let view = unsafe {
            let section = CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                std::ptr::null(),
                PAGE_READWRITE | SEC_RESERVE,
                0,
                SIZE as u32,
                std::ptr::null(),
            );
            assert!(!section.is_null());
            let view = MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, SIZE);
            CloseHandle(section);
            assert!(!view.Value.is_null());
            View(view)
        };
        let start = view.0.Value as usize;
        let commit = |at: usize, len| {
            // SAFETY: these pages belong to our reserved view.
            let committed = unsafe { VirtualAlloc(at as _, len, MEM_COMMIT, PAGE_READWRITE) };
            assert_eq!(committed as usize, at);
        };
        let early = start + BLOCK + 3 * KERNEL_PAGE;
        let late = start + 2 * BLOCK + 3 * KERNEL_PAGE;
        commit(start, BLOCK);
        commit(early, KERNEL_PAGE);
        let refresh = || guard(start, start + SIZE).unwrap();
        assert_eq!(refresh(), [start / PAGE, early / PAGE, late / PAGE]);
        // The last block is still entirely reserved.
        assert!(!refresh().contains(&(late / PAGE)));

        commit(late, KERNEL_PAGE); // Ordinary commit, without a runtime mapping notification.
        for at in [early, late] {
            assert!(writable(at));
            // SAFETY: each address is inside its committed read-write page.
            unsafe { (at as *mut u8).write_volatile(1) };
        }
        assert_eq!(refresh(), [early / PAGE, late / PAGE]);
    }

    /// A range of view memory whose first block is not mapped is a view
    /// all the same: its blocks are reported once, then the ones written
    /// or mapped since.
    #[test]
    fn a_range_that_begins_in_an_unmapped_block_is_guarded() {
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, MEM_PRESERVE_PLACEHOLDER, MEM_REPLACE_PLACEHOLDER,
            MEM_RESERVE_PLACEHOLDER, MapViewOfFile3, PAGE_NOACCESS, UnmapViewOfFile2,
            VirtualAlloc2,
        };
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        // SAFETY: a fresh placeholder of three blocks and a pagefile
        // section of three; the middle block is mapped first.
        let (p, section) = unsafe {
            let p = VirtualAlloc2(
                GetCurrentProcess(),
                std::ptr::null(),
                3 * BLOCK,
                MEM_RESERVE | MEM_RESERVE_PLACEHOLDER,
                PAGE_NOACCESS,
                std::ptr::null_mut(),
                0,
            ) as usize;
            assert_ne!(p, 0);
            let section = CreateFileMappingW(
                -1isize as _,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                (3 * BLOCK) as u32,
                std::ptr::null(),
            );
            assert!(!section.is_null());
            (p, section)
        };
        let map = |block: usize| {
            // SAFETY: splits the placeholder at the block and maps the
            // section's block there.
            let view = unsafe {
                VirtualFree(
                    (p + block * BLOCK) as *mut _,
                    BLOCK,
                    MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER,
                );
                MapViewOfFile3(
                    section,
                    GetCurrentProcess(),
                    (p + block * BLOCK) as *const _,
                    (block * BLOCK) as u64,
                    BLOCK,
                    MEM_REPLACE_PLACEHOLDER,
                    PAGE_READWRITE,
                    std::ptr::null_mut(),
                    0,
                )
            };
            assert_eq!(view.Value as usize, p + block * BLOCK);
            view
        };
        let pages = |block: usize| (p + block * BLOCK) / PAGE..(p + (block + 1) * BLOCK) / PAGE;
        let all: Vec<usize> = (0..3).flat_map(pages).collect();
        // Nothing mapped at all: a view with every block absent.
        assert!(is_view(p, p + 3 * BLOCK));
        let middle = map(1);
        title_memory_remapped(p + BLOCK, BLOCK);
        // The range begins in the placeholder, its middle block is a view.
        assert!(is_view(p, p + 3 * BLOCK));
        assert_eq!(guard(p, p + 3 * BLOCK), Some(all));
        assert_eq!(
            guard(p, p + 3 * BLOCK),
            Some(Vec::new()),
            "nothing written since"
        );
        // SAFETY: inside the mapped middle block.
        unsafe { ((p + BLOCK + 8) as *mut u8).write_volatile(1) };
        assert_eq!(guard(p, p + 3 * BLOCK), Some(pages(1).collect()));
        let first = map(0);
        title_memory_remapped(p, BLOCK);
        assert_eq!(guard(p, p + 3 * BLOCK), Some(pages(0).collect()));
        assert_eq!(guard(p, p + 3 * BLOCK), Some(Vec::new()));
        // Memory of ours that is no view stays none.
        let mut local = [0u8; 16];
        let at = local.as_mut_ptr() as usize;
        assert!(!is_view(at, at + local.len()));
        title_memory_remapped(p, 3 * BLOCK);
        // SAFETY: our views and placeholder.
        unsafe {
            UnmapViewOfFile2(GetCurrentProcess(), first, MEM_PRESERVE_PLACEHOLDER);
            UnmapViewOfFile2(GetCurrentProcess(), middle, MEM_PRESERVE_PLACEHOLDER);
            for block in 0..3 {
                VirtualFree((p + block * BLOCK) as *mut _, 0, MEM_RELEASE);
            }
            windows_sys::Win32::Foundation::CloseHandle(section);
        }
    }

    /// Blocks of a placeholder between views are reported once, then not
    /// again (they cannot be guarded) until the runtime maps them.
    #[test]
    fn unmapped_blocks_are_reported_once_until_mapped() {
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, MEM_PRESERVE_PLACEHOLDER, MEM_REPLACE_PLACEHOLDER,
            MEM_RESERVE_PLACEHOLDER, MapViewOfFile3, PAGE_NOACCESS, UnmapViewOfFile2,
            VirtualAlloc2,
        };
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        // SAFETY: a fresh placeholder of three blocks and a pagefile
        // section of two; block 0 shows the section's first block, as the
        // runtime maps physical pages.
        let (p, section) = unsafe {
            let p = VirtualAlloc2(
                GetCurrentProcess(),
                std::ptr::null(),
                3 * BLOCK,
                MEM_RESERVE | MEM_RESERVE_PLACEHOLDER,
                PAGE_NOACCESS,
                std::ptr::null_mut(),
                0,
            ) as usize;
            assert_ne!(p, 0);
            let section = CreateFileMappingW(
                -1isize as _,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                (2 * BLOCK) as u32,
                std::ptr::null(),
            );
            assert!(!section.is_null());
            (p, section)
        };
        let map = |block: usize| {
            // SAFETY: splits the placeholder at the block and maps the
            // section's block there.
            let view = unsafe {
                VirtualFree(
                    (p + block * BLOCK) as *mut _,
                    BLOCK,
                    MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER,
                );
                MapViewOfFile3(
                    section,
                    GetCurrentProcess(),
                    (p + block * BLOCK) as *const _,
                    (block * BLOCK) as u64,
                    BLOCK,
                    MEM_REPLACE_PLACEHOLDER,
                    PAGE_READWRITE,
                    std::ptr::null_mut(),
                    0,
                )
            };
            assert_eq!(view.Value as usize, p + block * BLOCK);
            view
        };
        let first = map(0);
        let pages = |block: usize| (p + block * BLOCK) / PAGE..(p + (block + 1) * BLOCK) / PAGE;
        let all: Vec<usize> = (0..3).flat_map(pages).collect();
        assert_eq!(guard(p, p + 3 * BLOCK), Some(all));
        assert_eq!(
            guard(p, p + 3 * BLOCK),
            Some(Vec::new()),
            "nothing written since"
        );
        let second = map(1);
        title_memory_remapped(p + BLOCK, BLOCK);
        assert_eq!(guard(p, p + 3 * BLOCK), Some(pages(1).collect()));
        assert_eq!(guard(p, p + 3 * BLOCK), Some(Vec::new()));
        title_memory_remapped(p, 3 * BLOCK);
        // SAFETY: our views and placeholder.
        unsafe {
            UnmapViewOfFile2(GetCurrentProcess(), first, MEM_PRESERVE_PLACEHOLDER);
            UnmapViewOfFile2(GetCurrentProcess(), second, MEM_PRESERVE_PLACEHOLDER);
            for block in 0..3 {
                VirtualFree((p + block * BLOCK) as *mut _, 0, MEM_RELEASE);
            }
            windows_sys::Win32::Foundation::CloseHandle(section);
        }
    }

    /// A pagefile section mapped twice: two views of the same physical
    /// pages, the second left for the test; the section handle, the two
    /// addresses.
    fn two_views(bytes: usize) -> (usize, usize, usize) {
        use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, FILE_MAP_WRITE, MapViewOfFile,
        };
        // SAFETY: a fresh pagefile section, mapped twice; the views keep
        // the section alive and the handle is closed by the caller.
        unsafe {
            let section = CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                bytes as u32,
                std::ptr::null(),
            );
            assert!(!section.is_null());
            let one = MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, bytes).Value as usize;
            let two = MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, bytes).Value as usize;
            assert_ne!(one, 0);
            assert_ne!(two, 0);
            crate::inplace::title_memory_view(one, bytes, section as usize, 0);
            crate::inplace::title_memory_view(two, bytes, section as usize, 0);
            (section as usize, one, two)
        }
    }

    /// The section of [`two_views`] is closed and its views unmapped; the
    /// guard is told they are gone first.
    fn drop_views(section: usize, views: &[usize], bytes: usize) {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Memory::{MEMORY_MAPPED_VIEW_ADDRESS, UnmapViewOfFile};
        for &view in views {
            title_memory_remapped(view, bytes);
            // SAFETY: a view of the section mapped by `two_views`.
            unsafe {
                UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                    Value: view as *mut _,
                })
            };
        }
        // SAFETY: the section's last reference.
        unsafe { CloseHandle(section as *mut std::ffi::c_void) };
    }

    /// The title wrote the pages of a guarded texture through another view
    /// of them: the fault at that view is the guard's to see, and the
    /// guarded view's next check reports the pages written.
    #[test]
    fn a_write_through_another_view_is_reported_at_the_guarded_one() {
        const SIZE: usize = 2 * BLOCK;
        let (section, guarded, other) = two_views(SIZE);
        let pages = |block: usize| (guarded + block * BLOCK) / PAGE;
        // The first check takes it in (and every view of its pages).
        assert_eq!(
            guard(guarded, guarded + SIZE),
            Some(vec![pages(0), pages(1)])
        );
        assert_eq!(
            guard(guarded, guarded + SIZE),
            Some(Vec::new()),
            "nothing written yet"
        );
        // Writing through the other view faults into the handler, which
        // opens that view and the guarded one: the next check sees it.
        // SAFETY: the second view, read-write once the handler has opened it.
        unsafe { ((other + BLOCK + 8) as *mut u8).write(1) };
        assert_eq!(guard(guarded, guarded + SIZE), Some(vec![pages(1)]));
        assert_eq!(guard(guarded, guarded + SIZE), Some(Vec::new()));
        drop_views(section, &[guarded, other], SIZE);
    }

    /// A view of guarded pages is mapped anew: it is writable, so the
    /// guarded view checks its pages again (and guards the new view).
    #[test]
    fn a_view_mapped_beside_a_guarded_one_makes_it_check_again() {
        const SIZE: usize = 2 * BLOCK;
        let (section, guarded, other) = two_views(SIZE);
        let pages = |block: usize| (guarded + block * BLOCK) / PAGE;
        assert_eq!(
            guard(guarded, guarded + SIZE),
            Some(vec![pages(0), pages(1)])
        );
        assert_eq!(
            guard(guarded, guarded + SIZE),
            Some(Vec::new()),
            "nothing written yet"
        );
        title_memory_remapped(other, SIZE);
        crate::inplace::title_memory_view(other, SIZE, section, 0);
        assert_eq!(
            guard(guarded, guarded + SIZE),
            Some(vec![pages(0), pages(1)])
        );
        assert_eq!(guard(guarded, guarded + SIZE), Some(Vec::new()));
        drop_views(section, &[guarded, other], SIZE);
    }

    /// A write of the runtime's own, told at one view of the pages
    /// (`writing`): the other views' next checks report the pages written.
    #[test]
    fn a_write_told_at_one_view_is_reported_at_the_others() {
        const SIZE: usize = 2 * BLOCK;
        let (section, guarded, other) = two_views(SIZE);
        let pages = |block: usize| (guarded + block * BLOCK) / PAGE;
        assert_eq!(
            guard(guarded, guarded + SIZE),
            Some(vec![pages(0), pages(1)])
        );
        assert_eq!(
            guard(guarded, guarded + SIZE),
            Some(Vec::new()),
            "nothing written yet"
        );
        writing(other + BLOCK, BLOCK);
        assert_eq!(guard(guarded, guarded + SIZE), Some(vec![pages(1)]));
        assert_eq!(guard(guarded, guarded + SIZE), Some(Vec::new()));
        // Half a page and the next: both blocks, not just the first's.
        writing(other + BLOCK / 2, BLOCK);
        assert_eq!(
            guard(guarded, guarded + SIZE),
            Some(vec![pages(0), pages(1)])
        );
        drop_views(section, &[guarded, other], SIZE);
    }

    /// Both views of the pages are checked, each for a copy of its own
    /// (the title shows one physical page at several addresses, and each
    /// address has its host copy): a write through either is reported at
    /// both, whichever is checked first. Guarding the pages for the first
    /// view's check must not note the second view clean, or its copy would
    /// keep the bytes from before the write.
    #[test]
    fn a_write_is_reported_at_every_checked_view() {
        const SIZE: usize = 2 * BLOCK;
        let (section, one, two) = two_views(SIZE);
        let pages = |view: usize, block: usize| (view + block * BLOCK) / PAGE;
        assert_eq!(
            guard(one, one + SIZE),
            Some(vec![pages(one, 0), pages(one, 1)])
        );
        assert_eq!(
            guard(two, two + SIZE),
            Some(vec![pages(two, 0), pages(two, 1)]),
            "the second view's first check"
        );
        assert_eq!(
            guard(one, one + SIZE),
            Some(Vec::new()),
            "nothing written yet"
        );
        assert_eq!(
            guard(two, two + SIZE),
            Some(Vec::new()),
            "nothing written yet"
        );
        // SAFETY: the second view, read-write once the handler has opened it.
        unsafe { ((two + BLOCK + 8) as *mut u8).write(1) };
        assert_eq!(guard(one, one + SIZE), Some(vec![pages(one, 1)]));
        assert_eq!(
            guard(two, two + SIZE),
            Some(vec![pages(two, 1)]),
            "written through this view, checked second"
        );
        // SAFETY: the first view, as above.
        unsafe { ((one + 8) as *mut u8).write(1) };
        assert_eq!(guard(two, two + SIZE), Some(vec![pages(two, 0)]));
        assert_eq!(
            guard(one, one + SIZE),
            Some(vec![pages(one, 0)]),
            "written through this view, checked second"
        );
        assert_eq!(guard(one, one + SIZE), Some(Vec::new()));
        assert_eq!(guard(two, two + SIZE), Some(Vec::new()));
        clear_test_page_stamps(one, SIZE);
        clear_test_page_stamps(two, SIZE);
        drop_views(section, &[one, two], SIZE);
    }

    /// A page shown in more than `MANY_VIEWS` views is guarded at each by
    /// itself: the others stay writable, and nothing looks through them all.
    #[test]
    fn a_page_of_many_views_is_guarded_one_view_at_a_time() {
        let (section, one, two) = two_views(BLOCK);
        let filler = one + (1 << 45);
        for n in 0..MANY_VIEWS {
            crate::inplace::title_memory_view(filler + n * BLOCK, BLOCK, section, 0);
        }
        assert_eq!(guard(one, one + BLOCK), Some(vec![one / PAGE]));
        assert!(writable(two), "another view of it is left alone");
        // SAFETY: the guarded view, opened by the handler.
        unsafe { ((one + 8) as *mut u8).write(1) };
        assert_eq!(guard(one, one + BLOCK), Some(vec![one / PAGE]));
        title_memory_remapped(filler, MANY_VIEWS * BLOCK);
        drop_views(section, &[one, two], BLOCK);
    }

    /// A write or mapping that runs past the end of the address space (a
    /// title's `fread` with a huge count) covers up to the end; the sum used
    /// to overflow. Up there no test has guarded blocks.
    #[test]
    fn ranges_past_the_address_space_end_do_not_overflow() {
        let top = usize::MAX - 16;
        writing(top, PAGE);
        title_memory_remapped(top, usize::MAX);
    }

    #[test]
    fn bits_set_any_all_clear_and_bounds() {
        static TEST_BITS: Bits = Bits::new();
        assert!(!TEST_BITS.any(0, 128));
        assert!(TEST_BITS.all(10, 10));
        assert!(!TEST_BITS.all(10, 11));

        // Word boundary (63..66) and 4 GiB piece boundary (65535..65537).
        let boundary = Bits::WORDS * 64;
        TEST_BITS.set(63);
        TEST_BITS.set(64);
        TEST_BITS.set(65);
        TEST_BITS.set(boundary - 1);
        TEST_BITS.set(boundary);
        assert!(TEST_BITS.all(63, 66));
        assert!(TEST_BITS.all(boundary - 1, boundary + 1));
        assert!(!TEST_BITS.any(0, 63));
        assert!(!TEST_BITS.any(66, boundary - 1));

        TEST_BITS.clear(64);
        assert!(TEST_BITS.any(63, 64));
        assert!(!TEST_BITS.any(64, 65));
        assert!(TEST_BITS.any(65, 66));
        assert!(!TEST_BITS.all(63, 66));

        TEST_BITS.set_range(120, 200);
        assert!(!TEST_BITS.any(119, 120));
        assert!(TEST_BITS.all(120, 200));
        assert!(!TEST_BITS.any(200, 201));

        TEST_BITS.clear_range(125, 195);
        assert!(TEST_BITS.all(120, 125));
        assert!(!TEST_BITS.any(125, 195));
        assert!(TEST_BITS.all(195, 200));

        // Out-of-bounds blocks do not panic or overflow.
        let oob = usize::MAX / BLOCK;
        TEST_BITS.set(oob);
        TEST_BITS.set_range(oob, usize::MAX);
        assert!(!TEST_BITS.any(oob, usize::MAX));
        assert!(!TEST_BITS.all(oob, usize::MAX));
        TEST_BITS.clear(oob);
        TEST_BITS.clear_range(oob, usize::MAX);
    }

    #[test]
    fn views_marks_guarded_views_clears_on_remap_and_falls_back_on_watched_memory() {
        use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, FILE_MAP_WRITE, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile,
            UnmapViewOfFile,
        };

        // Part A: real view memory sets VIEWS + PROTECTED, survives a write fault
        // in VIEWS (even after PROTECTED clears), and clears on title_memory_remapped.
        // SAFETY: fresh 2-block pagefile section mapped read-write.
        let (section, view) = unsafe {
            let section = CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                (2 * BLOCK) as u32,
                std::ptr::null(),
            );
            assert!(!section.is_null());
            let view = MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, 2 * BLOCK).Value as usize;
            (section, view)
        };
        assert_ne!(view, 0);
        let (fb, lb) = (view / BLOCK, (view + 2 * BLOCK) / BLOCK);
        title_memory_remapped(view, 2 * BLOCK);

        assert!(writes(view, view + 2 * BLOCK, true).is_some());
        assert!(VIEWS.all(fb, lb));
        assert!(PROTECTED.all(fb, lb));
        if light() && skip_views() {
            assert!(refused_as_view(view, view + 2 * BLOCK));
        }
        assert!(writes(view, view + 2 * BLOCK, false).is_none());

        // Write fault opens block 0: PROTECTED clears, VIEWS remains set.
        // SAFETY: inside our mapped view; handler restores write access.
        unsafe { ((view + 8) as *mut u8).write_volatile(1) };
        assert!(!PROTECTED.any(fb, fb + 1));
        assert!(VIEWS.any(fb, fb + 1));
        if light() && skip_views() {
            assert!(refused_as_view(view, view + BLOCK));
        }

        title_memory_remapped(view, 2 * BLOCK);
        assert!(!VIEWS.any(fb, lb));
        assert!(!PROTECTED.any(fb, lb));
        assert!(!refused_as_view(view, view + 2 * BLOCK));

        // SAFETY: unmap our view and close its section.
        unsafe {
            UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                Value: view as *mut _,
            });
            CloseHandle(section);
        }

        // Part B: stale VIEWS bits over MEM_WRITE_WATCH memory fall back cleanly
        // to GetWriteWatch and clear VIEWS.
        // SAFETY: fresh write-watched allocation, freed below.
        let p = unsafe {
            VirtualAlloc(
                std::ptr::null(),
                2 * PAGE,
                MEM_RESERVE | MEM_COMMIT | MEM_WRITE_WATCH,
                PAGE_READWRITE,
            )
        } as usize;
        assert_ne!(p, 0);
        let (wb, we) = (p / BLOCK, (p + 2 * PAGE) / BLOCK);
        // SAFETY: committed write-watched page of ours.
        unsafe { ((p + PAGE + 8) as *mut u8).write(1) };
        VIEWS.set_range(wb, we);
        assert!(VIEWS.all(wb, we));
        if light() && skip_views() {
            assert!(refused_as_view(p, p + 2 * PAGE));
        }
        let (now, pages) = writes(p, p + 2 * PAGE, true).expect("fallback to write watch");
        assert!(pages.contains(&(p / PAGE + 1, now)));
        assert!(!VIEWS.any(wb, we), "fallback must clear stale VIEWS bits");
        before_virtual_free(p, 2 * PAGE);
        // SAFETY: our allocation.
        unsafe { VirtualFree(p as *mut _, 0, MEM_RELEASE) };
    }
    /// Zeroes test-written page stamps for `[start, start + len)` while the
    /// caller still exclusively owns that VA range, so a recycled VA in a
    /// concurrent test (e.g. `written_pages_are_reported_to_every_asker`) does
    /// not observe stale stamps from this test.
    fn clear_test_page_stamps(start: usize, len: usize) {
        let first = start / PAGE;
        let last = start.saturating_add(len).div_ceil(PAGE);
        clear_subpages_range(first, last);
        for page in first..last {
            if let Some(p) = part(page, false) {
                p.0[page % PART].store(0, std::sync::atomic::Ordering::Release);
            }
            if let Some(p) = part_of(&WHOLE, page, false) {
                p.0[page % PART].store(0, std::sync::atomic::Ordering::Release);
            }
        }
    }

    fn syscall_counts() -> (u64, u64) {
        (
            OS_VIRTUAL_PROTECT_CALLS.with(|c| c.get()),
            OS_VIRTUAL_QUERY_CALLS.with(|c| c.get()),
        )
    }

    #[test]
    fn block_set_interval_invariants() {
        let mut s = BlockSet::new();
        assert!(s.is_empty());
        s.insert_range(10, 20);
        s.insert_range(30, 40);
        assert!(s.covers(10, 20) && !s.covers(10, 21) && !s.covers(20, 30));
        assert_eq!(s.run_end_at(15), Some(20));
        assert_eq!(s.run_end_at(20), None);
        assert_eq!(s.next_start_from(20), Some(30));
        assert_eq!(s.count_in(15, 35), 10);
        s.insert_range(20, 30);
        assert!(s.covers(10, 40));
        assert_eq!(s.0.len(), 1);
        let taken = s.take_range(15, 25);
        assert_eq!(taken, vec![(15, 25)]);
        assert_eq!(s.0.len(), 2);
        assert!(s.covers(10, 15) && s.covers(25, 40) && !s.intersects(15, 25));
        s.insert(20);
        assert!(s.contains(20) && s.remove(20) && !s.remove(20));
        s.insert_sorted_blocks(&[15, 16, 17, 100, 101, 102]);
        assert!(s.covers(10, 18) && s.covers(100, 103));
    }

    /// Helper managing a placeholder reservation with independently mapped
    /// section views and split 64 KiB placeholder holes.
    struct SparsePlaceholder {
        base: usize,
        blocks: usize,
        section: *mut std::ffi::c_void,
        mapped: Vec<(
            usize,
            usize,
            windows_sys::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS,
        )>,
    }

    impl SparsePlaceholder {
        fn new(blocks: usize) -> Self {
            use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
            use windows_sys::Win32::System::Memory::{
                CreateFileMappingW, MEM_RESERVE_PLACEHOLDER, PAGE_NOACCESS, VirtualAlloc2,
            };
            use windows_sys::Win32::System::Threading::GetCurrentProcess;
            // SAFETY: fresh placeholder reservation + pagefile section, freed in Drop.
            let (base, section) = unsafe {
                let base = VirtualAlloc2(
                    GetCurrentProcess(),
                    std::ptr::null(),
                    blocks * BLOCK,
                    MEM_RESERVE | MEM_RESERVE_PLACEHOLDER,
                    PAGE_NOACCESS,
                    std::ptr::null_mut(),
                    0,
                ) as usize;
                assert_ne!(base, 0);
                let section = CreateFileMappingW(
                    INVALID_HANDLE_VALUE,
                    std::ptr::null(),
                    PAGE_READWRITE,
                    0,
                    (blocks * BLOCK) as u32,
                    std::ptr::null(),
                );
                assert!(!section.is_null());
                (base, section)
            };
            Self {
                base,
                blocks,
                section,
                mapped: Vec::new(),
            }
        }

        fn split_holes_per_block(&mut self, from: usize, to: usize) {
            use windows_sys::Win32::System::Memory::MEM_PRESERVE_PLACEHOLDER;
            for b in from..to {
                // SAFETY: splits placeholder inside our reservation.
                unsafe {
                    VirtualFree(
                        (self.base + b * BLOCK) as *mut _,
                        BLOCK,
                        MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER,
                    );
                }
            }
        }

        fn map_run(&mut self, from: usize, to: usize) {
            use windows_sys::Win32::System::Memory::{
                MEM_PRESERVE_PLACEHOLDER, MEM_REPLACE_PLACEHOLDER, MapViewOfFile3,
            };
            use windows_sys::Win32::System::Threading::GetCurrentProcess;
            let len = (to - from) * BLOCK;
            let addr = self.base + from * BLOCK;
            // SAFETY: splits placeholder for `[from, to)` and replaces with a view.
            let view = unsafe {
                VirtualFree(addr as *mut _, len, MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER);
                MapViewOfFile3(
                    self.section,
                    GetCurrentProcess(),
                    addr as *const _,
                    (from * BLOCK) as u64,
                    len,
                    MEM_REPLACE_PLACEHOLDER,
                    PAGE_READWRITE,
                    std::ptr::null_mut(),
                    0,
                )
            };
            assert_eq!(view.Value as usize, addr);
            self.mapped.push((from, to, view));
        }
    }

    impl Drop for SparsePlaceholder {
        fn drop(&mut self) {
            use windows_sys::Win32::Foundation::CloseHandle;
            use windows_sys::Win32::System::Memory::{MEM_PRESERVE_PLACEHOLDER, UnmapViewOfFile2};
            use windows_sys::Win32::System::Threading::GetCurrentProcess;
            title_memory_remapped(self.base, self.blocks * BLOCK);
            // SAFETY: unmaps every view and releases all placeholder slices we own.
            unsafe {
                for &(_, _, view) in &self.mapped {
                    UnmapViewOfFile2(GetCurrentProcess(), view, MEM_PRESERVE_PLACEHOLDER);
                }
                for b in 0..self.blocks {
                    VirtualFree((self.base + b * BLOCK) as *mut _, 0, MEM_RELEASE);
                }
                CloseHandle(self.section);
            }
        }
    }

    /// Sparse placeholder holes (including individually split 64 KiB placeholders)
    /// skip `VirtualProtect` by default in O(1)-per-OS-region, re-detect silent
    /// remaps immediately when `VICTORY_PLACEHOLDER_SKIP` is off, and issue zero
    /// `VirtualQuery` syscalls when `skip_placeholders` is enabled.
    #[test]
    fn sparse_placeholder_gaps_are_skipped_without_probing_or_protecting() {
        const TOTAL: usize = 64;
        // Part A: default mode (skip_placeholders=false) skips unsplit contiguous
        // placeholder spans in O(1) VirtualQuery per OS region, issues 0 VirtualProtect
        // on steady passes, and detects silent remaps without title_memory_remapped.
        let mut sp = SparsePlaceholder::new(TOTAL);
        sp.map_run(0, 8);
        sp.map_run(24, 32);
        sp.map_run(56, 64);
        let (start, end) = (sp.base, sp.base + TOTAL * BLOCK);
        let base_page = start / PAGE;

        let (vp0, _) = syscall_counts();
        let first = guard(start, end).expect("view");
        let (vp1, _) = syscall_counts();
        assert_eq!(first.len(), TOTAL);
        assert!(
            vp1 - vp0 <= 4,
            "expected <=4 VirtualProtect calls (1 bulk attempt + 3 mapped views), got {}",
            vp1 - vp0
        );

        let (vp2, vq2) = syscall_counts();
        for _ in 0..16 {
            assert_eq!(guard(start, end), Some(Vec::new()));
        }
        let (vp3, vq3) = syscall_counts();
        assert_eq!(
            vp3 - vp2,
            0,
            "steady-state guard must issue 0 VirtualProtect calls"
        );
        assert!(
            vq3 - vq2 <= 32,
            "expected <=2 VirtualQuery calls per pass across 40 contiguous hole blocks (32 total), got {}",
            vq3 - vq2
        );

        // Write 1 byte inside mapped block 25: only block 25 is reported & re-protected.
        // SAFETY: block 25 is mapped in `sp`.
        unsafe { ((start + 25 * BLOCK + 16) as *mut u8).write_volatile(0x5A) };
        let (vp4, vq4) = syscall_counts();
        assert_eq!(guard(start, end), Some(vec![base_page + 25]));
        let (vp5, vq5) = syscall_counts();
        assert_eq!(
            vp5 - vp4,
            1,
            "only the 1 dirty block should be re-protected"
        );
        assert!(
            vq5 - vq4 <= 4,
            "expected <=4 VirtualQuery calls (lock-free scan to dirty block + locked regions), got {}",
            vq5 - vq4
        );

        // Silent remap of block 12 WITHOUT title_memory_remapped is detected by default.
        sp.map_run(12, 13);
        assert_eq!(guard(start, end), Some(vec![base_page + 12]));
        assert_eq!(guard(start, end), Some(Vec::new()));
        drop(sp);

        // Part B: with skip_placeholders=true (VICTORY_PLACEHOLDER_SKIP=1), even 40
        // individually split 64 KiB placeholder holes take 0 VirtualQuery syscalls
        // until title_memory_remapped.
        let mut split = SparsePlaceholder::new(TOTAL);
        split.map_run(0, 8);
        split.split_holes_per_block(8, 24);
        split.map_run(24, 32);
        split.split_holes_per_block(32, 56);
        split.map_run(56, 64);
        let (s2, e2) = (split.base, split.base + TOTAL * BLOCK);
        let p2 = s2 / PAGE;

        assert_eq!(
            guard_with_flags(s2, e2, true, true, true, true)
                .expect("view")
                .len(),
            TOTAL
        );
        let (vp6, vq6) = syscall_counts();
        for _ in 0..16 {
            assert_eq!(
                guard_with_flags(s2, e2, true, true, true, true),
                Some(Vec::new())
            );
        }
        let (vp7, vq7) = syscall_counts();
        assert_eq!(
            vp7 - vp6,
            0,
            "placeholder-skip mode must issue 0 VirtualProtect calls"
        );
        assert_eq!(
            vq7 - vq6,
            0,
            "placeholder-skip mode must issue 0 VirtualQuery calls across 40 split holes"
        );

        split.map_run(12, 13);
        title_memory_remapped(s2 + 12 * BLOCK, BLOCK);
        let (vp8, vq8) = syscall_counts();
        assert_eq!(
            guard_with_flags(s2, e2, true, true, true, true),
            Some(vec![p2 + 12])
        );
        let (vp9, vq9) = syscall_counts();
        assert_eq!(vp9 - vp8, 1);
        assert_eq!(vq9 - vq8, 0);
    }

    /// Contiguous runs coalesce both guard (PAGE_READONLY) and unguard (PAGE_READWRITE)
    /// into single syscalls per contiguous OS view, including partial unguards and
    /// multi-VAD boundaries.
    #[test]
    fn contiguous_runs_coalesce_guard_and_unguard_syscalls() {
        const N: usize = 32;
        let mut sp = SparsePlaceholder::new(N);
        sp.map_run(0, N);
        let (start, end) = (sp.base, sp.base + N * BLOCK);
        let base_page = start / PAGE;

        let (vp0, _) = syscall_counts();
        let all = guard(start, end).expect("view");
        let (vp1, _) = syscall_counts();
        assert_eq!(all.len(), N);
        assert_eq!(
            vp1 - vp0,
            1,
            "32 contiguous blocks must coalesce into 1 VirtualProtect"
        );

        let (vp2, _) = syscall_counts();
        writing(start + 8 * BLOCK, 16 * BLOCK);
        let (vp3, _) = syscall_counts();
        assert_eq!(
            vp3 - vp2,
            1,
            "opening 16 contiguous blocks must coalesce into 1 VirtualProtect"
        );

        let (vp4, vq4) = syscall_counts();
        let dirty = guard(start, end).expect("view");
        let (vp5, vq5) = syscall_counts();
        let expected: Vec<usize> = (base_page + 8..base_page + 24).collect();
        assert_eq!(dirty, expected);
        assert_eq!(
            vp5 - vp4,
            1,
            "O(dirty) re-arm must re-protect [8..24) in 1 VirtualProtect"
        );
        assert_eq!(vq5 - vq4, 0);

        let mut sp2 = SparsePlaceholder::new(N);
        sp2.map_run(0, 16);
        sp2.map_run(16, 32);
        let (s2, e2) = (sp2.base, sp2.base + N * BLOCK);

        let (vp6, _) = syscall_counts();
        assert_eq!(guard(s2, e2).expect("view").len(), N);
        let (vp7, _) = syscall_counts();
        assert_eq!(
            vp7 - vp6,
            3,
            "2 adjacent 16-block VADs must take 1 bulk attempt + 2 per-VAD VirtualProtect calls, not 32"
        );

        let (vp8, _) = syscall_counts();
        writing(s2, N * BLOCK);
        let (vp9, _) = syscall_counts();
        assert_eq!(
            vp9 - vp8,
            3,
            "opening 2 adjacent 16-block VADs must coalesce per VAD (3 calls, not 32)"
        );
    }

    /// Dual-aliased views coalesce alias protection and unprotection into single
    /// syscalls per contiguous run, and the second view's first guard marks already-
    /// protected blocks clean with 0 VirtualProtect calls.
    #[test]
    fn aliased_views_coalesce_protect_open_and_o_dirty_rearm() {
        const N: usize = 32;
        const SIZE: usize = N * BLOCK;
        let (section, one, two) = two_views(SIZE);

        let (vp0, _) = syscall_counts();
        assert_eq!(guard(one, one + SIZE).expect("view").len(), N);
        let (vp1, _) = syscall_counts();
        assert_eq!(
            vp1 - vp0,
            2,
            "primary + alias view should take 2 coalesced VirtualProtect calls"
        );

        let (vp2, _) = syscall_counts();
        assert_eq!(guard(two, two + SIZE).expect("view").len(), N);
        let (vp3, _) = syscall_counts();
        assert_eq!(
            vp3 - vp2,
            0,
            "already-protected alias blocks must require 0 VirtualProtect calls"
        );

        let (vp4, _) = syscall_counts();
        writing(one, SIZE);
        let (vp5, _) = syscall_counts();
        assert_eq!(
            vp5 - vp4,
            2,
            "opening aliased 32-block range must coalesce into 2 VirtualProtect calls"
        );

        assert_eq!(guard(one, one + SIZE).expect("view").len(), N);
        assert_eq!(guard(two, two + SIZE).expect("view").len(), N);
        // SAFETY: block 10 of `two` is mapped; VEH handler opens block 10 on both views.
        unsafe { ((two + 10 * BLOCK + 4) as *mut u8).write_volatile(0xAB) };
        let (vp6, _) = syscall_counts();
        assert_eq!(guard(one, one + SIZE), Some(vec![one / PAGE + 10]));
        let (vp7, _) = syscall_counts();
        assert_eq!(
            vp7 - vp6,
            2,
            "re-guarding 1 dirty aliased block takes 2 VirtualProtect calls"
        );
        assert_eq!(guard(two, two + SIZE), Some(vec![two / PAGE + 10]));
        assert_eq!(guard(one, one + SIZE), Some(Vec::new()));
        assert_eq!(guard(two, two + SIZE), Some(Vec::new()));

        clear_test_page_stamps(one, SIZE);
        clear_test_page_stamps(two, SIZE);
        drop_views(section, &[one, two], SIZE);
    }

    /// Ordinary non-placeholder `SEC_RESERVE` holes are recorded in `absent`
    /// only (never `placeholder_absent`), so a silent `VirtualAlloc(..., MEM_COMMIT)`
    /// without a runtime notification is detected immediately by default `guard()`.
    #[test]
    fn ordinary_reserve_is_not_placeholder_and_silent_commit_is_detected() {
        use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, FILE_MAP_WRITE, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile,
            SEC_RESERVE, UnmapViewOfFile,
        };

        const SIZE: usize = 3 * BLOCK;
        struct View(MEMORY_MAPPED_VIEW_ADDRESS);
        impl Drop for View {
            fn drop(&mut self) {
                title_memory_remapped(self.0.Value as usize, SIZE);
                // SAFETY: mapped by this test.
                unsafe { UnmapViewOfFile(self.0) };
            }
        }
        // SAFETY: reserved pagefile section, mapped once.
        let view = unsafe {
            let section = CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                std::ptr::null(),
                PAGE_READWRITE | SEC_RESERVE,
                0,
                SIZE as u32,
                std::ptr::null(),
            );
            assert!(!section.is_null());
            let v = MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, SIZE);
            CloseHandle(section);
            assert!(!v.Value.is_null());
            View(v)
        };
        let start = view.0.Value as usize;
        let base_block = start / BLOCK;
        // SAFETY: inside our reserved view.
        assert_eq!(
            unsafe { VirtualAlloc(start as *const _, BLOCK, MEM_COMMIT, PAGE_READWRITE) } as usize,
            start
        );

        assert_eq!(
            guard(start, start + SIZE),
            Some(vec![base_block, base_block + 1, base_block + 2])
        );
        {
            let g = guards_read();
            assert!(g.absent.covers(base_block + 1, base_block + 3));
            assert!(
                !g.placeholder_absent
                    .intersects(base_block + 1, base_block + 3),
                "ordinary SEC_RESERVE must never enter placeholder_absent"
            );
            assert_eq!(absent_kind(base_block + 1), ABSENT_ORDINARY);
            assert_eq!(absent_kind(base_block + 2), ABSENT_ORDINARY);
        }
        assert_eq!(guard(start, start + SIZE), Some(Vec::new()));

        // Silent VirtualAlloc(MEM_COMMIT) on block 2 with no runtime notification.
        // SAFETY: inside our reserved view.
        assert_eq!(
            unsafe {
                VirtualAlloc(
                    (start + 2 * BLOCK) as *const _,
                    BLOCK,
                    MEM_COMMIT,
                    PAGE_READWRITE,
                )
            } as usize,
            start + 2 * BLOCK
        );
        assert_eq!(guard(start, start + SIZE), Some(vec![base_block + 2]));
        assert_eq!(guard(start, start + SIZE), Some(Vec::new()));
    }

    /// `protected ∩ absent == ∅` holds by construction whenever a previously
    /// absent/placeholder span becomes protected, so subsequent opens/writes
    /// are never silently dropped.
    #[test]
    fn protected_and_absent_sets_remain_disjoint_after_protect() {
        let mut sp = SparsePlaceholder::new(4);
        sp.map_run(0, 1);
        sp.split_holes_per_block(1, 4);
        let (start, end) = (sp.base, sp.base + 4 * BLOCK);
        let base_block = start / BLOCK;

        assert_eq!(guard(start, end).expect("view").len(), 4);
        {
            let g = guards_read();
            assert!(g.absent.covers(base_block + 1, base_block + 4));
            assert!(g.placeholder_absent.covers(base_block + 1, base_block + 4));
            assert_eq!(absent_kind(base_block + 2), ABSENT_PLACEHOLDER);
        }

        // Map block 2 WITHOUT calling title_memory_remapped, then call note_protected
        // as protect_here would if a concurrent commit raced with absent marking.
        sp.map_run(2, 3);
        {
            let g = guards();
            assert!(os_virtual_protect(
                base_block + 2,
                base_block + 3,
                windows_sys::Win32::System::Memory::PAGE_READONLY,
            ));
            note_protected(&g, base_block + 2, base_block + 3, true);
            assert!(g.protected.contains(base_block + 2));
            assert!(
                !g.absent.contains(base_block + 2),
                "protected block must be evicted from absent"
            );
            assert!(
                !g.placeholder_absent.contains(base_block + 2),
                "protected block must be evicted from placeholder_absent"
            );
            assert_eq!(absent_kind(base_block + 2), ABSENT_NONE);
        }

        // SAFETY: block 2 is mapped in `sp`.
        unsafe { ((start + 2 * BLOCK + 8) as *mut u8).write_volatile(0x33) };
        assert_eq!(guard(start, end), Some(vec![base_block + 2]));
        assert_eq!(guard(start, end), Some(Vec::new()));
    }

    /// `unprotect_run_os` across a partially unmapped span skips the uncommitted
    /// gap via `VirtualQuery` (`MEM_COMMIT` check) without falling back to
    /// per-block failing `VirtualProtect` syscalls, and restores `PAGE_READWRITE`
    /// on both surrounding committed views.
    #[test]
    fn unprotect_run_os_skips_uncommitted_gap_without_per_block_failures() {
        const N: usize = 20;
        let mut sp = SparsePlaceholder::new(N);
        sp.map_run(0, 8);
        sp.split_holes_per_block(8, 12);
        sp.map_run(12, 20);
        let (start, end) = (sp.base, sp.base + N * BLOCK);
        let base_block = start / BLOCK;

        assert_eq!(guard(start, end).expect("view").len(), N);
        assert!(!writable(start) && !writable(start + 12 * BLOCK));

        let (vp0, _) = syscall_counts();
        unprotect_run_os(base_block, base_block + N);
        let (vp1, _) = syscall_counts();
        assert!(
            vp1 - vp0 <= 3,
            "expected <=3 VirtualProtect calls (1 failed bulk + 2 committed views), got {}",
            vp1 - vp0
        );
        assert!(writable(start) && writable(start + 7 * BLOCK));
        assert!(writable(start + 12 * BLOCK) && writable(start + 19 * BLOCK));
        title_memory_remapped(start, N * BLOCK);
    }

    /// Concurrent writers (both faulting stores and `writing()` calls) racing with
    /// concurrent `guard()` / `writes()` readers across aliased views stay crash-free
    /// and report every final write once writers finish.
    #[test]
    fn concurrent_writers_and_guard_readers_stay_coherent() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        const N: usize = 16;
        const SIZE: usize = N * BLOCK;
        let (section, one, two) = two_views(SIZE);
        let _ = writes(one, one + SIZE, true).expect("view");
        let _ = writes(two, two + SIZE, true).expect("view");

        let done = Arc::new(AtomicBool::new(false));
        let mut threads = Vec::new();
        for tid in 0..4usize {
            let view = if tid % 2 == 0 { one } else { two };
            threads.push(std::thread::spawn(move || {
                for step in 0..200usize {
                    let b = (tid * 4 + step) % N;
                    let addr = view + b * BLOCK + (tid * 16);
                    if step % 7 == 0 {
                        writing(addr, 64);
                    }
                    // SAFETY: committed mapped view; VEH opens guarded blocks before write completes.
                    unsafe { (addr as *mut u32).write_volatile((tid as u32) << 16 | step as u32) };
                }
            }));
        }
        for rid in 0..2usize {
            let done = done.clone();
            let view = if rid == 0 { one } else { two };
            threads.push(std::thread::spawn(move || {
                let mut loops = 0usize;
                while !done.load(Ordering::Acquire) && loops < 400 {
                    let res = writes(view, view + SIZE, true);
                    assert!(res.is_some());
                    loops += 1;
                }
            }));
        }
        for t in threads.drain(..4) {
            t.join().unwrap();
        }
        done.store(true, Ordering::Release);
        for t in threads {
            t.join().unwrap();
        }

        let _ = guard(one, one + SIZE).expect("view");
        let _ = guard(two, two + SIZE).expect("view");
        assert_eq!(guard(one, one + SIZE), Some(Vec::new()));
        assert_eq!(guard(two, two + SIZE), Some(Vec::new()));
        for b in 0..N {
            // SAFETY: mapped view `two`.
            unsafe { ((two + b * BLOCK) as *mut u8).write_volatile(0xEE) };
        }
        let reported_one = guard(one, one + SIZE).expect("view");
        let reported_two = guard(two, two + SIZE).expect("view");
        assert_eq!(reported_one.len(), N);
        assert_eq!(reported_two.len(), N);

        clear_test_page_stamps(one, SIZE);
        clear_test_page_stamps(two, SIZE);
        drop_views(section, &[one, two], SIZE);
    }

    /// Deterministic side-by-side benchmark comparing the pre-optimization baseline
    /// (`BTreeSet` per-block walk + per-block fallback `VirtualProtect` + per-block
    /// `VirtualQuery` on split placeholders) against the optimized `watch::guard` / `open`
    /// implementation on live Windows placeholders and aliased views.
    #[test]
    fn benchmark_guard_coalesce_and_absent_skip() {
        use std::collections::BTreeSet;
        use std::time::Instant;
        use windows_sys::Win32::System::Memory::{
            MEM_FREE, MEM_RESERVE, MEMORY_BASIC_INFORMATION, PAGE_READONLY, VirtualProtect,
            VirtualQuery,
        };

        struct LegacyGuards {
            protected: BTreeSet<usize>,
            clean: BTreeSet<usize>,
            absent: BTreeSet<usize>,
            vp_calls: u64,
            vq_calls: u64,
        }
        impl LegacyGuards {
            fn new() -> Self {
                Self {
                    protected: BTreeSet::new(),
                    clean: BTreeSet::new(),
                    absent: BTreeSet::new(),
                    vp_calls: 0,
                    vq_calls: 0,
                }
            }
            fn uncommitted_end(&mut self, block: usize) -> usize {
                self.vq_calls += 1;
                let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
                let n = unsafe {
                    VirtualQuery((block * BLOCK) as *const _, &mut info, size_of_val(&info))
                };
                if n != 0 && matches!(info.State, MEM_RESERVE | MEM_FREE) {
                    (info.BaseAddress as usize).saturating_add(info.RegionSize) / BLOCK
                } else {
                    block
                }
            }
            fn protect_here(&mut self, from: usize, to: usize) {
                let mut old = 0;
                self.vp_calls += 1;
                if unsafe {
                    VirtualProtect(
                        (from * BLOCK) as *const _,
                        (to - from) * BLOCK,
                        PAGE_READONLY,
                        &mut old,
                    )
                } != 0
                {
                    self.protected.extend(from..to);
                    self.clean.extend(from..to);
                    return;
                }
                for block in from..to {
                    self.vp_calls += 1;
                    if unsafe {
                        VirtualProtect((block * BLOCK) as *const _, BLOCK, PAGE_READONLY, &mut old)
                    } != 0
                    {
                        self.protected.insert(block);
                        self.clean.insert(block);
                    } else if block < self.uncommitted_end(block) {
                        self.absent.insert(block);
                    }
                }
            }
            fn guard_walk(&mut self, start: usize, end: usize) -> usize {
                let (first, last) = (start / BLOCK, end.div_ceil(BLOCK));
                let clean: Vec<usize> = self.clean.range(first..last).copied().collect();
                if clean.len() == last - first {
                    return 0;
                }
                let mut written = 0usize;
                let mut next_clean = 0;
                let mut uncommitted_until = first;
                let mut run: Option<usize> = None;
                for block in first..=last {
                    while next_clean < clean.len() && clean[next_clean] < block {
                        next_clean += 1;
                    }
                    let is_clean = next_clean < clean.len() && clean[next_clean] == block;
                    let mut open = block < last && !is_clean;
                    if open && self.absent.contains(&block) {
                        if block >= uncommitted_until {
                            uncommitted_until = self.uncommitted_end(block);
                        }
                        open = block >= uncommitted_until;
                        if open {
                            self.absent.remove(&block);
                        }
                    }
                    if open {
                        written += 1;
                        if self.protected.contains(&block) {
                            self.clean.insert(block);
                        } else {
                            run.get_or_insert(block);
                            continue;
                        }
                    }
                    if let Some(from) = run.take() {
                        self.protect_here(from, block);
                    }
                }
                written
            }
            fn open_range(&mut self, start: usize, end: usize) {
                let blocks: Vec<usize> = self
                    .protected
                    .range(start / BLOCK..end.div_ceil(BLOCK))
                    .copied()
                    .collect();
                for block in blocks {
                    self.protected.remove(&block);
                    self.clean.remove(&block);
                    let mut old = 0;
                    self.vp_calls += 1;
                    unsafe {
                        VirtualProtect((block * BLOCK) as *const _, BLOCK, PAGE_READWRITE, &mut old)
                    };
                }
            }
        }

        // Workload 1a: Contiguous sparse placeholder reservation (256 blocks = 16 MiB:
        // 4 mapped 16-block views + 192 hole blocks across 4 contiguous spans),
        // default mode (skip_placeholders=false): VirtualProtect coalesces >=3x.
        const SPARSE_BLOCKS: usize = 256;
        const SUBMISSIONS: usize = 60;
        let mut sp = SparsePlaceholder::new(SPARSE_BLOCKS);
        sp.map_run(0, 16);
        sp.map_run(64, 80);
        sp.map_run(128, 144);
        sp.map_run(192, 208);
        let (s_start, s_end) = (sp.base, sp.base + SPARSE_BLOCKS * BLOCK);

        let mut legacy = LegacyGuards::new();
        let t0 = Instant::now();
        let _ = legacy.guard_walk(s_start, s_end);
        for sub in 0..SUBMISSIONS {
            let dirty_base = match sub % 4 {
                0 => 0,
                1 => 64,
                2 => 128,
                _ => 192,
            };
            legacy.open_range(
                s_start + dirty_base * BLOCK,
                s_start + (dirty_base + 4) * BLOCK,
            );
            let w = legacy.guard_walk(s_start, s_end);
            assert_eq!(w, 4);
        }
        let legacy_sparse_ms = t0.elapsed().as_secs_f64() * 1000.0;
        let legacy_sparse_vp = legacy.vp_calls;
        legacy.open_range(s_start, s_end);

        let (vp0, _) = syscall_counts();
        let t1 = Instant::now();
        let _ = guard(s_start, s_end).expect("view");
        for sub in 0..SUBMISSIONS {
            let dirty_base = match sub % 4 {
                0 => 0,
                1 => 64,
                2 => 128,
                _ => 192,
            };
            writing(s_start + dirty_base * BLOCK, 4 * BLOCK);
            let w = guard(s_start, s_end).expect("view");
            assert_eq!(w.len(), 4);
        }
        let opt_sparse_ms = t1.elapsed().as_secs_f64() * 1000.0;
        let (vp1, _) = syscall_counts();
        let opt_sparse_vp = vp1 - vp0;
        drop(sp);

        // Workload 1b: Individually split 64 KiB placeholder holes (128 holes across 192 blocks)
        // comparing legacy per-block probing against skip_placeholders=true (VICTORY_PLACEHOLDER_SKIP=1).
        const SPLIT_BLOCKS: usize = 192;
        let mut split_sp = SparsePlaceholder::new(SPLIT_BLOCKS);
        split_sp.map_run(0, 16);
        split_sp.split_holes_per_block(16, 48);
        split_sp.map_run(48, 64);
        split_sp.split_holes_per_block(64, 96);
        split_sp.map_run(96, 112);
        split_sp.split_holes_per_block(112, 144);
        split_sp.map_run(144, 160);
        split_sp.split_holes_per_block(160, 192);
        let (sp_start, sp_end) = (split_sp.base, split_sp.base + SPLIT_BLOCKS * BLOCK);

        let mut legacy_split = LegacyGuards::new();
        let _ = legacy_split.guard_walk(sp_start, sp_end);
        for sub in 0..SUBMISSIONS {
            let dirty_base = match sub % 4 {
                0 => 0,
                1 => 48,
                2 => 96,
                _ => 144,
            };
            legacy_split.open_range(
                sp_start + dirty_base * BLOCK,
                sp_start + (dirty_base + 4) * BLOCK,
            );
            assert_eq!(legacy_split.guard_walk(sp_start, sp_end), 4);
        }
        let legacy_split_vq = legacy_split.vq_calls;
        legacy_split.open_range(sp_start, sp_end);

        let (_, vq0) = syscall_counts();
        let _ = guard_with_flags(sp_start, sp_end, true, true, true, true).expect("view");
        for sub in 0..SUBMISSIONS {
            let dirty_base = match sub % 4 {
                0 => 0,
                1 => 48,
                2 => 96,
                _ => 144,
            };
            writing(sp_start + dirty_base * BLOCK, 4 * BLOCK);
            assert_eq!(
                guard_with_flags(sp_start, sp_end, true, true, true, true)
                    .expect("view")
                    .len(),
                4
            );
        }
        let (_, vq1) = syscall_counts();
        let opt_split_vq = vq1 - vq0;
        drop(split_sp);

        const ALIAS_BLOCKS: usize = 64;
        const ALIAS_SIZE: usize = ALIAS_BLOCKS * BLOCK;
        const CYCLES: usize = 30;
        let (section, one, two) = two_views(ALIAS_SIZE);

        let mut legacy_alias = LegacyGuards::new();
        let t2 = Instant::now();
        let _ = legacy_alias.guard_walk(one, one + ALIAS_SIZE);
        for b in 0..ALIAS_BLOCKS {
            let mut old = 0;
            legacy_alias.vp_calls += 1;
            unsafe {
                VirtualProtect(
                    (two + b * BLOCK) as *const _,
                    BLOCK,
                    PAGE_READONLY,
                    &mut old,
                )
            };
            legacy_alias.protected.insert(two / BLOCK + b);
        }
        for _ in 0..CYCLES {
            legacy_alias.open_range(one, one + ALIAS_SIZE);
            legacy_alias.open_range(two, two + ALIAS_SIZE);
            let _ = legacy_alias.guard_walk(one, one + ALIAS_SIZE);
            for b in 0..ALIAS_BLOCKS {
                let mut old = 0;
                legacy_alias.vp_calls += 1;
                unsafe {
                    VirtualProtect(
                        (two + b * BLOCK) as *const _,
                        BLOCK,
                        PAGE_READONLY,
                        &mut old,
                    )
                };
                legacy_alias.protected.insert(two / BLOCK + b);
            }
        }
        let legacy_alias_ms = t2.elapsed().as_secs_f64() * 1000.0;
        let legacy_alias_vp = legacy_alias.vp_calls;
        legacy_alias.open_range(one, one + ALIAS_SIZE);
        legacy_alias.open_range(two, two + ALIAS_SIZE);

        let (vp2, _) = syscall_counts();
        let t3 = Instant::now();
        assert_eq!(
            guard(one, one + ALIAS_SIZE).expect("view").len(),
            ALIAS_BLOCKS
        );
        for _ in 0..CYCLES {
            writing(one, ALIAS_SIZE);
            assert_eq!(
                guard(one, one + ALIAS_SIZE).expect("view").len(),
                ALIAS_BLOCKS
            );
        }
        let opt_alias_ms = t3.elapsed().as_secs_f64() * 1000.0;
        let (vp3, _) = syscall_counts();
        let opt_alias_vp = vp3 - vp2;

        clear_test_page_stamps(one, ALIAS_SIZE);
        clear_test_page_stamps(two, ALIAS_SIZE);
        drop_views(section, &[one, two], ALIAS_SIZE);

        eprintln!(
            "[bench watch::guard] sparse-default(256blk, 192 holes, {SUBMISSIONS} subs): \
             VirtualProtect {legacy_sparse_vp} -> {opt_sparse_vp} ({:.1}x), \
             wall {legacy_sparse_ms:.3} ms -> {opt_sparse_ms:.3} ms",
            legacy_sparse_vp as f64 / opt_sparse_vp.max(1) as f64,
        );
        eprintln!(
            "[bench watch::guard] sparse-ph-skip(192blk, 128 split holes, {SUBMISSIONS} subs): \
             VirtualQuery {legacy_split_vq} -> {opt_split_vq} ({:.1}x)",
            legacy_split_vq as f64 / opt_split_vq.max(1) as f64,
        );
        eprintln!(
            "[bench watch::guard] aliased(64blk x2, {CYCLES} cycles): \
             VirtualProtect {legacy_alias_vp} -> {opt_alias_vp} ({:.1}x), \
             wall {legacy_alias_ms:.3} ms -> {opt_alias_ms:.3} ms",
            legacy_alias_vp as f64 / opt_alias_vp.max(1) as f64,
        );

        assert!(
            opt_sparse_vp * 3 <= legacy_sparse_vp,
            "sparse VirtualProtect calls should drop >=3x: legacy={legacy_sparse_vp}, opt={opt_sparse_vp}"
        );
        assert!(
            opt_split_vq * 30 <= legacy_split_vq,
            "split placeholder skip VirtualQuery calls should drop >=30x: legacy={legacy_split_vq}, opt={opt_split_vq}"
        );
        assert!(
            opt_alias_vp * 30 <= legacy_alias_vp,
            "aliased VirtualProtect calls should drop >=30x: legacy={legacy_alias_vp}, opt={opt_alias_vp}"
        );
    }

    /// With `VICTORY_PLACEHOLDER_SKIP` unset (the default), confirmed placeholder
    /// holes enter `placeholder_absent` and `ABSENT_PLACEHOLDER`, yet silent maps
    /// without `title_memory_remapped` are still re-detected on the next `guard()`.
    #[test]
    fn default_placeholder_redetects_silent_map_without_remapped_notification() {
        if std::env::var_os("VICTORY_PLACEHOLDER_SKIP").is_some_and(|v| v == "1") {
            return;
        }
        assert!(!placeholder_skip_enabled());
        let mut sp = SparsePlaceholder::new(8);
        sp.map_run(0, 2);
        let (start, end) = (sp.base, sp.base + 8 * BLOCK);
        let base_block = start / BLOCK;
        let base_page = start / PAGE;

        assert_eq!(guard(start, end).expect("view").len(), 8);
        {
            let g = guards();
            assert!(g.absent.covers(base_block + 2, base_block + 8));
            assert!(g.placeholder_absent.covers(base_block + 2, base_block + 8));
            assert_eq!(absent_kind(base_block + 4), ABSENT_PLACEHOLDER);
        }
        assert_eq!(guard(start, end), Some(Vec::new()));

        // Silent remap of block 4 WITHOUT calling title_memory_remapped.
        sp.map_run(4, 5);
        assert_eq!(guard(start, end), Some(vec![base_page + 4]));
        assert!(!writable(start + 4 * BLOCK));
        {
            let g = guards();
            assert!(!g.absent.contains(base_block + 4));
            assert!(!g.placeholder_absent.contains(base_block + 4));
            assert_eq!(absent_kind(base_block + 4), ABSENT_NONE);
        }
        assert_eq!(guard(start, end), Some(Vec::new()));

        // A write fault to block 4 opens it and is reported on the next pass.
        // SAFETY: block 4 is mapped in `sp`.
        unsafe { ((start + 4 * BLOCK + 16) as *mut u8).write_volatile(0x7E) };
        assert_eq!(guard(start, end), Some(vec![base_page + 4]));
        assert_eq!(guard(start, end), Some(Vec::new()));
    }

    /// Simulates `guard()` protecting newly mapped memory between `MapViewOfFile3`
    /// and `title_memory_remapped`: the remap notification must restore OS
    /// `PAGE_READWRITE` so subsequent title writes never hit an unhandled AV.
    #[test]
    fn remap_notification_restores_writable_after_concurrent_protect() {
        let mut sp = SparsePlaceholder::new(2);
        sp.map_run(0, 2);
        let (start, end) = (sp.base, sp.base + 2 * BLOCK);
        assert_eq!(guard(start, end).expect("view").len(), 2);
        assert!(!writable(start) && !writable(start + BLOCK));

        title_memory_remapped(start, 2 * BLOCK);
        assert!(writable(start) && writable(start + BLOCK));
        // SAFETY: both blocks are mapped and restored to PAGE_READWRITE.
        unsafe {
            ((start + 32) as *mut u8).write_volatile(0xA5);
            ((start + BLOCK + 32) as *mut u8).write_volatile(0x5A);
        }
    }

    /// Tests parsing of the diagnostic bisect switches and live execution of all
    /// disabled fallback paths (`coalesce=false`, `alias_batch=false`, `single_lock=false`).
    #[test]
    fn bisect_switches_parse_and_disable_paths_correctly() {
        use std::ffi::OsStr;

        assert!(!parse_bool_flag(None, false));
        assert!(parse_bool_flag(Some(OsStr::new("1")), false));
        assert!(!parse_bool_flag(Some(OsStr::new("0")), false));
        assert!(!parse_bool_flag(Some(OsStr::new("invalid")), false));

        assert!(parse_bool_flag(None, true));
        assert!(!parse_bool_flag(Some(OsStr::new("0")), true));
        assert!(parse_bool_flag(Some(OsStr::new("1")), true));
        assert!(parse_bool_flag(Some(OsStr::new("invalid")), true));

        // 1. Non-coalesced path (`VICTORY_WATCH_COALESCE=0`): issues per-block VirtualProtect.
        let mut sp = SparsePlaceholder::new(8);
        sp.map_run(0, 8);
        let (start, end) = (sp.base, sp.base + 8 * BLOCK);
        let base_block = start / BLOCK;
        let base_page = start / PAGE;

        let (vp0, _) = syscall_counts();
        let all = guard_with_flags(start, end, false, true, false, true).expect("view");
        let (vp1, _) = syscall_counts();
        assert_eq!(all.len(), 8);
        assert!(
            vp1 - vp0 >= 8,
            "coalesce=false must issue per-block VirtualProtect, got {}",
            vp1 - vp0
        );
        assert!(!writable(start) && !writable(start + 7 * BLOCK));

        unsafe { ((start + 3 * BLOCK + 8) as *mut u8).write_volatile(0x41) };
        assert_eq!(
            guard_with_flags(start, end, false, true, false, true),
            Some(vec![base_page + 3])
        );

        let (vp2, _) = syscall_counts();
        unprotect_run_os_with(base_block, base_block + 8, false);
        let (vp3, _) = syscall_counts();
        assert_eq!(vp3 - vp2, 8);
        assert!(writable(start) && writable(start + 7 * BLOCK));
        title_memory_remapped(start, 8 * BLOCK);
        drop(sp);

        // 2. Per-block alias path (`VICTORY_WATCH_ALIAS_BATCH=0`) & single-lock=false (`VICTORY_WATCH_SINGLE_LOCK=0`).
        const ALIAS_BLOCKS: usize = 8;
        const ALIAS_SIZE: usize = ALIAS_BLOCKS * BLOCK;
        let (section, one, two) = two_views(ALIAS_SIZE);
        let first =
            guard_with_flags(one, one + ALIAS_SIZE, false, false, true, false).expect("view");
        assert_eq!(first.len(), ALIAS_BLOCKS);
        assert!(
            !writable(two),
            "alias view must be protected via views_between + pages_of"
        );
        assert_eq!(
            guard_with_flags(two, two + ALIAS_SIZE, false, false, true, false)
                .expect("view")
                .len(),
            ALIAS_BLOCKS
        );
        assert!(clean_all(two / BLOCK, two / BLOCK + ALIAS_BLOCKS));

        {
            let g = guards();
            open_with(&g, one, one + ALIAS_SIZE, false, false);
        }
        assert!(
            !clean_all(two / BLOCK, two / BLOCK + 1),
            "open_with(!alias_batch) must clear lock-free CLEAN on aliases"
        );
        assert!(writable(one) && writable(two));
        // SAFETY: `one` was opened and is writable.
        unsafe { ((one + 2 * BLOCK + 12) as *mut u8).write_volatile(0x9C) };
        let dirty =
            guard_with_flags(two, two + ALIAS_SIZE, false, false, true, false).expect("view");
        assert_eq!(dirty.len(), ALIAS_BLOCKS);
        assert_eq!(
            guard_with_flags(two, two + ALIAS_SIZE, false, false, true, false),
            Some(Vec::new())
        );

        clear_test_page_stamps(one, ALIAS_SIZE);
        clear_test_page_stamps(two, ALIAS_SIZE);
        drop_views(section, &[one, two], ALIAS_SIZE);
    }

    /// Stresses concurrent placeholder mapping/unmapping, alias view registration,
    /// VEH write faults, kernel `writing()` opens, and `guard()` readers across threads.
    #[test]
    fn concurrent_remap_and_write_fault_stress() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::{Arc, RwLock};
        use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, FILE_MAP_WRITE, MEM_PRESERVE_PLACEHOLDER, MEM_REPLACE_PLACEHOLDER,
            MEM_RESERVE_PLACEHOLDER, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile, MapViewOfFile3,
            PAGE_NOACCESS, UnmapViewOfFile, UnmapViewOfFile2, VirtualAlloc2,
        };
        use windows_sys::Win32::System::Threading::GetCurrentProcess;

        const SLOTS: usize = 8;
        const TOTAL_SIZE: usize = SLOTS * BLOCK;

        // SAFETY: reserves placeholder arena, splits into 1-block placeholders, and maps an alias view.
        let (base, section, alias_base) = unsafe {
            let base = VirtualAlloc2(
                GetCurrentProcess(),
                std::ptr::null(),
                TOTAL_SIZE,
                MEM_RESERVE | MEM_RESERVE_PLACEHOLDER,
                PAGE_NOACCESS,
                std::ptr::null_mut(),
                0,
            ) as usize;
            assert_ne!(base, 0);
            for s in 0..SLOTS - 1 {
                VirtualFree(
                    (base + s * BLOCK) as *mut _,
                    BLOCK,
                    MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER,
                );
            }
            let section = CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                TOTAL_SIZE as u32,
                std::ptr::null(),
            );
            assert!(!section.is_null());
            let alias = MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, TOTAL_SIZE).Value as usize;
            assert_ne!(alias, 0);
            crate::inplace::title_memory_view(alias, TOTAL_SIZE, section as usize, 0);
            (base, section as usize, alias)
        };

        let slots: Arc<Vec<RwLock<Option<usize>>>> =
            Arc::new((0..SLOTS).map(|_| RwLock::new(None)).collect());

        // Map even slots initially before starting threads.
        for s in (0..SLOTS).step_by(2) {
            let addr = base + s * BLOCK;
            // SAFETY: slot `s` holds a 1-block placeholder.
            let view = unsafe {
                MapViewOfFile3(
                    section as *mut std::ffi::c_void,
                    GetCurrentProcess(),
                    addr as *const _,
                    (s * BLOCK) as u64,
                    BLOCK,
                    MEM_REPLACE_PLACEHOLDER,
                    PAGE_READWRITE,
                    std::ptr::null_mut(),
                    0,
                )
            };
            assert_eq!(view.Value as usize, addr);
            crate::inplace::title_memory_view(addr, BLOCK, section, (s * BLOCK) as u64);
            *slots[s].write().unwrap() = Some(addr);
        }

        let stop = Arc::new(AtomicBool::new(false));
        let stop_remap = stop.clone();
        let stop_write = stop.clone();
        let slots_remap = slots.clone();
        let slots_write = slots.clone();

        let remapper = std::thread::spawn(move || {
            for iter in 0..80usize {
                let s = (iter * 3 + 1) % SLOTS;
                let mut slot = slots_remap[s].write().unwrap();
                if let Some(view) = slot.take() {
                    title_memory_remapped(view, BLOCK);
                    // SAFETY: `view` is a live 1-block placeholder replacement view we own.
                    unsafe {
                        UnmapViewOfFile2(
                            GetCurrentProcess(),
                            MEMORY_MAPPED_VIEW_ADDRESS {
                                Value: view as *mut _,
                            },
                            MEM_PRESERVE_PLACEHOLDER,
                        );
                    }
                } else {
                    let addr = base + s * BLOCK;
                    // SAFETY: slot `s` is currently an unmapped 1-block placeholder.
                    let view = unsafe {
                        MapViewOfFile3(
                            section as *mut std::ffi::c_void,
                            GetCurrentProcess(),
                            addr as *const _,
                            (s * BLOCK) as u64,
                            BLOCK,
                            MEM_REPLACE_PLACEHOLDER,
                            PAGE_READWRITE,
                            std::ptr::null_mut(),
                            0,
                        )
                    };
                    assert_eq!(view.Value as usize, addr);
                    crate::inplace::title_memory_view(addr, BLOCK, section, (s * BLOCK) as u64);
                    // Mix notified remaps with silent remaps to stress default detection.
                    if iter % 4 != 0 {
                        title_memory_remapped(addr, BLOCK);
                    }
                    *slot = Some(addr);
                }
            }
            stop_remap.store(true, Ordering::Release);
        });

        let writer = std::thread::spawn(move || {
            let mut iter = 0usize;
            while !stop_write.load(Ordering::Acquire) || iter < 60 {
                let s = iter % SLOTS;
                {
                    let slot = slots_write[s].read().unwrap();
                    if let Some(addr) = *slot {
                        // SAFETY: `slot` lock guarantees `addr` stays mapped across the volatile write.
                        unsafe { ((addr + 64) as *mut u8).write_volatile((iter & 0xFF) as u8) };
                    }
                }
                if iter.is_multiple_of(3) {
                    // SAFETY: `alias_base` remains mapped for the duration of the test.
                    unsafe {
                        ((alias_base + s * BLOCK + 128) as *mut u8)
                            .write_volatile((iter & 0xFF) as u8)
                    };
                } else if iter.is_multiple_of(5) {
                    writing(alias_base + s * BLOCK, BLOCK);
                }
                iter += 1;
            }
        });

        while !stop.load(Ordering::Acquire) {
            let _ = guard(base, base + TOTAL_SIZE).expect("placeholder arena is a view");
            let _ = guard(alias_base, alias_base + TOTAL_SIZE).expect("alias view");
        }
        remapper.join().unwrap();
        writer.join().unwrap();

        // Invariant check: protected ∩ absent == ∅ across the whole arena.
        {
            let g = guards();
            let base_block = base / BLOCK;
            for b in base_block..base_block + SLOTS {
                assert!(
                    !(g.protected.contains(b) && g.absent.contains(b)),
                    "block {b} cannot be both protected and absent"
                );
            }
        }

        // Cleanup all mapped views, placeholder slices, and section handle.
        title_memory_remapped(base, TOTAL_SIZE);
        title_memory_remapped(alias_base, TOTAL_SIZE);
        clear_test_page_stamps(base, TOTAL_SIZE);
        clear_test_page_stamps(alias_base, TOTAL_SIZE);
        // SAFETY: threads joined; unmap every active slot, alias view, placeholder, and close section.
        unsafe {
            for s in 0..SLOTS {
                if let Some(view) = slots[s].write().unwrap().take() {
                    UnmapViewOfFile2(
                        GetCurrentProcess(),
                        MEMORY_MAPPED_VIEW_ADDRESS {
                            Value: view as *mut _,
                        },
                        MEM_PRESERVE_PLACEHOLDER,
                    );
                }
            }
            for s in 0..SLOTS {
                VirtualFree((base + s * BLOCK) as *mut _, 0, MEM_RELEASE);
            }
            UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                Value: alias_base as *mut _,
            });
            CloseHandle(section as *mut std::ffi::c_void);
        }
    }

    #[test]
    fn dirty4k_tracks_kernel_subpages_and_clears_on_whole_write() {
        DIRTY4K_IN_TEST.set(1);
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                DIRTY4K_IN_TEST.set(u8::MAX);
            }
        }
        let _reset = Reset;

        // SAFETY: fresh write-watched allocation freed before returning.
        let p = unsafe {
            VirtualAlloc(
                std::ptr::null(),
                2 * PAGE,
                MEM_RESERVE | MEM_COMMIT | MEM_WRITE_WATCH,
                PAGE_READWRITE,
            )
        } as usize;
        assert_ne!(p, 0);
        clear_test_page_stamps(p, 2 * PAGE);
        let page0 = p / PAGE;
        let (a0, _) = writes(p, p + 2 * PAGE, false).expect("watched");
        // Write 4 KiB sub-pages 2 and 5 of page 0.
        // SAFETY: committed pages of our allocation.
        unsafe {
            ((p + 2 * KERNEL_PAGE + 4) as *mut u8).write_volatile(1);
            ((p + 5 * KERNEL_PAGE + 4) as *mut u8).write_volatile(2);
        }
        let (a1, found1) = writes(p, p + 2 * PAGE, false).expect("watched");
        assert!(a1 > a0);
        assert_eq!(found1, vec![(page0, a1)]);
        let subs = subpages(p, p + 2 * PAGE);
        assert_eq!(subs, vec![(page0, a1, 0, (1 << 2) | (1 << 5))]);

        // Second write to sub-page 11: `before` must record `a1`.
        // SAFETY: committed page of our allocation.
        unsafe { ((p + 11 * KERNEL_PAGE + 4) as *mut u8).write_volatile(3) };
        let (a2, found2) = writes(p, p + 2 * PAGE, false).expect("watched");
        assert!(a2 > a1);
        assert_eq!(found2, vec![(page0, a2)]);
        assert_eq!(subpages(p, p + 2 * PAGE), vec![(page0, a2, a1, 1 << 11)]);

        // Whole-page write clears `SUB_PAGES` so refreshes fall back to whole page.
        let a3 = next_asking();
        written_set(page0, a3);
        assert!(subpages(p, p + 2 * PAGE).is_empty());

        clear_test_page_stamps(p, 2 * PAGE);
        title_memory_remapped(p, 2 * PAGE);
        // SAFETY: our allocation.
        unsafe {
            VirtualFree(p as *mut _, 0, MEM_RELEASE);
        }
    }

    #[test]
    fn port_guard_bits_and_guard_blocks_invariants() {
        static TB: Bits = Bits::new();
        assert!(TB.try_set(5));
        assert!(!TB.try_set(5));
        assert_eq!(TB.set_was(5), Some(true));
        assert_eq!(TB.set_was(64), Some(false));
        assert_eq!(TB.set_in(0, 128).collect::<Vec<_>>(), vec![5, 64]);
        assert_eq!(TB.count(), 2);
        assert!(TB.clear_was(5));
        assert!(!TB.clear_was(5));
        assert_eq!(TB.count(), 1);
        assert!(TB.clear_was(64));
        assert_eq!(TB.count(), 0);

        let gb = GuardBlocks::new();
        assert!(gb.is_empty());
        gb.insert_range(10, 20);
        gb.insert_range(30, 40);
        assert!(gb.covers(10, 20) && !gb.covers(10, 21) && !gb.covers(20, 30));
        assert_eq!(gb.run_end_in(15, 50), Some(20));
        assert_eq!(gb.run_end_in(20, 50), None);
        assert_eq!(gb.first_in(20, 50), Some(30));
        assert_eq!(gb.count_in(15, 35), 10);
        gb.insert_range(20, 30);
        assert!(gb.covers(10, 40));
        assert_eq!(gb.take_range(15, 25), vec![(15, 25)]);
        assert!(gb.covers(10, 15) && gb.covers(25, 40) && !gb.intersects(15, 25));
        assert!(gb.insert(20) && gb.contains(20) && gb.remove(20) && !gb.remove(20));
    }

    #[test]
    fn port_guard_shared_lock_and_claims_preserve_writes_and_aliases() {
        use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, FILE_MAP_WRITE, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile,
            UnmapViewOfFile,
        };
        const SIZE: usize = 4 * BLOCK;
        // SAFETY: fresh pagefile section mapped twice for aliased view testing.
        let (section, one, two) = unsafe {
            let section = CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                std::ptr::null(),
                PAGE_READWRITE,
                0,
                SIZE as u32,
                std::ptr::null(),
            );
            assert!(!section.is_null());
            let one = MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, SIZE).Value as usize;
            let two = MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, SIZE).Value as usize;
            assert_ne!(one, 0);
            assert_ne!(two, 0);
            crate::inplace::title_memory_view(one, SIZE, section as usize, 0);
            crate::inplace::title_memory_view(two, SIZE, section as usize, 0);
            (section, one, two)
        };

        let faults_before = PORT6[P6_FAULTS].load(std::sync::atomic::Ordering::Relaxed);
        let guards_before = PORT6[P6_GUARDS].load(std::sync::atomic::Ordering::Relaxed);

        std::thread::scope(|s| {
            for t in 0..4usize {
                s.spawn(move || {
                    PORT_GUARD_IN_TEST.set(true);
                    let view = if t % 2 == 0 { one } else { two };
                    for round in 0..32usize {
                        let _ = writes(view, view + SIZE, true).expect("view watched");
                        let off = ((t + round) % 4) * BLOCK + 16;
                        // SAFETY: inside our live mapped view; VEH handler opens it under shared lock.
                        unsafe { ((view + off) as *mut u8).write_volatile((round & 0xff) as u8) };
                    }
                    PORT_GUARD_IN_TEST.set(false);
                });
            }
        });

        assert!(
            PORT6[P6_FAULTS].load(std::sync::atomic::Ordering::Relaxed) > faults_before,
            "expected faults opened under shared lock"
        );
        assert!(
            PORT6[P6_GUARDS].load(std::sync::atomic::Ordering::Relaxed) > guards_before,
            "expected guards done under shared lock"
        );

        clear_test_page_stamps(one, SIZE);
        clear_test_page_stamps(two, SIZE);
        title_memory_remapped(one, SIZE);
        title_memory_remapped(two, SIZE);
        // SAFETY: our views and section.
        unsafe {
            UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                Value: one as *mut _,
            });
            UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                Value: two as *mut _,
            });
            CloseHandle(section);
        }
    }
}

#[cfg(test)]
mod mapping_tests {
    use super::{
        SHARDS, shards, title_memory_reading, title_memory_reading_now, title_memory_reading_range,
        title_memory_remapping, title_memory_remapping_range,
    };
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, mpsc};
    use std::time::Duration;

    /// One of these tests at a time: the first takes every lock, which the
    /// second's "at once" would meet.
    static ONE_AT_A_TIME: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// A change of mapping waits for a read in place that is under way, and
    /// a read in place waits for a change of mapping.
    #[test]
    fn reads_in_place_and_remappings_wait_for_each_other() {
        let _alone = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
        // A reader holds; the change does not begin until it lets go.
        let changed = Arc::new(AtomicBool::new(false));
        let reading = title_memory_reading();
        let (began, wait) = mpsc::channel();
        let change = {
            let changed = changed.clone();
            std::thread::spawn(move || {
                began.send(()).unwrap();
                let _changing = title_memory_remapping();
                changed.store(true, Ordering::SeqCst);
            })
        };
        wait.recv().unwrap();
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            !changed.load(Ordering::SeqCst),
            "the mapping changed under a read in place"
        );
        drop(reading);
        change.join().unwrap();
        assert!(changed.load(Ordering::SeqCst));
        // A change holds; the read does not begin until it is done.
        let read = Arc::new(AtomicBool::new(false));
        let changing = title_memory_remapping();
        let (began, wait) = mpsc::channel();
        let reader = {
            let read = read.clone();
            std::thread::spawn(move || {
                began.send(()).unwrap();
                let _reading = title_memory_reading();
                read.store(true, Ordering::SeqCst);
            })
        };
        wait.recv().unwrap();
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            !read.load(Ordering::SeqCst),
            "a read in place began during a change of mapping"
        );
        drop(changing);
        reader.join().unwrap();
        assert!(read.load(Ordering::SeqCst));
    }

    /// The locks a range takes: those of its 16 MB spans, ascending, and
    /// all of them for the whole of title memory or a range that wraps.
    #[test]
    fn a_range_takes_the_locks_of_its_spans() {
        let mb = 1 << 20;
        assert_eq!(shards(0, 0).len(), SHARDS);
        assert_eq!(shards(0x1_0000_0000 + 3 * mb, mb), vec![0]);
        assert_eq!(shards(0x1_0000_0000 + 15 * mb, 2 * mb), vec![0, 1]);
        // Spans that wrap past the last lock come back to the first.
        assert_eq!(shards(255 << 24, 32 * mb), vec![0, 255]);
        assert_eq!(shards(0, 256 << 24).len(), SHARDS);
    }

    /// A read in place holds off a change of the addresses it reads, and
    /// only those: a change elsewhere goes on meanwhile.
    #[test]
    fn a_read_holds_off_changes_of_its_own_addresses_only() {
        let _alone = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
        // Addresses no other test takes, far apart in the locks.
        let (here, there) = (0x7_0000_0000 + (40 << 24), 0x7_0000_0000 + (90 << 24));
        let reading = title_memory_reading_range(here, 64 << 10);
        // Elsewhere: at once.
        let elsewhere =
            std::thread::spawn(move || drop(title_memory_remapping_range(there, 64 << 10)));
        elsewhere.join().unwrap();
        // Here: not until the read lets go.
        let changed = Arc::new(AtomicBool::new(false));
        let change = {
            let changed = changed.clone();
            std::thread::spawn(move || {
                let _changing = title_memory_remapping_range(here + (32 << 10), 1);
                changed.store(true, Ordering::SeqCst);
            })
        };
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            !changed.load(Ordering::SeqCst),
            "the mapping changed under a read in place"
        );
        drop(reading);
        change.join().unwrap();
        // A change holds its addresses: a read there cannot be had at once,
        // one elsewhere can.
        let changing = title_memory_remapping_range(here, 1);
        let (asked, answer) = mpsc::channel();
        std::thread::spawn(move || {
            asked
                .send((
                    title_memory_reading_now(here, 1).is_some(),
                    title_memory_reading_now(there, 1).is_some(),
                ))
                .unwrap();
        });
        assert_eq!(answer.recv().unwrap(), (false, true));
        drop(changing);
    }
}
