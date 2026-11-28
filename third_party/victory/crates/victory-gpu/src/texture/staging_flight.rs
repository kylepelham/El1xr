//! (racefix, issue #57) A texture's staging buffer written while a
//! submission that copies from it may still be on the card.
//!
//! A staging buffer is upload memory the prelude of a submission copies into
//! the host texture (`PreludeUpload::record`). A later detiling of the same
//! texture writes the same buffer on the CPU, with nothing that waits for
//! the copies of the submissions in the air: the flights' guard
//! (`late_blocked`) is for title memory their shaders write, and a staging
//! buffer goes back to its pool only once the lists that hold it have
//! landed, which does not stop it being written while still held. If the
//! copy has not run when the bytes change, the host texture takes the next
//! frame's content a submission early, or half of each (a torn texture, for
//! a frame). The same holds inside one submission: a list's prelude copy
//! recorded before a title write and a detiling after it (`title_written`
//! checks textures again) both read the one buffer when the card runs them.
//!
//! Each submission that records a copy from a staging buffer stamps it with
//! its number ([`Flight::recorded`]). The number is taken
//! (`submissions_started`) before the submission's preludes are recorded, so
//! a stamp is never ahead of the started count: a buffer may be read by the
//! card exactly while its newest stamp is above the finished count.
//!
//! `VICTORY_STAGING_UNDER_FLIGHT=1` (live; diag): counts and names the
//! detilings that write a buffer in that state, the first 32 named.
//!
//! `VICTORY_STAGING_FLIGHT_WAIT=1` (live; fix): such a detiling goes into
//! another buffer of the same layout instead ([`Flight::target`]): one of up
//! to [`SPARES`] spares the staging state keeps once made (from the staging
//! pool, given back with it), the first whose copies have all finished.
//! The buffer written becomes the one the next prelude copies from, so the
//! copies already recorded read what they were recorded with, whole. Nothing
//! waits: with every spare still read, or the spares' bytes at
//! [`SPARE_BUDGET`], the buffer is written in place as before (counted).
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::OnceLock;

use windows::Win32::Graphics::Direct3D12::ID3D12Resource;

static ON: victory_core::settings::Live =
    victory_core::settings::Live::new("VICTORY_STAGING_UNDER_FLIGHT", parse);

static FIX: victory_core::settings::Live =
    victory_core::settings::Live::new("VICTORY_STAGING_FLIGHT_WAIT", parse);

fn parse(value: Option<&str>) -> u64 {
    u64::from(value.is_some_and(|v| v.trim() == "1"))
}

pub(super) fn on() -> bool {
    ON.get() != 0
}

fn fix() -> bool {
    FIX.get() != 0
}

/// Spare buffers a staging state may make besides its own.
pub(super) const SPARES: usize = 3;

/// The most bytes all spares together hold (each counted at its staging
/// buffer's bytes); past it a detiling under a copy writes in place.
pub(super) const SPARE_BUDGET: u64 = 512 << 20;

/// The bytes of the spares alive.
static SPARE_BYTES: AtomicU64 = AtomicU64::new(0);

/// A spare buffer: an upload buffer of the staging buffer's bytes and its
/// mapping.
struct Spare {
    buffer: ID3D12Resource,
    data: usize,
    bytes: u64,
}

/// A staging state's buffers as the copies see them: the stamp of each
/// (slot 0 its own buffer, then the spares), the one detiled last, and the
/// spares made.
pub(super) struct Flight {
    sent: [AtomicU64; SPARES + 1],
    current: AtomicUsize,
    spares: [OnceLock<Spare>; SPARES],
}

impl Flight {
    pub(super) fn new() -> Self {
        Self {
            sent: [const { AtomicU64::new(0) }; SPARES + 1],
            current: AtomicUsize::new(0),
            spares: [const { OnceLock::new() }; SPARES],
        }
    }

    /// The submission being prepared records a copy from the buffer
    /// detiled last: called on the submitting thread under the submission
    /// lock, after the submission took its number. The slot to copy from.
    pub(super) fn recorded(&self) -> usize {
        let slot = self.current.load(std::sync::atomic::Ordering::Acquire);
        if on() || fix() {
            self.sent[slot].store(crate::host::submissions_started(), std::sync::atomic::Ordering::Release);
        }
        slot
    }

    /// The buffer and offset of `slot` (`own`: slot 0's).
    pub(super) fn buffer<'a>(&'a self, slot: usize, own: (&'a ID3D12Resource, u64)) -> (&'a ID3D12Resource, u64) {
        match slot.checked_sub(1).and_then(|i| self.spares[i].get()) {
            Some(spare) => (&spare.buffer, 0),
            None => own,
        }
    }

    /// Where a detiling writes, with the staging state's `last` locked:
    /// the slot and its mapping (`own`: slot 0's). With
    /// `VICTORY_STAGING_FLIGHT_WAIT=1`, when the last detiled buffer may
    /// still be read by a copy, another one whose copies all finished,
    /// made with `make` (an upload buffer of the staging buffer's layout,
    /// its mapping and bytes) if need be. [`Flight::written`] once done.
    pub(super) fn target(
        &self,
        own: *mut u8,
        make: impl FnOnce() -> Option<(ID3D12Resource, *mut u8, u64)>,
    ) -> (usize, *mut u8) {
        let current = self.current.load(Relaxed);
        let at = |slot: usize| match slot {
            0 => Some(own),
            _ => self.spares[slot - 1].get().map(|s| s.data as *mut u8),
        };
        let mapped = (current, at(current).unwrap_or(own));
        if !fix() {
            return mapped;
        }
        let sent = |slot: usize| self.sent[slot].load(std::sync::atomic::Ordering::Acquire);
        let room = SPARE_BYTES.load(Relaxed) < SPARE_BUDGET;
        let started = crate::host::submissions_started();
        let finished = crate::host::submissions_finished();
        match pick(current, sent, |s| at(s).is_some(), started, finished, room) {
            Pick::Here => mapped,
            Pick::Other(slot) => {
                STATS[0].fetch_add(1, Relaxed);
                (slot, at(slot).unwrap_or(own))
            }
            Pick::Make(slot) => match make() {
                Some((buffer, data, bytes)) => {
                    SPARE_BYTES.fetch_add(bytes, Relaxed);
                    STATS[0].fetch_add(1, Relaxed);
                    STATS[1].fetch_add(1, Relaxed);
                    STATS[2].fetch_add(bytes, Relaxed);
                    let _ = self.spares[slot - 1].set(Spare {
                        buffer,
                        data: data as usize,
                        bytes,
                    });
                    (slot, data)
                }
                None => {
                    STATS[3].fetch_add(1, Relaxed);
                    mapped
                }
            },
            Pick::InPlace => {
                STATS[3].fetch_add(1, Relaxed);
                mapped
            }
        }
    }

    /// `slot` was detiled whole: the next prelude copies from it. Whether
    /// that moved off the buffer the copies read before (the texture is
    /// then due again: a list that took the change before the move copies
    /// the old buffer).
    pub(super) fn written(&self, slot: usize) -> bool {
        self.current.swap(slot, std::sync::atomic::Ordering::Release) != slot
    }

    /// The stamp of the buffer detiled last.
    fn current_sent(&self) -> u64 {
        self.sent[self.current.load(Relaxed)].load(std::sync::atomic::Ordering::Acquire)
    }

    /// The spares made, as (buffer, mapping, bytes): for the staging
    /// state's drop, once no list holds it.
    pub(super) fn take_spares(&mut self) -> Vec<(ID3D12Resource, usize, u64)> {
        self.spares
            .iter_mut()
            .filter_map(OnceLock::take)
            .map(|s| {
                SPARE_BYTES.fetch_sub(s.bytes, Relaxed);
                (s.buffer, s.data, s.bytes)
            })
            .collect()
    }
}

/// Where a detiling under [`Flight::target`] goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pick {
    /// The buffer detiled last: no copy may still read it.
    Here,
    /// A spare made before whose copies all finished.
    Other(usize),
    /// A spare to make in this slot.
    Make(usize),
    /// Every buffer may still be read and no spare can be made: in place.
    InPlace,
}

/// [`Flight::target`]'s choice: from `current`, the slots' stamps `sent`,
/// whether a slot has a buffer (`made`), the submission counts and whether
/// the budget has room for one more spare.
fn pick(
    current: usize,
    sent: impl Fn(usize) -> u64,
    made: impl Fn(usize) -> bool,
    started: u64,
    finished: u64,
    room: bool,
) -> Pick {
    let busy = |slot: usize| under(sent(slot), started, finished);
    if !busy(current) {
        return Pick::Here;
    }
    let others = || (1..=SPARES).map(|k| (current + k) % (SPARES + 1));
    if let Some(slot) = others().find(|&s| made(s) && !busy(s)) {
        return Pick::Other(slot);
    }
    match others().find(|&s| !made(s)) {
        Some(slot) if room => Pick::Make(slot),
        _ => Pick::InPlace,
    }
}

/// Whether a buffer stamped `sent` may still be read by a copy: stamped by
/// a submission that took its number (`started`) and is not finished.
pub(super) fn under(sent: u64, started: u64, finished: u64) -> bool {
    sent != 0 && sent <= started && sent > finished
}

/// Detilings since the last line: all, under a copy that may still run,
/// bytes of those.
static COUNTS: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

/// `VICTORY_STAGING_FLIGHT_WAIT` since the last line: detilings moved to
/// another buffer, spares made, their bytes, detilings under a copy written
/// in place (no free spare or the budget spent).
static STATS: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

/// A detiling of `bytes` into the staging buffer of title texture
/// `resource` whose buffers are `flight`, before [`Flight::target`].
pub(super) fn detiled(resource: usize, flight: &Flight, bytes: usize) {
    let on = on();
    if !on && !fix() {
        return;
    }
    if on {
        COUNTS[0].fetch_add(1, Relaxed);
        let sent = flight.current_sent();
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
    }
    line(on);
}

/// The counts every 300 frames.
fn line(on: bool) {
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
    if on {
        let [detilings, under, bytes] = COUNTS.each_ref().map(|c| c.swap(0, Relaxed));
        tracing::info!(
            frame,
            detilings,
            under_flight = under,
            under_kib = bytes >> 10,
            "staging under flight: detilings since the last line, and those that wrote a staging buffer a submission still on the card may copy from"
        );
    }
    if fix() {
        let [moved, made, made_bytes, in_place] = STATS.each_ref().map(|c| c.swap(0, Relaxed));
        tracing::info!(
            frame,
            moved,
            spares_made = made,
            spares_made_mib = made_bytes >> 20,
            in_place,
            spares_mib_now = SPARE_BYTES.load(Relaxed) >> 20,
            "staging flight wait: detilings under a copy moved to a spare staging buffer, spares made, and those written in place (no spare free, or the budget spent) (VICTORY_STAGING_FLIGHT_WAIT)"
        );
    }
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
    fn the_fix_is_off_unless_one() {
        assert_eq!(parse(Some("2")), 0);
        assert_eq!(FIX.get(), 0, "VICTORY_STAGING_FLIGHT_WAIT is not set in tests");
    }

    #[test]
    fn only_a_numbered_unfinished_copy_counts() {
        assert!(!under(0, 9, 3), "never copied");
        assert!(under(9, 9, 3), "the submission being prepared: recorded, runs later");
        assert!(under(4, 9, 3));
        assert!(!under(3, 9, 3), "finished");
    }

    #[test]
    fn a_detiling_moves_off_a_buffer_still_read() {
        let made = |s: usize| s <= 1;
        // Slot 0 stamped by submission 5, not finished: slot 1 (stamped by
        // 2, finished) is taken.
        let sent = |s: usize| [5, 2, 0, 0][s];
        assert_eq!(pick(0, sent, made, 6, 4, true), Pick::Other(1));
        // Both read: a spare is made in the next free slot.
        let sent = |s: usize| [5, 6, 0, 0][s];
        assert_eq!(pick(0, sent, made, 6, 4, true), Pick::Make(2));
        assert_eq!(pick(0, sent, made, 6, 4, false), Pick::InPlace, "budget spent");
        // Finished: in place.
        assert_eq!(pick(0, sent, made, 6, 5, true), Pick::Here);
        // Every slot made and read.
        let sent = |s: usize| [5, 6, 6, 5][s];
        assert_eq!(pick(2, sent, |_| true, 6, 4, true), Pick::InPlace);
    }

    /// A model of the queue and one texture's staging buffers: submissions
    /// take their number (`started`), record copies from the buffer
    /// detiled last (stamping it), and finish in order; detilings come
    /// anywhere between, also inside a submission's preparation. A copy
    /// reads its buffer at any moment until its submission finished, so it
    /// must find there what it was recorded with. With the fix a buffer is
    /// rewritten under a copy only when no spare was free (`in_place`);
    /// without it (always in place) that happens all the time.
    #[test]
    fn stress_no_copy_reads_a_buffer_rewritten_under_it() {
        fn run(fix: bool) -> (u64, u64, u64) {
            let mut rng = 0x2545_f491_4f6c_dd1du64;
            let mut next = || {
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                rng
            };
            let (mut started, mut finished) = (0u64, 0u64);
            let mut sent = [0u64; SPARES + 1];
            let mut made = [true, false, false, false];
            let mut content = [0u64; SPARES + 1];
            let mut current = 0usize;
            let mut version = 0u64;
            // (submission, slot, version recorded, torn allowed)
            let mut copies: Vec<(u64, usize, u64, bool)> = Vec::new();
            let (mut torn, mut in_place, mut moved) = (0u64, 0u64, 0u64);
            for _ in 0..200_000 {
                match next() % 20 {
                    0..=4 => {
                        started += 1;
                        if next() % 3 != 0 {
                            sent[current] = started;
                            copies.push((started, current, content[current], false));
                        }
                    }
                    5..=11 => {
                        if finished < started {
                            finished += 1;
                            for &(sub, slot, v, allowed) in &copies {
                                if sub == finished && content[slot] != v && !allowed {
                                    torn += 1;
                                }
                            }
                            copies.retain(|c| c.0 > finished);
                        }
                    }
                    _ => {
                        let choice = if fix {
                            pick(current, |s| sent[s], |s| made[s], started, finished, true)
                        } else {
                            Pick::Here
                        };
                        let slot = match choice {
                            Pick::Here => current,
                            Pick::Other(s) => s,
                            Pick::Make(s) => {
                                made[s] = true;
                                s
                            }
                            Pick::InPlace => {
                                in_place += 1;
                                copies.iter_mut().filter(|c| c.1 == current).for_each(|c| c.3 = true);
                                current
                            }
                        };
                        moved += u64::from(slot != current);
                        version += 1;
                        content[slot] = version;
                        current = slot;
                    }
                }
            }
            (torn, in_place, moved)
        }
        let (torn, _, _) = run(false);
        assert!(torn > 1000, "without the fix copies read rewritten buffers: {torn}");
        let (torn, in_place, moved) = run(true);
        assert_eq!(torn, 0, "with the fix no copy reads a buffer rewritten under it");
        assert!(moved > 1000, "{moved}");
        // Detilings here come far more often than submissions finish (more
        // than one a submission): spares still take most of them.
        assert!(in_place * 2 < moved, "spares mostly suffice: {in_place} in place, {moved} moved");
    }
}
