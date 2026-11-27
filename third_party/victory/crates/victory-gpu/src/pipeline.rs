//! Pipeline states. Both descriptions start as on PC: the root signature,
//! then the shader stages (five for graphics, one for compute); streams
//! carry the rest of the graphics state as subobjects of the standard
//! types.
//!
//! Host pipelines are made on a pipeline's first draw, not at creation:
//! The title creates some 60,000 pipelines in its first 20
//! seconds and draws with a few dozen (decision 0007, step 3). Shader
//! containers are shared by content hash and translated once, on first
//! use (decision 0003).
//!
//! shortcut: only vertex plus pixel pipelines from streams and compute
//! pipelines get host pipelines; mesh, tessellation and geometry stages,
//! and the plain graphics description, keep their containers only. The 32-byte
//! packet after the header stays zero: titles only copy it inline, which
//! our null command-list limits keep from running.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::collections::HashMap;
use std::mem::offset_of;
use std::sync::{Arc, Mutex, OnceLock};

use crate::object::{self, E_INVALIDARG, HRESULT, Iid, Object, PIPELINE, Table, This, guid};
use crate::resource::PAGEABLE_IIDS;

const PIPELINE_IIDS: [Iid; 4] = [
    guid(
        0x765a30f3,
        0xf624,
        0x4c6f,
        [0xa8, 0x28, 0xac, 0xe9, 0x48, 0x62, 0x24, 0x45],
    ),
    PAGEABLE_IIDS[0],
    PAGEABLE_IIDS[1],
    PAGEABLE_IIDS[2],
];

#[repr(C)]
pub(crate) struct Bytecode {
    code: *const u8,
    size: usize,
}

#[repr(C)]
pub(crate) struct GraphicsHead {
    root: This,
    /// Vertex, pixel, domain, hull, geometry.
    stages: [Bytecode; 5],
}

#[repr(C)]
pub(crate) struct ComputeHead {
    root: This,
    stage: Bytecode,
}

/// A shader container a title passed, shared by every pipeline with the
/// same bytes.
pub(crate) struct Shader {
    pub(crate) stage: &'static str,
    /// The first 8 bytes of its BLAKE3, as logged.
    pub(crate) hash: String,
    container: Vec<u8>,
    /// Its part in the stand-in for the indirect light (`crate::ambient`).
    #[cfg(windows)]
    pub(crate) ambient: Option<crate::ambient::Role>,
    #[cfg(windows)]
    translated: OnceLock<Option<Translated>>,
    /// A skin lighting pixel stage (`crate::skin_fill`), looked up on first asking.
    #[cfg(windows)]
    skin: OnceLock<bool>,
}

impl Shader {
    /// The program name in its container (`PS_VelocityFill_...`), if any.
    pub(crate) fn program(&self) -> Option<&str> {
        program_name(&self.container)
    }

    /// Whether it is a skin lighting pixel stage (`crate::skin_fill`).
    #[cfg(windows)]
    pub(crate) fn skin(&self) -> bool {
        *self.skin.get_or_init(|| self.stage == "pixel" && crate::skin_fill::skin_program(&self.container))
    }
}

impl std::fmt::Debug for Shader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}", self.stage, self.hash)?;
        match program_name(&self.container) {
            Some(name) => write!(f, " {name}"),
            None => Ok(()),
        }
    }
}

/// The program's name as its container spells it (the first `XX_Name`:
/// `CS_...`, `PS_...`), for the log lines that say a shader or pipeline
/// failed: a hash alone says nothing about what goes missing.
fn program_name(container: &[u8]) -> Option<&str> {
    let ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let mut i = 0;
    while i + 3 < container.len() {
        let w = &container[i..];
        let starts = w[0].is_ascii_uppercase()
            && w[1].is_ascii_uppercase()
            && w[2] == b'_'
            && (i == 0 || !ident(container[i - 1]));
        if starts {
            let len = w.iter().take_while(|&&b| ident(b)).count();
            if len >= 6 {
                return std::str::from_utf8(&w[..len]).ok();
            }
            i += len.max(1);
            continue;
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod name_tests {
    use super::program_name;

    #[test]
    fn a_container_names_its_program() {
        let mut bytes = b"\x00\x01DXBC..xCS_ignored".to_vec();
        bytes.extend_from_slice(b"\x00CS_ShadowRenderTiled_9a1b\x00PS_Other\x00");
        assert_eq!(program_name(&bytes), Some("CS_ShadowRenderTiled_9a1b"));
        assert_eq!(program_name(b"\x00AB_x\x00"), None);
        assert_eq!(program_name(b""), None);
    }
}

/// Every container seen, by full content hash.
static SHADERS: Mutex<Option<HashMap<[u8; 32], Arc<Shader>>>> = Mutex::new(None);

/// For the memory lines: containers kept and their bytes, translations
/// kept and the bytes of their host code. None of them is ever let go.
static KEPT: [std::sync::atomic::AtomicU64; 4] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 4];

/// What the shader table and the pipeline library keep in memory:
/// containers and their bytes, translations and their bytes, the bytes of
/// the library's file (kept whole while the library lives) and what its
/// last save wrote.
#[cfg(windows)]
#[cfg_attr(not(feature = "diagnostics"), allow(dead_code))]
pub(crate) fn kept() -> [u64; 7] {
    use std::sync::atomic::Ordering::Relaxed;
    [
        KEPT[0].load(Relaxed),
        KEPT[1].load(Relaxed),
        KEPT[2].load(Relaxed),
        KEPT[3].load(Relaxed),
        library::BLOB.load(Relaxed),
        library::SAVED.load(Relaxed),
        library::TRIMMED_BYTES.load(Relaxed),
    ]
}

/// The shared container for `bytes`.
fn shader(stage: &'static str, bytes: &[u8]) -> Arc<Shader> {
    let full = blake3::hash(bytes);
    let mut shaders = SHADERS.lock().unwrap_or_else(|e| e.into_inner());
    shaders
        .get_or_insert_with(HashMap::new)
        .entry(*full.as_bytes())
        .or_insert_with(|| {
            KEPT[0].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            KEPT[1].fetch_add(bytes.len() as u64, std::sync::atomic::Ordering::Relaxed);
            Arc::new(Shader {
                stage,
                hash: full.to_hex()[..16].to_owned(),
                container: bytes.to_vec(),
                #[cfg(windows)]
                ambient: (stage == "compute")
                    .then(|| crate::ambient::role(bytes))
                    .flatten(),
                #[cfg(windows)]
                translated: OnceLock::new(),
                #[cfg(windows)]
                skin: OnceLock::new(),
            })
        })
        .clone()
}

/// Puts the translation of a container of the title's into the cache on
/// disk, and with `compute` its host pipeline into the driver's, before any
/// run wants them. Whether it translated, and whether it has a pipeline
/// (`None`: not asked for, or left out).
#[cfg(windows)]
pub fn warm(stage: &'static str, container: &[u8], compute: bool) -> (bool, Option<bool>) {
    let full = blake3::hash(container);
    // Not through the table, which lets nothing go.
    let shader = Arc::new(Shader {
        stage,
        hash: full.to_hex()[..16].to_owned(),
        container: container.to_vec(),
        ambient: (stage == "compute")
            .then(|| crate::ambient::role(container))
            .flatten(),
        translated: OnceLock::new(),
        skin: OnceLock::new(),
    });
    let translated = shader.translated().is_some();
    let made = (translated && compute)
        .then(|| make::warm(vec![shader]))
        .flatten();
    (translated, made)
}

/// A translated shader and the console resources its code reads.
#[cfg(windows)]
pub(crate) struct Translated {
    pub(crate) host: victory_shader::emit::HostShader,
    pub(crate) used: Vec<victory_shader::UsedResource>,
    pub(crate) tables: crate::draw::TablePlan,
    /// Whether the shader's code can store to buffer memory ([`stores`]);
    /// true when its code is not known.
    pub(crate) stores: bool,
    /// The host DXIL, moved out of `host`: read only to make a pipeline. With
    /// `VICTORY_DROP_DXIL=1` it is let go after each pipeline and translated
    /// again (the disk cache) when another needs it ([`Shader::dxil`]).
    dxil: Mutex<Option<Arc<Vec<u8>>>>,
}

/// `VICTORY_DROP_DXIL=1` (off by default): shaders' host DXIL is let go once their pipeline is
/// made; every translated shader keeps it for good otherwise.
#[cfg(windows)]
pub(crate) fn drop_dxil() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_DROP_DXIL").is_some_and(|v| v == "1"))
}

/// DXIL bytes let go, times and bytes brought back, for the ram line.
#[cfg(windows)]
pub(crate) static DXIL_DROPS: [std::sync::atomic::AtomicU64; 3] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 3];

#[cfg(windows)]
impl Shader {
    /// The host DXIL of a translated shader: kept, or translated again from the container
    /// (a hit in the shader cache) after [`Shader::release_dxil`].
    pub(crate) fn dxil(&self) -> Option<Arc<Vec<u8>>> {
        use std::sync::atomic::Ordering::Relaxed;
        let t = self.translated()?;
        let mut held = t.dxil.lock().unwrap_or_else(|e| e.into_inner());
        if held.is_none() {
            let again = crate::shader_cache::host_shader(&self.container).ok()?.dxil;
            DXIL_DROPS[1].fetch_add(1, Relaxed);
            DXIL_DROPS[2].fetch_add(again.len() as u64, Relaxed);
            KEPT[3].fetch_add(again.len() as u64, Relaxed);
            *held = Some(Arc::new(again));
        }
        held.clone()
    }

    /// With `VICTORY_DROP_DXIL=1`: lets the host DXIL go (a pipeline being made holds its own
    /// `Arc` until it is done).
    pub(crate) fn release_dxil(&self) {
        use std::sync::atomic::Ordering::Relaxed;
        if !drop_dxil() {
            return;
        }
        let Some(Some(t)) = self.translated.get() else {
            return;
        };
        if let Some(d) = t.dxil.lock().unwrap_or_else(|e| e.into_inner()).take() {
            KEPT[3].fetch_sub(d.len() as u64, Relaxed);
            DXIL_DROPS[0].fetch_add(d.len() as u64, Relaxed);
        }
    }

    /// The translation, made on first use; a shader that does not
    /// translate is logged once.
    pub(crate) fn translated(&self) -> Option<&Translated> {
        let done = self.translated
            .get_or_init(|| {
                let _hitch = crate::hitch::span(crate::hitch::Phase::ShaderTranslate);
                let done = translate(&self.container);
                // `VICTORY_SHADER_SAVE=<dir>`: each shader as the title gave it (`.cdxil`) and as
                // translated (`.dxil`), by the hash crash reports name it with
                if let Some(dir) = std::env::var_os("VICTORY_SHADER_SAVE") {
                    let dir = std::path::PathBuf::from(dir);
                    let _ = std::fs::create_dir_all(&dir);
                    let stem = format!("{}.{}", self.hash, self.stage);
                    let _ = std::fs::write(dir.join(format!("{stem}.cdxil")), &self.container);
                    if let Ok(t) = &done {
                        let _ = std::fs::write(
                            dir.join(format!("{stem}.dxil")),
                            t.dxil
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .as_deref()
                                .map_or(&[][..], |d| &d[..]),
                        );
                    }
                }
                if let Ok(t) = &done {
                    KEPT[2].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    KEPT[3].fetch_add(
                        t.dxil
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .as_ref()
                            .map_or(0, |d| d.len() as u64),
                        std::sync::atomic::Ordering::Relaxed,
                    );
                }
                match &done {
                    Ok(t) => tracing::info!(
                        stage = self.stage,
                        hash = %self.hash,
                        user_data = t.host.user_data.len(),
                        samplers = t.host.samplers.len(),
                        "shader translated"
                    ),
                    #[cfg(windows)]
                    Err(e) if translate_ahead::on_ahead_thread() => translate_ahead::refused_ahead(
                        self,
                        program_name(&self.container).unwrap_or("?"),
                        e,
                    ),
                    Err(e) => self.log_refused(e),
                }
                done.ok()
            });
        let done = done.as_ref();
        // (bootfast) Refused ahead (`VICTORY_TRANSLATE_AHEAD`) and now
        // needed: reported as a refusal, as it would have been here.
        #[cfg(windows)]
        if done.is_none() && !translate_ahead::on_ahead_thread() && translate_ahead::needed(self) {
            self.log_refused("does not translate (refused ahead)");
        }
        done
    }

    fn log_refused(&self, error: &str) {
        tracing::warn!(
            stage = self.stage,
            hash = %self.hash,
            name = program_name(&self.container).unwrap_or("?"),
            error = %error,
            "shader does not translate; its pipelines' draws are dropped"
        );
    }
}

#[cfg(windows)]
fn translate(container: &[u8]) -> Result<Translated, String> {
    use victory_shader::{Container, FourCC, HwStage};
    // The translation depends on it (a view index past a capped heap).
    crate::descriptor::host::probe_view_limit();
    let c = Container::parse(container).map_err(|e| e.to_string())?;
    // (`ACCC`: native code another compiler made, with `VICTORY_WAVE32_NVIDIA=1`)
    let programs = match c.part(FourCC::DXIL).or_else(|| {
        c.part(FourCC::ACCC)
            .filter(|_| victory_shader::emit::nvidia::enabled())
    }) {
        Some(part) => victory_shader::native_programs(part.data).map_err(|e| e.to_string())?,
        None => Vec::new(),
    };
    let mut host = crate::shader_cache::host_shader(container)?;
    let dxil = Mutex::new(Some(Arc::new(std::mem::take(&mut host.dxil))));
    let stores = programs.is_empty() || programs.iter().any(stores);
    let used = programs
        .into_iter()
        .find(|p| {
            matches!(
                p.stage,
                HwStage::Vs | HwStage::Ps | HwStage::Ms | HwStage::Cs
            )
        })
        .map(|p| p.used)
        .unwrap_or_default();
    let tables = crate::draw::TablePlan::new(&used, &host.root_indexed_views, &host.half_views);
    Ok(Translated {
        host,
        used,
        tables,
        stores,
        dxil,
    })
}

/// The compute pipelines of the console's ray-tracing structure builder
/// (`XboxBvhBuilder.dll`; its programs are the ones with a spilled
/// user-data table) run **all or none**. Its stages feed each other
/// through memory: a stage that runs on what a dropped stage never wrote
/// computes its store offsets from garbage. The structures lie in plain
/// title memory, between the title's own objects, and a root view has no
/// size, so those stores land on the neighbours (run 98, with 14 of 35
/// stages translated: the title died within a second of the first builder
/// submission, on pointers that had become bounding-box bit patterns).
///
/// So a builder dispatch runs only while every builder pipeline the title
/// has created has a host pipeline; the first builder dispatch makes them
/// all on the spot instead of over the next frames (a stage "still being
/// made" would be a dropped stage). One that cannot be made turns the
/// builder off for the rest of the run.
///
/// shortcut: and only with `VICTORY_BVH_BUILDER=1`, until a run has shown
/// the whole builder and the host side of title memory by address
/// (`memory.rs`) working on a card; nothing reads the structures before
/// the programs that walk them translate.
#[cfg(windows)]
struct Builder {
    pipelines: Vec<Arc<make::Pending>>,
    /// Every one of `pipelines` has a host pipeline.
    whole: bool,
    /// One could not be made.
    off: bool,
}

#[cfg(windows)]
static BUILDER: Mutex<Builder> = Mutex::new(Builder {
    pipelines: Vec::new(),
    whole: false,
    off: false,
});

/// Whether `shaders` are a pipeline of the structure builder.
#[cfg(windows)]
fn of_builder(shaders: &[Arc<Shader>]) -> bool {
    use victory_shader::{Container, FourCC};
    let [shader] = shaders else { return false };
    if shader.stage != "compute" {
        return false;
    }
    let Ok(c) = Container::parse(&shader.container) else {
        return false;
    };
    c.part(FourCC::DXIL)
        .or_else(|| {
            c.part(FourCC::ACCC)
                .filter(|_| victory_shader::emit::nvidia::enabled())
        })
        .and_then(|part| victory_shader::native_programs(part.data).ok())
        .is_some_and(|programs| programs.iter().any(victory_shader::spill::is_spilled))
}

/// Whether a compute container is one of the title's `bvh_skinning`
/// programs: they pose every animated mesh (and its motion) once more, only
/// for the ray-tracing structures. With every ray a miss
/// (`VICTORY_BVH_NO_HIT=1`) and the structure builder off, nothing reads what
/// they write, and they were the largest share of the card's time in a
/// cutscene (~6 ms a dispatch, hundreds a frame).
///
/// The container holds its entry point's name, not the effect's: all 30
/// entry points named `CSComputeSkinning*` in the title's shader file are
/// `bvh_skinning`'s; the picture's own skinning is `CS_PedStandardSkinning*`,
/// `CS_PosOnlyLodSkinning*` and the cloth's `computeclothSkinning`.
///
/// Two more kinds only serve rays (after codec-d/cool_fox `make.rs`):
/// `CS_RaytracingResolve*` shades what a ray hit (some 1,000 programs) and
/// `CS_ScreenProbes_ActivateRays*` picks the screen probes that cast rays.
///
/// And the tracing itself against the structures, with the hair's geometry
/// made for them (8 % of the card's time in the opening cutscene): the
/// `bvh_skinning_smh` program `CSGenerateHairVB`, the screen probes'
/// `CS_ScreenProbes_TraceRays*_BVH*`. Their `Fallback` and `Voxels` forms
/// march through the title's voxels, not the structures, and light the
/// picture: they run. `CSGenerateHairVB` is grouped with `RayOnly::Trace`
/// so `CSComputeSkinning` remains on by default (opt-in skip only via
/// `VICTORY_SKIP_RT_SKIN=1`).
///
/// The reflections' `CS_RaytraceVBuffer*` (`RayOnly::VBuffer`) also traces
/// the structures. A frame dispatches it twice, each on its own list: after
/// `CS_RayPrePass_Opaque*`, for `CS_Primary_GatherVBuffer*`, and after
/// `CS_RayPrePass_Alpha*`, for the alpha fallbacks and
/// `CS_GatherAlphaVBuffer*`. The alpha one runs: left out, its hit records
/// keep stale values and the diner's window glass shows blotchy frosted
/// splotches and blue patches; run with every ray a miss, it writes clean
/// no-hit records. The opaque one stays out: with every ray a miss, skin
/// indoors reflects the sky and looks oily. `VICTORY_SKIP_RT_VBUFFER=1`
/// leaves out the alpha one too.
#[cfg(windows)]
fn for_rays(bytes: &[u8]) -> RayOnly {
    let has = |name: &[u8]| bytes.windows(name.len()).any(|w| w == name);
    if has(b"CSComputeSkinning") {
        RayOnly::Skin
    } else if has(b"CS_RaytracingResolve") {
        RayOnly::Resolve
    } else if has(b"CS_ScreenProbes_ActivateRays") {
        RayOnly::Activate
    } else if has(b"CS_RaytraceVBuffer") && !has(b"Voxels") {
        RayOnly::VBuffer
    } else if (has(b"CS_ScreenProbes_TraceRays") && has(b"_BVH")) || has(b"CSGenerateHairVB") {
        RayOnly::Trace
    } else {
        RayOnly::No
    }
}

/// What a compute pipeline does only for rays ([`for_rays`]).
#[cfg(windows)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum RayOnly {
    No,
    Skin,
    Resolve,
    Activate,
    Trace,
    VBuffer,
}

#[cfg(all(test, windows))]
#[test]
fn ray_programs_are_known_by_name() {
    assert_eq!(
        for_rays(b"..\0CSComputeSkinningRigidVelocity_1c34db0a_Wrapped\0.."),
        RayOnly::Skin
    );
    assert_eq!(
        for_rays(b"..\0CS_PedStandardSkinning_3_00aa11bb_Wrapped\0.."),
        RayOnly::No
    );
    assert_eq!(for_rays(b"computeclothSkinning"), RayOnly::No);
    assert_eq!(
        for_rays(b"..\0CS_RaytracingResolve_Opaque_12345678_Wrapped\0.."),
        RayOnly::Resolve
    );
    assert_eq!(
        for_rays(b"..\0CS_ScreenProbes_ActivateRays_0_Wrapped\0.."),
        RayOnly::Activate
    );
    assert_eq!(
        for_rays(b"..\0CSGenerateHairVB_c88a8f19_Wrapped\0.."),
        RayOnly::Trace
    );
    assert_eq!(
        for_rays(b"..\0CS_ScreenProbes_TraceRays_RayBinning_BVH_Mixed\0.."),
        RayOnly::Trace
    );
    assert_eq!(
        for_rays(b"..\0CS_ScreenProbes_TraceRays_Voxels\0.."),
        RayOnly::No
    );
    assert_eq!(
        for_rays(b"..\0CS_ScreenProbes_TraceRays_Fallback\0.."),
        RayOnly::No
    );
    assert_eq!(
        for_rays(b"..\0CS_RaytraceVBuffer_ShaderBinning_WithFar\0.."),
        RayOnly::VBuffer
    );
    assert_eq!(
        for_rays(
            b"..\0CS_RaytraceVBuffer_RayBinning_ShaderBinning_Mixed_WithFar_ca043b62_Wrapped\0.."
        ),
        RayOnly::VBuffer
    );
    assert_eq!(
        for_rays(b"..\0CS_RaytraceVBuffer_Voxels_RayBinning\0.."),
        RayOnly::No
    );
}

/// Whether a dispatch of `this` is left out because it only serves rays,
/// and nothing they could hit is there: every ray is a miss
/// (`VICTORY_BVH_NO_HIT=1` / `VICTORY_RT_MISS=1`) and the structure builder
/// is off. `CS_RaytracingResolve*`, `CS_ScreenProbes_ActivateRays*`, and
/// BVH-only trace/hair passes are skipped unless `VICTORY_RT_RESOLVE=1`,
/// `VICTORY_ACTIVATE_RAYS=1`, or `VICTORY_RT_TRACE=1`.
/// `CSComputeSkinning*` stays ON by default (skipping it caused a
/// deterministic `0x887A0006` GPU hang at world entry on Series X); opt-in
/// skip only via `VICTORY_SKIP_RT_SKIN=1`.
#[cfg(windows)]
pub(crate) fn skips_rt_skin(this: This, alpha_rays: bool) -> bool {
    static SKIPS: [[OnceLock<bool>; 2]; 6] = [const { [const { OnceLock::new() }; 2] }; 6];
    if this.is_null() {
        return false;
    }
    let kind = pipeline(this).rt_only;
    *SKIPS[kind as usize][alpha_rays as usize].get_or_init(|| {
        ray_kind_skipped(kind, alpha_rays, |name| {
            std::env::var_os(name).is_some_and(|v| v == "1")
        })
    })
}

/// Whether `this` is a `CS_RayPrePass*`: `Some(true)` for the alpha one,
/// `Some(false)` for the others (see [`for_rays`]).
#[cfg(windows)]
pub(crate) fn ray_pre_pass(this: This) -> Option<bool> {
    if this.is_null() {
        return None;
    }
    pipeline(this).pre_pass
}

/// For a program that walks the ray-tracing structures (`RayOnly::Trace`,
/// `RayOnly::VBuffer`): whether it is a `CS_RaytraceVBuffer*`, whether it
/// is the glass one ([`GLASS_TRACE`]), and the buffer views it reads from
/// tables as (root parameter, dword in the table, register) (`crate::rt_bind`).
/// `None` for any other program.
#[cfg(windows)]
pub(crate) fn ray_views(this: This) -> Option<(bool, bool, Vec<(u8, u32, u32)>)> {
    if this.is_null() {
        return None;
    }
    let p = pipeline(this);
    let vbuffer = match p.rt_only {
        RayOnly::VBuffer => true,
        RayOnly::Trace => false,
        _ => return None,
    };
    let shader = p.shaders.iter().find(|s| s.stage == "compute")?;
    let views = shader
        .translated()?
        .used
        .iter()
        .filter(|u| u.kind == victory_shader::ResourceType::Srv && u.root == victory_shader::RootType::Table && u.dwords == 4)
        .map(|u| (u.slot, u.table_offset_dwords, u.lower_bound))
        .collect();
    Some((vbuffer, shader.hash.starts_with(GLASS_TRACE), views))
}

/// `CS_RaytraceVBuffer_RayBinning_ShaderBinning_Mixed_WithFar`: the trace
/// the alpha pass (the window glass) runs.
#[cfg(windows)]
pub(crate) const GLASS_TRACE: &str = "6d7fe7745ab9c6d8";

/// [`ray_pre_pass`] of a compute program's container.
#[cfg(windows)]
fn pre_pass_of(bytes: &[u8]) -> Option<bool> {
    let has = |name: &[u8]| bytes.windows(name.len()).any(|w| w == name);
    has(b"CS_RayPrePass_").then(|| has(b"CS_RayPrePass_Alpha"))
}

/// Whether dispatches of `kind` are left out, `alpha_rays` telling whether
/// the list last ran the alpha ray pre-pass and `on(name)` whether the
/// switch `name` is `1` ([`skips_rt_skin`]).
#[cfg(windows)]
fn ray_kind_skipped(kind: RayOnly, alpha_rays: bool, on: impl Fn(&str) -> bool) -> bool {
    let off = |switch: &str| {
        (on("VICTORY_BVH_NO_HIT") || on("VICTORY_RT_MISS"))
            && !on("VICTORY_BVH_BUILDER")
            && !on(switch)
    };
    match kind {
        RayOnly::No => false,
        RayOnly::Skin => on("VICTORY_SKIP_RT_SKIN"),
        RayOnly::Resolve => off("VICTORY_RT_RESOLVE"),
        RayOnly::Activate => off("VICTORY_ACTIVATE_RAYS"),
        RayOnly::Trace => off("VICTORY_RT_TRACE"),
        RayOnly::VBuffer => {
            off("VICTORY_RT_TRACE") && (!alpha_rays || on("VICTORY_SKIP_RT_VBUFFER"))
        }
    }
}

#[cfg(all(test, windows))]
#[test]
fn the_alpha_vbuffer_trace_runs_when_every_ray_misses() {
    let with = |set: &'static [&'static str]| move |name: &str| set.contains(&name);
    let no_hit = with(&["VICTORY_BVH_NO_HIT"]);
    assert!(!ray_kind_skipped(RayOnly::VBuffer, true, no_hit));
    assert!(ray_kind_skipped(RayOnly::VBuffer, false, no_hit));
    for alpha in [false, true] {
        assert!(ray_kind_skipped(RayOnly::Trace, alpha, no_hit));
        assert!(ray_kind_skipped(RayOnly::Resolve, alpha, no_hit));
        assert!(ray_kind_skipped(RayOnly::Activate, alpha, no_hit));
        assert!(!ray_kind_skipped(RayOnly::Skin, alpha, no_hit));
    }
    assert!(ray_kind_skipped(
        RayOnly::VBuffer,
        true,
        with(&["VICTORY_BVH_NO_HIT", "VICTORY_SKIP_RT_VBUFFER"])
    ));
    for alpha in [false, true] {
        assert!(!ray_kind_skipped(
            RayOnly::VBuffer,
            alpha,
            with(&["VICTORY_BVH_NO_HIT", "VICTORY_SKIP_RT_VBUFFER", "VICTORY_RT_TRACE"])
        ));
        assert!(!ray_kind_skipped(RayOnly::VBuffer, alpha, with(&[])));
    }
}

#[cfg(all(test, windows))]
#[test]
fn ray_pre_passes_are_known_by_name() {
    assert_eq!(pre_pass_of(b"..\0CS_RayPrePass_Alpha_a80bdf75_Wrapped\0.."), Some(true));
    assert_eq!(pre_pass_of(b"..\0CS_RayPrePass_Opaque_SSR_a80bdf75_Wrapped\0.."), Some(false));
    assert_eq!(pre_pass_of(b"..\0CS_RaytraceVBuffer_Mixed_WithFar\0.."), None);
}

/// `VICTORY_BVH_QUARANTINE=1`: what the structure builder's stages store
/// stays in the host's copies and is not copied back into title memory. A
/// check for a first run of the whole builder: a stage that stores where
/// it should not can then not reach the title's own objects (run 98), and
/// the programs that walk the structures read the host's copies anyway.
/// shortcut: not a way to run: the title also reads results back (sizes,
/// counters) and gets none, and a copy's pages are overwritten from title
/// memory when the title writes them.
#[cfg(windows)]
pub(crate) fn builder_quarantined(this: This) -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    is_builder(this)
        && *ON.get_or_init(|| std::env::var_os("VICTORY_BVH_QUARANTINE").is_some_and(|v| v == "1"))
}

/// A compute pipeline's shader hash (cheap) and, when `group`, its
/// thread-group size (parses the container).
#[cfg(windows)]
pub(crate) fn compute_group(this: This, group: bool) -> Option<(String, Option<[u32; 3]>)> {
    use victory_shader::{Container, FourCC, native::StageIo};
    if this.is_null() {
        return None;
    }
    let shader = pipeline(this).shaders.iter().find(|s| s.stage == "compute")?;
    if !group {
        return Some((shader.hash.clone(), None));
    }
    let c = Container::parse(&shader.container).ok()?;
    let part = c.part(FourCC::DXIL).or_else(|| c.part(FourCC::ACCC))?;
    let programs = victory_shader::native_programs(part.data).ok()?;
    programs.iter().find_map(|p| match &p.io {
        StageIo::Cs(cs) => Some((shader.hash.clone(), Some(cs.threads))),
        _ => None,
    })
}

/// Whether the title's pipeline `this` is one of the structure builder's.
#[cfg(windows)]
pub(crate) fn is_builder(this: This) -> bool {
    !this.is_null() && pipeline(this).builder
}

/// A stage of the builder was dropped after all: none runs from here on.
#[cfg(windows)]
pub(crate) fn builder_off(why: &'static str) {
    let mut b = BUILDER.lock().unwrap_or_else(|e| e.into_inner());
    if !b.off {
        b.off = true;
        tracing::warn!(
            why,
            "the ray-tracing structure builder is turned off for the rest of the run: its stages run all or none"
        );
    }
}

/// At a dispatch of a builder pipeline: whether the builder runs (see
/// [`Builder`]).
#[cfg(windows)]
pub(crate) fn builder_runs() -> bool {
    static ALLOWED: OnceLock<bool> = OnceLock::new();
    if !*ALLOWED.get_or_init(|| std::env::var_os("VICTORY_BVH_BUILDER").is_some_and(|v| v == "1")) {
        return false;
    }
    let mut b = BUILDER.lock().unwrap_or_else(|e| e.into_inner());
    if b.off {
        return false;
    }
    if b.whole {
        return true;
    }
    let missing = b.pipelines.iter().filter(|p| !make::now(p)).count();
    if missing > 0 {
        b.off = true;
        tracing::warn!(
            missing,
            of = b.pipelines.len(),
            "the ray-tracing structure builder stays off: not all of its programs have a host pipeline, and its stages run all or none"
        );
        return false;
    }
    tracing::info!(
        pipelines = b.pipelines.len(),
        "the ray-tracing structure builder runs: all of its programs have host pipelines"
    );
    b.whole = true;
    true
}

/// Whether a console program can store to buffer memory: it has a buffer,
/// typed-buffer, scalar-memory, flat or global store or atomic, or code
/// that is not all known (an instruction of those kinds outside our table,
/// an indirect jump, code that does not decode). Stores to textures
/// (image instructions) and to the program's own scratch memory do not
/// count: neither reaches a buffer's host copy. After upstream #413,
/// which leaves scalar-memory stores out and counts image stores.
#[cfg(windows)]
fn stores(program: &victory_shader::NativeProgram) -> bool {
    use victory_shader::gfx10::{Encoding, decode};
    let Ok(code) = decode(&program.code) else {
        return true;
    };
    code.iter().any(|i| {
        let name = i.name();
        match i.encoding {
            Encoding::Mubuf
            | Encoding::Mtbuf
            | Encoding::Smem
            | Encoding::Flat
            | Encoding::Global => name.is_none_or(|n| {
                (n.contains("store") || n.contains("atomic")) && !n.contains("scratch")
            }),
            Encoding::Sop1 => name.is_none_or(|n| matches!(n, "s_setpc_b64" | "s_swappc_b64")),
            _ => false,
        }
    })
}

#[repr(C)]
struct Pipeline {
    object: Object,
    packet: [u32; 8],
    /// Each used stage's container.
    shaders: Vec<Arc<Shader>>,
    /// Whether a list has bound it yet (its shaders are logged then).
    bound: std::sync::atomic::AtomicBool,
    /// What its host pipeline is made from, and the host pipeline once made
    /// (by a worker: the title may release the pipeline meanwhile).
    #[cfg(windows)]
    host: Arc<make::Pending>,
    /// One of the ray-tracing structure builder's ([`Builder`]).
    #[cfg(windows)]
    builder: bool,
    /// One of the title's passes that only serve rays
    /// ([`for_rays`]).
    #[cfg(windows)]
    rt_only: RayOnly,
    /// [`ray_pre_pass`].
    #[cfg(windows)]
    pre_pass: Option<bool>,
}
const _: () = assert!(offset_of!(Pipeline, packet) == 0x10);

/// Stage names, in description order.
const GRAPHICS_STAGES: [&str; 5] = ["vertex", "pixel", "domain", "hull", "geometry"];

fn pipeline<'a>(this: This) -> &'a Pipeline {
    // SAFETY: the title passes one of our pipelines.
    unsafe { &*this.cast::<Pipeline>() }
}

/// The stages and hashes of title pipeline `this`, as `[mesh 0123..,
/// pixel 4567..]` (diagnostics, `crate::drop_why`); "no pipeline" for none.
#[cfg(windows)]
pub(crate) fn shaders_of(this: This) -> String {
    if this.is_null() {
        return "no pipeline".to_owned();
    }
    let names: Vec<String> = pipeline(this)
        .shaders
        .iter()
        .map(|s| format!("{} {}", s.stage, s.hash))
        .collect();
    format!("[{}]", names.join(", "))
}

/// List method: logs each pipeline's shaders the first time a list binds
/// it, so a run lists the shaders its frames need first.
pub(crate) fn bound(this: This) {
    if this.is_null() {
        return;
    }
    let p = pipeline(this);
    if p.bound.swap(true, std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    let frame = crate::frame::current();
    for shader in &p.shaders {
        tracing::info!(pipeline = ?this, frame, stage = shader.stage, size = shader.container.len(), hash = %shader.hash, "first bind");
    }
}

/// The host pipeline of a title pipeline; `None` if one of its shaders does
/// not translate, the host refuses it, or it is still being made (its first
/// use starts that, see [`make`]).
#[cfg(windows)]
pub(crate) fn host_pipeline<'a>(this: This) -> Option<&'a host::HostPipeline> {
    if this.is_null() {
        return None;
    }
    make::ready(&pipeline(this).host)
}

/// Host pipelines queued and not made yet.
#[cfg(windows)]
pub(crate) fn making() -> u64 {
    make::waiting()
}

/// Draws and dispatches dropped since the last [`take_drops`], by reason
/// (Neptune: in every build, so that a release run says which pipelines its dropped
/// draws were for).
#[cfg(windows)]
static DROPS: Mutex<Option<HashMap<String, u64>>> = Mutex::new(None);

/// Shader hashes as printed in the pipeline diagnostics.
#[cfg(all(windows, feature = "diagnostics"))]
fn parse_skip_hashes(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|hash| !hash.is_empty())
        .filter_map(|hash| {
            if hash.len() == 16 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                Some(hash.to_ascii_lowercase())
            } else {
                tracing::warn!(
                    hash,
                    "VICTORY_SKIP_PIPELINES expects 16-digit shader hashes"
                );
                None
            }
        })
        .collect()
}

#[cfg(all(windows, feature = "diagnostics"))]
fn skipped_shader(p: &Pipeline) -> Option<&str> {
    // A set, not a list: every draw and dispatch checks each of its shaders
    // against `VICTORY_SKIP_PIPELINES`.
    static HASHES: OnceLock<std::collections::HashSet<String>> = OnceLock::new();
    let hashes = HASHES.get_or_init(|| {
        let value = std::env::var("VICTORY_SKIP_PIPELINES").unwrap_or_default();
        parse_skip_hashes(&value).into_iter().collect()
    });
    if hashes.is_empty() {
        return None;
    }
    p.shaders
        .iter()
        .find(|shader| hashes.contains(&shader.hash))
        .map(|shader| shader.hash.as_str())
}

#[cfg(all(windows, feature = "diagnostics"))]
pub(crate) fn is_skipped(this: This) -> bool {
    !this.is_null() && skipped_shader(pipeline(this)).is_some()
}

/// Counts a draw or dispatch of `this` intentionally skipped or dropped
/// for want of a host pipeline (diagnostics).
#[cfg(windows)]
pub(crate) fn note_drop(this: This) {
    let why = if this.is_null() {
        "no pipeline set".to_owned()
    } else {
        let p = pipeline(this);
        #[cfg(feature = "diagnostics")]
        let skipped = skipped_shader(p).map(|hash| format!("skipped shader {hash}"));
        #[cfg(not(feature = "diagnostics"))]
        let skipped: Option<String> = None;
        skipped
            .or_else(|| make::missing(&p.host))
            .unwrap_or_else(|| {
                // (Neptune) Its host pipeline is there: the draw was left out for another
                // reason (its views or buffers could not be made ready, or a filter of ours).
                let shaders: Vec<String> = p
                    .shaders
                    .iter()
                    .map(|s| format!("{} {}", s.stage, s.hash))
                    .collect();
                format!("pipeline made, draw not run [{}]", shaders.join(", "))
            })
    };
    note_why(why);
}

#[cfg(all(windows, feature = "diagnostics"))]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum PrepareFailure {
    #[default]
    Unclassified,
    SkippedPipeline,
    MissingHostPipeline,
    PipelineKind,
    IndexBuffer,
    DynamicDescriptorTable,
    ShaderMemoryViews,
    RootUavBuffer,
    RootUavHeap,
    RootUavSlot,
    RootSrvHeap,
    RootSrvSlot,
}

/// Keep the first example of each failure per pipeline, with room for every
/// kind even when one compute pipeline repeatedly fails during world load.
#[cfg(all(windows, feature = "diagnostics"))]
fn new_indirect_example(
    seen: &mut Vec<(u32, String, PrepareFailure)>,
    kind: u32,
    key: &str,
    why: PrepareFailure,
) -> bool {
    const PER_KIND: usize = 8;
    if seen.iter().any(|(k, p, r)| *k == kind && p == key && *r == why)
        || seen.iter().filter(|(k, _, _)| *k == kind).count() >= PER_KIND
    {
        return false;
    }
    seen.push((kind, key.to_owned(), why));
    true
}

#[cfg(all(windows, feature = "diagnostics"))]
pub(crate) fn note_indirect_prepare(
    this: This,
    why: PrepareFailure,
    (kind, stride, max): (u32, u32, u32),
    (args, address): (&crate::shadow::Shadow, u64),
    counter: Option<&(Arc<crate::shadow::Shadow>, u64)>,
) {
    static ON: OnceLock<bool> = OnceLock::new();
    if !*ON.get_or_init(|| std::env::var_os("VICTORY_INDIRECT_PREPARE_LOG").is_some_and(|v| v == "1"))
    {
        return;
    }
    static SEEN: Mutex<Vec<(u32, String, PrepareFailure)>> = Mutex::new(Vec::new());
    let key = shaders_of(this);
    if !new_indirect_example(
        &mut SEEN.lock().unwrap_or_else(|e| e.into_inner()),
        kind,
        &key,
        why,
    ) {
        return;
    }
    let shaders: Vec<_> = if this.is_null() {
        Vec::new()
    } else {
        pipeline(this)
            .shaders
            .iter()
            .map(|s| (s.stage, s.hash.as_str()))
            .collect()
    };
    let command = match kind {
        0 => "draw",
        1 => "draw indexed",
        2 => "dispatch",
        10 => "dispatch mesh",
        _ => "unknown",
    };
    tracing::info!(
        frame = crate::frame::current(),
        kind,
        command,
        stride,
        max,
        reason = ?why,
        pipeline = ?this,
        guest_shaders = ?shaders,
        arguments = format_args!("{address:#x}"),
        argument_buffer = args.id(),
        argument_offset = args.offset(address),
        count_address = counter.map(|(_, at)| *at),
        count_buffer = counter.map(|(s, _)| s.id()),
        count_offset = counter.map(|(s, at)| s.offset(*at)),
        "indirect command preparation failed"
    );
}

/// Counts a draw or dispatch dropped for `why` (diagnostics).
#[cfg(windows)]
pub(crate) fn note_why(why: String) {
    #[cfg(feature = "diagnostics")]
    crate::capture::op(0, || format!("DROPPED {why}"));
    let mut drops = DROPS.lock().unwrap_or_else(|e| e.into_inner());
    *drops
        .get_or_insert_with(HashMap::new)
        .entry(why)
        .or_default() += 1;
}

/// The drops counted since the last call, most first (diagnostics).
#[cfg(windows)]
pub(crate) fn take_drops() -> Vec<(String, u64)> {
    let drops = DROPS.lock().unwrap_or_else(|e| e.into_inner()).take();
    let mut drops: Vec<(String, u64)> = drops.into_iter().flatten().collect();
    // Frame reports only show the first 40 reasons. Keep ties ordered so
    // a HashMap iteration order change cannot change which reasons appear.
    drops.sort_unstable_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    drops
}

/// The stage's container, if it has one.
fn copy(stage: &'static str, code: &Bytecode) -> Option<Arc<Shader>> {
    if code.code.is_null() || code.size == 0 {
        return None;
    }
    // SAFETY: the title passes `size` bytes of shader code.
    Some(shader(stage, unsafe {
        std::slice::from_raw_parts(code.code, code.size)
    }))
}

fn hand_out(
    shaders: Vec<Arc<Shader>>,
    #[cfg(windows)] graphics: Option<Box<host::GraphicsState>>,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    tracing::trace!(?shaders, "pipeline state");
    #[cfg(windows)]
    let host = Arc::new(make::Pending::new(shaders.clone(), graphics));
    #[cfg(windows)]
    make::early(&host);
    #[cfg(windows)]
    translate_ahead::ahead(&shaders);
    #[cfg(windows)]
    let builder = of_builder(&shaders);
    #[cfg(windows)]
    if builder {
        // Another stage: the group is looked at again at its next dispatch.
        let mut b = BUILDER.lock().unwrap_or_else(|e| e.into_inner());
        b.pipelines.push(host.clone());
        b.whole = false;
    }
    let pipeline = Box::new(Pipeline {
        object: Object::new(&FUNCTIONS),
        packet: [0; 8],
        #[cfg(windows)]
        host,
        #[cfg(windows)]
        builder,
        #[cfg(windows)]
        rt_only: shaders
            .iter()
            .filter(|s| s.stage == "compute")
            .map(|s| for_rays(&s.container))
            .find(|k| *k != RayOnly::No)
            .unwrap_or(RayOnly::No),
        #[cfg(windows)]
        pre_pass: shaders
            .iter()
            .filter(|s| s.stage == "compute")
            .find_map(|s| pre_pass_of(&s.container)),
        shaders,
        bound: std::sync::atomic::AtomicBool::new(false),
    });
    object::hand_out(PIPELINE, pipeline, iid, &PIPELINE_IIDS, out)
}

/// Device method.
pub(crate) extern "system" fn create_graphics(
    _: This,
    desc: *const GraphicsHead,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    // SAFETY: the title passes a description (null is tolerated).
    let Some(desc) = (unsafe { desc.as_ref() }) else {
        return E_INVALIDARG;
    };
    let stages = GRAPHICS_STAGES.iter().zip(&desc.stages);
    let shaders = stages.filter_map(|(s, c)| copy(s, c)).collect();
    hand_out(
        shaders,
        #[cfg(windows)]
        None,
        iid,
        out,
    )
}

/// Device method.
pub(crate) extern "system" fn create_compute(
    _: This,
    desc: *const ComputeHead,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    // SAFETY: the title passes a description (null is tolerated).
    let Some(desc) = (unsafe { desc.as_ref() }) else {
        return E_INVALIDARG;
    };
    hand_out(
        copy("compute", &desc.stage).into_iter().collect(),
        #[cfg(windows)]
        None,
        iid,
        out,
    )
}

/// Device method 0x1f0: the plain compute description plus console
/// extensions `(desc, extension count, extensions, iid, out)`.
/// shortcut: the extensions (console shader settings) are ignored, as is
/// the rest of the description.
pub(crate) extern "system" fn create_compute_x(
    this: This,
    desc: *const ComputeHead,
    _extensions: u32,
    _extended: *const u8,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    create_compute(this, desc, iid, out)
}

/// Slot 0x218 tries a precompiled compute pipeline. On a miss the caller
/// retries at 0x1f0 with its full shader container.
pub(crate) extern "system" fn cached_compute(
    _: This,
    _desc: *const u8,
    _iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    use std::sync::atomic::{AtomicU64, Ordering};
    static MISSES: AtomicU64 = AtomicU64::new(0);
    if !out.is_null() {
        // SAFETY: the caller supplies the pipeline out pointer.
        unsafe { out.write(std::ptr::null()) };
    }
    let misses = MISSES.fetch_add(1, Ordering::Relaxed) + 1;
    if misses.is_power_of_two() {
        tracing::info!(
            misses,
            "precompiled compute cache miss at 0x218; the caller can retry with its shader at 0x1f0"
        );
    }
    object::E_NOTIMPL
}

/// A pipeline described as a stream: its size and the subobjects, each a
/// type then its data, padded to 8 bytes.
#[repr(C)]
pub(crate) struct StreamDesc {
    size: usize,
    stream: *const u8,
}

/// A subobject type's size (type included), and the stage it is if it is a
/// shader. The console numbers amplification and mesh shaders 23 and 24
/// (PC: 24 and 25); its own types from 25 on are not sized yet.
fn subobject(kind: u32) -> Option<(usize, Option<&'static str>)> {
    let stage = |name| Some((24, Some(name)));
    match kind {
        1 => stage("vertex"),
        2 => stage("pixel"),
        3 => stage("domain"),
        4 => stage("hull"),
        5 => stage("geometry"),
        6 => stage("compute"),
        23 => stage("amplification"),
        24 => stage("mesh"),
        0 => Some((16, None)),
        12 | 19 => Some((24, None)),
        7 | 15 => Some((40, None)),
        8 => Some((336, None)),
        10 => Some((48, None)),
        11 => Some((56, None)),
        17 => Some((16, None)),
        21 => Some((64, None)),
        22 => Some((32, None)),
        9 | 13 | 14 | 16 | 18 | 20 => Some((8, None)),
        _ => None,
    }
}

/// Device method: a pipeline from a stream; the shaders are kept, and the
/// graphics state for the host pipeline.
pub(crate) extern "system" fn create_stream(
    _: This,
    desc: *const StreamDesc,
    iid: *const Iid,
    out: *mut This,
) -> HRESULT {
    // SAFETY: the title passes a description (null is tolerated).
    let Some(desc) = (unsafe { desc.as_ref() }) else {
        return E_INVALIDARG;
    };
    if desc.stream.is_null() {
        return E_INVALIDARG;
    }
    let mut shaders = Vec::new();
    #[cfg(windows)]
    let mut graphics = host::GraphicsState::default();
    let mut at = 0;
    while at + 8 <= desc.size {
        // SAFETY: inside the stream the title passes.
        let kind = unsafe { desc.stream.add(at).cast::<u32>().read_unaligned() };
        let Some((size, stage)) = subobject(kind) else {
            tracing::warn!(
                kind,
                at,
                "pipeline subobject of unknown size; the rest is ignored"
            );
            break;
        };
        if let Some(stage) = stage {
            // SAFETY: a shader subobject has its bytecode at +8.
            let code = unsafe { &*desc.stream.add(at + 8).cast::<Bytecode>() };
            shaders.extend(copy(stage, code));
        }
        #[cfg(windows)]
        // SAFETY: a whole subobject of this kind lies at `at`.
        unsafe {
            graphics.read(kind, desc.stream.add(at))
        };
        at += size;
    }
    hand_out(
        shaders,
        #[cfg(windows)]
        Some(Box::new(graphics)),
        iid,
        out,
    )
}

extern "system" fn query(this: This, iid: *const Iid, out: *mut This) -> HRESULT {
    let found = object::is(iid, &PIPELINE_IIDS).then_some(this);
    object::answer(PIPELINE, iid, found, out)
}
extern "system" fn release(this: This) -> u32 {
    object::release_boxed::<Pipeline>(this)
}

/// IUnknown 3, object 4, device child 1, pipeline 3.
static FUNCTIONS: Table<24> = {
    let mut f = object::d3d12_object(object::missing_table::<PIPELINE, 24>());
    f[0] = query as *const ();
    f[2] = release as *const ();
    f[7] = object::parent_device as *const (); // 0x38
    Table(f)
};

#[cfg(windows)]
pub(crate) mod host;
mod early;
#[cfg(windows)]
mod library;
#[cfg(windows)]
pub(crate) mod samplers;
#[cfg(windows)]
mod make;
#[cfg(windows)]
mod eager;
#[cfg(windows)]
mod recipe;
#[cfg(windows)]
pub use recipe::{Warmed as RecipesWarmed, warm as warm_recipes};
#[cfg(windows)]
mod translate_ahead;

/// Lets the pipeline library's mapped file go from memory when no pipeline
/// has loaded for a while (`library::trim`); called by the host's sweep.
#[cfg(windows)]
pub(crate) fn trim_library() {
    library::trim();
}

/// The pipeline library's part of the report when the host device is
/// removed (`library::removal_note`).
#[cfg(windows)]
pub(crate) fn library_removal_note() -> String {
    library::removal_note()
}

/// Keeps compiled host pipelines, and the list of title pipelines to start
/// early, in `dir` (a folder of the cache) across runs.
#[cfg(windows)]
pub fn use_pipeline_cache(dir: std::path::PathBuf) {
    library::use_pipeline_cache(dir);
}

/// Saves the pipeline cache now if anything was added (at the end of a run).
#[cfg(windows)]
pub fn save_pipeline_cache() {
    library::save(true);
}

#[cfg(test)]
mod tests {
    #[cfg(all(windows, feature = "diagnostics"))]
    #[test]
    fn skip_hashes_accepts_full_hashes_only() {
        assert!(super::parse_skip_hashes(" , ").is_empty());
        assert_eq!(
            super::parse_skip_hashes(" 0123456789ABCDEF, 0123456789abcde, xyz, 0011223344556677,"),
            ["0123456789abcdef", "0011223344556677"]
        );
    }

    use super::*;
    use crate::object::{S_OK, slot};

    fn shaders(p: This) -> Vec<(&'static str, Vec<u8>)> {
        pipeline(p)
            .shaders
            .iter()
            .map(|s| (s.stage, s.container.clone()))
            .collect()
    }

    #[test]
    fn pipelines_keep_their_shaders() {
        let (vs, ps) = ([1u8, 2, 3], [4u8; 5]);
        let none = || Bytecode {
            code: std::ptr::null(),
            size: 0,
        };
        let desc = GraphicsHead {
            root: std::ptr::null(),
            stages: [
                Bytecode {
                    code: vs.as_ptr(),
                    size: vs.len(),
                },
                Bytecode {
                    code: ps.as_ptr(),
                    size: ps.len(),
                },
                none(),
                none(),
                none(),
            ],
        };
        let mut p = std::ptr::null();
        assert_eq!(
            create_graphics(std::ptr::null(), &desc, &PIPELINE_IIDS[0], &mut p),
            S_OK
        );
        assert_eq!(
            shaders(p),
            [("vertex", vs.to_vec()), ("pixel", ps.to_vec())]
        );
        // A second pipeline with the same pixel shader shares its container.
        let mut q = std::ptr::null();
        create_graphics(std::ptr::null(), &desc, &PIPELINE_IIDS[0], &mut q);
        assert!(Arc::ptr_eq(
            &pipeline(p).shaders[1],
            &pipeline(q).shaders[1]
        ));
        assert_eq!(slot::<extern "system" fn(This) -> u32>(p, 0x10)(p), 0);
    }

    /// The stream layout the title builds (title+0x86f090): root
    /// signature, five shader slots (the first a mesh shader), blend, sample
    /// mask, rasterizer, depth-stencil 1, sample description, input layout,
    /// topology, render target and depth formats: 0x2b0 bytes.
    #[test]
    fn streams_yield_their_shaders() {
        let (ms, ps) = ([9u8; 7], [4u8; 5]);
        let mut stream = vec![0u8; 0x2b0];
        let mut put = |at: usize, v: u64| stream[at..at + 8].copy_from_slice(&v.to_le_bytes());
        for (at, kind) in [
            (0x10, 24),
            (0x28, 2),
            (0x40, 3),
            (0x58, 4),
            (0x70, 5),
            (0x88, 8),
            (0x1d8, 9),
            (0x1e0, 10),
            (0x210, 21),
            (0x250, 17),
            (0x260, 12),
            (0x278, 14),
            (0x280, 15),
            (0x2a8, 16),
        ] {
            put(at, kind);
        }
        put(0x18, ms.as_ptr() as u64);
        put(0x20, ms.len() as u64);
        put(0x30, ps.as_ptr() as u64);
        put(0x38, ps.len() as u64);
        // Topology type 3 (triangles), one render target of format 28.
        put(0x278, 14 | 3 << 32);
        put(0x280, 15 | 28 << 32);
        put(0x2a0, 1 << 32);
        let desc = StreamDesc {
            size: stream.len(),
            stream: stream.as_ptr(),
        };
        let mut p = std::ptr::null();
        let hr = create_stream(std::ptr::null(), &desc, &PIPELINE_IIDS[0], &mut p);
        assert_eq!(hr, S_OK);
        assert_eq!(shaders(p), [("mesh", ms.to_vec()), ("pixel", ps.to_vec())]);
        #[cfg(windows)]
        {
            let g = pipeline(p).host.graphics.as_deref().unwrap();
            assert_eq!(g.topology.0, 3);
            assert_eq!(
                (g.targets.NumRenderTargets, g.targets.RTFormats[0].0),
                (1, 28)
            );
        }
    }

    #[test]
    fn cached_compute_miss_falls_back_to_the_compute_container() {
        use crate::object::slot;
        type Cached = extern "system" fn(This, *const u8, *const Iid, *mut This) -> HRESULT;
        type Compute = extern "system" fn(
            This,
            *const ComputeHead,
            u32,
            *const u8,
            *const Iid,
            *mut This,
        ) -> HRESULT;
        let device = crate::device::device();
        let pair_data = [0u32; 7];
        let mut entry = [0usize; 9];
        for pair in entry[1..].chunks_exact_mut(2) {
            pair.copy_from_slice(&[pair_data.as_ptr() as usize, size_of_val(&pair_data)]);
        }
        let mut p = device;
        assert!(
            slot::<Cached>(device, 0x218)(device, entry.as_ptr().cast(), &PIPELINE_IIDS[0], &mut p)
                < 0
        );
        assert!(p.is_null());
        let code = [1u8, 2, 3, 4];
        let desc = ComputeHead {
            root: std::ptr::null(),
            stage: Bytecode {
                code: code.as_ptr(),
                size: code.len(),
            },
        };
        assert_eq!(
            slot::<Compute>(device, 0x1f0)(
                device,
                &desc,
                0,
                std::ptr::null(),
                &PIPELINE_IIDS[0],
                &mut p
            ),
            S_OK
        );
        assert_eq!(shaders(p), [("compute", code.to_vec())]);
        slot::<extern "system" fn(This) -> u32>(p, 0x10)(p);
    }

    #[cfg(all(windows, feature = "diagnostics"))]
    #[test]
    fn indirect_prepare_examples_preserve_each_command_kind() {
        use PrepareFailure::{DynamicDescriptorTable, IndexBuffer};

        let mut seen = Vec::new();
        assert!(new_indirect_example(
            &mut seen,
            2,
            "first",
            DynamicDescriptorTable
        ));
        assert!(!new_indirect_example(
            &mut seen,
            2,
            "first",
            DynamicDescriptorTable
        ));
        for key in 1..8 {
            assert!(new_indirect_example(
                &mut seen,
                2,
                &key.to_string(),
                DynamicDescriptorTable
            ));
        }
        assert!(!new_indirect_example(
            &mut seen,
            2,
            "later",
            DynamicDescriptorTable
        ));
        assert!(new_indirect_example(
            &mut seen,
            10,
            "first",
            DynamicDescriptorTable
        ));
        assert!(new_indirect_example(
            &mut seen,
            1,
            "other",
            DynamicDescriptorTable
        ));
        assert!(new_indirect_example(&mut seen, 1, "other", IndexBuffer));
    }

    #[cfg(all(windows, feature = "diagnostics"))]
    #[test]
    fn parse_skip_hashes_normalizes_and_filters_entries() {
        let set: std::collections::HashSet<String> = parse_skip_hashes(
            " 0123456789ABCDEF , invalid , fedcba9876543210, 0123456789abcdef ",
        )
        .into_iter()
        .collect();
        assert_eq!(set.len(), 2);
        assert!(set.contains("0123456789abcdef"));
        assert!(set.contains("fedcba9876543210"));
    }
}
