//! (El1xr) `VICTORY_BUDGET_ON_CARD`: a list's indirect budget words in the
//! card's memory, moved to the PC's mirror and zeroed at each run's end.

use super::*;

/// A run takes the list's slot, the end pass leaves it clean for the next
/// run; a run that used it and had no end pass loses it for good; past the
/// last slot lists keep the PC's words.
#[test]
fn slots_follow_the_runs() {
    let mut s = card_budget::Slots::default();
    assert_eq!(s.used(0x100), None, "not begun");
    assert_eq!(s.begin(0x100), Some(0));
    assert_eq!(s.begin(0x200), Some(1));
    assert_eq!(s.used(0x100), Some(0));
    assert_eq!(s.end(0x100), Some(0));
    assert_eq!(
        s.used(0x100),
        Some(0),
        "the prelude, after the close, still clamps into it"
    );
    assert_eq!(s.end(0x100), None, "one end pass a run");
    assert_eq!(s.begin(0x100), Some(0), "the next run, from zero");
    // 0x200's run closed without its end pass.
    assert_eq!(s.begin(0x200), None);
    assert_eq!(s.used(0x200), None);
    assert_eq!(s.end(0x200), None);
    assert_eq!(s.begin(0x200), None, "lost for good");
    for i in 2..card_budget::SLOTS {
        assert_eq!(s.begin(0x1000 + u64::from(i)), Some(i));
    }
    assert_eq!(s.begin(0x9_0000), None, "no slot left");
}

/// Draws and mesh records whose words the clamp charges and notes.
fn records(ptr: *mut u8, n: usize) -> Vec<(u32, u32)> {
    let mut kinds = Vec::with_capacity(n);
    for i in 0..n {
        let (kind, words): (u32, [u32; 4]) = match i % 3 {
            0 => (0, [3 + i as u32, 2, 7, 9]),
            1 => (10, [17 + i as u32 % 50, 1, 1, 0]),
            _ => (10, [70_000, 1, 1, 0]),
        };
        // SAFETY: a mapped buffer of `n` records of 64 bytes.
        unsafe { std::ptr::copy_nonoverlapping(words.as_ptr().cast::<u8>(), ptr.add(i * 64), 16) };
        kinds.push((kind, if kind == 0 { 16 } else { 12 }));
    }
    kinds
}

/// On WARP: with the words on the card and the end pass, the clamps make
/// the same records and the mirror gets the words the PC's budget had; the
/// card's words are zero after, so a second run gives the same again.
#[test]
fn warp_card_words_reach_the_mirror_and_start_from_zero() {
    let Some(mut warp) = WarpHarness::new() else {
        return;
    };
    let pass = make_from(&warp.device, card_budget::END, s!("budget end")).unwrap();
    const N: usize = 40;
    let (src, src_ptr, _) = warp.shared_buffer(N as u64 * 64);
    let kinds = records(src_ptr, N);
    let jobs: Vec<Ahead> = kinds
        .iter()
        .enumerate()
        .map(|(i, &(kind, stride))| (src.clone(), i as u64 * 64, kind, stride))
        .collect();
    let (card, card_gpu) = card_budget::zeroed_buffer(&warp.device, 4 * card_budget::SLOT).unwrap();
    // The third slot, as a list's.
    let slot = card_gpu + 2 * card_budget::SLOT;
    let chunk_bytes = AHEAD_SLOTS * AHEAD_SLOT;
    let mut runs = Vec::new();
    for on_card in [false, true, true] {
        let (_mirror, mirror_ptr, mirror_gpu) = warp.shared_buffer(256);
        let (back, back_ptr, _) = warp.shared_buffer(chunk_bytes + 256);
        let chunk = warp.default_uav(chunk_bytes);
        // SAFETY: mapped buffers of these sizes; nothing runs yet.
        unsafe {
            std::ptr::write_bytes(mirror_ptr, 0, 256);
            std::ptr::write_bytes(back_ptr, 0xEE, (chunk_bytes + 256) as usize);
        }
        let budget = if on_card { slot } else { mirror_gpu };
        let mut chunks = [(chunk.clone(), false)];
        record_ahead_with(&warp.clamp, &warp.list, budget, &mut chunks, &jobs);
        if on_card {
            card_budget::record_end(&warp.list, &pass, slot, mirror_gpu);
        }
        done_on(&warp.list, &chunk);
        // SAFETY: an open list and live buffers.
        unsafe {
            warp.list.ResourceBarrier(&[uav_barrier()]);
            warp.list.CopyBufferRegion(&back, 0, &chunk, 0, chunk_bytes);
            warp.list
                .CopyBufferRegion(&back, chunk_bytes, &card, 0, 4 * card_budget::SLOT);
        }
        warp.submit_and_wait();
        // SAFETY: the list has run.
        let (slots, words, card_words) = unsafe {
            (
                std::slice::from_raw_parts(back_ptr, N * AHEAD_SLOT as usize).to_vec(),
                std::slice::from_raw_parts(mirror_ptr.cast::<u32>(), 16).to_vec(),
                std::slice::from_raw_parts(back_ptr.add(chunk_bytes as usize).cast::<u32>(), 64)
                    .to_vec(),
            )
        };
        assert!(
            card_words.iter().all(|&w| w == 0),
            "the card's words are zero after the run"
        );
        runs.push((slots, words));
    }
    assert_ne!(runs[0].1[0], 0, "the records were charged");
    assert_ne!(runs[0].1[1], 0, "and noted");
    for run in &runs[1..] {
        assert_eq!(run.1, runs[0].1, "the words");
        for (i, &(_, stride)) in kinds.iter().enumerate() {
            let at = i * AHEAD_SLOT as usize;
            assert_eq!(
                run.0[at..at + stride as usize],
                runs[0].0[at..at + stride as usize],
                "slot {i}"
            );
        }
    }
}

/// On the card (`cargo test -p victory-gpu --lib bench_budget_on_the_card --
/// --ignored --nocapture`, under the `gpu` lock): 19 preludes of 88 records
/// a frame (the launcher's ~1,670 records clamped ahead), with the budget
/// words in the PC's memory and on the card, sources in either.
#[test]
#[ignore = "on the card, under the gpu lock"]
fn bench_budget_on_the_card() {
    const PRELUDES: usize = 19;
    const PER: usize = 88;
    const RUNS: usize = 30;
    let Some(host) = host() else {
        return;
    };
    let device = &host.device;
    let clamp = make_for(device, rest::Rest::Unset).unwrap();
    let (shared_src, src_ptr) = host.shared_buffer(PER as u64 * 64).unwrap();
    for i in 0..PER {
        let words = [3 + i as u32, 2, 7, 9];
        // SAFETY: a mapped buffer of `PER` records of 64 bytes.
        unsafe {
            std::ptr::copy_nonoverlapping(words.as_ptr().cast::<u8>(), src_ptr.add(i * 64), 16)
        };
    }
    let card_src = host.uav_buffer(PER as u64 * 64).unwrap();
    let chunks: Vec<ID3D12Resource> = (0..PRELUDES)
        .map(|_| host.uav_buffer(AHEAD_SLOTS * AHEAD_SLOT).unwrap())
        .collect();
    let mirrors: Vec<(ID3D12Resource, *mut u8)> = (0..PRELUDES)
        .map(|_| host.shared_buffer(256).unwrap())
        .collect();
    let (card, card_gpu) =
        card_budget::zeroed_buffer(device, PRELUDES as u64 * card_budget::SLOT).unwrap();
    let pass = make_from(device, card_budget::END, s!("budget end")).unwrap();
    let (stamps_buf, stamps_ptr) = host.shared_buffer(16).unwrap();
    let mut heap: Option<ID3D12QueryHeap> = None;
    // SAFETY: plain object creation; the frequency query; a copy into the
    // card's source, waited for.
    let frequency = unsafe {
        device
            .CreateQueryHeap(
                &D3D12_QUERY_HEAP_DESC {
                    Type: D3D12_QUERY_HEAP_TYPE_TIMESTAMP,
                    Count: 2,
                    NodeMask: 0,
                },
                &mut heap,
            )
            .unwrap();
        let list = host.new_list().unwrap();
        list.list
            .CopyBufferRegion(&card_src, 0, &shared_src, 0, PER as u64 * 64);
        list.close();
        host.execute(&[&list]);
        list.reset();
        host.queue().GetTimestampFrequency().unwrap() as f64
    };
    let heap = heap.unwrap();
    let _ = &card;
    eprintln!("adapter: {:?}", crate::host::adapter_summary().map(|a| a.0));
    for (src_on_card, on_card) in [(false, false), (false, true), (true, false), (true, true)] {
        let src = if src_on_card { &card_src } else { &shared_src };
        let jobs: Vec<Ahead> = (0..PER)
            .map(|i| (src.clone(), (i * 64) as u64, 0, 16))
            .collect();
        let mut card_ms = Vec::new();
        for _ in 0..RUNS {
            let list = host.new_list().unwrap();
            let l = &list.list;
            // SAFETY: an open list, our heap and buffers; the mirrors are
            // zeroed before the list runs.
            unsafe {
                for (_, ptr) in &mirrors {
                    std::ptr::write_bytes(*ptr, 0, 256);
                }
                l.EndQuery(&heap, D3D12_QUERY_TYPE_TIMESTAMP, 0);
            }
            for (p, chunk) in chunks.iter().enumerate() {
                // SAFETY: a live buffer.
                let mirror = unsafe { mirrors[p].0.GetGPUVirtualAddress() };
                let slot = card_gpu + p as u64 * card_budget::SLOT;
                let mut c = [(chunk.clone(), false)];
                record_ahead_with(
                    &clamp,
                    l,
                    if on_card { slot } else { mirror },
                    &mut c,
                    &jobs,
                );
                if on_card {
                    card_budget::record_end(l, &pass, slot, mirror);
                }
            }
            // SAFETY: an open list, our heap and a buffer the resolve may write.
            unsafe {
                l.EndQuery(&heap, D3D12_QUERY_TYPE_TIMESTAMP, 1);
                l.ResolveQueryData(&heap, D3D12_QUERY_TYPE_TIMESTAMP, 0, 2, &stamps_buf, 0);
                for c in &chunks {
                    l.ResourceBarrier(&[transition(
                        c,
                        D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT,
                        D3D12_RESOURCE_STATE_COMMON,
                    )]);
                }
            }
            list.close();
            host.execute(&[&list]);
            // SAFETY: the list has run.
            let s = unsafe { stamps_ptr.cast::<[u64; 2]>().read_volatile() };
            card_ms.push(s[1].wrapping_sub(s[0]) as f64 * 1e3 / frequency);
            list.reset();
        }
        card_ms.sort_by(f64::total_cmp);
        eprintln!(
            "sources {}, budget {}: card {:.3} ms a frame (best {:.3})",
            if src_on_card {
                "on the card"
            } else {
                "in the PC's memory"
            },
            if on_card {
                "on the card"
            } else {
                "in the PC's memory"
            },
            card_ms[RUNS / 2],
            card_ms[0]
        );
    }
}
