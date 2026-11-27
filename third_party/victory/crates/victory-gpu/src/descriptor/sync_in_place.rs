//! The heaps' records compared where they are (`VICTORY_SYNC_IN_PLACE=1`;
//! off by default), and the split of the [`super::sync_heaps`] pass's time.
//!
//! Ported from Belobug/VIctory-Belobug-AMD 15ab94e by way of
//! readmet3xt-lab/foxhound (`descriptor.rs` `sync_in_place`). There the
//! copy of the changed pages and the compare of their records were two
//! passes over all of each page, 7.4 and 5.3 ms a frame over six threads
//! (Belobug run heapsync-ab1); here one `victory-heap-sync` thread does the
//! pass and the submitting thread waits for it at the end of its preparing
//! (the `heap-sync rounds` line: 2.6-15 ms a frame over batch 2 and 3).
//!
//! Before (and for sampler heaps): a changed page that has slots the pass
//! asks about is copied out of title memory, then each such record is
//! hashed and its hash compared with the one last seen
//! (`HostHeap::mirror_copied_page`).
//!
//! With the switch, for view heaps (32-byte records): the page's records
//! are compared with the bytes last seen in title memory itself, 32 bytes
//! (one record) at a time, and only those that differ are copied
//! ([`crate::host::diff_copy_direct`]: the AVX2 scan whose fault on a
//! missing page ends it where it stands, as the plain copy's does). What it
//! did not compare is read with [`crate::host::read_into`] as before; a
//! page that cannot be read is left, as before. Each view heap keeps the
//! bytes of its records (32 bytes a slot) instead of an 8-byte hash.
//!
//! A record that changed while the pass left its slot to the draws that
//! read it through tables is held in `unseen` and mirrored when the pass
//! next asks about it, as the hash, which only those asked about update,
//! did.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use super::host::HostHeap;

/// Bytes of a view record, and of the scan's block.
const RECORD: usize = 32;

pub(crate) fn on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_SYNC_IN_PLACE").is_some_and(|v| v != "0"))
}

/// Since the last line: microseconds reading and comparing, microseconds
/// mirroring (both paths); pages compared in place, of them read partly
/// through `read_into`, of them not readable.
static PASS: [AtomicU64; 5] = [const { AtomicU64::new(0) }; 5];
const READ_US: usize = 0;
const MIRROR_US: usize = 1;
const PAGES: usize = 2;
const PARTLY: usize = 3;
const UNREAD: usize = 4;

pub(crate) fn now() -> std::time::Instant {
    std::time::Instant::now()
}

/// The pass mirrored from `began` to now.
pub(crate) fn mirrored_since(began: std::time::Instant) {
    PASS[MIRROR_US].fetch_add(began.elapsed().as_micros() as u64, Relaxed);
}

/// The pass read, compared and mirrored a batch of pages from `began` to
/// now (the mirroring is counted apart, and taken out by [`line`]).
pub(crate) fn batch_since(began: std::time::Instant) {
    PASS[READ_US].fetch_add(began.elapsed().as_micros() as u64, Relaxed);
}

/// The records of one view heap last seen by the pass.
#[derive(Default)]
pub(crate) struct Held {
    bytes: Vec<u8>,
    /// Per slot: its record changed while the pass left it to its draws.
    unseen: Vec<u64>,
}

/// Per host heap (its id), the records last seen.
static HELD: std::sync::Mutex<Option<std::collections::HashMap<u64, Held>>> =
    std::sync::Mutex::new(None);

/// The records last seen of every view heap, held for one pass.
pub(crate) struct AllHeld(
    std::sync::MutexGuard<'static, Option<std::collections::HashMap<u64, Held>>>,
);

pub(crate) fn all() -> AllHeld {
    AllHeld(HELD.lock().unwrap_or_else(|e| e.into_inner()))
}

impl AllHeld {
    /// Those of host heap `id`, of `slots` slots.
    pub(crate) fn of(&mut self, id: u64, slots: u32) -> &mut Held {
        let held = self
            .0
            .get_or_insert_with(Default::default)
            .entry(id)
            .or_default();
        let bytes = slots as usize * RECORD;
        if held.bytes.len() < bytes {
            held.bytes.resize(bytes, 0);
            held.unseen.resize((slots as usize).div_ceil(64), 0);
        }
        held
    }

    /// Lets go of those of heaps that are gone (ids not in `live`).
    pub(crate) fn keep_only(&mut self, live: &[u64]) {
        if let Some(held) = self.0.as_mut() {
            held.retain(|id, _| live.contains(id));
        }
    }
}

/// Scratch of one page for the pass.
#[derive(Default)]
pub(crate) struct Scratch {
    runs: Vec<(usize, usize)>,
    read: Vec<u8>,
    changed: Vec<(u32, u64)>,
}

/// The records of title slots `asked` (ascending) on `[from, to)` of the
/// view heap at `start` whose bytes are not the ones last seen, mirrored
/// into `host`. Only records lying whole in the range and below `limit`
/// slots are looked at, as `mirror_copied_page` does. Returns how many were
/// mirrored.
#[allow(clippy::too_many_arguments)]
pub(crate) fn page(
    host: &HostHeap,
    held: &mut Held,
    start: u64,
    from: usize,
    to: usize,
    limit: u32,
    asked: &[u32],
    scratch: &mut Scratch,
) -> u64 {
    let began = now();
    let start = start as usize;
    let first = (from - start).div_ceil(RECORD);
    let last = ((to - start) / RECORD).min(limit as usize);
    if first >= last || asked.is_empty() {
        return 0;
    }
    let lead = start + first * RECORD;
    let len = (last - first) * RECORD;
    // At most a page of 64 KiB: 2,048 records.
    let mut fresh = [0u64; 32];
    let seen = &mut held.bytes[first * RECORD..last * RECORD];
    scratch.runs.clear();
    let done = {
        let _hold = crate::shadow::lazy_hold_access(lead, len);
        match crate::host::diff_copy_direct(lead as *const u8, seen, 0, &mut scratch.runs) {
            Ok(()) => len,
            Err(done) => done,
        }
    };
    PASS[PAGES].fetch_add(1, Relaxed);
    for &(off, n) in &scratch.runs {
        for record in off / RECORD..(off + n) / RECORD {
            fresh[record / 64] |= 1 << (record % 64);
        }
    }
    // Records before `done` were compared; those after it are read here.
    let mut compared = last - first;
    if done < len {
        PASS[PARTLY].fetch_add(1, Relaxed);
        let rest = len - done;
        scratch.read.resize(rest, 0);
        if crate::host::read_into((lead + done) as *const u8, &mut scratch.read) {
            changed_records(&mut seen[done..], &scratch.read, done / RECORD, &mut fresh);
        } else {
            PASS[UNREAD].fetch_add(1, Relaxed);
            compared = done / RECORD;
        }
    }
    let changed = pick(
        held,
        first,
        compared,
        asked,
        &fresh,
        start,
        &mut scratch.changed,
    );
    let mirroring = now();
    let mirrored = host.mirror_changed(changed);
    mirrored_since(mirroring);
    batch_since(began);
    mirrored
}

/// Of `records` (32 bytes each), those that differ from `seen` go into
/// `seen` and are marked in `fresh` (bit `skip + n` for record `n`).
fn changed_records(seen: &mut [u8], records: &[u8], skip: usize, fresh: &mut [u64; 32]) {
    for (n, (old, new)) in seen
        .chunks_exact_mut(RECORD)
        .zip(records.chunks_exact(RECORD))
        .enumerate()
    {
        if old != new {
            old.copy_from_slice(new);
            let record = skip + n;
            fresh[record / 64] |= 1 << (record % 64);
        }
    }
}

/// The slots of `asked` among the `compared` records from slot `first` on
/// that changed now (`fresh`, by record from `first`) or while they were
/// left to their draws (`held.unseen`); the changed ones not asked about
/// are noted in `held.unseen`.
fn pick<'a>(
    held: &mut Held,
    first: usize,
    compared: usize,
    asked: &[u32],
    fresh: &[u64; 32],
    start: usize,
    out: &'a mut Vec<(u32, u64)>,
) -> &'a [(u32, u64)] {
    out.clear();
    let mut wanted = [0u64; 32];
    for &index in asked {
        let Some(record) = (index as usize).checked_sub(first) else {
            continue;
        };
        if record < compared {
            wanted[record / 64] |= 1 << (record % 64);
        }
    }
    let unseen = |held: &Held, record: usize| {
        let slot = first + record;
        held.unseen
            .get(slot / 64)
            .is_some_and(|w| w & (1 << (slot % 64)) != 0)
    };
    for word in 0..compared.div_ceil(64) {
        let mut candidates = fresh[word] | wanted[word];
        while candidates != 0 {
            let bit = candidates.trailing_zeros() as usize;
            candidates &= candidates - 1;
            let record = word * 64 + bit;
            if record >= compared {
                break;
            }
            let is_fresh = fresh[word] & (1 << bit) != 0;
            let is_wanted = wanted[word] & (1 << bit) != 0;
            let slot = first + record;
            let mask = 1u64 << (slot % 64);
            if is_wanted {
                if is_fresh || unseen(held, record) {
                    if let Some(w) = held.unseen.get_mut(slot / 64) {
                        *w &= !mask;
                    }
                    out.push((slot as u32, (start + slot * RECORD) as u64));
                }
            } else if let Some(w) = held.unseen.get_mut(slot / 64) {
                *w |= mask;
            }
        }
    }
    out
}

/// With the frame line: the split of the pass's time per frame.
pub(crate) fn line(frames: f64) {
    let [read, mirror, pages, partly, unread] = PASS.each_ref().map(|n| n.swap(0, Relaxed));
    if read == 0 && mirror == 0 {
        return;
    }
    let per = |n: u64| n as f64 / frames;
    tracing::info!(
        "heap-sync pass per frame (VICTORY_SYNC_IN_PLACE={}): reading and comparing {:.2} ms, mirroring {:.2} ms; \
         {:.1} pages compared in place, {:.1} of them partly read into a copy, {:.1} not readable",
        u8::from(on()),
        per(read.saturating_sub(mirror)) / 1000.0,
        per(mirror) / 1000.0,
        per(pages),
        per(partly),
        per(unread),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn held(slots: usize) -> Held {
        Held {
            bytes: vec![0; slots * RECORD],
            unseen: vec![0; slots.div_ceil(64)],
        }
    }

    #[test]
    fn changed_records_mark_and_take_only_those_that_differ() {
        let mut seen = vec![0u8; 4 * RECORD];
        let mut now = seen.clone();
        now[RECORD + 3] = 7;
        now[3 * RECORD] = 1;
        let mut fresh = [0u64; 32];
        changed_records(&mut seen, &now, 10, &mut fresh);
        assert_eq!(fresh[0], (1 << 11) | (1 << 13));
        assert_eq!(seen, now);
    }

    #[test]
    fn asked_slots_take_fresh_records_and_left_ones_wait_in_unseen() {
        let mut h = held(256);
        let mut out = Vec::new();
        let mut fresh = [0u64; 32];
        // Records 0, 1 and 5 from slot 64 changed; slot 65 is left to its draws.
        fresh[0] = 0b10_0011;
        let got = pick(&mut h, 64, 128, &[64, 66, 69], &fresh, 0x1000, &mut out).to_vec();
        assert_eq!(got, vec![(64, 0x1000 + 64 * 32), (69, 0x1000 + 69 * 32)]);
        assert_eq!(h.unseen[1], 1 << 1);
        // Next pass: nothing new, but slot 65 is asked about now.
        let got = pick(&mut h, 64, 128, &[64, 65], &[0; 32], 0x1000, &mut out).to_vec();
        assert_eq!(got, vec![(65, 0x1000 + 65 * 32)]);
        assert_eq!(h.unseen[1], 0);
        // And not again.
        assert!(pick(&mut h, 64, 128, &[65], &[0; 32], 0x1000, &mut out).is_empty());
    }

    /// Over random passes, the slots mirrored are those the hash of the
    /// slots asked about would mirror, plus only slots whose record changed
    /// and changed back while left to their draws (mirrored once more).
    #[test]
    fn as_the_hashes_of_the_asked_slots_did() {
        const SLOTS: usize = 192;
        let mut seed = 11u64;
        let mut random = move || {
            seed = seed
                .wrapping_mul(0x5851_F42D_4C95_7F2D)
                .wrapping_add(0x1405_7B7E_F767_814F);
            (seed >> 33) as usize
        };
        let mut title = vec![0u8; SLOTS * RECORD];
        let mut h = held(SLOTS);
        // The old way: per slot, the record last seen when asked about.
        let mut last_asked = vec![0u8; SLOTS * RECORD];
        let mut out = Vec::new();
        for _ in 0..400 {
            for _ in 0..random() % 6 {
                let slot = random() % SLOTS;
                // Few values, so that records change back now and then.
                title[slot * RECORD + random() % RECORD] = (random() % 3) as u8;
            }
            let asked: Vec<u32> = (0..SLOTS as u32).filter(|_| random() % 4 != 0).collect();
            let mut fresh = [0u64; 32];
            changed_records(&mut h.bytes, &title, 0, &mut fresh);
            let got: Vec<u32> = pick(&mut h, 0, SLOTS, &asked, &fresh, 0, &mut out)
                .iter()
                .map(|c| c.0)
                .collect();
            let mut want = Vec::new();
            for &slot in &asked {
                let at = slot as usize * RECORD..(slot as usize + 1) * RECORD;
                if last_asked[at.clone()] != title[at.clone()] {
                    last_asked[at.clone()].copy_from_slice(&title[at]);
                    want.push(slot);
                }
            }
            for slot in &want {
                assert!(got.contains(slot), "slot {slot} not mirrored");
            }
            for slot in got.iter().filter(|s| !want.contains(s)) {
                let at = *slot as usize * RECORD..(*slot as usize + 1) * RECORD;
                assert_eq!(last_asked[at.clone()], title[at], "slot {slot} mirrored for nothing");
            }
            assert_eq!(h.bytes, title);
        }
    }

    #[test]
    fn records_past_what_was_compared_are_left() {
        let mut h = held(128);
        let mut out = Vec::new();
        let mut fresh = [0u64; 32];
        fresh[0] = u64::MAX;
        let got = pick(&mut h, 0, 3, &[0, 1, 2, 3, 4], &fresh, 0, &mut out).to_vec();
        assert_eq!(got.iter().map(|c| c.0).collect::<Vec<_>>(), vec![0, 1, 2]);
        assert_eq!(h.unseen[0], 0);
    }

    #[test]
    fn a_page_of_ours_compared_in_place_finds_the_changed_records() {
        let mut title = vec![0u8; 64 * RECORD];
        let start = title.as_ptr() as usize;
        let mut h = held(64);
        // First look: every nonzero record is new.
        title[2 * RECORD] = 9;
        title[40 * RECORD + 31] = 1;
        let mut runs = Vec::new();
        let mut fresh = [0u64; 32];
        let r = crate::host::diff_copy_direct(start as *const u8, &mut h.bytes, 0, &mut runs);
        assert!(r.is_ok() || r == Err(0));
        if r == Err(0) {
            let copy = title.clone();
            changed_records(&mut h.bytes, &copy, 0, &mut fresh);
        } else {
            for &(off, n) in &runs {
                for record in off / RECORD..(off + n) / RECORD {
                    fresh[record / 64] |= 1 << (record % 64);
                }
            }
        }
        assert_eq!(fresh[0], (1 << 2) | (1 << 40));
        assert_eq!(h.bytes, title);
    }
}
