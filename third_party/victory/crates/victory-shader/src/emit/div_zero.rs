//! (`VICTORY_DIV_INF_ZERO=1`) `v_div_fixup` resolves zero and infinity operands before a NaN
//! quotient, in the console's order: a NaN operand, then 0/0 and inf/inf, then inf (a numerator
//! of inf or a denominator of 0), then 0 (a numerator of 0 or a denominator of inf), and only then
//! a NaN quotient as inf. Without it a NaN quotient was taken for inf ahead of the zero rules: a
//! division by ±inf (the reciprocal and fma steps before the fixup leave the quotient NaN) gave
//! ±inf instead of ±0, and 0/x with a NaN quotient inf, which `VICTORY_NAN_SCRUB` and the like
//! then turn black.
//!
//! Port of trustno1-coder/VIctory-NVIDIA `feb90adeb1` (Shawn, nawnie; a cherry-pick of
//! `6e6f879`, GPL-3.0-or-later). Every precise division goes through the fixup: retranslates
//! (shader cache key).

#[cfg(test)]
thread_local! {
    /// Tests: the switch on for this thread.
    pub(super) static FORCED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether the fixup takes the zero rules before a NaN quotient (`VICTORY_DIV_INF_ZERO=1`).
pub fn on() -> bool {
    #[cfg(test)]
    if FORCED.get() {
        return true;
    }
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_DIV_INF_ZERO").is_some_and(|v| v == "1"))
}
