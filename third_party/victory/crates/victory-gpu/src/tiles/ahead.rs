//! (worldstream, #26) Levels on demand off the submitting thread
//! (`VICTORY_TILES_AHEAD=1`; off by default), and the split of what giving
//! tiles costs (counted either way, on its own line).
//!
//! With the loading bundle (texture heaps and their block thread, staging
//! of 256 MiB, the DirectStorage poll wait, bake passes staging all) the
//! submission right before the first bake pass stood 0.9-1.1 s on the
//! submitting thread (b5s 02_hb f379-382, b5u 02_lb f298-300): all
//! `run_other`, 305 ms a frame in the preludes, of which recording the
//! lists took 14.6 ms for 2,791 uploads. The rest was the uploads' claims
//! (`PreludeUpload::finish_refresh`), where a texture made level by level
//! gets its tiles: one `UpdateTileMappings` a texture, 1,779 calls in
//! 1.32 s (hb) and 1,150 in 1.07 s (lb), 743-927 us a call. The arms
//! without the bundle made 818-824 calls in that window in 52-175 ms
//! (64-213 us a call): more textures reach that submission at once with
//! the bake passes staging all, and each call is slower beside the block
//! thread's heaps being made resident.
//!
//! With the switch:
//! - The thread that makes a texture ready (a prepare thread or a bake
//!   pass's thread, `crate::texture::prepare` `work`) gives its held
//!   levels their tiles right after detiling them, while the card has room
//!   ([`super::Tiled::ahead`]). Those levels are noted unwritten: no view
//!   shows them, and a card write zeroes them first, until the list that
//!   uploads the texture is submitted (that upload writes every level from
//!   the staging buffer). The call returns before the texture is filed
//!   ready, so before any list that uploads it is recorded: the mapping is
//!   ahead of the upload on the queue, as at the submission. The
//!   submission then finds the levels given and calls nothing.
//! - A spare heap of the pool is made on a thread of its own once less
//!   than half a heap of tiles is free, so a take seldom makes one inline.
//!
//! Off: nothing is given ahead and no spare is made; only the line counts.

use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::Instant;

/// `VICTORY_TILES_AHEAD=1`.
pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        let on = parse(std::env::var("VICTORY_TILES_AHEAD").ok().as_deref());
        if on {
            tracing::info!(
                "texture levels on demand get their tiles on the threads that make them ready, and a spare tile heap is made ahead (VICTORY_TILES_AHEAD=1)"
            );
        }
        on
    })
}

fn parse(value: Option<&str>) -> bool {
    value.is_some_and(|v| v.trim() == "1")
}

/// Where a mapping or a heap is made from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Path {
    /// The submission (an upload's claim, a step of the card's writes).
    Submit,
    /// A thread that makes a texture ready ([`super::Tiled::ahead`]).
    Ahead,
    /// The spare heap's thread.
    Spare,
}

thread_local! {
    static PATH: Cell<Path> = const { Cell::new(Path::Submit) };
}

/// Runs `f` as the giving ahead of a thread that makes a texture ready.
pub(super) fn on_ahead<R>(f: impl FnOnce() -> R) -> R {
    let before = PATH.replace(Path::Ahead);
    let out = f();
    PATH.set(before);
    out
}

/// Since the line before, by place in [`NAMES`].
const SUBMIT_CALLS: usize = 0;
const SUBMIT_US: usize = 1;
const AHEAD_CALLS: usize = 2;
const AHEAD_US: usize = 3;
const HEAP_SUBMIT: usize = 4;
const HEAP_SUBMIT_US: usize = 5;
const HEAP_AHEAD: usize = 6;
const HEAP_AHEAD_US: usize = 7;
const HEAP_SPARE: usize = 8;
const HEAP_SPARE_US: usize = 9;
const WAITS: usize = 10;
const WAIT_US: usize = 11;
const GIVEN: usize = 12;
const FOUND: usize = 13;
const NO_ROOM: usize = 14;
const N: usize = 15;

static SPLIT: [AtomicU64; N] = [const { AtomicU64::new(0) }; N];

fn us(began: Instant) -> u64 {
    began.elapsed().as_micros() as u64
}

/// The (count, microseconds) places of a call made on `path`: mapping
/// calls, or heaps made.
fn places(path: Path, heap: bool) -> (usize, usize) {
    match (path, heap) {
        (Path::Submit, false) => (SUBMIT_CALLS, SUBMIT_US),
        (Path::Ahead | Path::Spare, false) => (AHEAD_CALLS, AHEAD_US),
        (Path::Submit, true) => (HEAP_SUBMIT, HEAP_SUBMIT_US),
        (Path::Ahead, true) => (HEAP_AHEAD, HEAP_AHEAD_US),
        (Path::Spare, true) => (HEAP_SPARE, HEAP_SPARE_US),
    }
}

fn count(path: Path, heap: bool, began: Instant) {
    let (n, t) = places(path, heap);
    SPLIT[n].fetch_add(1, Relaxed);
    SPLIT[t].fetch_add(us(began), Relaxed);
}

/// An `UpdateTileMappings` call begun at `began` returned.
pub(super) fn mapped(began: Instant) {
    count(PATH.get(), false, began);
}

/// A heap of the pool begun at `began` is made.
pub(super) fn heap_made(began: Instant) {
    count(PATH.get(), true, began);
}

/// A wait for the sending thread before a mapping, begun at `began`.
pub(super) fn waited(began: Instant) {
    SPLIT[WAITS].fetch_add(1, Relaxed);
    SPLIT[WAIT_US].fetch_add(us(began), Relaxed);
}

/// Levels given ahead.
pub(super) fn given(levels: u64) {
    SPLIT[GIVEN].fetch_add(levels, Relaxed);
}

/// Levels given ahead that a list then wrote.
pub(super) fn found(levels: u64) {
    SPLIT[FOUND].fetch_add(levels, Relaxed);
}

/// A texture made ready while the card had no room: nothing given ahead.
pub(super) fn no_room() {
    SPLIT[NO_ROOM].fetch_add(1, Relaxed);
}

/// Whether the pool, with `free` tiles free, wants a spare heap made.
fn wants_spare(free: u64) -> bool {
    free < u64::from(super::HEAP_TILES / 2)
}

/// Whether the spare heap's thread is asked for a heap.
static ASKED: Mutex<bool> = Mutex::new(false);
static ASK: Condvar = Condvar::new();

/// After a take left `free` tiles free in the pool: with the switch, a
/// spare heap is asked for when that is under half a heap.
pub(super) fn low(free: u64) {
    if !on() || !wants_spare(free) {
        return;
    }
    static STARTED: OnceLock<bool> = OnceLock::new();
    let started = *STARTED.get_or_init(|| {
        std::thread::Builder::new()
            .name("victory-tile-heaps".into())
            .spawn(spare_thread)
            .is_ok()
    });
    if started {
        *ASKED.lock().unwrap_or_else(|e| e.into_inner()) = true;
        ASK.notify_one();
    }
}

fn spare_thread() {
    PATH.set(Path::Spare);
    loop {
        {
            let mut asked = ASKED.lock().unwrap_or_else(|e| e.into_inner());
            while !*asked {
                asked = ASK.wait(asked).unwrap_or_else(|e| e.into_inner());
            }
            *asked = false;
        }
        let free = super::free_tiles(&super::POOL.lock().unwrap_or_else(|e| e.into_inner()));
        if !wants_spare(free) {
            continue;
        }
        let Some(host) = crate::host::host() else {
            continue;
        };
        if let Some(made) = super::make_heap(&host.device) {
            let mut pool = super::POOL.lock().unwrap_or_else(|e| e.into_inner());
            if wants_spare(super::free_tiles(&pool)) {
                super::insert(&mut pool, made);
            }
        }
    }
}

/// The line, with the levels on demand line, when anything was counted.
pub(super) fn line() {
    let s: [u64; N] = std::array::from_fn(|i| SPLIT[i].swap(0, Relaxed));
    if s.iter().all(|&v| v == 0) {
        return;
    }
    let ms = |i: usize| s[i] as f64 / 1000.0;
    tracing::info!(
        "tiles split (VICTORY_TILES_AHEAD={}) since the line before: UpdateTileMappings on the submission path {} calls in {:.1} ms, \
         ahead on the threads making textures ready {} calls in {:.1} ms; tile heaps made in a take on the submission path {} in {:.1} ms, \
         in a take ahead {} in {:.1} ms, spare ahead {} in {:.1} ms; waits for the sending thread before a mapping {} in {:.2} ms; \
         levels given ahead {}, of them written by a list since {}; textures not given ahead for want of room {}",
        u8::from(on()),
        s[SUBMIT_CALLS],
        ms(SUBMIT_US),
        s[AHEAD_CALLS],
        ms(AHEAD_US),
        s[HEAP_SUBMIT],
        ms(HEAP_SUBMIT_US),
        s[HEAP_AHEAD],
        ms(HEAP_AHEAD_US),
        s[HEAP_SPARE],
        ms(HEAP_SPARE_US),
        s[WAITS],
        ms(WAIT_US),
        s[GIVEN],
        s[FOUND],
        s[NO_ROOM],
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_switch_is_off_unless_one() {
        assert!(!parse(None));
        assert!(!parse(Some("0")));
        assert!(!parse(Some("2")));
        assert!(parse(Some(" 1")));
    }

    /// A call is charged to the path it is made on: the giving ahead and the
    /// spare thread never to the submission's.
    #[test]
    fn calls_are_charged_to_their_path() {
        assert_eq!(places(Path::Submit, false), (SUBMIT_CALLS, SUBMIT_US));
        assert_eq!(places(Path::Ahead, false), (AHEAD_CALLS, AHEAD_US));
        assert_eq!(places(Path::Submit, true), (HEAP_SUBMIT, HEAP_SUBMIT_US));
        assert_eq!(places(Path::Ahead, true), (HEAP_AHEAD, HEAP_AHEAD_US));
        assert_eq!(places(Path::Spare, true), (HEAP_SPARE, HEAP_SPARE_US));
        assert_eq!(PATH.get(), Path::Submit);
        let inside = on_ahead(|| PATH.get());
        assert_eq!(inside, Path::Ahead);
        assert_eq!(PATH.get(), Path::Submit, "the path is put back");
    }

    /// A texture of levels 0 and 1 with tiles of their own and levels 2-3
    /// packed together, as `Tiled::make` cuts one, with nothing given.
    fn tiled() -> super::super::Tiled {
        use std::sync::atomic::AtomicU32;
        super::super::STATS[0].fetch_add(1, Relaxed);
        super::super::Tiled {
            standard: vec![4, 1],
            packed: 1,
            levels: 4,
            full: 0,
            given: Mutex::new(super::super::Given {
                levels: vec![false; 2],
                packed: false,
                runs: Vec::new(),
                fresh: 0,
            }),
            shown: AtomicU32::new(u32::MAX),
            held: AtomicU32::new(0),
            card: AtomicU32::new(0),
            claimed: AtomicU32::new(0),
        }
    }

    /// Given as `Tiled::ahead_on` leaves them: tiles, and noted unwritten.
    fn give_ahead(t: &super::super::Tiled, level: usize) {
        let mut given = t.given.lock().unwrap();
        if level < 2 {
            given.levels[level] = true;
            given.fresh |= 1 << level;
        } else {
            given.packed = true;
            given.fresh |= t.with_level(2);
        }
    }

    /// A level given ahead is not shown, and a card write zeroes it first,
    /// until a list's upload writes it; then it is shown as a level given
    /// at the upload's submission is.
    #[test]
    fn a_level_given_ahead_is_hidden_until_written() {
        let t = tiled();
        give_ahead(&t, 0);
        let named = 1 << 0;
        assert!(
            !t.shown_locked(&t.given.lock().unwrap(), named),
            "not shown while unwritten"
        );
        assert_eq!(t.first_shown(), None);
        assert_eq!(t.without(named), 0, "it has its tiles");
        assert_eq!(t.claim(named), named, "a card write zeroes it first");
        t.unclaim(named);
        // The upload's claim (`Tiled::uploaded_on`) with level 0 held.
        let mut given = t.given.lock().unwrap();
        given.fresh = t.written(given.fresh, named);
        assert_eq!(given.fresh, 0);
        assert!(t.shown_locked(&given, named), "shown once written");
        drop(given);
        assert_eq!(t.first_shown(), Some(0));
        assert_eq!(t.claim(named), 0, "written: not zeroed again");
    }

    /// The levels the card packs together are written together: a write of
    /// one of them writes the group.
    #[test]
    fn packed_levels_are_written_together() {
        let t = tiled();
        give_ahead(&t, 2);
        let fresh = t.given.lock().unwrap().fresh;
        assert_eq!(fresh, 0b1100);
        assert_eq!(t.written(fresh, 1 << 3), 0);
        assert_eq!(
            t.written(fresh, 1 << 1),
            fresh,
            "another level leaves them unwritten"
        );
        assert_eq!(t.expand(1 << 0), 1 << 0);
    }

    /// Without the switch no level is noted unwritten: a level with tiles
    /// is claimed and shown as before.
    #[test]
    fn without_giving_ahead_nothing_changes() {
        let t = tiled();
        let named = 1 << 1;
        t.given.lock().unwrap().levels[1] = true;
        assert_eq!(t.claim(named), 0);
        assert!(t.shown_locked(&t.given.lock().unwrap(), named));
        assert_eq!(t.first_shown(), Some(1));
    }

    #[test]
    fn a_spare_is_wanted_under_half_a_heap() {
        let half = u64::from(super::super::HEAP_TILES / 2);
        assert!(wants_spare(0));
        assert!(wants_spare(half - 1));
        assert!(!wants_spare(half));
        // Off (the test runs without the switch): nothing is started.
        if std::env::var_os("VICTORY_TILES_AHEAD").is_none() {
            low(0);
            assert!(!*ASKED.lock().unwrap());
        }
    }
}
