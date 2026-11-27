//! `VICTORY_INDIRECT_BUDGET_LOG2` (trustno1-coder/VIctory-NVIDIA #72): the
//! clamp compiled with another budget, and its count of emptied records.

use super::*;

/// Unset, the clamp's source is the one it always was; set, only the budget
/// line changes and the count joins.
#[test]
fn the_budget_switch_leaves_the_default_clamp_alone() {
    assert!(
        matches!(budget::clamp_source(SOURCE, None), std::borrow::Cow::Borrowed(s) if s == SOURCE)
    );
    let at_30 = budget::clamp_source(SOURCE, Some(30));
    assert!(at_30.contains("static const uint BUDGET = 1u << 30;"));
    assert!(!at_30.contains("1u << 27;"));
    assert!(at_30.contains("budget.InterlockedAdd(24, 1u, emptied)"));
    let at_27 = budget::clamp_source(SOURCE, Some(27));
    assert!(at_27.contains("static const uint BUDGET = 1u << 27;"));
    assert_eq!(
        at_27.replace(
            "\n        if (over) { uint emptied; budget.InterlockedAdd(24, 1u, emptied); }",
            ""
        ),
        SOURCE
    );
}

/// On WARP: with a budget of 2^20, four draws of 2^18 vertex instances fit;
/// the rest run empty, keep their other words, and are counted in word 6.
#[test]
fn warp_records_past_a_smaller_budget_run_empty_and_are_counted() {
    let Some(mut warp) = WarpHarness::new() else {
        return;
    };
    let clamp = make_from(
        &warp.device,
        &budget::clamp_source(SOURCE, Some(20)),
        s!("clamp"),
    )
    .expect("the clamp must compile with another budget");
    const KIND_INDEXED: u32 = 1;
    const STRIDE: u32 = 20;
    const RECORDS: u32 = 70; // two waves
    let total = (STRIDE * RECORDS) as usize;
    let (src_buf, src_ptr, _) = warp.shared_buffer(total as u64);
    let (_budget_buf, budget_ptr, budget_gpu) = warp.shared_buffer(256);
    let (back_buf, back_ptr, _) = warp.shared_buffer(total as u64);
    let dst_buf = warp.default_uav(total as u64);
    // SAFETY: mapped buffers of these sizes; nothing runs yet.
    unsafe {
        for i in 0..RECORDS as usize {
            let words = [1u32 << 18, 1, 7, 0, 100 + i as u32];
            std::ptr::copy_nonoverlapping(
                words.as_ptr().cast::<u8>(),
                src_ptr.add(i * STRIDE as usize),
                STRIDE as usize,
            );
        }
        std::ptr::write_bytes(budget_ptr, 0, 256);
    }
    assert!(record_clamp(
        &clamp,
        &warp.list,
        &dst_buf,
        budget_gpu,
        (&src_buf, 0),
        None,
        (KIND_INDEXED, STRIDE, RECORDS),
    ));
    done_on(&warp.list, &dst_buf);
    // SAFETY: an open list; the clamp's output copied back to read.
    unsafe {
        warp.list.ResourceBarrier(&[transition(
            &dst_buf,
            D3D12_RESOURCE_STATE_COMMON,
            D3D12_RESOURCE_STATE_COPY_SOURCE,
        )]);
        warp.list
            .CopyBufferRegion(&back_buf, 0, &dst_buf, 0, total as u64);
        warp.list.ResourceBarrier(&[transition(
            &dst_buf,
            D3D12_RESOURCE_STATE_COPY_SOURCE,
            D3D12_RESOURCE_STATE_COMMON,
        )]);
    }
    warp.submit_and_wait();
    // SAFETY: the list has run; mapped words.
    let (spent, emptied, out) = unsafe {
        let w = budget_ptr.cast::<u32>();
        (
            w.read_volatile(),
            w.add(budget::EMPTIED_WORD).read_volatile(),
            std::slice::from_raw_parts(back_ptr.cast::<u32>().cast_const(), total / 4).to_vec(),
        )
    };
    assert_eq!(spent, 1 << 20);
    let kept = (0..RECORDS as usize)
        .filter(|&i| out[i * 5] == 1 << 18)
        .count();
    assert_eq!(kept, 4);
    assert_eq!(emptied, RECORDS - 4);
    for i in 0..RECORDS as usize {
        let rec = &out[i * 5..i * 5 + 5];
        assert!(
            rec[..2] == [1 << 18, 1] || rec[..2] == [0, 0],
            "record {i}: {rec:?}"
        );
        assert_eq!(rec[2..], [7, 0, 100 + i as u32]);
    }
}
