//! (feedcard, #88) `VICTORY_SCRATCH_AT_LANDING=1` (Live; off by default): a
//! held list (`crate::list::Detached`) gives its scratch chunks to the spare
//! pool when its submission lands, next to its read-backs, instead of at its
//! reset after the bury lag (16 submissions, 64 on a PC of more than 32 GB).
//! The card has read them for the last time once the submission landed. The
//! chunks the pool does not take (past `VICTORY_SCRATCH_SPARE_MB`) stay with
//! the list and are freed at its reset as before: nothing is freed earlier.
//! Only with shared chunks (`VICTORY_SCRATCH_POOL`, on by default).
//!
//! Measured upstream (memory bench, RTX 4070 Ti, `VICTORY_RELAY=2`): 42
//! landed lists held 130.5 MB of 4 MiB chunks; with it, 0, private bytes
//! 591 -> 467 MB, upload bytes made 184 -> 63 MB. Here: fewer new 4 MiB
//! upload buffers per frame (each about 1 ms to make, 2 ms to free) when the
//! spares run dry.
//!
//! After trustno1-coder/VIctory-NVIDIA PR #112 (simbaui4life, `4ad8b9b`).
//! Not in the cache key: no shader or pipeline output.

use std::sync::atomic::{AtomicU64, Ordering};

static ON: victory_core::settings::Live =
    victory_core::settings::Live::flag("VICTORY_SCRATCH_AT_LANDING");

#[cfg(test)]
thread_local! {
    /// Tests: the switch for calls made on this thread.
    pub(crate) static FORCED: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

/// Live: read per landed list.
pub(crate) fn on() -> bool {
    #[cfg(test)]
    if let Some(on) = FORCED.with(std::cell::Cell::get) {
        return on;
    }
    ON.get_bool()
}

/// Since the last line: chunks and bytes given back at landing, chunks the
/// pool had no room for (kept to the reset).
static COUNTS: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

/// Offers `chunks` to the pool through `give` (back: no room); what it does
/// not take is given back to stay with the list.
pub(crate) fn spare_what_fits<C>(
    chunks: Vec<(C, u64)>,
    mut give: impl FnMut(C, u64) -> Option<C>,
) -> Vec<(C, u64)> {
    let mut kept = Vec::new();
    for (chunk, size) in chunks {
        match give(chunk, size) {
            None => {
                COUNTS[0].fetch_add(1, Ordering::Relaxed);
                COUNTS[1].fetch_add(size, Ordering::Relaxed);
            }
            Some(chunk) => {
                COUNTS[2].fetch_add(1, Ordering::Relaxed);
                kept.push((chunk, size));
            }
        }
    }
    kept
}

/// With the batch flush's lines.
pub(crate) fn line(frames: f64) {
    let [given, bytes, kept] =
        [0, 1, 2].map(|i| COUNTS[i].swap(0, Ordering::Relaxed) as f64 / frames);
    if on() {
        tracing::info!(
            "scratch given back at landing per frame (VICTORY_SCRATCH_AT_LANDING=1): {given:.1} chunks, {:.1} MB; \
             {kept:.1} chunks kept to the reset (spares full)",
            bytes / 1e6
        );
    }
}

#[cfg(test)]
mod tests {
    use super::spare_what_fits;

    #[test]
    fn the_pool_takes_what_fits_and_the_rest_stays_in_order() {
        let mut pool = Vec::new();
        let mut room = 10u64;
        let kept = spare_what_fits(vec![("a", 4), ("b", 4), ("c", 4), ("d", 2)], |c, s| {
            if s > room {
                return Some(c);
            }
            room -= s;
            pool.push(c);
            None
        });
        assert_eq!(pool, vec!["a", "b", "d"]);
        assert_eq!(kept, vec![("c", 4)]);
    }

    #[test]
    fn nothing_to_give_keeps_nothing() {
        let kept = spare_what_fits(Vec::<(u8, u64)>::new(), |c, _| Some(c));
        assert!(kept.is_empty());
    }
}
