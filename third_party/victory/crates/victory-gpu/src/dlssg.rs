//! NVIDIA DLSS Frame Generation over our presents (`VICTORY_FRAME_GEN=dlss`,
//! off by default), through Streamline (`streamline.rs`; MIT, SDK 2.14.16,
//! DLLs from `VICTORY_STREAMLINE_DIR`, none in the repository).
//!
//! Streamline is used with manual hooking (`eUseManualHooking`), not as the
//! interposer of our device: the host device, its queue and every other
//! D3D12 object stay native (made before Streamline is loaded, and used by
//! NGX, FidelityFX and our own code by raw pointer), and only the window's
//! swap chain is Streamline's proxy, made from a factory upgraded with
//! `slUpgradeInterface`. `Present`, `GetBuffer`, `ResizeBuffers` and
//! `GetCurrentBackBufferIndex` (the calls DLSS-G hooks) go to that proxy
//! through the host's swap chain as before.
//!
//! The pieces, in the order they run:
//! 1. [`create_swap_chain`] (first present): `slInit`, `slSetD3DDevice`,
//!    `slIsFeatureSupported(DLSS-G)` on the device's adapter, Reflex in
//!    low-latency mode, the proxy swap chain, DLSS-G options (one generated
//!    frame). Any failure: Streamline is shut down again, the plain swap chain
//!    is made and a `DLSS-G:` line says why.
//! 2. [`tag_inputs`], after each upscale: the title's depth and motion vectors
//!    tagged for this frame (copied by Streamline on the upscale's list), and
//!    the upscale's jitter, motion-vector scale, near/far and reset kept for
//!    the constants. Without an upscale hook (the default Series X path, no
//!    `VICTORY_UPSCALER=7`), [`tag_from_camera`] at the title's UI composite
//!    (on Series X 1.0.2.1 the first Scaleform draw onto the target
//!    `PS_TAA2_SpatialUpscaleWithDithering` wrote) tags the depth and motion
//!    of `PS_VelocityFill` the camera job noted, and the constants take the
//!    camera's jitter and motion-vector scale ([`frame_upscale`]).
//!    `VICTORY_DLSSG_EVERY_FRAME=1` also tags at the bind of that scene pass
//!    and holds the last velocity textures and valid camera for a few frames
//!    ([`crate::camera_tag`]): the UI draw comes only with the HUD.
//! 3. [`before_present`] / [`after_present`] (`pacing.rs`): the frame's
//!    constants from `camera::latest()` (`VICTORY_CAMERA_CAPTURE=1`), DLSS-G
//!    switched off for frames without a valid camera (menus, loading) or
//!    without this frame's depth and motion vectors, and on
//!    again with a reset, the backbuffer tag, the PC-latency markers around
//!    `Present`, then the next frame's token and `slReflexSleep`
//!    (`VICTORY_DLSSG_SLEEP=0` skips the sleep).
//!
//! With `VICTORY_CAMERA_HUDLESS=1` the camera job's copy of the picture just
//! before the title's UI blit (`camera_copy::hudless`) is tagged as the
//! HUD-less colour, so DLSS-G keeps the HUD out of the interpolation (it finds
//! the UI from the HUD-less picture and the back buffer). Without it the HUD
//! is interpolated with the scene.

use std::ffi::c_void;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, Ordering};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::core::Interface;

use crate::camera::{FrameCamera, Mat4};
use crate::streamline::{self as sl, Api};

/// The D3D12 state the upscale leaves depth and motion vectors in
/// (`NON_PIXEL_SHADER_RESOURCE`, `upscale.rs`'s `SHADER_READ`).
const SHADER_READ: u32 = 0x40;
/// The state the HUD-less copy rests in (`camera_copy::HUDLESS_STATE`).
const HUDLESS_STATE: u32 = crate::camera_copy::HUDLESS_STATE.0 as u32;
/// How often the `DLSS-G: window` line is written (real presents, those with
/// DLSS-G on, and `frames_per_present`: frames shown for the last real one,
/// 2 with one generated frame).
const WINDOW: Duration = Duration::from_secs(5);
/// The project id DLSS is started with (`ngx.rs`).
const PROJECT_ID: &std::ffi::CStr = c"5e2f9a3c-1b7d-4c8e-9a6f-0d3b2c1e4f57";

/// What the upscale gave this frame, for the constants.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Upscaled {
    pub render: [u32; 2],
    pub jitter: [f32; 2],
    pub motion_scale: [f32; 2],
    pub near: f32,
    pub far: f32,
    pub reset: bool,
}

struct Session {
    api: &'static Api,
    set_options: sl::DlssgSetOptions,
    get_state: sl::DlssgGetState,
    reflex_sleep: sl::ReflexSleep,
    set_marker: sl::PclSetMarker,
    options: sl::DlssgOptions,
    /// Generating now (mode on).
    on: bool,
    reset_next: bool,
    last: Option<Upscaled>,
    /// An upscale tagged this frame's depth and motion.
    upscaled_now: bool,
    /// [`tag_from_camera`] tagged them this frame, at this render size.
    camera_render: Option<[u32; 2]>,
    told_camera: bool,
    switches: u32,
    real: u64,
    /// Presents with DLSS-G on in this window.
    generating: u64,
    /// Frames of this window whose depth and motion came from an upscale,
    /// from the camera.
    upscale_tags: u64,
    camera_tags: u64,
    /// Camera tags of textures noted in an earlier frame, presents whose
    /// constants took an earlier frame's camera (`VICTORY_DLSSG_EVERY_FRAME`).
    held_tags: u64,
    held_cameras: u64,
    cameras: crate::camera_tag::CameraHold,
    /// Why DLSS-G was last off (for the window line).
    off_why: String,
    since: Instant,
    told_constants: bool,
    told_tags: bool,
    told_hudless: bool,
}

// SAFETY: Streamline's functions and our plain data, used under the mutex.
unsafe impl Send for Session {}

static SESSION: Mutex<Option<Session>> = Mutex::new(None);
static ACTIVE: AtomicBool = AtomicBool::new(false);
/// The current frame's index and token (`slGetNewFrameToken`).
static FRAME: AtomicU32 = AtomicU32::new(0);
static TOKEN: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

/// Whether DLSS-G's proxy swap chain presents.
pub(crate) fn active() -> bool {
    ACTIVE.load(Ordering::Relaxed)
}

fn viewport() -> sl::ViewportHandle {
    sl::ViewportHandle::new(0)
}

/// DLSS's `VICTORY_DLSS_MV_SIGN` and `VICTORY_DLSS_JITTER_SIGN` (`dlss.rs`),
/// shared so one flip fixes both.
fn signs() -> (f32, f32) {
    static SIGNS: std::sync::OnceLock<(f32, f32)> = std::sync::OnceLock::new();
    *SIGNS.get_or_init(|| {
        let sign = |name: &str| {
            std::env::var(name)
                .ok()
                .and_then(|v| v.trim().parse::<f32>().ok())
                .map_or(1.0, |v| if v < 0.0 { -1.0 } else { 1.0 })
        };
        (sign("VICTORY_DLSS_MV_SIGN"), sign("VICTORY_DLSS_JITTER_SIGN"))
    })
}

fn sleep_on() -> bool {
    std::env::var_os("VICTORY_DLSSG_SLEEP").is_none_or(|v| v != "0")
}

/// Generated frames per real one: `VICTORY_DLSSG_FRAMES` (1 to 3), default 1.
fn frames_to_generate() -> u32 {
    std::env::var("VICTORY_DLSSG_FRAMES")
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .map_or(1, |n| n.clamp(1, 3))
}

unsafe extern "C" fn log_message(ty: u32, msg: *const std::ffi::c_char) {
    if msg.is_null() {
        return;
    }
    // SAFETY: Streamline passes a NUL-terminated message.
    let text = unsafe { std::ffi::CStr::from_ptr(msg) }.to_string_lossy();
    let text = text.trim_end();
    match ty {
        0 => tracing::debug!("Streamline: {text}"),
        1 => tracing::warn!("Streamline: {text}"),
        _ => tracing::error!("Streamline: {text}"),
    }
}

/// The window's swap chain as Streamline's DLSS-G proxy; `None` (with a
/// `DLSS-G:` warning) makes the plain one.
pub(crate) fn create_swap_chain(
    factory: &IDXGIFactory2,
    device: &ID3D12Device,
    queue: &ID3D12CommandQueue,
    hwnd: HWND,
    desc: &DXGI_SWAP_CHAIN_DESC1,
) -> Option<IDXGISwapChain3> {
    if !crate::camera::on() {
        tracing::warn!(
            "DLSS-G: VICTORY_CAMERA_CAPTURE=1 is not set; without camera constants no frame is generated"
        );
    }
    let api = match sl::api() {
        Ok(a) => a,
        Err(why) => {
            tracing::warn!(
                why,
                "DLSS-G: Streamline not loaded (VICTORY_STREAMLINE_DIR); frame generation stays off"
            );
            return None;
        }
    };
    match start(api, factory, device, queue, hwnd, desc) {
        Ok(chain) => Some(chain),
        Err(why) => {
            tracing::warn!("DLSS-G: {why}; frame generation stays off, presenting without it");
            None
        }
    }
}

fn wide(p: &std::path::Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    p.as_os_str().encode_wide().chain([0]).collect()
}

fn start(
    api: &'static Api,
    factory: &IDXGIFactory2,
    device: &ID3D12Device,
    queue: &ID3D12CommandQueue,
    hwnd: HWND,
    desc: &DXGI_SWAP_CHAIN_DESC1,
) -> Result<IDXGISwapChain3, String> {
    let plugins = api
        .path
        .parent()
        .ok_or("the interposer has no folder")?
        .to_path_buf();
    let logs = std::env::temp_dir().join("victory-streamline");
    let _ = std::fs::create_dir_all(&logs);
    let (plugins_w, logs_w) = (wide(&plugins), wide(&logs));
    let paths = [plugins_w.as_ptr()];
    let features = [sl::FEATURE_DLSS_G, sl::FEATURE_REFLEX, sl::FEATURE_PCL];
    let mut pref = sl::Preferences::new();
    pref.paths_to_plugins = paths.as_ptr();
    pref.num_paths_to_plugins = 1;
    pref.path_to_logs_and_data = logs_w.as_ptr();
    pref.log_message_callback = Some(log_message);
    pref.flags = sl::PREF_DISABLE_CL_STATE_TRACKING
        | sl::PREF_USE_MANUAL_HOOKING
        | sl::PREF_USE_FRAME_BASED_RESOURCE_TAGGING;
    pref.features_to_load = features.as_ptr();
    pref.num_features_to_load = features.len() as u32;
    pref.engine_version = c"1.0".as_ptr();
    pref.project_id = PROJECT_ID.as_ptr();
    // SAFETY: preferences and the strings they point at live through the call.
    let r = unsafe { (api.init)(&pref, sl::SDK_VERSION) };
    if r != sl::OK {
        return Err(format!("slInit refused: {}", sl::result_name(r)));
    }
    match start_after_init(api, factory, device, queue, hwnd, desc) {
        Ok(chain) => Ok(chain),
        Err(why) => {
            // SAFETY: Streamline was started above and nothing of it is kept.
            unsafe { (api.shutdown)() };
            Err(why)
        }
    }
}

fn check(what: &str, r: i32) -> Result<(), String> {
    if r == sl::OK {
        Ok(())
    } else {
        Err(format!("{what}: {}", sl::result_name(r)))
    }
}

fn start_after_init(
    api: &'static Api,
    factory: &IDXGIFactory2,
    device: &ID3D12Device,
    queue: &ID3D12CommandQueue,
    hwnd: HWND,
    desc: &DXGI_SWAP_CHAIN_DESC1,
) -> Result<IDXGISwapChain3, String> {
    // SAFETY: Streamline calls with live objects and out structures.
    unsafe {
        check("slSetD3DDevice", (api.set_d3d_device)(device.as_raw()))?;
        let luid = device.GetAdapterLuid();
        let mut bytes = [0u8; 8];
        bytes[..4].copy_from_slice(&luid.LowPart.to_le_bytes());
        bytes[4..].copy_from_slice(&luid.HighPart.to_le_bytes());
        let adapter = sl::AdapterInfo::luid(&bytes);
        check(
            "DLSS-G not supported here (slIsFeatureSupported)",
            (api.is_feature_supported)(sl::FEATURE_DLSS_G, &adapter),
        )?;
        let mut loaded = false;
        let _ = (api.is_feature_loaded)(sl::FEATURE_DLSS_G, &mut loaded);
        let mut version = sl::FeatureVersion::new();
        let _ = (api.get_feature_version)(sl::FEATURE_DLSS_G, &mut version);
        let set_options: sl::DlssgSetOptions =
            std::mem::transmute(api.feature_function(sl::FEATURE_DLSS_G, c"slDLSSGSetOptions")?);
        let get_state: sl::DlssgGetState =
            std::mem::transmute(api.feature_function(sl::FEATURE_DLSS_G, c"slDLSSGGetState")?);
        let reflex_set: sl::ReflexSetOptions =
            std::mem::transmute(api.feature_function(sl::FEATURE_REFLEX, c"slReflexSetOptions")?);
        let reflex_sleep: sl::ReflexSleep =
            std::mem::transmute(api.feature_function(sl::FEATURE_REFLEX, c"slReflexSleep")?);
        let set_marker: sl::PclSetMarker =
            std::mem::transmute(api.feature_function(sl::FEATURE_PCL, c"slPCLSetMarker")?);
        check(
            "slReflexSetOptions",
            reflex_set(&sl::ReflexOptions::new(sl::REFLEX_LOW_LATENCY)),
        )?;
        let mut raw = factory.clone().into_raw();
        let r = (api.upgrade_interface)(&mut raw);
        let proxy_factory = IDXGIFactory2::from_raw(raw);
        check("slUpgradeInterface(factory)", r)?;
        let chain = proxy_factory
            .CreateSwapChainForHwnd(queue, hwnd, desc, None, None)
            .map_err(|e| format!("the proxy factory refused the swap chain: {e}"))?
            .cast::<IDXGISwapChain3>()
            .map_err(|e| format!("the proxy swap chain is not IDXGISwapChain3: {e}"))?;
        let mut options = sl::DlssgOptions::new();
        options.mode = sl::DLSSG_MODE_OFF;
        options.num_frames_to_generate = frames_to_generate();
        options.flags = sl::DLSSG_FLAG_RETAIN_RESOURCES_WHEN_OFF;
        options.num_back_buffers = desc.BufferCount;
        options.color_width = desc.Width;
        options.color_height = desc.Height;
        options.color_buffer_format = desc.Format.0 as u32;
        let mut token: sl::FrameToken = std::ptr::null();
        check(
            "slGetNewFrameToken",
            (api.get_new_frame_token)(&mut token, &0),
        )?;
        let frames = options.num_frames_to_generate;
        FRAME.store(0, Ordering::Relaxed);
        TOKEN.store(token.cast_mut(), Ordering::Relaxed);
        *SESSION.lock().unwrap_or_else(|e| e.into_inner()) = Some(Session {
            api,
            set_options,
            get_state,
            reflex_sleep,
            set_marker,
            options,
            on: false,
            reset_next: true,
            last: None,
            upscaled_now: false,
            camera_render: None,
            told_camera: false,
            switches: 0,
            real: 0,
            generating: 0,
            upscale_tags: 0,
            camera_tags: 0,
            held_tags: 0,
            held_cameras: 0,
            cameras: crate::camera_tag::CameraHold::default(),
            off_why: String::new(),
            since: Instant::now(),
            told_constants: false,
            told_tags: false,
            told_hudless: false,
        });
        ACTIVE.store(true, Ordering::Relaxed);
        tracing::info!(
            dll = %api.path.display(),
            width = desc.Width,
            height = desc.Height,
            format = desc.Format.0,
            loaded,
            sl = ?(version.sl.major, version.sl.minor, version.sl.build),
            ngx = ?(version.ngx.major, version.ngx.minor, version.ngx.build),
            frames_to_generate = frames,
            "DLSS-G: Streamline proxy swap chain (VICTORY_FRAME_GEN=dlss, manual hooking, Reflex low latency)"
        );
        Ok(chain)
    }
}

fn texture_tag(
    resource: &mut sl::Resource,
    ty: u32,
    lifecycle: u32,
    extent: sl::Extent,
) -> sl::ResourceTag {
    sl::ResourceTag::new(resource, ty, lifecycle, extent)
}

/// Tags the title's depth and motion vectors for this frame (copied now on
/// `list`, where they are readable by shaders) and keeps the upscale's
/// values for the constants. Nothing without the proxy swap chain.
pub(crate) fn tag_inputs(list: &ID3D12GraphicsCommandList, inputs: &crate::framegen::Inputs<'_>) {
    if !active() {
        return;
    }
    let mut guard = SESSION.lock().unwrap_or_else(|e| e.into_inner());
    let Some(s) = guard.as_mut() else {
        return;
    };
    s.last = Some(Upscaled {
        render: inputs.render,
        jitter: inputs.jitter,
        motion_scale: inputs.motion_scale,
        near: inputs.near,
        far: inputs.far,
        reset: inputs.reset,
    });
    let (Some(depth), Some(motion)) = (inputs.depth, inputs.motion) else {
        return;
    };
    s.upscaled_now = true;
    tag_depth_motion(s, list, depth, motion, inputs.render);
}

/// Without an upscale this frame: tags the velocity pass's depth and motion
/// vectors (`camera_copy::tag_textures`, `VICTORY_CAMERA_CAPTURE=1`), moved
/// to a shader-read state on `list` (at `point`, which runs after the
/// velocity pass) for Streamline's copy and back. Once a frame.
pub(crate) fn tag_from_camera(list: &crate::host::HostList, point: crate::camera_tag::Point) {
    if !active() {
        return;
    }
    let mut guard = SESSION.lock().unwrap_or_else(|e| e.into_inner());
    let Some(s) = guard.as_mut() else {
        return;
    };
    if s.upscaled_now || s.camera_render.is_some() {
        return;
    }
    let Some(((depth, motion), age)) = crate::camera_copy::tag_textures(point) else {
        return;
    };
    s.held_tags += u64::from(age > 0);
    // SAFETY: a live host texture.
    let desc = unsafe { motion.texture.GetDesc() };
    let render = [desc.Width as u32, desc.Height];
    let back = [
        crate::camera_copy::move_to(list, &depth, SHADER_READ),
        crate::camera_copy::move_to(list, &motion, SHADER_READ),
    ];
    tag_depth_motion(s, &list.list, &depth.texture, &motion.texture, render);
    for (t, b) in [&depth, &motion].into_iter().zip(back) {
        if let Some(before) = b {
            crate::camera_copy::move_to(list, t, before);
        }
    }
    s.camera_render = Some(render);
    if !s.told_camera {
        s.told_camera = true;
        // SAFETY: a live host texture.
        let d = unsafe { depth.texture.GetDesc() };
        let depth_size = [d.Width as u32, d.Height];
        tracing::info!(
            render = ?render,
            motion_format = desc.Format.0,
            depth_format = d.Format.0,
            depth_size = ?depth_size,
            "DLSS-G: depth and motion vectors from the camera's velocity pass (no upscale hook)"
        );
        if depth_size != render {
            tracing::warn!(
                ?depth_size,
                motion_size = ?render,
                "DLSS-G: the camera's depth and motion vectors differ in size; tagged at the motion size"
            );
        }
    }
}

/// The constants' upscale values from the camera when [`tag_from_camera`]
/// tagged this frame: motion in render pixels (previous minus current, as
/// DLSS-G wants) scaled to UV by the camera's scale, its jitter, reversed
/// depth (near 1).
pub(crate) fn from_camera(cam: &FrameCamera, render: [u32; 2]) -> Option<Upscaled> {
    let d = cam.camera.as_ref()?;
    let scale = |i: usize| cam.mv_scale[i] * render[i] as f32;
    Some(Upscaled {
        render,
        jitter: cam.jitter_px,
        motion_scale: [scale(0), scale(1)],
        // Reversed depth: `constants` reads near > far as inverted.
        near: d.near.max(d.far),
        far: d.near.min(d.far),
        reset: false,
    })
}

/// Why DLSS-G is off when nothing gave this frame depth and motion vectors.
const NOTHING_TAGGED: &str = "no upscale and no camera depth/motion tagged this frame (velocity pass, or the first UI draw after PS_TAA2_SpatialUpscale, not seen)";

/// The constants' upscale values for this frame: the upscale's when it
/// tagged this frame's depth and motion, else the camera's when
/// [`tag_from_camera`] did (`render`: the size it tagged), else none: an
/// earlier frame's values without this frame's tags would make DLSS-G
/// generate from inputs Streamline does not have.
pub(crate) fn frame_upscale(
    upscaled_now: bool,
    camera_render: Option<[u32; 2]>,
    last: Option<&Upscaled>,
    cam: Option<&FrameCamera>,
) -> Option<Upscaled> {
    if upscaled_now {
        return last.copied();
    }
    from_camera(cam?, camera_render?)
}

/// Tags `depth` and `motion` (in [`SHADER_READ`] on `list`) for this frame.
fn tag_depth_motion(
    s: &mut Session,
    list: &ID3D12GraphicsCommandList,
    depth: &ID3D12Resource,
    motion: &ID3D12Resource,
    render: [u32; 2],
) {
    let extent = sl::Extent {
        top: 0,
        left: 0,
        width: render[0],
        height: render[1],
    };
    let mut d = sl::Resource::texture(depth.as_raw(), SHADER_READ);
    let mut m = sl::Resource::texture(motion.as_raw(), SHADER_READ);
    let tags = [
        texture_tag(
            &mut d,
            sl::BUFFER_DEPTH,
            sl::LIFECYCLE_ONLY_VALID_NOW,
            extent,
        ),
        texture_tag(
            &mut m,
            sl::BUFFER_MOTION_VECTORS,
            sl::LIFECYCLE_ONLY_VALID_NOW,
            extent,
        ),
    ];
    let token = TOKEN.load(Ordering::Relaxed);
    // SAFETY: live resources and list; the tags are copied by Streamline.
    let r = unsafe {
        (s.api.set_tag_for_frame)(
            token,
            &viewport(),
            tags.as_ptr(),
            tags.len() as u32,
            list.as_raw(),
        )
    };
    if r != sl::OK && !s.told_tags {
        s.told_tags = true;
        tracing::warn!(
            result = sl::result_name(r),
            "DLSS-G: depth and motion vectors not tagged"
        );
    }
}

/// A column-vector matrix (`camera.rs`) as Streamline's row-vector one.
fn transpose(m: &Mat4) -> sl::Float4x4 {
    let mut t = [[0.0; 4]; 4];
    for (r, row) in m.iter().enumerate() {
        for (c, v) in row.iter().enumerate() {
            t[c][r] = *v;
        }
    }
    t
}

/// The frame's constants, or why there are none.
pub(crate) fn constants(
    cam: Option<&FrameCamera>,
    up: Option<&Upscaled>,
) -> Result<sl::Constants, String> {
    let cam = cam.ok_or("no camera (VICTORY_CAMERA_CAPTURE=1)")?;
    if !cam.valid {
        return Err(format!("camera not valid: {}", cam.why));
    }
    let d = cam.camera.as_ref().ok_or("no camera matrices")?;
    let up = up.ok_or(NOTHING_TAGGED)?;
    if up.render[0] == 0 || up.render[1] == 0 {
        return Err("no render size".into());
    }
    // Streamline wants the projection without the jitter (given apart).
    let mut proj = d.proj;
    proj[0][2] = 0.0;
    proj[1][2] = 0.0;
    let view_proj = crate::camera::mul(&proj, &d.view);
    let inv = crate::camera::inverse(&view_proj).ok_or("singular view-projection")?;
    let clip_to_prev = crate::camera::mul(&d.prev_view_proj, &inv);
    let prev_to_clip = crate::camera::inverse(&clip_to_prev).ok_or("singular clip-to-previous")?;
    let proj_inv = crate::camera::inverse(&proj).ok_or("singular projection")?;
    let finite = |v: f32| v.is_finite() && v > 0.0;
    let (near, far) = (up.near.min(up.far), up.near.max(up.far));
    let (near, far) = (
        if finite(near) {
            near
        } else {
            d.near.min(d.far)
        },
        if finite(far) { far } else { 1.0e5 },
    );
    let scale = |s: f32, n: u32| if s == 0.0 { 1.0 } else { s / n as f32 };
    let mut c = sl::Constants::new();
    c.camera_view_to_clip = transpose(&proj);
    c.clip_to_camera_view = transpose(&proj_inv);
    c.clip_to_prev_clip = transpose(&clip_to_prev);
    c.prev_clip_to_clip = transpose(&prev_to_clip);
    c.jitter_offset = up.jitter;
    c.mvec_scale = [
        scale(up.motion_scale[0], up.render[0]),
        scale(up.motion_scale[1], up.render[1]),
    ];
    c.camera_pos = [0.0; 3];
    c.camera_up = d.up;
    c.camera_right = d.right;
    c.camera_fwd = d.forward;
    c.camera_near = near;
    c.camera_far = far;
    c.camera_fov = d.fov_y;
    c.camera_aspect_ratio = d.aspect;
    c.depth_inverted = if up.near > up.far {
        sl::TRUE
    } else {
        sl::FALSE
    };
    c.camera_motion_included = sl::TRUE;
    c.motion_vectors_3d = sl::FALSE;
    c.reset = if up.reset { sl::TRUE } else { sl::FALSE };
    Ok(c)
}

/// The camera job's HUD-less copy of this camera's frame, when it made one.
fn hudless_for(cam: Option<&FrameCamera>) -> Option<crate::camera_copy::Hudless> {
    let cam = cam?;
    if !crate::camera_copy::hudless_on() || cam.hudless.is_none() {
        return None;
    }
    crate::camera_copy::hudless().filter(|h| h.frame == cam.frame)
}

/// Before the proxy's `Present`: the frame's constants (DLSS-G on while they
/// exist, off otherwise), the backbuffer tag and the markers up to
/// `ePresentStart`.
pub(crate) fn before_present() {
    if !active() {
        return;
    }
    before_present_with(crate::camera::latest());
}

fn before_present_with(latest: Option<std::sync::Arc<FrameCamera>>) {
    let mut guard = SESSION.lock().unwrap_or_else(|e| e.into_inner());
    let Some(s) = guard.as_mut() else {
        return;
    };
    let hold = if crate::camera_tag::every_frame() {
        crate::camera_tag::HOLD_FRAMES
    } else {
        0
    };
    let (cam, held) = s.cameras.pick(latest, hold);
    s.held_cameras += u64::from(held);
    let cam = cam.as_deref();
    let token = TOKEN.load(Ordering::Relaxed);
    let up = frame_upscale(s.upscaled_now, s.camera_render, s.last.as_ref(), cam);
    let consts = constants(cam, up.as_ref());
    if let Err(why) = &consts {
        s.off_why.clone_from(why);
    }
    let want = consts.is_ok();
    if want != s.on {
        s.options.mode = if want {
            sl::DLSSG_MODE_ON
        } else {
            sl::DLSSG_MODE_OFF
        };
        // SAFETY: on the presenting thread, before its present.
        let r = unsafe { (s.set_options)(&viewport(), &s.options) };
        s.on = want;
        s.reset_next |= want;
        s.switches += 1;
        if s.switches <= 20 || s.switches.is_power_of_two() {
            match &consts {
                Ok(_) => tracing::info!(
                    switches = s.switches,
                    result = sl::result_name(r),
                    "DLSS-G: generating"
                ),
                Err(why) => tracing::info!(
                    switches = s.switches,
                    result = sl::result_name(r),
                    "DLSS-G: paused ({why})"
                ),
            }
        }
    }
    if let Ok(mut c) = consts {
        if s.reset_next {
            c.reset = sl::TRUE;
            s.reset_next = false;
        }
        let (mv, jit) = signs();
        c.mvec_scale = c.mvec_scale.map(|v| v * mv);
        c.jitter_offset = c.jitter_offset.map(|v| v * jit);
        // SAFETY: plain constants; Streamline keeps a copy.
        let r = unsafe { (s.api.set_constants)(&c, token, &viewport()) };
        if r != sl::OK && !s.told_constants {
            s.told_constants = true;
            tracing::warn!(result = sl::result_name(r), "DLSS-G: constants refused");
        }
    }
    let back_extent = sl::Extent {
        top: 0,
        left: 0,
        width: s.options.color_width,
        height: s.options.color_height,
    };
    let mut back = sl::Resource::texture(std::ptr::null_mut(), 0);
    let mut tags = vec![texture_tag(
        &mut back,
        sl::BUFFER_BACKBUFFER,
        sl::LIFECYCLE_VALID_UNTIL_PRESENT,
        back_extent,
    )];
    let hudless = hudless_for(cam);
    let mut hud_resource;
    if let Some(h) = &hudless {
        if !s.told_hudless {
            s.told_hudless = true;
            tracing::info!(
                width = h.width,
                height = h.height,
                format = h.format.0,
                back = ?(back_extent.width, back_extent.height),
                "DLSS-G: tagging the HUD-less colour (VICTORY_CAMERA_HUDLESS=1)"
            );
        }
        hud_resource = sl::Resource::texture(h.resource.as_raw(), HUDLESS_STATE);
        tags.push(texture_tag(
            &mut hud_resource,
            sl::BUFFER_HUDLESS_COLOR,
            sl::LIFECYCLE_VALID_UNTIL_PRESENT,
            sl::Extent {
                top: 0,
                left: 0,
                width: h.width,
                height: h.height,
            },
        ));
    }
    // SAFETY: Streamline calls with its token and our structures.
    unsafe {
        let _ = (s.api.set_tag_for_frame)(token, &viewport(), tags.as_ptr(), tags.len() as u32, std::ptr::null_mut());
        for m in [
            sl::MARKER_SIMULATION_END,
            sl::MARKER_RENDER_SUBMIT_START,
            sl::MARKER_RENDER_SUBMIT_END,
            sl::MARKER_PRESENT_START,
        ] {
            let _ = (s.set_marker)(m, token);
        }
    }
}

/// After the proxy's `Present`: `ePresentEnd`, now and then the `DLSS-G:
/// window` line, the next frame's token, `slReflexSleep` and
/// `eSimulationStart`.
pub(crate) fn after_present() {
    if !active() {
        return;
    }
    let (sleep, marker, token) = {
        let mut guard = SESSION.lock().unwrap_or_else(|e| e.into_inner());
        let Some(s) = guard.as_mut() else {
            return;
        };
        let token = TOKEN.load(Ordering::Relaxed);
        // SAFETY: Streamline calls with its token and our structures.
        unsafe {
            let _ = (s.set_marker)(sl::MARKER_PRESENT_END, token);
        }
        s.real += 1;
        s.generating += u64::from(s.on);
        s.upscale_tags += u64::from(s.upscaled_now);
        s.camera_tags += u64::from(!s.upscaled_now && s.camera_render.is_some());
        s.upscaled_now = false;
        s.camera_render = None;
        if s.since.elapsed() >= WINDOW {
            let mut state = sl::DlssgState::new();
            // SAFETY: on the presenting thread; an out structure.
            let r = unsafe { (s.get_state)(&viewport(), &mut state, std::ptr::null()) };
            let secs = s.since.elapsed().as_secs_f64();
            tracing::info!(
                result = sl::result_name(r),
                real = s.real,
                real_fps = format_args!("{:.1}", s.real as f64 / secs),
                generating_presents = s.generating,
                upscale_tags = s.upscale_tags,
                camera_tags = s.camera_tags,
                held_tags = s.held_tags,
                held_cameras = s.held_cameras,
                every_frame = crate::camera_tag::every_frame(),
                frames_per_present = state.num_frames_actually_presented,
                generating = s.on,
                off_why = %s.off_why,
                status = %sl::dlssg_status(state.status),
                max_generated = state.num_frames_to_generate_max,
                vram_mb = state.estimated_vram_usage_in_bytes >> 20,
                "DLSS-G: window"
            );
            (s.real, s.generating, s.since) = (0, 0, Instant::now());
            (s.upscale_tags, s.camera_tags) = (0, 0);
            (s.held_tags, s.held_cameras) = (0, 0);
            s.off_why.clear();
        }
        let frame = FRAME.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
        let mut next: sl::FrameToken = std::ptr::null();
        // SAFETY: an out pointer and the frame index.
        let r = unsafe { (s.api.get_new_frame_token)(&mut next, &frame) };
        if r == sl::OK {
            TOKEN.store(next.cast_mut(), Ordering::Relaxed);
        }
        (s.reflex_sleep, s.set_marker, TOKEN.load(Ordering::Relaxed))
    };
    // Outside the lock: the sleep may hold this thread for a while.
    // SAFETY: Streamline calls with its token.
    unsafe {
        if sleep_on() {
            let _ = sleep(token);
        }
        let _ = marker(sl::MARKER_SIMULATION_START, token);
    }
}

#[cfg(test)]
#[path = "dlssg_tests.rs"]
mod tests;
