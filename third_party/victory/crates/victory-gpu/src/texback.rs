//! Textures shaders write that the title's own code then reads on the CPU.
//! On the console the CPU and the GPU share memory, so what a compute pass
//! stores into a texture is in title memory for the CPU to read; here it
//! stays in the texture's host copy. The title's ocean is one: its wave
//! passes write a 256x256 height map (R32_FLOAT) and a displacement map
//! (R32G32_FLOAT) that its CPU code samples, which is what boats float on.
//! Without the texels there the physics sees no water: boats sink.
//!
//! Off by default; `VICTORY_OCEAN_READBACK=1` switches it on. A list in which
//! shaders write a texture of a kind in `VICTORY_TEXTURE_BACK` (comma-
//! separated `<width>x<height>f<DXGI format>`, by default the kinds the
//! title was seen reading: `256x256f41,256x256f16,64x64f16,128x128f36`)
//! copies it into a read-back buffer of ours when it closes; once the list
//! has run ([`crate::host`]'s `read_backs`, which the relay and the late
//! settles already call for every list) the texels are tiled into the
//! texture's title memory in the console's layout, XOR swizzle modes
//! included (`victory_formats::texture::Layout::for_gpu`).
//!
//! Ported from Geckomacabre/vx-layer (commit eee5667, the copy-back), with
//! the XOR addressing from MiamiGame/XTP (commit 00c60fb), both GPL-3.0.
//!
//! shortcut: by kind, not by texture: textures of those kinds the title
//! never reads are copied back too (about a megabyte per such list). The
//! copy shows what the list's last write left; a title read in between sees
//! the frame before, as it would before the console's GPU has run.
//!
//! Each copy-back is tied to the allocation it was armed for ([`Ticket`],
//! from fxceless/victory-fork #999): if the title releases the texture, places
//! another texture at its memory, or the runtime remaps that memory before
//! the list has run, the texels are not written there.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use windows::Win32::Graphics::Direct3D12::*;

use crate::object::This;

/// Whether the copy-back is on (`VICTORY_OCEAN_READBACK=1`).
fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_OCEAN_READBACK").is_some_and(|v| v == "1"))
}

/// The kinds copied back: (width, height, format).
fn kinds() -> &'static [(u64, u32, u32)] {
    static KINDS: OnceLock<Vec<(u64, u32, u32)>> = OnceLock::new();
    KINDS.get_or_init(|| {
        if !on() {
            return Vec::new();
        }
        const DEFAULT: &str = "256x256f41,256x256f16,64x64f16,128x128f36";
        let list = std::env::var("VICTORY_TEXTURE_BACK").unwrap_or_else(|_| DEFAULT.to_owned());
        let kinds: Vec<(u64, u32, u32)> = list
            .split(',')
            .filter_map(|k| {
                let (size, format) = k.trim().split_once('f')?;
                let (w, h) = size.split_once('x')?;
                Some((w.parse().ok()?, h.parse().ok()?, format.parse().ok()?))
            })
            .collect();
        tracing::info!(?kinds, "textures of these kinds that shaders write are copied back into title memory (VICTORY_OCEAN_READBACK=1)");
        kinds
    })
}

/// The tile (swizzle) modes whose textures are copied back: `VICTORY_TEXTURE_BACK_MODES`, a
/// comma list of mode numbers, "0" (linear only) when unset.
fn modes_allowed() -> &'static [u32] {
    static MODES: std::sync::OnceLock<Vec<u32>> = std::sync::OnceLock::new();
    MODES.get_or_init(|| {
        std::env::var("VICTORY_TEXTURE_BACK_MODES")
            .unwrap_or_else(|_| "0".into())
            .split(',')
            .filter_map(|m| m.trim().parse().ok())
            .collect()
    })
}

/// Whether title texture `resource` is of a kind copied back.
pub(crate) fn wanted(resource: This) -> bool {
    let kinds = kinds();
    !kinds.is_empty() && crate::resource::texture_kind(resource).is_some_and(|k| kinds.contains(&k))
}

/// An outstanding copy-back into one allocation of title memory: changes of
/// mapping and of resources invalidate only the ones that could touch the
/// memory they changed.
struct Ticket {
    owner: usize,
    at: usize,
    end: usize,
    valid: AtomicBool,
}

static TICKETS: Mutex<Vec<Weak<Ticket>>> = Mutex::new(Vec::new());
static TRACKING: AtomicBool = AtomicBool::new(false);

fn track(owner: usize, at: usize, end: usize) -> Arc<Ticket> {
    let ticket = Arc::new(Ticket {
        owner,
        at,
        end,
        valid: AtomicBool::new(true),
    });
    let mut tickets = TICKETS.lock().unwrap_or_else(|e| e.into_inner());
    tickets.retain(|weak| weak.strong_count() != 0);
    tickets.push(Arc::downgrade(&ticket));
    TRACKING.store(true, Ordering::Relaxed);
    ticket
}

fn invalidate(matches: impl Fn(&Ticket) -> bool) {
    if !TRACKING.load(Ordering::Relaxed) {
        return;
    }
    TICKETS.lock().unwrap_or_else(|e| e.into_inner()).retain(|weak| {
        let Some(ticket) = weak.upgrade() else {
            return false;
        };
        if matches(&ticket) {
            ticket.valid.store(false, Ordering::Release);
        }
        true
    });
}

/// Title texture `owner` is being freed.
pub(crate) fn released(owner: usize) {
    invalidate(|ticket| ticket.owner == owner);
}

/// A texture was made at title memory `at`.
pub(crate) fn replaced(at: usize) {
    invalidate(|ticket| ticket.at == at);
}

/// The runtime mapped or unmapped `[at, at + len)`.
pub(crate) fn remapped(at: usize, len: usize) {
    let end = at.saturating_add(len);
    invalidate(|ticket| at < ticket.end && ticket.at < end);
}

/// A texture of a kind copied back that a list's shaders write, taken when
/// the draw or dispatch is prepared: its host copy, held apart from the
/// title's resource (which the title may release before the list closes),
/// its console layout and swizzle mode, and its allocation's ticket.
pub(crate) struct Target {
    pub(crate) copy: Arc<crate::resource::HostTexture>,
    layout: victory_formats::texture::Layout,
    mode: u32,
    ticket: Arc<Ticket>,
}

impl Target {
    pub(crate) fn new(resource: This) -> Option<Self> {
        let copy = crate::resource::host_copy(resource)?;
        let (layout, at, size, mode) = crate::resource::title_texture(resource)?;
        let end = at.checked_add(size).filter(|&end| at != 0 && end > at)?;
        let ticket = track(resource as usize, at, end);
        Some(Self {
            copy,
            layout,
            mode,
            ticket,
        })
    }

    /// The title texture it was taken for.
    pub(crate) fn owner(&self) -> usize {
        self.ticket.owner
    }
}

/// A list's shaders write title texture `owner`: its [`Target`] joins
/// `wanted` once. One already there whose allocation changed since (the
/// memory reused or remapped between two writes of the list) is taken
/// again in its place; kept, the copy-back would be dropped at the close.
pub(crate) fn want(wanted: &mut Vec<Target>, owner: usize, make: impl FnOnce() -> Option<Target>) {
    want_in(
        wanted,
        |t| t.owner() == owner,
        |t| t.ticket.valid.load(Ordering::Acquire),
        make,
    );
}

fn want_in<T>(
    wanted: &mut Vec<T>,
    same: impl Fn(&T) -> bool,
    fresh: impl Fn(&T) -> bool,
    make: impl FnOnce() -> Option<T>,
) {
    let at = wanted.iter().position(same);
    if at.is_some_and(|i| fresh(&wanted[i])) {
        return;
    }
    let Some(target) = make() else {
        return;
    };
    match at {
        Some(i) => wanted[i] = target,
        None => wanted.push(target),
    }
}

/// Pooled D3D12 read-back buffer reused across texture copy-backs so repeated
/// ocean/heightmap readbacks do not call `CreateCommittedResource` every frame.
struct ReadbackBuffer {
    resource: ID3D12Resource,
    capacity: u64,
}

// SAFETY: D3D12 resources are free-threaded; buffers are only returned to the
// pool when `Back` drops after its command list has completed on the GPU.
unsafe impl Send for ReadbackBuffer {}

const READBACK_POOL_MAX_BYTES: u64 = 16 << 20;
const READBACK_POOL_MAX_BUFFERS: usize = 32;

struct ReadbackPool {
    buffers: Vec<ReadbackBuffer>,
    bytes: u64,
}

static READBACK_POOL: Mutex<ReadbackPool> = Mutex::new(ReadbackPool {
    buffers: Vec::new(),
    bytes: 0,
});

#[cfg(test)]
thread_local! {
    static READBACK_ALLOCS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

fn take_readback_buffer(host: &crate::host::Host, total: u64) -> Option<ReadbackBuffer> {
    let max_fit = total.saturating_mul(2);
    {
        let mut pool = READBACK_POOL.lock().unwrap_or_else(|e| e.into_inner());
        let best = pool
            .buffers
            .iter()
            .enumerate()
            .filter(|(_, b)| b.capacity >= total && b.capacity <= max_fit)
            .min_by_key(|(_, b)| b.capacity)
            .map(|(i, _)| i);
        if let Some(i) = best {
            let buf = pool.buffers.swap_remove(i);
            pool.bytes = pool.bytes.saturating_sub(buf.capacity);
            return Some(buf);
        }
    }
    let resource = host.read_back_buffer(total)?;
    #[cfg(test)]
    READBACK_ALLOCS.with(|c| c.set(c.get() + 1));
    Some(ReadbackBuffer {
        resource,
        capacity: total,
    })
}

fn return_readback_buffer(buf: ReadbackBuffer) {
    if buf.capacity == 0 || buf.capacity > READBACK_POOL_MAX_BYTES / 2 {
        return;
    }
    let mut pool = READBACK_POOL.lock().unwrap_or_else(|e| e.into_inner());
    if pool.buffers.len() >= READBACK_POOL_MAX_BUFFERS
        || pool.bytes.saturating_add(buf.capacity) > READBACK_POOL_MAX_BYTES
    {
        return;
    }
    pool.bytes += buf.capacity;
    pool.buffers.push(buf);
}

/// A texture's copy into a read-back buffer, recorded into a list, and where
/// its texels go once the list has run.
pub(crate) struct Back {
    buffer: Option<ReadbackBuffer>,
    buffer_bytes: usize,
    footprints: Vec<D3D12_PLACED_SUBRESOURCE_FOOTPRINT>,
    rows: Vec<u32>,
    mips: u32,
    layout: victory_formats::texture::Layout,
    /// The allocation the texels go to (title memory, [`Ticket`]).
    ticket: Arc<Ticket>,
}

impl Back {
    /// The title memory the copy-back writes once its list has run.
    pub(crate) fn range(&self) -> (usize, usize) {
        (self.ticket.at, self.ticket.end)
    }
}

impl Drop for Back {
    fn drop(&mut self) {
        if let Some(buf) = self.buffer.take() {
            return_readback_buffer(buf);
        }
    }
}

/// Records into `list` the copy of `target`'s host copy (in the copy-source
/// state) into a read-back buffer; nothing if its allocation changed since.
pub(crate) fn record(list: &ID3D12GraphicsCommandList, target: Target) -> Option<Back> {
    if !target.ticket.valid.load(Ordering::Acquire) {
        return None;
    }
    let host = crate::host::host()?;
    let texture = &target.copy.texture;
    let (layout, mode) = (target.layout, target.mode);
    let (at, size) = (target.ticket.at, target.ticket.end - target.ticket.at);
    // SAFETY: a live texture.
    let desc = unsafe { texture.GetDesc() };
    if !layout.addressable() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            tracing::warn!(
                mode,
                format = desc.Format.0,
                "a texture to copy back has no addressing in its swizzle mode: not copied back"
            )
        });
        return None;
    }
    // Only the tile modes allowed (`VICTORY_TEXTURE_BACK_MODES`, default "0": linear): the
    // ocean's height maps are linear; copies in modes 2 and 27 turned the water to foam with
    // black patches (2026-11-25), likely the base-swizzle assumption of the XOR addressing.
    if !modes_allowed().contains(&mode) {
        return None;
    }
    arm_log(target.ticket.owner, at, &desc, size, mode);
    let mips = u32::from(desc.MipLevels.max(1));
    let subresources = mips * u32::from(desc.DepthOrArraySize.max(1));
    let n = subresources as usize;
    let mut footprints = vec![D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); n];
    let mut rows = vec![0u32; n];
    let mut total = 0u64;
    // SAFETY: out arrays of `n` entries.
    unsafe {
        host.device.GetCopyableFootprints(
            &desc,
            0,
            subresources,
            0,
            Some(footprints.as_mut_ptr()),
            Some(rows.as_mut_ptr()),
            None,
            Some(&mut total),
        )
    };
    // The last footprint's extent can leave out its row-end padding, and
    // the tiler reads whole pitched rows: the buffer must hold them.
    for (footprint, &rows) in footprints.iter().zip(&rows) {
        let f = footprint.Footprint;
        let pitched = u64::from(f.RowPitch)
            .checked_mul(u64::from(rows))?
            .checked_mul(u64::from(f.Depth))?;
        total = total.max(footprint.Offset.checked_add(pitched)?);
    }
    let buffer = take_readback_buffer(host, total)?;
    for (i, footprint) in footprints.iter().enumerate() {
        let to = D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: a borrowed pointer the location never releases.
            pResource: unsafe { std::mem::transmute_copy(&buffer.resource) },
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                PlacedFootprint: *footprint,
            },
        };
        let from = D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: as above.
            pResource: unsafe { std::mem::transmute_copy(texture) },
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                SubresourceIndex: i as u32,
            },
        };
        // SAFETY: an open list; the texture is in the copy-source state.
        unsafe { list.CopyTextureRegion(&to, 0, 0, 0, &from, None) };
    }
    Some(Back {
        buffer: Some(buffer),
        buffer_bytes: total as usize,
        footprints,
        rows,
        mips,
        layout,
        ticket: target.ticket,
    })
}

/// Logs the first copy of each texture: where it lies, its format and size,
/// and the tile mode its texels go in.
fn arm_log(resource: usize, at: usize, desc: &D3D12_RESOURCE_DESC, size: usize, mode: u32) {
    static SEEN: std::sync::Mutex<Vec<usize>> = std::sync::Mutex::new(Vec::new());
    let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    if seen.len() < 32 && !seen.contains(&at) {
        seen.push(at);
        tracing::info!(
            resource = format_args!("{resource:#x}"),
            address = format_args!("{at:#x}"),
            format = desc.Format.0,
            width = desc.Width,
            height = desc.Height,
            title_bytes = size,
            swizzle_mode = mode,
            "a texture shaders write is armed for copy-back into title memory (VICTORY_OCEAN_READBACK=1)"
        );
    }
}

/// Texture copy-backs landed in title memory since the last frame line.
pub(crate) static LANDED: AtomicU64 = AtomicU64::new(0);

/// Texture copy-backs replaced by a newer copy of the same allocation before
/// the background writer got to them.
static REPLACED: AtomicU64 = AtomicU64::new(0);

/// Whether texture read-backs are tiled and written into title memory on a
/// thread of their own (`VICTORY_TEXBACK_ASYNC`; on unless `0`).
///
/// Copying out of the D3D12 read-back buffer still runs on the landing thread
/// so the pooled buffer returns promptly; tiling and writing into title memory
/// move to [`writer`] so the landing thread never stalls on title memory
/// remaps or CPU tiling. When multiple copies of the same texture queue up,
/// only the newest is written.
fn asynchronous() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_TEXBACK_ASYNC").is_none_or(|v| v != "0"))
}

/// A read-back's linear texels copied out of its D3D12 buffer, ready to be
/// tiled into title memory.
struct Landing {
    footprints: Vec<D3D12_PLACED_SUBRESOURCE_FOOTPRINT>,
    rows: Vec<u32>,
    mips: u32,
    layout: victory_formats::texture::Layout,
    ticket: Arc<Ticket>,
    data: Vec<u8>,
}

/// Coalescing queue for [`writer`]: keeps only the newest landing per title
/// allocation address.
struct PendingMap<T> {
    waiting: std::collections::HashMap<usize, T>,
}

impl<T> PendingMap<T> {
    fn new() -> Self {
        Self {
            waiting: std::collections::HashMap::new(),
        }
    }

    /// Inserts `item` for title address `at`, returning `true` if it replaced
    /// an older unconsumed landing for the same allocation.
    fn push(&mut self, at: usize, item: T) -> bool {
        self.waiting.insert(at, item).is_some()
    }

    fn drain_all(&mut self) -> Vec<T> {
        self.waiting.drain().map(|(_, item)| item).collect()
    }

    fn is_empty(&self) -> bool {
        self.waiting.is_empty()
    }
}

/// Background thread that tiles and writes read-backs into title memory.
struct Writer {
    waiting: Mutex<PendingMap<Landing>>,
    more: std::sync::Condvar,
}

impl Writer {
    fn send(&self, landing: Landing) {
        let replaced = {
            let mut waiting = self.waiting.lock().unwrap_or_else(|e| e.into_inner());
            waiting.push(landing.ticket.at, landing)
        };
        if replaced {
            let n = REPLACED.fetch_add(1, Ordering::Relaxed) + 1;
            if n.is_power_of_two() {
                tracing::info!(
                    replaced = n,
                    landed = LANDED.load(Ordering::Relaxed),
                    "texture read-backs replaced by a newer copy before they were written (VICTORY_TEXBACK_ASYNC=0 writes each)"
                );
            }
        }
        self.more.notify_one();
    }
}

/// The background writer, if its thread could be started (if not, landings
/// are written inline on the landing thread).
fn writer() -> Option<&'static Writer> {
    static WRITER: OnceLock<Option<&'static Writer>> = OnceLock::new();
    *WRITER.get_or_init(|| {
        let writer: &'static Writer = Box::leak(Box::new(Writer {
            waiting: Mutex::new(PendingMap::new()),
            more: std::sync::Condvar::new(),
        }));
        let started = std::thread::Builder::new()
            .name("victory-texback".into())
            .spawn(move || {
                crate::topology::pin_background("victory-texback");
                loop {
                    let batch: Vec<Landing> = {
                        let mut waiting = writer.waiting.lock().unwrap_or_else(|e| e.into_inner());
                        while waiting.is_empty() {
                            waiting = writer.more.wait(waiting).unwrap_or_else(|e| e.into_inner());
                        }
                        waiting.drain_all()
                    };
                    for landing in &batch {
                        write(landing);
                    }
                }
            });
        match started {
            Ok(_) => {
                tracing::info!(
                    "texture read-backs are tiled and written on a thread of their own (VICTORY_TEXBACK_ASYNC=0: inline)"
                );
                Some(writer)
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "no thread for texture read-backs: they are written where they land"
                );
                None
            }
        }
    })
}

/// Once the list that recorded `back` has run: its texels, tiled into the
/// title's layout, into the texture's title memory (on [`writer`]'s thread
/// when [`asynchronous`]).
pub(crate) fn land(back: &Back) {
    let Some(buffer) = back.buffer.as_ref() else {
        return;
    };
    if stale_ticket(&back.ticket) {
        return;
    }
    let mut mapped = std::ptr::null_mut();
    // SAFETY: our read-back buffer, written by the list that has run; it was
    // created with at least `buffer_bytes` bytes.
    let data = unsafe {
        if buffer.resource.Map(0, None, Some(&mut mapped)).is_err() || mapped.is_null() {
            return;
        }
        let bytes = std::slice::from_raw_parts(mapped.cast::<u8>().cast_const(), back.buffer_bytes)
            .to_vec();
        buffer.resource.Unmap(0, None);
        bytes
    };
    let landing = Landing {
        footprints: back.footprints.clone(),
        rows: back.rows.clone(),
        mips: back.mips,
        layout: back.layout.clone(),
        ticket: Arc::clone(&back.ticket),
        data,
    };
    match asynchronous().then(writer).flatten() {
        Some(writer) => writer.send(landing),
        None => write(&landing),
    }
}

/// Tiles a read-back's linear texels into its title memory allocation.
fn write(back: &Landing) {
    if stale_ticket(&back.ticket) {
        return;
    }
    let (at, end) = (back.ticket.at, back.ticket.end);
    let size = end - at;
    let mut tiled = vec![0u8; size];
    // Bytes of title memory the subresources do not cover stay as they are.
    crate::host::copy_mapped(tiled.as_mut_ptr(), at as *const u8, size, false);
    let mut tiled_ok = true;
    for (i, (footprint, &rows)) in back.footprints.iter().zip(&back.rows).enumerate() {
        let (mip, slice) = (i as u32 % back.mips, i as u32 / back.mips);
        let f = footprint.Footprint;
        let pitch = f.RowPitch as usize;
        let len = pitch * rows as usize * f.Depth as usize;
        let off = footprint.Offset as usize;
        let Some(linear) = back.data.get(off..off.saturating_add(len)) else {
            tiled_ok = false;
            break;
        };
        if let Err(e) =
            back.layout
                .tile(&mut tiled, mip, slice, linear, pitch, pitch * rows as usize)
        {
            static ONCE: std::sync::Once = std::sync::Once::new();
            ONCE.call_once(|| {
                tracing::warn!(?e, mip, slice, "a texture copied back could not be tiled")
            });
            tiled_ok = false;
            break;
        }
    }
    // Looked at again just before the write: a change of the allocation
    // while the texels were tiled still keeps them out. (A remap between
    // this and the copy leaves an unmapped destination alone,
    // `crate::host::copy_mapped`.)
    if tiled_ok && !stale_ticket(&back.ticket) {
        crate::host::copy_mapped(at as *mut u8, tiled.as_ptr(), size, true);
        LANDED.fetch_add(1, Ordering::Relaxed);
    }
}

/// Whether `ticket`'s allocation changed since it was armed (counted, and
/// logged now and then).
fn stale_ticket(ticket: &Ticket) -> bool {
    static STALE: AtomicU64 = AtomicU64::new(0);
    if ticket.valid.load(Ordering::Acquire) {
        return false;
    }
    let skipped = STALE.fetch_add(1, Ordering::Relaxed) + 1;
    if skipped.is_power_of_two() {
        tracing::info!(
            skipped,
            "texture copy-backs skipped: their title allocation changed"
        );
    }
    true
}

#[cfg(test)]
mod tests;
