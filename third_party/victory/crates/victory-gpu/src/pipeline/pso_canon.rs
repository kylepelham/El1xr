//! (bootfast) One key per pipeline (`VICTORY_PSO_CANON=1`; unset: off, as
//! before).
//!
//! Ported by hand from zortos293/XSEmu `crates/xcl/src/hle/d3d12x/pso_canon.rs`
//! (d576ad06 sync, GPL-3.0). A title leaves the state the GPU ignores as its
//! memory held it: the blend factors and operations of a target that does
//! not blend, the logic operation of one with logic operations off, the
//! targets past the ones D3D12 reads (all but the first without independent
//! blending, those past the target count with it), the stencil masks and
//! faces with stencil off. A pipeline's key (`make::Pending`), its name in
//! the library (`library::name`) and its recipe all take the whole state
//! (`GraphicsState::describe`), so one pipeline the title made with other
//! leftovers came out under another key: compiled again instead of loaded
//! from the library, and not found on the early list of the run before.
//!
//! With the switch those fields are set to D3D12's defaults before the
//! pipeline is keyed, and the host pipeline is made from that state (the
//! same pipeline: D3D12 reads none of them). A state that already holds the
//! defaults keeps its key, so only the pipelines with leftovers are keyed
//! anew. The depth function and write mask stay as they are with depth off,
//! as XSEmu keeps them.
//!
//! The counter (`log`, the line `pipeline keys` the operator greps) counts,
//! in one run, the graphics pipelines, those with leftovers, and those whose
//! state equals an earlier one's once canonical but whose key did not (the
//! same pipeline under another key), with the switch or with
//! `VICTORY_PSO_DUPES=1`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Mutex, OnceLock};

use windows::Win32::Graphics::Direct3D12::*;

use super::host::GraphicsState;

/// Whether ignored state is set to D3D12's defaults before keying
/// (`VICTORY_PSO_CANON=1`).
pub(super) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        let on = std::env::var_os("VICTORY_PSO_CANON").is_some_and(|v| v == "1");
        if on {
            tracing::info!(
                "ignored pipeline state is set to D3D12's defaults before keying (VICTORY_PSO_CANON; XSEmu pso_canon)"
            );
        }
        on
    })
}

/// Whether duplicate keys are counted: with the switch, or
/// `VICTORY_PSO_DUPES=1` without it.
fn counting() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| on() || std::env::var_os("VICTORY_PSO_DUPES").is_some_and(|v| v == "1"))
}

fn default_target() -> D3D12_RENDER_TARGET_BLEND_DESC {
    GraphicsState::default().blend.RenderTarget[0]
}

fn default_face() -> D3D12_DEPTH_STENCILOP_DESC {
    GraphicsState::default().depth.FrontFace
}

/// `g` with the state D3D12 does not read set to its defaults.
pub(super) fn canonical(g: &mut GraphicsState) {
    let plain = default_target();
    let used = if g.blend.IndependentBlendEnable.as_bool() {
        (g.targets.NumRenderTargets as usize).min(8)
    } else {
        1
    };
    for (i, t) in g.blend.RenderTarget.iter_mut().enumerate() {
        if i >= used {
            *t = plain;
            continue;
        }
        if !t.BlendEnable.as_bool() {
            t.SrcBlend = plain.SrcBlend;
            t.DestBlend = plain.DestBlend;
            t.BlendOp = plain.BlendOp;
            t.SrcBlendAlpha = plain.SrcBlendAlpha;
            t.DestBlendAlpha = plain.DestBlendAlpha;
            t.BlendOpAlpha = plain.BlendOpAlpha;
        }
        if !t.LogicOpEnable.as_bool() {
            t.LogicOp = plain.LogicOp;
        }
    }
    if !g.depth.StencilEnable.as_bool() {
        g.depth.StencilReadMask = 0xff;
        g.depth.StencilWriteMask = 0xff;
        g.depth.FrontFace = default_face();
        g.depth.BackFace = default_face();
    }
}

/// The state part of a pipeline's key: `g`'s description, of its canonical
/// form with the switch (`g` is left in that form, which the host pipeline
/// is made from). `shaders`: the key's hasher with the stages taken in, for
/// the counter.
pub(super) fn state(g: &mut GraphicsState, shaders: &blake3::Hasher) -> String {
    if !counting() {
        return g.describe();
    }
    let raw = g.describe();
    let kept = (g.blend, g.depth);
    canonical(g);
    let canon = g.describe();
    let mut id = shaders.clone();
    id.update(canon.as_bytes());
    note(
        short(&id.finalize()),
        short(&blake3::hash(raw.as_bytes())),
        raw != canon,
    );
    if on() {
        canon
    } else {
        (g.blend, g.depth) = kept;
        raw
    }
}

fn short(h: &blake3::Hash) -> u128 {
    u128::from_le_bytes(h.as_bytes()[..16].try_into().unwrap())
}

/// Counters, by index: graphics pipelines keyed, those with state off the
/// defaults D3D12 ignores, those whose canonical state an earlier one had
/// under another key.
static COUNTS: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];
const GRAPHICS: usize = 0;
const LEFTOVERS: usize = 1;
const DUPLICATES: usize = 2;

/// The first full state (its hash) under each canonical key.
static SEEN: Mutex<Option<HashMap<u128, u128>>> = Mutex::new(None);

fn note(canonical: u128, raw: u128, leftovers: bool) {
    COUNTS[GRAPHICS].fetch_add(1, Relaxed);
    if leftovers {
        COUNTS[LEFTOVERS].fetch_add(1, Relaxed);
    }
    let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    let first = *seen
        .get_or_insert_with(HashMap::new)
        .entry(canonical)
        .or_insert(raw);
    if first != raw {
        COUNTS[DUPLICATES].fetch_add(1, Relaxed);
    }
}

/// The line the operator greps: `pipeline keys`.
pub(super) fn log() {
    if !counting() {
        return;
    }
    let c = |i: usize| COUNTS[i].load(Relaxed);
    tracing::info!(
        canonical = on(),
        graphics = c(GRAPHICS),
        with_leftovers = c(LEFTOVERS),
        same_pipeline_other_key = c(DUPLICATES),
        "pipeline keys: ignored state and duplicate keys (VICTORY_PSO_CANON / VICTORY_PSO_DUPES)"
    );
}

/// Of the graphics recipes in `book` (their states as encoded): how many,
/// how many hold leftovers, and how many are the same pipeline as an
/// earlier one under another key (the recipes file only grows, so this
/// counts across the runs that wrote it).
pub(super) fn recipe_duplicates<'a>(
    recipes: impl Iterator<Item = (&'a [(u8, [u8; 32])], &'a [u8])>,
) -> [usize; 4] {
    let mut seen: HashMap<u128, u128> = HashMap::new();
    let mut out = [0; 4];
    for (stages, bytes) in recipes {
        let Some(mut g) = GraphicsState::decode(bytes) else {
            out[3] += 1;
            continue;
        };
        out[0] += 1;
        let raw = g.describe();
        canonical(&mut g);
        let canon = g.describe();
        if raw != canon {
            out[1] += 1;
        }
        let mut id = blake3::Hasher::new();
        for (stage, h) in stages {
            id.update(&[*stage]);
            id.update(h);
        }
        id.update(canon.as_bytes());
        let raw = short(&blake3::hash(raw.as_bytes()));
        if *seen.entry(short(&id.finalize())).or_insert(raw) != raw {
            out[2] += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R8G8B8A8_UNORM;

    /// A title's state: one target blending, depth on, stencil off;
    /// `fill` in what D3D12 ignores.
    fn title(fill: i32) -> GraphicsState {
        let mut g = GraphicsState::default();
        g.targets.NumRenderTargets = 1;
        g.targets.RTFormats[0] = DXGI_FORMAT_R8G8B8A8_UNORM;
        g.blend.IndependentBlendEnable = true.into();
        let t = &mut g.blend.RenderTarget[0];
        t.BlendEnable = true.into();
        t.SrcBlend = D3D12_BLEND_SRC_ALPHA;
        t.DestBlend = D3D12_BLEND_INV_SRC_ALPHA;
        t.LogicOp = D3D12_LOGIC_OP(fill);
        for t in &mut g.blend.RenderTarget[1..] {
            t.BlendEnable = (fill & 1 != 0).into();
            t.SrcBlend = D3D12_BLEND(fill);
            t.RenderTargetWriteMask = fill as u8;
        }
        g.depth.StencilEnable = false.into();
        g.depth.StencilReadMask = fill as u8;
        g.depth.StencilWriteMask = (fill >> 1) as u8;
        g.depth.FrontFace.StencilFunc = D3D12_COMPARISON_FUNC(fill);
        g.depth.BackFace.StencilPassOp = D3D12_STENCIL_OP(fill);
        g
    }

    fn key(mut g: GraphicsState) -> String {
        canonical(&mut g);
        g.describe()
    }

    #[test]
    fn ignored_state_gives_one_key() {
        let (a, b) = (title(3), title(6));
        assert_ne!(
            a.describe(),
            b.describe(),
            "the leftovers alone make two keys"
        );
        assert_eq!(key(a), key(b));
    }

    #[test]
    fn the_canonical_form_is_kept_by_canonical() {
        let mut g = title(5);
        canonical(&mut g);
        let once = g.describe();
        canonical(&mut g);
        assert_eq!(g.describe(), once);
    }

    #[test]
    fn a_state_holding_the_defaults_keeps_its_key() {
        let plain = GraphicsState::default();
        assert_eq!(key(GraphicsState::default()), plain.describe());
    }

    #[test]
    fn state_d3d12_reads_keeps_its_own_key() {
        let base = key(title(3));
        let changes: [fn(&mut GraphicsState); 9] = [
            |g| g.blend.RenderTarget[0].DestBlend = D3D12_BLEND_ONE,
            |g| g.blend.RenderTarget[0].RenderTargetWriteMask = 7,
            |g| g.blend.RenderTarget[0].BlendEnable = false.into(),
            |g| {
                g.targets.NumRenderTargets = 2;
                g.targets.RTFormats[1] = DXGI_FORMAT_R8G8B8A8_UNORM;
            },
            |g| g.blend.IndependentBlendEnable = false.into(),
            |g| g.depth.DepthFunc = D3D12_COMPARISON_FUNC_GREATER,
            |g| g.depth.StencilEnable = true.into(),
            |g| g.rasterizer.CullMode = D3D12_CULL_MODE_NONE,
            |g| g.blend.AlphaToCoverageEnable = true.into(),
        ];
        for (i, change) in changes.iter().enumerate() {
            let mut g = title(3);
            change(&mut g);
            assert_ne!(key(g), base, "change {i}");
        }
    }

    #[test]
    fn with_stencil_on_its_faces_and_masks_are_kept() {
        let (mut a, mut b) = (title(3), title(6));
        a.depth.StencilEnable = true.into();
        b.depth.StencilEnable = true.into();
        assert_ne!(key(a), key(b));
    }

    #[test]
    fn with_independent_blending_off_only_the_first_target_counts() {
        let (mut a, mut b) = (title(3), title(6));
        a.blend.IndependentBlendEnable = false.into();
        b.blend.IndependentBlendEnable = false.into();
        a.targets.NumRenderTargets = 4;
        b.targets.NumRenderTargets = 4;
        assert_eq!(key(a), key(b));
    }

    #[test]
    fn with_logic_ops_on_the_logic_op_counts() {
        let (mut a, mut b) = (title(3), title(6));
        a.blend.RenderTarget[0].LogicOpEnable = true.into();
        b.blend.RenderTarget[0].LogicOpEnable = true.into();
        assert_ne!(key(a), key(b));
    }

    #[test]
    fn recipes_count_the_same_pipeline_under_another_key() {
        let stages = [(0u8, [1u8; 32]), (1, [2; 32])];
        let other = [(0u8, [1u8; 32]), (1, [3; 32])];
        let (a, b, c) = (title(3).encode(), title(6).encode(), title(6).encode());
        let plain = GraphicsState::default().encode();
        let recipes = [
            (&stages[..], &a[..]),
            (&stages[..], &b[..]),
            (&stages[..], &c[..]),
            (&other[..], &b[..]),
            (&stages[..], &plain[..]),
            (&stages[..], &[1u8, 2][..]),
        ];
        let [graphics, leftovers, duplicates, unreadable] = recipe_duplicates(recipes.into_iter());
        assert_eq!((graphics, leftovers, duplicates, unreadable), (5, 4, 2, 1));
    }
}
