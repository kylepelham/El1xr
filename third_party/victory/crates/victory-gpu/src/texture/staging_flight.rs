//! (racefix, issue #57) `VICTORY_STAGING_UNDER_FLIGHT=1` (live; off unless
//! set): counts and names the detilings that write a texture's staging
//! buffer while a submission that copies from it may still be on the card.
//!
//! A staging buffer is upload memory the prelude of a submission copies into
//! the host texture (`PreludeUpload::record`). A later detiling of the same
//! texture writes the same buffer on the CPU, on the submitting thread or a
//! thread of the prepare queue, with nothing that waits for the copies of
//! the submissions in the air: the flights' guard (`late_blocked`) is for
//! title memory their shaders write, and a staging buffer goes back to its
//! pool only once the lists that hold it have landed, which does not stop
//! it being written while still held. If the copy has not run when the
//! bytes change, the host texture takes the next frame's content a
//! submission early, or half of each (a torn texture, for a frame).
//!
//! Usually harmless: the title writes a texture's memory only after its
//! fence said the card is done with it, and our fences pass only once
//! flights land. This line tells whether it happens in the game: each
//! submission that recorded a copy from a staging buffer stamps it with its
//! number ([`copied`]); a detiling ([`detiled`]) of a buffer stamped by a
//! submission begun and not finished is counted, the first 32 named. Not
//! seen: a detiling by the prepare queue between a submission's stamp and
//! its start while an earlier copy still runs (the stamp is the newest).

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

static ON: victory_core::settings::Live =
    victory_core::settings::Live::new("VICTORY_STAGING_UNDER_FLIGHT", parse);

fn parse(value: Option<&str>) -> u64 {
    u64::from(value.is_some_and(|v| v.trim() == "1"))
}

pub(super) fn on() -> bool {
    ON.get() != 0
}

/// The submission being prepared recorded a copy from the staging buffer
/// whose stamp is `sent`: called on the submitting thread under the
/// submission lock, before the submission counts as started.
pub(super) fn copied(sent: &AtomicU64) {
    if on() {
        sent.store(crate::host::submissions_started() + 1, Relaxed);
    }
}

/// Whether a detiling now writes under a copy that may still run: the
/// buffer was stamped by a submission begun (`started`), the one being
/// prepared not counted, and not finished.
pub(super) fn under(sent: u64, started: u64, finished: u64) -> bool {
    sent != 0 && sent <= started && sent > finished
}

/// Detilings since the last line: all, under a copy that may still run,
/// bytes of those.
static COUNTS: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

/// A detiling of `bytes` into the staging buffer of title texture
/// `resource` stamped `sent`.
pub(super) fn detiled(resource: usize, sent: &AtomicU64, bytes: usize) {
    if !on() {
        return;
    }
    COUNTS[0].fetch_add(1, Relaxed);
    let sent = sent.load(Relaxed);
    let started = crate::host::submissions_started();
    let finished = crate::host::submissions_finished();
    if under(sent, started, finished) {
        let n = COUNTS[1].fetch_add(1, Relaxed) + 1;
        COUNTS[2].fetch_add(bytes as u64, Relaxed);
        static NAMED: AtomicU64 = AtomicU64::new(0);
        if NAMED.fetch_add(1, Relaxed) < 32 {
            tracing::info!(
                n,
                resource = format_args!("{resource:#x}"),
                bytes,
                copied_by = sent,
                started,
                finished,
                frame = crate::frame::current(),
                "staging under flight: a texture's staging buffer is detiled while the submission that copies from it may still be on the card (VICTORY_STAGING_UNDER_FLIGHT)"
            );
        }
    }
    line();
}

/// The counts every 300 frames.
fn line() {
    static LAST: AtomicU64 = AtomicU64::new(0);
    let frame = crate::frame::current();
    let last = LAST.load(Relaxed);
    if frame < last + 300
        || LAST
            .compare_exchange(last, frame, Relaxed, Relaxed)
            .is_err()
    {
        return;
    }
    let [detilings, under, bytes] = COUNTS.each_ref().map(|c| c.swap(0, Relaxed));
    tracing::info!(
        frame,
        detilings,
        under_flight = under,
        under_kib = bytes >> 10,
        "staging under flight: detilings since the last line, and those that wrote a staging buffer a submission still on the card may copy from"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_switch_is_off_unless_one() {
        assert_eq!(parse(None), 0);
        assert_eq!(parse(Some("0")), 0);
        assert_eq!(parse(Some(" 1")), 1);
    }

    #[test]
    fn only_a_begun_unfinished_copy_counts() {
        assert!(!under(0, 9, 3), "never copied");
        assert!(!under(10, 9, 3), "the submission being prepared");
        assert!(under(9, 9, 3));
        assert!(under(4, 9, 3));
        assert!(!under(3, 9, 3), "finished");
    }

    /// A model of the queue: submissions are prepared (each may copy from
    /// the buffer, stamping it with the number it will take), begun, and
    /// finished in order; detilings come between. A detiling counts as
    /// under a copy exactly when a copy recorded by a submission that has
    /// begun has not finished.
    #[test]
    fn stress_the_count_matches_a_model_of_the_queue() {
        let mut rng = 0x2545_f491_4f6c_dd1du64;
        let mut next = || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };
        let (mut started, mut finished, mut sent) = (0u64, 0u64, 0u64);
        let mut copies: Vec<u64> = Vec::new();
        let (mut hits, mut misses) = (0u64, 0u64);
        for _ in 0..100_000 {
            match next() % 4 {
                0 => {
                    // Prepared: a detiling during preparation sees its own
                    // copy not yet begun.
                    if next() % 2 == 0 {
                        sent = started + 1;
                        copies.push(sent);
                        assert!(!under(sent, started, finished));
                    }
                    started += 1;
                }
                1 => finished = (finished + 1).min(started),
                _ => {
                    let truth = copies.iter().any(|&c| c <= started && c > finished);
                    assert_eq!(under(sent, started, finished), truth);
                    if truth { hits += 1 } else { misses += 1 }
                }
            }
            copies.retain(|&c| c > finished);
        }
        assert!(hits > 1000 && misses > 1000, "{hits} {misses}");
    }
}
