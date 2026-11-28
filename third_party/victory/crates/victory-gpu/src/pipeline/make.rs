//! Host pipelines are made by workers, not by the title thread that first
//! records a draw with them. Translating a shader and the host driver's
//! compile take from milliseconds to seconds each (structurisation of the
//! biggest compute programs ~0.4 s, some driver compiles 3.4 s), and
//! the title's first world frame wants over 1,200 new pipelines: made
//! on its recording threads, that frame took more than 30 s and the
//! title's GPU watchdog ended the run (decision 0007, "Pipelines made off
//! the title's threads").
//!
//! A first use queues the pipeline and waits for it within a budget per
//! frame, so the cheap ones (menus, one-off passes such as a colour-grading
//! bake) are there at once as before; past the budget a draw or dispatch
//! whose pipeline is not ready is dropped, as an untranslated one is.
//!
//! The default gameplay budget is 25 ms per title frame for graphics and
//! 200 ms for compute, a window the recording threads wait in together
//! ([`DEFAULT_WAIT_MS`], [`DEFAULT_WAIT_COMPUTE_MS`]). A pass the title
//! runs only once and whose pipeline takes longer would be lost while a
//! world loads (when the title dispatches the passes that unpack its
//! models once): a frame may wait longer for a pipeline while a run's
//! loading allowance lasts ([`frame_budget`]), and never for one the
//! driver has held for long ([`held_too_long`]).
//! Ported from the sibling fork `joshuu/victory-amd`'s 4436f8f1 (its
//! loading waits only).

use std::cmp::Reverse;
use std::collections::{BinaryHeap, VecDeque};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::Shader;
use super::host::{GraphicsState, HostPipeline};

mod compute_wait;
mod head_map;
mod screen_wait;

/// What a host pipeline is made from, and the host pipeline once made.
pub(crate) struct Pending {
    shaders: Vec<Arc<Shader>>,
    pub(crate) graphics: Option<Box<GraphicsState>>,
    queue_state: AtomicU8,
    made: OnceLock<Option<HostPipeline>>,
    /// Recording threads waiting for this particular pipeline.
    completion: (Mutex<()>, Condvar),
    /// What identifies it across runs (shader containers and state), and
    /// whether a draw or dispatch has requested it yet.
    key: String,
    used: AtomicBool,
    /// Why it is not made, if it is not ([`left_out`]).
    left_out: Option<&'static str>,
    /// When a worker began it ([`slow`]).
    begun: OnceLock<Instant>,
    /// A pass the picture depends on once per run (character bakes, the
    /// face network): made at creation and waited for, not dropped
    /// ([`scene_input`]).
    scene_input: bool,
    /// Whether a draw has waited for it past the frame budget already: it
    /// is waited for so once ([`wait_scene_input`]).
    scene_waited: AtomicBool,
    /// (bootfast) Queued at creation by `VICTORY_PIPELINE_EAGER`
    /// ([`super::eager`]); set under [`QUEUE`]'s lock.
    eager: AtomicBool,
    /// A pass over the screen ([`GraphicsState::screen_pass`],
    /// [`screen_wait`]).
    screen: bool,
    /// The head blend-shape map's ([`head_map`]).
    head_map: bool,
}

/// Why a pipeline with these shaders is not made, if it is not.
fn left_out(shaders: &[Arc<Shader>]) -> Option<&'static str> {
    if named_out(shaders) {
        Some("named in VICTORY_LEAVE_OUT, left out")
    } else if without_structures(shaders) {
        Some("shades ray-tracing hits, left out")
    } else if too_dear(shaders) {
        Some("too dear to compile, left out")
    } else if water_light_out(shaders) {
        Some("the water's tiled light (VICTORY_WATER_LIGHT=0), left out")
    } else {
        None
    }
}

/// Shader hashes left out in every build (`VICTORY_LEAVE_OUT=<16 hex>[,..]`;
/// `VICTORY_SKIP_PIPELINES` does it in diagnostics builds only). After
/// readmet3xt-lab/foxhound 9f8838b109 `pipeline/make.rs` (its launcher sets
/// `02384a60fda6f2d0` the decal heap's CS_DefragHeap_ComputeRelocations,
/// 11.85 ms of the card a frame on a 32-lane host, and `731a39171b568298`
/// the heat-haze / in-scattering pass).
fn named_out(shaders: &[Arc<Shader>]) -> bool {
    static HASHES: OnceLock<Vec<String>> = OnceLock::new();
    let hashes = HASHES.get_or_init(|| {
        std::env::var("VICTORY_LEAVE_OUT")
            .unwrap_or_default()
            .split(',')
            .map(|h| h.trim().to_ascii_lowercase())
            .filter(|h| h.len() == 16 && h.bytes().all(|b| b.is_ascii_hexdigit()))
            .collect()
    });
    !hashes.is_empty() && shaders.iter().any(|s| hashes.contains(&s.hash))
}

/// The light in the water's volume, in tiles (`CS_TransmissionVolumetrics_Tiled` and
/// `_Tiled_LocalLights`): one 64-lane wave a group, run on a host of 32-lane waves in the
/// wave-wide form (`WAVE_WIDE_DEFAULT` in `shader_cache.rs`), which left whole tiles of
/// open water dark around boats, shores and bridges and took about 15 ms of the card a frame
/// (RTX 5070, 25 fps with them, 30 without). After trustno1-coder/VIctory-NVIDIA 35c156087e,
/// which leaves them out by default there; here only with `VICTORY_WATER_LIGHT=0` (unset or 1:
/// made as before). Left out, the water has no light of its own depth (darker, bluer).
const WATER_LIGHT: [&str; 2] = ["5dc3b25986622949", "6f225b641c5b0708"];

fn water_light_out(shaders: &[Arc<Shader>]) -> bool {
    static OUT: OnceLock<bool> = OnceLock::new();
    let out = *OUT.get_or_init(|| std::env::var("VICTORY_WATER_LIGHT").is_ok_and(|v| v.trim() == "0"));
    let named = out && water_light_named(shaders.iter().map(|s| s.hash.as_str()));
    if named {
        tracing::info!(shaders = ?shaders, "the water's tiled light left out (VICTORY_WATER_LIGHT=0)");
    }
    named
}

/// Whether one of these shader hashes is one of [`WATER_LIGHT`].
fn water_light_named<'a>(mut hashes: impl Iterator<Item = &'a str>) -> bool {
    hashes.any(|h| WATER_LIGHT.contains(&h))
}

/// Passes whose output lasts the whole run or feeds every face: the
/// character skin/decoration bakes (`crate::draw::BAKE_PIXEL_SHADERS`) and
/// the face network's producers (after foxhound's `scene_input`). A dropped
/// one leaves a wrong outfit or a blotched face for the run, so with
/// `VICTORY_SCENE_INPUT=1` they are started when the title creates them and
/// a draw waits for them past the frame budget ([`wait_scene_input`]: once
/// per pipeline, at most `VICTORY_SCENE_INPUT_WAIT_MS` (3000, no more than
/// [`SCENE_WAIT_MOST`]) a wait and `VICTORY_SCENE_INPUT_TOTAL_MS` (15000) a
/// run, so a cold cache cannot hold the world for long).
fn scene_input(shaders: &[Arc<Shader>]) -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    if !*ON.get_or_init(|| std::env::var_os("VICTORY_SCENE_INPUT").is_some_and(|v| v == "1")) {
        return false;
    }
    shaders.iter().any(|s| {
        matches!(
            s.hash.as_str(),
            "970c4fbb3b017c0e"
                | "068c90badc86f1fd"
                | "e4a7e088c7780ce8"
                | "ebe4e53f031f5249"
                | "2cc4c197ee29e291"
                | "369cbe906ea72764"
        ) || crate::draw::BAKE_PIXEL_SHADERS.contains(&s.hash.as_str())
            || crate::bake_name::extra(s)
    })
}

/// The longest one scene-input wait may be, whatever is asked.
const SCENE_WAIT_MOST: u64 = 10_000;

fn ms_of(value: Option<&str>, default: u64) -> u64 {
    value.and_then(|v| v.trim().parse().ok()).unwrap_or(default)
}

/// What one scene-input wait may last: `VICTORY_SCENE_INPUT_WAIT_MS`
/// (3000), at most [`SCENE_WAIT_MOST`].
fn scene_input_wait() -> u64 {
    static MS: victory_core::settings::Live = victory_core::settings::Live::new("VICTORY_SCENE_INPUT_WAIT_MS", |v| {
        ms_of(v, 3000).min(SCENE_WAIT_MOST)
    });
    MS.get()
}

/// What all scene-input waits of a run may last together:
/// `VICTORY_SCENE_INPUT_TOTAL_MS` (15000). Live: what was taken stays
/// taken; a larger total gives the rest.
fn scene_input_total() -> u64 {
    static MS: victory_core::settings::Live =
        victory_core::settings::Live::new("VICTORY_SCENE_INPUT_TOTAL_MS", |v| ms_of(v, 15_000));
    MS.get()
}

/// Takes what a scene-input wait may last, in milliseconds, from the run's
/// allowance `taken` of `total` (at most `each`); 0 when it is spent.
fn take_scene_wait(taken: &AtomicU64, each: u64, total: u64) -> u64 {
    let mut got = 0;
    let _ = taken.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |t| {
        got = each.min(total.saturating_sub(t));
        Some(t + got)
    });
    got
}

/// Whether some compute shader of `shaders` holds `name`.
fn named(shaders: &[Arc<Shader>], name: &[u8]) -> bool {
    shaders
        .iter()
        .any(|s| s.stage == "compute" && s.container.windows(name.len()).any(|w| w == name))
}

/// Whether a shader is left out for what it costs the host's driver:
/// `CS_ScreenProbes_ActivateRays` of the title
/// (39f0e9aeb156a61c, 66,501 instructions, 9,948 branches). As translated
/// before the clean-up passes (`emit::shader_opt`), AMD's took a core and
/// 0.8 GB more every 10 s for it and did not finish (RX 7800 XT: 47 GB after
/// 8 minutes); cleaned up, it takes 85 s and 1.3 GB, once.
/// `VICTORY_ACTIVATE_RAYS=0` leaves it out.
fn too_dear(shaders: &[Arc<Shader>]) -> bool {
    static OUT: OnceLock<bool> = OnceLock::new();
    let out =
        *OUT.get_or_init(|| std::env::var_os("VICTORY_ACTIVATE_RAYS").is_some_and(|v| v == "0"));
    out && named(shaders, b"CS_ScreenProbes_ActivateRays")
}

/// Whether a shader only shades what the console's ray-tracing structures
/// were hit by: about 1,000 compute programs of the title
/// (`CS_RaytracingResolve`, the `raytracingresolve_dispatch` pass of each
/// material), all bound at the world's first frame, each 5 to 9 s of AMD's
/// compiler. They stood in the queue before the passes that make the
/// picture, and with the structure builder off there is no hit to shade.
/// Made by default, the picture before loading time (with the builder on
/// they shade what reflections and indirect light hit);
/// `VICTORY_RT_RESOLVE=0` leaves them out while the builder is off.
fn without_structures(shaders: &[Arc<Shader>]) -> bool {
    static OUT: OnceLock<bool> = OnceLock::new();
    let out = *OUT.get_or_init(|| {
        let builder = std::env::var_os("VICTORY_BVH_BUILDER").is_some_and(|v| v == "1");
        std::env::var_os("VICTORY_RT_RESOLVE").is_some_and(|v| v == "0") && !builder
    });
    out && named(shaders, b"CS_RaytracingResolve")
}

// SAFETY: D3D12 objects are free-threaded; the graphics state (whose input
// elements point into its own names) is only read after creation.
unsafe impl Send for Pending {}
// SAFETY: as above.
unsafe impl Sync for Pending {}

impl Pending {
    pub(crate) fn new(shaders: Vec<Arc<Shader>>, graphics: Option<Box<GraphicsState>>) -> Pending {
        let mut key = blake3::Hasher::new();
        for s in &shaders {
            key.update(s.stage.as_bytes());
            key.update(s.hash.as_bytes());
        }
        key.update(
            graphics
                .as_ref()
                .map(|g| g.describe())
                .unwrap_or_default()
                .as_bytes(),
        );
        let left_out = left_out(&shaders);
        let head_map = head_map::is_head_map(shaders.iter().map(|s| (s.stage, s.hash.as_str())));
        let scene_input = left_out.is_none()
            && head_map::scene_input(scene_input(&shaders), head_map, head_map::wait_on());
        Pending {
            head_map,
            screen: graphics.as_ref().is_some_and(|g| g.screen_pass()),
            scene_input,
            scene_waited: AtomicBool::new(false),
            shaders,
            graphics,
            left_out,
            begun: OnceLock::new(),
            queue_state: AtomicU8::new(NOT_QUEUED),
            made: OnceLock::new(),
            completion: (Mutex::new(()), Condvar::new()),
            key: key.finalize().to_hex()[..32].to_owned(),
            used: AtomicBool::new(false),
            eager: AtomicBool::new(false),
        }
    }

    /// Whether it is a compute pipeline.
    fn computes(&self) -> bool {
        matches!(self.shaders.as_slice(), [only] if only.stage == "compute")
    }

    /// Whether the host's driver has been at it for longer than a frame
    /// waits for ([`held_too_long`]).
    fn slow(&self) -> bool {
        let compute = self.computes();
        self.begun
            .get()
            .is_some_and(|at| held_too_long(at.elapsed(), compute, wait_budget(compute)))
    }

    fn make(&self) {
        self.make_with(|| HostPipeline::new(&self.shaders, self.graphics.as_deref()));
        // (bootfast) `VICTORY_PIPELINE_RECIPES=1`: written down for the
        // offline warm-up.
        if super::recipe::on() && matches!(self.made.get(), Some(Some(_))) {
            super::recipe::made(&self.key, &self.shaders, self.graphics.as_deref());
        }
    }

    fn make_with(&self, build: impl FnOnce() -> Option<HostPipeline>) {
        let _ = self.begun.set(Instant::now());
        self.made.get_or_init(|| {
            // A failed translation must complete this queue entry and wake its waiters.
            match catch_unwind(AssertUnwindSafe(build)) {
                Ok(pipeline) => pipeline,
                Err(_) => {
                    tracing::warn!(shaders = ?self.shaders, "host pipeline build panicked");
                    None
                }
            }
        });
        let (lock, done) = &self.completion;
        let _held = lock.lock().unwrap_or_else(|e| e.into_inner());
        done.notify_all();
    }
}

/// Why a pipeline has no host pipeline (diagnostics): its shaders that do
/// not translate, else "refused" or "being made"; `None` if it has one.
pub(crate) fn missing(p: &Pending) -> Option<String> {
    match p.made.get() {
        Some(Some(_)) => None,
        None => Some(
            p.left_out
                .map_or_else(|| format!("being made {:?}", p.shaders), str::to_owned),
        ),
        Some(None) => {
            let failed: Vec<String> = p
                .shaders
                .iter()
                .filter(|s| matches!(s.translated.get(), Some(None)))
                .map(|s| format!("{} {}", s.stage, s.hash))
                .collect();
            Some(if failed.is_empty() {
                format!("refused {:?}", p.shaders)
            } else {
                failed.join(" + ")
            })
        }
    }
}

/// Waiting allowed per title frame, over all threads, by kind.
fn wait_budget(compute: bool) -> Duration {
    #[cfg(test)]
    if let Some(budget) = TEST_BUDGET.get() {
        return budget;
    }
    // Live: logged at its first use and when it changes.
    static OVERRIDE: victory_core::settings::Live = victory_core::settings::Live::new("VICTORY_PIPELINE_WAIT_MS", |v| {
        // Plus one: 0 unset.
        v.and_then(|value| value.trim().parse::<u64>().ok()).map_or(0, |ms| ms.saturating_add(1))
    });
    static LOGGED: AtomicU64 = AtomicU64::new(u64::MAX);
    let bits = OVERRIDE.get();
    let override_ms = bits.checked_sub(1);
    let graphics = override_ms.unwrap_or(DEFAULT_WAIT_MS);
    let compute_ms = override_ms.unwrap_or(DEFAULT_WAIT_COMPUTE_MS);
    if LOGGED.swap(bits, Ordering::Relaxed) != bits {
        tracing::info!(
            graphics,
            compute = compute_ms,
            "host pipeline wait budget per frame, per kind (ms)"
        );
    }
    Duration::from_millis(if compute { compute_ms } else { graphics })
}

/// The default gameplay base budgets of [`wait_budget`]: short enough that
/// mid-gameplay shader compiles do not freeze frames (and that
/// [`held_too_long`] stays active), while loading frames still wait up to
/// [`LOADING_FRAME`] / [`LOADING_FRAME_COMPUTE`] while
/// [`loading_allowance`] lasts. Setting `VICTORY_PIPELINE_WAIT_MS` overrides
/// both kinds to that value (for example `VICTORY_PIPELINE_WAIT_MS=12000`
/// restores a 12 s window for both).
const DEFAULT_WAIT_MS: u64 = 25;
const DEFAULT_WAIT_COMPUTE_MS: u64 = 200;

/// The title's GPU watchdog: one frame of more than this ends the run.
const TITLE_WATCHDOG: Duration = Duration::from_secs(30);

// Both kinds' windows, one after the other, stay under the watchdog.
const _: () =
    assert!(DEFAULT_WAIT_MS + DEFAULT_WAIT_COMPUTE_MS < TITLE_WATCHDOG.as_millis() as u64);
const _: () = assert!(
    LOADING_FRAME.as_millis() + LOADING_FRAME_COMPUTE.as_millis() < TITLE_WATCHDOG.as_millis()
);

#[cfg(test)]
thread_local! {
    /// A test's own frame budget, on its thread alone.
    static TEST_BUDGET: std::cell::Cell<Option<Duration>> = const { std::cell::Cell::new(None) };
}

/// What a frame may wait for one kind of pipeline while a world loads: they
/// are asked for by the hundred then, and a pass the title runs once is
/// lost if its pipeline is not there. Compute pipelines get more, apart:
/// the passes that unpack a world's models are dispatched once (the
/// sibling fork counted 130 compute pipelines dropped while a world loaded,
/// and walls, floors, ceilings and faces missing for the session).
const LOADING_FRAME: Duration = Duration::from_secs(4);
const LOADING_FRAME_COMPUTE: Duration = Duration::from_secs(12);

/// `VICTORY_PIPELINE_GATE_S` (120): the most a run waits past
/// [`wait_budget`] for each kind, in seconds, all frames together; 0 waits
/// as before. The title's GPU watchdog bounds one frame (30 s).
fn loading_allowance() -> Duration {
    static TOTAL: OnceLock<Duration> = OnceLock::new();
    *TOTAL.get_or_init(|| {
        let seconds = std::env::var("VICTORY_PIPELINE_GATE_S")
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .unwrap_or(120);
        tracing::info!(seconds, "host pipeline loading allowance per kind");
        Duration::from_secs(seconds)
    })
}

/// A frame's budget for a pipeline of one kind: the frame budget, or while
/// the run's allowance for that kind lasts (`spent` of it gone) up to
/// [`LOADING_FRAME`] or [`LOADING_FRAME_COMPUTE`].
fn frame_budget(base: Duration, compute: bool, allowance: Duration, spent: Duration) -> Duration {
    let loading = if compute {
        LOADING_FRAME_COMPUTE
    } else {
        LOADING_FRAME
    };
    base.max(loading.min(allowance.saturating_sub(spent)))
}

/// Whether the driver has held a pipeline for longer than most take, so no
/// frame waits for it (one compute program of the title takes 85 s): 10 s
/// for a compute pipeline, 3 s for others. Only while the frame budget
/// `base` (`VICTORY_PIPELINE_WAIT_MS`) is shorter than that: a budget set
/// at or above it is the bound alone, so a pipeline still being made is
/// waited for as long as the frame's budget lasts.
fn held_too_long(held: Duration, compute: bool, base: Duration) -> bool {
    let most = Duration::from_secs(if compute { 10 } else { 3 });
    base < most && held > most
}

/// Pipelines queued and not made yet.
static WAITING: AtomicU64 = AtomicU64::new(0);

/// Pipelines queued and not made yet (for the frame line).
pub(crate) fn waiting() -> u64 {
    WAITING.load(Ordering::Relaxed)
}

/// The host pipeline if it is made (or is made within the frame's budget);
/// queues it on first use.
pub(crate) fn ready(p: &Arc<Pending>) -> Option<&HostPipeline> {
    if p.left_out.is_some() {
        return None;
    }
    // A still-pending pipeline was requested by the title too. Prewarm it
    // on the next launch, even if this run never got to use it.
    if !p.used.load(Ordering::Relaxed) && !p.used.swap(true, Ordering::Relaxed) {
        super::library::note_used(&p.key);
        super::eager::first_use(origin(p), p.made.get().is_some());
    }
    let head_map = head_map::first(p);
    if p.made.get().is_none() {
        start(p, REQUESTED, 0);
        screen_wait::front(p);
        if p.scene_input {
            wait_scene_input(p, crate::frame::current());
        } else {
            wait(p, crate::frame::current());
        }
        compute_wait::after(p, crate::frame::current());
        screen_wait::after(p, crate::frame::current());
        super::eager::waited(p.made.get().is_some());
    }
    if let Some(first) = head_map {
        head_map::said(p, first);
    }
    p.made.get()?.as_ref()
}

/// A draw of `p` was dropped ([`screen_wait::dropped`]).
pub(crate) fn note_dropped(p: &Pending) {
    screen_wait::dropped(p);
}

/// At present `frame` ([`screen_wait::presented`]).
pub(crate) fn presented(frame: u64) {
    screen_wait::presented(frame);
}

/// How `p` came to be queued, at its first draw or dispatch
/// ([`super::eager`]'s counters).
fn origin(p: &Pending) -> super::eager::Origin {
    use super::eager::Origin;
    if p.eager.load(Ordering::Relaxed) {
        Origin::Eager
    } else if p.made.get().is_some() || p.queue_state.load(Ordering::Relaxed) != NOT_QUEUED {
        Origin::Early
    } else {
        Origin::AtDraw
    }
}

/// Makes `p` on this thread if it is not made yet (waiting for a worker
/// that is making it); whether it has a host pipeline.
pub(crate) fn now(p: &Arc<Pending>) -> bool {
    if p.left_out.is_some() {
        return false;
    }
    p.make();
    p.made.get().is_some_and(|made| made.is_some())
}

/// Translates the containers of compute programs and makes their host
/// pipelines, for the caches on disk; whether it has one (`None`: left out).
pub(crate) fn warm(shaders: Vec<Arc<Shader>>) -> Option<bool> {
    let p = Pending::new(shaders, None);
    if p.left_out.is_some() {
        return None;
    }
    let made = HostPipeline::new(&p.shaders, None).is_some();
    if made {
        // (bootfast) With `VICTORY_PIPELINE_RECIPES=1` a compute warm-up
        // writes them down too.
        super::recipe::made(&p.key, &p.shaders, None);
    }
    Some(made)
}

/// (bootfast) Makes the host pipeline of a recipe (`super::recipe`) for the
/// library on disk: the key it comes out with, and whether it has one
/// (`None`: left out).
pub(crate) fn warm_recipe(shaders: Vec<Arc<Shader>>, graphics: Option<Box<GraphicsState>>) -> (String, Option<bool>) {
    let p = Pending::new(shaders, graphics);
    if p.left_out.is_some() {
        return (p.key, None);
    }
    let made = HostPipeline::new(&p.shaders, p.graphics.as_deref()).is_some();
    (p.key, Some(made))
}

/// At the title's creation of a pipeline: starts it at once if an earlier
/// run drew with it (during loading, so the world's first frames find it).
pub(crate) fn early(p: &Arc<Pending>) {
    if p.left_out.is_some() || p.shaders.is_empty() {
        return;
    }
    if p.scene_input {
        start(p, REQUESTED, 0);
        if p.head_map {
            compute_wait::to_front(p);
        }
        return;
    }
    if let Some(rank) = super::library::wanted_early(&p.key) {
        super::library::EARLY.fetch_add(1, Ordering::Relaxed);
        start(p, EARLY, rank);
        return;
    }
    // (bootfast, Belobug #622) `VICTORY_PIPELINE_EAGER=N`: one no earlier
    // run drew with is queued now too, behind the early list.
    let limit = super::eager::limit();
    if limit > 0 && workers() {
        let mut queue = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
        if queue.push_eager(p, limit) {
            super::eager::queued();
            PEAK.fetch_max(
                WAITING.fetch_add(1, Ordering::Relaxed) + 1,
                Ordering::Relaxed,
            );
            AVAILABLE.notify_one();
        }
    }
}

/// The rank of a pipeline queued at creation by `VICTORY_PIPELINE_EAGER`:
/// after every pipeline of the early list, in the order of creation.
const EAGER_RANK: u32 = u32::MAX;

/// How the threads that wait in one frame share its budget
/// (`VICTORY_PIPELINE_WAIT_SHARED`): by default a window of waiting time
/// ([`Sharing::Window`]); `reserve` the way before it, `0` the way before
/// that. Counted only when a wait had ended, the budget was one per
/// thread: each of the title's five recording threads began its first
/// wait of a frame with nothing counted yet and waited the whole half
/// second, 2.5 s of waiting in every frame in which pipelines were being
/// made (runs 34-49: "2504 ms waiting for pipelines" in each such frame).
/// Reserved, a thread that waited took the rest of the budget, and every
/// other thread's new pipeline in that frame was dropped at once: a run
/// of about 835 drops as "being made" whose frame lines show at most 9 ms
/// waited (run fork-essentials).
fn shared_budget() -> Sharing {
    static ON: OnceLock<Sharing> = OnceLock::new();
    *ON.get_or_init(|| {
        sharing_from(
            std::env::var("VICTORY_PIPELINE_WAIT_SHARED")
                .ok()
                .as_deref(),
        )
    })
}

/// How the threads that wait in one frame count its budget for one kind
/// of pipeline (`VICTORY_PIPELINE_WAIT_SHARED`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sharing {
    /// The budget is a window of waiting time: every thread may wait while
    /// the time in which some thread has waited in this frame is shorter,
    /// and nothing is reserved, so no thread drops a pipeline because
    /// another is waiting. The threads' waits together take at most the
    /// budget.
    Window,
    /// `reserve`: a thread that waits takes what is left of the budget and
    /// gives back what it did not use; another thread asking meanwhile
    /// gets nothing.
    Reserve,
    /// `0`: each thread counts its waits once they have ended.
    PerThread,
}

/// The way of counting the switch's value asks for.
fn sharing_from(value: Option<&str>) -> Sharing {
    match value.map(str::trim) {
        Some("0") => Sharing::PerThread,
        Some("reserve") => Sharing::Reserve,
        _ => Sharing::Window,
    }
}

/// One frame's waiting for one kind of pipeline.
#[derive(Debug)]
struct FrameWait {
    frame: u64,
    /// The waiting counted against the budget so far (the window: time in
    /// which some thread waited, up to `since`).
    spent: Duration,
    /// The window: threads waiting now, and since when one has been.
    waiting: u32,
    since: Option<Instant>,
}

impl FrameWait {
    const fn new() -> FrameWait {
        FrameWait {
            frame: u64::MAX,
            spent: Duration::ZERO,
            waiting: 0,
            since: None,
        }
    }

    /// A thread is to wait in `frame` at `now`: how long it may.
    fn begin(&mut self, sharing: Sharing, frame: u64, budget: Duration, now: Instant) -> Duration {
        if self.frame != frame {
            *self = FrameWait {
                frame,
                ..FrameWait::new()
            };
        }
        if sharing == Sharing::Window {
            let open = self
                .since
                .map_or(Duration::ZERO, |at| now.saturating_duration_since(at));
            let left = budget.saturating_sub(self.spent + open);
            if !left.is_zero() {
                if self.waiting == 0 {
                    self.since = Some(now);
                }
                self.waiting += 1;
            }
            return left;
        }
        let left = budget.saturating_sub(self.spent);
        if sharing == Sharing::Reserve {
            self.spent += left;
        }
        left
    }

    /// The thread that [`begin`](Self::begin) gave `left` (more than
    /// nothing) in `frame` has waited `waited`, and stops at `now`. A frame
    /// that began meanwhile has its own budget.
    fn end(
        &mut self,
        sharing: Sharing,
        frame: u64,
        left: Duration,
        waited: Duration,
        now: Instant,
    ) {
        if self.frame != frame {
            return;
        }
        match sharing {
            Sharing::Window => {
                self.waiting = self.waiting.saturating_sub(1);
                if self.waiting == 0
                    && let Some(at) = self.since.take()
                {
                    self.spent += now.saturating_duration_since(at);
                }
            }
            Sharing::Reserve => self.spent = self.spent.saturating_sub(left - waited),
            Sharing::PerThread => self.spent += waited,
        }
    }
}

/// Waits for a [`scene_input`] pipeline outside the frame budgets: the
/// first time a draw finds it not made, for as long as the run's allowance
/// gives ([`take_scene_wait`]); after that, or with the allowance spent,
/// within the frame's budget as any pipeline. Every such wait is logged.
fn wait_scene_input(p: &Pending, frame: u64) {
    static TAKEN: AtomicU64 = AtomicU64::new(0);
    if p.scene_waited.swap(true, Ordering::Relaxed) {
        return wait(p, frame);
    }
    let allowed = take_scene_wait(&TAKEN, scene_input_wait(), scene_input_total());
    if allowed == 0 {
        tracing::warn!(shaders = ?p.shaders, "scene-input wait: the run's allowance (VICTORY_SCENE_INPUT_TOTAL_MS) is spent; waited as any pipeline");
        return wait(p, frame);
    }
    let _hitch = crate::hitch::span(crate::hitch::Phase::PipelineWait);
    let started = Instant::now();
    let left = Duration::from_millis(allowed);
    let (lock, done) = &p.completion;
    let mut held = lock.lock().unwrap_or_else(|e| e.into_inner());
    while p.made.get().is_none() {
        let rest = left.saturating_sub(started.elapsed());
        if rest.is_zero() {
            break;
        }
        held = done.wait_timeout(held, rest).unwrap_or_else(|e| e.into_inner()).0;
    }
    drop(held);
    let waited = started.elapsed();
    // What was not waited goes back to the run's allowance.
    let unused = allowed.saturating_sub(waited.as_millis() as u64);
    TAKEN.fetch_sub(unused, Ordering::Relaxed);
    let made = p.made.get().is_some();
    if made {
        tracing::info!(
            shaders = ?p.shaders,
            frame,
            waited_ms = waited.as_millis() as u64,
            run_ms = TAKEN.load(Ordering::Relaxed),
            "scene-input wait: a bake or face pipeline was made while the draw waited (VICTORY_SCENE_INPUT)"
        );
    } else {
        tracing::warn!(
            shaders = ?p.shaders,
            frame,
            waited_ms = waited.as_millis() as u64,
            run_ms = TAKEN.load(Ordering::Relaxed),
            "scene-input wait: a bake or face pipeline was not made in time (VICTORY_SCENE_INPUT_WAIT_MS): its draw is dropped"
        );
    }
}

/// Waits for `p` while frame `frame` has budget left.
fn wait(p: &Pending, frame: u64) {
    let compute = p.computes();
    let base = wait_budget(compute);
    if base.is_zero() || p.slow() {
        return;
    }
    // What was waited past the base budget since the start, in
    // milliseconds, by kind (others, compute).
    static PAST: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];
    let past = &PAST[usize::from(compute)];
    let budget = frame_budget(
        base,
        compute,
        loading_allowance(),
        Duration::from_millis(past.load(Ordering::Relaxed)),
    );
    // The frame the budget belongs to, and what was waited of it (or is
    // taken by a thread waiting now), by kind.
    static FRAMES: [Mutex<FrameWait>; 2] = [const { Mutex::new(FrameWait::new()) }; 2];
    let frame_wait = &FRAMES[usize::from(compute)];
    let sharing = shared_budget();
    let started = Instant::now();
    let left = frame_wait
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .begin(sharing, frame, budget, started);
    if left.is_zero() {
        return;
    }
    let _hitch = crate::hitch::span(crate::hitch::Phase::PipelineWait);
    let (lock, done) = &p.completion;
    let mut held = lock.lock().unwrap_or_else(|e| e.into_inner());
    while p.made.get().is_none() && !p.slow() {
        let rest = left.saturating_sub(started.elapsed());
        if rest.is_zero() {
            break;
        }
        // Woken now and then to see whether it has become a slow one.
        let rest = rest.min(Duration::from_millis(250));
        held = done
            .wait_timeout(held, rest)
            .unwrap_or_else(|e| e.into_inner())
            .0;
    }
    drop(held);
    let waited = started.elapsed().min(left);
    past.fetch_add(
        waited.saturating_sub(base).as_millis() as u64,
        Ordering::Relaxed,
    );
    frame_wait.lock().unwrap_or_else(|e| e.into_inner()).end(
        sharing,
        frame,
        left,
        waited,
        Instant::now(),
    );
    #[cfg(feature = "diagnostics")]
    crate::copystat::add(
        crate::copystat::C::PipelineWaitUs,
        started.elapsed().as_micros() as u64,
    );
}

const NOT_QUEUED: u8 = 0;
const EARLY: u8 = 1;
const REQUESTED: u8 = 2;
const MAKING: u8 = 3;

/// A pipeline started early, made in the order of its rank in the early
/// list ([`super::early`]), then of its creation.
struct Early {
    order: Reverse<(u32, u64)>,
    p: Arc<Pending>,
}

impl PartialEq for Early {
    fn eq(&self, other: &Self) -> bool {
        self.order == other.order
    }
}
impl Eq for Early {}
impl PartialOrd for Early {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Early {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.order.cmp(&other.order)
    }
}

struct Queue {
    early: BinaryHeap<Early>,
    created: u64,
    requested: VecDeque<Arc<Pending>>,
    /// Pipelines queued by [`Queue::push_eager`] not taken by a worker yet.
    eager: u64,
}

impl Queue {
    const fn new() -> Self {
        Self {
            early: BinaryHeap::new(),
            created: 0,
            requested: VecDeque::new(),
            eager: 0,
        }
    }

    /// Queues a pipeline nobody asked for yet at [`EAGER_RANK`] while fewer
    /// than `limit` such wait (read and raised under [`QUEUE`]'s lock, so
    /// however many threads create pipelines at once the limit holds);
    /// whether it was queued.
    fn push_eager(&mut self, p: &Arc<Pending>, limit: u64) -> bool {
        if self.eager >= limit || self.push(p, EARLY, EAGER_RANK) != Some(true) {
            return false;
        }
        p.eager.store(true, Ordering::Relaxed);
        self.eager += 1;
        true
    }

    /// Queues an unrequested pipeline or promotes one queued at creation
    /// (`rank`: its place in the early list). Returns whether it is new to
    /// the queue, or `None` if nothing changed.
    fn push(&mut self, p: &Arc<Pending>, priority: u8, rank: u32) -> Option<bool> {
        let prev = p.queue_state.load(Ordering::Relaxed);
        if prev >= priority {
            return None;
        }
        p.queue_state.store(priority, Ordering::Relaxed);
        if priority == REQUESTED {
            self.requested.push_back(p.clone());
        } else {
            self.created += 1;
            self.early.push(Early {
                order: Reverse((rank, self.created)),
                p: p.clone(),
            });
        }
        Some(prev == NOT_QUEUED)
    }

    /// Moves a requested pipeline not taken yet to the front
    /// ([`compute_wait`]).
    fn to_front(&mut self, p: &Arc<Pending>) {
        if let Some(at) = self.requested.iter().position(|q| Arc::ptr_eq(q, p))
            && let Some(q) = self.requested.remove(at)
        {
            self.requested.push_front(q);
        }
    }

    fn pop(&mut self) -> Option<Arc<Pending>> {
        let p = self.requested.pop_front().or_else(|| {
            // An early entry can have been promoted and claimed already.
            std::iter::from_fn(|| self.early.pop().map(|e| e.p))
                .find(|p| p.queue_state.load(Ordering::Relaxed) == EARLY)
        })?;
        p.queue_state.store(MAKING, Ordering::Relaxed);
        // Taken once: a promoted one is skipped in the heap after.
        if p.eager.load(Ordering::Relaxed) {
            self.eager -= 1;
        }
        Some(p)
    }
}

/// Queues `p` for the workers if it is not made or queued yet: `asked`, as
/// a draw's is (before the pipelines nothing has asked for), or else among
/// those made early. Does not wait (`VICTORY_BVH_BUILDER_EARLY`).
pub(crate) fn soon(p: &Arc<Pending>, asked: bool) {
    if p.left_out.is_none() && p.made.get().is_none() && !p.shaders.is_empty() {
        start(p, if asked { REQUESTED } else { EARLY }, 0);
    }
}

static QUEUE: Mutex<Queue> = Mutex::new(Queue::new());
static AVAILABLE: Condvar = Condvar::new();

/// Start or promote work for a draw without waiting behind the entire early
/// list. Queue changes and worker claims happen under the same lock.
fn start(p: &Arc<Pending>, priority: u8, rank: u32) {
    if p.queue_state.load(Ordering::Relaxed) >= priority {
        return;
    }
    if !workers() {
        if p.queue_state.swap(MAKING, Ordering::Relaxed) == NOT_QUEUED {
            p.make();
        }
        return;
    }
    let mut queue = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(new) = queue.push(p, priority, rank) {
        if new {
            PEAK.fetch_max(
                WAITING.fetch_add(1, Ordering::Relaxed) + 1,
                Ordering::Relaxed,
            );
        }
        AVAILABLE.notify_one();
    }
}

fn next() -> Arc<Pending> {
    let mut queue = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
    loop {
        if let Some(p) = queue.pop() {
            return p;
        }
        queue = AVAILABLE.wait(queue).unwrap_or_else(|e| e.into_inner());
    }
}

/// Reserved stack per pipeline compile worker (64 MiB): NVIDIA's shader
/// compiler recurses deeply on large world compute pipelines and overflowed
/// the default 2 MiB stack, failing compilation as out of memory.
const WORKER_STACK_BYTES: usize = 64 << 20;

/// Starts the workers ([`worker_count`]) on first use.
fn workers() -> bool {
    static RUNNING: OnceLock<bool> = OnceLock::new();
    *RUNNING.get_or_init(|| {
        let mut running = false;
        let workers = worker_count(
            std::thread::available_parallelism().map_or(2, |n| n.get()),
            std::env::var("VICTORY_PIPELINE_WORKERS").ok().as_deref(),
        );
        tracing::info!(workers, "host pipeline workers");
        for i in 0..workers {
            let spawned = std::thread::Builder::new()
                .name(format!("victory-pipelines-{i}"))
                // The driver compiles pipelines on this thread, recursively:
                // NVIDIA's compiler overflowed the default 2 MB on a world
                // compute program and refused it as out of memory (reserved
                // address space; committed only as used).
                .stack_size(WORKER_STACK_BYTES)
                .spawn(|| {
                    crate::topology::pin_background("victory-pipelines");
                    // Below the title's own threads (loading, recording).
                    // SAFETY: a plain call on our own thread.
                    unsafe {
                        use windows::Win32::System::Threading::*;
                        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
                    }
                    loop {
                        next().make();
                        if WAITING.fetch_sub(1, Ordering::Relaxed) == 1 {
                            drained();
                        } else {
                            save_library_async();
                        }
                    }
                });
            if let Err(e) = spawned {
                tracing::warn!(error = %e, "no pipeline worker");
            } else {
                running = true;
            }
        }
        running
    })
}

/// How many workers make pipelines: `VICTORY_PIPELINE_WORKERS`, else every
/// logical processor but 4 (2 to 28). Translating is most of a pipeline the
/// first time (the library has the host's part of nearly all of them: run
/// 19 loaded 2,386 and made 257), and each translation takes one processor:
/// with half of the 32 (at most 12), 1,300 to 1,800 pipelines were still
/// being made a minute after boot, while the title, on its start screens,
/// left most processors idle. The workers run below the title's threads.
fn worker_count(processors: usize, value: Option<&str>) -> usize {
    const MOST: usize = 28;
    value
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or_else(|| processors.saturating_sub(4))
        .clamp(2, MOST)
}

/// The most pipelines waiting at once since the workers last ran out.
static PEAK: AtomicU64 = AtomicU64::new(0);

/// The workers ran out of work: logged after a batch of 10 or more, and
/// the cache saved (at most every 20 s).
fn drained() {
    use super::library::{EARLY, LOADED, MADE};
    if PEAK.swap(0, Ordering::Relaxed) >= 10 {
        tracing::info!(
            made = MADE.load(Ordering::Relaxed),
            loaded = LOADED.load(Ordering::Relaxed),
            started_early = EARLY.load(Ordering::Relaxed),
            "host pipelines all made"
        );
        super::eager::log();
    }
    save_library_async();
}

/// Guards [`save_library_async`] so at most one background save runs at once.
static LIBRARY_SAVING: AtomicBool = AtomicBool::new(false);
/// `crate::host::now_ms().max(1)` of the last background save spawn/completion
/// (0: none yet), matching `library::save`'s 20 s interval so worker threads do
/// not spawn redundant threads between saves.
static LAST_SAVE_MS: AtomicU64 = AtomicU64::new(0);

/// Runs `super::library::save(false)` on a dedicated low-priority background
/// thread so pipeline workers never stall for hundreds of milliseconds
/// serialising and renaming a ~1 GiB+ pipeline library (`ID3D12PipelineLibrary1`
/// lives in a `'static` `OnceLock` and `Library` is `Send + Sync`; `save`
/// writes to `.tmp`, atomically renames over the cache file, and logs on
/// completion).
fn save_library_async() {
    const SAVE_EVERY_MS: u64 = 20_000;
    if LIBRARY_SAVING.load(Ordering::Acquire) {
        return;
    }
    let now = crate::host::now_ms().max(1);
    let last = LAST_SAVE_MS.load(Ordering::Relaxed);
    if last != 0 && now.saturating_sub(last) < SAVE_EVERY_MS {
        return;
    }
    if LIBRARY_SAVING
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return;
    }
    let now = crate::host::now_ms().max(1);
    let last = LAST_SAVE_MS.load(Ordering::Relaxed);
    if last != 0 && now.saturating_sub(last) < SAVE_EVERY_MS {
        LIBRARY_SAVING.store(false, Ordering::Release);
        return;
    }
    LAST_SAVE_MS.store(now, Ordering::Relaxed);
    let spawned = std::thread::Builder::new()
        .name("victory-library-save".into())
        .spawn(|| {
            struct Guard;
            impl Drop for Guard {
                fn drop(&mut self) {
                    LAST_SAVE_MS.store(crate::host::now_ms().max(1), Ordering::Relaxed);
                    LIBRARY_SAVING.store(false, Ordering::Release);
                }
            }
            let _guard = Guard;
            crate::topology::pin_background("victory-library-save");
            // Below both title threads and pipeline compile workers.
            // SAFETY: a plain call on our own thread.
            unsafe {
                use windows::Win32::System::Threading::*;
                let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_LOWEST);
            }
            super::library::save(false);
        });
    if let Err(e) = spawned {
        LIBRARY_SAVING.store(false, Ordering::Release);
        tracing::warn!(error = %e, "could not spawn pipeline library save thread");
    }
}

#[cfg(test)]
mod loading_tests {
    use super::{
        FrameWait, LOADING_FRAME, LOADING_FRAME_COMPUTE, Sharing, frame_budget, held_too_long,
        sharing_from,
    };
    use std::time::{Duration, Instant};

    /// Two threads ask for pipelines still being made in one frame, the
    /// second 1 ms after the first: by default both may wait, inside one
    /// window of the frame's budget, and neither drops its pipeline
    /// because the other is waiting. The window bounds their waiting
    /// together: once some thread has waited its whole length, a thread
    /// that asks gets nothing. A new frame has a window of its own.
    #[test]
    fn two_threads_in_one_frame_both_wait_inside_the_window() {
        let sharing = sharing_from(None);
        let (budget, t0, ms) = (
            Duration::from_secs(12),
            Instant::now(),
            Duration::from_millis(1),
        );
        let mut w = FrameWait::new();
        let a = w.begin(sharing, 7, budget, t0);
        let b = w.begin(sharing, 7, budget, t0 + ms);
        assert_eq!(a, budget);
        assert_eq!(
            b,
            budget - ms,
            "the second thread drops its pipeline at once: {sharing:?} gives it {b:?}"
        );
        // Both made after 2 s: 2 s of the window gone.
        w.end(
            sharing,
            7,
            a,
            Duration::from_secs(2),
            t0 + Duration::from_secs(2),
        );
        w.end(
            sharing,
            7,
            b,
            Duration::from_secs(2) - ms,
            t0 + Duration::from_secs(2),
        );
        assert_eq!(
            w.begin(sharing, 7, budget, t0 + Duration::from_secs(5)),
            Duration::from_secs(10)
        );
        // That thread waits out the rest: the window is used up.
        w.end(
            sharing,
            7,
            Duration::from_secs(10),
            Duration::from_secs(10),
            t0 + Duration::from_secs(15),
        );
        assert_eq!(
            w.begin(sharing, 7, budget, t0 + Duration::from_secs(16)),
            Duration::ZERO
        );
        // The next frame.
        assert_eq!(
            w.begin(sharing, 8, budget, t0 + Duration::from_secs(17)),
            budget
        );
    }

    /// `VICTORY_PIPELINE_WAIT_SHARED=reserve`, the default before the
    /// window: the second thread gets nothing while the first waits. `0`:
    /// each thread counts only waits that have ended.
    #[test]
    fn the_old_ways_of_counting_stay_behind_the_switch() {
        let (budget, t0) = (Duration::from_secs(12), Instant::now());
        let mut w = FrameWait::new();
        assert_eq!(w.begin(Sharing::Reserve, 1, budget, t0), budget);
        assert_eq!(w.begin(Sharing::Reserve, 1, budget, t0), Duration::ZERO);
        let mut w = FrameWait::new();
        assert_eq!(w.begin(Sharing::PerThread, 1, budget, t0), budget);
        assert_eq!(w.begin(Sharing::PerThread, 1, budget, t0), budget);
        w.end(Sharing::PerThread, 1, budget, Duration::from_secs(3), t0);
        assert_eq!(
            w.begin(Sharing::PerThread, 1, budget, t0),
            Duration::from_secs(9)
        );
        assert_eq!(sharing_from(Some("0")), Sharing::PerThread);
        assert_eq!(sharing_from(Some("reserve")), Sharing::Reserve);
    }

    /// While a run's allowance lasts a frame may wait 4 s for a graphics
    /// pipeline and 12 s for a compute one, then only the frame budget; a
    /// budget set above them stays.
    #[test]
    fn a_frame_waits_longer_while_the_loading_allowance_lasts() {
        let (base, allowance) = (Duration::from_millis(500), Duration::from_secs(120));
        assert_eq!(
            frame_budget(base, false, allowance, Duration::ZERO),
            LOADING_FRAME
        );
        assert_eq!(
            frame_budget(base, true, allowance, Duration::ZERO),
            LOADING_FRAME_COMPUTE
        );
        // 1 s of the allowance left: 1 s.
        assert_eq!(
            frame_budget(base, true, allowance, Duration::from_secs(119)),
            Duration::from_secs(1)
        );
        // Spent: the frame budget, as before.
        assert_eq!(
            frame_budget(base, true, allowance, Duration::from_secs(130)),
            base
        );
        assert_eq!(
            frame_budget(base, false, Duration::ZERO, Duration::ZERO),
            base,
            "VICTORY_PIPELINE_GATE_S=0"
        );
        assert_eq!(
            frame_budget(Duration::from_secs(20), true, allowance, Duration::ZERO),
            Duration::from_secs(20)
        );
    }

    /// No frame waits for a pipeline the driver has held past 10 s
    /// (compute) or 3 s (others), while the frame budget is shorter; a
    /// budget at or above that is the bound alone.
    #[test]
    fn a_pipeline_held_too_long_is_not_waited_for() {
        let base = Duration::from_millis(500);
        assert!(!held_too_long(Duration::from_secs(10), true, base));
        assert!(held_too_long(Duration::from_millis(10_001), true, base));
        assert!(!held_too_long(Duration::from_secs(3), false, base));
        assert!(held_too_long(Duration::from_millis(3_001), false, base));
        // VICTORY_PIPELINE_WAIT_MS=12000: waited for however long it is held.
        let long = Duration::from_secs(12);
        assert!(!held_too_long(Duration::from_secs(60), true, long));
        assert!(!held_too_long(Duration::from_secs(60), false, long));
        // 5 s: past it for compute pipelines only.
        let between = Duration::from_secs(5);
        assert!(held_too_long(Duration::from_secs(60), true, between));
        assert!(!held_too_long(Duration::from_secs(60), false, between));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wait_budgets_are_live() {
        use victory_core::settings::check_live;
        check_live("VICTORY_SCENE_INPUT_WAIT_MS", "50000", scene_input_wait, 3000, SCENE_WAIT_MOST);
        check_live("VICTORY_SCENE_INPUT_TOTAL_MS", "100", scene_input_total, 15_000, 100);
    }

    #[test]
    fn a_panicking_pipeline_is_refused_and_the_worker_continues() {
        let bad = Arc::new(Pending::new(Vec::new(), None));
        let good = Arc::new(Pending::new(Vec::new(), None));
        let failed = bad.clone();
        let next = good.clone();
        let worker = std::thread::spawn(move || {
            failed.make_with(|| panic!("synthetic pipeline build panic"));
            next.make_with(|| None);
        });

        worker.join().expect("the pipeline worker should survive");
        assert!(matches!(bad.made.get(), Some(None)));
        assert!(matches!(good.made.get(), Some(None)));

        bad.make_with(|| panic!("the refused pipeline should stay complete"));
    }

    /// Scene-input waits take from the run's allowance, each at most its
    /// own bound, and nothing once it is spent.
    #[test]
    fn scene_input_waits_are_bounded_each_and_per_run() {
        let taken = AtomicU64::new(0);
        assert_eq!(take_scene_wait(&taken, 3000, 7000), 3000);
        assert_eq!(take_scene_wait(&taken, 3000, 7000), 3000);
        assert_eq!(take_scene_wait(&taken, 3000, 7000), 1000);
        assert_eq!(take_scene_wait(&taken, 3000, 7000), 0);
        // What a wait gave back can be taken again.
        taken.fetch_sub(2500, Ordering::Relaxed);
        assert_eq!(take_scene_wait(&taken, 3000, 7000), 2500);
        assert_eq!(take_scene_wait(&AtomicU64::new(0), 3000, 0), 0);
    }

    /// A scene-input pipeline waits past the frame budget only the first
    /// time; it is made by then here.
    #[test]
    fn a_scene_input_pipeline_is_waited_for_once() {
        let mut p = Pending::new(Vec::new(), None);
        p.scene_input = true;
        let p = Arc::new(p);
        assert!(ready(&p).is_none());
        assert!(p.scene_waited.load(Ordering::Relaxed));
        assert!(matches!(p.made.get(), Some(None)), "the first use waits until the worker has answered");
    }

    /// The water light's two tiled programs are named, and no other shader; with
    /// `VICTORY_WATER_LIGHT` unset nothing is left out.
    #[test]
    fn the_water_light_is_named_and_left_out_only_with_its_switch() {
        assert!(water_light_named(["5dc3b25986622949"].into_iter()));
        assert!(water_light_named(["1aa247ed3653b616", "6f225b641c5b0708"].into_iter()));
        assert!(!water_light_named(["1aa247ed3653b616", "926c46bab3b86bd6"].into_iter()));
        assert!(!water_light_named(std::iter::empty()));
        if std::env::var_os("VICTORY_WATER_LIGHT").is_none() {
            assert!(!water_light_out(&[]));
            assert!(left_out(&[]).is_none());
        }
    }

    /// `VICTORY_LEAVE_OUT` is read once; with it unset nothing is named.
    #[test]
    fn nothing_is_named_out_without_the_switch() {
        if std::env::var_os("VICTORY_LEAVE_OUT").is_none() {
            assert!(!named_out(&[]));
        }
    }

    #[test]
    fn another_pipeline_can_finish_while_a_waiter_holds_its_lock() {
        let first = Pending::new(Vec::new(), None);
        let second = Pending::new(Vec::new(), None);
        let guard = first.completion.0.lock().unwrap();
        std::thread::scope(|scope| {
            let (sent, received) = std::sync::mpsc::channel();
            scope.spawn(move || {
                second.make();
                sent.send(()).unwrap();
            });
            let completed = received.recv_timeout(Duration::from_secs(5));
            drop(guard);
            completed.unwrap();
        });
    }

    /// A first use queues the pipeline and, within the budget, gets the
    /// worker's answer (here: no host pipeline for no shaders).
    #[test]
    fn first_use_is_made_by_a_worker() {
        let p = Arc::new(Pending::new(Vec::new(), None));
        assert!(ready(&p).is_none());
        assert_ne!(p.queue_state.load(Ordering::Relaxed), NOT_QUEUED);
        let started = Instant::now();
        while p.made.get().is_none() && started.elapsed() < Duration::from_secs(10) {
            std::thread::yield_now();
        }
        assert!(matches!(p.made.get(), Some(None)));
    }

    #[test]
    fn pending_pipeline_is_recorded_for_the_next_launch() {
        let mut p = Pending::new(Vec::new(), None);
        p.key = "pending-pipeline-test".to_owned();
        // Simulate a worker that has taken the job but cannot finish it yet.
        p.queue_state.store(MAKING, Ordering::Relaxed);
        let p = Arc::new(p);
        // No frame budget on this thread (`VICTORY_PIPELINE_WAIT_MS=0`):
        // the request is not waited for, where the default would wait out
        // a 12 s window for a pipeline that is never made.
        super::TEST_BUDGET.set(Some(std::time::Duration::ZERO));

        assert!(super::super::library::wanted_early(&p.key).is_none());
        assert!(ready(&p).is_none());
        assert!(p.made.get().is_none());
        assert!(super::super::library::wanted_early(&p.key).is_some());
    }

    #[test]
    fn draws_promote_early_pipelines_without_claiming_them_twice() {
        let earlier = Arc::new(Pending::new(Vec::new(), None));
        let promoted = Arc::new(Pending::new(Vec::new(), None));
        let requested = Arc::new(Pending::new(Vec::new(), None));
        let mut queue = Queue::new();

        assert_eq!(queue.push(&earlier, EARLY, 0), Some(true));
        assert_eq!(queue.push(&promoted, EARLY, 0), Some(true));
        assert_eq!(queue.push(&requested, REQUESTED, 0), Some(true));
        assert_eq!(queue.push(&promoted, REQUESTED, 0), Some(false));
        assert_eq!(queue.push(&promoted, REQUESTED, 0), None);

        assert!(Arc::ptr_eq(&queue.pop().unwrap(), &requested));
        assert!(Arc::ptr_eq(&queue.pop().unwrap(), &promoted));
        assert_eq!(queue.push(&promoted, REQUESTED, 0), None);
        assert!(Arc::ptr_eq(&queue.pop().unwrap(), &earlier));
        assert!(queue.pop().is_none());
    }

    #[test]
    fn early_pipelines_are_made_in_the_order_of_the_early_list() {
        let [late, first, second, requested] =
            std::array::from_fn(|_| Arc::new(Pending::new(Vec::new(), None)));
        let mut queue = Queue::new();
        assert_eq!(queue.push(&late, EARLY, 900), Some(true));
        assert_eq!(queue.push(&second, EARLY, 7), Some(true));
        assert_eq!(queue.push(&first, EARLY, 3), Some(true));
        assert_eq!(queue.push(&requested, REQUESTED, 0), Some(true));
        for expected in [&requested, &first, &second, &late] {
            assert!(Arc::ptr_eq(&queue.pop().unwrap(), expected));
        }
        assert!(queue.pop().is_none());
    }

    /// (bootfast, Belobug #622) Pipelines queued at creation go after the
    /// early list and every requested one, at most `limit` waiting at once;
    /// a draw promotes one ahead of the rest, and it is taken once.
    #[test]
    fn eager_pipelines_wait_behind_the_early_list_and_are_bounded() {
        let [a, b, c, listed, asked] =
            std::array::from_fn(|_| Arc::new(Pending::new(Vec::new(), None)));
        let mut queue = Queue::new();
        assert!(queue.push_eager(&a, 2));
        assert!(!queue.push_eager(&a, 2));
        assert!(queue.push_eager(&b, 2));
        assert!(!queue.push_eager(&c, 2));
        assert_eq!(c.queue_state.load(Ordering::Relaxed), NOT_QUEUED);
        assert!(!c.eager.load(Ordering::Relaxed));
        assert_eq!(queue.push(&listed, EARLY, 5), Some(true));
        assert_eq!(queue.push(&asked, REQUESTED, 0), Some(true));
        assert_eq!(queue.push(&b, REQUESTED, 0), Some(false));
        for expected in [&asked, &b, &listed] {
            assert!(Arc::ptr_eq(&queue.pop().unwrap(), expected));
        }
        assert_eq!(queue.eager, 1);
        assert!(queue.push_eager(&c, 2));
        for expected in [&a, &c] {
            assert!(Arc::ptr_eq(&queue.pop().unwrap(), expected));
        }
        assert!(queue.pop().is_none());
        assert_eq!(queue.eager, 0);
    }

    /// Many threads creating pipelines at once never queue more than the
    /// limit (Belobug #622's review test).
    #[test]
    fn concurrent_eager_pushes_hold_the_limit() {
        const LIMIT: u64 = 64;
        let queue = Mutex::new(Queue::new());
        let queued = AtomicU64::new(0);
        std::thread::scope(|s| {
            for _ in 0..8 {
                s.spawn(|| {
                    for _ in 0..200 {
                        let p = Arc::new(Pending::new(Vec::new(), None));
                        if queue.lock().unwrap().push_eager(&p, LIMIT) {
                            queued.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                });
            }
        });
        assert_eq!(queued.load(Ordering::Relaxed), LIMIT);
        assert_eq!(queue.lock().unwrap().eager, LIMIT);
    }

    #[test]
    fn workers_take_every_processor_but_four() {
        assert_eq!(worker_count(32, None), 28);
        assert_eq!(worker_count(16, None), 12);
        assert_eq!(worker_count(4, None), 2);
        assert_eq!(worker_count(64, None), 28);
        assert_eq!(worker_count(32, Some("6")), 6);
        assert_eq!(worker_count(32, Some("0")), 28);
        assert_eq!(worker_count(32, Some("x")), 28);
    }

    #[test]
    fn pipeline_worker_stack_size_reserves_64_mib() {
        assert_eq!(WORKER_STACK_BYTES, 64 << 20);
        // Verify the OS accepts a 64 MiB stack reservation and a 4 MiB local
        // frame (which would overflow the default 2 MiB Rust thread stack)
        // succeeds cleanly.
        let sum = std::thread::Builder::new()
            .name("victory-pipelines-stack-test".into())
            .stack_size(WORKER_STACK_BYTES)
            .spawn(|| {
                let buf = [1u8; 4 << 20];
                std::hint::black_box(&buf).iter().take(16).map(|&b| b as usize).sum::<usize>()
            })
            .expect("spawn 64 MiB stack worker")
            .join()
            .expect("4 MiB stack frame must not overflow 64 MiB worker stack");
        assert_eq!(sum, 16);
    }
}
