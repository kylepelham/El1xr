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
//! And for these two programs a host-loop retry (`VICTORY_LOOPS_WAVE=1`, [`super::loops_wave`])
//! that still needs 64-lane waves on a 32-lane host no longer counts as solved ([`retry_counts`]):
//! it kept the blocks where the halves have to meet from the wave-wide form, which then found
//! nothing to widen and refused Submerged. (foxhound's took that for every program; here it is
//! scoped to the two: everywhere it changed 18 more programs, among them the defrag and triangle
//! programs `VICTORY_WAVE_PAIRS_ONLY` holds.)
//!
//! Off by default. `VICTORY_WATER_WIDE=1` reaches the same two programs (and the untiled
//! `CS_TransmissionVolumetrics`) through the run-time list and `VICTORY_WAVE_WIDE=last`; this
//! switch does it in the translator, so offline translation and prewarm see it too.
//!
//! Ported from readmet3xt-lab/foxhound PR #16 (head `9d1fba51`, JokerAttack), GPL-3.0; the
//! PortalsOnly digest is from trustno1-coder/VIctory-NVIDIA #101.
//!
//! `VICTORY_VOLUMETRICS_WIDE=1` puts the untiled `CS_TransmissionVolumetrics`
//! (`6871d6e4b3ac9a10`) on the same list ([`UNTILED`]). Without it the program is refused ("a
//! lane mask holding lanes elsewhere read as data"); with `VICTORY_MASK_ELSEWHERE=1` it comes
//! out with 64-lane waves, which the driver refuses (`E_INVALIDARG`, "wants wave lane count of
//! 64"). Either way it is the one refused pipeline of every run: the title makes it at load.
//! The wave-wide form gives it 32-lane waves (14 branches taken wave-wide), with or without
//! `VICTORY_MASK_ELSEWHERE` or `VICTORY_LOOPS_WAVE`.

/// The containers' digests (bytes 4..20 of the container).
pub(super) const DIGESTS: [[u8; 16]; 2] = [
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

/// The untiled `CS_TransmissionVolumetrics` (`6871d6e4b3ac9a10`), `VICTORY_VOLUMETRICS_WIDE=1`.
pub(super) const UNTILED: [u8; 16] = [
    0xc4, 0x0b, 0xd9, 0x53, 0x09, 0x6e, 0x88, 0x25, 0xce, 0x64, 0x50, 0xb5, 0x53, 0xf1, 0x23, 0x5c,
];

pub(super) fn on() -> bool {
    #[cfg(test)]
    if let Some(on) = TEST_UNDERWATER_WIDE.get() {
        return on;
    }
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("VICTORY_UNDERWATER_WIDE").is_ok_and(|v| v.trim() == "1"))
}

/// Whether [`UNTILED`] is on the list (`VICTORY_VOLUMETRICS_WIDE=1`).
pub(super) fn untiled_on() -> bool {
    #[cfg(test)]
    if let Some(on) = TEST_VOLUMETRICS_WIDE.get() {
        return on;
    }
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("VICTORY_VOLUMETRICS_WIDE").is_ok_and(|v| v.trim() == "1"))
}

#[cfg(test)]
thread_local! {
    /// A test's choice for [`on`], for its thread only.
    pub(super) static TEST_UNDERWATER_WIDE: std::cell::Cell<Option<bool>> =
        const { std::cell::Cell::new(None) };
    /// A test's choice for [`untiled_on`], for its thread only.
    pub(super) static TEST_VOLUMETRICS_WIDE: std::cell::Cell<Option<bool>> =
        const { std::cell::Cell::new(None) };
}

/// Whether a switch puts the program of `digest` with groups of `threads` threads on the
/// wave-wide list.
pub(super) fn listed(threads: Option<u32>, digest: [u8; 16]) -> bool {
    threads == Some(64) && (on() && DIGESTS.contains(&digest) || untiled_on() && digest == UNTILED)
}

/// Whether a host-loop retry of the program of `digest` that gives waves of `wave` lanes on a
/// host of `host_wave` counts as solved: with the switch on, not for the two programs when it
/// still needs waves wider than the host's.
pub(super) fn retry_counts(digest: [u8; 16], wave: u32, host_wave: u32) -> bool {
    !on() || wave <= host_wave || !DIGESTS.contains(&digest)
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
        assert!(!retry_counts(DIGESTS[0], 64, 32));
        assert!(retry_counts(DIGESTS[0], 32, 32));
        assert!(retry_counts([0; 16], 64, 32));
        TEST_UNDERWATER_WIDE.set(Some(false));
        assert!(!listed(Some(64), DIGESTS[0]));
        assert!(retry_counts(DIGESTS[1], 64, 32));
        TEST_UNDERWATER_WIDE.set(was);
    }

    #[test]
    fn the_untiled_program_joins_the_list_only_with_its_own_switch() {
        let (under, untiled) = (TEST_UNDERWATER_WIDE.replace(Some(true)), TEST_VOLUMETRICS_WIDE.replace(Some(false)));
        assert!(!listed(Some(64), UNTILED), "not with VICTORY_UNDERWATER_WIDE alone");
        TEST_UNDERWATER_WIDE.set(Some(false));
        TEST_VOLUMETRICS_WIDE.set(Some(true));
        assert!(listed(Some(64), UNTILED));
        assert!(!listed(Some(128), UNTILED));
        assert!(!listed(Some(64), DIGESTS[0]), "the other two stay with VICTORY_UNDERWATER_WIDE");
        assert!(retry_counts(UNTILED, 64, 32));
        TEST_UNDERWATER_WIDE.set(under);
        TEST_VOLUMETRICS_WIDE.set(untiled);
    }
}

#[cfg(all(test, windows))]
#[path = "volumetrics_wide_tests.rs"]
mod volumetrics_wide_tests;
