//! `VICTORY_NULL_VIEW_SIZE=1` (off by default; part of the translation, so
//! the shader cache keys on it): the size fields a program reads from a
//! descriptor ([`super::lower`]'s `descriptor_data`) count a view that shows
//! nothing as one texel each way and one level, as the console's empty
//! descriptor reads. Without it a null view (a slot whose texture is not
//! there yet, `VICTORY_TABLE_DEFER`'s stand-ins, a view the host refused)
//! gives 0 - 1 in every field: 16,384 texels each way and 16 levels, so a
//! program that picks a level or scales coordinates by the size reads
//! garbage around a texture's arrival.
//!
//! Source: ProjectLeonida/mandy f803776825 (after brcly/TuxBox 0a5ebfa).

#[cfg(test)]
thread_local! {
    /// A test's choice for [`on`], for its thread only.
    pub(super) static TEST_ON: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

/// Whether the switch is on.
pub(super) fn on() -> bool {
    #[cfg(test)]
    if let Some(on) = TEST_ON.get() {
        return on;
    }
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_NULL_VIEW_SIZE").is_some_and(|v| v == "1"))
}
