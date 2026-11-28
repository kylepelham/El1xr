//! Diagnostics: what `run_other` on the "preparing breakdown" line is made
//! of (`VICTORY_RUN_SPLIT=1`, off by default).
//!
//! `run_other` is the time of `HostList::run_deferred` for every list of a
//! submission and of its prelude (`HostList::prelude`, with its uploads
//! claimed), less the flushes of one step: the list's steps walked in order
//! (fills, refreshes and shaders' writes noted for the flush, textures
//! checked), the title's buffer copies and steps of ours done on the CPU
//! (each flushing what came before it, `Batch::title_written`), host steps
//! (which check the textures pending before them), and the preludes recorded
//! (transitions, texture uploads, clamps ahead; an allocator and list reset
//! and a close). With the switch each step is timed from its start to the
//! next one's, so the line's "noting" parts carry the timing's own cost (a
//! clock read a step).
//!
//! (worldstream, #26) The uploads' claims before each prelude
//! (`PreludeUpload::finish_refresh`, where levels on demand get their tiles:
//! `crate::tiles::ahead`'s line splits that) are a part of their own; they
//! were in no part before, and were most of the bundle's pre-bake
//! submission. The prelude lists made on first use are told apart inside
//! the preludes' part.

use super::Step;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::Instant;

pub(super) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_RUN_SPLIT").is_some_and(|v| v == "1"))
}

/// The parts, by place in [`NS`] and [`COUNT`].
const FILL: usize = 0;
const REFRESH: usize = 1;
const GPU_WRITE: usize = 2;
const TEXTURE: usize = 3;
const COPY: usize = 4;
const RUN: usize = 5;
const HOST: usize = 6;
/// The preludes recorded, their uploads claimed.
const PRELUDE: usize = 7;
/// The uploads claimed before the preludes (levels on demand given).
const CLAIM: usize = 8;
/// Lists.
const LISTS: usize = 9;
/// Of the preludes, the lists made on first use (inside [`PRELUDE`]).
const MADE: usize = 10;
const PARTS: usize = 11;

const NAMES: [&str; PARTS] = [
    "fills noted",
    "refreshes noted",
    "shader writes noted",
    "textures checked",
    "title copies (with their flushes)",
    "steps of ours on title memory (with their flushes)",
    "host steps (with the texture checks before them)",
    "preludes recorded (with their uploads claimed)",
    "uploads claimed before the preludes (levels on demand given tiles)",
    "lists",
    "prelude lists made",
];

/// Since the last line: nanoseconds and counts by part.
static NS: [AtomicU64; PARTS] = [const { AtomicU64::new(0) }; PARTS];
static COUNT: [AtomicU64; PARTS] = [const { AtomicU64::new(0) }; PARTS];

fn kind(step: &Step) -> usize {
    match step {
        Step::Fill(..) => FILL,
        Step::Refresh(..) => REFRESH,
        Step::GpuWrite(..) => GPU_WRITE,
        Step::Texture(..) => TEXTURE,
        Step::Copy(..) | Step::CardCopy(..) => COPY,
        Step::Run(..) => RUN,
        Step::Host(..) => HOST,
    }
}

/// The time of one list's `run_deferred`, part by part: each interval is
/// charged to the part that opened it.
pub(super) struct Laps(Option<(Instant, usize)>);

impl Laps {
    pub(super) fn start() -> Self {
        Laps(on().then(|| (Instant::now(), usize::MAX)))
    }

    fn to(&mut self, part: usize) {
        if let Some((at, open)) = &mut self.0 {
            let now = Instant::now();
            if *open < PARTS {
                NS[*open].fetch_add((now - *at).as_nanos() as u64, Relaxed);
            }
            if part < PARTS {
                COUNT[part].fetch_add(1, Relaxed);
            }
            (*at, *open) = (now, part);
        }
    }

    /// A step begins.
    pub(super) fn step(&mut self, step: &Step) {
        if self.0.is_some() {
            self.to(kind(step));
        }
    }

    /// The list's steps are done.
    pub(super) fn end(mut self) {
        if self.0.is_some() {
            self.to(usize::MAX);
            COUNT[LISTS].fetch_add(1, Relaxed);
        }
    }
}

/// Times a prelude's recording, from now until it is dropped.
pub(super) struct Prelude(Option<Instant>);

pub(super) fn prelude() -> Prelude {
    Prelude(on().then(Instant::now))
}

impl Drop for Prelude {
    fn drop(&mut self) {
        if let Some(at) = self.0 {
            NS[PRELUDE].fetch_add(at.elapsed().as_nanos() as u64, Relaxed);
            COUNT[PRELUDE].fetch_add(1, Relaxed);
        }
    }
}

/// Times the claims of a list's uploads, from now until it is dropped.
pub(super) fn claims() -> Timed {
    Timed(on().then(Instant::now), CLAIM)
}

/// `make` (a prelude's allocator and list, on first use), timed.
pub(super) fn made<T>(make: impl FnOnce() -> T) -> T {
    let _timed = Timed(on().then(Instant::now), MADE);
    make()
}

/// Charges the time from its start until it is dropped to its part.
pub(super) struct Timed(Option<Instant>, usize);

impl Drop for Timed {
    fn drop(&mut self) {
        if let Some(at) = self.0 {
            NS[self.1].fetch_add(at.elapsed().as_nanos() as u64, Relaxed);
            COUNT[self.1].fetch_add(1, Relaxed);
        }
    }
}

/// The parts per frame over `frames`: (name, ms, count).
fn parts(frames: f64) -> Vec<(&'static str, f64, f64)> {
    (0..LISTS)
        .map(|i| {
            let ns = NS[i].swap(0, Relaxed) as f64;
            let n = COUNT[i].swap(0, Relaxed) as f64;
            (NAMES[i], ns / 1e6 / frames, n / frames)
        })
        .collect()
}

/// The frame line's, with the switch on.
pub(super) fn line(frames: f64) {
    if !on() {
        return;
    }
    let parts = parts(frames);
    let lists = COUNT[LISTS].swap(0, Relaxed) as f64 / frames;
    let made_ms = NS[MADE].swap(0, Relaxed) as f64 / 1e6 / frames;
    let made = COUNT[MADE].swap(0, Relaxed) as f64 / frames;
    let total: f64 = parts.iter().map(|p| p.1).sum();
    let text: Vec<String> = parts
        .iter()
        .map(|(name, ms, n)| format!("{name} {ms:.2} ms ({n:.0})"))
        .collect();
    tracing::info!(
        "run_other split per frame (VICTORY_RUN_SPLIT=1): {total:.2} ms over {lists:.1} lists: {}; of the preludes, {} {made_ms:.2} ms ({made:.1})",
        text.join(", "),
        NAMES[MADE]
    );
    // Outside run_other (card_before's `card_dirty`, and merges under writes).
    let [joins, ranges, kept, ms] = crate::joined_apart::taken(frames);
    tracing::info!(
        "changed ranges joined per frame (VICTORY_RUN_SPLIT=1): {joins:.0} joins of {ranges:.0} ranges in {ms:.3} ms, \
         {kept:.0} returned as they were (VICTORY_JOINED_APART={})",
        u8::from(crate::joined_apart::on())
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_part_has_a_name_and_laps_charge_the_part_that_opened_them() {
        assert_eq!(NAMES.len(), PARTS);
        // Charged by hand as `to` does with the switch on.
        let mut laps = Laps(Some((Instant::now(), usize::MAX)));
        let before = (NS[HOST].load(Relaxed), COUNT[HOST].load(Relaxed));
        laps.to(HOST);
        std::thread::sleep(std::time::Duration::from_millis(2));
        laps.to(PRELUDE);
        assert!(NS[HOST].load(Relaxed) - before.0 >= 1_000_000);
        assert!(COUNT[HOST].load(Relaxed) > before.1);
        laps.to(usize::MAX);
        // Off: nothing is timed.
        let mut off = Laps(None);
        off.to(HOST);
        off.end();
    }

    /// (worldstream) The claims and the prelude lists made are charged to
    /// parts of their own; the lists made are outside the line's sum.
    #[test]
    fn claims_and_lists_made_have_parts_of_their_own() {
        assert!(CLAIM < LISTS && MADE > LISTS, "claims summed, lists made not");
        let before = (NS[CLAIM].load(Relaxed), COUNT[MADE].load(Relaxed));
        drop(Timed(Some(Instant::now() - std::time::Duration::from_millis(1)), CLAIM));
        assert!(NS[CLAIM].load(Relaxed) - before.0 >= 1_000_000);
        let n = {
            let _t = Timed(Some(Instant::now()), MADE);
            7
        };
        assert_eq!(n, 7);
        assert!(COUNT[MADE].load(Relaxed) > before.1);
        // Off: nothing is charged.
        let off = COUNT[CLAIM].load(Relaxed);
        drop(Timed(None, CLAIM));
        assert_eq!(COUNT[CLAIM].load(Relaxed), off);
        if !on() {
            assert_eq!(made(|| 3), 3);
        }
    }
}
