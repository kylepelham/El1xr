//! Reservation-scoped, bounded submission queries. No allocation metadata
//! survives a pass. Only successful atomic GetWriteWatch resets are cached.
use super::*;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

static STATS: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

/// (worldstream) `VICTORY_ASK_OUTSIDE=1`: the asking's kernel calls made without the journal's lock.
pub(super) mod outside;

pub(super) fn enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var_os("VICTORY_WATCH_RESERVATION").is_some_and(|v| v == "1")
            && !["VICTORY_VIEW_ASK", "VICTORY_GUARD_ASK"]
                .iter()
                .any(|name| std::env::var_os(name).is_some_and(|v| v == "1"))
    })
}

fn stats() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_WATCH_STATS").is_some_and(|v| v == "1"))
}

pub(super) fn count_kernel(pages: usize) {
    if stats() {
        STATS[0].fetch_add(1, Relaxed);
        STATS[1].fetch_add(pages as u64, Relaxed);
    }
}

pub(crate) fn line(frames: f64) {
    super::ask_split::line(frames);
    if stats() && frames > 0.0 {
        let _journal = JOURNAL.read().unwrap_or_else(|e| e.into_inner());
        let [calls, pages, regions] = STATS.each_ref().map(|n| n.swap(0, Relaxed) as f64 / frames);
        tracing::info!(
            calls,
            pages,
            regions,
            "write watch/frame: GetWriteWatch calls, 4 KiB pages queried, allocation queries"
        );
    }
}

pub(super) fn covered(j: &Journal, first: usize, last: usize) -> bool {
    if j.ordered {
        let i = j.ahead.partition_point(|&(_, end)| end < last);
        j.ahead.get(i).is_some_and(|&(start, _)| start <= first)
    } else {
        j.ahead.iter().any(|&(a, b)| a <= first && last <= b)
    }
}

fn region(page: usize) -> Option<(usize, usize, bool)> {
    let asked = super::ask_split::asking().then(std::time::Instant::now);
    let answer = region_query(page);
    if let Some(asked) = asked {
        super::ask_split::add_us(super::ask_split::REGION_US, asked);
        super::ask_split::add(super::ask_split::REGIONS, 1);
    }
    answer
}

fn region_query(page: usize) -> Option<(usize, usize, bool)> {
    use windows_sys::Win32::System::Memory::{
        MemoryRegionInfo, QueryVirtualMemoryInformation, WIN32_MEMORY_REGION_INFORMATION,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    // SAFETY: plain output struct filled by QueryVirtualMemoryInformation.
    let mut info: WIN32_MEMORY_REGION_INFORMATION = unsafe { std::mem::zeroed() };
    if stats() {
        STATS[2].fetch_add(1, Relaxed);
    }
    // SAFETY: query only; matching writable output and valid address arithmetic.
    if unsafe {
        QueryVirtualMemoryInformation(
            GetCurrentProcess(),
            page.checked_mul(PAGE)? as *const _,
            MemoryRegionInfo,
            (&raw mut info).cast(),
            size_of_val(&info),
            std::ptr::null_mut(),
        )
    } == 0
    {
        return None;
    }
    let base = info.AllocationBase as usize;
    let end = base.checked_add(info.RegionSize)?;
    // SAFETY: a successful query initialized the union flags.
    Some((
        base.div_ceil(PAGE),
        end / PAGE,
        unsafe { info.Anonymous.Flags } & 1 != 0,
    ))
}

fn can_join(first: usize, last: usize, next: (usize, usize)) -> bool {
    last.max(next.1).saturating_sub(first) <= AHEAD_MAX
        && next.0.saturating_sub(last) <= AHEAD_GAP.max((last - first).min(next.1 - next.0))
}

fn keep(ahead: &mut Vec<(usize, usize)>, pieces: &[(usize, usize)]) {
    let mut same_call = false;
    for &(start, end) in pieces {
        if let Some((_, last)) = ahead.last_mut() {
            if end <= *last {
                continue;
            }
            if same_call && start <= *last {
                *last = end;
                continue;
            }
        }
        ahead.push((start, end));
        same_call = true;
    }
}

/// Queries additional uncovered private page spans `(first_page, last_page)`
/// while an `ask_once` pass is already active, merging any successful atomic
/// `GetWriteWatch` resets into `j.ahead` so parallel workers do not issue
/// individual `GetWriteWatch` syscalls during `Batch::flush`.
pub(super) fn ask_more(extra: &[(usize, usize)]) {
    if extra.is_empty() {
        return;
    }
    let Some(_mapping) = title_memory_reading_now(0, 0) else {
        return;
    };
    let mut pieces: Vec<(usize, usize)> = extra
        .iter()
        .copied()
        .filter(|&(a, b)| {
            a < b
                && a.checked_mul(PAGE).is_some_and(|start| {
                    b.checked_mul(PAGE)
                        .is_some_and(|end| start < end && !known_view(start))
                })
        })
        .collect();
    if pieces.is_empty() {
        return;
    }
    pieces.sort_unstable();
    pieces.dedup();
    let mut j = JOURNAL.write().unwrap_or_else(|e| e.into_inner());
    if !j.kept {
        return;
    }
    pieces.retain(|&(a, b)| !covered(&j, a, b));
    if pieces.is_empty() {
        return;
    }
    j.ahead_asked = next_asking();
    let mut added: Vec<(usize, usize)> = Vec::with_capacity(pieces.len());
    let mut slice: &[(usize, usize)] = &pieces;
    let mut allocation = None;
    while let Some(&(first, end)) = slice.first() {
        if allocation.is_none_or(|(a, b, _)| first < a || first >= b) {
            allocation = if slice.get(1).is_some_and(|&n| can_join(first, end, n))
                || known_view(first * PAGE)
            {
                region(first)
            } else {
                None
            };
        }
        let (_, limit, private) = allocation.unwrap_or((first, end, true));
        let mut last = end;
        let mut n = 1;
        while let Some(&next) = slice.get(n) {
            if last > limit || next.1 > limit || !can_join(first, last, next) {
                break;
            }
            last = last.max(next.1);
            n += 1;
        }
        if private
            && last <= limit
            && last - first <= AHEAD_MAX
            && !known_view(first * PAGE)
            && ask_kernel(&mut j, first, last)
        {
            keep(&mut added, &slice[..n]);
        } else if private && n > 1 {
            for &piece in &slice[..n] {
                if piece.1 - piece.0 <= AHEAD_MAX
                    && !covered(&j, piece.0, piece.1)
                    && !known_view(piece.0 * PAGE)
                    && ask_kernel(&mut j, piece.0, piece.1)
                {
                    keep(&mut added, &[piece]);
                }
            }
        }
        slice = &slice[n..];
    }
    if !added.is_empty() {
        j.ahead.extend(added);
        j.ahead.sort_unstable();
        let mut merged: Vec<(usize, usize)> = Vec::with_capacity(j.ahead.len());
        for &(a, b) in &j.ahead {
            match merged.last_mut() {
                Some((_, last)) if a <= *last => *last = (*last).max(b),
                _ => merged.push((a, b)),
            }
        }
        j.ahead = merged;
        j.ordered = true;
    }
}

pub(super) fn ask(ranges: &[(usize, usize)]) {
    use super::ask_split::{self as split, add_us};
    let started = std::time::Instant::now();
    let _asking = split::Asking::begin();
    ask_timed(ranges, started);
    add_us(split::ASK_US, started);
}

fn ask_timed(ranges: &[(usize, usize)], started: std::time::Instant) {
    use super::ask_split::{self as split, add, add_us};
    // Sort and remove duplicates before taking the global mapping permit.
    let mut pieces: Vec<_> = ranges
        .iter()
        .filter(|&&(a, b)| a < b)
        .map(|&(a, b)| (a / PAGE, b.div_ceil(PAGE)))
        .collect();
    pieces.sort_unstable();
    pieces.dedup();
    let Some(_mapping) = title_memory_reading_now(0, 0) else {
        add(split::BUSY, 1);
        return;
    };
    // Classify entire 64 KiB pieces (the actual kernel-query range).
    pieces.retain(|&(a, b)| {
        let Some(start) = a.checked_mul(PAGE) else {
            return false;
        };
        let Some(end) = b.checked_mul(PAGE) else {
            return false;
        };
        // Upstream holds the global mapping read permit throughout this pass.
        // Mapped views remain on the existing guard path.
        start < end && !known_view(start)
    });
    add(split::PIECES, pieces.len() as u64);
    add_us(split::FILTER_US, started);
    if outside::on() {
        outside::ask(&pieces);
        return;
    }
    let locking = std::time::Instant::now();
    let mut j = JOURNAL.write().unwrap_or_else(|e| e.into_inner());
    add_us(split::LOCK_US, locking);
    run(&mut j, &pieces);
    KEPT.store(j.kept, std::sync::atomic::Ordering::Release);
}

/// A completed external write (DirectStorage/kernel) may land after the
/// atomic reset. Publish a new journal stamp for intersecting cached pages
/// so every still-active view sees it. The regular guard and next-query
/// paths are unchanged. Called only after the actual write completed.
pub(super) fn completed_write(at: usize, len: usize) {
    if !enabled() || len == 0 || !ask_once_active() {
        return;
    }
    let first = at / PAGE;
    let last = at.saturating_add(len).div_ceil(PAGE);
    let mut j = JOURNAL.write().unwrap_or_else(|e| e.into_inner());
    if !j.kept {
        return;
    }
    // No temporary range vector or scan of disjoint earlier requests.
    let Journal {
        ahead,
        ordered,
        ahead_asked,
        ..
    } = &mut *j;
    let start = if *ordered {
        ahead.partition_point(|&(_, end)| end <= first)
    } else {
        0
    };
    let mut epoch = None;
    for &(a, b) in &ahead[start..] {
        if *ordered && a >= last {
            break;
        }
        let from = a.max(first);
        let to = b.min(last);
        if from >= to {
            continue;
        }
        let now = *epoch.get_or_insert_with(|| {
            let n = next_asking();
            *ahead_asked = n;
            n
        });
        for page in from..to {
            // An external writer may touch any subpage; `written_set` stamps
            // `WHOLE` (under `watch_4k`) and `PARTS` so no stale partial mask remains.
            written_set(page, now);
        }
    }
}

fn run(j: &mut Journal, pieces: &[(usize, usize)]) {
    j.ahead_asked = next_asking();
    j.ahead.clear();
    j.ordered = true;
    j.kept = !pieces.is_empty();
    j.ahead = plan(pieces, |first, last| ask_kernel(j, first, last));
}

/// The groups `pieces` are asked about in, each asked with `ask` (true: the
/// kernel answered for it): the ranges answered for, in order, as `ahead`.
fn plan(mut pieces: &[(usize, usize)], mut ask: impl FnMut(usize, usize) -> bool) -> Vec<(usize, usize)> {
    let mut ahead: Vec<(usize, usize)> = Vec::new();
    let covered = |ahead: &[(usize, usize)], first: usize, last: usize| {
        let i = ahead.partition_point(|&(_, end)| end < last);
        ahead.get(i).is_some_and(|&(start, _)| start <= first)
    };
    let mut allocation = None;
    while let Some(&(first, end)) = pieces.first() {
        if allocation.is_none_or(|(a, b, _)| first < a || first >= b) {
            allocation = if pieces.get(1).is_some_and(|&n| can_join(first, end, n))
                || known_view(first * PAGE)
            {
                region(first)
            } else {
                None
            };
        }
        let (_, limit, private) = allocation.unwrap_or((first, end, true));
        let mut last = end;
        let mut n = 1;
        while let Some(&next) = pieces.get(n) {
            if last > limit || next.1 > limit || !can_join(first, last, next) {
                break;
            }
            last = last.max(next.1);
            n += 1;
        }
        // Unknown/changed reservations cannot hide later requested pieces.
        // Leave failed groups to ordinary refreshes and mapped views to guards.
        if private
            && last <= limit
            && last - first <= AHEAD_MAX
            && !known_view(first * PAGE)
            && ask(first, last)
        {
            keep(&mut ahead, &pieces[..n]);
        } else if private && n > 1 {
            for &piece in &pieces[..n] {
                if piece.1 - piece.0 <= AHEAD_MAX
                    && !covered(&ahead, piece.0, piece.1)
                    && !known_view(piece.0 * PAGE)
                    && ask(piece.0, piece.1)
                {
                    keep(&mut ahead, &[piece]);
                }
            }
        }
        pieces = &pieces[n..];
    }
    ahead
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_DECOMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc,
        VirtualFree,
    };
    struct Memory(usize, usize);
    impl Memory {
        fn new(pages: usize) -> Self {
            let at = unsafe {
                VirtualAlloc(
                    std::ptr::null(),
                    pages * PAGE,
                    MEM_RESERVE | 0x200000,
                    PAGE_READWRITE,
                )
            };
            assert!(!at.is_null());
            Self(at as usize, pages)
        }
        fn commit(&self, page: usize) -> usize {
            let at = self.0 + page * PAGE;
            assert_eq!(
                unsafe { VirtualAlloc(at as *const _, KERNEL_PAGE, MEM_COMMIT, PAGE_READWRITE) }
                    as usize,
                at
            );
            at
        }
    }
    impl Drop for Memory {
        fn drop(&mut self) {
            title_memory_remapped(self.0, self.1 * PAGE);
            before_virtual_free(self.0, self.1 * PAGE);
            assert_ne!(unsafe { VirtualFree(self.0 as *mut _, 0, MEM_RELEASE) }, 0);
        }
    }

    #[test]
    #[ignore = "serial synthetic Windows kernel benchmark"]
    fn reservation_matrix() {
        for n in [100usize, 1000, 10000] {
            for fragmented in [false, true] {
                for multiple in [false, true] {
                    let stride = if fragmented { 3 } else { 1 };
                    let group = if multiple { 100 } else { n };
                    let memories: Vec<_> = (0..n.div_ceil(group))
                        .map(|_| Memory::new(group * stride))
                        .collect();
                    let at: Vec<_> = (0..n)
                        .map(|i| memories[i / group].commit((i % group) * stride))
                        .collect();
                    let mut ranges: Vec<_> = at.iter().map(|&p| (p, p + KERNEL_PAGE)).collect();
                    ranges.extend(at.iter().step_by(5).map(|&p| (p + 16, p + 128)));
                    let tracked: Vec<_> = at
                        .iter()
                        .map(|&p| Tracked {
                            start: p,
                            end: p + KERNEL_PAGE,
                            seen: Mutex::default(),
                        })
                        .collect();
                    // Initialize and drain earlier dirty bits; independent views also take them.
                    ask_once(&ranges);
                    for (t, &p) in tracked.iter().zip(&at) {
                        t.take(p, p + KERNEL_PAGE);
                    }
                    ask_once_end();
                    for (i, &p) in at.iter().enumerate() {
                        if !fragmented || i % 7 == 0 {
                            unsafe {
                                (p as *mut u8).write_volatile(0x5a);
                            }
                        }
                    }
                    for s in &STATS {
                        s.store(0, Relaxed);
                    }
                    let started = std::time::Instant::now();
                    ask_once(&ranges);
                    let ask_us = started.elapsed().as_micros();
                    let mut bytes = 0;
                    let mut copied = vec![0u8; KERNEL_PAGE];
                    for (i, (t, &p)) in tracked.iter().zip(&at).enumerate() {
                        let dirty = t.take(p, p + KERNEL_PAGE);
                        let expected = !fragmented || i % 7 == 0;
                        assert_eq!(!dirty.is_empty(), expected, "view {i}");
                        for (a, b) in dirty {
                            let len = b - a;
                            assert!(crate::host::read_into(a as *const u8, &mut copied[..len]));
                            assert_eq!(copied[0], 0x5a);
                            bytes += len;
                        }
                        assert!(t.take(p, p + KERNEL_PAGE).is_empty());
                    }
                    ask_once_end();
                    let total_us = started.elapsed().as_micros();
                    let [calls, pages, regions] = STATS.each_ref().map(|s| s.load(Relaxed));
                    eprintln!(
                        "watch_bench,grouped={},n={n},fragmented={fragmented},multiple={multiple},calls={calls},pages={pages},regions={regions},ask_us={ask_us},total_us={total_us},copied_bytes={bytes}",
                        enabled()
                    );
                }
            }
        }
    }

    #[test]
    fn reservation_gaps_and_racing_writes() {
        let memory = Memory::new(5);
        let a = memory.commit(0);
        let gap = memory.commit(2);
        let b = memory.commit(4);
        let pieces = [(a / PAGE, a / PAGE + 1), (b / PAGE, b / PAGE + 1)];
        let mut j = Journal {
            found: Vec::new(),
            ahead: Vec::new(),
            ordered: false,
            ahead_asked: 0,
            kept: false,
        };
        let stamp = |page: usize| written_in(page, page + 1).first().map(|&(_, w)| w);
        unsafe {
            (gap as *mut u8).write_volatile(1);
        }
        run(&mut j, &pieces);
        assert!(covered(&j, a / PAGE, a / PAGE + 1));
        assert!(!covered(&j, gap / PAGE, gap / PAGE + 1));
        assert_eq!(stamp(gap / PAGE), Some(j.ahead_asked));
        for value in 1..=16u8 {
            let gate = std::sync::Barrier::new(3);
            std::thread::scope(|s| {
                for at in [a, b] {
                    let gate = &gate;
                    s.spawn(move || {
                        gate.wait();
                        unsafe {
                            (at as *mut u8).write_volatile(value);
                        }
                    });
                }
                gate.wait();
                run(&mut j, &pieces);
            });
            let epoch = j.ahead_asked;
            run(&mut j, &pieces);
            assert!(stamp(a / PAGE).unwrap_or(0) >= epoch);
            assert!(stamp(b / PAGE).unwrap_or(0) >= epoch);
        }
        assert_ne!(
            unsafe { VirtualFree(b as *mut _, KERNEL_PAGE, MEM_DECOMMIT) },
            0
        );
        assert_eq!(memory.commit(4), b);
        unsafe {
            (b as *mut u8).write_volatile(0x78);
        }
        run(&mut j, &pieces);
        assert_eq!(stamp(b / PAGE), Some(j.ahead_asked));
    }

    #[test]
    #[ignore = "requires grouped ask-once in separate process"]
    fn reservation_parallel_readers_and_completed_storage() {
        assert!(enabled() && once());
        let memory = Memory::new(4);
        let a = memory.commit(0);
        let b = memory.commit(2);
        let ranges = [(a, a + KERNEL_PAGE), (b, b + KERNEL_PAGE)];
        let view = || Tracked {
            start: a,
            end: a + KERNEL_PAGE,
            seen: Mutex::default(),
        };
        let first = view();
        let second = view();
        ask_once(&ranges);
        first.take(a, a + KERNEL_PAGE);
        second.take(a, a + KERNEL_PAGE);
        ask_once_end();
        unsafe {
            (a as *mut u8).write_volatile(1);
        }
        ask_once(&ranges);
        let before = asked(a, a + KERNEL_PAGE).unwrap().0;
        std::thread::scope(|s| {
            let one = s.spawn(|| first.take(a, a + KERNEL_PAGE));
            let two = s.spawn(|| second.take(a, a + KERNEL_PAGE));
            assert_eq!(one.join().unwrap(), vec![(a, a + KERNEL_PAGE)]);
            assert_eq!(two.join().unwrap(), vec![(a, a + KERNEL_PAGE)]);
        });
        // Completion of an ordinary DirectStorage destination is announced
        // after the write; both overlapping readers must see the new epoch
        // even before another GetWriteWatch reset.
        unsafe {
            (a as *mut u8).write_volatile(2);
        }
        title_memory_written(a, 1, false);
        assert!(asked(a, a + KERNEL_PAGE).unwrap().0 > before);
        assert_eq!(first.take(a, a + KERNEL_PAGE), vec![(a, a + KERNEL_PAGE)]);
        assert_eq!(second.take(a, a + KERNEL_PAGE), vec![(a, a + KERNEL_PAGE)]);
        assert_ne!(
            unsafe { VirtualFree(b as *mut _, KERNEL_PAGE, MEM_DECOMMIT) },
            0
        );
        memory.commit(2);
        title_memory_remapped(b, KERNEL_PAGE);
        assert!(asked(b, b + KERNEL_PAGE).is_none());
        ask_once_end();
    }
}
