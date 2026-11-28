//! `VICTORY_SEND_OWN` on the software adapter: the masked pass reads a run's
//! blocks from the bytes its submission took, so a later write to the CPU
//! side before the pass runs does not reach the card side; without the bit it
//! reads the CPU side (the old pass, which took the later bytes).

use super::*;

const BLOCK: usize = crate::send_own::BLOCK;

/// Runs the masked pass of `source` over blocks 1 and 2 of a four-block copy:
/// the CPU side holds `taken` when the plan is made and `later` when the pass
/// runs; `own` reads the blocks from a chunk filled at the plan. Returns the
/// card side's bytes.
fn patched(source: &str, own: bool, taken: &[u8], later: &[u8], to: &[u8]) -> Option<Vec<u8>> {
    let mut warp = WarpHarness::new()?;
    let patch = compile_cs(&warp.device, source).expect("the card patch must compile on WARP");
    let size = to.len();
    let (_from, from_ptr, from_gpu) = warp.shared_buffer(size as u64);
    let (_to, to_ptr, to_gpu) = warp.shared_buffer(size as u64);
    let (_masks, masks_ptr, masks_gpu) = warp.shared_buffer(4096);
    let (_chunk, chunk_ptr, chunk_gpu) = warp.shared_buffer(2 * BLOCK as u64);
    let ranges = [(BLOCK + 1, BLOCK + 9), (2 * BLOCK + 100, 2 * BLOCK + 101)];
    // SAFETY: the mappings hold what is written and read; nothing runs yet.
    unsafe {
        // The plan: the CPU side as it is now, blocks 1 and 2 taken in table order.
        std::ptr::copy_nonoverlapping(taken.as_ptr(), from_ptr, size);
        std::ptr::copy_nonoverlapping(taken[BLOCK..].as_ptr(), chunk_ptr, 2 * BLOCK);
        std::ptr::copy_nonoverlapping(to.as_ptr(), to_ptr, size);
        std::ptr::write_bytes(masks_ptr, 0, 4096);
        let one = crate::host::block_mask(1, &ranges[..1]);
        let two = crate::host::block_mask(2, &ranges[1..]);
        std::ptr::copy_nonoverlapping(one.as_ptr(), masks_ptr.add(1024), 512);
        std::ptr::copy_nonoverlapping(two.as_ptr(), masks_ptr.add(1536), 512);
        let table: [[u32; 2]; 2] = [[BLOCK as u32, 1024], [(2 * BLOCK) as u32, 1536]];
        let table_at = 2048usize;
        std::ptr::copy_nonoverlapping(table.as_ptr().cast::<u8>(), masks_ptr.add(table_at), 16);
        // A later refresh writes the CPU side before the card comes to the pass.
        std::ptr::copy_nonoverlapping(later.as_ptr(), from_ptr, size);
        warp.list.SetComputeRootSignature(&patch.root);
        warp.list.SetPipelineState(&patch.pso);
        let blocks = 2u32 | if own { crate::send_own::OWN_BIT } else { 0 };
        let constants = [table_at as u32, blocks];
        warp.list
            .SetComputeRoot32BitConstants(0, 2, constants.as_ptr().cast(), 0);
        warp.list
            .SetComputeRootShaderResourceView(1, if own { chunk_gpu } else { from_gpu });
        warp.list.SetComputeRootShaderResourceView(2, masks_gpu);
        warp.list.SetComputeRootUnorderedAccessView(3, to_gpu);
        warp.list.Dispatch(16 * 2, 1, 1);
    }
    warp.submit_and_wait();
    // SAFETY: inside the mapping.
    Some(
        (0..size)
            .map(|i| unsafe { to_ptr.add(i).read_volatile() })
            .collect(),
    )
}

#[test]
fn warp_masked_pass_sends_the_bytes_taken_at_preparing() {
    let size = 4 * BLOCK;
    let taken: Vec<u8> = (0..size).map(|i| (i * 7 + 1) as u8).collect();
    let later: Vec<u8> = taken.iter().map(|b| b ^ 0x5a).collect();
    let to: Vec<u8> = (0..size).map(|i| (i * 13 + 128) as u8).collect();
    let mut wanted = to.clone();
    for (a, b) in [(BLOCK + 1, BLOCK + 9), (2 * BLOCK + 100, 2 * BLOCK + 101)] {
        wanted[a..b].copy_from_slice(&taken[a..b]);
    }
    let own = crate::send_own::patch_source(crate::host::CARD_PATCH, true);
    let Some(got) = patched(&own, true, &taken, &later, &to) else {
        return;
    };
    assert_eq!(
        got, wanted,
        "owned blocks: the bytes the plan took, no other byte written"
    );
    // The pass reading the CPU side when it runs (the switch off, or no bit) sends the later bytes.
    for (source, own) in [(crate::host::CARD_PATCH, false), (own.as_ref(), false)] {
        let got = patched(source, own, &taken, &later, &to).expect("WARP was there");
        assert_eq!(got[BLOCK + 1], later[BLOCK + 1]);
        assert_ne!(got, wanted);
    }
}
