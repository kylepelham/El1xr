//! Three more switches of the stand-in for the indirect light
//! (`crate::ambient`), each off by default; the passes are in its source.
//!
//! - `VICTORY_AMBIENT_SKY_BOUND=<multiple>` (wasonidas,
//!   trustno1-coder/VIctory-NVIDIA#63, at 1 by tippythetaidum #393): right
//!   after a dispatch of `CS_RayMiss` of the alpha layer (glass, the eyes' wet
//!   layer, decals) the rays' image keeps the title's sky light, and whatever
//!   `CS_DeferredFallbacksAlpha_SkyProbe` left there, up to that multiple of
//!   the geometric mean of the picture's light (`crate::ambient::sky_bound`).
//!   With `VICTORY_AMBIENT_SKY_BOUND_OPAQUE=1` the opaque layer's too, where
//!   the estimate along the reflected direction (`VICTORY_AMBIENT_SPECULAR`)
//!   did not run. Needs `VICTORY_AMBIENT` (the picture's light is kept by its
//!   pass); works with or without `VICTORY_AMBIENT_SPECULAR`, and with it the
//!   alpha layer is bounded instead of estimated (#393). Live.
//! - `VICTORY_AMBIENT_ONCE=1` (readmet3xt-lab 8cf0a75): the estimate is one
//!   bounce of the title's own light. Without it the stand-in's raised light
//!   comes back to it through the next frame's keep and settles at
//!   `level e / (1 - level a)` instead of one bounce (1.3x at walls that send
//!   out 0.6, 1.75x at 0.9). Live.
//! - `VICTORY_AMBIENT_CHROMA_SCRUB=1` (Vladimir-Human,
//!   trustno1-coder/VIctory-NVIDIA#48): the raise drops acid-green and
//!   electric-cyan leftovers of the probes, as it drops the magenta ones. Read
//!   at start (a define of the passes, as `VICTORY_AMBIENT_VIOLET`).

use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use victory_core::settings::Live;

fn parse_multiple(v: Option<&str>) -> u64 {
    let multiple = v
        .and_then(|v| v.trim().parse::<f32>().ok())
        .filter(|k| k.is_finite() && *k > 0.0)
        .unwrap_or(0.0);
    u64::from(multiple.to_bits())
}

/// `VICTORY_AMBIENT_SKY_BOUND=<multiple>`; `None`: off (unset, 0 or not a
/// number).
pub(crate) fn sky_bound() -> Option<f32> {
    static SET: Live = Live::new("VICTORY_AMBIENT_SKY_BOUND", parse_multiple);
    let multiple = f32::from_bits(SET.get() as u32);
    (multiple > 0.0).then_some(multiple)
}

/// `VICTORY_AMBIENT_SKY_BOUND_OPAQUE=1`.
pub(crate) fn sky_bound_opaque() -> bool {
    static SET: Live = Live::flag("VICTORY_AMBIENT_SKY_BOUND_OPAQUE");
    SET.get_bool()
}

/// The multiple the miss program's rays of `layer` are bounded by.
pub(crate) fn bound_for(layer: u32) -> Option<f32> {
    bound_of(layer, sky_bound(), sky_bound_opaque())
}

fn bound_of(layer: u32, multiple: Option<f32>, opaque: bool) -> Option<f32> {
    match layer {
        crate::ambient::MISS_ALPHA => multiple,
        crate::ambient::MISS_OPAQUE if opaque => multiple,
        _ => None,
    }
}

/// `VICTORY_AMBIENT_ONCE=1`.
pub(crate) fn once() -> bool {
    static SET: Live = Live::flag("VICTORY_AMBIENT_ONCE");
    SET.get_bool()
}

/// `VICTORY_AMBIENT_CHROMA_SCRUB=1`.
pub(crate) fn chroma() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("VICTORY_AMBIENT_CHROMA_SCRUB").is_ok_and(|v| v.trim() == "1"))
}

/// Dispatches of the miss program bounded, by layer (opaque, alpha), and
/// those the bound was asked for that recorded nothing.
static BOUNDED: [AtomicU64; 2] = [AtomicU64::new(0), AtomicU64::new(0)];
static MISSED: AtomicU64 = AtomicU64::new(0);

/// Counts what came of a dispatch the bound was asked for; every ten
/// seconds an info line says it.
pub(crate) fn note(ran: bool, layer: u32) {
    match (ran, layer) {
        (true, crate::ambient::MISS_OPAQUE) => BOUNDED[0].fetch_add(1, Relaxed),
        (true, _) => BOUNDED[1].fetch_add(1, Relaxed),
        (false, _) => MISSED.fetch_add(1, Relaxed),
    };
    static LAST: Mutex<Option<std::time::Instant>> = Mutex::new(None);
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    let now = std::time::Instant::now();
    let Some(since) = *last else {
        *last = Some(now);
        return;
    };
    let seconds = now.duration_since(since).as_secs_f32();
    if seconds < 10.0 {
        return;
    }
    *last = Some(now);
    tracing::info!(
        "reflections: the title's sky bounded by the picture's light (VICTORY_AMBIENT_SKY_BOUND={:?}, _OPAQUE={}) \
         in the last {seconds:.0} s: {} dispatches of CS_RayMiss of the alpha layer, {} of the opaque layer, \
         {} not bounded (no frame's light kept yet, or the rays' image not found)",
        sky_bound(),
        u8::from(sky_bound_opaque()),
        BOUNDED[1].swap(0, Relaxed),
        BOUNDED[0].swap(0, Relaxed),
        MISSED.swap(0, Relaxed),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_switches_are_live() {
        use victory_core::settings::check_live;
        check_live("VICTORY_AMBIENT_SKY_BOUND", "1", sky_bound, None, Some(1.0));
        check_live(
            "VICTORY_AMBIENT_SKY_BOUND",
            "2.5",
            sky_bound,
            None,
            Some(2.5),
        );
        check_live(
            "VICTORY_AMBIENT_SKY_BOUND_OPAQUE",
            "1",
            sky_bound_opaque,
            false,
            true,
        );
        check_live("VICTORY_AMBIENT_ONCE", "1", once, false, true);
    }

    #[test]
    fn multiples_parse() {
        let read = |v| f32::from_bits(parse_multiple(v) as u32);
        assert_eq!(read(None), 0.0);
        assert_eq!(read(Some("")), 0.0);
        assert_eq!(read(Some("0")), 0.0);
        assert_eq!(read(Some("-1")), 0.0);
        assert_eq!(read(Some("nan")), 0.0);
        assert_eq!(read(Some("off")), 0.0);
        assert_eq!(read(Some(" 1.5 ")), 1.5);
    }

    /// Off (no multiple), no layer is bounded; on, the alpha layer always,
    /// the opaque one only with `_OPAQUE`, water and the others never.
    #[test]
    fn layers_bounded() {
        use crate::ambient::{MISS_ALPHA, MISS_OPAQUE};
        for layer in [MISS_OPAQUE, MISS_ALPHA, 2, u32::MAX] {
            assert_eq!(bound_of(layer, None, true), None);
            assert_eq!(bound_of(layer, None, false), None);
        }
        assert_eq!(bound_of(MISS_ALPHA, Some(1.0), false), Some(1.0));
        assert_eq!(bound_of(MISS_OPAQUE, Some(1.0), false), None);
        assert_eq!(bound_of(MISS_OPAQUE, Some(2.0), true), Some(2.0));
        assert_eq!(bound_of(2, Some(1.0), true), None);
        assert_eq!(bound_of(u32::MAX, Some(1.0), true), None);
    }

    /// (readmet3xt-lab 8cf0a75) The keep and raise as they act on the light,
    /// frame after frame, for surfaces that send out `a` of the light they get
    /// and the title's own light `title` on each: the light it settles at on
    /// each surface. With `once` the keep counts each place's light by the
    /// share of it that is the title's own, as the passes' `ONCE` parts do.
    fn settle(level: f32, a: f32, title: &[f32], once: bool) -> Vec<f32> {
        let mut light = title.to_vec();
        let mut share = vec![1.0f32; title.len()];
        for _ in 0..500 {
            let sent: f32 = light
                .iter()
                .zip(&share)
                .map(|(l, s)| a * l * if once { *s } else { 1.0 })
                .sum::<f32>()
                / title.len() as f32;
            for (i, t) in title.iter().enumerate() {
                light[i] = t.max(level * sent);
                share[i] = t / light[i].max(1e-6);
            }
        }
        light
    }

    /// Without it the stand-in's own light comes back to it: a lit wall and a
    /// dark one settle at `level a t / 2 / (1 - level a / 2)` on the dark one,
    /// a third more than one bounce at walls that send out 0.6, three quarters
    /// more at 0.9. With it, one bounce of the title's light.
    #[test]
    fn once_is_one_bounce_of_the_titles_light() {
        for a in [0.6f32, 0.9] {
            let one_bounce = 0.8 * a / 2.0;
            let fed = settle(0.8, a, &[1.0, 0.0], false)[1];
            let once = settle(0.8, a, &[1.0, 0.0], true)[1];
            assert!(
                (fed - one_bounce / (1.0 - 0.8 * a / 2.0)).abs() < 1e-4,
                "a={a}: fed back {fed}"
            );
            assert!((once - one_bounce).abs() < 1e-4, "a={a}: once {once}");
            assert!(fed > 1.3 * once);
        }
        assert_eq!(settle(0.8, 0.6, &[1.0, 1.0], true), vec![1.0, 1.0]);
    }
}
