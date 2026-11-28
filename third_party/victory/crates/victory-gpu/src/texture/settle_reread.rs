//! A staged texture that was uploaded when a write the guards do not see
//! landed in its memory is read again whole once those writes settle
//! (`VICTORY_PAGE_SETTLE_REREAD=1`, or `=<quiet ms>`; off by default).
//!
//! Ported from darkworldhm/victory-fork-darkworldhm `08fdf0b`/`840243e`
//! (their `VICTORY_TABLED_REREAD` with `VICTORY_PAGE_SETTLE`; renamed here,
//! our `VICTORY_TABLED_REREAD` means something else). DirectStorage reads
//! land in physical pages without faulting a guard
//! (`crate::watch::title_memory_written`). The texture pass makes the
//! slots shaders pick by index due again after such a write, but leaves
//! out the slots draws read through tables (`slots_written_under`,
//! `tabled_lately`); a texture read that way sees the write only if bit 1
//! of `VICTORY_PAGE_WRITES` opened its blocks (not for reads to plain
//! addresses unless bit 4 is set), or at its hash turn. A texture
//! uploaded while its memory was still zero, before DirectStorage brought
//! its texels, can stay black.
//!
//! With the switch the ranges the texture pass takes from
//! `crate::descriptor::host::page_written` settle here: once no write has
//! landed in a range for the quiet time (200 ms by default), or 2 s after
//! its first write, every uploaded staged texture over it forgets what its
//! staging buffer was filled from, so the check at the next submission
//! that samples it reads and detiles its memory, and that list uploads it.
//! At most [`MOST`] textures and [`MOST_BYTES`] of their memory a look;
//! the rest wait for the next look.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex, Weak};

use super::{STAGING, Staging};

/// A range settles at the latest this long after its first write, written
/// since or not.
const LONGEST_MS: u64 = 2000;
/// The quiet time `1` asks for.
const QUIET_DEFAULT_MS: u64 = 200;
/// Textures one look reads again at most.
const MOST: usize = 64;
/// Bytes of title memory of the textures one look reads again at most
/// (the first whatever its size).
const MOST_BYTES: u64 = 16 << 20;
/// Ranges settling at most; past that, neighbours are joined.
const MOST_RANGES: usize = 4096;
/// Between two looks for settled ranges.
const LOOK_MS: u64 = 50;

/// Since the start: ranges noted, textures read again, their bytes, and
/// textures left for a later look (count now).
pub(crate) static SETTLE_REREAD: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

/// The quiet time in ms; 0 off.
fn quiet_ms() -> u64 {
    static MS: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *MS.get_or_init(|| {
        let ms = parse(std::env::var("VICTORY_PAGE_SETTLE_REREAD").ok().as_deref());
        if ms > 0 {
            tracing::info!(
                quiet_ms = ms,
                "staged textures under DirectStorage writes read again once the writes settle (VICTORY_PAGE_SETTLE_REREAD; darkworldhm)"
            );
        }
        ms
    })
}

fn parse(value: Option<&str>) -> u64 {
    match value.map(str::trim).and_then(|v| v.parse::<u64>().ok()) {
        Some(0) | None => 0,
        Some(1) => QUIET_DEFAULT_MS,
        Some(ms) => ms.min(LONGEST_MS),
    }
}

pub(crate) fn on() -> bool {
    quiet_ms() > 0
}

/// Ranges settling, by start: end, first write, last write (ms).
#[derive(Default)]
struct Settling {
    map: BTreeMap<u64, (u64, u64, u64)>,
}

impl Settling {
    /// A write into `[start, end)` at `now`: joined with the ranges it
    /// overlaps or touches, which keep their first write.
    fn note(&mut self, start: u64, end: u64, now: u64) {
        if start >= end {
            return;
        }
        let (mut start, mut end, mut first) = (start, end, now);
        let touching: Vec<u64> = self
            .map
            .range(..=end)
            .rev()
            .take_while(|&(_, &(e, ..))| e >= start)
            .map(|(&s, _)| s)
            .collect();
        for s in touching {
            if let Some((e, f, _)) = self.map.remove(&s) {
                start = start.min(s);
                end = end.max(e);
                first = first.min(f);
            }
        }
        self.map.insert(start, (end, first, now));
        if self.map.len() > MOST_RANGES {
            self.join_neighbours();
        }
    }

    /// Joins ranges in pairs until half of [`MOST_RANGES`] are left: more
    /// memory is looked at, none is left out.
    fn join_neighbours(&mut self) {
        while self.map.len() > MOST_RANGES / 2 {
            let old = std::mem::take(&mut self.map);
            let mut it = old.into_iter();
            while let Some((s, (e, f, l))) = it.next() {
                match it.next() {
                    Some((_, (e2, f2, l2))) => {
                        self.map.insert(s, (e.max(e2), f.min(f2), l.max(l2)));
                    }
                    None => {
                        self.map.insert(s, (e, f, l));
                    }
                }
            }
        }
    }

    /// Takes the ranges settled by `now`: no write for `quiet` ms, or
    /// [`LONGEST_MS`] since the first. By start.
    fn settled(&mut self, now: u64, quiet: u64) -> Vec<(u64, u64)> {
        let ready: Vec<u64> = self
            .map
            .iter()
            .filter(|&(_, &(_, first, last))| {
                now.saturating_sub(last) >= quiet || now.saturating_sub(first) >= LONGEST_MS
            })
            .map(|(&s, _)| s)
            .collect();
        ready
            .into_iter()
            .filter_map(|s| self.map.remove(&s).map(|(e, ..)| (s, e)))
            .collect()
    }
}

static SETTLING: Mutex<Settling> = Mutex::new(Settling {
    map: BTreeMap::new(),
});
/// Textures found under settled ranges that a look had no room for.
static LEFT: Mutex<Vec<Weak<Staging>>> = Mutex::new(Vec::new());
static LAST_LOOK: AtomicU64 = AtomicU64::new(0);

/// The texture pass took `ranges` (start, end) written behind the guards'
/// back at `now` (ms): they settle. Then, at most every [`LOOK_MS`], the
/// textures under ranges that settled are read again.
pub(crate) fn page_written(ranges: &[(u64, u64)], now: u64) {
    let quiet = quiet_ms();
    if quiet == 0 {
        return;
    }
    if !ranges.is_empty() {
        let mut settling = SETTLING.lock().unwrap_or_else(|e| e.into_inner());
        for &(start, end) in ranges {
            settling.note(start, end, now);
        }
        SETTLE_REREAD[0].fetch_add(ranges.len() as u64, Relaxed);
    }
    let last = LAST_LOOK.load(Relaxed);
    if now.saturating_sub(last) < LOOK_MS
        || LAST_LOOK
            .compare_exchange(last, now, Relaxed, Relaxed)
            .is_err()
    {
        return;
    }
    let settled = SETTLING
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .settled(now, quiet);
    let mut todo = std::mem::take(&mut *LEFT.lock().unwrap_or_else(|e| e.into_inner()));
    if !settled.is_empty() {
        todo.extend(staged_under(&settled).iter().map(Arc::downgrade));
    }
    if todo.is_empty() {
        return;
    }
    let left = reread(todo);
    SETTLE_REREAD[3].store(left.len() as u64, Relaxed);
    *LEFT.lock().unwrap_or_else(|e| e.into_inner()) = left;
}

/// Whether `[start, end)` overlaps one of `ranges` (by start, apart).
fn overlaps(ranges: &[(u64, u64)], start: u64, end: u64) -> bool {
    // The first range that ends past `start`.
    let i = ranges.partition_point(|&(_, e)| e <= start);
    ranges.get(i).is_some_and(|&(s, _)| s < end)
}

/// The uploaded staged textures whose memory overlaps `ranges`.
fn staged_under(ranges: &[(u64, u64)]) -> Vec<Arc<Staging>> {
    let map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
    let Some(map) = map.as_ref() else {
        return Vec::new();
    };
    map.values()
        .filter(|s| {
            let (at, len) = s.memory();
            s.uploaded.load(Relaxed) && overlaps(ranges, at as u64, (at + len) as u64)
        })
        .cloned()
        .collect()
}

/// Has the next check of up to [`MOST`] textures and [`MOST_BYTES`] read
/// them anew; returns the ones left for a later look.
fn reread(todo: Vec<Weak<Staging>>) -> Vec<Weak<Staging>> {
    let (mut read, mut bytes) = (0usize, 0u64);
    let mut left = Vec::new();
    for weak in todo {
        let Some(staging) = weak.upgrade() else {
            continue;
        };
        let size = staging.memory().1 as u64;
        if read >= MOST || (read > 0 && bytes.saturating_add(size) > MOST_BYTES) {
            left.push(weak);
            continue;
        }
        staging.forget();
        read += 1;
        bytes = bytes.saturating_add(size);
    }
    let total = SETTLE_REREAD[1].fetch_add(read as u64, Relaxed) + read as u64;
    let total_bytes = SETTLE_REREAD[2].fetch_add(bytes, Relaxed) + bytes;
    if read > 0 {
        static LOGGED: AtomicU64 = AtomicU64::new(0);
        let n = LOGGED.fetch_add(1, Relaxed);
        if n < 8 || n.is_multiple_of(64) {
            tracing::info!(
                read,
                left = left.len(),
                total,
                total_mb = total_bytes >> 20,
                ranges = SETTLE_REREAD[0].load(Relaxed),
                "settle reread: staged textures read again after DirectStorage writes settled (VICTORY_PAGE_SETTLE_REREAD)"
            );
        }
    }
    left
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switch_is_off_unless_set_and_one_means_200_ms() {
        assert_eq!(parse(None), 0);
        assert_eq!(parse(Some("0")), 0);
        assert_eq!(parse(Some("x")), 0);
        assert_eq!(parse(Some("1")), 200);
        assert_eq!(parse(Some(" 300 ")), 300);
        assert_eq!(parse(Some("99999")), LONGEST_MS);
    }

    /// Writes join their range and move its last write on; a range is
    /// taken once quiet, or 2 s after its first write.
    #[test]
    fn ranges_settle_once_quiet_or_old() {
        let mut s = Settling::default();
        s.note(0x1000, 0x2000, 100);
        s.note(0x2000, 0x3000, 250);
        s.note(0x8000, 0x9000, 150);
        assert_eq!(s.map.len(), 2);
        // 0x1000..0x3000 last written at 250: not quiet at 400.
        assert_eq!(s.settled(400, 200), vec![(0x8000, 0x9000)]);
        assert_eq!(s.settled(449, 200), vec![]);
        assert_eq!(s.settled(450, 200), vec![(0x1000, 0x3000)]);
        // Written every 100 ms: taken 2 s after the first write.
        for now in (10_000..=11_900).step_by(100) {
            s.note(0x5000, 0x6000, now);
            assert_eq!(s.settled(now, 200), vec![]);
        }
        s.note(0x5000, 0x6000, 12_000);
        assert_eq!(s.settled(12_000, 200), vec![(0x5000, 0x6000)]);
        assert!(s.map.is_empty());
    }

    #[test]
    fn a_full_list_joins_neighbours_and_keeps_every_byte() {
        let mut s = Settling::default();
        for i in 0..=MOST_RANGES as u64 {
            s.note(i * 0x2000, i * 0x2000 + 0x1000, 5);
        }
        assert!(s.map.len() <= MOST_RANGES / 2);
        let taken = s.settled(1_000, 200);
        for i in 0..=MOST_RANGES as u64 {
            assert!(overlaps(&taken, i * 0x2000, i * 0x2000 + 0x1000));
        }
    }

    /// An uploaded staged texture under a settled range forgets what its
    /// staging buffer was filled from (the next check reads it whole); a
    /// range elsewhere leaves it alone.
    #[test]
    fn a_staged_texture_under_a_settled_range_is_read_again() {
        use windows_sys::Win32::System::Memory::*;
        const RUN: usize = 0x1_0000;
        // SAFETY: a private allocation of our own, released at the end.
        let base = unsafe {
            VirtualAlloc(std::ptr::null(), 2 * RUN, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE)
        };
        assert!(!base.is_null());
        let at = base as u64;
        // A linear 4x4 RGBA8 texture, one level.
        let description = [3, 0, 0, 0, 4, 0, 4, 0x1_0001, 28, 1, 0, 1, 0, 0];
        let staged = crate::resource::texture_at(at, &description).and_then(|resource| {
            let texture = crate::resource::host_resource(resource)?;
            super::super::staging(resource, texture, 0)
        });
        // `None`: no GPU to make the host texture on.
        if let Some(staging) = staged {
            staging.uploaded.store(true, Relaxed);
            staging.last.lock().unwrap().asked = 7;
            let elsewhere = [(at + RUN as u64, at + 2 * RUN as u64)];
            let left = reread(staged_under(&elsewhere).iter().map(Arc::downgrade).collect());
            assert!(left.is_empty());
            assert_eq!(staging.last.lock().unwrap().asked, 7);
            let under = [(at + 0x10, at + 0x20)];
            let found = staged_under(&under);
            assert!(found.iter().any(|s| Arc::ptr_eq(s, &staging)));
            assert!(reread(found.iter().map(Arc::downgrade).collect()).is_empty());
            assert_eq!(staging.last.lock().unwrap().asked, 0, "not read again");
        }
        // SAFETY: the allocation above, released once.
        assert_ne!(unsafe { VirtualFree(base, 0, MEM_RELEASE) }, 0);
    }

    #[test]
    fn overlap_by_binary_search() {
        let ranges = [(0x1000, 0x2000), (0x5000, 0x6000)];
        assert!(overlaps(&ranges, 0x1800, 0x1900));
        assert!(overlaps(&ranges, 0x0, 0x1001));
        assert!(!overlaps(&ranges, 0x0, 0x1000));
        assert!(!overlaps(&ranges, 0x2000, 0x5000));
        assert!(overlaps(&ranges, 0x4fff, 0x7000));
        assert!(!overlaps(&ranges, 0x6000, 0x7000));
    }
}
