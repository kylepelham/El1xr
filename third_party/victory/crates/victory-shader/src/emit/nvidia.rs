//! The NVIDIA Wave32 lowering switch (ported from codec-d/cool_fox 8cc31ab3, "Port NVIDIA
//! Wave32 lowering"). A host of 32-lane waves (NVIDIA, which refuses wave64 pipelines with
//! `0x80070057`) drops every draw whose program asks for 64 lanes. With
//! `VICTORY_WAVE32_NVIDIA=1` the translator also takes:
//!
//! * `ACCC` containers (native code another compiler made, in place of `DXIL`) and the
//!   active-lane loop of such a compute program run wave-wide ([`super::widen`], `driver_loop`);
//! * a scalar select on the SCC of a mask operation as a wave-wide test (`scc_some`);
//! * a scalar saved to a constant VGPR lane and restored in the same block as that scalar,
//!   even where the VGPR has ordinary vector uses ([`crate::resource::Resources::restored_lanes`]);
//! * a shared-memory read at a lane-uniform address as lane-uniform (`lane_uniform`), so that
//!   the two host waves read the same word in order.
//!
//! Off unless asked for: the translation, and with it the shader cache entries, stay as
//! they were. The switch is part of the cache key (every `VICTORY_*` the translator reads is).

/// Whether the NVIDIA Wave32 lowering is on (`VICTORY_WAVE32_NVIDIA=1`).
pub fn enabled() -> bool {
    #[cfg(test)]
    if let Some(on) = TEST_ON.get() {
        return on;
    }
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| parse(std::env::var("VICTORY_WAVE32_NVIDIA").ok().as_deref()))
}

/// The switch's value: only `1` turns it on. Pure, for testing.
fn parse(value: Option<&str>) -> bool {
    value.map(str::trim) == Some("1")
}

#[cfg(test)]
thread_local! {
    /// A test's choice, for its thread only.
    static TEST_ON: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

/// Runs `f` with the switch set to `on` on this thread.
#[cfg(test)]
pub(crate) fn with<T>(on: bool, f: impl FnOnce() -> T) -> T {
    let before = TEST_ON.replace(Some(on));
    let result = f();
    TEST_ON.set(before);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_one_turns_it_on() {
        assert!(!parse(None));
        assert!(!parse(Some("")));
        assert!(!parse(Some("0")));
        assert!(!parse(Some("true")));
        assert!(parse(Some("1")));
        assert!(parse(Some(" 1 ")));
    }

    #[test]
    fn a_test_can_choose_for_its_thread() {
        assert!(with(true, enabled));
        assert!(!with(false, enabled));
    }
}
