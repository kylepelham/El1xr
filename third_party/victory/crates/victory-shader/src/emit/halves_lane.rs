//! `VICTORY_HALVES_LANE=1` (off by default, in the shader cache key): a console wave64 compute
//! program run as two host waves of 32 that exchanges nothing between them (the halves form)
//! takes a lane's bit of a 64-bit mask read as a number by its console lane, not its host lane.
//! Its host lanes are 0..31 in both host waves, so lanes 32..63 took the low dword's bits (a
//! select on an SGPR pair, `v_cndmask_b32` with a mask the lowering does not track).
//!
//! Ported from Diego Casares's PR #108 on trustno1-coder/VIctory-NVIDIA (`245cb85`), GPL-3.0.

/// `VICTORY_HALVES_LANE=1`; a test can choose for its own thread.
pub(super) fn enabled() -> bool {
    #[cfg(test)]
    if let Some(on) = TEST_HALVES_LANE.get() {
        return on;
    }
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("VICTORY_HALVES_LANE").is_ok_and(|v| v.trim() == "1"))
}

#[cfg(test)]
thread_local! {
    /// A test's choice for [`enabled`], for its thread only.
    pub(crate) static TEST_HALVES_LANE: std::cell::Cell<Option<bool>> =
        const { std::cell::Cell::new(None) };
}
