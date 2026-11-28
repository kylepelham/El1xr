//! Texture contents the title writes itself (decision 0007, "Texture
//! memory"): streamed pages, layout-library copies, movie frames, glyph
//! atlases. Such a texture lives in console memory in its tiled layout;
//! before a submission samples its host copy, the memory is detiled into a
//! staging buffer when its content changed, and the staging buffer is
//! copied into the host texture.
//!
//! "Changed" means a page of the memory was written since the last detiling
//! ([`crate::watch`]), in write-watched memory; in physical-page views,
//! which cannot be watched, a content hash of the whole memory at every
//! submission that samples it.
//!
//! shortcut: a change detiles the whole texture, not the pages written.
//! Only textures read through descriptor tables are brought up to date
//! (directly indexed views are not known per draw); plane 0 is uploaded
//! (every mip and array slice), depth and stencil planes are not.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use windows::Win32::Graphics::Direct3D12::*;

use crate::host::{HostList, host};
use crate::object::This;

pub(crate) mod alias_skip;
pub(crate) mod blind_hash;
pub(crate) mod card_mips;
mod gpu_detile;
mod hash_behind;
mod inside_copy;
mod mapped_holes;
mod partial_grown;
#[cfg(all(test, windows))]
mod inside_copy_tests;
pub(crate) mod prepare;
pub(crate) mod prepare_fenced;
#[cfg(all(test, windows))]
mod read_marks_tests;
mod recheck_mips;
mod reinterpret;
pub(crate) mod settle_reread;
mod short_remap;
mod slice_len;
mod stage_unlocked;
pub(crate) mod staged_ask;
mod stale_audit;
mod staging_flight;
pub(crate) mod stream_ahead;
pub(crate) mod take_by_table;

macro_rules! once {
    ($($arg:tt)*) => {{
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| tracing::warn!($($arg)*));
    }};
}

/// A texture's upload state: the staging buffer laid out as the host
/// texture's subresources, and what was last detiled.
pub(crate) struct Staging {
    /// The title texture (`crate::readable_once` is told when it is detiled anew).
    resource: usize,
    buffer: ID3D12Resource,
    data: *mut u8,
    footprints: Vec<D3D12_PLACED_SUBRESOURCE_FOOTPRINT>,
    rows: Vec<u32>,
    layout: victory_formats::texture::Layout,
    /// Mips per array slice.
    mips: u32,
    /// The first of them that is uploaded, and the bytes of title memory
    /// read for it and the smaller ones.
    first: u32,
    size: usize,
    /// The staging buffer's bytes (counted in `STAGED` while it lives).
    bytes: u64,
    /// The title texture's shape, for `crate::texstore`.
    shape: crate::texstore::Shape,
    memory: *const u8,
    last: Mutex<Last>,
    /// Whether the staging buffer was detiled since a list's prelude last
    /// uploaded it, and whether one has uploaded it at all: a list's prelude
    /// records `CopyTextureRegion` at submission time (after `refresh()`)
    /// only when `!uploaded || dirty`.
    dirty: std::sync::atomic::AtomicBool,
    uploaded: std::sync::atomic::AtomicBool,
    /// When a list last recorded a use of it (`crate::host::moment`).
    used: std::sync::atomic::AtomicU64,
    /// Levels on demand (`crate::tiles`): the texture's host copy was made
    /// without memory of its own when the staging buffer was made.
    on_demand: bool,
    /// For such a texture: the levels that were the title's at the last
    /// detiling (a bit a level; the staging buffer holds zeros for the
    /// others), and what that was judged by ([`Staging::judgement`]).
    detiled: std::sync::atomic::AtomicU32,
    judged: [std::sync::atomic::AtomicU64; 2],
    /// For such a texture: when an upload last left a held level without
    /// tiles (`crate::host::moment`, never 0; 0: none did). The upload is
    /// recorded again about a second later if the card has room by then
    /// ([`upload`]).
    waiting: std::sync::atomic::AtomicU64,
    /// Where in `buffer` `data` begins, and the piece of a block it is
    /// (`crate::texpool`, `VICTORY_STAGING_BLOCKS=1`; 0 and none for a
    /// buffer of its own), given back with this.
    base: u64,
    piece: Option<crate::texpool::StagingPiece>,
    /// Detiled on the card (`VICTORY_GPU_DETILE=1`, [`gpu_detile`]): the
    /// buffer then holds the title's bytes as laid out, and the prelude
    /// detiles them into this one's buffer, which its copies read.
    gpu: Option<gpu_detile::Gpu>,
    /// The submission that last recorded a copy from it
    /// (`VICTORY_STAGING_UNDER_FLIGHT`, [`staging_flight`]; 0: none seen).
    sent: std::sync::atomic::AtomicU64,
}

/// What a texture's staging buffer holds: the hash of the memory detiled
/// (memory not watched), or the write-watch asking it was detiled after.
#[derive(Default)]
struct Last {
    hash: Option<blake3::Hash>,
    asked: u64,
    /// The frame a partly mapped texture was last read in.
    partial: Option<u64>,
    /// What its partly mapped reads know of its mapping (`VICTORY_PARTIAL_GROWN`).
    grown: partial_grown::Grown,
    /// The texture was found changed without a write the guard saw
    /// ([`Staging::unseen_write`]): it is hashed at every check until this
    /// frame.
    unguarded: u64,
    /// `(hash, sample)` of the last whole read, with
    /// `VICTORY_TEXTURE_HASH_SAMPLED` ([`blind_hash::kept_sample`]).
    sample: Option<(blake3::Hash, blake3::Hash)>,
    /// Its look for an unseen write taken behind the submitting thread
    /// (`VICTORY_TEXTURE_HASH_BEHIND`, [`hash_behind`]).
    behind: hash_behind::Behind,
}

// SAFETY: the staging mapping is written only at submission, under
// `last`'s lock; the title memory is only read.
unsafe impl Send for Staging {}
// SAFETY: as above.
unsafe impl Sync for Staging {}

static STAGING: Mutex<Option<HashMap<usize, Arc<Staging>>>> = Mutex::new(None);

/// The buffer a texture the host writes is copied through into the texture
/// of a view the title placed in its memory in another format
/// ([`alias`]): every level of every layer as each of the two formats lays
/// it out for a copy (the same bytes: the texels are as wide), the written
/// texture's layout first. No buffer when the formats copy texture to
/// texture (`reinterpret::direct`).
struct Through {
    buffer: Option<ID3D12Resource>,
    parts: Vec<(
        D3D12_PLACED_SUBRESOURCE_FOOTPRINT,
        D3D12_PLACED_SUBRESOURCE_FOOTPRINT,
    )>,
    /// For a view placed inside the written texture (`inside_copy`): the
    /// written texture's subresource for each part; `None`: the part's own.
    from_parts: Option<Arc<[u32]>>,
}

impl Through {
    /// The written texture's subresource that part `index` is copied from.
    fn from(&self, index: usize) -> usize {
        self.from_parts.as_ref().map_or(index, |p| p[index] as usize)
    }

    /// Whether part `index` has nothing to copy from
    /// (`crate::resource::alias_compatible::NONE`).
    fn skips(&self, index: usize) -> bool {
        self.from_parts
            .as_ref()
            .is_some_and(|p| p[index] == crate::resource::alias_compatible::NONE)
    }
}

/// The most levels times layers [`alias`] copies (a cube of nine levels).
const ALIAS_PARTS: u32 = 54;

/// `VICTORY_ALIAS_AFTER_UPLOAD=1`: an alias copy is due the first time in a
/// list from each source (off: only at the view's first use, as before).
fn alias_after_upload() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_ALIAS_AFTER_UPLOAD").is_some_and(|v| v == "1"))
}

/// The list's key for "the copy from `source` into the view `placed` was
/// recorded": the top bit set, which no address of ours has, so it never
/// stands for a texture of the list's.
fn alias_key(placed: usize, source: usize) -> usize {
    (placed ^ source.rotate_left(29)) | 1 << (usize::BITS - 1)
}

/// Whether [`alias`] records its copy for the view `placed` in `list` (the
/// first time it asks; a later use of the source in another way shows as a
/// change of state). Marks the view used. A CPU upload of the view earlier
/// in the list (`upload`) also marks it used, and the copy of what the card
/// wrote into its memory was then left out: the view showed the title
/// memory's bytes, zeros where the card had drawn. Since
/// `VICTORY_ALIAS_AFTER_UPLOAD`, the copy is due the first time in the list
/// from each source. (TaynanGT/Vice-Tay PR #141)
fn alias_copy_due(list: &HostList, placed: usize, source: usize) -> bool {
    alias_copy_due_as(list, placed, source, alias_after_upload())
}

fn alias_copy_due_as(list: &HostList, placed: usize, source: usize, after_upload: bool) -> bool {
    let first = list.first_use(placed);
    if !after_upload {
        return first;
    }
    list.first_use(alias_key(placed, source))
}

// SAFETY: a device-local buffer only lists record copies with.
unsafe impl Send for Through {}
// SAFETY: as above.
unsafe impl Sync for Through {}

/// The copy buffers of [`alias`], by the placed view's texture.
static THROUGH: Mutex<Option<HashMap<usize, Arc<Through>>>> = Mutex::new(None);

/// Brings `placed`, the texture of a view the title placed in the memory
/// of `source` in another format (`crate::resource::alias_of`), up to date
/// with what the host wrote to `source`: the texels of every level and
/// layer are copied as they are, through a buffer (the host copies between
/// two formats of one texel width in no other way). Recorded the first time
/// the list reads the view
/// and again whenever the list used `source` in another way since (wrote
/// it, or read it itself). Leaves `placed` in the copy destination state,
/// as [`upload`] does. False if the copy cannot be made: the caller fills
/// the texture from title memory, as before.
pub(crate) fn alias(list: &HostList, placed: This, source: This) -> bool {
    const COPY_DEST: u32 = 0x400;
    const COPY_SOURCE: u32 = 0x800;
    let (Some(to), Some(from)) = (
        crate::resource::host_resource(placed),
        crate::resource::host_resource(source),
    ) else {
        return false;
    };
    let Some(host) = host() else { return false };
    let inside = crate::resource::placed_inside::inside_parts(placed, source)
        .or_else(|| crate::resource::alias_compatible::copy_parts(placed, source));
    let through = if let Some(parts) = &inside {
        let Some(through) = inside_copy::through(host, placed, source, from, to, parts) else {
            return false;
        };
        through
    } else {
        let mut map = THROUGH.lock().unwrap_or_else(|e| e.into_inner());
        let map = map.get_or_insert_with(HashMap::new);
        match map.get(&(placed as usize)) {
            Some(through) => through.clone(),
            None => {
                // SAFETY: live textures.
                let (of_from, of_to) = unsafe { (from.GetDesc(), to.GetDesc()) };
                // Levels times layers (a volume's slices are one part).
                let count = |d: &D3D12_RESOURCE_DESC| {
                    let layers = if d.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE3D {
                        1
                    } else {
                        u32::from(d.DepthOrArraySize)
                    };
                    u32::from(d.MipLevels) * layers
                };
                let parts = count(&of_from);
                if parts == 0 || parts > ALIAS_PARTS || parts != count(&of_to) {
                    return false;
                }
                let mut rows = [(); 2]
                    .map(|()| vec![D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); parts as usize]);
                let mut bytes = [0u64; 2];
                // SAFETY: plain queries; the out pointers are ours and
                // hold `parts` layouts each.
                unsafe {
                    host.device.GetCopyableFootprints(
                        &of_from,
                        0,
                        parts,
                        0,
                        Some(rows[0].as_mut_ptr()),
                        None,
                        None,
                        Some(&mut bytes[0]),
                    );
                    host.device.GetCopyableFootprints(
                        &of_to,
                        0,
                        parts,
                        0,
                        Some(rows[1].as_mut_ptr()),
                        None,
                        None,
                        Some(&mut bytes[1]),
                    );
                }
                let alike = rows[0].iter().zip(&rows[1]).all(|(a, b)| {
                    let (fa, fb) = (a.Footprint, b.Footprint);
                    a.Offset == b.Offset
                        && (fa.Width, fa.Height, fa.Depth, fa.RowPitch)
                            == (fb.Width, fb.Height, fb.Depth, fb.RowPitch)
                });
                if bytes[0] == 0 || bytes[0] != bytes[1] || !alike {
                    return false;
                }
                let direct = reinterpret::on() && reinterpret::direct(of_from.Format, of_to.Format);
                let buffer = if direct {
                    None
                } else {
                    let Some(buffer) = host.uav_buffer(bytes[0]) else {
                        return false;
                    };
                    crate::host::name(&buffer, || {
                        format!(
                            "alias copy {}x{} f{} to f{}",
                            of_from.Width, of_from.Height, of_from.Format.0, of_to.Format.0
                        )
                    });
                    Some(buffer)
                };
                tracing::info!(
                    view = ?placed,
                    texture = ?source,
                    width = of_from.Width,
                    height = of_from.Height,
                    parts,
                    written_as = of_from.Format.0,
                    read_as = of_to.Format.0,
                    direct = buffer.is_none(),
                    "a view placed in the memory of a texture the host writes, in another format: read from that texture by a copy on the host"
                );
                let [of_written, of_read] = rows;
                let through = Arc::new(Through {
                    buffer,
                    parts: of_written.into_iter().zip(of_read).collect(),
                    from_parts: None,
                });
                map.insert(placed as usize, through.clone());
                through
            }
        }
    };
    let first = alias_copy_due(list, placed as usize, source as usize);
    // The source's first use in the list records nothing (its prelude
    // brings it to the state); a use in another way since the last copy
    // shows as a change of state.
    let before = list
        .transition(source, COPY_SOURCE)
        .map(|(texture, before)| {
            let state = crate::host::legal_state(texture, COPY_SOURCE);
            let barrier = crate::host::transition(
                texture,
                D3D12_RESOURCE_STATES(before as i32),
                D3D12_RESOURCE_STATES(state as i32),
            );
            // SAFETY: an open list and a live texture.
            unsafe { list.list.ResourceBarrier(&[barrier]) };
            before
        });
    if !first && before.is_none() {
        return true;
    }
    card_writes(list, placed, to, None);
    crate::list::ensure_state(list, placed, COPY_DEST);
    let predicate = alias_skip::on()
        .then(|| crate::resource::host_copy(placed).zip(crate::resource::host_copy(source)))
        .flatten()
        .and_then(|(p, s)| {
            let always = crate::resource::host_tiled(placed).is_some()
                || crate::resource::host_tiled(source).is_some();
            alias_skip::predicate(list, &p, &s, always)
        });
    let part = |texture: &ID3D12Resource, index: usize| D3D12_TEXTURE_COPY_LOCATION {
        // SAFETY: a borrowed pointer the location never releases.
        pResource: unsafe { std::mem::transmute_copy(texture) },
        Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
            SubresourceIndex: index as u32,
        },
    };
    #[cfg(windows)]
    crate::gpucost::mark(&list.list, || "ours: alias copy");
    #[cfg(feature = "diagnostics")]
    list.note_op(|| format!("alias copy of {source:?} into {placed:?}"));
    let Some(buffer) = &through.buffer else {
        alias_skip::under(&list.list, predicate.as_ref(), || match &through.from_parts {
            Some(parts) => inside_copy::record_direct(&list.list, from, to, parts),
            None => reinterpret::record(&list.list, from, to, through.parts.len()),
        });
        return restore(list, source, before);
    };
    let rows = |row: &D3D12_PLACED_SUBRESOURCE_FOOTPRINT| D3D12_TEXTURE_COPY_LOCATION {
        // SAFETY: as above.
        pResource: unsafe { std::mem::transmute_copy(buffer) },
        Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
            PlacedFootprint: *row,
        },
    };
    let (common, read, written) = (
        D3D12_RESOURCE_STATE_COMMON,
        D3D12_RESOURCE_STATE_COPY_SOURCE,
        D3D12_RESOURCE_STATE_COPY_DEST,
    );
    // SAFETY: an open list and live resources; the buffer is as big as
    // the footprints and rests in the common state between copies.
    alias_skip::under(&list.list, predicate.as_ref(), || unsafe {
        list.list
            .ResourceBarrier(&[crate::host::transition(buffer, common, written)]);
        for (index, (of_written, _)) in through.parts.iter().enumerate() {
            if through.skips(index) {
                continue;
            }
            list.list
                .CopyTextureRegion(&rows(of_written), 0, 0, 0, &part(from, through.from(index)), None);
        }
        list.list
            .ResourceBarrier(&[crate::host::transition(buffer, written, read)]);
        for (index, (_, of_read)) in through.parts.iter().enumerate() {
            if through.skips(index) {
                continue;
            }
            list.list
                .CopyTextureRegion(&part(to, index), 0, 0, 0, &rows(of_read), None);
        }
        list.list
            .ResourceBarrier(&[crate::host::transition(buffer, read, common)]);
    });
    list.keep(buffer);
    if alias_pins() {
        list.keep(from);
        list.keep(to);
    }
    restore(list, source, before)
}

/// `VICTORY_ALIAS_PINS=1`: an alias copy keeps its source and placed host
/// textures alive until the list is reset, as it keeps the buffer between
/// them (off: the buffer only). A texture the title releases while the
/// list waits to run was freed under the copy: random texels, or a removed
/// device. (Belobug/VIctory-Belobug-AMD PR #559)
pub(crate) fn alias_pins() -> bool {
    #[cfg(test)]
    if let Some(on) = TEST_ALIAS_PINS.get() {
        return on;
    }
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_ALIAS_PINS").is_some_and(|v| v == "1"))
}

#[cfg(test)]
thread_local! {
    pub(crate) static TEST_ALIAS_PINS: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

/// A draw that also writes the source (a view of it handled before an
/// alias copy) needs it in that state again; the next read of the alias
/// then copies once more, as it must after a write.
fn restore(list: &HostList, source: This, before: Option<u32>) -> bool {
    const TARGET_OR_UNORDERED: u32 = 0x4 | 0x8;
    if let Some(before) = before
        && before & TARGET_OR_UNORDERED != 0
    {
        crate::list::ensure_state(list, source, before);
    }
    true
}

/// Whether a texture refresh that finds the runtime remapping title memory
/// goes on without waiting for it (`VICTORY_TEXTURE_NO_WAIT=1`; off by
/// default).
///
/// A texture whose memory is all mapped is read in place, under a lock
/// the runtime takes alone while it changes a mapping
/// (`crate::watch::title_memory_reading`), so that no page goes away under
/// the read. While the world streams in the runtime remaps all the time,
/// and each of the frame's 150-360 refreshes waited for it: runs 99-101
/// have 4, 15 and 16 frame lines with over 300 ms of "textures", up to
/// 3,708 ms, and on every one of them the wait for this lock is the whole
/// of it (1,345 of 1,381 ms; 3,630 of 3,708; the write-watch askings 3-12
/// ms, the texture's own lock 0).
///
/// With the switch the refresh asks for the lock without waiting. If the
/// runtime has it, the texture is read through the kernel instead, which
/// needs no lock: it fails if a page is not there, and then the texture is
/// left as it is and read at its next refresh, a frame later. A read
/// through the kernel costs about 0.6 ms a megabyte against 0.3 in place.
///
/// What changes for the picture: a texture that arrives while the runtime
/// is remapping may show a frame later. It never shows partly read.
fn no_wait() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_TEXTURE_NO_WAIT").is_some_and(|v| v == "1"))
}

/// Since the last frame-rate line, with [`no_wait`]: refreshes that read
/// through the kernel because title memory was being remapped, and those
/// left for the next refresh because a page was not there.
pub(crate) static UNLOCKED: [std::sync::atomic::AtomicU64; 2] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 2];

/// Since the last frame-rate line, of texture refreshes on every thread, in
/// microseconds: waiting for the texture's own lock (another thread is
/// refreshing the same texture), asking the write watch and the guards,
/// waiting for title memory to hold still (the lock the runtime takes
/// alone while it remaps, `crate::watch::title_memory_reading`), and the
/// refreshes in all.
///
/// While the world streams in, the frame line's "textures" is seconds a
/// frame in some runs and tenths in others for the same megabytes (2,566
/// ms for 110 MB in run 93, 314 ms for 132 MB in run 91; before these
/// counters, runs 81, 83, 85 and 94 each had lines of 1.5-2.9 s), and the
/// submitting thread is not running while it lasts (run 93: 240-310 ms of
/// its own time a frame beside 1,100-1,800 ms of "textures"): it waits.
/// These say for what.
pub(crate) static REFRESH_WAITS: [std::sync::atomic::AtomicU64; 4] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 4];

/// `f`'s result, its time added to [`REFRESH_WAITS`]' place `slot`.
fn waited<T>(slot: usize, f: impl FnOnce() -> T) -> T {
    let started = std::time::Instant::now();
    let out = f();
    REFRESH_WAITS[slot].fetch_add(
        started.elapsed().as_micros() as u64,
        std::sync::atomic::Ordering::Relaxed,
    );
    out
}

thread_local! {
    /// Room to read a texture's memory into: one buffer per submitting
    /// thread. Each texture kept its own, a second copy in memory of every
    /// write-watched or partly mapped texture ever sampled, for the run.
    static READ: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Calls `f` with `size` bytes of this thread's read buffer (their
/// contents are whatever the last texture left).
fn with_read<R>(size: usize, f: impl FnOnce(&mut [u8]) -> R) -> R {
    READ.with(|read| {
        let mut read = read.borrow_mut();
        if read.len() < size {
            read.resize(size, 0);
        }
        f(&mut read[..size])
    })
}

/// Whether a texture all of whose memory is mapped is read through the
/// kernel's copy instead of in place (`VICTORY_TEXTURE_COPY=1`; off by
/// default). Round 3 of the memory work read every such texture that way
/// to be safe from pages taken away under the read; it cost 0.6 ms per MB
/// against 0.3 in place (90-105 ms a frame while the world streams in, runs
/// 72 and 73, against 20-40), and both runs with it lost the graphics
/// device, for a reason not found. The read in place is safe now for
/// another reason (`crate::watch::title_memory_reading`); this switch is
/// left to tell whether the copy was what lost the device.
fn through_copy() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_TEXTURE_COPY").is_some_and(|v| v == "1"))
}

/// Calls `f` with the `size` bytes of title memory at `memory`, read
/// through the kernel into this thread's read buffer; pages that are not
/// there read as zeros. The bytes are those a read in place would see.
fn with_title<R>(memory: *const u8, size: usize, f: impl FnOnce(&[u8]) -> R) -> R {
    with_read(size, |read| {
        crate::host::copy_mapped(read.as_mut_ptr(), memory, size, false);
        f(read)
    })
}

/// Staging buffers alive (also those only a list still holds) and their
/// bytes.
static STAGED: [std::sync::atomic::AtomicU64; 2] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 2];

impl Drop for Staging {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering::Relaxed;
        STAGED[0].fetch_sub(1, Relaxed);
        STAGED[1].fetch_sub(self.bytes, Relaxed);
        if self.piece.is_some() {
            return;
        }
        // (Neptune) Its upload buffer is kept for the next staging buffer of its size class.
        // The last holder is gone: the texture's entry, and every list that recorded an
        // upload from it (a list is reset when the card has run it).
        let most = stage_pool_most();
        if most != 0 && !crate::commitline::upload_over_cap() {
            let (class, size) = stage_class(self.bytes);
            let mut pool = STAGE_POOL.lock().unwrap_or_else(|e| e.into_inner());
            if pool.bytes + size <= most {
                if pool.free.len() <= class {
                    pool.free.resize_with(class + 1, Vec::new);
                }
                pool.free[class].push((self.buffer.clone(), self.data as usize));
                pool.bytes += size;
            }
        }
    }
}

/// (Neptune) The upload buffers of staging buffers that are gone, by size class, and their
/// bytes: every texture streamed in made an upload buffer of its own and let it go with its
/// staging buffer (123,000 of them, 7.3 GB, in the first four minutes of a run), each a
/// call into the kernel that waits for the memory to be there; in a load the thread that
/// records the frame stood a seventh of its time in them.
struct StagePool {
    free: Vec<Vec<(ID3D12Resource, usize)>>,
    bytes: u64,
}

// SAFETY: upload buffers of the device and where they are mapped; taken and given under the
// lock, by whichever thread makes or drops a staging buffer.
unsafe impl Send for StagePool {}

static STAGE_POOL: Mutex<StagePool> = Mutex::new(StagePool {
    free: Vec::new(),
    bytes: 0,
});

/// (El1xr) Bytes the staging buffers' pool keeps, for the commit line.
pub(crate) fn stage_pool_bytes() -> u64 {
    STAGE_POOL.lock().unwrap_or_else(|e| e.into_inner()).bytes
}

/// (El1xr) Lets go of every upload buffer the staging buffers' pool keeps
/// (`VICTORY_UPLOAD_CAP_MB`), outside its lock: how many.
pub(crate) fn trim_stage_pool() -> u64 {
    let free = {
        let mut pool = STAGE_POOL.lock().unwrap_or_else(|e| e.into_inner());
        pool.bytes = 0;
        std::mem::take(&mut pool.free)
    };
    free.iter().map(|class| class.len() as u64).sum()
}

/// Upload buffers taken from the pool, and made, since the start.
pub(crate) static STAGE_POOLED: [std::sync::atomic::AtomicU64; 2] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 2];

/// The most bytes the pool keeps (`VICTORY_STAGE_POOL_MB`, 256; 0: no pool, a buffer of its
/// own for every staging buffer, as before).
fn stage_pool_most() -> u64 {
    static MOST: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *MOST.get_or_init(|| {
        std::env::var("VICTORY_STAGE_POOL_MB")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(256)
            << 20
    })
}

/// The size class of a staging buffer of `bytes` bytes and the bytes of its upload buffer: the
/// next power of two from 64 KiB.
fn stage_class(bytes: u64) -> (usize, u64) {
    let size = bytes.max(64 << 10).next_power_of_two();
    ((size.trailing_zeros() - 16) as usize, size)
}

/// An upload buffer for a staging buffer of `bytes` bytes, mapped: one of the pool's of its
/// class, or a new one of the class's size.
fn stage_buffer(host: &crate::host::Host, bytes: u64) -> Option<(ID3D12Resource, *mut u8)> {
    use std::sync::atomic::Ordering::Relaxed;
    if stage_pool_most() == 0 {
        return host.upload_buffer(bytes);
    }
    let (class, size) = stage_class(bytes);
    let pooled = {
        let mut pool = STAGE_POOL.lock().unwrap_or_else(|e| e.into_inner());
        let pooled = pool.free.get_mut(class).and_then(Vec::pop);
        if pooled.is_some() {
            pool.bytes -= size;
        }
        pooled
    };
    match pooled {
        Some((buffer, data)) => {
            STAGE_POOLED[0].fetch_add(1, Relaxed);
            // Zeros, as a buffer just made holds: a list can copy from a staging buffer
            // before anything was detiled into it (the texture's memory not there at that
            // moment, or its read left for the next refresh). From a new buffer that was a
            // black texture for some frames; from one of the pool's it was the texture the
            // buffer held before, as coloured noise on a character's arms or face.
            // SAFETY: the buffer's mapping, `size` bytes, which nothing else names now.
            unsafe { std::ptr::write_bytes(data as *mut u8, 0, size as usize) };
            Some((buffer, data as *mut u8))
        }
        None => {
            STAGE_POOLED[1].fetch_add(1, Relaxed);
            let started = std::time::Instant::now();
            let made = host.upload_buffer(size);
            let us = started.elapsed().as_micros() as u64;
            STAGED_HERE.with(|s| {
                let mut now = s.get();
                now[2] += 1;
                now[3] += us;
                s.set(now);
            });
            made
        }
    }
}

thread_local! {
    /// On this thread: staging buffers made, the microseconds that took
    /// (their upload buffers among them), and of those upload buffers the
    /// ones made new (not from the pool) and their microseconds.
    static STAGED_HERE: std::cell::Cell<[u64; 4]> = const { std::cell::Cell::new([0; 4]) };
}

/// (Neptune) What [`STAGED_HERE`] holds for the calling thread: the pass
/// over textures picked by index tells by these how much of its time went
/// into making staging buffers.
pub(crate) fn staged_here() -> [u64; 4] {
    STAGED_HERE.with(std::cell::Cell::get)
}

/// For the memory line: staging buffers alive, their bytes, and how many
/// of them their texture still has (the others wait for a list's reset).
pub(crate) fn memory() -> [u64; 3] {
    use std::sync::atomic::Ordering::Relaxed;
    let kept = STAGING
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map_or(0, |map| map.len() as u64);
    [STAGED[0].load(Relaxed), STAGED[1].load(Relaxed), kept]
}

/// For the resident line: the mapped staging buffers, as (address, length) each.
pub(crate) fn cpu_ranges() -> Vec<(usize, usize)> {
    let map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
    map.iter()
        .flat_map(|m| m.values())
        .filter(|s| !s.data.is_null())
        .map(|s| (s.data as usize, s.bytes as usize))
        .collect()
}

/// Staging buffers let go as idle since the start, and their bytes.
static LET_GO: [std::sync::atomic::AtomicU64; 2] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 2];

/// For the memory line: staging buffers let go as idle, and their bytes.
#[cfg(feature = "diagnostics")]
pub(crate) fn let_go() -> [u64; 2] {
    use std::sync::atomic::Ordering::Relaxed;
    [LET_GO[0].load(Relaxed), LET_GO[1].load(Relaxed)]
}

/// Lets go of the staging buffers no list recorded a use of for `IDLE_MS`
/// and `IDLE_FRAMES` frames and no list holds: a staging buffer is system memory the size of its
/// texture, kept so that an unchanged texture is not uploaded again, and
/// those of textures no longer sampled were kept for the run (2,246 of
/// them, 2.1 GB, at the end of run 24). The next list that samples such a
/// texture makes a staging buffer anew, detiles and uploads once more.
/// Called before a submission starts, by the thread that submits.
pub(crate) fn let_go_idle() {
    use std::sync::atomic::Ordering::Relaxed;
    const IDLE_FRAMES: u64 = 4;
    stale_audit::tick();
    let mut map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
    let Some(map) = map.as_mut() else {
        return;
    };
    if map.is_empty() {
        return;
    }
    let idle_ms = crate::host::ram_idle_ms(crate::host::available_physical_ram());
    // (El1xr) VICTORY_UPLOAD_CAP_MB / VICTORY_COMMIT_FLOOR_MB: past either, 2 s.
    let idle_ms = crate::commitline::staging_idle_ms(idle_ms);
    // (El1xr) VICTORY_STAGING_HOLD_MS while VICTORY_POOL_SLACK_MB is pressed.
    let (hold_ms, hold_frames) = crate::staging_hold::idle(idle_ms, IDLE_FRAMES);
    // A list that recorded an upload or a check holds the staging until it
    // is reset: one only the table holds is named by no list, recorded or
    // running.
    let (mut went, mut bytes) = (0u64, 0u64);
    map.retain(|&resource, s| {
        let used = s.used.load(Relaxed);
        let keep = Arc::strong_count(s) > 1 || !crate::host::long_ago(used, hold_ms, hold_frames);
        if !keep {
            if !crate::host::long_ago(used, idle_ms, IDLE_FRAMES) {
                crate::staging_hold::let_go(resource, s.bytes);
            }
            // (A live texture: one the title releases leaves the
            // table first, under this lock.)
            crate::resource::set_staged_first(resource as This, u32::MAX);
            crate::table_null::unstaged(resource, crate::table_null::Why::StagingIdle);
            LET_GO[0].fetch_add(1, Relaxed);
            LET_GO[1].fetch_add(s.bytes, Relaxed);
            went += 1;
            bytes += s.bytes;
        }
        keep
    });
    if went > 0 {
        crate::flicker_catch::periodic::ran(crate::flicker_catch::periodic::Task::StagingGone);
        crate::host::trail(|| {
            format!(
                "sweep: {went} staging buffers of textures let go ({} KiB), held by no list",
                bytes >> 10
            )
        });
    }
}

/// Textures the host GPU wrote by a copy or resolve: their contents are on
/// the host, not in title memory, which stays as it was.
static HOST_WRITTEN: Mutex<Option<std::collections::HashSet<usize>>> = Mutex::new(None);

/// A host copy or resolve writes `resource`.
pub(crate) fn host_written(resource: This) {
    host_written_sub(resource, None);
}

/// [`host_written`] of subresource `sub` of `resource` (`None`: all of them,
/// or one the caller cannot name): [`card_mips`].
pub(crate) fn host_written_sub(resource: This, sub: Option<u32>) {
    card_mips::wrote(resource as usize, sub);
    let new = {
        let mut set = HOST_WRITTEN.lock().unwrap_or_else(|e| e.into_inner());
        set.get_or_insert_with(Default::default)
            .insert(resource as usize)
    };
    crate::resource::mark_host_written(resource);
    // Diagnostics (run 177): where its title memory is, for placed views
    // the title reads that memory through ([`placed_in_host_written`]).
    if new && let Some((at, desc)) = crate::resource::texture_memory(resource) {
        // SAFETY: 56 bytes in the description's layout.
        let words = unsafe {
            std::ptr::from_ref(&desc)
                .cast::<[u32; 14]>()
                .read_unaligned()
        };
        let bytes = crate::resource::placed_bytes(&words).unwrap_or(0);
        if !at.is_null() && bytes > 0 {
            if crate::bake_reuse::on() {
                crate::bake_reuse::card_wrote(at as u64, bytes);
            }
            HOST_WRITTEN_AT
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(
                    at as u64,
                    (
                        bytes,
                        words[4],
                        words[6],
                        words[7],
                        words[8],
                        resource as usize,
                    ),
                );
        }
    }
    if new && crate::bake_watch::on() {
        // SAFETY: 56 bytes in the description's layout, as above.
        let words = crate::resource::texture_memory(resource)
            .map(|(_, desc)| unsafe { std::ptr::from_ref(&desc).cast::<[u32; 14]>().read_unaligned() });
        let [width, height, format] = words.map_or([0; 3], |w| [w[4], w[6], w[8]]);
        crate::bake_watch::marked(resource as usize, width, height, format);
    }
}

/// (`VICTORY_BAKE_WATCH`) The texture the host wrote, other than `except`,
/// whose memory holds part of `[address, address + len)`: its address
/// and resource.
pub(crate) fn in_host_written(address: u64, len: u64, except: usize) -> Option<(u64, usize)> {
    let map = HOST_WRITTEN_AT.lock().unwrap_or_else(|e| e.into_inner());
    let end = address.saturating_add(len);
    let before = map.range(..=address).next_back();
    let inside = map.range(address..end);
    before
        .into_iter()
        .chain(inside)
        .find(|(at, d)| **at < end && address < **at + d.0 && d.5 != except)
        .map(|(at, d)| (*at, d.5))
}

/// The title memory of textures the host copied into, by address: bytes
/// and description words 4, 6, 7 and 8 (width, height, layers and levels,
/// format).
static HOST_WRITTEN_AT: Mutex<std::collections::BTreeMap<u64, (u64, u32, u32, u32, u32, usize)>> =
    Mutex::new(std::collections::BTreeMap::new());

/// A texture the title placed at `address` (description words `desc`) was
/// made: says so once if it lies in the memory of a texture the host
/// copied into ([`host_written`]). Its own host copy is filled from title
/// memory, which the copy never reached: the copied texels are on the
/// card in the other texture, where no shader that reads this view finds
/// them.
pub(crate) fn placed_in_host_written(address: u64, desc: &[u32; 14]) {
    if crate::bake_reuse::on() {
        crate::bake_reuse::placed(address, desc);
    }
    let hit = {
        let map = HOST_WRITTEN_AT.lock().unwrap_or_else(|e| e.into_inner());
        map.range(..=address)
            .next_back()
            .filter(|(at, d)| address < **at + d.0)
            .map(|(at, d)| (*at, *d))
    };
    if let Some((at, (bytes, width, height, layers_levels, format, _))) = hit {
        static SAID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        if SAID.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 200 {
            tracing::warn!(
                view_address = format_args!("{address:#x}"),
                view_offset = address - at,
                view_width = desc[4],
                view_height = desc[6],
                view_layers_levels = format_args!("{:#x}", desc[7]),
                view_format = desc[8],
                copied_address = format_args!("{at:#x}"),
                copied_bytes = bytes,
                copied_width = width,
                copied_height = height,
                copied_layers_levels = format_args!("{layers_levels:#x}"),
                copied_format = format,
                "HERO LEAD: a placed texture view lies in the memory of a texture the host copied into: it is filled from title memory, which the copy never reached"
            );
        }
    }
}

/// Whether the host GPU wrote `resource` by a copy or resolve
/// ([`host_written`]): its host copy cannot be made again from title
/// memory.
pub(crate) fn host_wrote(resource: usize) -> bool {
    HOST_WRITTEN
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .is_some_and(|set| set.contains(&resource))
}

/// The title released a resource.
pub(crate) fn forget(resource: usize) {
    let mut map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(map) = map.as_mut() {
        map.remove(&resource);
    }
    // (Neptune) And what a prepare thread made ready for it: its memory goes.
    prepare::forget(resource);
    crate::table_null::forget(resource);
    crate::staging_hold::forget(resource);
    crate::idle_evict::forget(resource);
    card_mips::forget(resource);
    let mut set = HOST_WRITTEN.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(set) = set.as_mut() {
        set.remove(&resource);
    }
    // Its memory may be given to other textures now (run 177: most of the
    // "HERO LEAD" lines were views in memory a released texture had held).
    HOST_WRITTEN_AT
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|at, d| {
            // `VICTORY_BAKE_REUSE_WATCH`: the memory of a released card-written texture.
            if d.5 == resource && crate::bake_reuse::on() {
                crate::bake_reuse::released(*at, d.0, [d.1, d.2, d.3, d.4], resource);
            }
            d.5 != resource
        });
    let mut through = THROUGH.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(through) = through.as_mut() {
        through.remove(&resource);
    }
    crate::target_reuse::forget(resource);
    crate::face_watch::result_gone(resource);
}

/// Whether `resource`'s host copy takes its contents from title memory:
/// a texture the host GPU does not render to or write.
pub(crate) fn title_written(texture: &ID3D12Resource) -> bool {
    // SAFETY: a live resource.
    title_written_flags(unsafe { texture.GetDesc() }.Flags.0 as u32)
}

/// [`title_written`] of a texture with resource flags `flags`.
pub(crate) fn title_written_flags(flags: u32) -> bool {
    const TARGET_DEPTH_UNORDERED: u32 = 0x7;
    flags & TARGET_DEPTH_UNORDERED == 0
}

/// Registers, once per list, `resource`'s staging buffer so that at submission
/// time its memory is refreshed and, if changed (or never uploaded), copied
/// into its host texture on the list's prelude command list.
pub(crate) fn upload(list: &HostList, resource: This, texture: &ID3D12Resource) {
    upload_from(list, resource, texture, 0);
}

/// [`upload`] of the mips from `first` on, the ones a view shows: of a
/// placed texture the title may have no memory for the larger ones, and
/// what lies there is another texture's.
pub(crate) fn upload_from(list: &HostList, resource: This, texture: &ID3D12Resource, first: u32) {
    // (Both asked of the texture itself, not of the shared tables: every
    // read of every texture by every recording thread asked them.)
    let host_written = crate::resource::host_written_mark(resource);
    if host_written && crate::descriptor::host::swap_watch::on() {
        crate::descriptor::host::swap_watch::host_kept(resource, texture);
    }
    // `VICTORY_CARD_MIPS_TRACK=1` ([`card_mips`]): of a texture the card
    // wrote some subresources of, the others are the title's.
    let card_owned = if host_written && card_mips::on() {
        card_mips::partly(resource as usize, subresources(texture))
    } else {
        None
    };
    // shortcut: a texture the host copied into keeps the host's contents
    // even if the title later writes its memory too.
    if host_written && card_owned.is_none()
        || !list.first_use(resource as usize) && crate::resource::staged_first(resource) <= first
    {
        return;
    }
    alias_skip::uploaded(list, resource);
    let Some(copy) = crate::resource::host_copy(resource) else {
        return;
    };
    let Some(staging) = staging(resource, texture, first) else {
        return;
    };
    use std::sync::atomic::Ordering::Relaxed;
    // `VICTORY_TARGET_REUSE_REREAD` (`crate::target_reuse`): memory a target held is read
    // again from title memory.
    let (at, len) = staging.memory();
    if crate::target_reuse::reread(resource as usize, at as u64, len as u64) {
        staging.forget();
        staging.dirty.store(true, Relaxed);
    }
    staging.used.store(crate::host::moment(), Relaxed);
    // Levels on demand: a view of the texture now starts at another
    // level, or another texture now starts inside its span, with no byte
    // of its memory changed. The levels are judged anew: detiled again at
    // this list's submission, and uploaded by this list's prelude.
    if staging.on_demand && staging.judgement() != staging.judged() {
        staging.forget();
        staging.dirty.store(true, Relaxed);
    }
    // Levels on demand: the last upload left a held level without tiles
    // (the card had no room, or no heap could be had), and nothing else
    // would record an upload again until the title writes the texture: the
    // level stayed empty, the texture blurred or black. Asked again about
    // once a second, and only when the card has room; the staging buffer
    // still holds the level's bytes.
    if staging.on_demand {
        let since = staging.waiting.load(Relaxed);
        if since != 0 && crate::host::long_ago(since, 1000, 2) {
            if crate::resource::room_lately() {
                staging.waiting.store(0, Relaxed);
                staging.dirty.store(true, Relaxed);
                crate::tiles::STATS[10].fetch_add(1, Relaxed);
                crate::flicker_catch::periodic::ran(crate::flicker_catch::periodic::Task::LevelsAsked);
            } else {
                staging.waiting.store(crate::host::moment().max(1), Relaxed);
            }
        }
    }
    // At submission time, `staging.refresh()` runs first (in parallel via
    // `Batch::refresh_front` or `Batch::refresh_textures`), and only then does
    // `PreludeUpload::finish_refresh` decide whether the list's prelude must
    // record `CopyTextureRegion`. This guarantees zero frames of latency when
    // title memory changes (including 3-buffer Bink video rings) and eliminates
    // the race with `VICTORY_JOIN_PARTS` / `VICTORY_THIN`.
    let on_demand = if staging.on_demand {
        crate::resource::host_tiled(resource)
    } else {
        None
    };
    list.defer_texture(PreludeUpload {
        staging,
        copy,
        resource: resource as usize,
        on_demand,
        card_owned,
    });
}

/// `texture`'s subresources, planes left out.
fn subresources(texture: &ID3D12Resource) -> u32 {
    // SAFETY: a live resource.
    let d = unsafe { texture.GetDesc() };
    let layers = if d.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE3D {
        1
    } else {
        u32::from(d.DepthOrArraySize)
    };
    u32::from(d.MipLevels) * layers
}

/// Pure claim helper for [`PreludeUpload::finish_refresh`]: returns
/// `Some(first_upload)` when the host texture has never been uploaded or the
/// staging buffer was detiled since the last upload, and `None` when the host
/// texture already holds the staging buffer's current contents.
fn claim_upload(
    uploaded: &std::sync::atomic::AtomicBool,
    dirty: &std::sync::atomic::AtomicBool,
) -> Option<bool> {
    use std::sync::atomic::Ordering::AcqRel;
    let was_uploaded = uploaded.swap(true, AcqRel);
    let was_dirty = dirty.swap(false, AcqRel);
    (!was_uploaded || was_dirty).then_some(!was_uploaded)
}

/// A title texture deferred to submission time so `staging.refresh()` runs
/// before deciding whether `HostList::prelude` must record `CopyTextureRegion`.
#[derive(Clone)]
pub(crate) struct PreludeUpload {
    pub(crate) staging: Arc<Staging>,
    pub(crate) copy: Arc<crate::resource::HostTexture>,
    /// The title's texture, for the check at submission that the host has
    /// not written it since the list was recorded ([`host_wrote`]).
    pub(crate) resource: usize,
    on_demand: Option<crate::resource::OnDemand>,
    /// (`VICTORY_CARD_MIPS_TRACK`) The subresources the card had written
    /// when the read was recorded: left out of the copies ([`card_mips`]).
    card_owned: Option<Arc<[u64]>>,
}

/// `VICTORY_PRELUDE_RECHECK=1`: a prelude upload is checked again at
/// submission for a host write made after its list was recorded (off: not
/// checked, as before).
fn prelude_recheck() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_PRELUDE_RECHECK").is_some_and(|v| v == "1"))
}

impl PreludeUpload {
    /// The title memory the texture is read from: address and bytes.
    #[allow(dead_code)]
    pub(crate) fn memory(&self) -> (usize, usize) {
        self.staging.memory()
    }

    /// Called on the submitting thread after `self.staging.refresh()` has
    /// completed for this submission. Returns `Some(first_upload)` when the
    /// list's prelude must record `CopyTextureRegion` into `self.copy.texture`.
    pub(crate) fn finish_refresh(&self) -> Option<bool> {
        use std::sync::atomic::Ordering::Relaxed;
        // The host wrote the texture (a bake's copy into it) after this list
        // was recorded, on another recording thread: its title memory holds
        // what it held before, often another texture's bytes, and copying it
        // over the host's contents left them there for good (no later list
        // uploads a host-written texture): a character's baked skin drawn as
        // coloured specks or a blue face for a whole scene, in about half the
        // runs (the diner's bartender). (trustno1-coder/VIctory-NVIDIA #33)
        if prelude_recheck()
            && host_wrote(self.resource)
            && !self
                .card_owned
                .as_ref()
                .is_some_and(|seen| card_mips::same(self.resource, seen))
        {
            static SKIPPED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let n = SKIPPED.fetch_add(1, Relaxed) + 1;
            if n <= 8 || n.is_power_of_two() {
                // SAFETY: a live texture.
                let desc = unsafe { self.copy.texture.GetDesc() };
                tracing::warn!(
                    n,
                    resource = format_args!("{:#x}", self.resource),
                    width = desc.Width,
                    height = desc.Height,
                    format = desc.Format.0,
                    "prelude upload left out: the host wrote the texture after the list that samples it was recorded"
                );
            }
            crate::bake_watch::add(crate::bake_watch::C::PreludeSkipped, 1);
            return None;
        }
        let Some(first_upload) = claim_upload(&self.staging.uploaded, &self.staging.dirty) else {
            #[cfg(feature = "diagnostics")]
            crate::copystat::add(crate::copystat::C::TexSkipped, 1);
            return None;
        };
        // (texflicker) The host copy holds the title's texture from here on.
        self.copy
            .uploaded_from
            .fetch_min(self.staging.first, std::sync::atomic::Ordering::AcqRel);
        if crate::bake_watch::on() {
            let (at, len) = self.staging.memory();
            if let Some((baked_at, baked)) = in_host_written(at as u64, len as u64, self.resource) {
                crate::bake_watch::alias_upload(self.resource, at, baked, baked_at);
            }
        }
        // `VICTORY_BAKE_CHECK`: title memory uploaded over a bake result, or over its memory.
        if crate::bake_check::on() {
            crate::bake_check::written(0, self.resource, "title_upload", 0);
            let (at, len) = self.staging.memory();
            if let Some((_, baked)) = in_host_written(at as u64, len as u64, self.resource) {
                crate::bake_check::written(0, baked, "alias_upload", self.resource);
            }
        }
        // `VICTORY_BAKE_REUSE_WATCH`: title memory over a released bake result's texels.
        if crate::bake_reuse::on() {
            let (at, len) = self.staging.memory();
            crate::bake_reuse::title_upload(at as u64, len as u64, self.resource);
        }
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::TexUploads, 1);
        // Levels on demand (`crate::tiles`): after `refresh()` has detiled,
        // give the held levels their memory on the queue before the prelude
        // copies into them.
        if let Some(on_demand) = &self.on_demand {
            let waiting = on_demand.uploaded(
                self.staging.detiled.load(Relaxed),
                self.staging.memory as u64,
            );
            self.staging.waiting.store(
                if waiting {
                    crate::host::moment().max(1)
                } else {
                    0
                },
                Relaxed,
            );
            // A texture picked by index has no staging buffer after this
            // upload: its slots are made due again by the texture pass.
            if waiting {
                crate::descriptor::host::levels_waiting(self.staging.memory as u64);
            }
        }
        staging_flight::copied(&self.staging.sent);
        Some(first_upload)
    }

    /// Records `CopyTextureRegion` onto `cmd_list` (the prelude command list).
    /// `self.copy.texture` must already be in `D3D12_RESOURCE_STATE_COPY_DEST`.
    pub(crate) fn record(&self, host_list: &HostList, cmd_list: &ID3D12GraphicsCommandList) {
        crate::flicker_catch::count(crate::flicker_catch::Kind::Uploads);
        // A host copy made whole from the store (`crate::texstore`): its larger
        // levels, once, before the ones from title memory.
        let prefill = self
            .copy
            .prefill
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some((levels, bytes)) = prefill {
            match prefill_levels(host_list, cmd_list, &self.copy, levels, &bytes) {
                Prefilled::Done => crate::descriptor::host::levels_changed(self.staging.memory as u64),
                Prefilled::NoRoom if prefill_retry() => {
                    prefill_back(&self.copy.prefill, &self.staging.dirty, (levels, bytes));
                }
                Prefilled::NoRoom | Prefilled::Never => {}
            }
        }
        let texture = &self.copy.texture;
        // On the card's way: detiled here first, into the buffer the copies read.
        let gpu = self.staging.gpu.as_ref();
        if let Some(gpu) = gpu {
            gpu_detile::record(cmd_list, &self.staging.buffer, self.staging.base, gpu);
        }
        let (source, base) = match gpu {
            Some(gpu) => (gpu_detile::copy_source(gpu), 0),
            None => (&self.staging.buffer, self.staging.base),
        };
        // The staging buffer lives as long as `self.staging`, which the list's
        // deferred steps hold until the list is reset after GPU completion.
        let mut skipped = 0;
        for (i, footprint) in self.staging.footprints.iter().enumerate() {
            let sub = self.staging.subresource(i);
            if self.card_owned.as_deref().is_some_and(|o| card_mips::owns(o, sub)) {
                skipped += 1;
                continue;
            }
            let to = D3D12_TEXTURE_COPY_LOCATION {
                // SAFETY: a borrowed pointer the location never releases.
                pResource: unsafe { std::mem::transmute_copy(texture) },
                Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    SubresourceIndex: sub,
                },
            };
            let from = D3D12_TEXTURE_COPY_LOCATION {
                // SAFETY: as above.
                pResource: unsafe { std::mem::transmute_copy(source) },
                Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                        Offset: base + footprint.Offset,
                        ..*footprint
                    },
                },
            };
            // SAFETY: an open prelude list; both locations name live resources.
            unsafe { cmd_list.CopyTextureRegion(&to, 0, 0, 0, &from, None) };
        }
        if let Some(gpu) = gpu {
            gpu_detile::copied(cmd_list, gpu);
        }
        if let Some(owned) = &self.card_owned {
            let parts = self.staging.footprints.len() as u64;
            card_mips::uploaded(self.resource, owned, parts - skipped, skipped, || {
                // SAFETY: a live texture.
                let d = unsafe { texture.GetDesc() };
                format!(
                    "{}x{} f{} mips {} layers {} first {}",
                    d.Width, d.Height, d.Format.0, d.MipLevels, d.DepthOrArraySize, self.staging.first
                )
            });
        }
    }
}

/// What [`prefill_levels`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Prefilled {
    /// The copies are recorded.
    Done,
    /// The levels do not fit the texture: they never will.
    Never,
    /// No upload memory could be had for them this time.
    NoRoom,
}

/// `VICTORY_PREFILL_RETRY=1`: cached levels a prelude had no upload memory
/// for go back for the next list (off: dropped, as before).
fn prefill_retry() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_PREFILL_RETRY").is_some_and(|v| v == "1"))
}

/// Cached levels a prelude took but could not record go back to the host
/// copy, and the texture is made due again: the next list that samples it
/// uploads them. Taken and dropped, the texture kept no larger levels than
/// title memory holds for the rest of the run. (TaynanGT/Vice-Tay
/// c92d5cfbb0, adapted: our prelude already records them at submission.)
fn prefill_back(
    slot: &std::sync::Mutex<Option<(u32, Arc<Vec<u8>>)>>,
    dirty: &std::sync::atomic::AtomicBool,
    taken: (u32, Arc<Vec<u8>>),
) {
    let mut slot = slot.lock().unwrap_or_else(|e| e.into_inner());
    if slot.is_none() {
        *slot = Some(taken);
    }
    dirty.store(true, std::sync::atomic::Ordering::Release);
}

/// Records copies of levels `0..levels` of `copy` (in the copy destination
/// state) from `bytes`, laid out as the host lays out those subresources;
/// its views show them from then on.
fn prefill_levels(
    host_list: &HostList,
    cmd_list: &ID3D12GraphicsCommandList,
    copy: &crate::resource::HostTexture,
    levels: u32,
    bytes: &[u8],
) -> Prefilled {
    use std::sync::atomic::Ordering::Relaxed;
    let Some(host) = host() else {
        return Prefilled::Never;
    };
    let texture = &copy.texture;
    // SAFETY: a live resource.
    let d = unsafe { texture.GetDesc() };
    if levels == 0 || levels > u32::from(d.MipLevels) || d.DepthOrArraySize != 1 {
        return Prefilled::Never;
    }
    let mut footprints = vec![D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); levels as usize];
    let mut total = 0;
    // SAFETY: outputs for `levels` subresources.
    unsafe {
        host.device.GetCopyableFootprints(
            &d,
            0,
            levels,
            0,
            Some(footprints.as_mut_ptr()),
            None,
            None,
            Some(&mut total),
        );
    }
    if total as usize > bytes.len() {
        return Prefilled::Never;
    }
    let Some((buffer, data)) = host.upload_buffer(total) else {
        return Prefilled::NoRoom;
    };
    // SAFETY: the mapping holds `total` bytes.
    unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), data, total as usize) };
    host_list.keep(&buffer);
    for (i, footprint) in footprints.iter().enumerate() {
        let to = D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: a borrowed pointer the location never releases.
            pResource: unsafe { std::mem::transmute_copy(texture) },
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                SubresourceIndex: i as u32,
            },
        };
        let from = D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: as above.
            pResource: unsafe { std::mem::transmute_copy(&buffer) },
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                PlacedFootprint: *footprint,
            },
        };
        // SAFETY: an open prelude list; both locations name live resources.
        unsafe { cmd_list.CopyTextureRegion(&to, 0, 0, 0, &from, None) };
    }
    copy.prefilled.store(levels, Relaxed);
    crate::texstore::PREFILLED.fetch_add(1, Relaxed);
    Prefilled::Done
}

/// Levels on demand: the levels a list claimed for its zeros
/// (`crate::tiles::Tiled::claim`), held by the list's submission-time
/// step. A list that is reset without having been submitted gives the
/// claim up, or the next list that writes those levels would send no zeros
/// and what its write does not cover would be what the tiles' last owner
/// left.
struct Claimed {
    on_demand: crate::resource::OnDemand,
    fresh: u32,
    ran: std::sync::atomic::AtomicBool,
}

impl Drop for Claimed {
    fn drop(&mut self) {
        if self.fresh != 0
            && !*self.ran.get_mut()
            && let Some(tiled) = self.on_demand.tiled()
        {
            tiled.unclaim(self.fresh);
        }
    }
}

/// Records copies of zeros into every subresource of `texture`, which is in
/// the copy destination state.
pub(crate) fn zeros(list: &HostList, host: &crate::host::Host, texture: &ID3D12Resource) {
    crate::flicker_catch::count(crate::flicker_catch::Kind::Zeroed);
    // SAFETY: a live resource.
    let d = unsafe { texture.GetDesc() };
    let layers = if d.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE3D {
        1
    } else {
        u32::from(d.DepthOrArraySize)
    };
    let count = u32::from(d.MipLevels) * layers;
    let mut footprints = vec![D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); count as usize];
    let mut bytes = 0u64;
    // SAFETY: outputs for `count` subresources.
    unsafe {
        host.device.GetCopyableFootprints(
            &d,
            0,
            count,
            0,
            Some(footprints.as_mut_ptr()),
            None,
            None,
            Some(&mut bytes),
        );
    }
    if bytes == 0 || bytes == u64::MAX {
        return;
    }
    let Some((buffer, data)) = list.upload(host, bytes) else {
        return;
    };
    // SAFETY: a mapped buffer of `bytes` bytes, just made.
    unsafe { std::ptr::write_bytes(data, 0, bytes as usize) };
    for (i, footprint) in footprints.iter().enumerate() {
        let to = D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: a borrowed pointer the location never releases.
            pResource: unsafe { std::mem::transmute_copy(texture) },
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                SubresourceIndex: i as u32,
            },
        };
        let from = D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: as above; the list holds the buffer until it is reset.
            pResource: unsafe { std::mem::transmute_copy(&buffer) },
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                PlacedFootprint: *footprint,
            },
        };
        // SAFETY: an open list; both locations name live resources.
        unsafe { list.list.CopyTextureRegion(&to, 0, 0, 0, &from, None) };
    }
}

/// Levels on demand (`crate::tiles`): the card itself is about to write
/// title texture `resource` in `list` (a copy or a resolve into level
/// `level`, or into every level). If its host copy `texture` was made
/// without memory, the levels written get theirs when the list is
/// submitted, ahead of it on the queue; the ones that have none yet are
/// first written with zeros, here in the same list, so that what the
/// card's write does not cover is not what the tiles' last owner left.
/// To be called before the list is brought to the state of that write.
/// shortcut: two lists that write the same new level, submitted in the
/// other order than recorded, lose the first one's write under the zeros
/// of the other; so does an upload recorded and not yet submitted.
pub(crate) fn card_writes(
    list: &HostList,
    resource: This,
    texture: &ID3D12Resource,
    level: Option<u32>,
) {
    let Some(on_demand) = crate::resource::host_tiled(resource) else {
        return;
    };
    let Some(tiled) = on_demand.tiled() else {
        return;
    };
    let levels = match level {
        Some(level) => tiled.with_level(level),
        None => tiled.every_level(),
    };
    if levels == 0 {
        return;
    }
    let fresh = tiled.claim(levels);
    if fresh != 0 {
        crate::bake_watch::add(crate::bake_watch::C::ZerosOverBaked, u64::from(host_wrote(resource as usize)));
        if crate::bake_check::on() {
            crate::bake_check::written(0, resource as usize, "zeros", 0);
        }
    }
    // Of them, the ones whose zeros could not be recorded.
    let mut unzeroed = if host().is_none() { fresh } else { 0 };
    if fresh != 0
        && let Some(host) = host()
    {
        const COPY_DEST: u32 = 0x400;
        crate::list::ensure_state(list, resource, COPY_DEST);
        // SAFETY: a live resource.
        let d = unsafe { texture.GetDesc() };
        for level in
            (0..u32::from(d.MipLevels).min(32)).filter(|&level| fresh & (1u32 << level) != 0)
        {
            let mut footprint = D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default();
            let mut bytes = 0u64;
            // SAFETY: outputs for one subresource.
            unsafe {
                host.device.GetCopyableFootprints(
                    &d,
                    level,
                    1,
                    0,
                    Some(&mut footprint),
                    None,
                    None,
                    Some(&mut bytes),
                );
            }
            let Some((buffer, data)) = list.upload(host, bytes) else {
                // (With the levels that share its tiles, if it is one of
                // the small ones.)
                unzeroed |= tiled.with_level(level);
                continue;
            };
            // SAFETY: a mapped buffer of `bytes` bytes, just made.
            unsafe { std::ptr::write_bytes(data, 0, bytes as usize) };
            let to = D3D12_TEXTURE_COPY_LOCATION {
                // SAFETY: a borrowed pointer the location never releases.
                pResource: unsafe { std::mem::transmute_copy(texture) },
                Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    SubresourceIndex: level,
                },
            };
            let from = D3D12_TEXTURE_COPY_LOCATION {
                // SAFETY: as above; the list holds the buffer until it is reset.
                pResource: unsafe { std::mem::transmute_copy(&buffer) },
                Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    PlacedFootprint: footprint,
                },
            };
            // SAFETY: an open list; both locations name live resources.
            unsafe { list.list.CopyTextureRegion(&to, 0, 0, 0, &from, None) };
        }
    }
    // A new level without its zeros gets no tiles from this list: the
    // card's write into it is lost (the level reads zeros, as before the
    // write), which is less wrong than a part write over what the tiles'
    // last owner left. The next list that writes it claims it again.
    unzeroed &= fresh;
    if unzeroed != 0 {
        crate::bake_watch::levels(crate::bake_watch::C::LostNoUpload, resource as usize, unzeroed);
    }
    let (unclaim, levels) = unzeroed_levels(levels, unzeroed, card_write_keep());
    if unclaim != 0 {
        tiled.unclaim(unclaim);
    }
    if levels == 0 {
        return;
    }
    let memory = crate::resource::texture_memory(resource).map_or(0, |(memory, _)| memory as u64);
    let step = Claimed {
        on_demand,
        fresh: fresh & !unclaim,
        ran: std::sync::atomic::AtomicBool::new(false),
    };
    let title = resource as usize;
    list.defer_host(move || {
        step.ran.store(true, std::sync::atomic::Ordering::Relaxed);
        step.on_demand.card_wrote(levels, memory);
        if crate::bake_watch::on()
            && let Some(tiled) = step.on_demand.tiled()
        {
            let lost = tiled.without(levels);
            if lost != 0 {
                crate::bake_watch::levels(crate::bake_watch::C::LostNoTiles, title, lost);
            }
        }
    });
}

/// `VICTORY_CARD_WRITE_KEEP=1`: a card write (a bake's copy, a resolve)
/// into a new level on demand whose zeros could not be recorded (no
/// upload memory) still gets the level's tiles, and the write lands; off,
/// the level is left without tiles and the write is lost (it reads zeros:
/// a baked skin drawn black).
fn card_write_keep() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_CARD_WRITE_KEEP").is_some_and(|v| v == "1"))
}

/// Of the `levels` a card write names, with `unzeroed` the new ones whose
/// zeros could not be recorded: the levels to give back unclaimed, and
/// the levels the write gets tiles for ([`card_write_keep`]).
fn unzeroed_levels(levels: u32, unzeroed: u32, keep: bool) -> (u32, u32) {
    if keep { (0, levels) } else { (unzeroed, levels & !unzeroed) }
}

#[cfg(test)]
mod card_write_keep_tests {
    use super::unzeroed_levels;

    #[test]
    fn a_kept_write_keeps_the_levels_whose_zeros_failed() {
        assert_eq!(unzeroed_levels(0b111, 0b010, false), (0b010, 0b101));
        assert_eq!(unzeroed_levels(0b111, 0b010, true), (0, 0b111));
        assert_eq!(unzeroed_levels(0b111, 0, false), (0, 0b111));
    }
}

impl Staging {
    /// Levels on demand: what says, beside the mapping, which levels of
    /// the texture are the title's now. First the lowest level a live view
    /// of the texture shows (0 without a view that says): the title's own
    /// word, a texture it holds from level 3 on is viewed from level 3.
    /// Then where the next texture placed in this one's span starts, in
    /// bytes from this one's start (`u64::MAX`: none does): for a texture
    /// viewed from level 0 although a neighbour sits where its large
    /// levels would be.
    fn judgement(&self) -> [u64; 2] {
        let start = self.memory as u64;
        let first = crate::descriptor::host::first_viewed(start).map_or(0, u64::from);
        let limit =
            crate::descriptor::host::next_start(start, start.saturating_add(self.layout.size))
                .map_or(u64::MAX, |at| at - start);
        [first, limit]
    }

    /// What [`Staging::judgement`] said at the last detiling.
    fn judged(&self) -> [u64; 2] {
        use std::sync::atomic::Ordering::Relaxed;
        [self.judged[0].load(Relaxed), self.judged[1].load(Relaxed)]
    }

    /// Levels on demand: the levels that are the title's now, a bit a
    /// level. A level is held if no view starts above it, its bytes end
    /// before the next texture starts, and all of them are mapped. The
    /// smallest levels share the texture's first block and are held
    /// together. (The console layout: that block first, then the other
    /// levels from the smallest up, level 0 last:
    /// `victory_formats::texture::Layout`.) A texture this cannot tell
    /// the levels of holds them all.
    fn held_levels(&self, [first, limit]: [u64; 2]) -> u32 {
        let mips = &self.layout.mips;
        if self.layout.slices != 1 || mips.is_empty() || mips.len() > 32 {
            return u32::MAX;
        }
        let tail = (self.layout.first_tail_mip as usize).min(mips.len());
        let there = |from: u64, to: u64| {
            to > from
                && to <= limit
                && committed(
                    self.memory.wrapping_add(from as usize),
                    (to - from) as usize,
                )
        };
        let mut held = 0u32;
        for (level, m) in mips.iter().enumerate().take(tail) {
            let end = if level == 0 {
                self.layout.slice_size
            } else {
                mips[level - 1].offset
            };
            if level as u64 >= first && there(m.offset, end) {
                held |= 1 << level;
            }
        }
        if tail < mips.len() && there(0, self.layout.alignment.min(self.layout.slice_size)) {
            for level in tail..mips.len() {
                if level as u64 >= first {
                    held |= 1 << level;
                }
            }
        }
        held
    }

    /// Forgets what the staging buffer was last filled from: the next
    /// refresh detiles anew, whether the memory changed or not.
    fn forget(&self) {
        *self.last.lock().unwrap_or_else(|e| e.into_inner()) = Last::default();
    }

    /// The title memory the texture is read from: address and bytes.
    pub(crate) fn memory(&self) -> (usize, usize) {
        (self.memory as usize, self.size)
    }

    /// (Neptune) Levels on demand: the bytes at the start of the texture's memory that hold
    /// the levels that are the title's now (the smallest levels first, level 0 last). What
    /// lies behind them is not mapped, or is another texture's.
    fn held_bytes(&self) -> usize {
        let held = self.held_levels(self.judgement());
        if held == u32::MAX {
            return self.size;
        }
        if held == 0 {
            return (self.layout.alignment.min(self.layout.slice_size) as usize).min(self.size);
        }
        // The lowest bit is the largest level held.
        (self.layout.bytes_from(held.trailing_zeros()) as usize).min(self.size)
    }

    /// The mip and the array slice of the `i`th part of the staging buffer.
    fn part(&self, i: usize) -> (u32, u32) {
        let mips = self.mips - self.first;
        (self.first + i as u32 % mips, i as u32 / mips)
    }

    /// The texture's subresource the `i`th part of the staging buffer is.
    fn subresource(&self, i: usize) -> u32 {
        let (mip, slice) = self.part(i);
        slice * self.mips + mip
    }

    /// Detiles the texture memory into the staging buffer if it changed
    /// since the last time (and is mapped at all).
    pub(crate) fn refresh(self: &Arc<Self>) {
        use std::sync::atomic::Ordering::Relaxed;
        let started = std::time::Instant::now();
        let read = self.refresh_now();
        let preparing = &crate::host::PREPARING;
        preparing[2].fetch_add(started.elapsed().as_micros() as u64, Relaxed);
        preparing[3].fetch_add(read as u64, Relaxed);
    }

    /// Returns the bytes of title memory read.
    fn refresh_now(self: &Arc<Self>) -> usize {
        let whole = std::time::Instant::now();
        let mut blind = blind_hash::Read::Seen;
        let mut behind = None;
        let read = self.refresh_timed(&mut blind, &mut behind);
        if let Some(expect) = behind {
            let staging = Arc::clone(self);
            hash_behind::queue(move || staging.look_behind(expect));
        }
        let us = whole.elapsed().as_micros() as u64;
        REFRESH_WAITS[3].fetch_add(us, std::sync::atomic::Ordering::Relaxed);
        blind_hash::note_read(blind, read, us);
        read
    }

    /// The look for an unseen write handed to its threads by a check
    /// ([`hash_behind`]): the memory hashed in place, as the check would have
    /// under the same lock, and the result left for the next check.
    fn look_behind(&self, expect: blake3::Hash) {
        let size = self.size;
        let seen = {
            let mapped = if no_wait() {
                crate::watch::title_memory_reading_now(self.memory as usize, size)
            } else {
                Some(crate::watch::title_memory_reading_range(self.memory as usize, size))
            };
            match mapped {
                Some(_mapped) if committed(self.memory, size) => {
                    // SAFETY: committed memory of the texture's layout size,
                    // and it stays committed while `_mapped` is held.
                    let tiled = unsafe { std::slice::from_raw_parts(self.memory, size) };
                    hash_behind::hash_against(tiled, expect)
                }
                _ => hash_behind::Seen::Unread,
            }
        };
        let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
        let last = &mut *last;
        hash_behind::landed(&mut last.behind, last.hash, expect, seen);
    }

    /// `blind` says whether the texture was read although no write to it was
    /// seen, and how ([`blind_hash`]); `behind`, the hash a look handed to
    /// its threads checks against ([`hash_behind`]).
    fn refresh_timed(
        &self,
        blind: &mut blind_hash::Read,
        behind: &mut Option<blake3::Hash>,
    ) -> usize {
        let size = self.size;
        let start = self.memory as usize;
        let mut last = waited(0, || self.last.lock().unwrap_or_else(|e| e.into_inner()));
        let last = &mut *last;
        let asked_before = last.asked;
        if let Some((now, written)) = waited(1, || crate::watch::writes(start, start + size, false))
        {
            if last.asked != 0 && written.iter().all(|&(_, w)| w <= last.asked) {
                return 0;
            }
            last.asked = now;
            // Read through the copy, which stands unmapped pages: memory of
            // big pools is too slow to ask page by page.
            with_read(size, |read| {
                crate::host::copy_mapped(read.as_mut_ptr(), self.memory, size, false);
                self.detile(read);
            });
            #[cfg(feature = "diagnostics")]
            crate::copystat::add(crate::copystat::C::TexWatched, size as u64);
            return size;
        }
        // View memory has no write-watch, and a texture there was hashed
        // whole at every check: 229 MB and 51 ms a frame in a gameplay view
        // where two textures change (run 58). It is guarded now, as buffers
        // in view memory are: a write of the title's to it faults once and
        // is noted, a mapping is announced by the runtime. While neither
        // happened the texture is as it was. What the guard cannot see (a
        // write to the same pages through another view) is still found by
        // the hash, which is taken every eighth frame only, by the
        // texture's address, so the textures take turns.
        // `VICTORY_TEXTURE_GUARD=0` hashes at every check, as before.
        static GUARD: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        // A texture that changed without the guard seeing it is hashed at
        // every check for a while (the intro's movie: three pictures in turn
        // that only changed every eighth frame, one old picture after
        // another on the screen).
        let mut unseen = false;
        // `VICTORY_TEXTURE_HASH_SAMPLED`: a guarded texture's sample is taken with its
        // whole reads, and a look for an unseen write that is not a whole one reads the
        // sample alone.
        let mut sampling = false;
        let mut sampled_look = false;
        if *GUARD.get_or_init(|| std::env::var_os("VICTORY_TEXTURE_GUARD").is_none_or(|v| v != "0"))
            && let Some((now, written)) =
                waited(1, || crate::watch::writes(start, start + size, true))
        {
            // (Neptune) `VICTORY_TEXTURE_HASH_EVERY=<frames>` (8): how often a guarded texture is
            // hashed all the same. The hash is for a write the guards did not see; they see
            // writes through every view of a page now, and the hashing was 5 ms of every
            // frame of the first room.
            let hash_every = blind_hash::hash_every();
            let unwritten = last.asked != 0 && written.iter().all(|&(_, w)| w <= last.asked);
            last.asked = now;
            let frame = crate::frame::current();
            let turn = frame
                .wrapping_add((start >> 16) as u64)
                .is_multiple_of(hash_every);
            // Linear pictures of one level are what the processor draws (the
            // planes of a movie), through a view of its own.
            let desc = &self.layout.desc;
            let film = desc.swizzle.0 == 0 && desc.mips == 1;
            // Without `VICTORY_TEXTURE_HASH_BEHIND` (and no look of it under
            // way) skips exactly when unwritten, not the turn, not a film
            // and past its window.
            let can_behind = blind_hash::full_every().is_none() && !through_copy();
            match hash_behind::look(
                &mut last.behind,
                hash_behind::on(),
                unwritten,
                turn,
                film || frame < last.unguarded,
                can_behind,
                last.hash,
            ) {
                hash_behind::Look::Skip => return 0,
                hash_behind::Look::Behind(expect) => {
                    *behind = Some(expect);
                    return 0;
                }
                hash_behind::Look::Read => {}
            }
            unseen = unwritten;
            if unwritten {
                *blind = blind_hash::Read::Blind;
            }
            if let Some(full) = blind_hash::full_every() {
                sampling = true;
                sampled_look = unwritten
                    && !film
                    && frame >= last.unguarded
                    && !blind_hash::full_turn(frame, start, full);
            }
        }
        // From here on the texture's memory may be read in place. Held until
        // that read is done: the runtime changes what title memory shows
        // only with this taken alone, so what `committed` finds mapped
        // stays mapped (see `crate::watch::title_memory_reading`; without
        // it a page went between the asking and the read in runs 71 and
        // 74).
        let mapped = if no_wait() {
            // Not waited for: while the runtime remaps title memory the
            // texture is read through the kernel instead, whole or not at
            // all (`no_wait`).
            let Some(mapped) = crate::watch::title_memory_reading_now(self.memory as usize, size)
            else {
                return self.refresh_unlocked(last, size);
            };
            mapped
        } else {
            // Only the texture's own addresses: remaps elsewhere go on.
            waited(2, || {
                crate::watch::title_memory_reading_range(self.memory as usize, size)
            })
        };
        if !committed(self.memory, size) {
            // Partly mapped: a streamed texture with only some mips' pages
            // there, the usual state while it streams in. Read through the
            // copy, which stands unmapped pages (zeros), and upload what
            // is there; nothing at all mapped changes nothing.
            // shortcut: read again at most every 30 frames (reading around
            // unmapped pages is slow, and nothing tells us when pages
            // arrive): a mip that streams in shows up to 30 frames late.
            // `VICTORY_PARTIAL_GROWN=1`: read again soon when more of it is mapped.
            let frame = crate::frame::current();
            let grown = partial_grown::on();
            let memory = self.memory;
            if partial_grown::holds(last.partial, frame, grown, &mut last.grown, || {
                mapped_bytes(memory, size)
            }) {
                if last.grown.waiting() {
                    last.asked = asked_before;
                }
                return 0;
            }
            if grown {
                let early = last.partial.is_some_and(|at| frame < at + partial_grown::WINDOW);
                partial_grown::read(&mut last.grown, mapped_bytes(memory, size), early);
            }
            last.partial = Some(frame);
            #[cfg(feature = "diagnostics")]
            crate::copystat::add(crate::copystat::C::TexHashed, size as u64);
            let changed = short_remap::with_title_snapshot(self.memory, size, mapped, short_remap::on(), |read| {
                let hash = blake3::hash(read);
                if last.hash == Some(hash) {
                    return false;
                }
                last.hash = Some(hash);
                if read.iter().all(|&b| b == 0) {
                    return false;
                }
                self.detile(read);
                true
            });
            #[cfg(feature = "diagnostics")]
            if changed {
                crate::copystat::add(crate::copystat::C::TexChanged, size as u64);
            }
            let _ = changed;
            return size;
        }
        if through_copy() {
            return short_remap::with_title_snapshot(self.memory, size, mapped, short_remap::on(), |tiled| {
                let hash = blake3::hash(tiled);
                #[cfg(feature = "diagnostics")]
                crate::copystat::add(crate::copystat::C::TexHashed, size as u64);
                if last.hash == Some(hash) {
                    return size;
                }
                if unseen && last.hash.is_some() {
                    self.unseen_write(last);
                }
                last.hash = Some(hash);
                self.detile(tiled);
                self.keep_whole(tiled, Some(hash));
                #[cfg(feature = "diagnostics")]
                crate::copystat::add(crate::copystat::C::TexChanged, size as u64);
                size
            });
        }
        // SAFETY: committed memory of the texture's layout size, and it
        // stays committed while `mapped` is held.
        let tiled = unsafe { std::slice::from_raw_parts(self.memory, size) };
        let sample = sampling.then(|| blind_hash::sample(tiled));
        let kept = blind_hash::kept_sample(last.sample, last.hash);
        if sampled_look && sample.is_some() && sample == kept {
            *blind = blind_hash::Read::Sampled;
            return blind_hash::sampled_bytes(size);
        }
        let hash = blake3::hash(tiled);
        if let Some(sample) = sample {
            last.sample = Some((hash, sample));
        }
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::TexHashed, size as u64);
        if last.hash == Some(hash) {
            return size;
        }
        if unseen && last.hash.is_some() {
            if sample.is_some() && sample == kept {
                blind_hash::note_sample_missed();
            }
            self.unseen_write(last);
        }
        last.hash = Some(hash);
        self.detile(tiled);
        self.keep_whole(tiled, Some(hash));
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::TexChanged, size as u64);
        size
    }

    /// The texture's memory changed and the guard says nothing wrote it: it
    /// is hashed at every check for the next frames, the longer the more
    /// often this happens (a texture streamed in once is soon guarded
    /// again, a movie's is not).
    fn unseen_write(&self, last: &mut Last) {
        blind_hash::note_changed();
        let frame = crate::frame::current();
        let more = if frame < last.unguarded { 120 } else { 16 };
        #[cfg(feature = "diagnostics")]
        {
            use windows_sys::Win32::System::Memory::{MEMORY_BASIC_INFORMATION, VirtualQuery};
            static SEEN: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            if SEEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 24 {
                // SAFETY: plain data filled in by the query.
                let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
                // SAFETY: a valid out pointer of the right size.
                unsafe { VirtualQuery(self.memory.cast(), &mut info, size_of_val(&info)) };
                tracing::info!(
                    frame,
                    memory = ?self.memory,
                    size = self.layout.size,
                    protection = format_args!("{:#x}", info.Protect),
                    kind = format_args!("{:#x}", info.Type),
                    run = info.RegionSize,
                    desc = ?self.layout.desc,
                    "a guarded texture changed and no write to it was seen"
                );
            }
        }
        last.unguarded = frame + more;
    }

    /// A refresh while the runtime is remapping title memory ([`no_wait`]):
    /// the texture is read through the kernel, which fails if a page of it
    /// is not there at this moment. When the staging buffer has never been
    /// filled (`last.hash.is_none()`), falls back to [`crate::host::copy_mapped`]
    /// so a partly mapped texture still gets its mapped pages detiled instead
    /// of staying all zeros; otherwise nothing is taken and the texture is
    /// looked at again at its next refresh. Returns the bytes read.
    fn refresh_unlocked(&self, last: &mut Last, size: usize) -> usize {
        refresh_unlocked_with(self.memory, last, size, |bytes, whole, hash| {
            self.detile(bytes);
            if whole {
                self.keep_whole(bytes, Some(hash));
            }
        })
    }

    /// `tiled`, read whole, was detiled: a texture of all its levels from
    /// level 0 is given to the store (`crate::texstore`); detiling, digests
    /// and disk writes run on the store's own thread.
    fn keep_whole(&self, tiled: &[u8], hash: Option<blake3::Hash>) {
        if !crate::texstore::on()
            || self.first != 0
            || self.on_demand
            || self.layout.slices != 1
            || tiled.len() as u64 != self.layout.size
            || self.footprints.len() != self.mips as usize
        {
            return;
        }
        let parts: Vec<(u64, u32, u32)> = self
            .footprints
            .iter()
            .zip(&self.rows)
            .map(|(f, &rows)| (f.Offset, f.Footprint.RowPitch, rows * f.Footprint.Depth))
            .collect();
        crate::texstore::keep(
            self.shape,
            &self.layout,
            tiled,
            parts,
            hash.map(crate::texstore::digest_hash),
        );
    }

    /// Detiles `tiled` (the texture's memory) into the staging buffer.
    fn detile(&self, tiled: &[u8]) {
        staging_flight::detiled(self.resource, &self.sent, tiled.len());
        crate::readable_once::bump(self.resource);
        self.dirty.store(true, std::sync::atomic::Ordering::Release);
        // Levels on demand: which levels are the title's is judged here,
        // once, for what the staging buffer holds from now on. The step
        // that gives levels their memory goes by this answer.
        let held = if self.on_demand {
            use std::sync::atomic::Ordering::Relaxed;
            let judged = self.judgement();
            self.judged[0].store(judged[0], Relaxed);
            self.judged[1].store(judged[1], Relaxed);
            let held = self.held_levels(judged);
            self.detiled.store(held, Relaxed);
            held
        } else {
            u32::MAX
        };
        #[cfg(feature = "diagnostics")]
        if tiled.iter().all(|&b| b == 0) {
            tracing::debug!(
                memory = ?self.memory,
                desc = ?self.layout.desc,
                "a sampled texture's memory is all zero (not streamed yet, or a failed BCPack read)"
            );
        }
        if let Some(gpu) = &self.gpu {
            // SAFETY: the mapping holds `gpu.raw.bytes`, written only here
            // (under the staging's `last`), as the PC's way writes it.
            unsafe { gpu_detile::stage(gpu, self.data, tiled, held) };
            return;
        }
        let started = gpu_detile::on().then(std::time::Instant::now);
        for (i, (footprint, &rows)) in self.footprints.iter().zip(&self.rows).enumerate() {
            let (mip, slice) = self.part(i);
            let f = footprint.Footprint;
            let pitch = f.RowPitch as usize;
            let len = slice_len::subresource_len(
                footprint.Offset as usize,
                pitch,
                rows as usize,
                f.Depth as usize,
                self.bytes as usize,
            );
            // SAFETY: the subresource's part of the mapped staging buffer, up
            // to its end (`slice_len`).
            let linear = unsafe {
                std::slice::from_raw_parts_mut(self.data.add(footprint.Offset as usize), len)
            };
            if mip < 32 && held & (1 << mip) == 0 {
                // Not the title's: zeros, not what lies in that memory.
                linear.fill(0);
                continue;
            }
            if let Err(e) =
                self.layout
                    .detile(tiled, mip, slice, linear, pitch, pitch * rows as usize)
            {
                tracing::warn!(?e, mip, slice, "texture not detiled");
                return;
            }
        }
        if let Some(started) = started {
            gpu_detile::detiled_on_pc(started.elapsed().as_micros() as u64);
        }
    }
}

/// Lets go of a title texture's staging buffer (lists that recorded an
/// upload from it hold it until they have run); the next upload makes one
/// anew.
pub(crate) fn unstage(resource: This) {
    let mut map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(map) = map.as_mut()
        && map.remove(&(resource as usize)).is_some()
    {
        crate::resource::set_staged_first(resource, u32::MAX);
    }
}

/// Has the next list that uploads a title texture with a staging buffer
/// copy it whole, changed or not since: what the check at a submission
/// finds changed is copied by the next list that samples the texture
/// through a table, and there may be none for one that shaders now pick by
/// index alone.
pub(crate) fn upload_again(resource: This) {
    let staging = {
        let map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
        map.as_ref()
            .and_then(|map| map.get(&(resource as usize)).cloned())
    };
    if let Some(staging) = staging {
        staging
            .uploaded
            .store(false, std::sync::atomic::Ordering::Relaxed);
        staging
            .last
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .partial = None;
    }
}

/// (El1xr) For `crate::idle_evict`: whether title texture `resource`'s host copy holds its
/// title memory as it is now. `Some(true)`: its staging buffer was uploaded and not detiled
/// since, and no page of its memory was written after that detiling, by the write watch (or
/// the guard of view memory, with no unseen write found lately); `Some(false)`: written or not
/// uploaded since; `None`: it cannot tell (no staging buffer, levels on demand, memory neither
/// watched nor guarded).
pub(crate) fn unchanged_since_upload(resource: This) -> Option<bool> {
    use std::sync::atomic::Ordering::Acquire;
    let s = {
        let map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
        map.as_ref()?.get(&(resource as usize))?.clone()
    };
    if s.on_demand {
        return None;
    }
    let last = s.last.lock().unwrap_or_else(|e| e.into_inner());
    let start = s.memory as usize;
    let end = start + s.size;
    let written = crate::watch::writes(start, end, false)
        .map(|(_, w)| (w, false))
        .or_else(|| crate::watch::writes(start, end, true).map(|(_, w)| (w, true)));
    let (written, guarded) = written?;
    unchanged(
        s.uploaded.load(Acquire),
        s.dirty.load(Acquire),
        last.asked,
        guarded && crate::frame::current() < last.unguarded,
        &written,
    )
}

/// [`unchanged_since_upload`] from what the staging state says: uploaded, detiled since,
/// the asking its detiling went by (0: none), a write the guard missed lately, and the pages
/// written with the asking that found each.
fn unchanged(
    uploaded: bool,
    dirty: bool,
    asked: u64,
    unseen: bool,
    written: &[(usize, u64)],
) -> Option<bool> {
    if asked == 0 || unseen {
        return None;
    }
    Some(uploaded && !dirty && written.iter().all(|&(_, w)| w <= asked))
}

/// For the tests: `resource`'s staging buffer as detiled from its title memory now.
#[cfg(all(test, windows))]
pub(crate) fn detiled_for_test(resource: This) -> Option<Vec<u8>> {
    let s = {
        let map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
        map.as_ref()?.get(&(resource as usize))?.clone()
    };
    let mut tiled = vec![0u8; s.size];
    crate::host::copy_mapped(tiled.as_mut_ptr(), s.memory, s.size, false);
    s.detile(&tiled);
    let mut out = vec![0u8; s.bytes as usize];
    // SAFETY: the staging buffer's mapping of `bytes`, alive with `s`.
    unsafe { std::ptr::copy_nonoverlapping(s.data, out.as_mut_ptr(), out.len()) };
    Some(out)
}

#[cfg(test)]
mod unchanged_tests {
    /// Unchanged only when uploaded, not detiled since, and no page written after the asking
    /// its detiling went by; it cannot tell without an asking or after a write the guard
    /// missed.
    #[test]
    fn a_copy_is_unchanged_only_with_no_write_after_its_upload() {
        let w = [(10usize, 5u64), (11, 7)];
        assert_eq!(super::unchanged(true, false, 7, false, &w), Some(true));
        assert_eq!(super::unchanged(true, false, 6, false, &w), Some(false));
        assert_eq!(super::unchanged(true, true, 7, false, &w), Some(false));
        assert_eq!(super::unchanged(false, false, 7, false, &w), Some(false));
        assert_eq!(super::unchanged(true, false, 0, false, &w), None);
        assert_eq!(super::unchanged(true, false, 7, true, &w), None);
        assert_eq!(super::unchanged(true, false, 1, false, &[]), Some(true));
    }
}

/// Where a staged title texture's memory is: address and bytes.
pub(crate) fn staged_memory(resource: This) -> Option<(u64, u64)> {
    let s = {
        let map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
        map.as_ref()?.get(&(resource as usize))?.clone()
    };
    // (Neptune) Of a texture made level by level: the levels the title holds. The memory
    // behind them is another texture's, and its changes are not this one's.
    let len = if s.on_demand { s.held_bytes() } else { s.size };
    recheck_mips::note(s.memory as u64, len as u64, &s.layout);
    Some((s.memory as u64, len as u64))
}

/// A fingerprint of the title memory `[at, at + len)`, never 0: of all of
/// it up to 48 KiB, else of twelve pieces of 4 KiB spread evenly over it,
/// the first and the last included. Read through the kernel, so pages not
/// there count as zeros. (A texture whose change falls between the pieces
/// is missed, unless `VICTORY_HEAP_RECHECK_MIPS=1` noted the start of each
/// of its mips for this memory: [`recheck_mips`].)
pub(crate) fn fingerprint(at: u64, len: u64) -> u64 {
    match recheck_mips::noted(at, len) {
        Some(ranges) => fingerprint_ranges(at, len, &ranges),
        None => fingerprint_ranges(at, len, &[]),
    }
}

/// [`fingerprint`] that also reads `ranges` (offsets from `at`, bytes, each
/// at most 48 KiB) after the pieces.
fn fingerprint_ranges(at: u64, len: u64, ranges: &[(u64, u64)]) -> u64 {
    const PIECE: usize = 4 << 10;
    const PIECES: usize = 12;
    thread_local! {
        static READ: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
    }
    let len = len as usize;
    READ.with(|read| {
        let mut read = read.borrow_mut();
        read.resize(PIECES * PIECE, 0);
        let mut hasher = blake3::Hasher::new();
        let mut take = |from: usize, bytes: usize| {
            read[..bytes].fill(0);
            crate::host::copy_mapped(
                read.as_mut_ptr(),
                (at as usize + from) as *const u8,
                bytes,
                false,
            );
            hasher.update(&read[..bytes]);
        };
        if len <= PIECES * PIECE {
            take(0, len);
        } else {
            for n in 0..PIECES - 1 {
                take(((len - PIECE) / (PIECES - 1) * n) & !0xfff, PIECE);
            }
            take(len - PIECE, PIECE);
        }
        for &(from, bytes) in ranges {
            take(from as usize, (bytes as usize).min(PIECES * PIECE));
        }
        let hash = hasher.finalize();
        let mut first = [0u8; 8];
        first.copy_from_slice(&hash.as_bytes()[..8]);
        // 0 says "none" to the texture queue (`Recheck`, `checked`).
        u64::from_le_bytes(first) | 1
    })
}

/// Largest texture [`fingerprint_for`] reads whole when its slot is live.
const LIVE_WHOLE_BYTES: u64 = 4 << 20;

/// [`fingerprint`], or for a live slot (one found changing after its upload)
/// no larger than 4 MiB, one of all of its memory. The sampled 4 KiB pieces
/// miss a video plane's change when its sampled rows are a still background
/// (a TV news shot: the reporter moves between them), so a plane looked
/// unchanged and waited while the other planes of the same video frame were
/// uploaded: one frame's luma under another's chroma.
pub(crate) fn fingerprint_for(at: u64, len: u64, live: bool) -> u64 {
    if !live || len > LIVE_WHOLE_BYTES {
        return fingerprint(at, len);
    }
    const CHUNK: usize = 64 << 10;
    thread_local! {
        static READ: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
    }
    let len = len as usize;
    READ.with(|read| {
        let mut read = read.borrow_mut();
        read.resize(CHUNK, 0);
        let mut hasher = blake3::Hasher::new();
        let mut from = 0;
        while from < len {
            let bytes = CHUNK.min(len - from);
            read[..bytes].fill(0);
            crate::host::copy_mapped(
                read.as_mut_ptr(),
                (at as usize + from) as *const u8,
                bytes,
                false,
            );
            hasher.update(&read[..bytes]);
            from += bytes;
        }
        let hash = hasher.finalize();
        let mut first = [0u8; 8];
        first.copy_from_slice(&hash.as_bytes()[..8]);
        u64::from_le_bytes(first) | 1
    })
}

/// Whether a title texture has a staging buffer: a list recorded its
/// upload, and the lists that sample it check it at submission.
/// For the frame listing: whether a list has uploaded `resource`'s staging
/// buffer, and whether it was detiled since. None: it has none.
#[cfg(feature = "diagnostics")]
pub(crate) fn staged_state(resource: This) -> Option<(bool, bool)> {
    use std::sync::atomic::Ordering::Relaxed;
    let map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
    let staging = map.as_ref()?.get(&(resource as usize))?;
    Some((staging.uploaded.load(Relaxed), staging.dirty.load(Relaxed)))
}

pub(crate) fn is_staged(resource: This) -> bool {
    let map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
    map.as_ref()
        .is_some_and(|map| map.contains_key(&(resource as usize)))
}

/// The bytes of a title texture's memory, and whether all of it is mapped,
/// if the texture has been staged for upload.
pub(crate) fn staged(resource: This) -> Option<(u64, bool)> {
    let s = {
        let map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
        map.as_ref()?.get(&(resource as usize))?.clone()
    };
    if s.on_demand {
        // (Neptune) A texture made level by level is all there when the levels the title
        // holds are: that the others' memory is not mapped is its lasting state, not a
        // texture streaming in. Told "not whole", the pass over the heap's views uploaded
        // each of them again every 120 frames for good: 1,300 slots "uploaded in part" in
        // the first room, some 200 uploads a second, each of the texture's whole span
        // read, hashed and detiled. When the title maps more of it or writes its view
        // the slot is due again (`texture_slots_remapped`, `note_buffer`).
        return Some((s.held_bytes() as u64, true));
    }
    Some((s.size as u64, staged_ask::whole(resource, s.memory, s.size, committed)))
}

/// The first mip a title texture's staging buffer holds; `u32::MAX` without
/// one.
#[cfg(test)]
pub(crate) fn staged_from(resource: This) -> u32 {
    let map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
    map.as_ref()
        .and_then(|map| map.get(&(resource as usize)))
        .map_or(u32::MAX, |s| s.first)
}

/// Whether `[at, at + len)` is committed memory (streamed textures may be
/// unmapped until their pages arrive).
pub(crate) fn committed(at: *const u8, len: usize) -> bool {
    if by_query() {
        return committed_by_query(at, len);
    }
    if len == 0 {
        return true;
    }
    let (there, of) = readable(at as usize, len, true);
    there == of
}

/// Whether the two questions above and below are put to `VirtualQuery`, as
/// they were (`VICTORY_COMMITTED_QUERY=1`). `VirtualQuery` answers with the
/// length of the run of pages in the same state, which it finds by walking
/// the page tables to the run's end: 8.8 ms for one question in the title's
/// 1.1 GB block (round 6). The pass over the heap's views asks three or
/// more of them for every texture slot that is due, 3,000 slots a frame
/// when the card has no room (run 75): that pass then ran 208-264 ms, and
/// while it ran the submitting thread's write-watch askings waited behind
/// the walks (run 76: 8-22 askings a frame of 6-21 ms each, 46-172 ms).
fn by_query() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_COMMITTED_QUERY").is_some_and(|v| v == "1"))
}

/// Of the kernel's pages that hold `[at, at + len)` (`len > 0`): how many
/// can be read, and how many there are; with `all`, it stops at the first
/// that cannot. One question to the kernel for up to 256 pages at once
/// (`QueryWorkingSetEx`: the state of each page asked about, no walk); a
/// page not in the working set may still be there (never touched, or
/// trimmed), and a one-byte read through the kernel says.
/// A page that is committed and may not be read counts as not there, which
/// is what the callers ask for: they read or write the memory next.
fn readable(at: usize, len: usize, all: bool) -> (usize, usize) {
    use windows_sys::Win32::System::ProcessStatus::{
        K32QueryWorkingSetEx, PSAPI_WORKING_SET_EX_INFORMATION,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    const KERNEL_PAGE: usize = 4096;
    let (first, last) = (at / KERNEL_PAGE, at.saturating_add(len - 1) / KERNEL_PAGE);
    let of = last - first + 1;
    let mut there = 0;
    // SAFETY: plain data.
    let mut info: [PSAPI_WORKING_SET_EX_INFORMATION; 256] = unsafe { std::mem::zeroed() };
    let mut page = first;
    while page <= last {
        let n = (last - page + 1).min(info.len());
        for (i, entry) in info[..n].iter_mut().enumerate() {
            entry.VirtualAddress = ((page + i) * KERNEL_PAGE) as *mut _;
        }
        // SAFETY: `n` entries of the size given; our own process.
        let asked = unsafe {
            K32QueryWorkingSetEx(
                GetCurrentProcess(),
                info.as_mut_ptr().cast(),
                (n * size_of::<PSAPI_WORKING_SET_EX_INFORMATION>()) as u32,
            )
        } != 0;
        for (i, entry) in info[..n].iter().enumerate() {
            // Bit 0 of the answer: the page is in the working set.
            // SAFETY: plain data the query filled in.
            let valid = asked && unsafe { entry.VirtualAttributes.Flags } & 1 != 0;
            let mut byte = [0u8];
            if valid || crate::host::read_into(((page + i) * KERNEL_PAGE) as *const u8, &mut byte) {
                there += 1;
            } else if all {
                return (there, of);
            }
        }
        page += n;
    }
    (there, of)
}

fn committed_by_query(at: *const u8, len: usize) -> bool {
    use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEMORY_BASIC_INFORMATION, VirtualQuery};
    let mut p = at as usize;
    let end = p + len;
    while p < end {
        // SAFETY: plain data filled in by the query.
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: a valid out pointer of the right size.
        let n = unsafe { VirtualQuery(p as *const _, &mut info, size_of_val(&info)) };
        if n == 0 || info.State != MEM_COMMIT {
            return false;
        }
        p = info.BaseAddress as usize + info.RegionSize;
    }
    true
}

/// How many bytes of `[at, at + len)` are committed memory: all of a
/// texture that is there, some of one streaming in (its smaller mips lie
/// at the start of its memory, and their pages are mapped first), none of
/// one not there. With neither the first nor the last page committed the
/// answer is none without a look at the pages between (one query for the
/// slots whose texture is away, which are asked again and again).
pub(crate) fn mapped_bytes(at: *const u8, len: usize) -> u64 {
    use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEMORY_BASIC_INFORMATION, VirtualQuery};
    use windows_sys::Win32::System::ProcessStatus::{
        K32QueryWorkingSetEx, PSAPI_WORKING_SET_EX_INFORMATION,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    if len == 0 {
        return 0;
    }
    let start = at as usize;
    let end = start.saturating_add(len);
    if by_query() {
        let holes = mapped_holes::on();
        if !holes && !committed_by_query(at, 1) && !committed_by_query((end - 1) as *const u8, 1) {
            return 0;
        }
        let (mut p, mut there) = (start, 0u64);
        while p < end {
            // SAFETY: plain data filled in by the query.
            let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
            // SAFETY: a valid out pointer of the right size.
            let n = unsafe { VirtualQuery(p as *const _, &mut info, size_of_val(&info)) };
            let next = (info.BaseAddress as usize)
                .saturating_add(info.RegionSize)
                .min(end);
            if n == 0 || next <= p {
                break;
            }
            if if holes { mapped_holes::region_counts(&info) } else { info.State == MEM_COMMIT } {
                there += (next - p) as u64;
            }
            p = next;
        }
        return there;
    }
    const KERNEL_PAGE: usize = 4096;
    // SAFETY: pseudo-handle for our own process.
    let process = unsafe { GetCurrentProcess() };
    let first_page = start & !(KERNEL_PAGE - 1);
    let last_page = (end - 1) & !(KERNEL_PAGE - 1);
    // Query the first and last pages in one `QueryWorkingSetEx` call so
    // unmapped ("away") slots and single-block textures take a single query.
    // SAFETY: plain data.
    let mut ends: [PSAPI_WORKING_SET_EX_INFORMATION; 2] = unsafe { std::mem::zeroed() };
    ends[0].VirtualAddress = first_page as *mut _;
    let end_count = if last_page == first_page {
        1
    } else {
        ends[1].VirtualAddress = last_page as *mut _;
        2
    };
    // SAFETY: `end_count` entries of the size given; our own process.
    let asked = unsafe {
        K32QueryWorkingSetEx(
            process,
            ends.as_mut_ptr().cast(),
            (end_count * size_of::<PSAPI_WORKING_SET_EX_INFORMATION>()) as u32,
        )
    } != 0;
    // SAFETY: plain data the query filled in.
    let valid0 = asked && unsafe { ends[0].VirtualAttributes.Flags } & 1 != 0;
    let mut byte = [0u8];
    let first_ok = valid0 || crate::host::read_into(first_page as *const u8, &mut byte);
    if !first_ok {
        // SAFETY: plain data the query filled in (only read when `end_count == 2`).
        let last_ok = end_count == 2
            && ((asked && unsafe { ends[1].VirtualAttributes.Flags } & 1 != 0)
                || crate::host::read_into(last_page as *const u8, &mut byte));
        if !last_ok {
            return if mapped_holes::on() { mapped_holes::between(start, end) } else { 0 };
        }
    }
    static BY_PAGE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *BY_PAGE.get_or_init(|| std::env::var_os("VICTORY_MAPPED_BY_PAGE").is_some_and(|v| v == "1"))
    {
        // By page, without the walk: whole pages, cut to the range.
        let (there, _) = readable(start, len, false);
        return (there as u64 * KERNEL_PAGE as u64).min(len as u64);
    }
    // By 64 KiB block, one page asked of each: the title's memory is
    // mapped in blocks of that size (the console's page), and the
    // answer is a measure of how much of a texture has arrived, not a
    // promise that every byte can be read (the upload reads through the
    // kernel). By page, a texture half there cost a failed kernel read
    // for each of its missing pages, every frame it stayed due: 1,663
    // textures "uploaded in part" kept the pass over the heap's views at
    // 90-160 ms a frame (run 81). `VICTORY_MAPPED_BY_PAGE=1`: by page.
    // Batch up to 256 block probes into a single `QueryWorkingSetEx` call
    // (16 MiB per query) instead of one kernel call per 64 KiB block.
    const BLOCK: usize = 64 << 10;
    let block0 = start & !(BLOCK - 1);
    let next0 = block0.saturating_add(BLOCK);
    let mut there = if first_ok {
        (next0.min(end) - start) as u64
    } else {
        0
    };
    if next0 == block0 || next0 >= end {
        return there;
    }
    // SAFETY: plain data.
    let mut info: [PSAPI_WORKING_SET_EX_INFORMATION; 256] = unsafe { std::mem::zeroed() };
    let mut spans = [0u64; 256];
    let mut block = next0;
    while block < end {
        let mut n = 0usize;
        while block < end && n < info.len() {
            info[n].VirtualAddress = block as *mut _;
            info[n].VirtualAttributes.Flags = 0;
            let next = block.saturating_add(BLOCK);
            spans[n] = (next.min(end) - block) as u64;
            n += 1;
            if next == block {
                block = end;
                break;
            }
            block = next;
        }
        // SAFETY: `n` entries of the size given; our own process.
        let asked = unsafe {
            K32QueryWorkingSetEx(
                process,
                info.as_mut_ptr().cast(),
                (n * size_of::<PSAPI_WORKING_SET_EX_INFORMATION>()) as u32,
            )
        } != 0;
        for i in 0..n {
            // SAFETY: plain data the query filled in.
            let valid = asked && unsafe { info[i].VirtualAttributes.Flags } & 1 != 0;
            if valid || crate::host::read_into(info[i].VirtualAddress as *const u8, &mut byte) {
                there += spans[i];
            }
        }
    }
    there
}

/// The first mip a staging buffer holds and the bytes of title memory read
/// for it and the smaller ones, for a texture of `mips` mips a view shows
/// from `first` on: from `first` only where the layout can end behind the
/// smaller mips (one 2-D slice: `bytes_from` is the whole layout for any
/// other), else all of it.
fn staged_part(layout: &victory_formats::texture::Layout, mips: u32, first: u32) -> (u32, usize) {
    let first = if first < mips && layout.bytes_from(first) < layout.size {
        first
    } else {
        0
    };
    (first, layout.bytes_from(first) as usize)
}

/// (Neptune) The bytes of title memory a staging buffer of `resource` from
/// mip `first` on reads ([`staged_part`]), as the pass counts them; 0 for
/// what has no console layout.
fn staged_bytes(resource: This, first: u32) -> u64 {
    let Some((_, desc)) = crate::resource::texture_memory(resource) else {
        return 0;
    };
    let Some(texture) = crate::resource::texture_layout(&desc) else {
        return 0;
    };
    let Some(plane) = texture.planes.first() else {
        return 0;
    };
    staged_part(&plane.layout, desc.mip_levels(), first).1 as u64
}

/// The staging state of a title texture from mip `first` on, made on first
/// use (and made anew for larger mips than the one there holds).
fn staging(resource: This, texture: &ID3D12Resource, first: u32) -> Option<Arc<Staging>> {
    // Asked before the table is locked: the sweep lets go of a staging
    // buffer with its texture's host side locked, in that order.
    let on_demand = crate::resource::host_tiled(resource).is_some();
    // (Neptune) A texture made level by level stages every level (zeros for the ones the
    // title does not hold): its upload gives the card's memory by the same answer.
    let first = if on_demand { 0 } else { first };
    if stage_unlocked::on() {
        return stage_unlocked::staging(resource, texture, first, on_demand);
    }
    let mut map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
    let map = map.get_or_insert_with(HashMap::new);
    if let Some(s) = map.get(&(resource as usize))
        && s.first <= first
    {
        return Some(s.clone());
    }
    // (Neptune) One a prepare thread made and detiled (`prepare`): taken as
    // it is (`VICTORY_TEXTURE_PREPARE_THREADS`; none without).
    if let Some(s) = prepare::take_ready(resource, first, on_demand) {
        map.insert(resource as usize, s.clone());
        crate::resource::set_staged_first(resource, s.first);
        return Some(s);
    }
    let s = build_staging(resource, texture, first, on_demand)?;
    map.insert(resource as usize, s.clone());
    crate::resource::set_staged_first(resource, s.first);
    Some(s)
}

/// (Neptune) For a prepare thread (`prepare`): the staging state of title
/// texture `resource` from mip `first` on, made as [`staging`] makes it but
/// not put in the table (the pass that records its upload puts it there).
/// `None` if the table has one that serves already, or none can be made.
fn prepared_staging(resource: This, texture: &ID3D12Resource, first: u32) -> Option<Arc<Staging>> {
    let on_demand = crate::resource::host_tiled(resource).is_some();
    let first = if on_demand { 0 } else { first };
    {
        let map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
        if map
            .as_ref()
            .and_then(|map| map.get(&(resource as usize)))
            .is_some_and(|s| s.first <= first)
        {
            return None;
        }
    }
    build_staging(resource, texture, first, on_demand)
}

/// A staging state for title texture `resource` from mip `first` on (as
/// [`staging`] decided it), its upload buffer made or taken from the pool;
/// nothing is detiled into it yet. Timed into [`STAGED_HERE`].
fn build_staging(
    resource: This,
    texture: &ID3D12Resource,
    first: u32,
    on_demand: bool,
) -> Option<Arc<Staging>> {
    let started = std::time::Instant::now();
    let made = build_staging_untimed(resource, texture, first, on_demand);
    let us = started.elapsed().as_micros() as u64;
    STAGED_HERE.with(|s| {
        let mut now = s.get();
        now[0] += 1;
        now[1] += us;
        s.set(now);
    });
    made
}

fn build_staging_untimed(
    resource: This,
    texture: &ID3D12Resource,
    first: u32,
    on_demand: bool,
) -> Option<Arc<Staging>> {
    let (memory, desc) = crate::resource::texture_memory(resource)?;
    let Some(layout) = crate::resource::texture_layout(&desc).map(|t| t.planes[0].layout.clone())
    else {
        once!(
            ?desc,
            "texture contents without a console layout not uploaded"
        );
        return None;
    };
    if layout.desc.swizzle.xor() {
        once!(?desc, "texture contents in an XOR mode not uploaded");
        return None;
    }
    let host = host()?;
    // SAFETY: a live resource.
    let d = unsafe { texture.GetDesc() };
    let (first, size) = staged_part(&layout, u32::from(d.MipLevels), first);
    let count = if d.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE3D {
        u32::from(d.MipLevels)
    } else {
        u32::from(d.MipLevels) * u32::from(d.DepthOrArraySize) - first
    };
    let mut footprints = vec![D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); count as usize];
    let mut rows = vec![0u32; count as usize];
    let mut total = 0;
    // SAFETY: outputs for `count` subresources.
    unsafe {
        host.device.GetCopyableFootprints(
            &d,
            first,
            count,
            0,
            Some(footprints.as_mut_ptr()),
            Some(rows.as_mut_ptr()),
            None,
            Some(&mut total),
        );
    }
    // On the card's way (`VICTORY_GPU_DETILE=1`): its dispatches and buffer,
    // and the staging buffer holds the bytes as laid out, the mask and tables.
    let gpu = gpu_detile::plan(
        &layout,
        size as u64,
        &footprints,
        &rows,
        |i| {
            let mips = u32::from(d.MipLevels) - first;
            (first + i as u32 % mips, i as u32 / mips)
        },
        total,
    );
    let total = gpu.as_ref().map_or(total, |g| g.raw.bytes);
    let (piece, buffer, base, data) = match crate::texpool::staging(host, total) {
        Some((piece, buffer, base, data)) => (Some(piece), buffer, base, data),
        None => {
            let (buffer, data) = stage_buffer(host, total)?;
            crate::host::name(&buffer, || {
                format!(
                    "staging of {resource:?} {}x{}x{} f{}",
                    d.Width, d.Height, d.DepthOrArraySize, d.Format.0
                )
            });
            (None, buffer, 0, data)
        }
    };
    if let Some(gpu) = &gpu {
        // SAFETY: a new staging buffer's mapping of `raw.bytes`, ours alone.
        unsafe { gpu_detile::prepare(gpu, &layout, data) };
    }
    tracing::debug!(?desc, total, "texture contents uploaded from title memory");
    STAGED[0].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    STAGED[1].fetch_add(total, std::sync::atomic::Ordering::Relaxed);
    let s = Arc::new(Staging {
        resource: resource as usize,
        buffer,
        data,
        footprints,
        rows,
        layout,
        mips: u32::from(d.MipLevels),
        first,
        size,
        bytes: total,
        shape: desc.shape(),
        memory,
        last: Mutex::default(),
        dirty: std::sync::atomic::AtomicBool::new(true),
        uploaded: std::sync::atomic::AtomicBool::new(false),
        used: std::sync::atomic::AtomicU64::new(crate::host::moment()),
        on_demand,
        detiled: std::sync::atomic::AtomicU32::new(0),
        judged: [const { std::sync::atomic::AtomicU64::new(0) }; 2],
        waiting: std::sync::atomic::AtomicU64::new(0),
        base,
        piece,
        gpu,
        sent: std::sync::atomic::AtomicU64::new(0),
    });
    Some(s)
}

#[cfg(test)]
mod first_mip_tests {
    use victory_formats::texture::{Dimension, Swizzle, Texture, TextureDesc};

    fn layout(depth_or_array: u32) -> victory_formats::texture::Layout {
        let desc = TextureDesc {
            dimension: Dimension::Tex2d,
            width: 1024,
            height: 1024,
            depth_or_array,
            mips: 9,
            format: 71,
            samples: 1,
            swizzle: Swizzle::SW_4KB_S,
        };
        Texture::new(&desc).unwrap().planes[0].layout.clone()
    }

    /// A view from mip 4 of a 1024x1024 BC1 texture of 9 mips stages mips
    /// 4 to 8 and reads only the memory they take; a view of all of it, an
    /// array's view, or a first mip past the chain stage all of it.
    #[test]
    fn a_view_of_the_smaller_mips_stages_only_their_memory() {
        let l = layout(1);
        assert_eq!(super::staged_part(&l, 9, 4), (4, l.bytes_from(4) as usize));
        assert!(l.bytes_from(4) < l.size);
        assert_eq!(super::staged_part(&l, 9, 0), (0, l.size as usize));
        assert_eq!(
            super::staged_part(&l, 9, 9),
            (0, l.size as usize),
            "past the chain"
        );
        let array = layout(2);
        assert_eq!(
            super::staged_part(&array, 9, 4),
            (0, array.size as usize),
            "slices each hold their chain"
        );
    }

    /// A 2-D view's first mip is its fifth word; other views show all mips.
    #[test]
    fn a_2d_view_names_its_first_mip() {
        assert_eq!(
            crate::resource::first_mip(&[71, 4, 0x1688, 0, 4, 5, 0, 0, 0, 0]),
            4
        );
        assert_eq!(
            crate::resource::first_mip(&[71, 5, 0x1688, 0, 4, 5, 0, 0, 0, 0]),
            0,
            "an array view"
        );
        assert_eq!(
            crate::resource::first_mip(&[71, 4, 0x1688, 0, 99, 5, 0, 0, 0, 0]),
            15
        );
    }
}

/// Shared body of [`Staging::refresh_unlocked`], separated for unit tests.
fn refresh_unlocked_with(
    memory: *const u8,
    last: &mut Last,
    size: usize,
    detile: impl FnOnce(&[u8], bool, blake3::Hash),
) -> usize {
    use std::sync::atomic::Ordering::Relaxed;
    let read = with_read(size, |read| {
        if crate::host::read_into(memory, read) {
            let hash = blake3::hash(read);
            if last.hash != Some(hash) {
                last.hash = Some(hash);
                detile(read, true, hash);
            }
            return true;
        }
        // If the staging buffer was never filled, a partly mapped streamed
        // texture would otherwise stay all zeros forever when `no_wait()`
        // misses the remap lock. Read mapped pages via `copy_mapped` instead.
        if last.hash.is_none() {
            crate::host::copy_mapped(read.as_mut_ptr(), memory, size, false);
            if read.iter().any(|&b| b != 0) {
                let hash = blake3::hash(read);
                last.hash = Some(hash);
                last.partial = Some(crate::frame::current());
                detile(read, false, hash);
                return true;
            }
        }
        false
    });
    if read {
        UNLOCKED[0].fetch_add(1, Relaxed);
        size
    } else {
        UNLOCKED[1].fetch_add(1, Relaxed);
        last.asked = 0;
        0
    }
}

#[cfg(test)]
mod title_memory_tests {
    use super::with_title;

    #[test]
    fn no_wait_unlocked_falls_back_for_never_filled_partly_mapped_texture() {
        use windows_sys::Win32::System::Memory::*;
        const MAPPED: usize = 0x1000;
        const SIZE: usize = 0x4000;
        // SAFETY: private reservation owned by this test; committed head has
        // known bytes, uncommitted tail forces `read_into` to fail.
        unsafe {
            let base = VirtualAlloc(std::ptr::null(), SIZE, MEM_RESERVE, PAGE_NOACCESS).cast::<u8>();
            assert!(!base.is_null());
            assert_eq!(VirtualAlloc(base.cast(), MAPPED, MEM_COMMIT, PAGE_READWRITE), base.cast());
            base.write_bytes(0x3c, MAPPED);

            let mut last = super::Last { asked: 7, ..Default::default() };
            let mut detiled = Vec::new();
            let mut was_whole = true;
            let read = super::refresh_unlocked_with(base, &mut last, SIZE, |bytes, whole, _| {
                detiled = bytes.to_vec();
                was_whole = whole;
            });
            assert!(!was_whole);
            assert_eq!(read, SIZE);
            assert!(last.hash.is_some());
            assert!(last.partial.is_some());
            assert_eq!(detiled.len(), SIZE);
            assert!(detiled[..MAPPED].iter().all(|&b| b == 0x3c));
            assert!(detiled[MAPPED..].iter().all(|&b| b == 0));

            // Once filled, a failed `read_into` does not overwrite the staging buffer.
            let mut detiled_again = false;
            let read2 = super::refresh_unlocked_with(base, &mut last, SIZE, |_, _, _| {
                detiled_again = true;
            });
            assert_eq!(read2, 0);
            assert_eq!(last.asked, 0);
            assert!(!detiled_again);

            assert_ne!(VirtualFree(base.cast(), 0, MEM_RELEASE), 0);
        }
    }

    /// Title memory whose tail is reserved and not committed reads through
    /// the copy as its bytes and then zeros, without a fault; so does
    /// memory a page of which is taken away between two reads, and the
    /// zeros are zeros whatever the read buffer held before.
    #[test]
    fn an_uncommitted_tail_reads_as_zeros() {
        use windows_sys::Win32::System::Memory::*;
        const HEAD: usize = 0xb_0000;
        const SIZE: usize = 0x20_0000;
        // SAFETY: a private reservation of our own; only committed pages
        // are written in place, and nothing else reads them in place.
        unsafe {
            // The read buffer holds something else first.
            let other = vec![0x77u8; SIZE];
            with_title(other.as_ptr(), SIZE, |bytes| {
                assert!(bytes.iter().all(|&b| b == 0x77))
            });
            let base =
                VirtualAlloc(std::ptr::null(), SIZE, MEM_RESERVE, PAGE_NOACCESS).cast::<u8>();
            assert!(!base.is_null());
            assert_eq!(
                VirtualAlloc(base.cast(), HEAD, MEM_COMMIT, PAGE_READWRITE),
                base.cast()
            );
            base.write_bytes(0x5a, HEAD);
            with_title(base, SIZE, |bytes| {
                assert!(bytes[..HEAD].iter().all(|&b| b == 0x5a), "what is mapped");
                assert!(bytes[HEAD..].iter().all(|&b| b == 0), "what is not: zeros");
            });
            // The title takes a page in the middle away: zeros from then
            // on, the rest as before.
            assert_ne!(
                VirtualFree(base.add(0x4_0000).cast(), 0x1_0000, MEM_DECOMMIT),
                0
            );
            with_title(base, SIZE, |bytes| {
                assert!(bytes[..0x4_0000].iter().all(|&b| b == 0x5a));
                assert!(bytes[0x4_0000..0x5_0000].iter().all(|&b| b == 0));
                assert!(bytes[0x5_0000..HEAD].iter().all(|&b| b == 0x5a));
                assert!(bytes[HEAD..].iter().all(|&b| b == 0));
            });
            assert_ne!(VirtualFree(base.cast(), 0, MEM_RELEASE), 0);
        }
    }

    /// The texture both device-loss runs named last (16 texels wide, 4 or 8
    /// high, format 10, layout 0x100, console flags 0x300000; its memory
    /// began 0x300 into a page in run 73 and 0x700 in run 72, and here also
    /// 0xf00, so that a page line runs through it): what the detiling is
    /// given and what it writes are the same read in place and read through
    /// the copy, and the rows it writes end where the staging buffer the
    /// host sizes for it ends (896 bytes for 4 rows of 128 bytes 256 apart,
    /// 1,920 for 8).
    #[test]
    fn the_small_texture_of_runs_72_and_73_detiles_the_same_both_ways() {
        use windows_sys::Win32::System::Memory::*;
        for (height, within) in [(4u32, 0x300usize), (8, 0x700), (4, 0xf00), (8, 0xf00)] {
            // The description as the log printed it, word for word.
            let words: [u32; 14] = [
                3, 0, 0, 0, 16, 0, height, 65537, 10, 1, 0, 256, 3_145_728, 0,
            ];
            // SAFETY: fourteen words are a resource description (56 bytes,
            // plain numbers).
            let desc: crate::resource::ResourceDesc = unsafe { std::mem::transmute(words) };
            let layout = crate::resource::texture_layout(&desc)
                .expect("a console layout")
                .planes[0]
                .layout
                .clone();
            let size = layout.size as usize;
            let (row, pitch) = (16 * 8, 256usize);
            assert!(
                size >= row * height as usize,
                "the layout holds every texel"
            );
            // Committed pages and a reserved one after them; the texture
            // begins `within` bytes into the first, as the title placed it.
            // SAFETY: our own reservation; the committed part is written
            // and read in place, by this thread alone.
            unsafe {
                let pages = size.div_ceil(4096) + 1;
                let base = VirtualAlloc(
                    std::ptr::null(),
                    (pages + 1) * 4096,
                    MEM_RESERVE,
                    PAGE_NOACCESS,
                )
                .cast::<u8>();
                assert!(!base.is_null());
                assert_eq!(
                    VirtualAlloc(base.cast(), pages * 4096, MEM_COMMIT, PAGE_READWRITE),
                    base.cast()
                );
                for i in 0..pages * 4096 {
                    base.add(i).write((i * 7 + i / 251) as u8);
                }
                let memory = base.add(within);
                let in_place = std::slice::from_raw_parts(memory, size);
                // The bytes: the same in place and through the copy.
                with_title(memory, size, |copied| assert_eq!(copied, in_place));
                // What the detiling writes: the same, and inside the bytes
                // the host's staging buffer for this texture has.
                let staged = (height as usize - 1) * pitch + row;
                assert_eq!(staged, if height == 4 { 896 } else { 1920 });
                let detile = |tiled: &[u8]| {
                    // The slice `Staging::detile` hands over: whole rows.
                    let mut linear = vec![0xeeu8; pitch * height as usize];
                    layout
                        .detile(tiled, 0, 0, &mut linear, pitch, pitch * height as usize)
                        .expect("detiled");
                    linear
                };
                let direct = detile(in_place);
                let through = with_title(memory, size, |copied| detile(copied));
                assert_eq!(direct, through);
                for y in 0..height as usize {
                    assert!(
                        direct[y * pitch..][..row].iter().any(|&b| b != 0xee),
                        "row {y} written"
                    );
                    assert!(
                        direct[y * pitch + row..(y + 1) * pitch]
                            .iter()
                            .all(|&b| b == 0xee),
                        "nothing past row {y}"
                    );
                }
                // A buffer of exactly the staged bytes is enough.
                let mut exact = vec![0u8; staged];
                layout
                    .detile(in_place, 0, 0, &mut exact, pitch, pitch * height as usize)
                    .expect("the staging buffer's bytes are enough");
                assert_ne!(VirtualFree(base.cast(), 0, MEM_RELEASE), 0);
            }
        }
    }
}

#[cfg(test)]
mod mapped_bytes_tests {
    use std::time::Instant;

    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_DECOMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_NOACCESS, PAGE_READWRITE,
        VirtualAlloc, VirtualFree,
    };

    use super::*;

    const BLOCK: usize = 64 << 10;

    fn oracle_block_bytes(start: usize, len: usize, block_mapped: impl Fn(usize) -> bool) -> u64 {
        if len == 0 {
            return 0;
        }
        let end = start.saturating_add(len);
        let mut there = 0u64;
        let mut block = start & !(BLOCK - 1);
        while block < end {
            let (lo, hi) = (block.max(start), block.saturating_add(BLOCK).min(end));
            if block_mapped(lo) {
                there += (hi - lo) as u64;
            }
            block = block.saturating_add(BLOCK);
        }
        there
    }

    #[test]
    fn mapped_bytes_matches_block_oracle_across_layouts() {
        const BLOCKS: usize = 320; // 20 MiB (> 256 blocks: crosses multiple 256-entry batches)
        const TOTAL: usize = BLOCKS * BLOCK;
        // SAFETY: isolated reservation owned by this test and released before return.
        let base = unsafe {
            VirtualAlloc(std::ptr::null(), TOTAL, MEM_RESERVE, PAGE_NOACCESS).cast::<u8>()
        };
        assert!(!base.is_null(), "VirtualAlloc reserve failed");

        let commit = |first_block: usize, count: usize, touch: bool| unsafe {
            let ptr = base.add(first_block * BLOCK);
            let bytes = count * BLOCK;
            let got = VirtualAlloc(ptr.cast(), bytes, MEM_COMMIT, PAGE_READWRITE);
            assert_eq!(got, ptr.cast(), "VirtualAlloc commit failed");
            if touch {
                for b in 0..count {
                    ptr.add(b * BLOCK).write((b & 0xff) as u8);
                }
            }
        };
        let decommit = |first_block: usize, count: usize| unsafe {
            let ptr = base.add(first_block * BLOCK);
            assert_ne!(
                VirtualFree(ptr.cast(), count * BLOCK, MEM_DECOMMIT),
                0,
                "VirtualFree decommit failed"
            );
        };

        // 1. Zero-length and completely unmapped spans (sub-page, 1 block, 4 MiB, 20 MiB).
        assert_eq!(mapped_bytes(base, 0), 0);
        assert_eq!(mapped_bytes(base, 128), 0);
        assert_eq!(mapped_bytes(base, BLOCK), 0);
        assert_eq!(mapped_bytes(base, 4 << 20), 0);
        assert_eq!(mapped_bytes(base, TOTAL), 0);

        // 2. Committed-but-untouched pages (not yet in the working set -> read_into fallback).
        commit(0, 6, false);
        assert_eq!(mapped_bytes(base, 6 * BLOCK), (6 * BLOCK) as u64);
        assert_eq!(mapped_bytes(base, 12 * BLOCK), (6 * BLOCK) as u64);

        // 3. Single block and sub-page ranges when mapped.
        commit(0, 6, true);
        assert_eq!(mapped_bytes(base, 128), 128);
        assert_eq!(mapped_bytes(base, 4096), 4096);
        assert_eq!(mapped_bytes(base, BLOCK), BLOCK as u64);

        // 4. Unaligned start and unaligned end spanning committed + uncommitted blocks.
        let unaligned_start = 12_345usize;
        let unaligned_len = 8 * BLOCK + 7_777;
        let expected = oracle_block_bytes(base as usize + unaligned_start, unaligned_len, |lo| {
            let idx = (lo - base as usize) / BLOCK;
            idx < 6
        });
        assert_eq!(
            unsafe { mapped_bytes(base.add(unaligned_start), unaligned_len) },
            expected
        );

        // 5. Middle hole + >256 blocks (20 MiB): blocks [0..96) and [200..320) committed.
        commit(6, 90, true); // [0..96)
        commit(200, 120, true); // [200..320)
        assert_eq!(mapped_bytes(base, TOTAL), ((96 + 120) * BLOCK) as u64);

        // 6. Unmapped start, mapped tail: [96..200) unmapped, [200..208) mapped.
        let tail_start = unsafe { base.add(96 * BLOCK) };
        let tail_span = (208 - 96) * BLOCK;
        assert_eq!(mapped_bytes(tail_start, tail_span), (8 * BLOCK) as u64);

        // 7. Edge case: single 64 KiB block where first 4 KiB page is uncommitted, last 4 KiB page IS committed.
        // Fast check sees last page committed, then block probe checks block start page (uncommitted) -> 0.
        decommit(100, 1);
        unsafe {
            let last_page = base.add(100 * BLOCK + BLOCK - 4096);
            assert_eq!(
                VirtualAlloc(last_page.cast(), 4096, MEM_COMMIT, PAGE_READWRITE),
                last_page.cast()
            );
            last_page.write(0x5a);
            assert_eq!(mapped_bytes(base.add(100 * BLOCK), BLOCK), 0);
            assert_ne!(VirtualFree(last_page.cast(), 4096, MEM_DECOMMIT), 0);
        }

        // SAFETY: releases the reservation allocated above.
        unsafe {
            assert_ne!(VirtualFree(base.cast(), 0, MEM_RELEASE), 0);
        }
    }

    #[test]
    fn mapped_bytes_streaming_sweep_benchmark() {
        const MB: usize = 1 << 20;
        const SWEEPS: usize = 10;

        // Allocate distinct synthetic backing regions for each residency class:
        // - 16 MiB fully resident (touched)
        // - 4 MiB fully resident (touched)
        // - 4 MiB half resident (2 MiB committed + touched, 2 MiB reserved)
        // - 4 MiB unmapped ("away" slot)
        // - 64 KiB fully resident (1 block)
        let alloc_region = |total: usize, commit_bytes: usize| -> *mut u8 {
            // SAFETY: isolated test reservation.
            unsafe {
                let p =
                    VirtualAlloc(std::ptr::null(), total, MEM_RESERVE, PAGE_NOACCESS).cast::<u8>();
                assert!(!p.is_null());
                if commit_bytes > 0 {
                    assert_eq!(
                        VirtualAlloc(p.cast(), commit_bytes, MEM_COMMIT, PAGE_READWRITE),
                        p.cast()
                    );
                    let mut off = 0;
                    while off < commit_bytes {
                        p.add(off).write(((off >> 12) & 0xff) as u8);
                        off += 4096;
                    }
                }
                p
            }
        };

        let r_16m_full = alloc_region(16 * MB, 16 * MB);
        let r_4m_full = alloc_region(4 * MB, 4 * MB);
        let r_4m_half = alloc_region(4 * MB, 2 * MB);
        let r_4m_away = alloc_region(4 * MB, 0);
        let r_64k_full = alloc_region(BLOCK, BLOCK);

        let run_category =
            |ptr: *const u8, len: usize, count: usize, expected_each: u64| -> (f64, u64) {
                // Warmup once.
                assert_eq!(mapped_bytes(ptr, len), expected_each);
                let t0 = Instant::now();
                let mut sum = 0u64;
                for _ in 0..SWEEPS {
                    for _ in 0..count {
                        sum += std::hint::black_box(mapped_bytes(
                            std::hint::black_box(ptr),
                            std::hint::black_box(len),
                        ));
                    }
                }
                let ms = t0.elapsed().as_secs_f64() * 1000.0;
                assert_eq!(sum, expected_each * (count as u64) * (SWEEPS as u64));
                (ms, sum)
            };

        let (ms_16m_full, _) = run_category(r_16m_full, 16 * MB, 100, (16 * MB) as u64);
        let (ms_4m_full, _) = run_category(r_4m_full, 4 * MB, 400, (4 * MB) as u64);
        let (ms_4m_half, _) = run_category(r_4m_half, 4 * MB, 400, (2 * MB) as u64);
        let (ms_4m_away, _) = run_category(r_4m_away, 4 * MB, 1600, 0);
        let (ms_64k_full, _) = run_category(r_64k_full, BLOCK, 400, BLOCK as u64);
        let total_ms = ms_16m_full + ms_4m_full + ms_4m_half + ms_4m_away + ms_64k_full;

        eprintln!(
            "mapped_bytes_bench ({SWEEPS} sweeps x 2900 slots): total={total_ms:.2}ms \
             [16MiB_full(100)={ms_16m_full:.2}ms 4MiB_full(400)={ms_4m_full:.2}ms \
             4MiB_half(400)={ms_4m_half:.2}ms 4MiB_away(1600)={ms_4m_away:.2}ms \
             64KiB_full(400)={ms_64k_full:.2}ms]"
        );

        // SAFETY: release all test reservations.
        unsafe {
            for p in [r_16m_full, r_4m_full, r_4m_half, r_4m_away, r_64k_full] {
                assert_ne!(VirtualFree(p.cast(), 0, MEM_RELEASE), 0);
            }
        }
    }
}

#[cfg(test)]
mod prelude_upload_tests {
    use super::claim_upload;
    use std::sync::atomic::{AtomicBool, Ordering::Release};

    #[test]
    fn first_upload_clean_skip_and_immediate_same_frame_reupload() {
        let uploaded = AtomicBool::new(false);
        let dirty = AtomicBool::new(true);

        // Frame 1: first upload runs after refresh() detiled into staging.
        assert_eq!(claim_upload(&uploaded, &dirty), Some(true));
        // Second list in the same submission skips redundant copy.
        assert_eq!(claim_upload(&uploaded, &dirty), None);

        // Frame 2: texture unchanged — refresh() does not detile, prelude skips.
        assert_eq!(claim_upload(&uploaded, &dirty), None);

        // Frame 3: texture changed — refresh()/detile() runs BEFORE claim_upload,
        // so upload is claimed on the exact same frame (0 frames late).
        dirty.store(true, Release);
        assert_eq!(claim_upload(&uploaded, &dirty), Some(false));

        // Frame 4: consecutive change (e.g. refresh_front() on every frame)
        // must also upload immediately, never alternating upload/skip.
        dirty.store(true, Release);
        assert_eq!(claim_upload(&uploaded, &dirty), Some(false));
    }

    #[test]
    fn three_buffer_ring_never_displays_stale_frame() {
        let ring: [(AtomicBool, AtomicBool); 3] =
            std::array::from_fn(|_| (AtomicBool::new(false), AtomicBool::new(true)));
        let mut gpu_frame = [0u32; 3];

        // 12 decoded frames cycling through the 3-buffer ring (like Bink intro),
        // with frame 5 repeated once to exercise the clean-frame transition.
        let schedule = [
            (0, 1, true),
            (1, 2, true),
            (2, 3, true),
            (0, 4, true),
            (1, 5, true),
            (1, 5, false), // held frame: unchanged
            (2, 6, true),
            (0, 7, true),
            (1, 8, true), // immediately after held frame: must NOT be stale!
            (2, 9, true),
            (0, 10, true),
            (1, 11, true),
        ];

        for (slot, video_frame, changed) in schedule {
            let (uploaded, dirty) = &ring[slot];
            if changed {
                dirty.store(true, Release);
            }
            if claim_upload(uploaded, dirty).is_some() {
                gpu_frame[slot] = video_frame;
            }
            assert_eq!(
                gpu_frame[slot], video_frame,
                "slot {slot} displayed stale frame {} instead of {video_frame}",
                gpu_frame[slot]
            );
        }
    }
}

#[cfg(test)]
mod racefix_tests {
    use super::*;

    /// An alias copy's key is no texture's address and differs by source.
    #[test]
    fn alias_keys_stand_apart_from_textures() {
        let (placed, a, b) = (0x7ff6_1234_5670usize, 0x7ff6_2000_0000usize, 0x7ff6_3000_0000usize);
        let key = alias_key(placed, a);
        assert_ne!(key >> (usize::BITS - 1), 0);
        assert_ne!(key, placed);
        assert_ne!(key, alias_key(placed, b));
        assert_eq!(key, alias_key(placed, a));
    }

    /// A CPU upload of the view earlier in the list no longer keeps the
    /// alias copy out; a second ask in the list records nothing more.
    #[test]
    fn an_upload_earlier_in_the_list_does_not_hide_the_alias_copy() {
        let Some(host) = host() else { return };
        let (placed, source) = (0x7ff6_1234_5670usize, 0x7ff6_2000_0000usize);
        let list = host.new_list().unwrap();
        // (What `upload_from` asks first.)
        assert!(list.first_use(placed));
        assert!(!alias_copy_due_as(&list, placed, source, false), "as before: left out");
        assert!(alias_copy_due_as(&list, placed, source, true));
        assert!(!alias_copy_due_as(&list, placed, source, true));
        // The view stays marked used for the upload after it.
        assert!(!list.first_use(placed));
        list.close();
    }

    /// Levels a prelude could not record go back, and the texture is due.
    #[test]
    fn cached_levels_without_room_go_back() {
        use std::sync::atomic::{AtomicBool, Ordering::Acquire};
        let slot = Mutex::new(None);
        let dirty = AtomicBool::new(false);
        let bytes = Arc::new(vec![7u8; 16]);
        prefill_back(&slot, &dirty, (2, bytes.clone()));
        assert!(dirty.load(Acquire));
        assert_eq!(slot.lock().unwrap().as_ref().map(|(l, b)| (*l, b.len())), Some((2, 16)));
        // Levels put there since are kept.
        prefill_back(&slot, &dirty, (3, Arc::new(vec![1u8; 4])));
        assert_eq!(slot.lock().unwrap().as_ref().map(|(l, _)| *l), Some(2));
    }
}
