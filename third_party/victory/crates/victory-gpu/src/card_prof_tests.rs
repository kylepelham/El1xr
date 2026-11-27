use super::*;

/// `card_before`'s gathering and joining as they are with the switch off:
/// per copy, each list's ranges looked for among those so far, then each
/// copy's ranges copied, sorted and joined.
fn today(lists: &[Vec<(usize, Vec<(usize, usize)>)>]) -> Vec<(usize, Vec<(usize, usize)>)> {
    let mut declared: Vec<(usize, Vec<(usize, usize)>)> = Vec::new();
    let mut decl_index: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();
    for list in lists {
        for (copy, ranges) in list {
            if let Some(&i) = decl_index.get(copy) {
                let dst = &mut declared[i].1;
                for &range in ranges {
                    if !dst.contains(&range) {
                        dst.push(range);
                    }
                }
            } else {
                decl_index.insert(*copy, declared.len());
                declared.push((*copy, ranges.clone()));
            }
        }
    }
    declared
        .into_iter()
        .map(|(copy, ranges)| {
            let mut sorted = ranges.clone();
            sorted.sort_unstable();
            let mut spans: Vec<(usize, usize)> = Vec::new();
            for (from, to) in sorted {
                match spans.last_mut() {
                    Some(last) if from <= last.1 => last.1 = last.1.max(to),
                    _ => spans.push((from, to)),
                }
            }
            (copy, spans)
        })
        .collect()
}

fn joined(lists: &[Vec<(usize, Vec<(usize, usize)>)>]) -> Vec<(usize, Vec<(usize, usize)>)> {
    let mut join = DeclJoin::new();
    for list in lists {
        for (copy, ranges) in list {
            join.add(*copy, || *copy, ranges);
        }
    }
    join.finish()
}

struct Rng(u64);

impl Rng {
    fn next(&mut self, below: usize) -> usize {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((self.0 >> 33) as usize) % below
    }
}

/// A list's declared writes as `HostList::defer_gpu_write` keeps them:
/// per copy, in the order first written, each range once.
fn list(
    rng: &mut Rng,
    copies: usize,
    most: usize,
    room: usize,
) -> Vec<(usize, Vec<(usize, usize)>)> {
    let mut out: Vec<(usize, Vec<(usize, usize)>)> = Vec::new();
    for _ in 0..rng.next(most) + 1 {
        let copy = 0x1000 + 0x40 * rng.next(copies);
        let from = rng.next(room);
        let range = (from, from + rng.next(room / 8 + 1));
        match out.iter_mut().find(|(c, _)| *c == copy) {
            Some((_, ranges)) => {
                if !ranges.contains(&range) {
                    ranges.push(range);
                }
            }
            None => out.push((copy, vec![range])),
        }
    }
    out
}

#[test]
fn joined_declared_writes_are_what_card_before_gives_the_compare_today() {
    let mut rng = Rng(0x5eed_53);
    for case in 0..2000 {
        let lists: Vec<_> = (0..rng.next(8) + 1)
            .map(|_| {
                list(
                    &mut rng,
                    1 + case % 5,
                    1 + case % 40,
                    64 + 4096 * (case % 3),
                )
            })
            .collect();
        assert_eq!(joined(&lists), today(&lists), "case {case}: {lists:?}");
    }
}

#[test]
fn joined_declared_writes_keep_the_copies_in_the_order_first_seen() {
    let lists = vec![
        vec![(7, vec![(10, 20)]), (3, vec![(0, 4)])],
        vec![
            (3, vec![(4, 8), (0, 4)]),
            (9, vec![(1, 1)]),
            (7, vec![(5, 10), (30, 40)]),
        ],
    ];
    let want = vec![
        (7, vec![(5, 20), (30, 40)]),
        (3, vec![(0, 8)]),
        (9, vec![(1, 1)]),
    ];
    assert_eq!(joined(&lists), want);
    assert_eq!(today(&lists), want);
}

#[test]
fn joined_spans_join_what_touches_and_keep_what_does_not() {
    assert_eq!(joined_spans(vec![]), vec![]);
    assert_eq!(
        joined_spans(vec![
            (20, 30),
            (0, 10),
            (10, 12),
            (11, 11),
            (31, 40),
            (35, 36)
        ]),
        vec![(0, 12), (20, 30), (31, 40)]
    );
}

/// The tests that turn `VICTORY_CARD_PROF` on or look at the sums.
static STATS_TESTS: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn laps_go_nowhere_when_off() {
    let _one = STATS_TESTS.lock().unwrap_or_else(|e| e.into_inner());
    // The crate's tests run without VICTORY_CARD_PROF.
    if on() {
        return;
    }
    let mut lap = Lap::new();
    lap.mark(P::Declared);
    add(P::Plans, 1);
    assert_eq!(STATS[P::Declared as usize].load(Ordering::Relaxed), 0);
    assert_eq!(STATS[P::Plans as usize].load(Ordering::Relaxed), 0);
}

#[test]
fn decl_join_is_live() {
    // Unchanged, the switch reads what the environment said, as before.
    assert_eq!(decl_join(), std::env::var_os("VICTORY_CARD_DECL_JOIN").is_some_and(|v| v == "1"));
    if std::env::var_os("VICTORY_CARD_DECL_JOIN").is_some() {
        return;
    }
    victory_core::settings::set("VICTORY_CARD_DECL_JOIN", "1").unwrap();
    let now = decl_join();
    victory_core::settings::set("VICTORY_CARD_DECL_JOIN", "").unwrap();
    assert!(now, "a change takes effect while running");
    assert!(!decl_join(), "and clears back to the launch value");
}

#[test]
fn card_prof_is_live_and_drops_its_sums_when_off() {
    let _one = STATS_TESTS.lock().unwrap_or_else(|e| e.into_inner());
    let old = ["VICTORY_CARD_PROF", "VICTORY_FLUSH_PROF"]
        .iter()
        .any(|name| std::env::var_os(name).is_some_and(|v| v == "1"));
    assert_eq!(on(), old, "unchanged, as before");
    if old || std::env::var_os("VICTORY_CARD_PROF").is_some() {
        return;
    }
    victory_core::settings::set("VICTORY_CARD_PROF", "1").unwrap();
    let was_on = on();
    victory_core::settings::set("VICTORY_CARD_PROF", "").unwrap();
    assert!(was_on, "a change takes effect while running");
    assert!(!on());
    STATS[P::Plans as usize].fetch_add(3, Ordering::Relaxed);
    log(1.0);
    assert_eq!(STATS[P::Plans as usize].load(Ordering::Relaxed), 0, "sums from before are dropped");
}
