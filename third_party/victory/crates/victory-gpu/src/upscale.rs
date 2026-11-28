//! Opt-in host upscaling in place of the title's FidelityFX passes
//! (`VICTORY_UPSCALE=stretch|dlss`, off by default).
//!
//! Ported from `Peppacodes/playablegames-peppa` (`b3c3766546`, from
//! `avatarmanz/VIctory-7900XTX` PR #203 by YeokiNoewChi and
//! `CuteBooty/victory-8gb`). Kept in its own module so `host.rs` stays
//! untouched and below the 512 KiB asset-policy cap.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// `NVSDK_NGX_DLSS_Feature_Flags` bit positions from the public NVIDIA DLSS SDK.
pub const DLSS_FLAG_HDR: i32 = 1 << 0;
pub const DLSS_FLAG_MV_LOW_RES: i32 = 1 << 1;
pub const DLSS_FLAG_MV_JITTERED: i32 = 1 << 2;
pub const DLSS_FLAG_DEPTH_INVERTED: i32 = 1 << 3;
pub const DLSS_FLAG_AUTO_EXPOSURE: i32 = 1 << 6;

/// Name of the optional NVIDIA DLSS Super Resolution runtime DLL.
pub const DLSS_DLL_NAME: &str = "nvngx_dlss.dll";

/// What the title gives its upscale (`ffxDispatch`), as plain words so any
/// recorded stream can carry it cleanly without borrowing title structs.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Upscale {
    pub colour: u64,
    pub depth: u64,
    pub motion: u64,
    pub exposure: u64,
    pub output: u64,
    /// The title's reactive mask (0: none).
    pub reactive: u64,
    pub render: [u32; 2],
    pub jitter: [f32; 2],
    pub motion_scale: [f32; 2],
    pub reset: bool,
    pub pre_exposure: f32,
    pub near: f32,
    pub far: f32,
    pub frame_ms: f32,
    /// Use DLSS when NGX initialises; else stretch.
    pub dlss: bool,
    /// Vertical field of view (radians) and view units per metre, for frame
    /// generation (`crate::framegen`).
    pub fov_y: f32,
    pub metres_per_unit: f32,
}

impl Upscale {
    pub const WORDS: usize = 13;

    pub fn to_words(self) -> [u64; Self::WORDS] {
        let pair = |a: f32, b: f32| u64::from(a.to_bits()) | (u64::from(b.to_bits()) << 32);
        [
            self.colour,
            self.depth,
            self.motion,
            self.exposure,
            self.output,
            u64::from(self.render[0]) | (u64::from(self.render[1]) << 32),
            pair(self.jitter[0], self.jitter[1]),
            pair(self.motion_scale[0], self.motion_scale[1]),
            pair(self.pre_exposure, self.frame_ms),
            pair(self.near, self.far),
            u64::from(self.reset) | (u64::from(self.dlss) << 1),
            pair(self.fov_y, self.metres_per_unit),
            self.reactive,
        ]
    }

    pub fn from_words(w: &[u64]) -> Option<Self> {
        if w.len() < Self::WORDS {
            return None;
        }
        let pair = |v: u64| [f32::from_bits(v as u32), f32::from_bits((v >> 32) as u32)];
        let [pre_exposure, frame_ms] = pair(w[8]);
        let [near, far] = pair(w[9]);
        let [fov_y, metres_per_unit] = pair(w[11]);
        Some(Self {
            colour: w[0],
            depth: w[1],
            motion: w[2],
            exposure: w[3],
            output: w[4],
            render: [w[5] as u32, (w[5] >> 32) as u32],
            jitter: pair(w[6]),
            motion_scale: pair(w[7]),
            pre_exposure,
            frame_ms,
            near,
            far,
            reset: w[10] & 1 != 0,
            dlss: w[10] & 2 != 0,
            fov_y,
            metres_per_unit,
            reactive: w[12],
        })
    }
}

/// Whether a DXGI format number is treated as HDR by default for DLSS
/// (`R32G32B32A32_FLOAT`, `R16G16B16A16_TYPELESS`/`FLOAT`, `R10G10B10A2_UNORM`,
/// `R11G11B10_FLOAT`).
pub fn is_hdr_format(dxgi_format: i32) -> bool {
    matches!(dxgi_format, 2 | 9 | 10 | 24 | 26)
}

/// Computes `DLSS.Feature.Create.Flags` from the colour format, camera planes,
/// exposure availability, and optional `VICTORY_DLSS_HDR` /
/// `VICTORY_DLSS_DEPTH_INVERTED` overrides.
pub fn dlss_create_flags(
    colour_format: i32,
    near: f32,
    far: f32,
    has_exposure: bool,
    hdr_override: Option<bool>,
    depth_inverted_override: Option<bool>,
) -> i32 {
    let hdr = hdr_override.unwrap_or_else(|| is_hdr_format(colour_format));
    let inverted = depth_inverted_override.unwrap_or(near > far);
    let mut flags = DLSS_FLAG_MV_LOW_RES | DLSS_FLAG_MV_JITTERED;
    if hdr {
        flags |= DLSS_FLAG_HDR;
    }
    if inverted {
        flags |= DLSS_FLAG_DEPTH_INVERTED;
    }
    if !has_exposure {
        flags |= DLSS_FLAG_AUTO_EXPOSURE;
    }
    flags
}

/// Resolves the directory searched for `nvngx_dlss.dll`: `VICTORY_DLSS_DIR`
/// when set and non-empty, otherwise the executable's directory.
pub fn resolve_dlss_dir(env_dir: Option<&OsStr>, exe_dir: Option<&Path>) -> Option<PathBuf> {
    if let Some(dir) = env_dir {
        let non_empty = match dir.to_str() {
            Some(s) => !s.trim().is_empty(),
            None => !dir.is_empty(),
        };
        if non_empty {
            return Some(PathBuf::from(dir));
        }
    }
    exe_dir.map(Path::to_path_buf)
}

/// Whether `nvngx_dlss.dll` exists directly inside `dir`.
pub fn dlss_dll_present(dir: &Path) -> bool {
    dir.join(DLSS_DLL_NAME).is_file()
}

/// Records the title's upscale in place of its FidelityFX passes: DLSS when
/// asked for (`up.dlss`) and NGX (the driver's core) succeeds;
/// otherwise a plain bilinear stretch (`crate::stretch`). Returns `false` if
/// nothing was recorded (the caller may fall back to the title's DLL).
#[cfg(windows)]
pub fn upscale(this: *const std::ffi::c_void, up: &Upscale) -> bool {
    use crate::object::This;

    const SHADER_READ: u32 = 0x40;
    const UNORDERED: u32 = 0x8;

    let this: This = this.cast();
    if this.is_null() || up.colour == 0 || up.output == 0 {
        return false;
    }
    let colour: This = up.colour as This;
    let output: This = up.output as This;
    if !crate::resource::is_texture(colour) || !crate::resource::is_texture(output) {
        return false;
    }

    let mut done = false;
    let recorded = crate::list::with_host(this, |list, state| {
        let (Some(to), Some(from), Some(host)) = (
            crate::resource::host_resource(output),
            crate::resource::host_resource(colour),
            crate::host::host(),
        ) else {
            return;
        };
        if crate::texture::title_written(from) {
            crate::texture::upload(list, colour, from);
        }
        list.twin_use(colour, false, false);
        crate::list::ensure_state(list, colour, SHADER_READ);
        crate::list::ensure_state(list, output, UNORDERED);

        let input = |addr: u64| {
            if addr == 0 {
                return None;
            }
            let r: This = addr as This;
            if !crate::resource::is_texture(r) {
                return None;
            }
            let host_tex = crate::resource::host_resource(r)?;
            if crate::texture::title_written(host_tex) {
                crate::texture::upload(list, r, host_tex);
            }
            list.twin_use(r, false, false);
            crate::list::ensure_state(list, r, SHADER_READ);
            Some(host_tex)
        };
        if up.dlss {
            let inputs = crate::dlss::Inputs {
                colour: from,
                depth: input(up.depth),
                motion: input(up.motion),
                exposure: input(up.exposure),
                reactive: if crate::dlss::reactive() { input(up.reactive) } else { None },
                output: to,
                jitter: up.jitter,
                motion_scale: up.motion_scale,
                render: up.render,
                reset: up.reset,
                pre_exposure: up.pre_exposure,
                near: up.near,
                far: up.far,
                frame_ms: up.frame_ms,
            };
            done = crate::dlss::record(&host.device, &list.list, &inputs);
        }

        if !done {
            done = crate::stretch::record_stretch(&host.device, &list.list, to, from, up.render);
        }
        if done {
            crate::texture::host_written(output);
            if crate::framegen::active() {
                let (depth, motion) = (input(up.depth), input(up.motion));
                // SAFETY: a live resource.
                let max = unsafe { from.GetDesc() };
                let inputs = crate::framegen::Inputs {
                    depth,
                    motion,
                    render: up.render,
                    max_render: [max.Width as u32, max.Height],
                    jitter: up.jitter,
                    motion_scale: up.motion_scale,
                    frame_ms: up.frame_ms,
                    reset: up.reset,
                    near: up.near,
                    far: up.far,
                    fov_y: up.fov_y,
                    metres_per_unit: up.metres_per_unit,
                };
                crate::framegen::prepare(&host.device, &list.list, &inputs);
                crate::dlssg::tag_inputs(&list.list, &inputs);
            }
        }
        state.after_external(list);
    });
    recorded && done
}

/// For the upscale probe (`VICTORY_UPSCALE_PROBE=1`): each title texture's
/// host copy as `WxH fmt N` (DXGI format number), or `none`.
#[cfg(windows)]
pub fn upscale_inputs(textures: &[*const std::ffi::c_void]) -> Vec<String> {
    textures
        .iter()
        .map(|&t| {
            let title: crate::object::This = t.cast();
            if title.is_null() || !crate::resource::is_texture(title) {
                return "none".to_owned();
            }
            match crate::resource::host_resource(title) {
                Some(r) => {
                    // SAFETY: a live host resource.
                    let d = unsafe { r.GetDesc() };
                    format!("{}x{} fmt {}", d.Width, d.Height, d.Format.0)
                }
                None => "none".to_owned(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upscale_words_round_trip() {
        let up = Upscale {
            colour: 0x1122_3344_5566_7788,
            depth: 0x2233_4455_6677_8899,
            motion: 0x3344_5566_7788_99aa,
            exposure: 0x4455_6677_8899_aabb,
            output: 0x5566_7788_99aa_bbcc,
            reactive: 0x6677_8899_aabb_ccdd,
            render: [2560, 1440],
            jitter: [-0.25, 0.375],
            motion_scale: [-2560.0, 1440.0],
            reset: true,
            pre_exposure: 1.5,
            near: 1000.0,
            far: 0.1,
            frame_ms: 16.666,
            dlss: true,
            fov_y: 0.9,
            metres_per_unit: 1.0,
        };
        let words = up.to_words();
        assert_eq!(Upscale::from_words(&words), Some(up));
        assert_eq!(Upscale::from_words(&words[..Upscale::WORDS - 1]), None);

        let stretch_only = Upscale {
            reset: false,
            dlss: false,
            ..up
        };
        assert_eq!(
            Upscale::from_words(&stretch_only.to_words()),
            Some(stretch_only)
        );
    }

    #[test]
    fn dlss_flags_infer_hdr_depth_inversion_and_auto_exposure() {
        // R16G16B16A16_FLOAT (10), reversed-Z (near > far), no exposure texture.
        let flags = dlss_create_flags(10, 1000.0, 0.1, false, None, None);
        assert_eq!(
            flags,
            DLSS_FLAG_MV_LOW_RES
                | DLSS_FLAG_MV_JITTERED
                | DLSS_FLAG_HDR
                | DLSS_FLAG_DEPTH_INVERTED
                | DLSS_FLAG_AUTO_EXPOSURE
        );

        // R8G8B8A8_UNORM (28), standard Z (near < far), explicit exposure texture.
        let sdr = dlss_create_flags(28, 0.1, 1000.0, true, None, None);
        assert_eq!(sdr, DLSS_FLAG_MV_LOW_RES | DLSS_FLAG_MV_JITTERED);

        // Overrides take precedence.
        let overridden = dlss_create_flags(10, 1000.0, 0.1, true, Some(false), Some(false));
        assert_eq!(overridden, DLSS_FLAG_MV_LOW_RES | DLSS_FLAG_MV_JITTERED);
    }

    #[test]
    fn resolve_dlss_dir_and_dll_detection() {
        let tmp = tempfile::tempdir().unwrap();
        let exe_dir = tmp.path().join("exe");
        let custom_dir = tmp.path().join("dlss");
        std::fs::create_dir_all(&exe_dir).unwrap();
        std::fs::create_dir_all(&custom_dir).unwrap();

        // Empty or whitespace env var falls back to exe dir.
        assert_eq!(
            resolve_dlss_dir(None, Some(&exe_dir)),
            Some(exe_dir.clone())
        );
        assert_eq!(
            resolve_dlss_dir(Some(OsStr::new("   ")), Some(&exe_dir)),
            Some(exe_dir.clone())
        );
        assert_eq!(
            resolve_dlss_dir(Some(custom_dir.as_os_str()), Some(&exe_dir)),
            Some(custom_dir.clone())
        );
        assert_eq!(resolve_dlss_dir(None, None), None);

        // Absent DLL is detected cleanly, present DLL is found.
        assert!(!dlss_dll_present(&custom_dir));
        std::fs::write(custom_dir.join(DLSS_DLL_NAME), b"").unwrap();
        assert!(dlss_dll_present(&custom_dir));
    }

    #[cfg(windows)]
    #[test]
    fn upscale_rejects_null_inputs_cleanly() {
        assert!(!upscale(std::ptr::null(), &Upscale::default()));
        let missing_textures = Upscale {
            colour: 0,
            output: 0,
            ..Upscale::default()
        };
        assert!(!upscale(
            1usize as *const std::ffi::c_void,
            &missing_textures
        ));
        assert_eq!(upscale_inputs(&[std::ptr::null()]), vec!["none".to_owned()]);
    }
}
