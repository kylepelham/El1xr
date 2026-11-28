//! (El1xr) `VICTORY_BVH_DECLARE_OUTPUTS=1` (off by default): a list that
//! reads and writes title memory by address (`crate::memory`: the structure
//! builder) declares written only the copies the builder can write, not
//! every numbered copy.
//!
//! Without it every list with a builder dispatch declares all numbered
//! copies written: each is snapshotted on the CPU (or compared on the card,
//! with a card side) and landed after. Batch 5 part F/I: 6-8 GB of windows,
//! snapshots 580-770 ms a frame, then card windows grown until the PC ran out
//! of commit; round 6b still 56 ms of copies (308 MB) and 55 ms batch_flush.
//!
//! With it a copy is written when one of its 4 MiB cells is:
//! - a build's destination or scratch, as the builder hook names them
//!   (`victory_hle::bvh_hook`: batched builds, rehydrate, copy), and the
//!   cell after each (a structure may cross a window's end, as
//!   `crate::memory` names them);
//! - named by a writing dispatch's root arguments (views, address pairs,
//!   256-byte units), and the cell after, unless the hook named that cell
//!   only as an input.
//!
//! Vertices, indices, transforms, instances and an update's source are
//! inputs: read only to the builder. Their copies are still brought up to
//! date, but get no snapshot, no compare and no landing, and no card side
//! beyond the windows' own total (`shadow::builder_windows`). A copy once
//! written stays written for the run (a superset is safe).
//!
//! Risk: a write through a pointer the builder reads out of memory, into a
//! copy nothing above names (a fault-made copy), is not copied back: title
//! memory stays stale, and the next refresh puts the title's bytes over the
//! card's. `VICTORY_BVH_DECLARE_CHECK=N` (with the switch): every N frames
//! one copy not declared written is declared for one builder list, and a
//! landing of card bytes in it is logged ("written though undeclared").

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};

/// A cell: the 4 MiB windows `crate::memory` names.
pub(crate) const CELL: u64 = 4 << 20;
/// Cells of the 40-bit title address space.
const CELLS: usize = 1 << (40 - 22);

/// A build's destination or scratch (the hook).
pub(crate) const HOOK_WRITTEN: u8 = 1;
/// Named by the hook (every range of a build: its outputs too, which
/// [`HOOK_WRITTEN`] marks as well; the rest are inputs: vertices, indices,
/// transforms, instances, an update's source).
pub(crate) const HOOK_READ: u8 = 2;
/// Named by a writing dispatch's root arguments.
pub(crate) const ROOT_OUT: u8 = 4;

/// Whether only the builder's outputs are declared written.
pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        let on = std::env::var("VICTORY_BVH_DECLARE_OUTPUTS").is_ok_and(|v| v.trim() == "1");
        if on {
            tracing::info!(
                "bvh declare outputs: lists that write title memory by address declare written only the builds' \
                 destinations, scratch and what their root arguments name (VICTORY_BVH_DECLARE_OUTPUTS=1); check every {} frames",
                check_every()
            );
        }
        on
    })
}

/// `VICTORY_BVH_DECLARE_CHECK=N`: frames between checks (0: none).
pub(crate) fn check_every() -> u64 {
    static N: OnceLock<u64> = OnceLock::new();
    *N.get_or_init(|| {
        std::env::var("VICTORY_BVH_DECLARE_CHECK")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0)
    })
}

/// Whether a cell's marks make it written: the hook's output, or a root
/// output the hook did not name as an input.
pub(crate) fn written(flags: u8) -> bool {
    flags & HOOK_WRITTEN != 0 || (flags & ROOT_OUT != 0 && flags & HOOK_READ == 0)
}

/// Per cell of title address space, its marks.
pub(crate) struct Cells(Box<[AtomicU8]>);

impl Cells {
    pub(crate) fn new() -> Cells {
        Cells((0..CELLS).map(|_| AtomicU8::new(0)).collect())
    }

    fn at(&self, address: u64) -> Option<&AtomicU8> {
        self.0.get((address / CELL) as usize)
    }

    pub(crate) fn get(&self, address: u64) -> u8 {
        self.at(address).map_or(0, |c| c.load(Ordering::Relaxed))
    }

    /// Marks `address`'s cell and the one after with `bit`.
    pub(crate) fn mark(&self, address: u64, bit: u8) {
        for a in [address, (address & !(CELL - 1)) + CELL] {
            if let Some(c) = self.at(a) {
                c.fetch_or(bit, Ordering::Relaxed);
            }
        }
    }

    /// The hook names every range of a build (`named`) and which of them it
    /// writes (`outputs`); zeros are passed over. A named range marks only
    /// its own cell.
    pub(crate) fn hook(&self, outputs: &[u64], named: &[u64]) {
        for &a in outputs.iter().filter(|&&a| a != 0) {
            self.mark(a, HOOK_WRITTEN);
        }
        for &a in named.iter().filter(|&&a| a != 0) {
            if let Some(c) = self.at(a) {
                c.fetch_or(HOOK_READ, Ordering::Relaxed);
            }
        }
    }

    /// A writing dispatch's root arguments name `address`: its cell and the
    /// one after are root outputs, unless the hook named its cell only as an
    /// input (then nothing is marked). Whether it was marked.
    pub(crate) fn root_output(&self, address: u64) -> bool {
        let f = self.get(address);
        if f & HOOK_READ != 0 && f & HOOK_WRITTEN == 0 {
            return false;
        }
        self.mark(address, ROOT_OUT);
        true
    }

    /// Whether any cell of `[start, start + len)` is written.
    pub(crate) fn span_written(&self, start: u64, len: u64) -> bool {
        let (first, last) = (start / CELL, (start + len.max(1) - 1) / CELL);
        (first..=last).any(|c| written(self.get(c * CELL)))
    }
}

/// The run's marks.
pub(crate) fn cells() -> &'static Cells {
    static CELLS_: OnceLock<Cells> = OnceLock::new();
    CELLS_.get_or_init(Cells::new)
}

/// The copies (by index) a writing list declares now: with `from` the
/// generation the list last declared at (`None`: it declared nothing yet, so
/// every written copy), those whose `written_at` (0: not written) is newer.
pub(crate) fn to_declare(written_at: &[u64], from: Option<u64>) -> Vec<usize> {
    let from = from.unwrap_or(0);
    written_at
        .iter()
        .enumerate()
        .filter(|&(_, &at)| at > from)
        .map(|(n, _)| n)
        .collect()
}

/// The next copy to check after `cursor`: one not written, round robin.
pub(crate) fn pick_check(written_at: &[u64], cursor: usize) -> Option<usize> {
    let n = written_at.len();
    (1..=n)
        .map(|i| (cursor + i) % n)
        .find(|&i| written_at[i] == 0)
}

/// The copy under check (its id, 0: none), the frame it was armed in, and
/// whether something else had declared it written before.
static CHECK: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];
/// Checks armed, checks that landed bytes, bytes landed.
static CHECKED: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

/// A check of copy `id` (`[start, start + len)`) is armed in `frame`;
/// `others`: something had declared it written before.
pub(crate) fn arm(id: u64, frame: u64, others: bool) {
    CHECK[1].store(frame, Ordering::Relaxed);
    CHECK[2].store(u64::from(others), Ordering::Relaxed);
    CHECK[0].store(id, Ordering::Relaxed);
    CHECKED[0].fetch_add(1, Ordering::Relaxed);
}

/// A merge of copy `id` (`[start, start + len)`) wrote `bytes` of the
/// card's at `address` into title memory. Logged when it is the copy under
/// check.
pub(crate) fn landed(id: u64, start: u64, len: u64, address: u64, bytes: usize) {
    if bytes == 0 || id == 0 || CHECK[0].load(Ordering::Relaxed) != id {
        return;
    }
    let first = CHECK[0].swap(0, Ordering::Relaxed) == id;
    if first {
        CHECKED[1].fetch_add(1, Ordering::Relaxed);
    }
    CHECKED[2].fetch_add(bytes as u64, Ordering::Relaxed);
    let n = CHECKED[1].load(Ordering::Relaxed);
    if first && (n <= 32 || n.is_multiple_of(256)) {
        tracing::warn!(
            copy_start = format_args!("{start:#x}"),
            copy_bytes = len,
            at = format_args!("{address:#x}"),
            bytes,
            armed_frame = CHECK[1].load(Ordering::Relaxed),
            frame = crate::frame::current(),
            others_declare_it = CHECK[2].load(Ordering::Relaxed) == 1,
            "bvh declare check: a copy not declared written was written though undeclared ({n} of {} checks)",
            CHECKED[0].load(Ordering::Relaxed)
        );
    }
}

/// Checks armed and checks that found card bytes.
pub(crate) fn checks() -> [u64; 2] {
    [
        CHECKED[0].load(Ordering::Relaxed),
        CHECKED[1].load(Ordering::Relaxed),
    ]
}

/// Windows turned down for a card side past the windows' own total because
/// the builder only reads them, and their bytes.
pub(crate) static READ_ONLY_NO_CARD: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

/// Whether a window `[start, start + size)` may take a card side past the
/// windows' own total: always without the switch; with it only when the
/// builder writes it.
pub(crate) fn card_side(start: u64, size: u64) -> bool {
    if !on() || cells().span_written(start, size) {
        return true;
    }
    READ_ONLY_NO_CARD[0].fetch_add(1, Ordering::Relaxed);
    READ_ONLY_NO_CARD[1].fetch_add(size, Ordering::Relaxed);
    false
}

#[cfg(test)]
#[path = "bvh_declare_tests.rs"]
mod tests;
