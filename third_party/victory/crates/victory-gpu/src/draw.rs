//! Draws on the host GPU (decision 0007, step 3). A list keeps the state
//! the title sets for its draws (pipeline, descriptor heaps, root
//! arguments, vertex and index buffers, targets); each draw then brings the
//! host list up to date: the host pipeline and our root signature, the
//! user data the translated shaders read, the host copies of the buffers
//! and descriptors they read, and the states of the textures involved.
//! State the host keeps across draws (viewports, scissors, blend factor,
//! stencil reference, topology, targets) is set on the host list as the
//! title sets it.

mod root_slots;
mod table_plan;
#[cfg(windows)]
pub(crate) use table_plan::TablePlan;
#[cfg(all(test, windows))]
mod texture_read_tests;

use std::sync::atomic::{AtomicU64, Ordering};

/// Direct dispatches of CS_DefragHeap_ComputeRelocations, the groups they named,
/// and indirect dispatches whose group count is not known here.
static DEFRAG_DISPATCHES: AtomicU64 = AtomicU64::new(0);
static DEFRAG_GROUPS: AtomicU64 = AtomicU64::new(0);
static DEFRAG_INDIRECT: AtomicU64 = AtomicU64::new(0);

const DEFRAG_HASH: &str = "02384a60fda6f2d0";

pub(crate) fn defrag_counts() -> (u64, u64, u64) {
    (
        DEFRAG_DISPATCHES.load(Ordering::Relaxed),
        DEFRAG_GROUPS.load(Ordering::Relaxed),
        DEFRAG_INDIRECT.load(Ordering::Relaxed),
    )
}

fn note_defrag(p: &HostPipeline, groups: Option<u64>) {
    let Some(stage) = p.stages.first() else {
        return;
    };
    if stage.shader.hash != DEFRAG_HASH {
        return;
    }
    if let Some(groups) = groups {
        DEFRAG_DISPATCHES.fetch_add(1, Ordering::Relaxed);
        DEFRAG_GROUPS.fetch_add(groups, Ordering::Relaxed);
    } else {
        DEFRAG_INDIRECT.fetch_add(1, Ordering::Relaxed);
    }
}
use victory_shader::{ResourceType, RootType, UsedResource};

use crate::descriptor::View;
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT;
use windows::core::Interface;

use crate::host::HostList;
use crate::object::This;
use crate::pipeline::host::HostPipeline;

/// A root argument as the title set it.
#[derive(Debug, Clone, Default)]
enum Root {
    #[default]
    None,
    /// A descriptor table's first descriptor (an address in a heap).
    Table(u64),
    Constants(Vec<u32>),
    /// A root view's GPU address.
    View(u64),
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct VertexView {
    address: u64,
    size: u32,
    stride: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct IndexView {
    address: u64,
    size: u32,
    format: u32,
}

/// Host states of a depth target, and of a texture shaders read.
const DEPTH_WRITE: u32 = 0x10;
const DEPTH_READ: u32 = 0x20;
const SHADER_RESOURCE: u32 = 0x40 | 0x80;

thread_local! {
    /// While a draw is prepared (`State::bind`, then its targets' states):
    /// the depth target the draw only tests (0: none, or one it writes),
    /// and whether a view its shaders read is of that same texture.
    static TESTED_DEPTH: std::cell::Cell<(usize, bool)> =
        const { std::cell::Cell::new((0, false)) };
    /// While a stage's tables are walked (`State::tables`): whether that
    /// stage's shader can store to buffers (`Translated::stores`).
    static STORES: std::cell::Cell<bool> = const { std::cell::Cell::new(true) };
    /// While `State::early_textures` runs the texture pass: the
    /// milliseconds and the bytes that run may take, in place of the
    /// pass's own per frame, and whether what it uploads must be there now
    /// (a bake draw's: not handed to the prepare threads,
    /// `crate::texture::prepare`).
    static PASS_BUDGET: std::cell::Cell<Option<(u64, u64, bool)>> =
        const { std::cell::Cell::new(None) };
}

/// The milliseconds of every frame the pass that uploads textures picked
/// by index takes before a shader that picks by index has asked for it
/// (`VICTORY_EARLY_TEXTURES_MS`, 12; 0: never, as before), and the
/// milliseconds it may take just before a draw of the title's decoration
/// bake (`VICTORY_BAKE_TEXTURES_MS`, 1500; 0: nothing special).
///
/// The pass ran only from the first world frame on, where it met some
/// 24,000 slots at once and needed 60-80 frames for them. The title bakes
/// its characters' skin and decorations into textures of its own while it
/// loads, 159-1,165 frames before that (runs 156-174), with a pixel shader
/// that picks its sources by index: whatever was not on the card then was
/// baked as nothing, once, for the run. On the console a texture is there
/// when its bytes are.
/// Maximum draws in the previous frame for a frame to be treated as a
/// loading-screen / low-draw 2D frame eligible for texture pre-draining.
const LOADING_DRAIN_MAX_DRAWS: u64 = 150;
/// Default time budget (ms) for a loading-screen texture pre-drain pass,
/// safely below the Windows 2-second TDR limit.
const LOADING_DRAIN_MS: u64 = 400;
/// Maximum time budget (ms) for an early / loading-screen texture pass,
/// keeping a wide margin below the 2,000 ms Windows TDR watchdog.
const LOADING_DRAIN_MAX_MS: u64 = 1500;
/// Default byte budget (256 MiB) for a loading-screen texture pre-drain pass.
const LOADING_DRAIN_BYTES: u64 = 256 << 20;
/// Default byte budget (64 MiB) for a normal `early_textures` pass.
const EARLY_PASS_DEFAULT_BYTES: u64 = 64 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EarlyPassConfig {
    /// `VICTORY_EARLY_TEXTURES_MS` (default 40; 0 disables normal early passes).
    early_ms: u64,
    /// `VICTORY_BAKE_TEXTURES_MS` if explicitly set (`None`: 10_000 ms during
    /// initial loading passes, 250 ms once steady gameplay begins).
    bake_ms: Option<u64>,
    /// `VICTORY_GAMEPLAY_TEXTURES_MS` (default 3 ms during steady 3D gameplay).
    gameplay_ms: u64,
    /// `VICTORY_TEXTURE_PASS_MB` in bytes, if configured.
    pass_bytes_override: Option<u64>,
    /// `VICTORY_LOADING_DRAIN` (enabled by default; `0` disables).
    loading_drain: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EarlyPassInput {
    first_in_frame: bool,
    is_bake: bool,
    bake_ms_left: u64,
    prev_frame_draws: u64,
    has_due_textures: bool,
    world_gameplay: bool,
    texture_budget_remaining: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EarlyPassKind {
    Normal,
    LoadingDrain,
    Bake,
}

fn parse_mb_bytes(val: &str) -> Option<u64> {
    val.trim()
        .parse::<u64>()
        .ok()
        .map(|mb| mb.min(u64::MAX >> 20) << 20)
}

fn early_pass_config() -> EarlyPassConfig {
    static CFG: std::sync::OnceLock<EarlyPassConfig> = std::sync::OnceLock::new();
    *CFG.get_or_init(|| {
        let ms = |name: &str, default: u64| {
            std::env::var(name)
                .ok()
                .and_then(|v| v.trim().parse::<u64>().ok())
                .unwrap_or(default)
        };
        // (Neptune) 40 and 10,000 (they were 12 and 1,500): the first bake draws
        // of a launch met 9,371 textures still due, and 1.5 s uploaded some 4,000
        // of them; what was missing was baked as nothing (a head left black, a
        // hand of noise). With these the first bakes met 504.
        // Once the world is running (`HEAP_BUFFER_PASSES > 0`), regular non-bake
        // early texture uploads are capped to `VICTORY_GAMEPLAY_TEXTURES_MS` (3 ms)
        // so gameplay frames do not stall on `early_textures`. After initial
        // loading passes (`HEAP_BUFFER_PASSES > 60`), default bake uploads are
        // capped to 250 ms unless `VICTORY_BAKE_TEXTURES_MS` is set explicitly.
        let bake_ms = std::env::var("VICTORY_BAKE_TEXTURES_MS")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok());
        EarlyPassConfig {
            early_ms: ms("VICTORY_EARLY_TEXTURES_MS", 40),
            bake_ms,
            gameplay_ms: ms("VICTORY_GAMEPLAY_TEXTURES_MS", 3),
            pass_bytes_override: std::env::var("VICTORY_TEXTURE_PASS_MB")
                .ok()
                .as_deref()
                .and_then(parse_mb_bytes),
            loading_drain: std::env::var_os("VICTORY_LOADING_DRAIN").is_none_or(|v| v != "0"),
        }
    })
}

/// Computes the `(ms, bytes, now)` `PASS_BUDGET` for `State::early_textures`.
///
/// During low-draw frames (e.g. loading screens / 2D menus where the
/// previous frame had fewer than [`LOADING_DRAIN_MAX_DRAWS`] draws and the
/// `textures picked by index` due queue is non-empty), the pass budget is
/// boosted up to [`LOADING_DRAIN_BYTES`] / [`LOADING_DRAIN_MS`] (or
/// `VICTORY_TEXTURE_PASS_MB` when set), capped below the Windows 2-second
/// TDR limit and by `VICTORY_TEXTURE_BUDGET_MB` when configured.
fn compute_early_pass_budget(
    cfg: EarlyPassConfig,
    input: EarlyPassInput,
) -> Option<(EarlyPassKind, (u64, u64, bool))> {
    if input.is_bake && input.bake_ms_left > 0 {
        // What a bake may read must be there now: up to 2 GiB.
        return Some((EarlyPassKind::Bake, (input.bake_ms_left, 2 << 30, true)));
    }
    if !input.first_in_frame {
        return None;
    }
    let loading_boost =
        cfg.loading_drain && input.prev_frame_draws < LOADING_DRAIN_MAX_DRAWS && input.has_due_textures;
    let (kind, ms, mut bytes) = if loading_boost {
        (
            EarlyPassKind::LoadingDrain,
            LOADING_DRAIN_MS.max(cfg.early_ms).min(LOADING_DRAIN_MAX_MS),
            cfg.pass_bytes_override.unwrap_or(LOADING_DRAIN_BYTES),
        )
    } else if cfg.early_ms > 0 {
        let ms = if input.world_gameplay && input.prev_frame_draws >= LOADING_DRAIN_MAX_DRAWS {
            cfg.early_ms.min(cfg.gameplay_ms)
        } else {
            cfg.early_ms.min(LOADING_DRAIN_MAX_MS)
        };
        if ms == 0 {
            return None;
        }
        (
            EarlyPassKind::Normal,
            ms,
            cfg.pass_bytes_override.unwrap_or(EARLY_PASS_DEFAULT_BYTES),
        )
    } else {
        return None;
    };
    if let Some(remaining) = input.texture_budget_remaining {
        bytes = bytes.min(remaining);
    }
    if bytes == 0 {
        return None;
    }
    Some((kind, (ms, bytes, false)))
}

/// Wall-clock budget (ms) for draining already-prepared `Slot::Ready` textures
/// under the default smooth gameplay cap (`VICTORY_TEXTURE_PREPARE_THREADS > 0`).
/// Each ready texture already has its host copy and detiled staging buffer in
/// memory and only records `CopyTextureRegion` (< 0.05 ms), so draining bursts
/// promptly frees prepare-queue headroom before `prune()` discards them.
const READY_DRAIN_MS: u64 = 12;

fn compute_ready_drain_budget(
    budget: std::time::Duration,
    default_smooth_pass: bool,
) -> std::time::Duration {
    if default_smooth_pass && !budget.is_zero() {
        budget.max(std::time::Duration::from_millis(READY_DRAIN_MS))
    } else {
        budget
    }
}

/// Tracks previous-frame and current-frame host draw counts for `frame`.
fn note_frame_draws(frame: u64) -> (u64, u64) {
    static TRACKED_FRAME: AtomicU64 = AtomicU64::new(u64::MAX);
    static FRAME_START_DRAWN: AtomicU64 = AtomicU64::new(0);
    static PREV_FRAME_DRAWS: AtomicU64 = AtomicU64::new(0);

    let total_drawn = drawn();
    let prev_frame = TRACKED_FRAME.load(Ordering::Relaxed);
    if prev_frame != frame
        && TRACKED_FRAME
            .compare_exchange(prev_frame, frame, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
    {
        let prev_start = FRAME_START_DRAWN.swap(total_drawn, Ordering::Relaxed);
        let avg_draws = if prev_frame == u64::MAX {
            0
        } else {
            let elapsed_frames = frame.saturating_sub(prev_frame).max(1);
            total_drawn.saturating_sub(prev_start) / elapsed_frames
        };
        PREV_FRAME_DRAWS.store(avg_draws, Ordering::Relaxed);
    }
    let prev = PREV_FRAME_DRAWS.load(Ordering::Relaxed);
    let current = total_drawn.saturating_sub(FRAME_START_DRAWN.load(Ordering::Relaxed));
    (prev, current)
}

/// The pixel shaders of the title's decoration bake (the title:
/// `PS_BakeDecorations_*` and `PS_DecorationBlend_*`), by the first 16 hex
/// digits of their hash as the frame listings print it. They draw into a
/// 256x256 corner of 2048x2048 targets and name no texture in a table.
pub(crate) const BAKE_PIXEL_SHADERS: [&str; 7] = [
    "c0a7fe77b1862e10",
    "0d1575a396dfe513",
    "b7cfd8dda91ebb6b",
    "280967ad158cee0b",
    "d126bcae5d5d8d26",
    "fcee612943c008e0",
    "fa455c8a3c32577b",
];

/// Since the start: early runs of the texture pass, and those of them
/// made for a bake draw.
pub(crate) static EARLY_TEXTURE_PASSES: [AtomicU64; 2] = [AtomicU64::new(0), AtomicU64::new(0)];

/// Whether a buffer range a shader names through an unordered access view
/// is given to shaders as written (snapshot before the submission, compare
/// and write back after) only if that shader's code can store to buffers.
/// The title binds buffers its shaders only read through such views, and
/// every range declared costs a snapshot and a compare of its bytes: 3.97
/// GB a frame in 31 submissions, 0.49 s of snapshots and 0.28 s of
/// comparing (run 28). On by default; `VICTORY_WRITE_SKIP=0` declares
/// every such range, as before.
fn write_skip() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_WRITE_SKIP").is_none_or(|v| v != "0"))
}

/// A run of indirect records clamped in one pass ([`windows`]): the host
/// copy they came from (its number), their kind and stride, the offset of
/// the first in that copy, how many, and the list's scratch buffer that
/// holds them clamped, record `i` at `i * step`.
#[derive(Clone)]
struct Window {
    copy: u64,
    kind: u32,
    stride: u32,
    /// How far apart the records are in the copy, and in the scratch
    /// buffer: what the title walks them by ([`windows`]).
    step: u32,
    base: u64,
    records: u32,
    scratch: ID3D12Resource,
}

/// The most records clamped in one pass ([`windows`]).
const WINDOW_MOST: u64 = 64;

/// Whether indirect draws that read record after record of one argument
/// buffer are clamped a window at a time (`VICTORY_INDIRECT_WINDOW=1`; off
/// by default).
///
/// The title issues its mesh draws one record per call, each from the next
/// slot of the same argument buffer: 3,460 a frame in gameplay, and before
/// each our clamp ran as a compute dispatch of its own between two waits,
/// so the card went from drawing to compute and back for every record. In
/// the timing run of the lit scene (run 79) a shadow draw of next to
/// nothing cost 14-16 microseconds, 4,126 records a frame.
///
/// With the switch, when a draw asks for a record a short step after the
/// one drawn before, the clamp takes that record and the ones that step
/// apart after it (16, then twice as many each time, [`WINDOW_MOST`] at
/// most, never past the buffer) into
/// the list's scratch buffer in one pass, and the draws that follow read
/// their record there with no pass of ours, until one of these: a compute
/// dispatch, a direct draw, an indirect call with a count buffer or more
/// than one record, a draw whose shaders can store to buffers, a record
/// outside the window, another host list.
///
/// The step is the one the title is seen to take, not the stride its
/// command signature names: it draws one record a call, for which the
/// stride means nothing, and the title names 12 bytes (the three
/// words of a mesh dispatch) for records it keeps 32 bytes apart (2,173 of
/// a frame's 3,349 mesh records in run 87's listed frame; 283 are 16
/// apart, which ends a window). Walked by the named stride the window
/// never opened: run 87, with the switch, still clamped 5,068 records a
/// frame one at a time.
///
/// What it costs the guard against garbage records: nothing against a
/// hang (every record is bounded as before and charged to the list's
/// budget as before), but the budget is charged when a window is clamped,
/// so also for records of the window's tail that are never drawn. Those
/// hold garbage in this title (slots of all ones: run 93 spent lists'
/// budgets on them and emptied the draws after), so in a window a record
/// that asks for more than can run is left empty and charges nothing
/// (`crate::indirect::bound_ahead`); a tail record that looks real is
/// charged what it asks, a few thousand of 2^27. And arguments the title's CPU side changes are seen as
/// before, at the next submission.
/// (Neptune) An experiment, `VICTORY_INDIRECT_RAW=1`: a mesh draw of one
/// record without a count buffer reads the title's own argument record,
/// with no clamp of ours before it. It measures what the clamp costs on
/// the card (a compute dispatch between two waits and the change from
/// drawing to compute and back, for each of some 5,000 records a frame).
/// What it gives up: a garbage record runs as it stands.
/// (Neptune) Whether the clamp of a single indirect record goes into its
/// list's prelude (`crate::indirect::record_ahead`); `VICTORY_CLAMP_AHEAD=0`:
/// in place before the draw, as before.
fn clamp_ahead() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_CLAMP_AHEAD").is_none_or(|v| v != "0"))
}

/// (Neptune) Since the last frame line: indirect records clamped ahead in
/// their lists' preludes, and clamped in place.
pub(crate) static CLAMPS: [AtomicU64; 3] =
    [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)];

/// (Neptune) Since they were last taken: tables made afresh that a draw found ready, found
/// made but made ready again, and made (and the slots made for them).
pub(crate) static TABLES: [AtomicU64; 9] = [const { AtomicU64::new(0) }; 9];

/// (Neptune) Whether a list's tables made afresh are told apart by their contents alone
/// (`VICTORY_TABLES_BY_CONTENT=0`: by their address too, as before).
fn tables_by_content() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_TABLES_BY_CONTENT").is_none_or(|v| v != "0"))
}

/// (Neptune) `VICTORY_INDIRECT_RAW=2`: see `State::execute_indirect`.
fn raw_all() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_INDIRECT_RAW").is_some_and(|v| v == "2"))
}

fn raw_indirect() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_INDIRECT_RAW").is_some_and(|v| v == "1"))
}

fn windows() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_INDIRECT_WINDOW").is_some_and(|v| v == "1"))
}

/// Whether redundant host binds (render targets, topology, viewports, scissors,
/// blend factor, stencil reference, depth bounds) are skipped when the host
/// list already holds the same state. On by default; `VICTORY_BIND_CACHE=0`
/// records every bind onto the host list.
fn bind_cache() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_BIND_CACHE").is_none_or(|v| v != "0"))
}

/// Resolved render-target and depth-stencil views currently bound on the host
/// list. Comparing both the descriptor values (`RtvSnapshot`) and the resolved
/// host texture pointer catches descriptors rewritten behind the same handle
/// as well as textures whose host copy appeared or changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct BoundTargets {
    rtv_count: u8,
    rtvs: [Option<(crate::descriptor::RtvSnapshot, usize)>; 8],
    depth: Option<(crate::descriptor::RtvSnapshot, usize)>,
}

fn viewports_eq(a: &[D3D12_VIEWPORT], b: &[D3D12_VIEWPORT]) -> bool {
    a.len() == b.len()
        && a.iter().zip(b.iter()).all(|(x, y)| {
            x.TopLeftX.to_bits() == y.TopLeftX.to_bits()
                && x.TopLeftY.to_bits() == y.TopLeftY.to_bits()
                && x.Width.to_bits() == y.Width.to_bits()
                && x.Height.to_bits() == y.Height.to_bits()
                && x.MinDepth.to_bits() == y.MinDepth.to_bits()
                && x.MaxDepth.to_bits() == y.MaxDepth.to_bits()
        })
}

fn rects_eq(a: &[RECT], b: &[RECT]) -> bool {
    a.len() == b.len()
        && a.iter().zip(b.iter()).all(|(x, y)| {
            x.left == y.left && x.top == y.top && x.right == y.right && x.bottom == y.bottom
        })
}

fn opt_f32_array_eq<const N: usize>(a: Option<[f32; N]>, b: Option<[f32; N]>) -> bool {
    match (a, b) {
        (Some(x), Some(y)) => x
            .iter()
            .zip(y.iter())
            .all(|(u, v)| u.to_bits() == v.to_bits()),
        (None, None) => true,
        _ => false,
    }
}

/// What one title list has set, and what its host list was last given.
#[derive(Default)]
pub(crate) struct State {
    pipeline: usize,
    /// View heap, sampler heap.
    heaps: [usize; 2],
    graphics: Vec<Root>,
    compute: Vec<Root>,
    vertex: [VertexView; 32],
    index: IndexView,
    /// Bound render target and depth resources, for their host states.
    targets: Vec<usize>,
    depth: usize,
    /// The host state the bound depth view needs ([`depth_state`]).
    depth_state: u32,
    /// The title's primitive topology (the host's may differ).
    topology: u32,
    /// What the title set that goes straight onto the host list, kept for
    /// the host list that continues this one after a cut ([`State::replay`]):
    /// whether a topology was set, the values of the render target and
    /// depth views as they were when set (the title may rewrite their
    /// descriptors before a cut), the viewports, the scissors, the blend
    /// factor and the stencil reference.
    topology_set: bool,
    target_views: Vec<Option<crate::descriptor::RtvSnapshot>>,
    depth_view: Option<crate::descriptor::RtvSnapshot>,
    targets_set: bool,
    viewports: Vec<D3D12_VIEWPORT>,
    scissors: Vec<RECT>,
    blend: Option<[f32; 4]>,
    stencil: Option<u32>,
    depth_bounds: Option<[f32; 2]>,
    /// The indirect records clamped last, if later draws may still read
    /// them ([`windows`]).
    window: Option<Window>,
    /// What the host list has: pipeline, root signature, heaps, targets and
    /// fixed-function state (skipped when unchanged under `VICTORY_BIND_CACHE`).
    host_pipeline: usize,
    /// Graphics, compute.
    host_roots: [usize; 2],
    host_heaps: [usize; 2],
    bound_targets: Option<BoundTargets>,
    /// Cached `(host_list_ptr, list_changes, depth_state)` for which all bound
    /// render targets and depth have already been ensured into their required
    /// host states, skipping per-draw resource locks and state-map lookups.
    targets_ready: Option<(usize, u64, u32)>,
    host_topology: Option<u32>,
    host_viewports_set: bool,
    host_scissors_set: bool,
    host_blend_set: bool,
    host_stencil_set: bool,
    host_depth_bounds_set: bool,
    /// Cached host vertex buffer views and binding state for the current host list.
    host_vertex: [D3D12_VERTEX_BUFFER_VIEW; 32],
    host_vertex_bound: u32,
    host_vertex_dirty: bool,
    host_vb_list: usize,
    host_vb_list_changes: u64,
    /// Cached host index buffer view and binding state for the current host list.
    host_index: Option<D3D12_INDEX_BUFFER_VIEW>,
    host_index_dirty: bool,
    host_ib_list: usize,
    host_ib_list_changes: u64,
    /// Scratch buffers reused across `State::bind` / `State::tables` calls to
    /// avoid per-draw heap allocations.
    scratch_made: Vec<(u8, u32)>,
    scratch_selected: Vec<u32>,
    scratch_textures: Vec<usize>,
    /// Tables made afresh in this list: (table, slot count, contents) → (first
    /// dynamic slot, scratch CBV epoch if backed by scratch constants).
    fresh_tables: std::collections::HashMap<(u64, u32, u64), (u32, Option<u64>)>,
    /// (Neptune) Of those, the ones whose views were made ready for a draw
    /// (`readable`) with nothing to do again while the host list's count of state
    /// changes stands: the host list, that count, and the textures the table shows.
    /// A draw that only tests its depth target and reads it too is not among them.
    ready_tables: std::collections::HashMap<(u64, u32, u64), (usize, u64, Vec<usize>)>,
    /// (Neptune) The tables (address, slot count, contents) this list has seen, with tables
    /// told apart by their contents alone (`tables_by_content`): one at a new address whose
    /// contents another had takes that one's host slots, and only its own title slots'
    /// notes are made here.
    seen_tables: std::collections::HashSet<(u64, u32, u64)>,
    /// The dynamic slots the list holds: title heap, host heap identity,
    /// first, count; given back when the list has run, is reset or released.
    dynamic: Vec<(usize, u64, u32, u32)>,
    /// Root SRVs bound as one-view tables in this list: (title heap, title
    /// address; 0 for none) → dynamic slot, and the host copy the slot
    /// shows: later draws of the list bring that one up to date, whatever
    /// copy the address would find by then (`shadow.rs` `tails`).
    root_srv_slots: std::collections::HashMap<(usize, u64), RootSlot>,
    /// Root UAVs bound as one-view tables in this list, likewise.
    root_uav_slots: std::collections::HashMap<(usize, u64), RootSlot>,
    /// The tables the last bind made afresh for a program the stand-in for
    /// the indirect light follows (`crate::ambient`): root parameter, bytes
    /// into the view heap.
    ambient_tables: Vec<(u8, u32)>,
    /// Title texture slots whose live descriptor write was deferred by
    /// `VICTORY_SHOW_AFTER` until this list's upload submission finishes on
    /// the GPU: `(title heap, slot index, descriptor handle, expected key)`.
    pending_shows: Vec<(usize, u32, u64, u64)>,
    /// The list ran and its dynamic slots were given back.
    tables_gone: bool,
}

impl Drop for State {
    fn drop(&mut self) {
        crate::descriptor::release_dynamic_many(&self.dynamic);
        self.commit_pending_shows();
    }
}

impl State {
    fn commit_pending_shows(&mut self) {
        for (heap, index, handle, key) in self.pending_shows.drain(..) {
            crate::descriptor::host::commit_show(heap, index, handle, key);
        }
    }

    /// The list's submission has run (submissions are waited for): the
    /// dynamic slots its tables were made in are given back, not kept
    /// until the title resets the list. A frame of the opening cutscene
    /// asks for 12,500 tables; held until the resets they ran the 131,072
    /// slots out, and up to 4,900 draws a frame went without their tables.
    /// shortcut: a list the title runs a second time without a reset then
    /// reads slots others may have taken (logged once).
    pub(crate) fn tables_done(&mut self) {
        self.commit_pending_shows();
        if self.tables_gone && self.dynamic.is_empty() {
            return;
        }
        if self.tables_gone {
            static ONCE: std::sync::Once = std::sync::Once::new();
            ONCE.call_once(|| {
                tracing::warn!("a list ran again after its descriptor tables were given back")
            });
        }
        crate::descriptor::release_dynamic_many(&self.dynamic);
        self.dynamic.clear();
        self.fresh_tables.clear();
        self.ready_tables.clear();
        self.seen_tables.clear();
        self.root_srv_slots.clear();
        self.root_uav_slots.clear();
        self.tables_gone = true;
    }
}

/// An experiment on the title's scissor rectangles (`VICTORY_SCISSOR_TEST=full`):
/// they are all widened to the whole target. The title's room at
/// the start of the prologue lost its walls in five runs of seven, and in
/// those runs the player's body and the world outside were cut by one
/// rectangle around the window (run 85, frame 2046): one run with this on
/// says whether the walls are drawn and cut away by such a rectangle, or
/// not drawn at all. Off by default: a rectangle the title sets is part of
/// its picture.
fn scissor_test() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("VICTORY_SCISSOR_TEST").as_deref() == Ok("full"))
}

/// An experiment on mesh draws (`VICTORY_MESH_TEST`): 1 for `skip` (none
/// is issued), 2 for `scissor` (each runs its shaders but rasterises one
/// pixel), 0 otherwise. It tells a card hung by a mesh shader from one hung
/// by what the shader's triangles cover.
fn mesh_test() -> u8 {
    static ON: std::sync::OnceLock<u8> = std::sync::OnceLock::new();
    *ON.get_or_init(|| match std::env::var("VICTORY_MESH_TEST").as_deref() {
        Ok("skip") => 1,
        Ok("scissor") => 2,
        _ => 0,
    })
}

/// `VICTORY_MESH_TEST=keep:<hex digits>[:<hex digits>]`: only the mesh
/// draws whose mesh shader's hash ends in one of the first digits (and
/// whose digit before it is one of the second, if given) are issued: a
/// search for the shaders that stop the card.
fn mesh_kept(pipeline: usize) -> bool {
    static KEEP: std::sync::OnceLock<Option<(String, String)>> = std::sync::OnceLock::new();
    let Some((last, before)) = KEEP.get_or_init(|| {
        let value = std::env::var("VICTORY_MESH_TEST").ok()?;
        let mut parts = value.strip_prefix("keep:")?.split(':');
        Some((
            parts.next()?.to_owned(),
            parts.next().unwrap_or("").to_owned(),
        ))
    }) else {
        return true;
    };
    let Some(p) = crate::pipeline::host_pipeline(pipeline as This) else {
        return true;
    };
    let Some(at) = p.name.find("mesh ") else {
        return true;
    };
    let hash = p.name[at + 5..].as_bytes();
    if hash.len() < 16 {
        return true;
    }
    last.as_bytes().contains(&hash[15])
        && (before.is_empty() || before.as_bytes().contains(&hash[14]))
}

/// Draws recorded on the host GPU since the last frame line.
static DRAWN: AtomicU64 = AtomicU64::new(0);

/// Draws run on the host so far.
pub(crate) fn drawn() -> u64 {
    DRAWN.load(Ordering::Relaxed)
}

/// The draws traced since the last present (diagnostics builds, with
/// debug logging for this module): logged when the frame comes out black.
#[cfg(feature = "diagnostics")]
static FRAME: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

/// The draws traced since the last call.
#[cfg(feature = "diagnostics")]
pub(crate) fn take_frame() -> Vec<String> {
    std::mem::take(&mut *FRAME.lock().unwrap_or_else(|e| e.into_inner()))
}

/// Draws, mesh dispatches and dispatches recorded but not run on the host
/// (no host pipeline) since the last call.
pub(crate) fn take_dropped() -> u64 {
    let rt_skin = RT_SKIN_SKIPPED.swap(0, Ordering::Relaxed);
    if rt_skin > 0 {
        tracing::info!(
            skipped = rt_skin,
            "ray-only dispatches skipped since the last frame line (VICTORY_BVH_NO_HIT=1, builder off; VICTORY_RT_RESOLVE, VICTORY_ACTIVATE_RAYS =1 run them; VICTORY_SKIP_RT_SKIN=1 skips skinning)"
        );
    }
    DROPPED.swap(0, Ordering::Relaxed)
}
/// A root SRV's dynamic slot in a list, and the host copy it shows.
type RootSlot = (u32, Option<std::sync::Arc<crate::shadow::Shadow>>);

/// Takes `more` from the time a run may spend bringing in a long queue of
/// textures at more than the usual pace; whether some is left.
/// `VICTORY_TEXTURE_GATE_S` sets it (0: none).
fn catching_up(more: std::time::Duration) -> bool {
    static SPENT: AtomicU64 = AtomicU64::new(0);
    static TOTAL: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    let total = *TOTAL.get_or_init(|| {
        let seconds = std::env::var("VICTORY_TEXTURE_GATE_S")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(60);
        seconds * 1000
    });
    let more = more.as_millis() as u64;
    SPENT.fetch_add(more, Ordering::Relaxed) + more < total
}

/// Buffer views of the heap whose slot showed another host buffer than
/// the copy the pass brought up to date for them, since the start
/// (diagnostics; for the memory line). It stays 0 while the pass and the
/// slots agree; with `VICTORY_VIEW_EXTENT=0` it counts the views that were
/// read stale.
#[cfg(feature = "diagnostics")]
pub(crate) static HEAP_VIEWS_ELSEWHERE: AtomicU64 = AtomicU64::new(0);
/// Passes over the view heap's buffer views for shaders that pick them by
/// index ([`State::heap_buffers`]), and the microseconds they took.
pub(crate) static HEAP_BUFFER_PASSES: AtomicU64 = AtomicU64::new(0);
/// Per host heap (keyed by monotonic [`HostHeap::id`]): the host copy and
/// range each slot's buffer view reads, kept from pass to pass by
/// [`State::heap_buffers`].
type PickedRanges = crate::heap_pass::Picked<std::sync::Arc<crate::shadow::Shadow>>;
static RANGES: std::sync::Mutex<Option<std::collections::HashMap<u64, PickedRanges>>> =
    std::sync::Mutex::new(None);

/// Called when the title heap is destroyed, after the shader-heap registry
/// lock is released. Drop cached resource references outside the cache lock.
pub(crate) fn forget_heap_ranges(id: u64) {
    let ranges = RANGES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_mut()
        .and_then(|heaps| heaps.remove(&id));
    drop(ranges);
}

#[cfg(test)]
#[test]
fn heap_range_cache_forgets_dropped_heap() {
    // IDs are allocated monotonically from 1. Reserve synthetic high IDs.
    let id = u64::MAX - 1;
    let other = u64::MAX - 2;
    {
        let mut ranges = RANGES.lock().unwrap_or_else(|e| e.into_inner());
        let heaps = ranges.get_or_insert_with(Default::default);
        heaps.insert(id, PickedRanges::new(false));
        heaps.insert(other, PickedRanges::new(false));
    }
    forget_heap_ranges(id);
    {
        let ranges = RANGES.lock().unwrap_or_else(|e| e.into_inner());
        let heaps = ranges.as_ref().unwrap();
        assert!(!heaps.contains_key(&id));
        assert!(heaps.contains_key(&other));
    }
    forget_heap_ranges(other);
}
/// Monotonic lifetime count of [`State::heap_buffers`] passes (never reset by
/// stats logging), used for boot-vs-gameplay budget cutoffs.
static HEAP_BUFFER_PASSES_TOTAL: AtomicU64 = AtomicU64::new(0);
pub(crate) static HEAP_BUFFER_MICROS: AtomicU64 = AtomicU64::new(0);

/// Nanoseconds of [`State::bind`] since the last frame line, on all
/// threads: the whole of it, then its parts: the early texture pass,
/// descriptor tables, title memory by address; and the binds counted
/// (`VICTORY_BIND_SPLIT=0`: not timed).
pub(crate) static BIND_NS: [AtomicU64; 5] = [const { AtomicU64::new(0) }; 5];

/// Whether [`BIND_NS`] is kept (`VICTORY_BIND_SPLIT`, on unless `0`).
pub(crate) fn bind_split() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_BIND_SPLIT").is_none_or(|v| v != "0"))
}

/// Adds the time from its start to its drop to [`BIND_NS`]`[part]`.
struct BindLap(Option<(usize, std::time::Instant)>);

impl BindLap {
    #[inline]
    fn start(part: usize) -> Self {
        BindLap(bind_split().then(|| (part, std::time::Instant::now())))
    }
}

impl Drop for BindLap {
    #[inline]
    fn drop(&mut self) {
        if let Some((part, began)) = self.0 {
            BIND_NS[part].fetch_add(began.elapsed().as_nanos() as u64, Ordering::Relaxed);
            if part == 0 {
                BIND_NS[4].fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

/// With the frame line: [`BIND_NS`] per frame over `frames`, taken.
pub(crate) fn bind_split_line(frames: f64) {
    if !bind_split() {
        return;
    }
    let v: [u64; 5] = std::array::from_fn(|i| BIND_NS[i].swap(0, Ordering::Relaxed));
    if v[4] == 0 {
        return;
    }
    let ms = |ns: u64| ns as f64 / 1e6 / frames;
    tracing::info!(
        "binding draws and dispatches per frame (VICTORY_BIND_SPLIT=0: not timed): {:.0} binds {:.1} ms (all threads), of          them the early texture pass {:.1} ms, descriptor tables {:.1} ms, title memory by address {:.1} ms, the rest (root          views and their refreshes, the heap pass, state) {:.1} ms",
        v[4] as f64 / frames,
        ms(v[0]),
        ms(v[1]),
        ms(v[2]),
        ms(v[3]),
        ms(v[0].saturating_sub(v[1] + v[2] + v[3]))
    );
}
/// Of [`HEAP_BUFFER_MICROS`], the microseconds of the pass over textures
/// picked by index that runs inside it (`heap_textures`), and the buffer
/// slots the pass over buffer views looked at; since the last frame line.
pub(crate) static HEAP_PASS_PARTS: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];
/// (Neptune) Where the time of the pass over textures picked by index
/// (`heap_textures`, on the thread that records the frame) goes, since the
/// last frame line: slots looked at; uploads recorded; microseconds looking
/// at slots (their views, how much of their memory is mapped, the card's
/// room; with prepare threads, handing out), making the host slots and the
/// host copies (`mirror`, `host_resource`), staging and recording uploads
/// (`readable`: staging buffers, copies), and the rest of each slot (whether
/// all of it is mapped, the fingerprint, the queue); host copies made by the
/// pass and their microseconds; staging buffers made by it and their
/// microseconds, and of their upload buffers the ones made new (not from
/// the pool) and their microseconds; slots handed to the prepare threads,
/// and slots passed over waiting for them (`crate::texture::prepare`); and
/// the microseconds before the pass looks at a slot (the queue's
/// bookkeeping, the sweep, the looks after uploads).
pub(crate) static HEAP_TEXTURE_PARTS: [AtomicU64; 15] = [const { AtomicU64::new(0) }; 15];

/// The frame whose bake draws are timed and when their time is up
/// (`crate::host::now_ms`).
static BAKE_DEADLINE: std::sync::Mutex<(u64, u64)> = std::sync::Mutex::new((u64::MAX, 0));

/// Milliseconds left of frame `frame`'s bake budget of `bake_ms` at `now`:
/// the first bake draw of a frame starts the clock for all of them.
fn bake_left(frame: u64, bake_ms: u64, now: u64) -> u64 {
    let mut deadline = BAKE_DEADLINE.lock().unwrap_or_else(|e| e.into_inner());
    if deadline.0 != frame {
        *deadline = (frame, now + bake_ms);
    }
    deadline.1.saturating_sub(now)
}

/// The parts of a pass's time ([`HEAP_TEXTURE_PARTS`]), in nanoseconds: each
/// lap adds the time since the one before to a part.
struct Laps {
    at: std::time::Instant,
    ns: [u64; 4],
}

impl Laps {
    const LOOK: usize = 0;
    const MAKE: usize = 1;
    const STAGE: usize = 2;
    const REST: usize = 3;

    fn new() -> Laps {
        Laps {
            at: std::time::Instant::now(),
            ns: [0; 4],
        }
    }

    fn lap(&mut self, part: usize) {
        let now = std::time::Instant::now();
        self.ns[part] += now.duration_since(self.at).as_nanos() as u64;
        self.at = now;
    }

    fn us(&self, part: usize) -> u64 {
        self.ns[part] / 1000
    }
}
/// Textures brought up to date for shaders that pick them by index
/// ([`State::heap_textures`]), the bytes of their memory, and the slots
/// still waiting.
pub(crate) static HEAP_TEXTURES: AtomicU64 = AtomicU64::new(0);
pub(crate) static HEAP_TEXTURE_BYTES: AtomicU64 = AtomicU64::new(0);
pub(crate) static HEAP_TEXTURES_WAITING: AtomicU64 = AtomicU64::new(0);
/// Passes that made no texture because the card had no room
/// (`crate::resource::room`), for the memory line.
pub(crate) static HEAP_TEXTURES_FULL: AtomicU64 = AtomicU64::new(0);

/// Looks at uploaded textures picked by index that found the memory as it
/// was, that found it changed (uploaded again), since the start; and the
/// slots that wait for such a look now.
pub(crate) static HEAP_TEXTURES_RECHECKED: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

/// Whether textures picked by index are looked at again after their upload
/// (`VICTORY_HEAP_RECHECK=0`: no).
fn heap_recheck() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_HEAP_RECHECK").is_none_or(|v| v != "0"))
}

/// Uploads of textures already on the card made while it had no room for
/// new ones, since the start.
pub(crate) static HEAP_TEXTURES_REUPLOADED_FULL: AtomicU64 = AtomicU64::new(0);

/// Whether a pass over the textures picked by index on a full card holds
/// back the slots that need a new host copy (`VICTORY_FULL_CARD_PASS=0`:
/// no, as before: they stay due, each pass looks at the same ones again
/// and stops at the 257th). Held back, such a slot is looked at again
/// after about four seconds or as soon as the card has room, and the pass
/// goes on to the slots behind it: the ones whose host copy exists and
/// only wants bringing up to date, and the ones whose memory went (letting
/// their host copies go is what makes room). What is uploaded while the
/// card has room is the same either way.
fn full_card_pass() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_FULL_CARD_PASS").is_none_or(|v| v != "0"))
}

/// Whether a texture that has its host copy is still brought up to date
/// while the card has no room for new ones (`VICTORY_REUPLOAD_WHEN_FULL=0`:
/// no).
fn reupload_when_full() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_REUPLOAD_WHEN_FULL").is_none_or(|v| v != "0"))
}
/// Times a placed texture's slot was passed over because its memory is not
/// mapped.
pub(crate) static HEAP_TEXTURES_AWAY: AtomicU64 = AtomicU64::new(0);
/// Slots the pass found up to date already: their texture is uploaded by
/// the draws that read it through a table (it has a staging buffer).
pub(crate) static HEAP_TEXTURES_TABLED: AtomicU64 = AtomicU64::new(0);
/// The queue of textures picked by index at the last pass
/// (`TextureQueue::census`): slots in it, due, waiting for their memory,
/// uploaded in part, and the frames the oldest due one has waited.
pub(crate) static HEAP_TEXTURES_QUEUE: [AtomicU64; 5] = [const { AtomicU64::new(0) }; 5];
/// Since the start: looks at a slot uploaded in part that found no more of
/// its memory mapped; uploads of textures not all mapped; and of those,
/// the ones whose first page was not there (asked by the first page alone,
/// as before, they counted as not there at all).
pub(crate) static HEAP_TEXTURES_PARTLY: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];
static DROPPED: AtomicU64 = AtomicU64::new(0);
/// Ray-tracing skinning dispatches left out since the last frame line
/// (`crate::pipeline::skips_rt_skin`).
static RT_SKIN_SKIPPED: AtomicU64 = AtomicU64::new(0);

/// Marks the last traced draw as not run on the host.
fn dropped(pipeline: usize) {
    crate::pipeline::note_drop(pipeline as This);
    #[cfg(feature = "diagnostics")]
    if let Some(last) = FRAME.lock().unwrap_or_else(|e| e.into_inner()).last_mut() {
        last.push_str(" DROPPED");
    }
}

/// Dispatches run on the host so far.
static DISPATCHED: AtomicU64 = AtomicU64::new(0);

/// Title draws and mesh dispatches recorded so far, direct and indirect,
/// whether they ran on the host or were dropped; never compute dispatches,
/// run or dropped. What the heavy scene's detector counts
/// (`crate::capture`, `VICTORY_CAPTURE_HEAVY_FRAMES`): one count however a
/// build fares with the frame's pipelines.
static ATTEMPTS: AtomicU64 = AtomicU64::new(0);

/// See [`ATTEMPTS`].
#[cfg(all(windows, feature = "diagnostics"))]
pub(crate) fn attempted() -> u64 {
    ATTEMPTS.load(Ordering::Relaxed)
}

pub(crate) fn dispatched() -> u64 {
    DISPATCHED.load(Ordering::Relaxed)
}

/// The host address of a root view: its buffer's host copy, or for a
/// constant buffer not 256-byte aligned there (the console needs 16) a
/// copy in upload memory.
/// shortcut: a root view has no size; constant buffers are taken as 64 KiB
/// (cut at their buffer's end), other views as all that is left of their
/// buffer. (They were taken as 1 MiB: 1,452 of the 2,021 mesh vertex
/// streams of a cutscene frame reach further, and what lay past the first
/// megabyte was never brought up to date - meshes without bodies, exploded
/// triangles. `VICTORY_ROOT_VIEW_MB` sets a limit again.)
fn root_view(list: &HostList, address: u64, srv: bool) -> Option<u64> {
    static LIMIT: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    let limit = *LIMIT.get_or_init(|| {
        std::env::var("VICTORY_ROOT_VIEW_MB")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .map_or(u64::MAX, |mb| mb << 20)
    });
    let shadow = crate::shadow::find(address, 1)?;
    let len = shadow.left(address).min(if srv { limit } else { 1 << 16 });
    let gpu = shadow.gpu(address);
    if srv || gpu % 256 == 0 {
        shadow.refresh(list, address, len);
        return Some(gpu);
    }
    static REUSE_ROOT_CBV: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let reuse = *REUSE_ROOT_CBV
        .get_or_init(|| std::env::var_os("VICTORY_REUSE_ROOT_CBV").is_some_and(|v| v == "1"));
    let (gpu, _) = list.fill_root_cbv(address as *const u8, len, reuse)?;
    Some(gpu)
}

macro_rules! once {
    ($($arg:tt)*) => {{
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| tracing::warn!($($arg)*));
    }};
}

fn set(roots: &mut Vec<Root>, slot: u32, root: Root) {
    let slot = slot as usize;
    if roots.len() <= slot {
        roots.resize(slot + 1, Root::None);
    }
    roots[slot] = root;
}

impl State {
    pub(crate) fn reset(&mut self, new_pipeline: This) {
        for (heap, index, _, _) in self.pending_shows.drain(..) {
            crate::descriptor::host::requeue_show(heap, index);
        }
        crate::descriptor::release_dynamic_many(&self.dynamic);
        // Exhaustive destructuring ensures every field is reset while reusing
        // heap capacities of per-list containers across frames.
        let Self {
            pipeline,
            heaps,
            graphics,
            compute,
            vertex,
            index,
            targets,
            depth,
            depth_state,
            topology,
            topology_set,
            target_views,
            depth_view,
            targets_set,
            viewports,
            scissors,
            blend,
            stencil,
            depth_bounds,
            window,
            host_pipeline,
            host_roots,
            host_heaps,
            bound_targets,
            targets_ready,
            host_topology,
            host_viewports_set,
            host_scissors_set,
            host_blend_set,
            host_stencil_set,
            host_depth_bounds_set,
            host_vertex,
            host_vertex_bound,
            host_vertex_dirty,
            host_vb_list,
            host_vb_list_changes,
            host_index,
            host_index_dirty,
            host_ib_list,
            host_ib_list_changes,
            scratch_made,
            scratch_selected,
            scratch_textures,
            fresh_tables,
            ready_tables,
            seen_tables,
            dynamic,
            root_srv_slots,
            root_uav_slots,
            ambient_tables,
            pending_shows,
            tables_gone,
        } = self;
        *pipeline = new_pipeline as usize;
        *heaps = [0; 2];
        graphics.clear();
        compute.clear();
        *vertex = [VertexView::default(); 32];
        *index = IndexView::default();
        targets.clear();
        *depth = 0;
        *depth_state = 0;
        *topology = 0;
        *topology_set = false;
        target_views.clear();
        *depth_view = None;
        *targets_set = false;
        viewports.clear();
        scissors.clear();
        *blend = None;
        *stencil = None;
        *depth_bounds = None;
        *window = None;
        *host_pipeline = 0;
        *host_roots = [0; 2];
        *host_heaps = [0; 2];
        *bound_targets = None;
        *targets_ready = None;
        *host_topology = None;
        *host_viewports_set = false;
        *host_scissors_set = false;
        *host_blend_set = false;
        *host_stencil_set = false;
        *host_depth_bounds_set = false;
        *host_vertex = [D3D12_VERTEX_BUFFER_VIEW::default(); 32];
        *host_vertex_bound = 0;
        *host_vertex_dirty = false;
        *host_vb_list = 0;
        *host_vb_list_changes = 0;
        *host_index = None;
        *host_index_dirty = false;
        *host_ib_list = 0;
        *host_ib_list_changes = 0;
        scratch_made.clear();
        scratch_selected.clear();
        scratch_textures.clear();
        fresh_tables.clear();
        if fresh_tables.capacity() > 4096 {
            fresh_tables.shrink_to(1024);
        }
        ready_tables.clear();
        if ready_tables.capacity() > 4096 {
            ready_tables.shrink_to(1024);
        }
        seen_tables.clear();
        if seen_tables.capacity() > 4096 {
            seen_tables.shrink_to(1024);
        }
        dynamic.clear();
        root_srv_slots.clear();
        root_uav_slots.clear();
        ambient_tables.clear();
        pending_shows.clear();
        *tables_gone = false;
    }

    /// The title's topology; console-only ones become host lists (see
    /// [`crate::primitive`]).
    pub(crate) fn set_topology(&mut self, list: &HostList, topology: u32) {
        self.topology = topology;
        self.topology_set = true;
        let host = match topology {
            RECTANGLES => 4,
            TRIANGLE_FAN | LINE_LOOP | QUADS | QUAD_STRIP => {
                once!(topology, "console topology drawn as a triangle list");
                4
            }
            t => t,
        };
        if bind_cache() && self.host_topology == Some(host) {
            return;
        }
        self.host_topology = Some(host);
        // SAFETY: an open list.
        unsafe {
            list.list
                .IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY(host as i32))
        };
    }

    pub(crate) fn set_pipeline(&mut self, pipeline: This) {
        self.pipeline = pipeline as usize;
    }

    pub(crate) fn set_heaps(&mut self, heaps: &[This]) {
        self.heaps = [0; 2];
        for &h in heaps {
            if !h.is_null() {
                let samplers = crate::descriptor::heap(h).samplers();
                self.heaps[usize::from(samplers)] = h as usize;
            }
        }
    }

    fn roots(&mut self, compute: bool) -> &mut Vec<Root> {
        if compute {
            &mut self.compute
        } else {
            &mut self.graphics
        }
    }

    pub(crate) fn set_table(&mut self, compute: bool, slot: u32, handle: u64) {
        set(self.roots(compute), slot, Root::Table(handle));
    }

    pub(crate) fn set_view(&mut self, compute: bool, slot: u32, address: u64) {
        set(self.roots(compute), slot, Root::View(address));
    }

    pub(crate) fn set_constants(&mut self, compute: bool, slot: u32, values: &[u32], offset: u32) {
        let roots = self.roots(compute);
        let s = slot as usize;
        if !matches!(roots.get(s), Some(Root::Constants(_))) {
            set(roots, slot, Root::Constants(Vec::new()));
        }
        if let Root::Constants(c) = &mut roots[s] {
            let end = offset as usize + values.len();
            if c.len() < end {
                c.resize(end, 0);
            }
            c[offset as usize..end].copy_from_slice(values);
        }
    }

    pub(crate) fn set_vertex_buffers(&mut self, start: u32, views: &[VertexView]) {
        for (i, v) in views.iter().enumerate() {
            let idx = start as usize + i;
            if let Some(slot) = self.vertex.get_mut(idx) {
                *slot = *v;
                self.host_vertex[idx] = D3D12_VERTEX_BUFFER_VIEW::default();
                self.host_vertex_bound &= !(1u32 << idx);
                self.host_vertex_dirty = true;
            }
        }
    }

    pub(crate) fn clear_vertex_buffers(&mut self, start: u32, count: u32) {
        let start = start as usize;
        let end = start.saturating_add(count as usize).min(self.vertex.len());
        for idx in start..end {
            self.vertex[idx] = VertexView::default();
            self.host_vertex[idx] = D3D12_VERTEX_BUFFER_VIEW::default();
            self.host_vertex_bound &= !(1u32 << idx);
            self.host_vertex_dirty = true;
        }
    }

    pub(crate) fn set_index_buffer(&mut self, view: Option<&IndexView>) {
        self.index = view.copied().unwrap_or_default();
        self.host_index = None;
        self.host_index_dirty = true;
    }

    /// Render targets and depth, set on the host list at once.
    pub(crate) fn set_targets(&mut self, list: &HostList, handles: &[u64], depth: u64) {
        self.target_views.clear();
        self.target_views.extend(
            handles
                .iter()
                .take(D3D12_SIMULTANEOUS_RENDER_TARGET_COUNT as usize)
                .map(|&h| {
            let record = crate::descriptor::rtv_record(h);
            if record.is_none() {
                once!(
                    handle = format_args!("{h:#x}"),
                    "render target without a recorded view"
                );
            }
            record.map(|r| r.snapshot())
        }),
        );
        self.depth_view = crate::descriptor::dsv_record(depth).map(|r| r.snapshot());
        self.targets_set = true;
        self.record_targets(list);
    }

    /// Makes the host views from the values kept when the title set the
    /// targets, not from descriptor slots it may have rewritten since.
    fn record_targets(&mut self, list: &HostList) {
        let Some(host) = crate::host::host() else {
            return;
        };
        let cache_on = bind_cache();
        let count = self
            .target_views
            .len()
            .min(D3D12_SIMULTANEOUS_RENDER_TARGET_COUNT as usize);
        let mut key = BoundTargets {
            rtv_count: count as u8,
            rtvs: [None; 8],
            depth: None,
        };
        let mut resolved_rtvs: [Option<(crate::descriptor::RtvRecord, Option<&ID3D12Resource>)>;
            8] = [None; 8];
        for (i, slot) in self.target_views[..count].iter().enumerate() {
            if let Some(snapshot) = *slot {
                let record = snapshot.restore();
                let texture = crate::resource::host_resource(record.resource);
                key.rtvs[i] = Some((snapshot, texture.map_or(0, |t| t.as_raw() as usize)));
                resolved_rtvs[i] = Some((record, texture));
            }
        }
        let resolved_depth = self.depth_view.and_then(|snapshot| {
            let record = snapshot.restore();
            let texture = crate::resource::host_resource(record.resource)?;
            key.depth = Some((snapshot, texture.as_raw() as usize));
            Some((record, texture))
        });
        if cache_on && self.bound_targets == Some(key) {
            return;
        }
        self.targets_ready = None;
        self.targets.clear();
        self.depth = 0;
        self.depth_state = DEPTH_WRITE;
        host.output_views(|output| {
            let mut rtvs = [D3D12_CPU_DESCRIPTOR_HANDLE::default(); 8];
            let mut rtv_len = 0usize;
            for entry in resolved_rtvs[..count].iter().flatten() {
                let (record, texture) = *entry;
                let Some(texture) = texture else {
                    // A typed null view (or a target without a host copy) still
                    // takes its slot: left out, the next live target would move
                    // into it and the pixel shader's outputs reach the wrong
                    // targets.
                    // shortcut: a view without a description or a format has no
                    // null view to stand for it and is left out, as before.
                    let null = record
                        .desc()
                        .filter(|d| d.Format.0 != 0 && d.ViewDimension.0 != 0);
                    if let Some(desc) = null {
                        rtvs[rtv_len] = output.rtv(None, Some(&desc));
                        rtv_len += 1;
                    }
                    continue;
                };
                self.targets.push(record.resource as usize);
                #[cfg(feature = "diagnostics")]
                crate::capture::note_target(record.resource);
                rtvs[rtv_len] = output.rtv(Some(texture), record.desc().as_ref());
                rtv_len += 1;
            }
            let dsv = resolved_depth.map(|(record, texture)| {
                let desc = record.depth_desc();
                self.depth = record.resource as usize;
                self.depth_state = depth_state(texture, desc.as_ref());
                if crate::resource::depth_and_unordered(record.resource) {
                    crate::resource::DEPTH_UNORDERED[1].fetch_add(1, Ordering::Relaxed);
                }
                #[cfg(feature = "diagnostics")]
                crate::capture::note_target(record.resource);
                output.dsv(texture, desc.as_ref())
            });
            // SAFETY: an open list; the views live through the call.
            unsafe {
                list.list.OMSetRenderTargets(
                    rtv_len as u32,
                    (rtv_len > 0).then_some(rtvs.as_ptr()),
                    false,
                    dsv.as_ref().map(std::ptr::from_ref),
                )
            };
        });
        self.bound_targets = cache_on.then_some(key);
    }

    /// Viewports, scissors, the blend factor and the stencil reference: set
    /// on the host list at once, and kept for [`State::replay`].
    pub(crate) fn set_viewports(&mut self, list: &HostList, viewports: &[D3D12_VIEWPORT]) {
        if bind_cache() && self.host_viewports_set && viewports_eq(&self.viewports, viewports) {
            return;
        }
        self.viewports.clear();
        self.viewports.extend_from_slice(viewports);
        self.host_viewports_set = true;
        crate::draw::viewports(list, viewports);
    }

    pub(crate) fn set_scissors(&mut self, list: &HostList, rects: &[RECT]) {
        if bind_cache()
            && self.host_scissors_set
            && !scissor_test()
            && rects_eq(&self.scissors, rects)
        {
            return;
        }
        self.scissors.clear();
        self.scissors.extend_from_slice(rects);
        self.host_scissors_set = true;
        if scissor_test() {
            // The experiment: every rectangle the title sets is widened to
            // the largest target there is, so nothing is cut by one. What
            // the listing shows (`op_text`) stays the title's own.
            let full = RECT {
                left: 0,
                top: 0,
                right: 16384,
                bottom: 16384,
            };
            crate::draw::scissors(list, &vec![full; rects.len()]);
            return;
        }
        crate::draw::scissors(list, rects);
    }

    pub(crate) fn set_blend_factor(&mut self, list: &HostList, factor: Option<&[f32; 4]>) {
        let next = factor.copied();
        if bind_cache() && self.host_blend_set && opt_f32_array_eq(self.blend, next) {
            return;
        }
        self.blend = next;
        self.host_blend_set = true;
        crate::draw::blend_factor(list, factor);
    }

    pub(crate) fn set_stencil_ref(&mut self, list: &HostList, reference: u32) {
        if bind_cache() && self.host_stencil_set && self.stencil == Some(reference) {
            return;
        }
        self.stencil = Some(reference);
        self.host_stencil_set = true;
        crate::draw::stencil_ref(list, reference);
    }

    pub(crate) fn set_depth_bounds(&mut self, list: &HostList, min: f32, max: f32) {
        let next = Some([min, max]);
        if bind_cache() && self.host_depth_bounds_set && opt_f32_array_eq(self.depth_bounds, next) {
            return;
        }
        self.depth_bounds = next;
        self.host_depth_bounds_set = true;
        crate::draw::depth_bounds(list, min, max);
    }

    /// A pass of ours was recorded into the title's list
    /// (`HostList::plane_now`): it set a descriptor heap, a graphics root
    /// signature, a pipeline, a depth target, a viewport, a scissor and the
    /// topology. What the title set is set again: targets, viewports,
    /// scissors and topology here; heaps, root signatures and pipeline by
    /// the next `bind`, told that the list holds none of the title's (heaps
    /// as no title's set, so the title's go back even if it set none). The
    /// tables stay: they are in this host list. The pass leaves the blend
    /// factor, stencil reference and depth bounds alone.
    /// Host work the title does not know of (a PC library recorded onto the
    /// list) has run: it set its own heaps, root signature and pipeline.
    pub(crate) fn after_external(&mut self, list: &HostList) {
        self.after_our_pass(list);
    }

    fn after_our_pass(&mut self, list: &HostList) {
        self.host_pipeline = 0;
        self.host_roots = [0; 2];
        self.host_heaps = [usize::MAX; 2];
        self.bound_targets = None;
        self.targets_ready = None;
        self.host_topology = None;
        self.host_viewports_set = false;
        self.host_scissors_set = false;
        self.host_vertex = [D3D12_VERTEX_BUFFER_VIEW::default(); 32];
        self.host_vertex_bound = 0;
        self.host_vertex_dirty = true;
        self.host_vb_list = 0;
        self.host_index = None;
        self.host_index_dirty = true;
        self.host_ib_list = 0;
        if self.topology_set {
            self.set_topology(list, self.topology);
        }
        if self.targets_set {
            self.record_targets(list);
        }
        if !self.viewports.is_empty() {
            crate::draw::viewports(list, &self.viewports);
            self.host_viewports_set = true;
        }
        if !self.scissors.is_empty() {
            let rects = std::mem::take(&mut self.scissors);
            self.set_scissors(list, &rects);
        }
    }

    /// `list` is a new host list that continues the title's list (it was
    /// cut at a wait for a memory value, `crate::list::cut`): a new host
    /// list has nothing set, so what the title set so far is set again.
    /// The pipeline, the root signatures and the heaps are set by the next
    /// draw or dispatch (`bind`, which is told here that the host list has
    /// none); root arguments, vertex and index buffers are set by every
    /// draw anyway. The dynamic descriptor slots and the tables made in
    /// them stay: the parts before the cut have not run yet, and they are
    /// given back when the title's list is reset. The render target views
    /// are made again from the values kept when the title set them.
    pub(crate) fn replay(&mut self, list: &HostList) {
        // AscilCH #742: only scratch-backed tables expire at a cut.
        self.fresh_tables.retain(|_, (_, epoch)| epoch.is_none());
        self.host_pipeline = 0;
        self.host_roots = [0; 2];
        self.host_heaps = [usize::MAX; 2];
        self.bound_targets = None;
        self.targets_ready = None;
        self.host_topology = None;
        self.host_viewports_set = false;
        self.host_scissors_set = false;
        self.host_blend_set = false;
        self.host_stencil_set = false;
        self.host_depth_bounds_set = false;
        self.host_vertex = [D3D12_VERTEX_BUFFER_VIEW::default(); 32];
        self.host_vertex_bound = 0;
        self.host_vertex_dirty = true;
        self.host_vb_list = 0;
        self.host_index = None;
        self.host_index_dirty = true;
        self.host_ib_list = 0;
        if self.topology_set {
            self.set_topology(list, self.topology);
        }
        if self.targets_set {
            self.record_targets(list);
        } else {
            // Unbind any DSV left active by an internal pass so it cannot remain
            // bound as a writable DSV when transitioned to SHADER_RESOURCE.
            // SAFETY: live graphics command list.
            unsafe { list.list.OMSetRenderTargets(0, None, false, None) };
        }
        if !self.viewports.is_empty() {
            crate::draw::viewports(list, &self.viewports);
            self.host_viewports_set = true;
        }
        if !self.scissors.is_empty() {
            let rects = std::mem::take(&mut self.scissors);
            self.set_scissors(list, &rects);
        }
        if let Some(factor) = self.blend {
            crate::draw::blend_factor(list, Some(&factor));
            self.host_blend_set = true;
        }
        if let Some(reference) = self.stencil {
            crate::draw::stencil_ref(list, reference);
            self.host_stencil_set = true;
        }
        if let Some([min, max]) = self.depth_bounds {
            crate::draw::depth_bounds(list, min, max);
            self.host_depth_bounds_set = true;
        }
    }

    /// The user-data value of a root argument, as translated shaders read
    /// it: a table's byte offset in its heap, a constant's value.
    fn user_value(&self, compute: bool, slot: u8, root: RootType, dword: u32) -> u32 {
        match (self.roots_ref(compute).get(usize::from(slot)), root) {
            (Some(Root::Table(handle)), RootType::Table) => match self.heap_of(*handle) {
                Some(heap) => (handle - heap.start) as u32,
                None => {
                    once!(
                        handle = format_args!("{handle:#x}"),
                        "descriptor table outside the bound heaps"
                    );
                    0
                }
            },
            (Some(Root::Constants(c)), RootType::Constants) => {
                c.get(dword as usize).copied().unwrap_or(0)
            }
            // The host's own ([`Self::host_word`]): no root argument.
            (_, RootType::Other(_)) => 0,
            _ => {
                once!(
                    slot,
                    ?root,
                    "a shader reads a root argument the title did not set"
                );
                0
            }
        }
    }

    /// A user-data dword the host computes for a translated shader
    /// (`victory_shader::emit::USER_MEMORY_BASE`, `USER_VIEW_ADDRESS`).
    fn host_word(
        &self,
        compute: bool,
        kind: u32,
        slot: u8,
        dword: u32,
        memory: Option<&crate::memory::Ready>,
    ) -> u32 {
        use victory_shader::emit::{
            USER_GROUP_BASE, USER_MEMORY_BASE, USER_VIEW_ADDRESS, USER_VIEW_BIAS,
        };
        // Set between the parts of a large dispatch (`group_base`).
        if kind == USER_GROUP_BASE {
            return 0;
        }
        if kind == USER_MEMORY_BASE {
            return memory.map_or(0, |m| m.base);
        }
        if kind == USER_VIEW_BIAS {
            // The view begins on 16 bytes of the copy the address finds
            // (`root_uav`, `root_srv_table`).
            // shortcut: found again here, not taken from the list's slot.
            return match self.roots_ref(compute).get(usize::from(slot)) {
                Some(&Root::View(address)) => crate::shadow::find(address, 1).map_or(0, |s| {
                    // SAFETY: a live buffer.
                    let base = unsafe { s.buffer.GetGPUVirtualAddress() };
                    ((s.gpu(address) - base) & 15) as u32
                }),
                _ => 0,
            };
        }
        if kind != USER_VIEW_ADDRESS && kind != USER_VIEW_ADDRESS + 1 {
            once!(kind, "a shader reads a host value this build does not know");
            return 0;
        }
        // The title address of the buffer view at that dword of the table.
        let address = match self.roots_ref(compute).get(usize::from(slot)) {
            Some(&Root::Table(table)) => crate::descriptor::view(table + u64::from(dword) * 4)
                .and_then(|view| crate::descriptor::host::buffer_range(&view))
                .map_or(0, |range| range.0),
            _ => 0,
        };
        if address == 0 {
            once!(
                slot,
                dword,
                "a shader takes the address of a buffer view that is not there: it reads 0"
            );
        }
        if kind == USER_VIEW_ADDRESS {
            address as u32
        } else {
            (address >> 32) as u32
        }
    }

    /// For a shader that reads title memory by address
    /// ([`crate::memory`]): its directory, fault notes and view slots,
    /// with the pages of the root views the list has set made ready.
    fn memory_ready(
        &self,
        list: &HostList,
        compute: bool,
        writes: bool,
    ) -> Option<crate::memory::Ready> {
        let heap = match self.heaps[0] {
            0 => None,
            h => crate::descriptor::heap(h as This).host(),
        }?;
        let (mut addresses, mut constants) = (Vec::new(), Vec::new());
        for root in self.roots_ref(compute) {
            match root {
                Root::View(address) => addresses.push(*address),
                Root::Constants(values) => constants.extend_from_slice(values),
                _ => {}
            }
        }
        crate::memory::ready(list, heap, &addresses, &constants, writes)
    }

    /// The bound heap holding `handle`.
    fn heap_of(&self, handle: u64) -> Option<&'static crate::descriptor::Heap> {
        self.heaps
            .iter()
            .filter(|&&h| h != 0)
            .map(|&h| crate::descriptor::heap(h as This))
            .find(|h| h.index(handle).is_some())
    }

    /// Brings the host list up to date for a draw; false if the draw cannot
    /// run on the host.
    fn prepare(&mut self, list: &HostList, indexed: bool) -> bool {
        self.prepare_for(list, indexed, false)
    }

    /// As [`State::prepare`], for draws (`mesh` false) or mesh dispatches.
    fn prepare_for(&mut self, list: &HostList, indexed: bool, mesh: bool) -> bool {
        #[cfg(feature = "diagnostics")]
        if crate::pipeline::is_skipped(self.pipeline as This) {
            return false;
        }
        let Some(p) = crate::pipeline::host_pipeline(self.pipeline as This) else {
            return false;
        };
        if p.compute || p.mesh != mesh {
            once!(
                mesh,
                "a draw or mesh dispatch with another kind of pipeline"
            );
            return false;
        }
        if p.rects && self.topology != RECTANGLES {
            // Its geometry stage would turn each triangle into a rectangle.
            once!("a rectangle pipeline drawn with another topology");
        }
        if self.depth != 0 {
            // A depth texture shaders also write: sync its depth/stencil plane
            // before `bind` sets heaps, root signature, and pipeline state, so
            // any inline `pass_plane` is replayed cleanly inside `bind`.
            list.twin_use(self.depth as This, false, true);
        }
        if !self.bind(list, p, false) {
            return false;
        }
        let l = &list.list;
        let list_ptr = std::ptr::from_ref(list) as usize;
        let list_changes = list.changes.load(Ordering::Relaxed);
        // Vertex buffers the input layout reads.
        if p.slots != 0 {
            if self.host_vb_list != list_ptr || self.host_vb_list_changes != list_changes {
                self.host_vertex = [D3D12_VERTEX_BUFFER_VIEW::default(); 32];
                self.host_vertex_bound = 0;
                self.host_vb_list = list_ptr;
                self.host_vb_list_changes = list_changes;
                self.host_vertex_dirty = true;
            }
            let missing = p.slots & !self.host_vertex_bound;
            if self.host_vertex_dirty || missing != 0 {
                if missing != 0 {
                    let mut bits = missing;
                    while bits != 0 {
                        let slot = bits.trailing_zeros() as usize;
                        bits &= bits - 1;
                        let v = self.vertex[slot];
                        if v.address == 0 || v.size == 0 {
                            self.host_vertex[slot] = D3D12_VERTEX_BUFFER_VIEW::default();
                            continue;
                        }
                        let Some(shadow) = crate::shadow::find_any(v.address, u64::from(v.size))
                        else {
                            self.host_vertex[slot] = D3D12_VERTEX_BUFFER_VIEW::default();
                            continue;
                        };
                        shadow.refresh(list, v.address, u64::from(v.size));
                        self.host_vertex[slot] = D3D12_VERTEX_BUFFER_VIEW {
                            BufferLocation: shadow.gpu(v.address),
                            SizeInBytes: v.size,
                            StrideInBytes: v.stride,
                        };
                    }
                    self.host_vertex_bound |= missing;
                    let count = (32 - self.host_vertex_bound.leading_zeros()) as usize;
                    // SAFETY: an open list; the views name live host buffers.
                    unsafe {
                        if count > 0 {
                            l.IASetVertexBuffers(0, Some(&self.host_vertex[..count]));
                        }
                    }
                }
                self.host_vertex_dirty = false;
            }
        }
        if indexed {
            if self.host_index_dirty
                || self.host_index.is_none()
                || self.host_ib_list != list_ptr
                || self.host_ib_list_changes != list_changes
            {
                let v = self.index;
                let Some(shadow) = crate::shadow::find_any(v.address, u64::from(v.size)) else {
                    self.host_index = None;
                    self.host_index_dirty = true;
                    return false;
                };
                shadow.refresh(list, v.address, u64::from(v.size));
                let view = D3D12_INDEX_BUFFER_VIEW {
                    BufferLocation: shadow.gpu(v.address),
                    SizeInBytes: v.size,
                    Format: DXGI_FORMAT(v.format as i32),
                };
                // SAFETY: an open list; the view names a live host buffer.
                unsafe { l.IASetIndexBuffer(Some(&view)) };
                self.host_index = Some(view);
                self.host_index_dirty = false;
                self.host_ib_list = list_ptr;
                self.host_ib_list_changes = list_changes;
            }
        }
        // A view that writes neither plane keeps the texture readable:
        // the depth read state, together with the shader read states
        // if this draw's shaders read the same texture (the write state
        // put it out of the state those views need, and a move to it
        // and back for every such draw).
        let depth_state = if self.depth != 0 {
            if self.depth_state == DEPTH_READ {
                let (tested, read) = TESTED_DEPTH.get();
                if read && tested == self.depth {
                    DEPTH_READ | SHADER_RESOURCE
                } else {
                    DEPTH_READ
                }
            } else {
                DEPTH_WRITE
            }
        } else {
            0
        };
        let now_changes = list.changes.load(Ordering::Relaxed);
        if self.targets_ready != Some((list_ptr, now_changes, depth_state)) {
            const RENDER_TARGET: u32 = 0x4;
            for &t in &self.targets {
                crate::list::ensure_state(list, t as This, RENDER_TARGET);
            }
            if self.depth != 0 {
                crate::list::ensure_state(list, self.depth as This, depth_state);
            }
            self.targets_ready = Some((list_ptr, list.changes.load(Ordering::Relaxed), depth_state));
        }
        true
    }

    /// A descriptor a shader reads through a table: its host slot made if
    /// it changed, constant data copied, textures made readable.
    /// What draws and dispatches share: heaps, our root signature, the
    /// pipeline, and per stage the user data, root views and the
    /// descriptors its tables hold.
    /// False if the draw or dispatch must not run: a root SRV it reads
    /// could not be given a view.
    pub(crate) fn bind(&mut self, list: &HostList, p: &HostPipeline, compute: bool) -> bool {
        let _whole = BindLap::start(0);
        let l = &list.list;
        // Textures picked by index, before anything asks for them.
        {
            let _lap = BindLap::start(1);
            self.early_textures(list, p, compute);
        }
        if crate::host::walls_trace() {
            self.walls_census(p, compute);
        }
        if list.take_plane_clobbered() {
            self.replay(list);
        }
        // Heaps first: a root signature with directly indexed heaps must be
        // set after them.
        if self.host_heaps != self.heaps {
            let heaps: Vec<Option<ID3D12DescriptorHeap>> = self
                .heaps
                .iter()
                .filter(|&&h| h != 0)
                .filter_map(|&h| crate::descriptor::heap(h as This).host())
                .map(|h| Some(h.heap.clone()))
                .collect();
            heaps.iter().flatten().for_each(|h| list.keep(h));
            if !heaps.is_empty() {
                list.note_heap_bind();
            }
            // SAFETY: an open list and shader-visible heaps.
            unsafe { l.SetDescriptorHeaps(&heaps) };
            self.host_heaps = self.heaps;
            self.host_roots = [0; 2];
        }
        // The depth target a draw only tests, for the views of it that the
        // draw's shaders read (`readable`).
        let tested = if !compute && self.depth_state == DEPTH_READ {
            self.depth
        } else {
            0
        };
        TESTED_DEPTH.set((tested, false));
        let root = p.root.as_raw() as usize;
        if self.host_roots[usize::from(compute)] != root {
            // SAFETY: an open list and a live root signature.
            unsafe {
                if compute {
                    l.SetComputeRootSignature(&p.root);
                } else {
                    l.SetGraphicsRootSignature(&p.root);
                }
            }
            self.host_roots[usize::from(compute)] = root;
        }
        // By host object: a title pipeline released and another made at its
        // address is another host pipeline (the list keeps the old one).
        if self.host_pipeline != p.pso.as_raw() as usize {
            // SAFETY: an open list and a live pipeline.
            unsafe { l.SetPipelineState(&p.pso) };
            self.host_pipeline = p.pso.as_raw() as usize;
            // The title may release the pipeline before it submits the list.
            // Kept and named once per list even when alternating pipelines.
            list.bind_pipeline(&p.pso, &p.root, &p.name);
        }
        let mut made = std::mem::take(&mut self.scratch_made);
        for stage in &p.stages {
            let t = stage.translated();
            STORES.set(t.stores);
            {
                let _lap = BindLap::start(2);
                self.tables(list, &t.tables, compute, &mut made);
            }
            STORES.set(true);
            if list.take_plane_clobbered() {
                self.scratch_made = made;
                self.replay(list);
                return self.bind(list, p, compute);
            }
            if made.iter().any(|m| m.1 == u32::MAX) {
                self.scratch_made = made;
                once!("a draw whose descriptor table found no free slots is dropped");
                return false;
            }
            if stage.shader.ambient.is_some() {
                self.ambient_tables.clear();
                self.ambient_tables.extend(made.iter().copied());
            }
            // What the shader stores through root views and raw addresses is
            // copied back into title memory afterwards.
            let writes = (t.stores || !write_skip())
                && !crate::pipeline::builder_quarantined(self.pipeline as This);
            // Title memory by address: what the shader finds it by.
            let memory = if t
                .host
                .root_srvs
                .contains(&victory_shader::emit::SLOT_MEMORY)
            {
                let ready = {
                    let _lap = BindLap::start(3);
                    self.memory_ready(list, compute, writes)
                };
                let Some(ready) = ready else {
                    self.scratch_made = made;
                    once!(
                        "a shader that reads title memory by address without a view heap to hold its views: the draw is dropped"
                    );
                    return false;
                };
                Some(ready)
            } else {
                None
            };
            if let Some((param, dwords)) = stage.user_data
                && dwords > 0
            {
                let mut stack = [0u32; 64];
                let mut heap_buf = Vec::new();
                let values: &mut [u32] = if (dwords as usize) <= stack.len() {
                    &mut stack[..dwords as usize]
                } else {
                    heap_buf.resize(dwords as usize, 0);
                    &mut heap_buf
                };
                for u in &t.host.user_data {
                    if let RootType::Other(kind) = u.root {
                        values[u.index as usize] =
                            self.host_word(compute, kind, u.slot, u.dword, memory.as_ref());
                        continue;
                    }
                    let fresh = made
                        .iter()
                        .find(|m| m.0 == u.slot && u.root == RootType::Table);
                    values[u.index as usize] = match fresh {
                        Some(&(_, offset)) => offset,
                        None => self.user_value(compute, u.slot, u.root, u.dword),
                    };
                }
                // SAFETY: an open list; `values` covers the parameter.
                unsafe {
                    if compute {
                        l.SetComputeRoot32BitConstants(param, dwords, values.as_ptr().cast(), 0);
                    } else {
                        l.SetGraphicsRoot32BitConstants(param, dwords, values.as_ptr().cast(), 0);
                    }
                }
            }
            for (views, srv) in [(&stage.root_cbvs, false), (&stage.root_srvs, true)] {
                for &(slot, param) in views {
                    if srv && slot == victory_shader::emit::SLOT_MEMORY {
                        // Ours: the directory of title memory by address.
                        if let Some(m) = &memory {
                            // SAFETY: an open list; the directory lives as
                            // long as the process and the list holds it.
                            unsafe {
                                if compute {
                                    l.SetComputeRootShaderResourceView(param, m.directory);
                                } else {
                                    l.SetGraphicsRootShaderResourceView(param, m.directory);
                                }
                            }
                        }
                        continue;
                    }
                    if srv && slot == victory_shader::emit::SLOT_STRIDES {
                        // Ours, not the title's: the shader picks buffers
                        // from the view heap by an index it computes.
                        match self.heap_buffers(list) {
                            // SAFETY: an open list; the table lives as long
                            // as the heap the list holds.
                            Some(gpu) => unsafe {
                                if compute {
                                    l.SetComputeRootShaderResourceView(param, gpu);
                                } else {
                                    l.SetGraphicsRootShaderResourceView(param, gpu);
                                }
                            },
                            None => {
                                once!("a shader picks buffers from a view heap that is not set")
                            }
                        }
                        continue;
                    }
                    let tabled = srv && crate::pipeline::host::bounded_root_srvs();
                    let Some(&Root::View(address)) = self.roots_ref(compute).get(usize::from(slot))
                    else {
                        once!(slot, "a shader reads a root view the title did not set");
                        // A table left unset is undefined: a null view.
                        if tabled && !self.root_srv_table(list, None, param, compute, false) {
                            return false;
                        }
                        continue;
                    };
                    // The shader adds the bytes a raw view begins early by.
                    let biased = srv
                        && t.host.user_data.iter().any(|u| {
                            u.slot == slot
                                && u.root == RootType::Other(victory_shader::emit::USER_VIEW_BIAS)
                        });
                    if tabled {
                        if !self.root_srv_table(list, Some(address), param, compute, biased) {
                            return false;
                        }
                        continue;
                    }
                    let Some(mut gpu) = root_view(list, address, srv) else {
                        continue;
                    };
                    if biased {
                        gpu -= u64::from(self.host_word(
                            compute,
                            victory_shader::emit::USER_VIEW_BIAS,
                            slot,
                            0,
                            None,
                        ));
                    }
                    // SAFETY: an open list; the address is in live memory.
                    unsafe {
                        match (compute, srv) {
                            (false, false) => l.SetGraphicsRootConstantBufferView(param, gpu),
                            (false, true) => l.SetGraphicsRootShaderResourceView(param, gpu),
                            (true, false) => l.SetComputeRootConstantBufferView(param, gpu),
                            (true, true) => l.SetComputeRootShaderResourceView(param, gpu),
                        }
                    }
                }
            }
            for &(slot, param) in &stage.root_uavs {
                if slot == victory_shader::emit::SLOT_MEET {
                    if let Some(gpu) = crate::defrag::bind_defrag_counter(&list.list) {
                        // SAFETY: an open list; the buffer lives for the process.
                        unsafe {
                            if compute {
                                l.SetComputeRootUnorderedAccessView(param, gpu);
                            }
                        }
                    }
                    continue;
                }
                if slot == victory_shader::emit::SLOT_FAULTS {
                    // Ours: where shaders note the pages they found empty.
                    if let Some(m) = &memory {
                        // SAFETY: an open list; the buffer lives as long as
                        // the process and the list holds it.
                        unsafe {
                            if compute {
                                l.SetComputeRootUnorderedAccessView(param, m.faults);
                            } else {
                                l.SetGraphicsRootUnorderedAccessView(param, m.faults);
                            }
                        }
                    }
                    continue;
                }
                let address = match self.roots_ref(compute).get(usize::from(slot)) {
                    Some(&Root::View(address)) => Some(address),
                    _ => {
                        once!(
                            slot,
                            "a shader writes through a root view the title did not set"
                        );
                        None
                    }
                };
                if !self.root_uav(list, address, param, compute, writes) {
                    return false;
                }
            }
        }
        self.scratch_made = made;
        // Depth textures it writes through unordered access: their colour
        // twins hold what they lack from here on.
        list.twins_written();
        true
    }

    /// Binds a root UAV: a raw unordered access view of the title buffer's
    /// host copy from `address` on, as a one-view table (a null view
    /// without an address, without a host copy shaders can write, or at an
    /// address not on 16 bytes), or as a root descriptor with
    /// `VICTORY_ROOT_SRV_DIRECT=1` (the dispatch is dropped then if there
    /// is nothing to bind). With `writes`, what the shader stored is copied
    /// back into title memory when the list has run. False if the draw
    /// must be dropped.
    /// shortcut: a root view has no size: the view, the refresh and the
    /// copy back cover all that is left of the title's buffer
    /// (`VICTORY_ROOT_UAV_MB` sets a limit for all three).
    fn root_uav(
        &mut self,
        list: &HostList,
        address: Option<u64>,
        param: u32,
        compute: bool,
        writes: bool,
    ) -> bool {
        const UNORDERED: i32 = D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS.0;
        static LIMIT: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
        let limit = *LIMIT.get_or_init(|| {
            std::env::var("VICTORY_ROOT_UAV_MB")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .map_or(u64::MAX, |mb| mb << 20)
        });
        let find = |a: u64| {
            let s = crate::shadow::find(a, 1)?;
            // SAFETY: a live buffer.
            let flags = unsafe { s.buffer.GetDesc() }.Flags;
            if flags.0 & UNORDERED == 0 {
                once!("a root UAV of a buffer whose host copy shaders cannot write");
                return None;
            }
            Some(s)
        };
        let ready = |a: u64, s: &std::sync::Arc<crate::shadow::Shadow>| {
            let len = s.left(a).min(limit);
            s.refresh(list, a, len);
            if writes {
                s.gpu_writes(list, a, len);
            }
        };
        if !crate::pipeline::host::bounded_root_srvs() {
            let Some((a, s)) = address.and_then(|a| Some((a, find(a)?))) else {
                once!("a root UAV with nothing to bind: the draw is dropped");
                return false;
            };
            ready(a, &s);
            // From the 16 bytes below: the shader adds the rest.
            // SAFETY: a live buffer.
            let base = unsafe { s.buffer.GetGPUVirtualAddress() };
            let gpu = base + ((s.gpu(a) - base) & !15);
            // SAFETY: an open list; the address is in a live buffer.
            unsafe {
                if compute {
                    list.list.SetComputeRootUnorderedAccessView(param, gpu);
                } else {
                    list.list.SetGraphicsRootUnorderedAccessView(param, gpu);
                }
            }
            return true;
        }
        let heap = match self.heaps[0] {
            0 => None,
            h => Some(crate::descriptor::heap(h as This)),
        };
        let Some((heap, host)) = heap.and_then(|h| Some((h, h.host()?))) else {
            once!("a root UAV without a bound view heap: the draw is dropped");
            return false;
        };
        let key = (std::ptr::from_ref(heap) as usize, address.unwrap_or(0));
        let (index, shadow) = match self.root_uav_slots.get(&key) {
            Some((index, shadow)) => (*index, address.zip(shadow.clone())),
            None => {
                let shadow = address.and_then(|a| find(a).map(|s| (a, s)));
                let Some(index) = host.dynamic(1) else {
                    return false;
                };
                self.dynamic.push((key.0, host.id(), index, 1));
                if let Some((a, s)) = &shadow {
                    // SAFETY: a live buffer.
                    let base = unsafe { s.buffer.GetGPUVirtualAddress() };
                    let offset = s.gpu(*a) - base;
                    // Raw views start on 16 bytes: from the 16 bytes
                    // below, and the shader adds the rest to its offsets
                    // (`victory_shader::emit::USER_VIEW_BIAS`). (An address
                    // not on 16 bytes got a null view at first: the
                    // builder's counters are at such addresses.)
                    let bias = offset & 15;
                    host.raw_uav_at(
                        index,
                        &s.buffer,
                        offset - bias,
                        s.left(*a).min(limit) + bias,
                    );
                }
                if shadow.is_none() {
                    host.null_raw_uav_at(index);
                }
                self.root_uav_slots
                    .insert(key, (index, shadow.as_ref().map(|(_, s)| s.clone())));
                (index, shadow)
            }
        };
        if let Some((a, s)) = &shadow {
            ready(*a, s);
        }
        let handle = host.gpu_handle(index);
        // SAFETY: an open list; the slot is in the bound view heap.
        unsafe {
            if compute {
                list.list.SetComputeRootDescriptorTable(param, handle);
            } else {
                list.list.SetGraphicsRootDescriptorTable(param, handle);
            }
        }
        true
    }

    /// Binds a root SRV as a one-view table
    /// ([`crate::pipeline::host::bounded_root_srvs`]): a raw view of the
    /// title buffer's host copy from `address` to the copy's end, or a null
    /// view (no address, no host copy, or an address not on 16 bytes). One
    /// dynamic slot per address per list, given back with the list's
    /// others. False if no slot could be had or no view heap is bound.
    fn root_srv_table(
        &mut self,
        list: &HostList,
        address: Option<u64>,
        param: u32,
        compute: bool,
        biased: bool,
    ) -> bool {
        let heap = match self.heaps[0] {
            0 => None,
            h => Some(crate::descriptor::heap(h as This)),
        };
        let Some((heap, host)) = heap.and_then(|h| Some((h, h.host()?))) else {
            once!("a root SRV without a bound view heap: the draw is dropped");
            return false;
        };
        // A view that begins early is another view than one that does not
        // (title addresses stay below bit 63).
        let key = (
            std::ptr::from_ref(heap) as usize,
            address.unwrap_or(0) | u64::from(biased) << 63,
        );
        let (index, shadow) = match self.root_srv_slots.get(&key) {
            Some((index, shadow)) => (*index, address.zip(shadow.clone())),
            None => {
                let shadow = address.and_then(|a| crate::shadow::find(a, 1).map(|s| (a, s)));
                let Some(index) = host.dynamic(1) else {
                    return false;
                };
                self.dynamic.push((key.0, host.id(), index, 1));
                self.root_srv_slots
                    .insert(key, (index, shadow.as_ref().map(|(_, s)| s.clone())));
                match &shadow {
                    Some((a, s)) => {
                        // SAFETY: a live buffer.
                        let base = unsafe { s.buffer.GetGPUVirtualAddress() };
                        let offset = s.gpu(*a) - base;
                        if biased {
                            // From the 16 bytes below; the shader adds the
                            // rest (`victory_shader::emit::USER_VIEW_BIAS`).
                            let bias = offset & 15;
                            host.raw_at(index, &s.buffer, offset - bias, s.left(*a) + bias);
                        } else if offset.is_multiple_of(16) {
                            host.raw_at(index, &s.buffer, offset, s.left(*a));
                        } else {
                            // Raw views start on 16 bytes; one from the
                            // aligned start would shift every read.
                            once!(
                                address = format_args!("{a:#x}"),
                                "a root SRV not on 16 bytes reads zeros"
                            );
                            host.null_raw_at(index);
                        }
                    }
                    None => host.null_raw_at(index),
                }
                (index, shadow)
            }
        };
        // As for root descriptors (`root_view`): all that is left of the
        // buffer is brought up to date. The first 1 MiB only left what lay
        // past it stale (most mesh vertex streams reach further).
        if let Some((a, s)) = &shadow {
            s.refresh(list, *a, s.left(*a));
        }
        let handle = host.gpu_handle(index);
        // SAFETY: an open list; the slot is in the bound view heap.
        unsafe {
            if compute {
                list.list.SetComputeRootDescriptorTable(param, handle);
            } else {
                list.list.SetGraphicsRootDescriptorTable(param, handle);
            }
        }
        true
    }

    /// For a shader that picks buffers from the view heap by an index it
    /// computes (SM 6.6 dynamic resources): the GPU address of the heap's
    /// table of element strides, with every buffer view of the heap given
    /// its host slot and its memory brought up to date when `list` is
    /// submitted. Tables tell us which views a draw reads; nothing tells
    /// us which of the heap's a computed index names, so all of them are
    /// made ready, once per list.
    /// shortcut: the buffers are read, not written: what such a shader
    /// writes through a picked unordered access view stays in the host
    /// copy. A list refreshes them where it first runs such a shader;
    /// title writes recorded later in the same list are not seen.
    fn heap_buffers(&mut self, list: &HostList) -> Option<u64> {
        let _hitch = crate::hitch::span(crate::hitch::Phase::HeapBuffers);
        let heap = self
            .heaps
            .iter()
            .filter(|&&h| h != 0)
            .map(|&h| crate::descriptor::heap(h as This))
            .find(|h| !h.samplers())?;
        let host = heap.host()?;
        let gpu = host.strides_gpu()?;
        // One key per list, as textures have: no title resource is there.
        const ONCE: usize = usize::MAX - 0x5e;
        if !list.first_use(ONCE) {
            return Some(gpu);
        }
        // The list may outlive the title's heap object, which owns the table.
        if let Some(buffer) = host.strides_buffer() {
            list.keep(buffer);
        }
        // The pass itself once per frame: per list it ran up to 51 times a
        // frame over 81,000 views and 4,100 host copies, 18 s of every
        // frame (2026-11-24, run 11).
        // shortcut: what the title writes into such buffers between lists
        // of one frame is seen a frame late.
        static FRAME: AtomicU64 = AtomicU64::new(u64::MAX);
        let frame = crate::frame::current();
        if FRAME.swap(frame, Ordering::Relaxed) == frame {
            return Some(gpu);
        }
        let started = std::time::Instant::now();
        // Per host heap: the host copy and range each slot's buffer view
        // reads, kept from pass to pass. Only the slots written since the
        // last pass are looked at again (83,000 slots a pass otherwise, 0.1 s
        // at best); all of them every 16 passes, because a view of a
        // released resource goes stale without a write, and a slot written
        // before its memory had a host copy is only made then.
        // Which slots are looked at is `crate::heap_pass` (the full sweep every
        // 16 passes unless `VICTORY_HEAP_PASS_DIRTY=1`).
        let mut kept = RANGES.lock().unwrap_or_else(|e| e.into_inner());
        let heaps = kept.get_or_insert_with(Default::default);
        let forgotten = crate::descriptor::take_forgotten();
        for picked in heaps.values_mut() {
            picked.note_forgotten(forgotten.as_deref());
        }
        let ranges = heaps
            .entry(host.id())
            .or_insert_with(|| PickedRanges::new(crate::heap_pass::full_pass()));
        let changed = host.buffer_changes();
        let record = |index: u32| heap.handle(index).map(crate::descriptor::record_at);
        // Always take dirty pages (including on pass 0) so `buffer_written`
        // records its baseline stamps before pass 0 reads every slot.
        let dirty_slots: Option<Vec<(u32, u32)>> = if ranges.will_scan() {
            heap.dirty_buffer_pages()
        } else {
            None
        }
        .and_then(|byte_ranges| {
            let stride = heap.slot_stride();
            if stride == 0 {
                return None;
            }
            let start = heap.start as usize;
            let count = heap.count() as usize;
            let mut slot_ranges = Vec::with_capacity(byte_ranges.len());
            for (a, b) in byte_ranges {
                let lo = if a > start {
                    let off = a - start;
                    let base = off / stride;
                    if off % stride != 0 {
                        base.saturating_sub(1)
                    } else {
                        base
                    }
                } else {
                    0
                };
                let hi = b.saturating_sub(start).div_ceil(stride).min(count);
                if lo < hi {
                    slot_ranges.push((lo as u32, hi as u32));
                }
            }
            Some(slot_ranges)
        });
        let slots = ranges.plan_in_ranges(
            changed,
            || host.buffer_slots(),
            record,
            crate::descriptor::record_entry,
            dirty_slots.as_deref(),
        );
        // Whether the device was there before the pass: a view that removes
        // it is named below.
        static GONE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        let mut alive =
            !GONE.load(Ordering::Relaxed) && crate::descriptor::host::device_removed().is_none();
        HEAP_PASS_PARTS[1].fetch_add(slots.len() as u64, Ordering::Relaxed);
        for (n, &index) in slots.iter().enumerate() {
            let Some(handle) = heap.handle(index) else {
                ranges.forget(index);
                continue;
            };
            let record = crate::descriptor::record_at(handle);
            let entry = crate::descriptor::record_entry(record);
            let Some((view, resolved)) = host.mirror_heap_buffer(index, handle) else {
                ranges.forget(index);
                continue;
            };
            // Whether making the view just resolved the copy the slot reads
            // (`buffer_copy`): the slot then shows that copy, as it was put
            // in `kept` with it.
            #[cfg(feature = "diagnostics")]
            let made = resolved.is_some();
            // Every 64 views while the device lives (a cheap query).
            if alive
                && n % 64 == 63
                && let Some(why) = crate::descriptor::host::device_removed()
            {
                alive = false;
                GONE.store(true, Ordering::Relaxed);
                let last: Vec<String> = slots[n - 63..=n]
                    .iter()
                    .filter_map(|&i| {
                        crate::descriptor::view(heap.handle(i)?).map(|v| format!("{i}: {v:?}"))
                    })
                    .map(|s| s.chars().take(200).collect())
                    .collect();
                tracing::error!(
                    why,
                    slot = index,
                    "the host device went while buffer views of the heap were made; one of: {last:#?}"
                );
                crate::host::log_removal_context();
            }
            // The copy the slot shows (`buffer_copy`), not the one the
            // view's range alone would find: the one making the view just
            // resolved, when it did, and only otherwise looked up again.
            let (shadow, address, len) = match resolved {
                Some(copy) => copy,
                None => match crate::descriptor::host::buffer_copy(&view) {
                    Some(copy) => copy,
                    None => {
                        if crate::descriptor::host::buffer_range(&view)
                            .is_some_and(|(_, len)| len > 0)
                        {
                            ranges.orphan(index, record, entry);
                        } else {
                            ranges.forget(index);
                        }
                        continue;
                    }
                },
            };
            #[cfg(feature = "diagnostics")]
            if !made && host.shows(index, &shadow.buffer) == Some(false) {
                HEAP_VIEWS_ELSEWHERE.fetch_add(1, Ordering::Relaxed);
            }
            let key = std::sync::Arc::as_ptr(&shadow) as usize;
            ranges.keep(index, key, &shadow, [address, address + len], record, entry);
        }
        let views = ranges.views() as u32;
        // Memory that is not write-watched is copied whole by every refresh.
        // It was once refreshed only every 120 frames (unmapped pages read
        // one by one cost 14 GB a frame in run 11, before they were
        // remembered): streamed geometry then drew from stale buffers, and
        // the garbage ended runs (the card stopped answering 14-90 frames
        // into the opening cutscene; with every frame it ran on). The pass
        // costs some 30 ms a frame this way.
        static WHOLE: std::sync::Mutex<Option<std::collections::HashMap<usize, u64>>> =
            std::sync::Mutex::new(None);
        // `VICTORY_UNWATCHED_FRAMES` sets the interval (1: every frame).
        static EVERY: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
        let every = *EVERY.get_or_init(|| {
            std::env::var("VICTORY_UNWATCHED_FRAMES")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(1)
        });
        let mut copies = 0usize;
        let mut bytes = 0u64;
        {
            let mut whole = WHOLE.lock().unwrap_or_else(|e| e.into_inner());
            let whole = whole.get_or_insert_with(Default::default);
            // Per host copy, the span its picked views cover (visited in place,
            // without allocating a Vec or cloning Arc<Shadow> per copy).
            ranges.for_each_span(|shadow, from, to| {
                copies += 1;
                bytes += to - from;
                if shadow.unwatched() {
                    // By the copy's number: a copy made where one let go was
                    // (`shadow::let_go_idle`) has never been refreshed.
                    let key = shadow.id() as usize;
                    match whole.get(&key) {
                        Some(&last) if frame < last + every => return,
                        _ => {
                            whole.insert(key, frame);
                        }
                    }
                }
                shadow.refresh(list, from, to - from);
            });
        }
        drop(kept);
        let textures_began = std::time::Instant::now();
        {
            let _hitch = crate::hitch::span(crate::hitch::Phase::HeapTextures);
            self.heap_textures(list, heap, host);
        }
        HEAP_PASS_PARTS[0].fetch_add(
            textures_began.elapsed().as_micros() as u64,
            Ordering::Relaxed,
        );
        HEAP_BUFFER_PASSES.fetch_add(1, Ordering::Relaxed);
        HEAP_BUFFER_PASSES_TOTAL.fetch_add(1, Ordering::Relaxed);
        HEAP_BUFFER_MICROS.fetch_add(started.elapsed().as_micros() as u64, Ordering::Relaxed);
        static LOGGED: AtomicU64 = AtomicU64::new(0);
        // The first pass and every growth of the set by half.
        let seen = LOGGED.load(Ordering::Relaxed);
        if u64::from(views) > seen + seen / 2 {
            LOGGED.store(u64::from(views), Ordering::Relaxed);
            tracing::info!(
                slots = slots.len(),
                views,
                copies,
                mb = bytes >> 20,
                ms = started.elapsed().as_millis(),
                "buffer views of the heap made ready for shaders that pick them by index"
            );
        }
        Some(gpu)
    }

    /// Runs the pass that uploads textures picked by index
    /// ([`State::heap_textures`]) where nothing has asked for it yet
    /// ([`early_pass_config`]): once a frame for a few milliseconds, at the
    /// frame's first draw with a view heap set (boosted on low-draw
    /// loading-screen frames when due textures wait), and with a larger
    /// budget of its own before every draw whose pixel shader is one of the
    /// title's bake programs. The uploads are recorded into `list` ahead
    /// of the draw being bound, as the world's pass records them.
    fn early_textures(&mut self, list: &HostList, p: &HostPipeline, compute: bool) {
        let _hitch = crate::hitch::span(crate::hitch::Phase::EarlyTextures);
        let cfg = early_pass_config();
        let heap_passes = HEAP_BUFFER_PASSES_TOTAL.load(Ordering::Relaxed);
        let world_running = heap_passes > 60;
        let bake_ms = cfg.bake_ms.unwrap_or(if world_running { 32 } else { 10_000 });
        // Once gameplay is running, cap bake draws (`VICTORY_BAKE_GAMEPLAY_MS`,
        // default 32 ms) so a launcher-level `VICTORY_BAKE_TEXTURES_MS=500`
        // still gives fast loading bakes without 500 ms gameplay hitches.
        let bake_ms = if world_running {
            static GAMEPLAY_BAKE_MS: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
            let cap = *GAMEPLAY_BAKE_MS.get_or_init(|| {
                std::env::var("VICTORY_BAKE_GAMEPLAY_MS")
                    .ok()
                    .and_then(|v| v.trim().parse().ok())
                    .unwrap_or(32)
            });
            bake_ms.min(cap)
        } else {
            bake_ms
        };
        if compute || (cfg.early_ms == 0 && bake_ms == 0 && !cfg.loading_drain) {
            return;
        }
        let bake = bake_ms > 0 && p.is_bake;
        static EARLY_FRAME: AtomicU64 = AtomicU64::new(u64::MAX);
        let frame = crate::frame::current();
        let left = if bake {
            bake_left(crate::queue::present_calls(), bake_ms, crate::host::now_ms())
        } else {
            0
        };
        if EARLY_FRAME.load(Ordering::Relaxed) == frame && !(bake && left > 0) {
            return;
        }
        // Look up the view heap before claiming the frame's `first` slot so
        // draws without a view heap do not waste the frame's early pass.
        let Some(heap) = self
            .heaps
            .iter()
            .filter(|&&h| h != 0)
            .map(|&h| crate::descriptor::heap(h as This))
            .find(|h| !h.samplers())
        else {
            return;
        };
        let Some(host) = heap.host() else {
            return;
        };
        let first = EARLY_FRAME.load(Ordering::Relaxed) != frame
            && EARLY_FRAME.swap(frame, Ordering::Relaxed) != frame;
        if !first && !(bake && left > 0) {
            return;
        }
        let (prev_frame_draws, _) = note_frame_draws(frame);
        let has_due_textures = first
            && cfg.loading_drain
            && prev_frame_draws < LOADING_DRAIN_MAX_DRAWS
            && host.texture_slots_has_due(frame);
        let input = EarlyPassInput {
            first_in_frame: first,
            is_bake: bake,
            bake_ms_left: left,
            prev_frame_draws,
            has_due_textures,
            world_gameplay: heap_passes > 0
                && HEAP_TEXTURES_WAITING.load(Ordering::Relaxed) < 2500,
            texture_budget_remaining: crate::resource::texture_budget_remaining(),
        };
        let Some((kind, budget)) = compute_early_pass_budget(cfg, input) else {
            return;
        };
        let is_bake = matches!(kind, EarlyPassKind::Bake);
        let n = EARLY_TEXTURE_PASSES[usize::from(is_bake)].fetch_add(1, Ordering::Relaxed) + 1;
        if is_bake && n.is_power_of_two() {
            let [slots, due, ..] = HEAP_TEXTURES_QUEUE
                .each_ref()
                .map(|c| c.load(Ordering::Relaxed));
            tracing::info!(
                frame,
                bakes = n,
                queued_at_the_last_pass = slots,
                due_at_the_last_pass = due,
                uploaded_so_far = HEAP_TEXTURES.load(Ordering::Relaxed),
                "a draw of the title's decoration bake: textures picked by index are uploaded first (VICTORY_BAKE_TEXTURES_MS=0: not)"
            );
        } else if matches!(kind, EarlyPassKind::LoadingDrain) {
            static LOADING_PASSES: AtomicU64 = AtomicU64::new(0);
            let drain_n = LOADING_PASSES.fetch_add(1, Ordering::Relaxed) + 1;
            if drain_n.is_power_of_two() {
                let [slots, due, ..] = HEAP_TEXTURES_QUEUE
                    .each_ref()
                    .map(|c| c.load(Ordering::Relaxed));
                tracing::info!(
                    frame,
                    passes = drain_n,
                    prev_frame_draws,
                    budget_ms = budget.0,
                    budget_mb = budget.1 >> 20,
                    queued_at_the_last_pass = slots,
                    due_at_the_last_pass = due,
                    uploaded_so_far = HEAP_TEXTURES.load(Ordering::Relaxed),
                    "loading-screen / low-draw frame: boosting texture pre-drain pass budget (VICTORY_LOADING_DRAIN=0: off)"
                );
            }
        }
        PASS_BUDGET.set(Some(budget));
        self.heap_textures(list, heap, host);
        PASS_BUDGET.set(None);

    }

    /// The queue's side of the card accounting line
    /// (`crate::resource::card_accounting`, which says what the line
    /// holds): the slots that are due, by what their texture has on the
    /// card. Off unless `VICTORY_CARD_ACCOUNTING=1`; then at the first pass
    /// that finds the card without room, and every five seconds after it.
    /// Nothing is made, uploaded or counted as used here. For 18,000 due
    /// slots: 18,000 looks at a title slot and at the texture it views, a
    /// lock each, some milliseconds of the thread that records the frame.
    fn card_accounting(
        heap: &crate::descriptor::Heap,
        host: &crate::descriptor::host::HostHeap,
        frame: u64,
        room: bool,
    ) {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if !*ON
            .get_or_init(|| std::env::var_os("VICTORY_CARD_ACCOUNTING").is_some_and(|v| v == "1"))
        {
            return;
        }
        // When the next line is due (0: none yet, the card has had room).
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let now = crate::host::now_ms().max(1);
        let next = NEXT.load(Ordering::Relaxed);
        // (With levels on demand the card may never be without room: the
        // lines then start with the first texture made that way.)
        let by_level = crate::tiles::STATS[4].load(Ordering::Relaxed) > 0;
        let due_now = if next == 0 {
            !room || by_level
        } else {
            now >= next
        };
        if !due_now
            || NEXT
                .compare_exchange(next, now + 5000, Ordering::Relaxed, Ordering::Relaxed)
                .is_err()
        {
            return;
        }
        // Due slots; of them uploaded before; of them viewing nothing the
        // title fills; and the textures (each once) whose host copy is
        // alive, with their bytes on the card.
        let mut due = [0u64; 5];
        // Textures (each once) that never had a host copy, and ones whose
        // host copy was let go: where their memory is, and its bytes.
        let (mut never, mut gone) = (Vec::new(), Vec::new());
        let mut seen = std::collections::BTreeSet::new();
        for (index, uploaded) in host.texture_slots_all_due(frame) {
            due[0] += 1;
            due[1] += u64::from(uploaded);
            let state = match heap.handle(index).and_then(crate::descriptor::view) {
                Some(View::Placed(false, resource, desc)) if resource[0] != 1 => {
                    let address = u64::from(desc[10]) | u64::from(desc[11]) << 32;
                    match crate::resource::placed_known(address, &resource) {
                        Some(texture) => crate::resource::copy_state(texture),
                        // No title texture was made for it: the pass never
                        // got as far as making one.
                        None => Some((
                            0,
                            crate::resource::placed_bytes(&resource).unwrap_or(0),
                            address,
                        )),
                    }
                }
                Some(View::Shader(object, _)) if crate::resource::title_fills(object as This) => {
                    crate::resource::copy_state(object as This)
                }
                _ => None,
            };
            let Some((state, bytes, memory)) = state else {
                due[2] += 1;
                continue;
            };
            if !seen.insert(memory) {
                continue;
            }
            match state {
                2 => {
                    due[3] += 1;
                    due[4] += bytes;
                }
                1 => gone.push((memory, bytes)),
                _ => never.push((memory, bytes)),
            }
        }
        crate::resource::card_accounting(room, due, never, gone);
    }

    /// The texture side of [`State::heap_buffers`]: textures the title fills
    /// itself (streamed pages, layout-library copies) whose views sit in the
    /// view heap are created (placed textures have no host texture until
    /// something reads them) and uploaded, as a table read would do, for
    /// shaders that pick them by a computed index. Within a budget of bytes
    /// per frame and in all; the rest waits for later lists and frames.
    /// The most recently written slots first (`TextureQueue::due`); slots
    /// draws read through tables are not in the queue.
    /// shortcut: uploaded once, and again only when the title rewrites the
    /// slot. A placed texture not all mapped yet is uploaded with what is
    /// there and looked at again (1, 2, 4, ... frames later, at most about
    /// a second): uploaded again when more of it is mapped. Pages mapped
    /// in place of others, and contents changed with no descriptor write,
    /// are missed (hook the page-mapping events for those).
    /// `VICTORY_HEAP_TEXTURES=0` turns it off.
    fn heap_textures(
        &mut self,
        list: &HostList,
        heap: &crate::descriptor::Heap,
        host: &crate::descriptor::host::HostHeap,
    ) {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if !*ON.get_or_init(|| std::env::var_os("VICTORY_HEAP_TEXTURES").is_none_or(|v| v != "0")) {
            return;
        }
        let entered = std::time::Instant::now();
        /// Bytes of texture memory staged per frame.
        // `VICTORY_TEXTURE_PASS_MB` overrides it directly; otherwise
        // `VICTORY_TEXTURE_STAGE_MB` raises it so a backlog of tens of
        // thousands of textures drains in fewer frames.
        static PER_FRAME_MB: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
        let pass_override = early_pass_config().pass_bytes_override;
        let per_frame: u64 = pass_override.unwrap_or_else(|| {
            *PER_FRAME_MB.get_or_init(|| {
                std::env::var("VICTORY_TEXTURE_STAGE_MB")
                    .ok()
                    .and_then(|v| v.trim().parse().ok())
                    .unwrap_or(192)
            }) << 20
        });
        static FRAME: AtomicU64 = AtomicU64::new(u64::MAX);
        static SPENT: AtomicU64 = AtomicU64::new(0);
        let frame = crate::frame::current();
        if FRAME.swap(frame, Ordering::Relaxed) != frame {
            SPENT.store(0, Ordering::Relaxed);
        }
        // No more of them are made while the card has no room
        // (`resource::room`): by what is alive now, not by what was ever
        // staged (4 GB in all, after which later textures stayed empty for
        // the run). The ones nothing names any more are let go, which
        // makes room again: so the slots are still looked at (the pass
        // returned here, and a slot whose texture had been streamed out
        // kept its host copy for as long as the card was full).
        let mut room = crate::resource::room();
        // Bytes uploaded by this pass since the card was last asked
        // whether it has room: asked again every 32 MiB (a pass may upload
        // 192 MB and more, and one asking a pass let the queue run some
        // 400 MB past its line while the world loaded: run 193).
        let mut since_asked = 0u64;
        if !room {
            HEAP_TEXTURES_FULL.fetch_add(1, Ordering::Relaxed);
            once!("textures picked by index: the card has no room for more; they wait");
        }
        let mut refused = 0;
        // Room again: the slots held back while the card was full are due
        // now (`full_card_pass`).
        if room {
            host.texture_slots_room(frame);
        }
        // Slots whose texture's memory the runtime mapped, unmapped or
        // wrote since the last pass are due again.
        host.texture_slots_remapped(frame);
        // Levels on demand: the slots of textures whose first level with
        // memory changed since the last pass are made again from it.
        host.texture_views_remade();
        // And with room, the textures whose upload left a held level
        // without the card's memory are uploaded again.
        if room {
            host.texture_slots_waiting(frame);
        }
        host.texture_sweep(heap, frame);
        if heap_recheck() {
            let [same, changed, waiting] = host.texture_slots_rechecked(frame);
            HEAP_TEXTURES_RECHECKED[0].fetch_add(same, Ordering::Relaxed);
            HEAP_TEXTURES_RECHECKED[1].fetch_add(changed, Ordering::Relaxed);
            HEAP_TEXTURES_RECHECKED[2].store(waiting, Ordering::Relaxed);
        }
        let slot_limit = match PASS_BUDGET.get() {
            Some((_, bytes, _)) if bytes >= (1 << 30) => usize::MAX,
            _ => 4096,
        };
        let (mut due, census) = host.texture_slots_due(frame, slot_limit);
        HEAP_TEXTURES_WAITING.store(census[0], Ordering::Relaxed);
        for (count, now) in HEAP_TEXTURES_QUEUE.iter().zip(census) {
            count.store(now, Ordering::Relaxed);
        }
        // Where the card's memory is, and what the due slots would need
        // (`VICTORY_CARD_ACCOUNTING=1`; a line when the card first has no
        // room, and every five seconds after it).
        Self::card_accounting(heap, host, frame, room);
        // The frames in about four seconds, between 16 and 256: how long a
        // slot with nothing to upload waits at most for its next look. In
        // frames alone it was 120: four minutes at the opening cutscene's
        // half a frame per second, while the title streams by the clock.
        // (About a second before the runtime told of the memory it maps:
        // the looks are now only for what that does not tell.)
        static PASSED: AtomicU64 = AtomicU64::new(0);
        let now = crate::host::now_ms();
        let took = now
            .saturating_sub(PASSED.swap(now, Ordering::Relaxed))
            .max(1);
        // (`took` is since the last pass, of any heap and list: several a
        // frame, so this was often 256 frames; `VICTORY_TEXTURE_LOOK_MS`
        // counts by the frame's time.)
        let most = match look_ms() {
            Some(ms) => (ms / frame_ms(frame, now)).clamp(1, 256),
            None => (4000 / took).clamp(16, 256),
        };
        // What a table entry of a texture a shader samples says.
        let sampled = UsedResource {
            kind: ResourceType::Srv,
            root: RootType::Table,
            table_offset_dwords: 0,
            lower_bound: 0,
            stride: 0,
            dwords: 8,
            user_data: 0,
            slot: 0,
        };
        // Also within a time per pass: the pass runs on a thread that
        // records the frame, and making and staging 192 MB of textures took
        // 1.6-2.2 s of each of the world's first twenty frames (run 20).
        // The rest waits for the next frames, as it does past the bytes.
        static SPAN_MS: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
        let span = std::time::Duration::from_millis(*SPAN_MS.get_or_init(|| {
            std::env::var("VICTORY_TEXTURE_STAGE_MS")
                .ok()
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(250)
        }));
        // A long queue (the world's first minutes: over 20,000 slots, which
        // at the usual pace took three and a half minutes with the materials
        // black) gets 512 MB and 1.5 s a pass while the run's allowance for
        // catching up lasts (`catching_up`, `VICTORY_TEXTURE_GATE_S`, 60 s):
        // joshuu/victory-amd's pace, which brought more in than tripling the
        // usual budget once 1,024 were due. `VICTORY_TEXTURE_CATCHUP=0`
        // keeps the plain budget.
        const PER_FRAME_CATCHING_UP: u64 = 512 << 20;
        const SPAN_CATCHING_UP: std::time::Duration = std::time::Duration::from_millis(1500);
        const LONG: u64 = 2_000;
        static CATCHUP: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        let catchup = *CATCHUP
            .get_or_init(|| std::env::var_os("VICTORY_TEXTURE_CATCHUP").is_none_or(|v| v != "0"));
        let plain = span;
        let (per_frame, span) =
            if catchup && census[0] > LONG && catching_up(std::time::Duration::ZERO) {
                (
                    if pass_override.is_some() {
                        per_frame
                    } else {
                        per_frame.max(PER_FRAME_CATCHING_UP)
                    },
                    span.max(SPAN_CATCHING_UP),
                )
            } else {
                (per_frame, span)
            };
        // Smooth wall-clock budget for `heap_buffers -> heap_textures`
        // (`VICTORY_HEAP_TEXTURES_MS` overrides; otherwise 4 ms / 10 ms in
        // steady gameplay and 200 ms during initial world-entry loading passes
        // to eliminate main-thread upload spikes while draining streaming fast).
        static HEAP_TEXTURES_MS: std::sync::OnceLock<Option<u64>> = std::sync::OnceLock::new();
        let heap_textures_ms = *HEAP_TEXTURES_MS.get_or_init(|| {
            std::env::var("VICTORY_HEAP_TEXTURES_MS")
                .ok()
                .and_then(|v| v.trim().parse::<u64>().ok())
        });
        let smooth_budget = std::time::Duration::from_millis(match heap_textures_ms {
            Some(ms) => ms,
            None => {
                if HEAP_BUFFER_PASSES_TOTAL.load(Ordering::Relaxed) > 60 {
                    if due.len() < 4000 { 4 } else { 10 }
                } else {
                    40
                }
            }
        });
        // A run made early (`State::early_textures`) has a budget of its
        // own: its bytes on top of what the frame has spent so far.
        // (Not `most`: that is the longest wait between looks, used below.)
        let (budget, bytes_at_most) = match PASS_BUDGET.get() {
            Some((ms, bytes, _)) => (
                std::time::Duration::from_millis(ms),
                SPENT.load(Ordering::Relaxed).saturating_add(bytes),
            ),
            None => {
                let cap = if heap_textures_ms.is_some() {
                    smooth_budget
                } else {
                    smooth_budget.min(span)
                };
                (cap, per_frame)
            }
        };
        // (Neptune) `VICTORY_TEXTURE_PREPARE_THREADS`: the textures this pass
        // would make (host copy, staging buffer, detiling) are handed to
        // threads of their own, up to `per_frame` bytes a frame and out at
        // once (`crate::texture::prepare`); it records the uploads of the
        // ones they made ready. Not for a bake draw's run, whose textures
        // must be there now. Without the switch: as before.
        let prepare = crate::texture::prepare::threads() > 0
            && !PASS_BUDGET.get().is_some_and(|(_, _, now)| now);
        // What this pass recorded uploads of, by resource and first mip
        // (with `prepare`: a second view of one goes the old way, to `uploaded`).
        let mut recorded = std::collections::HashSet::new();
        // With `prepare`: nothing more may be handed out in this pass.
        let mut full = false;
        let [mut looked, mut handed, mut waited, mut uploads] = [0u64; 4];
        let ready_count = if prepare {
            crate::texture::prepare::prune();
            let (ready_count, skipped_waiting) = crate::texture::prepare::prioritize_due(&mut due);
            waited += skipped_waiting;
            looked += skipped_waiting;
            ready_count
        } else {
            0
        };
        let ready_budget = compute_ready_drain_budget(
            budget,
            PASS_BUDGET.get().is_none() && heap_textures_ms.is_none(),
        );
        let ([made_before, made_us_before], staged_before) =
            (crate::resource::made_here(), crate::texture::staged_here());
        HEAP_TEXTURE_PARTS[14].fetch_add(entered.elapsed().as_micros() as u64, Ordering::Relaxed);
        let mut laps = Laps::new();
        let began = std::time::Instant::now();
        let mut uploaded_this_pass = 0u64;
        let mut checked = 0u64;
        // Several slots can show the same texture from the same mip. The list
        // holds each upload even after its staging entry is removed. Reuse
        // it within this pass, where there are no intervening title copies.
        let mut uploaded: std::collections::HashMap<
            (usize, u32),
            Option<((u64, u64), u64, bool)>,
        > = std::collections::HashMap::new();
        for (i, (index, write)) in due.into_iter().enumerate() {
            // The time since the last lap: the end of the slot before.
            laps.lap(Laps::REST);
            let is_ready = prepare && i < ready_count;
            // All ready-prepared textures sit at `0..ready_count`; once the
            // prepare budget is full there is nothing more to hand out.
            if !is_ready && full {
                break;
            }
            let slot_budget = if is_ready { ready_budget } else { budget };
            if SPENT.load(Ordering::Relaxed) >= bytes_at_most
                || ((uploaded_this_pass > 0 || checked > 0) && entered.elapsed() >= slot_budget)
            {
                break;
            }
            checked += 1;
            looked += 1;
            // A placed texture whose memory is not mapped (not streamed in
            // yet, or streamed out again: the heap keeps tens of thousands
            // of such views) gets no host texture; looked at again soon (a
            // title writes a texture's view before its pages are mapped,
            // and the pass runs early in a frame). One of which some is
            // mapped (a texture streaming in: its smaller mips lie at the
            // start of its memory and arrive first) is uploaded with what
            // is there, and again when more is. (It was uploaded again
            // every 120 frames, changed or not; and asked by its first
            // page alone, one whose first page is the one missing counted
            // as not there.)
            let mut partly = None;
            if let Some(View::Placed(false, resource, desc)) =
                heap.handle(index).and_then(crate::descriptor::view)
            {
                let address = u64::from(desc[10]) | u64::from(desc[11]) << 32;
                // Of the mips the view shows: what lies behind them is
                // another texture's.
                let len = crate::resource::placed_bytes_from(
                    &resource,
                    crate::resource::first_mip(&desc),
                )
                .unwrap_or(1);
                let mapped = crate::texture::mapped_bytes(address as *const u8, len as usize);
                if mapped == 0 {
                    HEAP_TEXTURES_AWAY.fetch_add(1, Ordering::Relaxed);
                    // Streamed out: what the slot showed is let go.
                    host.texture_slot_gone(index);
                    host.texture_slot_later(index, write, frame, most);
                    continue;
                }
                if mapped < len {
                    if mapped <= host.texture_slot_mapped(index) {
                        HEAP_TEXTURES_PARTLY[0].fetch_add(1, Ordering::Relaxed);
                        host.texture_slot_later(index, write, frame, most);
                        continue;
                    }
                    if !crate::texture::committed(address as *const u8, 1) {
                        HEAP_TEXTURES_PARTLY[2].fetch_add(1, Ordering::Relaxed);
                    }
                    partly = Some(mapped);
                }
            }
            // A texture object the host renders to or writes holds the
            // host's own contents: nothing to bring up to date, and this
            // pass is no use of it (its host copy is made when something
            // uses it, and its slots then: `descriptor::make_waiting`).
            if let Some(View::Shader(object, _)) =
                heap.handle(index).and_then(crate::descriptor::view)
                && crate::resource::buffer_bytes(object as This).is_none()
                && !crate::resource::title_fills(object as This)
            {
                host.texture_slot_done(index, write, None);
                continue;
            }
            // From here on a host texture is made: not without room. The
            // slot stays due.
            // Not so a slot already made from this very view: its host
            // texture exists, and uploading it again takes none of the
            // card's memory. It was refused all the same, so a texture the
            // title filled after its first upload kept its empty host copy
            // from the moment the card reached the line, which is the first
            // world frame in eight launches of nine: the same slots were
            // found changed again at every pass and never uploaded (run
            // 179: 33,000 such findings), and the main character's face
            // had its colour only in the launches that entered the world
            // under the line. `VICTORY_REUPLOAD_WHEN_FULL=0`: refused, as
            // before.
            if !room {
                let has_its_texture = reupload_when_full()
                    && heap
                        .handle(index)
                        .is_some_and(|handle| host.texture_slot_made_from(index, handle));
                // (Neptune) Nor a slot whose texture a prepare thread made
                // ready (its host copy made while the card had room).
                let prepared = is_ready;
                if !has_its_texture && !prepared {
                    refused += 1;
                    if full_card_pass() {
                        // Held back for the frames of about four seconds
                        // (`most`), or until the card has room; the pass
                        // goes on, within a count that bounds one pass.
                        host.texture_slot_held(index, write, frame + most);
                        if refused > 1024 {
                            break;
                        }
                    } else if refused > 256 {
                        break;
                    }
                    continue;
                }
                if has_its_texture {
                    let n = HEAP_TEXTURES_REUPLOADED_FULL.fetch_add(1, Ordering::Relaxed) + 1;
                    if n == 1 || n.is_multiple_of(500) {
                        tracing::info!(
                            so_far = n,
                            slot = index,
                            "a texture already on the card is brought up to date although the card has no room for new ones"
                        );
                    }
                }
            }
            // (Neptune) With prepare threads: a texture whose host copy or
            // staging buffer would be made here is handed to them instead,
            // and the slot stays due; the next pass that finds it ready
            // records its upload below, as before.
            if prepare
                && let Some(seen) = heap.handle(index).and_then(crate::descriptor::view)
                && let Some(View::Shader(resource, _)) =
                    crate::descriptor::host::placed_texture(&seen).or_else(|| Some(seen.clone()))
            {
                let first = match &seen {
                    View::Placed(false, _, desc) => crate::resource::first_mip(desc),
                    _ => 0,
                };
                use crate::texture::prepare::Look;
                match crate::texture::prepare::look(
                    index,
                    resource as This,
                    first,
                    &recorded,
                    per_frame,
                    frame,
                ) {
                    Look::Go => {}
                    Look::Wait => {
                        waited += 1;
                        laps.lap(Laps::LOOK);
                        continue;
                    }
                    Look::Handed(bytes) => {
                        handed += 1;
                        // The card is asked again as often as when the pass
                        // makes them itself.
                        since_asked += bytes;
                        if room && since_asked >= 32 << 20 {
                            since_asked = 0;
                            room = crate::resource::room();
                        }
                        laps.lap(Laps::LOOK);
                        continue;
                    }
                    Look::Full => {
                        full = true;
                        laps.lap(Laps::LOOK);
                        continue;
                    }
                }
            }
            laps.lap(Laps::LOOK);
            let Some(handle) = heap.handle(index) else {
                host.texture_slot_done(index, write, None);
                continue;
            };
            let (view, deferred_key) = host.mirror_deferred(index, handle);
            let placed = view
                .as_ref()
                .and_then(crate::descriptor::host::placed_texture);
            laps.lap(Laps::MAKE);
            let resource = match placed.as_ref().or(view.as_ref()) {
                Some(&View::Shader(resource, _)) => resource as This,
                _ => {
                    if deferred_key.is_some() {
                        let _ = host.mirror(index, handle, true);
                    }
                    host.texture_slot_done(index, write, None);
                    continue;
                }
            };
            let title_fills =
                crate::resource::host_resource(resource).is_some_and(crate::texture::title_written);
            if !title_fills {
                // a render target, a depth or unordered access texture: the host's own
                if deferred_key.is_some() {
                    let _ = host.mirror(index, handle, true);
                }
                host.texture_slot_done(index, write, None);
                continue;
            }
            laps.lap(Laps::MAKE);
            let first = match &view {
                Some(View::Placed(false, _, desc)) => crate::resource::first_mip(desc),
                _ => 0,
            };
            let key = (resource as usize, first);
            let previous = uploaded.get(&key).copied();
            // A texture that has a staging buffer now is one draws read
            // through a table: they upload it, and check it at every
            // submission that samples it.
            let tabled = previous.is_none() && crate::texture::is_staged(resource);
            if tabled {
                crate::texture::upload_again(resource);
            }
            if previous.is_none()
                && let Some(view) = &view
            {
                if prepare {
                    crate::texture::prepare::serving(Some(index));
                }
                readable(list, sampled.kind, view);
                if prepare {
                    crate::texture::prepare::serving(None);
                    recorded.insert((resource as usize, first));
                }
            }
            laps.lap(Laps::STAGE);
            // A texture view the host refuses removes the device: name it.
            static GONE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
            if !GONE.load(Ordering::Relaxed)
                && let Some(why) = crate::descriptor::host::device_removed()
            {
                GONE.store(true, Ordering::Relaxed);
                let text: String = format!("{view:?}").chars().take(300).collect();
                tracing::error!(
                    why,
                    slot = index,
                    "the host device went while textures picked by index were made ready; last: {text}"
                );
                crate::host::log_removal_context();
                return;
            }
            if let Some(key) = deferred_key {
                self.pending_shows
                    .push((std::ptr::from_ref(heap) as usize, index, handle, key));
            }
            if let Some(recheck) = previous {
                let live = host.texture_slot_done(index, write, None);
                if let Some((memory, print, was_live)) = recheck {
                    let print = if was_live == live {
                        print
                    } else {
                        crate::texture::fingerprint_for(memory.0, memory.1, live)
                    };
                    host.texture_slot_recheck(index, frame, memory, print, live);
                }
                if prepare {
                    crate::texture::prepare::served(index, resource);
                }
                continue;
            }
            uploaded_this_pass += 1;
            let (bytes, whole) = crate::texture::staged(resource).unwrap_or((0, true));
            // Not all of it there: looked at again next frame, and from
            // then on as a slot with nothing new (a placed texture: until
            // more of it is mapped; a texture object at an address the
            // title mapped: again in 120 frames, as before).
            let again = match partly {
                Some(mapped) => Some((frame + 1, mapped)),
                None => (!whole).then_some((frame + 120, 0)),
            };
            if again.is_some() {
                HEAP_TEXTURES_PARTLY[1].fetch_add(1, Ordering::Relaxed);
            }
            if tabled {
                // Nothing was staged here, so nothing is counted or spent,
                // and the staging buffer stays: taking it away made the
                // next draw that reads the texture through a table stage,
                // detile and upload all of it once more.
                HEAP_TEXTURES_TABLED.fetch_add(1, Ordering::Relaxed);
                host.texture_slot_done(index, write, again);
                continue;
            }
            // Uploaded: the list holds the staging buffer until it has run;
            // a table read, or the next look at a texture not all mapped
            // yet, makes it anew. (Kept for those until now: every partly
            // streamed texture of the heap held a staging buffer of its full
            // size for good.)
            let memory = if again.is_none() && heap_recheck() {
                crate::texture::staged_memory(resource)
            } else {
                None
            };
            crate::texture::unstage(resource);
            SPENT.fetch_add(bytes, Ordering::Relaxed);
            since_asked += bytes;
            if room && since_asked >= 32 << 20 {
                since_asked = 0;
                room = crate::resource::room();
            }
            HEAP_TEXTURE_BYTES.fetch_add(bytes, Ordering::Relaxed);
            HEAP_TEXTURES.fetch_add(1, Ordering::Relaxed);
            uploads += 1;
            let live = host.texture_slot_done(index, write, again);
            // Whole and uploaded: looked at again (`texture_slots_rechecked`).
            let recheck = memory.map(|(at, len)| {
                (
                    (at, len),
                    crate::texture::fingerprint_for(at, len, live),
                    live,
                )
            });
            if let Some((memory, print, live)) = recheck {
                host.texture_slot_recheck(index, frame, memory, print, live);
            }
            if again.is_none() {
                uploaded.insert(key, recheck);
            }
        }
        laps.lap(Laps::REST);
        // Where this pass's time went, for the frame line (`HEAP_TEXTURE_PARTS`).
        let [made, made_us] = crate::resource::made_here();
        let staged_now = crate::texture::staged_here();
        let parts = [
            looked,
            uploads,
            laps.us(Laps::LOOK),
            laps.us(Laps::MAKE),
            laps.us(Laps::STAGE),
            laps.us(Laps::REST),
            made - made_before,
            made_us - made_us_before,
            staged_now[0] - staged_before[0],
            staged_now[1] - staged_before[1],
            staged_now[2] - staged_before[2],
            staged_now[3] - staged_before[3],
            handed,
            waited,
        ];
        for (count, part) in HEAP_TEXTURE_PARTS.iter().zip(parts) {
            count.fetch_add(part, Ordering::Relaxed);
        }
        catching_up(began.elapsed().saturating_sub(plain));
    }

    /// For the frame listing, after a dropped dispatch's line: its group
    /// counts and the compute root arguments it was recorded with (root
    /// constants as hex dwords, root views with the title buffer they
    /// point into). A program that does not translate has no bindings to
    /// list; these say which memory it would have read and written (the
    /// ray-tracing structure builder passes its buffers this way).
    #[cfg(feature = "diagnostics")]
    fn list_dropped_roots(&self, groups: [u32; 3]) {
        crate::capture::op(0, || {
            use std::fmt::Write;
            let [x, y, z] = groups;
            let mut text = format!("  its roots ({x}x{y}x{z}):");
            for (slot, root) in self.compute.iter().enumerate() {
                match root {
                    Root::None => {}
                    Root::Table(a) => {
                        let _ = write!(text, " {slot}=table {a:#x}");
                    }
                    Root::Constants(v) => {
                        let _ = write!(text, " {slot}=constants {:x?}", &v[..v.len().min(48)]);
                    }
                    Root::View(a) => {
                        let _ = write!(text, " {slot}=view {a:#x}");
                        match crate::shadow::extent(*a) {
                            Some((start, len)) => {
                                let _ = write!(text, " in buffer {start:#x}+{len:#x}");
                            }
                            None => text.push_str(" in no buffer"),
                        }
                    }
                }
            }
            text
        });
    }

    /// The stand-in for the indirect light after a dispatch of `p`
    /// (`crate::ambient`). Whether it recorded anything.
    fn ambient(&self, list: &HostList, p: &HostPipeline, role: crate::ambient::Role) -> bool {
        use crate::ambient::{AIR, DEPTH, LIGHT, RADIANCE, Role, Views};
        let Some(t) = p.stages.first().map(|s| s.translated()) else {
            return false;
        };
        let Some(heap) = (self.heaps[0] != 0)
            .then(|| crate::descriptor::heap(self.heaps[0] as This).host())
            .flatten()
        else {
            return false;
        };
        let entry = |kind: ResourceType, (slot, offset): (u8, u32)| {
            t.used.iter().find(|u| {
                u.kind == kind
                    && u.root == RootType::Table
                    && (u.slot, u.table_offset_dwords) == (slot, offset)
            })?;
            let Some(Root::Table(table)) = self.roots_ref(true).get(usize::from(slot)) else {
                return None;
            };
            let resource = match (
                kind,
                crate::descriptor::view(table + u64::from(offset) * 4)?,
            ) {
                (ResourceType::Srv, View::Shader(r, _))
                | (ResourceType::Uav, View::Unordered(r, ..)) => r,
                _ => return None,
            };
            let start = match self.ambient_tables.iter().find(|m| m.0 == slot) {
                Some(m) => m.1,
                None => self.user_value(true, slot, RootType::Table, 0),
            };
            let host = crate::resource::host_resource(resource as This)?;
            // SAFETY: a live resource.
            let desc = unsafe { host.GetDesc() };
            Some((desc, heap.gpu_handle((start + offset * 4) / 32)))
        };
        let at = match role {
            Role::Resolve => LIGHT,
            Role::Radiance => RADIANCE,
            Role::Air => AIR,
        };
        let Some((of, image)) = entry(ResourceType::Uav, at) else {
            once!(
                ?role,
                "no stand-in for the indirect light: missing image UAV"
            );
            return false;
        };
        let depth = match role {
            Role::Resolve => {
                let Some((seen, depth)) = entry(ResourceType::Srv, DEPTH) else {
                    once!(
                        ?role,
                        "no stand-in for the indirect light: missing depth SRV"
                    );
                    return false;
                };
                if (seen.Width, seen.Height) != (of.Width, of.Height) {
                    once!(
                        ?role,
                        "no stand-in for the indirect light: depth dimensions differ"
                    );
                    return false;
                }
                Some(depth)
            }
            Role::Radiance | Role::Air => None,
        };
        // (R32_UINT holding R9G9B9E5; R11G11B10_FLOAT.)
        let format = if role == Role::Resolve { 42 } else { 26 };
        let volume = of.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE3D;
        if of.Format.0 != format || volume != (role == Role::Air) {
            once!(
                ?role,
                format = of.Format.0,
                "no stand-in for the indirect light: another image"
            );
            return false;
        }
        crate::ambient::after(
            list,
            role,
            &Views {
                image,
                depth,
                size: [of.Width as u32, of.Height],
                slices: u32::from(of.DepthOrArraySize),
            },
        )
    }

    /// Runs a dispatch on the host.
    pub(crate) fn dispatch(&mut self, list: &HostList, x: u32, y: u32, z: u32) {
        // It may write the arguments a window holds clamped.
        self.window = None;
        #[cfg(feature = "diagnostics")]
        if crate::pipeline::is_skipped(self.pipeline as This) {
            DROPPED.fetch_add(1, Ordering::Relaxed);
            crate::pipeline::note_drop(self.pipeline as This);
            return;
        }
        if crate::pipeline::skips_rt_skin(self.pipeline as This) {
            once!(
                "passes that only serve rays are left out (VICTORY_BVH_NO_HIT=1, builder off; VICTORY_RT_RESOLVE, VICTORY_ACTIVATE_RAYS, VICTORY_RT_TRACE =1 run them; VICTORY_SKIP_RT_SKIN=1 skips skinning)"
            );
            RT_SKIN_SKIPPED.fetch_add(1, Ordering::Relaxed);
            return;
        }
        // The ray-tracing structure builder's stages run all or none.
        if crate::pipeline::is_builder(self.pipeline as This) && !crate::pipeline::builder_runs() {
            DROPPED.fetch_add(1, Ordering::Relaxed);
            #[cfg(feature = "diagnostics")]
            {
                crate::pipeline::note_why(
                    "a stage of the ray-tracing structure builder, which is off".to_owned(),
                );
                self.list_dropped_roots([x, y, z]);
            }
            return;
        }
        let Some(p) = crate::pipeline::host_pipeline(self.pipeline as This) else {
            once!(
                pipeline = format_args!("{:#x}", self.pipeline),
                "a dispatch without a host pipeline dropped"
            );
            DROPPED.fetch_add(1, Ordering::Relaxed);
            #[cfg(feature = "diagnostics")]
            {
                crate::pipeline::note_drop(self.pipeline as This);
                self.list_dropped_roots([x, y, z]);
            }
            return;
        };
        if !p.compute {
            once!("a dispatch with a graphics pipeline");
            return;
        }
        if !self.bind(list, p, true) {
            DROPPED.fetch_add(1, Ordering::Relaxed);
            // A stage of the structure builder that cannot be bound is a
            // dropped stage: the others stop too.
            if crate::pipeline::is_builder(self.pipeline as This) {
                crate::pipeline::builder_off("one of its dispatches could not be bound");
            }
            return;
        }
        #[cfg(feature = "diagnostics")]
        list.note_op(|| self.op_text(format!("dispatch {x}x{y}x{z}"), true));
        note_defrag(p, Some(u64::from(x) * u64::from(y) * u64::from(z)));
        match group_base(p) {
            Some((param, dword)) if x > crate::indirect::PART => {
                // More than the host runs in one dimension: in parts.
                // SAFETY: an open list with everything set; the parameter
                // and dword are the pipeline's.
                unsafe {
                    let mut base = 0;
                    while base < x {
                        list.list.SetComputeRoot32BitConstant(param, base, dword);
                        list.list
                            .Dispatch((x - base).min(crate::indirect::PART), y, z);
                        base += crate::indirect::PART;
                    }
                    list.list.SetComputeRoot32BitConstant(param, 0, dword);
                }
            }
            // SAFETY: an open list with everything set.
            _ => unsafe { list.list.Dispatch(x, y, z) },
        }
        #[cfg(feature = "diagnostics")]
        crate::capture::dump_after(list, self.pipeline as This, &[]);
        DISPATCHED.fetch_add(1, Ordering::Relaxed);
        if let Some(role) = p.stages.first().and_then(|s| s.shader.ambient)
            && crate::ambient::on()
            && self.ambient(list, p, role)
        {
            self.host_pipeline = 0;
            self.host_roots[1] = 0;
        }
    }

    /// What a draw or dispatch runs, for the device-removed report
    /// (diagnostics): the pipeline, then per stage the user data it gets
    /// (with the view at that heap index, if one is), the host addresses
    /// of its root views and the views its tables name.
    #[cfg(feature = "diagnostics")]
    fn op_text(&self, kind: String, compute: bool) -> String {
        use std::fmt::Write;
        let Some(p) = crate::pipeline::host_pipeline(self.pipeline as This) else {
            return kind;
        };
        let mut text = format!("{kind} {}", p.name);
        // What it draws into (a dispatch writes through its views below).
        if !compute {
            let described = |t: usize| {
                crate::resource::host_made(t as This).map(|r| {
                    // SAFETY: a live resource.
                    let d = unsafe { r.GetDesc() };
                    format!("{t:#x} {}x{} f{}", d.Width, d.Height, d.Format.0)
                })
            };
            let targets: Vec<String> = self.targets.iter().filter_map(|&t| described(t)).collect();
            let _ = write!(
                text,
                " targets {targets:?} depth {:?} state [{}]",
                described(self.depth),
                p.state
            );
            // The title's scissor and viewport at this draw (the first of
            // each): the title draws what it sees through a
            // portal (a window, from inside a room) under the portal's
            // rectangle, and the listing did not say which draws those are.
            if let Some(r) = self.scissors.first() {
                let _ = write!(
                    text,
                    " scissor [{} {} {} {}]",
                    r.left, r.top, r.right, r.bottom
                );
            }
            if let Some(v) = self.viewports.first() {
                let _ = write!(
                    text,
                    " viewport [{} {} {} {} depth {}..{}]",
                    v.TopLeftX, v.TopLeftY, v.Width, v.Height, v.MinDepth, v.MaxDepth
                );
            }
        }
        let views = (self.heaps[0] != 0)
            .then(|| crate::descriptor::heap(self.heaps[0] as This).host())
            .flatten();
        for stage in &p.stages {
            let t = stage.translated();
            let _ = write!(text, " | {}:", stage.shader.hash);
            for u in &t.host.user_data {
                let v = match u.root {
                    RootType::Other(kind) => self.host_word(compute, kind, u.slot, u.dword, None),
                    _ => self.user_value(compute, u.slot, u.root, u.dword),
                };
                let _ = write!(text, " u{}={v:#x}", u.index);
                let index = if u.root == RootType::Table { v / 32 } else { v };
                if let Some(slot) = views.and_then(|h| h.describe_slot(index)) {
                    let _ = write!(text, " [heap {index}: {slot}]");
                }
            }
            for &(slot, _) in stage
                .root_cbvs
                .iter()
                .chain(&stage.root_srvs)
                .chain(&stage.root_uavs)
            {
                if let Some(Root::View(address)) = self.roots_ref(compute).get(usize::from(slot)) {
                    let host = crate::shadow::find(*address, 1)
                        .map(|s| (s.gpu(*address), s.left(*address)));
                    let _ = write!(text, " root{slot}={address:#x} host {host:#x?}");
                }
            }
        }
        let _ = write!(text, " tables {:?}", self.reads(compute));
        if !compute && !list_slots().is_empty() {
            text.push_str(&self.slots_text());
        }
        text
    }

    /// For the frame listing, after a draw whose shader `VICTORY_LIST_SLOTS`
    /// names: the heap slots it will pick textures from by index, and what
    /// the title and the host hold there (one `~slot` line each), with the
    /// words they were read from (`~raw` lines, to decode offline what is
    /// not decoded here).
    ///
    /// Where the slots are, for the title's character shaders (the
    /// characters' main G-buffer pixel shader, read from its native code):
    /// in the small buffers and constants of the draw's own table (every
    /// word of those that names a texture view gets a line); in the
    /// material record, at
    /// the byte offset the draw's constants (table 1, dword 56) hold at
    /// 0x1c, of the buffer at table 0, dword 160; and in the record at the
    /// byte offset the second root constant holds of the buffer at table 0,
    /// dword 688. A word that is a small number by chance and names a
    /// texture gets a line too.
    #[cfg(feature = "diagnostics")]
    fn slots_text(&self) -> String {
        use std::fmt::Write;
        const MATERIALS: u32 = 160;
        const GLOBAL: u32 = 688;
        const DRAW_CONSTANTS: u32 = 56;
        let wanted = list_slots();
        let Some(p) = crate::pipeline::host_pipeline(self.pipeline as This) else {
            return String::new();
        };
        let Some(stage) = p
            .stages
            .iter()
            .find(|s| wanted.iter().any(|w| s.shader.hash.starts_with(w.as_str())))
        else {
            return String::new();
        };
        let Some(heap) = self
            .heaps
            .iter()
            .filter(|&&h| h != 0)
            .map(|&h| crate::descriptor::heap(h as This))
            .find(|h| !h.samplers())
        else {
            return String::new();
        };
        let t = stage.translated();
        let mut text = String::new();
        let entry = |slot: u8, dwords: u32| -> Option<View> {
            let Some(Root::Table(table)) = self.roots_ref(false).get(usize::from(slot)) else {
                return None;
            };
            crate::descriptor::view(table + u64::from(dwords) * 4)
        };
        let mut material = None;
        for u in t
            .used
            .iter()
            .filter(|u| u.root == RootType::Table && u.kind != ResourceType::Sampler)
        {
            let label = format!("s{}+{}", u.slot, u.table_offset_dwords);
            let Some(view) = entry(u.slot, u.table_offset_dwords) else {
                let _ = write!(text, "\n    ~table {label} no view");
                continue;
            };
            let range = match &view {
                View::Constant(address, size) => Some((*address, u64::from(*size))),
                other => crate::descriptor::host::buffer_range(other),
            };
            match range {
                // A small buffer or constants: its words.
                Some((address, len)) if len <= 2048 => {
                    Self::slot_words(&mut text, heap, &label, address, len as usize);
                    if u.slot == 1 && u.table_offset_dwords == DRAW_CONSTANTS && len >= 0x20 {
                        let mut word = [0u8; 4];
                        if crate::host::read_into((address + 0x1c) as *const u8, &mut word) {
                            material = Some(u32::from_le_bytes(word));
                        }
                    }
                }
                Some(_) => {}
                // A texture read through the table: the title slot it sits in.
                None => {
                    let Some(Root::Table(table)) = self.roots_ref(false).get(usize::from(u.slot))
                    else {
                        continue;
                    };
                    match heap.index(table + u64::from(u.table_offset_dwords) * 4) {
                        Some(index) => text.push_str(&Self::slot_line(
                            heap,
                            &format!("table:{label}"),
                            0,
                            index,
                        )),
                        None => {
                            let _ = write!(
                                text,
                                "\n    ~table {label} title=[{}]",
                                view_state(Some(&view))
                            );
                        }
                    }
                }
            }
        }
        let uses = |slot: u8, dwords: u32| {
            t.used.iter().any(|u| {
                u.root == RootType::Table && u.slot == slot && u.table_offset_dwords == dwords
            })
        };
        if let Some(at) = material
            && uses(0, MATERIALS)
            && let Some((base, len)) = entry(0, MATERIALS)
                .as_ref()
                .and_then(crate::descriptor::host::buffer_range)
        {
            let at = u64::from(at);
            let _ = write!(
                text,
                "\n    ~material record at {at:#x} of the buffer at {base:#x} bytes {len}"
            );
            if at + 64 <= len {
                Self::slot_words(&mut text, heap, "M", base + at, 64);
            }
            if at + 1936 + 512 <= len {
                Self::slot_words(&mut text, heap, "M+1936", base + at + 1936, 512);
            }
        }
        if uses(0, GLOBAL)
            && let Some(slot) = t
                .used
                .iter()
                .find(|u| u.root == RootType::Constants)
                .map(|u| u.slot)
            && let Some(Root::Constants(words)) = self.roots_ref(false).get(usize::from(slot))
            && let Some(&at) = words.get(1)
            && let Some((base, len)) = entry(0, GLOBAL)
                .as_ref()
                .and_then(crate::descriptor::host::buffer_range)
        {
            let at = u64::from(at);
            let _ = write!(
                text,
                "\n    ~root constants {words:08x?}: record at {at:#x} of the buffer at {base:#x} bytes {len}"
            );
            if at + 64 <= len {
                Self::slot_words(&mut text, heap, "G", base + at, 64);
            }
        }
        text
    }

    /// Up to 512 bytes of title memory at `address` as hex words (a `~raw`
    /// line), and a `~slot` line for every word of them that names a
    /// texture view in `heap`.
    #[cfg(feature = "diagnostics")]
    fn slot_words(
        text: &mut String,
        heap: &crate::descriptor::Heap,
        label: &str,
        address: u64,
        len: usize,
    ) {
        use std::fmt::Write;
        let mut bytes = vec![0u8; len.min(512) & !3];
        if bytes.is_empty() || !crate::host::read_into(address as *const u8, &mut bytes) {
            let _ = write!(
                text,
                "\n    ~raw {label} at {address:#x} bytes {len}: unreadable"
            );
            return;
        }
        let words: Vec<u32> = bytes
            .chunks_exact(4)
            .map(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]]))
            .collect();
        let _ = write!(text, "\n    ~raw {label} at {address:#x} bytes {len}:");
        for w in &words {
            let _ = write!(text, " {w:08x}");
        }
        for (i, &index) in words.iter().enumerate() {
            let names = index != 0
                && heap
                    .handle(index)
                    .and_then(crate::descriptor::view)
                    .as_ref()
                    .is_some_and(crate::descriptor::host::is_texture_view);
            if names {
                text.push_str(&Self::slot_line(heap, label, i * 4, index));
            }
        }
    }

    /// One `~slot` line: where the slot number was read (`from`, byte
    /// `at`), what the title's heap holds in slot `index` with the state
    /// of that texture ([`view_state`]), what the host slot was made from,
    /// and what the queue of textures picked by index knows of it.
    #[cfg(feature = "diagnostics")]
    fn slot_line(heap: &crate::descriptor::Heap, from: &str, at: usize, index: u32) -> String {
        let title = heap.handle(index).and_then(crate::descriptor::view);
        let host: String = heap
            .host()
            .and_then(|h| h.describe_slot(index))
            .unwrap_or_else(|| "none".into())
            .chars()
            .take(90)
            .collect();
        let queue = match heap.host() {
            Some(h) => h.texture_slot_state(index, crate::frame::current()),
            None => "no host heap".into(),
        };
        format!(
            "\n    ~slot from={from}+{at:#x} slot={index} title=[{}] host=[{host}] queue=[{queue}]",
            view_state(title.as_ref())
        )
    }

    /// A mesh draw under `VICTORY_MESH_TEST=scissor` rasterises one pixel:
    /// the scissors before it (`on`), the title's after it.
    fn mesh_scissor(&self, list: &HostList, on: bool) {
        if mesh_test() != 2 {
            return;
        }
        if on {
            let one = RECT {
                left: 0,
                top: 0,
                right: 1,
                bottom: 1,
            };
            crate::draw::scissors(list, &vec![one; self.scissors.len().max(1)]);
        } else if !self.scissors.is_empty() {
            crate::draw::scissors(list, &self.scissors);
        }
    }

    /// Runs a mesh dispatch of `x` groups on the host (the title's mesh
    /// programs are one-dimensional).
    pub(crate) fn dispatch_mesh(&mut self, list: &HostList, x: u32) {
        ATTEMPTS.fetch_add(1, Ordering::Relaxed);
        self.window = None;
        #[cfg(feature = "diagnostics")]
        self.trace("mesh", x, 0);
        let ran = mesh_test() != 1
            && mesh_kept(self.pipeline)
            && self.prepare_for(list, false, true)
            && match list.list.cast::<ID3D12GraphicsCommandList6>() {
                Ok(l6) => {
                    #[cfg(feature = "diagnostics")]
                    list.note_op(|| self.op_text(format!("mesh {x}"), false));
                    self.mesh_scissor(list, true);
                    // SAFETY: an open list with everything set.
                    unsafe { l6.DispatchMesh(x, 1, 1) };
                    self.mesh_scissor(list, false);
                    #[cfg(feature = "diagnostics")]
                    self.dump_after(list);
                    DRAWN.fetch_add(1, Ordering::Relaxed);
                    true
                }
                Err(_) => false,
            };
        if !ran {
            DROPPED.fetch_add(1, Ordering::Relaxed);
            dropped(self.pipeline);
        }
    }

    /// The walls trace (`crate::host::walls_trace`): one log line per
    /// frame with the draws of its main pass (four or more colour targets)
    /// by scissor rectangle, and how often each of the title's culling
    /// programs was dispatched. The title clips what it draws
    /// through a window or door to that opening's rectangle while it holds
    /// the camera to be in a room (run 182, frame 1529: 1,386 draws under
    /// `531 0 1789 1337`); in a launch without the room's walls the same
    /// shot has no such rectangle (run 187, frame 1414). The line shows
    /// the frame in which that changes.
    fn walls_census(&self, p: &HostPipeline, compute: bool) {
        // The title's culling programs by hash, each with what kind of pass
        // it is (described here, not named: names are the title's).
        const CULL: [(&str, &str); 9] = [
            ("55679a0547649d0b", "tree instance culling"),
            (
                "89d08e561897d4e5",
                "culling of trees drawn as distant cards",
            ),
            ("9209777e8face9dd", "grass instance culling"),
            ("17af15c4015cb4b6", "prop instance culling"),
            ("fdd56d97a806bcca", "prop batching without culling"),
            ("42f717f1b4d18e94", "plain instance culling"),
            ("1c4b3c0547e3e6fa", "far low-detail culling"),
            ("9146291b38ea9ce1", "meshlet culling argument setup"),
            ("42683eebec081520", "depth pyramid for CPU occlusion tests"),
        ];
        type Census = (u64, std::collections::BTreeMap<[i32; 4], u32>, [u32; 9]);
        static CENSUS: std::sync::Mutex<Census> =
            std::sync::Mutex::new((u64::MAX, std::collections::BTreeMap::new(), [0; 9]));
        let frame = crate::frame::current();
        let mut census = CENSUS.lock().unwrap_or_else(|e| e.into_inner());
        if census.0 != frame {
            let draws: u32 = census.1.values().sum();
            if draws > 0 {
                let mut rects: Vec<([i32; 4], u32)> =
                    census.1.iter().map(|(r, n)| (*r, *n)).collect();
                rects.sort_unstable_by_key(|r| std::cmp::Reverse(r.1));
                let scissors: Vec<String> = rects
                    .iter()
                    .take(5)
                    .map(|(r, n)| format!("{} {} {} {} x{n}", r[0], r[1], r[2], r[3]))
                    .collect();
                let counts = census.2;
                let culling: Vec<String> = CULL
                    .iter()
                    .zip(counts)
                    .filter(|(_, n)| *n > 0)
                    .map(|(c, n)| format!("{} ({}): {n}", c.1, c.0))
                    .collect();
                tracing::info!(
                    frame = census.0,
                    main_pass_draws = draws,
                    rectangles = rects.len(),
                    scissors = ?scissors,
                    culling = ?culling,
                    "walls trace: the frame's main pass by scissor rectangle, and its culling dispatches"
                );
            }
            census.0 = frame;
            census.1.clear();
            census.2 = [0; 9];
        }
        if compute {
            for (i, c) in CULL.iter().enumerate() {
                if p.stages.iter().any(|s| s.shader.hash.as_str() == c.0) {
                    census.2[i] += 1;
                }
            }
        } else if self.targets.iter().filter(|&&t| t != 0).count() >= 4 {
            let r = self
                .scissors
                .first()
                .map_or([0; 4], |r| [r.left, r.top, r.right, r.bottom]);
            *census.1.entry(r).or_insert(0) += 1;
        }
    }

    fn roots_ref(&self, compute: bool) -> &Vec<Root> {
        if compute {
            &self.compute
        } else {
            &self.graphics
        }
    }

    fn table_entry(&self, list: &HostList, u: &UsedResource, compute: bool) {
        let Some(Root::Table(table)) = self.roots_ref(compute).get(usize::from(u.slot)) else {
            return;
        };
        let handle = table + u64::from(u.table_offset_dwords) * 4;
        let Some(heap) = self.heap_of(handle) else {
            return;
        };
        let (Some(host), Some(index)) = (heap.host(), heap.index(handle)) else {
            return;
        };
        if let Some(view) = host.mirror(index, handle, true) {
            readable(list, u.kind, &view);
            // Up to date by this draw: not the texture pass's to make.
            if crate::descriptor::host::is_texture_view(&view) {
                host.texture_slot_tabled(index, true);
            }
        }
    }

    /// The descriptors a shader reads through tables. Tables holding a
    /// constant buffer are made afresh for the draw in dynamic slots, with
    /// the constant data copied to upload memory when the list is
    /// submitted: the host wants constant buffers 256-byte aligned, the
    /// console only 16. Returns those tables' new byte offsets by root
    /// parameter; the other tables' slots are made from the title's.
    ///
    /// Within a list, the same table holding the same descriptors is made
    /// once: its constant data is copied at submission either way, and the
    /// big tables the world binds would otherwise take hundreds of slots per
    /// draw, wrapping the dynamic slots onto lists not yet submitted.
    ///
    /// `half`: the table descriptors the shader moves sixteen bits a
    /// channel as unsigned integers (`HostShader::half_views`). A table
    /// with a 16-bit float or signed buffer view at one of them is made
    /// afresh as well, with that view in the unsigned format of the same
    /// layout (`descriptor::host::half_view`): the title's own slot keeps
    /// the title's format for the shaders that read it as floats.
    pub(crate) fn tables(
        &mut self,
        list: &HostList,
        plan: &TablePlan,
        compute: bool,
        made: &mut Vec<(u8, u32)>,
    ) {
        made.clear();
        let mut fresh = root_slots::RootSlots::default();
        for group in &plan.groups {
            let slot = group.slot;
            let table = match self.roots_ref(compute).get(usize::from(slot)) {
                Some(&Root::Table(table)) => Some(table),
                _ => None,
            };
            let mut needs_snapshot = group.has_cbv;
            if let Some(table) = table {
                for &at in &group.half {
                    let Some(view) = crate::descriptor::view(table + u64::from(at) * 4) else {
                        continue;
                    };
                    if crate::descriptor::host::half_view(&view).is_some() {
                        needs_snapshot = true;
                    } else if let Some(format) = crate::descriptor::host::half_inexact(&view) {
                        once!(
                            format,
                            "a shader moves a buffer's 16-bit channels as integers through a view whose values the console converts: they are read and written unconverted"
                        );
                    }
                }
                // Tables reaching past a host heap smaller than the title's (the
                // host's limit, 1,000,000 views on NVIDIA) are made afresh too: the
                // title's slots there have no host slot of their own.
                if !needs_snapshot
                    && self.heap_of(table).is_some_and(|heap| {
                        heap.host()
                            .is_some_and(|host| group.needs_snapshot(heap, host.count(), table))
                    })
                {
                    needs_snapshot = true;
                }
            }
            if needs_snapshot {
                fresh.insert(slot);
            }
        }
        for &(g, e) in &plan.direct {
            let group = &plan.groups[g];
            if !fresh.contains(group.slot) {
                self.table_entry(list, &group.entries[e], compute);
            }
        }
        let mut selected = std::mem::take(&mut self.scratch_selected);
        let mut textures = std::mem::take(&mut self.scratch_textures);
        let mut cbv_epoch = None;
        for group in &plan.groups {
            let slot = group.slot;
            if !fresh.contains(slot) {
                continue;
            }
            let Some(&Root::Table(table)) = self.roots_ref(compute).get(usize::from(slot)) else {
                continue;
            };
            let Some(heap) = self.heap_of(table) else {
                continue;
            };
            if heap.samplers() || (table - heap.start) % 32 != 0 {
                continue;
            }
            let Some(host) = heap.host() else {
                continue;
            };
            let entries = &group.entries;
            selected.clear();
            for index in &group.indexed {
                let constant = self.user_value(
                    compute,
                    index.constant_slot,
                    RootType::Constants,
                    index.constant_dword,
                );
                // The shader adds i32 byte offsets and shifts by 5.
                selected.push(index.byte_offset.wrapping_add(constant << index.shift) / 32);
            }
            // The descriptors the shader moves halves through are made too,
            // and the table reaches them: they may lie past the range starts
            // the entries give (a range of several views names only its
            // first; review on #927).
            selected.extend(group.half.iter().map(|&at| at / 8));
            if selected.len() > 1 {
                selected.sort_unstable();
                selected.dedup();
            }
            let n = group
                .max_dword
                .map(|d| d / 8)
                .into_iter()
                .chain(selected.last().copied())
                .max()
                .unwrap_or(0)
                + 1;
            let last = table.checked_add(u64::from(n - 1) * 32);
            if n > crate::descriptor::host::DYNAMIC
                || last.and_then(|handle| heap.index(handle)).is_none()
            {
                once!(slot, n, "fresh descriptor table exceeds the available span");
                continue;
            }
            // The table and what its entries hold (record words).
            let key = {
                const K: u64 = 0x517c_c1b7_2722_0a95;
                let mut hash = u64::from(n).wrapping_mul(K);
                for k in 0..n {
                    let at = table + u64::from(k) * 32;
                    // SAFETY: a descriptor slot in title memory.
                    let words = unsafe { (at as *const [u32; 3]).read_unaligned() };
                    let w01 = u64::from(words[0]) | (u64::from(words[1]) << 32);
                    hash = (hash.rotate_left(5) ^ w01).wrapping_mul(K);
                    hash = (hash.rotate_left(5) ^ u64::from(words[2])).wrapping_mul(K);
                    // (Another table than the same slots made for a shader
                    // that reads them in the title's formats.)
                    hash = (hash.rotate_left(5) ^ u64::from(group.has_half(k * 8)))
                        .wrapping_mul(K);
                }
                // (Neptune) By contents alone (`tables_by_content`): the title writes every
                // draw's table at a place of its own, and two thirds of a frame's tables
                // (9,600 of 15,200, 73,000 slots) were made afresh although others with the
                // same views in the same order had been made in the same list.
                (if tables_by_content() { 0 } else { table }, n, hash)
            };
            let cbv_epoch = *cbv_epoch.get_or_insert_with(|| list.cbv_epoch());
            let here = (table, n, key.2);
            if let Some(&(first, epoch)) = self.fresh_tables.get(&key)
                && epoch.is_none_or(|e| e == cbv_epoch)
            {
                if tables_by_content() && self.seen_tables.insert(here) {
                    // Another table's host slots; this one's own title slots are noted as
                    // its making would have noted them.
                    TABLES[4].fetch_add(1, Ordering::Relaxed);
                    if let Some(title) = heap.index(table).filter(|first| first + n <= host.count())
                    {
                        for k in 0..n {
                            let handle = table + u64::from(k) * 32;
                            if let Some(view) = crate::descriptor::view(handle)
                                && crate::descriptor::host::is_texture_view(&view)
                            {
                                let entry = group.entry(k);
                                let kind = entry.map(|u| u.kind).or_else(|| {
                                    selected
                                        .binary_search(&k)
                                        .ok()
                                        .and_then(|_| view_kind(&view))
                                });
                                host.texture_slot_tabled(title + k, kind.is_some());
                                if kind.is_some() {
                                    host.mirror(title + k, handle, true);
                                }
                            }
                        }
                    }
                }
                // (Neptune) The same table again in the same host list, with no texture's
                // state changed since its views were made ready: nothing to do. A fifth
                // of the recording threads' working time went into doing it again for
                // every draw.
                let here = std::ptr::from_ref(list) as usize;
                let (tested, _) = TESTED_DEPTH.get();
                let list_changes = list.changes.load(Ordering::Relaxed);
                // Why not: no note of it, another host list, the list's count moved, or
                // the draw's tested depth target among its textures.
                let miss_stat = match self.ready_tables.get(&key) {
                    Some((at, changes, ready_textures))
                        if ready_tables_on()
                            && *at == here
                            && *changes == list_changes
                            && (tested == 0 || !ready_textures.contains(&tested)) =>
                    {
                        TABLES[0].fetch_add(1, Ordering::Relaxed);
                        made.push((slot, first * 32));
                        continue;
                    }
                    None => 5,
                    Some((at, _, _)) if *at != here => 6,
                    Some((_, changes, _)) if *changes != list_changes => 7,
                    Some(_) => 8,
                };
                TABLES[1].fetch_add(1, Ordering::Relaxed);
                TABLES[miss_stat].fetch_add(1, Ordering::Relaxed);
                // Whether the table is one to remember: views that are only read, of
                // textures without another texture's memory behind them.
                let mut plain = true;
                textures.clear();
                let mut note = |kind: ResourceType, view: &View| {
                    // (A sampler needs nothing made ready; only what shaders write is not
                    // remembered.)
                    if kind == ResourceType::Uav {
                        plain = false;
                    }
                    let placed = crate::descriptor::host::placed_texture(view);
                    if let View::Shader(resource, _) = placed.as_ref().unwrap_or(view) {
                        // (A texture with a twin is plain too: a read that finds nothing to
                        // bring over does nothing, and anything else moves the list's count.)
                        if crate::resource::alias_of(*resource as This).is_some() {
                            plain = false;
                        }
                        textures.push(*resource);
                    }
                };
                for u in entries.iter().filter(|u| u.kind != ResourceType::Cbv)
                {
                    let handle = table + u64::from(u.table_offset_dwords) * 4;
                    if let Some(view) = crate::descriptor::view(handle) {
                        note(u.kind, &view);
                        readable(list, u.kind, &view);
                    }
                }
                for &k in &selected {
                    if group.entry(k).is_some() {
                        continue;
                    }
                    let handle = table + u64::from(k) * 32;
                    if let Some(view) = crate::descriptor::view(handle)
                        && let Some(kind) = view_kind(&view)
                    {
                        note(kind, &view);
                        readable(list, kind, &view);
                    }
                }
                if plain && (tested == 0 || !textures.contains(&tested)) {
                    let changes = list.changes.load(Ordering::Relaxed);
                    match self.ready_tables.entry(key) {
                        std::collections::hash_map::Entry::Occupied(mut e) => {
                            let (at, c, stored) = e.get_mut();
                            *at = here;
                            *c = changes;
                            stored.clear();
                            stored.extend_from_slice(&textures);
                        }
                        std::collections::hash_map::Entry::Vacant(e) => {
                            let stored = if textures.is_empty() {
                                Vec::new()
                            } else {
                                textures.clone()
                            };
                            e.insert((here, changes, stored));
                        }
                    }
                } else {
                    self.ready_tables.remove(&key);
                }
                made.push((slot, first * 32));
                continue;
            }
            let Some(first) = host.dynamic(n) else {
                // No room: the draw must not run with the title's own
                // offsets (slots never made for it); `bind` drops it.
                made.push((slot, u32::MAX));
                continue;
            };
            let mut scratch_epoch = None;
            self.seen_tables.insert(here);
            TABLES[2].fetch_add(1, Ordering::Relaxed);
            TABLES[3].fetch_add(u64::from(n), Ordering::Relaxed);
            self.dynamic
                .push((std::ptr::from_ref(heap) as usize, host.id(), first, n));
            // The title's own slots of this table: what the draw reads of
            // them it brings up to date here, in the dynamic slots.
            let title_first = heap.index(table).filter(|first| first + n <= host.count());
            let mut plain = ready_tables_on() && !group.has_uav;
            textures.clear();
            // The whole range: a shader may index past the entries its
            // used-resource list names (descriptor arrays).
            for k in 0..n {
                let index = first + k;
                let entry = group.entry(k);
                match crate::descriptor::view(table + u64::from(k) * 32) {
                    Some(View::Constant(address, size)) => {
                        let in_place = crate::port_cbv::in_place_on();
                        let copies = crate::port_cbv::copies_on();
                        if in_place || copies {
                            let shadow = crate::shadow::find(address, u64::from(size));
                            let choice = crate::port_cbv::table_use(
                                in_place,
                                copies,
                                shadow.is_some(),
                                shadow.as_ref().is_some_and(|s| s.in_place()),
                            );
                            match (choice, shadow) {
                                (crate::port_cbv::TableUse::InPlace, Some(shadow)) => {
                                    if host.constant_raw_at(
                                        index,
                                        &shadow.buffer,
                                        shadow.gpu(address),
                                        size,
                                    ) {
                                        crate::port_cbv::in_place(size);
                                        continue;
                                    }
                                    crate::port_cbv::copied(false);
                                }
                                (crate::port_cbv::TableUse::HostCopy, Some(shadow)) => {
                                    if host.constant_raw_at(
                                        index,
                                        &shadow.buffer,
                                        shadow.gpu(address),
                                        size,
                                    ) {
                                        shadow.refresh(list, address, u64::from(size));
                                        crate::port_cbv::in_copy(size);
                                        continue;
                                    }
                                    crate::port_cbv::copied(false);
                                }
                                _ => crate::port_cbv::copied(true),
                            }
                        }
                        if crate::port_cbv::on() {
                            scratch_epoch = Some(cbv_epoch);
                            let Some((buffer, gpu, to)) = list.scratch_held(u64::from(size)) else {
                                host.make_null_buffer(index);
                                continue;
                            };
                            list.defer_constants(to, address as *const u8, size as usize);
                            if !host.constant_raw_at(index, &buffer, gpu, size) {
                                host.make_null_buffer(index);
                            }
                            continue;
                        }
                        scratch_epoch = Some(cbv_epoch);
                        let Some(gpu) = list.constant_scratch(address, size) else {
                            host.make_null(index);
                            continue;
                        };
                        host.constant_at(index, gpu, size);
                    }
                    Some(view) => {
                        match group
                            .has_half(k * 8)
                            .then(|| crate::descriptor::host::half_view(&view))
                            .flatten()
                        {
                            Some(shown) => host.make_at(index, &shown),
                            None => host.make_at(index, &view),
                        }
                        let kind = entry.map(|u| u.kind).or_else(|| {
                            selected
                                .binary_search(&k)
                                .ok()
                                .and_then(|_| view_kind(&view))
                        });
                        if let Some(kind) = kind {
                            if plain && kind != ResourceType::Cbv {
                                if kind == ResourceType::Uav {
                                    plain = false;
                                } else {
                                    let placed = crate::descriptor::host::placed_texture(&view);
                                    if let View::Shader(resource, _) =
                                        placed.as_ref().unwrap_or(&view)
                                    {
                                        if crate::resource::alias_of(*resource as This).is_some() {
                                            plain = false;
                                        }
                                        textures.push(*resource);
                                    }
                                }
                            }
                            readable(list, kind, &view);
                        } else {
                            // Another shader sharing this table may read this slot, so do not
                            // mark the table ready for all future shaders without readable().
                            plain = false;
                        }
                        // The title's slot is a table's: out of the queue
                        // of textures picked by index if the draw read it
                        // (its texture is up to date now), and made in
                        // place as the texture pass would have made it,
                        // for a shader that picks it by index all the same.
                        if let Some(title) = title_first
                            && crate::descriptor::host::is_texture_view(&view)
                        {
                            host.texture_slot_tabled(title + k, kind.is_some());
                            if kind.is_some() {
                                host.mirror(title + k, table + u64::from(k) * 32, true);
                            }
                        }
                    }
                    None => {
                        host.make_null(index);
                        if entry.is_some() {
                            once!(slot, "a table entry without a view");
                        }
                    }
                }
            }
            self.fresh_tables.insert(key, (first, scratch_epoch));
            if plain {
                let here = std::ptr::from_ref(list) as usize;
                let (tested, _) = TESTED_DEPTH.get();
                if tested == 0 || !textures.contains(&tested) {
                    let stored = if textures.is_empty() {
                        Vec::new()
                    } else {
                        textures.clone()
                    };
                    self.ready_tables
                        .insert(key, (here, list.changes.load(Ordering::Relaxed), stored));
                } else {
                    self.ready_tables.remove(&key);
                }
            } else {
                self.ready_tables.remove(&key);
            }
            made.push((slot, first * 32));
        }
        self.scratch_selected = selected;
        self.scratch_textures = textures;
    }

    /// The first draws, at debug level: what they bind and read
    /// (diagnostics builds).
    #[cfg(feature = "diagnostics")]
    fn trace(&self, kind: &str, count: u32, first: u32) {
        static TRACED: AtomicU64 = AtomicU64::new(0);
        if !tracing::enabled!(tracing::Level::DEBUG) {
            return;
        }
        let listed = crate::capture::listed(crate::queue::presented() + 1);
        let logged = listed || TRACED.fetch_add(1, Ordering::Relaxed) < 64;
        let targets: Vec<String> = self
            .targets
            .iter()
            .filter_map(|&t| crate::resource::host_made(t as This))
            .map(|t| {
                // SAFETY: a live resource.
                let d = unsafe { t.GetDesc() };
                format!("{}x{} f{}", d.Width, d.Height, d.Format.0)
            })
            .collect();
        let v = self.vertex[0];
        let line = format!(
            "{kind} {count} from {first} topology {} pipeline {:#x} targets {targets:?} vb {:#x}+{} stride {} reads {:?}",
            self.topology,
            self.pipeline,
            v.address,
            v.size,
            v.stride,
            self.reads(false)
        );
        if logged {
            tracing::debug!("draw {line}");
        }
        let mut frame = FRAME.lock().unwrap_or_else(|e| e.into_inner());
        if frame.len() < 256 {
            frame.push(line);
        }
    }

    /// What a draw's shaders read, one line per used resource (diagnostics
    /// builds).
    #[cfg(feature = "diagnostics")]
    fn reads(&self, compute: bool) -> Vec<String> {
        let Some(p) = crate::pipeline::host_pipeline(self.pipeline as This) else {
            return vec!["no host pipeline".into()];
        };
        let mut out = Vec::new();
        for stage in &p.stages {
            let t = stage.translated();
            for u in &t.used {
                let what = match (u.root, self.roots_ref(compute).get(usize::from(u.slot))) {
                    (RootType::Table, Some(Root::Table(table))) => {
                        let handle = table + u64::from(u.table_offset_dwords) * 4;
                        describe(crate::descriptor::view(handle).as_ref())
                    }
                    (root, _) => format!("{root:?}"),
                };
                out.push(format!(
                    "{} {:?} s{}+{}: {what}",
                    stage.shader.hash, u.kind, u.slot, u.table_offset_dwords
                ));
            }
        }
        out
    }

    pub(crate) fn draw(
        &mut self,
        list: &HostList,
        vertices: u32,
        instances: u32,
        first: u32,
        first_instance: u32,
    ) {
        ATTEMPTS.fetch_add(1, Ordering::Relaxed);
        self.window = None;
        #[cfg(feature = "diagnostics")]
        self.trace("draw", vertices, first);
        let ready = self.prepare(list, false);
        // A pipeline with the rectangle geometry stage draws the title's
        // vertices as they are; any other gets its rectangles expanded.
        let rects = self.topology == RECTANGLES && !self.completes_rects();
        let ran = ready && (!rects || self.rectangles(list, vertices, first));
        if ran {
            // Rectangles were expanded into their own vertex buffers.
            let (count, from) = if rects {
                (vertices / 3 * 6, 0)
            } else {
                (vertices, first)
            };
            #[cfg(feature = "diagnostics")]
            list.note_op(|| self.op_text(format!("draw {count}x{instances}"), false));
            // SAFETY: an open list with everything set.
            unsafe {
                list.list
                    .DrawInstanced(count, instances, from, first_instance)
            };
            #[cfg(feature = "diagnostics")]
            self.dump_after(list);
            DRAWN.fetch_add(1, Ordering::Relaxed);
        }
        if !ran {
            DROPPED.fetch_add(1, Ordering::Relaxed);
            dropped(self.pipeline);
        }
    }

    /// Whether the bound pipeline completes rectangles itself, in the
    /// geometry stage rectangle pipelines get (`HostPipeline::rects`).
    fn completes_rects(&self) -> bool {
        crate::pipeline::host_pipeline(self.pipeline as This).is_some_and(|p| p.rects)
    }

    /// Binds vertex buffers holding the rectangles of vertices `first..first
    /// + count` as triangles, made when the list is submitted.
    fn rectangles(&mut self, list: &HostList, count: u32, first: u32) -> bool {
        let Some(p) = crate::pipeline::host_pipeline(self.pipeline as This) else {
            return false;
        };
        let rects = u64::from(count / 3);
        for slot in 0..32 {
            let elements: Vec<(u32, u32)> = p
                .inputs
                .iter()
                .filter(|e| e.slot == slot && !e.per_instance)
                .map(|e| (e.offset, e.format))
                .collect();
            if elements.is_empty() {
                continue;
            }
            let v = self.vertex[slot as usize];
            let stride = u64::from(v.stride);
            let from = v.address + u64::from(first) * stride;
            if v.address == 0
                || stride == 0
                || u64::from(first + count) * stride > u64::from(v.size)
                || crate::shadow::find(from, rects * 3 * stride).is_none()
            {
                once!(slot, "rectangle vertices outside their buffer");
                return false;
            }
            let Some((gpu, to)) = list.scratch(rects * 6 * stride) else {
                return false;
            };
            let (from, to, stride) = (from as usize, to as usize, stride as usize);
            let n = rects as usize;
            list.defer_host(move || {
                // The vertices are read in place, at submission: no change
                // of mapping under the read (see
                // `crate::watch::title_memory_reading`).
                let _mapped = crate::watch::title_memory_reading_range(from, n * 3 * stride);
                // SAFETY: the vertices lie in a title buffer (checked when
                // recorded) and the scratch memory is the list's.
                let (src, dst) = unsafe {
                    (
                        std::slice::from_raw_parts(from as *const u8, n * 3 * stride),
                        std::slice::from_raw_parts_mut(to as *mut u8, n * 6 * stride),
                    )
                };
                crate::primitive::expand_rects(src, dst, stride, &elements);
            });
            let view = D3D12_VERTEX_BUFFER_VIEW {
                BufferLocation: gpu,
                SizeInBytes: (rects * 6 * stride as u64) as u32,
                StrideInBytes: v.stride,
            };
            // SAFETY: an open list; the view names live scratch memory.
            unsafe { list.list.IASetVertexBuffers(slot, Some(&[view])) };
            self.host_vertex[slot as usize] = D3D12_VERTEX_BUFFER_VIEW::default();
            self.host_vertex_bound &= !(1u32 << slot);
            self.host_vertex_dirty = true;
        }
        true
    }

    pub(crate) fn draw_indexed(
        &mut self,
        list: &HostList,
        indices: u32,
        instances: u32,
        first: u32,
        base: i32,
        first_instance: u32,
    ) {
        ATTEMPTS.fetch_add(1, Ordering::Relaxed);
        self.window = None;
        #[cfg(feature = "diagnostics")]
        self.trace("indexed", indices, first);
        // Indexed rectangles are drawn by the rectangle geometry stage
        // alone (nothing expands them on the CPU).
        let lost = self.topology == RECTANGLES && !self.completes_rects();
        if lost {
            once!("indexed rectangles not drawn");
        }
        let ran = !lost && self.prepare(list, true);
        if ran {
            #[cfg(feature = "diagnostics")]
            list.note_op(|| self.op_text(format!("indexed {indices}x{instances}"), false));
            // SAFETY: an open list with everything set.
            unsafe {
                list.list
                    .DrawIndexedInstanced(indices, instances, first, base, first_instance)
            };
            #[cfg(feature = "diagnostics")]
            self.dump_after(list);
            DRAWN.fetch_add(1, Ordering::Relaxed);
        }
        if !ran {
            DROPPED.fetch_add(1, Ordering::Relaxed);
            dropped(self.pipeline);
        }
    }
}

impl State {
    /// Indirect execution of a one-argument signature of `kind` (PC
    /// numbering: 0 draw, 1 indexed draw, 2 dispatch, 10 mesh dispatch) with
    /// records `stride` bytes apart at title address `arguments`, and the
    /// record count at `count` if the title gives one: the GPU reads them
    /// from the buffer's host copy, where earlier dispatches may have
    /// written them. False if it cannot run on the host.
    pub(crate) fn execute_indirect(
        &mut self,
        list: &HostList,
        (kind, stride, max): (u32, u32, u32),
        arguments: u64,
        count: Option<u64>,
    ) -> bool {
        let ran = self.run_indirect(list, (kind, stride, max), arguments, count);
        // A listed frame: what the records of one that did not run asked
        // for (a dropped execution of many meshes is a whole class of
        // geometry missing; an empty one is no loss).
        #[cfg(feature = "diagnostics")]
        if !ran && crate::capture::recording_listed() {
            self.dropped_records(list, (kind, stride, max), arguments, count);
        }
        ran
    }

    /// Straight after a draw: its render targets and depth, and what it
    /// writes through unordered access, saved if its pipeline is named in
    /// `VICTORY_DUMP_AFTER` (`crate::capture::dump_after`).
    #[cfg(feature = "diagnostics")]
    fn dump_after(&self, list: &HostList) {
        let mut targets = self.targets.clone();
        targets.push(self.depth);
        crate::capture::dump_after(list, self.pipeline as This, &targets);
    }

    /// In a listed frame, for an indirect execution that did not run: its
    /// records and count as the title's copies hold them, logged once the
    /// list has run (`crate::capture::read_indirect`).
    #[cfg(feature = "diagnostics")]
    fn dropped_records(
        &self,
        list: &HostList,
        (kind, stride, max): (u32, u32, u32),
        arguments: u64,
        count: Option<u64>,
    ) {
        let len = u64::from(stride) * u64::from(max.max(1));
        let Some(args) = crate::shadow::find_any(arguments, len) else {
            return;
        };
        args.refresh(list, arguments, len);
        let counter = count.and_then(|at| crate::shadow::find_any(at, 4).map(|s| (s, at)));
        if let Some((s, at)) = &counter {
            s.refresh(list, *at, 4);
        }
        list.keep(&args.buffer);
        let records = crate::capture::Records::Title(&args.buffer, args.offset(arguments));
        let count = counter.as_ref().map(|(s, at)| (&s.buffer, s.offset(*at)));
        let name = format!(
            "DROPPED {}",
            crate::pipeline::shaders_of(self.pipeline as This)
        );
        crate::capture::read_indirect(list, records, count, (kind, stride, max), name);
    }

    fn run_indirect(
        &mut self,
        list: &HostList,
        (kind, stride, max): (u32, u32, u32),
        arguments: u64,
        count: Option<u64>,
    ) -> bool {
        // A dispatch's kind (2) is compute.
        if kind != 2 {
            ATTEMPTS.fetch_add(1, Ordering::Relaxed);
        }
        // Skip before the argument clamp can mutate the indirect buffer.
        #[cfg(feature = "diagnostics")]
        if crate::pipeline::is_skipped(self.pipeline as This) {
            crate::pipeline::note_drop(self.pipeline as This);
            return false;
        }
        if crate::pipeline::skips_rt_skin(self.pipeline as This) {
            once!(
                "passes that only serve rays are left out (VICTORY_BVH_NO_HIT=1, builder off; VICTORY_RT_RESOLVE, VICTORY_ACTIVATE_RAYS =1 run them; VICTORY_SKIP_RT_SKIN=1 skips skinning)"
            );
            RT_SKIN_SKIPPED.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        if crate::pipeline::is_builder(self.pipeline as This) && !crate::pipeline::builder_runs() {
            return false;
        }
        const DRAW: u32 = 0;
        const INDEXED: u32 = 1;
        const DISPATCH: u32 = 2;
        const MESH: u32 = 10;
        #[cfg(feature = "diagnostics")]
        let why = |what: &str| crate::pipeline::note_why(format!("indirect kind {kind}: {what}"));
        #[cfg(not(feature = "diagnostics"))]
        let why = |_: &str| {};
        let Some(p) = crate::pipeline::host_pipeline(self.pipeline as This) else {
            #[cfg(feature = "diagnostics")]
            crate::pipeline::note_drop(self.pipeline as This);
            return false;
        };
        // Indirect rectangles (the title's tiled lighting, 256
        // indirect draws a frame, makes their corners from the vertex
        // number) are completed by the pipeline's geometry stage: the
        // arguments stay the title's, three vertices a rectangle. They
        // cannot be expanded on the CPU, which does not know their count.
        // shortcut: in a pipeline without that stage, a rectangle with no
        // per-vertex inputs is drawn as its first triangle (three vertices,
        // half of it), and one with vertex buffers is not drawn.
        let made_rects = !p.rects
            && kind == DRAW
            && self.topology == RECTANGLES
            && !p.inputs.iter().any(|e| !e.per_instance);
        if made_rects {
            once!("indirect rectangles without vertex inputs drawn as their first triangle");
        }
        let matches_kind = match kind {
            DRAW | INDEXED => {
                (self.topology != RECTANGLES || made_rects || p.rects) && !p.compute && !p.mesh
            }
            MESH => !p.compute && p.mesh,
            DISPATCH => p.compute,
            _ => false,
        };
        if !matches_kind {
            why(&format!(
                "pipeline mismatch (compute {}, mesh {}, topology {})",
                p.compute, p.mesh, self.topology
            ));
            return false;
        }
        let len = u64::from(stride) * u64::from(max.max(1));
        let Some(args) = crate::shadow::find_any(arguments, len) else {
            why("arguments outside title buffers");
            return false;
        };
        if crate::indirect::after_writer()
            && matches!(kind, DRAW | INDEXED | MESH)
            && max == 1
            && count.is_none()
        {
            crate::indirect::learn(arguments as usize, kind);
        }
        let counter = match count {
            Some(at) => match crate::shadow::find_any(at, 4) {
                Some(s) => Some((s, at)),
                None => {
                    why("count outside title buffers");
                    return false;
                }
            },
            None => None,
        };
        let Some(signature) = matches!(kind, DRAW | INDEXED | DISPATCH | MESH)
            .then(|| host_signature(kind, stride))
            .flatten()
        else {
            why("no host signature");
            return false;
        };
        if kind == MESH && (mesh_test() == 1 || !mesh_kept(self.pipeline)) {
            why("mesh draws are switched off (VICTORY_MESH_TEST=skip)");
            return false;
        }
        if raw_indirect() && kind == MESH && max == 1 && count.is_none() {
            self.window = None;
            args.refresh(list, arguments, len);
            if !self.prepare_for(list, false, true) {
                why("prepare failed");
                return false;
            }
            #[cfg(feature = "diagnostics")]
            list.note_op(|| {
                self.op_text(
                    format!("indirect kind {kind} max {max} at {arguments:#x}"),
                    false,
                )
            });
            // SAFETY: an open list with everything set; the list keeps the host copy
            // alive, and the signature is cached.
            unsafe {
                list.list.ExecuteIndirect(
                    signature,
                    1,
                    &args.buffer,
                    args.offset(arguments),
                    None::<&ID3D12Resource>,
                    0,
                )
            };
            DRAWN.fetch_add(1, Ordering::Relaxed);
            return true;
        }
        // (Neptune) `VICTORY_INDIRECT_RAW=2`, an experiment: every single record without a
        // count buffer (draws, indexed draws, mesh draws) is drawn from the title's own
        // arguments in their host copy, with no pass of ours to bound them. A frame of the
        // first room bounded 3,200 records in place, each a dispatch of ours between two
        // waits for every earlier shader write, in the middle of the title's draws; the card
        // was busy all frame at a third of its power. Garbage arguments are then the card's
        // to deal with.
        // (`VICTORY_INDIRECT_RAW=2`: also dispatches and records with a count, as the title
        // gave them; not a dispatch that is split into parts for its first group's number.)
        let split = kind == DISPATCH && max == 1 && count.is_none() && group_base(p).is_some();
        // (Neptune) `VICTORY_CLAMP_AFTER_WRITER=1`: a single record that shaders of the list write
        // runs raw once a pass of ours has made it safe in place (`crate::indirect::sanitized`);
        // when none can, it is clamped as before.
        let shape = crate::indirect::after_writer()
            && matches!(kind, DRAW | INDEXED | MESH)
            && max == 1
            && count.is_none();
        let written = shape && list.gpu_written(arguments as usize, len as usize);
        let after_writer = written && {
            args.refresh(list, arguments, len);
            let (safe, ran) = crate::indirect::sanitized(list, &args, arguments, kind);
            if ran {
                // The pass bound a compute pipeline and root signature of its own.
                self.host_pipeline = 0;
                self.host_roots[1] = 0;
            }
            safe
        };
        if shape {
            crate::indirect::SAN[if after_writer { 3 } else { 4 }].fetch_add(1, Ordering::Relaxed);
            if !written {
                crate::indirect::WHY[1].fetch_add(1, Ordering::Relaxed);
            }
            static SHOWN: AtomicU64 = AtomicU64::new(0);
            if !after_writer && SHOWN.fetch_add(1, Ordering::Relaxed) < 5 {
                let ranges = list.with_san(|s| s.ranges());
                tracing::info!(
                    args = format_args!("{arguments:#x}"),
                    kind,
                    written,
                    ranges = ?ranges.iter().map(|(a, b)| format!("{a:#x}-{b:#x}")).collect::<Vec<_>>(),
                    "VICTORY_CLAMP_AFTER_WRITER: a reader fell back to the clamp"
                );
            }
        }
        if after_writer || (raw_all() && matches!(kind, DRAW | INDEXED | MESH | DISPATCH) && !split)
        {
            self.window = None;
            args.refresh(list, arguments, len);
            if let Some((s, at)) = &counter {
                s.refresh(list, *at, 4);
            }
            let ran = match kind {
                DRAW | INDEXED => self.prepare(list, kind == INDEXED),
                MESH => self.prepare_for(list, false, true),
                _ => self.bind(list, p, true),
            };
            if !ran {
                why("prepare failed");
                return false;
            }
            #[cfg(feature = "diagnostics")]
            list.note_op(|| {
                self.op_text(
                    format!("indirect kind {kind} max {max} at {arguments:#x}"),
                    kind == DISPATCH,
                )
            });
            if kind == MESH {
                self.mesh_scissor(list, true);
            }
            // SAFETY: an open list with everything set; the list keeps the host copies
            // alive, and the signature is cached.
            unsafe {
                list.list.ExecuteIndirect(
                    signature,
                    max.max(1),
                    &args.buffer,
                    args.offset(arguments),
                    counter.as_ref().map(|(s, _)| &s.buffer),
                    counter.as_ref().map_or(0, |(s, at)| s.offset(*at)),
                )
            };
            if kind == MESH {
                self.mesh_scissor(list, false);
            }
            CLAMPS[2].fetch_add(1, Ordering::Relaxed);
            if kind == DISPATCH {
                DISPATCHED.fetch_add(1, Ordering::Relaxed);
            } else {
                DRAWN.fetch_add(1, Ordering::Relaxed);
            }
            return true;
        }
        // (Neptune) A single record without a count buffer, which nothing
        // the list has recorded so far lets its shaders write: clamped in
        // the list's prelude, and drawn from its slot there.
        if clamp_ahead()
            && !windows()
            && kind != DISPATCH
            && max == 1
            && count.is_none()
            && crate::indirect::ahead_layout(kind, stride, args.offset(arguments))
            && !list.gpu_written(arguments as usize, len as usize)
        {
            args.refresh(list, arguments, len);
            if let Some((slots, at)) =
                list.clamp_ahead(&args.buffer, args.offset(arguments), kind, stride)
            {
                self.window = None;
                let ran = match kind {
                    DRAW | INDEXED => self.prepare(list, kind == INDEXED),
                    MESH => self.prepare_for(list, false, true),
                    _ => false,
                };
                if !ran {
                    why("prepare failed");
                    return false;
                }
                #[cfg(feature = "diagnostics")]
                list.note_op(|| {
                    self.op_text(
                        format!("indirect kind {kind} max {max} at {arguments:#x}"),
                        false,
                    )
                });
                if kind == MESH {
                    self.mesh_scissor(list, true);
                }
                // SAFETY: an open list with everything set; the list keeps its
                // ahead buffers, and the signature is cached.
                unsafe {
                    list.list
                        .ExecuteIndirect(signature, 1, &slots, at, None::<&ID3D12Resource>, 0)
                };
                if kind == MESH {
                    self.mesh_scissor(list, false);
                }
                CLAMPS[0].fetch_add(1, Ordering::Relaxed);
                DRAWN.fetch_add(1, Ordering::Relaxed);
                return true;
            }
        }
        CLAMPS[1].fetch_add(1, Ordering::Relaxed);
        // A window of records clamped in one pass (`windows`): for single
        // records without a count buffer, drawn by shaders that store to no
        // buffer. Anything else ends the window: it uses the scratch buffer
        // or may write the arguments.
        const WINDOW_USE: usize = usize::MAX - 0x77;
        let offset = args.offset(arguments);
        // A dispatch that may have more groups than the host runs in one
        // dimension, by a program that takes the number of its first group.
        let parts = if kind == DISPATCH && max == 1 && count.is_none() {
            group_base(p).zip(host_signature(DISPATCH, crate::indirect::PART_BYTES as u32))
        } else {
            None
        };
        let windowed = windows()
            && kind != DISPATCH
            && max == 1
            && count.is_none()
            && !p.stages.iter().any(|stage| stage.translated().stores);
        // Another host list (a new one after a cut, or this one after its
        // reset) has another scratch buffer, or this one's with old bytes.
        if !windowed || list.first_use(WINDOW_USE) {
            self.window = None;
        }
        let same = |w: &Window| w.copy == args.id() && w.kind == kind && w.stride == stride;
        let hit = self
            .window
            .as_ref()
            .filter(|w| same(w) && offset >= w.base)
            .filter(|w| {
                (offset - w.base) % u64::from(w.step) == 0
                    && (offset - w.base) / u64::from(w.step) < u64::from(w.records)
            })
            .map(|w| (w.scratch.clone(), offset - w.base));
        let (bounded, at) = match hit {
            Some(hit) => hit,
            None => {
                // A record a short step after the last window's last one
                // (the same step as that window's, if it had one): the
                // title walks the buffer, so the ones that step apart
                // after it are clamped with it.
                let (step, records) = match &self.window {
                    Some(w) if windowed && same(w) => {
                        let last = w.base + u64::from(w.records - 1) * u64::from(w.step);
                        let step = offset.saturating_sub(last);
                        let walks = step >= u64::from(stride)
                            && step <= 256
                            && step % 4 == 0
                            && (w.records == 1 || step == u64::from(w.step));
                        if walks {
                            let fit = (args.left(arguments) / step).min(WINDOW_MOST) as u32;
                            (
                                step as u32,
                                (if w.records == 1 { 16 } else { w.records * 2 })
                                    .min(fit)
                                    .max(1),
                            )
                        } else {
                            (stride, max)
                        }
                    }
                    _ => (stride, max),
                };
                args.refresh(
                    list,
                    arguments,
                    len.max(u64::from(step) * u64::from(records)),
                );
                if let Some((s, at)) = &counter {
                    s.refresh(list, *at, 4);
                }
                // Bounded first: the clamp binds a compute pipeline of its
                // own and copies clamped active records into a scratch
                // buffer at offset 0.
                let count_arg = counter.as_ref().map(|(s, at)| (&s.buffer, s.offset(*at)));
                // More than the record asked for: a window clamped ahead,
                // whose unfilled slots must not spend the list's budget.
                let bounded = if parts.is_some() {
                    crate::indirect::bound_split(list, (&args.buffer, offset))
                } else if windowed && records > 1 {
                    crate::indirect::bound_ahead(
                        list,
                        (&args.buffer, offset),
                        (kind, step, records),
                    )
                } else {
                    crate::indirect::bound(
                        list,
                        (&args.buffer, offset),
                        count_arg,
                        (kind, step, records),
                    )
                };
                let Some(bounded) = bounded else {
                    self.window = None;
                    why("not bounded (no clamp, or arguments not 4-byte aligned)");
                    return false;
                };
                self.host_pipeline = 0;
                self.host_roots[1] = 0;
                self.window = windowed.then(|| Window {
                    copy: args.id(),
                    kind,
                    stride,
                    step,
                    base: offset,
                    records,
                    scratch: bounded.clone(),
                });
                (bounded, 0)
            }
        };
        let ran = match kind {
            DRAW | INDEXED => self.prepare(list, kind == INDEXED),
            MESH => self.prepare_for(list, false, true),
            DISPATCH => {
                let bound = self.bind(list, p, true);
                if bound {
                    note_defrag(p, None);
                }
                bound
            }
            _ => false,
        };
        if !ran {
            why("prepare failed");
            return false;
        }
        #[cfg(feature = "diagnostics")]
        list.note_op(|| {
            self.op_text(
                format!("indirect kind {kind} max {max} at {arguments:#x}"),
                kind == DISPATCH,
            )
        });
        if kind == MESH {
            self.mesh_scissor(list, true);
        }
        // SAFETY: an open list with everything set; the scratch buffer and
        // host copies are kept alive by the list, and the signature is cached.
        unsafe {
            if let Some(((param, dword), signature)) = &parts {
                for part in 0..crate::indirect::SPLIT {
                    list.list.SetComputeRoot32BitConstant(
                        *param,
                        part * crate::indirect::PART,
                        *dword,
                    );
                    let at = u64::from(part) * crate::indirect::PART_BYTES;
                    list.list
                        .ExecuteIndirect(*signature, 1, &bounded, at, None, 0);
                }
                list.list.SetComputeRoot32BitConstant(*param, 0, *dword);
            } else {
                list.list.ExecuteIndirect(
                    signature,
                    max,
                    &bounded,
                    at,
                    counter.as_ref().map(|(s, _)| &s.buffer),
                    counter.as_ref().map_or(0, |(s, at)| s.offset(*at)),
                )
            }
        };
        if kind == MESH {
            self.mesh_scissor(list, false);
        }
        #[cfg(feature = "diagnostics")]
        if kind == DISPATCH {
            crate::capture::dump_after(list, self.pipeline as This, &[]);
        } else {
            self.dump_after(list);
        }
        // A listed frame: what the clamped records asked for.
        #[cfg(feature = "diagnostics")]
        if crate::capture::recording_listed() {
            let count = counter.as_ref().map(|(s, at)| (&s.buffer, s.offset(*at)));
            let name = crate::pipeline::shaders_of(self.pipeline as This);
            crate::capture::read_indirect(
                list,
                crate::capture::Records::Clamped(&bounded, at),
                count,
                (kind, stride, max),
                name,
            );
        }
        if kind == DISPATCH {
            DISPATCHED.fetch_add(1, Ordering::Relaxed);
        } else {
            DRAWN.fetch_add(1, Ordering::Relaxed);
        }
        true
    }
}

/// Where a compute pipeline's program takes the number of its first thread
/// group (`victory_shader::emit::USER_GROUP_BASE`): the root parameter of
/// its user data and the dword in it.
fn group_base(p: &crate::pipeline::host::HostPipeline) -> Option<(u32, u32)> {
    let stage = p.stages.first().filter(|_| p.compute)?;
    let (param, _) = stage.user_data?;
    let base = RootType::Other(victory_shader::emit::USER_GROUP_BASE);
    let word = stage
        .translated()
        .host
        .user_data
        .iter()
        .find(|u| u.root == base)?;
    Some((param, word.index))
}

#[cfg(all(test, windows))]
mod indirect_signature_tests;

/// The host command signature for one argument of `kind` with records
/// `stride` bytes apart (no root arguments, so no root signature), made
/// once and kept for good. Each thread remembers the last one it asked
/// for: the title's threads ask for the same few, and the lock and the
/// object's reference count were shared by all of them.
/// Looks up `key` in `cache`, calling `create` outside the mutex on a miss
/// so slow driver `CreateCommandSignature` calls never block other recording
/// threads on the cache lock.
fn cached_command_signature<K: Copy + Eq, V: 'static>(
    cache: &std::sync::Mutex<Vec<(K, &'static V)>>,
    key: K,
    create: impl FnOnce() -> Option<V>,
) -> Option<&'static V> {
    {
        let made = cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(&(_, s)) = made.iter().find(|(k, _)| *k == key) {
            return Some(s);
        }
    }
    let created = create()?;
    let mut made = cache.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(&(_, s)) = made.iter().find(|(k, _)| *k == key) {
        drop(created);
        return Some(s);
    }
    let leaked: &'static V = Box::leak(Box::new(created));
    made.push((key, leaked));
    Some(leaked)
}

fn host_signature(kind: u32, stride: u32) -> Option<&'static ID3D12CommandSignature> {
    type Made = ((u32, u32), &'static ID3D12CommandSignature);
    thread_local! {
        static LAST: std::cell::Cell<Option<Made>> = const { std::cell::Cell::new(None) };
    }
    if let Some((_, s)) = LAST.get().filter(|(k, _)| *k == (kind, stride)) {
        return Some(s);
    }
    static MADE: std::sync::Mutex<Vec<Made>> = std::sync::Mutex::new(Vec::new());
    let s = cached_command_signature(&MADE, (kind, stride), || {
        let argument = D3D12_INDIRECT_ARGUMENT_DESC {
            Type: D3D12_INDIRECT_ARGUMENT_TYPE(kind as i32),
            ..Default::default()
        };
        let desc = D3D12_COMMAND_SIGNATURE_DESC {
            ByteStride: stride,
            NumArgumentDescs: 1,
            pArgumentDescs: &argument,
            NodeMask: 0,
        };
        let mut signature: Option<ID3D12CommandSignature> = None;
        // SAFETY: a description of one argument; no root arguments.
        let result = unsafe {
            crate::host::host()?.device.CreateCommandSignature(
                &desc,
                None::<&ID3D12RootSignature>,
                &mut signature,
            )
        };
        if let Err(e) = result {
            once!(error = %e, kind, stride, "host command signature refused");
            return None;
        }
        signature
    })?;
    LAST.set(Some(((kind, stride), s)));
    Some(s)
}

/// Gets what a shader reads or writes through a view ready: buffer data
/// copied, textures moved to the state they need.
fn view_kind(view: &View) -> Option<ResourceType> {
    match view {
        View::Shader(..) | View::Placed(false, ..) => Some(ResourceType::Srv),
        View::Unordered(..) | View::Placed(true, ..) => Some(ResourceType::Uav),
        _ => None,
    }
}

/// The host state a depth view needs of its texture: the read state if the
/// view writes no plane the format has (the title's read-only flags), else
/// the write state.
/// shortcut: whole textures have one state, so a view that writes one of
/// two planes counts as writing.
fn depth_state(texture: &ID3D12Resource, desc: Option<&D3D12_DEPTH_STENCIL_VIEW_DESC>) -> u32 {
    // D16_UNORM and D32_FLOAT: depth formats without a stencil plane.
    const ONE_PLANE: [i32; 2] = [55, 40];
    let Some(desc) = desc else {
        return DEPTH_WRITE;
    };
    let format = if desc.Format.0 == 0 {
        // SAFETY: a live texture; a view without a format has its own.
        unsafe { texture.GetDesc() }.Format.0
    } else {
        desc.Format.0
    };
    let mut needed = D3D12_DSV_FLAG_READ_ONLY_DEPTH.0;
    if !ONE_PLANE.contains(&format) {
        needed |= D3D12_DSV_FLAG_READ_ONLY_STENCIL.0;
    }
    if desc.Flags.0 & needed == needed {
        DEPTH_READ
    } else {
        DEPTH_WRITE
    }
}

/// (Neptune) Whether a table made afresh is remembered as ready within its host list
/// (`VICTORY_READY_TABLES=0`: its views are made ready again for every draw, as before).
fn ready_tables_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_READY_TABLES").is_none_or(|v| v != "0"))
}

fn readable(list: &HostList, kind: ResourceType, view: &View) {
    const UNORDERED_ACCESS: u32 = 0x8;
    let placed = crate::descriptor::host::placed_texture(view);
    let view = placed.as_ref().unwrap_or(view);
    // Buffer views read title memory: their range is copied at submission;
    // what shaders write through unordered access goes back to it after.
    if let Some((shadow, address, len)) = crate::descriptor::host::buffer_copy(view) {
        shadow.refresh(list, address, len);
        if kind == ResourceType::Uav {
            // A shader that cannot store to buffers leaves the range as
            // the title has it: nothing to find afterwards.
            if STORES.get() || !write_skip() {
                shadow.gpu_writes(list, address, len);
            } else {
                #[cfg(feature = "diagnostics")]
                {
                    crate::copystat::add(crate::copystat::C::GpuWritesLeftOut, 1);
                    crate::copystat::add(crate::copystat::C::GpuWritesLeftOutBytes, len);
                }
            }
        }
    }
    match (view, kind) {
        (View::Shader(resource, _), ResourceType::Srv) => {
            // A view the title placed, in another format, in the memory of
            // a texture the host writes: that texture's texels, copied on
            // the host (title memory does not have them).
            let aliased = crate::resource::alias_of(*resource as This)
                .is_some_and(|source| crate::texture::alias(list, *resource as This, source.get()));
            // One lookup of the host copy (and twin) for the upload, the
            // twin and the state: each lookup takes the texture's lock, and
            // the copy's flags are kept, not asked of the driver.
            let Some((copy, twin)) = crate::resource::host_side(*resource as This) else {
                return;
            };
            if !aliased && copy.title_written() {
                // Of a texture the title placed, the mips the view shows.
                let first = match (&placed, view) {
                    (Some(_), View::Shader(_, Some(desc))) => crate::resource::first_mip(desc),
                    _ => 0,
                };
                crate::texture::upload_from(list, *resource as This, &copy.texture, first);
            }
            // A depth texture shaders also write: what they wrote, first.
            if let Some(twin) = &twin {
                list.twin_use_held(&copy, twin, false, false);
            }
            // The depth target the draw only tests is read in both ways at
            // once: both read states, which its preparation asks for too.
            let (tested, _) = TESTED_DEPTH.get();
            let state = if tested != 0 && tested == *resource {
                TESTED_DEPTH.set((tested, true));
                DEPTH_READ | SHADER_RESOURCE
            } else {
                SHADER_RESOURCE
            };
            crate::list::ensure_held_state(list, &copy, state);
            #[cfg(feature = "diagnostics")]
            if let Some(t) = crate::resource::host_resource(*resource as This) {
                // SAFETY: a live resource.
                let d = unsafe { t.GetDesc() };
                if d.Flags.0 & 7 == 0 {
                    tracing::debug!(
                        resource = format_args!("{resource:#x}"),
                        ?d,
                        "a shader reads a texture the title fills"
                    );
                }
            }
        }
        (View::Unordered(resource, _, _), ResourceType::Uav) => {
            #[cfg(feature = "diagnostics")]
            crate::capture::note_target(*resource as This);
            #[cfg(feature = "diagnostics")]
            crate::capture::note_written(std::ptr::from_ref(list) as usize, *resource as This);
            // A texture of a kind the title's CPU reads (the ocean's height
            // map, `VICTORY_OCEAN_READBACK=1`): copied back when the list closes.
            list.texture_written(*resource as This);
            // Unordered access to a depth texture is its colour twin's.
            if !list.twin_use(*resource as This, true, true) {
                crate::list::ensure_state(list, *resource as This, UNORDERED_ACCESS);
            }
        }
        _ => {}
    }
}

/// The pixel shaders whose draws get their slots listed
/// ([`State::slots_text`]): `VICTORY_LIST_SLOTS`, hash prefixes separated
/// by commas. None by default.
#[cfg(feature = "diagnostics")]
fn list_slots() -> &'static [String] {
    static LIST: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    LIST.get_or_init(|| {
        std::env::var("VICTORY_LIST_SLOTS")
            .unwrap_or_default()
            .split(',')
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty())
            .collect()
    })
}

/// A fingerprint of the 4 KiB of title memory at `at` (FNV-1a, 64 bits; of
/// 4 KiB of zeros it is always the same value, which `slots.py` knows), or
/// "unreadable".
#[cfg(feature = "diagnostics")]
fn print_4k(at: u64) -> String {
    let mut bytes = [0u8; 4096];
    if at == 0 || !crate::host::read_into(at as *const u8, &mut bytes) {
        return "unreadable".into();
    }
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for b in bytes {
        hash = (hash ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// The `bytes` of title memory at `at`: how many are mapped, and the
/// fingerprints of the first and the last 4 KiB (a texture's smallest
/// levels lie first, its largest last).
#[cfg(feature = "diagnostics")]
fn memory_state(at: u64, bytes: u64) -> String {
    let mapped = crate::texture::mapped_bytes(at as *const u8, bytes as usize);
    let last = at + bytes.saturating_sub(4096);
    format!(
        "memory={at:#x} mapped={mapped}/{bytes} first4k={} last4k={}",
        print_4k(at),
        print_4k(last)
    )
}

/// Title texture `this` for the frame listing: its size, whether it has a
/// host copy (not made here), its staging buffer's state, whether the host
/// copied into it, and its memory ([`memory_state`]).
#[cfg(feature = "diagnostics")]
fn texture_state(this: This) -> String {
    let Some((at, desc)) = crate::resource::texture_memory(this) else {
        return "not a texture".into();
    };
    // SAFETY: 56 bytes in the description's layout.
    let words = unsafe {
        std::ptr::from_ref(&desc)
            .cast::<[u32; 14]>()
            .read_unaligned()
    };
    let bytes = crate::resource::placed_bytes(&words).unwrap_or(0);
    let staged = match crate::texture::staged_state(this) {
        Some((uploaded, dirty)) => format!("staged=yes uploaded={uploaded} dirty={dirty}"),
        None => "staged=no".into(),
    };
    format!(
        "{}x{} f{} levels={} host_copy={} {staged} host_written={} title_fills={} {}",
        u64::from(words[4]) | u64::from(words[5]) << 32,
        words[6],
        words[8],
        words[7] >> 16,
        crate::resource::host_made(this).is_some(),
        crate::texture::host_wrote(this as usize),
        crate::resource::title_fills(this),
        memory_state(at as u64, bytes)
    )
}

/// A view of the title's heap for the frame listing: a texture with its
/// state ([`texture_state`]; a placed one that has no resource yet with
/// its memory alone), anything else in a few words.
#[cfg(feature = "diagnostics")]
fn view_state(view: Option<&View>) -> String {
    match view {
        None => "no view".into(),
        Some(View::Placed(false, r, d)) if r[0] != 1 => {
            let address = u64::from(d[10]) | u64::from(d[11]) << 32;
            let state = match crate::resource::texture_at_made(address, r) {
                Some(this) => texture_state(this),
                None => format!(
                    "{}x{} f{} levels={} resource=none {}",
                    u64::from(r[4]) | u64::from(r[5]) << 32,
                    r[6],
                    r[8],
                    r[7] >> 16,
                    memory_state(address, crate::resource::placed_bytes(r).unwrap_or(0))
                ),
            };
            // (Last, so that readers of the older lines still find
            // theirs: the level the view starts at, and how many it shows.)
            format!(
                "placed texture view_format={} {state} view_levels={}+{}",
                d[0], d[4], d[5] as i32
            )
        }
        Some(View::Shader(r, d)) if crate::resource::texture_memory(*r as This).is_some() => {
            format!(
                "texture object {r:#x} view_format={} {} view_levels={}",
                d.as_ref().map_or(0, |d| d[0]),
                texture_state(*r as This),
                d.as_ref()
                    .map_or("all".to_owned(), |d| format!("{}+{}", d[4], d[5] as i32))
            )
        }
        Some(v) => format!("{v:?}").chars().take(80).collect(),
    }
}

/// A view in a few words (diagnostics builds).
#[cfg(feature = "diagnostics")]
fn describe(view: Option<&View>) -> String {
    let texture = |r: This| {
        crate::resource::host_made(r).map(|t| {
            // SAFETY: a live resource.
            let d = unsafe { t.GetDesc() };
            format!(
                "{}x{} f{} flags {:#x} of {r:?}",
                d.Width, d.Height, d.Format.0, d.Flags.0
            )
        })
    };
    match view {
        None => "no view".into(),
        Some(v @ View::Shader(r, _)) => texture(*r as This)
            .unwrap_or_else(|| format!("buffer {:?}", crate::descriptor::host::buffer_range(v))),
        Some(v @ View::Unordered(r, ..)) => match texture(*r as This) {
            Some(t) => format!("uav texture {t}"),
            None => format!(
                "uav resource {r:#x} buffer {:?}",
                crate::descriptor::host::buffer_range(v)
            ),
        },
        Some(View::Placed(u, r, d)) => format!(
            "placed uav={u} dim {} {}x{} f{} layout {:#x} at {:#x}",
            r[0],
            u64::from(r[4]) | u64::from(r[5]) << 32,
            r[6],
            r[8],
            r[11],
            u64::from(d[10]) | u64::from(d[11]) << 32
        ),
        // The first four values, as a colour rectangle's constants are.
        Some(View::Constant(address, size))
            if *size >= 16 && crate::texture::committed(*address as *const u8, 16) =>
        {
            // SAFETY: 16 committed bytes of title memory.
            let v = unsafe { (*address as *const [f32; 4]).read_unaligned() };
            format!("Constant({address:#x}, {size}) {v:?}")
        }
        Some(v) => format!("{v:?}").chars().take(60).collect(),
    }
}

/// The console's own primitive topologies.
const TRIANGLE_FAN: u32 = 6;
const RECTANGLES: u32 = 17;
const LINE_LOOP: u32 = 18;
const QUADS: u32 = 19;
const QUAD_STRIP: u32 = 20;

/// Fixed-function state, straight onto the host list.
pub(crate) fn viewports(list: &HostList, viewports: &[D3D12_VIEWPORT]) {
    // SAFETY: an open list.
    unsafe { list.list.RSSetViewports(viewports) };
}

pub(crate) fn scissors(list: &HostList, rects: &[RECT]) {
    // SAFETY: an open list.
    unsafe { list.list.RSSetScissorRects(rects) };
}

pub(crate) fn blend_factor(list: &HostList, factor: Option<&[f32; 4]>) {
    // SAFETY: an open list.
    unsafe { list.list.OMSetBlendFactor(factor) };
}

pub(crate) fn stencil_ref(list: &HostList, reference: u32) {
    #[cfg(feature = "diagnostics")]
    crate::capture::op(std::ptr::from_ref(list) as usize, || {
        format!("STENCIL REF {reference:#x}")
    });
    // SAFETY: an open list.
    unsafe { list.list.OMSetStencilRef(reference) };
}

pub(crate) fn depth_bounds(list: &HostList, min: f32, max: f32) {
    #[cfg(feature = "diagnostics")]
    crate::capture::op(std::ptr::from_ref(list) as usize, || {
        format!("DEPTH BOUNDS {min}..{max}")
    });
    if crate::host::host().is_some_and(|h| h.depth_bounds())
        && let Ok(l1) = list.list.cast::<ID3D12GraphicsCommandList1>()
    {
        // SAFETY: an open list on a device that supports depth bounds.
        unsafe { l1.OMSetDepthBounds(min, max) };
    }
}

/// The longest wait, in milliseconds, between two looks at a texture slot
/// whose memory is not there yet (`VICTORY_TEXTURE_LOOK_MS`; unset: about
/// four seconds counted in passes, as before). The looks back off 1, 2, 4,
/// ... frames up to it.
fn look_ms() -> Option<u64> {
    static MS: std::sync::OnceLock<Option<u64>> = std::sync::OnceLock::new();
    *MS.get_or_init(|| {
        let ms = std::env::var("VICTORY_TEXTURE_LOOK_MS")
            .ok()?
            .trim()
            .parse::<u64>()
            .ok()
            .filter(|&ms| ms > 0)?;
        tracing::info!(
            ms,
            "texture slots waiting for their memory are looked at at least this often (VICTORY_TEXTURE_LOOK_MS)"
        );
        Some(ms)
    })
}

/// The milliseconds of a frame lately (a mean over the last frames), from
/// the pass's frame number and clock.
fn frame_ms(frame: u64, now: u64) -> u64 {
    use std::sync::Mutex;
    static LAST: Mutex<(u64, u64, f64)> = Mutex::new((0, 0, 50.0));
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    let (was_frame, was_ms, mean) = *last;
    if frame > was_frame {
        if was_frame != 0 {
            let each = now.saturating_sub(was_ms) as f64 / (frame - was_frame) as f64;
            last.2 = mean * 0.9 + each.clamp(1.0, 2000.0) * 0.1;
        }
        last.0 = frame;
        last.1 = now;
    }
    (last.2 as u64).max(1)
}

#[cfg(test)]
mod loading_drain_tests {
    use super::{
        EarlyPassConfig, EarlyPassInput, EarlyPassKind, LOADING_DRAIN_MAX_DRAWS,
        compute_early_pass_budget, parse_mb_bytes,
    };

    fn default_early_cfg() -> EarlyPassConfig {
        EarlyPassConfig {
            early_ms: 40,
            bake_ms: Some(1500),
            gameplay_ms: 3,
            pass_bytes_override: None,
            loading_drain: true,
        }
    }

    #[test]
    fn loading_screen_boosts_pass_budget_when_due_and_low_draw() {
        let cfg = default_early_cfg();
        let input = EarlyPassInput {
            first_in_frame: true,
            is_bake: false,
            bake_ms_left: 1500,
            prev_frame_draws: 11,
            has_due_textures: true,
            world_gameplay: true,
            texture_budget_remaining: None,
        };
        assert_eq!(
            compute_early_pass_budget(cfg, input),
            Some((EarlyPassKind::LoadingDrain, (400, 256 << 20, false)))
        );

        // No boost when the due queue is empty.
        assert_eq!(
            compute_early_pass_budget(
                cfg,
                EarlyPassInput {
                    has_due_textures: false,
                    ..input
                }
            ),
            Some((EarlyPassKind::Normal, (40, 64 << 20, false)))
        );

        // No boost once 3D draw counts reach the threshold; steady gameplay
        // keeps the `gameplay_ms` cap.
        assert_eq!(
            compute_early_pass_budget(
                cfg,
                EarlyPassInput {
                    prev_frame_draws: LOADING_DRAIN_MAX_DRAWS,
                    ..input
                }
            ),
            Some((EarlyPassKind::Normal, (3, 64 << 20, false)))
        );

        // Second+ draw in the same frame gets no early or loading pass.
        assert_eq!(
            compute_early_pass_budget(
                cfg,
                EarlyPassInput {
                    first_in_frame: false,
                    ..input
                }
            ),
            None
        );
    }

    #[test]
    fn pass_budget_respects_env_overrides_and_tdr_and_texture_budget() {
        let loading_input = EarlyPassInput {
            first_in_frame: true,
            is_bake: false,
            bake_ms_left: 1500,
            prev_frame_draws: 5,
            has_due_textures: true,
            world_gameplay: false,
            texture_budget_remaining: None,
        };

        // VICTORY_LOADING_DRAIN=0 disables loading-screen boost.
        let disabled = EarlyPassConfig {
            loading_drain: false,
            ..default_early_cfg()
        };
        assert_eq!(
            compute_early_pass_budget(disabled, loading_input),
            Some((EarlyPassKind::Normal, (40, 64 << 20, false)))
        );

        // VICTORY_TEXTURE_PASS_MB overrides both loading and normal byte caps.
        let pass_mb = EarlyPassConfig {
            pass_bytes_override: parse_mb_bytes("512"),
            ..default_early_cfg()
        };
        assert_eq!(
            compute_early_pass_budget(pass_mb, loading_input),
            Some((EarlyPassKind::LoadingDrain, (400, 512 << 20, false)))
        );
        assert_eq!(
            compute_early_pass_budget(
                pass_mb,
                EarlyPassInput {
                    prev_frame_draws: 2000,
                    ..loading_input
                }
            ),
            Some((EarlyPassKind::Normal, (40, 512 << 20, false)))
        );

        // VICTORY_TEXTURE_BUDGET_MB remaining bytes cap the pass budget.
        assert_eq!(
            compute_early_pass_budget(
                pass_mb,
                EarlyPassInput {
                    texture_budget_remaining: Some(128 << 20),
                    ..loading_input
                }
            ),
            Some((EarlyPassKind::LoadingDrain, (400, 128 << 20, false)))
        );
        // Exhausted VICTORY_TEXTURE_BUDGET_MB skips pre-drain.
        assert_eq!(
            compute_early_pass_budget(
                default_early_cfg(),
                EarlyPassInput {
                    texture_budget_remaining: Some(0),
                    ..loading_input
                }
            ),
            None
        );

        // Time budget stays safely below the Windows 2-second TDR limit.
        let high_ms = EarlyPassConfig {
            early_ms: 5000,
            ..default_early_cfg()
        };
        assert_eq!(
            compute_early_pass_budget(high_ms, loading_input),
            Some((EarlyPassKind::LoadingDrain, (1500, 256 << 20, false)))
        );

        // Bake draws still take precedence over loading/normal passes.
        assert_eq!(
            compute_early_pass_budget(
                default_early_cfg(),
                EarlyPassInput {
                    is_bake: true,
                    bake_ms_left: 900,
                    ..loading_input
                }
            ),
            Some((EarlyPassKind::Bake, (900, 2 << 30, true)))
        );

        // Overflow-safe MB parsing.
        assert_eq!(parse_mb_bytes(" 256 "), Some(256 << 20));
        assert_eq!(parse_mb_bytes("0"), Some(0));
        assert_eq!(parse_mb_bytes("not-a-num"), None);
    }

    #[test]
    fn ready_drain_budget_expands_only_default_smooth_gameplay_cap() {
        use std::time::Duration;
        // Default 4 ms gameplay cap gets READY_DRAIN_MS (12 ms) for already-prepared textures.
        assert_eq!(
            super::compute_ready_drain_budget(Duration::from_millis(4), true),
            Duration::from_millis(super::READY_DRAIN_MS)
        );
        // Larger default budgets (e.g. 200 ms initial loading) are never reduced.
        assert_eq!(
            super::compute_ready_drain_budget(Duration::from_millis(200), true),
            Duration::from_millis(200)
        );
        // Zero budget or explicit override (`VICTORY_HEAP_TEXTURES_MS` / `PASS_BUDGET`) stays exact.
        assert_eq!(
            super::compute_ready_drain_budget(Duration::ZERO, true),
            Duration::ZERO
        );
        assert_eq!(
            super::compute_ready_drain_budget(Duration::from_millis(4), false),
            Duration::from_millis(4)
        );
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_function_state_comparators_are_bit_exact() {
        let vp = D3D12_VIEWPORT {
            TopLeftX: 0.0,
            TopLeftY: 0.0,
            Width: 1920.0,
            Height: 1080.0,
            MinDepth: 0.0,
            MaxDepth: 1.0,
        };
        let mut vp_neg_zero = vp;
        vp_neg_zero.TopLeftX = -0.0;
        assert!(viewports_eq(&[vp], &[vp]));
        assert!(!viewports_eq(&[vp], &[vp_neg_zero]));
        assert!(!viewports_eq(&[vp], &[]));

        let r1 = RECT { left: 0, top: 0, right: 1280, bottom: 720 };
        let r2 = RECT { left: 0, top: 0, right: 1920, bottom: 1080 };
        assert!(rects_eq(&[r1], &[r1]));
        assert!(!rects_eq(&[r1], &[r2]));

        assert!(opt_f32_array_eq(Some([0.0f32, 1.0, 2.0, 3.0]), Some([0.0, 1.0, 2.0, 3.0])));
        assert!(!opt_f32_array_eq(Some([0.0f32, 1.0, 2.0, 3.0]), Some([-0.0, 1.0, 2.0, 3.0])));
        assert!(opt_f32_array_eq::<4>(None, None));
        assert!(!opt_f32_array_eq(Some([0.0f32; 2]), None));
    }

    #[test]
    fn bound_targets_detects_descriptor_or_texture_changes() {
        let mut slot_a = [0u8; 112];
        let mut slot_b = [0u8; 112];
        let res_a = 0x1000usize as This;
        let desc1 = [28u32, 4, 0, 0, 0, 0];
        let desc2 = [28u32, 4, 1, 0, 0, 0];
        crate::descriptor::create_rtv(std::ptr::null(), res_a, &desc1, slot_a.as_mut_ptr() as u64);
        crate::descriptor::create_rtv(std::ptr::null(), res_a, &desc2, slot_b.as_mut_ptr() as u64);
        let snap1 = crate::descriptor::rtv_record(slot_a.as_ptr() as u64)
            .unwrap()
            .snapshot();
        let snap2 = crate::descriptor::rtv_record(slot_b.as_ptr() as u64)
            .unwrap()
            .snapshot();

        let mut a = BoundTargets {
            rtv_count: 1,
            rtvs: [None; 8],
            depth: None,
        };
        a.rtvs[0] = Some((snap1, 0xaaaa));
        let mut same = a;
        assert_eq!(a, same);

        // Changing view description behind the same resource misses.
        same.rtvs[0] = Some((snap2, 0xaaaa));
        assert_ne!(a, same);

        // Changing resolved host texture pointer misses.
        same.rtvs[0] = Some((snap1, 0xbbbb));
        assert_ne!(a, same);

        // Changing rtv_count misses.
        same.rtvs[0] = Some((snap1, 0xaaaa));
        same.rtv_count = 2;
        assert_ne!(a, same);
    }

    #[test]
    fn state_reset_clears_state_while_reusing_container_capacity() {
        let mut s = State::default();
        s.graphics.reserve(16);
        s.graphics.push(Root::Table(0x1234));
        s.fresh_tables.reserve(64);
        s.fresh_tables.insert((1, 2, 3), (4, None));
        s.targets.push(0x9999);
        s.depth = 0x8888;
        s.host_pipeline = 0x7777;
        s.host_topology = Some(4);
        s.host_viewports_set = true;
        s.targets_ready = Some((0x5555, 12, DEPTH_WRITE));
        s.tables_gone = true;

        let fresh_cap_before = s.fresh_tables.capacity();
        let gfx_cap_before = s.graphics.capacity();
        let pipe = 0x4321usize as This;
        s.reset(pipe);

        assert_eq!(s.pipeline, 0x4321);
        assert!(s.graphics.is_empty());
        assert!(s.fresh_tables.is_empty());
        assert!(s.targets.is_empty());
        assert_eq!(s.depth, 0);
        assert_eq!(s.host_pipeline, 0);
        assert_eq!(s.host_topology, None);
        assert!(!s.host_viewports_set);
        assert_eq!(s.targets_ready, None);
        assert!(!s.tables_gone);
        // Capacities are retained across resets to avoid per-frame reallocations.
        assert!(s.fresh_tables.capacity() >= fresh_cap_before);
        assert!(s.graphics.capacity() >= gfx_cap_before);
    }

    #[test]
    fn host_list_atomic_cbv_epoch_and_twin_fast_paths() {
        let Some(host) = crate::host::host() else {
            return;
        };
        let list = host.new_list().unwrap();
        assert_eq!(list.cbv_epoch(), 0);
        list.defer(|| {});
        assert_eq!(list.cbv_epoch(), 1);
        let mut dst = [0u8; 4];
        let src = [1u8; 4];
        list.defer_copy(dst.as_mut_ptr(), src.as_ptr(), 4);
        assert_eq!(list.cbv_epoch(), 2);
        // Null resource takes the lock-free early return.
        assert!(!list.twin_use(std::ptr::null(), false, true));
        // No twin writing: twins_written takes the lock-free fast path without bumping changes.
        let before = list.changes.load(Ordering::Relaxed);
        list.twins_written();
        assert_eq!(list.changes.load(Ordering::Relaxed), before);
        list.close();
        list.reset();
        assert_eq!(list.cbv_epoch(), 0);

#[cfg(test)]
mod bake_budget_tests {
    use super::bake_left;

    #[test]
    fn a_frames_bake_draws_share_one_deadline() {
        // The first draw of frame 7 at 1000 ms starts 500 ms for all of them.
        assert_eq!(bake_left(7, 500, 1000), 500);
        // Another thread's draw 300 ms later has what is left, not 500 less
        // what finished.
        assert_eq!(bake_left(7, 500, 1300), 200);
        assert_eq!(bake_left(7, 500, 1600), 0);
        // The next frame starts again.
        assert_eq!(bake_left(8, 500, 1700), 500);
    }
}
    }
}
