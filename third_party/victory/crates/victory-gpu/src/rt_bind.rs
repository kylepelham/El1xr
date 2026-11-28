//! `VICTORY_NATIVE_RT_BIND=1` (off by default; with `VICTORY_BVH_HOOK=1`
//! and `VICTORY_NATIVE_BVH=1`): each dispatch of a program that walks the
//! ray-tracing structures (`CS_RaytraceVBuffer*`, the BVH traces) is
//! matched to the host top level `crate::native_rt` built for the same
//! title structure. This counts, every 5 s, the dispatches a hardware
//! replacement could serve (a host top level is there) and the ones that
//! would fall back to the title's own program.
//!
//! The title names its top level to a trace program through a buffer view
//! in one of its descriptor tables: a ray-tracing structure view (dimension
//! 11), whose `Location` is the structure's title address (after fxceless
//! victory-fork `9a9091d180`, GPL-3.0-or-later). In the glass trace
//! `6d7fe7745ab9c6d8` that is the view at dword 8 of root parameter 1
//! (`t355`): the program reads the structure's header at its address + 48
//! and starts its walk at the address shifted right by 8. That address is
//! what `crate::native_rt` keys its structures by (the build's
//! destination), so the two meet without the program's help. The glass
//! trace also walks a second structure, the view at dword 32 (`t354`), for
//! the rays whose offset is short (its near walk): both are matched and
//! counted apart (`glass_*` for `t355`, `glass354_*`).
//!
//! `VICTORY_NATIVE_GLASS=1` (off by default; needs `VICTORY_NATIVE_BVH=1`)
//! also matches without `VICTORY_NATIVE_RT_BIND` and keeps the glass
//! dispatch's two host top levels for its root SRVs
//! (`victory_shader::emit::SLOT_TLAS`, `SLOT_TLAS_NEAR`): what [`dispatch`]
//! returns. `VICTORY_NATIVE_SHADOW=1` does the same for the shadow traces the
//! shadow splice knows (`victory_shader::emit::shadow_structures`): the host
//! top levels of their two structure views (t355, t356 for the distant
//! traces; the one view, in both, for Checkerboard), for `SLOT_TLAS` and
//! `SLOT_TLAS_NEAR` (counted as `shadow_*`, one count a view looked up). The
//! TraceShadowRays variants pick their top level per wave from the resource
//! heap (t357 or t353): the host binds the candidates it built (one is
//! enough) and gives the shader their title addresses >> 8 ([`Bound::bases`]);
//! a wave whose pick matches neither takes the title's walk. Each variant's
//! candidates are logged (`native rt bind: shadow variant`, first few) and
//! counted (`shadow_variant_*`).

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::descriptor::View;
use crate::object::This;

/// Whether dispatches are matched (`VICTORY_NATIVE_RT_BIND=1`,
/// `VICTORY_NATIVE_GLASS=1` or `VICTORY_NATIVE_SHADOW=1`).
pub(crate) fn on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var_os("VICTORY_NATIVE_RT_BIND").is_some_and(|v| v == "1")
            || victory_shader::emit::native_glass()
            || victory_shader::emit::native_shadow()
    })
}

/// The glass trace's structure registers: its far walk's (`t355`, the one
/// a dispatch is counted by) and its near walk's (`t354`).
const GLASS_FAR: u32 = 355;
const GLASS_NEAR: u32 = 354;

/// `D3D12_SRV_DIMENSION_RAYTRACING_ACCELERATION_STRUCTURE`.
const STRUCTURE: u32 = 11;

/// The title address a ray-tracing structure view names: the description's
/// `RaytracingAccelerationStructure.Location` (words 4-5, after the format,
/// dimension, component mapping and padding). `None` for any other view.
pub(crate) fn structure_address(view: &View) -> Option<u64> {
    let words = match view {
        View::Shader(_, Some(desc)) => &desc[..],
        View::Placed(false, _, desc) => &desc[..10],
        _ => return None,
    };
    (words[1] == STRUCTURE)
        .then(|| u64::from(words[4]) | u64::from(words[5]) << 32)
        .filter(|&at| at != 0)
}

/// What a dispatch found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Found {
    /// A structure view naming a top level the host built.
    Host,
    /// A structure view, no host top level for its address.
    NoHost,
    /// No structure view among the program's table views.
    NoView,
}

/// Counts since the last summary: [glass `t355`, glass `t354`, other
/// VBuffer, other traces, shadow traces] × [host, no host, no view].
static COUNTS: [[AtomicU64; 3]; 5] = [const { [const { AtomicU64::new(0) }; 3] }; 5];
/// Per-wave shadow variant dispatches since the last summary, by how many of
/// their two candidates have a host top level: [none, one, both].
static VARIANTS: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

/// What a matched dispatch binds: the host top levels for `SLOT_TLAS` and
/// `SLOT_TLAS_NEAR` (0: none), and for a shadow trace that picks per wave the
/// title addresses >> 8 (low 32 bits) of the candidates with a host top level
/// (0 for one without), for `USER_SHADOW_BASE_FIRST`/`_SECOND`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Bound {
    pub host: [u64; 2],
    pub bases: [u32; 2],
}

/// The bases of a per-wave variant: a candidate's address >> 8 where the host
/// built its top level.
fn bases(titles: [Option<u64>; 2], host: [u64; 2]) -> [u32; 2] {
    std::array::from_fn(|k| match titles[k] {
        Some(at) if host[k] != 0 => (at >> 8) as u32,
        _ => 0,
    })
}

/// The first few distinct candidate sets of the per-wave variants, logged.
fn log_variant(hash: &str, registers: [Option<u32>; 2], titles: [Option<u64>; 2], host: [u64; 2]) {
    static SEEN: Mutex<Vec<(&'static str, [Option<u64>; 2])>> = Mutex::new(Vec::new());
    let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    let key = (victory_shader::emit::shadow_binding(hash).map_or("", |b| b.hash), titles);
    if seen.len() >= 16 || seen.contains(&key) {
        return;
    }
    seen.push(key);
    let t = |k: usize| format!("t{}", registers[k].unwrap_or(0));
    let at = |k: usize| titles[k].map_or("none".to_owned(), |a| format!("{a:#x}"));
    tracing::info!(
        variant = hash,
        first = format_args!("{} {} host={}", t(0), at(0), host[0] != 0),
        second = format_args!("{} {} host={}", t(1), at(1), host[1] != 0),
        "native rt bind: shadow variant candidates (a wave whose picked top level matches neither takes the title's walk)"
    );
}

/// The first title addresses that found no host top level (a few, for the
/// log: whether they are near what was built tells a wrong key from a
/// missing build).
static MISSED: Mutex<Vec<u64>> = Mutex::new(Vec::new());

/// The first structure address among `views` (root parameter, dword,
/// register) whose register `register` allows (any with `None`),
/// `view(slot, dword)` giving the title's view at that dword of the table at
/// root parameter `slot`.
fn find_structure(
    views: &[(u8, u32, u32)],
    register: Option<u32>,
    view: impl Fn(u8, u32) -> Option<View>,
) -> Option<u64> {
    views
        .iter()
        .filter(|v| register.is_none_or(|r| r == v.2))
        .find_map(|&(slot, dword, _)| view(slot, dword).as_ref().and_then(structure_address))
}

fn classify(address: Option<u64>, host: impl Fn(u64) -> Option<u64>) -> Found {
    match address {
        None => Found::NoView,
        Some(at) if host(at).is_some() => Found::Host,
        Some(_) => Found::NoHost,
    }
}

/// A dispatch of `this` (one hook in `crate::draw`); `table(slot)` gives
/// the first descriptor of the table bound at compute root parameter `slot`.
/// For the glass trace with both structures matched: their host top levels
/// (`t355`, `t354`), for root SRVs (`SLOT_TLAS`, `SLOT_TLAS_NEAR`); for a
/// known shadow trace with its structures matched: their host top levels (a
/// per-wave variant: those matched, one is enough, and their bases); `None`
/// otherwise, when the dispatch takes the title's own walk.
pub(crate) fn dispatch(
    this: This,
    alpha: bool,
    table: impl Fn(u8) -> Option<u64>,
) -> Option<Bound> {
    if !on() {
        return None;
    }
    use crate::pipeline::Walks;
    let (kind, views) = crate::pipeline::ray_views(this)?;
    let glass = kind == Walks::Glass;
    let view = |slot: u8, dword: u32| crate::descriptor::view(table(slot)? + u64::from(dword) * 4);
    if let Walks::Shadow(b) = kind
        && b.any
    {
        return auto(b, &views, view);
    }
    let mut matched = [0u64; 2];
    // (register, count row, the top levels it is matched for: 0 first, 1 second, 2 both).
    let walks: &[(Option<u32>, usize, u8)] = match kind {
        Walks::Glass => &[(Some(GLASS_FAR), 0, 0), (Some(GLASS_NEAR), 1, 1)],
        Walks::VBuffer => &[(None, 2, 3)],
        Walks::Trace => &[(None, 3, 3)],
        Walks::Shadow(b) if b.registers[0] == b.registers[1] => &[(b.registers[0], 4, 2)],
        Walks::Shadow(b) => &[(b.registers[0], 4, 0), (b.registers[1], 4, 1)],
    };
    let mut titles = [None; 2];
    for &(register, row, to) in walks {
        let address = find_structure(&views, register, view);
        let host = address.and_then(crate::native_rt::top_level);
        let found = classify(address, |_| host);
        if let (Found::NoHost, Some(at)) = (found, address) {
            let mut missed = MISSED.lock().unwrap_or_else(|e| e.into_inner());
            if missed.len() < 4 && !missed.contains(&at) {
                missed.push(at);
                tracing::info!(
                    address = format_args!("{at:#x}"),
                    glass,
                    register,
                    alpha,
                    "native rt bind: a trace names a structure the host did not build"
                );
            }
        }
        match to {
            0 | 1 => {
                matched[usize::from(to)] = host.unwrap_or(0);
                titles[usize::from(to)] = address;
            }
            2 => matched = [host.unwrap_or(0); 2],
            _ => {}
        }
        COUNTS[row][found as usize].fetch_add(1, Ordering::Relaxed);
    }
    summary();
    let bound = |host| Bound { host, bases: [0; 2] };
    match kind {
        Walks::Shadow(b) if b.per_wave => {
            let built = matched.iter().filter(|&&m| m != 0).count();
            VARIANTS[built].fetch_add(1, Ordering::Relaxed);
            log_variant(b.hash, b.registers, titles, matched);
            (built > 0).then(|| Bound { host: matched, bases: bases(titles, matched) })
        }
        Walks::Glass | Walks::Shadow(_) => both(matched).map(bound),
        Walks::VBuffer | Walks::Trace => None,
    }
}

/// The first two distinct structure addresses among `views`.
fn distinct_structures(
    views: &[(u8, u32, u32)],
    view: impl Fn(u8, u32) -> Option<View>,
) -> Vec<u64> {
    let mut found = Vec::new();
    for &(slot, dword, _) in views {
        if let Some(at) = view(slot, dword).as_ref().and_then(structure_address)
            && !found.contains(&at)
        {
            found.push(at);
            if found.len() == 2 {
                break;
            }
        }
    }
    found
}

/// A program the automatic splice changed (`VICTORY_NATIVE_SHADOW_AUTO=1`): its candidates are
/// its first two distinct structures. Walks that pick per wave compare their base with the
/// candidates on the card, so any are safe; a walk that does not runs on the first host top
/// level, so such a program is bound only with exactly one structure.
fn auto(
    b: victory_shader::emit::ShadowBinding,
    views: &[(u8, u32, u32)],
    view: impl Fn(u8, u32) -> Option<View>,
) -> Option<Bound> {
    let found = distinct_structures(views, view);
    let mut titles = [None; 2];
    let mut matched = [0u64; 2];
    for (k, &at) in found.iter().enumerate() {
        titles[k] = Some(at);
        matched[k] = crate::native_rt::top_level(at).unwrap_or(0);
    }
    let row = match (found.len(), matched[0] != 0 || matched[1] != 0) {
        (0, _) => Found::NoView,
        (_, true) => Found::Host,
        (_, false) => Found::NoHost,
    };
    COUNTS[4][row as usize].fetch_add(1, Ordering::Relaxed);
    summary();
    let bound = auto_bound(b.per_wave, titles, matched);
    log_auto(b.per_wave, titles, matched, bound.is_some());
    bound
}

/// The first few distinct candidate sets of automatically spliced programs, logged.
fn log_auto(per_wave: bool, titles: [Option<u64>; 2], host: [u64; 2], bound: bool) {
    static SEEN: Mutex<Vec<[Option<u64>; 2]>> = Mutex::new(Vec::new());
    let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    if seen.len() >= 16 || seen.contains(&titles) {
        return;
    }
    seen.push(titles);
    let at = |k: usize| titles[k].map_or("none".to_owned(), |a| format!("{a:#x} host={}", host[k] != 0));
    tracing::info!(
        per_wave,
        first = %at(0),
        second = %at(1),
        bound,
        "native rt bind: auto-spliced shadow walks (not bound: the title's walk)"
    );
}

/// What [`auto`] binds, from the candidates' title addresses and host top levels.
fn auto_bound(per_wave: bool, titles: [Option<u64>; 2], matched: [u64; 2]) -> Option<Bound> {
    if per_wave {
        let built = matched.iter().filter(|&&m| m != 0).count();
        VARIANTS[built].fetch_add(1, Ordering::Relaxed);
        return (built > 0).then(|| Bound { host: matched, bases: bases(titles, matched) });
    }
    (titles[1].is_none() && matched[0] != 0)
        .then(|| Bound { host: [matched[0]; 2], bases: [0; 2] })
}

fn both(m: [u64; 2]) -> Option<[u64; 2]> {
    (m[0] != 0 && m[1] != 0).then_some(m)
}

/// Every 5 s: the dispatches matched since.
fn summary() {
    static LAST: Mutex<Option<std::time::Instant>> = Mutex::new(None);
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    let now = std::time::Instant::now();
    if last.is_some_and(|t| now.duration_since(t).as_secs() < 5) {
        return;
    }
    *last = Some(now);
    drop(last);
    let take = |row: usize| -> [u64; 3] {
        std::array::from_fn(|i| COUNTS[row][i].swap(0, Ordering::Relaxed))
    };
    let [glass, near, vbuffer, trace, shadow] = [take(0), take(1), take(2), take(3), take(4)];
    tracing::info!(
        glass_host = glass[0],
        glass_fallback = glass[1] + glass[2],
        glass_no_view = glass[2],
        glass354_host = near[0],
        glass354_fallback = near[1] + near[2],
        glass354_no_view = near[2],
        vbuffer_host = vbuffer[0],
        vbuffer_fallback = vbuffer[1] + vbuffer[2],
        trace_host = trace[0],
        trace_fallback = trace[1] + trace[2],
        trace_no_view = trace[2],
        shadow_host = shadow[0],
        shadow_fallback = shadow[1] + shadow[2],
        shadow_no_view = shadow[2],
        shadow_variant_none = VARIANTS[0].swap(0, Ordering::Relaxed),
        shadow_variant_one = VARIANTS[1].swap(0, Ordering::Relaxed),
        shadow_variant_both = VARIANTS[2].swap(0, Ordering::Relaxed),
        tops_kept = crate::native_rt::tops_kept(),
        "native rt bind: trace dispatches in the last 5 s (host: a host top level is there; fallback: the title's program)"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rt_view(at: u64) -> View {
        let mut desc = [0u32; 10];
        desc[1] = STRUCTURE;
        desc[4] = at as u32;
        desc[5] = (at >> 32) as u32;
        View::Shader(0, Some(desc))
    }

    #[test]
    fn structure_views_name_their_location() {
        assert_eq!(
            structure_address(&rt_view(0x12_3456_7800)),
            Some(0x12_3456_7800)
        );
        // A null structure view names nothing.
        assert_eq!(structure_address(&rt_view(0)), None);
        // A buffer view (dimension 1) is not a structure.
        let mut buffer = [0u32; 10];
        buffer[1] = 1;
        buffer[4] = 0x1000;
        assert_eq!(structure_address(&View::Shader(0, Some(buffer))), None);
        assert_eq!(structure_address(&View::Shader(0, None)), None);
        assert_eq!(structure_address(&View::Constant(0x1000, 256)), None);
        // A view of placed memory: the plain description comes first.
        let mut placed = [0u32; 16];
        placed[1] = STRUCTURE;
        placed[4] = 0x4000;
        assert_eq!(
            structure_address(&View::Placed(false, [0; 14], placed)),
            Some(0x4000)
        );
        assert_eq!(
            structure_address(&View::Placed(true, [0; 14], placed)),
            None
        );
    }

    #[test]
    fn the_first_structure_view_is_the_one() {
        // The glass trace's table views: the structure at dword 8 of root 1.
        let views = [(1, 0, 400), (1, 8, 355), (1, 16, 356), (4, 8, 346)];
        let found = find_structure(&views, None, |slot, dword| match (slot, dword) {
            (1, 8) => Some(rt_view(0xabc00)),
            (4, 8) => Some(rt_view(0xdef00)),
            (1, 0) => Some(View::Constant(0x10, 16)),
            _ => None,
        });
        assert_eq!(found, Some(0xabc00));
        assert_eq!(find_structure(&views, None, |_, _| None), None);
    }

    #[test]
    fn the_glass_trace_names_two_structures_by_register() {
        // t355 at dword 8 and t354 at dword 32 of root parameter 1, as the
        // glass trace 6d7fe7745ab9c6d8 declares them.
        let views = [(1, 8, GLASS_FAR), (1, 32, GLASS_NEAR), (0, 8, 346)];
        let view = |slot, dword| match (slot, dword) {
            (1, 8) => Some(rt_view(0xa000)),
            (1, 32) => Some(rt_view(0xb000)),
            _ => None,
        };
        assert_eq!(find_structure(&views, Some(GLASS_FAR), view), Some(0xa000));
        assert_eq!(find_structure(&views, Some(GLASS_NEAR), view), Some(0xb000));
        assert_eq!(find_structure(&views, Some(999), view), None);
        // Bound only when both have a host top level.
        assert_eq!(both([0x10, 0x20]), Some([0x10, 0x20]));
        assert_eq!(both([0x10, 0]), None);
        assert_eq!(both([0, 0x20]), None);
    }

    #[test]
    fn host_fallback_and_no_view() {
        let host = |at: u64| (at == 0xabc00).then_some(0x5000);
        assert_eq!(classify(Some(0xabc00), host), Found::Host);
        assert_eq!(classify(Some(0xdef00), host), Found::NoHost);
        assert_eq!(classify(None, host), Found::NoView);
    }

    #[test]
    fn off_by_default() {
        assert!(!on());
    }

    #[test]
    fn an_automatic_splice_binds_its_first_two_structures() {
        let views = [(1, 0, 400), (1, 8, 12), (1, 16, 13), (2, 8, 14), (4, 8, 15)];
        let view = |slot, dword| match (slot, dword) {
            (1, 8) | (2, 8) => Some(rt_view(0xa000)),
            (1, 16) => Some(rt_view(0xb000)),
            (4, 8) => Some(rt_view(0xc000)),
            (1, 0) => Some(View::Constant(0x10, 16)),
            _ => None,
        };
        assert_eq!(distinct_structures(&views, view), vec![0xa000, 0xb000]);
        assert_eq!(distinct_structures(&views, |_, _| None), Vec::<u64>::new());
        let two = [Some(0xa000), Some(0xb000)];
        // Per wave: the built candidates and their bases; none built, not bound.
        assert_eq!(
            auto_bound(true, two, [0x10, 0]),
            Some(Bound { host: [0x10, 0], bases: [0xa0, 0] })
        );
        assert_eq!(auto_bound(true, two, [0, 0]), None);
        // A fixed walk: only with one structure, and it built.
        assert_eq!(auto_bound(false, two, [0x10, 0x20]), None);
        assert_eq!(
            auto_bound(false, [Some(0xa000), None], [0x10, 0]),
            Some(Bound { host: [0x10, 0x10], bases: [0; 2] })
        );
        assert_eq!(auto_bound(false, [Some(0xa000), None], [0, 0]), None);
    }

    #[test]
    fn a_per_wave_variant_gives_the_bases_of_the_top_levels_built() {
        let titles = [Some(0x12_3456_7800), Some(0x9a_bcde_f000)];
        assert_eq!(bases(titles, [0x5000, 0x6000]), [0x1234_5678, 0x9abc_def0]);
        assert_eq!(bases(titles, [0, 0x6000]), [0, 0x9abc_def0]);
        assert_eq!(bases([None, Some(0x9a_bcde_f000)], [0x5000, 0x6000])[0], 0);
    }
}
