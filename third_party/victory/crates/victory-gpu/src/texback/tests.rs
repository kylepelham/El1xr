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
