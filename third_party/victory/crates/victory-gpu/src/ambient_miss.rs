//! (foxhound `9f8838b`) What the stand-in for reflections
//! (`VICTORY_AMBIENT_SPECULAR`, `crate::ambient::Role::Miss`) did: counted
//! per dispatch of the title's miss program, said in one line every ten
//! seconds, so a run shows that it ran and on which surfaces.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// What came of a dispatch of the miss program.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum Note {
    /// Followed by the estimate; the rays' layer (its first root constant).
    Ran(u32),
    /// Left as the title had it: water, or the alpha layer with
    /// `VICTORY_AMBIENT_SPECULAR_ALPHA=0`.
    Water,
    /// Left as the title had it: no frame's light kept yet, or its image or
    /// depth not found.
    NotReady,
    /// Its image found and no depth: the light around without a direction
    /// (`VICTORY_AMBIENT_SPECULAR_BOUND=1`); the rays' layer.
    Bound(u32),
}

/// Dispatches followed, frames they were in, left as they were (water,
/// not ready), the last frame counted, and the layers seen (bit 0 opaque,
/// 1 alpha, 2 water, 3 others).
static RAN: AtomicU64 = AtomicU64::new(0);
static FRAMES: AtomicU64 = AtomicU64::new(0);
static WATER: AtomicU64 = AtomicU64::new(0);
static NOT_READY: AtomicU64 = AtomicU64::new(0);
static BOUND: AtomicU64 = AtomicU64::new(0);
static LAST_FRAME: AtomicU64 = AtomicU64::new(u64::MAX);
static LAYERS: AtomicU64 = AtomicU64::new(0);

/// Whether the miss program's rays of `layer` take the estimate: the opaque
/// layer, and the alpha one unless `alpha` is false. Water's are left as
/// they are (the depth there is of what lies under it).
pub(crate) fn takes(layer: u32, alpha: bool) -> bool {
    layer == crate::ambient::MISS_OPAQUE || (alpha && layer == crate::ambient::MISS_ALPHA)
}

/// Counts what came of a dispatch of the miss program; every ten seconds an
/// info line says it.
pub(crate) fn note(note: Note) {
    let Some(set) = crate::ambient::specular() else {
        return;
    };
    match note {
        Note::Ran(layer) => {
            RAN.fetch_add(1, Relaxed);
            LAYERS.fetch_or(1 << layer.min(3), Relaxed);
            let frame = crate::frame::current();
            if LAST_FRAME.swap(frame, Relaxed) != frame {
                FRAMES.fetch_add(1, Relaxed);
            }
        }
        Note::Water => {
            WATER.fetch_add(1, Relaxed);
        }
        Note::NotReady => {
            NOT_READY.fetch_add(1, Relaxed);
        }
        Note::Bound(layer) => {
            BOUND.fetch_add(1, Relaxed);
            LAYERS.fetch_or(1 << layer.min(3), Relaxed);
        }
    }
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
    let layers = LAYERS.swap(0, Relaxed);
    tracing::info!(
        "a stand-in for reflections (VICTORY_AMBIENT_SPECULAR={},{}) in the last {seconds:.0} s: {} frames, \
         {} dispatches of CS_RayMiss followed (surfaces: {}), each by an estimate along the reflected direction \
         put in right after it; {} of water (or the alpha layer with VICTORY_AMBIENT_SPECULAR_ALPHA=0) and {} \
         before a frame's light was kept or without its views left as the title had them; {} without a depth \
         given the light around instead (VICTORY_AMBIENT_SPECULAR_BOUND={})",
        set.share,
        set.sky,
        FRAMES.swap(0, Relaxed),
        RAN.swap(0, Relaxed),
        named(layers),
        WATER.swap(0, Relaxed),
        NOT_READY.swap(0, Relaxed),
        BOUND.swap(0, Relaxed),
        u8::from(crate::ambient_reflect::bound()),
    );
}

/// The layers of the bits `layers`, for the line.
fn named(layers: u64) -> String {
    let named: Vec<&str> = [(1, "opaque"), (2, "alpha"), (4, "water"), (8, "other")]
        .iter()
        .filter(|(bit, _)| layers & bit != 0)
        .map(|(_, name)| *name)
        .collect();
    if named.is_empty() {
        "none".to_owned()
    } else {
        named.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layers_taken() {
        assert!(takes(0, false));
        assert!(takes(0, true));
        assert!(takes(1, true));
        assert!(!takes(1, false));
        assert!(!takes(2, true));
        assert!(!takes(u32::MAX, true));
    }

    #[test]
    fn layers_named() {
        assert_eq!(named(0), "none");
        assert_eq!(named(0b11), "opaque, alpha");
        assert_eq!(named(0b1100), "water, other");
    }
}
