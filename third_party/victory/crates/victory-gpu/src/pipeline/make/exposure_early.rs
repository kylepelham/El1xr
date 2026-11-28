//! (`VICTORY_EXPOSURE_EARLY=1`) Six more compute programs made as scene inputs
//! (`super::scene_input`): at creation, even on a cold cache, and waited for once past the frame
//! budget instead of dropped while queued. The title's auto-exposure (the luminance downsample,
//! `47d8c225abfcb246`, then the exposure value, `fdce2a0a9fb1521a`): dropped while queued, the
//! world draws black until it is made. And four computes the same notes found late in every
//! run. Only with `VICTORY_SCENE_INPUT=1`.
//!
//! Port of crbns/victory-tapin `09c6420bdc` (fox2 r16, "black intro / black world fix",
//! GPL-3.0-or-later). Not in the shader or pipeline key: only when a pipeline is made changes.

use std::sync::OnceLock;

/// The programs, by the hashes of the title's own containers.
const EXPOSURE: [&str; 6] = [
    "47d8c225abfcb246",
    "fdce2a0a9fb1521a",
    "528278df1274cb33",
    "477b52e73ddeb6f2",
    "8b437ab61d8008fe",
    "17af85f170e66f30",
];

fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_EXPOSURE_EARLY").is_some_and(|v| v == "1"))
}

/// Whether a shader with this hash is one of them ([`EXPOSURE`]), the switch aside.
fn named(hash: &str) -> bool {
    EXPOSURE.contains(&hash)
}

/// Whether a shader with this hash makes its pipeline a scene input (`VICTORY_EXPOSURE_EARLY=1`).
pub(super) fn scene_input(hash: &str) -> bool {
    let named = named(hash);
    if named && on() {
        tracing::info!(
            hash,
            "exposure early: made as a scene input (VICTORY_EXPOSURE_EARLY=1)"
        );
    }
    named && on()
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_six_programs_and_no_other() {
        for h in super::EXPOSURE {
            assert!(super::named(h));
        }
        assert!(!super::named("970c4fbb3b017c0e"));
        assert!(!super::named(""));
    }
}
