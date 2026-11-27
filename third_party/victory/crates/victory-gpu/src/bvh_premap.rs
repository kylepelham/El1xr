//! `VICTORY_BVH_PREMAP=1` (off by default; only with `VICTORY_BVH_BUILDER=1`
//! and the wrappers of `VICTORY_BVH_HOOK=1`): when the title asks the
//! structure builder for builds, the host copies of every range those
//! builds name are made at once, before the builder's first dispatch
//! (`crate::memory::premap`).
//!
//! Before each builder dispatch the host makes ready what the dispatch's
//! root arguments name (`crate::memory`). What a stage reaches only through
//! pointers it reads out of memory (a batched build's items, the instances
//! of a top level, a bottom level's vertices and indices) gets a copy from
//! the stage's faults, a frame later: until then its reads give 0 and its
//! stores are dropped. In an upstream replay (Marvisak/VIctory issue #3486,
//! ZeOs360) the builder's writes were withheld in 722 of 723 frames, and
//! copies made on demand let bad writes land on live data; the fix there
//! was copies of the build ranges made ahead.
//! shortcut: a range is named by its first address (the copy around it and
//! the 4 MiB after); its size (the prebuild sizes) is not asked.

use std::sync::OnceLock;

/// Whether the builds' ranges are made ready ahead.
pub fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        let set = |name: &str| std::env::var(name).is_ok_and(|v| v.trim() == "1");
        set("VICTORY_BVH_PREMAP") && set("VICTORY_BVH_BUILDER")
    })
}

/// The title's builds name `addresses` (zeros are passed over): their
/// copies are made now ([`on`]).
pub fn ranges(addresses: &[u64]) {
    if on() && !addresses.is_empty() {
        crate::memory::premap(addresses);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn off_by_default() {
        assert!(!super::on());
        super::ranges(&[0x1000]);
    }
}
