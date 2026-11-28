//! `VICTORY_NATIVE_BVH=1` (LNX, off by default, with `VICTORY_BVH_HOOK=1`):
//! beside the console builder, the same ray-tracing structures are built
//! with the host's own DXR, from the same inputs (the title's standard
//! D3D12 descriptions). Nothing reads them yet: this is the step that makes
//! them and checks they build (sizes, counts); the translated shaders keep
//! reading the console's structures.
//!
//! Each structure is kept by its title address (the description's
//! destination): bottom levels from the title's triangles (its index,
//! vertex and transform buffers named through their host copies), top
//! levels from its instances, read from title memory with each instance's
//! bottom level replaced by the host one built for that title address.
//! A bottom level built again from the same inputs is kept as it is, and
//! structures neither built nor named for [`IDLE_FRAMES`] are let go.
//!
//! Ours on top of the port: a bottom level with a geometry DXR would not
//! take (a position or index format outside the device's tier, partial
//! triangles) is not built, since an invalid build can remove the device;
//! and the title's clones and compactions name the host structure of their
//! source, so top levels that point at the compacted copy still find one.
//! The title's meshlet indices and quantized positions (most of its
//! geometry) are decoded first ([`packed`]).
//!
//! Source: Linox-IDK/LNX-victory 012fbf0526, acbd3e6d96 (GPL-3.0-or-later).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_SAMPLE_DESC};
use windows::core::Interface;

use crate::object::Object;

mod batch;
mod gpu_instances;
mod packed;
mod refit;
#[cfg(test)]
mod refit_bench;

/// Whether host structures are built (`VICTORY_NATIVE_BVH=1`).
pub fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_NATIVE_BVH").is_some_and(|v| v == "1"))
}

/// One triangle geometry, as the title describes it (title addresses).
#[derive(Debug, Clone, Copy)]
pub struct Triangles {
    pub opaque: bool,
    pub transform: u64,
    pub index_format: u32,
    pub vertex_format: u32,
    pub index_count: u32,
    pub vertex_count: u32,
    pub index_buffer: u64,
    pub vertex_buffer: u64,
    pub stride: u64,
}

/// One build the title asked for (title addresses).
#[derive(Debug, Clone)]
pub enum Build {
    Bottom {
        dest: u64,
        /// An update's source (0: none named).
        source: u64,
        flags: u32,
        geometries: Vec<Triangles>,
    },
    Top {
        dest: u64,
        /// An update's source (0: none named).
        source: u64,
        flags: u32,
        count: u32,
        instances: u64,
    },
}

/// A host structure and its size.
struct Structure {
    buffer: ID3D12Resource,
    size: u64,
    /// What it was built from (bottom levels; 0 for others).
    sig: u64,
    /// The last frame it was built or named by a top level.
    used: u64,
    /// A top level whose build was recorded into this buffer: what
    /// [`top_level`] hands to rays (a buffer made but not built is not).
    top_built: bool,
    /// A build or refit was recorded into it, with `ALLOW_UPDATE`
    /// (`updatable`) and of this shape ([`refit`]).
    built: bool,
    updatable: bool,
    shape: u64,
}

/// Structures neither built nor named by a top level for this many frames
/// are let go (the title has reused or freed their memory).
const IDLE_FRAMES: u64 = 300;

/// `D3D12_RAYTRACING_INSTANCE_DESC`: 64 bytes, the instance id (low 24
/// bits) and mask (high 8) in the dword at 48, the bottom level's address
/// at 56.
const INSTANCE_SIZE: usize = 64;
const INSTANCE_MASK_AT: usize = 48;
const INSTANCE_BLAS_AT: usize = 56;

/// `D3D12_RAYTRACING_ACCELERATION_STRUCTURE_BUILD_FLAG_ALLOW_UPDATE`.
const ALLOW_UPDATE: u32 = 1;

/// The host structures by title address, and the shared scratch buffer.
struct Store {
    by_title: HashMap<u64, Structure>,
    scratch: Option<Structure>,
}

static STORE: Mutex<Option<Store>> = Mutex::new(None);

/// Counts since the last summary: built bottom/top levels, skipped ones
/// (an input without a host copy, an unknown bottom level), failures.
static BUILT_BOTTOM: AtomicU64 = AtomicU64::new(0);
static BUILT_TOP: AtomicU64 = AtomicU64::new(0);
static SKIPPED: AtomicU64 = AtomicU64::new(0);
static MISSING_BLAS: AtomicU64 = AtomicU64::new(0);
static BYTES: AtomicU64 = AtomicU64::new(0);
static REUSED: AtomicU64 = AtomicU64::new(0);
static FREED: AtomicU64 = AtomicU64::new(0);
static ALIASED: AtomicU64 = AtomicU64::new(0);
/// Bottom levels with a geometry DXR would not take (format, counts).
static REFUSED: AtomicU64 = AtomicU64::new(0);
/// Builds the title asked as updates; of them refit on the host, and
/// built from scratch though `VICTORY_NATIVE_BVH_REFIT=1` ([`refit`]).
static UPDATES_ASKED: AtomicU64 = AtomicU64::new(0);
static REFIT: AtomicU64 = AtomicU64::new(0);
static REFIT_REFUSED: AtomicU64 = AtomicU64::new(0);
/// Barriers recorded after or between builds (one per build without
/// `VICTORY_NATIVE_BVH_BATCH=1`, [`batch`]).
static BARRIERS: AtomicU64 = AtomicU64::new(0);
/// Top levels whose instances were made on the card ([`gpu_instances`]).
static GPU_INSTANCES: AtomicU64 = AtomicU64::new(0);
/// Geometries whose meshlet indices or quantized positions were decoded,
/// and bottom levels refused for one that would not decode ([`packed`]).
static PACKED_INDICES: AtomicU64 = AtomicU64::new(0);
static PACKED_POSITIONS: AtomicU64 = AtomicU64::new(0);
static PACKED_REFUSED: AtomicU64 = AtomicU64::new(0);

/// What a bottom level is built from: its flags and each geometry's
/// description (addresses, counts, formats). Equal: the same structure, unless
/// the title rebuilds it because the vertices moved (it then allows updates).
fn signature(flags: u32, geometries: &[Triangles]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    flags.hash(&mut h);
    for g in geometries {
        (
            g.opaque,
            g.transform,
            g.index_format,
            g.vertex_format,
            g.index_count,
            g.vertex_count,
            g.index_buffer,
            g.vertex_buffer,
            g.stride,
        )
            .hash(&mut h);
    }
    h.finish() | 1
}

/// Bytes per index DXR is given for a DXGI index format (0: no index
/// buffer). The title's meshlet indices are decoded to `R16_UINT` first.
fn index_size(format: u32) -> u64 {
    match format {
        57 | packed::MESHLET_INDICES => 2, // R16_UINT
        42 => 4,                           // R32_UINT
        _ => 0,
    }
}

/// Whether DXR takes `format` as a vertex position format: the tier 1.0
/// formats, and with `tier_1_1` the ones tier 1.1 adds. A build naming any
/// other is invalid and can remove the device, so such a geometry is skipped.
fn vertex_format_ok(format: u32, tier_1_1: bool) -> bool {
    match format {
        // R32G32B32_FLOAT, R16G16B16A16_FLOAT, R16G16B16A16_SNORM,
        // R32G32_FLOAT, R16G16_FLOAT, R16G16_SNORM.
        6 | 10 | 13 | 16 | 34 | 37 => true,
        // R16G16B16A16_UNORM, R10G10B10A2_UNORM, R8G8B8A8_UNORM,
        // R8G8B8A8_SNORM, R16G16_UNORM, R8G8_UNORM, R8G8_SNORM.
        11 | 24 | 28 | 31 | 35 | 49 | 51 => tier_1_1,
        _ => false,
    }
}

/// Whether a triangle geometry can be given to the host's DXR as it is:
/// a known index format with an index buffer (none without), a position
/// format the device takes, and whole triangles.
fn geometry_ok(g: &Triangles, tier_1_1: bool) -> bool {
    let indices_ok = if g.index_buffer == 0 {
        g.vertex_count % 3 == 0
    } else {
        index_size(g.index_format) != 0 && g.index_count % 3 == 0
    };
    indices_ok && vertex_format_ok(g.vertex_format, tier_1_1) && packed::stride(g.stride) != 0
}

/// The host device's ray-tracing tier: 10 (1.0), 11 (1.1) or more; 0 none.
fn tier(device: &ID3D12Device) -> i32 {
    static TIER: OnceLock<i32> = OnceLock::new();
    *TIER.get_or_init(|| {
        let mut o = D3D12_FEATURE_DATA_D3D12_OPTIONS5::default();
        // SAFETY: the structure the feature names, with its size.
        let asked = unsafe {
            device.CheckFeatureSupport(
                D3D12_FEATURE_D3D12_OPTIONS5,
                (&raw mut o).cast(),
                size_of::<D3D12_FEATURE_DATA_D3D12_OPTIONS5>() as u32,
            )
        };
        let t = if asked.is_ok() { o.RaytracingTier.0 } else { 0 };
        tracing::info!(tier = t, "native bvh: host ray-tracing tier");
        t
    })
}

/// `D3D12_RAYTRACING_ACCELERATION_STRUCTURE_COPY_MODE`: clone (0) and
/// compact (1) make a structure that answers the same rays as its source.
fn copy_keeps_rays(mode: u32) -> bool {
    mode <= 1
}

/// The title copied the structure at `source` to `dest` (a clone or a
/// compaction): top levels naming `dest` get the host structure of `source`.
pub fn copy(dest: u64, source: u64, mode: u32) {
    if !on() || !copy_keeps_rays(mode) || dest == source {
        return;
    }
    let mut store = STORE.lock().unwrap_or_else(|e| e.into_inner());
    let Some(store) = store.as_mut() else { return };
    let Some(src) = store.by_title.get(&source) else {
        SKIPPED.fetch_add(1, Ordering::Relaxed);
        return;
    };
    let alias = Structure {
        buffer: src.buffer.clone(),
        // Not counted again in the live bytes: the same buffer.
        size: 0,
        sig: src.sig,
        used: crate::queue::presented(),
        top_built: src.top_built,
        built: src.built,
        updatable: src.updatable,
        shape: src.shape,
    };
    store.by_title.insert(dest, alias);
    ALIASED.fetch_add(1, Ordering::Relaxed);
}

/// The instances read from title memory with each bottom level's title
/// address replaced by `host_va` of it; an instance without one is masked
/// out (mask 0). Returns how many had none.
fn retarget_instances(raw: &mut [u8], mut host_va: impl FnMut(u64) -> u64) -> u64 {
    let mut missing = 0;
    for inst in raw.chunks_exact_mut(INSTANCE_SIZE) {
        let at = INSTANCE_BLAS_AT;
        let title = u64::from_le_bytes(inst[at..at + 8].try_into().unwrap_or_default());
        let va = host_va(title);
        if va == 0 {
            missing += 1;
            inst[INSTANCE_MASK_AT..INSTANCE_MASK_AT + 4].fill(0);
        }
        inst[at..at + 8].copy_from_slice(&va.to_le_bytes());
    }
    missing
}

/// A host GPU address for the title address `address` with `len` bytes
/// behind it, its host copy brought up to date before the list runs.
fn host_address(list: &crate::host::HostList, address: u64, len: u64) -> Option<u64> {
    if address == 0 {
        return Some(0);
    }
    let shadow = crate::shadow::find_any(address, len.max(1))?;
    shadow.refresh(list, address, len.max(1));
    Some(shadow.gpu(address))
}

fn buffer(
    device: &ID3D12Device,
    size: u64,
    state: D3D12_RESOURCE_STATES,
) -> Option<ID3D12Resource> {
    let heap = D3D12_HEAP_PROPERTIES {
        Type: D3D12_HEAP_TYPE_DEFAULT,
        ..Default::default()
    };
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: size.max(256),
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
        ..Default::default()
    };
    let mut out: Option<ID3D12Resource> = None;
    // SAFETY: plain creation of a buffer.
    let made = unsafe {
        device.CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &desc, state, None, &mut out)
    };
    if let Err(e) = made {
        tracing::warn!(error = %e, size, "native bvh: no buffer");
        return None;
    }
    out
}

/// The title's flags without an update (built from scratch), or with one
/// for a host refit ([`refit::host_flags`]).
fn flags(title: u32, refit: bool) -> D3D12_RAYTRACING_ACCELERATION_STRUCTURE_BUILD_FLAGS {
    D3D12_RAYTRACING_ACCELERATION_STRUCTURE_BUILD_FLAGS(refit::host_flags(title, refit) as i32)
}

/// Builds `builds` into the host list behind the title list `list`.
pub fn build(list: *const Object, builds: &[Build]) {
    if !on() || builds.is_empty() {
        return;
    }
    let Some(device) = crate::host::host().map(|h| h.device.clone()) else {
        return;
    };
    let Ok(device5) = device.cast::<ID3D12Device5>() else {
        static SAID: std::sync::Once = std::sync::Once::new();
        SAID.call_once(|| tracing::warn!("native bvh: the host device has no DXR (ID3D12Device5)"));
        return;
    };
    let tier = tier(&device);
    if tier < 10 {
        static SAID: std::sync::Once = std::sync::Once::new();
        SAID.call_once(|| tracing::warn!("native bvh: the host device has no ray tracing"));
        return;
    }
    crate::list::with_state(list, |host, state| {
        let Ok(l4) = host.list.cast::<ID3D12GraphicsCommandList4>() else {
            return;
        };
        let mut store = STORE.lock().unwrap_or_else(|e| e.into_inner());
        let store = store.get_or_insert_with(|| Store {
            by_title: HashMap::new(),
            scratch: None,
        });
        // Once a frame: let go of structures the title stopped using.
        let frame = crate::queue::presented();
        static SWEPT: AtomicU64 = AtomicU64::new(u64::MAX);
        if SWEPT.swap(frame, Ordering::Relaxed) != frame {
            store.by_title.retain(|_, st| {
                let keep = frame.saturating_sub(st.used) < IDLE_FRAMES;
                if !keep {
                    // The list may still read it: it holds it until done.
                    host.keep(&st.buffer);
                    sub_bytes(st.size);
                    FREED.fetch_add(1, Ordering::Relaxed);
                }
                keep
            });
        }
        let mut batch = batch::on().then(batch::Batch::default);
        for b in builds {
            build_one(&device, &device5, &l4, host, store, b, tier >= 11, batch.as_mut());
        }
        if batch.as_ref().is_some_and(batch::Batch::pending) {
            // The structures are ready before the list reads them.
            barrier_all(&l4);
        }
        state.replay(host);
    });
    summary();
}

fn sub_bytes(size: u64) {
    BYTES.fetch_sub(size.min(BYTES.load(Ordering::Relaxed)), Ordering::Relaxed);
}

fn build_one(
    device: &ID3D12Device,
    device5: &ID3D12Device5,
    l4: &ID3D12GraphicsCommandList4,
    host: &crate::host::HostList,
    store: &mut Store,
    b: &Build,
    tier_1_1: bool,
    batch: Option<&mut batch::Batch>,
) {
    let frame = crate::queue::presented();
    let (title_flags, source_title, shape) = match b {
        Build::Bottom {
            dest,
            source,
            flags: f,
            geometries,
        } => (*f, if *source == 0 { *dest } else { *source }, refit::bottom_shape(*f, geometries)),
        Build::Top {
            dest,
            source,
            flags: f,
            count,
            ..
        } => (*f, if *source == 0 { *dest } else { *source }, refit::top_shape(*f, *count)),
    };
    let update = refit::asked(title_flags);
    if update {
        UPDATES_ASKED.fetch_add(1, Ordering::Relaxed);
    }
    let refit = update && refit::on() && refit_planned(store, b, source_title, shape);
    // A bottom level built from the same inputs, not one that moves: kept.
    let mut sig = 0;
    if let Build::Bottom {
        dest,
        flags: f,
        geometries,
        ..
    } = b
    {
        sig = signature(*f, geometries);
        let moves = f & ALLOW_UPDATE != 0;
        if let Some(st) = store.by_title.get_mut(dest)
            && st.sig == sig
            && !moves
            && !(update && refit::on())
        {
            st.used = frame;
            REUSED.fetch_add(1, Ordering::Relaxed);
            return;
        }
    }
    // The inputs, with host addresses.
    let mut geometry = Vec::new();
    let mut uploads: Vec<ID3D12Resource> = Vec::new();
    let (dest, inputs) = match b {
        Build::Bottom {
            dest,
            flags: f,
            geometries,
            ..
        } => {
            if !geometries.iter().all(|g| geometry_ok(g, tier_1_1)) {
                REFUSED.fetch_add(1, Ordering::Relaxed);
                return;
            }
            for g in geometries {
                let (ib, vb) = match unpacked(host, g, &mut uploads) {
                    Ok(Some(v)) => v,
                    Ok(None) => {
                        SKIPPED.fetch_add(1, Ordering::Relaxed);
                        return;
                    }
                    Err(why) => {
                        refused_packed(g, why);
                        return;
                    }
                };
                let Some(tr) = host_address(host, g.transform, 48) else {
                    SKIPPED.fetch_add(1, Ordering::Relaxed);
                    return;
                };
                geometry.push(D3D12_RAYTRACING_GEOMETRY_DESC {
                    Type: D3D12_RAYTRACING_GEOMETRY_TYPE_TRIANGLES,
                    Flags: if g.opaque {
                        D3D12_RAYTRACING_GEOMETRY_FLAG_OPAQUE
                    } else {
                        D3D12_RAYTRACING_GEOMETRY_FLAG_NONE
                    },
                    Anonymous: D3D12_RAYTRACING_GEOMETRY_DESC_0 {
                        Triangles: D3D12_RAYTRACING_GEOMETRY_TRIANGLES_DESC {
                            Transform3x4: tr,
                            IndexFormat: DXGI_FORMAT(if ib.0 == 0 { 0 } else { ib.1 as i32 }),
                            VertexFormat: DXGI_FORMAT(vb.1 as i32),
                            IndexCount: if ib.0 == 0 { 0 } else { g.index_count },
                            VertexCount: g.vertex_count,
                            IndexBuffer: ib.0,
                            VertexBuffer: D3D12_GPU_VIRTUAL_ADDRESS_AND_STRIDE {
                                StartAddress: vb.0,
                                StrideInBytes: vb.2,
                            },
                        },
                    },
                });
            }
            (
                *dest,
                D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS {
                    Type: D3D12_RAYTRACING_ACCELERATION_STRUCTURE_TYPE_BOTTOM_LEVEL,
                    Flags: flags(*f, refit),
                    NumDescs: geometry.len() as u32,
                    DescsLayout: D3D12_ELEMENTS_LAYOUT_ARRAY,
                    Anonymous: D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS_0 {
                        pGeometryDescs: geometry.as_ptr(),
                    },
                },
            )
        }
        Build::Top {
            dest,
            flags: f,
            count,
            instances,
            ..
        } => {
            // The instances from title memory, each bottom level replaced by ours.
            let n = *count as usize;
            let mut raw = vec![0u8; n * INSTANCE_SIZE];
            let on_card = gpu_instances::on();
            let read = n == 0 || crate::host::read_into(*instances as *const u8, &mut raw);
            if !read && !on_card {
                SKIPPED.fetch_add(1, Ordering::Relaxed);
                return;
            }
            if read {
                // On the card's path only to keep the named bottom levels
                // and count those without ours.
                let missing = retarget_instances(&mut raw, |title| {
                    store.by_title.get_mut(&title).map_or(0, |s| {
                        s.used = frame;
                        // SAFETY: a live buffer.
                        unsafe { s.buffer.GetGPUVirtualAddress() }
                    })
                });
                MISSING_BLAS.fetch_add(missing, Ordering::Relaxed);
            }
            let size = (n * INSTANCE_SIZE).max(INSTANCE_SIZE) as u64;
            let va = if on_card {
                let pairs = store
                    .by_title
                    .iter()
                    .filter(|(_, s)| s.built && !s.top_built)
                    // SAFETY: live buffers.
                    .map(|(t, s)| (*t, unsafe { s.buffer.GetGPUVirtualAddress() }))
                    .collect();
                let made = host_address(host, *instances, size)
                    .and_then(|title_va| gpu_instances::record(device, host, title_va, *count, pairs));
                let Some(array) = made else {
                    SKIPPED.fetch_add(1, Ordering::Relaxed);
                    return;
                };
                GPU_INSTANCES.fetch_add(1, Ordering::Relaxed);
                // SAFETY: a live buffer.
                let va = unsafe { array.GetGPUVirtualAddress() };
                uploads.push(array);
                va
            } else {
                let upload = crate::host::host().and_then(|h| h.upload_buffer(size));
                let Some((upload, data)) = upload else {
                    SKIPPED.fetch_add(1, Ordering::Relaxed);
                    return;
                };
                // SAFETY: the mapped upload buffer holds at least `raw`.
                unsafe { std::ptr::copy_nonoverlapping(raw.as_ptr(), data, raw.len()) };
                // SAFETY: a live buffer.
                let va = unsafe { upload.GetGPUVirtualAddress() };
                uploads.push(upload);
                va
            };
            (
                *dest,
                D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS {
                    Type: D3D12_RAYTRACING_ACCELERATION_STRUCTURE_TYPE_TOP_LEVEL,
                    Flags: flags(*f, refit),
                    NumDescs: *count,
                    DescsLayout: D3D12_ELEMENTS_LAYOUT_ARRAY,
                    Anonymous: D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS_0 {
                        InstanceDescs: va,
                    },
                },
            )
        }
    };
    let mut info = D3D12_RAYTRACING_ACCELERATION_STRUCTURE_PREBUILD_INFO::default();
    // SAFETY: inputs naming live host memory (or none).
    unsafe { device5.GetRaytracingAccelerationStructurePrebuildInfo(&inputs, &mut info) };
    if info.ResultDataMaxSizeInBytes == 0 {
        SKIPPED.fetch_add(1, Ordering::Relaxed);
        return;
    }
    let result_size = info.ResultDataMaxSizeInBytes.next_multiple_of(256);
    let scratch_size = if refit {
        info.UpdateScratchDataSizeInBytes
    } else {
        info.ScratchDataSizeInBytes
    }
    .next_multiple_of(256);
    // A refit reads its source: its address before the destination may be
    // made again below, and the source held until the list is done.
    let source_va = if refit {
        let Some(src) = store.by_title.get(&source_title) else {
            return;
        };
        host.keep(&src.buffer);
        // SAFETY: a live buffer.
        unsafe { src.buffer.GetGPUVirtualAddress() }
    } else {
        0
    };
    // The result buffer: kept for this title address, made bigger when needed.
    let fits = store
        .by_title
        .get(&dest)
        .is_some_and(|s| s.size >= result_size);
    if !fits {
        let Some(buf) = buffer(
            device,
            result_size,
            D3D12_RESOURCE_STATE_RAYTRACING_ACCELERATION_STRUCTURE,
        ) else {
            return;
        };
        BYTES.fetch_add(result_size, Ordering::Relaxed);
        let fresh = Structure {
            buffer: buf,
            size: result_size,
            sig,
            used: frame,
            top_built: false,
            built: false,
            updatable: false,
            shape: 0,
        };
        if let Some(old) = store.by_title.insert(dest, fresh) {
            // The list may still read the old one: it holds it until done.
            host.keep(&old.buffer);
            sub_bytes(old.size);
        }
    } else if let Some(st) = store.by_title.get_mut(&dest) {
        st.sig = sig;
        st.used = frame;
    }
    // With `batch`: this build's part of the arena, after a barrier if it
    // depends on a pending build or the arena is full.
    let top = matches!(b, Build::Top { .. });
    let mut scratch_at = 0;
    let mut scratch_want = scratch_size;
    let mut batch = batch;
    if let Some(batch) = batch.as_mut() {
        let arena = store.scratch.as_ref().map_or(0, |s| s.size);
        let reads = if refit { source_title } else { 0 };
        if batch.must_flush(top, dest, reads, scratch_size, arena) {
            barrier_all(l4);
            batch.flushed();
        }
        scratch_want = scratch_size.max(batch::ARENA_MIN);
    }
    let scratch_fits = store
        .scratch
        .as_ref()
        .is_some_and(|s| s.size >= batch.as_ref().map_or(0, |b| b.used()) + scratch_size);
    if !scratch_fits {
        if let Some(batch) = batch.as_mut()
            && batch.pending()
        {
            // A bigger arena: start it after the pending builds (the old
            // one is held by the list until done).
            barrier_all(l4);
            batch.flushed();
        }
        let Some(buf) = buffer(device, scratch_want, D3D12_RESOURCE_STATE_UNORDERED_ACCESS) else {
            return;
        };
        let fresh = Structure {
            buffer: buf,
            size: scratch_want,
            sig: 0,
            used: frame,
            top_built: false,
            built: false,
            updatable: false,
            shape: 0,
        };
        if let Some(old) = store.scratch.replace(fresh) {
            host.keep(&old.buffer);
        }
    }
    let (Some(result), Some(scratch)) = (store.by_title.get(&dest), store.scratch.as_ref()) else {
        return;
    };
    if let Some(batch) = batch.as_mut() {
        scratch_at = batch.take(top, dest, scratch_size);
    }
    // SAFETY: live buffers.
    let desc = unsafe {
        D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_DESC {
            DestAccelerationStructureData: result.buffer.GetGPUVirtualAddress(),
            Inputs: inputs,
            SourceAccelerationStructureData: source_va,
            ScratchAccelerationStructureData: scratch.buffer.GetGPUVirtualAddress() + scratch_at,
        }
    };
    // SAFETY: an open list; the description names live buffers.
    unsafe { l4.BuildRaytracingAccelerationStructure(&desc, None) };
    if batch.is_none() {
        // The shared scratch and this result are ready before the next build reads them.
        let barriers = [uav_barrier(&scratch.buffer), uav_barrier(&result.buffer)];
        // SAFETY: an open list.
        unsafe { l4.ResourceBarrier(&barriers) };
        BARRIERS.fetch_add(1, Ordering::Relaxed);
    }
    host.keep(&result.buffer);
    host.keep(&scratch.buffer);
    for upload in &uploads {
        host.keep(upload);
    }
    if let Some(st) = store.by_title.get_mut(&dest) {
        st.built = true;
        st.updatable = refit::host_flags(title_flags, refit) & refit::ALLOW_UPDATE != 0;
        st.shape = shape;
    }
    match b {
        _ if refit => {
            if let (Build::Top { .. }, Some(st)) = (b, store.by_title.get_mut(&dest)) {
                st.top_built = true;
            }
            REFIT.fetch_add(1, Ordering::Relaxed)
        }
        Build::Bottom { .. } => BUILT_BOTTOM.fetch_add(1, Ordering::Relaxed),
        Build::Top { .. } => {
            if let Some(st) = store.by_title.get_mut(&dest) {
                st.top_built = true;
            }
            BUILT_TOP.fetch_add(1, Ordering::Relaxed)
        }
    };
}

/// A geometry's index buffer (host address, DXGI format) and vertex buffer
/// (host address, DXGI format, stride) as DXR takes them: plain ones as
/// their host copies, the title's meshlet indices and quantized positions
/// decoded from title memory into upload buffers held in `uploads`
/// ([`packed`]). `None`: no host copy.
#[allow(clippy::type_complexity)]
fn unpacked(
    host: &crate::host::HostList,
    g: &Triangles,
    uploads: &mut Vec<ID3D12Resource>,
) -> Result<Option<((u64, u32), (u64, u32, u64))>, packed::Refused> {
    let mut read = |at: u64, out: &mut [u8]| crate::host::read_into(at as *const u8, out);
    let stride = packed::stride(g.stride);
    let ib = if g.index_format == packed::MESHLET_INDICES && g.index_buffer != 0 {
        let tris = g.index_count / 3;
        let flip = packed::flips(g.stride);
        let ix = packed::indices(g.index_buffer, tris, g.vertex_count, flip, &mut read)?;
        let bytes: Vec<u8> = ix.iter().flat_map(|i| i.to_le_bytes()).collect();
        let Some(va) = upload(&bytes, uploads) else {
            return Ok(None);
        };
        PACKED_INDICES.fetch_add(1, Ordering::Relaxed);
        packed_said(g, "meshlet indices", ix.iter().copied().max().map_or(0, u32::from), None);
        (va, packed::R16_UINT)
    } else {
        let len = u64::from(g.index_count) * index_size(g.index_format);
        let Some(va) = host_address(host, g.index_buffer, len) else {
            return Ok(None);
        };
        (va, g.index_format)
    };
    let vb = if g.vertex_format == packed::QUANTIZED_POSITIONS {
        let ps = packed::positions(g.vertex_buffer, g.vertex_count, stride, &mut read)?;
        let bytes: Vec<u8> = ps.iter().flatten().flat_map(|c| c.to_le_bytes()).collect();
        let Some(va) = upload(&bytes, uploads) else {
            return Ok(None);
        };
        PACKED_POSITIONS.fetch_add(1, Ordering::Relaxed);
        let bounds = ps.iter().fold(([f32::MAX; 3], [f32::MIN; 3]), |(lo, hi), p| {
            (
                std::array::from_fn(|c| lo[c].min(p[c])),
                std::array::from_fn(|c| hi[c].max(p[c])),
            )
        });
        packed_said(g, "quantized positions", 0, Some(bounds));
        (va, packed::R32G32B32_FLOAT, packed::POSITION_STRIDE)
    } else {
        let len = u64::from(g.vertex_count) * stride;
        let Some(va) = host_address(host, g.vertex_buffer, len) else {
            return Ok(None);
        };
        (va, g.vertex_format, stride)
    };
    Ok(Some((ib, vb)))
}

/// `bytes` in a fresh upload buffer held in `uploads`: its host address.
fn upload(bytes: &[u8], uploads: &mut Vec<ID3D12Resource>) -> Option<u64> {
    let (buffer, data) = crate::host::host()?.upload_buffer(bytes.len().max(4) as u64)?;
    // SAFETY: the mapped upload buffer holds at least `bytes`.
    unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), data, bytes.len()) };
    // SAFETY: a live buffer.
    let va = unsafe { buffer.GetGPUVirtualAddress() };
    uploads.push(buffer);
    Some(va)
}

/// The first few decoded geometries, for a batch to check the decode by.
fn packed_said(g: &Triangles, what: &str, max_index: u32, bounds: Option<([f32; 3], [f32; 3])>) {
    static SAID: AtomicU64 = AtomicU64::new(0);
    if SAID.fetch_add(1, Ordering::Relaxed) < 12 {
        tracing::info!(
            what,
            index_count = g.index_count,
            vertex_count = g.vertex_count,
            max_index,
            bounds = ?bounds,
            index_buffer = format!("{:#x}", g.index_buffer),
            vertex_buffer = format!("{:#x}", g.vertex_buffer),
            "native bvh packed: decoded (the first 12)"
        );
    }
}

/// A bottom level not built: one of its geometries would not decode.
fn refused_packed(g: &Triangles, why: packed::Refused) {
    PACKED_REFUSED.fetch_add(1, Ordering::Relaxed);
    static SAID: AtomicU64 = AtomicU64::new(0);
    if SAID.fetch_add(1, Ordering::Relaxed) < 8 {
        tracing::info!(
            reason = ?why,
            index_format = g.index_format,
            vertex_format = g.vertex_format,
            index_count = g.index_count,
            vertex_count = g.vertex_count,
            stride = g.stride,
            index_buffer = format!("{:#x}", g.index_buffer),
            vertex_buffer = format!("{:#x}", g.vertex_buffer),
            "native bvh packed: a bottom level refused (the first 8)"
        );
    }
}

/// Whether the update `b` the title asked (of this `shape`, from
/// `source_title`) can be a host refit; a refusal is counted, the first
/// few logged with the reason.
fn refit_planned(store: &Store, b: &Build, source_title: u64, shape: u64) -> bool {
    let dest = match b {
        Build::Bottom { dest, .. } | Build::Top { dest, .. } => *dest,
    };
    let source = store.by_title.get(&source_title).map(|s| refit::Source {
        updatable: s.updatable,
        shape: s.shape,
        size: s.size,
        built: s.built,
    });
    let Err(why) = refit::plan(source, shape, source_title == dest) else {
        return true;
    };
    REFIT_REFUSED.fetch_add(1, Ordering::Relaxed);
    static SAID: AtomicU64 = AtomicU64::new(0);
    if SAID.fetch_add(1, Ordering::Relaxed) < 8 {
        tracing::info!(
            reason = ?why,
            dest = format!("{dest:#x}"),
            source = format!("{source_title:#x}"),
            top = matches!(b, Build::Top { .. }),
            "native bvh refit: an update built from scratch (the first 8)"
        );
    }
    false
}

/// The host GPU address of the top level built for title address
/// `address`, if one was built and is kept (`crate::rt_bind`). Asking keeps
/// it from being let go.
pub(crate) fn top_level(address: u64) -> Option<u64> {
    if !on() {
        return None;
    }
    let mut store = STORE.lock().unwrap_or_else(|e| e.into_inner());
    let st = store.as_mut()?.by_title.get_mut(&address)?;
    if !st.top_built {
        return None;
    }
    st.used = crate::queue::presented();
    // SAFETY: a live buffer.
    Some(unsafe { st.buffer.GetGPUVirtualAddress() })
}

/// How many top levels are kept, for `crate::rt_bind`'s summary.
pub(crate) fn tops_kept() -> usize {
    STORE
        .lock()
        .ok()
        .and_then(|s| s.as_ref().map(|s| s.by_title.values().filter(|st| st.top_built).count()))
        .unwrap_or(0)
}

/// A barrier on every unordered access before it ([`batch`]): the builds
/// recorded since are done before what follows reads or reuses them.
fn barrier_all(l4: &ID3D12GraphicsCommandList4) {
    let all = D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            UAV: std::mem::ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER {
                pResource: std::mem::ManuallyDrop::new(None),
            }),
        },
    };
    // SAFETY: an open list; a barrier naming no resource.
    unsafe { l4.ResourceBarrier(&[all]) };
    BARRIERS.fetch_add(1, Ordering::Relaxed);
}

fn uav_barrier(r: &ID3D12Resource) -> D3D12_RESOURCE_BARRIER {
    D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            UAV: std::mem::ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER {
                // SAFETY: a borrowed copy of the interface pointer, not released by the barrier.
                pResource: unsafe { std::mem::transmute_copy(r) },
            }),
        },
    }
}

/// Every 5 s: what was built since.
fn summary() {
    static LAST: Mutex<Option<std::time::Instant>> = Mutex::new(None);
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    let now = std::time::Instant::now();
    if last.is_some_and(|t| now.duration_since(t).as_secs() < 5) {
        return;
    }
    *last = Some(now);
    let kept = STORE
        .lock()
        .ok()
        .and_then(|s| s.as_ref().map(|s| s.by_title.len()))
        .unwrap_or(0);
    tracing::info!(
        bottom = BUILT_BOTTOM.swap(0, Ordering::Relaxed),
        top = BUILT_TOP.swap(0, Ordering::Relaxed),
        skipped = SKIPPED.swap(0, Ordering::Relaxed),
        instances_without_ours = MISSING_BLAS.swap(0, Ordering::Relaxed),
        reused = REUSED.swap(0, Ordering::Relaxed),
        freed = FREED.swap(0, Ordering::Relaxed),
        aliased = ALIASED.swap(0, Ordering::Relaxed),
        refused = REFUSED.swap(0, Ordering::Relaxed),
        updates_asked = UPDATES_ASKED.swap(0, Ordering::Relaxed),
        refit = REFIT.swap(0, Ordering::Relaxed),
        refit_refused = REFIT_REFUSED.swap(0, Ordering::Relaxed),
        barriers = BARRIERS.swap(0, Ordering::Relaxed),
        gpu_instances = GPU_INSTANCES.swap(0, Ordering::Relaxed),
        packed_indices = PACKED_INDICES.swap(0, Ordering::Relaxed),
        packed_positions = PACKED_POSITIONS.swap(0, Ordering::Relaxed),
        packed_refused = PACKED_REFUSED.swap(0, Ordering::Relaxed),
        structures_kept = kept,
        mb_live = BYTES.load(Ordering::Relaxed) >> 20,
        "native bvh: host structures built in the last 5 s"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tri(vb: u64) -> Triangles {
        Triangles {
            opaque: true,
            transform: 0,
            index_format: 57,
            vertex_format: 6,
            index_count: 3,
            vertex_count: 3,
            index_buffer: 0x100,
            vertex_buffer: vb,
            stride: 12,
        }
    }

    #[test]
    fn off_by_default() {
        assert!(std::env::var_os("VICTORY_NATIVE_BVH").is_none());
        assert!(!on());
        // Off: nothing is touched, even with a null list.
        build(
            std::ptr::null(),
            &[Build::Top {
                dest: 1,
                source: 0,
                flags: 0,
                count: 0,
                instances: 0,
            }],
        );
        assert_eq!(BUILT_TOP.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn signature_follows_the_inputs() {
        let a = signature(0, &[tri(0x200)]);
        assert_eq!(a, signature(0, &[tri(0x200)]));
        assert_ne!(a, signature(0, &[tri(0x300)]));
        assert_ne!(a, signature(4, &[tri(0x200)]));
        // Never 0, the mark of a structure that is not a bottom level.
        assert_eq!(a & 1, 1);
    }

    #[test]
    fn only_formats_dxr_takes_are_built() {
        assert!(geometry_ok(&tri(0x200), false));
        // RGBA8 positions: tier 1.1 only.
        let rgba8 = Triangles {
            vertex_format: 28,
            ..tri(0x200)
        };
        assert!(!geometry_ok(&rgba8, false));
        assert!(geometry_ok(&rgba8, true));
        // RGBA32 float positions are not a DXR vertex format.
        assert!(!geometry_ok(
            &Triangles {
                vertex_format: 2,
                ..tri(0x200)
            },
            true
        ));
        // An index buffer of an unknown format, or a partial triangle.
        assert!(!geometry_ok(
            &Triangles {
                index_format: 0,
                ..tri(0x200)
            },
            true
        ));
        assert!(!geometry_ok(
            &Triangles {
                index_count: 4,
                ..tri(0x200)
            },
            true
        ));
        // No index buffer: whole triangles of vertices.
        let plain = Triangles {
            index_buffer: 0,
            index_format: 0,
            vertex_count: 6,
            ..tri(0x200)
        };
        assert!(geometry_ok(&plain, false));
        assert!(!geometry_ok(
            &Triangles {
                vertex_count: 7,
                ..plain
            },
            false
        ));
        assert!(!geometry_ok(
            &Triangles {
                stride: 0,
                ..tri(0x200)
            },
            false
        ));
    }

    #[test]
    fn clones_and_compactions_alias_their_source() {
        assert!(copy_keeps_rays(0));
        assert!(copy_keeps_rays(1));
        // Visualization, serialize, deserialize: not the same structure.
        assert!(!copy_keeps_rays(2));
        assert!(!copy_keeps_rays(3));
        assert!(!copy_keeps_rays(4));
        // Off: nothing is recorded.
        copy(0x2000, 0x1000, 1);
        assert_eq!(ALIASED.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn index_sizes() {
        assert_eq!(index_size(57), 2);
        assert_eq!(index_size(42), 4);
        assert_eq!(index_size(0), 0);
        // The title's meshlet indices (R16_TYPELESS), decoded to R16_UINT.
        assert_eq!(index_size(53), 2);
    }

    #[test]
    fn the_titles_own_forms_are_built() {
        // Batch 5 part F: 134 of 140 logged geometries, refused before.
        let meshlets = Triangles {
            index_format: 53,
            vertex_format: 16,
            stride: 8,
            ..tri(0x200)
        };
        assert!(geometry_ok(&meshlets, false));
        assert!(geometry_ok(&Triangles { vertex_format: 6, stride: 12, ..meshlets }, false));
        // Bit 15 of the stride is a flag: a stride of 0 under it is none.
        assert!(!geometry_ok(&Triangles { stride: 0x8000, ..meshlets }, false));
    }

    #[test]
    fn instances_get_host_bottom_levels_or_are_masked_out() {
        let mut raw = vec![0u8; 2 * INSTANCE_SIZE];
        for (i, inst) in raw.chunks_exact_mut(INSTANCE_SIZE).enumerate() {
            inst[INSTANCE_MASK_AT..INSTANCE_MASK_AT + 4]
                .copy_from_slice(&0xff00_0007u32.to_le_bytes());
            inst[INSTANCE_BLAS_AT..].copy_from_slice(&(0x1000 * (i as u64 + 1)).to_le_bytes());
        }
        let missing = retarget_instances(&mut raw, |t| if t == 0x1000 { 0xdead_0000 } else { 0 });
        assert_eq!(missing, 1);
        let blas = |i: usize| {
            let at = i * INSTANCE_SIZE + INSTANCE_BLAS_AT;
            u64::from_le_bytes(raw[at..at + 8].try_into().unwrap())
        };
        let mask = |i: usize| {
            let at = i * INSTANCE_SIZE + INSTANCE_MASK_AT;
            u32::from_le_bytes(raw[at..at + 4].try_into().unwrap())
        };
        assert_eq!((blas(0), mask(0)), (0xdead_0000, 0xff00_0007));
        assert_eq!((blas(1), mask(1)), (0, 0));
    }
}
