//! AMD FSR frame generation over our presents (`VICTORY_FRAME_GEN=fsr`, off by
//! default): one generated frame between two of the title's.
//!
//! AMD's signed FidelityFX DLLs are loaded at runtime from `VICTORY_FSR_FG_DIR`
//! (else beside the executable): `amd_fidelityfx_loader_dx12.dll` (SDK 2.x,
//! with `amd_fidelityfx_framegeneration_dx12.dll` beside it) or
//! `amd_fidelityfx_dx12.dll` (FSR 3.1). Nothing of AMD's is in the repository;
//! the structures below follow the public FidelityFX API headers (MIT,
//! `ffx_api.h`, `ffx_api_types.h`, `ffx_framegeneration.h`,
//! `ffx_api_framegeneration_dx12.h`, `ffx_api_dx12.h`).
//!
//! The pieces, in the order they run:
//! 1. [`create_swap_chain`]: the window's swap chain is FidelityFX's proxy
//!    (it presents the title's frame and the generated one, and paces them).
//! 2. [`prepare`], after each upscale: the frame generation context is made
//!    once the sizes are known, then configured and its prepare step recorded
//!    on the upscale's list with the title's depth, motion vectors, jitter,
//!    render size, near/far and field of view.
//! 3. [`active`]: `pacing.rs` leaves its present caps to the proxy.
//!
//! No HUD-less colour exists (the title composes its HUD into the frame we
//! present), so the HUD is interpolated with the scene: moving HUD parts can
//! ghost. `VICTORY_FRAME_GEN=dlss` is not supported (DLSS-G needs camera
//! matrices); it stays off and says so.

use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::core::Interface;

/// What `VICTORY_FRAME_GEN` asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Off,
    Fsr,
    Dlss,
}

impl Kind {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "" | "0" | "off" => Some(Self::Off),
            "fsr" => Some(Self::Fsr),
            "dlss" => Some(Self::Dlss),
            _ => None,
        }
    }
}

/// `VICTORY_FRAME_GEN=fsr`.
pub(crate) fn asked() -> bool {
    static ASKED: OnceLock<bool> = OnceLock::new();
    *ASKED.get_or_init(|| {
        let Ok(v) = std::env::var("VICTORY_FRAME_GEN") else {
            return false;
        };
        match Kind::parse(&v) {
            Some(Kind::Fsr) => true,
            Some(Kind::Off) => false,
            Some(Kind::Dlss) => {
                tracing::warn!("VICTORY_FRAME_GEN=dlss is not supported (DLSS-G needs camera matrices); frame generation stays off");
                false
            }
            None => {
                tracing::warn!(value = v, "VICTORY_FRAME_GEN: expected off, fsr or dlss; frame generation stays off");
                false
            }
        }
    })
}

/// Whether the proxy swap chain presents (frame generation is running).
static ACTIVE: AtomicBool = AtomicBool::new(false);

/// Whether frame generation owns the presents' pacing.
pub(crate) fn active() -> bool {
    ACTIVE.load(Ordering::Relaxed)
}

// --- The FidelityFX API (public headers, MIT) ---

pub(crate) const RETURN_OK: u32 = 0;
const EFFECT_FRAMEGENERATION: u64 = 0x0002_0000;
const EFFECT_FRAMEGENERATIONSWAPCHAIN: u64 = 0x0003_0000;
const BACKEND_DX12: u64 = 0;
pub(crate) const CREATE_BACKEND_DX12: u64 = BACKEND_DX12 | 0x02;
pub(crate) const CREATE_FRAMEGENERATION: u64 = EFFECT_FRAMEGENERATION | 0x01;
pub(crate) const CONFIGURE_FRAMEGENERATION: u64 = EFFECT_FRAMEGENERATION | 0x02;
pub(crate) const DISPATCH_FRAMEGENERATION_PREPARE: u64 = EFFECT_FRAMEGENERATION | 0x04;
pub(crate) const CREATE_SWAPCHAIN_FOR_HWND_DX12: u64 =
    BACKEND_DX12 | EFFECT_FRAMEGENERATIONSWAPCHAIN | 0x06;

const FG_DEPTH_INVERTED: u32 = 1 << 3;
const FG_HIGH_DYNAMIC_RANGE: u32 = 1 << 5;

const RESOURCE_TYPE_TEXTURE2D: u32 = 2;
const RESOURCE_USAGE_READ_ONLY: u32 = 0;
const RESOURCE_USAGE_UAV: u32 = 1 << 1;
const RESOURCE_USAGE_DEPTHTARGET: u32 = 1 << 2;
pub(crate) const STATE_COMPUTE_READ: u32 = 1 << 2;

#[repr(C)]
pub(crate) struct Header {
    pub ty: u64,
    pub next: *mut Header,
}

impl Header {
    fn new(ty: u64) -> Self {
        Self {
            ty,
            next: std::ptr::null_mut(),
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq)]
pub(crate) struct ResourceDesc {
    pub ty: u32,
    pub format: u32,
    pub width: u32,
    pub height: u32,
    pub depth: u32,
    pub mip_count: u32,
    pub flags: u32,
    pub usage: u32,
}

/// `FfxApiResource`.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct Resource {
    pub resource: *mut c_void,
    pub desc: ResourceDesc,
    pub state: u32,
}

impl Resource {
    const NONE: Self = Self {
        resource: std::ptr::null_mut(),
        desc: ResourceDesc {
            ty: 0,
            format: 0,
            width: 0,
            height: 0,
            depth: 0,
            mip_count: 0,
            flags: 0,
            usage: 0,
        },
        state: 0,
    };
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(crate) struct Dim2 {
    pub width: u32,
    pub height: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(crate) struct Coords2 {
    pub x: f32,
    pub y: f32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(crate) struct Rect2 {
    pub left: i32,
    pub top: i32,
    pub width: i32,
    pub height: i32,
}

#[repr(C)]
pub(crate) struct CreateBackendDx12 {
    pub header: Header,
    pub device: *mut c_void,
}

#[repr(C)]
pub(crate) struct CreateFrameGeneration {
    pub header: Header,
    pub flags: u32,
    pub display: Dim2,
    pub max_render: Dim2,
    pub back_buffer_format: u32,
}

#[repr(C)]
pub(crate) struct CreateSwapChainForHwnd {
    pub header: Header,
    pub swap_chain: *mut *mut c_void,
    pub hwnd: *mut c_void,
    pub desc: *const DXGI_SWAP_CHAIN_DESC1,
    pub fullscreen: *const DXGI_SWAP_CHAIN_FULLSCREEN_DESC,
    pub factory: *mut c_void,
    pub queue: *mut c_void,
}

/// `ffxDispatchDescFrameGeneration` is only passed through, by pointer.
type FgCallback = unsafe extern "C" fn(params: *mut Header, user: *mut c_void) -> u32;

#[repr(C)]
pub(crate) struct ConfigureFrameGeneration {
    pub header: Header,
    pub swap_chain: *mut c_void,
    pub present_callback: *const c_void,
    pub present_user: *mut c_void,
    pub fg_callback: Option<FgCallback>,
    pub fg_user: *mut c_void,
    pub enabled: bool,
    pub allow_async: bool,
    pub hudless: Resource,
    pub flags: u32,
    pub only_present_generated: bool,
    pub rect: Rect2,
    pub frame_id: u64,
}

/// `ffxDispatchDescFrameGenerationPrepare` (the one every provider knows).
#[repr(C)]
pub(crate) struct Prepare {
    pub header: Header,
    pub frame_id: u64,
    pub flags: u32,
    pub list: *mut c_void,
    pub render: Dim2,
    pub jitter: Coords2,
    pub motion_scale: Coords2,
    pub frame_ms: f32,
    pub unused_reset: bool,
    pub near: f32,
    pub far: f32,
    pub fov_y: f32,
    pub metres_per_unit: f32,
    pub depth: Resource,
    pub motion: Resource,
}

type CreateContext =
    unsafe extern "C" fn(ctx: *mut *mut c_void, desc: *mut Header, mem: *const c_void) -> u32;
type Configure = unsafe extern "C" fn(ctx: *mut *mut c_void, desc: *const Header) -> u32;
type Dispatch = unsafe extern "C" fn(ctx: *mut *mut c_void, desc: *const Header) -> u32;

pub(crate) struct Api {
    pub path: PathBuf,
    pub create: CreateContext,
    pub configure: Configure,
    pub dispatch: Dispatch,
}

/// The DLL names tried, newest SDK first.
pub(crate) const DLL_NAMES: [&str; 2] =
    ["amd_fidelityfx_loader_dx12.dll", "amd_fidelityfx_dx12.dll"];

pub(crate) fn dir() -> Option<PathBuf> {
    std::env::var_os("VICTORY_FSR_FG_DIR")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::current_exe().ok()?.parent().map(PathBuf::from))
}

fn load() -> Result<Api, String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::System::LibraryLoader::{
        GetProcAddress, LOAD_LIBRARY_SEARCH_DEFAULT_DIRS, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR,
        LoadLibraryExW,
    };
    let dir = dir().ok_or("no VICTORY_FSR_FG_DIR")?;
    let path = DLL_NAMES
        .iter()
        .map(|n| dir.join(n))
        .find(|p| p.is_file())
        .ok_or_else(|| format!("no {} in {}", DLL_NAMES.join(" or "), dir.display()))?;
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
    // SAFETY: a full path; the loader looks for its effect DLLs beside it.
    let module = unsafe {
        LoadLibraryExW(
            wide.as_ptr(),
            std::ptr::null_mut(),
            LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_DEFAULT_DIRS,
        )
    };
    if module.is_null() {
        return Err(format!("{} would not load", path.display()));
    }
    let export = |name: &[u8]| {
        // SAFETY: a loaded module and a NUL-terminated name.
        unsafe { GetProcAddress(module, name.as_ptr()) }
            .ok_or_else(|| format!("{} lacks an export", path.display()))
    };
    // SAFETY: the exports have the API's signatures.
    unsafe {
        Ok(Api {
            create: std::mem::transmute::<_, CreateContext>(export(b"ffxCreateContext\0")?),
            configure: std::mem::transmute::<_, Configure>(export(b"ffxConfigure\0")?),
            dispatch: std::mem::transmute::<_, Dispatch>(export(b"ffxDispatch\0")?),
            path,
        })
    }
}

pub(crate) fn api() -> Result<&'static Api, &'static str> {
    static API: OnceLock<Result<Api, String>> = OnceLock::new();
    API.get_or_init(load).as_ref().map_err(String::as_str)
}

/// `ffxApiGetSurfaceFormatDX12` for the formats the title's depth, motion
/// vectors and our back buffer use; 0 (unknown) for the rest.
pub(crate) fn surface_format(f: DXGI_FORMAT) -> u32 {
    match f {
        DXGI_FORMAT_R32G32B32A32_FLOAT => 3,
        DXGI_FORMAT_R16G16B16A16_FLOAT => 4,
        DXGI_FORMAT_R16G16B16A16_TYPELESS => 30,
        DXGI_FORMAT_R32G32_FLOAT => 6,
        DXGI_FORMAT_R32G32_TYPELESS => 31,
        DXGI_FORMAT_R8G8B8A8_TYPELESS => 9,
        DXGI_FORMAT_R8G8B8A8_UNORM => 10,
        DXGI_FORMAT_R8G8B8A8_UNORM_SRGB => 12,
        DXGI_FORMAT_B8G8R8A8_TYPELESS => 13,
        DXGI_FORMAT_B8G8R8A8_UNORM => 14,
        DXGI_FORMAT_B8G8R8A8_UNORM_SRGB => 15,
        DXGI_FORMAT_R11G11B10_FLOAT => 16,
        DXGI_FORMAT_R10G10B10A2_UNORM => 17,
        DXGI_FORMAT_R10G10B10A2_TYPELESS => 32,
        DXGI_FORMAT_R16G16_FLOAT => 18,
        DXGI_FORMAT_R16G16_TYPELESS => 33,
        DXGI_FORMAT_R16G16_SINT => 20,
        DXGI_FORMAT_R16G16_UINT => 19,
        DXGI_FORMAT_D32_FLOAT | DXGI_FORMAT_R32_FLOAT => 28,
        DXGI_FORMAT_R32_TYPELESS => 37,
        DXGI_FORMAT_R32_UINT => 8,
        DXGI_FORMAT_R16_FLOAT => 21,
        DXGI_FORMAT_R16_UNORM | DXGI_FORMAT_D16_UNORM => 23,
        DXGI_FORMAT_R16_TYPELESS => 34,
        _ => 0,
    }
}

/// `ffxApiGetResourceDX12` for a 2D texture in `state`.
pub(crate) fn resource(r: Option<&ID3D12Resource>, state: u32) -> Resource {
    let Some(r) = r else {
        return Resource::NONE;
    };
    // SAFETY: a live resource.
    let d = unsafe { r.GetDesc() };
    let mut usage = if d.Format == DXGI_FORMAT_D32_FLOAT || d.Format == DXGI_FORMAT_D16_UNORM {
        RESOURCE_USAGE_DEPTHTARGET
    } else {
        RESOURCE_USAGE_READ_ONLY
    };
    if d.Flags.contains(D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS) {
        usage |= RESOURCE_USAGE_UAV;
    }
    Resource {
        resource: r.as_raw(),
        desc: ResourceDesc {
            ty: RESOURCE_TYPE_TEXTURE2D,
            format: surface_format(d.Format),
            width: d.Width as u32,
            height: d.Height,
            depth: u32::from(d.DepthOrArraySize),
            mip_count: u32::from(d.MipLevels),
            flags: 0,
            usage,
        },
        state,
    }
}

// --- Our side ---

/// The proxy swap chain and the frame generation context.
struct Fg {
    swap_chain: *mut c_void,
    back_buffer: DXGI_FORMAT,
    display: Dim2,
    hdr: bool,
    /// The frame generation context, made at the first prepare; its key
    /// (max render size, flags).
    context: Option<(Box<*mut c_void>, [u32; 3])>,
    told: bool,
}

// SAFETY: the FidelityFX objects are used under the mutex only.
unsafe impl Send for Fg {}

static FG: Mutex<Option<Fg>> = Mutex::new(None);
static FRAME_ID: AtomicU64 = AtomicU64::new(0);

/// The window's swap chain as FidelityFX's proxy, when `VICTORY_FRAME_GEN=fsr`
/// asked and it could be made; `None` makes the plain one.
pub(crate) fn create_swap_chain(
    factory: &IDXGIFactory2,
    queue: &ID3D12CommandQueue,
    hwnd: HWND,
    desc: &DXGI_SWAP_CHAIN_DESC1,
) -> Option<IDXGISwapChain3> {
    if !asked() {
        return None;
    }
    let api = match api() {
        Ok(a) => a,
        Err(why) => {
            tracing::warn!(
                why,
                "frame generation: no FidelityFX DLL (VICTORY_FSR_FG_DIR); presenting without it"
            );
            return None;
        }
    };
    let mut ctx: Box<*mut c_void> = Box::new(std::ptr::null_mut());
    let mut chain: *mut c_void = std::ptr::null_mut();
    let mut create = CreateSwapChainForHwnd {
        header: Header::new(CREATE_SWAPCHAIN_FOR_HWND_DX12),
        swap_chain: &mut chain,
        hwnd: hwnd.0,
        desc,
        fullscreen: std::ptr::null(),
        factory: factory.as_raw(),
        queue: queue.as_raw(),
    };
    // SAFETY: live descriptions and objects; the proxy is returned with one reference.
    let r = unsafe { (api.create)(&mut *ctx, &mut create.header, std::ptr::null()) };
    if r != RETURN_OK || chain.is_null() {
        tracing::warn!(result = r, dll = %api.path.display(), "frame generation: the proxy swap chain was refused; presenting without it");
        return None;
    }
    // SAFETY: an `IDXGISwapChain4` with the reference we own.
    let swap_chain = unsafe { IDXGISwapChain4::from_raw(chain) };
    let plain: IDXGISwapChain3 = swap_chain.cast().ok()?;
    // The swap chain context lives as long as the process.
    std::mem::forget(ctx);
    *FG.lock().unwrap_or_else(|e| e.into_inner()) = Some(Fg {
        swap_chain: swap_chain.as_raw(),
        back_buffer: desc.Format,
        display: Dim2 {
            width: desc.Width,
            height: desc.Height,
        },
        hdr: matches!(
            desc.Format,
            DXGI_FORMAT_R16G16B16A16_FLOAT | DXGI_FORMAT_R10G10B10A2_UNORM
        ),
        context: None,
        told: false,
    });
    std::mem::forget(swap_chain);
    ACTIVE.store(true, Ordering::Relaxed);
    tracing::info!(dll = %api.path.display(), width = desc.Width, height = desc.Height, format = desc.Format.0, "frame generation: FSR proxy swap chain (VICTORY_FRAME_GEN=fsr)");
    Some(plain)
}

/// What one upscale hands the prepare step, as host resources.
pub(crate) struct Inputs<'a> {
    pub depth: Option<&'a ID3D12Resource>,
    pub motion: Option<&'a ID3D12Resource>,
    pub render: [u32; 2],
    pub max_render: [u32; 2],
    pub jitter: [f32; 2],
    pub motion_scale: [f32; 2],
    pub frame_ms: f32,
    pub reset: bool,
    pub near: f32,
    pub far: f32,
    pub fov_y: f32,
    pub metres_per_unit: f32,
}

unsafe extern "C" fn dispatch_generation(params: *mut Header, user: *mut c_void) -> u32 {
    let Ok(api) = api() else {
        return 1;
    };
    // SAFETY: `user` is the context box this module keeps alive.
    unsafe { (api.dispatch)(user.cast(), params) }
}

/// Configures frame generation for this frame and records its prepare step
/// on `list` (after the upscale; depth and motion vectors readable by compute).
/// Nothing without the proxy swap chain.
pub(crate) fn prepare(
    device: &ID3D12Device,
    list: &ID3D12GraphicsCommandList,
    inputs: &Inputs<'_>,
) {
    if !active() {
        return;
    }
    let Ok(api) = api() else {
        return;
    };
    let mut guard = FG.lock().unwrap_or_else(|e| e.into_inner());
    let Some(fg) = guard.as_mut() else {
        return;
    };
    let (Some(depth), Some(motion)) = (inputs.depth, inputs.motion) else {
        if !fg.told {
            fg.told = true;
            tracing::warn!(
                "frame generation: an upscale without depth or motion vectors; that frame is not prepared"
            );
        }
        return;
    };
    let inverted = inputs.near > inputs.far;
    let flags = (if inverted { FG_DEPTH_INVERTED } else { 0 })
        | (if fg.hdr { FG_HIGH_DYNAMIC_RANGE } else { 0 });
    let key = [inputs.max_render[0], inputs.max_render[1], flags];
    if fg.context.as_ref().is_none_or(|(_, k)| *k != key) {
        // A replaced context is leaked: destroying it while the proxy may
        // still dispatch it is not safe, and sizes change rarely.
        let mut ctx: Box<*mut c_void> = Box::new(std::ptr::null_mut());
        let mut backend = CreateBackendDx12 {
            header: Header::new(CREATE_BACKEND_DX12),
            device: device.as_raw(),
        };
        let mut create = CreateFrameGeneration {
            header: Header::new(CREATE_FRAMEGENERATION),
            flags,
            display: fg.display,
            max_render: Dim2 {
                width: key[0],
                height: key[1],
            },
            back_buffer_format: surface_format(fg.back_buffer),
        };
        create.header.next = &mut backend.header;
        // SAFETY: live descriptions; the context box outlives every use.
        let r = unsafe { (api.create)(&mut *ctx, &mut create.header, std::ptr::null()) };
        if r != RETURN_OK {
            tracing::warn!(
                result = r,
                ?key,
                "frame generation: no context; frames are presented as they are"
            );
            ACTIVE.store(false, Ordering::Relaxed);
            return;
        }
        tracing::info!(max_render = ?(key[0], key[1]), depth_inverted = inverted, hdr = fg.hdr, "frame generation: context made");
        if let Some((old, _)) = fg.context.take() {
            std::mem::forget(old);
        }
        fg.context = Some((ctx, key));
    }
    let Some((ctx, _)) = fg.context.as_mut() else {
        return;
    };
    let ctx: *mut *mut c_void = &mut **ctx;
    let frame_id = FRAME_ID.fetch_add(1, Ordering::Relaxed);
    let configure = ConfigureFrameGeneration {
        header: Header::new(CONFIGURE_FRAMEGENERATION),
        swap_chain: fg.swap_chain,
        present_callback: std::ptr::null(),
        present_user: std::ptr::null_mut(),
        fg_callback: Some(dispatch_generation),
        fg_user: ctx.cast(),
        enabled: true,
        allow_async: false,
        hudless: Resource::NONE,
        flags: 0,
        only_present_generated: false,
        rect: Rect2 {
            left: 0,
            top: 0,
            width: fg.display.width as i32,
            height: fg.display.height as i32,
        },
        frame_id,
    };
    let scale = |s: f32| if s == 0.0 { 1.0 } else { s };
    let prep = Prepare {
        header: Header::new(DISPATCH_FRAMEGENERATION_PREPARE),
        frame_id,
        flags: 0,
        list: list.as_raw(),
        render: Dim2 {
            width: inputs.render[0],
            height: inputs.render[1],
        },
        jitter: Coords2 {
            x: inputs.jitter[0],
            y: inputs.jitter[1],
        },
        motion_scale: Coords2 {
            x: scale(inputs.motion_scale[0]),
            y: scale(inputs.motion_scale[1]),
        },
        frame_ms: inputs.frame_ms,
        unused_reset: inputs.reset,
        near: inputs.near,
        far: inputs.far,
        fov_y: inputs.fov_y,
        metres_per_unit: if inputs.metres_per_unit > 0.0 {
            inputs.metres_per_unit
        } else {
            1.0
        },
        depth: resource(Some(depth), STATE_COMPUTE_READ),
        motion: resource(Some(motion), STATE_COMPUTE_READ),
    };
    // SAFETY: a live context, descriptions and list.
    let (c, d) = unsafe {
        (
            (api.configure)(ctx, &configure.header),
            (api.dispatch)(ctx, &prep.header),
        )
    };
    if (c != RETURN_OK || d != RETURN_OK) && !fg.told {
        fg.told = true;
        tracing::warn!(
            configure = c,
            prepare = d,
            "frame generation: configure or prepare failed"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{offset_of, size_of};

    /// Offsets as the C headers lay them out on x64.
    #[test]
    fn layouts_match_the_headers() {
        assert_eq!(size_of::<Header>(), 16);
        assert_eq!(size_of::<ResourceDesc>(), 32);
        assert_eq!(size_of::<Resource>(), 48);
        assert_eq!(offset_of!(Resource, state), 40);
        assert_eq!(size_of::<CreateFrameGeneration>(), 40);
        assert_eq!(offset_of!(CreateFrameGeneration, back_buffer_format), 36);
        assert_eq!(offset_of!(CreateSwapChainForHwnd, queue), 56);
        assert_eq!(offset_of!(ConfigureFrameGeneration, enabled), 56);
        assert_eq!(offset_of!(ConfigureFrameGeneration, allow_async), 57);
        assert_eq!(offset_of!(ConfigureFrameGeneration, hudless), 64);
        assert_eq!(offset_of!(ConfigureFrameGeneration, flags), 112);
        assert_eq!(
            offset_of!(ConfigureFrameGeneration, only_present_generated),
            116
        );
        assert_eq!(offset_of!(ConfigureFrameGeneration, rect), 120);
        assert_eq!(offset_of!(ConfigureFrameGeneration, frame_id), 136);
        assert_eq!(offset_of!(Prepare, frame_id), 16);
        assert_eq!(offset_of!(Prepare, flags), 24);
        assert_eq!(offset_of!(Prepare, list), 32);
        assert_eq!(offset_of!(Prepare, render), 40);
        assert_eq!(offset_of!(Prepare, frame_ms), 64);
        assert_eq!(offset_of!(Prepare, unused_reset), 68);
        assert_eq!(offset_of!(Prepare, near), 72);
        assert_eq!(offset_of!(Prepare, metres_per_unit), 84);
        assert_eq!(offset_of!(Prepare, depth), 88);
        assert_eq!(offset_of!(Prepare, motion), 136);
        assert_eq!(size_of::<Prepare>(), 184);
    }

    #[test]
    fn struct_types_and_switch_parse() {
        assert_eq!(CREATE_FRAMEGENERATION, 0x0002_0001);
        assert_eq!(CONFIGURE_FRAMEGENERATION, 0x0002_0002);
        assert_eq!(DISPATCH_FRAMEGENERATION_PREPARE, 0x0002_0004);
        assert_eq!(CREATE_SWAPCHAIN_FOR_HWND_DX12, 0x0003_0006);
        assert_eq!(CREATE_BACKEND_DX12, 0x2);
        assert_eq!(Kind::parse("FSR"), Some(Kind::Fsr));
        assert_eq!(Kind::parse("off"), Some(Kind::Off));
        assert_eq!(Kind::parse("dlss"), Some(Kind::Dlss));
        assert_eq!(Kind::parse("x4"), None);
        assert_eq!(surface_format(DXGI_FORMAT_R16G16_FLOAT), 18);
        assert_eq!(surface_format(DXGI_FORMAT_D32_FLOAT), 28);
        assert_eq!(surface_format(DXGI_FORMAT_R10G10B10A2_UNORM), 17);
        assert_eq!(surface_format(DXGI_FORMAT_BC1_UNORM), 0);
    }
}
