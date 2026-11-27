//! NVIDIA NGX through the driver's own core (`_nvngx.dll`), loaded at runtime:
//! no SDK and no NVIDIA code in the build (the SDK's static loader does the
//! same lookup and calls these same exports).
//!
//! The core's folder is `HKLM\SOFTWARE\NVIDIA Corporation\Global\NGXCore`'s
//! `FullPath` (the driver store). Its exports take the SDK's public arguments
//! except `Init_ProjectID`, which takes the SDK version before the feature
//! info. A parameter object is the SDK's `NVSDK_NGX_Parameter`, a C++ class
//! with only virtual methods: MSVC groups overloads and lays each group out in
//! reverse order of declaration, so the slots below. `tests::ngx_probe` (run
//! by hand on an NVIDIA card) pins all of it against the installed driver.

use std::ffi::{c_char, c_int, c_uint, c_void};
use std::path::PathBuf;
use std::sync::OnceLock;

pub(crate) type NgxResult = c_int;
pub(crate) const SUCCESS: NgxResult = 1;
pub(crate) const FEATURE_SUPER_SAMPLING: c_int = 1;
pub(crate) const ENGINE_CUSTOM: c_int = 0;
/// `NVSDK_NGX_Version_API` of the DLSS 3.x/4.x SDK.
pub(crate) const VERSION_API: c_int = 0x15;

#[repr(C)]
pub(crate) struct PathList {
    pub path: *const *const u16,
    pub length: c_uint,
}

#[repr(C)]
pub(crate) struct Logging {
    pub callback: *const c_void,
    pub minimum: c_int,
    pub disable_other_sinks: bool,
}

/// `NVSDK_NGX_FeatureCommonInfo`.
#[repr(C)]
pub(crate) struct CommonInfo {
    pub paths: PathList,
    pub internal: *const c_void,
    pub logging: Logging,
}

type InitProjectId = unsafe extern "C" fn(
    project: *const c_char,
    engine: c_int,
    engine_version: *const c_char,
    data_path: *const u16,
    device: *mut c_void,
    version: c_int,
    info: *const CommonInfo,
) -> NgxResult;
type GetParams = unsafe extern "C" fn(out: *mut *mut c_void) -> NgxResult;
type CreateFeature = unsafe extern "C" fn(
    list: *mut c_void,
    feature: c_int,
    params: *mut c_void,
    out: *mut *mut c_void,
) -> NgxResult;
type ReleaseFeature = unsafe extern "C" fn(handle: *mut c_void) -> NgxResult;
type EvaluateFeature = unsafe extern "C" fn(
    list: *mut c_void,
    handle: *const c_void,
    params: *mut c_void,
    callback: *const c_void,
) -> NgxResult;

/// The core's exports this crate calls.
pub(crate) struct Core {
    pub dir: PathBuf,
    pub init_project_id: InitProjectId,
    pub get_capability_parameters: GetParams,
    pub create_feature: CreateFeature,
    pub release_feature: ReleaseFeature,
    pub evaluate_feature: EvaluateFeature,
}

/// The driver's NGX core folder, from the registry.
pub(crate) fn core_dir() -> Option<PathBuf> {
    crate::frametime::registry_string(r"SOFTWARE\NVIDIA Corporation\Global\NGXCore", "FullPath")
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
}

/// The core's file names, in the order the SDK's loader tries them.
pub(crate) const CORE_NAMES: [&str; 2] = ["_nvngx.dll", "nvngx.dll"];

fn load() -> Result<Core, String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::System::LibraryLoader::{
        GetProcAddress, LOAD_LIBRARY_SEARCH_DEFAULT_DIRS, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR,
        LoadLibraryExW,
    };
    let dir = core_dir().ok_or("no NGXCore FullPath in the registry (not an NVIDIA driver)")?;
    let path = CORE_NAMES
        .iter()
        .map(|n| dir.join(n))
        .find(|p| p.is_file())
        .ok_or_else(|| format!("no _nvngx.dll in {}", dir.display()))?;
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
    // SAFETY: a full path to the driver's own signed DLL.
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
    let export = |name: &str| -> Result<unsafe extern "system" fn() -> isize, String> {
        let c: Vec<u8> = name.bytes().chain([0]).collect();
        // SAFETY: a loaded module and a NUL-terminated name.
        unsafe { GetProcAddress(module, c.as_ptr()) }
            .ok_or_else(|| format!("{} has no {name}", path.display()))
    };
    // SAFETY: each export has the signature the SDK's loader calls it with.
    unsafe {
        Ok(Core {
            init_project_id: std::mem::transmute::<_, InitProjectId>(export(
                "NVSDK_NGX_D3D12_Init_ProjectID",
            )?),
            get_capability_parameters: std::mem::transmute::<_, GetParams>(export(
                "NVSDK_NGX_D3D12_GetCapabilityParameters",
            )?),
            create_feature: std::mem::transmute::<_, CreateFeature>(export(
                "NVSDK_NGX_D3D12_CreateFeature",
            )?),
            release_feature: std::mem::transmute::<_, ReleaseFeature>(export(
                "NVSDK_NGX_D3D12_ReleaseFeature",
            )?),
            evaluate_feature: std::mem::transmute::<_, EvaluateFeature>(export(
                "NVSDK_NGX_D3D12_EvaluateFeature",
            )?),
            dir: path,
        })
    }
}

/// The driver's NGX core, loaded once; `Err` says why there is none.
pub(crate) fn core() -> Result<&'static Core, &'static str> {
    static CORE: OnceLock<Result<Core, String>> = OnceLock::new();
    CORE.get_or_init(load).as_ref().map_err(String::as_str)
}

/// `NVSDK_NGX_Parameter`'s vtable slots (MSVC layout of the SDK's class).
mod slot {
    pub const SET_D3D12_RESOURCE: usize = 1;
    pub const SET_I: usize = 3;
    pub const SET_UI: usize = 4;
    pub const SET_F: usize = 6;
    pub const GET_D3D12_RESOURCE: usize = 9;
    pub const GET_I: usize = 11;
    pub const GET_UI: usize = 12;
    pub const GET_F: usize = 14;
}

/// A parameter object the core made.
#[derive(Clone, Copy)]
pub(crate) struct Params(pub *mut c_void);

impl Params {
    /// # Safety
    /// `self.0` is a live parameter object; `slot` holds a method of type `F`.
    unsafe fn method<F: Copy>(self, slot: usize) -> F {
        // SAFETY: the caller's promise; a C++ object starts with its vtable.
        unsafe {
            let vtable = *(self.0 as *const *const usize);
            std::mem::transmute_copy::<usize, F>(&*vtable.add(slot))
        }
    }

    /// # Safety
    /// A live parameter object; `name` NUL-terminated.
    pub(crate) unsafe fn set_ui(self, name: *const c_char, v: c_uint) {
        type F = unsafe extern "system" fn(*mut c_void, *const c_char, c_uint);
        unsafe { self.method::<F>(slot::SET_UI)(self.0, name, v) }
    }

    /// # Safety
    /// As [`Self::set_ui`].
    pub(crate) unsafe fn set_i(self, name: *const c_char, v: c_int) {
        type F = unsafe extern "system" fn(*mut c_void, *const c_char, c_int);
        unsafe { self.method::<F>(slot::SET_I)(self.0, name, v) }
    }

    /// # Safety
    /// As [`Self::set_ui`].
    pub(crate) unsafe fn set_f(self, name: *const c_char, v: f32) {
        type F = unsafe extern "system" fn(*mut c_void, *const c_char, f32);
        unsafe { self.method::<F>(slot::SET_F)(self.0, name, v) }
    }

    /// # Safety
    /// As [`Self::set_ui`]; `v` a live `ID3D12Resource` or null.
    pub(crate) unsafe fn set_resource(self, name: *const c_char, v: *mut c_void) {
        type F = unsafe extern "system" fn(*mut c_void, *const c_char, *mut c_void);
        unsafe { self.method::<F>(slot::SET_D3D12_RESOURCE)(self.0, name, v) }
    }

    /// # Safety
    /// As [`Self::set_ui`].
    pub(crate) unsafe fn get_i(self, name: *const c_char) -> Option<c_int> {
        type F = unsafe extern "system" fn(*mut c_void, *const c_char, *mut c_int) -> NgxResult;
        let mut v = 0;
        let r = unsafe { self.method::<F>(slot::GET_I)(self.0, name, &mut v) };
        (r == SUCCESS).then_some(v)
    }

    /// # Safety
    /// As [`Self::set_ui`].
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) unsafe fn get_ui(self, name: *const c_char) -> Option<c_uint> {
        type F = unsafe extern "system" fn(*mut c_void, *const c_char, *mut c_uint) -> NgxResult;
        let mut v = 0;
        let r = unsafe { self.method::<F>(slot::GET_UI)(self.0, name, &mut v) };
        (r == SUCCESS).then_some(v)
    }

    /// # Safety
    /// As [`Self::set_ui`].
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) unsafe fn get_f(self, name: *const c_char) -> Option<f32> {
        type F = unsafe extern "system" fn(*mut c_void, *const c_char, *mut f32) -> NgxResult;
        let mut v = 0.0;
        let r = unsafe { self.method::<F>(slot::GET_F)(self.0, name, &mut v) };
        (r == SUCCESS).then_some(v)
    }

    /// # Safety
    /// As [`Self::set_ui`].
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) unsafe fn get_resource(self, name: *const c_char) -> Option<*mut c_void> {
        type F =
            unsafe extern "system" fn(*mut c_void, *const c_char, *mut *mut c_void) -> NgxResult;
        let mut v = std::ptr::null_mut();
        let r = unsafe { self.method::<F>(slot::GET_D3D12_RESOURCE)(self.0, name, &mut v) };
        (r == SUCCESS).then_some(v)
    }
}

/// A parameter name as the SDK spells it.
macro_rules! name {
    ($s:literal) => {
        concat!($s, "\0").as_ptr().cast::<std::ffi::c_char>()
    };
}
pub(crate) use name;

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
    use windows::Win32::Graphics::Direct3D12::*;
    use windows::Win32::Graphics::Dxgi::Common::*;
    use windows::Win32::System::Threading::{CreateEventW, INFINITE, WaitForSingleObject};
    use windows::core::Interface;

    #[test]
    fn slots_follow_msvc_reverse_overload_order() {
        // Declared: Set(u64, f32, f64, u32, i32, D3D11*, D3D12*, void*), then
        // Get in the same order, then Reset.
        assert_eq!(slot::SET_F, 7 - 1);
        assert_eq!(slot::SET_UI, 7 - 3);
        assert_eq!(slot::SET_I, 7 - 4);
        assert_eq!(slot::SET_D3D12_RESOURCE, 7 - 6);
        assert_eq!(slot::GET_F, 8 + slot::SET_F);
        assert_eq!(slot::GET_UI, 8 + slot::SET_UI);
        assert_eq!(slot::GET_I, 8 + slot::SET_I);
        assert_eq!(slot::GET_D3D12_RESOURCE, 8 + slot::SET_D3D12_RESOURCE);
    }

    fn texture(
        device: &ID3D12Device,
        w: u32,
        h: u32,
        format: DXGI_FORMAT,
        uav: bool,
    ) -> ID3D12Resource {
        let mut r: Option<ID3D12Resource> = None;
        // SAFETY: plain resource creation.
        unsafe {
            device
                .CreateCommittedResource(
                    &D3D12_HEAP_PROPERTIES {
                        Type: D3D12_HEAP_TYPE_DEFAULT,
                        ..Default::default()
                    },
                    D3D12_HEAP_FLAG_NONE,
                    &D3D12_RESOURCE_DESC {
                        Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
                        Width: u64::from(w),
                        Height: h,
                        DepthOrArraySize: 1,
                        MipLevels: 1,
                        Format: format,
                        SampleDesc: DXGI_SAMPLE_DESC {
                            Count: 1,
                            Quality: 0,
                        },
                        Flags: if uav {
                            D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS
                        } else {
                            D3D12_RESOURCE_FLAG_NONE
                        },
                        ..Default::default()
                    },
                    if uav {
                        D3D12_RESOURCE_STATE_UNORDERED_ACCESS
                    } else {
                        D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE
                    },
                    None,
                    &mut r,
                )
                .unwrap();
        }
        r.unwrap()
    }

    /// The microbenchmark that pins the core's exports and the parameter
    /// slots on this machine's driver (not the game; take the `gpu` lock):
    /// `cargo test -p victory-gpu ngx_probe -- --ignored --nocapture`.
    /// `VICTORY_DLSS_DIR` adds a feature folder, as `dlss.rs` does.
    #[test]
    #[ignore = "needs an NVIDIA card and driver; run by hand"]
    fn ngx_probe() {
        let core = core().expect("NGX core");
        println!("core: {}", core.dir.display());
        // SAFETY: plain D3D12 object creation and NGX calls with live objects.
        unsafe {
            let mut device: Option<ID3D12Device> = None;
            windows::Win32::Graphics::Direct3D12::D3D12CreateDevice(
                None,
                D3D_FEATURE_LEVEL_12_0,
                &mut device,
            )
            .unwrap();
            let device = device.unwrap();
            let data: Vec<u16> = std::env::temp_dir()
                .join("victory-ngx")
                .as_os_str()
                .encode_wide_nul();
            let _ = std::fs::create_dir_all(std::env::temp_dir().join("victory-ngx"));
            let dir: Option<Vec<u16>> =
                std::env::var_os("VICTORY_DLSS_DIR").map(|d| d.as_os_str().encode_wide_nul());
            let paths: Vec<*const u16> = dir.iter().map(|d| d.as_ptr()).collect();
            let info = CommonInfo {
                paths: PathList {
                    path: if paths.is_empty() {
                        std::ptr::null()
                    } else {
                        paths.as_ptr()
                    },
                    length: paths.len() as c_uint,
                },
                internal: std::ptr::null(),
                logging: Logging {
                    callback: std::ptr::null(),
                    minimum: 0,
                    disable_other_sinks: false,
                },
            };
            let r = (core.init_project_id)(
                c"5e2f9a3c-1b7d-4c8e-9a6f-0d3b2c1e4f57".as_ptr(),
                ENGINE_CUSTOM,
                c"1.0".as_ptr(),
                data.as_ptr(),
                device.as_raw(),
                VERSION_API,
                &info,
            );
            println!("Init_ProjectID: {r:#x}");
            assert_eq!(r, SUCCESS);
            let mut p = std::ptr::null_mut();
            let r = (core.get_capability_parameters)(&mut p);
            println!("GetCapabilityParameters: {r:#x}");
            assert_eq!(r, SUCCESS);
            let p = Params(p);
            let available = p.get_i(name!("SuperSampling.Available"));
            let needs_driver = p.get_i(name!("SuperSampling.NeedsUpdatedDriver"));
            println!("SuperSampling.Available={available:?} NeedsUpdatedDriver={needs_driver:?}");
            assert_eq!(available, Some(1));
            p.set_ui(name!("Victory.Probe.UI"), 1234);
            p.set_i(name!("Victory.Probe.I"), -7);
            p.set_f(name!("Victory.Probe.F"), 2.5);
            let colour = texture(&device, 960, 540, DXGI_FORMAT_R16G16B16A16_FLOAT, false);
            p.set_resource(name!("Victory.Probe.R"), colour.as_raw());
            let got = (
                p.get_ui(name!("Victory.Probe.UI")),
                p.get_i(name!("Victory.Probe.I")),
                p.get_f(name!("Victory.Probe.F")),
                p.get_resource(name!("Victory.Probe.R")) == Some(colour.as_raw()),
            );
            println!("round trips: {got:?}");
            assert_eq!(got, (Some(1234), Some(-7), Some(2.5), true));

            let queue: ID3D12CommandQueue = device
                .CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                    Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
                    ..Default::default()
                })
                .unwrap();
            let alloc: ID3D12CommandAllocator = device
                .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
                .unwrap();
            let list: ID3D12GraphicsCommandList = device
                .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &alloc, None)
                .unwrap();
            let depth = texture(&device, 960, 540, DXGI_FORMAT_R32_FLOAT, false);
            let motion = texture(&device, 960, 540, DXGI_FORMAT_R16G16_FLOAT, false);
            let output = texture(&device, 1920, 1080, DXGI_FORMAT_R16G16B16A16_FLOAT, true);
            p.set_ui(name!("CreationNodeMask"), 1);
            p.set_ui(name!("VisibilityNodeMask"), 1);
            p.set_ui(name!("Width"), 960);
            p.set_ui(name!("Height"), 540);
            p.set_ui(name!("OutWidth"), 1920);
            p.set_ui(name!("OutHeight"), 1080);
            p.set_i(name!("PerfQualityValue"), 0);
            p.set_i(
                name!("DLSS.Feature.Create.Flags"),
                crate::upscale::DLSS_FLAG_HDR | crate::upscale::DLSS_FLAG_DEPTH_INVERTED | (1 << 6),
            );
            p.set_i(name!("DLSS.Enable.Output.Subrects"), 0);
            let mut handle = std::ptr::null_mut();
            let r = (core.create_feature)(list.as_raw(), FEATURE_SUPER_SAMPLING, p.0, &mut handle);
            println!("CreateFeature(DLSS 960x540 -> 1920x1080): {r:#x}");
            assert_eq!(r, SUCCESS);
            p.set_resource(name!("Color"), colour.as_raw());
            p.set_resource(name!("Output"), output.as_raw());
            p.set_resource(name!("Depth"), depth.as_raw());
            p.set_resource(name!("MotionVectors"), motion.as_raw());
            p.set_resource(name!("ExposureTexture"), std::ptr::null_mut());
            p.set_f(name!("Jitter.Offset.X"), 0.25);
            p.set_f(name!("Jitter.Offset.Y"), -0.25);
            p.set_f(name!("MV.Scale.X"), 1.0);
            p.set_f(name!("MV.Scale.Y"), 1.0);
            p.set_i(name!("Reset"), 1);
            p.set_f(name!("FrameTimeDeltaInMsec"), 33.3);
            p.set_ui(name!("DLSS.Render.Subrect.Dimensions.Width"), 960);
            p.set_ui(name!("DLSS.Render.Subrect.Dimensions.Height"), 540);
            let mut times = Vec::new();
            for _ in 0..8 {
                let t = std::time::Instant::now();
                let r = (core.evaluate_feature)(list.as_raw(), handle, p.0, std::ptr::null());
                times.push(t.elapsed().as_micros());
                assert_eq!(r, SUCCESS, "EvaluateFeature: {r:#x}");
                p.set_i(name!("Reset"), 0);
            }
            println!("EvaluateFeature x8 ok, record us {times:?}");
            list.Close().unwrap();
            let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap();
            let t = std::time::Instant::now();
            queue.ExecuteCommandLists(&[Some(list.cast().unwrap())]);
            queue.Signal(&fence, 1).unwrap();
            let event: HANDLE = CreateEventW(None, false, false, None).unwrap();
            fence.SetEventOnCompletion(1, event).unwrap();
            WaitForSingleObject(event, INFINITE);
            println!(
                "GPU: 8 evaluations in {} us; device removed reason {:?}",
                t.elapsed().as_micros(),
                device.GetDeviceRemovedReason()
            );
            device.GetDeviceRemovedReason().unwrap();
            println!("ReleaseFeature: {:#x}", (core.release_feature)(handle));
        }
    }

    trait WideNul {
        fn encode_wide_nul(&self) -> Vec<u16>;
    }

    impl WideNul for std::ffi::OsStr {
        fn encode_wide_nul(&self) -> Vec<u16> {
            use std::os::windows::ffi::OsStrExt;
            self.encode_wide().chain([0]).collect()
        }
    }
}
