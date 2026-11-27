//! Opt-in copy-back skip for write-combining title memory with small-range keep
//! (`VICTORY_SKIP_WC_BACK=1`, `VICTORY_SKIP_WC_KEEP=<bytes>`).
//!
//! Ported from Peppacodes/playablegames-peppa commits `6035addd51`,
//! `3cc0a8d63e`, and `ca2432bd64` (originally fxceless/victory-fork #868 and
//! #1379). Off by default (`VICTORY_SKIP_WC_BACK=1` enables it).
//!
//! When enabled, shader writes to card copies whose declared guest range lies
//! wholly inside a single `XMemVirtualAlloc` reservation that the title asked
//! for with `PAGE_WRITECOMBINE` (`0x400`) are left on the card instead of
//! copied back into title memory, EXCEPT declared ranges of at most
//! `VICTORY_SKIP_WC_KEEP` bytes (default 1 MiB — small CPU-read results such
//! as exposure/occlusion stay fetched so the picture remains intact).

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// Console `PAGE_WRITECOMBINE` protection bit recorded before host masking.
const PAGE_WRITECOMBINE: u32 = 0x400;

/// Default `VICTORY_SKIP_WC_KEEP` threshold: 1 MiB (per fxceless #1379 and
/// Peppacodes PR #17/#18, keeps small CPU-read results fetched back while
/// skipping bulk WC readbacks). Set `VICTORY_SKIP_WC_KEEP=0` to keep none.
pub(crate) const DEFAULT_KEEP_BYTES: usize = 1 << 20;

/// A reservation of `XMemVirtualAlloc`, as the title asked for it.
struct Reservation {
    len: usize,
    protect: u32,
}

static RESERVATIONS: Mutex<BTreeMap<usize, Reservation>> = Mutex::new(BTreeMap::new());

/// Pieces left on the card by [`skip_back`]: count and bytes.
static SKIPPED: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

/// Whether `VICTORY_SKIP_WC_BACK=1` is active (off by default).
pub(crate) fn skip_wc_back() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        let on = std::env::var_os("VICTORY_SKIP_WC_BACK").is_some_and(|v| v == "1");
        if on {
            let keep = keep_bytes();
            tracing::warn!(
                keep_bytes = keep,
                "VICTORY_SKIP_WC_BACK=1: shader writes to write-combining title memory larger than \
                 {keep} bytes (VICTORY_SKIP_WC_KEEP) are NOT copied back (experiment)"
            );
        }
        on
    })
}

fn parse_keep(raw: Option<&str>) -> usize {
    raw.and_then(|v| v.trim().parse().ok())
        .unwrap_or(DEFAULT_KEEP_BYTES)
}

/// `VICTORY_SKIP_WC_KEEP=<bytes>`: declared ranges of at most this many bytes
/// are still fetched back even when inside write-combining title memory.
pub(crate) fn keep_bytes() -> usize {
    static KEEP: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *KEEP.get_or_init(|| parse_keep(std::env::var("VICTORY_SKIP_WC_KEEP").ok().as_deref()))
}

/// Records that `XMemVirtualAlloc` reserved `[at, at + len)` with the title's
/// original console `protect` flags (before `HOST_PROTECTION` masking).
/// No-op when `VICTORY_SKIP_WC_BACK` is off.
pub fn title_memory_reserved(at: usize, len: usize, protect: u32) {
    if !skip_wc_back() || at == 0 || len == 0 {
        return;
    }
    let mut reservations = RESERVATIONS.lock().unwrap_or_else(|e| e.into_inner());
    insert_reservation(&mut reservations, at, len, protect);
}

fn insert_reservation(
    reservations: &mut BTreeMap<usize, Reservation>,
    at: usize,
    len: usize,
    protect: u32,
) {
    let Some(end) = at.checked_add(len) else {
        return;
    };
    // Evict any stale reservation that overlaps the newly reserved range
    // (for example after an earlier allocation at these addresses was freed).
    let stale: Vec<usize> = reservations
        .range(..end)
        .rev()
        .take_while(|&(&base, r)| base.saturating_add(r.len) > at)
        .map(|(&base, _)| base)
        .collect();
    for base in stale {
        reservations.remove(&base);
    }
    reservations.insert(at, Reservation { len, protect });
}

/// Every declared title range must lie wholly inside a single WC reservation.
/// A card copy (especially an aligned loose window) can cover several.
fn all_wc_in(reservations: &BTreeMap<usize, Reservation>, ranges: &[(usize, usize)]) -> bool {
    !ranges.is_empty()
        && ranges.iter().all(|&(from, to)| {
            // Seeking from the exclusive end also detects a new reservation
            // beginning inside the range. An allocation ending at `to` fits.
            from < to
                && reservations
                    .range(..to)
                    .next_back()
                    .is_some_and(|(&base, r)| {
                        base <= from
                            && r.protect & PAGE_WRITECOMBINE != 0
                            && base.checked_add(r.len).is_some_and(|end| to <= end)
                    })
        })
}

#[cfg(test)]
fn should_skip_in(
    reservations: &BTreeMap<usize, Reservation>,
    declared: (usize, usize),
    keep: usize,
) -> bool {
    if declared.1.saturating_sub(declared.0) <= keep {
        return false;
    }
    all_wc_in(reservations, &[declared])
}

/// Whether a flagged card-copy piece of `bytes` belonging to declared title
/// range `declared = (from, to)` should be skipped instead of copied back.
pub(crate) fn skip_back(declared: (usize, usize), bytes: u64) -> bool {
    if !skip_wc_back() {
        return false;
    }
    let keep = keep_bytes();
    if declared.1.saturating_sub(declared.0) <= keep {
        return false;
    }
    let wc = {
        let reservations = RESERVATIONS.lock().unwrap_or_else(|e| e.into_inner());
        all_wc_in(&reservations, &[declared])
    };
    if wc {
        let n = SKIPPED[0].fetch_add(1, Ordering::Relaxed) + 1;
        let b = SKIPPED[1].fetch_add(bytes, Ordering::Relaxed) + bytes;
        if n.is_power_of_two() {
            tracing::info!(
                "VICTORY_SKIP_WC_BACK: {n} pieces ({} MB) of write-combining memory left on the card so far",
                b >> 20
            );
        }
    }
    wc
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reservation(len: usize, protect: u32) -> Reservation {
        Reservation { len, protect }
    }

    #[test]
    fn a_write_in_the_ordinary_half_of_a_card_window_still_comes_back() {
        const MIB: usize = 1 << 20;
        let window = 8 * MIB;
        let middle = window + 2 * MIB;
        let map = BTreeMap::from([
            (window, reservation(2 * MIB, 0x404)),
            (middle, reservation(2 * MIB, 0x04)),
        ]);
        let wc = (middle - 65536, middle);
        let ordinary = (middle, middle + 65536);
        assert!(all_wc_in(&map, &[wc]));
        assert!(!all_wc_in(&map, &[ordinary]));
        assert!(!all_wc_in(&map, &[wc, ordinary]));
        assert!(!all_wc_in(&map, &[(wc.0, ordinary.1)]));
    }

    #[test]
    fn a_declaration_must_fit_one_known_reservation() {
        let map = BTreeMap::from([
            (0x10000, reservation(0x10000, 0x404)),
            (0x20000, reservation(0x10000, 0x404)),
            (usize::MAX - 8, reservation(16, 0x404)),
        ]);
        assert!(all_wc_in(&map, &[(0x10000, 0x20000), (0x20000, 0x30000)]));
        assert!(!all_wc_in(&map, &[(0x1ffff, 0x20001)]));
        assert!(!all_wc_in(&map, &[(0x30000, 0x30001)]));
        assert!(!all_wc_in(&map, &[(0x10000, 0x10000)]));
        assert!(!all_wc_in(&map, &[(usize::MAX - 8, 1)]));
        assert!(!all_wc_in(&map, &[(usize::MAX - 8, usize::MAX)]));
    }

    #[test]
    fn small_ranges_up_to_keep_threshold_are_still_fetched() {
        const MIB: usize = 1 << 20;
        let base = 16 * MIB;
        let map = BTreeMap::from([
            (base, reservation(8 * MIB, 0x404)),
            (base + 8 * MIB, reservation(8 * MIB, 0x04)),
        ]);
        // At or below 1 MiB keep threshold: fetched back even inside WC.
        assert!(!should_skip_in(
            &map,
            (base, base + 65536),
            DEFAULT_KEEP_BYTES
        ));
        assert!(!should_skip_in(
            &map,
            (base, base + MIB),
            DEFAULT_KEEP_BYTES
        ));
        // Strictly above 1 MiB keep threshold inside WC: skipped.
        assert!(should_skip_in(
            &map,
            (base, base + MIB + 1),
            DEFAULT_KEEP_BYTES
        ));
        assert!(should_skip_in(
            &map,
            (base, base + 4 * MIB),
            DEFAULT_KEEP_BYTES
        ));
        // Strictly above 1 MiB keep threshold outside WC: fetched back.
        assert!(!should_skip_in(
            &map,
            (base + 8 * MIB, base + 12 * MIB),
            DEFAULT_KEEP_BYTES
        ));
        // With keep == 0: even small WC ranges are skipped.
        assert!(should_skip_in(&map, (base, base + 4096), 0));
    }

    #[test]
    fn re_reservation_evicts_stale_overlapping_wc_entry() {
        const MIB: usize = 1 << 20;
        let base = 32 * MIB;
        let mut map = BTreeMap::new();
        insert_reservation(&mut map, base, 4 * MIB, 0x404);
        assert!(should_skip_in(
            &map,
            (base, base + 2 * MIB),
            DEFAULT_KEEP_BYTES
        ));
        // Address range reused as ordinary (non-WC) memory: must no longer skip.
        insert_reservation(&mut map, base, 4 * MIB, 0x04);
        assert!(!should_skip_in(
            &map,
            (base, base + 2 * MIB),
            DEFAULT_KEEP_BYTES
        ));
    }

    #[test]
    fn default_keep_is_one_mib_and_parse_handles_overrides() {
        assert_eq!(DEFAULT_KEEP_BYTES, 1_048_576);
        assert_eq!(parse_keep(None), 1_048_576);
        assert_eq!(parse_keep(Some("0")), 0);
        assert_eq!(parse_keep(Some("65536")), 65536);
        assert_eq!(parse_keep(Some(" 2097152 ")), 2_097_152);
        assert_eq!(parse_keep(Some("invalid")), 1_048_576);
    }
}
