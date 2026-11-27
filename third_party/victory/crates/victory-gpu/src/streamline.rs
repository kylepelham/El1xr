//! NVIDIA Streamline's C ABI, as far as DLSS Frame Generation needs it
//! (`dlssg.rs` uses it). Streamline (MIT, github.com/NVIDIA-RTX/Streamline,
//! SDK 2.14.16) is not in the repository: its signed `sl.interposer.dll` and
//! plugins (`sl.common.dll`, `sl.dlss_g.dll`, `sl.reflex.dll`, `sl.pcl.dll`,
//! `nvngx_dlssg.dll`) are loaded at runtime from `VICTORY_STREAMLINE_DIR`
//! (else beside the executable). The structures below are written from the
//! SDK's public headers (`sl_struct.h`, `sl_core_types.h`, `sl_consts.h`,
//! `sl_core_api.h`, `sl_dlss_g.h`, `sl_reflex.h`, `sl_pcl.h`), x64 layout,
//! pinned by the tests at the end.

use std::ffi::{c_char, c_void};
use std::path::PathBuf;
use std::sync::OnceLock;

/// `sl::kSDKVersion` of SDK 2.14.16.
pub(crate) const SDK_VERSION: u64 = (2u64 << 48) | (14u64 << 32) | (16u64 << 16) | 0xfedc;

/// `sl::Feature`.
pub(crate) const FEATURE_REFLEX: u32 = 3;
pub(crate) const FEATURE_PCL: u32 = 4;
pub(crate) const FEATURE_DLSS_G: u32 = 1000;

/// `sl::PreferenceFlags`.
pub(crate) const PREF_DISABLE_CL_STATE_TRACKING: u64 = 1 << 0;
pub(crate) const PREF_USE_MANUAL_HOOKING: u64 = 1 << 2;
pub(crate) const PREF_USE_FRAME_BASED_RESOURCE_TAGGING: u64 = 1 << 7;

/// `sl::BufferType`.
pub(crate) const BUFFER_DEPTH: u32 = 0;
pub(crate) const BUFFER_MOTION_VECTORS: u32 = 1;
pub(crate) const BUFFER_HUDLESS_COLOR: u32 = 2;
pub(crate) const BUFFER_UI_COLOR_AND_ALPHA: u32 = 23;
pub(crate) const BUFFER_BACKBUFFER: u32 = 53;

/// `sl::ResourceLifecycle`.
pub(crate) const LIFECYCLE_ONLY_VALID_NOW: u32 = 0;
pub(crate) const LIFECYCLE_VALID_UNTIL_PRESENT: u32 = 1;

/// `sl::Boolean`.
pub(crate) const FALSE: u8 = 0;
pub(crate) const TRUE: u8 = 1;
pub(crate) const INVALID: u8 = 2;

/// `sl::INVALID_FLOAT`.
pub(crate) const INVALID_FLOAT: f32 = f32::MAX;

/// `sl::DLSSGMode`, `sl::DLSSGFlags`.
pub(crate) const DLSSG_MODE_OFF: u32 = 0;
pub(crate) const DLSSG_MODE_ON: u32 = 1;
pub(crate) const DLSSG_FLAG_RETAIN_RESOURCES_WHEN_OFF: u32 = 1 << 3;

/// `sl::ReflexMode`.
pub(crate) const REFLEX_LOW_LATENCY: i32 = 1;

/// `sl::PCLMarker`.
pub(crate) const MARKER_SIMULATION_START: u32 = 0;
pub(crate) const MARKER_SIMULATION_END: u32 = 1;
pub(crate) const MARKER_RENDER_SUBMIT_START: u32 = 2;
pub(crate) const MARKER_RENDER_SUBMIT_END: u32 = 3;
pub(crate) const MARKER_PRESENT_START: u32 = 4;
pub(crate) const MARKER_PRESENT_END: u32 = 5;

/// `sl::EngineType::eCustom`, `sl::RenderAPI::eD3D12`, `sl::LogLevel::eDefault`.
const ENGINE_CUSTOM: u32 = 0;
const RENDER_API_D3D12: u32 = 1;
const LOG_DEFAULT: u32 = 1;

/// `sl::Result` names, by value (for the log).
const RESULTS: [&str; 40] = [
    "eOk",
    "eErrorIO",
    "eErrorDriverOutOfDate",
    "eErrorOSOutOfDate",
    "eErrorOSDisabledHWS",
    "eErrorDeviceNotCreated",
    "eErrorNoSupportedAdapterFound",
    "eErrorAdapterNotSupported",
    "eErrorNoPlugins",
    "eErrorVulkanAPI",
    "eErrorDXGIAPI",
    "eErrorD3DAPI",
    "eErrorNRDAPI",
    "eErrorNVAPI",
    "eErrorReflexAPI",
    "eErrorNGXFailed",
    "eErrorJSONParsing",
    "eErrorMissingProxy",
    "eErrorMissingResourceState",
    "eErrorInvalidIntegration",
    "eErrorMissingInputParameter",
    "eErrorNotInitialized",
    "eErrorComputeFailed",
    "eErrorInitNotCalled",
    "eErrorExceptionHandler",
    "eErrorInvalidParameter",
    "eErrorMissingConstants",
    "eErrorDuplicatedConstants",
    "eErrorMissingOrInvalidAPI",
    "eErrorCommonConstantsMissing",
    "eErrorUnsupportedInterface",
    "eErrorFeatureMissing",
    "eErrorFeatureNotSupported",
    "eErrorFeatureMissingHooks",
    "eErrorFeatureFailedToLoad",
    "eErrorFeatureWrongPriority",
    "eErrorFeatureMissingDependency",
    "eErrorFeatureManagerInvalidState",
    "eErrorInvalidState",
    "eWarnOutOfVRAM",
];

pub(crate) const OK: i32 = 0;

/// A `sl::Result` as its name.
pub(crate) fn result_name(r: i32) -> &'static str {
    usize::try_from(r)
        .ok()
        .and_then(|i| RESULTS.get(i))
        .copied()
        .unwrap_or("unknown")
}

/// `sl::StructType` (a GUID).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct StructType {
    pub data1: u32,
    pub data2: u16,
    pub data3: u16,
    pub data4: [u8; 8],
}

const fn guid(data1: u32, data2: u16, data3: u16, data4: [u8; 8]) -> StructType {
    StructType {
        data1,
        data2,
        data3,
        data4,
    }
}

/// `sl::BaseStructure`.
#[repr(C)]
#[derive(Debug)]
pub(crate) struct Base {
    pub next: *mut Base,
    pub ty: StructType,
    pub version: usize,
}

impl Base {
    const fn new(ty: StructType, version: usize) -> Self {
        Self {
            next: std::ptr::null_mut(),
            ty,
            version,
        }
    }
}

const PREFERENCES: StructType = guid(
    0x1ca1_0965,
    0xbf8e,
    0x432b,
    [0x8d, 0xa1, 0x67, 0x16, 0xd8, 0x79, 0xfb, 0x14],
);
const RESOURCE: StructType = guid(
    0x3a9d_70cf,
    0x2418,
    0x4b72,
    [0x83, 0x91, 0x13, 0xf8, 0x72, 0x1c, 0x72, 0x61],
);
const RESOURCE_TAG: StructType = guid(
    0x4c6a_5aad,
    0xb445,
    0x496c,
    [0x87, 0xff, 0x1a, 0xf3, 0x84, 0x5b, 0xe6, 0x53],
);
const CONSTANTS: StructType = guid(
    0xdcd3_5ad7,
    0x4e4a,
    0x4bad,
    [0xa9, 0x0c, 0xe0, 0xc4, 0x9e, 0xb2, 0x3a, 0xfe],
);
const VIEWPORT_HANDLE: StructType = guid(
    0x171b_6435,
    0x9b3c,
    0x4fc8,
    [0x99, 0x94, 0xfb, 0xe5, 0x25, 0x69, 0xaa, 0xa4],
);
const ADAPTER_INFO: StructType = guid(
    0x0677_315f,
    0xa746,
    0x4492,
    [0x9f, 0x42, 0xcb, 0x61, 0x42, 0xc9, 0xc3, 0xd4],
);
const FEATURE_VERSION: StructType = guid(
    0x6d5b_51f0,
    0x076b,
    0x486d,
    [0x99, 0x95, 0x5a, 0x56, 0x10, 0x43, 0xf5, 0xc1],
);
const DLSSG_OPTIONS: StructType = guid(
    0xfac5_f1cb,
    0x2dfd,
    0x4f36,
    [0xa1, 0xe6, 0x3a, 0x9e, 0x86, 0x52, 0x56, 0xc5],
);
const DLSSG_STATE: StructType = guid(
    0xcc8a_c8e1,
    0xa179,
    0x44f5,
    [0x97, 0xfa, 0xe7, 0x41, 0x12, 0xf9, 0xbc, 0x61],
);
const REFLEX_OPTIONS: StructType = guid(
    0xf03a_f81a,
    0x6d0b,
    0x4902,
    [0xa6, 0x51, 0xc4, 0x96, 0x5e, 0x21, 0x54, 0x34],
);

pub(crate) type LogCallback = unsafe extern "C" fn(ty: u32, msg: *const c_char);

/// `sl::Preferences` (version 1).
#[repr(C)]
pub(crate) struct Preferences {
    pub base: Base,
    pub show_console: bool,
    pub log_level: u32,
    pub paths_to_plugins: *const *const u16,
    pub num_paths_to_plugins: u32,
    pub path_to_logs_and_data: *const u16,
    pub allocate_callback: *const c_void,
    pub release_callback: *const c_void,
    pub log_message_callback: Option<LogCallback>,
    pub flags: u64,
    pub features_to_load: *const u32,
    pub num_features_to_load: u32,
    pub application_id: u32,
    pub engine: u32,
    pub engine_version: *const c_char,
    pub project_id: *const c_char,
    pub render_api: u32,
}

impl Preferences {
    pub(crate) fn new() -> Self {
        Self {
            base: Base::new(PREFERENCES, 1),
            show_console: false,
            log_level: LOG_DEFAULT,
            paths_to_plugins: std::ptr::null(),
            num_paths_to_plugins: 0,
            path_to_logs_and_data: std::ptr::null(),
            allocate_callback: std::ptr::null(),
            release_callback: std::ptr::null(),
            log_message_callback: None,
            flags: 0,
            features_to_load: std::ptr::null(),
            num_features_to_load: 0,
            application_id: 0,
            engine: ENGINE_CUSTOM,
            engine_version: std::ptr::null(),
            project_id: std::ptr::null(),
            render_api: RENDER_API_D3D12,
        }
    }
}

/// `sl::Resource` (version 1), a D3D12 texture.
#[repr(C)]
pub(crate) struct Resource {
    pub base: Base,
    /// `sl::ResourceType` (a `char` enum): 0 = `eTex2d`.
    pub ty: u8,
    pub native: *mut c_void,
    pub memory: *mut c_void,
    pub view: *mut c_void,
    /// The D3D12 resource state.
    pub state: u32,
    pub width: u32,
    pub height: u32,
    pub native_format: u32,
    pub mip_levels: u32,
    pub array_layers: u32,
    pub gpu_virtual_address: u64,
    pub flags: u32,
    pub usage: u32,
    pub internal_flags: u16,
    pub reserved: u16,
}

impl Resource {
    pub(crate) fn texture(native: *mut c_void, state: u32) -> Self {
        Self {
            base: Base::new(RESOURCE, 1),
            ty: 0,
            native,
            memory: std::ptr::null_mut(),
            view: std::ptr::null_mut(),
            state,
            width: 0,
            height: 0,
            native_format: 0,
            mip_levels: 0,
            array_layers: 0,
            gpu_virtual_address: 0,
            flags: 0,
            usage: 0,
            internal_flags: 0,
            reserved: 0,
        }
    }
}

/// `sl::Extent`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Extent {
    pub top: u32,
    pub left: u32,
    pub width: u32,
    pub height: u32,
}

/// `sl::ResourceTag` (version 1).
#[repr(C)]
pub(crate) struct ResourceTag {
    pub base: Base,
    pub resource: *mut Resource,
    pub ty: u32,
    pub lifecycle: u32,
    pub extent: Extent,
}

impl ResourceTag {
    pub(crate) fn new(resource: *mut Resource, ty: u32, lifecycle: u32, extent: Extent) -> Self {
        Self {
            base: Base::new(RESOURCE_TAG, 1),
            resource,
            ty,
            lifecycle,
            extent,
        }
    }
}

/// `sl::float4x4`: four rows, row vectors (`v' = v M`, translation in row 3).
pub(crate) type Float4x4 = [[f32; 4]; 4];

pub(crate) const IDENTITY: Float4x4 = [
    [1.0, 0.0, 0.0, 0.0],
    [0.0, 1.0, 0.0, 0.0],
    [0.0, 0.0, 1.0, 0.0],
    [0.0, 0.0, 0.0, 1.0],
];

/// `sl::Constants` (version 2).
#[repr(C)]
#[derive(Debug)]
pub(crate) struct Constants {
    pub base: Base,
    pub camera_view_to_clip: Float4x4,
    pub clip_to_camera_view: Float4x4,
    pub clip_to_lens_clip: Float4x4,
    pub clip_to_prev_clip: Float4x4,
    pub prev_clip_to_clip: Float4x4,
    pub jitter_offset: [f32; 2],
    pub mvec_scale: [f32; 2],
    pub camera_pinhole_offset: [f32; 2],
    pub camera_pos: [f32; 3],
    pub camera_up: [f32; 3],
    pub camera_right: [f32; 3],
    pub camera_fwd: [f32; 3],
    pub camera_near: f32,
    pub camera_far: f32,
    pub camera_fov: f32,
    pub camera_aspect_ratio: f32,
    pub motion_vectors_invalid_value: f32,
    pub depth_inverted: u8,
    pub camera_motion_included: u8,
    pub motion_vectors_3d: u8,
    pub reset: u8,
    pub orthographic_projection: u8,
    pub motion_vectors_dilated: u8,
    pub motion_vectors_jittered: u8,
    pub min_relative_linear_depth_object_separation: f32,
}

impl Constants {
    /// The header's defaults.
    pub(crate) fn new() -> Self {
        Self {
            base: Base::new(CONSTANTS, 2),
            camera_view_to_clip: IDENTITY,
            clip_to_camera_view: IDENTITY,
            clip_to_lens_clip: IDENTITY,
            clip_to_prev_clip: IDENTITY,
            prev_clip_to_clip: IDENTITY,
            jitter_offset: [0.0; 2],
            mvec_scale: [1.0; 2],
            camera_pinhole_offset: [0.0; 2],
            camera_pos: [0.0; 3],
            camera_up: [0.0, 1.0, 0.0],
            camera_right: [1.0, 0.0, 0.0],
            camera_fwd: [0.0, 0.0, 1.0],
            camera_near: INVALID_FLOAT,
            camera_far: INVALID_FLOAT,
            camera_fov: INVALID_FLOAT,
            camera_aspect_ratio: INVALID_FLOAT,
            motion_vectors_invalid_value: INVALID_FLOAT,
            depth_inverted: INVALID,
            camera_motion_included: INVALID,
            motion_vectors_3d: INVALID,
            reset: INVALID,
            orthographic_projection: FALSE,
            motion_vectors_dilated: FALSE,
            motion_vectors_jittered: FALSE,
            min_relative_linear_depth_object_separation: 40.0,
        }
    }
}

/// `sl::ViewportHandle` (version 1).
#[repr(C)]
pub(crate) struct ViewportHandle {
    pub base: Base,
    pub value: u32,
}

impl ViewportHandle {
    pub(crate) fn new(value: u32) -> Self {
        Self {
            base: Base::new(VIEWPORT_HANDLE, 1),
            value,
        }
    }
}

/// `sl::AdapterInfo` (version 1).
#[repr(C)]
pub(crate) struct AdapterInfo {
    pub base: Base,
    pub device_luid: *const u8,
    pub device_luid_size: u32,
    pub vk_physical_device: *mut c_void,
}

impl AdapterInfo {
    pub(crate) fn luid(luid: &[u8; 8]) -> Self {
        Self {
            base: Base::new(ADAPTER_INFO, 1),
            device_luid: luid.as_ptr(),
            device_luid_size: 8,
            vk_physical_device: std::ptr::null_mut(),
        }
    }
}

/// `sl::Version`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Version {
    pub major: u32,
    pub minor: u32,
    pub build: u32,
}

/// `sl::FeatureVersion` (version 1).
#[repr(C)]
pub(crate) struct FeatureVersion {
    pub base: Base,
    pub sl: Version,
    pub ngx: Version,
}

impl FeatureVersion {
    pub(crate) fn new() -> Self {
        Self {
            base: Base::new(FEATURE_VERSION, 1),
            sl: Version::default(),
            ngx: Version::default(),
        }
    }
}

/// `sl::DLSSGOptions` (version 5).
#[repr(C)]
pub(crate) struct DlssgOptions {
    pub base: Base,
    pub mode: u32,
    pub num_frames_to_generate: u32,
    pub flags: u32,
    pub dynamic_res_width: u32,
    pub dynamic_res_height: u32,
    pub num_back_buffers: u32,
    pub mvec_depth_width: u32,
    pub mvec_depth_height: u32,
    pub color_width: u32,
    pub color_height: u32,
    pub color_buffer_format: u32,
    pub mvec_buffer_format: u32,
    pub depth_buffer_format: u32,
    pub hudless_buffer_format: u32,
    pub ui_buffer_format: u32,
    pub on_error_callback: *const c_void,
    pub reserved15: u8,
    pub queue_parallelism_mode: u32,
    pub enable_user_interface_recomposition: u8,
    pub dynamic_target_frame_rate: f32,
}

impl DlssgOptions {
    pub(crate) fn new() -> Self {
        Self {
            base: Base::new(DLSSG_OPTIONS, 5),
            mode: DLSSG_MODE_OFF,
            num_frames_to_generate: 1,
            flags: 0,
            dynamic_res_width: 0,
            dynamic_res_height: 0,
            num_back_buffers: 0,
            mvec_depth_width: 0,
            mvec_depth_height: 0,
            color_width: 0,
            color_height: 0,
            color_buffer_format: 0,
            mvec_buffer_format: 0,
            depth_buffer_format: 0,
            hudless_buffer_format: 0,
            ui_buffer_format: 0,
            on_error_callback: std::ptr::null(),
            reserved15: INVALID,
            queue_parallelism_mode: 0,
            enable_user_interface_recomposition: FALSE,
            dynamic_target_frame_rate: 0.0,
        }
    }
}

/// `sl::DLSSGState` (version 4).
#[repr(C)]
pub(crate) struct DlssgState {
    pub base: Base,
    pub estimated_vram_usage_in_bytes: u64,
    pub status: u32,
    pub min_width_or_height: u32,
    pub num_frames_actually_presented: u32,
    pub num_frames_to_generate_max: u32,
    pub reserved4: u8,
    pub is_vsync_support_available: u8,
    pub inputs_processing_completion_fence: *mut c_void,
    pub last_present_inputs_processing_completion_fence_value: u64,
    pub is_dynamic_mfg_supported: u8,
}

impl DlssgState {
    pub(crate) fn new() -> Self {
        Self {
            base: Base::new(DLSSG_STATE, 4),
            estimated_vram_usage_in_bytes: 0,
            status: 0,
            min_width_or_height: 0,
            num_frames_actually_presented: 0,
            num_frames_to_generate_max: 0,
            reserved4: 0,
            is_vsync_support_available: 0,
            inputs_processing_completion_fence: std::ptr::null_mut(),
            last_present_inputs_processing_completion_fence_value: 0,
            is_dynamic_mfg_supported: 0,
        }
    }
}

/// `sl::DLSSGStatus` bits as words, for the log.
pub(crate) fn dlssg_status(status: u32) -> String {
    const NAMES: [&str; 6] = [
        "resolution too low",
        "Reflex not detected",
        "HDR format not supported",
        "common constants invalid",
        "GetCurrentBackBufferIndex not called",
        "reserved",
    ];
    if status == 0 {
        return "ok".to_owned();
    }
    NAMES
        .iter()
        .enumerate()
        .filter(|(i, _)| status & (1 << i) != 0)
        .map(|(_, n)| *n)
        .collect::<Vec<_>>()
        .join(", ")
}

/// `sl::ReflexOptions` (version 1).
#[repr(C)]
pub(crate) struct ReflexOptions {
    pub base: Base,
    pub mode: i32,
    pub frame_limit_us: u32,
    pub use_markers_to_optimize: bool,
    pub virtual_key: u16,
    pub id_thread: u32,
}

impl ReflexOptions {
    pub(crate) fn new(mode: i32) -> Self {
        Self {
            base: Base::new(REFLEX_OPTIONS, 1),
            mode,
            frame_limit_us: 0,
            use_markers_to_optimize: false,
            virtual_key: 0,
            id_thread: 0,
        }
    }
}

/// A `sl::FrameToken*` (opaque; Streamline owns it).
pub(crate) type FrameToken = *const c_void;

type Init = unsafe extern "C" fn(pref: *const Preferences, sdk_version: u64) -> i32;
type Shutdown = unsafe extern "C" fn() -> i32;
type IsFeatureSupported = unsafe extern "C" fn(feature: u32, adapter: *const AdapterInfo) -> i32;
type IsFeatureLoaded = unsafe extern "C" fn(feature: u32, loaded: *mut bool) -> i32;
type SetD3DDevice = unsafe extern "C" fn(device: *mut c_void) -> i32;
type UpgradeInterface = unsafe extern "C" fn(interface: *mut *mut c_void) -> i32;
type GetFeatureFunction =
    unsafe extern "C" fn(feature: u32, name: *const c_char, function: *mut *mut c_void) -> i32;
type GetFeatureVersion = unsafe extern "C" fn(feature: u32, version: *mut FeatureVersion) -> i32;
type GetNewFrameToken = unsafe extern "C" fn(token: *mut FrameToken, index: *const u32) -> i32;
type SetConstants = unsafe extern "C" fn(
    values: *const Constants,
    frame: FrameToken,
    viewport: *const ViewportHandle,
) -> i32;
type SetTagForFrame = unsafe extern "C" fn(
    frame: FrameToken,
    viewport: *const ViewportHandle,
    tags: *const ResourceTag,
    count: u32,
    list: *mut c_void,
) -> i32;

/// The core exports of `sl.interposer.dll`.
pub(crate) struct Api {
    pub path: PathBuf,
    pub init: Init,
    pub shutdown: Shutdown,
    pub is_feature_supported: IsFeatureSupported,
    pub is_feature_loaded: IsFeatureLoaded,
    pub set_d3d_device: SetD3DDevice,
    pub upgrade_interface: UpgradeInterface,
    pub get_feature_function: GetFeatureFunction,
    pub get_feature_version: GetFeatureVersion,
    pub get_new_frame_token: GetNewFrameToken,
    pub set_constants: SetConstants,
    pub set_tag_for_frame: SetTagForFrame,
}

// SAFETY: plain function pointers into a module that is never unloaded.
unsafe impl Send for Api {}
unsafe impl Sync for Api {}

pub(crate) type DlssgSetOptions =
    unsafe extern "C" fn(viewport: *const ViewportHandle, options: *const DlssgOptions) -> i32;
pub(crate) type DlssgGetState = unsafe extern "C" fn(
    viewport: *const ViewportHandle,
    state: *mut DlssgState,
    options: *const DlssgOptions,
) -> i32;
pub(crate) type ReflexSetOptions = unsafe extern "C" fn(options: *const ReflexOptions) -> i32;
pub(crate) type ReflexSleep = unsafe extern "C" fn(frame: FrameToken) -> i32;
pub(crate) type PclSetMarker = unsafe extern "C" fn(marker: u32, frame: FrameToken) -> i32;

pub(crate) const INTERPOSER: &str = "sl.interposer.dll";

/// `VICTORY_STREAMLINE_DIR`, else the executable's folder.
pub(crate) fn dir() -> Option<PathBuf> {
    std::env::var_os("VICTORY_STREAMLINE_DIR")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::current_exe().ok()?.parent().map(PathBuf::from))
}

/// The DLLs DLSS-G needs beside the interposer, the first one missing.
pub(crate) fn missing_plugin(dir: &std::path::Path) -> Option<&'static str> {
    const NEEDED: [&str; 5] = [
        "sl.common.dll",
        "sl.dlss_g.dll",
        "sl.reflex.dll",
        "sl.pcl.dll",
        "nvngx_dlssg.dll",
    ];
    NEEDED.into_iter().find(|n| !dir.join(n).is_file())
}

fn load() -> Result<Api, String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::System::LibraryLoader::{
        GetProcAddress, LOAD_LIBRARY_SEARCH_DEFAULT_DIRS, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR,
        LoadLibraryExW,
    };
    let dir = dir().ok_or("no VICTORY_STREAMLINE_DIR")?;
    let path = dir.join(INTERPOSER);
    if !path.is_file() {
        return Err(format!("no {INTERPOSER} in {}", dir.display()));
    }
    if let Some(m) = missing_plugin(&dir) {
        return Err(format!("no {m} in {}", dir.display()));
    }
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
    // SAFETY: a full path; the interposer finds its plugins through the preferences.
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
        unsafe { GetProcAddress(module, name.as_ptr()) }.ok_or_else(|| {
            format!(
                "{} lacks {}",
                path.display(),
                String::from_utf8_lossy(&name[..name.len() - 1])
            )
        })
    };
    // SAFETY: the exports have the signatures of `sl_core_api.h`.
    unsafe {
        use std::mem::transmute as t;
        Ok(Api {
            init: t::<_, Init>(export(b"slInit\0")?),
            shutdown: t::<_, Shutdown>(export(b"slShutdown\0")?),
            is_feature_supported: t::<_, IsFeatureSupported>(export(b"slIsFeatureSupported\0")?),
            is_feature_loaded: t::<_, IsFeatureLoaded>(export(b"slIsFeatureLoaded\0")?),
            set_d3d_device: t::<_, SetD3DDevice>(export(b"slSetD3DDevice\0")?),
            upgrade_interface: t::<_, UpgradeInterface>(export(b"slUpgradeInterface\0")?),
            get_feature_function: t::<_, GetFeatureFunction>(export(b"slGetFeatureFunction\0")?),
            get_feature_version: t::<_, GetFeatureVersion>(export(b"slGetFeatureVersion\0")?),
            get_new_frame_token: t::<_, GetNewFrameToken>(export(b"slGetNewFrameToken\0")?),
            set_constants: t::<_, SetConstants>(export(b"slSetConstants\0")?),
            set_tag_for_frame: t::<_, SetTagForFrame>(export(b"slSetTagForFrame\0")?),
            path,
        })
    }
}

/// The interposer, loaded once.
pub(crate) fn api() -> Result<&'static Api, &'static str> {
    static API: OnceLock<Result<Api, String>> = OnceLock::new();
    API.get_or_init(load).as_ref().map_err(String::as_str)
}

impl Api {
    /// A feature's function by name (after `slSetD3DDevice`).
    pub(crate) fn feature_function(
        &self,
        feature: u32,
        name: &std::ffi::CStr,
    ) -> Result<*mut c_void, String> {
        let mut f = std::ptr::null_mut();
        // SAFETY: a NUL-terminated name and an out pointer.
        let r = unsafe { (self.get_feature_function)(feature, name.as_ptr(), &mut f) };
        if r != OK || f.is_null() {
            return Err(format!(
                "{} unavailable ({})",
                name.to_string_lossy(),
                result_name(r)
            ));
        }
        Ok(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{offset_of, size_of};

    /// Offsets as the SDK's C++ headers lay them out on x64.
    #[test]
    fn layouts_match_the_headers() {
        assert_eq!(size_of::<StructType>(), 16);
        assert_eq!(size_of::<Base>(), 32);
        assert_eq!(offset_of!(Base, ty), 8);
        assert_eq!(offset_of!(Base, version), 24);

        assert_eq!(offset_of!(Preferences, show_console), 32);
        assert_eq!(offset_of!(Preferences, log_level), 36);
        assert_eq!(offset_of!(Preferences, paths_to_plugins), 40);
        assert_eq!(offset_of!(Preferences, num_paths_to_plugins), 48);
        assert_eq!(offset_of!(Preferences, path_to_logs_and_data), 56);
        assert_eq!(offset_of!(Preferences, log_message_callback), 80);
        assert_eq!(offset_of!(Preferences, flags), 88);
        assert_eq!(offset_of!(Preferences, features_to_load), 96);
        assert_eq!(offset_of!(Preferences, num_features_to_load), 104);
        assert_eq!(offset_of!(Preferences, application_id), 108);
        assert_eq!(offset_of!(Preferences, engine), 112);
        assert_eq!(offset_of!(Preferences, engine_version), 120);
        assert_eq!(offset_of!(Preferences, project_id), 128);
        assert_eq!(offset_of!(Preferences, render_api), 136);
        assert_eq!(size_of::<Preferences>(), 144);

        assert_eq!(offset_of!(Resource, ty), 32);
        assert_eq!(offset_of!(Resource, native), 40);
        assert_eq!(offset_of!(Resource, view), 56);
        assert_eq!(offset_of!(Resource, state), 64);
        assert_eq!(offset_of!(Resource, array_layers), 84);
        assert_eq!(offset_of!(Resource, gpu_virtual_address), 88);
        assert_eq!(offset_of!(Resource, flags), 96);
        assert_eq!(offset_of!(Resource, usage), 100);
        assert_eq!(offset_of!(Resource, internal_flags), 104);
        assert_eq!(size_of::<Resource>(), 112);

        assert_eq!(size_of::<Extent>(), 16);
        assert_eq!(offset_of!(ResourceTag, resource), 32);
        assert_eq!(offset_of!(ResourceTag, ty), 40);
        assert_eq!(offset_of!(ResourceTag, lifecycle), 44);
        assert_eq!(offset_of!(ResourceTag, extent), 48);
        assert_eq!(size_of::<ResourceTag>(), 64);

        assert_eq!(offset_of!(Constants, camera_view_to_clip), 32);
        assert_eq!(offset_of!(Constants, prev_clip_to_clip), 288);
        assert_eq!(offset_of!(Constants, jitter_offset), 352);
        assert_eq!(offset_of!(Constants, mvec_scale), 360);
        assert_eq!(offset_of!(Constants, camera_pos), 376);
        assert_eq!(offset_of!(Constants, camera_fwd), 412);
        assert_eq!(offset_of!(Constants, camera_near), 424);
        assert_eq!(offset_of!(Constants, camera_aspect_ratio), 436);
        assert_eq!(offset_of!(Constants, motion_vectors_invalid_value), 440);
        assert_eq!(offset_of!(Constants, depth_inverted), 444);
        assert_eq!(offset_of!(Constants, reset), 447);
        assert_eq!(offset_of!(Constants, motion_vectors_jittered), 450);
        assert_eq!(
            offset_of!(Constants, min_relative_linear_depth_object_separation),
            452
        );
        assert_eq!(size_of::<Constants>(), 456);

        assert_eq!(offset_of!(ViewportHandle, value), 32);
        assert_eq!(size_of::<ViewportHandle>(), 40);
        assert_eq!(offset_of!(AdapterInfo, device_luid_size), 40);
        assert_eq!(size_of::<AdapterInfo>(), 56);
        assert_eq!(offset_of!(FeatureVersion, ngx), 44);
        assert_eq!(size_of::<FeatureVersion>(), 56);

        assert_eq!(offset_of!(DlssgOptions, mode), 32);
        assert_eq!(offset_of!(DlssgOptions, num_frames_to_generate), 36);
        assert_eq!(offset_of!(DlssgOptions, num_back_buffers), 52);
        assert_eq!(offset_of!(DlssgOptions, color_width), 64);
        assert_eq!(offset_of!(DlssgOptions, ui_buffer_format), 88);
        assert_eq!(offset_of!(DlssgOptions, on_error_callback), 96);
        assert_eq!(offset_of!(DlssgOptions, reserved15), 104);
        assert_eq!(offset_of!(DlssgOptions, queue_parallelism_mode), 108);
        assert_eq!(
            offset_of!(DlssgOptions, enable_user_interface_recomposition),
            112
        );
        assert_eq!(offset_of!(DlssgOptions, dynamic_target_frame_rate), 116);
        assert_eq!(size_of::<DlssgOptions>(), 120);

        assert_eq!(offset_of!(DlssgState, status), 40);
        assert_eq!(offset_of!(DlssgState, num_frames_actually_presented), 48);
        assert_eq!(offset_of!(DlssgState, num_frames_to_generate_max), 52);
        assert_eq!(offset_of!(DlssgState, is_vsync_support_available), 57);
        assert_eq!(
            offset_of!(DlssgState, inputs_processing_completion_fence),
            64
        );
        assert_eq!(offset_of!(DlssgState, is_dynamic_mfg_supported), 80);
        assert_eq!(size_of::<DlssgState>(), 88);

        assert_eq!(offset_of!(ReflexOptions, mode), 32);
        assert_eq!(offset_of!(ReflexOptions, frame_limit_us), 36);
        assert_eq!(offset_of!(ReflexOptions, use_markers_to_optimize), 40);
        assert_eq!(offset_of!(ReflexOptions, virtual_key), 42);
        assert_eq!(offset_of!(ReflexOptions, id_thread), 44);
        assert_eq!(size_of::<ReflexOptions>(), 48);
    }

    #[test]
    fn versions_names_and_defaults() {
        assert_eq!(SDK_VERSION, 0x0002_000e_0010_fedc);
        assert_eq!(result_name(0), "eOk");
        assert_eq!(result_name(32), "eErrorFeatureNotSupported");
        assert_eq!(result_name(39), "eWarnOutOfVRAM");
        assert_eq!(result_name(-1), "unknown");
        assert_eq!(dlssg_status(0), "ok");
        assert_eq!(
            dlssg_status(0b1010),
            "Reflex not detected, common constants invalid"
        );
        assert_eq!(Constants::new().base.version, 2);
        assert_eq!(DlssgOptions::new().base.version, 5);
        assert_eq!(DlssgState::new().base.version, 4);
        assert_eq!(DlssgOptions::new().reserved15, INVALID);
        assert_eq!(DLSSG_OPTIONS.data1, 0xfac5_f1cb);
        assert_eq!(CONSTANTS.data4[1], 0x0c);
    }

    #[test]
    fn missing_dir_and_plugins_are_named() {
        let dir = std::env::temp_dir().join("victory-sl-test-empty");
        let _ = std::fs::create_dir_all(&dir);
        assert_eq!(missing_plugin(&dir), Some("sl.common.dll"));
    }
}
