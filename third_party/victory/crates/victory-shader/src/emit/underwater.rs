//! The water's underwater and portal transmission programs in the wave-wide form on a host of
//! 32-lane waves (`VICTORY_UNDERWATER_WIDE=1`).
//!
//! `water_shading:CS_TransmissionVolumetricsSubmerged` (`926c46bab3b86bd6`) and
//! `_VolumePortalsOnly` (`a10179ab7eda0dc3`), GTA VI 1.0.2.1: groups of one console wave with LDS
//! read and written in one lane-branch region, as their tiled siblings. No other form takes them
//! to 32-lane waves: Submerged is refused (its dispatches dropped: near-black picture under
//! water), PortalsOnly comes out with 64-lane waves, which the driver refuses. With the switch
//! they join the programs that take the wave-wide branches before any approximate form
//! ([`listed`], digest-scoped; the tiled siblings `CS_TransmissionVolumetrics_Tiled*` are not
//! added by this switch).
//!
//! And a host-loop retry (`VICTORY_LOOPS_WAVE=1`, [`super::loops_wave`]) that still needs 64-lane
//! waves on a 32-lane host no longer counts as solved ([`retry_counts`]): it kept the blocks where
//! the halves have to meet from the wave-wide form, which then found nothing to widen and
//! refused Submerged.
//!
//! Off by default. `VICTORY_WATER_WIDE=1` reaches the same two programs (and the untiled
//! `CS_TransmissionVolumetrics`) through the run-time list and `VICTORY_WAVE_WIDE=last`; this
//! switch does it in the translator, so offline translation and prewarm see it too.
//!
//! Ported from readmet3xt-lab/foxhound PR #16 (head `9d1fba51`, JokerAttack), GPL-3.0; the
//! PortalsOnly digest is from trustno1-coder/VIctory-NVIDIA #101.

/// The containers' digests (bytes 4..20 of the container).
const DIGESTS: [[u8; 16]; 2] = [
    // CS_TransmissionVolumetricsSubmerged (926c46bab3b86bd6)
    [
        0xf1, 0xf1, 0x61, 0xe4, 0x7e, 0xef, 0xa3, 0x64, 0x69, 0x18, 0x39, 0xfd, 0x72, 0x06, 0xcc,
        0xf6,
    ],
    // CS_TransmissionVolumetrics_VolumePortalsOnly (a10179ab7eda0dc3)
    [
        0x4f, 0xa5, 0x83, 0x20, 0xd7, 0x61, 0xbf, 0x55, 0x81, 0x20, 0xf9, 0x2c, 0x19, 0xd3, 0x84,
        0xaa,
    ],
];

pub(super) fn on() -> bool {
    #[cfg(test)]
    if let Some(on) = TEST_UNDERWATER_WIDE.get() {
        return on;
    }
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("VICTORY_UNDERWATER_WIDE").is_ok_and(|v| v.trim() == "1"))
}

#[cfg(test)]
thread_local! {
    /// A test's choice for [`on`], for its thread only.
    pub(super) static TEST_UNDERWATER_WIDE: std::cell::Cell<Option<bool>> =
        const { std::cell::Cell::new(None) };
}

/// Whether the switch puts the program of `digest` with groups of `threads` threads on the
/// wave-wide list.
pub(super) fn listed(threads: Option<u32>, digest: [u8; 16]) -> bool {
    threads == Some(64) && on() && DIGESTS.contains(&digest)
}

/// Whether a host-loop retry that gives waves of `wave` lanes on a host of `host_wave` counts as
/// solved: with the switch on, not when it still needs waves wider than the host's.
pub(super) fn retry_counts(wave: u32, host_wave: u32) -> bool {
    !on() || wave <= host_wave
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_two_programs_join_the_list_only_with_the_switch() {
        let was = TEST_UNDERWATER_WIDE.replace(Some(true));
        assert!(listed(Some(64), DIGESTS[0]));
        assert!(listed(Some(64), DIGESTS[1]));
        assert!(!listed(Some(128), DIGESTS[0]));
        assert!(!listed(Some(64), [0; 16]));
        assert!(!retry_counts(64, 32));
        assert!(retry_counts(32, 32));
        TEST_UNDERWATER_WIDE.set(Some(false));
        assert!(!listed(Some(64), DIGESTS[0]));
        assert!(retry_counts(64, 32));
        TEST_UNDERWATER_WIDE.set(was);
    }
}
