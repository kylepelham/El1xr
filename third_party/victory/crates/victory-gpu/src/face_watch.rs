//! `VICTORY_FACE_WATCH=1|<pixel hashes>` (diagnostic, live): what every texture a character
//! material draw picks from its record shows when the draw is bound.
//!
//! Batch 5 parts L and M: a character's skin (face, neck) draws pure black for 10-30 s after the
//! character first shows, while the eyes, brows, lashes, hair and clothes of the same character
//! are lit, then lights up by itself (Lucia 54-56 s black, lit from 64 s; the waitress black at
//! 60-80 s with Lucia lit beside her). The bake checks never moved with it. This names, per
//! draw, which record slot reads what: a null or empty host slot, a stale or held view, memory
//! still all zeros, a texture the card wrote (a bake result, or memory a bake result lies in),
//! a stand-in format; and when a slot that read badly turns good, after how many frames.
//!
//! `1` watches the two deferred character material shaders (`crate::material_demand`); a list
//! of 16-digit pixel shader hashes watches those (with their record fields when known, else
//! the first nine record words).

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// Live: 1 when the switch holds anything but `0`.
pub(crate) static ON: victory_core::settings::Live =
    victory_core::settings::Live::new("VICTORY_FACE_WATCH", |v| {
        u64::from(v.is_some_and(|v| !v.trim().is_empty() && v.trim() != "0"))
    });

pub(crate) fn on() -> bool {
    ON.get_bool()
}

/// The two deferred character material shaders watched by `1`.
const DEFAULT_HASHES: [&str; 2] = ["a402e650d0acbc92", "b15ba4896d650511"];
/// The record fields read for a listed shader of unknown layout: the first nine words.
const FIRST_WORDS: [u32; 9] = [0, 4, 8, 12, 16, 20, 24, 28, 32];
/// Detail lines a frame at most.
const LINES_A_FRAME: u64 = 24;
/// Slots remembered at most (a scene draws a few hundred character records).
const MOST: usize = 4096;

/// The pixel shader hashes switch value `value` names.
pub(crate) fn hashes(value: &str) -> Vec<String> {
    let v = value.trim();
    if v == "1" {
        return DEFAULT_HASHES.iter().map(|h| (*h).to_owned()).collect();
    }
    v.split(|c: char| c == ',' || c == ';' || c.is_whitespace())
        .map(|h| h.trim().to_ascii_lowercase())
        .filter(|h| h.len() == 16 && h.bytes().all(|b| b.is_ascii_hexdigit()))
        .collect()
}

/// The record fields to read for pixel shader `hash` when `listed` names the watched ones.
pub(crate) fn fields_for(hash: &str, listed: &[String]) -> Option<&'static [u32]> {
    if !listed.iter().any(|h| h == hash) {
        return None;
    }
    Some(crate::material_demand::fields(hash).unwrap_or(&FIRST_WORDS))
}

/// The watched hashes now (the switch's value read again once a frame).
fn listed() -> std::sync::MutexGuard<'static, (u64, Vec<String>)> {
    static LISTED: Mutex<(u64, Vec<String>)> = Mutex::new((u64::MAX, Vec::new()));
    LISTED.lock().unwrap_or_else(|e| e.into_inner())
}

/// The record fields to read for pixel shader `hash`, if it is watched.
pub(crate) fn fields(hash: &str, frame: u64) -> Option<&'static [u32]> {
    let mut l = listed();
    if l.0 != frame {
        *l = (
            frame,
            hashes(&victory_core::settings::get("VICTORY_FACE_WATCH").unwrap_or_default()),
        );
    }
    fields_for(hash, &l.1)
}

/// `hash` as a `&'static str`, each distinct one kept once (a few watched shaders).
pub(crate) fn intern(hash: &str) -> &'static str {
    static SEEN: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());
    let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(h) = seen.iter().find(|h| **h == hash) {
        return h;
    }
    let h: &'static str = Box::leak(hash.to_owned().into_boxed_str());
    seen.push(h);
    h
}

/// One record slot as the draw reads it.
#[derive(Clone, Debug, Default)]
pub(crate) struct Tex {
    pub(crate) slot: u32,
    /// `crate::bake_check::flags` of the slot.
    pub(crate) flags: u16,
    /// `crate::bake_check::slot_text` of the slot.
    pub(crate) text: String,
    /// `crate::bake_fresh` class of the slot.
    pub(crate) fresh: &'static str,
    /// Its memory reads zeros in every sampled run (None: not a placed texture, or not there).
    pub(crate) zero: Option<bool>,
    /// The card wrote its title texture (a copy or resolve).
    pub(crate) card_written: bool,
    /// Its host copy is in a stand-in format.
    pub(crate) stand_in: bool,
    /// Its title texture is a bake result: `prog@frame flag`.
    pub(crate) baked: Option<String>,
    /// Its memory lies in a bake result's memory, through another texture object.
    pub(crate) in_bake: bool,
    /// That bake result was released by the title since (its memory given to others).
    pub(crate) bake_released: bool,
    /// The start of a texture the host writes (a target) whose memory holds part of it now.
    pub(crate) over_target: Option<u64>,
}

impl Tex {
    /// What [`in_result`] said of its memory; an earlier answer is kept.
    pub(crate) fn note_bake(&mut self, found: Option<bool>) {
        if let Some(released) = found
            && !self.in_bake
        {
            self.in_bake = true;
            self.bake_released = released;
        }
    }
}

/// Why a slot reads badly, the first reason found; None when it looks current.
pub(crate) fn reason(t: &Tex) -> Option<&'static str> {
    use crate::bake_check::{EMPTY, HELD_HERE, HELD_OTHER, NULL, PARTLY, STALE, UNMAPPED};
    let f = t.flags;
    Some(if f & EMPTY != 0 {
        "empty"
    } else if f & NULL != 0 {
        "null"
    } else if f & UNMAPPED != 0 {
        "unmapped"
    } else if t.zero == Some(true) {
        "zero_bytes"
    } else if t.in_bake && t.baked.is_none() {
        "bake_alias"
    } else if f & STALE != 0 {
        "stale"
    } else if f & (HELD_HERE | HELD_OTHER) != 0 {
        "held"
    } else if f & PARTLY != 0 {
        "partly"
    } else if t.fresh == "other_bytes" {
        "other_bytes"
    } else if t.stand_in {
        "stand_in"
    } else {
        return None;
    })
}

const REASONS: [&str; 11] = [
    "empty",
    "null",
    "unmapped",
    "zero_bytes",
    "bake_alias",
    "stale",
    "held",
    "partly",
    "other_bytes",
    "stand_in",
    "ok",
];

/// A slot's text for a detail line.
pub(crate) fn tex_text(t: &Tex) -> String {
    let mut s = format!("{} why={}", t.text, reason(t).unwrap_or("ok"));
    s.push_str(&format!(" fresh={}", t.fresh));
    if let Some(z) = t.zero {
        s.push_str(if z { " zero=yes" } else { " zero=no" });
    }
    if t.card_written {
        s.push_str(" card_written");
    }
    if t.stand_in {
        s.push_str(" stand_in");
    }
    if let Some(b) = &t.baked {
        s.push_str(&format!(" baked={b}"));
    }
    if t.in_bake {
        s.push_str(if t.bake_released {
            " in_released_bake_memory"
        } else {
            " in_bake_memory"
        });
    }
    if let Some(at) = t.over_target {
        s.push_str(&format!(" over_target={at:#x}"));
    }
    s
}

/// A slot's place: pixel shader, record's table and offset, heap slot.
type Key = (&'static str, u32, u32, u32);

#[derive(Default)]
struct Book {
    /// By slot: since which frame it reads badly and why (None: good), last frame seen.
    slots: HashMap<Key, (Option<(u64, &'static str)>, u64)>,
    frame: u64,
    draws: u64,
    unread: u64,
    counts: [u64; REASONS.len()],
    lines: u64,
}

fn book() -> std::sync::MutexGuard<'static, Book> {
    static BOOK: std::sync::OnceLock<Mutex<Book>> = std::sync::OnceLock::new();
    BOOK.get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

impl Book {
    /// A new frame: the last one's line, counts cleared.
    fn turn(&mut self, frame: u64) -> Option<String> {
        if frame == self.frame {
            return None;
        }
        let line = (self.draws + self.unread > 0).then(|| self.frame_line());
        self.frame = frame;
        self.draws = 0;
        self.unread = 0;
        self.counts = [0; REASONS.len()];
        self.lines = 0;
        line
    }

    fn frame_line(&self) -> String {
        let counts = REASONS
            .iter()
            .zip(&self.counts)
            .filter(|(_, n)| **n > 0)
            .map(|(r, n)| format!("{r}={n}"))
            .collect::<Vec<_>>()
            .join(" ");
        format!(
            "face watch frame frame={} draws={} unread={} slots: {}",
            self.frame, self.draws, self.unread, counts
        )
    }

    /// A watched draw at `frame` read `texs` from record (`table`, `record`): the lines to
    /// write (a slot newly bad, or bad turned good).
    fn draw(
        &mut self,
        frame: u64,
        hash: &'static str,
        table: u32,
        record: u32,
        texs: &[Tex],
    ) -> Vec<String> {
        self.draws += 1;
        let mut out = Vec::new();
        let mut newly_bad = false;
        for t in texs {
            let why = reason(t);
            let i = REASONS
                .iter()
                .position(|r| Some(*r) == why)
                .unwrap_or(REASONS.len() - 1);
            self.counts[i] += 1;
            let key = (hash, table, record, t.slot);
            if self.slots.len() >= MOST && !self.slots.contains_key(&key) {
                let cut = frame.saturating_sub(600);
                self.slots.retain(|_, (_, seen)| *seen >= cut);
                if self.slots.len() >= MOST {
                    continue;
                }
            }
            let entry = self.slots.entry(key).or_insert((None, frame));
            let was = entry.0;
            entry.1 = frame;
            match (was, why) {
                (None, Some(w)) => {
                    entry.0 = Some((frame, w));
                    newly_bad = true;
                }
                (Some((_, old)), Some(w)) if old != w => {
                    entry.0 = Some((entry.0.map_or(frame, |e| e.0), w));
                    newly_bad = true;
                }
                (Some((since, old)), None) => {
                    entry.0 = None;
                    if self.lines < LINES_A_FRAME {
                        self.lines += 1;
                        out.push(format!(
                            "face watch: frame {frame} pixel {hash} record={table:#x}:{record:#x} slot {} good after {} frames (was {old}): {}",
                            t.slot,
                            frame - since,
                            tex_text(t)
                        ));
                    }
                }
                _ => {}
            }
        }
        if newly_bad && self.lines < LINES_A_FRAME {
            self.lines += 1;
            let slots = texs.iter().map(tex_text).collect::<Vec<_>>().join(" | ");
            out.push(format!(
                "face watch: frame {frame} pixel {hash} record={table:#x}:{record:#x} reads badly: [{slots}]"
            ));
        }
        out
    }
}

/// A watched draw of pixel shader `hash` at `frame` read `texs` from its record.
pub(crate) fn drawn(frame: u64, hash: &'static str, table: u32, record: u32, texs: &[Tex]) {
    let (last, lines) = {
        let mut b = book();
        let last = b.turn(frame);
        (last, b.draw(frame, hash, table, record, texs))
    };
    if let Some(l) = last {
        tracing::info!("{l}");
    }
    for l in lines {
        tracing::info!("{l}");
    }
}

/// A watched draw whose record could not be read.
pub(crate) fn unread(frame: u64) {
    let last = {
        let mut b = book();
        let last = b.turn(frame);
        b.unread += 1;
        last
    };
    if let Some(l) = last {
        tracing::info!("{l}");
    }
}

/// A bake result's title memory: address, bytes, its texture, whether the title released it.
type Result = (u64, u64, usize, bool);

/// Title memory of bake results (`crate::bake_check` results), the newest few.
fn results() -> std::sync::MutexGuard<'static, Vec<Result>> {
    static R: Mutex<Vec<Result>> = Mutex::new(Vec::new());
    R.lock().unwrap_or_else(|e| e.into_inner())
}

/// Ranges kept at most.
const RESULT_RANGES: usize = 1024;
static NOTED: AtomicU64 = AtomicU64::new(0);

/// Bake result `resource` lies in title memory `address`, `len` bytes.
pub(crate) fn note_result(address: u64, len: u64, resource: usize) {
    if address == 0 || len == 0 {
        return;
    }
    let mut r = results();
    if let Some(e) = r
        .iter_mut()
        .find(|e| e.0 == address && e.1 == len && e.2 == resource)
    {
        e.3 = false;
        return;
    }
    if r.len() >= RESULT_RANGES {
        r.drain(..RESULT_RANGES / 2);
    }
    r.push((address, len, resource, false));
    NOTED.fetch_add(1, Relaxed);
}

/// The title released texture `resource`: a bake result's memory is noted as released.
pub(crate) fn result_gone(resource: usize) {
    if !on() {
        return;
    }
    for e in results().iter_mut().filter(|e| e.2 == resource) {
        e.3 = true;
    }
}

/// Whether `[address, address + len)` overlaps a bake result's memory: `Some(released)`,
/// false when one of them is still the title's.
pub(crate) fn in_result(address: u64, len: u64) -> Option<bool> {
    overlaps(&results(), address, len)
}

fn overlaps(ranges: &[Result], address: u64, len: u64) -> Option<bool> {
    let end = address.saturating_add(len.max(1));
    ranges
        .iter()
        .filter(|e| address < e.0.saturating_add(e.1) && e.0 < end)
        .map(|e| e.3)
        .reduce(|a, b| a && b)
}

#[cfg(test)]
mod tests;
