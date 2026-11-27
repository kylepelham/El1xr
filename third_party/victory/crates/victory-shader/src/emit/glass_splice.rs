//! `VICTORY_NATIVE_GLASS=1` (off by default; needs `VICTORY_NATIVE_BVH=1`):
//! the glass trace `6d7fe7745ab9c6d8`'s near walk on host top levels, by
//! inline ray queries on root SRVs [`super::SLOT_TLAS`] (its `t355`) and
//! [`super::SLOT_TLAS_NEAR`] (its `t354`), which the host binds per dispatch
//! (`victory-gpu`'s `rt_bind`). The switch is part of the shader cache key.
//! It touches no program yet: the splice of the walk is still to come.

/// The glass trace's container hash, as the log names shaders.
pub const GLASS_TRACE: &str = "6d7fe7745ab9c6d8";

/// Whether the glass trace's walk goes to host top levels
/// (`VICTORY_NATIVE_GLASS=1` with `VICTORY_NATIVE_BVH=1`).
pub fn native_glass() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        let on = |name: &str| std::env::var_os(name).is_some_and(|v| v == "1");
        on("VICTORY_NATIVE_GLASS") && on("VICTORY_NATIVE_BVH")
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn off_by_default() {
        assert!(!super::native_glass());
    }
}
