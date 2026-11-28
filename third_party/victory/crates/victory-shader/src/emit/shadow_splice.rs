//! The shadow splice (`VICTORY_NATIVE_SHADOW=1` with `VICTORY_NATIVE_BVH=1`): the title's
//! shadow traces find their occluders with inline ray queries on the host top level instead of
//! their own walks of console structures, as the glass splice does for the reflection trace
//! ([`super::glass_splice`]).

/// Whether the shadow traces' walks go to the host top level.
pub fn native_shadow() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        let on = |name: &str| std::env::var_os(name).is_some_and(|v| v == "1");
        on("VICTORY_NATIVE_SHADOW") && on("VICTORY_NATIVE_BVH")
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn off_by_default() {
        assert!(!super::native_shadow());
    }
}
