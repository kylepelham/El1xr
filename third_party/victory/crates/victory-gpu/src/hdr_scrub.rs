//! `VICTORY_HDR_SCRUB` (after elaboratecl/VIctory-AMD-4K `221ece13e1`): after each dispatch of
//! the title's HDR scene-colour programs ([`NAMES`]: deferred lighting, temporal resolve, TAA,
//! bloom, luminance, transmission), a compute pass of ours goes over every 2D float image the
//! program wrote (R11G11B10, RGBA16F/32F, R16F/R32F) and counts its NaN and infinite texels.
//!
//! - `1`: count only. Nothing the title sees changes. A frame with any is logged
//!   (`hdr scrub: frame N ...`, by program), and every 600 frames a summary, so a run tells
//!   whether NaNs appear on the frames of the one-frame black flicker (`flicker catch: frame N`
//!   uses the same frame numbers).
//! - `2`: count, and a NaN or infinite channel is written back as 0. The source's peak clamp
//!   (16) and zeroing of negatives are left out: they change the look of every bright pixel.
//!
//! The counts go into a ring of [`RING`] frames in a buffer the CPU reads; a frame's are read
//! [`LAG`] presents after it, when its GPU work is done.
//!
//! The original also scrubbed through a live config key and a peak cap; neither is ported.
//!
//! `VICTORY_HDR_SCRUB_POST=1` (with the above): the title's tiled post passes over the scene
//! colour are counted too ([`POST`]: the window-glass transmission blur pyramid and its depth
//! tiles, the depth of field's prefilter and blur, the motion blur downsample), each by name,
//! so a frame's line says which of them first wrote a NaN or an infinity (#96: soft dark
//! squares over the bright window). Meant with `VICTORY_NAN_SCRUB=0 VICTORY_NAN_GUARD=0`:
//! with either on, the title's shaders make a NaN or infinity 0 (or 65504) before they store
//! it and there is nothing left to count.
//!
//! `VICTORY_HDR_SCRUB_GLASS=1` (with `VICTORY_HDR_SCRUB`; the post passes too): also counts the
//! glass chain around the alpha trace `6d7fe7745ab9c6d8` and the transmission pyramid's first
//! stage ([`GLASS`]: `CS_AlphTransmissionPrefilterMip`, which the post names miss by the title's
//! spelling, the alpha fallbacks, ray miss, screen-space resolve, budgeting fill, alpha shade and
//! filters, the reflection spatial filters and the caustics resolve the trace reads), and each
//! frame line ends `first=<program>`: of the counted programs, the first in the card's order to
//! leave a NaN or infinity in an image that frame (the count after each dispatch takes the slot
//! only if no earlier one did). Candidates for `VICTORY_CS_STORE_CLAMP_ONLY` (#96).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Mutex, OnceLock};

use windows::Win32::Graphics::Direct3D12::*;
use windows::core::s;

/// The title's programs whose images are counted, by the name in their container: the first
/// [`BASE`] always, [`POST`] with `VICTORY_HDR_SCRUB_POST=1`, [`GLASS`] with
/// `VICTORY_HDR_SCRUB_GLASS=1`.
pub(crate) const NAMES: [&str; 26] = [
    "CS_ApplyCombinedDeferred",
    "CS_SceneColorTemporalResolve",
    "CS_TAA2_Resolve",
    "CS_BloomDownsampleKaris",
    "CS_ComputeLuminanceBilateralGrid",
    "CS_TransmissionVolumetrics",
    "CS_AlphaTransmissionDepthTile",
    "CS_AlphaTransmissionBlurGaussian",
    "CS_AlphaTransmissionDenoise",
    "CS_AlphaTransmissionHistoryResolve",
    "CS_GatherAlphaVBuffer",
    "CS_PrefilterBoundaryColor",
    "CS_MainBlur",
    "CS_MotionBlurDownSample",
    "CS_BilateralBlur",
    "CS_AlphTransmissionPrefilterMip",
    "CS_DeferredFallbacksAlpha",
    "CS_RayMiss_",
    "CS_ResolveScreenSpace",
    "CS_DummyResolve",
    "CS_RayBudgeting_Fill",
    "CS_ShadeAlpha",
    "CS_VarianceFilterAlpha",
    "CS_SpatialFilterAlpha",
    "CS_SpatialFilter_",
    "CS_ScreenProbes_TemporalResolve_ReflectiveCaustics",
];
/// The names counted without `VICTORY_HDR_SCRUB_POST`.
const BASE: usize = 6;
/// The end of the tiled post passes in [`NAMES`].
const POST_END: usize = 15;
/// The tiled post passes of [`NAMES`].
pub(crate) const POST: &[&str] = NAMES.split_at(POST_END).0.split_at(BASE).1;
/// The glass chain of [`NAMES`].
pub(crate) const GLASS: &[&str] = NAMES.split_at(POST_END).1;

/// `VICTORY_HDR_SCRUB_GLASS=1`: [`POST`] and [`GLASS`] counted, the first writer named. Read
/// once.
fn glass() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        let on = std::env::var("VICTORY_HDR_SCRUB_GLASS").is_ok_and(|v| v.trim() == "1");
        if on && mode() != 0 {
            tracing::info!(
                "HDR scrub counts the glass chain too and names each frame's first NaN/inf writer: {GLASS:?} (VICTORY_HDR_SCRUB_GLASS)"
            );
        }
        on
    })
}

/// How many of [`NAMES`] are counted.
fn counted() -> usize {
    if glass() {
        NAMES.len()
    } else if post() {
        POST_END
    } else {
        BASE
    }
}

/// `VICTORY_HDR_SCRUB_POST=1`: [`POST`] counted too. Read once.
fn post() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        let on = std::env::var("VICTORY_HDR_SCRUB_POST").is_ok_and(|v| v.trim() == "1");
        if on && mode() != 0 {
            tracing::info!(
                "HDR scrub counts the tiled post passes too: {POST:?} (VICTORY_HDR_SCRUB_POST)"
            );
        }
        on
    })
}
/// Frames in the counts' ring.
const RING: u64 = 8;
/// Presents after a frame that its counts are read.
const LAG: u64 = 5;
/// Bytes of the counts: per frame of the ring, per program, NaN and infinite texels; then per
/// frame of the ring the first program (its place in [`NAMES`] + 1) that had any.
const COUNT_BYTES: u64 = RING * NAMES.len() as u64 * 8 + RING * 4;

/// The byte offset of the first-writer word of ring frame `slot`.
fn first_at(slot: u64) -> u32 {
    (RING * NAMES.len() as u64 * 8 + slot * 4) as u32
}
/// Frame lines logged at most.
const LOGGED: u64 = 2000;
/// Frames between the summaries.
const TOLD_EVERY: u64 = 600;

/// The switch: 0 off, 1 count, 2 count and scrub. Read once.
pub(crate) fn mode() -> u8 {
    static MODE: OnceLock<u8> = OnceLock::new();
    *MODE.get_or_init(|| {
        let mode = parse(std::env::var("VICTORY_HDR_SCRUB").ok().as_deref());
        if mode != 0 {
            tracing::info!(
                mode,
                "HDR images counted for NaN and infinity after the scene-colour programs (VICTORY_HDR_SCRUB; 2: scrubbed too)"
            );
        }
        mode
    })
}

fn parse(value: Option<&str>) -> u8 {
    match value.map(str::trim) {
        Some("1") => 1,
        Some("2") => 2,
        _ => 0,
    }
}

/// The program `container` is, of [`NAMES`].
pub(crate) fn pass_of(container: &[u8]) -> Option<u8> {
    pass_among(container, counted())
}

/// The program `container` is, of the first `names` of [`NAMES`].
fn pass_among(container: &[u8], names: usize) -> Option<u8> {
    NAMES[..names]
        .iter()
        .position(|name| {
            let name = name.as_bytes();
            container.windows(name.len()).any(|w| w == name)
        })
        .map(|i| i as u8)
}

/// The element of an image of host format `format` the pass reads, if it reads that format.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Kind {
    /// R11G11B10_FLOAT.
    Float3 = 0,
    /// R16G16B16A16_FLOAT, R32G32B32A32_FLOAT.
    Float4 = 1,
    /// R32_FLOAT, R16_FLOAT.
    Float1 = 2,
}

pub(crate) fn kind_of(format: i32) -> Option<Kind> {
    match format {
        26 => Some(Kind::Float3),
        10 | 2 => Some(Kind::Float4),
        41 | 54 => Some(Kind::Float1),
        _ => None,
    }
}

const SHADER: &str = r#"
#define RS "RootConstants(num32BitConstants = 6, b0), DescriptorTable(UAV(u0)), UAV(u1)"
cbuffer Params : register(b0) { uint width; uint height; uint at; uint scrub; uint first_at; uint tag; };
#if defined(F3)
#define T float3
#elif defined(F4)
#define T float4
#else
#define T float
#endif
RWTexture2D<T> image : register(u0);
RWByteAddressBuffer counts : register(u1);
groupshared uint found[2];

// 2: NaN, 1: infinite, 0: a number. On the bits, so no compiler folds it away.
uint bad(float v) {
    uint m = asuint(v) & 0x7fffffffu;
    return m > 0x7f800000u ? 2u : (m == 0x7f800000u ? 1u : 0u);
}
float fixed(float v) {
    return bad(v) != 0 ? 0.0 : v;
}

[RootSignature(RS)]
[numthreads(8, 8, 1)]
void main(uint3 id : SV_DispatchThreadID, uint index : SV_GroupIndex) {
    if (index == 0) {
        found[0] = 0;
        found[1] = 0;
    }
    GroupMemoryBarrierWithGroupSync();
    if (id.x < width && id.y < height) {
        T c = image[id.xy];
#if defined(F3)
        uint b = max(bad(c.x), max(bad(c.y), bad(c.z)));
#elif defined(F4)
        uint b = max(max(bad(c.x), bad(c.y)), max(bad(c.z), bad(c.w)));
#else
        uint b = bad(c);
#endif
        if (b != 0) {
            uint was;
            InterlockedAdd(found[2 - b], 1u, was);
            if (scrub != 0) {
#if defined(F3)
                image[id.xy] = float3(fixed(c.x), fixed(c.y), fixed(c.z));
#elif defined(F4)
                image[id.xy] = float4(fixed(c.x), fixed(c.y), fixed(c.z), fixed(c.w));
#else
                image[id.xy] = 0.0;
#endif
            }
        }
    }
    GroupMemoryBarrierWithGroupSync();
    if (index == 0) {
        uint was;
        if (found[0] != 0) {
            counts.InterlockedAdd(at * 8, found[0], was);
        }
        if (found[1] != 0) {
            counts.InterlockedAdd(at * 8 + 4, found[1], was);
        }
        if (found[0] != 0 || found[1] != 0) {
            counts.InterlockedCompareExchange(first_at, 0u, tag, was);
        }
    }
}
"#;

struct Pass {
    root: ID3D12RootSignature,
    /// By [`Kind`].
    pipelines: [ID3D12PipelineState; 3],
}

impl Pass {
    fn new(device: &ID3D12Device) -> windows::core::Result<Pass> {
        let mut root: Option<ID3D12RootSignature> = None;
        let mut pipeline = |define: &str| -> windows::core::Result<ID3D12PipelineState> {
            let code = crate::host::compile(
                &format!("#define {define}\n{SHADER}"),
                s!("main"),
                s!("cs_5_1"),
            )?;
            // SAFETY: the root signature is inside the shader; plain creations.
            unsafe {
                if root.is_none() {
                    root = Some(device.CreateRootSignature(0, &code)?);
                }
                device.CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                    pRootSignature: std::mem::transmute_copy(&root),
                    CS: D3D12_SHADER_BYTECODE {
                        pShaderBytecode: code.as_ptr().cast(),
                        BytecodeLength: code.len(),
                    },
                    ..Default::default()
                })
            }
        };
        let pipelines = [pipeline("F3")?, pipeline("F4")?, pipeline("F1")?];
        Ok(Pass {
            root: root.ok_or_else(windows::core::Error::empty)?,
            pipelines,
        })
    }

    /// Records the pass over `image` (a view in the bound shader-visible heap of an image of
    /// `kind`, `size`, in the unordered access state), its counts into `counts` (a buffer
    /// address) at entry `at`; `tag` goes into the word at byte `first` if that is still 0 and
    /// the pass found any.
    #[allow(clippy::too_many_arguments)]
    fn record(
        &self,
        list: &ID3D12GraphicsCommandList,
        kind: Kind,
        image: D3D12_GPU_DESCRIPTOR_HANDLE,
        size: [u32; 2],
        at: u32,
        scrub: bool,
        counts: u64,
        first: u32,
        tag: u32,
    ) {
        let words = [size[0], size[1], at, u32::from(scrub), first, tag];
        let barrier = D3D12_RESOURCE_BARRIER {
            Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
            ..Default::default()
        };
        // SAFETY: an open list with the image's heap bound; our own objects.
        unsafe {
            list.ResourceBarrier(std::slice::from_ref(&barrier));
            list.SetComputeRootSignature(&self.root);
            list.SetComputeRoot32BitConstants(0, words.len() as u32, words.as_ptr().cast(), 0);
            list.SetComputeRootDescriptorTable(1, image);
            list.SetComputeRootUnorderedAccessView(2, counts);
            list.SetPipelineState(&self.pipelines[kind as usize]);
            list.Dispatch(size[0].div_ceil(8), size[1].div_ceil(8), 1);
            list.ResourceBarrier(std::slice::from_ref(&barrier));
        }
    }
}

struct Made {
    pass: Pass,
    counts: ID3D12Resource,
    words: *mut u32,
}

// SAFETY: D3D12 objects are free-threaded; `words` is the mapped counts buffer, which lives as
// long as the process.
unsafe impl Send for Made {}
unsafe impl Sync for Made {}

fn made() -> Option<&'static Made> {
    static MADE: OnceLock<Option<Made>> = OnceLock::new();
    MADE.get_or_init(|| {
        let host = crate::host::host()?;
        let pass = Pass::new(&host.device)
            .inspect_err(|e| tracing::warn!(error = %e, "no HDR scrub pass (VICTORY_HDR_SCRUB)"))
            .ok()?;
        let (counts, words) = host.shared_buffer(COUNT_BYTES)?;
        // SAFETY: a mapped buffer of that size, nothing runs on it yet.
        unsafe { std::ptr::write_bytes(words, 0, COUNT_BYTES as usize) };
        Some(Made {
            pass,
            counts,
            words: words.cast(),
        })
    })
    .as_ref()
}

/// Images counted (by program), and views passed over: (program, format) -> times.
static RUN: [AtomicU64; NAMES.len()] = [const { AtomicU64::new(0) }; NAMES.len()];
static SKIPPED: Mutex<Option<HashMap<(u8, i32), u64>>> = Mutex::new(None);
/// Since the start: NaN and infinite texels by program, frames with any, lines logged.
static TOTAL: Mutex<[[u64; 2]; NAMES.len()]> = Mutex::new([[0; 2]; NAMES.len()]);
static BAD_FRAMES: AtomicU64 = AtomicU64::new(0);
static LINES: AtomicU64 = AtomicU64::new(0);

/// After a dispatch of program `pass`: counts (and with `2` scrubs) its image `image` of
/// `kind`, `size`, a view in the bound heap. Whether it recorded anything (the compute root
/// signature and pipeline are then ours).
pub(crate) fn run(
    list: &crate::host::HostList,
    pass: u8,
    kind: Kind,
    image: D3D12_GPU_DESCRIPTOR_HANDLE,
    size: [u32; 2],
) -> bool {
    if size[0] == 0 || size[1] == 0 {
        return false;
    }
    let Some(made) = made() else {
        return false;
    };
    let frame = crate::queue::presented() + 1;
    let at = (frame % RING) as u32 * NAMES.len() as u32 + u32::from(pass);
    list.keep(&made.counts);
    // SAFETY: a live buffer.
    let counts = unsafe { made.counts.GetGPUVirtualAddress() };
    let first = first_at(frame % RING);
    made.pass.record(
        &list.list,
        kind,
        image,
        size,
        at,
        mode() == 2,
        counts,
        first,
        u32::from(pass) + 1,
    );
    RUN[usize::from(pass)].fetch_add(1, Relaxed);
    true
}

/// A view of program `pass`'s that the pass does not read (format `format`; -1: not an
/// image of a resource object).
pub(crate) fn skipped(pass: u8, format: i32) {
    let mut skipped = SKIPPED.lock().unwrap_or_else(|e| e.into_inner());
    *skipped
        .get_or_insert_with(HashMap::new)
        .entry((pass, format))
        .or_default() += 1;
}

/// At the present of frame `frame`: the counts of frame `frame - LAG` are read, logged and
/// cleared for the ring's next round.
pub(crate) fn presented(frame: u64) {
    if mode() == 0 || frame <= LAG {
        return;
    }
    let Some(made) = made() else {
        return;
    };
    let of = frame - LAG;
    let first = (of % RING) as usize * NAMES.len();
    let mut found = [[0u32; 2]; NAMES.len()];
    // SAFETY: the mapped counts; frame `of`'s work is done and no list of a frame being
    // recorded writes these words.
    let writer = unsafe {
        for (i, f) in found.iter_mut().enumerate() {
            for (k, n) in f.iter_mut().enumerate() {
                let word = made.words.add((first + i) * 2 + k);
                *n = word.read_volatile();
                word.write_volatile(0);
            }
        }
        let word = made.words.add(first_at(of % RING) as usize / 4);
        let writer = word.read_volatile();
        word.write_volatile(0);
        writer
    };
    if found.iter().any(|f| f[0] != 0 || f[1] != 0) {
        let mut total = TOTAL.lock().unwrap_or_else(|e| e.into_inner());
        for (t, f) in total.iter_mut().zip(found) {
            t[0] += u64::from(f[0]);
            t[1] += u64::from(f[1]);
        }
        drop(total);
        BAD_FRAMES.fetch_add(1, Relaxed);
        if LINES.fetch_add(1, Relaxed) < LOGGED {
            let writer = if glass() { writer } else { 0 };
            tracing::info!("hdr scrub: frame {of} {}", line(&found, writer));
        }
    }
    if frame.is_multiple_of(TOLD_EVERY) {
        summary(frame);
    }
}

/// The programs with NaN or infinite texels in `found`, and the first to write one (`writer`:
/// its place in [`NAMES`] + 1; 0: not named).
fn line(found: &[[u32; 2]; NAMES.len()], writer: u32) -> String {
    let mut parts = Vec::new();
    for (name, f) in NAMES.iter().zip(found) {
        if f[0] != 0 || f[1] != 0 {
            parts.push(format!("{name} nan={} inf={}", f[0], f[1]));
        }
    }
    let first = (writer as usize)
        .checked_sub(1)
        .and_then(|i| NAMES.get(i))
        .map(|name| format!(" first={name}"))
        .unwrap_or_default();
    format!("NaN/inf texels: {}{first}", parts.join(", "))
}

fn summary(frame: u64) {
    let total = *TOTAL.lock().unwrap_or_else(|e| e.into_inner());
    let per = NAMES
        .iter()
        .zip(&total)
        .zip(&RUN)
        .take(counted())
        .map(|((name, t), run)| {
            format!(
                "{name} images={} nan={} inf={}",
                run.load(Relaxed),
                t[0],
                t[1]
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    let skipped = SKIPPED.lock().unwrap_or_else(|e| e.into_inner());
    let mut skipped: Vec<_> = skipped.iter().flatten().collect();
    skipped.sort_unstable();
    let skipped = skipped
        .iter()
        .map(|((pass, format), n)| format!("{} format {format} x{n}", NAMES[usize::from(*pass)]))
        .collect::<Vec<_>>()
        .join(", ");
    tracing::info!(
        "hdr scrub, since the start (frame {frame}): {} frames with NaN/inf; {per}; views not read: [{skipped}]",
        BAD_FRAMES.load(Relaxed)
    );
}

#[cfg(test)]
mod tests;
