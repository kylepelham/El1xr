//! `amd_fidelityfx_loader_xbox.dll`: opt-in upscale dispatch hook
//! (`VICTORY_UPSCALE=stretch|dlss`, off by default).
//!
//! Ported from `Peppacodes/playablegames-peppa` (`b3c3766546`, from
//! `avatarmanz/VIctory-7900XTX` PR #203 by YeokiNoewChi and
//! `CuteBooty/victory-8gb`).
//!
//! When opted in (on Series S, or on Series X with `VICTORY_UPSCALER=7`),
//! `ffxDispatch` alone is taken here: an upscale dispatch becomes native
//! NVIDIA DLSS SR (`VICTORY_UPSCALE=dlss`, through the driver's NGX core and
//! an `nvngx_dlss.dll`) or a plain bilinear stretch (`VICTORY_UPSCALE=stretch`,
//! also the fallback if DLSS cannot run). Any other dispatch, and any upscale
//! we could not record, is forwarded to the title's own DLL.
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
            let live = live_mode();
            #[cfg(windows)]
            if live != Some(victory_gpu::upscale_live::Live::Native) {
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
}
