//! (racefix) The D3D12 debug layer on the indirect tests' software device,
//! with `VICTORY_TEST_D3D12_DEBUG=1` (tests only; off unless set): the
//! layer is enabled before the device is made, and its messages of warning
//! severity or worse are read after every submission of a [`WarpHarness`],
//! printed and kept. Run serially (`--test-threads=1`): the software
//! adapter's device is one per process, and one made while another without
//! the layer still lives is that one.

use super::*;

/// Enables the layer for devices made from now on, if wanted.
pub(super) fn enable() {
    crate::test_d3d12_debug::enable();
}

/// The layer's message queue of `device`, if wanted and there.
pub(super) fn messages(device: &ID3D12Device) -> Option<ID3D12InfoQueue> {
    crate::test_d3d12_debug::messages(device)
}

/// Moves the queue's messages of warning severity or worse into `out`
/// (printed as they come), and empties the queue.
pub(super) fn drain(queue: Option<&ID3D12InfoQueue>, out: &mut Vec<String>) {
    crate::test_d3d12_debug::drain(queue, out);
}

/// A list's ahead buffer used by its prelude in one submission and again
/// in a later one (the list reset between them): the second prelude names
/// `INDIRECT_ARGUMENT` as the before-state of a buffer that decayed to
/// `COMMON` when the first submission ended. Well formed: a buffer in
/// `COMMON` is promoted to that read state, and with
/// `VICTORY_TEST_D3D12_DEBUG=1` the layer (GPU-based validation too) says
/// nothing; the clamp comes out right.
#[test]
fn warp_ahead_buffer_reused_by_a_later_submission() {
    let Some(mut warp) = WarpHarness::new() else {
        return;
    };
    let (src_buf, src_ptr, _) = warp.shared_buffer(256);
    let (_budget_buf, budget_ptr, budget_gpu) = warp.shared_buffer(256);
    let (readback_buf, readback_ptr, _) = warp.shared_buffer(AHEAD_SLOT);
    let chunk = warp.default_uav(AHEAD_SLOTS * AHEAD_SLOT);
    // SAFETY: mapped buffers of these sizes.
    unsafe {
        std::ptr::write_bytes(src_ptr, 0, 256);
        std::ptr::copy_nonoverlapping([5u32, 3, 2].as_ptr().cast(), src_ptr, 12);
        std::ptr::write_bytes(budget_ptr, 0, 256);
        std::ptr::write_bytes(readback_ptr, 0, AHEAD_SLOT as usize);
    }
    let jobs: Vec<Ahead> = vec![(src_buf.clone(), 0, 2, 12)];
    let mut chunks = [(chunk.clone(), false)];
    // First submission: the prelude leaves it in the indirect-argument
    // state; it decays when the submission ends.
    record_ahead_with(&warp.clamp, &warp.list, budget_gpu, &mut chunks, &jobs);
    assert!(chunks[0].1);
    warp.submit_and_wait();
    // A later submission's prelude.
    record_ahead_with(&warp.clamp, &warp.list, budget_gpu, &mut chunks, &jobs);
    done_on(&warp.list, &chunk);
    // SAFETY: an open list and live buffers.
    unsafe { warp.list.CopyBufferRegion(&readback_buf, 0, &chunk, 0, 24) };
    warp.submit_and_wait();
    // SAFETY: the mapping holds 24 bytes the copy wrote.
    let made: [u32; 6] = unsafe { readback_ptr.cast::<[u32; 6]>().read_volatile() };
    assert_eq!(made, [5, 3, 2, 0, 3, 2]);
    assert!(warp.messages.is_empty(), "{:?}", warp.messages);
}

/// The layer is on when asked: a before-state that does not match the
/// one the same list set is named.
#[test]
fn warp_debug_layer_names_a_mismatched_before_state() {
    // Not well formed: only recorded for the layer to refuse.
    let Some(mut warp) = WarpHarness::new().filter(|w| w.debug.is_some()) else {
        return;
    };
    let buffer = warp.default_uav(64 << 10);
    // SAFETY: an open list and a live buffer.
    unsafe {
        warp.list.ResourceBarrier(&[transition(
            &buffer,
            D3D12_RESOURCE_STATE_COMMON,
            D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
        )]);
        warp.list.ResourceBarrier(&[transition(
            &buffer,
            D3D12_RESOURCE_STATE_COMMON,
            D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT,
        )]);
    }
    warp.submit_and_wait();
    assert!(
        warp.messages.iter().any(|m| m.contains("id 527")),
        "{:?}",
        warp.messages
    );
}
