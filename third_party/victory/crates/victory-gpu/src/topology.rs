//! Soft thread placement across a processor's two last-level caches
//! (`VICTORY_VCACHE_PIN`, opt-in: with it on by default, a capture showed
//! black faces, the wrong jacket and orange hair, likely texture races that
//! the changed thread timing exposes).
//!
//! On a dual-CCD 3D V-Cache part such as the Ryzen 9 9950X3D (CCD0: 16 sets
//! on 96 MiB of L3, CCD1: 16 sets on 32 MiB) every core reports the same
//! efficiency class; only the L3 sizes from
//! `GetLogicalProcessorInformationEx(RelationCache)` tell the CCDs apart
//! ([`crate::cores::l3_of_sets`]). This module splits the sets in two:
//!
//! - hot threads ([`pin_hot`]: submission, relay, heap sync, the title's
//!   recording threads) prefer the largest-L3 sets;
//! - bulk workers ([`pin_background`]: pipeline compiles, texture prepare,
//!   texture store, read-back writer, land-aside, DirectStorage) prefer the
//!   others, so their streaming does not evict the hot working set.
//!
//! Both go through `SetThreadSelectedCpuSets`, a preference the scheduler
//! drops when the preferred sets are busy (unlike an affinity mask). Unlike
//! `VICTORY_CACHE_CORES` ([`crate::cores`]), the wide copy/merge helpers are
//! left unpinned: putting 24+ of them on CCD0's 8 cores starved `sync_heaps`.
//! On a hybrid P/E-core part the split goes by efficiency class instead.

use std::sync::OnceLock;

/// What told the two groups apart.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Split {
    /// Nothing did: all sets alike.
    Uniform,
    /// Efficiency class (hybrid P/E cores).
    Class,
    /// Last-level cache size, in bytes for the hot and the other sets.
    Cache { hot: u32, other: u32 },
}

/// Set ids for each kind of thread.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Placement {
    pub split: Split,
    pub hot: Vec<u32>,
    pub background: Vec<u32>,
    pub total: usize,
}

/// Splits `sets` (id, efficiency class, core) by class if there are two,
/// else by the last-level cache `l3` (bytes by set id) if it differs and the
/// largest serves at least eight sets (the rules of [`crate::cores`]).
pub fn classify(sets: &[(u32, u8, u8)], l3: &[(u32, u32)]) -> Placement {
    let others = |hot: &[u32]| -> Vec<u32> {
        sets.iter()
            .map(|s| s.0)
            .filter(|id| !hot.contains(id))
            .collect()
    };
    let by_class = crate::cores::fast_by_class(sets);
    if !by_class.is_empty() {
        let background = others(&by_class);
        return Placement {
            split: Split::Class,
            hot: by_class,
            background,
            total: sets.len(),
        };
    }
    let by_cache = crate::cores::fast_by_cache(sets, l3);
    if !by_cache.is_empty() {
        let size = |id: u32| l3.iter().find(|s| s.0 == id).map_or(0, |s| s.1);
        let background = others(&by_cache);
        let split = Split::Cache {
            hot: size(by_cache[0]),
            other: background.iter().map(|&id| size(id)).max().unwrap_or(0),
        };
        return Placement {
            split,
            hot: by_cache,
            background,
            total: sets.len(),
        };
    }
    Placement {
        split: Split::Uniform,
        hot: Vec::new(),
        background: Vec::new(),
        total: sets.len(),
    }
}

/// `VICTORY_VCACHE_PIN`: `1` on wherever the sets split, `cache` only where
/// the last-level caches differ, anything else (unset) off.
pub fn active(value: Option<&str>, split: Split) -> bool {
    match value.map(str::trim) {
        Some("1") => split != Split::Uniform,
        Some("cache") => matches!(split, Split::Cache { .. }),
        _ => false,
    }
}

/// Whether placement applies on this machine (see [`active`]).
pub fn vcache_pin_enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        active(
            std::env::var("VICTORY_VCACHE_PIN").ok().as_deref(),
            discover().split,
        )
    })
}

/// The sets a thread of this kind prefers (empty: leave it alone).
pub fn target(placement: &Placement, hot: bool) -> &[u32] {
    if placement.split == Split::Uniform {
        return &[];
    }
    if hot {
        &placement.hot
    } else {
        &placement.background
    }
}

/// This machine's placement, probed once.
pub fn discover() -> &'static Placement {
    static PLACEMENT: OnceLock<Placement> = OnceLock::new();
    PLACEMENT.get_or_init(|| {
        let placement = classify(&crate::cores::cpu_sets(), &crate::cores::l3_of_sets());
        tracing::info!(
            split = ?placement.split,
            hot = placement.hot.len(),
            background = placement.background.len(),
            of = placement.total,
            "processor sets split for thread placement (VICTORY_VCACHE_PIN)"
        );
        placement
    })
}

/// Prefers the largest-L3 (or fastest-class) sets for the calling thread,
/// which a frame waits for. Once per thread; nothing without the switch.
pub fn pin_hot(what: &str) {
    crate::priority::helper();
    pin(what, true);
}

/// Prefers the other sets for the calling bulk worker. Once per thread;
/// nothing without the switch.
pub fn pin_background(what: &str) {
    pin(what, false);
}

fn pin(what: &str, hot: bool) {
    use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadSelectedCpuSets};
    thread_local! {
        static DONE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }
    if !vcache_pin_enabled() || DONE.with(|done| done.replace(true)) {
        return;
    }
    let sets = target(discover(), hot);
    if sets.is_empty() {
        return;
    }
    // SAFETY: the calling thread's pseudo-handle and the system's own set ids.
    let ok =
        unsafe { SetThreadSelectedCpuSets(GetCurrentThread(), sets.as_ptr(), sets.len() as u32) };
    if ok == 0 {
        tracing::warn!(what, hot, error = %std::io::Error::last_os_error(), "thread placement refused");
    } else {
        tracing::info!(
            what,
            hot,
            sets = sets.len(),
            "thread placed (VICTORY_VCACHE_PIN)"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MB: u32 = 1 << 20;

    /// `n` sets of one class, two to a core, from id 256.
    fn alike(n: u32) -> Vec<(u32, u8, u8)> {
        (0..n).map(|i| (256 + i, 0, (i / 2) as u8)).collect()
    }

    fn l3(n: u32, big_first: bool) -> Vec<(u32, u32)> {
        (0..n)
            .map(|i| {
                (
                    256 + i,
                    if (i < n / 2) == big_first {
                        96 * MB
                    } else {
                        32 * MB
                    },
                )
            })
            .collect()
    }

    #[test]
    fn the_9950x3d_splits_hot_onto_the_vcache_ccd_and_bulk_onto_the_other() {
        let p = classify(&alike(32), &l3(32, true));
        assert_eq!(
            p.split,
            Split::Cache {
                hot: 96 * MB,
                other: 32 * MB
            }
        );
        assert_eq!(p.hot, (256..272).collect::<Vec<_>>());
        assert_eq!(p.background, (272..288).collect::<Vec<_>>());
        assert_eq!(target(&p, true), &p.hot[..]);
        assert_eq!(target(&p, false), &p.background[..]);
        // The stacked cache on the second CCD.
        let flipped = classify(&alike(32), &l3(32, false));
        assert_eq!(flipped.hot, (272..288).collect::<Vec<_>>());
        assert_eq!(flipped.background, (256..272).collect::<Vec<_>>());
    }

    #[test]
    fn alike_caches_or_a_single_ccd_leave_threads_alone() {
        let even: Vec<(u32, u32)> = (0..32).map(|i| (256 + i, 32 * MB)).collect();
        let p = classify(&alike(32), &even);
        assert_eq!(p.split, Split::Uniform);
        assert!(target(&p, true).is_empty() && target(&p, false).is_empty());
        let one: Vec<(u32, u32)> = (0..16).map(|i| (256 + i, 96 * MB)).collect();
        assert_eq!(classify(&alike(16), &one).split, Split::Uniform);
        assert_eq!(classify(&alike(32), &[]).split, Split::Uniform);
    }

    #[test]
    fn hybrid_cores_split_by_class() {
        let sets: Vec<(u32, u8, u8)> = (0..16)
            .map(|n| (n, 1, n as u8 / 2))
            .chain((16..24).map(|n| (n, 0, n as u8)))
            .collect();
        let p = classify(&sets, &[]);
        assert_eq!(p.split, Split::Class);
        assert_eq!(p.hot, (0..16).collect::<Vec<_>>());
        assert_eq!(p.background, (16..24).collect::<Vec<_>>());
    }

    #[test]
    fn placement_is_opt_in() {
        let cache = Split::Cache {
            hot: 96 * MB,
            other: 32 * MB,
        };
        assert!(!active(None, cache));
        assert!(active(Some("cache"), cache));
        assert!(!active(Some("cache"), Split::Class));
        assert!(!active(None, Split::Uniform));
        assert!(!active(Some("0"), cache));
        assert!(active(Some(" 1 "), Split::Class));
        assert!(active(Some("1"), cache));
        assert!(!active(Some("1"), Split::Uniform));
    }

    #[test]
    fn this_machine_is_probed_and_its_sets_accepted() {
        use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadSelectedCpuSets};
        let p = discover();
        assert!(p.total > 0);
        eprintln!(
            "topology probe: {:?}, hot {}, background {}",
            p.split,
            p.hot.len(),
            p.background.len()
        );
        for hot in [true, false] {
            let sets = target(p, hot).to_vec();
            if sets.is_empty() {
                continue;
            }
            std::thread::spawn(move || {
                // SAFETY: the calling thread's pseudo-handle and the system's own set ids.
                let ok = unsafe {
                    SetThreadSelectedCpuSets(GetCurrentThread(), sets.as_ptr(), sets.len() as u32)
                };
                assert_ne!(ok, 0, "{}", std::io::Error::last_os_error());
            })
            .join()
            .unwrap();
        }
    }
}
