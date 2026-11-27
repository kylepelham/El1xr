//! A draw's table that takes a texture the prepare threads made ready
//! (`prepare::take_ready` with no slot served) retires that entry of their
//! queue at once (`VICTORY_PREPARE_TAKE_BY_TABLE=1`; off unless set): the
//! table of staging buffers holds it from then on, and the texture pass
//! finds it there. Off, the entry, its slots and its bytes stayed in the
//! queue until the prune two seconds later, counted as made ready and let
//! go untaken, and their bytes held the pass's budget (`Look::Full`).

/// The switch (`VICTORY_PREPARE_TAKE_BY_TABLE`; on only when `1`).
pub(crate) fn on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| switch(std::env::var_os("VICTORY_PREPARE_TAKE_BY_TABLE").as_deref()))
}

fn switch(value: Option<&std::ffi::OsStr>) -> bool {
    value.is_some_and(|v| v == "1")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_switch_is_off_unless_one() {
        assert!(!switch(None));
        assert!(!switch(Some(std::ffi::OsStr::new("0"))));
        assert!(switch(Some(std::ffi::OsStr::new("1"))));
    }
}
