//! `VICTORY_INDIRECT_REST` (trustno1-coder/VIctory-NVIDIA 545f890): what a
//! record over what is left of its list's budget takes, and the counts.

use super::*;

/// Unset, the clamp's source is the one it always was; `=1` only adds the
/// count; `=0` adds the count and charges nothing for a record over.
#[test]
fn the_rest_switch_leaves_the_default_clamp_alone() {
    let unset = rest::clamp_source(SOURCE, rest::Rest::Unset);
    assert!(matches!(unset, std::borrow::Cow::Borrowed(s) if s == SOURCE));
    let count = "\n        if (over) { uint told_rest; budget.InterlockedAdd(before < BUDGET ? 28u : 32u, 1u, told_rest); }";
    let counted = rest::clamp_source(SOURCE, rest::Rest::Rest);
    assert!(counted.contains(count));
    assert_eq!(counted.replace(count, ""), SOURCE);
    let nothing = rest::clamp_source(SOURCE, rest::Rest::Nothing);
    assert!(nothing.contains(count));
    assert!(!nothing.contains(rest::TAKE_REST_LINE));
    assert!(nothing.contains("if (BUDGET - before < cost) break;"));
    // With the budget switch too, both changes are in.
    let both = rest::clamp_source(&budget::clamp_source(SOURCE, Some(30)), rest::Rest::Nothing)
        .into_owned();
    assert!(both.contains("1u << 30;") && both.contains("told_rest") && both.contains("emptied"));
}

/// The switch reads live: unset, then `0`, then back.
#[test]
fn the_rest_switch_is_live() {
    victory_core::settings::check_live(
        "VICTORY_INDIRECT_REST",
        "0",
        rest::mode,
        rest::Rest::Unset,
        rest::Rest::Nothing,
    );
}

/// On WARP, with `taking`: a list whose budget has 10 left clamps one draw
/// of 1,000 vertices, then (a second execution) eight draws of one vertex.
/// Returns the budget spent, the eight draws' vertex counts, and words 7
/// and 8.
fn over_then_small(taking: rest::Rest) -> Option<(u32, Vec<u32>, u32, u32)> {
    let mut warp = WarpHarness::new()?;
    let clamp = make_from(
        &warp.device,
        &rest::clamp_source(SOURCE, taking),
        s!("clamp"),
    )
    .expect("the clamp must compile with the rest switch");
    const KIND_DRAW: u32 = 0;
    const STRIDE: u32 = 16;
    const SMALL: u32 = 8;
    const BUDGET: u32 = 1 << 27;
    let total = (STRIDE * (1 + SMALL)) as usize;
    let (src_buf, src_ptr, _) = warp.shared_buffer(total as u64);
    let (_budget_buf, budget_ptr, budget_gpu) = warp.shared_buffer(256);
    let (back_buf, back_ptr, _) = warp.shared_buffer(total as u64);
    let big_dst = warp.default_uav(STRIDE as u64);
    let small_dst = warp.default_uav(total as u64);
    // SAFETY: mapped buffers of these sizes; nothing runs yet.
    unsafe {
        let words = src_ptr.cast::<u32>();
        for (i, w) in [1000u32, 1, 0, 0].into_iter().enumerate() {
            words.add(i).write(w);
        }
        for r in 1..=SMALL as usize {
            for (i, w) in [1u32, 1, 0, r as u32].into_iter().enumerate() {
                words.add(r * 4 + i).write(w);
            }
        }
        std::ptr::write_bytes(budget_ptr, 0, 256);
        budget_ptr.cast::<u32>().write_volatile(BUDGET - 10);
    }
    assert!(record_clamp(
        &clamp,
        &warp.list,
        &big_dst,
        budget_gpu,
        (&src_buf, 0),
        None,
        (KIND_DRAW, STRIDE, 1),
    ));
    done_on(&warp.list, &big_dst);
    warp.submit_and_wait();
    assert!(record_clamp(
        &clamp,
        &warp.list,
        &small_dst,
        budget_gpu,
        (&src_buf, u64::from(STRIDE)),
        None,
        (KIND_DRAW, STRIDE, SMALL),
    ));
    done_on(&warp.list, &small_dst);
    let bytes = u64::from(STRIDE * SMALL);
    // SAFETY: an open list; the clamp's output copied back to read.
    unsafe {
        warp.list.ResourceBarrier(&[transition(
            &small_dst,
            D3D12_RESOURCE_STATE_COMMON,
            D3D12_RESOURCE_STATE_COPY_SOURCE,
        )]);
        warp.list
            .CopyBufferRegion(&back_buf, 0, &small_dst, 0, bytes);
        warp.list.ResourceBarrier(&[transition(
            &small_dst,
            D3D12_RESOURCE_STATE_COPY_SOURCE,
            D3D12_RESOURCE_STATE_COMMON,
        )]);
    }
    warp.submit_and_wait();
    // SAFETY: the list has run; mapped words.
    unsafe {
        let w = budget_ptr.cast::<u32>();
        let out = back_ptr.cast::<u32>();
        Some((
            w.read_volatile(),
            (0..SMALL as usize)
                .map(|r| out.add(r * 4).read_volatile())
                .collect(),
            w.add(rest::OVER_WORD).read_volatile(),
            w.add(rest::SPENT_WORD).read_volatile(),
        ))
    }
}

/// Unset and `=1`: the record over takes the rest and the eight draws after
/// it run empty (`=1` counts one over, eight after). `=0`: it takes nothing,
/// the eight draws run and charge 8 of the 10 left; one over, none after.
#[test]
fn warp_a_record_over_the_budget_takes_the_rest_or_nothing() {
    let Some((spent, small, over, after)) = over_then_small(rest::Rest::Unset) else {
        return;
    };
    assert_eq!((spent, small, over, after), (1 << 27, vec![0; 8], 0, 0));
    let (spent, small, over, after) = over_then_small(rest::Rest::Rest).unwrap();
    assert_eq!((spent, small, over, after), (1 << 27, vec![0; 8], 1, 8));
    let (spent, small, over, after) = over_then_small(rest::Rest::Nothing).unwrap();
    assert_eq!(
        (spent, small, over, after),
        ((1 << 27) - 2, vec![1; 8], 1, 0)
    );
}

/// A list's run, as the frame line counts it.
#[test]
fn a_list_counts_its_empty_records_and_whether_it_ran_out() {
    assert_eq!(rest::tally(0, 0, 0, 27), [0, 0, 0, 0]);
    assert_eq!(rest::tally(500, 0, 0, 27), [0, 0, 1, 0]);
    assert_eq!(rest::tally(1 << 27, 1, 8, 27), [1, 8, 1, 1]);
    assert_eq!(rest::tally((1 << 27) - 2, 1, 0, 27), [1, 0, 1, 0]);
    assert_eq!(rest::tally(1 << 27, 0, 0, 30), [0, 0, 1, 0]);
    assert_eq!(rest::tally(u32::MAX, 0, 3, 31), [0, 3, 1, 1]);
}
