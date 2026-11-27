//! `VICTORY_NATIVE_RT_BIND=1` (off by default; with `VICTORY_BVH_HOOK=1`
//! and `VICTORY_NATIVE_BVH=1`): each dispatch of a program that walks the
//! ray-tracing structures (`CS_RaytraceVBuffer*`, the BVH traces) is
//! matched to the host top level `crate::native_rt` built for the same
//! title structure. Nothing is bound or changed yet: this counts, every
//! 5 s, the dispatches a hardware replacement could serve (a host top level
//! is there) and the ones that would fall back to the title's own program.
//!
//! The title names its top level to a trace program through a buffer view
//! in one of its descriptor tables: a ray-tracing structure view (dimension
//! 11), whose `Location` is the structure's title address (after fxceless
//! victory-fork `9a9091d180`, GPL-3.0-or-later). In the glass trace
//! `6d7fe7745ab9c6d8` that is the view at dword 8 of root parameter 1
//! (`t355`): the program reads the structure's header at its address + 48
//! and starts its walk at the address shifted right by 8. That address is
//! what `crate::native_rt` keys its structures by (the build's
//! destination), so the two meet without the program's help.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::descriptor::View;
use crate::object::This;

/// Whether dispatches are matched (`VICTORY_NATIVE_RT_BIND=1`).
pub(crate) fn on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_NATIVE_RT_BIND").is_some_and(|v| v == "1"))
}

/// `D3D12_SRV_DIMENSION_RAYTRACING_ACCELERATION_STRUCTURE`.
const STRUCTURE: u32 = 11;

/// The title address a ray-tracing structure view names: the description's
/// `RaytracingAccelerationStructure.Location` (words 4-5, after the format,
/// dimension, component mapping and padding). `None` for any other view.
fn structure_address(view: &View) -> Option<u64> {
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

/// Counts since the last summary: [glass, other VBuffer, other traces] ×
/// [host, no host, no view].
static COUNTS: [[AtomicU64; 3]; 3] = [const { [const { AtomicU64::new(0) }; 3] }; 3];

/// The first title addresses that found no host top level (a few, for the
/// log: whether they are near what was built tells a wrong key from a
/// missing build).
static MISSED: Mutex<Vec<u64>> = Mutex::new(Vec::new());

/// The first structure address among `views`, `view(slot, dword)` giving
/// the title's view at that dword of the table at root parameter `slot`.
fn find_structure(views: &[(u8, u32)], view: impl Fn(u8, u32) -> Option<View>) -> Option<u64> {
    views
        .iter()
        .find_map(|&(slot, dword)| view(slot, dword).as_ref().and_then(structure_address))
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
pub(crate) fn dispatch(this: This, alpha: bool, table: impl Fn(u8) -> Option<u64>) {
    if !on() {
        return;
    }
    let Some((vbuffer, glass, views)) = crate::pipeline::ray_views(this) else {
        return;
    };
    let address = find_structure(&views, |slot, dword| {
        crate::descriptor::view(table(slot)? + u64::from(dword) * 4)
    });
    let found = classify(address, crate::native_rt::top_level);
    if let (Found::NoHost, Some(at)) = (found, address) {
        let mut missed = MISSED.lock().unwrap_or_else(|e| e.into_inner());
        if missed.len() < 4 && !missed.contains(&at) {
            missed.push(at);
            tracing::info!(
                address = format_args!("{at:#x}"),
                glass,
                alpha,
                "native rt bind: a trace names a structure the host did not build"
            );
        }
    }
    let row = if glass {
        0
    } else if vbuffer {
        1
    } else {
        2
    };
    COUNTS[row][found as usize].fetch_add(1, Ordering::Relaxed);
    summary();
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
    let [glass, vbuffer, trace] = [take(0), take(1), take(2)];
    tracing::info!(
        glass_host = glass[0],
        glass_fallback = glass[1] + glass[2],
        glass_no_view = glass[2],
        vbuffer_host = vbuffer[0],
        vbuffer_fallback = vbuffer[1] + vbuffer[2],
        trace_host = trace[0],
        trace_fallback = trace[1] + trace[2],
        trace_no_view = trace[2],
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
        let views = [(1, 0), (1, 8), (1, 16), (4, 8)];
        let found = find_structure(&views, |slot, dword| match (slot, dword) {
            (1, 8) => Some(rt_view(0xabc00)),
            (4, 8) => Some(rt_view(0xdef00)),
            (1, 0) => Some(View::Constant(0x10, 16)),
            _ => None,
        });
        assert_eq!(found, Some(0xabc00));
        assert_eq!(find_structure(&views, |_, _| None), None);
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
}
