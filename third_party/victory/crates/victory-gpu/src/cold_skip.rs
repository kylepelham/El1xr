//! Refreshes of host copies that pass over the title memory nothing was
//! noted written in (`VICTORY_COLD_SKIP=1`; off by default). From
//! fxceless/victory-fork pull request 1358 (TG0000), ported via
//! Peppacodes/playablegames-peppa (`44d8d30b97`).
//!
//! A refresh walks every 64 KiB page of its range and compares the asking the
//! copy last took the page at with the asking that last found it written
//! (`crate::shadow::Shadow::copy_stale`). A large copy that many submissions
//! refresh is walked whole each time, mostly to find nothing.
//!
//! With the switch the write journal (`crate::watch`) keeps, per 2 MiB chunk,
//! a lock-free monotonic counter ([`AtomicMarks`]) bumped after every page
//! stamp update (`Release`). A copy keeps, per chunk, the mark it saw when a
//! walk of the whole chunk left every page [`fresh`] by the copy's own note
//! of it ([`Chunks::walked`]). A fresh page becomes stale only by a new note,
//! which moves the mark, or by its copy's note going back, which forgets the
//! chunk ([`forget`]). So while the mark has not moved no page of the chunk
//! is stale, and the chunk is passed over ([`Chunks::unchanged`]). The marks
//! are read (`Acquire`) before the pages in `PARTS` (`Acquire`), so a walk
//! can never pair a newer mark with an older page stamp.
//!
//! A page that is still stale after a walk took it (only part of it was in the
//! range, its page is lazy-deferred, or its last write is numbered after the
//! asking that stands) keeps its chunk walked, so it is taken again at the
//! next refresh as before.

use std::ops::Range;
use std::sync::atomic::{AtomicPtr, AtomicU64, Ordering};

/// Pages of a chunk: 2 MiB of 64 KiB pages.
pub(crate) const CHUNK_PAGES: usize = 32;

/// Chunks per 4 GiB part (`watch::PART / CHUNK_PAGES = 65536 / 32 = 2048`).
const PART_CHUNKS: usize = 2048;

/// Parts covering the 48-bit address space (matching `watch::PARTS`).
const PART_COUNT: usize = 1 << 16;

/// Whether the journal keeps chunk marks (`VICTORY_COLD_SKIP=1`).
pub(crate) fn kept() -> bool {
    static KEPT: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *KEPT.get_or_init(|| std::env::var_os("VICTORY_COLD_SKIP").is_some_and(|v| v == "1"))
}

/// Whether a refresh made now passes over chunks with no write since.
pub(crate) fn on() -> bool {
    kept()
}

/// Whether a page taken at asking `taken` (0: never) holds its last write,
/// found at asking `write` (0: none).
pub(crate) fn fresh(taken: u64, write: u64) -> bool {
    taken != 0 && write <= taken
}

/// Lock-free 2-level table of per-2 MiB chunk write marks covering the 48-bit
/// address space. Each allocated 4 GiB part is 16 KiB (`2048 * 8` bytes),
/// created lazily on first write and kept for the lifetime of the process.
pub(crate) struct AtomicMarks {
    parts: [AtomicPtr<[AtomicU64; PART_CHUNKS]>; PART_COUNT],
}

impl AtomicMarks {
    pub(crate) const fn new() -> Self {
        Self {
            parts: [const { AtomicPtr::new(std::ptr::null_mut()) }; PART_COUNT],
        }
    }

    fn part(&self, chunk: usize, make: bool) -> Option<&[AtomicU64; PART_CHUNKS]> {
        let slot = self.parts.get(chunk / PART_CHUNKS)?;
        let ptr = slot.load(Ordering::Acquire);
        if !ptr.is_null() {
            // SAFETY: non-null entries point to leaked boxed arrays kept for
            // the lifetime of `self`.
            return Some(unsafe { &*ptr });
        }
        if !make {
            return None;
        }
        let fresh = Box::into_raw(Box::new([const { AtomicU64::new(0) }; PART_CHUNKS]));
        match slot.compare_exchange(
            std::ptr::null_mut(),
            fresh,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => {
                // SAFETY: `fresh` was just published and will never be freed.
                Some(unsafe { &*fresh })
            }
            Err(existing) => {
                // SAFETY: `fresh` was never published; reclaim our losing allocation.
                unsafe { drop(Box::from_raw(fresh)) };
                // SAFETY: `existing` was published by the winning thread.
                Some(unsafe { &*existing })
            }
        }
    }

    /// `page` was noted written in `PARTS`: advance its 2 MiB chunk's mark
    /// with `Release` so any reader that observes the new mark with `Acquire`
    /// is guaranteed to see the page's updated stamp in `PARTS`.
    pub(crate) fn note_page(&self, page: usize) {
        let chunk = page / CHUNK_PAGES;
        if let Some(part) = self.part(chunk, true) {
            part[chunk % PART_CHUNKS].fetch_add(1, Ordering::Release);
        }
    }

    /// Current mark of `chunk` (0 if never written), read with `Acquire`.
    pub(crate) fn mark(&self, chunk: usize) -> u64 {
        self.part(chunk, false)
            .map_or(0, |part| part[chunk % PART_CHUNKS].load(Ordering::Acquire))
    }

    /// Appends `(chunk, mark)` in chunk order for all non-zero marks covering
    /// pages `[first, last)`.
    pub(crate) fn read(&self, first: usize, last: usize, marks: &mut Vec<(usize, u64)>) {
        if first >= last {
            return;
        }
        let mut chunk = first / CHUNK_PAGES;
        let end = last.div_ceil(CHUNK_PAGES);
        while chunk < end {
            let stop = ((chunk / PART_CHUNKS + 1) * PART_CHUNKS).min(end);
            if let Some(part) = self.part(chunk, false) {
                for c in chunk..stop {
                    let m = part[c % PART_CHUNKS].load(Ordering::Acquire);
                    if m != 0 {
                        marks.push((c, m));
                    }
                }
            }
            chunk = stop;
        }
    }
}

/// Process-wide lock-free chunk marks updated by `watch::stamp_set`.
static CHUNK_MARKS: AtomicMarks = AtomicMarks::new();

/// Advances the chunk mark for `page` after its stamp in `watch::PARTS` moved.
pub(crate) fn note_chunk(page: usize) {
    CHUNK_MARKS.note_page(page);
}

/// Current mark of 2 MiB chunk `chunk` (`Acquire`).
pub(crate) fn chunk_mark(chunk: usize) -> u64 {
    CHUNK_MARKS.mark(chunk)
}

/// Reads all non-zero chunk marks covering pages `[first, last)` (`Acquire`).
pub(crate) fn read_marks(first: usize, last: usize, marks: &mut Vec<(usize, u64)>) {
    CHUNK_MARKS.read(first, last, marks);
}

/// With `VICTORY_COLD_SKIP` on, since the last frame line: pages looked at
/// one by one, pages passed over.
static COLD_SKIP: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

/// Records `walked` and `passed` 64 KiB pages for the frame-rate log line.
pub(crate) fn count(walked: u64, passed: u64) {
    if walked != 0 {
        COLD_SKIP[0].fetch_add(walked, Ordering::Relaxed);
    }
    if passed != 0 {
        COLD_SKIP[1].fetch_add(passed, Ordering::Relaxed);
    }
}

/// Logged with the frame-rate line if any refresh went through `VICTORY_COLD_SKIP`.
pub(crate) fn line(frames: f64) {
    let [walked, passed] = [0, 1].map(|i| COLD_SKIP[i].swap(0, Ordering::Relaxed));
    if walked + passed == 0 {
        return;
    }
    tracing::info!(
        "refreshes with VICTORY_COLD_SKIP per frame: {:.0} pages of 64 KiB passed over, {:.0} looked at ({:.1}% passed over)",
        passed as f64 / frames,
        walked as f64 / frames,
        passed as f64 * 100.0 / (walked + passed) as f64
    );
}

/// Reference `BTreeMap` chunk marks (used by unit tests and pure-std rig).
#[cfg(test)]
pub(crate) struct Marks {
    by_chunk: std::collections::BTreeMap<usize, u64>,
    /// Numbers the notes.
    notes: u64,
}

#[cfg(test)]
impl Marks {
    pub(crate) const fn new() -> Self {
        Marks {
            by_chunk: std::collections::BTreeMap::new(),
            notes: 0,
        }
    }

    /// `page` was noted written: its chunk's mark moves.
    pub(crate) fn note(&mut self, page: usize) {
        self.notes += 1;
        self.by_chunk.insert(page / CHUNK_PAGES, self.notes);
    }

    /// Into `marks`, the marks of the chunks that hold pages `[first, last)`,
    /// as (chunk, mark) in order.
    pub(crate) fn read(&self, first: usize, last: usize, marks: &mut Vec<(usize, u64)>) {
        let chunks = first / CHUNK_PAGES..last.div_ceil(CHUNK_PAGES);
        marks.extend(self.by_chunk.range(chunks).map(|(&c, &m)| (c, m)));
    }
}

/// Pages `[first, last]` cut where chunks begin.
pub(crate) fn spans(first: usize, last: usize) -> impl Iterator<Item = Range<usize>> {
    (first / CHUNK_PAGES..=last / CHUNK_PAGES).map(move |chunk| {
        (chunk * CHUNK_PAGES).max(first)..((chunk + 1) * CHUNK_PAGES).min(last + 1)
    })
}

/// A copy's note of the page `page` went back (or may have): what it keeps of
/// the page's chunk is forgotten. `base`: the copy's first page; `verified`
/// as [`Chunks`] keeps it.
pub(crate) fn forget(verified: &mut [u64], base: usize, page: usize) {
    if let Some(at) = (page / CHUNK_PAGES)
        .checked_sub(base / CHUNK_PAGES)
        .and_then(|at| verified.get_mut(at))
    {
        *at = 0;
    }
}

/// What a copy knows of its chunks, for one walk.
pub(crate) struct Chunks<'a> {
    /// The copy's pages.
    pages: Range<usize>,
    /// Per chunk from the one that holds the copy's first page: the mark seen
    /// when a walk last left the chunk fresh, plus one (0: never).
    verified: &'a mut Vec<u64>,
    /// The marks read with the walk's entries ([`read_marks`]).
    marks: &'a [(usize, u64)],
}

impl<'a> Chunks<'a> {
    /// `verified` is the copy's own, empty before its first walk.
    pub(crate) fn new(
        verified: &'a mut Vec<u64>,
        pages: Range<usize>,
        marks: &'a [(usize, u64)],
    ) -> Self {
        verified.resize(
            pages.end.div_ceil(CHUNK_PAGES) - pages.start / CHUNK_PAGES,
            0,
        );
        Chunks {
            pages,
            verified,
            marks,
        }
    }

    /// Where `verified` keeps the chunk of `page`, and the chunk's mark.
    fn of(&self, page: usize) -> (usize, u64) {
        let chunk = page / CHUNK_PAGES;
        let mark = self
            .marks
            .binary_search_by_key(&chunk, |m| m.0)
            .map_or(0, |i| self.marks[i].1);
        (chunk - self.pages.start / CHUNK_PAGES, mark)
    }

    /// Whether no page of `span` (one of [`spans`]) can be stale.
    pub(crate) fn unchanged(&self, span: &Range<usize>) -> bool {
        let (at, mark) = self.of(span.start);
        self.verified[at] != 0 && mark < self.verified[at]
    }

    /// `span` was walked, and `all_fresh` says that every page of it is fresh
    /// now by the copy's own notes: remembered if it held all of the copy's
    /// pages in its chunk.
    pub(crate) fn walked(&mut self, span: &Range<usize>, all_fresh: bool) {
        let chunk = span.start / CHUNK_PAGES;
        let whole = span.start <= (chunk * CHUNK_PAGES).max(self.pages.start)
            && span.end >= ((chunk + 1) * CHUNK_PAGES).min(self.pages.end);
        if all_fresh && whole {
            let (at, mark) = self.of(span.start);
            self.verified[at] = mark + 1;
        }
    }
}

#[cfg(test)]
#[allow(clippy::single_range_in_vec_init)] // a walk's runs of one are meant
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// A copy and a journal, walked as `Shadow::copy_stale` walks them.
    struct Rig {
        pages: Range<usize>,
        copied: Vec<u64>,
        verified: Vec<u64>,
        written: BTreeMap<usize, u64>,
        marks: Marks,
        asked: u64,
    }

    /// The runs of pages a refresh copied, and the pages it passed over.
    type Walk = (Vec<Range<usize>>, usize);

    impl Rig {
        fn new(base: usize, pages: usize) -> Self {
            Rig {
                pages: base..base + pages,
                copied: vec![0; pages],
                verified: Vec::new(),
                written: BTreeMap::new(),
                marks: Marks::new(),
                asked: 0,
            }
        }

        fn ask(&mut self) -> u64 {
            self.asked += 1;
            self.asked
        }

        fn write(&mut self, page: usize) {
            let asking = self.ask();
            self.written.insert(page, asking);
            self.marks.note(page);
        }

        fn refresh(&mut self, first: usize, last: usize, skip: bool) -> Walk {
            let now = self.ask();
            self.refresh_at(now, first, last, skip, |_| true)
        }

        /// A refresh of pages `[first, last]` at asking `now`; `whole` says
        /// whether the range holds all of a page.
        fn refresh_at(
            &mut self,
            now: u64,
            first: usize,
            last: usize,
            skip: bool,
            whole: impl Fn(usize) -> bool,
        ) -> Walk {
            let mut marks = Vec::new();
            if skip {
                self.marks.read(first, last + 1, &mut marks);
            }
            let written: BTreeMap<_, _> = self
                .written
                .range(first..last + 1)
                .map(|(&p, &w)| (p, w))
                .collect();
            let mut chunks =
                skip.then(|| Chunks::new(&mut self.verified, self.pages.clone(), &marks));
            let (mut runs, mut run, mut passed) = (Vec::new(), None, 0);
            for span in spans(first, last) {
                if chunks.as_ref().is_some_and(|c| c.unchanged(&span)) {
                    runs.extend(run.take().map(|a| a..span.start));
                    passed += span.len();
                    continue;
                }
                let mut all_fresh = true;
                for page in span.clone() {
                    let write = written.get(&page).copied().unwrap_or(0);
                    let at = &mut self.copied[page - self.pages.start];
                    if !fresh(*at, write) {
                        if whole(page) {
                            *at = now;
                        }
                        run.get_or_insert(page);
                    } else {
                        runs.extend(run.take().map(|a| a..page));
                    }
                    all_fresh &= fresh(*at, write);
                }
                if let Some(chunks) = chunks.as_mut() {
                    chunks.walked(&span, all_fresh);
                }
            }
            runs.extend(run.map(|a| a..last + 1));
            (runs, passed)
        }
    }

    #[test]
    fn spans_are_cut_where_chunks_begin() {
        assert_eq!(spans(5, 5).collect::<Vec<_>>(), [5..6]);
        assert_eq!(spans(31, 64).collect::<Vec<_>>(), [31..32, 32..64, 64..65]);
    }

    #[test]
    fn the_first_walk_takes_all_and_the_next_passes_over_all() {
        let mut r = Rig::new(40, 100);
        assert_eq!(r.refresh(40, 139, true), (vec![40..140], 0));
        assert_eq!(r.refresh(40, 139, true), (vec![], 100));
    }

    #[test]
    fn a_write_has_its_chunk_walked_and_no_other() {
        let mut r = Rig::new(0, 128);
        r.refresh(0, 127, true);
        r.write(70);
        assert_eq!(r.refresh(0, 127, true), (vec![70..71], 96));
        assert_eq!(r.refresh(0, 127, true), (vec![], 128));
    }

    #[test]
    fn a_chunk_walked_in_part_is_not_remembered() {
        let mut r = Rig::new(0, 64);
        r.refresh(0, 39, true);
        assert_eq!(r.refresh(0, 63, true), (vec![40..64], 32));
    }

    #[test]
    fn a_page_taken_in_part_keeps_its_chunk_walked() {
        let mut r = Rig::new(0, 32);
        let now = r.ask();
        r.refresh_at(now, 0, 31, true, |p| p != 5);
        assert_eq!(r.refresh(0, 31, true), (vec![5..6], 0));
    }

    #[test]
    fn a_write_numbered_after_the_standing_asking_keeps_its_chunk_walked() {
        let mut r = Rig::new(0, 32);
        r.refresh(0, 31, true);
        let standing = r.ask();
        r.write(3);
        // Without the switch the page is taken at every refresh until a later
        // asking: so it is with it.
        for _ in 0..2 {
            assert_eq!(
                r.refresh_at(standing, 0, 31, true, |_| true),
                (vec![3..4], 0)
            );
        }
        assert_eq!(r.refresh(0, 31, true), (vec![3..4], 0));
        assert_eq!(r.refresh(0, 31, true), (vec![], 32));
    }

    #[test]
    fn a_passed_over_chunk_ends_a_run_where_it_starts() {
        let mut r = Rig::new(0, 96);
        r.refresh(32, 63, true);
        assert_eq!(r.refresh(0, 95, true), (vec![0..32, 64..96], 32));
    }

    #[test]
    fn a_note_gone_back_has_its_chunk_walked_again() {
        let mut r = Rig::new(0, 64);
        r.refresh(0, 63, true);
        // Another hand put an older number on page 40 (as a merge under
        // shaders' writes may) after a write it had not seen.
        r.write(40);
        r.refresh(0, 63, true);
        r.copied[40] = 1;
        forget(&mut r.verified, 0, 40);
        assert_eq!(r.refresh(0, 63, true), (vec![40..41], 32));
    }

    #[test]
    fn the_same_runs_with_and_without_the_skip() {
        let (mut on, mut off) = (Rig::new(17, 300), Rig::new(17, 300));
        let mut x: u64 = 0x2545_f491_4f6c_dd1d;
        let mut next = move |n: usize| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x % n as u64) as usize
        };
        let (mut standing, mut passed) = (None, 0);
        for _ in 0..20_000 {
            match next(8) {
                0 | 1 => {
                    let p = 17 + next(300);
                    on.write(p);
                    off.write(p);
                }
                // An asking that stands for the refreshes that follow.
                2 => {
                    let asking = on.ask();
                    assert_eq!(asking, off.ask());
                    standing = Some(asking);
                }
                3 => standing = None,
                _ => {
                    let a = 17 + next(300);
                    let b = (a + next(200)).min(316);
                    let new = on.ask();
                    assert_eq!(new, off.ask());
                    let now = standing.unwrap_or(new);
                    // The range may hold its end pages only in part.
                    let (head, tail) = (next(2) == 0, next(2) == 0);
                    let whole = move |p| !(p == a && head || p == b && tail);
                    let (runs_on, skipped) = on.refresh_at(now, a, b, true, whole);
                    let (runs_off, none) = off.refresh_at(now, a, b, false, whole);
                    assert_eq!(runs_on, runs_off);
                    assert_eq!(on.copied, off.copied);
                    assert_eq!(none, 0);
                    passed += skipped;
                }
            }
        }
        assert!(passed > 0);
    }

    #[test]
    fn every_note_moves_its_chunks_mark() {
        let mut m = Marks::new();
        let mut marks = Vec::new();
        m.read(0, 64, &mut marks);
        assert!(marks.is_empty());
        m.note(3);
        m.note(33);
        m.read(0, 64, &mut marks);
        assert_eq!(marks, [(0, 1), (1, 2)]);
        // The same page again moves its mark still.
        m.note(3);
        marks.clear();
        m.read(0, 32, &mut marks);
        assert_eq!(marks, [(0, 3)]);
    }

    #[test]
    fn atomic_marks_track_chunks_across_parts() {
        static TEST_MARKS: AtomicMarks = AtomicMarks::new();
        let m = &TEST_MARKS;
        let mut marks = Vec::new();
        // Pages spanning the 4 GiB part boundary (65536 pages = 2048 chunks).
        let boundary_page = PART_CHUNKS * CHUNK_PAGES;
        m.read(0, boundary_page + 64, &mut marks);
        assert!(marks.is_empty());

        m.note_page(3);
        m.note_page(33);
        m.note_page(boundary_page + 5);
        m.read(0, boundary_page + 64, &mut marks);
        assert_eq!(marks, [(0, 1), (1, 1), (PART_CHUNKS, 1)]);

        // Noting the same chunk again strictly increments its mark.
        m.note_page(3);
        assert_eq!(m.mark(0), 2);
        assert_eq!(m.mark(1), 1);
        assert_eq!(m.mark(2), 0);
        assert_eq!(m.mark(PART_CHUNKS), 1);

        marks.clear();
        m.read(0, 64, &mut marks);
        assert_eq!(marks, [(0, 2), (1, 1)]);
    }
}
