//! `amd_fidelityfx_loader_xbox.dll`: opt-in upscale dispatch hook
//! (`VICTORY_UPSCALE=stretch|dlss`, off by default).
//!
//! Ported from `Peppacodes/playablegames-peppa` (`b3c3766546`, from
//! `avatarmanz/VIctory-7900XTX` PR #203 by YeokiNoewChi and
//! `CuteBooty/victory-8gb`).
//!
//! When opted in (on Series S, or on Series X with `VICTORY_UPSCALER=7`),
//! `ffxDispatch` is taken here: an upscale dispatch becomes native
//! NVIDIA DLSS SR (`VICTORY_UPSCALE=dlss`, through the driver's NGX core and
//! an `nvngx_dlss.dll`) or a plain bilinear stretch (`VICTORY_UPSCALE=stretch`,
//! also the fallback if DLSS cannot run). Any other dispatch, and any upscale
//! we could not record, is forwarded to the title's own DLL.
//! `ffxQuery` is taken with it: the title's queries its own DLL fails (its
//! FSR 4 context refuses 4K) are answered here, see [`query`].
//!
//! The structures match the public FidelityFX API (`ffx_api.h`,
//! `ffx_upscale.h`): a header of a type and a next pointer, and resources of
//! a pointer, a 32-byte description and a state.

use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

/// The DLL whose `ffxDispatch` is taken when `VICTORY_UPSCALE` is enabled.
pub const LOADER: &str = "amd_fidelityfx_loader_xbox.dll";

/// Which host upscaler stands in for FidelityFX `ffxDispatch`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Stretch,
    Dlss,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stretch => "stretch",
            Self::Dlss => "dlss",
        }
    }
}

/// Parses a `VICTORY_UPSCALE` value (`stretch` or `dlss`, case-insensitive).
pub fn parse_mode(spec: &str) -> Option<Mode> {
    let trimmed = spec.trim();
    if trimmed.eq_ignore_ascii_case("stretch") {
        Some(Mode::Stretch)
    } else if trimmed.eq_ignore_ascii_case("dlss") {
        Some(Mode::Dlss)
    } else {
        None
    }
}

/// `VICTORY_UPSCALE=stretch|dlss`: `None` when unset or unrecognised (off by default).
pub fn asked_mode() -> Option<Mode> {
    let spec = std::env::var("VICTORY_UPSCALE").ok()?;
    parse_mode(&spec)
}

/// Whether the exe's import of `amd_fidelityfx_loader_xbox.dll!ffxDispatch`
/// should be intercepted. Off unless explicitly opted in via `VICTORY_UPSCALE`,
/// never conflicts with `VICTORY_FSR_DLL`, and never alters default Series X
/// rendering (which uses the title's own upscaler 6 unless `VICTORY_UPSCALER=7`).
pub fn should_take_dispatch(
    console_is_series_s: bool,
    asked_upscaler: Option<u32>,
    fsr_dll_enabled: bool,
    mode: Option<Mode>,
) -> bool {
    !fsr_dll_enabled && mode.is_some() && (console_is_series_s || asked_upscaler == Some(7))
}

static REAL: AtomicUsize = AtomicUsize::new(0);

/// The title's own `ffxDispatch`, for dispatches not handled here.
pub fn set_real_dispatch(address: usize) {
    REAL.store(address, Ordering::Relaxed);
}

#[repr(C)]
struct Header {
    kind: u64,
    next: *const Header,
}

#[repr(C)]
struct Resource {
    resource: *const c_void,
    description: [u32; 8],
    state: u32,
}

#[repr(C)]
struct Upscale {
    header: Header,
    list: *const c_void,
    colour: Resource,
    depth: Resource,
    motion: Resource,
    exposure: Resource,
    reactive: Resource,
    composition: Resource,
    output: Resource,
    jitter: [f32; 2],
    motion_scale: [f32; 2],
    render: [u32; 2],
    upscale: [u32; 2],
    // The rest of `ffxDispatchDescUpscale` (`ffx_upscale.h`).
    enable_sharpening: u8,
    sharpness: f32,
    frame_time_delta: f32,
    pre_exposure: f32,
    reset: u8,
    camera_near: f32,
    camera_far: f32,
    camera_fov: f32,
    view_to_meters: f32,
    flags: u32,
}

/// `VICTORY_UPSCALE_PROBE=1`: logs what the title gives each upscale (the
/// inputs a temporal upscaler such as DLSS or FSR 2 needs), now and then.
fn probe() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_UPSCALE_PROBE").is_some_and(|v| v == "1"))
}

const _: () = assert!(size_of::<Resource>() == 48);
const _: () = assert!(std::mem::offset_of!(Upscale, output) == 0x138);

/// `FFX_API_RETURN_ERROR`: what a dispatch nothing could answer returns.
const ERROR: u32 = 1;

/// `FFX_API_DISPATCH_DESC_TYPE_UPSCALE`.
const UPSCALE: u64 = 0x0001_0001;

type Dispatch = extern "system" fn(*mut *mut c_void, *const Header) -> u32;

extern "system" fn dispatch(context: *mut *mut c_void, desc: *const Header) -> u32 {
    // SAFETY: the title passes a dispatch description, whose header says
    // what follows it.
    if let Some(header) = unsafe { desc.as_ref() } {
        static SEEN: AtomicU32 = AtomicU32::new(0);
        let first = SEEN.fetch_add(1, Ordering::Relaxed) < 4;
        if header.kind == UPSCALE {
            // SAFETY: an upscale description, by its type.
            let d = unsafe { &*desc.cast::<Upscale>() };
            if first {
                tracing::info!(
                    list = ?d.list,
                    colour = ?d.colour.resource,
                    colour_size = ?(d.colour.description[2], d.colour.description[3]),
                    output = ?d.output.resource,
                    output_size = ?(d.output.description[2], d.output.description[3]),
                    render = ?d.render,
                    upscale = ?d.upscale,
                    "upscale dispatch"
                );
            }
            #[cfg(windows)]
            if probe() {
                static N: AtomicU32 = AtomicU32::new(0);
                let n = N.fetch_add(1, Ordering::Relaxed);
                // The first 12 upscales, then one in 300.
                if n < 12 || n.is_multiple_of(300) {
                    let inputs = victory_gpu::upscale::upscale_inputs(&[
                        d.colour.resource,
                        d.depth.resource,
                        d.motion.resource,
                        d.exposure.resource,
                        d.reactive.resource,
                        d.composition.resource,
                        d.output.resource,
                    ]);
                    tracing::info!(
                        n,
                        colour = %inputs[0],
                        depth = %inputs[1],
                        motion = %inputs[2],
                        exposure = %inputs[3],
                        reactive = %inputs[4],
                        composition = %inputs[5],
                        output = %inputs[6],
                        jitter = ?d.jitter,
                        motion_scale = ?d.motion_scale,
                        render = ?d.render,
                        upscale = ?d.upscale,
                        sharpening = d.enable_sharpening,
                        sharpness = d.sharpness,
                        frame_time_delta = d.frame_time_delta,
                        pre_exposure = d.pre_exposure,
                        reset = d.reset,
                        near = d.camera_near,
                        far = d.camera_far,
                        fov = d.camera_fov,
                        meters = d.view_to_meters,
                        flags = format_args!("{:#x}", d.flags),
                        "upscale probe"
                    );
                }
            }
            #[cfg(windows)]
            if victory_gpu::camera::on() {
                note_camera(d);
            }
            #[cfg(windows)]
            let live = live_mode();
            // (None: the hook is only observing for `VICTORY_CAMERA_CAPTURE`.)
            #[cfg(windows)]
            if live.is_some() && live != Some(victory_gpu::upscale_live::Live::Native) {
                let up = victory_gpu::upscale::Upscale {
                    colour: d.colour.resource as u64,
                    depth: d.depth.resource as u64,
                    motion: d.motion.resource as u64,
                    exposure: d.exposure.resource as u64,
                    output: d.output.resource as u64,
                    render: d.render,
                    jitter: d.jitter,
                    motion_scale: d.motion_scale,
                    reset: d.reset != 0 || mode_changed(),
                    pre_exposure: d.pre_exposure,
                    near: d.camera_near,
                    far: d.camera_far,
                    frame_ms: d.frame_time_delta,
                    dlss: live == Some(victory_gpu::upscale_live::Live::Dlss),
                    fov_y: d.camera_fov,
                    metres_per_unit: d.view_to_meters,
                };
                if victory_gpu::upscale::upscale(d.list, &up) {
                    return 0;
                }
            }
            if first {
                tracing::warn!("upscale dispatch not handled: left to the title's upscaler");
            }
        } else if first {
            tracing::info!(
                kind = format_args!("{:#x}", header.kind),
                "another FidelityFX dispatch"
            );
        }
    }
    match REAL.load(Ordering::Relaxed) {
        // Neither handled nor passed on: the output was not written, and
        // the title is told so instead of presenting it as it is.
        0 => {
            static TOLD: std::sync::Once = std::sync::Once::new();
            TOLD.call_once(|| {
                tracing::error!(
                    "a FidelityFX dispatch was not answered: no upscale was recorded and the title's own ffxDispatch was not found"
                );
            });
            ERROR
        }
        // SAFETY: the title's own `ffxDispatch`, with its own arguments.
        real => unsafe { std::mem::transmute::<usize, Dispatch>(real)(context, desc) },
    }
}

/// `VICTORY_CAMERA_CAPTURE=1`: the dispatch's inputs for the frame's camera
/// record (`victory_gpu::camera`); the output is the HUD-less scene.
#[cfg(windows)]
fn note_camera(d: &Upscale) {
    use victory_gpu::camera::format_of;
    victory_gpu::camera::note_dispatch(victory_gpu::camera::Dispatch {
        colour: d.colour.resource as u64,
        depth: d.depth.resource as u64,
        motion: d.motion.resource as u64,
        output: d.output.resource as u64,
        formats: [
            format_of(d.colour.resource as u64),
            format_of(d.depth.resource as u64),
            format_of(d.motion.resource as u64),
            format_of(d.output.resource as u64),
        ],
        jitter: d.jitter,
        motion_scale: d.motion_scale,
        render: d.render,
        upscale: d.upscale,
        near: d.camera_near,
        far: d.camera_far,
        fov: d.camera_fov,
        frame_ms: d.frame_time_delta,
        reset: d.reset != 0,
    });
}

/// `VICTORY_CAMERA_CAPTURE=1` installs the hook to observe the dispatch
/// (passed on to the title's DLL) where an upscale switch could take it.
pub fn should_observe_dispatch(
    console_is_series_s: bool,
    asked_upscaler: Option<u32>,
    fsr_dll_enabled: bool,
    camera_capture: bool,
) -> bool {
    camera_capture && !fsr_dll_enabled && (console_is_series_s || asked_upscaler == Some(7))
}

/// `VICTORY_CAMERA_CAPTURE=1`.
pub fn camera_capture() -> bool {
    std::env::var_os("VICTORY_CAMERA_CAPTURE").is_some_and(|v| v == "1")
}

/// The upscale to record: a live request (`victory_gpu::upscale_live`) over
/// `VICTORY_UPSCALE`. `Native` passes the dispatch to the title's own DLL.
#[cfg(windows)]
fn live_mode() -> Option<victory_gpu::upscale_live::Live> {
    use victory_gpu::upscale_live::Live;
    victory_gpu::upscale_live::mode().or_else(|| {
        asked_mode().map(|m| match m {
            Mode::Stretch => Live::Stretch,
            Mode::Dlss => Live::Dlss,
        })
    })
}

/// Whether the upscale mode changed since the last upscale recorded here:
/// the new upscaler starts without the old one's history.
#[cfg(windows)]
fn mode_changed() -> bool {
    static SEEN: AtomicU32 = AtomicU32::new(0);
    let now = victory_gpu::upscale_live::generation();
    SEEN.swap(now, Ordering::Relaxed) != now
}

/// What the title's import of `ffxDispatch` is bound to. The DLL is not an
/// HLE module: the title's own must load, for its other entry points and
/// for the dispatches passed on.
pub fn dispatch_address() -> usize {
    dispatch as *const () as usize
}

// --- `ffxQuery`, taken with `ffxDispatch` ---
//
// The title's upscaler DLL is a shipping FSR 4 build with only the
// fixed-size Quality tables (maxUpscaleSize 1920x1080, render 2/3 of it).
// At 4K output, or at another render scale, its `ffxCreateContext` refuses
// ("Shipping FSR4 configuration mismatch") and the title goes on with no
// context: its jitter phase count query (made right after creating the
// context, into a field left 0) fails, and after the first upscale it
// divides its frame index by that 0 (title+0x28f03e8, batch 4 part B; run
// res-6 on Series S at 0.5). With the upscale ours, the title's queries that
// fail are answered here with the public FSR formulas (`ffx_upscale.h`,
// FSR 2/3 `ffxFsr*GetJitterPhaseCount` / `GetJitterOffset`); answers its DLL
// gives are kept.

/// `FFX_API_QUERY_DESC_TYPE_UPSCALE_GETJITTERPHASECOUNT`.
const GET_JITTER_PHASE_COUNT: u64 = 0x0001_0004;
/// `FFX_API_QUERY_DESC_TYPE_UPSCALE_GETJITTEROFFSET`.
const GET_JITTER_OFFSET: u64 = 0x0001_0005;
/// `FFX_API_QUERY_DESC_TYPE_UPSCALE_GPU_MEMORY_USAGE`.
const GPU_MEMORY_USAGE: u64 = 0x0001_0008;

/// `ffxQueryDescUpscaleGetJitterPhaseCount`.
#[repr(C)]
struct PhaseCount {
    header: Header,
    render_width: u32,
    display_width: u32,
    out: *mut i32,
}

/// `ffxQueryDescUpscaleGetJitterOffset`.
#[repr(C)]
struct JitterOffset {
    header: Header,
    index: i32,
    phase_count: i32,
    out_x: *mut f32,
    out_y: *mut f32,
}

/// `ffxQueryDescUpscaleGetGPUMemoryUsage`: a pointer to two `u64`s.
#[repr(C)]
struct MemoryUsage {
    header: Header,
    out: *mut [u64; 2],
}

/// FSR's jitter phase count: 8 times the square of the upscale ratio, at
/// least 1 (the title divides by it).
pub fn jitter_phase_count(render_width: u32, display_width: u32) -> i32 {
    const BASE: f32 = 8.0;
    if render_width == 0 {
        return BASE as i32;
    }
    let ratio = display_width as f32 / render_width as f32;
    ((BASE * ratio * ratio) as i32).max(1)
}

/// The Halton sequence's `index`th value in `base`.
fn halton(mut index: i32, base: i32) -> f32 {
    let (mut f, mut result) = (1.0f32, 0.0f32);
    while index > 0 {
        f /= base as f32;
        result += f * (index % base) as f32;
        index /= base;
    }
    result
}

/// FSR's sub-pixel jitter for `index` of a sequence of `phase_count`
/// (Halton 2, 3, centred): pixels in -0.5..0.5.
pub fn jitter_offset(index: i32, phase_count: i32) -> (f32, f32) {
    let n = phase_count.max(1);
    let i = index.rem_euclid(n) + 1;
    (halton(i, 2) - 0.5, halton(i, 3) - 0.5)
}

static REAL_QUERY: AtomicUsize = AtomicUsize::new(0);

/// The title's own `ffxQuery`, asked first.
pub fn set_real_query(address: usize) {
    REAL_QUERY.store(address, Ordering::Relaxed);
}

type Query = extern "system" fn(*mut *mut c_void, *mut Header) -> u32;

extern "system" fn query(context: *mut *mut c_void, desc: *mut Header) -> u32 {
    let real = match REAL_QUERY.load(Ordering::Relaxed) {
        0 => ERROR,
        // SAFETY: the title's own `ffxQuery`, with its own arguments.
        q => unsafe { std::mem::transmute::<usize, Query>(q)(context, desc) },
    };
    // SAFETY: the title passes a query description, whose header says what
    // follows it.
    unsafe { answer(desc, real) }
}

/// The title's query `desc` after its DLL answered `real`: kept if that
/// answer is usable, else answered here (`FFX_API_RETURN_OK`).
///
/// # Safety
/// `desc` is null or a query description laid out as its type says, whose
/// out pointers are null or writable.
unsafe fn answer(desc: *mut Header, real: u32) -> u32 {
    // SAFETY: the caller's promise.
    let Some(header) = (unsafe { desc.as_ref() }) else {
        return real;
    };
    match header.kind {
        GET_JITTER_PHASE_COUNT => {
            // SAFETY: a phase count query, by its type.
            let q = unsafe { &*desc.cast::<PhaseCount>() };
            // SAFETY: null or the title's `int32_t`.
            let Some(out) = (unsafe { q.out.as_mut() }) else {
                return real;
            };
            if real == 0 && *out > 0 {
                return real;
            }
            *out = jitter_phase_count(q.render_width, q.display_width);
            static TOLD: std::sync::Once = std::sync::Once::new();
            TOLD.call_once(|| {
                tracing::warn!(
                    real,
                    render_width = q.render_width,
                    display_width = q.display_width,
                    phases = *out,
                    "FidelityFX: the title's jitter phase count query failed (its FSR 4 context refused this size); answered here"
                );
            });
            0
        }
        GET_JITTER_OFFSET if real != 0 => {
            // SAFETY: a jitter offset query, by its type.
            let q = unsafe { &*desc.cast::<JitterOffset>() };
            let (x, y) = jitter_offset(q.index, q.phase_count);
            // SAFETY: null or the title's floats.
            unsafe {
                if let Some(o) = q.out_x.as_mut() {
                    *o = x;
                }
                if let Some(o) = q.out_y.as_mut() {
                    *o = y;
                }
            }
            0
        }
        GPU_MEMORY_USAGE if real != 0 => {
            // SAFETY: a memory usage query, by its type; null or the title's two u64s.
            if let Some(o) = unsafe { (*desc.cast::<MemoryUsage>()).out.as_mut() } {
                *o = [0, 0];
            }
            0
        }
        _ => real,
    }
}

/// What the title's import of `ffxQuery` is bound to, with `ffxDispatch`.
pub fn query_address() -> usize {
    query as *const () as usize
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::offset_of;

    /// The upscale description as the public API lays it out: what the
    /// stretch/DLSS path reads sits where the title wrote it.
    #[test]
    fn the_upscale_description_is_laid_out_as_the_public_api() {
        assert_eq!(size_of::<Header>(), 16);
        assert_eq!(offset_of!(Upscale, list), 0x10);
        assert_eq!(offset_of!(Upscale, colour), 0x18);
        assert_eq!(offset_of!(Upscale, depth), 0x48);
        assert_eq!(offset_of!(Upscale, motion), 0x78);
        assert_eq!(offset_of!(Upscale, output), 0x138);
        assert_eq!(offset_of!(Upscale, jitter), 0x168);
        assert_eq!(offset_of!(Upscale, motion_scale), 0x170);
        assert_eq!(offset_of!(Upscale, render), 0x178);
        assert_eq!(offset_of!(Upscale, upscale), 0x180);
        assert_eq!(offset_of!(Upscale, enable_sharpening), 0x188);
        assert_eq!(offset_of!(Upscale, sharpness), 0x18c);
        assert_eq!(offset_of!(Upscale, reset), 0x198);
        assert_eq!(offset_of!(Upscale, camera_near), 0x19c);
        assert_eq!(offset_of!(Upscale, flags), 0x1ac);
        assert_eq!(offset_of!(Resource, description), 8);
        assert_eq!(offset_of!(Resource, state), 40);
    }

    #[test]
    fn parse_mode_and_opt_in_gating() {
        assert_eq!(parse_mode("stretch"), Some(Mode::Stretch));
        assert_eq!(parse_mode("  DLSS "), Some(Mode::Dlss));
        assert_eq!(parse_mode(""), None);
        assert_eq!(parse_mode("off"), None);
        assert_eq!(parse_mode("fsr"), None);

        // Off by default on both Series X and Series S.
        assert!(!should_take_dispatch(false, None, false, None));
        assert!(!should_take_dispatch(true, None, false, None));

        // Default native Series X rendering is never intercepted without VICTORY_UPSCALER=7.
        assert!(!should_take_dispatch(false, None, false, Some(Mode::Dlss)));
        assert!(!should_take_dispatch(
            false,
            Some(6),
            false,
            Some(Mode::Stretch)
        ));

        // Opt-in on Series S or Series X with VICTORY_UPSCALER=7.
        assert!(should_take_dispatch(true, None, false, Some(Mode::Stretch)));
        assert!(should_take_dispatch(true, None, false, Some(Mode::Dlss)));
        assert!(should_take_dispatch(
            false,
            Some(7),
            false,
            Some(Mode::Dlss)
        ));

        // Never conflicts with VICTORY_FSR_DLL redirect.
        assert!(!should_take_dispatch(true, None, true, Some(Mode::Dlss)));
        assert!(!should_take_dispatch(
            false,
            Some(7),
            true,
            Some(Mode::Stretch)
        ));
    }

    #[test]
    fn camera_capture_observes_only_where_the_dispatch_can_be_hooked() {
        assert!(!should_observe_dispatch(false, None, false, true));
        assert!(!should_observe_dispatch(true, None, false, false));
        assert!(should_observe_dispatch(true, None, false, true));
        assert!(should_observe_dispatch(false, Some(7), false, true));
        assert!(!should_observe_dispatch(false, Some(7), true, true));
    }

    /// A dispatch that is not an upscale, with no DLL of the title's to
    /// pass it to, is an error and not a silent success; when `REAL` is set,
    /// non-upscale dispatches forward to it.
    #[test]
    fn unhandled_dispatch_errors_or_forwards_to_real() {
        extern "system" fn fake_real(_: *mut *mut c_void, _: *const Header) -> u32 {
            42
        }
        let before = REAL.swap(0, Ordering::Relaxed);
        let other = Header {
            kind: 0x0002_0001,
            next: std::ptr::null(),
        };
        assert_eq!(dispatch(std::ptr::null_mut(), &other), ERROR);
        assert_eq!(dispatch(std::ptr::null_mut(), std::ptr::null()), ERROR);

        set_real_dispatch(fake_real as *const () as usize);
        assert_eq!(dispatch(std::ptr::null_mut(), &other), 42);
        REAL.store(before, Ordering::Relaxed);
    }

    #[test]
    fn query_layouts_are_the_public_apis() {
        assert_eq!(offset_of!(PhaseCount, render_width), 0x10);
        assert_eq!(offset_of!(PhaseCount, display_width), 0x14);
        assert_eq!(offset_of!(PhaseCount, out), 0x18);
        assert_eq!(offset_of!(JitterOffset, index), 0x10);
        assert_eq!(offset_of!(JitterOffset, phase_count), 0x14);
        assert_eq!(offset_of!(JitterOffset, out_x), 0x18);
        assert_eq!(offset_of!(JitterOffset, out_y), 0x20);
        assert_eq!(offset_of!(MemoryUsage, out), 0x10);
    }

    #[test]
    fn the_formulas_are_fsrs() {
        // 4K from 2/3 (what batch 4 ran): 8 * 1.4994^2.
        assert_eq!(jitter_phase_count(2561, 3840), 17);
        assert_eq!(jitter_phase_count(1280, 1920), 18);
        assert_eq!(jitter_phase_count(1920, 1920), 8);
        assert_eq!(jitter_phase_count(3840, 3840), 8);
        assert!(jitter_phase_count(0, 3840) > 0);
        assert!(jitter_phase_count(3840, 0) > 0);
        // Halton(1, 2) = 1/2, Halton(1, 3) = 1/3; index wraps at the count.
        assert_eq!(jitter_offset(0, 8), (0.0, 1.0 / 3.0 - 0.5));
        assert_eq!(jitter_offset(8, 8), jitter_offset(0, 8));
        assert_eq!(jitter_offset(1, 8), (0.25 - 0.5, 2.0 / 3.0 - 0.5));
        assert_eq!(jitter_offset(5, 0), jitter_offset(0, 1));
        for i in 0..64 {
            let (x, y) = jitter_offset(i, 17);
            assert!((-0.5..0.5).contains(&x) && (-0.5..0.5).contains(&y));
        }
    }

    /// Batch 4 part B: the title's DLL refused the 4K context, its phase
    /// count query failed and left the title's field 0, which it then divided
    /// by. Through our `ffxQuery` the field is the FSR count, never 0.
    #[test]
    fn a_failed_phase_count_query_is_answered_and_never_zero() {
        extern "system" fn refusing(_: *mut *mut c_void, _: *mut Header) -> u32 {
            ERROR
        }
        extern "system" fn zero_ok(_: *mut *mut c_void, desc: *mut Header) -> u32 {
            // SAFETY: the test's phase count query.
            unsafe { *(*desc.cast::<PhaseCount>()).out = 0 };
            0
        }
        extern "system" fn answers(_: *mut *mut c_void, desc: *mut Header) -> u32 {
            // SAFETY: the test's query.
            unsafe {
                match (*desc).kind {
                    GET_JITTER_PHASE_COUNT => *(*desc.cast::<PhaseCount>()).out = 99,
                    _ => *(*desc.cast::<JitterOffset>()).out_x = 0.125,
                }
            }
            0
        }
        let before = REAL_QUERY.swap(0, Ordering::Relaxed);
        let mut field = 0i32;
        let mut q = PhaseCount {
            header: Header {
                kind: GET_JITTER_PHASE_COUNT,
                next: std::ptr::null(),
            },
            render_width: 2561,
            display_width: 3840,
            out: &mut field,
        };
        let ask = |q: &mut PhaseCount| query(std::ptr::null_mut(), (q as *mut PhaseCount).cast());
        for real in [
            0,
            refusing as *const () as usize,
            zero_ok as *const () as usize,
        ] {
            field = 0;
            REAL_QUERY.store(real, Ordering::Relaxed);
            assert_eq!(ask(&mut q), 0);
            assert_eq!(field, 17, "the title's 0 is replaced");
            // The title's next step: its frame index modulo the count.
            assert_eq!(5u32 % field as u32, 5);
        }
        REAL_QUERY.store(answers as *const () as usize, Ordering::Relaxed);
        assert_eq!(ask(&mut q), 0);
        assert_eq!(field, 99, "a usable answer of the title's DLL is kept");

        let (mut x, mut y) = (f32::NAN, f32::NAN);
        let mut j = JitterOffset {
            header: Header {
                kind: GET_JITTER_OFFSET,
                next: std::ptr::null(),
            },
            index: 1,
            phase_count: 17,
            out_x: &mut x,
            out_y: &mut y,
        };
        REAL_QUERY.store(refusing as *const () as usize, Ordering::Relaxed);
        assert_eq!(query(std::ptr::null_mut(), (&raw mut j).cast()), 0);
        assert_eq!((x, y), jitter_offset(1, 17));
        REAL_QUERY.store(answers as *const () as usize, Ordering::Relaxed);
        assert_eq!(query(std::ptr::null_mut(), (&raw mut j).cast()), 0);
        assert_eq!(x, 0.125);

        let mut usage = [7u64, 7];
        let mut m = MemoryUsage {
            header: Header {
                kind: GPU_MEMORY_USAGE,
                next: std::ptr::null(),
            },
            out: &mut usage,
        };
        REAL_QUERY.store(refusing as *const () as usize, Ordering::Relaxed);
        assert_eq!(query(std::ptr::null_mut(), (&raw mut m).cast()), 0);
        assert_eq!(usage, [0, 0], "not left as stack garbage");

        // Another query is the DLL's answer, as it gave it.
        let mut other = Header {
            kind: 0x0001_0002,
            next: std::ptr::null(),
        };
        assert_eq!(query(std::ptr::null_mut(), &mut other), ERROR);
        REAL_QUERY.store(before, Ordering::Relaxed);
    }
}
