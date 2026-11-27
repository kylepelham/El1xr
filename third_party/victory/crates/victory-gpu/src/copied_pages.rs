//! Lock-free per-page copy timestamps for [`crate::shadow::Shadow`].
//!
//! `Batch::flush` splits large refreshes (e.g. the 174 MiB world heap) into
//! disjoint page-aligned 1 MiB chunks run concurrently across worker threads.
//! Keeping each 64 KiB page's last-copied asking in an `AtomicU64` paired with
//! an atomic uncopied bitmask lets every worker check `nothing_new` and plan
//! stale runs without serializing on a per-shadow mutex.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use crate::watch::PAGE;

/// Stack-allocated run list for [`CopiedPages::plan_stale`]: avoids heap
/// allocation for up to 8 runs per refresh while spilling seamlessly to `Vec`.
pub(crate) enum SmallRuns {
    Inline {
        buf: [(usize, usize); 8],
        len: usize,
    },
    Heap(Vec<(usize, usize)>),
}

impl SmallRuns {
    pub(crate) fn new() -> Self {
        Self::Inline {
            buf: [(0, 0); 8],
            len: 0,
        }
    }

    pub(crate) fn push(&mut self, a: usize, b: usize) {
        match self {
            Self::Inline { buf, len } if *len < buf.len() => {
                buf[*len] = (a, b);
                *len += 1;
            }
            Self::Inline { buf, len } => {
                let mut v = Vec::with_capacity(16);
                v.extend_from_slice(&buf[..*len]);
                v.push((a, b));
                *self = Self::Heap(v);
            }
            Self::Heap(v) => v.push((a, b)),
        }
    }

    pub(crate) fn iter(&self) -> std::iter::Copied<std::slice::Iter<'_, (usize, usize)>> {
        match self {
            Self::Inline { buf, len } => buf[..*len].iter().copied(),
            Self::Heap(v) => v.iter().copied(),
        }
    }
}

/// Lock-free per-page last-copied write-watch asking (`0` = never copied whole),
/// paired with a 64-page uncopied bitmask so initialized subranges hit an O(1)
/// bitmask check and skip initialized pages 64 at a time.
pub(crate) struct CopiedPages {
    pub(crate) at: Box<[AtomicU64]>,
    /// Bit `i` (word `i / 64`, bit `i % 64`) is 1 iff `at[i] == 0`.
    /// Unused high bits in the final word are always 0.
    uncopied_bits: Box<[AtomicU64]>,
    uncopied: AtomicUsize,
}

impl CopiedPages {
    pub(crate) fn new(pages: usize) -> Self {
        let words = pages.div_ceil(64);
        let mut uncopied_bits = Vec::with_capacity(words);
        for w in 0..words {
            let mask = if w + 1 == words && pages % 64 != 0 {
                (1u64 << (pages % 64)) - 1
            } else {
                u64::MAX
            };
            uncopied_bits.push(AtomicU64::new(mask));
        }
        let at: Vec<AtomicU64> = (0..pages).map(|_| AtomicU64::new(0)).collect();
        Self {
            at: at.into_boxed_slice(),
            uncopied_bits: uncopied_bits.into_boxed_slice(),
            uncopied: AtomicUsize::new(pages),
        }
    }

    #[inline]
    pub(crate) fn get(&self, index: usize) -> Option<u64> {
        self.at.get(index).map(|a| a.load(Ordering::Acquire))
    }

    /// Sets page `index`'s copied asking to `now`.
    ///
    /// Ordering invariants:
    /// - `0 -> nonzero`: stores `now` into `at[index]` BEFORE clearing the bit
    ///   in `uncopied_bits` and decrementing `uncopied`, so any reader seeing
    ///   `range_all_copied == true` is guaranteed to read a nonzero stamp.
    /// - `nonzero -> 0`: sets the bit in `uncopied_bits` and increments
    ///   `uncopied` BEFORE zeroing `at[index]`, preserving the same invariant.
    #[inline]
    pub(crate) fn set(&self, index: usize, now: u64) {
        let Some(slot) = self.at.get(index) else {
            return;
        };
        let bit = 1u64 << (index % 64);
        let word = &self.uncopied_bits[index / 64];
        if now == 0 {
            if slot.load(Ordering::Acquire) == 0 {
                return;
            }
            self.uncopied.fetch_add(1, Ordering::AcqRel);
            word.fetch_or(bit, Ordering::AcqRel);
            let mut cur = slot.load(Ordering::Acquire);
            loop {
                if cur == 0 {
                    self.uncopied.fetch_sub(1, Ordering::AcqRel);
                    return;
                }
                match slot.compare_exchange_weak(cur, 0, Ordering::AcqRel, Ordering::Acquire) {
                    Ok(_) => return,
                    Err(next) => cur = next,
                }
            }
        } else {
            let mut cur = slot.load(Ordering::Acquire);
            loop {
                match slot.compare_exchange_weak(cur, now, Ordering::AcqRel, Ordering::Acquire) {
                    Ok(_) => {
                        if cur == 0 {
                            word.fetch_and(!bit, Ordering::Release);
                            self.uncopied.fetch_sub(1, Ordering::Release);
                        }
                        return;
                    }
                    Err(next) => cur = next,
                }
            }
        }
    }

    /// Advances page `index` to `now` iff its current stamp is nonzero and
    /// `>= before`. Returns `true` if updated (used by `Shadow::holds`).
    pub(crate) fn advance_if_at_least(&self, index: usize, before: u64, now: u64) -> bool {
        let Some(slot) = self.at.get(index) else {
            return false;
        };
        if now == 0 {
            return false;
        }
        let mut cur = slot.load(Ordering::Acquire);
        loop {
            if cur == 0 || cur < before {
                return false;
            }
            match slot.compare_exchange_weak(cur, now, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return true,
                Err(next) => cur = next,
            }
        }
    }

    pub(crate) fn all_uncopied(&self) -> bool {
        self.uncopied.load(Ordering::Acquire) == self.at.len()
    }

    #[cfg(test)]
    pub(crate) fn all_copied(&self) -> bool {
        self.uncopied.load(Ordering::Acquire) == 0
    }

    #[cfg(feature = "diagnostics")]
    pub(crate) fn copied_count(&self) -> usize {
        self.at
            .len()
            .saturating_sub(self.uncopied.load(Ordering::Acquire))
    }

    /// Whether every page index in `lo..=hi` has been copied whole at least once.
    #[inline]
    pub(crate) fn range_all_copied(&self, lo: usize, hi: usize) -> bool {
        if self.uncopied.load(Ordering::Acquire) == 0 {
            return true;
        }
        let first_w = lo / 64;
        let last_w = hi / 64;
        let lo_mask = u64::MAX << (lo % 64);
        let hi_mask = u64::MAX >> (63 - (hi % 64));
        if first_w == last_w {
            return (self.uncopied_bits[first_w].load(Ordering::Acquire) & lo_mask & hi_mask) == 0;
        }
        if (self.uncopied_bits[first_w].load(Ordering::Acquire) & lo_mask) != 0 {
            return false;
        }
        for word in &self.uncopied_bits[first_w + 1..last_w] {
            if word.load(Ordering::Acquire) != 0 {
                return false;
            }
        }
        (self.uncopied_bits[last_w].load(Ordering::Acquire) & hi_mask) == 0
    }

    /// Lowest page index `>= from_idx` and `<= hi` that has never been copied whole.
    pub(crate) fn next_uncopied(&self, from_idx: usize, hi: usize) -> Option<usize> {
        if from_idx > hi || self.uncopied.load(Ordering::Acquire) == 0 {
            return None;
        }
        let mut w = from_idx / 64;
        let last_w = hi / 64;
        let mut word =
            self.uncopied_bits[w].load(Ordering::Acquire) & (u64::MAX << (from_idx % 64));
        loop {
            if w == last_w {
                word &= u64::MAX >> (63 - (hi % 64));
            }
            if word != 0 {
                return Some(w * 64 + word.trailing_zeros() as usize);
            }
            if w == last_w {
                return None;
            }
            w += 1;
            word = self.uncopied_bits[w].load(Ordering::Acquire);
        }
    }

    /// Plans the coalesced byte ranges `[run_from, run_to)` inside `[from, to)`
    /// that must be copied from title memory, updating whole-page copied
    /// timestamps to `now`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn plan_stale(
        &self,
        base: usize,
        start: usize,
        end: usize,
        from: usize,
        to: usize,
        now: u64,
        written: &[(usize, u64)],
    ) -> SmallRuns {
        self.plan_stale_filtered(base, start, end, from, to, now, written, |_| false)
    }

    /// Like [`Self::plan_stale`], skipping page indices for which `skip_idx`
    /// returns `true` (used by lazy GPU-resident write-back so `DEFERRED`
    /// pages are neither copied from title memory nor coalesced across).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn plan_stale_filtered(
        &self,
        base: usize,
        start: usize,
        end: usize,
        from: usize,
        to: usize,
        now: u64,
        written: &[(usize, u64)],
        skip_idx: impl Fn(usize) -> bool,
    ) -> SmallRuns {
        let mut runs = SmallRuns::new();
        if from >= to {
            return runs;
        }
        let first_page = from / PAGE;
        let last = (to - 1) / PAGE;
        let lo = first_page - base;
        let hi = last - base;

        let push_page =
            |runs: &mut SmallRuns, run: &mut Option<(usize, usize)>, page: usize| match *run {
                Some((_, run_end)) if page < run_end => {}
                Some((run_start, run_end)) if page == run_end => {
                    *run = Some((run_start, page + 1));
                }
                Some((run_start, run_end)) => {
                    runs.push((run_start * PAGE).max(from), (run_end * PAGE).min(to));
                    *run = Some((page, page + 1));
                }
                None => *run = Some((page, page + 1)),
            };

        let mut run: Option<(usize, usize)> = None;
        let w_start = if written.len() > 8 {
            written.partition_point(|&(p, _)| p < first_page)
        } else {
            0
        };

        // Fast path: every page in `[from, to)` has been initialized at least
        // once (even if other pages of the same buffer/window have not).
        if self.range_all_copied(lo, hi) {
            if written.is_empty() {
                return runs;
            }
            for &(page, write) in &written[w_start..] {
                if page < first_page {
                    continue;
                }
                if page > last {
                    break;
                }
                let idx = page - base;
                if skip_idx(idx) || write <= self.at[idx].load(Ordering::Acquire) {
                    continue;
                }
                if (page * PAGE).max(start) >= from && ((page + 1) * PAGE).min(end) <= to {
                    self.set(idx, now);
                }
                push_page(&mut runs, &mut run, page);
            }
            if let Some((run_start, run_end)) = run {
                runs.push((run_start * PAGE).max(from), (run_end * PAGE).min(to));
            }
            return runs;
        }

        // Sparse merge of uncopied pages (via 64-page bitmask) and dirty
        // `written` entries in `[first_page, last]`.
        let mut wi = w_start;
        while wi < written.len() && written[wi].0 < first_page {
            wi += 1;
        }
        let mut next_u = self.next_uncopied(lo, hi);
        loop {
            while wi < written.len() && written[wi].0 <= last {
                let (wp, write) = written[wi];
                let idx = wp - base;
                let prev = self.at[idx].load(Ordering::Acquire);
                if prev == 0 || write > prev {
                    break;
                }
                wi += 1;
            }
            let next_w =
                (wi < written.len() && written[wi].0 <= last).then(|| written[wi].0 - base);
            let idx = match (next_u, next_w) {
                (Some(u), Some(w)) if u < w => {
                    next_u = self.next_uncopied(u + 1, hi);
                    u
                }
                (Some(u), Some(w)) if w < u => {
                    wi += 1;
                    w
                }
                (Some(u), Some(_)) => {
                    next_u = self.next_uncopied(u + 1, hi);
                    wi += 1;
                    u
                }
                (Some(u), None) => {
                    next_u = self.next_uncopied(u + 1, hi);
                    u
                }
                (None, Some(w)) => {
                    wi += 1;
                    w
                }
                (None, None) => break,
            };
            if skip_idx(idx) {
                continue;
            }
            let page = base + idx;
            if (page * PAGE).max(start) >= from && ((page + 1) * PAGE).min(end) <= to {
                self.set(idx, now);
            }
            push_page(&mut runs, &mut run, page);
        }
        if let Some((run_start, run_end)) = run {
            runs.push((run_start * PAGE).max(from), (run_end * PAGE).min(to));
        }
        runs
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn concurrent_disjoint_chunks_update_cleanly() {
        const PAGES: usize = 256;
        let cp = Arc::new(CopiedPages::new(PAGES));
        assert!(cp.all_uncopied());
        let handles: Vec<_> = (0..8)
            .map(|t| {
                let cp = Arc::clone(&cp);
                std::thread::spawn(move || {
                    let lo = t * 32;
                    let hi = lo + 32;
                    for i in lo..hi {
                        cp.set(i, (t as u64 + 1) * 10);
                    }
                    assert!(cp.range_all_copied(lo, hi - 1));
                    for i in lo..hi {
                        assert_eq!(cp.get(i), Some((t as u64 + 1) * 10));
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert!(cp.all_copied());
        assert!(!cp.all_uncopied());
        assert!(cp.advance_if_at_least(5, 10, 99));
        assert_eq!(cp.get(5), Some(99));
        assert!(!cp.advance_if_at_least(5, 100, 120));
        assert_eq!(cp.get(5), Some(99));
        cp.set(5, 0);
        assert!(!cp.all_copied());
        assert!(!cp.range_all_copied(0, 10));
        assert_eq!(cp.next_uncopied(0, PAGES - 1), Some(5));
        assert!(!cp.advance_if_at_least(5, 0, 50));
    }
}
