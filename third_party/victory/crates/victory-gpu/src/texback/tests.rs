//! Pooled D3D12 read-back buffer reuse across texture copy-backs.

use std::cell::Cell;

use super::*;

#[test]
fn readback_buffer_pool_reuses_across_rounds() {
    let Some(host) = crate::host::host() else {
        return; // no host GPU here
    };
    const ROUNDS: usize = 8;
    const SIZES: [u64; 2] = [256 * 256 * 4, 256 * 256 * 8];
    READBACK_ALLOCS.with(|c| c.set(0));
    for _ in 0..ROUNDS {
        let bufs = [
            take_readback_buffer(host, SIZES[0]).unwrap(),
            take_readback_buffer(host, SIZES[1]).unwrap(),
        ];
        for buf in bufs {
            return_readback_buffer(buf);
        }
    }
    let allocs = READBACK_ALLOCS.with(Cell::get);
    assert!(
        allocs <= 2,
        "expected readback buffer pool reuse across {ROUNDS} rounds, got {allocs} D3D12 allocations"
    );
}

/// Only the copy-backs whose allocation changed are invalidated: a remap
/// touches those it overlaps, a texture made at an address the one there,
/// a release its owner's.
#[test]
fn only_readbacks_for_changed_allocations_are_invalidated() {
    use std::sync::atomic::Ordering;
    let first = track(91, 0x1000, 0x2000);
    let next = track(92, 0x2000, 0x3000);
    let far = track(93, 0x4000, 0x5000);

    remapped(0x0f00, 0x200);
    assert!(!first.valid.load(Ordering::Acquire));
    assert!(next.valid.load(Ordering::Acquire));
    replaced(0x2000);
    assert!(!next.valid.load(Ordering::Acquire));
    assert!(far.valid.load(Ordering::Acquire));

    released(93);
    assert!(!far.valid.load(Ordering::Acquire));
    assert!(track(94, 0x2000, 0x3000).valid.load(Ordering::Acquire));
}

/// A texture written twice in a list is wanted once; if its allocation
/// changed between the writes, the stale target is taken again in place.
#[test]
fn a_stale_wanted_texture_is_taken_again_in_place() {
    // (owner, fresh, generation)
    let mut wanted: Vec<(usize, bool, u32)> = Vec::new();
    let want = |owner: usize, generation: u32, wanted: &mut Vec<(usize, bool, u32)>| {
        want_in(wanted, |t| t.0 == owner, |t| t.1, || Some((owner, true, generation)));
    };
    want(7, 1, &mut wanted);
    want(7, 2, &mut wanted);
    assert_eq!(wanted, vec![(7, true, 1)], "a fresh target is kept");
    wanted[0].1 = false;
    want(7, 3, &mut wanted);
    assert_eq!(wanted, vec![(7, true, 3)], "a stale target is replaced in place");
    want(8, 4, &mut wanted);
    assert_eq!(wanted, vec![(7, true, 3), (8, true, 4)]);
}

#[test]
fn async_writer_coalesces_newer_landings_per_address() {
    let mut pending = PendingMap::new();
    assert!(pending.is_empty());
    assert!(!pending.push(0x1000, 1u32));
    assert!(!pending.push(0x2000, 2u32));
    // A newer landing for 0x1000 replaces the older unconsumed one.
    assert!(pending.push(0x1000, 3u32));
    let mut drained = pending.drain_all();
    drained.sort_unstable();
    assert_eq!(drained, vec![2, 3]);
    assert!(pending.is_empty());
}

#[test]
fn stale_ticket_rejects_invalidated_async_landing() {
    let ticket = track(99, 0x8000, 0x9000);
    assert!(!stale_ticket(&ticket));
    released(99);
    assert!(stale_ticket(&ticket));
}
