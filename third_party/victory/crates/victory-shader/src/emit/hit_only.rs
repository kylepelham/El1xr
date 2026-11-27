//! Translations whose ray-tracing node tests all miss, chosen per program by
//! the caller (`victory-gpu`'s `VICTORY_RT_HIT_ONLY`): such a program's
//! `image_bvh*_intersect_ray` answers as with `VICTORY_BVH_NO_HIT=1`, and
//! reads no structure. The other programs walk the structures.

use std::cell::Cell;

thread_local! {
    /// The translation on this thread misses every node test ([`with_rays_missed`]).
    static MISSED: Cell<bool> = const { Cell::new(false) };
}

/// Runs the translation `f` with every node test a miss when `missed`.
pub fn with_rays_missed<R>(missed: bool, f: impl FnOnce() -> R) -> R {
    if !missed {
        return f();
    }
    let before = MISSED.replace(true);
    let r = f();
    MISSED.set(before);
    r
}

/// Whether the translation on this thread misses every node test.
pub(super) fn missed() -> bool {
    MISSED.get()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missed_only_inside() {
        assert!(!missed());
        assert!(with_rays_missed(true, missed));
        assert!(!missed());
        assert!(!with_rays_missed(false, missed));
    }
}
