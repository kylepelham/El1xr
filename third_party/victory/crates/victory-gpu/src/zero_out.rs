//! `VICTORY_ZERO_OUT=<program>[,..]`: a dispatch of a named program clears the views it
//! writes through its tables instead of running.
//!
//! Port of voidopcode/vilocity `VILOCITY_ZERO_OUT` (`601d5db6d5`, `29111fec6f`, GPL-3.0-or-later).
//! An entry is a program name the container holds (`CS_ScreenProbes_Inject`) or a shader hash
//! as logged (16 hex digits, `02384a60fda6f2d0`). Unset: nothing changes. Leaving a program out
//! (`VICTORY_LEAVE_OUT`) is not the same: its outputs then keep whatever their memory held.
//!
//! - `CS_ScreenProbes_Inject` puts what the rays found into the screen probes' radiance. With
//!   the rays finding nothing it put in what the ray passes' buffers held, which changes run to
//!   run: rainbow glitter on faces and jackets and a red tint on orange jumpsuits through the
//!   final gather (vilocity's dump of every target of a world frame). Here it is cleared only
//!   while the probes' rays find nothing (`crate::ambient::probes_blind`), and its radiance
//!   atlas ([`INJECTED`]) takes the ambient stand-in's light, as skin's copy of it does
//!   (`crate::skin_fill`), instead of 0.
//! - `CS_DefragHeap_ComputeRelocations` (`02384a60fda6f2d0`), the title's defragmenter of the
//!   decal/effects heap: cleared, it plans no moves (vilocity: 230 ms of card time a frame in
//!   some launches, with black faces; four fast launches in a row once cleared).
//!
//! Not in the shader or pipeline key: no translated shader or pipeline changes.

use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// What a named program's dispatch becomes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Zero {
    /// Every table view it writes cleared to 0.
    Plain,
    /// `CS_ScreenProbes_Inject`: cleared while the probes' rays find nothing, its radiance
    /// atlas given the ambient light.
    Inject,
}

/// The screen probes' inject program.
const INJECT: &[u8] = b"CS_ScreenProbes_Inject";

/// Where the inject program writes the probes' radiance (R32_UINT holding R9G9B9E5, a
/// 2048x456 atlas): root parameter and place in its table, in dwords (vilocity `601d5db6d5`).
pub(crate) const INJECTED: (u8, u32) = (1, 112);

/// The entries of `VICTORY_ZERO_OUT`, trimmed, empty ones dropped.
fn parse(set: &str) -> Vec<String> {
    set.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

fn names() -> &'static [String] {
    static SET: OnceLock<Vec<String>> = OnceLock::new();
    SET.get_or_init(|| {
        let names = parse(&std::env::var("VICTORY_ZERO_OUT").unwrap_or_default());
        if !names.is_empty() {
            tracing::info!(?names, "dispatches of these programs clear their outputs instead of running (VICTORY_ZERO_OUT)");
        }
        names
    })
}

fn is_hash(entry: &str) -> bool {
    entry.len() == 16 && entry.bytes().all(|b| b.is_ascii_hexdigit())
}

/// What a compute program with shader hash `hash` and container `container` becomes under
/// the entries `names`: `None` when no entry names it.
pub(crate) fn named(names: &[String], hash: &str, container: &[u8]) -> Option<Zero> {
    let has = |name: &[u8]| !name.is_empty() && container.windows(name.len()).any(|w| w == name);
    let hit = names.iter().any(|n| {
        if is_hash(n) {
            hash.eq_ignore_ascii_case(n)
        } else {
            has(n.as_bytes())
        }
    });
    hit.then(|| {
        if has(INJECT) {
            Zero::Inject
        } else {
            Zero::Plain
        }
    })
}

/// [`named`] under `VICTORY_ZERO_OUT`.
pub(crate) fn of(hash: &str, container: &[u8]) -> Option<Zero> {
    let names = names();
    if names.is_empty() {
        return None;
    }
    named(names, hash, container)
}

/// Dispatches cleared, views cleared, views left (another kind of view, or no slot), atlases
/// given the ambient light; inject dispatches run as the title's because its rays find
/// something.
static DISPATCHES: AtomicU64 = AtomicU64::new(0);
static CLEARED: AtomicU64 = AtomicU64::new(0);
static LEFT: AtomicU64 = AtomicU64::new(0);
static LIT: AtomicU64 = AtomicU64::new(0);
static RAN: AtomicU64 = AtomicU64::new(0);

/// After a dispatch of program `name` (`hash`) was cleared: `cleared` views cleared, `left`
/// not, `lit` atlases given the light. One line the first time a program is, then at every
/// power of two of all of them.
pub(crate) fn note(hash: &str, name: &str, cleared: u64, left: u64, lit: u64) {
    CLEARED.fetch_add(cleared, Relaxed);
    LEFT.fetch_add(left, Relaxed);
    LIT.fetch_add(lit, Relaxed);
    let n = DISPATCHES.fetch_add(1, Relaxed) + 1;
    static SEEN: Mutex<Vec<String>> = Mutex::new(Vec::new());
    let first = {
        let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
        let first = !seen.iter().any(|h| h == hash);
        if first {
            seen.push(hash.to_owned());
        }
        first
    };
    if first {
        tracing::info!(
            hash,
            name,
            cleared,
            left,
            lit,
            "a dispatch's outputs cleared instead of run (VICTORY_ZERO_OUT)"
        );
    }
    if n.is_power_of_two() {
        tracing::info!(
            dispatches = n,
            cleared = CLEARED.load(Relaxed),
            left = LEFT.load(Relaxed),
            lit = LIT.load(Relaxed),
            ran = RAN.load(Relaxed),
            "VICTORY_ZERO_OUT dispatches cleared"
        );
    }
}

/// An inject dispatch ran as the title's (its rays find something).
pub(crate) fn ran() {
    RAN.fetch_add(1, Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(s: &str) -> Vec<String> {
        parse(s)
    }

    #[test]
    fn entries_are_trimmed_and_empty_ones_dropped() {
        assert_eq!(
            set(" CS_A , ,02384a60fda6f2d0,"),
            vec!["CS_A", "02384a60fda6f2d0"]
        );
        assert!(set("").is_empty());
        assert!(set(" , ").is_empty());
    }

    #[test]
    fn a_program_is_named_by_its_name_or_its_hash() {
        let inject = b"\0DXBC..CS_ScreenProbes_Inject_Wrapped\0".to_vec();
        let defrag = b"\0DXBC..CS_DefragHeap_ComputeRelocations\0".to_vec();
        let other = b"\0DXBC..CS_ScreenProbes_TemporalResolve\0".to_vec();
        let names = set("CS_ScreenProbes_Inject,02384a60fda6f2d0");
        assert_eq!(
            named(&names, "1111111111111111", &inject),
            Some(Zero::Inject)
        );
        assert_eq!(
            named(&names, "02384a60fda6f2d0", &defrag),
            Some(Zero::Plain)
        );
        assert_eq!(
            named(&names, "02384A60FDA6F2D0", &defrag),
            Some(Zero::Plain)
        );
        assert_eq!(named(&names, "2222222222222222", &defrag), None);
        assert_eq!(named(&names, "3333333333333333", &other), None);
        // By name too; and an inject named by its hash is still the inject.
        let by_name = set("CS_DefragHeap_ComputeRelocations");
        assert_eq!(
            named(&by_name, "2222222222222222", &defrag),
            Some(Zero::Plain)
        );
        assert_eq!(
            named(&set("1111111111111111"), "1111111111111111", &inject),
            Some(Zero::Inject)
        );
    }

    #[test]
    fn nothing_is_named_without_entries() {
        let inject = b"CS_ScreenProbes_Inject".to_vec();
        assert_eq!(named(&[], "1111111111111111", &inject), None);
        // A 16-character entry that is not hex is a name, not a hash.
        assert!(!is_hash("CS_Defrag_Heap_X"));
        assert!(is_hash("02384a60fda6f2d0"));
    }
}
