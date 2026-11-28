use super::*;
use windows::core::Interface;

static EPOCH_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn cpu_heap(slots: u32) -> Option<(ID3D12DescriptorHeap, usize, usize)> {
    let host = crate::host::host()?;
    // SAFETY: plain object creation and queries; CPU-only descriptor heap.
    unsafe {
        let heap: ID3D12DescriptorHeap = host
            .device
            .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                NumDescriptors: slots,
                ..Default::default()
            })
            .ok()?;
        let cpu = heap.GetCPUDescriptorHandleForHeapStart().ptr;
        let step = host
            .device
            .GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV)
            as usize;
        Some((heap, cpu, step))
    }
}

fn slot_bytes(base: usize, step: usize, index: u32) -> Vec<u8> {
    let ptr = (base + index as usize * step) as *const u8;
    // SAFETY: `index` is within a CPU-only descriptor heap.
    unsafe { std::slice::from_raw_parts(ptr, step) }.to_vec()
}

fn made_bytes(view: &View) -> Vec<u8> {
    let (_heap, base, step) = cpu_heap(1).unwrap();
    make(view, D3D12_CPU_DESCRIPTOR_HANDLE { ptr: base }).unwrap();
    slot_bytes(base, step, 0)
}

/// A buffer UAV view of a `TitleBuffer`.
fn buffer_uav_view(title: &view_cache::tests::TitleBuffer) -> (View, [u32; 3]) {
    let (View::Placed(_, resource, srv_desc), mut key) = title.view() else {
        unreachable!()
    };
    let mut desc = [0u32; 16];
    desc[0] = 39;
    desc[1] = 1;
    desc[4] = srv_desc[6];
    desc[8] = D3D12_BUFFER_UAV_FLAG_RAW.0 as u32;
    desc[10] = srv_desc[10];
    desc[11] = srv_desc[11];
    key[2] ^= 0x1000;
    (View::Placed(true, resource, desc), key)
}

#[test]
fn the_memo_switch_is_live_and_off_by_default() {
    assert_eq!(ON.name(), "VICTORY_TABLE_VIEW_MEMO");
    assert!(!on() || std::env::var_os("VICTORY_TABLE_VIEW_MEMO").is_some());
}

/// `CopyBatch` coalesces contiguous destination/source runs into one range
/// (`CopyDescriptorsSimple` path) and tracks scattered/gapped runs cleanly,
/// rejecting pushes once inline capacity is reached.
#[test]
fn copy_batch_coalesces_adjacent_and_tracks_scattered_runs() {
    let mut b = CopyBatch::new();
    assert!(b.is_empty());
    // Contiguous in both dst and src -> 1 dst range, 1 src range.
    for k in 0..8 {
        assert!(b.push_raw(100 + k, 20 + k));
    }
    assert_eq!(b.len, 1);
    assert_eq!(b.runs[0].dst, 100);
    assert_eq!(b.runs[0].src, 20);
    assert_eq!(b.runs[0].len, 8);
    assert_eq!(b.total, 8);

    // Adjacent dst, jumped src -> starts a 2nd run of 2.
    assert!(b.push_raw(108, 50));
    assert!(b.push_raw(109, 51));
    assert_eq!(b.len, 2);
    assert_eq!(b.runs[1].dst, 108);
    assert_eq!(b.runs[1].src, 50);
    assert_eq!(b.runs[1].len, 2);

    // Gapped dst (e.g. a CBV at 110) -> starts a 3rd run.
    assert!(b.push_raw(111, 52));
    assert_eq!(b.len, 3);
    assert_eq!(b.runs[2].dst, 111);
    assert_eq!(b.runs[2].src, 52);
    assert_eq!(b.runs[2].len, 1);
    assert_eq!(b.total, 11);

    // Filling past BATCH_CAP returns false without corrupting state.
    let mut full = CopyBatch::new();
    for i in 0..BATCH_CAP as u32 {
        assert!(full.push_raw(i * 2, i * 2));
    }
    assert_eq!(full.len, BATCH_CAP);
    assert!(!full.push_raw(1000, 1000));
    assert_eq!(full.total, BATCH_CAP as u32);
}

/// Texture memo hit copies the exact descriptor; when the texture's host copy
/// is let go and remade mid-list, the memo epoch invalidates the entry so the
/// next draw gets the new host copy's descriptor, never the stale one.
#[test]
fn texture_host_copy_replaced_mid_list_never_gets_stale_descriptor() {
    let _guard = EPOCH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (Some(host), Some(mut memo), Some((_dst_heap, dst_base, step))) =
        (crate::host::host(), ViewMemo::new(), cpu_heap(16))
    else {
        return; // no host GPU here
    };
    let device = &host.device;
    let image = crate::resource::test_lazy_texture();
    let view = View::Shader(image as usize, None);
    let key = [super::super::VIEW_TAG, 0x7d_0001, 11];
    let mut batch = CopyBatch::new();

    // First draw (miss): resolved via ViewCache, memoized.
    let mut first_kept = None;
    let ok = memo.resolve_slot(
        1,
        key,
        &view,
        0,
        D3D12_CPU_DESCRIPTOR_HANDLE { ptr: dst_base },
        &mut batch,
        true,
        true,
        |b, sb, ss| b.flush_raw(device, dst_base, step, sb, ss),
        |r| first_kept = Some(r.clone()),
    );
    assert!(ok);
    batch.flush_raw(device, dst_base, step, memo.start, memo.step);
    let first_bytes = slot_bytes(dst_base, step, 0);
    assert_eq!(first_bytes, made_bytes(&view));
    let first_raw = first_kept.as_ref().unwrap().as_raw();

    // Second draw in same list (memo hit): identical bytes & resource.
    let mut hit_kept = None;
    let ok = memo.resolve_slot(
        1,
        key,
        &view,
        1,
        D3D12_CPU_DESCRIPTOR_HANDLE {
            ptr: dst_base + step,
        },
        &mut batch,
        true,
        true,
        |b, sb, ss| b.flush_raw(device, dst_base, step, sb, ss),
        |r| hit_kept = Some(r.clone()),
    );
    assert!(ok);
    batch.flush_raw(device, dst_base, step, memo.start, memo.step);
    assert_eq!(hit_kept.as_ref().unwrap().as_raw(), first_raw);
    assert_eq!(slot_bytes(dst_base, step, 1), first_bytes);

    // Mid-list: texture's host copy is evicted and remade (same recording = 1!).
    crate::resource::test_let_go_host_copy(image);
    let now = crate::resource::host_resource(image).unwrap().clone();
    assert_ne!(now.as_raw(), first_raw, "new host copy created");

    // Third draw in the SAME list (recording = 1): epoch moved -> must rebuild!
    let mut after_kept = None;
    let ok = memo.resolve_slot(
        1,
        key,
        &view,
        2,
        D3D12_CPU_DESCRIPTOR_HANDLE {
            ptr: dst_base + 2 * step,
        },
        &mut batch,
        true,
        true,
        |b, sb, ss| b.flush_raw(device, dst_base, step, sb, ss),
        |r| after_kept = Some(r.clone()),
    );
    assert!(ok);
    batch.flush_raw(device, dst_base, step, memo.start, memo.step);
    assert_eq!(
        after_kept.as_ref().unwrap().as_raw(),
        now.as_raw(),
        "must show the new host copy, not the evicted one"
    );
    assert_eq!(
        slot_bytes(dst_base, step, 2),
        made_bytes(&view),
        "descriptor bytes must match the new host copy"
    );
    drop((first_kept, hit_kept, after_kept, now, memo));
    crate::resource::test_release_texture(image);
}

/// Buffer SRV and UAV memo hits match `make`; when the title buffer is released
/// and another created at the same address mid-list (same recording, same record),
/// the shadow epoch invalidates the memo and the slot gets the new buffer's descriptor.
#[test]
fn buffer_released_and_recreated_mid_list_never_gets_stale_descriptor() {
    let _guard = EPOCH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (Some(host), Some(mut memo), Some((_dst_heap, dst_base, step))) =
        (crate::host::host(), ViewMemo::new(), cpu_heap(16))
    else {
        return; // no host GPU here
    };
    let device = &host.device;
    let mut title = view_cache::tests::TitleBuffer::new(0x7e57_c001);
    let (srv_view, srv_key) = title.view();
    let (uav_view, uav_key) = buffer_uav_view(&title);
    let mut batch = CopyBatch::new();

    // First draw: both SRV (via ViewCache) and UAV (via make) populate memo.
    let mut first_srv = None;
    let mut first_uav = None;
    assert!(memo.resolve_slot(
        1,
        srv_key,
        &srv_view,
        0,
        D3D12_CPU_DESCRIPTOR_HANDLE { ptr: dst_base },
        &mut batch,
        true,
        true,
        |b, sb, ss| b.flush_raw(device, dst_base, step, sb, ss),
        |r| first_srv = Some(r.clone()),
    ));
    assert!(memo.resolve_slot(
        1,
        uav_key,
        &uav_view,
        1,
        D3D12_CPU_DESCRIPTOR_HANDLE {
            ptr: dst_base + step
        },
        &mut batch,
        true,
        true,
        |b, sb, ss| b.flush_raw(device, dst_base, step, sb, ss),
        |r| first_uav = Some(r.clone()),
    ));
    // Both contiguous slots flush in ONE CopyDescriptorsSimple(2) call!
    assert_eq!(batch.len, 1);
    assert_eq!(batch.total, 2);
    batch.flush_raw(device, dst_base, step, memo.start, memo.step);
    let old_srv_bytes = slot_bytes(dst_base, step, 0);
    let old_uav_bytes = slot_bytes(dst_base, step, 1);
    assert_eq!(old_srv_bytes, made_bytes(&srv_view));
    assert_eq!(old_uav_bytes, made_bytes(&uav_view));

    // Second draw (same list): memo hits for both, batched in one call.
    let mut hit_srv = None;
    let mut hit_uav = None;
    assert!(memo.resolve_slot(
        1,
        srv_key,
        &srv_view,
        2,
        D3D12_CPU_DESCRIPTOR_HANDLE {
            ptr: dst_base + 2 * step,
        },
        &mut batch,
        true,
        true,
        |b, sb, ss| b.flush_raw(device, dst_base, step, sb, ss),
        |r| hit_srv = Some(r.clone()),
    ));
    assert!(memo.resolve_slot(
        1,
        uav_key,
        &uav_view,
        3,
        D3D12_CPU_DESCRIPTOR_HANDLE {
            ptr: dst_base + 3 * step,
        },
        &mut batch,
        true,
        true,
        |b, sb, ss| b.flush_raw(device, dst_base, step, sb, ss),
        |r| hit_uav = Some(r.clone()),
    ));
    batch.flush_raw(device, dst_base, step, memo.start, memo.step);
    assert_eq!(
        hit_srv.as_ref().unwrap().as_raw(),
        first_srv.as_ref().unwrap().as_raw()
    );
    assert_eq!(
        hit_uav.as_ref().unwrap().as_raw(),
        first_uav.as_ref().unwrap().as_raw()
    );
    assert_eq!(slot_bytes(dst_base, step, 2), old_srv_bytes);
    assert_eq!(slot_bytes(dst_base, step, 3), old_uav_bytes);

    // Mid-list: release buffer and recreate at the exact same address!
    title.made_again(0x7e57_c002);

    // Third draw (same list, recording = 1): epoch moved -> must rebuild both!
    let mut new_srv = None;
    let mut new_uav = None;
    assert!(memo.resolve_slot(
        1,
        srv_key,
        &srv_view,
        4,
        D3D12_CPU_DESCRIPTOR_HANDLE {
            ptr: dst_base + 4 * step,
        },
        &mut batch,
        true,
        true,
        |b, sb, ss| b.flush_raw(device, dst_base, step, sb, ss),
        |r| new_srv = Some(r.clone()),
    ));
    assert!(memo.resolve_slot(
        1,
        uav_key,
        &uav_view,
        5,
        D3D12_CPU_DESCRIPTOR_HANDLE {
            ptr: dst_base + 5 * step,
        },
        &mut batch,
        true,
        true,
        |b, sb, ss| b.flush_raw(device, dst_base, step, sb, ss),
        |r| new_uav = Some(r.clone()),
    ));
    batch.flush_raw(device, dst_base, step, memo.start, memo.step);
    assert_ne!(
        new_srv.as_ref().unwrap().as_raw(),
        first_srv.as_ref().unwrap().as_raw(),
        "SRV must point to new host buffer"
    );
    assert_ne!(
        new_uav.as_ref().unwrap().as_raw(),
        first_uav.as_ref().unwrap().as_raw(),
        "UAV must point to new host buffer"
    );
    assert_eq!(slot_bytes(dst_base, step, 4), made_bytes(&srv_view));
    assert_eq!(slot_bytes(dst_base, step, 5), made_bytes(&uav_view));
    assert_ne!(slot_bytes(dst_base, step, 4), old_srv_bytes);
    assert_ne!(slot_bytes(dst_base, step, 5), old_uav_bytes);
    drop((
        first_srv, first_uav, hit_srv, hit_uav, new_srv, new_uav, memo,
    ));
}

/// Batched `CopyDescriptors` (scattered sources and gapped destinations) writes
/// the exact same bytes to every destination slot as individual `make` calls,
/// and a mid-table stale rebuild flushes prior slots before overwriting the slot.
#[test]
fn batched_scattered_and_mid_table_stale_copy_exact_bytes() {
    let _guard = EPOCH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (Some(host), Some(mut memo), Some((_dst_heap, dst_base, step))) =
        (crate::host::host(), ViewMemo::new(), cpu_heap(16))
    else {
        return;
    };
    let device = &host.device;
    let img_a = crate::resource::test_lazy_texture();
    let img_b = crate::resource::test_lazy_texture();
    let mut title = view_cache::tests::TitleBuffer::new(0x7e57_c003);
    let view_a = View::Shader(img_a as usize, None);
    let view_b = View::Shader(img_b as usize, None);
    let (view_buf, key_buf) = title.view();
    let key_a = [super::super::VIEW_TAG, 0x7d_0010, 1];
    let key_b = [super::super::VIEW_TAG, 0x7d_0011, 2];

    let mut batch = CopyBatch::new();
    // Populate in order: A (slot 0), B (slot 1), Buf (slot 2).
    for (k, (key, view)) in [(key_a, &view_a), (key_b, &view_b), (key_buf, &view_buf)]
        .into_iter()
        .enumerate()
    {
        assert!(memo.resolve_slot(
            7,
            key,
            view,
            k as u32,
            D3D12_CPU_DESCRIPTOR_HANDLE {
                ptr: dst_base + k * step,
            },
            &mut batch,
            true,
            true,
            |b, sb, ss| b.flush_raw(device, dst_base, step, sb, ss),
            |_| {},
        ));
    }
    batch.flush_raw(device, dst_base, step, memo.start, memo.step);

    // Now build a scattered + gapped table: dst 4 <- Buf(2), dst 5 <- A(0), (gap at 6), dst 7 <- B(1).
    let table_ops = [
        (4u32, key_buf, &view_buf),
        (5, key_a, &view_a),
        (7, key_b, &view_b),
    ];
    for (dst_idx, key, view) in table_ops {
        assert!(memo.resolve_slot(
            7,
            key,
            view,
            dst_idx,
            D3D12_CPU_DESCRIPTOR_HANDLE {
                ptr: dst_base + dst_idx as usize * step,
            },
            &mut batch,
            true,
            true,
            |b, sb, ss| b.flush_raw(device, dst_base, step, sb, ss),
            |_| {},
        ));
    }
    // Exercises CopyDescriptors (multiple dst ranges and multiple src ranges).
    assert_eq!(batch.len, 3);
    batch.flush_raw(device, dst_base, step, memo.start, memo.step);
    assert_eq!(slot_bytes(dst_base, step, 4), made_bytes(&view_buf));
    assert_eq!(slot_bytes(dst_base, step, 5), made_bytes(&view_a));
    assert_eq!(slot_bytes(dst_base, step, 7), made_bytes(&view_b));

    // Mid-table stale rebuild: slot 8 hits old Buf, then buffer is recreated,
    // then slot 9 sees stale Buf in the SAME table -> slot 8 must flush BEFORE
    // slot 2 is overwritten, so slot 8 keeps old bytes and slot 9 gets new bytes!
    let old_buf_bytes = made_bytes(&view_buf);
    assert!(memo.resolve_slot(
        7,
        key_buf,
        &view_buf,
        8,
        D3D12_CPU_DESCRIPTOR_HANDLE {
            ptr: dst_base + 8 * step,
        },
        &mut batch,
        true,
        true,
        |b, sb, ss| b.flush_raw(device, dst_base, step, sb, ss),
        |_| {},
    ));
    assert!(!batch.is_empty());
    title.made_again(0x7e57_c004);
    assert!(memo.resolve_slot(
        7,
        key_buf,
        &view_buf,
        9,
        D3D12_CPU_DESCRIPTOR_HANDLE {
            ptr: dst_base + 9 * step,
        },
        &mut batch,
        true,
        true,
        |b, sb, ss| b.flush_raw(device, dst_base, step, sb, ss),
        |_| {},
    ));
    batch.flush_raw(device, dst_base, step, memo.start, memo.step);
    let new_buf_bytes = made_bytes(&view_buf);
    assert_ne!(new_buf_bytes, old_buf_bytes);
    assert_eq!(slot_bytes(dst_base, step, 8), old_buf_bytes);
    assert_eq!(slot_bytes(dst_base, step, 9), new_buf_bytes);

    drop(memo);
    crate::resource::test_release_texture(img_a);
    crate::resource::test_release_texture(img_b);
}

/// A buffer epoch bump does not invalidate memoized textures, and a texture
/// epoch bump does not invalidate memoized buffers.
#[test]
fn texture_and_buffer_epochs_are_independent() {
    let _guard = EPOCH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (Some(host), Some(mut memo), Some((_dst_heap, dst_base, step))) =
        (crate::host::host(), ViewMemo::new(), cpu_heap(8))
    else {
        return;
    };
    let device = &host.device;
    let img = crate::resource::test_lazy_texture();
    let mut title = view_cache::tests::TitleBuffer::new(0x7e57_c005);
    let tex_view = View::Shader(img as usize, None);
    let tex_key = [super::super::VIEW_TAG, 0x7d_0020, 5];
    let (buf_view, buf_key) = title.view();
    let mut batch = CopyBatch::new();

    for (i, (key, view)) in [(tex_key, &tex_view), (buf_key, &buf_view)]
        .into_iter()
        .enumerate()
    {
        assert!(memo.resolve_slot(
            9,
            key,
            view,
            i as u32,
            D3D12_CPU_DESCRIPTOR_HANDLE {
                ptr: dst_base + i * step,
            },
            &mut batch,
            true,
            true,
            |b, sb, ss| b.flush_raw(device, dst_base, step, sb, ss),
            |_| {},
        ));
    }
    batch.flush_raw(device, dst_base, step, memo.start, memo.step);

    // Recreate buffer: texture entry's epoch must still match resource::view_epoch().
    title.made_again(0x7e57_c006);
    let tex_entry = memo.entries.get(&record_key(tex_key)).unwrap();
    assert_eq!(tex_entry.epoch, crate::resource::view_epoch());
    let buf_entry = memo.entries.get(&record_key(buf_key)).unwrap();
    assert_ne!(buf_entry.epoch, crate::shadow::view_epoch());

    // Re-resolve buffer, then evict texture: buffer entry's epoch must still match.
    assert!(memo.resolve_slot(
        9,
        buf_key,
        &buf_view,
        2,
        D3D12_CPU_DESCRIPTOR_HANDLE {
            ptr: dst_base + 2 * step,
        },
        &mut batch,
        true,
        true,
        |b, sb, ss| b.flush_raw(device, dst_base, step, sb, ss),
        |_| {},
    ));
    batch.flush_raw(device, dst_base, step, memo.start, memo.step);
    crate::resource::test_let_go_host_copy(img);
    let buf_entry = memo.entries.get(&record_key(buf_key)).unwrap();
    assert_eq!(buf_entry.epoch, crate::shadow::view_epoch());
    let tex_entry = memo.entries.get(&record_key(tex_key)).unwrap();
    assert_ne!(tex_entry.epoch, crate::resource::view_epoch());

    drop(memo);
    crate::resource::test_release_texture(img);
}

/// End-to-end `HostHeap::make_memoized` + `CopyBatch::flush` + `forget`:
/// releasing a texture mid-list bumps `resource::view_epoch`, and `forget`
/// drops all memo entries (and their held `ID3D12Resource` refs) immediately.
#[test]
fn host_heap_make_memoized_release_and_forget() {
    let _guard = EPOCH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let Some(heap) = HostHeap::new(false, 64) else {
        return;
    };
    let rec = 0x55aa_0001u64;
    let img1 = crate::resource::test_lazy_texture();
    let view1 = View::Shader(img1 as usize, None);
    let key = [super::super::VIEW_TAG, 0x7d_0099, 77];
    let mut batch = CopyBatch::new();
    let d0 = heap.dynamic(2).unwrap();

    assert!(heap.make_memoized(rec, key, &view1, d0, &mut batch));
    assert!(heap.make_memoized(rec, key, &view1, d0 + 1, &mut batch));
    batch.flush(&heap);

    let epoch_before = crate::resource::view_epoch();
    // Release img1 mid-list and create img2.
    crate::resource::test_release_texture(img1);
    assert!(
        crate::resource::view_epoch() > epoch_before,
        "releasing a texture must advance resource::view_epoch"
    );
    let img2 = crate::resource::test_lazy_texture();
    let view2 = View::Shader(img2 as usize, None);
    let d1 = heap.dynamic(1).unwrap();
    assert!(heap.make_memoized(rec, key, &view2, d1, &mut batch));
    batch.flush(&heap);
    let held2 = heap.kept[d1 as usize]
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .as_raw();
    let want2 = crate::resource::host_resource(img2).unwrap().as_raw();
    assert_eq!(held2, want2, "rebuilt dynamic slot must keep the new texture");

    forget(rec);
    MEMO.with(|cell| {
        if let Some(m) = cell.borrow().as_ref() {
            assert!(m.entries.is_empty(), "forget must clear memo entries");
            assert_eq!(m.recording, 0);
        }
    });
    drop(heap);
    crate::resource::test_release_texture(img2);
}
