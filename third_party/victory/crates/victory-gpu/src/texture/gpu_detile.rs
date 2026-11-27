//! Title textures detiled on the card (`VICTORY_GPU_DETILE=1`; off unless
//! set).
//!
//! Without it a title texture's memory is detiled on the PC into its
//! staging buffer (`Staging::detile`), element run by element run, on the
//! submitting thread and on the prepare threads. With it the staging
//! buffer holds the texture's memory as the title laid it out (one plain
//! copy), followed by a mask of the levels that are the title's and the
//! layout's pattern tables, and a compute shader in the list's prelude
//! detiles it into a buffer of the card's own laid out as the host
//! texture's subresources, which the prelude's copies then read.
//!
//! What the PC still does per texture: the write watch, reading and
//! hashing the memory (change detection is as it was), and the plain copy.
//! What moves to the card: the address arithmetic of
//! `victory_formats::texture::Layout::detile` for every mode it covers
//! without pipe or bank XOR (linear, `S`, `D`, thin and thick), by the
//! layout's pattern tables, so no mode has code of its own. A texture that
//! cannot go this way is detiled on the PC as before, and the reason is
//! counted ([`line`]).
//!
//! When: the prelude records the detiling with the copies of the upload it
//! claims (`PreludeUpload::record`), at submission, so the card reads the
//! staging buffer at the moment the CPU path's copy read it.
//!
//! Exact: the shader moves the same bytes to the same places; the tests
//! compare it with the PC's detiling byte for byte, on the software
//! adapter and (ignored, run by hand under the `gpu` lock) on the card.
//!
//! Source: fxceless/victory-fork 3461f52faa (`gpudetile.rs`), ported to
//! our prelude uploads, partial stagings (from a first level) and pools.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use victory_formats::texture::Layout;
use windows::Win32::Graphics::Direct3D12::*;

/// The switch (`VICTORY_GPU_DETILE`; on only when `1`). Asked when a
/// texture's staging buffer is made: a texture keeps the way it was made.
pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| switch(std::env::var_os("VICTORY_GPU_DETILE").as_deref()))
}

fn switch(value: Option<&std::ffi::OsStr>) -> bool {
    value.is_some_and(|v| v == "1")
}

/// The root constants of one dispatch (see [`SOURCE`]'s cbuffer).
const CONSTANTS: usize = 22;

/// The shader. One thread writes one 32-bit word of a row of the linear
/// subresource: a word of an element of 4 bytes or more, or the 4 or 2
/// elements of 1 or 2 bytes the word holds. Root SRV: the staging buffer
/// (the tiled bytes, then the mask of held levels, then the pattern
/// tables); root UAV: the linear buffer.
const SOURCE: &str = r#"
#define RS "RootConstants(num32BitConstants=22, b0), SRV(t0), UAV(u0)"
cbuffer P : register(b0) {
    uint flags;       // bit 0: linear
    uint n;           // element bytes: 1, 2, 4, 8, 16
    uint width;       // elements
    uint height;
    uint depth;
    uint first;       // the array slice
    uint slab;        // depth slices a slice of the layout holds
    uint slice_size;
    uint mip_offset;
    uint pitch;       // the mip's padded width in elements
    uint bw;
    uint bh;
    uint log2;        // log2 of the block's bytes
    uint tx;          // the mip's place in the tail block
    uint ty;
    uint tables;      // byte offset of the x table (y, then z, follow)
    uint dst;         // byte offset of the subresource in the linear buffer
    uint row_pitch;
    uint depth_pitch;
    uint words;       // 32-bit words a row of the subresource holds
    uint mask_at;     // byte offset of the held levels' mask
    uint mip;
};
ByteAddressBuffer src : register(t0);
RWByteAddressBuffer out_buf : register(u0);

uint element(uint x, uint y, uint z) {
    uint s = (first + z) / slab;
    uint inner = (first + z) % slab;
    uint base = s * slice_size + mip_offset;
    if ((flags & 1u) != 0u) {
        return base + (y * pitch + x) * n;
    }
    x += tx;
    y += ty;
    uint row = ((y / bh) * (pitch / bw)) << log2;
    uint col = (x / bw) << log2;
    uint py = src.Load(tables + 4u * (bw + (y % bh)));
    uint pz = src.Load(tables + 4u * (bw + bh + inner));
    uint px = src.Load(tables + 4u * (x % bw));
    return base + row + col + (py | pz) + px;
}

[RootSignature(RS)]
[numthreads(64, 1, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    if (id.x >= words || id.y >= height || id.z >= depth) return;
    uint at = dst + id.z * depth_pitch + id.y * row_pitch + id.x * 4u;
    uint held = src.Load(mask_at);
    if (mip < 32u && ((held >> mip) & 1u) == 0u) {
        out_buf.Store(at, 0u);
        return;
    }
    uint v = 0u;
    if (n >= 4u) {
        uint per = n >> 2;
        v = src.Load(element(id.x / per, id.y, id.z) + 4u * (id.x % per));
    } else {
        uint per = 4u / n;
        uint keep = n == 1u ? 0xffu : 0xffffu;
        [unroll]
        for (uint k = 0u; k < 4u; k++) {
            uint x = id.x * per + k;
            if (k < per && x < width) {
                uint a = element(x, id.y, id.z);
                uint w = src.Load(a & ~3u);
                v |= ((w >> ((a & 3u) * 8u)) & keep) << (k * n * 8u);
            }
        }
    }
    out_buf.Store(at, v);
}
"#;

/// The shader's root signature and pipeline.
pub(crate) struct Detiler {
    root: ID3D12RootSignature,
    pso: ID3D12PipelineState,
}

// SAFETY: immutable D3D12 objects, which are free-threaded.
unsafe impl Send for Detiler {}
// SAFETY: as above.
unsafe impl Sync for Detiler {}

/// Compiles [`SOURCE`] (`cs_5_1`, the root signature inside it) on `device`.
fn make(device: &ID3D12Device) -> windows::core::Result<Detiler> {
    use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
    use windows::Win32::Graphics::Direct3D::ID3DBlob;
    let mut code: Option<ID3DBlob> = None;
    let mut errors: Option<ID3DBlob> = None;
    // SAFETY: the source and names live through the call; the out
    // pointers are ours.
    let compiled = unsafe {
        D3DCompile(
            SOURCE.as_ptr().cast(),
            SOURCE.len(),
            windows::core::s!("texture detile"),
            None,
            None,
            windows::core::s!("main"),
            windows::core::s!("cs_5_1"),
            0,
            0,
            &mut code,
            Some(&mut errors),
        )
    };
    if let Err(e) = compiled {
        if let Some(errors) = errors {
            // SAFETY: the blob's own bytes.
            let text = unsafe {
                std::slice::from_raw_parts(
                    errors.GetBufferPointer().cast::<u8>(),
                    errors.GetBufferSize(),
                )
            };
            tracing::warn!(
                "the texture detile shader: {}",
                String::from_utf8_lossy(text)
            );
        }
        return Err(e);
    }
    let code = code.ok_or_else(windows::core::Error::empty)?;
    // SAFETY: the blob's own bytes; the root signature is inside the shader.
    unsafe {
        let bytes =
            std::slice::from_raw_parts(code.GetBufferPointer().cast::<u8>(), code.GetBufferSize());
        let root: ID3D12RootSignature = device.CreateRootSignature(0, bytes)?;
        let pso: ID3D12PipelineState =
            device.CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                pRootSignature: std::mem::transmute_copy(&root),
                CS: D3D12_SHADER_BYTECODE {
                    pShaderBytecode: bytes.as_ptr().cast(),
                    BytecodeLength: bytes.len(),
                },
                ..Default::default()
            })?;
        Ok(Detiler { root, pso })
    }
}

/// The host's detiler, made at the first ask; `None` (said once) if the
/// shader could not be made: textures are then detiled on the PC.
fn detiler() -> Option<&'static Detiler> {
    static MADE: OnceLock<Option<Detiler>> = OnceLock::new();
    MADE.get_or_init(|| {
        let host = crate::host::host()?;
        match make(&host.device) {
            Ok(d) => {
                tracing::info!("title textures are detiled on the card (VICTORY_GPU_DETILE=1)");
                Some(d)
            }
            Err(e) => {
                tracing::warn!(error = %e, "VICTORY_GPU_DETILE: the detile shader could not be made; textures are detiled on the PC");
                None
            }
        }
    })
    .as_ref()
}

/// Where things lie in a staging buffer on the card's way: the tiled bytes
/// at 0, the held levels' mask, the pattern tables, and the buffer's bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Raw {
    mask_at: u64,
    tables_at: u64,
    pub(crate) bytes: u64,
}

/// [`Raw`] for `size` bytes of `layout`'s memory (the staged part).
fn raw_layout(layout: &Layout, size: u64) -> Raw {
    let mask_at = size.next_multiple_of(16);
    let tables_at = mask_at + 16;
    let words: usize = layout.pattern_tables().iter().map(|t| t.len()).sum();
    Raw {
        mask_at,
        tables_at,
        bytes: tables_at + 4 * words as u64,
    }
}

/// Writes the pattern tables of `layout` and an all-held mask.
///
/// # Safety
/// `data` is the mapping of a buffer of at least `raw.bytes` bytes that
/// nothing else writes now.
unsafe fn write_tables(layout: &Layout, raw: &Raw, data: *mut u8) {
    let mut at = raw.tables_at as usize;
    // SAFETY: inside the buffer, by the caller's word.
    unsafe {
        write_mask(raw, data, u32::MAX);
        for table in layout.pattern_tables() {
            for &v in table {
                data.add(at).cast::<u32>().write_unaligned(v);
                at += 4;
            }
        }
    }
}

/// Writes the held levels' mask (a bit a level; levels past 31 are held).
///
/// # Safety
/// As [`write_tables`].
unsafe fn write_mask(raw: &Raw, data: *mut u8, held: u32) {
    // SAFETY: inside the buffer, by the caller's word.
    unsafe {
        data.add(raw.mask_at as usize)
            .cast::<u32>()
            .write_unaligned(held)
    };
}

/// One subresource's dispatch: its constants and its groups.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Job {
    constants: [u32; CONSTANTS],
    groups: [u32; 3],
}

/// Why a texture is detiled on the PC with the switch on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fallback {
    /// The shader could not be made.
    NoShader = 0,
    /// A mode the shader does not cover (XOR), or a layout over 2 GiB.
    Mode = 1,
    /// A subresource the host lays out otherwise than the layout (rows
    /// wider than their pitch, more rows), or past 4 GiB of linear bytes.
    Footprint = 2,
    /// No buffer of the card's could be had for the linear bytes.
    NoBuffer = 3,
}

/// The dispatches that detile every part of a staging buffer laid out as
/// `footprints` (and `rows`), part `i` being (mip, slice) `part(i)`.
fn jobs(
    layout: &Layout,
    footprints: &[D3D12_PLACED_SUBRESOURCE_FOOTPRINT],
    rows: &[u32],
    part: impl Fn(usize) -> (u32, u32),
    raw: &Raw,
) -> Result<Vec<Job>, Fallback> {
    let tables_at = u32::try_from(raw.tables_at).map_err(|_| Fallback::Mode)?;
    let mask_at = u32::try_from(raw.mask_at).map_err(|_| Fallback::Mode)?;
    let mut out = Vec::with_capacity(footprints.len());
    for (i, (footprint, &row_count)) in footprints.iter().zip(rows).enumerate() {
        let (mip, slice) = part(i);
        let p = layout.detile_params(mip, slice).ok_or(Fallback::Mode)?;
        let f = footprint.Footprint;
        let bytes = u64::from(p.width) * u64::from(p.element_bytes);
        let depth_pitch = u64::from(f.RowPitch) * u64::from(row_count);
        let end = footprint.Offset + depth_pitch * u64::from(f.Depth);
        if bytes > u64::from(f.RowPitch)
            || p.height > row_count
            || p.depth > f.Depth
            || end > u64::from(u32::MAX)
            || p.height > 65535
            || p.depth > 65535
        {
            return Err(Fallback::Footprint);
        }
        let words = bytes.div_ceil(4) as u32;
        out.push(Job {
            constants: [
                u32::from(p.linear),
                p.element_bytes,
                p.width,
                p.height,
                p.depth,
                p.first,
                p.slab,
                p.slice_size,
                p.mip_offset,
                p.pitch,
                p.block_w,
                p.block_h,
                p.block_log2,
                p.tail_x,
                p.tail_y,
                tables_at,
                footprint.Offset as u32,
                f.RowPitch,
                depth_pitch as u32,
                words,
                mask_at,
                mip,
            ],
            groups: [words.div_ceil(64), p.height, p.depth],
        });
    }
    Ok(out)
}

/// A staging buffer's detiling on the card.
pub(crate) struct Gpu {
    /// The card's buffer the shader writes and the copies read, laid out
    /// as the staging buffer's footprints; in the common state between
    /// preludes.
    linear: ID3D12Resource,
    /// Its pool size (0: not pooled).
    pooled: u64,
    pub(crate) raw: Raw,
    jobs: Vec<Job>,
}

impl Drop for Gpu {
    fn drop(&mut self) {
        if self.pooled == 0 {
            return;
        }
        let mut spare = SPARE.lock().unwrap_or_else(|e| e.into_inner());
        if spare.1 + self.pooled <= SPARE_MOST {
            spare.1 += self.pooled;
            spare
                .0
                .entry(self.pooled)
                .or_default()
                .push(Linear(self.linear.clone()));
        }
    }
}

/// The card's buffers of [`Gpu::linear`] nothing holds, by size, and their
/// bytes: making one is a call into the driver, which resource creation
/// serialises (measured on the 5090 in the streaming window).
static SPARE: std::sync::Mutex<(std::collections::BTreeMap<u64, Vec<Linear>>, u64)> =
    std::sync::Mutex::new((std::collections::BTreeMap::new(), 0));
const SPARE_MOST: u64 = 512 << 20;

struct Linear(ID3D12Resource);
// SAFETY: a buffer of the card's only lists name.
unsafe impl Send for Linear {}

/// A buffer of the card's of at least `total` bytes shaders may write, in
/// the common state, and its pool size (0: not pooled). What it holds is
/// what the last texture left: the shader writes every row's own bytes.
fn linear_buffer(total: u64) -> Option<(ID3D12Resource, u64)> {
    let size = total.max(64 << 10).next_power_of_two();
    let host = crate::host::host()?;
    if size > SPARE_MOST {
        return host.uav_buffer(total).map(|b| (b, 0));
    }
    let taken = {
        let mut spare = SPARE.lock().unwrap_or_else(|e| e.into_inner());
        let taken = spare.0.get_mut(&size).and_then(Vec::pop);
        if taken.is_some() {
            spare.1 -= size;
        }
        taken
    };
    match taken {
        Some(Linear(buffer)) => Some((buffer, size)),
        None => host.uav_buffer(size).map(|b| (b, size)),
    }
}

/// With the switch on: the detiling on the card of a staging buffer of
/// `size` bytes of `layout`'s memory laid out as `footprints` (part `i`
/// being `part(i)`), and the bytes its staging buffer takes then; `None`
/// (its reason counted) for one detiled on the PC.
pub(crate) fn plan(
    layout: &Layout,
    size: u64,
    footprints: &[D3D12_PLACED_SUBRESOURCE_FOOTPRINT],
    rows: &[u32],
    part: impl Fn(usize) -> (u32, u32),
    total: u64,
) -> Option<Gpu> {
    if !on() {
        return None;
    }
    let fallback = |why: Fallback| {
        FALLBACKS[why as usize].fetch_add(1, Relaxed);
        None
    };
    if detiler().is_none() {
        return fallback(Fallback::NoShader);
    }
    let raw = raw_layout(layout, size);
    let jobs = match jobs(layout, footprints, rows, part, &raw) {
        Ok(jobs) => jobs,
        Err(why) => return fallback(why),
    };
    let Some((linear, pooled)) = linear_buffer(total) else {
        return fallback(Fallback::NoBuffer);
    };
    STATS[CARD_MADE].fetch_add(1, Relaxed);
    Some(Gpu {
        linear,
        pooled,
        raw,
        jobs,
    })
}

/// A new staging buffer's mapping `data` (of `gpu.raw.bytes`) gets the
/// tables and an all-held mask.
///
/// # Safety
/// As [`write_tables`].
pub(crate) unsafe fn prepare(gpu: &Gpu, layout: &Layout, data: *mut u8) {
    // SAFETY: by the caller's word.
    unsafe { write_tables(layout, &gpu.raw, data) };
}

/// `Staging::detile` on the card's way: the title's bytes as they lie, and
/// which levels are the title's (`held`); the prelude's shader does the rest.
///
/// # Safety
/// `data` is the staging buffer's mapping, of `gpu.raw.bytes`, that only
/// the caller writes now.
pub(crate) unsafe fn stage(gpu: &Gpu, data: *mut u8, tiled: &[u8], held: u32) {
    let started = std::time::Instant::now();
    let size = tiled.len().min(gpu.raw.mask_at as usize);
    // SAFETY: inside the mapping, by the caller's word.
    unsafe {
        std::ptr::copy_nonoverlapping(tiled.as_ptr(), data, size);
        write_mask(&gpu.raw, data, held);
    }
    STATS[RAW_BYTES].fetch_add(size as u64, Relaxed);
    STATS[RAW_US].fetch_add(started.elapsed().as_micros() as u64, Relaxed);
    line();
}

/// Records in the prelude `list` the detiling of the staging buffer
/// (`raw`, from byte `base`) into `gpu.linear`, which is left in the copy
/// source state for the copies; [`copied`] brings it back after them.
/// Leaves the list's compute root signature and pipeline the detiler's.
pub(crate) fn record(list: &ID3D12GraphicsCommandList, raw: &ID3D12Resource, base: u64, gpu: &Gpu) {
    let Some(d) = detiler() else {
        return;
    };
    record_with(list, d, raw, base, gpu);
    STATS[CARD_TEXTURES].fetch_add(1, Relaxed);
    STATS[CARD_DISPATCHES].fetch_add(gpu.jobs.len() as u64, Relaxed);
}

fn record_with(
    list: &ID3D12GraphicsCommandList,
    d: &Detiler,
    raw: &ID3D12Resource,
    base: u64,
    gpu: &Gpu,
) {
    use crate::host::transition;
    // SAFETY: an open list; live buffers the caller keeps until the list
    // has run; the detiler's own root signature.
    unsafe {
        list.ResourceBarrier(&[transition(
            &gpu.linear,
            D3D12_RESOURCE_STATE_COMMON,
            D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
        )]);
        list.SetComputeRootSignature(&d.root);
        list.SetPipelineState(&d.pso);
        list.SetComputeRootShaderResourceView(1, raw.GetGPUVirtualAddress() + base);
        list.SetComputeRootUnorderedAccessView(2, gpu.linear.GetGPUVirtualAddress());
        for job in &gpu.jobs {
            list.SetComputeRoot32BitConstants(
                0,
                CONSTANTS as u32,
                job.constants.as_ptr().cast(),
                0,
            );
            list.Dispatch(job.groups[0], job.groups[1], job.groups[2]);
        }
        list.ResourceBarrier(&[transition(
            &gpu.linear,
            D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
            D3D12_RESOURCE_STATE_COPY_SOURCE,
        )]);
    }
}

/// After the prelude's copies from `gpu.linear`: back to the common state,
/// which the next detiling starts from.
pub(crate) fn copied(list: &ID3D12GraphicsCommandList, gpu: &Gpu) {
    // SAFETY: an open list and a live buffer.
    unsafe {
        list.ResourceBarrier(&[crate::host::transition(
            &gpu.linear,
            D3D12_RESOURCE_STATE_COPY_SOURCE,
            D3D12_RESOURCE_STATE_COMMON,
        )])
    };
}

/// The buffer the prelude copies from, and the offset of the footprints in it.
pub(crate) fn copy_source(gpu: &Gpu) -> &ID3D12Resource {
    &gpu.linear
}

const CARD_TEXTURES: usize = 0;
const CARD_DISPATCHES: usize = 1;
const RAW_BYTES: usize = 2;
const RAW_US: usize = 3;
const CARD_MADE: usize = 4;
const CPU_TEXTURES: usize = 5;
const CPU_US: usize = 6;
/// Since the last line: textures detiled on the card, their dispatches,
/// the bytes the PC copied as laid out for them and the microseconds that
/// took, staging buffers made for the card's way; textures detiled on the
/// PC with the switch on, and the microseconds that took.
static STATS: [AtomicU64; 7] = [const { AtomicU64::new(0) }; 7];
/// Since the last line: staging buffers made for the PC's way with the
/// switch on, by [`Fallback`].
static FALLBACKS: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

/// With the switch on: `Staging::detile` on the PC took `us`.
pub(crate) fn detiled_on_pc(us: u64) {
    STATS[CPU_TEXTURES].fetch_add(1, Relaxed);
    STATS[CPU_US].fetch_add(us, Relaxed);
    line();
}

/// About every five seconds while the switch is on: the counts per frame.
fn line() {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    static FRAME: AtomicU64 = AtomicU64::new(0);
    let now = crate::host::now_ms();
    let next = NEXT.load(Relaxed);
    if now < next
        || NEXT
            .compare_exchange(next, now + 5000, Relaxed, Relaxed)
            .is_err()
    {
        return;
    }
    let frame = crate::frame::current();
    let frames = frame.saturating_sub(FRAME.swap(frame, Relaxed)).max(1) as f64;
    let s = STATS.each_ref().map(|c| c.swap(0, Relaxed) as f64 / frames);
    let f = FALLBACKS
        .each_ref()
        .map(|c| c.swap(0, Relaxed) as f64 / frames);
    tracing::info!(
        "texture detiling per frame (VICTORY_GPU_DETILE=1): on the card {:.1} textures in {:.0} dispatches, \
         the PC copying {:.2} MB as laid out in {:.2} ms, {:.1} staging buffers made for it; on the PC {:.1} \
         textures in {:.2} ms; staging buffers made for the PC's way: {:.2} without the shader, {:.2} in a mode \
         it does not cover (XOR) or over 2 GiB, {:.2} laid out otherwise by the host, {:.2} without a card buffer",
        s[CARD_TEXTURES],
        s[CARD_DISPATCHES],
        s[RAW_BYTES] / 1e6,
        s[RAW_US] / 1e3,
        s[CARD_MADE],
        s[CPU_TEXTURES],
        s[CPU_US] / 1e3,
        f[Fallback::NoShader as usize],
        f[Fallback::Mode as usize],
        f[Fallback::Footprint as usize],
        f[Fallback::NoBuffer as usize],
    );
}

#[cfg(test)]
mod tests;
