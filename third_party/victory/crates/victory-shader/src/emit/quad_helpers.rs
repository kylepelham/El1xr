//! `VICTORY_QUAD_HELPERS=1`: in a pixel program, a read of another lane of the quad (a DPP
//! `quad_perm`, a quad `ds_swizzle`) goes through quad reads, which reach helper lanes, not
//! through `WaveReadLaneAt` and a ballot, which leave them out (ported from Belobug's
//! VIctory-Belobug-AMD, via VItoriaBr 2b3c339ef1). Off by default; part of the shader cache
//! key (every `VICTORY_*` the translator names is).

/// Whether `VICTORY_QUAD_HELPERS=1`.
pub(super) fn on() -> bool {
    #[cfg(test)]
    if let Some(on) = TEST_ON.get() {
        return on;
    }
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| parse(std::env::var("VICTORY_QUAD_HELPERS").ok().as_deref()))
}

/// The switch's value: only `1` turns it on.
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
pub(super) fn with<T>(on: bool, f: impl FnOnce() -> T) -> T {
    let before = TEST_ON.replace(Some(on));
    let result = f();
    TEST_ON.set(before);
    result
}

#[cfg(test)]
mod tests {
    use super::parse;

    #[test]
    fn only_one_turns_it_on() {
        assert!(!parse(None));
        assert!(!parse(Some("")));
        assert!(!parse(Some("0")));
        assert!(!parse(Some("2")));
        assert!(parse(Some("1")));
        assert!(parse(Some(" 1 ")));
    }
}
