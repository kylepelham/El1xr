use super::*;

const KEY: [u32; 3] = [super::super::VIEW_TAG, 0x7c_0001, 77];

/// A heap the shaders do not see, of one slot, and its slot.
pub(super) fn one_slot() -> Option<(ID3D12DescriptorHeap, D3D12_CPU_DESCRIPTOR_HANDLE)> {
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
        copies: [0; 2],
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
            copies: [0; 2],
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

/// Title memory registered as a title buffer, as a buffer object's creation
/// registers it (`crate::shadow::register`), under resource number `owner`.
pub(crate) struct TitleBuffer {
    base: *mut u8,
    owner: usize,
}

/// 64 KiB: one write-watch granule and more.
const TITLE_LEN: usize = 1 << 16;

impl TitleBuffer {
    pub(crate) fn new(owner: usize) -> TitleBuffer {
        let layout = std::alloc::Layout::from_size_align(TITLE_LEN, 1 << 16).unwrap();
        // SAFETY: a non-zero size.
        let base = unsafe { std::alloc::alloc_zeroed(layout) };
        assert!(!base.is_null());
        crate::shadow::register(owner, base as u64, TITLE_LEN as u64);
        TitleBuffer { base, owner }
    }

    /// The title releases the buffer and makes another at the same address.
    pub(crate) fn made_again(&mut self, owner: usize) {
        crate::shadow::unregister(self.owner, self.base as u64);
        self.owner = owner;
        crate::shadow::register(owner, self.base as u64, TITLE_LEN as u64);
    }

    /// A raw shader view of all of it, placed in memory (no resource
    /// object), and a record naming it.
    pub(crate) fn view(&self) -> (View, [u32; 3]) {
        let len = TITLE_LEN as u64;
        let address = self.base as u64;
        let mut resource = [0u32; 14];
        resource[0] = 1;
        (resource[4], resource[5]) = (len as u32, (len >> 32) as u32);
        let mut desc = [0u32; 16];
        // R32_TYPELESS, dimension buffer, default mapping; elements; raw.
        desc[..3].copy_from_slice(&[39, 1, D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING]);
        desc[6] = (len / 4) as u32;
        desc[8] = D3D12_BUFFER_SRV_FLAG_RAW.0 as u32;
        (desc[10], desc[11]) = (address as u32, (address >> 32) as u32);
        let record = [super::super::VIEW_TAG, address as u32, 2 << 30 | (address >> 32) as u32];
        (View::Placed(false, resource, desc), record)
    }
}

impl Drop for TitleBuffer {
    fn drop(&mut self) {
        crate::shadow::unregister(self.owner, self.base as u64);
        let layout = std::alloc::Layout::from_size_align(TITLE_LEN, 1 << 16).unwrap();
        // SAFETY: allocated in `new` with this layout. Copies made of it read
        // it no more once unregistered (none is asked for again).
        unsafe { std::alloc::dealloc(self.base, layout) };
    }
}

/// Whether `key`'s entry is the one made on the host copies `view` reads now.
fn buffer_cached(key: &[u32; 3], view: &View) -> bool {
    let (address, len, desc, buffer_view) = super::super::buffer_srv_args(view).unwrap();
    let copies = super::super::buffer_srv_copies(address, len, &desc, buffer_view).unwrap();
    let shard = cache().unwrap().shards[shard_of(key)].lock().unwrap();
    shard.found(key, &Shows::buffer(&copies)).is_some()
}

/// A buffer view copied from the cache holds the bytes `make` writes for it
/// and the same host buffer, made and copied. The title buffer released and
/// another made at the same address (same record, new host copy with a new
/// number): the entry made on the old copy is never copied again; the slot
/// shows the new copy, whose descriptor differs from the old one's.
#[test]
fn a_buffer_released_and_made_again_never_gets_its_old_descriptor() {
    let (Some(cache), Some((_heap, to))) = (cache(), one_slot()) else {
        return; // no host GPU here
    };
    let mut title = TitleBuffer::new(0x7e57_b001);
    let (view, key) = title.view();
    let first = cache.copy_buffer(&view, key, to).expect("a buffer view is kept");
    assert_eq!(bytes(to), made_bytes(&view));
    assert!(buffer_cached(&key, &view));
    let again = cache.copy_buffer(&view, key, to).unwrap();
    assert_eq!(again.as_raw(), first.as_raw(), "copied: the same host buffer");
    assert_eq!(bytes(to), made_bytes(&view), "copied: the same bytes");
    let old = bytes(to);
    title.made_again(0x7e57_b002);
    assert!(!buffer_cached(&key, &view), "the entry is of the old copy");
    let after = cache.copy_buffer(&view, key, to).unwrap();
    assert_ne!(after.as_raw(), first.as_raw(), "another host copy");
    assert_eq!(bytes(to), made_bytes(&view), "the slot shows the new copy");
    assert_ne!(bytes(to), old, "not the descriptor of the buffer let go");
    assert!(buffer_cached(&key, &view));
    drop((first, again, after));
}

/// Only buffer shader views take the buffer path: texture views, constant
/// buffers, unordered access and empty buffers do not.
#[test]
fn only_buffer_shader_views_are_kept_as_buffers() {
    use super::super::buffer_srv_args;
    let title = TitleBuffer::new(0x7e57_b003);
    let (view, _) = title.view();
    assert!(buffer_srv_args(&view).is_some());
    let View::Placed(_, mut resource, desc) = view.clone() else {
        unreachable!()
    };
    assert!(buffer_srv_args(&View::Placed(true, resource, desc)).is_none());
    resource[4] = 0;
    assert!(buffer_srv_args(&View::Placed(false, resource, desc)).is_none());
    assert!(buffer_srv_args(&View::Constant(0x1000, 256)).is_none());
    assert!(buffer_srv_args(&View::Unordered(0x2000, 0, None)).is_none());
    let image = crate::resource::test_lazy_texture();
    assert!(buffer_srv_args(&View::Shader(image as usize, Some([28, 4, 0, 0, 0, 0, 0, 0, 0, 0]))).is_none());
    crate::resource::test_release_texture(image);
    drop(title);
}

#[test]
fn the_buffer_switch_is_live_and_off_by_default() {
    assert_eq!(BUFFERS.name(), "VICTORY_TABLE_VIEW_CACHE_BUFFERS");
    assert!(!buffers_on() || std::env::var_os("VICTORY_TABLE_VIEW_CACHE_BUFFERS").is_some());
}
