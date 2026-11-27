//! Translated shaders kept on disk (decision 0003): under the configured
//! cache folder, keyed by the container's content and by the translator
//! that translated it (its source fingerprint and settings, see `build`), so
//! a changed translator never serves an old translation. Entries go through
//! `victory-pipeline`'s content-addressed cache, which writes them
//! atomically; an entry that does not read back is translated again.
//!
//! The entry is our own encoding of [`HostShader`]: a field added to it
//! must be added to `encode` and `decode` too (entries of older builds
//! are ignored anyway).

use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use victory_pipeline::{Cache, Converter};
use victory_shader::emit::{HostShader, StaticSampler, UserData};
use victory_shader::resource::RootIndexedView;
use victory_shader::{HwStage, RootType};

static CACHE: OnceLock<Cache> = OnceLock::new();
static HITS: AtomicU64 = AtomicU64::new(0);
static MISSES: AtomicU64 = AtomicU64::new(0);

/// Keeps translated shaders in `dir` from now on.
pub fn use_shader_cache(dir: PathBuf) {
    match Cache::open(dir) {
        Ok(cache) => {
            let _ = CACHE.set(cache);
        }
        Err(e) => tracing::warn!(error = %e, "no shader cache"),
    }
}

/// Shaders found in the cache and translated since the start.
pub(crate) fn counts() -> (u64, u64) {
    (HITS.load(Ordering::Relaxed), MISSES.load(Ordering::Relaxed))
}

/// What identifies the translation, besides the container: the shader
/// crate's fingerprint (its source, every switch it reads, the host facts the
/// device gave it, the loop limit, barrier waits and clean-up passes) and
/// this file's encoding of the result. Not the whole executable any more: a
/// rebuild that leaves the translator alone keeps its translations (each new
/// build used to translate the title's thousands of shaders again, which kept
/// the first minutes of every test run short of pipelines).
/// `VICTORY_SHADER_CACHE_KEY=exe` restores the old key (the executable's hash).
/// Worked out per lookup, so a translation made before the device said its
/// wave size is never filed under the later key.
fn build(wave_wide: bool, wave_pairs: bool, wave_pairs_vt: bool) -> [u8; 32] {
    static WARNED: std::sync::Once = std::sync::Once::new();
    let limit = victory_shader::emit::loop_limit();
    if limit != victory_shader::emit::DEFAULT_LOOP_LIMIT {
        WARNED.call_once(|| {
            tracing::warn!(
                limit,
                "VICTORY_SHADER_LOOP_LIMIT: translated loops stop sooner"
            )
        });
    }
    static EXE: OnceLock<Option<[u8; 32]>> = OnceLock::new();
    let exe = EXE.get_or_init(|| {
        if std::env::var_os("VICTORY_SHADER_CACHE_KEY").is_some_and(|v| v == "exe") {
            match std::env::current_exe().and_then(std::fs::read) {
                Ok(bytes) => Some(*blake3::hash(&bytes).as_bytes()),
                Err(e) => {
                    tracing::warn!(error = %e, "cannot read our executable; keying on the translator");
                    None
                }
            }
        } else {
            None
        }
    });
    // (Neptune) The view heap's null slot is part of the translation (it differs by host
    // GPU): found before it goes into the key, so that no translation is made without it.
    crate::descriptor::host::probe_view_limit();
    let _env_read = WAVE_WIDE_ENV.read().unwrap_or_else(|e| e.into_inner());
    let mut h = blake3::Hasher::new();
    h.update(b"victory shader cache key 2\0");
    if let Some(exe) = exe {
        h.update(exe);
    }
    h.update(&victory_shader::emit::translation_fingerprint());
    if victory_shader::emit::constants_raw() {
        h.update(b"VICTORY_PORT_CBV=1");
    }
    if victory_shader::emit::wave_collapse() {
        h.update(b"VICTORY_WAVE_COLLAPSE=1");
    }
    if victory_shader::emit::meet::enabled() {
        h.update(b"VICTORY_MEET_PRUNE=1");
    }
    if victory_shader::emit::exchange_pairs() {
        h.update(b"VICTORY_EXCHANGE_PAIRS=1");
    }
    if victory_shader::emit::nvidia::enabled() {
        h.update(b"VICTORY_WAVE32_NVIDIA=1");
    }
    if wave_wide && std::env::var_os("VICTORY_WAVE_WIDE").is_none() {
        h.update(b"VICTORY_WAVE_WIDE_ONLY=last");
    }
    if wave_pairs && std::env::var_os("VICTORY_WAVE_PAIRS").is_some_and(|v| v == "0") {
        h.update(b"VICTORY_WAVE_PAIRS_ONLY=1");
    }
    if wave_pairs_vt {
        h.update(b"VICTORY_WAVE_PAIRS_VT=1");
    }
    if victory_shader::emit::native_glass() {
        h.update(b"VICTORY_NATIVE_GLASS=1");
    }
    if let Some(key) = victory_shader::emit::miss_sky_key() {
        h.update(key.as_bytes());
    }
    if let Some(key) = crate::rt_hit_only::key() {
        h.update(key.as_bytes());
    }
    if victory_shader::emit::dispatchx_mask() {
        h.update(b"VICTORY_DISPATCHX_MASK=1");
    }
    h.update(include_bytes!("shader_cache.rs"));
    *h.finalize().as_bytes()
}

/// The containers (by the 16-digit hash the log names shaders with) whose
/// wave64 compute program may take the wave-wide form on a host of 32-lane
/// waves (`VICTORY_WAVE_WIDE=last`): `VICTORY_WAVE_WIDE_ONLY`, a list of
/// hashes, or by default the programs of this title that come out of it with
/// 32-lane waves and otherwise do not run at all on such a host (the driver
/// refuses their 64-lane form: water transmission volumetrics, both kinds;
/// particle visibility; the tiling of the screen-space reflection pass; the
/// effects heap's relocations, `CS_DefragHeap_ComputeRelocations`; the
/// meshlets' triangle collection, `CS_CollectTriangles_Meshlets`; and
/// `CS_AlphTransmissionPrefilterMip` as fallback). `VICTORY_WAVE_WIDE_ONLY=none`
/// or `0` allows none.
static WAVE_WIDE: OnceLock<Vec<String>> = OnceLock::new();
const WAVE_WIDE_DEFAULT: [&str; 7] = [
    "5dc3b25986622949",
    "6f225b641c5b0708",
    "369cbe906ea72764",
    "2cc4c197ee29e291",
    "02384a60fda6f2d0",
    "6b82bc7d22016cb5",
    "2da4beb386509b77",
];

/// The containers (by the 16-digit hash the log names shaders with) whose
/// wave64 compute program may take the paired-lanes form (`VICTORY_WAVE_PAIRS=1`)
/// even when `VICTORY_WAVE_PAIRS=0` disables it globally: `VICTORY_WAVE_PAIRS_ONLY`,
/// a list of hashes, or by default the two compute shaders of this title that
/// still require paired lanes on a 32-lane host (`CS_DefragHeap_ComputeRelocations`
/// and `CS_CollectTriangles_Meshlets`). `VICTORY_WAVE_PAIRS_ONLY=none` or `0`
/// allows none.
static WAVE_PAIRS: OnceLock<Vec<String>> = OnceLock::new();
const WAVE_PAIRS_DEFAULT: [&str; 2] = ["02384a60fda6f2d0", "6b82bc7d22016cb5"];

/// `VICTORY_WAVE_PAIRS_VT=1` (off by default): the compute programs TaynanGT/Vice-Tay #208
/// pairs by default take the paired wave64 form with that pull request's rules first
/// (`victory_shader::emit::with_wave_pairs_vt`; the other forms if it refuses one). On a host
/// of 32-lane waves their wave-wide sums, first-lane reads and LDS shared within a console wave
/// are exact only in that form; the others left (RTX 4090) whole 16x16 tiles black or lit in
/// the tiled shadow mask, dark blocks on open water and a halo around dark silhouettes against
/// the sky. Only these containers' cache keys change with the switch.
const WAVE_PAIRS_VT: [&str; 29] = [
    // shadow mask, `CS_ShadowRenderTiled*`
    "191ecaed66eea3b1", "1d44a723bdebbf27", "20b78ee12ecdb57d", "240cb516c1af0bc3",
    "31d703207b51549a", "466ec41d3f9d24c0", "56f3bf2d9388e5a7", "65de113e23fa4fdd",
    "804b342cb4b9d1e9", "922e1d0ee1e19a85", "ab5455fc6abb804f", "abd7a22106898c66",
    "b4b0427232707598", "be6ce5d849d51127", "ce79e9eba7ae0348", "d7db57731ada919b",
    "f133e35efed2a48a", "fa0bd10a35bfef3c",
    // water, `CS_TransmissionVolumetrics_Tiled`, `_Tiled_LocalLights`, `_GenerateTiles`,
    // `_Submerged`
    "5dc3b25986622949", "6f225b641c5b0708", "926c46bab3b86bd6", "1aa247ed3653b616",
    // atmospheric diffusion and its depth scan, `CS_GatherDiffusion`, `CS_ScanMip0..3`,
    // `CS_DeferredFallbacksAlpha_SkyProbe`, `CS_TemporalResolve2_Opaque_FrameStatistics`
    "ce2378e5e8711aad", "b7953a05b189dfc1", "71883e40620a567a",
    "3c196d81eca7ff15", "acf14a70f37fb012", "f79824724d8e2daf", "f0aa2b1e79275592",
];

fn wave_pairs_vt_on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("VICTORY_WAVE_PAIRS_VT").is_ok_and(|v| v.trim() == "1"))
}

fn allow_wave_pairs_vt(container: &[u8]) -> bool {
    wave_pairs_vt_on() && WAVE_PAIRS_VT.contains(&&blake3::hash(container).to_hex()[..16])
}

/// Whether `VICTORY_WAVE_PAIRS_VT=1` names this container (its 16-digit log hash): for the
/// offline translation (`victory translate`), which calls the translator without this cache.
pub fn wave_pairs_vt_listed(container: &[u8]) -> bool {
    allow_wave_pairs_vt(container)
}

/// [`translate_container`], for a container [`allow_wave_pairs_vt`] names inside
/// `with_wave_pairs_vt`, with one log line saying what the paired form made of it.
fn translate_vt(container: &[u8], wide: bool, pairs: bool, vt: bool) -> Result<HostShader, String> {
    victory_shader::emit::with_rays_missed(crate::rt_hit_only::misses(container), || {
        translate_paired_vt(container, wide, pairs, vt)
    })
}

/// [`translate_vt`] inside `VICTORY_RT_HIT_ONLY`'s choice.
fn translate_paired_vt(container: &[u8], wide: bool, pairs: bool, vt: bool) -> Result<HostShader, String> {
    if !vt {
        return translate_container(container, wide, pairs);
    }
    let r = victory_shader::emit::with_wave_pairs_vt(true, || translate_container(container, wide, pairs));
    let hex = blake3::hash(container).to_hex();
    let shader = &hex[..16];
    match victory_shader::emit::take_wave_pairs_vt() {
        Some(Ok(())) => tracing::info!(shader, "wave pairs vt: paired Wave64"),
        Some(Err(why)) => tracing::warn!(shader, why, "wave pairs vt: refused, other forms"),
        None => tracing::warn!(shader, "wave pairs vt: not tried (not wave64 compute on 32-lane waves)"),
    }
    r
}

/// Serializes temporary `VICTORY_WAVE_WIDE=last` and `VICTORY_WAVE_PAIRS=1`
/// overrides for allowlisted containers so concurrent translations and
/// `translation_fingerprint()` never observe the overrides on non-allowlisted
/// shaders.
static WAVE_WIDE_ENV: std::sync::RwLock<()> = std::sync::RwLock::new(());

fn parse_hash_list(value: Option<&str>, default: &[&str]) -> Vec<String> {
    match value {
        None => default.iter().map(|&s| s.to_owned()).collect(),
        Some(raw) => {
            let trimmed = raw.trim();
            if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("none") || trimmed == "0" {
                Vec::new()
            } else {
                trimmed
                    .split(',')
                    .map(|s| s.trim().to_ascii_lowercase())
                    .filter(|s| !s.is_empty())
                    .collect()
            }
        }
    }
}

/// `VICTORY_WATER_WIDE=1` (issue #38): the water transmission programs the list above leaves
/// out, which no other form takes on a 32-lane host: `CS_TransmissionVolumetrics` and
/// `_Submerged` are refused (their dispatches dropped), `_VolumePortalsOnly` comes out with
/// 64-lane waves. The wave-wide form gives each 32-lane waves, as it does their tiled siblings.
const WATER_WIDE: [&str; 3] = ["6871d6e4b3ac9a10", "926c46bab3b86bd6", "a10179ab7eda0dc3"];

fn parse_wave_wide_only(value: Option<&str>, water: bool) -> Vec<String> {
    let mut list = parse_hash_list(value, &WAVE_WIDE_DEFAULT);
    if water {
        for h in WATER_WIDE {
            if !list.iter().any(|s| s == h) {
                list.push(h.to_owned());
            }
        }
    }
    list
}

fn parse_wave_pairs_only(value: Option<&str>) -> Vec<String> {
    parse_hash_list(value, &WAVE_PAIRS_DEFAULT)
}

fn wave_wide_list() -> &'static [String] {
    WAVE_WIDE.get_or_init(|| {
        let list = parse_wave_wide_only(
            std::env::var("VICTORY_WAVE_WIDE_ONLY").ok().as_deref(),
            std::env::var("VICTORY_WATER_WIDE").is_ok_and(|v| v.trim() == "1"),
        );
        let more = crate::wave_more::wide(std::env::var("VICTORY_WAVE_WIDE_MORE").ok().as_deref());
        crate::wave_more::extend(list, &more)
    })
}

fn wave_pairs_list() -> &'static [String] {
    WAVE_PAIRS.get_or_init(|| {
        let list = parse_wave_pairs_only(std::env::var("VICTORY_WAVE_PAIRS_ONLY").ok().as_deref());
        let more = crate::wave_more::pairs(std::env::var("VICTORY_WAVE_PAIRS_MORE").ok().as_deref());
        crate::wave_more::extend(list, &more)
    })
}

fn hash_in_list(container: &[u8], list: &[String]) -> bool {
    if list.is_empty() {
        return false;
    }
    let hex = blake3::hash(container).to_hex();
    let short = &hex[..16];
    list.iter().any(|h| h == short)
}

fn allow_wave_wide(container: &[u8]) -> bool {
    hash_in_list(container, wave_wide_list())
}

fn allow_wave_pairs(container: &[u8]) -> bool {
    hash_in_list(container, wave_pairs_list())
}

fn translate_container(
    container: &[u8],
    wave_wide: bool,
    wave_pairs: bool,
) -> Result<HostShader, String> {
    let need_wide = wave_wide && std::env::var_os("VICTORY_WAVE_WIDE").is_none();
    let need_pairs = wave_pairs && std::env::var_os("VICTORY_WAVE_PAIRS").is_some_and(|v| v == "0");
    if need_wide || need_pairs {
        let _guard = WAVE_WIDE_ENV.write().unwrap_or_else(|e| e.into_inner());
        let set_wide = wave_wide && std::env::var_os("VICTORY_WAVE_WIDE").is_none();
        let set_pairs =
            wave_pairs && std::env::var_os("VICTORY_WAVE_PAIRS").is_some_and(|v| v == "0");
        if set_wide || set_pairs {
            struct ResetEnv {
                restore_wide: bool,
                restore_pairs: bool,
            }
            impl Drop for ResetEnv {
                fn drop(&mut self) {
                    // SAFETY: serialized by `WAVE_WIDE_ENV`; restored before unlocking.
                    if self.restore_wide {
                        unsafe { std::env::remove_var("VICTORY_WAVE_WIDE") };
                    }
                    if self.restore_pairs {
                        unsafe { std::env::set_var("VICTORY_WAVE_PAIRS", "0") };
                    }
                }
            }
            if set_wide {
                // SAFETY: serialized by `WAVE_WIDE_ENV`; restored by `ResetEnv`.
                unsafe { std::env::set_var("VICTORY_WAVE_WIDE", "last") };
            }
            if set_pairs {
                // SAFETY: serialized by `WAVE_WIDE_ENV`; restored by `ResetEnv`.
                unsafe { std::env::set_var("VICTORY_WAVE_PAIRS", "1") };
            }
            let _reset = ResetEnv {
                restore_wide: set_wide,
                restore_pairs: set_pairs,
            };
            return victory_shader::emit::translate(container).map_err(|e| e.to_string());
        }
    }
    let _guard = WAVE_WIDE_ENV.read().unwrap_or_else(|e| e.into_inner());
    victory_shader::emit::translate(container).map_err(|e| e.to_string())
}

/// The offline warm-up (`victory translate --warm`): `container` is
/// translated into the cache unless it is there already. `Ok(true)`: it
/// was translated now. Needs [`use_shader_cache`] and the same host facts
/// the run will have (`victory_shader::emit::translation_fingerprint`).
pub fn warm(container: &[u8]) -> Result<bool, String> {
    let cache = CACHE.get().ok_or("no shader cache")?;
    let wide = allow_wave_wide(container);
    let pairs = allow_wave_pairs(container);
    let vt = allow_wave_pairs_vt(container);
    let ran = AtomicBool::new(false);
    let translate = |c: &[u8]| {
        ran.store(true, Ordering::Relaxed);
        translate_vt(c, wide, pairs, vt)
    };
    cached(cache, &build(wide, pairs, vt), container, translate).map(|_| ran.load(Ordering::Relaxed))
}

/// The translation of a console shader container, from the cache when
/// there is one.
pub(crate) fn host_shader(container: &[u8]) -> Result<HostShader, String> {
    let wide = allow_wave_wide(container);
    let pairs = allow_wave_pairs(container);
    let vt = allow_wave_pairs_vt(container);
    let translate = |c: &[u8]| translate_vt(c, wide, pairs, vt);
    match CACHE.get() {
        Some(cache) => cached(cache, &build(wide, pairs, vt), container, translate),
        None => translate(container),
    }
}

/// The cache entry's converter: the build and the container in, the
/// encoded translation out. `ran` tells a miss from a hit.
struct Translate<F> {
    translate: F,
    ran: AtomicBool,
}

impl<F: Fn(&[u8]) -> Result<HostShader, String> + Send + Sync> Converter for Translate<F> {
    fn id(&self) -> &'static str {
        "shader.dxil"
    }
    fn version(&self) -> u32 {
        // The build fingerprint in the input does the versioning.
        1
    }
    fn convert(&self, input: &[u8]) -> anyhow::Result<Vec<u8>> {
        self.ran.store(true, Ordering::Relaxed);
        let shader = (self.translate)(&input[32..]).map_err(anyhow::Error::msg)?;
        Ok(encode(&shader))
    }
}

fn cached(
    cache: &Cache,
    build: &[u8; 32],
    container: &[u8],
    translate: impl Fn(&[u8]) -> Result<HostShader, String> + Send + Sync,
) -> Result<HostShader, String> {
    let input = [&build[..], container].concat();
    let converter = Translate {
        translate,
        ran: AtomicBool::new(false),
    };
    for _ in 0..2 {
        let path = cache
            .get_or_convert(&converter, &input)
            .map_err(|e| format!("{e:#}"))?;
        if let Some(shader) = std::fs::read(&path).ok().and_then(|b| decode(&b)) {
            let (counter, what) = if converter.ran.load(Ordering::Relaxed) {
                (&MISSES, "translated and cached")
            } else {
                (&HITS, "from the cache")
            };
            counter.fetch_add(1, Ordering::Relaxed);
            tracing::debug!(path = %path.display(), "shader {what}");
            return Ok(shader);
        }
        tracing::debug!(path = %path.display(), "unreadable shader cache entry; translating again");
        let _ = std::fs::remove_file(&path);
    }
    MISSES.fetch_add(1, Ordering::Relaxed);
    (converter.translate)(container)
}

const MAGIC: &[u8; 4] = b"VSH1";
const STAGES: [HwStage; 8] = [
    HwStage::Ls,
    HwStage::Hs,
    HwStage::Es,
    HwStage::Gs,
    HwStage::Vs,
    HwStage::Ps,
    HwStage::Ms,
    HwStage::Cs,
];

fn root_code(r: RootType) -> u32 {
    match r {
        RootType::Constants => 0,
        RootType::Descriptor => 1,
        RootType::Table => 2,
        RootType::ImmediateSampler => 3,
        RootType::ResourceHeap => 4,
        RootType::SamplerHeap => 5,
        RootType::Other(x) => 6 + x,
    }
}

fn root_type(code: u32) -> RootType {
    match code {
        0 => RootType::Constants,
        1 => RootType::Descriptor,
        2 => RootType::Table,
        3 => RootType::ImmediateSampler,
        4 => RootType::ResourceHeap,
        5 => RootType::SamplerHeap,
        x => RootType::Other(x - 6),
    }
}

/// Little-endian words and length-prefixed lists.
fn encode(s: &HostShader) -> Vec<u8> {
    let mut out = MAGIC.to_vec();
    let word = |out: &mut Vec<u8>, v: u32| out.extend(v.to_le_bytes());
    let stage = STAGES.iter().position(|&x| x == s.stage).unwrap_or(0) as u32;
    word(&mut out, stage);
    word(&mut out, s.dxil.len() as u32);
    out.extend(&s.dxil);
    word(&mut out, s.user_data.len() as u32);
    for u in &s.user_data {
        for v in [u.index, u32::from(u.slot), root_code(u.root), u.dword] {
            word(&mut out, v);
        }
    }
    word(&mut out, s.samplers.len() as u32);
    for sampler in &s.samplers {
        for &v in sampler {
            word(&mut out, v);
        }
    }
    word(&mut out, s.static_samplers.len() as u32);
    for t in &s.static_samplers {
        let [u, v, w] = t.address;
        let words = [
            t.filter,
            u,
            v,
            w,
            t.mip_lod_bias.to_bits(),
            t.max_anisotropy,
            t.comparison,
            t.border,
            t.min_lod.to_bits(),
            t.max_lod.to_bits(),
        ];
        for v in words {
            word(&mut out, v);
        }
    }
    for slots in [&s.root_cbvs, &s.root_srvs, &s.root_uavs] {
        word(&mut out, slots.len() as u32);
        out.extend(slots);
    }
    word(&mut out, s.root_indexed_views.len() as u32);
    for view in &s.root_indexed_views {
        for value in [
            u32::from(view.table_slot),
            u32::from(view.constant_slot),
            view.constant_dword,
            view.byte_offset,
            u32::from(view.shift),
        ] {
            word(&mut out, value);
        }
    }
    word(&mut out, s.half_views.len() as u32);
    for &(slot, dword) in &s.half_views {
        word(&mut out, u32::from(slot));
        word(&mut out, dword);
    }
    out
}

fn decode(bytes: &[u8]) -> Option<HostShader> {
    struct Reader<'a>(&'a [u8]);
    impl<'a> Reader<'a> {
        fn take(&mut self, n: usize) -> Option<&'a [u8]> {
            let (head, rest) = self.0.split_at_checked(n)?;
            self.0 = rest;
            Some(head)
        }
        fn word(&mut self) -> Option<u32> {
            Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
        }
        fn list(&mut self) -> Option<Vec<u8>> {
            let n = self.word()? as usize;
            Some(self.take(n)?.to_vec())
        }
    }
    let mut r = Reader(bytes);
    if r.take(4)? != MAGIC {
        return None;
    }
    let stage = *STAGES.get(r.word()? as usize)?;
    let dxil = r.list()?;
    let n = r.word()? as usize;
    let mut user_data = Vec::with_capacity(n.min(256));
    for _ in 0..n {
        user_data.push(UserData {
            index: r.word()?,
            slot: u8::try_from(r.word()?).ok()?,
            root: root_type(r.word()?),
            dword: r.word()?,
        });
    }
    let n = r.word()? as usize;
    let mut samplers = Vec::with_capacity(n.min(256));
    for _ in 0..n {
        samplers.push([r.word()?, r.word()?, r.word()?, r.word()?]);
    }
    let n = r.word()? as usize;
    let mut static_samplers = Vec::with_capacity(n.min(256));
    for _ in 0..n {
        static_samplers.push(StaticSampler {
            filter: r.word()?,
            address: [r.word()?, r.word()?, r.word()?],
            mip_lod_bias: f32::from_bits(r.word()?),
            max_anisotropy: r.word()?,
            comparison: r.word()?,
            border: r.word()?,
            min_lod: f32::from_bits(r.word()?),
            max_lod: f32::from_bits(r.word()?),
        });
    }
    let root_cbvs = r.list()?;
    let root_srvs = r.list()?;
    let root_uavs = r.list()?;
    let n = r.word()? as usize;
    let mut root_indexed_views = Vec::with_capacity(n.min(256));
    for _ in 0..n {
        root_indexed_views.push(RootIndexedView {
            table_slot: u8::try_from(r.word()?).ok()?,
            constant_slot: u8::try_from(r.word()?).ok()?,
            constant_dword: r.word()?,
            byte_offset: r.word()?,
            shift: u8::try_from(r.word()?).ok().filter(|&s| s < 32)?,
        });
    }
    let n = r.word()? as usize;
    let mut half_views = Vec::with_capacity(n.min(256));
    for _ in 0..n {
        half_views.push((u8::try_from(r.word()?).ok()?, r.word()?));
    }
    r.0.is_empty().then_some(HostShader {
        stage,
        dxil,
        user_data,
        samplers,
        static_samplers,
        root_cbvs,
        root_srvs,
        root_uavs,
        root_indexed_views,
        half_views,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shader() -> HostShader {
        HostShader {
            stage: HwStage::Ps,
            dxil: b"DXBC and the rest".to_vec(),
            user_data: vec![UserData {
                index: 8,
                slot: 1,
                root: RootType::Table,
                dword: 0,
            }],
            samplers: vec![[0x400, 0xc0_0000, 0x18f0_0000, 0]],
            static_samplers: vec![StaticSampler::from_words([
                0x400,
                0xc0_0000,
                0x18f0_0000,
                0,
            ])],
            root_cbvs: vec![2],
            root_srvs: vec![0, 3],
            root_uavs: vec![1],
            root_indexed_views: vec![RootIndexedView {
                table_slot: 1,
                constant_slot: 2,
                constant_dword: 3,
                byte_offset: 32,
                shift: 5,
            }],
            half_views: vec![(0, 8), (4, 0)],
        }
    }

    #[test]
    fn translations_come_back_from_the_cache() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::open(dir.path()).unwrap();
        let build = [7u8; 32];
        let container = b"a console container".as_slice();
        let runs = AtomicU64::new(0);
        let translate = |_: &[u8]| {
            runs.fetch_add(1, Ordering::Relaxed);
            Ok(shader())
        };
        assert_eq!(cached(&cache, &build, container, translate), Ok(shader()));
        assert_eq!(cached(&cache, &build, container, translate), Ok(shader()));
        assert_eq!(runs.load(Ordering::Relaxed), 1, "the second came from disk");

        // Another build translates again.
        assert_eq!(cached(&cache, &[8; 32], container, translate), Ok(shader()));
        assert_eq!(runs.load(Ordering::Relaxed), 2);

        // A damaged entry is a miss, translated and stored again.
        for e in walk(dir.path()) {
            std::fs::write(e, b"VSH1 broken").unwrap();
        }
        assert_eq!(cached(&cache, &build, container, translate), Ok(shader()));
        assert_eq!(runs.load(Ordering::Relaxed), 3);
        assert_eq!(cached(&cache, &build, container, translate), Ok(shader()));
        assert_eq!(runs.load(Ordering::Relaxed), 3);

        // Failures are not stored.
        let fail = |_: &[u8]| Err::<HostShader, _>("unsupported".to_owned());
        assert!(cached(&cache, &build, b"other", fail).is_err());
    }

    fn walk(dir: &std::path::Path) -> Vec<PathBuf> {
        let mut files = Vec::new();
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            if e.path().is_dir() {
                files.extend(walk(&e.path()));
            } else {
                files.push(e.path());
            }
        }
        files
    }

    #[test]
    fn entries_round_trip_and_reject_damage() {
        let bytes = encode(&shader());
        assert_eq!(decode(&bytes), Some(shader()));
        assert_eq!(decode(&bytes[..bytes.len() - 1]), None);
        assert_eq!(decode(&[bytes.as_slice(), &[0]].concat()), None);
        assert_eq!(decode(b"VSH0"), None);
    }

    #[test]
    fn wave_wide_only_parses_defaults_disable_and_lists() {
        let default = parse_wave_wide_only(None, false);
        assert!(default.contains(&"02384a60fda6f2d0".to_owned()));
        assert!(default.contains(&"6b82bc7d22016cb5".to_owned()));
        assert!(default.contains(&"2da4beb386509b77".to_owned()));
        assert_eq!(default.len(), WAVE_WIDE_DEFAULT.len());

        for off in ["0", "none", "NONE", "", "   "] {
            assert!(parse_wave_wide_only(Some(off), false).is_empty(), "{off:?}");
        }
        assert_eq!(
            parse_wave_wide_only(Some("02384a60fda6f2d0, 6B82BC7D22016CB5,"), false),
            vec!["02384a60fda6f2d0", "6b82bc7d22016cb5"]
        );
    }

    #[test]
    fn water_wide_adds_the_three_water_programs_once() {
        let off = parse_wave_wide_only(None, false);
        for h in WATER_WIDE {
            assert!(!off.contains(&h.to_owned()), "{h} is in the default list");
        }
        let on = parse_wave_wide_only(None, true);
        assert_eq!(on[..WAVE_WIDE_DEFAULT.len()], off[..]);
        assert_eq!(on[WAVE_WIDE_DEFAULT.len()..], WATER_WIDE.map(str::to_owned));
        let named = parse_wave_wide_only(Some("926c46bab3b86bd6"), true);
        assert_eq!(named, vec!["926c46bab3b86bd6", "6871d6e4b3ac9a10", "a10179ab7eda0dc3"]);
        assert_eq!(parse_wave_wide_only(Some("none"), true), WATER_WIDE.map(str::to_owned));
    }

    #[test]
    fn wave_pairs_only_parses_defaults_disable_and_lists() {
        let default = parse_wave_pairs_only(None);
        assert!(default.contains(&"02384a60fda6f2d0".to_owned()));
        assert!(default.contains(&"6b82bc7d22016cb5".to_owned()));
        assert_eq!(default.len(), WAVE_PAIRS_DEFAULT.len());

        for off in ["0", "none", "NONE", "", "   "] {
            assert!(parse_wave_pairs_only(Some(off)).is_empty(), "{off:?}");
        }
        assert_eq!(
            parse_wave_pairs_only(Some("02384a60fda6f2d0, 6B82BC7D22016CB5,")),
            vec!["02384a60fda6f2d0", "6b82bc7d22016cb5"]
        );
    }
}
