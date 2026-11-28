//! (phonescreen, #115; deepscout B #1) `VICTORY_NATIVE_PIPELINE_DIAG=1`
//! (live, off by default): whether the title makes a pipeline another way
//! after device method 0x218 (a compute pipeline from a program it compiled
//! itself: its mip-generation programs) or 0x1c8 (the same for graphics)
//! refused one. Both answer `E_NOTIMPL` (at least 2048 and 16384 calls a
//! run, all in the first second of boot). If the title does not retry at
//! 0x1f0 (or 0x58, 0x50, 0x2b0) with its container, those programs never
//! run: mip chains the card should make stay black, which may be why a
//! render target sampled small (the phone's video) shows black.
//!
//! Per thread, a refusal waits for the next pipeline the same thread makes:
//! made of the same kind (compute after 0x218, graphics after 0x1c8) counts
//! `retried`; another refusal first, or a pipeline of the other kind,
//! counts `not_retried`. Logged:
//! - `native pipelines: a 0x218 program refused` for the first
//!   [`MOST_PROGRAM_LINES`] distinct programs (their 64-bit hash, code and
//!   resource table bytes, thread-group size, as DexNom PR #98 reads the
//!   0x60-byte description), and `native pipelines: retried` for the first
//!   [`MOST_PAIR_LINES`] pairs (the refused hash, the hash of the container
//!   made after it);
//! - `native pipelines: summary` at every power of two of events (refusals
//!   plus pipelines made) from 64 on: per slot `refused`, `distinct`,
//!   `retried`, `not_retried`, and the compute and graphics pipelines made
//!   since the first refusal.

use std::cell::Cell;
use std::collections::HashSet;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use victory_core::settings::Live;

static ON: Live = Live::new("VICTORY_NATIVE_PIPELINE_DIAG", |v| {
    u64::from(v.is_some_and(|v| v.trim() == "1"))
});

const MOST_PROGRAM_LINES: u32 = 32;
const MOST_PAIR_LINES: u32 = 32;
/// Summaries start at this many events.
const FIRST_SUMMARY: u64 = 64;

/// The two refusing slots.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Slot {
    /// 0x218: compute.
    Compute,
    /// 0x1c8: graphics.
    Graphics,
}

impl Slot {
    fn index(self) -> usize {
        match self {
            Slot::Compute => 0,
            Slot::Graphics => 1,
        }
    }
}

/// A refusal waiting for the thread's next pipeline: its slot and program
/// hash (0 when not known).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Refused {
    slot: Slot,
    hash: u64,
}

/// What a thread's next event means for the refusal it waits on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Outcome {
    Retried(Refused),
    NotRetried(Refused),
    Nothing,
}

/// The outcome of a pipeline made (`compute` or not) while `waiting`.
fn on_made(waiting: Option<Refused>, compute: bool) -> Outcome {
    match waiting {
        Some(r) if (r.slot == Slot::Compute) == compute => Outcome::Retried(r),
        Some(r) => Outcome::NotRetried(r),
        None => Outcome::Nothing,
    }
}

/// The outcome of another refusal while `waiting`.
fn on_refused(waiting: Option<Refused>) -> Outcome {
    waiting.map_or(Outcome::Nothing, Outcome::NotRetried)
}

thread_local! {
    static WAITING: Cell<Option<Refused>> = const { Cell::new(None) };
}

static REFUSED: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];
static RETRIED: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];
static NOT_RETRIED: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];
/// Compute and graphics pipelines made since the first refusal.
static MADE: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];
static EVENTS: AtomicU64 = AtomicU64::new(0);
static PROGRAM_LINES: AtomicU32 = AtomicU32::new(0);
static PAIR_LINES: AtomicU32 = AtomicU32::new(0);
static DISTINCT: Mutex<Option<[HashSet<u64>; 2]>> = Mutex::new(None);

fn on() -> bool {
    ON.get() != 0
}

fn count(outcome: Outcome, made_hash: Option<&str>) {
    match outcome {
        Outcome::Retried(r) => {
            RETRIED[r.slot.index()].fetch_add(1, Ordering::Relaxed);
            if PAIR_LINES.fetch_add(1, Ordering::Relaxed) < MOST_PAIR_LINES {
                tracing::warn!(
                    slot = ?r.slot,
                    refused_hash = format_args!("{:016x}", r.hash),
                    made = made_hash.unwrap_or("?"),
                    "native pipelines: retried"
                );
            }
        }
        Outcome::NotRetried(r) => {
            NOT_RETRIED[r.slot.index()].fetch_add(1, Ordering::Relaxed);
        }
        Outcome::Nothing => {}
    }
}

fn event() {
    let n = EVENTS.fetch_add(1, Ordering::Relaxed) + 1;
    if n >= FIRST_SUMMARY && n.is_power_of_two() {
        summary();
    }
}

fn summary() {
    let distinct = DISTINCT
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map_or([0, 0], |d| [d[0].len(), d[1].len()]);
    let get = |a: &[AtomicU64; 2], i: usize| a[i].load(Ordering::Relaxed);
    tracing::warn!(
        refused_0x218 = get(&REFUSED, 0),
        distinct_0x218 = distinct[0],
        retried_0x218 = get(&RETRIED, 0),
        not_retried_0x218 = get(&NOT_RETRIED, 0),
        refused_0x1c8 = get(&REFUSED, 1),
        distinct_0x1c8 = distinct[1],
        retried_0x1c8 = get(&RETRIED, 1),
        not_retried_0x1c8 = get(&NOT_RETRIED, 1),
        compute_made_since = get(&MADE, 0),
        graphics_made_since = get(&MADE, 1),
        "native pipelines: summary"
    );
}

/// What a 0x218 description says of its program: hash, code and table
/// bytes, thread-group size.
#[derive(Debug, PartialEq, Eq)]
struct Program {
    hash: u64,
    code_bytes: u64,
    table_bytes: u64,
    threads: [u16; 3],
}

/// From the description's twelve words and its register block (DexNom PR
/// #98: words 1-2 registers, 5-6 code, 7-8 resource table, 11 hash; the
/// thread-group size is three u16 at register byte 0xc).
fn program(words: &[u64; 12], registers: Option<&[u8]>) -> Program {
    let threads = registers.filter(|r| r.len() >= 0x12).map_or([0; 3], |r| {
        std::array::from_fn(|i| u16::from_le_bytes([r[0xc + 2 * i], r[0xd + 2 * i]]))
    });
    Program {
        hash: words[11],
        code_bytes: words[6],
        table_bytes: words[8],
        threads,
    }
}

#[cfg(windows)]
fn title_words(desc: *const u8) -> Option<[u64; 12]> {
    let mut bytes = [0u8; 0x60];
    if desc.is_null() || !crate::host::read_into(desc, &mut bytes) {
        return None;
    }
    Some(std::array::from_fn(|k| {
        u64::from_le_bytes(bytes[8 * k..8 * k + 8].try_into().unwrap_or_default())
    }))
}

#[cfg(windows)]
fn registers(words: &[u64; 12]) -> Option<Vec<u8>> {
    let (at, size) = (words[1] as usize, words[2] as usize);
    if at == 0 || !(0x12..=0x100).contains(&size) {
        return None;
    }
    let mut bytes = vec![0u8; size];
    crate::host::read_into(at as *const u8, &mut bytes).then_some(bytes)
}

/// Device method `slot` refused description `desc`.
pub(crate) fn refused(slot: Slot, desc: *const u8) {
    if !on() {
        return;
    }
    let mut hash = 0;
    #[cfg(windows)]
    if slot == Slot::Compute
        && let Some(words) = title_words(desc)
    {
        let p = program(&words, registers(&words).as_deref());
        hash = p.hash;
        let new = DISTINCT
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_or_insert_with(Default::default)[0]
            .insert(hash);
        if new && PROGRAM_LINES.fetch_add(1, Ordering::Relaxed) < MOST_PROGRAM_LINES {
            tracing::warn!(
                hash = format_args!("{:016x}", p.hash),
                code_bytes = p.code_bytes,
                table_bytes = p.table_bytes,
                threads = ?p.threads,
                "native pipelines: a 0x218 program refused"
            );
        }
    }
    #[cfg(not(windows))]
    let _ = desc;
    if slot == Slot::Graphics {
        // No program hash is known for 0x1c8: count descriptions by address.
        DISTINCT
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_or_insert_with(Default::default)[1]
            .insert(desc as u64);
    }
    REFUSED[slot.index()].fetch_add(1, Ordering::Relaxed);
    let waiting = WAITING.with(|w| w.replace(Some(Refused { slot, hash })));
    count(on_refused(waiting), None);
    event();
}

/// A pipeline was made with `shaders`.
pub(crate) fn made(shaders: &[std::sync::Arc<super::Shader>]) {
    if !on() {
        return;
    }
    let compute = shaders.iter().any(|s| s.stage == "compute");
    if REFUSED[0].load(Ordering::Relaxed) + REFUSED[1].load(Ordering::Relaxed) > 0 {
        MADE[usize::from(!compute)].fetch_add(1, Ordering::Relaxed);
    }
    let waiting = WAITING.with(Cell::take);
    count(
        on_made(waiting, compute),
        shaders.first().map(|s| s.hash.as_str()),
    );
    event();
}

#[cfg(test)]
mod tests {
    use super::*;

    const COMPUTE: Refused = Refused {
        slot: Slot::Compute,
        hash: 7,
    };
    const GRAPHICS: Refused = Refused {
        slot: Slot::Graphics,
        hash: 0,
    };

    #[test]
    fn the_switch_is_live() {
        victory_core::settings::check_live("VICTORY_NATIVE_PIPELINE_DIAG", "1", || ON.get(), 0, 1);
    }

    /// A refusal followed by a pipeline of its kind on the same thread is a
    /// retry; of the other kind, or another refusal first, is not.
    #[test]
    fn retries_are_told_by_the_next_pipeline_of_the_thread() {
        assert_eq!(on_made(Some(COMPUTE), true), Outcome::Retried(COMPUTE));
        assert_eq!(on_made(Some(COMPUTE), false), Outcome::NotRetried(COMPUTE));
        assert_eq!(on_made(Some(GRAPHICS), false), Outcome::Retried(GRAPHICS));
        assert_eq!(on_made(None, true), Outcome::Nothing);
        assert_eq!(on_refused(Some(COMPUTE)), Outcome::NotRetried(COMPUTE));
        assert_eq!(on_refused(None), Outcome::Nothing);
    }

    /// The description's words and registers read as PR #98 reads them.
    #[test]
    fn a_description_names_its_program() {
        let mut words = [0u64; 12];
        (words[6], words[8], words[11]) = (0x400, 0x88, 0xdead_beef_0123_4567);
        let mut registers = [0u8; 0x1c];
        registers[0xc..0x12].copy_from_slice(&[8, 0, 8, 0, 1, 0]);
        assert_eq!(
            program(&words, Some(&registers)),
            Program {
                hash: 0xdead_beef_0123_4567,
                code_bytes: 0x400,
                table_bytes: 0x88,
                threads: [8, 8, 1],
            }
        );
        assert_eq!(program(&words, None).threads, [0; 3]);
    }
}
