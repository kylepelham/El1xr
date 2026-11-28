use super::*;

const KEY: [u32; 3] = [super::super::VIEW_TAG, 0x7c_0001, 77];

/// A heap the shaders do not see, of one slot, and its slot.
fn one_slot() -> Option<(ID3D12DescriptorHeap, D3D12_CPU_DESCRIPTOR_HANDLE)> {
    let host = crate::host::host()?;
    // SAFETY: plain object creation and a query.
    unsafe {
        let heap: ID3D12DescriptorHeap = host
            .device
            .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                NumDescriptors: 1,
                ..Default::default()
            })
            .ok()?;
        let at = heap.GetCPUDescriptorHandleForHeapStart();
        Some((heap, at))
    }
}

fn bytes(at: D3D12_CPU_DESCRIPTOR_HANDLE) -> Vec<u8> {
    let step = cache().unwrap().step;
    // SAFETY: a slot of a heap the shaders do not see (CPU memory), `step` bytes.
    unsafe { std::slice::from_raw_parts(at.ptr as *const u8, step) }.to_vec()
}

/// The bytes `make` writes for `view` now, in a slot of its own.
fn made_bytes(view: &View) -> Vec<u8> {
    let (_heap, at) = one_slot().unwrap();
    make(view, at).unwrap();
    bytes(at)
}

fn cached(key: &[u32; 3], view: &View) -> bool {
    let View::Shader(title, _) = *view else {
        unreachable!()
    };
    let resource = title as crate::object::This;
    let copy = crate::resource::host_copy(resource).unwrap();
    let shows = Shows {
        title,
        copy: std::sync::Arc::as_ptr(&copy),
        first: crate::resource::first_shown(resource),
        prefilled: copy.prefilled.load(Relaxed),
    };
    let shard = cache().unwrap().shards[shard_of(key)].lock().unwrap();
    shard.found(key, &shows).is_some()
}

/// A slot copied from the cache holds the bytes `make` writes for the view
/// and the same host texture, the first time (made) and after (copied).
/// The host copy let go and made again: the view is made again for the new
/// one, not copied from the old. The cache keeps no host copy alive.
#[test]
fn a_cached_view_is_the_view_made_and_follows_its_host_copy() {
    let (Some(cache), Some((_heap, to))) = (cache(), one_slot()) else {
        return; // no host GPU here
    };
    let image = crate::resource::test_lazy_texture();
    let view = View::Shader(image as usize, None);
    let first = cache.copy(&view, KEY, to).expect("a texture view is kept");
    assert_eq!(Some(&first), crate::resource::host_resource(image));
    assert_eq!(bytes(to), made_bytes(&view));
    assert!(cached(&KEY, &view));
    let again = cache.copy(&view, KEY, to).unwrap();
    assert_eq!(again, first);
    assert_eq!(bytes(to), made_bytes(&view), "copied: the same bytes");
    // Let go (idle) and made again at the next use.
    crate::resource::test_let_go_host_copy(image);
    let now = crate::resource::host_resource(image).unwrap().clone();
    assert_ne!(now, first, "another host copy");
    assert!(!cached(&KEY, &view), "the entry is of the old copy");
    let after = cache.copy(&view, KEY, to).unwrap();
    assert_eq!(after, now);
    assert_eq!(
        bytes(to),
        made_bytes(&view),
        "the slot shows the new host copy"
    );
    let copy = crate::resource::host_copy(image).unwrap();
    assert_eq!(
        std::sync::Arc::strong_count(&copy),
        2,
        "held by the texture and here only"
    );
    drop((first, again, after, now, copy));
    crate::resource::test_release_texture(image);
}

/// Views the cache does not keep are left to the table's slot.
#[test]
fn only_texture_views_with_a_host_copy_are_kept() {
    let (Some(cache), Some((_heap, to))) = (cache(), one_slot()) else {
        return;
    };
    assert!(cache.copy(&View::Constant(0x1000, 256), KEY, to).is_none());
    assert!(
        cache
            .copy(&View::Unordered(0x2000, 0, None), KEY, to)
            .is_none()
    );
    assert!(cache.copy(&View::Sampler([0; 13]), KEY, to).is_none());
    assert!(
        cache
            .copy(&View::Placed(true, [0; 14], [0; 16]), KEY, to)
            .is_none()
    );
}

/// A record made again keeps its slot; past the shard's slots the oldest
/// slot goes to the new record, and its old record is forgotten.
#[test]
fn a_full_shard_gives_the_oldest_slot_to_the_next_record() {
    let mut shard = Shard::default();
    let key = |i: u32| [1, i, 0];
    let entry = |slot| Entry {
        slot,
        shows: Shows {
            title: 1,
            copy: std::ptr::null(),
            first: None,
            prefilled: 0,
        },
        _copy: Weak::new(),
    };
    for i in 0..PER_SHARD {
        let slot = shard.take(key(i));
        assert_eq!(slot, i);
        shard.entries.insert(key(i), entry(slot));
    }
    // Made again: its own slot.
    assert_eq!(shard.take(key(5)), 5);
    shard.entries.insert(key(5), entry(5));
    // A new record: slot 0, and record 0 is forgotten.
    assert_eq!(shard.take(key(PER_SHARD)), 0);
    assert!(!shard.entries.contains_key(&key(0)));
    assert!(shard.entries.contains_key(&key(1)));
}
