//! (popin, #55) `VICTORY_SWEEP_FAST=1` (or `=<cap>`, live; off unless set):
//! lifts the per-chunk put-back cap and cadence of [`super::HostHeap::texture_sweep`]
//! and resumes mid-chunk when the cap is hit.
//!
//! In batch 5 part V (`01_sv.log`, `VICTORY_VIEW_AGES=1`), `texture_sweep` scanned
//! 32,768 slots every 100 ms (~2.85 s per 1M-slot lap) with `MOST = 128` put-backs
//! per chunk, dropping the remaining orphaned `Claim::Free` placed textures in that
//! chunk for a full lap even while the upload queue was nearly idle (~210 slots,
//! 0.4 ms/frame). After a streaming burst left ~2,500–9,800 orphaned textures in
//! chunks 1–3 (`32k..131k`), dripping 128 per 2.85 s lap held real BC7/BC1 world
//! textures black (`Null`) or stale (`KeptOther`/`KeptSame`) for 43–116 seconds.
//!
//! With `VICTORY_SWEEP_FAST=1`:
//! - `MOST` rises from 128 to 2048 (or the given integer clamped to `128..=4000`,
//!   still bounded by the queue's `SHORT = 4000` cap).
//! - `EVERY_MS` drops from 100 ms to 50 ms (~1.5 s per 1M-slot lap).
//! - When a chunk hits the cap before finishing, the sweep cursor resumes at the
//!   next slot instead of skipping the rest of the chunk for a full lap.
//!
//! Metric line: `sweep fast:` (once per lap when put-backs occurred or every 5 s).

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};

/// Default per-chunk put-back cap when `VICTORY_SWEEP_FAST` is off.
pub(super) const DEFAULT_MOST: u32 = 128;
/// Default interval between sweep steps when `VICTORY_SWEEP_FAST` is off.
pub(super) const DEFAULT_EVERY_MS: u64 = 100;
/// Default per-chunk put-back cap when `VICTORY_SWEEP_FAST=1`.
pub(super) const FAST_MOST: u32 = 2048;
/// Maximum per-chunk put-back cap (matches `SHORT` in `sweep_part`).
pub(super) const MAX_MOST: u32 = 4000;
/// Interval between sweep steps when `VICTORY_SWEEP_FAST` is on.
pub(super) const FAST_EVERY_MS: u64 = 50;

fn parse(value: Option<&str>) -> u64 {
    let Some(raw) = value.map(str::trim).filter(|s| !s.is_empty()) else {
        return 0;
    };
    match raw.parse::<u32>() {
        Ok(0) => 0,
        Ok(1) => u64::from(FAST_MOST),
        Ok(n) => u64::from(n.clamp(DEFAULT_MOST, MAX_MOST)),
        Err(_) => 0,
    }
}

/// Active per-chunk put-back cap when `VICTORY_SWEEP_FAST` is set (`None` when off).
pub(super) fn cap() -> Option<u32> {
    static CAP: victory_core::settings::Live =
        victory_core::settings::Live::new("VICTORY_SWEEP_FAST", parse);
    let v = CAP.get() as u32;
    (v > 0).then_some(v)
}

/// Whether `VICTORY_SWEEP_FAST` is enabled.
#[cfg(test)]
pub(super) fn on() -> bool {
    cap().is_some()
}

/// Interval between `texture_sweep` steps, in milliseconds.
pub(super) fn every_ms() -> u64 {
    if cap().is_some() {
        FAST_EVERY_MS
    } else {
        DEFAULT_EVERY_MS
    }
}

/// Maximum orphaned slots put back into the texture queue in one sweep step.
pub(super) fn most() -> u32 {
    cap().unwrap_or(DEFAULT_MOST)
}

/// Whether a sweep step that reaches [`most`] resumes at the next slot
/// instead of skipping the rest of the 32,768-slot chunk for a full lap.
pub(super) fn resume_on_cap() -> bool {
    cap().is_some()
}

#[derive(Default)]
struct Stats {
    laps: u64,
    lap_start_ms: u64,
    last_log_ms: u64,
    lap_put_back: u64,
    total_put_back: u64,
    lap_max_chunk: u32,
    lap_resumes: u64,
    total_resumes: u64,
}

static STATS: Mutex<Stats> = Mutex::new(Stats {
    laps: 0,
    lap_start_ms: 0,
    last_log_ms: 0,
    lap_put_back: 0,
    total_put_back: 0,
    lap_max_chunk: 0,
    lap_resumes: 0,
    total_resumes: 0,
});

/// Records one `texture_sweep` step while `VICTORY_SWEEP_FAST` is active and
/// logs a `sweep fast:` summary on lap completion.
pub(super) fn note_step(back: u32, resumed: bool, now_ms: u64, lap_done: bool) {
    let Some(active_cap) = cap() else {
        return;
    };
    static SAID: AtomicBool = AtomicBool::new(false);
    if !SAID.swap(true, Relaxed) {
        tracing::info!(
            cap = active_cap,
            every_ms = FAST_EVERY_MS,
            resume = true,
            "fast texture sweep on (VICTORY_SWEEP_FAST)"
        );
    }
    let mut s = STATS.lock().unwrap_or_else(|e| e.into_inner());
    if s.lap_start_ms == 0 {
        s.lap_start_ms = now_ms;
    }
    s.lap_put_back += u64::from(back);
    s.total_put_back += u64::from(back);
    s.lap_max_chunk = s.lap_max_chunk.max(back);
    if resumed {
        s.lap_resumes += 1;
        s.total_resumes += 1;
    }
    if lap_done {
        s.laps += 1;
        let lap_ms = now_ms.saturating_sub(s.lap_start_ms);
        let should_log =
            s.laps == 1 || s.lap_put_back > 0 || now_ms.saturating_sub(s.last_log_ms) >= 5000;
        if should_log {
            tracing::info!(
                lap = s.laps,
                lap_ms,
                put_back = s.lap_put_back,
                total_put_back = s.total_put_back,
                max_chunk = s.lap_max_chunk,
                resumes = s.lap_resumes,
                total_resumes = s.total_resumes,
                cap = active_cap,
                "sweep fast:"
            );
            s.last_log_ms = now_ms;
        }
        s.lap_start_ms = now_ms;
        s.lap_put_back = 0;
        s.lap_max_chunk = 0;
        s.lap_resumes = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_switch_is_live_and_off_by_default() {
        assert!(!on());
        assert_eq!(every_ms(), DEFAULT_EVERY_MS);
        assert_eq!(most(), DEFAULT_MOST);
        assert!(!resume_on_cap());
        victory_core::settings::check_live("VICTORY_SWEEP_FAST", "1", on, false, true);
    }

    #[test]
    fn parse_accepts_flag_and_clamped_cap() {
        assert_eq!(parse(None), 0);
        assert_eq!(parse(Some("")), 0);
        assert_eq!(parse(Some("0")), 0);
        assert_eq!(parse(Some("bogus")), 0);
        assert_eq!(parse(Some("1")), u64::from(FAST_MOST));
        assert_eq!(parse(Some("32")), u64::from(DEFAULT_MOST));
        assert_eq!(parse(Some("1024")), 1024);
        assert_eq!(parse(Some("99999")), u64::from(MAX_MOST));
    }
}
