//! A stand-in for the indirect light the title traces rays for.
//!
//! With the ray-tracing structure builder off (`crate::pipeline::builder_runs`)
//! the title's rays find nothing. Its program that resolves the diffuse light
//! of its screen probes (`CS_ScreenProbes_TemporalResolve_*`) then leaves the
//! sky's light outdoors, nothing indoors (walls are black where no lamp or
//! sun reaches) and, in blocks of the picture's tiles, what the probes'
//! memory held. Right after that program the light is raised to an estimate
//! and bounded by a multiple of it.
//!
//! That a stand-in can be put there, and in which of the program's images,
//! is beloooo's find (VIctory-Belobug-AMD, 06031237, 4f45ab88, cd9f78ce,
//! GPL-3.0-or-later), which puts a constant there, shaded in creases from the
//! scene's depth. The estimate here is worked out from the frame instead, so
//! it follows the hour, the room and its lamps by itself: the light the
//! visible surfaces send out (the title keeps it for its own screen traces:
//! `CS_CacheScreenSpaceRadiance`, the frame before) is averaged in 64x36
//! cells, and every point gets the mean of the cells weighted by the share of
//! its surroundings they cover, as a directional value (first-order harmonics
//! at 160x90 points, taken between them by depth).
//!
//! What it assumes: what is not on screen sends out what is (with a slow
//! mean of the last seconds, settled by wall-clock time so turning into a
//! dark corner is not black at once and walking into a room settles smoothly).
//! No occlusion but the creases'.
//!
//! `VICTORY_AMBIENT`:
//! - unset or `0`/`off`: the title's own output.
//! - a number: that share of the estimate (1: all of it).
//! - `constant:<level>`: the constant, as beloooo's fork has it.
//!
//! `VICTORY_AMBIENT_SHADE=<strength>,<radius in metres>` sets the creases.
//! `VICTORY_AMBIENT_MOST=<multiple>` caps what the title's resolve leaves (4 when unset).
//! `VICTORY_AMBIENT_SETTLE=<ms>[,<jump factor>]` sets time-based settling (default `250,3`; `0` for per-frame `0.04`).
//!
//! The air of a room is lit by the same probes: without rays they hold the
//! sky's light indoors too, a brown veil at sunset. `VICTORY_AMBIENT_AIR`
//! bounds what the air scatters by a multiple of the light around and scrubs
//! NaN/Inf values from the scattering volume.
//!
//! (foxhound `9f8838b`) Reflections are traced too, at half the picture's
//! size: rays the title's screen-space march resolves
//! (`CS_ResolveScreenSpace`) take the light seen on screen; the others go to
//! the structures, find nothing, and `CS_RayMiss` gives each the sky's light
//! from the sky's cube maps: bright sky in dark rooms, on glass and on
//! eyes. `VICTORY_AMBIENT_SPECULAR` puts in, right after that program, the
//! same estimate taken along the reflected direction ([`Role::Miss`]); the
//! screen-space resolve that comes next still writes what it found over it.
//!
//! `crate::ambient_reflect` has three more, off by default: the same without
//! a depth (`VICTORY_AMBIENT_SPECULAR_BOUND`), a hold of the reflections'
//! temporal resolves (`VICTORY_AMBIENT_REFLECT`, [`Role::Reflect`]) and a
//! soft knee after their history pass (`VICTORY_REFLECT_KNEE`, [`Role::Knee`]).
//!
//! `crate::ambient_sky` has three more, off by default: the title's sky in
//! reflections bounded by the picture's light (`VICTORY_AMBIENT_SKY_BOUND`,
//! [`sky_bound`]), one bounce of the title's light only
//! (`VICTORY_AMBIENT_ONCE`) and the green and cyan probe leftovers dropped
//! (`VICTORY_AMBIENT_CHROMA_SCRUB`).

use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use windows::Win32::Graphics::Direct3D12::*;
use windows::core::s;

use crate::host::{HostList, host};

/// Programs of the title the stand-in follows.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Role {
    /// Resolves the diffuse light.
    Resolve,
    /// Keeps the light visible surfaces send out.
    Radiance,
    /// Adds the lamps' light to what the air scatters (the last program
    /// that writes that volume before it is summed up along the view).
    Air,
    /// Gives the reflection rays that found nothing the sky's light
    /// (`CS_RayMiss`, not `CS_RayMiss_Volumetrics` nor the screen probes'):
    /// only with `VICTORY_AMBIENT_SPECULAR` set.
    Miss,
    /// Resolves the traced reflections over time: only with
    /// `VICTORY_AMBIENT_REFLECT` set (`crate::ambient_reflect`).
    Reflect,
    /// Keeps the reflections' history (`CS_UpdateHistory`): only with
    /// `VICTORY_REFLECT_KNEE` set.
    Knee,
}

impl Role {
    /// Whether a pass of ours holds the image this program wrote, and so
    /// must not follow a dispatch that may have run no group.
    pub(crate) fn holds_image(self) -> bool {
        matches!(self, Role::Reflect | Role::Knee)
    }
}

/// The program's part in this, by its name in the container.
pub(crate) fn role(container: &[u8]) -> Option<Role> {
    let has = |name: &[u8]| container.windows(name.len()).any(|w| w == name);
    if has(b"CS_ScreenProbes_TemporalResolve") {
        Some(Role::Resolve)
    } else if has(b"CS_CacheScreenSpaceRadiance") {
        Some(Role::Radiance)
    } else if has(b"CS_AccumulateLocalLightInscatteringVolume") {
        Some(Role::Air)
    } else if is_miss(container) {
        // Whatever the switches: the sky bound (`crate::ambient_sky`) can be
        // turned on while running; `crate::draw` `miss_views` does nothing
        // after it while neither it nor `VICTORY_AMBIENT_SPECULAR` is on.
        Some(Role::Miss)
    } else if crate::ambient_reflect::resolves(container, crate::ambient_reflect::reflect_in()) {
        Some(Role::Reflect)
    } else if crate::ambient_reflect::knee().is_some() && crate::ambient_reflect::keeps_history(container) {
        Some(Role::Knee)
    } else {
        None
    }
}

/// `CS_RayMiss_` and the eight hex digits of a variant's hash.
fn is_miss(container: &[u8]) -> bool {
    const NAME: &[u8] = b"CS_RayMiss_";
    container.windows(NAME.len() + 9).any(|w| {
        w.starts_with(NAME)
            && w[NAME.len()..NAME.len() + 8].iter().all(u8::is_ascii_hexdigit)
            && w[NAME.len() + 8] == b'_'
    })
}

/// Where the resolve writes the diffuse light (R9G9B9E5 as 32-bit integers)
/// and reads the scene's depth, and where the other writes the light of
/// visible surfaces: root parameter and place in its table, in dwords.
pub(crate) const LIGHT: (u8, u32) = (1, 176);
pub(crate) const DEPTH: (u8, u32) = (0, 464);
pub(crate) const RADIANCE: (u8, u32) = (1, 16);
/// Where the light the air scatters is.
pub(crate) const AIR: (u8, u32) = (1, 32);
/// Where the miss program writes the reflected light (R11G11B10, half the
/// picture's size) and where it reads the depth it is given (a 32-bit depth
/// of the picture's size).
pub(crate) const MISS_LIGHT: (u8, u32) = (1, 104);
pub(crate) const MISS_DEPTH: (u8, u32) = (0, 1000);
/// The miss program's first root constant: which surfaces its rays are of.
pub(crate) const MISS_OPAQUE: u32 = 0;
pub(crate) const MISS_ALPHA: u32 = 1;

#[derive(Clone, Copy, PartialEq, Debug)]
enum Set {
    Off,
    Estimate(f32),
    Constant(f32),
}

fn parse(set: &str) -> Set {
    let set = set.trim();
    let number = |t: &str| t.trim().parse::<f32>().ok().filter(|v| v.is_finite() && *v > 0.0);
    match set.strip_prefix("constant:") {
        Some(level) => number(level).map_or(Set::Off, Set::Constant),
        None => number(set).map_or(Set::Off, Set::Estimate),
    }
}

/// Live, as are the air, the cap, the settling and the shade: each pass
/// takes them as root constants when it is recorded. (The specular share,
/// its bound, the holds and the violet drop choose the pipelines the pass
/// is made with once, and stay read at start.)
fn set() -> Set {
    static SET: victory_core::settings::Live = victory_core::settings::Live::new("VICTORY_AMBIENT", |v| {
        match parse(v.unwrap_or_default()) {
            Set::Off => 0,
            Set::Estimate(level) => 1 << 32 | u64::from(level.to_bits()),
            Set::Constant(level) => 2 << 32 | u64::from(level.to_bits()),
        }
    });
    let bits = SET.get();
    let level = f32::from_bits(bits as u32);
    match bits >> 32 {
        1 => Set::Estimate(level),
        2 => Set::Constant(level),
        _ => Set::Off,
    }
}

/// Two `f32`s in a [`victory_core::settings::Live`] cell's bits, and back.
fn pack(a: f32, b: f32) -> u64 {
    u64::from(a.to_bits()) << 32 | u64::from(b.to_bits())
}

fn unpack(bits: u64) -> [f32; 2] {
    [f32::from_bits((bits >> 32) as u32), f32::from_bits(bits as u32)]
}

/// Whether the stand-in following `role` is on: set, and the title's rays
/// find nothing there ([`rays_find`]).
pub(crate) fn on(role: Role) -> bool {
    (set() != Set::Off || specular().is_some())
        && !(crate::pipeline::builder_runs()
            && rays_find(
                role,
                victory_core::settings::ray_tracing::on(),
                crate::rt_hit_only::reflections_walk(),
                crate::rt_hit_only::probes_walk(),
            ))
}

/// Whether the screen probes' rays find nothing ([`rays_find`] for the diffuse light), the
/// stand-in set or not (`crate::zero_out`'s inject).
pub(crate) fn probes_blind() -> bool {
    !(crate::pipeline::builder_runs()
        && rays_find(
            Role::Resolve,
            victory_core::settings::ray_tracing::on(),
            crate::rt_hit_only::reflections_walk(),
            crate::rt_hit_only::probes_walk(),
        ))
}

/// With the structure builder running: whether the rays `role`'s stand-in
/// stands in for find something. Turned on by its own switches, the builder
/// lets every ray walk (the whole stand-in steps aside, as before).
/// `VICTORY_RAY_TRACING=on` lets only the listed traces walk
/// (`VICTORY_RT_HIT_ONLY`): the reflection stand-ins step aside when the
/// reflections walk, the diffuse one when the light probes do, so nothing
/// is lit twice and nothing the rays still miss goes dark.
fn rays_find(role: Role, ray_tracing: bool, reflections: bool, probes: bool) -> bool {
    match role {
        _ if !ray_tracing => true,
        Role::Miss | Role::Reflect | Role::Knee => reflections,
        Role::Resolve | Role::Radiance | Role::Air => probes,
    }
}

/// `VICTORY_AMBIENT_SPECULAR=<share>[,<sky>]` (foxhound `9f8838b`; off by
/// default): reflections whose rays found nothing take `share` of the
/// estimate along the reflected direction instead of the sky's light; `sky`
/// (0 to 1, 0 if not given) of them stays the title's sky, made no brighter
/// than the estimate. Unset or 0: the title's sky.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) struct Specular {
    pub(crate) share: f32,
    pub(crate) sky: f32,
}

fn parse_specular(set: &str) -> Option<Specular> {
    let mut parts = set
        .split(',')
        .map(|t| t.trim().parse::<f32>().ok().filter(|v| v.is_finite()));
    let share = parts.next().flatten().filter(|v| *v > 0.0)?;
    let sky = parts.next().flatten().map_or(0.0, |v| v.clamp(0.0, 1.0));
    Some(Specular { share, sky })
}

pub(crate) fn specular() -> Option<Specular> {
    static SET: OnceLock<Option<Specular>> = OnceLock::new();
    *SET.get_or_init(|| parse_specular(&std::env::var("VICTORY_AMBIENT_SPECULAR").unwrap_or_default()))
}

/// `VICTORY_AMBIENT_SPECULAR_ALPHA=0`: the alpha layer's misses (glass,
/// foam, wakes; the depth read is the opaque one behind them) keep the
/// title's sky. Unset: they take the estimate too (foxhound's default: glass
/// in a dark room mirrored the bright sky without it).
pub(crate) fn specular_alpha() -> bool {
    static SET: victory_core::settings::Live = victory_core::settings::Live::new("VICTORY_AMBIENT_SPECULAR_ALPHA", |v| {
        u64::from(!v.is_some_and(|v| v.trim() == "0"))
    });
    SET.get_bool()
}

/// The most the air scatters, as a multiple of the light around
/// (`VICTORY_AMBIENT_AIR=<multiple>`; unset or 0: what the title has).
fn air() -> f32 {
    static SET: victory_core::settings::Live = victory_core::settings::Live::new("VICTORY_AMBIENT_AIR", |v| {
        let set = v.unwrap_or_default();
        u64::from(set.trim().parse().ok().filter(|v: &f32| v.is_finite() && *v > 0.0).unwrap_or(0.0).to_bits())
    });
    f32::from_bits(SET.get() as u32)
}

/// How many times the estimate the title's own result may keep
/// (`VICTORY_AMBIENT_MOST=<multiple>`, 4 when unset). Without rays the
/// probes hold the sky's light indoors too; at 4 times the estimate a room
/// looked towards a window or the sky goes very bright.
fn most() -> f32 {
    static SET: victory_core::settings::Live = victory_core::settings::Live::new("VICTORY_AMBIENT_MOST", |v| {
        let set = v.unwrap_or_default();
        u64::from(set.trim().parse().ok().filter(|v: &f32| v.is_finite() && *v > 0.0).unwrap_or(4.0).to_bits())
    });
    f32::from_bits(SET.get() as u32)
}

/// How the slow mean of the light around follows the frames
/// (`VICTORY_AMBIENT_SETTLE=<time constant in ms>[,<jump factor>]`; unset:
/// `250,3`; `0`: 0.04 of the way a frame, as before). Set, it is the same
/// share of the way a second whatever the frame rate, and a change by more
/// than the jump factor (default 3; 0: none) is taken at once.
#[derive(Clone, Copy, PartialEq, Debug)]
struct Settle {
    ms: f32,
    jump: f32,
}

fn parse_settle(set: &str) -> Option<Settle> {
    let mut parts = set.split(',').map(|p| p.trim().parse::<f32>().ok());
    let ms = parts.next().flatten().filter(|v| v.is_finite() && *v > 0.0)?;
    let jump = parts.next().flatten().filter(|v| v.is_finite() && *v >= 0.0).unwrap_or(3.0);
    Some(Settle { ms, jump })
}

fn settle() -> Option<Settle> {
    // 0: none (a settling's `ms` is above 0).
    static SET: victory_core::settings::Live = victory_core::settings::Live::new("VICTORY_AMBIENT_SETTLE", |v| {
        parse_settle(v.unwrap_or("250,3")).map_or(0, |s| pack(s.ms, s.jump))
    });
    let bits = SET.get();
    let [ms, jump] = unpack(bits);
    (bits != 0).then_some(Settle { ms, jump })
}

/// Strength and radius of the creases' shade.
fn shade() -> [f32; 2] {
    static SET: victory_core::settings::Live = victory_core::settings::Live::new("VICTORY_AMBIENT_SHADE", |v| {
        let mut shade = [1.0, 0.6];
        let set = v.unwrap_or_default();
        for (to, v) in shade.iter_mut().zip(set.split(',')) {
            if let Ok(v) = v.trim().parse() {
                *to = v;
            }
        }
        pack(shade[0], shade[1])
    });
    unpack(SET.get())
}

const SOURCE: &str = r#"
#define RS "RootConstants(num32BitConstants=12, b0), DescriptorTable(SRV(t0)), DescriptorTable(UAV(u0)), UAV(u1), UAV(u2)"
#define CX 64
#define CY 36
#define CELLS (CX * CY)
#define PX 160
#define PY 90
cbuffer Set : register(b0) {
    float level; float constant; float strength; float radius;
    float near; float tan_y; uint width; uint height;
    float follow; float most; float air; float jump;
};
Texture2D<float> depth : register(t0);
// Cells: the light they send out (then, at CELLS, the slow mean), from
// CELLS + 1 on where they are, from SHARES on the share of the title's own
// light at each of the 25 places of a cell the keep reads (`ONCE`; x, and y:
// written), at GEOMEAN the slow geometric mean of the cells' luminance (`SKY`).
#define SHARES (2 * CELLS + 1)
#define GEOMEAN (SHARES + 25 * CELLS)
RWStructuredBuffer<float4> cells : register(u1);
// Three a point: red, green and blue, each a mean and a direction.
RWStructuredBuffer<float4> points : register(u2);
static const float3 LUMA = float3(0.2126, 0.7152, 0.0722);

// The point seen at a texel, in metres from the eye (x right, y up, z away).
float3 seen(int2 p, float2 size) {
    float d = depth.Load(int3(clamp(p, int2(0, 0), int2(size) - 1), 0));
    float z = min(near / max(d, 1e-7), 1000.0);
    float2 ndc = (float2(p) + 0.5) / size * 2.0 - 1.0;
    return float3(ndc.x * tan_y * size.x / size.y * z, -ndc.y * tan_y * z, z);
}

#ifdef KEEP
RWTexture2D<float3> radiance : register(u0);
[RootSignature(RS)]
[numthreads(8, 4, 1)]
void keep(uint3 at : SV_DispatchThreadID) {
    uint w, h;
    radiance.GetDimensions(w, h);
    float3 sum = 0;
    for (uint y = 0; y < 5; y++) for (uint x = 0; x < 5; x++) {
        uint2 p = uint2((float2(at.xy) + (float2(x, y) + 0.5) / 5.0) * float2(w, h) / float2(CX, CY));
        float3 c = max(radiance[p], 0.0);
        c = any(isnan(c)) || any(isinf(c)) ? 0.0 : c;
        // Lamps and glints count, but not for everything.
#ifdef ONCE
        // (readmet3xt-lab 8cf0a75) Not the stand-in's own light, reflected:
        // counted by the share of the title's own light the raise noted there.
        c /= max(1.0, dot(c, LUMA) / 20.0);
        float4 share = cells[SHARES + (at.y * CX + at.x) * 25 + y * 5 + x];
        sum += share.y > 0.0 ? c * saturate(share.x) : c;
#else
        sum += c / max(1.0, dot(c, LUMA) / 20.0);
#endif
    }
    cells[at.y * CX + at.x] = float4(sum / 25.0, 1.0);
}
[RootSignature(RS)]
[numthreads(1, 1, 1)]
void mean() {
    float3 sum = 0;
    for (uint i = 0; i < CELLS; i++) sum += cells[i].rgb;
    float4 was = cells[CELLS];
    float3 now = sum / CELLS;
    float was_l = dot(was.rgb, LUMA);
    float now_l = dot(now, LUMA);
    bool jumped = jump > 0.0 && was.a > 0.0 && (now_l > was_l * jump || was_l > now_l * jump);
    cells[CELLS] = float4((was.a > 0.0 && !jumped) ? lerp(was.rgb, now, follow) : now, 1.0);
}
#endif

#ifdef PLACE
[RootSignature(RS)]
[numthreads(8, 4, 1)]
void place(uint3 at : SV_DispatchThreadID) {
    float2 size = float2(width, height);
    float3 p = seen(int2((float2(at.xy) + 0.5) * size / float2(CX, CY)), size);
    cells[CELLS + 1 + at.y * CX + at.x] = float4(p, p.z * p.z);
}
#endif

#ifdef GATHER
[RootSignature(RS)]
[numthreads(8, 2, 1)]
void gather(uint3 at : SV_DispatchThreadID) {
    float2 size = float2(width, height);
    float3 p = seen(int2((float2(at.xy) + 0.5) * size / float2(PX, PY)), size);
    float r2 = 0.25 + 0.0025 * p.z * p.z;
    float whole = 0;
    float3 toward = 0, light = 0, red = 0, green = 0, blue = 0;
    for (uint i = 0; i < CELLS; i++) {
        float4 c = cells[CELLS + 1 + i];
        float3 L = cells[i].rgb;
        float3 d = c.xyz - p;
        float dd = dot(d, d);
        float w = c.w / (dd + r2);
        float3 dir = d / sqrt(dd + 0.25 * r2);
        whole += w;
        toward += w * dir;
        light += w * L;
        red += w * L.r * dir;
        green += w * L.g * dir;
        blue += w * L.b * dir;
    }
    whole = max(whole, 1e-6);
    light /= whole;
    toward /= whole;
    uint at0 = (at.y * PX + at.x) * 3;
    points[at0] = float4(red / whole - light.r * toward, light.r);
    points[at0 + 1] = float4(green / whole - light.g * toward, light.g);
    points[at0 + 2] = float4(blue / whole - light.b * toward, light.b);
}
#endif

#ifdef AIR
RWTexture3D<float3> scattered : register(u0);
// No more than a multiple of the light around: of the room as the last
// seconds showed it, and of the surface seen in this direction (towards a
// window the air outside may be as bright as the sky is).
[RootSignature(RS)]
[numthreads(8, 8, 2)]
void clear_air(uint3 at : SV_DispatchThreadID) {
    uint w, h, d;
    scattered.GetDimensions(w, h, d);
    if (at.x >= w || at.y >= h || at.z >= d) return;
    uint2 q = min(uint2((float2(at.xy) + 0.5) / float2(w, h) * float2(PX, PY)), uint2(PX - 1, PY - 1));
    uint at0 = (q.y * PX + q.x) * 3;
    float3 seen_there = float3(points[at0].w, points[at0 + 1].w, points[at0 + 2].w);
    float far = float(at.z) / float(d);
    float cap = air * lerp(dot(cells[CELLS].rgb, LUMA), dot(seen_there, LUMA), far);
    float3 c = scattered[at];
    // A NaN or infinity in the air is summed along every view ray
    // through it: black or white patches floating in the room.
    if (any(isnan(c)) || any(isinf(c))) { scattered[at] = 0; return; }
    float has = dot(c, LUMA);
    if (has > cap) scattered[at] = c * (cap / has);
}
#endif

#ifdef RAISE
RWTexture2D<uint> light : register(u0);
float3 unpack(uint v) {
    return float3(v & 511, (v >> 9) & 511, (v >> 18) & 511) * exp2(float(v >> 27) - 24.0);
}
uint pack(float3 c) {
    c = clamp(c, 0.0, 65408.0);
    float top = max(c.r, max(c.g, c.b));
    int e = clamp(int(floor(log2(max(top, 1e-9)))) + 16, 0, 31);
    float scale = exp2(float(24 - e));
    if (top * scale >= 511.5) { e = min(e + 1, 31); scale = exp2(float(24 - e)); }
    uint3 m = min(uint3(c * scale + 0.5), 511u);
    return m.r | (m.g << 9) | (m.b << 18) | (uint(e) << 27);
}
[RootSignature(RS)]
[numthreads(8, 8, 1)]
void raise(uint3 at : SV_DispatchThreadID) {
    float2 size = float2(width, height);
    if (at.x >= width || at.y >= height) return;
    int2 p = int2(at.xy);
    int2 max_p = int2(size) - 1;
    float3 c = seen(p, size);
    float3 l = seen(p + int2(-1, 0), size), r = seen(p + int2(1, 0), size);
    float3 u = seen(p + int2(0, -1), size), d = seen(p + int2(0, 1), size);
    // Minimum-slope neighbor on each axis so 1-pixel depth discontinuities at
    // concave corners/crevasses do not flip the reconstructed surface normal.
    float3 dx = p.x <= 0 ? r - c : (p.x >= max_p.x ? c - l : (abs(r.z - c.z) < abs(c.z - l.z) ? r - c : c - l));
    float3 dy = p.y <= 0 ? d - c : (p.y >= max_p.y ? c - u : (abs(d.z - c.z) < abs(c.z - u.z) ? d - c : c - u));
    float3 n = cross(dy, dx);
    float nn = dot(n, n);
    n = nn >= 1e-12 ? n * rsqrt(nn) : float3(0, 0, -1);
    if (dot(n, c) > 0.0) n = -n;
    // How open it is: crisp quadratic spiral AO in creases and corners.
    float open = 1.0;
    float px = radius * size.y / (2.0 * tan_y * c.z);
    float noise = frac(52.9829189 * frac(dot(float2(p), float2(0.06711056, 0.00583715))));
    if (strength > 0.0 && px >= 1.5 && c.z < 400.0 && nn >= 1e-12) {
        float max_r = min(px, 64.0);
        float min_r = min(2.0, max_r);
        float sum = 0.0;
        float r2 = radius * radius;
        const int N = 16;
        for (int i = 0; i < N; i++) {
            float a = float(i) * 2.399963 + noise * 6.2831853;
            float t = (float(i) + 0.5) / float(N);
            float2 o = float2(cos(a), sin(a)) * lerp(min_r, max_r, t * t);
            float3 v = seen(p + int2(round(o)), size) - c;
            float vv = dot(v, v);
            float over = dot(v, n) - 0.002 * c.z - 0.003;
            float falloff = 1.0 - smoothstep(0.0, r2, vv);
            sum += max(over, 0.0) * rsqrt(vv + 1e-6) * falloff;
        }
        open = saturate(pow(max(1.0 - strength * 2.0 * sum / float(N), 1e-4), 1.35));
    }
    float3 found;
    if (constant > 0.0) {
        found = float3(1.0, 0.98, 0.94) * constant * open;
        light[p] = pack(found);
        return;
    }
    // Between the four points around, those at its own depth first; which
    // four is dithered a little.
    float2 g = (float2(p) + 0.5) / size * float2(PX, PY) - 0.5 + (noise - 0.5) * 0.5;
    int2 g0 = int2(floor(g));
    float2 f = g - float2(g0);
    float4 sr = 0, sg = 0, sb = 0;
    float sum = 0;
    for (int k = 0; k < 4; k++) {
        int2 q = clamp(g0 + int2(k & 1, k >> 1), int2(0, 0), int2(PX, PY) - 1);
        float z = seen(int2((float2(q) + 0.5) * size / float2(PX, PY)), size).z;
        float2 b = float2(k & 1 ? f.x : 1 - f.x, k >> 1 ? f.y : 1 - f.y);
        float w = (b.x * b.y + 1e-3) / (0.02 + abs(z - c.z) / c.z);
        uint at0 = (q.y * PX + q.x) * 3;
        sr += w * points[at0];
        sg += w * points[at0 + 1];
        sb += w * points[at0 + 2];
        sum += w;
    }
    float3 all = float3(sr.w, sg.w, sb.w) / sum;
    found = all + 1.2 * float3(dot(sr.xyz, n), dot(sg.xyz, n), dot(sb.xyz, n)) / sum;
    found = clamp(found, 0.2 * all, 2.5 * all);
    found = lerp(found, cells[CELLS].rgb, 0.2) * level * open;
    float3 was = unpack(light[p]);
    // Magenta (green far below red and blue) is what the probes' memory held, not light.
    if (was.g < 0.5 * min(was.r, was.b) && max(was.r, was.b) > 0.02) was = 0.0;
#ifdef VIOLET
    // And the bluer kind of it (blue over 2.5x green and over red), which the line above
    // leaves as a faint violet (readmet3xt-lab/foxhound 9f8838b109: a purple-blue glow on a
    // lit sheet of paper in the prison cell, RTX 5080). The sky's light keeps green above
    // 0.4x blue, sunsets keep blue low, so neither is touched.
    if (was.b > 2.5 * was.g && was.b > was.r && was.b > 0.02) was = 0.0;
#endif
#ifdef CHROMA
    // (Vladimir-Human, trustno1-coder/VIctory-NVIDIA#48) Acid green (green over
    // 2.5x red and blue) and electric cyan (red under 0.35x green and blue):
    // what unbuilt or stale probes left on thin foliage edges.
    if (was.g > 2.5 * max(was.r, was.b) && was.g > 0.02) was = 0.0;
    if (was.r < 0.35 * min(was.g, was.b) && max(was.g, was.b) > 0.02) was = 0.0;
#endif
    // What the program left counts as far as it can be true: the sky's light
    // outdoors, not the blocks of other memory.
    float top = max(dot(found, LUMA) * most, 1e-6);
    float has = dot(was, LUMA);
    was *= min(1.0, top / max(has, 1e-6));
    light[p] = pack(max(was, found));
#ifdef ONCE
    // (readmet3xt-lab 8cf0a75) At the places the next keep reads, the share
    // of the light that is the title's own.
    float3 now = max(was, found);
    float2 gc = (float2(p) + 0.5) / size * float2(CX, CY);
    uint2 cell = min(uint2(gc), uint2(CX - 1, CY - 1));
    uint2 place = min(uint2((gc - float2(cell)) * 5.0), uint2(4, 4));
    uint2 spot = uint2((float2(cell) + (float2(place) + 0.5) / 5.0) * size / float2(CX, CY));
    if (spot.x == uint(p.x) && spot.y == uint(p.y)) {
        float share = dot(was, LUMA) / max(dot(now, LUMA), 1e-6);
        cells[SHARES + (cell.y * CX + cell.x) * 25 + place.y * 5 + place.x] = float4(share, 1.0, 0.0, 0.0);
    }
#endif
}
#endif

#ifdef REFLECT
RWTexture2D<float3> reflected : register(u0);
// Each texel of the reflections' image (half the picture's size): the light
// the points around send along the reflected direction, as `raise` takes it
// along the normal; `level` is the share, `constant` the share of the
// title's sky kept, made no brighter than that.
[RootSignature(RS)]
[numthreads(8, 8, 1)]
void reflect_light(uint3 at : SV_DispatchThreadID) {
    uint w, h;
    reflected.GetDimensions(w, h);
    if (at.x >= w || at.y >= h) return;
    float2 size = float2(width, height);
    int2 p = int2((float2(at.xy) + 0.5) * size / float2(w, h));
    float3 c = seen(p, size);
    // Nothing is reflected by the sky.
    if (c.z >= 999.0) return;
    float3 l = seen(p + int2(-1, 0), size), r = seen(p + int2(1, 0), size);
    float3 u = seen(p + int2(0, -1), size), d = seen(p + int2(0, 1), size);
    float3 dx = abs(l.z - c.z) < abs(r.z - c.z) ? c - l : r - c;
    float3 dy = abs(u.z - c.z) < abs(d.z - c.z) ? c - u : d - c;
    float3 n = normalize(cross(dy, dx) + float3(0, 0, -1e-9));
    if (dot(n, c) > 0.0) n = -n;
    float3 v = normalize(c);
    float3 ray = v - 2.0 * dot(v, n) * n;
    float2 g = (float2(p) + 0.5) / size * float2(PX, PY) - 0.5;
    int2 g0 = int2(floor(g));
    float2 f = g - float2(g0);
    float4 sr = 0, sg = 0, sb = 0;
    float sum = 0;
    for (int k = 0; k < 4; k++) {
        int2 q = clamp(g0 + int2(k & 1, k >> 1), int2(0, 0), int2(PX, PY) - 1);
        float z = seen(int2((float2(q) + 0.5) * size / float2(PX, PY)), size).z;
        float2 b = float2(k & 1 ? f.x : 1 - f.x, k >> 1 ? f.y : 1 - f.y);
        float wgt = (b.x * b.y + 1e-3) / (0.02 + abs(z - c.z) / c.z);
        uint at0 = (q.y * PX + q.x) * 3;
        sr += wgt * points[at0];
        sg += wgt * points[at0 + 1];
        sb += wgt * points[at0 + 2];
        sum += wgt;
    }
    float3 all = float3(sr.w, sg.w, sb.w) / sum;
    // Radiance from the first moments: three times the direction's share
    // for an even spread of directions, a little less for a smooth image.
    float3 found = all + 2.0 * float3(dot(sr.xyz, ray), dot(sg.xyz, ray), dot(sb.xyz, ray)) / sum;
    found = clamp(found, 0.1 * all, 3.0 * all);
    found = lerp(found, cells[CELLS].rgb, 0.2) * level;
    found = any(isnan(found)) || any(isinf(found)) ? 0.0 : max(found, 0.0);
    float3 was = reflected[at.xy];
    was = any(isnan(was)) || any(isinf(was)) ? 0.0 : max(was, 0.0);
    float has = dot(was, LUMA);
    was *= min(1.0, dot(found, LUMA) / max(has, 1e-6));
    reflected[at.xy] = lerp(found, was, constant);
}
#endif
#ifdef BOUND
RWTexture2D<float3> reflected : register(u0);
// (trustno1-coder/VIctory-NVIDIA#124) The rays' image where the scene's depth
// was not found (no point, no normal, so no reflected direction): each texel
// takes the light the points around the surface seen there hold, whatever
// its direction. `level` and `constant` as in `reflect_light`.
[RootSignature(RS)]
[numthreads(8, 8, 1)]
void bound_light(uint3 at : SV_DispatchThreadID) {
    uint w, h;
    reflected.GetDimensions(w, h);
    if (at.x >= w || at.y >= h) return;
    uint2 q = min(uint2((float2(at.xy) + 0.5) / float2(w, h) * float2(PX, PY)), uint2(PX - 1, PY - 1));
    uint at0 = (q.y * PX + q.x) * 3;
    float3 seen_there = float3(points[at0].w, points[at0 + 1].w, points[at0 + 2].w);
    float3 found = lerp(seen_there, cells[CELLS].rgb, 0.2) * level;
    found = any(isnan(found)) || any(isinf(found)) ? 0.0 : max(found, 0.0);
    float3 was = reflected[at.xy];
    was = any(isnan(was)) || any(isinf(was)) ? 0.0 : max(was, 0.0);
    float has = dot(was, LUMA);
    was *= min(1.0, dot(found, LUMA) / max(has, 1e-6));
    reflected[at.xy] = lerp(found, was, constant);
}
#endif

#ifdef SKY
RWTexture2D<float3> reflected : register(u0);
// (wasonidas, trustno1-coder/VIctory-NVIDIA#63) The slow geometric mean of the
// luminance of the cells that send out anything: a window with the sunset in
// it is a third of a cell's picture and 200 times the room, and the plain mean
// is the window's; this one is the room's.
[RootSignature(RS)]
[numthreads(1, 1, 1)]
void geo_mean() {
    float logs = 0, lit = 0;
    for (uint i = 0; i < CELLS; i++) {
        float l = dot(cells[i].rgb, LUMA);
        if (l > 1e-5) { logs += log(l); lit += 1.0; }
    }
    float4 was = cells[GEOMEAN];
    float g = lit > 0.0 ? exp(logs / lit) : 0.0;
    cells[GEOMEAN] = float4(was.a > 0.0 ? lerp(was.r, g, follow) : g, 0.0, 0.0, 1.0);
}
// (#63, #393) The texels of a reflections' image keep the title's sky light,
// but no more than `level` times the light around: the geometric mean, one
// value for the whole picture (nothing of the texel's own depth or normal is
// read, so nothing can be ragged). What is taken away goes towards the colour
// around, so that what is left of a blue sky indoors is not a blue sheen.
[RootSignature(RS)]
[numthreads(8, 8, 1)]
void bound_sky(uint3 at : SV_DispatchThreadID) {
    uint w, h;
    reflected.GetDimensions(w, h);
    if (at.x >= w || at.y >= h) return;
    float3 was = reflected[at.xy];
    if (any(isnan(was)) || any(isinf(was))) { reflected[at.xy] = 0.0; return; }
    was = max(was, 0.0);
    float has = dot(was, LUMA);
    if (has <= 0.0) return;
    float3 room = max(cells[CELLS].rgb, 0.0);
    float4 geo = cells[GEOMEAN];
    float around = geo.a > 0.0 ? geo.r : dot(room, LUMA);
    float cap = level * around;
    if (has <= cap) return;
    float kept = cap / has;
    float3 toned = room * (cap / max(dot(room, LUMA), 1e-6));
    reflected[at.xy] = lerp(toned, was * kept, kept);
}
#endif

#ifdef HOLD
RWTexture2D<uint> held : register(u0);
float3 unpack_held(uint v) {
    return float3(v & 511, (v >> 9) & 511, (v >> 18) & 511) * exp2(float(v >> 27) - 24.0);
}
uint pack_held(float3 c) {
    c = clamp(c, 0.0, 65408.0);
    float top = max(c.r, max(c.g, c.b));
    int e = clamp(int(floor(log2(max(top, 1e-9)))) + 16, 0, 31);
    float scale = exp2(float(24 - e));
    if (top * scale >= 511.5) { e = min(e + 1, 31); scale = exp2(float(24 - e)); }
    uint3 m = min(uint3(c * scale + 0.5), 511u);
    return m.r | (m.g << 9) | (m.b << 18) | (uint(e) << 27);
}
// The light around a texel of an image of the picture's shape: the estimate
// at the four points about it, with a fifth of the room's slow mean, or the
// constant.
float around_level(uint2 at, uint w, uint h) {
    if (constant > 0.0) return dot(float3(1.0, 0.98, 0.94) * constant, LUMA);
    float2 g = (float2(at) + 0.5) / float2(w, h) * float2(PX, PY) - 0.5;
    int2 g0 = int2(floor(g));
    float2 f = g - float2(g0);
    float3 around = 0;
    for (int k = 0; k < 4; k++) {
        int2 q = clamp(g0 + int2(k & 1, k >> 1), int2(0, 0), int2(PX, PY) - 1);
        float2 b = float2(k & 1 ? f.x : 1 - f.x, k >> 1 ? f.y : 1 - f.y);
        uint at0 = (q.y * PX + q.x) * 3;
        around += b.x * b.y * float3(points[at0].w, points[at0 + 1].w, points[at0 + 2].w);
    }
    return dot(lerp(around, cells[CELLS].rgb, 0.2), LUMA);
}
// (Vice-Tay, from avatarmanz/VIctory-7900XTX#314) The reflections no brighter
// than `most` times the light around, hue kept.
[RootSignature(RS)]
[numthreads(8, 8, 1)]
void hold(uint3 at : SV_DispatchThreadID) {
    uint w, h;
    held.GetDimensions(w, h);
    if (at.x >= w || at.y >= h) return;
    float top = most * around_level(at.xy, w, h);
    float3 c = unpack_held(held[at.xy]);
    float has = dot(c, LUMA);
    if (has > top) held[at.xy] = pack_held(c * (top / max(has, 1e-9)));
}
// (CCS03) A soft knee: as it is up to half the ceiling (`most` times the
// light around), bent towards the ceiling above that and never reaching it;
// by the brightest channel, so a saturated colour bends as hard as white.
[RootSignature(RS)]
[numthreads(8, 8, 1)]
void knee(uint3 at : SV_DispatchThreadID) {
    uint w, h;
    held.GetDimensions(w, h);
    if (at.x >= w || at.y >= h) return;
    float ceiling = max(most * around_level(at.xy, w, h), 1e-6);
    float3 c = unpack_held(held[at.xy]);
    float top = max(c.r, max(c.g, c.b));
    float start = 0.5 * ceiling;
    if (!(top > start)) return;
    float room = ceiling - start;
    float bent = start + room * (1.0 - exp(-(top - start) / room));
    held[at.xy] = pack_held(c * (bent / top));
}
#endif
"#;

/// Bytes of the cells' buffer and of the points'.
const CELLS: u64 = 64 * 36;
const CELL_BYTES: u64 = (2 * CELLS + 1 + 25 * CELLS + 1) * 16;
const POINT_BYTES: u64 = 160 * 90 * 48;

struct Pass {
    root: ID3D12RootSignature,
    keep: ID3D12PipelineState,
    mean: ID3D12PipelineState,
    place: ID3D12PipelineState,
    gather: ID3D12PipelineState,
    raise: ID3D12PipelineState,
    air: ID3D12PipelineState,
    /// Made only with `VICTORY_AMBIENT_SPECULAR` set.
    reflect: Option<ID3D12PipelineState>,
    /// Made only with their switches set (`crate::ambient_reflect`); one
    /// that does not compile is left out, the others stand.
    bound: Option<ID3D12PipelineState>,
    hold: Option<ID3D12PipelineState>,
    knee: Option<ID3D12PipelineState>,
    cells: ID3D12Resource,
    points: ID3D12Resource,
}
// SAFETY: D3D12 objects are free-threaded.
unsafe impl Send for Pass {}
// SAFETY: as above.
unsafe impl Sync for Pass {}

/// Whether bluish-violet probe leftovers are dropped from the title's light too
/// (`VICTORY_AMBIENT_VIOLET=1`; off by default: only the magenta kind is).
fn violet() -> bool {
    static SET: OnceLock<bool> = OnceLock::new();
    *SET.get_or_init(|| std::env::var("VICTORY_AMBIENT_VIOLET").is_ok_and(|v| v.trim() == "1"))
}

/// The defines every pass is made with: the violet drop's and the green and
/// cyan scrub's (`crate::ambient_sky::chroma`), both read at start.
fn defines() -> String {
    let violet = if violet() { "#define VIOLET\n" } else { "" };
    let chroma = if crate::ambient_sky::chroma() { "#define CHROMA\n" } else { "" };
    format!("{violet}{chroma}")
}

/// A pass made only once a live switch first asks for it
/// (`crate::ambient_sky`), with the pass's root signature: none of them is
/// compiled in a run that never turns its switch on.
struct Late(Option<ID3D12PipelineState>);
// SAFETY: D3D12 objects are free-threaded.
unsafe impl Send for Late {}
// SAFETY: as above.
unsafe impl Sync for Late {}

fn late(cell: &'static OnceLock<Late>, pass: &Pass, defines: &str, entry: windows::core::PCSTR) -> Option<&'static ID3D12PipelineState> {
    cell.get_or_init(|| {
        let made = || -> windows::core::Result<ID3D12PipelineState> {
            let code = crate::host::compile(&format!("{defines}{}{SOURCE}", self::defines()), entry, s!("cs_5_1"))?;
            let host = host().ok_or_else(windows::core::Error::empty)?;
            // SAFETY: the pass's root signature is the one inside the shader; a plain creation.
            unsafe {
                let desc = D3D12_COMPUTE_PIPELINE_STATE_DESC {
                    pRootSignature: std::mem::transmute_copy(&pass.root),
                    CS: D3D12_SHADER_BYTECODE { pShaderBytecode: code.as_ptr().cast(), BytecodeLength: code.len() },
                    ..Default::default()
                };
                host.device.CreateComputePipelineState(&desc)
            }
        };
        Late(made().inspect_err(|e| tracing::warn!(error = %e, defines, "a pass of the stand-in left out")).ok())
    })
    .0
    .as_ref()
}

/// `VICTORY_AMBIENT_ONCE`'s forms of the keep and the raise.
fn keep_once(pass: &Pass) -> Option<&'static ID3D12PipelineState> {
    static CELL: OnceLock<Late> = OnceLock::new();
    late(&CELL, pass, "#define KEEP\n#define ONCE\n", s!("keep"))
}

fn raise_once(pass: &Pass) -> Option<&'static ID3D12PipelineState> {
    static CELL: OnceLock<Late> = OnceLock::new();
    late(&CELL, pass, "#define RAISE\n#define ONCE\n", s!("raise"))
}

/// `VICTORY_AMBIENT_SKY_BOUND`'s geometric mean and bound.
fn geo_mean(pass: &Pass) -> Option<&'static ID3D12PipelineState> {
    static CELL: OnceLock<Late> = OnceLock::new();
    late(&CELL, pass, "#define SKY\n", s!("geo_mean"))
}

fn bound_sky(pass: &Pass) -> Option<&'static ID3D12PipelineState> {
    static CELL: OnceLock<Late> = OnceLock::new();
    late(&CELL, pass, "#define SKY\n", s!("bound_sky"))
}

fn make(device: &ID3D12Device, buffer: impl Fn(u64) -> Option<ID3D12Resource>) -> windows::core::Result<Pass> {
    let mut root: Option<ID3D12RootSignature> = None;
    let violet = defines();
    let mut pipeline = |part: &str, entry| -> windows::core::Result<ID3D12PipelineState> {
        let code = crate::host::compile(&format!("#define {part}\n{violet}{SOURCE}"), entry, s!("cs_5_1"))?;
        // SAFETY: the root signature is inside the shader; plain creations.
        unsafe {
            if root.is_none() {
                root = Some(device.CreateRootSignature(0, &code)?);
            }
            let desc = D3D12_COMPUTE_PIPELINE_STATE_DESC {
                pRootSignature: std::mem::transmute_copy(&root),
                CS: D3D12_SHADER_BYTECODE { pShaderBytecode: code.as_ptr().cast(), BytecodeLength: code.len() },
                ..Default::default()
            };
            device.CreateComputePipelineState(&desc)
        }
    };
    let keep = pipeline("KEEP", s!("keep"))?;
    let mean = pipeline("KEEP", s!("mean"))?;
    let place = pipeline("PLACE", s!("place"))?;
    let gather = pipeline("GATHER", s!("gather"))?;
    let raise = pipeline("RAISE", s!("raise"))?;
    let air = pipeline("AIR", s!("clear_air"))?;
    let reflect = match specular() {
        Some(_) => Some(pipeline("REFLECT", s!("reflect_light"))?),
        None => None,
    };
    let mut optional = |on: bool, part: &str, entry| {
        on.then(|| {
            pipeline(part, entry)
                .inspect_err(|e| tracing::warn!(error = %e, part, "a pass of the reflections' stand-in left out"))
                .ok()
        })
        .flatten()
    };
    let bound = optional(specular().is_some() && crate::ambient_reflect::bound(), "BOUND", s!("bound_light"));
    let hold = optional(crate::ambient_reflect::reflect().is_some(), "HOLD", s!("hold"));
    let knee = optional(crate::ambient_reflect::knee().is_some(), "HOLD", s!("knee"));
    let made = |bytes| buffer(bytes).ok_or_else(windows::core::Error::empty);
    Ok(Pass {
        root: root.ok_or_else(windows::core::Error::empty)?,
        keep,
        mean,
        place,
        gather,
        raise,
        air,
        reflect,
        bound,
        hold,
        knee,
        cells: made(CELL_BYTES)?,
        points: made(POINT_BYTES)?,
    })
}

fn pass() -> Option<&'static Pass> {
    static PASS: OnceLock<Option<Pass>> = OnceLock::new();
    PASS.get_or_init(|| {
        let host = host()?;
        make(&host.device, |bytes| host.uav_buffer(bytes))
            .inspect_err(|e| tracing::warn!(error = %e, "no stand-in for the indirect light"))
            .ok()
    })
    .as_ref()
}

fn uav_barrier(list: &ID3D12GraphicsCommandList) {
    let barrier = D3D12_RESOURCE_BARRIER { Type: D3D12_RESOURCE_BARRIER_TYPE_UAV, ..Default::default() };
    // SAFETY: an open list; the barrier names no resource.
    unsafe { list.ResourceBarrier(&[barrier]) };
}

/// Whether the light of a frame has been kept: the estimate has something
/// to begin with.
static KEPT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether the points have been gathered once: what the holds and the
/// stand-in without a depth read of them.
static GATHERED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// What `crate::skin_fill` fills skin's probe inputs with: the cells' buffer (the room's
/// slow mean), the share of it, the constant. None while the diffuse stand-in is off or,
/// for the estimate, before a frame's light has been kept.
pub(crate) fn skin_light() -> Option<crate::skin_fill::Light<'static>> {
    if !on(Role::Resolve) {
        return None;
    }
    let (level, constant) = match set() {
        Set::Off => return None,
        Set::Estimate(level) => (level, 0.0),
        Set::Constant(level) => (0.0, level),
    };
    if constant == 0.0 && !KEPT.load(std::sync::atomic::Ordering::Relaxed) {
        return None;
    }
    Some(crate::skin_fill::Light { cells: &pass()?.cells, level, constant })
}

/// What the passes read and write, as views in the heap the list has set.
pub(crate) struct Views {
    /// The image the title's program just wrote.
    pub(crate) image: D3D12_GPU_DESCRIPTOR_HANDLE,
    /// The scene's depth, for [`Role::Resolve`].
    pub(crate) depth: Option<D3D12_GPU_DESCRIPTOR_HANDLE>,
    /// The size of the image, and how many slices it has.
    pub(crate) size: [u32; 2],
    pub(crate) slices: u32,
    /// The size of the depth (the image's but for [`Role::Miss`]).
    pub(crate) depth_size: [u32; 2],
}

/// After the title's program of that role, in `list` with the program's view
/// heap set. Replaces the list's compute root signature and pipeline; false
/// if it recorded nothing.
pub(crate) fn after(list: &HostList, role: Role, views: &Views) -> bool {
    use std::sync::atomic::Ordering;
    let Some(pass) = pass() else {
        return false;
    };
    // The share of the estimate, and the constant (for reflections: the
    // share of the title's sky kept); the keep runs for either stand-in.
    let (level, constant) = match (role, set(), specular()) {
        (Role::Miss, _, Some(s)) => (s.share, s.sky),
        (Role::Miss, _, None) => return false,
        (Role::Radiance, Set::Off, Some(_)) => (0.0, 0.0),
        (_, Set::Off, _) => return false,
        (_, Set::Estimate(level), _) => (level, 0.0f32),
        (_, Set::Constant(level), _) => (0.0, level),
    };
    let estimate = role == Role::Miss || constant == 0.0;
    if role != Role::Radiance && estimate && !KEPT.load(Ordering::Relaxed) {
        if role == Role::Miss {
            crate::ambient_miss::note(crate::ambient_miss::Note::NotReady);
        }
        return false;
    }
    let reflect = match (role, &pass.reflect) {
        (Role::Miss, None) => return false,
        (_, reflect) => reflect,
    };
    // The holds: their pass, and the multiple of the light around.
    let held = match role {
        Role::Reflect => pass.hold.as_ref().zip(crate::ambient_reflect::reflect()),
        Role::Knee => pass.knee.as_ref().zip(crate::ambient_reflect::knee()),
        _ => None,
    };
    if role.holds_image() && (held.is_none() || estimate && !GATHERED.load(Ordering::Relaxed)) {
        return false;
    }
    if role == Role::Air && (constant > 0.0 || air() == 0.0) {
        return false;
    }
    static SAID: Mutex<u32> = Mutex::new(0);
    {
        let mut said = SAID.lock().unwrap_or_else(|e| e.into_inner());
        if *said < 4 {
            *said += 1;
            tracing::info!(?role, size = ?views.size, level, constant, "a stand-in for the indirect light");
        }
    }
    list.keep(&pass.cells);
    list.keep(&pass.points);
    let [strength, radius] = shade();
    let (follow, jump) = match (role, settle()) {
        (Role::Radiance, Some(s)) => {
            static LAST: Mutex<Option<Instant>> = Mutex::new(None);
            let now = Instant::now();
            let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
            let follow = match last.replace(now) {
                Some(prev) => {
                    let dt_ms = now.saturating_duration_since(prev).as_secs_f32() * 1000.0;
                    (1.0 - (-dt_ms / s.ms).exp()).clamp(0.01, 1.0)
                }
                None => 1.0,
            };
            (follow, s.jump)
        }
        (_, Some(s)) => (0.04, s.jump),
        (_, None) => (0.04, 0.0),
    };
    let floats = [level, constant, strength, radius, 0.1, 0.45];
    let mut words = [0u32; 12];
    for (to, v) in words.iter_mut().zip(floats) {
        *to = v.to_bits();
    }
    words[6] = views.depth_size[0];
    words[7] = views.depth_size[1];
    words[8] = follow.to_bits();
    words[9] = held.map_or_else(most, |h| h.1).to_bits();
    words[10] = air().to_bits();
    words[11] = jump.to_bits();
    let l = &list.list;
    // SAFETY: an open list with the heap of the views set; the title's
    // program has just used both views, so their textures are in the states
    // these views need; buffers the list keeps.
    unsafe {
        uav_barrier(l);
        l.SetComputeRootSignature(&pass.root);
        l.SetComputeRoot32BitConstants(0, 12, words.as_ptr().cast(), 0);
        if let Some(depth) = views.depth {
            l.SetComputeRootDescriptorTable(1, depth);
        }
        l.SetComputeRootDescriptorTable(2, views.image);
        l.SetComputeRootUnorderedAccessView(3, pass.cells.GetGPUVirtualAddress());
        l.SetComputeRootUnorderedAccessView(4, pass.points.GetGPUVirtualAddress());
        let run = |pso: &ID3D12PipelineState, x, y, z| {
            l.SetPipelineState(pso);
            l.Dispatch(x, y, z);
            uav_barrier(l);
        };
        match role {
            Role::Radiance => {
                let keep = if crate::ambient_sky::once() { keep_once(pass) } else { None };
                run(keep.unwrap_or(&pass.keep), 8, 9, 1);
                run(&pass.mean, 1, 1, 1);
                if crate::ambient_sky::sky_bound().is_some()
                    && let Some(geo) = geo_mean(pass)
                {
                    run(geo, 1, 1, 1);
                }
                KEPT.store(true, Ordering::Relaxed);
            }
            Role::Air => run(&pass.air, views.size[0].div_ceil(8), views.size[1].div_ceil(8), views.slices.div_ceil(2)),
            Role::Resolve => {
                if constant == 0.0 {
                    run(&pass.place, 8, 9, 1);
                    run(&pass.gather, 20, 45, 1);
                    GATHERED.store(true, Ordering::Relaxed);
                }
                let raise = if crate::ambient_sky::once() { raise_once(pass) } else { None };
                run(raise.unwrap_or(&pass.raise), views.size[0].div_ceil(8), views.size[1].div_ceil(8), 1);
            }
            Role::Miss => {
                // The points afresh, from this frame's depth: the diffuse
                // stand-in makes them later in the frame.
                run(&pass.place, 8, 9, 1);
                run(&pass.gather, 20, 45, 1);
                GATHERED.store(true, Ordering::Relaxed);
                if let Some(reflect) = reflect {
                    run(reflect, views.size[0].div_ceil(8), views.size[1].div_ceil(8), 1);
                }
            }
            Role::Reflect | Role::Knee => {
                if let Some((pso, _)) = held {
                    run(pso, views.size[0].div_ceil(8), views.size[1].div_ceil(8), 1);
                }
            }
        }
    }
    true
}

/// (trustno1-coder/VIctory-NVIDIA#124) After a dispatch of the miss program
/// whose tables hold the rays' image (`views.image`, R11G11B10) but no depth
/// of the picture, in `list` with the program's view heap set
/// (`VICTORY_AMBIENT_SPECULAR_BOUND=1`): each texel takes the light the
/// points gathered around the surface seen there (by its place on screen).
/// Replaces the list's compute root signature and pipeline; false if it
/// recorded nothing (off, or no frame's light kept and gathered yet).
pub(crate) fn without_depth(list: &HostList, views: &Views) -> bool {
    use std::sync::atomic::Ordering;
    let Some(Specular { share, sky }) = specular() else {
        return false;
    };
    if !KEPT.load(Ordering::Relaxed) || !GATHERED.load(Ordering::Relaxed) {
        return false;
    }
    let Some((pass, bound)) = pass().and_then(|p| Some((p, p.bound.as_ref()?))) else {
        return false;
    };
    static SAID: std::sync::Once = std::sync::Once::new();
    SAID.call_once(|| {
        tracing::info!(size = ?views.size, share, sky, "a stand-in for reflections without the scene's depth (VICTORY_AMBIENT_SPECULAR_BOUND=1)");
    });
    list.keep(&pass.cells);
    list.keep(&pass.points);
    let mut words = [0u32; 12];
    words[0] = share.to_bits();
    words[1] = sky.to_bits();
    let l = &list.list;
    // SAFETY: an open list with the heap of the view set; the title's program
    // has just written the image through this view, so its texture is in the
    // state the view needs; the pass reads no depth; buffers the list keeps.
    unsafe {
        uav_barrier(l);
        l.SetComputeRootSignature(&pass.root);
        l.SetComputeRoot32BitConstants(0, 12, words.as_ptr().cast(), 0);
        l.SetComputeRootDescriptorTable(2, views.image);
        l.SetComputeRootUnorderedAccessView(3, pass.cells.GetGPUVirtualAddress());
        l.SetComputeRootUnorderedAccessView(4, pass.points.GetGPUVirtualAddress());
        l.SetPipelineState(bound);
        l.Dispatch(views.size[0].div_ceil(8), views.size[1].div_ceil(8), 1);
        uav_barrier(l);
    }
    true
}

/// (wasonidas, trustno1-coder/VIctory-NVIDIA#63; tippythetaidum #393) After a
/// dispatch of the miss program, in `list` with the program's view heap set
/// (`VICTORY_AMBIENT_SKY_BOUND=<multiple>`): each texel of the rays' image
/// (`views.image`, R11G11B10) keeps what is in it, the title's sky light and
/// whatever an earlier pass put there, up to `multiple` times the geometric
/// mean of the picture's light. Reads no depth. Replaces the list's compute
/// root signature and pipeline; false if it recorded nothing (no frame's
/// light kept yet, or the pass not made).
pub(crate) fn sky_bound(list: &HostList, views: &Views, multiple: f32) -> bool {
    use std::sync::atomic::Ordering;
    if !KEPT.load(Ordering::Relaxed) {
        return false;
    }
    let Some((pass, bound)) = pass().and_then(|p| Some((p, bound_sky(p)?))) else {
        return false;
    };
    static SAID: std::sync::Once = std::sync::Once::new();
    SAID.call_once(|| {
        tracing::info!(size = ?views.size, multiple, "reflections: the title's sky bounded by the picture's light (VICTORY_AMBIENT_SKY_BOUND)");
    });
    list.keep(&pass.cells);
    list.keep(&pass.points);
    let mut words = [0u32; 12];
    words[0] = multiple.to_bits();
    let l = &list.list;
    // SAFETY: an open list with the heap of the view set; the title's program
    // has just written the image through this view, so its texture is in the
    // state the view needs; the pass reads no depth; buffers the list keeps.
    unsafe {
        uav_barrier(l);
        l.SetComputeRootSignature(&pass.root);
        l.SetComputeRoot32BitConstants(0, 12, words.as_ptr().cast(), 0);
        l.SetComputeRootDescriptorTable(2, views.image);
        l.SetComputeRootUnorderedAccessView(3, pass.cells.GetGPUVirtualAddress());
        l.SetComputeRootUnorderedAccessView(4, pass.points.GetGPUVirtualAddress());
        l.SetPipelineState(bound);
        l.Dispatch(views.size[0].div_ceil(8), views.size[1].div_ceil(8), 1);
        uav_barrier(l);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_constants_are_live() {
        use victory_core::settings::check_live;
        check_live("VICTORY_AMBIENT", "0.5", set, Set::Off, Set::Estimate(0.5));
        check_live("VICTORY_AMBIENT_AIR", "2", air, 0.0, 2.0);
        check_live("VICTORY_AMBIENT_MOST", "1.5", most, 4.0, 1.5);
        check_live("VICTORY_AMBIENT_SETTLE", "100,2", settle, Some(Settle { ms: 250.0, jump: 3.0 }), Some(Settle { ms: 100.0, jump: 2.0 }));
        check_live("VICTORY_AMBIENT_SHADE", "0.5", shade, [1.0, 0.6], [0.5, 0.6]);
        check_live("VICTORY_AMBIENT_SPECULAR_ALPHA", "0", specular_alpha, true, false);
    }

    /// The builder on by its own switches: every stand-in steps aside. With
    /// `VICTORY_RAY_TRACING=on` (reflections walk, probes miss): the
    /// reflection stand-ins step aside, the diffuse one stays.
    #[test]
    fn stand_ins_step_aside_where_the_rays_find_something() {
        let all = [Role::Resolve, Role::Radiance, Role::Air, Role::Miss, Role::Reflect, Role::Knee];
        for role in all {
            assert!(rays_find(role, false, false, false), "{role:?}");
            assert!(rays_find(role, true, true, true), "{role:?}");
            assert!(!rays_find(role, true, false, false), "{role:?}");
        }
        for role in [Role::Miss, Role::Reflect, Role::Knee] {
            assert!(rays_find(role, true, true, false), "{role:?}");
        }
        for role in [Role::Resolve, Role::Radiance, Role::Air] {
            assert!(!rays_find(role, true, true, false), "{role:?}");
        }
    }

    #[test]
    fn settings() {
        assert_eq!(parse(""), Set::Off);
        assert_eq!(parse("off"), Set::Off);
        assert_eq!(parse("0"), Set::Off);
        assert_eq!(parse("0.8"), Set::Estimate(0.8));
        assert_eq!(parse("constant:0.3"), Set::Constant(0.3));
        assert_eq!(parse("constant:nan"), Set::Off);
    }

    #[test]
    fn settle_settings() {
        assert_eq!(parse_settle("250,3"), Some(Settle { ms: 250.0, jump: 3.0 }));
        assert_eq!(parse_settle("100"), Some(Settle { ms: 100.0, jump: 3.0 }));
        assert_eq!(parse_settle("500,0"), Some(Settle { ms: 500.0, jump: 0.0 }));
        assert_eq!(parse_settle("0"), None);
        assert_eq!(parse_settle("off"), None);
    }

    #[test]
    fn specular_settings() {
        assert_eq!(parse_specular(""), None);
        assert_eq!(parse_specular("0"), None);
        assert_eq!(parse_specular("off"), None);
        assert_eq!(parse_specular("1"), Some(Specular { share: 1.0, sky: 0.0 }));
        assert_eq!(parse_specular("0.6, 0.25"), Some(Specular { share: 0.6, sky: 0.25 }));
        assert_eq!(parse_specular("0.6,7"), Some(Specular { share: 0.6, sky: 1.0 }));
        assert_eq!(parse_specular("0.6,x"), Some(Specular { share: 0.6, sky: 0.0 }));
    }

    #[test]
    fn miss_programs_by_name() {
        assert!(is_miss(b"..CS_RayMiss_de6b91a6_Wrapped.."));
        assert!(!is_miss(b"..CS_RayMiss_Volumetrics_3c5d8c00_Wrapped.."));
        assert!(!is_miss(b"..CS_ScreenProbe_RayMiss_80a88544_Wrapped.."));
        // Whatever the switches (the sky bound is live): `crate::draw`
        // `miss_views` does nothing after it while both are off.
        assert_eq!(role(b"..CS_RayMiss_de6b91a6_Wrapped.."), Some(Role::Miss));
        assert_eq!(role(b"..CS_RayMiss_Volumetrics_3c5d8c00_Wrapped.."), None);
    }

    /// The source without the reflections' passes: what it was before.
    fn source_before() -> String {
        let mut source = SOURCE.to_owned();
        for block in ["#ifdef REFLECT", "#ifdef BOUND", "#ifdef SKY", "#ifdef HOLD"] {
            let from = source.find(block).expect("the reflections' pass");
            let to = from + source[from..].find("#endif\n").expect("its end") + "#endif\n".len();
            source = format!("{}{}", &source[..from], &source[to..]);
        }
        // `crate::ambient_sky`'s parts of the keep and the raise: what is
        // left without their defines (an `#else`'s text, else nothing).
        for block in ["#ifdef ONCE\n", "#ifdef CHROMA\n"] {
            while let Some(from) = source.find(block) {
                let end = from + source[from..].find("#endif\n").expect("its end");
                let inner = &source[from + block.len()..end];
                let kept = inner.find("#else\n").map_or("", |e| &inner[e + "#else\n".len()..]).to_owned();
                source = format!("{}{kept}{}", &source[..from], &source[end + "#endif\n".len()..]);
            }
        }
        source
    }

    /// The passes of `crate::ambient_reflect` compile, and the host takes them.
    #[test]
    fn the_holds_build() {
        for (part, entry) in [("BOUND", s!("bound_light")), ("HOLD", s!("hold")), ("HOLD", s!("knee"))] {
            let code = crate::host::compile(&format!("#define {part}\n{SOURCE}"), entry, s!("cs_5_1"))
                .unwrap_or_else(|e| panic!("{part}: {e}"));
            let Some(host) = host() else {
                continue;
            };
            // SAFETY: the root signature is inside the shader; plain creations.
            unsafe {
                let root: ID3D12RootSignature = host.device.CreateRootSignature(0, &code).expect("its root signature");
                let desc = D3D12_COMPUTE_PIPELINE_STATE_DESC {
                    pRootSignature: std::mem::transmute_copy(&root),
                    CS: D3D12_SHADER_BYTECODE { pShaderBytecode: code.as_ptr().cast(), BytecodeLength: code.len() },
                    ..Default::default()
                };
                let _: ID3D12PipelineState = host.device.CreateComputePipelineState(&desc).expect("its pipeline");
            }
        }
    }

    /// Every pass that was there compiles to the same bytes with the
    /// reflections' pass in the source (with and without
    /// `VICTORY_AMBIENT_VIOLET`): switched off, nothing of ours changes.
    #[test]
    fn the_other_passes_are_unchanged() {
        let before = source_before();
        assert!(!before.contains("reflect_light") && !before.contains("bound_light") && !before.contains("void hold"));
        assert!(!before.contains("bound_sky") && !before.contains("cells[SHARES") && !before.contains("0.35 * min"));
        let parts = [
            ("KEEP", s!("keep")),
            ("KEEP", s!("mean")),
            ("PLACE", s!("place")),
            ("GATHER", s!("gather")),
            ("RAISE", s!("raise")),
            ("AIR", s!("clear_air")),
        ];
        for violet in ["", "#define VIOLET\n"] {
            for (part, entry) in parts {
                let made = |source: &str| {
                    crate::host::compile(&format!("#define {part}\n{violet}{source}"), entry, s!("cs_5_1"))
                        .expect("the pass")
                };
                assert_eq!(made(SOURCE), made(&before), "{part} {violet:?}");
            }
        }
    }

    /// The reflections' pass compiles, and the host takes it.
    #[test]
    fn the_reflections_pass_builds() {
        let code = crate::host::compile(&format!("#define REFLECT\n{SOURCE}"), s!("reflect_light"), s!("cs_5_1"))
            .expect("the reflections' pass");
        let Some(host) = host() else {
            return;
        };
        // SAFETY: the root signature is inside the shader; plain creations.
        unsafe {
            let root: ID3D12RootSignature = host.device.CreateRootSignature(0, &code).expect("its root signature");
            let desc = D3D12_COMPUTE_PIPELINE_STATE_DESC {
                pRootSignature: std::mem::transmute_copy(&root),
                CS: D3D12_SHADER_BYTECODE { pShaderBytecode: code.as_ptr().cast(), BytecodeLength: code.len() },
                ..Default::default()
            };
            let _: ID3D12PipelineState = host.device.CreateComputePipelineState(&desc).expect("its pipeline");
        }
    }

    #[test]
    fn roles_by_name() {
        assert_eq!(role(b"..CS_ScreenProbes_TemporalResolve_a911b49c_Wrapped.."), Some(Role::Resolve));
        assert_eq!(role(b"..CS_CacheScreenSpaceRadiance_eed2fc35.."), Some(Role::Radiance));
        assert_eq!(role(b"CS_AccumulateLocalLightInscatteringVolume_7e2f5eef"), Some(Role::Air));
        assert_eq!(role(b"CS_TemporalResolve"), None);
    }

    /// `VICTORY_AMBIENT_VIOLET=1`'s form of the light-raising pass compiles, and differs.
    #[test]
    fn the_violet_drop_compiles() {
        let raise = |violet: &str| {
            crate::host::compile(&format!("#define RAISE\n{violet}{SOURCE}"), s!("raise"), s!("cs_5_1"))
                .expect("the raising pass")
        };
        assert_ne!(raise(""), raise("#define VIOLET\n"));
    }

    /// `crate::ambient_sky`'s passes compile, the host takes them, and the
    /// keep, the raise and the raise's scrub differ from their plain forms.
    #[test]
    fn the_sky_passes_build() {
        let made = |defines: &str, entry| {
            crate::host::compile(&format!("{defines}{SOURCE}"), entry, s!("cs_5_1"))
                .unwrap_or_else(|e| panic!("{defines}: {e}"))
        };
        assert_ne!(made("#define KEEP\n", s!("keep")), made("#define KEEP\n#define ONCE\n", s!("keep")));
        assert_ne!(made("#define RAISE\n", s!("raise")), made("#define RAISE\n#define ONCE\n", s!("raise")));
        assert_ne!(made("#define RAISE\n", s!("raise")), made("#define RAISE\n#define CHROMA\n", s!("raise")));
        made("#define RAISE\n#define ONCE\n#define CHROMA\n#define VIOLET\n", s!("raise"));
        made("#define SKY\n", s!("geo_mean"));
        let Some(pass) = pass() else {
            return;
        };
        assert!(keep_once(pass).is_some() && raise_once(pass).is_some());
        assert!(geo_mean(pass).is_some() && bound_sky(pass).is_some());
    }

    /// The passes compile with the system's compiler and the host takes them.
    #[test]
    fn the_passes_build() {
        let Some(host) = host() else {
            return;
        };
        make(&host.device, |bytes| host.uav_buffer(bytes)).expect("the passes");
    }
}
