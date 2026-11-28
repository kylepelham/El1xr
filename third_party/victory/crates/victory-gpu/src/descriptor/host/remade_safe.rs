//! (racefix, issue #57) Levels on demand: the view slots of a texture whose
//! first level with memory changed are made again only at a moment no
//! submission is in the air, on the submitting thread, right before the
//! next submission is sent (`VICTORY_VIEWS_REMADE_SAFE`; off by default).
//!
//! [`super::HostHeap::texture_views_remade`] wrote those slots at the
//! texture pass, on a recording thread, whatever was on the card (hangsafe:
//! 8,749 of 10,255 remakes in run `b5r2_flick/03_fen` with a submission in
//! the air). Two things go wrong there:
//! - a level newly shown is given its tiles (`UpdateTileMappings`) and
//!   written by the submission whose upload it is (`crate::tiles`); a list
//!   sent before that one and still running reads the new view of a level
//!   that, at its place on the queue, has no tiles (zeros: black) or holds
//!   what the tiles' last owner left (garbage), for one frame;
//! - a live descriptor the card is reading is rewritten (Direct3D 12 asks
//!   that a slot a submitted list reads is left alone until it has run): a
//!   torn read mixes the two views.
//!
//! `VICTORY_VIEWS_REMADE_QUIET=1` waited for a texture pass with nothing in
//! the air, checked on a recording thread (a submission could start between
//! the check and the write), and `VICTORY_SLOT_WAIT_ALL=1` waits for the
//! submissions begun before the change, writing while later ones run. Here
//! the remakes are queued at the texture pass and done by
//! [`point`], called by `Host::submit` under the submission lock where the
//! sweep runs: at least once a frame (at every present) nothing is in the
//! air there, and nothing can be sent while the slots are written. The
//! submission about to go, and every one after, sees the new level; none
//! still on the card does. A remake is done only if the host slot still
//! shows the view it was queued for (`HostHeap::remake_slot`).
//!
//! `1`: a remake that found no such moment for [`LATE_MS`] is done at the
//! next submission once every submission begun when it was queued has
//! finished (the new level is then mapped and written everywhere a list
//! can reach it), so a level is never held back long; counted as late.
//! `2`: only with nothing in the air. Live: turned off, what is queued is
//! still done at the next quiet moment.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

static ON: victory_core::settings::Live =
    victory_core::settings::Live::new("VICTORY_VIEWS_REMADE_SAFE", parse);

fn parse(value: Option<&str>) -> u64 {
    match value.map(str::trim) {
        Some("1") => 1,
        Some("2") => 2,
        _ => 0,
    }
}

/// Milliseconds a queued remake waits for a moment with nothing in the air
/// before it is done anyway (with `1`), once its submissions finished.
pub(super) const LATE_MS: u64 = 250;

pub(super) fn on() -> bool {
    static LOGGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    let on = ON.get() != 0;
    if on && !LOGGED.load(Relaxed) && !LOGGED.swap(true, Relaxed) {
        tracing::info!(
            value = ON.get(),
            late_ms = LATE_MS,
            "views remade safe: view slots of textures with levels on demand are made again on the submitting thread with no submission in the air (VICTORY_VIEWS_REMADE_SAFE)"
        );
    }
    on
}

/// What a remake waits with.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Waiting {
    key: u64,
    /// The newest submission begun when it was queued.
    after: u64,
    /// When it was first queued (ms).
    since: u64,
}

/// A remake taken to be done.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct Due {
    pub(super) host: usize,
    pub(super) index: u32,
    pub(super) key: u64,
    pub(super) waited_ms: u64,
    pub(super) late: bool,
}

/// Remakes queued, by (host heap, slot).
#[derive(Default)]
pub(super) struct Line {
    waiting: BTreeMap<(usize, u32), Waiting>,
}

impl Line {
    pub(super) const fn new() -> Self {
        Self {
            waiting: BTreeMap::new(),
        }
    }

    /// Queues the remake of slot `index` of host heap `host` showing `key`,
    /// behind submission `after`. The slot already waiting takes the newer
    /// view and the later submission and keeps its first moment: false then.
    pub(super) fn push(&mut self, host: usize, index: u32, key: u64, after: u64, now: u64) -> bool {
        match self.waiting.entry((host, index)) {
            std::collections::btree_map::Entry::Occupied(mut slot) => {
                let w = slot.get_mut();
                w.key = key;
                w.after = w.after.max(after);
                false
            }
            std::collections::btree_map::Entry::Vacant(slot) => {
                slot.insert(Waiting {
                    key,
                    after,
                    since: now,
                });
                true
            }
        }
    }

    /// Takes what may be done now: everything when `quiet` (nothing in the
    /// air), else with `late_ms` the remakes waiting that long whose
    /// submissions have all finished (`finished`).
    pub(super) fn due(
        &mut self,
        quiet: bool,
        finished: u64,
        now: u64,
        late_ms: Option<u64>,
    ) -> Vec<Due> {
        let mut out = Vec::new();
        self.waiting.retain(|&(host, index), w| {
            let waited_ms = now.saturating_sub(w.since);
            let late = !quiet && late_ms.is_some_and(|ms| waited_ms >= ms) && w.after <= finished;
            if quiet || late {
                out.push(Due {
                    host,
                    index,
                    key: w.key,
                    waited_ms,
                    late,
                });
                false
            } else {
                true
            }
        });
        out
    }

    pub(super) fn len(&self) -> usize {
        self.waiting.len()
    }
}

static LINE: Mutex<Line> = Mutex::new(Line::new());
/// Remakes waiting in [`LINE`] (so [`point`] takes no lock with none).
static WAITING: AtomicU64 = AtomicU64::new(0);

/// Since the last line: remakes queued (new slots), done with nothing in
/// the air, done late, dropped (the slot moved on), milliseconds waited by
/// the ones done, the longest wait, quiet moments that did any.
static STATS: [AtomicU64; 7] = [const { AtomicU64::new(0) }; 7];

/// The newest submission begun when a texture's first level with memory
/// last changed (`super::levels_changed`, in the submission's own step).
static CHANGED: AtomicU64 = AtomicU64::new(0);

/// A texture's first level with memory changed ([`CHANGED`]).
pub(super) fn level_changed() {
    CHANGED.fetch_max(crate::host::submissions_started(), Relaxed);
}

/// What a late remake may count as finished: a remake shows the first level
/// a texture has now, which a submission still running may have mapped
/// after the remake was queued (its own remake is queued only at the next
/// texture pass). So none is done late while any level change waits for its
/// submission: `finished` only once `changed` is, else 0.
fn late_finished(finished: u64, changed: u64) -> u64 {
    if changed <= finished { finished } else { 0 }
}

/// Queues a remake of slot `index` of the host heap at `host` showing `key`.
pub(super) fn queue(host: usize, index: u32, key: u64) {
    let after = crate::host::submissions_started();
    let mut line = LINE.lock().unwrap_or_else(|e| e.into_inner());
    if line.push(host, index, key, after, crate::host::now_ms()) {
        STATS[0].fetch_add(1, Relaxed);
    }
    WAITING.store(line.len() as u64, Relaxed);
}

impl super::HostHeap {
    /// The texture pass with the switch: the slots of the textures whose
    /// first level changed are queued for [`point`] instead of made now.
    /// Returns how many were queued.
    pub(super) fn views_remade_safe(&self) -> usize {
        let starts = std::mem::take(&mut *super::RESHOWN.lock().unwrap_or_else(|e| e.into_inner()));
        if starts.is_empty() {
            return 0;
        }
        let slots: Vec<u32> = {
            let placed = self.placed.lock().unwrap_or_else(|e| e.into_inner());
            starts.iter().flat_map(|&start| placed.slots_at(start)).collect()
        };
        let host = std::ptr::from_ref(self) as usize;
        let made = self.made.lock().unwrap_or_else(|e| e.into_inner());
        let mut queued = 0;
        for slot in slots {
            if let Some(&key) = made.get(slot as usize)
                && key != 0
            {
                queue(host, slot, key);
                queued += 1;
            }
        }
        queued
    }
}

/// Called by `Host::submit` under the submission lock before the
/// submission counts as started, where the sweep runs. `flying`: whether
/// flights are in the air (asked only with remakes waiting).
pub(crate) fn point(flying: fn() -> bool) {
    if WAITING.load(Relaxed) == 0 {
        return;
    }
    let quiet = !flying() && crate::host::submissions_in_flight() == 0;
    let late_ms = (ON.get() == 1).then_some(LATE_MS);
    let finished = late_finished(crate::host::submissions_finished(), CHANGED.load(Relaxed));
    let due = {
        let mut line = LINE.lock().unwrap_or_else(|e| e.into_inner());
        let due = line.due(quiet, finished, crate::host::now_ms(), late_ms);
        WAITING.store(line.len() as u64, Relaxed);
        due
    };
    if due.is_empty() {
        return;
    }
    if quiet {
        STATS[6].fetch_add(1, Relaxed);
    }
    for d in due {
        STATS[if d.late { 2 } else { 1 }].fetch_add(1, Relaxed);
        STATS[4].fetch_add(d.waited_ms, Relaxed);
        STATS[5].fetch_max(d.waited_ms, Relaxed);
        if !super::slot_wait::remake(d.host, d.index, d.key) {
            STATS[3].fetch_add(1, Relaxed);
        }
    }
    line_now();
}

/// The line, every 300 frames.
fn line_now() {
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
    let [queued, quiet, late, dropped, waited, longest, points] =
        STATS.each_ref().map(|s| s.swap(0, Relaxed));
    let done = quiet + late;
    tracing::info!(
        frame,
        queued,
        made_quiet = quiet,
        made_late = late,
        dropped,
        mean_wait_ms = format_args!(
            "{:.1}",
            if done > 0 {
                waited as f64 / done as f64
            } else {
                0.0
            }
        ),
        longest_wait_ms = longest,
        quiet_points = points,
        waiting_now = WAITING.load(Relaxed),
        "views remade safe: level-on-demand view slots made again with nothing in the air (made_quiet) or late after their submissions finished (made_late), since the last line"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_switch_reads_one_or_two() {
        assert_eq!(parse(None), 0);
        assert_eq!(parse(Some("0")), 0);
        assert_eq!(parse(Some(" 1 ")), 1);
        assert_eq!(parse(Some("2")), 2);
        assert_eq!(parse(Some("3")), 0);
    }

    #[test]
    fn nothing_is_done_while_submissions_are_in_the_air() {
        let mut line = Line::new();
        assert!(line.push(1, 5, 9, 7, 0));
        assert!(line.due(false, 7, 100, None).is_empty(), "never with 2");
        assert!(
            line.due(false, 6, 1_000, Some(LATE_MS)).is_empty(),
            "late only once its submissions finished"
        );
        assert!(
            line.due(false, 7, 100, Some(LATE_MS)).is_empty(),
            "late only after LATE_MS"
        );
        let due = line.due(true, 0, 101, None);
        assert_eq!(
            due,
            vec![Due {
                host: 1,
                index: 5,
                key: 9,
                waited_ms: 101,
                late: false
            }]
        );
        assert_eq!(line.len(), 0);
    }

    #[test]
    fn a_late_remake_waits_for_its_submissions_and_the_time() {
        let mut line = Line::new();
        line.push(1, 5, 9, 7, 0);
        let due = line.due(false, 7, LATE_MS, Some(LATE_MS));
        assert_eq!(due.len(), 1);
        assert!(due[0].late);
    }

    #[test]
    fn a_slot_queued_again_keeps_its_first_moment_and_takes_the_newer_view() {
        let mut line = Line::new();
        assert!(line.push(1, 5, 9, 7, 10));
        assert!(!line.push(1, 5, 11, 4, 50));
        assert!(line.push(2, 5, 9, 7, 50), "another heap's slot is its own");
        assert!(
            line.due(false, 6, 10 + LATE_MS, Some(LATE_MS)).is_empty(),
            "it waits for submission 7 still"
        );
        let mut due = line.due(true, 0, 60, None);
        due.sort_by_key(|d| d.host);
        assert_eq!((due[0].key, due[0].waited_ms), (11, 50));
        assert_eq!((due[1].key, due[1].waited_ms), (9, 10));
    }

    /// A model of the queue: submissions are sent and land in order while
    /// texture passes queue remakes and submissions call the point. A
    /// submission's upload may map a slot's new level (`levels_changed`:
    /// the next texture pass queues the remake, and a remake shows the
    /// level mapped last). Checked at every write: the level it shows was
    /// mapped by a submission that has finished; with `2` nothing is on the
    /// card; with `1` only a late write lands under running lists. And every
    /// remake queued is done once a quiet moment comes.
    #[test]
    fn stress_no_remake_lands_under_a_running_list() {
        for mode in [1u64, 2] {
            let mut rng = 0x9e37_79b9_7f4a_7c15u64;
            let mut next = || {
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                rng
            };
            let mut line = Line::new();
            let (mut started, mut finished, mut changed) = (0u64, 0u64, 0u64);
            let mut now = 0u64;
            // Per slot: the submission whose upload mapped its level last.
            let mut mapped_by: BTreeMap<u32, u64> = BTreeMap::new();
            let mut reshown = std::collections::BTreeSet::new();
            let (mut queued, mut done, mut under, mut late_done) = (0u64, 0u64, 0u64, 0u64);
            for _ in 0..200_000 {
                now += next() % 3;
                match next() % 6 {
                    // A submission: the point first, then it counts as
                    // started, then its uploads may map levels.
                    0 | 1 => {
                        let quiet = started == finished;
                        let late = (mode == 1).then_some(LATE_MS);
                        for d in line.due(quiet, late_finished(finished, changed), now, late) {
                            done += 1;
                            assert!(mapped_by[&d.index] <= finished, "a level shown before its submission finished");
                            if started > finished {
                                assert!(d.late, "only a late remake is written under a running list");
                                under += 1;
                            }
                            late_done += u64::from(d.late);
                        }
                        started += 1;
                        if next() % 3 == 0 {
                            let index = (next() % 64) as u32;
                            mapped_by.insert(index, started);
                            reshown.insert(index);
                            changed = changed.max(started);
                        }
                    }
                    // The card finishes the oldest submission.
                    2 | 3 => finished = (finished + 1).min(started),
                    // A texture pass queues the remakes of the slots reshown.
                    _ => {
                        for index in std::mem::take(&mut reshown) {
                            if line.push(7, index, u64::from(index) + 1, started, now) {
                                queued += 1;
                            }
                        }
                    }
                }
            }
            // A quiet moment does everything left.
            finished = started;
            done += line.due(true, finished, now, None).len() as u64;
            assert_eq!(line.len(), 0);
            assert!(queued > 1000 && done >= queued, "mode {mode}: queued {queued} done {done}");
            if mode == 2 {
                assert_eq!(under, 0);
            } else {
                assert!(late_done > 0, "the model reaches the late path");
            }
        }
    }
}
