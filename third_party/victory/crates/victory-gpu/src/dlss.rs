//! NVIDIA DLSS in place of the title's FidelityFX upscale (`VICTORY_UPSCALE=dlss`).
//!
//! Ported from `Peppacodes/playablegames-peppa` (`b3c3766546`, from
//! `avatarmanz/VIctory-7900XTX` PR #203 by YeokiNoewChi).
//!
//! No SDK in the build: NGX is the driver's own core, loaded at runtime
//! (`crate::ngx`). `nvngx_dlss.dll` is looked for in `VICTORY_DLSS_DIR`, else
//! beside the executable, else the newest DLSS the driver keeps in its
//! model store (`%ProgramData%/NVIDIA/NGX/models/dlss`, copied once as
//! `%TEMP%/victory-ngx/dlss-<version>/nvngx_dlss.dll`). Any failure leaves the
//! caller to stretch.
//!
//! Switches: `VICTORY_DLSS_QUALITY` (0 performance .. 2 quality, 5 DLAA; 2 by
//! default), `VICTORY_DLSS_MV_SIGN=-1` (motion vectors the other way),
//! `VICTORY_DLSS_JITTER_SIGN=-1`, `VICTORY_DLSS_HDR=0|1` (default: by the colour
//! format), `VICTORY_DLSS_DEPTH_INVERTED=0|1` (default: by near > far),
//! `VICTORY_DLSS_SHARPNESS=<0..1>`, `VICTORY_DLSS_PRESET=default|e|f|j|k|l|m`
//! (the model for every quality mode; k is the transformer, live: a change
//! makes a new feature) and `VICTORY_DLSS_REACTIVE=1` (the title's reactive
//! mask as DLSS's current-colour bias mask, live). Preset and reactive mask
//! after readmet3xt-lab/foxhound `crates/victory-gpu/src/streamline.rs` and
//! `streamline/plan.rs` (its `VICTORY_DLSS_PRESET`, `VICTORY_DLSS_REACTIVE`),
//! on the NGX parameters here instead of Streamline's.

use std::ffi::{CString, c_int, c_void};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use windows::Win32::Graphics::Direct3D12::*;
use windows::core::Interface;

use crate::ngx::{
    CommonInfo, ENGINE_CUSTOM, FEATURE_SUPER_SAMPLING, Logging, Params, PathList, SUCCESS,
    VERSION_API, name,
};
use crate::upscale::{DLSS_DLL_NAME, dlss_create_flags, dlss_dll_present, resolve_dlss_dir};

/// What the title gives an upscale, as the host's resources.
pub(crate) struct Inputs<'a> {
    pub colour: &'a ID3D12Resource,
    pub depth: Option<&'a ID3D12Resource>,
    pub motion: Option<&'a ID3D12Resource>,
    pub exposure: Option<&'a ID3D12Resource>,
    /// The title's reactive mask (`ffxDispatchDescUpscale::reactive`).
    pub reactive: Option<&'a ID3D12Resource>,
    pub output: &'a ID3D12Resource,
    pub jitter: [f32; 2],
    pub motion_scale: [f32; 2],
    pub render: [u32; 2],
    pub reset: bool,
    pub pre_exposure: f32,
    pub near: f32,
    pub far: f32,
    pub frame_ms: f32,
}

struct Feature {
    handle: *mut c_void,
    /// render width and height the feature was made for, output size, flags,
    /// quality mode, preset
    key: [u32; 7],
}

struct Dlss {
    core: &'static crate::ngx::Core,
    params: Params,
    feature: Option<Feature>,
    /// Features replaced by a live change, released once no list uses them.
    retired: crate::upscale_live::Retired<usize>,
    /// A key a live change asked for that NGX refused: stretched, not retried.
    failed: Option<[u32; 7]>,
    /// Whether the last evaluation had a reactive mask (logged on a change).
    told_reactive: Option<bool>,
}

// SAFETY: the SDK's objects are used under the mutex only.
unsafe impl Send for Dlss {}

/// `None` until tried; `Some(None)`: unavailable, never tried again.
static DLSS: Mutex<Option<Option<Dlss>>> = Mutex::new(None);

fn env_f(name: &str, default: f32) -> f32 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default)
}

/// `VICTORY_DLSS_PRESET`: the `NVSDK_NGX_DLSS_Hint_Render_Preset` named
/// (0: the DLL's own choice per mode); `None` for a name it does not have.
pub(crate) fn preset_value(name: &str) -> Option<u32> {
    Some(match name.trim().to_ascii_lowercase().as_str() {
        "" | "default" | "0" => 0,
        "e" => 5,
        "f" => 6,
        "j" => 10,
        "k" => 11,
        "l" => 12,
        "m" => 13,
        _ => return None,
    })
}

fn preset_bits(value: Option<&str>) -> u64 {
    let value = value.unwrap_or("");
    match preset_value(value) {
        Some(p) => u64::from(p),
        None => {
            tracing::warn!(value, "DLSS: VICTORY_DLSS_PRESET is not one of default, e, f, j, k, l, m; the default model");
            0
        }
    }
}

static PRESET: victory_core::settings::Live = victory_core::settings::Live::new("VICTORY_DLSS_PRESET", preset_bits);
static REACTIVE: victory_core::settings::Live = victory_core::settings::Live::flag("VICTORY_DLSS_REACTIVE");

/// The preset asked for now (`VICTORY_DLSS_PRESET`, live).
pub(crate) fn preset() -> u32 {
    PRESET.get() as u32
}

/// Whether the title's reactive mask goes to DLSS (`VICTORY_DLSS_REACTIVE=1`,
/// live).
pub(crate) fn reactive() -> bool {
    REACTIVE.get_bool()
}

/// The NGX names of the per-mode preset hints: one preset for every mode.
const PRESET_HINTS: [*const std::ffi::c_char; 6] = [
    name!("DLSS.Hint.Render.Preset.DLAA"),
    name!("DLSS.Hint.Render.Preset.Quality"),
    name!("DLSS.Hint.Render.Preset.Balanced"),
    name!("DLSS.Hint.Render.Preset.Performance"),
    name!("DLSS.Hint.Render.Preset.UltraPerformance"),
    name!("DLSS.Hint.Render.Preset.UltraQuality"),
];

fn env_flag(name: &str) -> Option<bool> {
    std::env::var(name).ok().map(|v| v.trim() == "1")
}

fn wide(s: &std::ffi::OsStr) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    s.encode_wide().chain([0]).collect()
}

/// The newest DLSS in the driver's model store: `versions/<n>/files/*.bin`
/// (each a renamed `nvngx_dlss.dll`), with its version number.
pub(crate) fn newest_in_store(store: &Path) -> Option<(u64, PathBuf)> {
    std::fs::read_dir(store.join("versions"))
        .ok()?
        .flatten()
        .filter_map(|v| {
            let n: u64 = v.file_name().to_str()?.parse().ok()?;
            let bin = std::fs::read_dir(v.path().join("files"))
                .ok()?
                .flatten()
                .map(|f| f.path())
                .find(|f| f.extension().is_some_and(|x| x == "bin") && f.is_file())?;
            Some((n, bin))
        })
        .max_by_key(|(n, _)| *n)
}

/// A folder holding the driver store's newest `nvngx_dlss.dll`, copied under
/// `data` once per version.
fn from_driver_store(data: &Path) -> Option<PathBuf> {
    let store = PathBuf::from(std::env::var_os("ProgramData")?).join("NVIDIA/NGX/models/dlss");
    let (version, bin) = newest_in_store(&store)?;
    let dir = data.join(format!("dlss-{version}"));
    if !dlss_dll_present(&dir) {
        std::fs::create_dir_all(&dir).ok()?;
        let tmp = dir.join("nvngx_dlss.dll.part");
        std::fs::copy(&bin, &tmp).ok()?;
        std::fs::rename(&tmp, dir.join(DLSS_DLL_NAME)).ok()?;
    }
    tracing::info!(store = %bin.display(), version, "DLSS: using the driver store's nvngx_dlss.dll");
    Some(dir)
}

fn init(device: &ID3D12Device) -> Option<Dlss> {
    let core = match crate::ngx::core() {
        Ok(c) => c,
        Err(why) => {
            tracing::warn!(why, "DLSS: no NGX core; the upscale stays a stretch");
            return None;
        }
    };
    let data = std::env::temp_dir().join("victory-ngx");
    let env_dir = std::env::var_os("VICTORY_DLSS_DIR");
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()));
    let dir = resolve_dlss_dir(env_dir.as_deref(), exe_dir.as_deref())
        .filter(|d| dlss_dll_present(d))
        .or_else(|| from_driver_store(&data));
    let Some(dir) = dir else {
        tracing::warn!(
            "DLSS: no nvngx_dlss.dll (VICTORY_DLSS_DIR, the executable's folder, the driver store); the upscale stays a stretch"
        );
        return None;
    };
    let _ = std::fs::create_dir_all(&data);
    let dir_w = wide(dir.as_os_str());
    let data_w = wide(data.as_os_str());
    let paths = [dir_w.as_ptr()];
    let info = CommonInfo {
        paths: PathList {
            path: paths.as_ptr(),
            length: 1,
        },
        internal: std::ptr::null(),
        logging: Logging {
            callback: std::ptr::null(),
            minimum: 0,
            disable_other_sinks: false,
        },
    };
    let project = CString::new("5e2f9a3c-1b7d-4c8e-9a6f-0d3b2c1e4f57").ok()?;
    let version = CString::new("1.0").ok()?;
    // SAFETY: live strings and device; the core copies what it keeps.
    let r = unsafe {
        (core.init_project_id)(
            project.as_ptr(),
            ENGINE_CUSTOM,
            version.as_ptr(),
            data_w.as_ptr(),
            device.as_raw(),
            VERSION_API,
            &info,
        )
    };
    if r != SUCCESS {
        tracing::warn!(
            result = format_args!("{r:#x}"),
            dir = %dir.display(),
            "DLSS: NGX did not start; the upscale stays a stretch"
        );
        return None;
    }
    let mut params = std::ptr::null_mut();
    // SAFETY: an out pointer.
    let r = unsafe { (core.get_capability_parameters)(&mut params) };
    if r != SUCCESS || params.is_null() {
        tracing::warn!(
            result = format_args!("{r:#x}"),
            "DLSS: no capability parameters"
        );
        return None;
    }
    let params = Params(params);
    // SAFETY: live parameters.
    let available = unsafe { params.get_i(name!("SuperSampling.Available")) }.unwrap_or(0);
    if available == 0 {
        tracing::warn!("DLSS: super sampling is not available on this card or driver");
        return None;
    }
    tracing::info!(
        dir = %dir.display(),
        core = %core.dir.display(),
        "DLSS: started; the title's upscale is DLSS (VICTORY_UPSCALE=dlss)"
    );
    Some(Dlss {
        core,
        params,
        feature: None,
        retired: crate::upscale_live::Retired::new(),
        failed: None,
        told_reactive: None,
    })
}

/// Records DLSS over `inputs` into `list`; false if it could not (the caller
/// stretches instead). The list's descriptor heaps and root state are the
/// SDK's afterwards: the caller replays the title's.
pub(crate) fn record(
    device: &ID3D12Device,
    list: &ID3D12GraphicsCommandList,
    inputs: &Inputs<'_>,
) -> bool {
    let mut guard = DLSS.lock().unwrap_or_else(|e| e.into_inner());
    let dlss = match guard.get_or_insert_with(|| init(device)) {
        Some(d) => d,
        None => return false,
    };
    // SAFETY: live resources.
    let (out, colour) = unsafe { (inputs.output.GetDesc(), inputs.colour.GetDesc()) };
    let render = [
        if inputs.render[0] == 0 {
            colour.Width as u32
        } else {
            inputs.render[0]
        },
        if inputs.render[1] == 0 {
            colour.Height
        } else {
            inputs.render[1]
        },
    ];
    // The feature's input size is the largest the colour texture holds, so
    // that a dynamic render size inside it needs no new feature.
    let (in_w, in_h) = (colour.Width as u32, colour.Height);
    let flags = dlss_create_flags(
        colour.Format.0,
        inputs.near,
        inputs.far,
        inputs.exposure.is_some(),
        env_flag("VICTORY_DLSS_HDR"),
        env_flag("VICTORY_DLSS_DEPTH_INVERTED"),
    );
    let hdr = flags & crate::upscale::DLSS_FLAG_HDR != 0;
    let inverted = flags & crate::upscale::DLSS_FLAG_DEPTH_INVERTED != 0;
    let preset = preset();
    // A preset changed while running is a live change too.
    let live = crate::upscale_live::any() || dlss.feature.as_ref().is_some_and(|f| f.key[6] != preset);
    let release = dlss.core.release_feature;
    // SAFETY: handles the core made, no longer used by any list in flight.
    dlss.retired
        .tick(|h| unsafe { _ = release(h as *mut c_void) });
    let quality = crate::upscale_live::quality()
        .unwrap_or_else(|| env_f("VICTORY_DLSS_QUALITY", 2.0) as c_int);
    let key = [
        in_w,
        in_h,
        out.Width as u32,
        out.Height,
        flags as u32,
        quality as u32,
        preset,
    ];
    if dlss.failed == Some(key) {
        return false;
    }
    let p = dlss.params;
    if dlss.feature.as_ref().is_none_or(|f| f.key != key) {
        if let Some(old) = dlss.feature.take() {
            if live {
                // A live change: lists in flight may still evaluate it.
                dlss.retired.retire(old.handle as usize);
            } else {
                // SAFETY: a handle the core made.
                unsafe { release(old.handle) };
            }
        }
        let mut handle = std::ptr::null_mut();
        // SAFETY: live parameters and an open list.
        let r = unsafe {
            p.set_ui(name!("CreationNodeMask"), 1);
            p.set_ui(name!("VisibilityNodeMask"), 1);
            p.set_ui(name!("Width"), in_w);
            p.set_ui(name!("Height"), in_h);
            p.set_ui(name!("OutWidth"), out.Width as u32);
            p.set_ui(name!("OutHeight"), out.Height);
            p.set_i(name!("PerfQualityValue"), quality);
            p.set_i(name!("DLSS.Feature.Create.Flags"), flags);
            p.set_i(name!("DLSS.Enable.Output.Subrects"), 0);
            for hint in PRESET_HINTS {
                p.set_ui(hint, preset);
            }
            (dlss.core.create_feature)(list.as_raw(), FEATURE_SUPER_SAMPLING, p.0, &mut handle)
        };
        if r != SUCCESS || handle.is_null() {
            tracing::warn!(
                result = format_args!("{r:#x}"),
                ?key,
                "DLSS: no feature for these sizes; the upscale stays a stretch"
            );
            if live {
                dlss.failed = Some(key);
            } else {
                *guard = Some(None);
            }
            return false;
        }
        tracing::info!(
            input = ?(in_w, in_h),
            output = ?(out.Width, out.Height),
            hdr,
            depth_inverted = inverted,
            auto_exposure = inputs.exposure.is_none(),
            quality,
            preset,
            live,
            colour_format = colour.Format.0,
            "DLSS: feature made"
        );
        dlss.feature = Some(Feature { handle, key });
    }
    let Some(feature) = dlss.feature.as_ref() else {
        return false;
    };
    let reactive = inputs.reactive.filter(|_| reactive());
    if dlss.told_reactive != Some(reactive.is_some()) {
        dlss.told_reactive = Some(reactive.is_some());
        // SAFETY: a live resource.
        let mask = reactive.map(|r| unsafe { r.GetDesc() });
        tracing::info!(
            asked = self::reactive(),
            title_gave = inputs.reactive.is_some(),
            mask_size = ?mask.map(|d| (d.Width, d.Height)),
            mask_format = mask.map_or(0, |d| d.Format.0),
            "DLSS: reactive mask {}",
            if reactive.is_some() { "given (DLSS.Input.Bias.Current.Color.Mask)" } else { "not given" }
        );
    }
    let mv = env_f("VICTORY_DLSS_MV_SIGN", 1.0);
    let jit = env_f("VICTORY_DLSS_JITTER_SIGN", 1.0);
    let raw = |r: Option<&ID3D12Resource>| r.map_or(std::ptr::null_mut(), |r| r.as_raw());
    let scale = |s: f32| if s == 0.0 { 1.0 } else { s };
    // SAFETY: live parameters, resources and feature; an open list.
    let r = unsafe {
        p.set_resource(name!("Color"), inputs.colour.as_raw());
        p.set_resource(name!("Output"), inputs.output.as_raw());
        p.set_resource(name!("Depth"), raw(inputs.depth));
        p.set_resource(name!("MotionVectors"), raw(inputs.motion));
        p.set_resource(name!("ExposureTexture"), raw(inputs.exposure));
        p.set_resource(name!("DLSS.Input.Bias.Current.Color.Mask"), raw(reactive));
        p.set_f(name!("Jitter.Offset.X"), jit * inputs.jitter[0]);
        p.set_f(name!("Jitter.Offset.Y"), jit * inputs.jitter[1]);
        p.set_f(name!("MV.Scale.X"), mv * scale(inputs.motion_scale[0]));
        p.set_f(name!("MV.Scale.Y"), mv * scale(inputs.motion_scale[1]));
        p.set_i(name!("Reset"), inputs.reset as c_int);
        p.set_f(
            name!("Sharpness"),
            crate::upscale_live::sharpness()
                .unwrap_or_else(|| env_f("VICTORY_DLSS_SHARPNESS", 0.0)),
        );
        p.set_f(
            name!("DLSS.Pre.Exposure"),
            if inputs.pre_exposure > 0.0 {
                inputs.pre_exposure
            } else {
                1.0
            },
        );
        p.set_f(name!("FrameTimeDeltaInMsec"), inputs.frame_ms);
        p.set_ui(
            name!("DLSS.Render.Subrect.Dimensions.Width"),
            render[0].min(in_w),
        );
        p.set_ui(
            name!("DLSS.Render.Subrect.Dimensions.Height"),
            render[1].min(in_h),
        );
        (dlss.core.evaluate_feature)(list.as_raw(), feature.handle, p.0, std::ptr::null())
    };
    if r != SUCCESS {
        static TOLD: std::sync::Once = std::sync::Once::new();
        TOLD.call_once(|| {
            tracing::warn!(
                result = format_args!("{r:#x}"),
                "DLSS: an evaluation failed; that upscale is a stretch"
            )
        });
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::{newest_in_store, preset, preset_value, reactive};

    #[test]
    fn preset_names_are_the_ngx_presets() {
        assert_eq!(preset_value("default"), Some(0));
        assert_eq!(preset_value(""), Some(0));
        assert_eq!(preset_value(" K "), Some(11));
        assert_eq!(preset_value("j"), Some(10));
        assert_eq!(preset_value("m"), Some(13));
        assert_eq!(preset_value("e"), Some(5));
        assert_eq!(preset_value("a"), None);
        assert_eq!(preset_value("n"), None);
    }

    #[test]
    fn preset_and_reactive_are_live() {
        use victory_core::settings::check_live;
        check_live("VICTORY_DLSS_PRESET", "k", preset, 0, 11);
        check_live("VICTORY_DLSS_REACTIVE", "1", reactive, false, true);
    }

    #[test]
    fn the_newest_store_version_with_a_bin_wins() {
        let tmp = tempfile::tempdir().unwrap();
        let store = tmp.path();
        assert_eq!(newest_in_store(store), None);
        for (v, file) in [
            ("131356", "160_A.bin"),
            ("20316928", "160_B.bin"),
            ("30000000", "readme.txt"),
        ] {
            let files = store.join("versions").join(v).join("files");
            std::fs::create_dir_all(&files).unwrap();
            std::fs::write(files.join(file), b"MZ").unwrap();
        }
        std::fs::create_dir_all(store.join("versions/notanumber/files")).unwrap();
        let (v, bin) = newest_in_store(store).unwrap();
        assert_eq!(v, 20316928);
        assert!(bin.ends_with("160_B.bin"));
    }
}
