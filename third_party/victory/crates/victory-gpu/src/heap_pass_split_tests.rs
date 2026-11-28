use std::cell::RefCell;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use super::*;
use crate::heap_pass::{Picked, Record};

/// A heap as the pass sees it: per slot the record in title memory, the key
/// it shows (`made_too`; 0 not made) and, per view entry, the copy and
/// range the view reads (`None`: no host copy yet; range 0: empty).
#[derive(Clone)]
struct Model {
    records: Vec<Record>,
    shown: Vec<u64>,
    reads: BTreeMap<u32, Option<(u32, u64, u64)>>,
}

const TAG: u32 = 7;

fn key(record: Record) -> u64 {
    u64::from(record[2]) << 32 | u64::from(record[1])
}

fn entry(record: Record) -> Option<u32> {
    (record[0] == TAG).then_some(record[1])
}

/// A small deterministic generator (no crate for it in the tree).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn model(slots: u32, rng: &mut Rng) -> Model {
    let mut m = Model {
        records: Vec::new(),
        shown: Vec::new(),
        reads: BTreeMap::new(),
    };
    for s in 0..slots {
        let e = s + 1000;
        let record = match rng.below(20) {
            0 => [0, 0, 0],
            _ => [TAG, e, 1 + rng.below(3) as u32],
        };
        m.records.push(record);
        // Most slots show their view already, as in the world's full looks.
        m.shown
            .push(if rng.below(10) < 8 { key(record) } else { 0 });
        let read = match rng.below(25) {
            0 => None,
            1 => Some((s / 16, 0, 0)),
            _ => {
                let from = u64::from(s) * 64 + rng.below(32);
                Some((s / 16, from, from + 16 + rng.below(512)))
            }
        };
        m.reads.insert(e, read);
    }
    m
}

impl Model {
    fn view(&self, slot: u32) -> Option<Record> {
        let record = self.records[slot as usize];
        (record[0] == TAG).then_some(record)
    }

    fn look_view(&self, record: Record) -> Look<u32> {
        match self.reads.get(&record[1]).copied().flatten() {
            Some((copy, from, to)) if to > from => Look::Keep {
                copy,
                from,
                to,
                made: false,
            },
            Some(_) => Look::Forget,
            None => Look::Orphan,
        }
    }

    /// `mirror_heap_buffer`'s fast path alone (`shown_already`).
    fn pre(&self, slot: u32) -> Pre<u32> {
        let record = self.view(slot)?;
        let shown = self.shown[slot as usize];
        (shown != 0 && shown == key(record)).then(|| (shown, self.look_view(record)))
    }

    fn still(&self, slot: u32, k: u64) -> bool {
        self.shown[slot as usize] == k && self.view(slot).is_some_and(|r| key(r) == k)
    }

    /// The whole old path: makes the slot (or lets it go) and looks.
    fn slow(&mut self, slot: u32) -> Look<u32> {
        let Some(record) = self.view(slot) else {
            self.shown[slot as usize] = 0;
            return Look::Forget;
        };
        let made = self.shown[slot as usize] != key(record);
        self.shown[slot as usize] = key(record);
        match self.look_view(record) {
            Look::Keep { copy, from, to, .. } => Look::Keep {
                copy,
                from,
                to,
                made,
            },
            other => other,
        }
    }
}

type Applied = Vec<(usize, u32, Look<u32>)>;

/// One pass over `slots` of `m` into `p`, looked at beside it on `threads`
/// threads first (1: the serial pass); `between` changes the heap after
/// the look beside and before the pass takes the slots in.
fn pass(
    m: &mut Model,
    p: &mut Picked<u32>,
    slots: &[u32],
    threads: usize,
    between: impl FnOnce(&mut Model),
) -> Applied {
    let pre = if threads > 1 {
        let seen = m.clone();
        pre_look(slots, threads, |s| seen.pre(s))
    } else {
        Vec::new()
    };
    between(m);
    let m = RefCell::new(m);
    let mut applied = Vec::new();
    run(
        slots,
        pre,
        |s, k| m.borrow().still(s, k),
        |s| m.borrow_mut().slow(s),
        |n, s, look| {
            let record = m.borrow().records[s as usize];
            match &look {
                Look::Forget => p.forget(s),
                Look::Orphan => p.orphan(s, record, entry(record)),
                Look::Keep { copy, from, to, .. } => {
                    p.keep(s, *copy as usize, copy, [*from, *to], record, entry(record))
                }
            }
            applied.push((n, s, look));
        },
    );
    applied
}

fn spans(p: &mut Picked<u32>) -> Vec<(u32, u64, u64)> {
    let mut out = p.spans();
    out.sort_unstable();
    out
}

/// Some slots get a new record (a new key), some lose their view, some
/// are let go by their slot (`shown` 0).
fn churn(m: &mut Model, rng: &mut Rng, n: u32) {
    let slots = m.records.len() as u64;
    for _ in 0..n {
        let s = rng.below(slots) as usize;
        match rng.below(3) {
            0 => m.records[s][2] += 1,
            1 => m.records[s] = [0, 0, 0],
            _ => m.shown[s] = 0,
        }
    }
}

#[test]
fn the_split_pass_takes_in_what_the_serial_pass_does() {
    for seed in 1..=12u64 {
        let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
        let n = [1500, 4000, 9000][seed as usize % 3];
        let base = model(n, &mut rng);
        let slots: Vec<u32> = (0..n).collect();
        let (mut serial, mut split) = (base.clone(), base.clone());
        let (mut ps, mut pp) = (Picked::new(false), Picked::new(false));
        for round in 0..4u64 {
            let mut churn_rng = Rng(seed * 31 + round + 1);
            let changes = n / 20;
            // The serial pass runs on the heap as it stands when the split
            // pass takes its slots in (after the churn).
            churn(&mut serial, &mut Rng(seed * 31 + round + 1), changes);
            let want = pass(&mut serial, &mut ps, &slots, 1, |_| {});
            let threads = [2, 8, 16, 33][round as usize];
            let got = pass(&mut split, &mut pp, &slots, threads, |m| {
                churn(m, &mut churn_rng, changes)
            });
            assert_eq!(got, want, "seed {seed} round {round}: what was taken in");
            assert_eq!(
                split.shown, serial.shown,
                "seed {seed} round {round}: slots made"
            );
            assert_eq!(
                spans(&mut pp),
                spans(&mut ps),
                "seed {seed} round {round}: spans"
            );
            assert_eq!(pp.views(), ps.views());
        }
    }
}

#[test]
fn a_slot_changed_after_the_look_goes_the_old_way() {
    let mut rng = Rng(77);
    let mut m = model(2048, &mut rng);
    for s in 0..2048 {
        m.records[s] = [TAG, s as u32 + 1000, 1];
        m.shown[s] = key(m.records[s]);
    }
    let slots: Vec<u32> = (0..2048).collect();
    let mut p = Picked::new(false);
    let pre = pre_look(&slots, 8, |s| m.pre(s));
    assert!(pre.iter().all(Option::is_some));
    m.records[5][2] = 9;
    m.shown[6] = 0;
    let m = RefCell::new(m);
    let mut slow = Vec::new();
    let counts = run(
        &slots,
        pre,
        |s, k| m.borrow().still(s, k),
        |s| {
            slow.push(s);
            m.borrow_mut().slow(s)
        },
        |_, s, look| {
            if let Look::Keep { copy, from, to, .. } = look {
                let r = m.borrow().records[s as usize];
                p.keep(s, copy as usize, &copy, [from, to], r, entry(r));
            }
        },
    );
    assert_eq!(slow, vec![5, 6]);
    assert_eq!(counts, [2046, 2]);
    assert_eq!(m.borrow().shown[5], key([TAG, 1005, 9]));
}

#[test]
fn looks_come_back_in_the_order_of_the_slots() {
    for (n, threads) in [
        (0, 8),
        (1, 8),
        (127, 3),
        (128, 3),
        (129, 3),
        (5000, 1),
        (5000, 7),
        (70_001, 16),
    ] {
        let slots: Vec<u32> = (0..n).map(|s| s * 3 + 1).collect();
        let got = pre_look(&slots, threads, |s| {
            (s % 5 != 0).then_some((u64::from(s), Look::<u32>::Forget))
        });
        let want: Vec<Pre<u32>> = slots
            .iter()
            .map(|&s| (s % 5 != 0).then_some((u64::from(s), Look::Forget)))
            .collect();
        assert_eq!(got, want, "{n} slots on {threads} threads");
    }
}

#[test]
fn small_passes_and_the_switch_off_stay_on_the_pass_thread() {
    assert_eq!(threads(0, 90_000), 1);
    assert_eq!(threads(1, LEAST - 1), 1);
    assert_eq!(threads(1, LEAST), DEFAULT_THREADS);
    assert_eq!(threads(4, 90_000), 4);
    assert_eq!(threads(1000, 90_000), MOST_THREADS);
}

#[test]
fn the_switch_is_live() {
    victory_core::settings::check_live("VICTORY_HEAP_PASS_SPLIT", "4", || threads_for(LEAST), 1, 4);
}

#[test]
fn the_in_place_switch_is_live() {
    victory_core::settings::check_live("VICTORY_HEAP_NET_IN_PLACE", "1", net_in_place, false, true);
}

/// A synthetic pass of the census's size: 90,000 slots over 5,300 host
/// copies, each look reading the record, the slot's key, a view table
/// under a shared lock, and two lookups in a buffer table under another
/// (`buffer_copy`'s `find` of the whole buffer and of the view's bytes),
/// then the slots taken into a fresh `Picked` (the safety net forgets all).
/// Ignored (timing): `cargo test -p victory-gpu --lib heap_pass_split::tests::bench -- --ignored --nocapture`.
#[test]
#[ignore]
fn bench() {
    const SLOTS: u32 = 90_000;
    const COPIES: u64 = 5_300;
    let records: Vec<Record> = (0..SLOTS).map(|s| [TAG, s, 1]).collect();
    let shown: Vec<AtomicU64> = records.iter().map(|&r| AtomicU64::new(key(r))).collect();
    let views: RwLock<Vec<([u32; 12], u32)>> =
        RwLock::new((0..SLOTS).map(|s| ([s; 12], 1)).collect());
    let stride = 1 << 20;
    let buffers: RwLock<BTreeMap<u64, (u64, Arc<u64>)>> = RwLock::new(
        (0..COPIES)
            .map(|c| (c * stride, (c * stride + stride, Arc::new(c))))
            .collect(),
    );
    let find = |at: u64, len: u64| -> Option<Arc<u64>> {
        let map = buffers.read().unwrap_or_else(|e| e.into_inner());
        map.range(..=at)
            .rev()
            .take(64)
            .find(|(_, (end, _))| *end >= at + len)
            .map(|(_, (_, c))| c.clone())
    };
    let look = |s: u32| -> Pre<Arc<u64>> {
        let record = records[s as usize];
        let k = shown[s as usize].load(Ordering::Acquire);
        if k != key(record) {
            return None;
        }
        let view = {
            let v = views.read().unwrap_or_else(|e| e.into_inner());
            let (view, generation) = v.get(record[1] as usize)?;
            (*generation == record[2]).then_some(*view)?
        };
        let at =
            u64::from(view[0]) * COPIES * stride / u64::from(SLOTS) + u64::from(view[1] % 64) * 16;
        let whole = find(at - at % stride, stride)?;
        let copy = find(at, 256).unwrap_or(whole);
        Some((
            k,
            Look::Keep {
                copy,
                from: at,
                to: at + 256,
                made: false,
            },
        ))
    };
    let slots: Vec<u32> = (0..SLOTS).collect();
    let keep_all = |p: &mut Picked<Arc<u64>>, s: u32, l: Look<Arc<u64>>| {
        if let Look::Keep { copy, from, to, .. } = l {
            let key = Arc::as_ptr(&copy) as usize;
            p.keep(s, key, &copy, [from, to], records[s as usize], entry(records[s as usize]));
        }
    };
    // `in_place`: the safety net with VICTORY_HEAP_NET_IN_PLACE, over what
    // the passes before kept (otherwise into emptied tables).
    let time = |threads: usize, in_place: bool| {
        let mut p: Picked<Arc<u64>> = Picked::new(false);
        if in_place {
            for &s in &slots {
                keep_all(&mut p, s, look(s).map_or(Look::Forget, |(_, l)| l));
            }
        }
        let began = std::time::Instant::now();
        let pre = if threads > 1 {
            pre_look(&slots, threads, look)
        } else {
            Vec::new()
        };
        let looked = began.elapsed();
        let counts = run(
            &slots,
            pre,
            |s, k| shown[s as usize].load(Ordering::Acquire) == k && key(records[s as usize]) == k,
            |s| look(s).map_or(Look::Forget, |(_, l)| l),
            |_, s, l| keep_all(&mut p, s, l),
        );
        let all = began.elapsed();
        assert_eq!(p.views(), SLOTS as usize);
        (looked, all, counts)
    };
    for round in 0..3 {
        for (threads, in_place) in [(1, false), (1, true), (4, true), (8, false), (8, true), (16, true)] {
            let (looked, all, counts) = time(threads, in_place);
            println!(
                "round {round}: {SLOTS} slots, {COPIES} copies, {threads:>2} threads, in place {in_place:>5}: {:>7.1} ms ({:.1} ms looked at beside, {:.1} taken in; {counts:?})",
                all.as_secs_f64() * 1e3,
                looked.as_secs_f64() * 1e3,
                (all - looked).as_secs_f64() * 1e3,
            );
        }
    }
}
