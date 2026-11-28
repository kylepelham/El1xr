//! (feedcard, #88) The ranges shaders of a submission not yet copied back may
//! be writing in a copy ([`super::Shadow`]'s `pending`), kept by start in a
//! set, so a look for the ranges that overlap a window visits only those that
//! can: the ones starting before the window's end and at most the longest
//! range kept before its start. It was a list scanned whole by every declared
//! range (`Shadow::writing`: once to find it, once more to cut the window,
//! with a vector made per range kept) and by every refresh and flight check
//! that met a copy with ranges pending.
//!
//! After jzvgrow-dot/Victory-4080Super `7f5f98353b` (the set) and `73aff0d123`
//! (the cut without a vector per range, stopping at a range that holds the
//! window whole); the bound by the longest range is ours. No switch: the same
//! ranges come out, in start order instead of the order they were declared,
//! and every caller either sorts them or only asks about overlaps (tests
//! below against the list).

use std::collections::BTreeSet;

#[derive(Default)]
pub(crate) struct Pending {
    set: BTreeSet<(usize, usize)>,
    /// At least the length of every range in `set` (0 when it is empty).
    longest: usize,
}

impl Pending {
    pub(crate) fn is_empty(&self) -> bool {
        self.set.is_empty()
    }

    pub(crate) fn contains(&self, range: &(usize, usize)) -> bool {
        self.set.contains(range)
    }

    pub(crate) fn insert(&mut self, range: (usize, usize)) {
        self.longest = self.longest.max(range.1.saturating_sub(range.0));
        self.set.insert(range);
    }

    pub(crate) fn retain(&mut self, keep: impl FnMut(&(usize, usize)) -> bool) {
        self.set.retain(keep);
        if self.set.is_empty() {
            self.longest = 0;
        }
    }

    /// The ranges `(a, e)` with `a < to && e > from`, by start.
    pub(crate) fn overlapping(
        &self,
        from: usize,
        to: usize,
    ) -> impl Iterator<Item = (usize, usize)> + '_ {
        // A range starting before `low` ends at or before `from`.
        let low = from.saturating_sub(self.longest).min(to);
        self.set
            .range((low, 0)..(to, 0))
            .copied()
            .filter(move |&(_, e)| e > from)
    }

    /// `inside(address, len)` without the ranges kept, each cut by `inside`
    /// too (`Shadow::writing`: the parts of a newly declared range that no
    /// range declared earlier holds).
    pub(crate) fn uncovered(
        &self,
        address: usize,
        len: usize,
        inside: impl Fn(usize, usize) -> (usize, usize),
    ) -> Vec<(usize, usize)> {
        let end = address + len;
        let mut parts = vec![inside(address, len)];
        let mut rest = Vec::with_capacity(4);
        for (a, e) in self.overlapping(address, end) {
            if a <= address && e >= end {
                parts.clear();
                break;
            }
            let (a, e) = inside(a, e - a);
            rest.clear();
            for &(from, to) in &parts {
                if e <= from || a >= to {
                    rest.push((from, to));
                    continue;
                }
                if from < a {
                    rest.push((from, a));
                }
                if e < to {
                    rest.push((e, to));
                }
            }
            std::mem::swap(&mut parts, &mut rest);
            if parts.is_empty() {
                break;
            }
        }
        parts
    }
}

#[cfg(test)]
mod tests {
    use super::Pending;

    /// A small deterministic generator (no rand dependency in this crate's tests).
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

    /// `Shadow::inside` for a copy of `[start, end)`.
    fn inside(start: usize, end: usize) -> impl Fn(usize, usize) -> (usize, usize) {
        move |address, len| {
            let from = address.clamp(start, end);
            (from - start, (address + len).clamp(from, end) - start)
        }
    }

    /// The cut as `Shadow::writing` made it over the list, in its order.
    fn uncovered_list(
        list: &[(usize, usize)],
        address: usize,
        len: usize,
        inside: &impl Fn(usize, usize) -> (usize, usize),
    ) -> Vec<(usize, usize)> {
        let mut parts = vec![inside(address, len)];
        for &(a, e) in list {
            let (a, e) = inside(a, e - a);
            let mut rest = Vec::with_capacity(parts.len() + 1);
            for (from, to) in parts {
                if e <= from || a >= to {
                    rest.push((from, to));
                    continue;
                }
                if from < a {
                    rest.push((from, a));
                }
                if e < to {
                    rest.push((e, to));
                }
            }
            parts = rest;
        }
        parts
    }

    /// What the callers do with the parts: the non-empty ones, in any order.
    fn used(mut parts: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
        parts.retain(|&(from, to)| from < to);
        parts.sort_unstable();
        parts
    }

    #[test]
    fn same_ranges_and_cuts_as_the_list() {
        for seed in 1..=60u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
            let (start, end) = (1000 + rng.below(200), 3000 + rng.below(200));
            let inside = inside(start, end);
            let mut list: Vec<(usize, usize)> = Vec::new();
            let mut set = Pending::default();
            for _ in 0..400 {
                let a = rng.below(4200);
                // Mostly short ranges, some empty, now and then a long one.
                let len = match rng.below(10) {
                    0 => 0,
                    1 => rng.below(2500),
                    _ => rng.below(120),
                };
                let range = (a, a + len);
                match rng.below(12) {
                    0 => {
                        // A landing: some ranges are done with.
                        let done: Vec<(usize, usize)> =
                            list.iter().copied().filter(|_| rng.below(3) == 0).collect();
                        list.retain(|r| !done.contains(r));
                        set.retain(|r| !done.contains(r));
                    }
                    1 => {
                        list.clear();
                        set.retain(|_| false);
                    }
                    _ => {
                        assert_eq!(set.contains(&range), list.contains(&range));
                        assert_eq!(
                            used(set.uncovered(a, len, &inside)),
                            used(uncovered_list(&list, a, len, &inside)),
                            "seed {seed} range {range:?}"
                        );
                        if !list.contains(&range) {
                            list.push(range);
                        }
                        set.insert(range);
                    }
                }
                assert_eq!(set.is_empty(), list.is_empty());
                let (from, to) = (rng.below(4300), rng.below(4300));
                let mut want: Vec<(usize, usize)> = list
                    .iter()
                    .copied()
                    .filter(|&(a, e)| a < to && e > from)
                    .collect();
                want.sort_unstable();
                let got: Vec<(usize, usize)> = set.overlapping(from, to).collect();
                assert_eq!(got, want, "seed {seed} window {from}..{to}");
            }
        }
    }

    #[test]
    fn a_range_holding_the_window_leaves_nothing() {
        let inside = inside(0, 1 << 20);
        let mut set = Pending::default();
        set.insert((100, 900));
        set.insert((200, 300));
        assert!(set.uncovered(150, 100, &inside).is_empty());
        assert_eq!(set.uncovered(50, 100, &inside), vec![(50, 100)]);
        assert_eq!(set.uncovered(850, 100, &inside), vec![(900, 950)]);
    }

    #[test]
    fn the_bound_forgets_long_ranges_once_empty() {
        let mut set = Pending::default();
        set.insert((0, 1 << 30));
        set.retain(|_| false);
        set.insert((5000, 5010));
        assert_eq!(set.longest, 10);
        assert_eq!(
            set.overlapping(5005, 5006).collect::<Vec<_>>(),
            vec![(5000, 5010)]
        );
        assert_eq!(set.overlapping(5010, 6000).count(), 0);
        assert_eq!(set.overlapping(0, 5000).count(), 0);
        // Empty and backwards windows: as the list's filter.
        assert_eq!(
            set.overlapping(5005, 5005).collect::<Vec<_>>(),
            vec![(5000, 5010)]
        );
        assert_eq!(set.overlapping(6000, 10).count(), 0);
    }

    /// The cut of a new range against `n` ranges declared before (the case of
    /// a big copy with many dispatches' ranges in flight), list against set.
    /// Run with `cargo test --release -p victory-gpu bench_pending -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn bench_pending() {
        let inside = inside(0, 1 << 30);
        for n in [16usize, 128, 512, 2048] {
            let mut rng = Rng(0x1234_5678 + n as u64);
            let ranges: Vec<(usize, usize)> = (0..n)
                .map(|_| {
                    let a = rng.below(1 << 28) & !0xff;
                    (a, a + 256 + rng.below(64 << 10))
                })
                .collect();
            let mut list = Vec::new();
            let mut set = Pending::default();
            for &r in &ranges {
                if !list.contains(&r) {
                    list.push(r);
                }
                set.insert(r);
            }
            let asks: Vec<(usize, usize)> = (0..2000)
                .map(|_| (rng.below(1 << 28), 256 + rng.below(16 << 10)))
                .collect();
            let started = std::time::Instant::now();
            let mut sink = 0usize;
            for &(a, len) in &asks {
                sink += usize::from(list.contains(&(a, a + len)));
                sink += uncovered_list(&list, a, len, &inside).len();
            }
            let list_us = started.elapsed().as_secs_f64() * 1e6 / asks.len() as f64;
            let started = std::time::Instant::now();
            for &(a, len) in &asks {
                sink += usize::from(set.contains(&(a, a + len)));
                sink += set.uncovered(a, len, &inside).len();
            }
            let set_us = started.elapsed().as_secs_f64() * 1e6 / asks.len() as f64;
            println!(
                "{n} ranges pending: list {list_us:.2} us, set {set_us:.2} us a declared range ({sink})"
            );
        }
    }
}
