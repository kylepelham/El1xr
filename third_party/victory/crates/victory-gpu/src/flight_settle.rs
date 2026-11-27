//! Wider flight settle room (`VICTORY_SETTLE_ROOM`, ported from
//! `fxceless/victory-fork` `06702fe86b`).
//!
//! Gives in-flight submissions twice the list-count headroom (`late_most`:
//! 256 -> 512) and eight times the changed-run merge budget under shader writes
//! (`under_writes_runs`: 512 -> 4096) before forcing a settle wait on the GPU.
//! Explicit `VICTORY_LATE_WAIT_LISTS`, `VICTORY_UNDER_WRITES_RUNS`, and
//! `VICTORY_MERGE_RUNS` overrides are always respected.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Instant;

/// Default maximum lists in flight before `WHY_MOST` settles them.
const BASE_LATE_LISTS: usize = 256;
/// Default maximum changed runs merged under flights' shader writes before
/// `WHY_STALE` settles them.
const BASE_MERGE_RUNS: usize = 512;
/// Hard ceiling for `under_writes_runs` (matches `1 << 16`).
const MAX_MERGE_RUNS: usize = 1 << 16;

const MODE_OFF: u8 = 0;
const MODE_ON: u8 = 1;
const MODE_ALT: u8 = 2;

/// Parses `VICTORY_SETTLE_ROOM`: `"0"` off (base caps), `"alt"` 20 s on/off,
/// unset or `"1"` on by default.
fn parse_settle_room(value: Option<&str>) -> u8 {
    match value.map(str::trim) {
        Some("0") => MODE_OFF,
        Some("alt") => MODE_ALT,
        _ => MODE_ON,
    }
}

/// Parses an explicit `VICTORY_LATE_WAIT_LISTS` override, or returns the base
/// default with `explicit = false`.
fn parse_late_lists(value: Option<&str>) -> (bool, usize) {
    match value.and_then(|v| v.trim().parse::<usize>().ok()) {
        Some(n) => (true, n.max(1)),
        None => (false, BASE_LATE_LISTS),
    }
}

/// Applies `settle_room` scaling (`x2`) to [`parse_late_lists`] when no
/// explicit override was given.
fn effective_late_most((explicit, base): (bool, usize), room_on: bool) -> usize {
    if explicit || !room_on {
        base
    } else {
        base.saturating_mul(2)
    }
}

/// Parses an explicit `VICTORY_UNDER_WRITES_RUNS` / `VICTORY_MERGE_RUNS`
/// override (clamped to `1..=65536`), or returns the base default with
/// `explicit = false`.
fn parse_merge_runs(primary: Option<&str>, fallback: Option<&str>) -> (bool, usize) {
    let parsed = primary
        .or(fallback)
        .and_then(|v| v.trim().parse::<usize>().ok())
        .map(|n| n.clamp(1, MAX_MERGE_RUNS));
    match parsed {
        Some(n) => (true, n),
        None => (false, BASE_MERGE_RUNS),
    }
}

/// Applies `settle_room` scaling (`x8`, capped at `65536`) to
/// [`parse_merge_runs`] when no explicit override was given.
fn effective_under_writes_runs((explicit, base): (bool, usize), room_on: bool) -> usize {
    if explicit || !room_on {
        base
    } else {
        base.saturating_mul(8).min(MAX_MERGE_RUNS)
    }
}

/// Whether flights get twice the room before "lists in the air" settles them
/// ([`late_most`]: 256 -> 512) and eight times the changed runs before the
/// merge under shaders' writes gives up and settles them ([`under_writes_runs`]:
/// 512 -> 4096) (`VICTORY_SETTLE_ROOM`: on by default; `0`: off, the base
/// caps; `alt`: twenty seconds with, twenty without, in turn, each change said
/// in the log). Explicit `VICTORY_LATE_WAIT_LISTS`, `VICTORY_UNDER_WRITES_RUNS`,
/// or `VICTORY_MERGE_RUNS` overrides are always respected.
pub(crate) fn settle_room() -> bool {
    static MODE: OnceLock<u8> = OnceLock::new();
    match *MODE
        .get_or_init(|| parse_settle_room(std::env::var("VICTORY_SETTLE_ROOM").ok().as_deref()))
    {
        MODE_OFF => false,
        MODE_ON => true,
        _ => {
            static EPOCH: OnceLock<Instant> = OnceLock::new();
            let on = (EPOCH.get_or_init(Instant::now).elapsed().as_secs() / 20) % 2 == 1;
            static LAST: AtomicU8 = AtomicU8::new(2);
            if LAST.swap(u8::from(on), Ordering::Relaxed) != u8::from(on) {
                tracing::info!(
                    "flights get more room (lists in the air x2, changed runs merged x8) before they are settled from now (VICTORY_SETTLE_ROOM=alt): {}",
                    if on { "ON" } else { "off, the caps as set" }
                );
            }
            on
        }
    }
}

/// Maximum host command lists kept in flight before `WHY_MOST` settles them.
pub(crate) fn late_most() -> usize {
    static MOST: OnceLock<(bool, usize)> = OnceLock::new();
    let cfg = *MOST
        .get_or_init(|| parse_late_lists(std::env::var("VICTORY_LATE_WAIT_LISTS").ok().as_deref()));
    effective_late_most(cfg, settle_room())
}

/// Maximum changed runs of title bytes a submission merges into card copies
/// under flights' shader writes before settling them (`VICTORY_UNDER_WRITES_RUNS`
/// / `VICTORY_MERGE_RUNS`, default 512, or 4096 with [`settle_room`]).
pub(crate) fn under_writes_runs() -> usize {
    static RUNS: OnceLock<(bool, usize)> = OnceLock::new();
    let cfg = *RUNS.get_or_init(|| {
        parse_merge_runs(
            std::env::var("VICTORY_UNDER_WRITES_RUNS").ok().as_deref(),
            std::env::var("VICTORY_MERGE_RUNS").ok().as_deref(),
        )
    });
    effective_under_writes_runs(cfg, settle_room())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_settle_room_modes() {
        assert_eq!(parse_settle_room(None), MODE_ON);
        assert_eq!(parse_settle_room(Some("1")), MODE_ON);
        assert_eq!(parse_settle_room(Some("0")), MODE_OFF);
        assert_eq!(parse_settle_room(Some(" 0 ")), MODE_OFF);
        assert_eq!(parse_settle_room(Some("alt")), MODE_ALT);
    }

    #[test]
    fn late_most_doubles_default_and_respects_explicit_override() {
        let default_cfg = parse_late_lists(None);
        assert_eq!(default_cfg, (false, 256));
        assert_eq!(effective_late_most(default_cfg, false), 256);
        assert_eq!(effective_late_most(default_cfg, true), 512);

        let explicit = parse_late_lists(Some("1024"));
        assert_eq!(explicit, (true, 1024));
        assert_eq!(effective_late_most(explicit, false), 1024);
        assert_eq!(effective_late_most(explicit, true), 1024);

        assert_eq!(parse_late_lists(Some("0")), (true, 1));
    }

    #[test]
    fn under_writes_runs_scales_default_and_respects_overrides() {
        let default_cfg = parse_merge_runs(None, None);
        assert_eq!(default_cfg, (false, 512));
        assert_eq!(effective_under_writes_runs(default_cfg, false), 512);
        assert_eq!(effective_under_writes_runs(default_cfg, true), 4096);

        let primary = parse_merge_runs(Some("100000"), Some("256"));
        assert_eq!(primary, (true, MAX_MERGE_RUNS));
        assert_eq!(effective_under_writes_runs(primary, true), MAX_MERGE_RUNS);

        let fallback = parse_merge_runs(None, Some("2048"));
        assert_eq!(fallback, (true, 2048));
        assert_eq!(effective_under_writes_runs(fallback, false), 2048);
        assert_eq!(effective_under_writes_runs(fallback, true), 2048);

        assert_eq!(parse_merge_runs(Some("0"), None), (true, 1));
    }
}
