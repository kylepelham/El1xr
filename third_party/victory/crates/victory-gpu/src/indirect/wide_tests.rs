//! `VICTORY_INDIRECT_WIDE` (after bolado-dev/mandy-port `e5e11b3e`): the
//! clamp lets an indirect compute record run up to 2^20 groups in x, and
//! counts the records it ran wide and the ones it still cut.

use super::*;

/// Unset, the clamp's source is the one it always was.
#[test]
fn the_wide_switch_leaves_the_default_clamp_alone() {
    assert!(
        matches!(wide::clamp_source(SOURCE, false), std::borrow::Cow::Borrowed(s) if s == SOURCE)
    );
    let on = wide::clamp_source(SOURCE, true);
    assert!(on.contains("if (asked.x <= GROUPS)"));
    assert!(on.contains("budget.InterlockedAdd(36, 1u, noted)"));
    assert!(on.contains("part + 1 == SPLIT ? groups.x - before"));
}

/// Clamps the compute `records` (one call, or one call each in parts) with
/// `source` on WARP: the arguments written and the budget buffer's words.
fn clamped(source: &str, records: &[[u32; 3]], parts: bool) -> Option<(Vec<u32>, Vec<u32>)> {
    let mut warp = WarpHarness::new()?;
    let clamp = make_from(&warp.device, source, s!("clamp")).expect("the clamp must compile");
    let per = if parts { SPLIT as usize * 3 } else { 3 };
    let total = (records.len() * per * 4) as u64;
    let src_bytes = (records.len() * 12) as u64;
    let (src_buf, src_ptr, _) = warp.shared_buffer(src_bytes);
    let (_budget_buf, budget_ptr, budget_gpu) = warp.shared_buffer(256);
    let (back_buf, back_ptr, _) = warp.shared_buffer(total);
    let dst_buf = warp.default_uav(total);
    // SAFETY: mapped buffers of these sizes; nothing runs yet.
    unsafe {
        std::ptr::copy_nonoverlapping(records.as_ptr().cast::<u8>(), src_ptr, src_bytes as usize);
        std::ptr::write_bytes(budget_ptr, 0, 256);
    }
    if parts {
        for i in 0..records.len() {
            // Each record's parts into a scratch of their own, copied after.
            let one = warp.default_uav(u64::from(SPLIT) * PART_BYTES);
            assert!(record_clamp_as(
                &clamp,
                &warp.list,
                (&one, D3D12_RESOURCE_STATE_COMMON),
                budget_gpu,
                (&src_buf, i as u64 * 12),
                None,
                (2, 12, 1),
                IN_PARTS,
            ));
            done_on(&warp.list, &one);
            // SAFETY: an open list; live buffers in the common state.
            unsafe {
                warp.list.ResourceBarrier(&[transition(
                    &one,
                    D3D12_RESOURCE_STATE_COMMON,
                    D3D12_RESOURCE_STATE_COPY_SOURCE,
                )]);
                warp.list.CopyBufferRegion(
                    &back_buf,
                    (i * per * 4) as u64,
                    &one,
                    0,
                    u64::from(SPLIT) * PART_BYTES,
                );
            }
            warp.submit_and_wait();
        }
    } else {
        assert!(record_clamp(
            &clamp,
            &warp.list,
            &dst_buf,
            budget_gpu,
            (&src_buf, 0),
            None,
            (2, 12, records.len() as u32),
        ));
        done_on(&warp.list, &dst_buf);
        // SAFETY: an open list; the clamp's output copied back to read.
        unsafe {
            warp.list.ResourceBarrier(&[transition(
                &dst_buf,
                D3D12_RESOURCE_STATE_COMMON,
                D3D12_RESOURCE_STATE_COPY_SOURCE,
            )]);
            warp.list.CopyBufferRegion(&back_buf, 0, &dst_buf, 0, total);
        }
        warp.submit_and_wait();
    }
    // SAFETY: the lists have run; mapped words.
    unsafe {
        let out =
            std::slice::from_raw_parts(back_ptr.cast::<u32>().cast_const(), total as usize / 4);
        let words = std::slice::from_raw_parts(budget_ptr.cast::<u32>().cast_const(), 16);
        Some((out.to_vec(), words.to_vec()))
    }
}

/// One dispatch each: without the switch every record is cut to 65,535 in x
/// (and counted cut, word 4); with it, up to 2^20 run as asked (counted in
/// word 9), and what is past that or past 2^20 groups in all is still cut.
#[test]
fn warp_wide_records_run_as_asked() {
    let records = [
        [67_440, 1, 1],
        [98_248, 1, 1],
        [1 << 20, 1, 1],
        [(1 << 20) + 1, 1, 1],
        [70_000, 20, 1],
        [65_535, 1, 1],
    ];
    let Some((plain, plain_words)) = clamped(SOURCE, &records, false) else {
        return;
    };
    let cut: Vec<[u32; 3]> = plain.chunks(3).map(|c| [c[0], c[1], c[2]]).collect();
    assert_eq!(
        cut,
        [
            [65_535, 1, 1],
            [65_535, 1, 1],
            [65_535, 1, 1],
            [65_535, 1, 1],
            [0, 0, 0],
            [65_535, 1, 1]
        ]
    );
    assert_eq!(plain_words[3], (1 << 20) + 1, "the largest x asked for");
    assert_eq!((plain_words[4], plain_words[wide::WIDE_WORD]), (5, 0));
    let (out, words) = clamped(&wide::clamp_source(SOURCE, true), &records, false).unwrap();
    let ran: Vec<[u32; 3]> = out.chunks(3).map(|c| [c[0], c[1], c[2]]).collect();
    assert_eq!(
        ran,
        [
            [67_440, 1, 1],
            [98_248, 1, 1],
            [1 << 20, 1, 1],
            [65_535, 1, 1],
            [0, 0, 0],
            [65_535, 1, 1]
        ]
    );
    assert_eq!((words[4], words[wide::WIDE_WORD]), (2, 3));
}

/// In parts (a program with a group base): up to 131,070 groups run in two
/// parts of at most 65,535 with or without the switch, and are not counted
/// cut; past that, without the switch the dispatch is cut to its first part,
/// with it the last part takes the rest.
#[test]
fn warp_wide_dispatches_in_parts_take_the_rest_in_their_last_part() {
    let records = [[98_248, 1, 1], [200_000, 1, 1]];
    let Some((plain, plain_words)) = clamped(SOURCE, &records, true) else {
        return;
    };
    assert_eq!(plain, [65_535, 1, 1, 32_713, 1, 1, 65_535, 1, 1, 0, 1, 1]);
    assert_eq!((plain_words[4], plain_words[wide::WIDE_WORD]), (1, 0));
    let (out, words) = clamped(&wide::clamp_source(SOURCE, true), &records, true).unwrap();
    assert_eq!(
        out,
        [65_535, 1, 1, 32_713, 1, 1, 65_535, 1, 1, 134_465, 1, 1]
    );
    assert_eq!((words[4], words[wide::WIDE_WORD]), (0, 1));
}
