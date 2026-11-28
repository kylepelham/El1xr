//! The ranges of a submission's one write-watch asking (`VICTORY_ASK_ONCE`):
//! per host copy, the span of what the submission's lists refresh.
//!
//! `VICTORY_ASK_SPANS_KEPT=1` (live; off by default) merges the lists' spans
//! in a map the submitting thread keeps, with FxHash's step on the copies'
//! addresses, instead of a SipHash map made anew at every submission: the
//! same ranges (in another order, which the asking sorts anyway), without
//! SipHash per span or the map's allocations. A CPU microbenchmark
//! (`gathering_cost`) gave 3.3-3.6 us against 10.0 for 7 lists of 140 spans,
//! 20 us against 87 for 30 lists of 300.

use super::{HostList, Step, step_index};
use std::cell::RefCell;
use victory_core::settings::Live;

/// Whether the spans are merged in the kept map ([`ranges`]).
fn kept() -> bool {
    static ON: Live = Live::flag("VICTORY_ASK_SPANS_KEPT");
    ON.get_bool()
}

/// The ranges `runs` refresh, one per watched host copy: from the lowest
/// address refreshed to the highest end.
pub(super) fn ranges(runs: &[&HostList]) -> Vec<(usize, usize)> {
    use crate::watch::ask_split::{self as split, add, add_us};
    let started = std::time::Instant::now();
    let mut spans = 0u64;
    let each = |visit: &mut dyn FnMut(usize, usize, usize)| {
        if step_index() {
            for list in runs {
                for (shadow, from, to) in &list.deferred().refresh_spans {
                    if !shadow.unwatched() {
                        visit(std::sync::Arc::as_ptr(shadow) as usize, *from, *to);
                    }
                }
            }
        } else {
            for list in runs {
                for step in &list.deferred().copies {
                    if let Step::Refresh(shadow, address, len) = step
                        && !shadow.unwatched()
                    {
                        visit(
                            std::sync::Arc::as_ptr(shadow) as usize,
                            *address,
                            *address + *len,
                        );
                    }
                }
            }
        }
    };
    let out = if kept() {
        merged_kept(|visit| {
            each(&mut |key, from, to| {
                spans += 1;
                visit(key, from, to)
            })
        })
    } else {
        merged_hashed(|visit| {
            each(&mut |key, from, to| {
                spans += 1;
                visit(key, from, to)
            })
        })
    };
    add(split::SUBMISSIONS, 1);
    add(split::SPANS, spans);
    add(split::RANGES, out.len() as u64);
    add_us(split::GATHER_US, started);
    out
}

/// The spans `each` gives, merged per key in a map (as before the switch).
fn merged_hashed(each: impl FnOnce(&mut dyn FnMut(usize, usize, usize))) -> Vec<(usize, usize)> {
    let mut spans: std::collections::HashMap<usize, (usize, usize)> =
        std::collections::HashMap::new();
    each(&mut |key, from, to| {
        let span = spans.entry(key).or_insert((from, to));
        *span = (span.0.min(from), span.1.max(to));
    });
    spans.into_values().collect()
}

/// FxHash's multiply-rotate step for the copies' addresses.
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

    fn write_usize(&mut self, n: usize) {
        self.write_u64(n as u64);
    }

    fn write_u64(&mut self, n: u64) {
        self.0 = (self.0.rotate_left(5) ^ n).wrapping_mul(0x517c_c1b7_2722_0a95);
    }
}

/// The spans `each` gives, merged per key in a map this thread keeps (its room
/// stays from one submission to the next) with a cheap hash.
fn merged_kept(each: impl FnOnce(&mut dyn FnMut(usize, usize, usize))) -> Vec<(usize, usize)> {
    type Map = std::collections::HashMap<usize, usize, std::hash::BuildHasherDefault<Fx>>;
    thread_local! {
        static AT: RefCell<Map> = RefCell::new(Map::default());
    }
    AT.with(|at| {
        let mut at = at.borrow_mut();
        at.clear();
        let mut out: Vec<(usize, usize)> = Vec::new();
        each(&mut |key, from, to| match at.entry(key) {
            std::collections::hash_map::Entry::Occupied(i) => {
                let span = &mut out[*i.get()];
                *span = (span.0.min(from), span.1.max(to));
            }
            std::collections::hash_map::Entry::Vacant(v) => {
                v.insert(out.len());
                out.push((from, to));
            }
        });
        out
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(
        items: &[(usize, usize, usize)],
    ) -> impl FnOnce(&mut dyn FnMut(usize, usize, usize)) + '_ {
        move |visit| {
            for &(k, a, b) in items {
                visit(k, a, b);
            }
        }
    }

    #[test]
    fn the_kept_map_gives_the_ranges_the_fresh_map_gives() {
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for round in 0..200 {
            let n = (next() % 600) as usize + round % 3;
            let keys = (next() % 50) as usize + 1;
            let items: Vec<(usize, usize, usize)> = (0..n)
                .map(|_| {
                    let key = 0x1000 + (next() as usize % keys) * 0x40;
                    let from = (next() as usize % (1 << 30)) & !0xf;
                    let len = (next() as usize % (1 << 20)) + 1;
                    (key, from, from + len)
                })
                .collect();
            let mut hashed = merged_hashed(feed(&items));
            // Twice: the second finds the map holding the first's room.
            for _ in 0..2 {
                let mut kept = merged_kept(feed(&items));
                kept.sort_unstable();
                hashed.sort_unstable();
                assert_eq!(hashed, kept, "round {round}");
            }
        }
    }

    /// `cargo test -p victory-gpu --lib host::ask -- --ignored --nocapture`: a submission of
    /// `lists` lists refreshing `per_list` copies each out of `copies`.
    #[test]
    #[ignore = "CPU microbenchmark"]
    fn gathering_cost() {
        for (lists, per_list, copies) in [(7usize, 140usize, 200usize), (7, 400, 600), (30, 300, 1500)] {
            let items: Vec<(usize, usize, usize)> = (0..lists * per_list)
                .map(|i| {
                    let key = 0x10_0000 + ((i * 7919) % copies) * 0x140;
                    (key, i * 64, i * 64 + 4096)
                })
                .collect();
            let best = |f: &dyn Fn() -> Vec<(usize, usize)>| {
                (0..2000)
                    .map(|_| {
                        let t = std::time::Instant::now();
                        std::hint::black_box(f());
                        t.elapsed().as_secs_f64() * 1e6
                    })
                    .fold(f64::MAX, f64::min)
            };
            let hashed = best(&|| merged_hashed(feed(&items)));
            let kept = best(&|| merged_kept(feed(&items)));
            eprintln!(
                "{lists} lists x {per_list} spans over {copies} copies, best of 2000: map {hashed:.1} us, kept map {kept:.1} us"
            );
        }
    }

    #[test]
    fn one_range_per_copy_from_lowest_start_to_highest_end() {
        let items = [
            (8, 100, 200),
            (16, 5, 6),
            (8, 50, 120),
            (8, 300, 301),
            (16, 1, 2),
        ];
        assert_eq!(merged_kept(feed(&items)), vec![(50, 301), (1, 6)]);
        assert!(merged_kept(feed(&[])).is_empty());
    }
}
