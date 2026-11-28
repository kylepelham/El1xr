//! Per-frame camera record for frame generation (`VICTORY_CAMERA_CAPTURE=1`,
//! off by default; it only reads, the picture is unchanged).
//!
//! Where the title keeps its camera (read statically from its velocity
//! shaders, `PS_VelocityFill`/`VS_VelocityFill` and
//! `CS_2GBufFixupAndClearTemp_Velocity`, which rebuild a position from depth
//! and project it into the previous frame):
//! - a per-view record, a structured buffer SRV of stride 288 bytes, element
//!   0: `+0x08`/`+0x0c` the depth linearisation `A`, `B`
//!   (`z = A / (B + 1 - depth)`, reversed depth), `+0x10..+0x34` a 3x3
//!   matrix taking `(ndc.x, ndc.y, 1)` to a camera-relative world ray (its
//!   columns: right x tan(fov_x/2), up x tan(fov_y/2), forward plus the
//!   jitter), `+0x38` the far distance, `+0x44` the viewport size;
//! - the pass constants, CBV `b0`, `+0x1a0`: a float4x4 (row-vector order)
//!   taking a camera-relative world position to the previous frame's clip
//!   space (the previous view-projection with the camera's move folded in).
//!
//! From these, [`derive`] builds view, projection, view-projection, their
//! inverse, clip-to-previous-clip and the previous view; [`check`] tests
//! them against each other and against the upscale dispatch (field of view,
//! near/far, jitter, aspect). The HUD-less colour is the scene before
//! `PS_BlitUISDRToBackbuffer`, which blends the UI (an SDR texture with
//! alpha, its second SRV) onto the back buffer; with the FidelityFX hook,
//! the upscale output is the scene before post-processing and UI.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

/// Row-major 4x4, column vectors: `v' = M v`.
pub type Mat4 = [[f32; 4]; 4];

/// `VICTORY_CAMERA_CAPTURE=1`.
pub fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_CAMERA_CAPTURE").is_some_and(|v| v == "1"))
}

/// Bytes of one per-view record (the SRV's structure stride).
pub const VIEW_STRIDE: usize = 288;
/// Where the previous-clip matrix lies in the pass constants (`b0`).
pub const PREV_CLIP_OFFSET: u64 = 0x1a0;

/// The words of the per-view record the camera is built from.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ViewRecord {
    pub lin: [f32; 2],
    /// `ray[component][coefficient of ndc.x, ndc.y, 1]`.
    pub ray: [[f32; 3]; 3],
    pub far: f32,
    pub viewport: [f32; 2],
}

fn word(bytes: &[u8], at: usize) -> f32 {
    f32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

impl ViewRecord {
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < 0x4c {
            return None;
        }
        let mut ray = [[0.0; 3]; 3];
        for (i, row) in ray.iter_mut().enumerate() {
            for (j, v) in row.iter_mut().enumerate() {
                *v = word(bytes, 0x10 + (i * 3 + j) * 4);
            }
        }
        Some(Self {
            lin: [word(bytes, 0x08), word(bytes, 0x0c)],
            ray,
            far: word(bytes, 0x38),
            viewport: [word(bytes, 0x44), word(bytes, 0x48)],
        })
    }
}

/// The title's row-vector float4x4 (64 bytes) as a column-vector matrix.
pub fn matrix_from_title(bytes: &[u8]) -> Option<Mat4> {
    if bytes.len() < 64 {
        return None;
    }
    let mut m = [[0.0; 4]; 4];
    for (r, row) in m.iter_mut().enumerate() {
        for (c, v) in row.iter_mut().enumerate() {
            *v = word(bytes, (c * 4 + r) * 4);
        }
    }
    Some(m)
}

/// What the upscale dispatch (`ffxDispatch`) gave this frame.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Dispatch {
    /// Title resources: colour in (render size), depth, motion vectors,
    /// output (the HUD-less, pre-post upscaled scene).
    pub colour: u64,
    pub depth: u64,
    pub motion: u64,
    pub output: u64,
    /// DXGI formats of colour, depth, motion, output (0: unknown).
    pub formats: [i32; 4],
    pub jitter: [f32; 2],
    pub motion_scale: [f32; 2],
    pub render: [u32; 2],
    pub upscale: [u32; 2],
    pub near: f32,
    pub far: f32,
    pub fov: f32,
    pub frame_ms: f32,
    pub reset: bool,
}

/// The UI composite: the UI texture (SDR, alpha = coverage) and the target
/// it is blended onto (holding the HUD-less scene just before).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Ui {
    pub texture: u64,
    pub target: u64,
}

/// A texture of the frame: the title's (or, for the HUD-less copy, ours),
/// its host DXGI format and its size.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Surface {
    pub title: u64,
    pub format: i32,
    pub size: [u32; 2],
}

/// The camera as built from the title's constants.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Derived {
    /// Camera-relative world to view (x right, y up, z forward).
    pub view: Mat4,
    pub proj: Mat4,
    pub view_proj: Mat4,
    pub clip_to_view_proj_inv: Mat4,
    pub prev_view_proj: Mat4,
    pub clip_to_prev_clip: Mat4,
    pub prev_clip_to_clip: Mat4,
    /// The previous view (rotation and translation, current camera origin).
    pub prev_view: Mat4,
    pub right: [f32; 3],
    pub up: [f32; 3],
    pub forward: [f32; 3],
    /// The camera's move since the previous frame (world units).
    pub moved: [f32; 3],
    pub tan_half: [f32; 2],
    pub fov_y: f32,
    pub aspect: f32,
    pub near: f32,
    pub far: f32,
    /// Projection offset in NDC (what jitter adds to `ndc`).
    pub jitter_ndc: [f32; 2],
    /// Forward part of the ray at ndc 0 (1: the depth is view z).
    pub depth_scale: f32,
}

/// One frame's camera for frame generation (`latest`).
#[derive(Clone, Debug, Default)]
pub struct FrameCamera {
    pub frame: u64,
    /// Every self-check passed this frame.
    pub valid: bool,
    pub why: String,
    pub camera: Option<Derived>,
    pub record: Option<ViewRecord>,
    pub dispatch: Option<Dispatch>,
    pub ui: Option<Ui>,
    /// The HUD-less colour, copied before the UI blit
    /// (`VICTORY_CAMERA_HUDLESS=1`; `title` is 0: the texture is ours, from
    /// `crate::camera_copy::hudless`).
    pub hudless: Option<Surface>,
    /// The UI texture the blit composes (alpha: coverage), the UI mask.
    pub ui_mask: Option<Surface>,
    /// Depth (reversed: near 1, far 0) and motion vectors (pixels, previous
    /// minus current, render size) as the velocity pass reads and writes them.
    pub depth: Option<Surface>,
    pub motion: Option<Surface>,
    /// Multiplies the motion vectors into UV units (1 / render size).
    pub mv_scale: [f32; 2],
    /// The projection jitter in render pixels (x right, y down).
    pub jitter_px: [f32; 2],
    /// Title addresses the constants were read at (view record, `b0`).
    pub sources: [u64; 2],
    /// Absolute camera position: not found yet (matrices are camera-relative).
    pub position_known: bool,
}

fn add(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}
#[cfg(test)]
fn sub(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}
fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}
fn scale(a: [f32; 3], s: f32) -> [f32; 3] {
    [a[0] * s, a[1] * s, a[2] * s]
}
fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}
fn length(a: [f32; 3]) -> f32 {
    dot(a, a).sqrt()
}

pub fn mul(a: &Mat4, b: &Mat4) -> Mat4 {
    let mut m = [[0.0; 4]; 4];
    for (r, row) in m.iter_mut().enumerate() {
        for (c, v) in row.iter_mut().enumerate() {
            *v = (0..4).map(|k| a[r][k] * b[k][c]).sum();
        }
    }
    m
}

pub fn transform(m: &Mat4, v: [f32; 4]) -> [f32; 4] {
    let mut out = [0.0; 4];
    for (r, o) in out.iter_mut().enumerate() {
        *o = (0..4).map(|k| m[r][k] * v[k]).sum();
    }
    out
}

/// Gauss-Jordan with partial pivoting; `None` if singular.
pub fn inverse(m: &Mat4) -> Option<Mat4> {
    let mut a: [[f64; 8]; 4] = [[0.0; 8]; 4];
    for r in 0..4 {
        for c in 0..4 {
            a[r][c] = f64::from(m[r][c]);
        }
        a[r][4 + r] = 1.0;
    }
    for col in 0..4 {
        let pivot = (col..4).max_by(|&x, &y| a[x][col].abs().total_cmp(&a[y][col].abs()))?;
        if a[pivot][col].abs() < 1e-12 {
            return None;
        }
        a.swap(col, pivot);
        let p = a[col][col];
        for v in &mut a[col] {
            *v /= p;
        }
        for r in 0..4 {
            if r != col {
                let f = a[r][col];
                let pivot_row = a[col];
                for (v, pv) in a[r].iter_mut().zip(pivot_row) {
                    *v -= f * pv;
                }
            }
        }
    }
    let mut out = [[0.0; 4]; 4];
    for r in 0..4 {
        for c in 0..4 {
            out[r][c] = a[r][4 + c] as f32;
        }
    }
    Some(out)
}

/// Builds the camera from the per-view record and the previous-clip matrix.
pub fn derive(v: &ViewRecord, prev_clip: &Mat4) -> Option<Derived> {
    let col = |j: usize| [v.ray[0][j], v.ray[1][j], v.ray[2][j]];
    let (c0, c1, c2) = (col(0), col(1), col(2));
    let (tx, ty) = (length(c0), length(c1));
    if !(tx > 1e-6 && ty > 1e-6) {
        return None;
    }
    let right = scale(c0, 1.0 / tx);
    let up = scale(c1, 1.0 / ty);
    let mut forward = cross(right, up);
    let n = length(forward);
    if n < 1e-6 {
        return None;
    }
    forward = scale(forward, 1.0 / n);
    if dot(forward, c2) < 0.0 {
        forward = scale(forward, -1.0);
    }
    let k = dot(c2, forward);
    let (a, b) = (dot(c2, right), dot(c2, up));
    let [lin_a, lin_b] = v.lin;
    let view = [
        [right[0], right[1], right[2], 0.0],
        [up[0], up[1], up[2], 0.0],
        [forward[0], forward[1], forward[2], 0.0],
        [0.0, 0.0, 0.0, 1.0],
    ];
    // ndc = (k x / z - a) / tx; depth = 1 + B - A k / z.
    let proj = [
        [k / tx, 0.0, -a / tx, 0.0],
        [0.0, k / ty, -b / ty, 0.0],
        [0.0, 0.0, 1.0 + lin_b, -lin_a * k],
        [0.0, 0.0, 1.0, 0.0],
    ];
    let view_proj = mul(&proj, &view);
    let inv = inverse(&view_proj)?;
    let clip_to_prev_clip = mul(prev_clip, &inv);
    let prev_clip_to_clip = inverse(&clip_to_prev_clip).unwrap_or_default();
    let prev_view = mul(&inverse(&proj)?, prev_clip);
    let rows = |m: &Mat4, r: usize| [m[r][0], m[r][1], m[r][2]];
    // t = -R (prev_pos - pos) with pos the origin: the move (pos - prev_pos) is R^T t.
    let t = [prev_view[0][3], prev_view[1][3], prev_view[2][3]];
    let mut moved = [0.0; 3];
    for (r, tr) in t.iter().enumerate() {
        moved = add(moved, scale(rows(&prev_view, r), *tr));
    }
    let near = if lin_b.abs() > 1e-12 {
        k * lin_a / lin_b
    } else {
        f32::INFINITY
    };
    let far_d = 1.0 + lin_b;
    let far = if far_d.abs() > 1e-9 && k * lin_a / far_d > 0.0 {
        k * lin_a / far_d
    } else {
        f32::INFINITY
    };
    Some(Derived {
        view,
        proj,
        view_proj,
        clip_to_view_proj_inv: inv,
        prev_view_proj: *prev_clip,
        clip_to_prev_clip,
        prev_clip_to_clip,
        prev_view,
        right,
        up,
        forward,
        moved,
        tan_half: [tx / k, ty / k],
        fov_y: 2.0 * (ty / k).atan(),
        aspect: tx / ty,
        near,
        far,
        jitter_ndc: [-a / tx, -b / ty],
        depth_scale: k,
    })
}

/// The upper 3x3 rows of `m` are unit length and mutually orthogonal.
pub fn check_orthonormal(m: &Mat4, tolerance: f32) -> Result<(), String> {
    let r = |i: usize| [m[i][0], m[i][1], m[i][2]];
    for i in 0..3 {
        let l = length(r(i));
        if (l - 1.0).abs() > tolerance {
            return Err(format!("row {i} length {l:.4}"));
        }
        for j in i + 1..3 {
            let d = dot(r(i), r(j));
            if d.abs() > tolerance {
                return Err(format!("rows {i},{j} dot {d:.4}"));
            }
        }
    }
    Ok(())
}

/// `vp == p * v` within `tolerance` relative to the largest element.
pub fn check_product(vp: &Mat4, p: &Mat4, v: &Mat4, tolerance: f32) -> Result<(), String> {
    let want = mul(p, v);
    let big = want.iter().flatten().fold(1e-6_f32, |m, x| m.max(x.abs()));
    let (mut worst, mut at) = (0.0_f32, (0, 0));
    for r in 0..4 {
        for c in 0..4 {
            let e = (vp[r][c] - want[r][c]).abs() / big;
            if e > worst {
                worst = e;
                at = (r, c);
            }
        }
    }
    if worst > tolerance {
        return Err(format!("vp != p*v at [{}][{}] by {worst:.4}", at.0, at.1));
    }
    Ok(())
}

fn near_rel(a: f32, b: f32, rel: f32) -> bool {
    const HUGE: f32 = 1e6;
    if a.abs() >= HUGE || b.abs() >= HUGE || !a.is_finite() || !b.is_finite() {
        return (a.abs() >= HUGE || !a.is_finite()) && (b.abs() >= HUGE || !b.is_finite());
    }
    (a - b).abs() <= rel * a.abs().max(b.abs()).max(1e-6)
}

/// The projection agrees with the dispatch's field of view, near/far,
/// render aspect and jitter (pixels, either sign).
pub fn check_projection(c: &Derived, d: &Dispatch) -> Result<(), String> {
    if d.fov > 0.0 && (c.fov_y - d.fov).abs() > 0.01 {
        return Err(format!("fov {:.4} vs dispatch {:.4}", c.fov_y, d.fov));
    }
    if d.render[0] > 0 && d.render[1] > 0 {
        let want = d.render[0] as f32 / d.render[1] as f32;
        if !near_rel(c.aspect, want, 0.01) {
            return Err(format!("aspect {:.4} vs render {want:.4}", c.aspect));
        }
        let px = [
            c.jitter_ndc[0] * d.render[0] as f32 * 0.5,
            c.jitter_ndc[1] * d.render[1] as f32 * 0.5,
        ];
        let same = |s: f32| {
            (px[0] - s * d.jitter[0]).abs() < 0.05 && (px[1] + s * d.jitter[1]).abs() < 0.05
        };
        let flipped = |s: f32| {
            (px[0] - s * d.jitter[0]).abs() < 0.05 && (px[1] - s * d.jitter[1]).abs() < 0.05
        };
        if !(same(1.0) || same(-1.0) || flipped(1.0) || flipped(-1.0)) {
            return Err(format!(
                "jitter {:.3},{:.3} px vs dispatch {:.3},{:.3}",
                px[0], px[1], d.jitter[0], d.jitter[1]
            ));
        }
    }
    if d.near != 0.0 || d.far != 0.0 {
        let (lo, hi) = (c.near.min(c.far), c.near.max(c.far));
        let (dlo, dhi) = (d.near.min(d.far), d.near.max(d.far));
        if !near_rel(lo, dlo, 0.01) || !near_rel(hi, dhi, 0.01) {
            return Err(format!(
                "near/far {:.4}/{:.1} vs dispatch {:.4}/{:.1}",
                c.near, c.far, d.near, d.far
            ));
        }
    }
    Ok(())
}

/// One pixel's motion: render-space pixel, its depth, and its motion
/// vector as the title wrote it (scaled by the dispatch's `motion_scale`
/// to pixels: previous minus current).
#[derive(Clone, Copy, Debug)]
pub struct MotionSample {
    pub pixel: [f32; 2],
    pub depth: f32,
    pub motion: [f32; 2],
}

/// The share of samples whose motion `clip_to_prev_clip` predicts within
/// `tolerance` pixels (moving objects never match; the static world does).
pub fn motion_agreement(
    clip_to_prev_clip: &Mat4,
    samples: &[MotionSample],
    render: [u32; 2],
    motion_scale: [f32; 2],
    tolerance: f32,
) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let (w, h) = (render[0] as f32, render[1] as f32);
    let hits = samples
        .iter()
        .filter(|s| {
            let ndc = [s.pixel[0] / w * 2.0 - 1.0, 1.0 - s.pixel[1] / h * 2.0];
            let p = transform(clip_to_prev_clip, [ndc[0], ndc[1], s.depth, 1.0]);
            if p[3].abs() < 1e-9 {
                return false;
            }
            let prev = [(p[0] / p[3] * 0.5 + 0.5) * w, (0.5 - p[1] / p[3] * 0.5) * h];
            let want = [prev[0] - s.pixel[0], prev[1] - s.pixel[1]];
            let got = [s.motion[0] * motion_scale[0], s.motion[1] * motion_scale[1]];
            (want[0] - got[0]).abs() < tolerance && (want[1] - got[1]).abs() < tolerance
        })
        .count();
    hits as f32 / samples.len() as f32
}

/// Every self-check of one frame's camera.
pub fn check(c: &Derived, dispatch: Option<&Dispatch>) -> Result<(), String> {
    check_orthonormal(&c.view, 1e-3).map_err(|e| format!("view {e}"))?;
    check_product(&c.view_proj, &c.proj, &c.view, 1e-4)?;
    if (c.depth_scale - 1.0).abs() > 0.05 {
        return Err(format!(
            "ray forward part {:.4} (not view z)",
            c.depth_scale
        ));
    }
    // The previous clip is a projection times a rigid view.
    check_orthonormal(&c.prev_view, 2e-2).map_err(|e| format!("previous view {e}"))?;
    let mut rigid = c.prev_view;
    rigid[3] = [0.0, 0.0, 0.0, 1.0];
    check_product(&c.prev_view_proj, &c.proj, &rigid, 2e-2).map_err(|e| format!("previous {e}"))?;
    if let Some(d) = dispatch {
        check_projection(c, d)?;
    }
    Ok(())
}

#[derive(Default)]
struct Pending {
    view: Option<(u64, u64)>,
    constants: Option<(u64, u64)>,
    dispatch: Option<Dispatch>,
    ui: Option<Ui>,
    ui_mask: Option<Surface>,
    hudless: Option<Surface>,
    depth: Option<Surface>,
    motion: Option<Surface>,
}

static PENDING: Mutex<Pending> = Mutex::new(Pending {
    view: None,
    constants: None,
    dispatch: None,
    ui: None,
    ui_mask: None,
    hudless: None,
    depth: None,
    motion: None,
});
static LATEST: Mutex<Option<Arc<FrameCamera>>> = Mutex::new(None);
static FRAME: AtomicU64 = AtomicU64::new(0);
static TOLD_NONE: AtomicBool = AtomicBool::new(false);

fn pending() -> std::sync::MutexGuard<'static, Pending> {
    PENDING.lock().unwrap_or_else(|e| e.into_inner())
}

/// A velocity pass bound its view record (`address`, `len`) and pass
/// constants (`b0`).
pub fn note_view(view: (u64, u64), constants: (u64, u64)) {
    let mut p = pending();
    p.view = Some(view);
    p.constants = Some(constants);
}

pub fn note_dispatch(d: Dispatch) {
    pending().dispatch = Some(d);
}

pub fn note_ui(ui: Ui) {
    pending().ui = Some(ui);
}

/// The UI texture as a surface (format and size known at the blit).
pub fn note_ui_mask(s: Surface) {
    pending().ui_mask = Some(s);
}

/// What the velocity pass reads as depth and writes as motion vectors.
pub fn note_velocity_targets(depth: Option<Surface>, motion: Option<Surface>) {
    let mut p = pending();
    if depth.is_some() {
        p.depth = depth;
    }
    if motion.is_some() {
        p.motion = motion;
    }
}

/// The HUD-less copy was recorded this frame.
pub fn note_hudless(s: Surface) {
    pending().hudless = Some(s);
}

/// Whether a HUD-less copy is already noted for the frame being built.
pub fn hudless_noted() -> bool {
    pending().hudless.is_some()
}

/// The index of the frame being built (the next `FrameCamera::frame`).
pub fn frame() -> u64 {
    FRAME.load(Ordering::Relaxed)
}

/// Motion vectors in pixels to UV units, and the jitter in pixels (y down),
/// for a render target of `size`.
pub fn frame_scales(c: Option<&Derived>, size: [u32; 2]) -> ([f32; 2], [f32; 2]) {
    if size[0] == 0 || size[1] == 0 {
        return ([0.0; 2], [0.0; 2]);
    }
    let (w, h) = (size[0] as f32, size[1] as f32);
    let jitter = c.map_or([0.0; 2], |c| {
        [c.jitter_ndc[0] * w * 0.5, -c.jitter_ndc[1] * h * 0.5]
    });
    ([1.0 / w, 1.0 / h], jitter)
}

/// The last finished frame's camera, for frame generation.
pub fn latest() -> Option<Arc<FrameCamera>> {
    LATEST.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Builds the frame's record from what was noted, reading title memory with
/// `read` (address, buffer: false if unreadable).
pub fn finish(read: impl Fn(u64, &mut [u8]) -> bool) -> FrameCamera {
    let p = std::mem::take(&mut *pending());
    let frame = FRAME.fetch_add(1, Ordering::Relaxed);
    let mut out = FrameCamera {
        frame,
        dispatch: p.dispatch,
        ui: p.ui,
        ui_mask: p.ui_mask,
        hudless: p.hudless,
        depth: p.depth,
        motion: p.motion,
        ..FrameCamera::default()
    };
    let (Some((view_at, view_len)), Some((cb_at, cb_len))) = (p.view, p.constants) else {
        out.why = "no velocity pass this frame".into();
        return out;
    };
    out.sources = [view_at, cb_at];
    let mut record = [0u8; VIEW_STRIDE];
    let mut matrix = [0u8; 64];
    if view_len < 0x4c || cb_len < PREV_CLIP_OFFSET + 64 {
        out.why = format!("short views: record {view_len} B, b0 {cb_len} B");
        return out;
    }
    let len = (view_len as usize).min(VIEW_STRIDE);
    if !read(view_at, &mut record[..len]) || !read(cb_at + PREV_CLIP_OFFSET, &mut matrix) {
        out.why = "constants unreadable".into();
        return out;
    }
    out.record = ViewRecord::parse(&record[..len]);
    let prev = matrix_from_title(&matrix);
    out.camera = out
        .record
        .as_ref()
        .zip(prev.as_ref())
        .and_then(|(r, m)| derive(r, m));
    match &out.camera {
        None => out.why = "degenerate view record".into(),
        Some(c) => match check(c, out.dispatch.as_ref()) {
            Ok(()) => out.valid = true,
            Err(e) => out.why = e,
        },
    }
    let size = out
        .motion
        .map(|m| m.size)
        .or(out.record.map(|r| [r.viewport[0] as u32, r.viewport[1] as u32]))
        .unwrap_or_default();
    (out.mv_scale, out.jitter_px) = frame_scales(out.camera.as_ref(), size);
    out
}

/// End of a frame (present): finish, publish, and log now and then.
pub fn frame_end(read: impl Fn(u64, &mut [u8]) -> bool) {
    static LAST_LOG: Mutex<Option<std::time::Instant>> = Mutex::new(None);
    let cam = finish(read);
    #[cfg(windows)]
    crate::camera_mv::frame_end(&cam);
    if crate::camera_diag::on() {
        crate::camera_diag::frame_end(cam.frame, &found(&cam));
    }
    let due = {
        let mut last = LAST_LOG.lock().unwrap_or_else(|e| e.into_inner());
        let due = last.is_none_or(|t| t.elapsed().as_secs_f32() >= 3.0);
        if due {
            *last = Some(std::time::Instant::now());
        }
        due
    };
    if due {
        log(&cam);
    }
    *LATEST.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(cam));
}

/// What the frame's record holds, for the diag's frame line.
fn found(cam: &FrameCamera) -> String {
    format!(
        "velocity={} why=\"{}\" ui={:?} hudless={:?} depth={:?} motion={:?}",
        cam.sources[0] != 0,
        cam.why,
        cam.ui.map(|u| (u.texture, u.target)),
        cam.hudless.map(|s| (s.format, s.size)),
        cam.depth.map(|s| (s.title, s.format, s.size)),
        cam.motion.map(|s| (s.title, s.format, s.size)),
    )
}

fn log(cam: &FrameCamera) {
    let verdict = if cam.valid {
        "ok".to_owned()
    } else {
        format!("mismatch {}", cam.why)
    };
    let Some(c) = &cam.camera else {
        if !TOLD_NONE.swap(true, Ordering::Relaxed) || cam.sources[0] != 0 {
            tracing::info!(frame = cam.frame, "camera: none {}", cam.why);
        }
        return;
    };
    let d = cam.dispatch.unwrap_or_default();
    #[cfg(windows)]
    let held = {
        let r = crate::camera_copy::resources();
        format!(
            "frame {} depth {} motion {} ui {} hudless {:?}",
            r.frame,
            r.depth.is_some(),
            r.motion.is_some(),
            r.ui_mask.is_some(),
            r.hudless.map(|h| (h.frame, h.width, h.height))
        )
    };
    #[cfg(not(windows))]
    let held = String::new();
    tracing::info!(
        held,
        frame = cam.frame,
        fov_y = c.fov_y,
        aspect = c.aspect,
        near = c.near,
        far = c.far,
        jitter_ndc = ?c.jitter_ndc,
        depth_scale = c.depth_scale,
        moved = ?c.moved,
        forward = ?c.forward,
        dispatch_fov = d.fov,
        dispatch_near = d.near,
        dispatch_far = d.far,
        dispatch_jitter = ?d.jitter,
        render = ?d.render,
        viewport = ?cam.record.map(|r| r.viewport),
        ui = ?cam.ui.map(|u| (u.texture, u.target)),
        hudless_output = d.output,
        hudless = ?cam.hudless.map(|s| (s.format, s.size)),
        ui_mask = ?cam.ui_mask.map(|s| (s.title, s.format, s.size)),
        depth = ?cam.depth.map(|s| (s.title, s.format, s.size)),
        motion = ?cam.motion.map(|s| (s.title, s.format, s.size)),
        mv_scale = ?cam.mv_scale,
        jitter_px = ?cam.jitter_px,
        "camera: found view=srv(stride {VIEW_STRIDE})@{:#x}+0x08..0x4c prev=b0@{:#x}+{PREV_CLIP_OFFSET:#x} {verdict}",
        cam.sources[0],
        cam.sources[1],
    );
}

/// A title texture's DXGI format (0: not a texture with a host copy).
#[cfg(windows)]
pub fn format_of(title: u64) -> i32 {
    let t: crate::object::This = title as crate::object::This;
    if t.is_null() || !crate::resource::is_texture(t) {
        return 0;
    }
    crate::resource::host_resource(t).map_or(0, |r| {
        // SAFETY: a live host resource.
        unsafe { r.GetDesc() }.Format.0
    })
}

#[cfg(test)]
#[path = "camera_tests.rs"]
mod tests;
