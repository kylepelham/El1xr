//! `VICTORY_BVH_HOOK=1` (LNX, off by default): the executable's calls into the
//! console's ray-tracing structure builder (`XboxBvhBuilder.dll`) go through
//! wrappers here first, which log what the title asks for (counts, kinds,
//! geometry) and then call the builder as before. With `VICTORY_NATIVE_BVH=1`
//! the same builds are also made with the host's own DXR
//! ([`victory_gpu::native_rt`]); nothing reads those yet.
//!
//! The builder is a C++ class; its methods take `this` first (the x64
//! convention for member functions is the plain one with `this` in `rcx`),
//! so each wrapper is an `extern "C"` function with `this` as its first
//! argument. Its inputs are the standard D3D12 ray-tracing structures, plus
//! two of the console's own (the batched build and the stream description),
//! logged raw until their layout is known.
//!
//! Source: Linox-IDK/LNX-victory 9698777e79, 012fbf0526 (GPL-3.0-or-later).

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

/// The builder's DLL as the executable imports it.
pub const DLL: &str = "XboxBvhBuilder.dll";

fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_BVH_HOOK").is_some_and(|v| v == "1"))
}

/// After the executable's imports are bound: its imports of [`DLL`] go
/// through the wrappers here (`VICTORY_BVH_HOOK=1`; otherwise nothing).
pub fn install(image: &victory_loader::LoadedImage) -> anyhow::Result<()> {
    if !on() {
        return Ok(());
    }
    let wrapped = image.wrap_imports(DLL, &wrap)?;
    println!("wrapped        {wrapped} imports of {DLL}");
    Ok(())
}

/// Detailed logs per kind of call before only the counts go on.
const DETAILED: u64 = 6;

/// The builder's own functions, filled when the wrappers are installed.
static CTOR: AtomicUsize = AtomicUsize::new(0);
static DTOR: AtomicUsize = AtomicUsize::new(0);
static BATCHED: AtomicUsize = AtomicUsize::new(0);
static COPY: AtomicUsize = AtomicUsize::new(0);
static PREBUILD: AtomicUsize = AtomicUsize::new(0);
static REHYDRATE: AtomicUsize = AtomicUsize::new(0);
static MORTON: AtomicUsize = AtomicUsize::new(0);

/// Calls (and items in them) per kind, since the last summary.
struct Counts {
    batched: AtomicU64,
    batched_items: AtomicU64,
    copy: AtomicU64,
    prebuild: AtomicU64,
    prebuild_bottom: AtomicU64,
    prebuild_top: AtomicU64,
    prebuild_geometries: AtomicU64,
    prebuild_triangles: AtomicU64,
    rehydrate: AtomicU64,
    rehydrate_items: AtomicU64,
}

static COUNTS: Counts = Counts {
    batched: AtomicU64::new(0),
    batched_items: AtomicU64::new(0),
    copy: AtomicU64::new(0),
    prebuild: AtomicU64::new(0),
    prebuild_bottom: AtomicU64::new(0),
    prebuild_top: AtomicU64::new(0),
    prebuild_geometries: AtomicU64::new(0),
    prebuild_triangles: AtomicU64::new(0),
    rehydrate: AtomicU64::new(0),
    rehydrate_items: AtomicU64::new(0),
};

/// Totals since the start (for the detailed-log budget).
static SEEN_BATCHED: AtomicU64 = AtomicU64::new(0);
static SEEN_PREBUILD: AtomicU64 = AtomicU64::new(0);
static SEEN_REHYDRATE: AtomicU64 = AtomicU64::new(0);
static SEEN_COPY: AtomicU64 = AtomicU64::new(0);

/// Which builder method an import of [`DLL`] names (its MSVC-decorated
/// name), if one the hook wraps.
fn slot_for(name: &str) -> Option<(&'static AtomicUsize, usize)> {
    let method = name.strip_prefix('?')?;
    Some(if name.starts_with("??0XboxBvhBuilder") {
        (&CTOR, ctor as *const () as usize)
    } else if name.starts_with("??1XboxBvhBuilder") {
        (&DTOR, dtor as *const () as usize)
    } else if method.starts_with("BuildRaytracingAccelerationStructureBatched2X@") {
        (&BATCHED, batched as *const () as usize)
    } else if method.starts_with("CopyRaytracingAccelerationStructure@") {
        (&COPY, copy as *const () as usize)
    } else if method.starts_with("GetRaytracingAccelerationStructurePrebuildInfo@") {
        (&PREBUILD, prebuild as *const () as usize)
    } else if method.starts_with("RehydrateRaytracingAccelerationStructureBatched2X@") {
        (&REHYDRATE, rehydrate as *const () as usize)
    } else if method.starts_with("Set64BitMortonCodes@") {
        (&MORTON, morton as *const () as usize)
    } else {
        return None;
    })
}

/// For [`DLL`]'s import `name` bound to `original`: the wrapper to bind
/// instead, when the hook is on and the import is one it knows.
pub fn wrap(name: &str, original: usize) -> Option<usize> {
    if !on() || original == 0 {
        return None;
    }
    let Some((slot, wrapper)) = slot_for(name) else {
        tracing::info!(name, "bvh hook: an import of the builder left as it is");
        return None;
    };
    slot.store(original, Ordering::Relaxed);
    tracing::info!(name, "bvh hook: wrapped");
    Some(wrapper)
}

/// The original behind `slot`, as a function of type `F`.
///
/// # Safety
/// `F` must be the builder function's real signature.
unsafe fn original<F: Copy>(slot: &AtomicUsize) -> F {
    let address = slot.load(Ordering::Relaxed);
    assert!(
        address != 0,
        "bvh hook: a wrapper ran before its original was known"
    );
    // SAFETY: the caller names the signature; `F` is a function pointer type.
    unsafe { std::mem::transmute_copy(&address) }
}

/// `len` bytes at `p` as hex, 16 per group (empty for a null pointer).
fn hex(p: *const u8, len: usize) -> String {
    if p.is_null() {
        return "null".to_owned();
    }
    // SAFETY: the title passed the pointer for at least this structure.
    let bytes = unsafe { std::slice::from_raw_parts(p, len) };
    bytes
        .chunks(16)
        .map(|c| c.iter().map(|b| format!("{b:02x}")).collect::<String>())
        .collect::<Vec<_>>()
        .join(" ")
}

/// A field of type `T` at byte `offset` of `p`.
///
/// # Safety
/// `p + offset` must be readable for `T`.
unsafe fn field<T: Copy>(p: *const u8, offset: usize) -> T {
    // SAFETY: by the caller.
    unsafe { p.add(offset).cast::<T>().read_unaligned() }
}

/// Every few seconds: the calls since the last summary.
fn summary() {
    static LAST: Mutex<Option<Instant>> = Mutex::new(None);
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    let now = Instant::now();
    let due = last.is_none_or(|t| now.duration_since(t).as_secs() >= 5);
    if !due {
        return;
    }
    *last = Some(now);
    let take = |a: &AtomicU64| a.swap(0, Ordering::Relaxed);
    let c = &COUNTS;
    tracing::info!(
        batched = take(&c.batched),
        batched_items = take(&c.batched_items),
        rehydrate = take(&c.rehydrate),
        rehydrate_items = take(&c.rehydrate_items),
        prebuild = take(&c.prebuild),
        bottom = take(&c.prebuild_bottom),
        top = take(&c.prebuild_top),
        geometries = take(&c.prebuild_geometries),
        triangles = take(&c.prebuild_triangles),
        copy = take(&c.copy),
        "bvh hook: builder calls in the last 5 s"
    );
}

extern "C" fn ctor(this: *mut u8, device: *mut u8, flag: u8) -> *mut u8 {
    tracing::info!(?this, ?device, flag, "bvh hook: builder made");
    // SAFETY: the constructor's signature (`this`, ID3D12Device11*, bool) -> this.
    let f: extern "C" fn(*mut u8, *mut u8, u8) -> *mut u8 = unsafe { original(&CTOR) };
    f(this, device, flag)
}

extern "C" fn dtor(this: *mut u8) {
    tracing::info!(?this, "bvh hook: builder let go");
    // SAFETY: the destructor's signature (`this`).
    let f: extern "C" fn(*mut u8) = unsafe { original(&DTOR) };
    f(this)
}

extern "C" fn morton(this: *mut u8, on: u8) {
    // The title switches it around every build (batch 5 part F: 14 lines a second): the first few only.
    static LOGGED: AtomicUsize = AtomicUsize::new(0);
    if LOGGED.fetch_add(1, Ordering::Relaxed) < 4 {
        tracing::info!(on, "bvh hook: 64-bit Morton codes (the first 4 calls)");
    }
    // SAFETY: (`this`, bool).
    let f: extern "C" fn(*mut u8, u8) = unsafe { original(&MORTON) };
    f(this, on)
}

/// The console's batched build: `count` descriptions of a layout of its own
/// (`D3D12XBOX_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_BATCHED_DESC2`).
extern "C" fn batched(this: *mut u8, list: *mut u8, count: u32, descs: *const u8) {
    COUNTS.batched.fetch_add(1, Ordering::Relaxed);
    COUNTS
        .batched_items
        .fetch_add(u64::from(count), Ordering::Relaxed);
    let detailed = SEEN_BATCHED.fetch_add(1, Ordering::Relaxed) < DETAILED;
    let mut builds = Vec::new();
    if !descs.is_null() {
        for i in 0..count as usize {
            // SAFETY: `count` items of BATCHED_ITEM_SIZE bytes, a standard
            // build description first (destination, inputs at byte 8, ...).
            let d = unsafe { descs.add(i * BATCHED_ITEM_SIZE) };
            let show = detailed && i < 2;
            if show {
                tracing::info!(
                    i,
                    item = hex(d, BATCHED_ITEM_SIZE),
                    "bvh hook: batched item (raw)"
                );
            }
            // SAFETY: as above.
            unsafe { read_inputs(d.add(8), "batched", show) };
            if victory_gpu::native_rt::on() {
                // SAFETY: as above; the destination first, an update's source at 32.
                if let Some(b) = unsafe { parse(field::<u64>(d, 0), field::<u64>(d, 32), d.add(8)) } {
                    builds.push(b);
                }
            }
        }
    }
    victory_gpu::native_rt::build(list.cast_const().cast(), &builds);
    // SAFETY: `count` items of BATCHED_ITEM_SIZE bytes, each a standard
    // build description first.
    unsafe { premap(descs, count, BATCHED_ITEM_SIZE) };
    summary();
    // SAFETY: (`this`, ID3D12GraphicsCommandList6*, UINT, const DESC2*).
    let f: extern "C" fn(*mut u8, *mut u8, u32, *const u8) = unsafe { original(&BATCHED) };
    f(this, list, count, descs)
}

extern "C" fn copy(this: *mut u8, list: *mut u8, dest: u64, source: u64, mode: u32) {
    COUNTS.copy.fetch_add(1, Ordering::Relaxed);
    if SEEN_COPY.fetch_add(1, Ordering::Relaxed) < DETAILED {
        tracing::info!(
            dest = format!("{dest:#x}"),
            source = format!("{source:#x}"),
            mode,
            "bvh hook: copy"
        );
    }
    victory_gpu::native_rt::copy(dest, source, mode);
    victory_gpu::bvh_premap::builds(&[dest], &[dest, source]);
    summary();
    // SAFETY: (`this`, ID3D12GraphicsCommandList4*, u64, u64, COPY_MODE).
    let f: extern "C" fn(*mut u8, *mut u8, u64, u64, u32) = unsafe { original(&COPY) };
    f(this, list, dest, source, mode)
}

/// `D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS`: type, flags,
/// count, layout, then the descriptions (an array or an array of pointers).
const INPUTS_SIZE: usize = 24;
/// `D3D12_RAYTRACING_GEOMETRY_DESC`: type, flags, then the triangles
/// (transform, index format, vertex format, index count, vertex count,
/// index buffer, vertex buffer start and stride) or the boxes.
const GEOMETRY_SIZE: usize = 56;
/// `D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_DESC`: destination,
/// inputs, source, scratch.
const BUILD_DESC_SIZE: usize = 8 + INPUTS_SIZE + 8 + 8;
/// An item of the console's batched build (`..._BATCHED_DESC2`), as logged
/// from the title: a standard build description, then a u64 (1 seen), a
/// pointer, and 16 bytes of zeros.
const BATCHED_ITEM_SIZE: usize = 80;

/// Counts the geometry of `inputs` (bottom level: its geometries and
/// triangles) and, while `detailed`, logs the first geometries.
///
/// # Safety
/// `inputs` points to the title's inputs structure.
unsafe fn read_inputs(inputs: *const u8, what: &str, detailed: bool) {
    if inputs.is_null() {
        return;
    }
    // SAFETY: the inputs structure (24 bytes).
    let (kind, flags, n, layout, descs) = unsafe {
        (
            field::<u32>(inputs, 0),
            field::<u32>(inputs, 4),
            field::<u32>(inputs, 8),
            field::<u32>(inputs, 12),
            field::<u64>(inputs, 16) as *const u8,
        )
    };
    if kind == 0 {
        COUNTS.prebuild_top.fetch_add(1, Ordering::Relaxed);
    } else {
        COUNTS.prebuild_bottom.fetch_add(1, Ordering::Relaxed);
        COUNTS
            .prebuild_geometries
            .fetch_add(u64::from(n), Ordering::Relaxed);
    }
    if detailed {
        tracing::info!(
            what,
            level = if kind == 0 { "top" } else { "bottom" },
            flags = format!("{flags:#x}"),
            count = n,
            layout = if layout == 0 { "array" } else { "pointers" },
            "bvh hook: inputs"
        );
    }
    if kind == 0 || descs.is_null() {
        return;
    }
    for i in 0..n as usize {
        // SAFETY: the array (or array of pointers) of `n` geometry descriptions.
        let g = unsafe { geometry_at(descs, layout, i) };
        if g.is_null() {
            continue;
        }
        // SAFETY: one geometry description.
        let gkind = unsafe { field::<u32>(g, 0) };
        if gkind == 0 {
            // SAFETY: its triangles part.
            let t = unsafe { triangles(g) };
            let tris = if t.index_buffer != 0 {
                t.index_count / 3
            } else {
                t.vertex_count / 3
            };
            COUNTS
                .prebuild_triangles
                .fetch_add(u64::from(tris), Ordering::Relaxed);
            if detailed && i < 3 {
                tracing::info!(
                    i,
                    opaque = t.opaque,
                    transform = format!("{:#x}", t.transform),
                    index_format = t.index_format,
                    vertex_format = t.vertex_format,
                    index_count = t.index_count,
                    vertex_count = t.vertex_count,
                    index_buffer = format!("{:#x}", t.index_buffer),
                    vertex_buffer = format!("{:#x}", t.vertex_buffer),
                    stride = t.stride,
                    "bvh hook: triangles"
                );
            }
        } else if detailed && i < 3 {
            tracing::info!(
                i,
                kind = gkind,
                "bvh hook: a geometry that is not triangles (boxes)"
            );
        }
    }
}

/// The `i`th geometry description of an array (`layout` 0) or an array of
/// pointers (`layout` 1) at `descs`.
///
/// # Safety
/// `descs` holds at least `i + 1` descriptions (or pointers to them).
unsafe fn geometry_at(descs: *const u8, layout: u32, i: usize) -> *const u8 {
    // SAFETY: by the caller.
    unsafe {
        if layout == 0 {
            descs.add(i * GEOMETRY_SIZE)
        } else {
            field::<u64>(descs, i * 8) as *const u8
        }
    }
}

/// The triangles part of the geometry description at `g`.
///
/// # Safety
/// `g` points to a triangle geometry description ([`GEOMETRY_SIZE`] bytes).
unsafe fn triangles(g: *const u8) -> victory_gpu::native_rt::Triangles {
    // SAFETY: by the caller.
    unsafe {
        victory_gpu::native_rt::Triangles {
            opaque: field::<u32>(g, 4) & 1 != 0,
            transform: field(g, 8),
            index_format: field(g, 16),
            vertex_format: field(g, 20),
            index_count: field(g, 24),
            vertex_count: field(g, 28),
            index_buffer: field(g, 32),
            vertex_buffer: field(g, 40),
            stride: field(g, 48),
        }
    }
}

/// `VICTORY_BVH_PREMAP=1` ([`victory_gpu::bvh_premap`]): the copies of the
/// ranges `count` build descriptions `size` bytes apart name are made now.
///
/// # Safety
/// `descs` is null or points to `count` descriptions `size` bytes apart,
/// each a standard build description first.
unsafe fn premap(descs: *const u8, count: u32, size: usize) {
    if !victory_gpu::bvh_premap::wanted() || descs.is_null() {
        return;
    }
    let (mut outputs, mut ranges) = (Vec::new(), Vec::new());
    for i in 0..count as usize {
        // SAFETY: by the caller.
        unsafe { build_ranges(descs.add(i * size), &mut ranges) };
        // SAFETY: as above; destination and scratch.
        unsafe { outputs.extend(build_outputs(descs.add(i * size))) };
    }
    victory_gpu::bvh_premap::builds(&outputs, &ranges);
}

/// What a standard build description writes: its destination and scratch.
///
/// # Safety
/// `d` points to a standard build description.
unsafe fn build_outputs(d: *const u8) -> [u64; 2] {
    // SAFETY: by the caller.
    unsafe { [field::<u64>(d, 0), field::<u64>(d, 40)] }
}

/// The ranges a standard build description names: its destination, source
/// and scratch, and its inputs' instances (top level) or each triangle
/// geometry's transform, indices and vertices.
///
/// # Safety
/// `d` points to a standard build description.
unsafe fn build_ranges(d: *const u8, out: &mut Vec<u64>) {
    // SAFETY: by the caller; destination, inputs at byte 8, source, scratch.
    unsafe {
        out.extend([field::<u64>(d, 0), field::<u64>(d, 32), field::<u64>(d, 40)]);
        let inputs = d.add(8);
        let (kind, n, layout) = (
            field::<u32>(inputs, 0),
            field::<u32>(inputs, 8),
            field::<u32>(inputs, 12),
        );
        let descs = field::<u64>(inputs, 16);
        if kind == 0 {
            out.push(descs);
            return;
        }
        let descs = descs as *const u8;
        if descs.is_null() {
            return;
        }
        for i in 0..n as usize {
            let g = geometry_at(descs, layout, i);
            if !g.is_null() && field::<u32>(g, 0) == 0 {
                out.extend([field::<u64>(g, 8), field::<u64>(g, 32), field::<u64>(g, 40)]);
            }
        }
    }
}

/// The build `inputs` describe into `dest` (an update: from `source`), for
/// the host's own structures ([`victory_gpu::native_rt`]); `None` for a
/// layout it does not take.
///
/// # Safety
/// `inputs` points to the title's inputs structure.
unsafe fn parse(dest: u64, source: u64, inputs: *const u8) -> Option<victory_gpu::native_rt::Build> {
    use victory_gpu::native_rt::Build;
    if inputs.is_null() {
        return None;
    }
    // SAFETY: the inputs structure.
    let (kind, flags, n, layout, descs) = unsafe {
        (
            field::<u32>(inputs, 0),
            field::<u32>(inputs, 4),
            field::<u32>(inputs, 8),
            field::<u32>(inputs, 12),
            field::<u64>(inputs, 16),
        )
    };
    if kind == 0 {
        // Top level: `descs` is the instances' GPU address (an array).
        return (layout == 0).then_some(Build::Top {
            dest,
            source,
            flags,
            count: n,
            instances: descs,
        });
    }
    let descs = descs as *const u8;
    if descs.is_null() {
        return None;
    }
    let mut geometries = Vec::with_capacity(n as usize);
    for i in 0..n as usize {
        // SAFETY: the array (or array of pointers) of `n` descriptions.
        let g = unsafe { geometry_at(descs, layout, i) };
        // SAFETY: one geometry description, triangles (type 0) only.
        if g.is_null() || unsafe { field::<u32>(g, 0) } != 0 {
            return None;
        }
        // SAFETY: as above.
        geometries.push(unsafe { triangles(g) });
    }
    Some(Build::Bottom {
        dest,
        source,
        flags,
        geometries,
    })
}

extern "C" fn prebuild(this: *mut u8, inputs: *const u8, stream: *const u8, info: *mut u64) {
    COUNTS.prebuild.fetch_add(1, Ordering::Relaxed);
    let detailed = SEEN_PREBUILD.fetch_add(1, Ordering::Relaxed) < DETAILED;
    // SAFETY: the title's inputs.
    unsafe { read_inputs(inputs, "prebuild", detailed) };
    if detailed {
        tracing::info!(
            stream = hex(stream, 32),
            "bvh hook: prebuild stream description (raw)"
        );
    }
    // SAFETY: (`this`, const INPUTS*, const XDXR_STREAM_DESC*, PREBUILD_INFO*).
    let f: extern "C" fn(*mut u8, *const u8, *const u8, *mut u64) = unsafe { original(&PREBUILD) };
    f(this, inputs, stream, info);
    if detailed && !info.is_null() {
        // SAFETY: the prebuild info the builder just wrote (three u64).
        let (result, scratch, update) = unsafe { (*info, *info.add(1), *info.add(2)) };
        tracing::info!(result, scratch, update, "bvh hook: prebuild sizes");
    }
    summary();
}

/// Structures the title brought ready-made (from its files): `count` build
/// descriptions (standard layout) and, beside them, one u64 each.
extern "C" fn rehydrate(
    this: *mut u8,
    list: *mut u8,
    count: u32,
    descs: *const u8,
    extra: *const u64,
) -> i32 {
    COUNTS.rehydrate.fetch_add(1, Ordering::Relaxed);
    COUNTS
        .rehydrate_items
        .fetch_add(u64::from(count), Ordering::Relaxed);
    if SEEN_REHYDRATE.fetch_add(1, Ordering::Relaxed) < DETAILED && !descs.is_null() {
        for i in 0..(count as usize).min(2) {
            // SAFETY: `count` build descriptions.
            let d = unsafe { descs.add(i * BUILD_DESC_SIZE) };
            // SAFETY: the description's destination and source.
            let (dest, source) = unsafe { (field::<u64>(d, 0), field::<u64>(d, 32)) };
            let x = if extra.is_null() {
                0
            } else {
                // SAFETY: `extra` has one u64 per description when not null.
                unsafe { *extra.add(i) }
            };
            tracing::info!(
                count,
                i,
                dest = format!("{dest:#x}"),
                source = format!("{source:#x}"),
                extra = format!("{x:#x}"),
                "bvh hook: rehydrate"
            );
            // SAFETY: its inputs, at byte 8.
            unsafe { read_inputs(d.add(8), "rehydrate", true) };
        }
    }
    if victory_gpu::native_rt::on() && !descs.is_null() {
        let builds: Vec<_> = (0..count as usize)
            .filter_map(|i| {
                // SAFETY: `count` standard build descriptions.
                let d = unsafe { descs.add(i * BUILD_DESC_SIZE) };
                // SAFETY: as above; inputs at byte 8, an update's source at 32.
                unsafe { parse(field::<u64>(d, 0), field::<u64>(d, 32), d.add(8)) }
            })
            .collect();
        victory_gpu::native_rt::build(list.cast_const().cast(), &builds);
    }
    // SAFETY: `count` standard build descriptions.
    unsafe { premap(descs, count, BUILD_DESC_SIZE) };
    summary();
    // SAFETY: (`this`, ID3D12GraphicsCommandList8*, UINT, const DESC*, const UINT64*) -> HRESULT.
    let f: extern "C" fn(*mut u8, *mut u8, u32, *const u8, *const u64) -> i32 =
        unsafe { original(&REHYDRATE) };
    f(this, list, count, descs, extra)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layouts_match_d3d12() {
        assert_eq!(INPUTS_SIZE, 24);
        assert_eq!(GEOMETRY_SIZE, 56);
        assert_eq!(BUILD_DESC_SIZE, 48);
        assert_eq!(BATCHED_ITEM_SIZE, BUILD_DESC_SIZE + 32);
    }

    #[test]
    fn off_by_default_wraps_nothing() {
        assert!(std::env::var_os("VICTORY_BVH_HOOK").is_none());
        assert!(wrap("?Set64BitMortonCodes@XboxBvhBuilder@@QEAAX_N@Z", 0x1000).is_none());
    }

    #[test]
    fn names_the_builder_methods() {
        for name in [
            "??0XboxBvhBuilder@@QEAA@PEAUID3D12Device11@@_N@Z",
            "??1XboxBvhBuilder@@QEAA@XZ",
            "?BuildRaytracingAccelerationStructureBatched2X@XboxBvhBuilder@@QEAAXPEAUX@@IPEBU1@@Z",
            "?CopyRaytracingAccelerationStructure@XboxBvhBuilder@@QEAAX@Z",
            "?GetRaytracingAccelerationStructurePrebuildInfo@XboxBvhBuilder@@QEAAX@Z",
            "?RehydrateRaytracingAccelerationStructureBatched2X@XboxBvhBuilder@@QEAAJ@Z",
            "?Set64BitMortonCodes@XboxBvhBuilder@@QEAAX_N@Z",
        ] {
            assert!(slot_for(name).is_some(), "{name}");
        }
        assert!(slot_for("?SomethingElse@XboxBvhBuilder@@QEAAXXZ").is_none());
        assert!(slot_for("plain_c_export").is_none());
    }

    /// A bottom-level inputs structure over an array of two triangle
    /// geometries reads back as the same build.
    #[test]
    fn parses_a_bottom_level_array() {
        let mut geo = [0u8; GEOMETRY_SIZE * 2];
        for (i, g) in geo.chunks_mut(GEOMETRY_SIZE).enumerate() {
            let i = i as u64;
            g[4..8].copy_from_slice(&1u32.to_le_bytes()); // opaque
            g[8..16].copy_from_slice(&(0x1000 + i).to_le_bytes());
            g[16..20].copy_from_slice(&57u32.to_le_bytes()); // R16_UINT
            g[20..24].copy_from_slice(&6u32.to_le_bytes()); // R32G32B32_FLOAT
            g[24..28].copy_from_slice(&36u32.to_le_bytes());
            g[28..32].copy_from_slice(&8u32.to_le_bytes());
            g[32..40].copy_from_slice(&(0x2000 + i).to_le_bytes());
            g[40..48].copy_from_slice(&(0x3000 + i).to_le_bytes());
            g[48..56].copy_from_slice(&12u64.to_le_bytes());
        }
        let mut inputs = [0u8; INPUTS_SIZE];
        inputs[0..4].copy_from_slice(&1u32.to_le_bytes()); // bottom
        inputs[4..8].copy_from_slice(&0x4u32.to_le_bytes());
        inputs[8..12].copy_from_slice(&2u32.to_le_bytes());
        inputs[16..24].copy_from_slice(&(geo.as_ptr() as u64).to_le_bytes());
        // SAFETY: both structures live on this stack frame.
        let b = unsafe { parse(0xabc0, 0xdef0, inputs.as_ptr()) };
        let Some(victory_gpu::native_rt::Build::Bottom {
            dest,
            source,
            flags,
            geometries,
        }) = b
        else {
            panic!("not a bottom level: {b:?}");
        };
        assert_eq!((dest, source, flags, geometries.len()), (0xabc0, 0xdef0, 4, 2));
        let g = geometries[1];
        assert!(g.opaque);
        assert_eq!(
            (g.transform, g.index_buffer, g.vertex_buffer, g.stride),
            (0x1001, 0x2001, 0x3001, 12)
        );
        assert_eq!((g.index_format, g.vertex_format), (57, 6));
        assert_eq!((g.index_count, g.vertex_count), (36, 8));
    }

    /// `VICTORY_BVH_PREMAP`: a build description names its destination,
    /// source and scratch, and a top level its instances or a bottom level
    /// each triangle geometry's transform, indices and vertices.
    #[test]
    fn a_build_names_its_ranges() {
        let mut geo = [0u8; GEOMETRY_SIZE * 2];
        for (i, g) in geo.chunks_mut(GEOMETRY_SIZE).enumerate() {
            let i = i as u64;
            g[8..16].copy_from_slice(&(0x1000 + i).to_le_bytes());
            g[32..40].copy_from_slice(&(0x2000 + i).to_le_bytes());
            g[40..48].copy_from_slice(&(0x3000 + i).to_le_bytes());
        }
        geo[GEOMETRY_SIZE..GEOMETRY_SIZE + 4].copy_from_slice(&1u32.to_le_bytes()); // boxes
        let mut desc = [0u8; BUILD_DESC_SIZE];
        desc[0..8].copy_from_slice(&0xa000u64.to_le_bytes());
        desc[8..12].copy_from_slice(&1u32.to_le_bytes()); // bottom
        desc[16..20].copy_from_slice(&2u32.to_le_bytes());
        desc[24..32].copy_from_slice(&(geo.as_ptr() as u64).to_le_bytes());
        desc[32..40].copy_from_slice(&0xb000u64.to_le_bytes());
        desc[40..48].copy_from_slice(&0xc000u64.to_le_bytes());
        let mut out = Vec::new();
        // SAFETY: the description and its geometries live on this frame.
        unsafe { build_ranges(desc.as_ptr(), &mut out) };
        assert_eq!(out, [0xa000, 0xb000, 0xc000, 0x1000, 0x2000, 0x3000]);
        desc[8..12].copy_from_slice(&0u32.to_le_bytes()); // top
        desc[24..32].copy_from_slice(&0x5000u64.to_le_bytes());
        out.clear();
        // SAFETY: as above.
        unsafe { build_ranges(desc.as_ptr(), &mut out) };
        assert_eq!(out, [0xa000, 0xb000, 0xc000, 0x5000]);
        // SAFETY: as above.
        assert_eq!(unsafe { build_outputs(desc.as_ptr()) }, [0xa000, 0xc000]);
    }

    /// A geometry that is not triangles (boxes) is not taken.
    #[test]
    fn refuses_boxes_and_top_pointer_layouts() {
        let mut geo = [0u8; GEOMETRY_SIZE];
        geo[0..4].copy_from_slice(&1u32.to_le_bytes()); // procedural AABBs
        let mut inputs = [0u8; INPUTS_SIZE];
        inputs[0..4].copy_from_slice(&1u32.to_le_bytes());
        inputs[8..12].copy_from_slice(&1u32.to_le_bytes());
        inputs[16..24].copy_from_slice(&(geo.as_ptr() as u64).to_le_bytes());
        // SAFETY: both structures live on this stack frame.
        assert!(unsafe { parse(0x10, 0, inputs.as_ptr()) }.is_none());
        // Top level with an array of pointers: not taken.
        let mut top = [0u8; INPUTS_SIZE];
        top[8..12].copy_from_slice(&3u32.to_le_bytes());
        top[12..16].copy_from_slice(&1u32.to_le_bytes());
        // SAFETY: as above.
        assert!(unsafe { parse(0x20, 0, top.as_ptr()) }.is_none());
        // Top level as an array: the instances' address and count.
        top[12..16].copy_from_slice(&0u32.to_le_bytes());
        top[16..24].copy_from_slice(&0x5000u64.to_le_bytes());
        // SAFETY: as above.
        let t = unsafe { parse(0x20, 0x20, top.as_ptr()) };
        assert!(matches!(
            t,
            Some(victory_gpu::native_rt::Build::Top {
                dest: 0x20,
                source: 0x20,
                count: 3,
                instances: 0x5000,
                ..
            })
        ));
    }
}
