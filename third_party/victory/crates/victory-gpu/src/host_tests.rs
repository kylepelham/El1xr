use super::*;

#[test]
fn read_buffer_into_copies_small_buffers_without_floor() {
    let from: Vec<u8> = (0..64u32).map(|i| (i * 3 + 1) as u8).collect();
    for len in [1, 4, 16, 64] {
        let mut to = vec![0u8; len];
        assert!(read_buffer_into(from.as_ptr(), &mut to));
        assert_eq!(to, from[..len]);
    }
}

#[test]
fn root_cbv_fill_reuses_until_copy_or_deferred_write_and_resets() {
    let Some(host) = host() else { return };
    let list = host.new_list().unwrap();
    let mut input = vec![0x11u8; 512];
    let source = input.as_mut_ptr();
    let fresh = vec![0x22u8; 512];
    let a = list.fill_root_cbv(source, 512, true).unwrap();
    assert_eq!(list.fill_root_cbv(source, 512, true).unwrap(), a);
    assert_ne!(list.fill_root_cbv(source, 256, true).unwrap().0, a.0);
    list.defer_copy(source, fresh.as_ptr(), 512);
    let b = list.fill_root_cbv(source, 512, true).unwrap();
    assert_ne!(b.0, a.0);
    let addr = source as usize;
    list.defer(move || {
        // SAFETY: input remains mutably owned and live until execution finishes.
        unsafe { (addr as *mut u8).write_bytes(0x33, 512) };
    });
    let c = list.fill_root_cbv(source, 512, true).unwrap();
    assert_ne!(c.0, b.0);
    list.close();
    host.execute(&[&list]);
    for ((_, ptr), value) in [(a, 0x11), (b, 0x22), (c, 0x33)] {
        // SAFETY: the corresponding scratch allocations remain alive until reset.
        assert!(
            unsafe { std::slice::from_raw_parts(ptr, 512) }
                .iter()
                .all(|&b| b == value)
        );
    }
    list.reset();
    assert!(list.deferred().filled_root_cbvs.is_empty());
}

#[test]
#[ignore = "native submission A/B timing; run alone"]
fn root_cbv_fill_native_benchmark() {
    let host = host().expect("native D3D12 host");
    let list = host.new_list().unwrap();
    let input = vec![0x7bu8; 16 * 1024 + 16];
    for count in [1, 128, 2048] {
        let mut samples = [Vec::new(), Vec::new()];
        for pair in 0..23 {
            for mode in [pair % 2, (pair + 1) % 2] {
                let started = std::time::Instant::now();
                let mut last = std::ptr::null_mut();
                for _ in 0..count {
                    // SAFETY: the extra 16 bytes keep the whole source in the allocation.
                    (_, last) = list
                        .fill_root_cbv(unsafe { input.as_ptr().add(16) }, 16 * 1024, mode != 0)
                        .unwrap();
                }
                list.close();
                host.execute(&[&list]);
                let ns = started.elapsed().as_nanos();
                // SAFETY: `last` points into the list's scratch allocation until `reset`.
                assert_eq!(unsafe { last.read() }, 0x7b);
                assert_eq!(
                    list.deferred()
                        .copies
                        .iter()
                        .filter(|s| matches!(s, Step::Fill(..)))
                        .count(),
                    if mode == 0 { count } else { 1 }
                );
                list.reset();
                if pair >= 3 {
                    samples[mode].push(ns);
                }
            }
        }
        for (v, times) in samples.iter_mut().enumerate() {
            times.sort_unstable();
            println!(
                "ROOT_CBV_AB,{count},{v},{:.4}",
                (times[9] + times[10]) as f64 / 2e6
            );
        }
    }
}

#[test]
fn sparse_copies_preserve_islands_and_observe_remaps() {
    use windows_sys::Win32::System::Memory::*;
    const N: usize = 5 << 16;
    // SAFETY: we reserve a private region, commit selected pages inside it,
    // and only directly access the pages while they are writable.
    unsafe {
        let base = VirtualAlloc(std::ptr::null(), N, MEM_RESERVE, PAGE_NOACCESS).cast::<u8>();
        assert!(!base.is_null());
        let mut expected = vec![0; N];
        for (offset, value) in [(0, 9), (2 << 16, 37), (N - 4096, 85)] {
            let at = base.add(offset);
            assert_eq!(
                VirtualAlloc(at.cast(), 4096, MEM_COMMIT, PAGE_READWRITE),
                at.cast()
            );
            at.write_bytes(value, 4096);
            expected[offset..offset + 4096].fill(value);
        }
        for remap in [false, true] {
            if remap {
                let mut old = 0;
                assert_ne!(
                    VirtualProtect(base.cast(), 4096, PAGE_NOACCESS, &mut old),
                    0
                );
                expected[..4096].fill(0);
                let at = base.add(1 << 16);
                assert_eq!(
                    VirtualAlloc(at.cast(), 4096, MEM_COMMIT, PAGE_READWRITE),
                    at.cast()
                );
                at.write_bytes(0xc7, 4096);
                expected[1 << 16..(1 << 16) + 4096].fill(0xc7);
            }
            let mut out = vec![0xee; N];
            super::copy_mapped(out.as_mut_ptr().add(27), base.add(27), N - 54, false);
            assert_eq!(out[27..N - 27], expected[27..N - 27]);
            assert!(out[..27].iter().chain(&out[N - 27..]).all(|&v| v == 0xee));
        }
        assert_ne!(VirtualFree(base.cast(), 0, MEM_RELEASE), 0);
    }
}

#[test]
fn warp_adapter_accepts_only_software() {
    // This probes DXGI without creating a D3D12 device or running a title.
    // SAFETY: plain DXGI adapter queries.
    let raw = unsafe {
        let Ok(factory) = CreateDXGIFactory2::<IDXGIFactory4>(DXGI_CREATE_FACTORY_FLAGS(0)) else {
            return;
        };
        factory.EnumWarpAdapter::<IDXGIAdapter>()
    };
    let Ok(raw) = raw else { return };
    // SAFETY: EnumWarpAdapter returns a DXGI adapter, which implements
    // IDXGIAdapter1; `raw` owns that interface.
    let desc = unsafe { raw.cast::<IDXGIAdapter1>().unwrap().GetDesc1().unwrap() };
    eprintln!(
        "EnumWarpAdapter: vendor={:#x}, flags={:#x}",
        desc.VendorId, desc.Flags
    );
    assert_eq!(
        warp_adapter().is_ok(),
        desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0,
    );
}

/// A thread waiting for the card does not hold `submitted`: another
/// thread takes it while the wait goes on, and the wait ends once the
/// card reaches the value.
#[test]
fn waiting_for_the_card_leaves_submitted_free() {
    use std::sync::atomic::AtomicBool;
    use std::time::{Duration, Instant};
    let Some(host) = Host::new_warp() else { return };
    // SAFETY: a fence of our own.
    let Ok(gate) = (unsafe {
        host.device
            .CreateFence::<ID3D12Fence>(0, D3D12_FENCE_FLAG_NONE)
    }) else {
        return;
    };
    // Far above any value a submission reaches.
    let target = 1u64 << 40;
    // SAFETY: the card signals `target` only after the processor opens the gate.
    unsafe {
        host.queue.Wait(&gate, 1).unwrap();
        host.queue.Signal(&host.fence, target).unwrap();
    }
    let done = AtomicBool::new(false);
    let (taken, early) = std::thread::scope(|s| {
        s.spawn(|| {
            host.wait_fence(target);
            done.store(true, Ordering::Release);
        });
        std::thread::sleep(Duration::from_millis(50));
        let early = done.load(Ordering::Acquire);
        let started = Instant::now();
        let mut taken = false;
        while !taken && started.elapsed() < Duration::from_secs(1) {
            taken = host.submitted.try_lock().is_ok();
            std::thread::yield_now();
        }
        // Opened whatever happened above, so the waiter always returns.
        // SAFETY: our fence.
        unsafe { gate.Signal(1).unwrap() };
        (taken, early)
    });
    assert!(!early, "the wait ended before the card reached the value");
    assert!(
        taken,
        "`submitted` stayed held while a thread waited for the card"
    );
    assert!(done.load(Ordering::Acquire));
    // SAFETY: a plain fence query.
    assert!(unsafe { host.fence.GetCompletedValue() } >= target);
}

mod mask_tests {
    use super::super::{block_mask, masked_blocks};

    fn block_mask_scalar(block: usize, ranges: &[(usize, usize)]) -> [u8; 512] {
        let mut mask = [0u8; 512];
        for &(a, b) in ranges {
            for at in a - block * 4096..b - block * 4096 {
                mask[at / 8] |= 1 << (at % 8);
            }
        }
        mask
    }

    /// Every range is a copy or in a block's mask, once; a block has a
    /// mask only with enough ranges; a range in two blocks is a copy.
    #[test]
    fn crowded_blocks_go_up_by_mask_and_the_rest_as_copies() {
        let ranges = [
            (8, 12),
            (100, 104),
            (4090, 4100),
            (4200, 4204),
            (8192, 8196),
            (8200, 8204),
            (8300, 8301),
            (12288, 20000),
        ];
        let (copies, blocks) = masked_blocks(&ranges, 3);
        assert_eq!(
            copies,
            [
                (8, 12),
                (100, 104),
                (4090, 4100),
                (4200, 4204),
                (12288, 20000)
            ]
        );
        assert_eq!(
            blocks,
            [(2, vec![(8192, 8196), (8200, 8204), (8300, 8301)])]
        );
        let (copies, blocks) = masked_blocks(&ranges, 2);
        assert_eq!(copies, [(4090, 4100), (4200, 4204), (12288, 20000)]);
        assert_eq!(blocks.iter().map(|b| b.0).collect::<Vec<_>>(), [0, 2]);
        assert!(masked_blocks(&[], 2).1.is_empty());
        // Check `least <= 1` splitting across 4 KiB boundaries (and `>= 2 * BLOCK` staying a copy).
        let ranges1 = [
            (8, 12),
            (100, 104),
            (4090, 4100),
            (4200, 4204),
            (8192, 8196),
            (8200, 8204),
            (8300, 8301),
            (24576, 33000),
        ];
        let (copies1, blocks1) = masked_blocks(&ranges1, 1);
        assert_eq!(copies1, [(24576, 33000)]);
        assert_eq!(
            blocks1,
            vec![
                (0, vec![(8, 12), (100, 104), (4090, 4096)]),
                (1, vec![(4096, 4100), (4200, 4204)]),
                (2, vec![(8192, 8196), (8200, 8204), (8300, 8301)]),
            ]
        );
        // The mask: bit i of the block's bytes, low bit first.
        let mask = block_mask(2, &[(8192, 8196), (8200, 8204), (8300, 8301)]);
        assert_eq!(mask[0], 0x0f);
        assert_eq!(mask[1], 0x0f);
        assert_eq!(mask[(8300 - 8192) / 8], 1 << ((8300 - 8192) % 8));
        assert_eq!(mask.iter().map(|m| m.count_ones()).sum::<u32>(), 9);
        // Word-packed `block_mask` matches scalar reference across edge cases.
        for sample in [
            &[(4096, 4097)][..],
            &[(4096 + 63, 4096 + 65)],
            &[
                (4096 + 7, 4096 + 200),
                (4096 + 511, 4096 + 1025),
                (4096 + 4095, 4096 + 4096),
            ],
            &[(4096, 8192)],
        ] {
            assert_eq!(block_mask(1, sample), block_mask_scalar(1, sample));
        }
    }
}

mod relay_merge_tests {
    use super::super::balanced_runs;

    #[test]
    fn runs_cover_every_piece_once_in_order() {
        let sizes = [64, 64, 1, 64, 64, 64, 8, 64, 64, 64];
        let runs = balanced_runs(&sizes, 4);
        assert!(runs.len() <= 4);
        assert_eq!(runs.first().unwrap().0, 0);
        assert_eq!(runs.last().unwrap().1, sizes.len());
        assert!(runs.windows(2).all(|w| w[0].1 == w[1].0));
    }

    #[test]
    fn few_pieces_make_few_runs() {
        assert_eq!(balanced_runs(&[10], 8), vec![(0, 1)]);
        assert!(balanced_runs(&[], 8).is_empty());
    }
}

mod relay_tests {
    use super::super::{flagged_pieces, landable};

    /// Two flights and what follows each: nothing of the second lands
    /// before the card has run it, and nothing behind a flight lands
    /// before the flight.
    #[test]
    fn flights_land_in_the_order_they_were_sent() {
        // Flight (fence 5), its write, a signal, flight (fence 6), its write.
        let line = [Some(5), None, None, Some(6), None];
        assert_eq!(landable(&line, 4), 0);
        assert_eq!(landable(&line, 5), 3);
        assert_eq!(landable(&line, 6), 5);
        assert_eq!(landable(&line, u64::MAX), 5);
        // Signals with no flight ahead land at once; behind one they wait.
        assert_eq!(landable(&[None, None], 0), 2);
        assert_eq!(landable(&[None, Some(9), None], 8), 1);
        assert_eq!(landable(&[], 3), 0);
        // After the first three landed, the rest when its turn comes.
        assert_eq!(landable(&line[3..], 5), 0);
        assert_eq!(landable(&line[3..], 6), 2);
    }

    /// A piece flagged under one range of a plan is fetched under every
    /// range that holds a part of it; pieces of other copies are not.
    #[test]
    fn a_piece_flagged_under_one_range_is_fetched_under_all() {
        let (a, b) = (0x1000usize, 0x2000usize);
        // Copy a: piece 3 under two ranges, flagged under the first only;
        // piece 4 not flagged; copy b: piece 3 not flagged, piece 9 flagged.
        let flags = [
            (a, 3, true),
            (a, 4, false),
            (a, 3, false),
            (b, 3, false),
            (b, 9, true),
        ];
        let fetch = flagged_pieces(&flags);
        assert!(fetch.contains(&(a, 3)));
        assert!(!fetch.contains(&(a, 4)));
        assert!(!fetch.contains(&(b, 3)));
        assert!(fetch.contains(&(b, 9)));
        assert_eq!(fetch.len(), 2);
    }
}

mod late_tests {
    use super::super::{continues, join_ranges, touches};

    /// Copies join only when both ends continue and the whole does not
    /// overlap itself.
    #[test]
    fn copies_join_only_end_to_end_and_apart() {
        // (destination, source, bytes)
        assert!(continues((1000, 5000, 16), (1016, 5016, 32)));
        assert!(continues((5000, 1000, 16), (5016, 1016, 32)));
        // A gap, or only one side continues.
        assert!(!continues((1000, 5000, 16), (1020, 5016, 32)));
        assert!(!continues((1000, 5000, 16), (1016, 5020, 32)));
        assert!(!continues((1000, 5000, 16), (1000, 5000, 16)));
        // Together they would overlap: the second reads what the first wrote.
        assert!(!continues((1000, 1016, 16), (1016, 1032, 16)));
        assert!(!continues((1016, 1000, 16), (1032, 1016, 16)));
        // They touch without overlapping.
        assert!(continues((1000, 1032, 16), (1016, 1048, 16)));
        assert!(!continues((usize::MAX - 8, 0, 8), (0, 8, 8)));
    }

    /// Ranges are found by what they hold, not by where they start, and a
    /// range that only touches another's end does not touch it.
    #[test]
    fn ranges_touch_where_they_overlap() {
        let mut ranges = Vec::new();
        join_ranges(
            &mut ranges,
            [(40, 50), (10, 20), (20, 25), (45, 60), (7, 7)],
        );
        assert_eq!(ranges, [(10, 25), (40, 60)]);
        join_ranges(&mut ranges, [(25, 40)]);
        assert_eq!(ranges, [(10, 60)]);
        let ranges = [(10usize, 25usize), (40, 60), (100, 101)];
        for (from, to, hit) in [
            (0, 10, false),
            (0, 11, true),
            (24, 25, true),
            (25, 40, false),
            (25, 41, true),
            (59, 200, true),
            (60, 100, false),
            (100, 101, true),
            (101, 500, false),
            (30, 30, false),
            (50, 50, false),
        ] {
            assert_eq!(touches(&ranges, from, to), hit, "{from}..{to}");
        }
        assert!(!touches(&[], 0, usize::MAX));
    }
}

mod landings_tests {
    use super::super::{CARD_COPY, HOST_COPY, Landings, READ_BACK};

    /// A frame's line comes when the next frame's first landing does: read
    /// backs summed per destination with their texture and the earliest
    /// frame they were recorded in, a card copy merged back, and a host
    /// copy found unchanged only counted.
    #[test]
    fn a_frames_landings_are_one_line_once_the_next_frame_begins() {
        let mut l = Landings::default();
        assert!(
            l.add(5, READ_BACK, 0x1000, 0x200, 0x100, 0x100, 0xabc, Some(4))
                .is_none()
        );
        assert!(
            l.add(5, READ_BACK, 0x1000, 0x200, 0x100, 0x100, 0xabc, Some(3))
                .is_none()
        );
        assert!(
            l.add(5, CARD_COPY, 0x2000, 0x40, 0x40, 0x10, 0, None)
                .is_none()
        );
        assert!(
            l.add(5, HOST_COPY, 0x3000, 0x40, 0x40, 0, 0, None)
                .is_none()
        );
        let (frame, count, unchanged, landed) = l
            .add(6, CARD_COPY, 0x2000, 0x40, 0x40, 0x40, 0, None)
            .expect("frame 5's line");
        assert_eq!((frame, count, unchanged), (5, 2, 1));
        assert_eq!(
            landed,
            [
                "card copy 0x2000+0x40: x1, 64 bytes, 16 written",
                "read-back 0x1000+0x200: x2, 512 bytes, 512 written, from 0xabc, recorded in frame 3",
            ]
        );
        // Frame 6 holds its own.
        assert_eq!(l.take().map(|t| (t.0, t.1)), Some((6, 1)));
        assert!(l.take().is_none());
    }
}

mod trail {
    /// The trail keeps the newest things, and the report made when the
    /// host device is removed says them and the pipeline library's state
    /// (no card is asked for any of it).
    #[test]
    fn the_removal_report_says_the_newest_things_done() {
        for n in 0..super::super::TRAIL_MOST + 5 {
            super::super::trail(|| format!("thing {n} done"));
        }
        let text = super::super::removal_context();
        assert!(text.contains("the submitting thread was"));
        assert!(text.contains("pipeline library: no call running"));
        assert!(text.contains(&format!("thing {} done", super::super::TRAIL_MOST + 4)));
        assert!(
            !text.contains("thing 4 done"),
            "the oldest entries are dropped"
        );
    }
}

mod spare {
    use super::super::Spare;

    /// Kept things come back by size, the last given first, and nothing is
    /// kept past the limit.
    #[test]
    fn kept_things_come_back_by_size_within_the_limit() {
        let mut spare: Spare<u32> = Spare::new();
        assert_eq!(spare.take(4), None);
        assert_eq!(spare.give(1, 4, 10), None);
        assert_eq!(spare.give(2, 1, 10), None);
        assert_eq!(spare.give(3, 4, 10), None);
        assert_eq!(spare.bytes, 9);
        // Past the limit: handed back, nothing changes.
        assert_eq!(spare.give(4, 4, 10), Some(4));
        assert_eq!(spare.bytes, 9);
        assert_eq!(spare.take(2), None, "no thing of that size");
        assert_eq!(spare.take(4), Some(3));
        assert_eq!(spare.take(4), Some(1));
        assert_eq!(spare.take(4), None);
        assert_eq!(spare.take(1), Some(2));
        assert_eq!(spare.bytes, 0);
        assert!(spare.kept.is_empty());
    }
}

mod batch {
    use super::super::Batch;
    use std::sync::Arc;

    #[test]
    fn a_texture_is_checked_once_per_submission() {
        let mut batch = Batch::default();
        let (a, b) = (Arc::new([0u8; 16]), Arc::new([0u8; 16]));
        // The first list sampling each texture checks it ...
        assert!(batch.texture_due(&a));
        assert!(batch.texture_due(&b));
        // ... later lists of the same submission do not.
        assert!(!batch.texture_due(&a));
        assert!(!batch.texture_due(&a.clone()));
        assert!(!batch.texture_due(&b));
    }

    #[test]
    fn title_writes_make_textures_due_again() {
        let mut batch = Batch::default();
        let a = Arc::new(1u32);
        assert!(batch.texture_due(&a));
        assert!(!batch.texture_due(&a));
        // A copy into title memory or work that may write it may have
        // changed the texture's memory.
        batch.title_written();
        assert!(batch.texture_due(&a));
        assert!(!batch.texture_due(&a));
        // A fresh submission starts over.
        let mut next = Batch::default();
        assert!(next.texture_due(&a));
    }

    #[test]
    fn checks_started_early_are_joined_where_pending_checks_ran() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let done = Arc::new(AtomicUsize::new(0));
        let start = |batch: &mut Batch, n: usize| {
            for i in 0..n {
                let done = done.clone();
                batch.early.start(move || {
                    if i % 2 == 0 {
                        std::thread::sleep(std::time::Duration::from_millis(1));
                    }
                    done.fetch_add(1, Ordering::SeqCst);
                });
            }
        };
        let mut batch = Batch::default();
        // Before a step that writes title memory.
        start(&mut batch, 4);
        assert!(batch.early.started());
        batch.title_written();
        assert_eq!(done.load(Ordering::SeqCst), 4);
        assert!(!batch.early.started());
        // Before a host callback or the preludes of the lists so far.
        start(&mut batch, 3);
        batch.flush_textures();
        assert_eq!(done.load(Ordering::SeqCst), 7);
        // At the end of the submission's last flush.
        start(&mut batch, 5);
        batch.flush();
        assert_eq!(done.load(Ordering::SeqCst), 12);
        assert!(!batch.early.started());
        // A batch let go with checks started waits for them.
        start(&mut batch, 2);
        drop(batch);
        assert_eq!(done.load(Ordering::SeqCst), 14);
    }

    #[test]
    fn refreshes_merge_up_to_the_next_step_between_them() {
        use super::super::{ShadowStep, push_refresh_step};
        let (mut span, mut steps) = (None, Vec::new());
        push_refresh_step(&mut span, &mut steps, 100, 200);
        push_refresh_step(&mut span, &mut steps, 50, 120);
        push_refresh_step(&mut span, &mut steps, 180, 260);
        assert_eq!(steps, [ShadowStep::Refresh(50, 260)]);
        steps.push(ShadowStep::Barrier);
        push_refresh_step(&mut span, &mut steps, 300, 400);
        steps.push(ShadowStep::Write(320, 8));
        push_refresh_step(&mut span, &mut steps, 10, 20);
        push_refresh_step(&mut span, &mut steps, 30, 40);
        assert_eq!(
            steps,
            [
                ShadowStep::Refresh(50, 260),
                ShadowStep::Barrier,
                ShadowStep::Refresh(300, 400),
                ShadowStep::Write(320, 8),
                ShadowStep::Refresh(10, 40),
            ]
        );
        assert_eq!(span, Some((10, 400)));
    }

    #[test]
    fn a_copys_refreshes_and_shader_writes_keep_their_order() {
        use super::super::{CopyOps, ShadowStep};
        let mut ops: CopyOps<&str> = CopyOps::default();
        ops.refresh(1, &"a", 100, 200);
        ops.refresh(2, &"b", 0, 10);
        ops.gpu_write(1, &"a", 150, 20);
        ops.refresh(1, &"a", 300, 400);
        ops.refresh(2, &"b", 5, 30);
        ops.refresh(1, &"a", 50, 60);
        ops.gpu_write(2, &"b", 8, 4);
        ops.gpu_write(1, &"a", 150, 20);
        ops.refresh(1, &"a", 500, 600);
        ops.gpu_write(2, &"b", 8, 4);
        ops.refresh(2, &"b", 40, 50);
        ops.gpu_write(3, &"c", 0, 64);
        let copies = ops.take();
        let got: Vec<_> = copies
            .iter()
            .map(|c| (c.shadow, c.refresh_span, c.steps.as_slice()))
            .collect();
        assert_eq!(
            got,
            [
                (
                    "a",
                    Some((50, 600)),
                    &[
                        ShadowStep::Refresh(100, 200),
                        ShadowStep::Write(150, 20),
                        ShadowStep::Refresh(50, 400),
                        ShadowStep::Barrier,
                        ShadowStep::Refresh(500, 600),
                    ][..]
                ),
                (
                    "b",
                    Some((0, 50)),
                    &[
                        ShadowStep::Refresh(0, 30),
                        ShadowStep::Write(8, 4),
                        ShadowStep::Refresh(40, 50),
                    ][..]
                ),
                ("c", None, &[ShadowStep::Write(0, 64)][..]),
            ]
        );
        ops.gpu_write(1, &"a", 150, 20);
        assert!(ops.take().is_empty());
        ops.refresh(1, &"a", 0, 8);
        ops.gpu_write(1, &"a", 150, 20);
        ops.refresh(1, &"a", 16, 24);
        let copies = ops.take();
        assert_eq!(copies.len(), 1);
        assert_eq!(
            copies[0].steps,
            [
                ShadowStep::Refresh(0, 8),
                ShadowStep::Barrier,
                ShadowStep::Refresh(16, 24)
            ]
        );
    }

    #[test]
    fn a_range_given_again_keeps_no_bytes_across_flushes() {
        use super::super::{CopyOps, ShadowStep};
        let declarations: [(usize, usize, usize); 10] = [
            (7, 64, 32),
            (3, 0, 16),
            (7, 64, 32),
            (7, 0, 8),
            (3, 0, 16),
            (9, 4, 4),
            (3, 32, 16),
            (7, 64, 16),
            (9, 4, 4),
            (7, 0, 8),
        ];
        let mut ops: CopyOps<usize> = CopyOps::default();
        let mut writes: Vec<(usize, usize, usize)> = Vec::new();
        for (i, &(copy, address, len)) in declarations.iter().enumerate() {
            ops.gpu_write(copy, &copy, address, len);
            if i == 4 || i == declarations.len() - 1 {
                for c in ops.take() {
                    for step in &c.steps {
                        match *step {
                            ShadowStep::Write(address, len) => {
                                writes.push((c.shadow, address, len))
                            }
                            ref other => panic!("only writes were given, not {other:?}"),
                        }
                    }
                }
            }
        }
        assert_eq!(
            writes,
            [
                (7, 64, 32),
                (7, 0, 8),
                (3, 0, 16),
                (9, 4, 4),
                (3, 32, 16),
                (7, 64, 16)
            ]
        );
        let mut firsts: Vec<_> = declarations.to_vec();
        firsts.sort_unstable();
        firsts.dedup();
        writes.sort_unstable();
        assert_eq!(writes, firsts);
    }

    /// The hashing a submission does for textures in physical-page memory,
    /// with every list checking the textures it samples (before) and with
    /// each checked once (after). Run with
    /// `cargo test --release -p victory-gpu -- --ignored --nocapture texture_checks`.
    #[test]
    #[ignore = "benchmark"]
    fn texture_checks_per_submission() {
        const TEXTURES: usize = 48;
        const SIZE: usize = 4 << 20;
        const LISTS: usize = 12;
        let textures: Vec<Arc<Vec<u8>>> = (0..TEXTURES)
            .map(|t| Arc::new((0..SIZE).map(|i| (i * 31 + t) as u8).collect()))
            .collect();
        let submit = |dedupe: bool| {
            let mut batch = Batch::default();
            let mut hashed = 0usize;
            let started = std::time::Instant::now();
            for _ in 0..LISTS {
                for t in &textures {
                    if !dedupe || batch.texture_due(t) {
                        std::hint::black_box(blake3::hash(t));
                        hashed += 1;
                    }
                }
            }
            (started.elapsed(), hashed)
        };
        submit(false); // warm up
        let (before, n_before) = submit(false);
        let (after, n_after) = submit(true);
        assert_eq!((n_before, n_after), (TEXTURES * LISTS, TEXTURES));
        println!(
            "{LISTS} lists sampling {TEXTURES} textures of {} MiB: \
             before {n_before} hashes in {before:?}, after {n_after} in {after:?} ({:.1}x)",
            SIZE >> 20,
            before.as_secs_f64() / after.as_secs_f64()
        );
    }

    #[test]
    fn vram_idle_ms_scales_with_pressure() {
        use super::super::vram_idle_ms;
        let gib = 1024 * 1024 * 1024;
        // Normal (< 75%)
        assert_eq!(vram_idle_ms(Some([10 * gib, 0, 0, 0])), 5_000);
        assert_eq!(vram_idle_ms(Some([10 * gib, 7 * gib, 0, 0])), 5_000);
        assert_eq!(vram_idle_ms(Some([100, 74, 0, 0])), 5_000);
        // Moderate (75%..85%)
        assert_eq!(vram_idle_ms(Some([100, 75, 0, 0])), 2_500);
        assert_eq!(vram_idle_ms(Some([100, 84, 0, 0])), 2_500);
        // High (>= 85%, including over-budget)
        assert_eq!(vram_idle_ms(Some([100, 85, 0, 0])), 1_000);
        assert_eq!(vram_idle_ms(Some([10 * gib, 12 * gib, 0, 0])), 1_000);
        // Missing or zero budget falls back to normal
        assert_eq!(vram_idle_ms(None), 5_000);
        assert_eq!(vram_idle_ms(Some([0, 100, 0, 0])), 5_000);
    }

    #[test]
    fn ram_idle_ms_scales_with_pressure() {
        use super::super::ram_idle_ms;
        let gib = 1024 * 1024 * 1024;
        // Normal (>= 4 GiB)
        assert_eq!(ram_idle_ms(Some(8 * gib)), 10_000);
        assert_eq!(ram_idle_ms(Some(4 * gib)), 10_000);
        // Moderate (2 GiB..4 GiB)
        assert_eq!(ram_idle_ms(Some(4 * gib - 1)), 5_000);
        assert_eq!(ram_idle_ms(Some(2 * gib)), 5_000);
        // High (< 2 GiB)
        assert_eq!(ram_idle_ms(Some(2 * gib - 1)), 2_000);
        assert_eq!(ram_idle_ms(Some(0)), 2_000);
        // Missing falls back to normal
        assert_eq!(ram_idle_ms(None), 10_000);
    }
}

#[cfg(all(target_arch = "x86_64", windows))]
mod first_diff_tests {
    use super::super::*;

    #[test]
    fn diff_copy_direct_handles_diffs_spans_tails_and_faults() {
        if !std::arch::is_x86_feature_detected!("avx2") || !ensure_veh() {
            return;
        }
        const N: usize = 64 * 1024;
        let src: Vec<u8> = (0..N)
            .map(|i| (i.wrapping_mul(37) ^ (i >> 8)) as u8)
            .collect();
        let mut held = src.clone();
        let mut runs = Vec::new();

        // (a) Identical 64 KiB buffer -> Ok(()), no runs.
        assert_eq!(
            diff_copy_direct(src.as_ptr(), &mut held, 0, &mut runs),
            Ok(())
        );
        assert!(runs.is_empty());
        assert_eq!(held, src);

        // (b) Single-byte diffs at exact offsets across word & chunk boundaries.
        for &off in &[0usize, 31, 32, 2047, 2048, 4097, 65535] {
            held[off] ^= 0x5A;
            runs.clear();
            assert_eq!(
                diff_copy_direct(src.as_ptr(), &mut held, 128, &mut runs),
                Ok(())
            );
            assert_eq!(held, src);
            assert_eq!(runs, vec![(128 + (off & !31), 32)]);
        }

        // (c) Multi-span contiguous (including across u64 word boundary) & separated diffs.
        for &off in &[64usize, 96, 127, 2047, 2048, 2079, 4096] {
            held[off] ^= 0xA5;
        }
        runs.clear();
        runs.push((0, 64)); // pre-existing adjacent run merges with chunk at offset 64
        assert_eq!(
            diff_copy_direct(src.as_ptr(), &mut held, 0, &mut runs),
            Ok(())
        );
        assert_eq!(held, src);
        assert_eq!(runs, vec![(0, 128), (2016, 64), (4096, 32)]);

        // (d) All-different 64 KiB buffer (every mask word == u64::MAX) -> single merged run.
        for b in &mut held {
            *b ^= 0xFF;
        }
        runs.clear();
        assert_eq!(
            diff_copy_direct(src.as_ptr(), &mut held, 256, &mut runs),
            Ok(())
        );
        assert_eq!(held, src);
        assert_eq!(runs, vec![(256, N)]);

        // (e) < 32-byte tail (len = 45): [0..32] copied & recorded, tail left for caller.
        let mut small_held = src[..45].to_vec();
        small_held[5] ^= 0x33;
        small_held[40] ^= 0x77;
        let untouched_tail = small_held[32..45].to_vec();
        runs.clear();
        assert_eq!(
            diff_copy_direct(src.as_ptr(), &mut small_held, 512, &mut runs),
            Err(32)
        );
        assert_eq!(&small_held[..32], &src[..32]);
        assert_eq!(&small_held[32..45], &untouched_tail[..]);
        assert_eq!(runs, vec![(512, 32)]);

        // (f) Mid-chunk MEM_RESERVE page after 4 KiB (with diffs in first 4 KiB).
        use windows_sys::Win32::System::Memory::*;
        // SAFETY: private reserved 8 KiB region with only the first 4 KiB committed.
        unsafe {
            let base =
                VirtualAlloc(std::ptr::null(), 8192, MEM_RESERVE, PAGE_NOACCESS).cast::<u8>();
            assert!(!base.is_null());
            assert_eq!(
                VirtualAlloc(base.cast(), 4096, MEM_COMMIT, PAGE_READWRITE),
                base.cast()
            );
            std::ptr::copy_nonoverlapping(src.as_ptr(), base, 4096);
            *base.add(100) ^= 0x5A;
            *base.add(2048) ^= 0xC3;
            let mut dst = src[..8192].to_vec();
            runs.clear();
            assert_eq!(diff_copy_direct(base, &mut dst, 1024, &mut runs), Err(4096));
            assert_eq!(dst[100], *base.add(100));
            assert_eq!(dst[2048], *base.add(2048));
            assert_eq!(runs, vec![(1024 + 96, 32), (1024 + 2048, 32)]);
            assert_eq!(FAST_FAULT_RIP.with(std::cell::Cell::get), 0);
            assert_eq!(FAST_GUARDED.with(std::cell::Cell::get), 0);
            assert_eq!(FAST_COPY.with(std::cell::Cell::get), (0, 0, false));
            assert_ne!(VirtualFree(base.cast(), 0, MEM_RELEASE), 0);
        }

        // (g) Mid-chunk PAGE_GUARD fault clears FAST_GUARDED and re-arms the guard.
        // SAFETY: private 8 KiB committed region with PAGE_GUARD on the second 4 KiB page.
        unsafe {
            let base = VirtualAlloc(
                std::ptr::null(),
                8192,
                MEM_COMMIT | MEM_RESERVE,
                PAGE_READWRITE,
            )
            .cast::<u8>();
            assert!(!base.is_null());
            std::ptr::copy_nonoverlapping(src.as_ptr(), base, 8192);
            *base.add(64) ^= 0x11;
            let mut old = 0u32;
            assert_ne!(
                VirtualProtect(
                    base.add(4096).cast(),
                    4096,
                    PAGE_READWRITE | PAGE_GUARD,
                    &mut old
                ),
                0
            );
            let mut dst = src[..8192].to_vec();
            runs.clear();
            assert_eq!(diff_copy_direct(base, &mut dst, 0, &mut runs), Err(4096));
            assert_eq!(dst[64], *base.add(64));
            assert_eq!(runs, vec![(64, 32)]);
            assert_eq!(FAST_FAULT_RIP.with(std::cell::Cell::get), 0);
            assert_eq!(FAST_GUARDED.with(std::cell::Cell::get), 0);
            assert_eq!(FAST_COPY.with(std::cell::Cell::get), (0, 0, false));
            let mut info: MEMORY_BASIC_INFORMATION = core::mem::zeroed();
            assert_ne!(
                VirtualQuery(
                    base.add(4096).cast(),
                    &mut info,
                    size_of::<MEMORY_BASIC_INFORMATION>(),
                ),
                0
            );
            assert_ne!(info.Protect & PAGE_GUARD, 0);
            assert_ne!(VirtualFree(base.cast(), 0, MEM_RELEASE), 0);
        }
    }

    #[test]
    fn compare_hash_eight_way_unroll_matches_scalar_loop() {
        const SEED: [u32; 4] = [0x9E37_79B9, 0x85EB_CA6B, 0xC2B2_AE35, 0x27D4_EB2F];
        const STEP: [u32; 4] = [0x0100_0193, 0x9E37_79B1, 0x85EB_CA77, 0xC2B2_AE3D];
        let step = |h: [u32; 4], v: [u32; 4]| -> [u32; 4] {
            let m = [
                (h[0] ^ v[0]).wrapping_mul(STEP[0]),
                (h[1] ^ v[1]).wrapping_mul(STEP[1]),
                (h[2] ^ v[2]).wrapping_mul(STEP[2]),
                (h[3] ^ v[3]).wrapping_mul(STEP[3]),
            ];
            [
                m[1] ^ (m[0] >> 15),
                m[2] ^ (m[1] >> 15),
                m[3] ^ (m[2] >> 15),
                m[0] ^ (m[3] >> 15),
            ]
        };
        let load4 = |buf: &[u8], at: u32| -> [u32; 4] {
            let s = &buf[at as usize..at as usize + 16];
            [
                u32::from_le_bytes(s[0..4].try_into().unwrap()),
                u32::from_le_bytes(s[4..8].try_into().unwrap()),
                u32::from_le_bytes(s[8..12].try_into().unwrap()),
                u32::from_le_bytes(s[12..16].try_into().unwrap()),
            ]
        };
        let buf: Vec<u8> = (0..65568usize)
            .map(|i| ((i.wrapping_mul(131) ^ (i >> 3)) & 0xff) as u8)
            .collect();
        for &(lo, hi) in &[
            (0u32, 0u32),
            (0, 16),
            (0, 1024),
            (0, 7168),
            (0, 8192),
            (0, 8208),
            (0, 65536),
            (0, 65568),
            (32, 16400),
        ] {
            for thread in [0u32, 1, 31, 63] {
                let init = SEED.map(|x| x.wrapping_add(thread));
                let mut ref_h = init;
                let mut at = lo + thread * 16;
                while at < hi {
                    ref_h = step(ref_h, load4(&buf, at));
                    at += 1024;
                }
                let mut unrolled_h = init;
                let mut at = lo + thread * 16;
                while at + 7168 < hi {
                    for k in 0..8u32 {
                        unrolled_h = step(unrolled_h, load4(&buf, at + k * 1024));
                    }
                    at += 8192;
                }
                while at < hi {
                    unrolled_h = step(unrolled_h, load4(&buf, at));
                    at += 1024;
                }
                assert_eq!(unrolled_h, ref_h, "lo={lo} hi={hi} thread={thread}");
            }
        }
    }
}
