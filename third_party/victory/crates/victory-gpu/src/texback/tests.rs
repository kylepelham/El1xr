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
