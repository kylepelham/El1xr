//! (feedcard, #88) `VICTORY_STALE_BATCH=1`: a refresh that copies a copy's
//! stale pages run by run (`Shadow::copy_stale`, one `Shadow::copy` per
//! run, 4 KiB runs with `VICTORY_DIRTY_4K`) notes what each run changed
//! here, on its own thread, and the copy takes the notes once at the end
//! of the refresh: one lock of its kept pieces and one of its card ranges
//! for the refresh, not two for every run. The ranges, their order and
//! the merge are those of the per-run calls (`push_dirty` is the merge
//! `Shadow::card_changed_runs` does), so the card gets the same ranges.
//! Off by default.

use std::cell::RefCell;

static ON: victory_core::settings::Live =
    victory_core::settings::Live::new("VICTORY_STALE_BATCH", |v| u64::from(v == Some("1")));

/// Live: read once per refresh (`begin`).
pub(crate) fn on() -> bool {
    ON.get_bool()
}

/// What the runs of one refresh changed, in the order the runs were copied.
#[derive(Default, Debug, PartialEq)]
pub(crate) struct Notes {
    /// For the kept pieces (`Shadow::changed`): `[from, to)` offsets.
    pub(crate) changed: Vec<(usize, usize)>,
    /// For the card (`Shadow::card_changed_runs` after its coalesce):
    /// `[from, to)` offsets.
    pub(crate) card: Vec<(usize, usize)>,
    /// The copy whose refresh takes them (its address): a copy of another
    /// copy on this thread meanwhile (a fault's) notes as before.
    owner: usize,
}

thread_local! {
    static NOTES: RefCell<Option<Notes>> = const { RefCell::new(None) };
}

/// Notes are taken on this thread now (`Batch::end` hands them over).
pub(crate) struct Batch {
    active: bool,
}

/// Starts taking notes on this thread if the switch is on and none are
/// taken yet (a nested refresh leaves them to the outer one).
pub(crate) fn begin(owner: usize) -> Batch {
    if !on() {
        return Batch { active: false };
    }
    let active = NOTES.with(|n| {
        let mut n = n.borrow_mut();
        if n.is_some() {
            false
        } else {
            *n = Some(Notes { owner, ..Notes::default() });
            true
        }
    });
    Batch { active }
}

impl Batch {
    /// The notes taken since `begin`, if this batch took them.
    pub(crate) fn end(mut self) -> Option<Notes> {
        if !self.active {
            return None;
        }
        self.active = false;
        NOTES.with(|n| n.borrow_mut().take())
    }
}

impl Drop for Batch {
    fn drop(&mut self) {
        if self.active {
            NOTES.with(|n| n.borrow_mut().take());
        }
    }
}

/// Whether notes are taken on this thread for `owner`.
pub(crate) fn active(owner: usize) -> bool {
    NOTES.with(|n| n.borrow().as_ref().is_some_and(|n| n.owner == owner))
}

/// Notes a run's change: `[from, to)` for the kept pieces and `runs`
/// (offset, bytes; already coalesced as `card_changed_runs` would) of the
/// stretch at offset `at` for the card.
pub(crate) fn note(from: usize, to: usize, at: usize, runs: &[(usize, usize)]) {
    NOTES.with(|n| {
        let mut n = n.borrow_mut();
        let n = n.as_mut().expect("stale_batch::note outside a batch");
        if from < to {
            n.changed.push((from, to));
        }
        n.card
            .extend(runs.iter().map(|&(r, m)| (at + r, at + r + m)));
    });
}

/// The merge of `Shadow::card_changed_runs`: each range joins the last one
/// if they touch or overlap, else is pushed.
pub(crate) fn push_dirty(dirty: &mut Vec<(usize, usize)>, ranges: &[(usize, usize)]) {
    for &(from, to) in ranges {
        match dirty.last_mut() {
            Some(last) if from <= last.1 && to >= last.0 => {
                *last = (last.0.min(from), last.1.max(to))
            }
            _ => dirty.push((from, to)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verbatim the per-run merge `Shadow::card_changed_runs` does under its lock.
    fn per_run(dirty: &mut Vec<(usize, usize)>, at: usize, iter: &[(usize, usize)]) {
        for &(r, m) in iter {
            let (from, to) = (at + r, at + r + m);
            match dirty.last_mut() {
                Some(last) if from <= last.1 && to >= last.0 => {
                    *last = (last.0.min(from), last.1.max(to))
                }
                _ => dirty.push((from, to)),
            }
        }
    }

    /// Verbatim `Shadow::card_changed` (the plain copy's path).
    fn per_plain(dirty: &mut Vec<(usize, usize)>, from: usize, to: usize) {
        if from >= to {
            return;
        }
        match dirty.last_mut() {
            Some(last) if from <= last.1 && to >= last.0 => {
                *last = (last.0.min(from), last.1.max(to))
            }
            _ => dirty.push((from, to)),
        }
    }

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: u64) -> usize {
            (self.next() % n) as usize
        }
    }

    /// Random refreshes: runs (some plain, some with diff runs, in increasing
    /// and sometimes repeated stretches) noted and pushed once give exactly
    /// the card ranges and kept ranges the per-run calls give, starting from
    /// a dirty list that already holds ranges.
    #[test]
    fn batched_notes_give_the_per_run_ranges() {
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        for _ in 0..3000 {
            let mut start_dirty = Vec::new();
            for _ in 0..rng.below(4) {
                let a = rng.below(1 << 20);
                start_dirty.push((a, a + 1 + rng.below(8192)));
            }
            let mut want = start_dirty.clone();
            let mut want_changed = Vec::new();
            let batch = {
                // Force the batch on for the test regardless of the switch.
                NOTES.with(|n| *n.borrow_mut() = Some(Notes::default()));
                Batch { active: true }
            };
            let mut at = rng.below(1 << 16) & !4095;
            for _ in 0..1 + rng.below(40) {
                let len = 4096 * (1 + rng.below(16));
                if rng.below(4) == 0 {
                    per_plain(&mut want, at, at + len);
                    if len > 0 {
                        want_changed.push((at, at + len));
                    }
                    note(at, at + len, 0, &[(at, len)]);
                } else {
                    let mut runs = Vec::new();
                    let mut off = rng.below(64) * 32;
                    while off < len && runs.len() < 12 {
                        let m = 32 * (1 + rng.below(20));
                        let m = m.min(len - off);
                        runs.push((off, m));
                        off += m + 32 * (1 + rng.below(40));
                    }
                    if let (Some(first), Some(last)) = (runs.first(), runs.last()) {
                        let (a, b) = (at + first.0, at + last.0 + last.1);
                        per_run(&mut want, at, &runs);
                        want_changed.push((a, b));
                        note(a, b, at, &runs);
                    }
                }
                at += len + 4096 * rng.below(3);
                if rng.below(10) == 0 {
                    at = at.saturating_sub(4096 * rng.below(8));
                }
            }
            let notes = batch.end().unwrap();
            let mut got = start_dirty;
            push_dirty(&mut got, &notes.card);
            assert_eq!(got, want);
            assert_eq!(notes.changed, want_changed);
        }
    }

    #[test]
    fn begin_is_off_without_the_switch_and_nests() {
        if on() {
            return;
        }
        assert!(begin(1).end().is_none());
        assert!(!active(1));
    }

    #[test]
    fn a_dropped_batch_leaves_no_notes() {
        NOTES.with(|n| *n.borrow_mut() = Some(Notes::default()));
        drop(Batch { active: true });
        assert!(NOTES.with(|n| n.borrow().is_none()));
    }

    /// Microbenchmark (ignored; `--ignored --nocapture`): what the stale copy's
    /// bytes cost by themselves, as `Shadow::copy` diffs them
    /// (`host::diff_copy_direct`), in 4 KiB runs and in 64 KiB runs, against
    /// the per-run locking and coalescing `copy` adds after each run.
    #[test]
    #[ignore]
    fn bench_stale_copy_parts() {
        use std::sync::Mutex;
        use std::time::Instant;
        const MB: usize = 86;
        let n = MB << 20;
        let src: Vec<u8> = (0..n).map(|i| (i * 31 % 251) as u8).collect();
        let mut dst = vec![0u8; n];
        let mut runs = Vec::new();
        for (label, run) in [("4 KiB", 4096usize), ("64 KiB", 65536)] {
            for pass in 0..2 {
                // pass 0: every byte differs; pass 1: none (the copy is current).
                let t = Instant::now();
                let mut off = 0;
                let mut count = 0;
                while off < n {
                    runs.clear();
                    let _ = crate::host::diff_copy_direct(
                        src[off..].as_ptr(),
                        &mut dst[off..off + run],
                        off,
                        &mut runs,
                    );
                    off += run;
                    count += 1;
                }
                let us = t.elapsed().as_micros();
                println!(
                    "diff copy {MB} MB in {label} runs ({count}), {}: {us} us = {:.2} GB/s, {:.3} us a run",
                    if pass == 0 {
                        "all differing"
                    } else {
                        "none differing"
                    },
                    n as f64 / us as f64 / 1000.0,
                    us as f64 / count as f64
                );
            }
        }
        let count = n / 4096;
        let kept = Mutex::new(vec![true; 4096]);
        let dirty = Mutex::new(Vec::<(usize, usize)>::new());
        let t = Instant::now();
        for i in 0..count {
            let r = [(0usize, 4096usize)];
            let merged = crate::shadow::coalesce_runs_for_bench(&r);
            {
                let mut k = kept.lock().unwrap();
                k[i % 4096] = false;
            }
            let mut d = dirty.lock().unwrap();
            per_run(&mut d, i * 8192, &merged);
        }
        let per_run_us = t.elapsed().as_micros();
        dirty.lock().unwrap().clear();
        let t = Instant::now();
        NOTES.with(|n| *n.borrow_mut() = Some(Notes::default()));
        let b = Batch { active: true };
        for i in 0..count {
            let r = [(0usize, 4096usize)];
            note(
                i * 8192,
                i * 8192 + 4096,
                i * 8192,
                &crate::shadow::coalesce_runs_for_bench(&r),
            );
        }
        let notes = b.end().unwrap();
        {
            let mut k = kept.lock().unwrap();
            for (i, _) in notes.changed.iter().enumerate() {
                k[i % 4096] = false;
            }
        }
        push_dirty(&mut dirty.lock().unwrap(), &notes.card);
        let batched_us = t.elapsed().as_micros();
        println!(
            "notes for {count} runs, one thread, no contention: per run {per_run_us} us, batched {batched_us} us"
        );
    }
}
