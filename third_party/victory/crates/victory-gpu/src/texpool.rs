//! Host memory for the textures the title fills and for their staging
//! buffers, cut from blocks of 64 MiB made once, instead of an allocation
//! each (`VICTORY_TEXTURE_HEAPS=1`, `VICTORY_STAGING_BLOCKS=1`; both off by
//! default: an allocation each, as before).
//!
//! Every host copy of a title texture was a `CreateCommittedResource` of its
//! own (`crate::resource` `host_texture_as`), made on the title's recording
//! threads, and every staging buffer an upload buffer of its own. Upstream
//! measured about 310 us an allocation on an RX 7900 XTX, whatever its size
//! and serialised in the driver, against about 6 for a piece of a block;
//! `allocation_costs` below measures this PC's card.
//!
//! Source: uiliannn/VItoriaBr 5668b8bbdc (port of myokex/victory-amd
//! c3723f3, after shadowdog00 vi-pc-combo 0781f61): the two switches, and
//! which textures may lie in a block: placed by the title and filled from a
//! layout the upload reads, so what a piece held before is never seen.
//!
//! Source: DoubleGhosty/cool_fox 42b95b9, 0322b39abd, f1d3eb2b64,
//! 3b73cb0eec (`pool.rs`): the pool; a texture's piece given back with the
//! last reference to it and rested until the card has run what was sent
//! before; and the two guards without which fresh blocks lost the device in
//! their runs n08 and n11: blocks made ahead and cut only once every
//! submission begun before them has finished (`VICTORY_RIPE_BLOCKS`), and
//! new blocks made resident and mapped into the card's address space before
//! anything names them (`VICTORY_WAIT_RESIDENT`).
//!
//! GPL-3.0-or-later, as the projects it comes from.

use std::collections::{BTreeMap, VecDeque};
use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

use windows::Win32::Graphics::Direct3D12::*;
use windows::core::{GUID, HRESULT, IUnknown, Interface};

mod buffers;
mod reuse;
#[cfg(test)]
mod unmap_costs;
pub(crate) use buffers::{card_buffer, shared_buffer, upload_buffer};

/// Bytes of a block. Pieces of more than half of one are not cut (with
/// blocks made ahead, [`ahead`]: of more than one).
const BLOCK: u64 = 64 << 20;
/// Empty blocks kept while the card is under [`ROOM_PERCENT`] of its
/// budget, and past it.
const SPARE_KEPT: usize = 16;
const SPARE: usize = 2;
/// The same for blocks of staging buffers (the PC's memory).
const STAGING_KEPT: usize = 8;
const ROOM_PERCENT: u64 = 85;
/// How many empty texture blocks stay now ([`refresh_spares`]).
static SPARES: AtomicUsize = AtomicUsize::new(SPARE);

/// A piece given back rests at least this long (ms), and until [`LAG`]
/// submissions past every one begun when it came back have finished; or
/// [`REST`] ms, whichever comes first. (Windows resets a card that runs one
/// submission that long.)
const SETTLE: u64 = 100;
const REST: u64 = 2500;
/// Submissions past the newest begun that must finish too: one may be in
/// preparation, its lists recorded, its number not taken yet.
const LAG: u64 = 2;

fn switch(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|v| v == "1")
}

/// `VICTORY_TEXTURE_HEAPS=1`: host copies of the textures the title places
/// and fills lie in blocks ([`texture`]).
pub(crate) fn texture_heaps() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        let on = switch("VICTORY_TEXTURE_HEAPS");
        if on {
            tracing::info!(
                ahead = ahead(),
                resident = wait_resident(),
                "host textures the title fills lie in 64 MiB blocks (VICTORY_TEXTURE_HEAPS=1)"
            );
        }
        on
    })
}

/// `VICTORY_STAGING_BLOCKS=1`: staging buffers are pieces of upload blocks
/// ([`staging`]).
pub(crate) fn staging_blocks() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        let on = switch("VICTORY_STAGING_BLOCKS");
        if on {
            tracing::info!(
                "staging buffers are pieces of 64 MiB upload blocks (VICTORY_STAGING_BLOCKS=1)"
            );
        }
        on
    })
}

/// New blocks are made not resident, made resident and waited for until
/// the card has them mapped ([`mapped`]) before a piece of one is named (on
/// with the blocks; `VICTORY_WAIT_RESIDENT=0`: not).
fn wait_resident() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_WAIT_RESIDENT").is_none_or(|v| v != "0"))
}

/// How many empty texture blocks are made ahead, so that the block a piece
/// is cut from has been there while the lists sent before it ran
/// (`VICTORY_RIPE_BLOCKS`, 12; 0: none, a new block is cut at once). Never
/// more than are kept ([`SPARES`]).
fn ahead() -> usize {
    static AHEAD: OnceLock<usize> = OnceLock::new();
    *AHEAD.get_or_init(|| {
        std::env::var("VICTORY_RIPE_BLOCKS")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(12)
    })
}

/// Submissions begun and finished, and how many blocks to make ahead.
#[derive(Clone, Copy)]
struct Ripe {
    started: u64,
    finished: u64,
    ahead: usize,
}

impl Ripe {
    const ALL: Ripe = Ripe {
        started: 0,
        finished: 0,
        ahead: 0,
    };

    fn now() -> Ripe {
        Ripe::ahead(SPARES.load(Ordering::Relaxed))
    }

    /// Now, with at most `most` blocks made ahead.
    fn ahead(most: usize) -> Ripe {
        Ripe {
            started: crate::host::submissions_started(),
            finished: crate::host::submissions_finished(),
            ahead: ahead().min(most),
        }
    }

    /// Every submission begun when `block` was made has finished.
    fn is<B>(self, block: &Block<B>) -> bool {
        self.ahead == 0 || block.made <= self.finished
    }
}

/// When a piece came back (ms), and the submissions begun by then.
#[derive(Clone, Copy)]
struct Resting {
    at: u64,
    begun: u64,
}

impl Resting {
    fn now(at: u64) -> Resting {
        Resting {
            at,
            begun: crate::host::submissions_started(),
        }
    }

    fn over(self, now: u64, finished: u64) -> bool {
        now >= self.at + REST || self.done(now, finished)
    }

    /// (El1xr) Every submission begun when it came back has finished, with no
    /// time limit: before the copy queue writes zeros over it (`buffers::reuse`).
    fn done(self, now: u64, finished: u64) -> bool {
        now >= self.at + SETTLE && finished >= self.begun + LAG
    }
}

/// The free ranges of a block: start and length, never two that touch.
struct Ranges(BTreeMap<u64, u64>);

impl Ranges {
    fn new(size: u64) -> Ranges {
        Ranges(BTreeMap::from([(0, size)]))
    }

    /// The lowest place for `len` bytes at a multiple of `align`, from
    /// `least` on.
    fn take(&mut self, len: u64, align: u64, least: u64) -> Option<u64> {
        let (start, size, at) = self.0.iter().find_map(|(&start, &size)| {
            let at = start.max(least).next_multiple_of(align);
            (at + len <= start + size).then_some((start, size, at))
        })?;
        self.0.remove(&start);
        if at > start {
            self.0.insert(start, at - start);
        }
        if at + len < start + size {
            self.0.insert(at + len, start + size - at - len);
        }
        Some(at)
    }

    fn give(&mut self, mut start: u64, mut len: u64) {
        if let Some((&before, &size)) = self.0.range(..start).next_back()
            && before + size == start
        {
            self.0.remove(&before);
            (start, len) = (before, len + size);
        }
        if let Some(size) = self.0.remove(&(start + len)) {
            len += size;
        }
        self.0.insert(start, len);
    }

    fn whole(&self, size: u64) -> bool {
        self.0.get(&0) == Some(&size)
    }
}

/// A part of a block.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Piece {
    block: usize,
    start: u64,
    len: u64,
}

struct Block<B> {
    memory: B,
    free: Ranges,
    /// The end of the highest piece ever taken: above it the memory is as
    /// it was made.
    used_to: u64,
    /// The submissions begun when it was made ([`Ripe`]).
    made: u64,
}

struct Pool<B> {
    /// `None`: a block let go, its place to be used again.
    blocks: Vec<Option<Block<B>>>,
    resting: VecDeque<(Resting, Piece)>,
    /// The most empty blocks kept.
    most: usize,
    /// No block is asked for before this (ms): the host refused the last.
    retry_at: u64,
    /// Blocks alive, for [`free_texture_bytes`].
    alive: &'static AtomicU64,
    /// Pieces are cut only from memory never used before, as it was made
    /// (zeros); a block every piece of which has come back is let go
    /// (`buffers`: memory the card writes that we cannot clear here).
    fresh: bool,
    /// A thread is making a block for this pool outside its lock
    /// ([`take_from`]).
    making: bool,
    /// (El1xr) A pool of the PC's memory: past `VICTORY_UPLOAD_CAP_MB` it
    /// keeps no empty block (`crate::commitline`).
    capped: bool,
    /// (El1xr) A fresh pool whose pieces are zeroed and cut again
    /// (`VICTORY_CARD_BLOCK_REUSE`, `buffers::reuse`): `free` holds only memory
    /// as it was made or zeroed since; a piece back and done resting waits in
    /// `dirty` until the copy queue zeroes it (`clearing`, by fence value).
    reuse: bool,
    dirty: Vec<Piece>,
    clearing: VecDeque<(u64, Piece)>,
}

impl<B: Clone> Pool<B> {
    const fn new(most: usize, alive: &'static AtomicU64) -> Pool<B> {
        Pool {
            blocks: Vec::new(),
            resting: VecDeque::new(),
            most,
            retry_at: 0,
            alive,
            fresh: false,
            making: false,
            capped: false,
            reuse: false,
            dirty: Vec::new(),
            clearing: VecDeque::new(),
        }
    }

    /// The same, kept small past `VICTORY_UPLOAD_CAP_MB` ([`Pool::capped`]).
    const fn capped(mut self) -> Pool<B> {
        self.capped = true;
        self
    }

    /// The empty blocks it keeps now: none past the cap.
    fn spares(&self) -> usize {
        if self.capped && crate::commitline::upload_over_cap() { 0 } else { SPARES.load(Ordering::Relaxed) }
    }

    /// A pool that cuts only fresh memory ([`Pool::fresh`]).
    const fn fresh(most: usize, alive: &'static AtomicU64) -> Pool<B> {
        Pool {
            blocks: Vec::new(),
            resting: VecDeque::new(),
            most,
            retry_at: 0,
            alive,
            fresh: true,
            making: false,
            capped: false,
            reuse: false,
            dirty: Vec::new(),
            clearing: VecDeque::new(),
        }
    }

    /// Where a piece of `block` may start: past what was ever used in a
    /// fresh pool.
    fn least(&self, block: &Block<B>) -> u64 {
        if self.only_unused() { block.used_to } else { 0 }
    }

    /// Pieces are cut only above what was ever used: fresh, and not reusing.
    fn only_unused(&self) -> bool {
        self.fresh && !self.reuse
    }

    /// A block with no piece that can be cut whole again.
    fn empty(&self, block: &Block<B>) -> bool {
        block.free.whole(BLOCK) && (!self.only_unused() || block.used_to == 0)
    }

    /// A piece of `len` bytes, the memory it lies in, and whether that part
    /// was used before; `make` makes a block when none has room.
    #[cfg(test)]
    fn take(
        &mut self,
        len: u64,
        align: u64,
        now: u64,
        make: impl FnMut() -> Option<B>,
    ) -> Option<(Piece, B, bool)> {
        self.take_ripe(len, align, now, Ripe::ALL, make)
    }

    /// The same from a ripe block if one has room, else from the oldest;
    /// `make` makes one ahead too while fewer than `ripe.ahead` are empty.
    /// (All under the caller's lock: the tests'. The pools themselves go
    /// through [`take_from`], which makes blocks outside it.)
    #[cfg(test)]
    fn take_ripe(
        &mut self,
        len: u64,
        align: u64,
        now: u64,
        ripe: Ripe,
        mut make: impl FnMut() -> Option<B>,
    ) -> Option<(Piece, B, bool)> {
        if too_big(len, ripe) {
            return None;
        }
        let mut freed = Vec::new();
        let taken = match self.cut(len, align, now, ripe, &mut freed) {
            Some(taken) => taken,
            None => {
                let memory = self.grow(now, &mut make)?;
                self.add(memory, ripe.started);
                self.cut(len, align, now, ripe, &mut freed)?
            }
        };
        if self.wants_ahead(ripe)
            && let Some(memory) = self.grow(now, &mut make)
        {
            self.add(memory, ripe.started);
        }
        Some(taken)
    }

    /// A piece of `len` bytes from the blocks there are (none is made):
    /// from a ripe block if one has room, else from the oldest. The blocks
    /// [`Pool::reclaim`] lets go of are put in `freed`, for the caller to
    /// drop outside its lock.
    fn cut(&mut self, len: u64, align: u64, now: u64, ripe: Ripe, freed: &mut Vec<B>) -> Option<(Piece, B, bool)> {
        freed.extend(self.reclaim(now, crate::host::submissions_finished(), self.spares()));
        let mut found = None;
        let fresh = self.only_unused();
        for (i, block) in self.blocks.iter_mut().enumerate() {
            if let Some(block) = block
                && ripe.is(block)
                && let Some(start) = block.free.take(len, align, if fresh { block.used_to } else { 0 })
            {
                found = Some((i, start));
                break;
            }
        }
        if found.is_none() && ripe.ahead > 0 {
            let mut unripe: Vec<(u64, usize)> =
                self.blocks.iter().enumerate().filter_map(|(i, b)| Some((b.as_ref()?.made, i))).collect();
            unripe.sort_unstable();
            found = unripe.into_iter().find_map(|(_, i)| {
                let least = self.least(self.blocks[i].as_ref()?);
                Some((i, self.blocks[i].as_mut()?.free.take(len, align, least)?))
            });
        }
        let (i, start) = found?;
        if ripe.ahead > 0 && !ripe.is(self.blocks[i].as_ref()?) {
            let n = STATS[3].fetch_add(1, Ordering::Relaxed) + 1;
            if n.is_power_of_two() {
                tracing::info!("pieces cut from blocks made while lists ran that still run: {n}");
            }
        }
        let block = self.blocks[i].as_mut()?;
        let again = start < block.used_to;
        block.used_to = block.used_to.max(start + len);
        Some((Piece { block: i, start, len }, block.memory.clone(), again))
    }

    /// Fewer than `ripe.ahead` blocks are empty: one is to be made ahead.
    fn wants_ahead(&self, ripe: Ripe) -> bool {
        ripe.ahead > 0 && self.blocks.iter().flatten().filter(|b| self.empty(b)).count() < ripe.ahead
    }

    /// No block is asked for: the host refused one in the last half second.
    fn held_back(&self, now: u64) -> bool {
        now < self.retry_at
    }

    fn refused(&mut self, now: u64) {
        self.retry_at = now.saturating_add(crate::alloc_backoff::RETRY_MS);
    }

    /// A new block, unless the host refused one in the last half second.
    #[cfg(test)]
    fn grow(&mut self, now: u64, make: &mut impl FnMut() -> Option<B>) -> Option<B> {
        if self.held_back(now) {
            return None;
        }
        let block = make();
        if block.is_none() {
            self.refused(now);
        }
        block
    }

    fn add(&mut self, memory: B, made: u64) -> usize {
        let i = match self.blocks.iter().position(Option::is_none) {
            Some(i) => i,
            None => {
                self.blocks.push(None);
                self.blocks.len() - 1
            }
        };
        self.blocks[i] = Some(Block {
            memory,
            free: Ranges::new(BLOCK),
            used_to: 0,
            made,
        });
        self.alive.fetch_add(1, Ordering::Relaxed);
        i
    }

    fn give(&mut self, piece: Piece, now: u64) {
        self.resting.push_back((Resting::now(now), piece));
    }

    /// Frees the pieces that rested long enough, and lets go of the empty
    /// blocks past `spares` (and past the pool's most): returned, to be
    /// dropped outside the pool's lock (a block of mapped memory is
    /// unmapped and freed when it goes).
    /// (El1xr) Frees the pieces that rested long enough, then takes every
    /// empty block, kept or not (`VICTORY_UPLOAD_CAP_MB`).
    fn trim(&mut self, now: u64, finished: u64) -> Vec<B> {
        let mut gone = self.reclaim(now, finished, 0);
        for block in &mut self.blocks {
            if block.as_ref().is_some_and(|b| b.free.whole(BLOCK)) {
                gone.extend(block.take().map(|b| b.memory));
                self.alive.fetch_sub(1, Ordering::Relaxed);
            }
        }
        gone
    }

    /// (El1xr) For the commit line: bytes its pieces in use hold (resting ones not counted),
    /// and bytes a new piece may still be cut from (in a fresh pool only above what was ever
    /// used). The rest of its blocks is resting or, in a fresh pool, used once and never cut
    /// again until the whole block is back.
    fn usage(&self) -> [u64; 2] {
        let (mut held, mut cuttable) = (0u64, 0u64);
        for block in self.blocks.iter().flatten() {
            let free: u64 = block.free.0.values().sum();
            held += BLOCK - free;
            cuttable += if self.only_unused() { BLOCK - block.used_to } else { free };
        }
        let resting: u64 = self.resting.iter().map(|(_, piece)| piece.len).sum::<u64>()
            + self.dirty.iter().map(|piece| piece.len).sum::<u64>()
            + self.clearing.iter().map(|(_, piece)| piece.len).sum::<u64>();
        [held.saturating_sub(resting), cuttable]
    }

    fn reclaim(&mut self, now: u64, finished: u64, spares: usize) -> Vec<B> {
        let mut gone = Vec::new();
        let mut any = false;
        let reuse = self.reuse;
        while let Some(&(resting, piece)) = self.resting.front()
            && if reuse { resting.done(now, finished) } else { resting.over(now, finished) }
        {
            self.resting.pop_front();
            if reuse {
                self.dirty.push(piece);
                any = true;
            } else if let Some(block) = self.blocks[piece.block].as_mut() {
                block.free.give(piece.start, piece.len);
                any |= block.free.whole(BLOCK);
            }
        }
        if !any {
            return gone;
        }
        if reuse {
            gone.extend(self.let_go_unheld());
        }
        gone.extend(self.let_go_spares(spares));
        gone
    }

    /// (El1xr) Reuse: the blocks every piece of which is back, free or waiting
    /// for zeros (none held, resting or being zeroed), go unzeroed, as used
    /// blocks of a fresh pool go.
    fn let_go_unheld(&mut self) -> Vec<B> {
        let mut back = vec![0u64; self.blocks.len()];
        for piece in &self.dirty {
            back[piece.block] += piece.len;
        }
        let mut gone = Vec::new();
        for (i, block) in self.blocks.iter_mut().enumerate() {
            if back[i] > 0 && block.as_ref().is_some_and(|b| b.free.0.values().sum::<u64>() + back[i] == BLOCK) {
                gone.extend(block.take().map(|b| b.memory));
                self.alive.fetch_sub(1, Ordering::Relaxed);
            }
        }
        if !gone.is_empty() {
            let blocks = &self.blocks;
            self.dirty.retain(|piece| blocks[piece.block].is_some());
        }
        gone
    }

    /// (El1xr) Reuse: up to `most` bytes of the pieces waiting for zeros (at
    /// least one), oldest first, each with its block's memory; they are in no
    /// list until [`Pool::clearing`] or [`Pool::dirty_again`] takes them back,
    /// so their blocks stay.
    fn take_dirty(&mut self, most: u64) -> Vec<(Piece, B)> {
        let mut bytes = 0;
        let n = self.dirty.iter().take_while(|piece| {
            bytes += piece.len;
            bytes <= most
        });
        let n = n.count().max(1).min(self.dirty.len());
        let blocks = &self.blocks;
        self.dirty
            .drain(..n)
            .filter_map(|piece| Some((piece, blocks[piece.block].as_ref()?.memory.clone())))
            .collect()
    }

    /// (El1xr) Reuse: `pieces` are being zeroed by the copy that fence value
    /// `value` follows.
    fn clearing(&mut self, value: u64, pieces: impl IntoIterator<Item = Piece>) {
        self.clearing.extend(pieces.into_iter().map(|piece| (value, piece)));
    }

    /// (El1xr) Reuse: `pieces` were not sent to be zeroed; they wait again.
    fn dirty_again(&mut self, pieces: impl IntoIterator<Item = Piece>) {
        self.dirty.extend(pieces);
    }

    /// (El1xr) Reuse: the pieces whose zeros the copy queue has written by
    /// fence value `completed` may be cut again; the empty blocks past
    /// `spares` go.
    fn cleared(&mut self, completed: u64, spares: usize) -> Vec<B> {
        let mut any = false;
        while let Some(&(value, piece)) = self.clearing.front()
            && value <= completed
        {
            self.clearing.pop_front();
            if let Some(block) = self.blocks[piece.block].as_mut() {
                block.free.give(piece.start, piece.len);
                any |= block.free.whole(BLOCK);
            }
        }
        if any { self.let_go_spares(spares) } else { Vec::new() }
    }

    /// The empty blocks past `spares` (and past the pool's most) go, and in a
    /// fresh pool that does not reuse, every used one.
    fn let_go_spares(&mut self, spares: usize) -> Vec<B> {
        let mut gone = Vec::new();
        let most = spares.min(self.most);
        let fresh = self.only_unused();
        let mut spare = 0;
        for block in &mut self.blocks {
            if let Some(b) = block.as_ref()
                && b.free.whole(BLOCK)
            {
                spare += 1;
                if spare > most || (fresh && b.used_to > 0) {
                    gone.extend(block.take().map(|b| b.memory));
                    self.alive.fetch_sub(1, Ordering::Relaxed);
                }
            }
        }
        gone
    }

    #[cfg(test)]
    fn blocks(&self) -> usize {
        self.blocks.iter().flatten().count()
    }
}

/// A piece too large for a block of the pool (more than half of one, or
/// with blocks made ahead more than one), or of nothing.
fn too_big(len: u64, ripe: Ripe) -> bool {
    len == 0 || len > if ripe.ahead > 0 { BLOCK } else { BLOCK / 2 }
}

/// The pools, for their time lines ([`TIME`]).
#[derive(Clone, Copy)]
pub(crate) enum Which {
    Texture,
    Staging,
    Card,
    Shared,
    Upload,
}

const NAMES: [&str; 5] = ["texture", "staging", "card", "shared", "upload"];

/// Per pool, since its last time line: pieces taken, microseconds taking
/// them (lock waits, cuts, placing, zeroing), of which waiting for the
/// pool's lock; blocks made and their microseconds, of which making them
/// resident and waiting for the card to map them; blocks let go and their
/// microseconds; bytes of reused pieces zeroed and the microseconds;
/// pieces left to a committed allocation of their own because another
/// thread was making the block they needed.
pub(crate) static TIME: [[AtomicU64; 11]; 5] = [const { [const { AtomicU64::new(0) }; 11] }; 5];
pub(crate) const T_PIECES: usize = 0;
pub(crate) const T_TAKE_US: usize = 1;
pub(crate) const T_WAIT_US: usize = 2;
pub(crate) const T_MADE: usize = 3;
pub(crate) const T_MAKE_US: usize = 4;
pub(crate) const T_RESIDENT_US: usize = 5;
pub(crate) const T_FREED: usize = 6;
pub(crate) const T_FREE_US: usize = 7;
pub(crate) const T_ZERO_BYTES: usize = 8;
pub(crate) const T_ZERO_US: usize = 9;
pub(crate) const T_BUSY: usize = 10;

pub(crate) fn time(which: Which, field: usize, value: u64) {
    TIME[which as usize][field].fetch_add(value, Ordering::Relaxed);
}

fn us(since: std::time::Instant) -> u64 {
    since.elapsed().as_micros() as u64
}

/// The pools' time lines, one a pool that did anything, at most every five
/// seconds: grep `pool time (` (and `title memory remapped` beside them).
fn time_lines(now: u64) {
    static LAST: AtomicU64 = AtomicU64::new(0);
    let last = LAST.load(Ordering::Relaxed);
    if now < last + 5000 || LAST.compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed).is_err() {
        return;
    }
    let secs = if last == 0 { 5.0 } else { (now - last) as f64 / 1e3 };
    for (i, name) in NAMES.iter().enumerate() {
        let t = TIME[i].each_ref().map(|t| t.swap(0, Ordering::Relaxed));
        if t.iter().all(|&v| v == 0) {
            continue;
        }
        tracing::info!(
            "pool time ({name}) over {secs:.1} s: {} pieces, {:.1} ms taking them ({:.1} ms of it waiting for the lock), \
             {} blocks made in {:.1} ms ({:.1} ms of it making them resident), {} let go in {:.1} ms, {:.1} MB zeroed in {:.1} ms, \
             {} left to committed while a block was made",
            t[T_PIECES],
            t[T_TAKE_US] as f64 / 1e3,
            t[T_WAIT_US] as f64 / 1e3,
            t[T_MADE],
            t[T_MAKE_US] as f64 / 1e3,
            t[T_RESIDENT_US] as f64 / 1e3,
            t[T_FREED],
            t[T_FREE_US] as f64 / 1e3,
            t[T_ZERO_BYTES] as f64 / 1e6,
            t[T_ZERO_US] as f64 / 1e3,
            t[T_BUSY],
        );
    }
}

/// Drops blocks a pool let go of, outside its lock, timed. (texflicker)
/// The staging pool's with `VICTORY_PREPARE_FENCED=1`: on `victory-reaper`
/// once the submissions begun before have finished
/// (`crate::texture::prepare_fenced`), not by the thread taking a piece.
fn let_go<B: 'static>(which: Which, freed: Vec<B>) {
    if freed.is_empty() {
        return;
    }
    let began = std::time::Instant::now();
    let n = freed.len() as u64;
    if matches!(which, Which::Staging) && crate::texture::prepare_fenced::on() {
        crate::texture::prepare_fenced::let_go(freed, n as usize, true);
    } else {
        drop(freed);
    }
    time(which, T_FREED, n);
    time(which, T_FREE_US, us(began));
}

/// Makes a block for `pool` outside its lock, then adds it.
fn make_into<B: Clone>(
    pool: &'static Mutex<Pool<B>>,
    which: Which,
    started: u64,
    make: &impl Fn() -> Option<B>,
) -> bool {
    let began = std::time::Instant::now();
    let memory = make();
    let made = us(began);
    let mut p = lock(pool);
    p.making = false;
    match memory {
        Some(memory) => {
            p.add(memory, started);
            drop(p);
            time(which, T_MADE, 1);
            time(which, T_MAKE_US, made);
            true
        }
        None => {
            p.refused(now());
            false
        }
    }
}

/// A piece of `len` bytes of `pool` (as [`Pool::take_ripe`]), with every
/// block made and let go outside the pool's lock: a block's making
/// (`CreateHeap`, making it resident and waiting for the card to map it,
/// mapping it) took tens of milliseconds in the game, and with it under
/// the lock every thread that wanted a piece waited (batch 1: card pieces
/// took 222 us each in the game against 6 on a quiet card). One thread
/// makes a pool's block at a time; another that needs one meanwhile gets
/// `None` and makes a committed allocation of its own, as without the pool.
fn take_from<B: Clone + 'static>(
    pool: &'static Mutex<Pool<B>>,
    which: Which,
    len: u64,
    align: u64,
    ripe: Ripe,
    make: impl Fn() -> Option<B>,
) -> Option<(Piece, B, bool)> {
    if too_big(len, ripe) {
        return None;
    }
    let began = std::time::Instant::now();
    let at = now();
    time_lines(at);
    let mut freed = Vec::new();
    let mut tries = 0;
    let taken = loop {
        let waited = std::time::Instant::now();
        let mut p = lock(pool);
        time(which, T_WAIT_US, us(waited));
        if let Some(taken) = p.cut(len, align, at, ripe, &mut freed) {
            let ahead = p.wants_ahead(ripe) && !p.making && !p.held_back(at);
            p.making |= ahead;
            drop(p);
            let_go(which, std::mem::take(&mut freed));
            if ahead {
                make_into(pool, which, ripe.started, &make);
            }
            break Some(taken);
        }
        if tries > 0 || p.making || p.held_back(at) {
            if p.making {
                time(which, T_BUSY, 1);
            }
            drop(p);
            let_go(which, std::mem::take(&mut freed));
            break None;
        }
        p.making = true;
        drop(p);
        let_go(which, std::mem::take(&mut freed));
        if !make_into(pool, which, ripe.started, &make) {
            break None;
        }
        tries += 1;
    };
    if taken.is_some() {
        time(which, T_PIECES, 1);
    }
    time(which, T_TAKE_US, us(began));
    taken
}

/// A mapped upload buffer.
#[derive(Clone)]
struct Upload(ID3D12Resource, *mut u8);
// SAFETY: the mapping is written by the owners of its pieces, each its own.
unsafe impl Send for Upload {}

static TEXTURE_BLOCKS: AtomicU64 = AtomicU64::new(0);
static STAGING_BLOCKS: AtomicU64 = AtomicU64::new(0);
static TEXTURES: Mutex<Pool<ID3D12Heap>> = Mutex::new(Pool::new(SPARE_KEPT, &TEXTURE_BLOCKS));
static STAGING: Mutex<Pool<Upload>> = Mutex::new(Pool::new(STAGING_KEPT, &STAGING_BLOCKS).capped());

/// (El1xr) Bytes of a block, for `crate::commitline`.
pub(crate) const BLOCK_BYTES: u64 = BLOCK;

/// (El1xr) Blocks alive per pool, as [`Which`] names them: texture,
/// staging, card, shared, upload.
pub(crate) fn blocks_alive() -> [u64; 5] {
    let b = buffers::blocks_alive();
    [TEXTURE_BLOCKS.load(Ordering::Relaxed), STAGING_BLOCKS.load(Ordering::Relaxed), b[0], b[1], b[2]]
}

/// (El1xr) Per pool, as [`blocks_alive`] names them: bytes held by pieces in use, and bytes
/// a piece may still be cut from (`Pool::usage`).
pub(crate) fn pool_usage() -> [[u64; 2]; 5] {
    let b = buffers::usage();
    [lock(&TEXTURES).usage(), lock(&STAGING).usage(), b[0], b[1], b[2]]
}

/// (El1xr) Frees the pieces of the pools of the PC's memory (staging,
/// shared, upload) that rested long enough and lets go of every empty block
/// they keep (`VICTORY_UPLOAD_CAP_MB`): the blocks let go.
pub(crate) fn trim_host_pools() -> u64 {
    let at = now();
    let finished = crate::host::submissions_finished();
    let freed = lock(&STAGING).trim(at, finished);
    let n = freed.len() as u64;
    let_go(Which::Staging, freed);
    n + buffers::trim(at, finished)
}

fn lock<T>(m: &'static Mutex<T>) -> std::sync::MutexGuard<'static, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn now() -> u64 {
    crate::host::now_ms()
}

/// Bytes of the pieces textures hold now.
static HELD: AtomicU64 = AtomicU64::new(0);

/// Since the start: textures placed in blocks and their microseconds,
/// texture blocks made, pieces cut from blocks that were not ripe, staging
/// pieces, staging blocks made, textures the blocks did not take.
static STATS: [AtomicU64; 7] = [const { AtomicU64::new(0) }; 7];

/// Bytes of texture blocks no texture holds. The system counts a block
/// whole as the card's memory in use: `crate::host` `video_memory` takes
/// these off, so the texture queue and the idle sweep do not see a fuller
/// card than there is.
pub(crate) fn free_texture_bytes() -> u64 {
    (TEXTURE_BLOCKS.load(Ordering::Relaxed) * BLOCK).saturating_sub(HELD.load(Ordering::Relaxed))
}

/// Sets how many empty texture blocks stay ([`SPARE_KEPT`] while the card
/// is under [`ROOM_PERCENT`] of its budget, else [`SPARE`]), once a second.
fn refresh_spares(now: u64) {
    static LAST: AtomicU64 = AtomicU64::new(0);
    let last = LAST.load(Ordering::Relaxed);
    if now < last + 1000
        || LAST
            .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
    {
        return;
    }
    let memory = crate::host::host().and_then(|h| h.video_memory());
    SPARES.store(spares(memory), Ordering::Relaxed);
}

fn spares(memory: Option<[u64; 4]>) -> usize {
    match memory {
        Some([budget, used, ..])
            if u128::from(used) * 100 < u128::from(budget) * u128::from(ROOM_PERCENT) =>
        {
            SPARE_KEPT
        }
        _ => SPARE,
    }
}

/// Part of a block of mapped upload memory, back in the pool when dropped.
pub(crate) struct StagingPiece(Piece);

impl Drop for StagingPiece {
    fn drop(&mut self) {
        lock(&STAGING).give(self.0, now());
    }
}

/// `bytes` of upload memory, all zeros, from a block ([`staging_blocks`]):
/// the piece, the block's buffer, where in it the piece starts (a multiple
/// of 512, as copies from a buffer want) and the piece's mapping. `None`:
/// switched off, too large, or the host refused a block.
pub(crate) fn staging(
    host: &crate::host::Host,
    bytes: u64,
) -> Option<(StagingPiece, ID3D12Resource, u64, *mut u8)> {
    if !staging_blocks() {
        return None;
    }
    let bytes = bytes.max(1).next_multiple_of(512);
    let (piece, memory, again) = take_from(&STAGING, Which::Staging, bytes, 512, Ripe::ALL, || {
        let (buffer, data) = host.upload_buffer(BLOCK)?;
        crate::host::name(&buffer, || "staging of textures (a block)".into());
        STATS[5].fetch_add(1, Ordering::Relaxed);
        Some(Upload(buffer, data))
    })?;
    STATS[4].fetch_add(1, Ordering::Relaxed);
    let owned = StagingPiece(piece);
    // SAFETY: the piece lies in the block's mapping.
    let data = unsafe { memory.1.add(piece.start as usize) };
    if again {
        // What is not detiled into it (memory not there yet) uploads zeros,
        // as from a buffer of its own.
        let began = std::time::Instant::now();
        // SAFETY: the piece is ours alone.
        unsafe { std::ptr::write_bytes(data, 0, bytes as usize) };
        time(Which::Staging, T_ZERO_BYTES, bytes);
        time(Which::Staging, T_ZERO_US, us(began));
    }
    Some((owned, memory.0, piece.start, data))
}

/// An object the host texture holds and releases when it goes: its piece
/// is given back then.
#[repr(C)]
struct Reclaim {
    methods: &'static [*const (); 3],
    references: AtomicU32,
    piece: Piece,
    give: fn(Piece),
}

struct Methods([*const (); 3]);
// SAFETY: function addresses only.
unsafe impl Sync for Methods {}

static RECLAIM: Methods = Methods([
    query as *const (),
    add_ref as *const (),
    release as *const (),
]);

unsafe extern "system" fn query(
    this: *mut c_void,
    iid: *const GUID,
    out: *mut *mut c_void,
) -> HRESULT {
    const E_NOINTERFACE: HRESULT = HRESULT(0x8000_4002_u32 as i32);
    const E_POINTER: HRESULT = HRESULT(0x8000_4003_u32 as i32);
    // SAFETY: COM's arguments.
    unsafe {
        if out.is_null() {
            return E_POINTER;
        }
        if iid.as_ref().is_some_and(|iid| *iid == IUnknown::IID) {
            add_ref(this);
            *out = this;
            return HRESULT(0);
        }
        *out = std::ptr::null_mut();
    }
    E_NOINTERFACE
}

unsafe extern "system" fn add_ref(this: *mut c_void) -> u32 {
    // SAFETY: one of ours, alive while a reference to it is.
    unsafe {
        (*this.cast::<Reclaim>())
            .references
            .fetch_add(1, Ordering::Relaxed)
            + 1
    }
}

unsafe extern "system" fn release(this: *mut c_void) -> u32 {
    // SAFETY: as above; the last reference frees the box it was made as.
    unsafe {
        let left = (*this.cast::<Reclaim>())
            .references
            .fetch_sub(1, Ordering::AcqRel)
            - 1;
        if left == 0 {
            let gone = Box::from_raw(this.cast::<Reclaim>());
            (gone.give)(gone.piece);
        }
        left
    }
}

/// A texture's piece back in its pool.
fn give_texture(piece: Piece) {
    HELD.fetch_sub(piece.len, Ordering::Relaxed);
    lock(&TEXTURES).give(piece, now());
}

/// Makes `texture` give `piece` back with `give` when the last reference
/// to it goes.
fn give_back_with(texture: &ID3D12Resource, piece: Piece, give: fn(Piece)) {
    const KEY: GUID = GUID::from_u128(0x6f0e2f0c_5a59_4b1e_9a3f_7f4d2c9be713);
    attach(texture, &KEY, piece, give);
}

/// (El1xr) Tells `crate::commitline` that a committed upload buffer of
/// `bytes` is gone when the last reference to `resource` goes.
pub(crate) fn on_release(resource: &ID3D12Resource, bytes: u64) {
    const KEY: GUID = GUID::from_u128(0x0c7e4a51_93d2_4f6b_8e1a_2b5d9c3f6a47);
    attach(resource, &KEY, Piece { block: 0, start: 0, len: bytes }, |piece| crate::commitline::upload_gone(piece.len));
}

/// A [`Reclaim`] of `piece` under `key` of `texture`: `give` gets the piece
/// when the resource's last reference goes.
fn attach(texture: &ID3D12Resource, key: &GUID, piece: Piece, give: fn(Piece)) {
    let made = Box::into_raw(Box::new(Reclaim {
        methods: &RECLAIM.0,
        references: AtomicU32::new(1),
        piece,
        give,
    }));
    // SAFETY: a COM object of ours with one reference, which this takes and
    // drops at the end; the texture takes its own.
    unsafe {
        let object = IUnknown::from_raw(made.cast());
        if texture.SetPrivateDataInterface(key, &object).is_err() {
            // Never given back: the piece stays taken, nothing is placed
            // under a texture that may be alive.
            std::mem::forget(object);
        }
    }
}

/// A block of texture memory, resident and mapped by the card when it is
/// returned ([`wait_resident`]).
fn texture_block(device: &ID3D12Device) -> Option<ID3D12Heap> {
    if crate::alloc_backoff::refused() {
        return None;
    }
    let resident = wait_resident();
    let desc = D3D12_HEAP_DESC {
        SizeInBytes: BLOCK,
        Properties: D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_DEFAULT,
            ..Default::default()
        },
        Alignment: 0,
        // Not cleared: what lies in a block is only seen through textures
        // uploaded before they are read, as a piece used before.
        Flags: D3D12_HEAP_FLAG_ALLOW_ONLY_NON_RT_DS_TEXTURES
            | D3D12_HEAP_FLAG_CREATE_NOT_ZEROED
            | if resident {
                D3D12_HEAP_FLAG_CREATE_NOT_RESIDENT
            } else {
                D3D12_HEAP_FLAG_NONE
            },
    };
    let mut heap: Option<ID3D12Heap> = None;
    // SAFETY: plain creation; the out pointer is ours.
    if let Err(e) = unsafe { device.CreateHeap(&desc, &mut heap) } {
        crate::alloc_backoff::note(&e);
        tracing::warn!(error = %e, "no block of texture memory");
        return None;
    }
    let heap = heap?;
    let began = std::time::Instant::now();
    if resident && let Err(e) = made_resident(device, &heap) {
        tracing::warn!(error = %e, "a block of texture memory is not resident");
        return None;
    }
    time(Which::Texture, T_RESIDENT_US, us(began));
    STATS[2].fetch_add(1, Ordering::Relaxed);
    crate::host::name(&heap, || "host textures (a block)".into());
    Some(heap)
}

/// `heap` (made not resident) made resident, then waited for until the
/// card has it mapped ([`mapped`]).
fn made_resident(device: &ID3D12Device, heap: &ID3D12Heap) -> windows::core::Result<()> {
    let pageable: ID3D12Pageable = heap.cast()?;
    // SAFETY: a live heap of the device's.
    unsafe { device.MakeResident(&[Some(pageable)]) }?;
    mapped(device)
}

/// A queue of its own that new memory is waited for with ([`mapped`]).
struct Aside {
    queue: ID3D12CommandQueue,
    list: ID3D12CommandList,
    fence: ID3D12Fence,
    event: windows::Win32::Foundation::HANDLE,
    value: u64,
    _keep: (ID3D12CommandAllocator, [ID3D12Resource; 2]),
}
// SAFETY: D3D12 objects are free-threaded; the event is waited on by one
// thread at a time (the lock).
unsafe impl Send for Aside {}

/// New memory of the card is mapped into its address space when this
/// returns: a copy of a few bytes is sent to a queue of ours, which the
/// system starts only once the mappings asked for so far are done, and
/// waited for. (`MakeResident` came back before that: cool_fox's run n11
/// was lost as n08 was with it alone.)
fn mapped(device: &ID3D12Device) -> windows::core::Result<()> {
    use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;
    use windows::Win32::System::Threading::{CreateEventW, INFINITE, WaitForSingleObject};
    static ASIDE: Mutex<Option<Aside>> = Mutex::new(None);
    let mut aside = lock(&ASIDE);
    // SAFETY: plain object creation; the objects the list names are kept.
    unsafe {
        if aside.is_none() {
            let buffer = |kind: D3D12_HEAP_TYPE,
                          state: D3D12_RESOURCE_STATES|
             -> windows::core::Result<ID3D12Resource> {
                let mut made: Option<ID3D12Resource> = None;
                device.CreateCommittedResource(
                    &D3D12_HEAP_PROPERTIES {
                        Type: kind,
                        ..Default::default()
                    },
                    D3D12_HEAP_FLAG_NONE,
                    &D3D12_RESOURCE_DESC {
                        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                        Width: 256,
                        Height: 1,
                        DepthOrArraySize: 1,
                        MipLevels: 1,
                        SampleDesc: DXGI_SAMPLE_DESC {
                            Count: 1,
                            Quality: 0,
                        },
                        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                        ..Default::default()
                    },
                    state,
                    None,
                    &mut made,
                )?;
                made.ok_or_else(windows::core::Error::empty)
            };
            let from = buffer(D3D12_HEAP_TYPE_UPLOAD, D3D12_RESOURCE_STATE_GENERIC_READ)?;
            let to = buffer(D3D12_HEAP_TYPE_READBACK, D3D12_RESOURCE_STATE_COPY_DEST)?;
            let queue: ID3D12CommandQueue =
                device.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                    Type: D3D12_COMMAND_LIST_TYPE_COPY,
                    ..Default::default()
                })?;
            let allocator: ID3D12CommandAllocator =
                device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_COPY)?;
            let list: ID3D12GraphicsCommandList =
                device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_COPY, &allocator, None)?;
            list.CopyBufferRegion(&to, 0, &from, 0, 16);
            list.Close()?;
            *aside = Some(Aside {
                queue,
                list: list.cast()?,
                fence: device.CreateFence(0, D3D12_FENCE_FLAG_NONE)?,
                event: CreateEventW(None, false, false, None)?,
                value: 0,
                _keep: (allocator, [from, to]),
            });
        }
        let Some(a) = aside.as_mut() else {
            return Ok(());
        };
        a.queue.ExecuteCommandLists(&[Some(a.list.clone())]);
        a.value += 1;
        a.queue.Signal(&a.fence, a.value)?;
        if a.fence.GetCompletedValue() < a.value {
            a.fence.SetEventOnCompletion(a.value, a.event)?;
            WaitForSingleObject(a.event, INFINITE);
        }
    }
    Ok(())
}

/// A host texture of `desc` (no target, depth or unordered access, one
/// sample, no clear value) in a piece of a block, in `state`
/// ([`texture_heaps`]). `None`: switched off, larger than a block, or the
/// host refused; the caller makes an allocation of its own then.
///
/// A piece used before is not cleared, where a committed texture starts as
/// zeros: only for textures that are uploaded before they are read.
///
/// # Safety
/// `desc` points at a valid description (it may be unaligned).
pub(crate) unsafe fn texture(
    device: &ID3D12Device,
    desc: *const D3D12_RESOURCE_DESC,
    state: D3D12_RESOURCE_STATES,
) -> Option<ID3D12Resource> {
    if !texture_heaps() {
        return None;
    }
    let started = std::time::Instant::now();
    let at = now();
    refresh_spares(at);
    // Small textures lie at multiples of 4 KiB where the host allows it
    // (the largest level 64 KiB at most), 64 KiB each otherwise.
    // SAFETY: the caller's description.
    let mut d = unsafe { desc.read_unaligned() };
    d.Alignment = u64::from(D3D12_SMALL_RESOURCE_PLACEMENT_ALIGNMENT);
    // SAFETY: plain queries.
    let mut info = unsafe { device.GetResourceAllocationInfo(0, &[d]) };
    if info.SizeInBytes == u64::MAX || info.Alignment != d.Alignment {
        d.Alignment = 0;
        // SAFETY: as above.
        info = unsafe { device.GetResourceAllocationInfo(0, &[d]) };
    }
    if info.SizeInBytes == u64::MAX || info.SizeInBytes == 0 {
        STATS[6].fetch_add(1, Ordering::Relaxed);
        return None;
    }
    let taken = take_from(
        &TEXTURES,
        Which::Texture,
        info.SizeInBytes,
        info.Alignment.max(1),
        Ripe::now(),
        || texture_block(device),
    );
    let Some((piece, heap, _)) = taken else {
        STATS[6].fetch_add(1, Ordering::Relaxed);
        return None;
    };
    let mut made: Option<ID3D12Resource> = None;
    // SAFETY: the piece is no one else's; the heap lives while a texture
    // placed in it does.
    let result =
        unsafe { device.CreatePlacedResource(&heap, piece.start, &d, state, None, &mut made) };
    match (result, made) {
        (Ok(()), Some(made)) => {
            HELD.fetch_add(piece.len, Ordering::Relaxed);
            give_back_with(&made, piece, give_texture);
            let us = started.elapsed().as_micros() as u64;
            let n = STATS[0].fetch_add(1, Ordering::Relaxed) + 1;
            STATS[1].fetch_add(us, Ordering::Relaxed);
            if n.is_power_of_two() || n.is_multiple_of(8192) {
                log_stats(n);
            }
            Some(made)
        }
        (result, _) => {
            if let Err(e) = result {
                tracing::warn!(error = %e, ?d, "no host texture in a block");
            }
            lock(&TEXTURES).give(piece, now());
            STATS[6].fetch_add(1, Ordering::Relaxed);
            None
        }
    }
}

/// The pool's line, for grepping a run's log.
fn log_stats(placed: u64) {
    let s = STATS.each_ref().map(|s| s.load(Ordering::Relaxed));
    tracing::info!(
        "texture heaps: {placed} textures placed, {:.1} us each, {} texture blocks made, {} alive, {} MB free in them, {} cut from unripe blocks, {} not placed; staging: {} pieces, {} blocks made, {} alive",
        s[1] as f64 / placed.max(1) as f64,
        s[2],
        TEXTURE_BLOCKS.load(Ordering::Relaxed),
        free_texture_bytes() >> 20,
        s[3],
        s[6],
        s[4],
        s[5],
        STAGING_BLOCKS.load(Ordering::Relaxed),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    static ALIVE: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn ranges_are_cut_aligned_and_join_again() {
        let mut r = Ranges::new(1024);
        assert_eq!(r.take(100, 64, 0), Some(0));
        assert_eq!(r.take(100, 64, 0), Some(128));
        assert_eq!(r.take(28, 4, 0), Some(100));
        assert_eq!(r.take(2000, 4, 0), None);
        r.give(128, 100);
        r.give(0, 100);
        assert!(!r.whole(1024));
        r.give(100, 28);
        assert!(r.whole(1024));
    }

    /// Blocks are made outside the pool's lock, one at a time: a thread
    /// that needs one while another makes it is told to go without; a
    /// refusal holds the next block back; blocks are made ahead.
    #[test]
    fn blocks_are_made_outside_the_lock_one_at_a_time() {
        static ALIVE_HERE: AtomicU64 = AtomicU64::new(0);
        static POOL: Mutex<Pool<u64>> = Mutex::new(Pool::new(4, &ALIVE_HERE));
        let made = AtomicU64::new(0);
        let make = || {
            // The pool is not locked while a block is made.
            assert!(POOL.try_lock().is_ok());
            Some(made.fetch_add(1, Ordering::Relaxed) + 1)
        };
        let (p, b, again) = take_from(&POOL, Which::Upload, 65536, 65536, Ripe::ALL, make).unwrap();
        assert_eq!((p.start, b, again), (0, 1, false));
        assert_eq!(take_from(&POOL, Which::Upload, BLOCK / 2, 65536, Ripe::ALL, make).unwrap().1, 1);
        // Another thread is making a block: no piece, nothing made.
        lock(&POOL).making = true;
        assert!(take_from(&POOL, Which::Upload, BLOCK / 2, 65536, Ripe::ALL, make).is_none());
        assert_eq!(made.load(Ordering::Relaxed), 1);
        lock(&POOL).making = false;
        // Room in the block there is: no block made.
        assert_eq!(take_from(&POOL, Which::Upload, 65536, 65536, Ripe::ALL, make).unwrap().1, 1);
        assert_eq!(made.load(Ordering::Relaxed), 1);
        // Made ahead: two empty blocks kept.
        let ahead = Ripe { started: 0, finished: 0, ahead: 2 };
        take_from(&POOL, Which::Upload, 65536, 65536, ahead, make).unwrap();
        take_from(&POOL, Which::Upload, 65536, 65536, ahead, make).unwrap();
        assert_eq!(made.load(Ordering::Relaxed), 3);
        assert!(!lock(&POOL).making);
        // Refused: none, and the next is not asked for at once.
        static ALIVE_REFUSED: AtomicU64 = AtomicU64::new(0);
        static REFUSING: Mutex<Pool<u64>> = Mutex::new(Pool::new(4, &ALIVE_REFUSED));
        let asked = AtomicU64::new(0);
        let refuse = || {
            asked.fetch_add(1, Ordering::Relaxed);
            None
        };
        assert!(take_from(&REFUSING, Which::Upload, 65536, 65536, Ripe::ALL, refuse).is_none());
        assert!(take_from(&REFUSING, Which::Upload, 65536, 65536, Ripe::ALL, refuse).is_none());
        assert_eq!(asked.load(Ordering::Relaxed), 1);
        assert!(!lock(&REFUSING).making);
    }

    #[test]
    fn empty_blocks_stay_while_the_card_has_room() {
        const GB: u64 = 1 << 30;
        assert_eq!(spares(Some([100 * GB, 84 * GB, 0, 0])), SPARE_KEPT);
        assert_eq!(spares(Some([100 * GB, 85 * GB, 0, 0])), SPARE);
        assert_eq!(spares(None), SPARE);
    }

    #[test]
    fn a_refused_block_is_not_asked_for_again_at_once() {
        let mut pool: Pool<u32> = Pool::new(2, &ALIVE);
        let calls = std::cell::Cell::new(0);
        let refuse = || {
            calls.set(calls.get() + 1);
            None
        };
        for size in [704008, 179720, 24080, 524288] {
            assert!(pool.take(size, 512, 12, refuse).is_none());
        }
        assert_eq!(calls.get(), 1);
        assert!(
            pool.take(24080, 512, 12 + crate::alloc_backoff::RETRY_MS, || Some(1))
                .is_some()
        );
    }

    #[test]
    fn pieces_never_overlap_and_rest_before_they_are_used_again() {
        let mut pool: Pool<u32> = Pool::new(SPARE, &ALIVE);
        let made = std::cell::Cell::new(0);
        let make = || {
            made.set(made.get() + 1);
            Some(made.get())
        };
        let mut taken: Vec<Piece> = Vec::new();
        for i in 0..500u64 {
            let (p, _, again) = pool.take(65536 * (1 + i % 7), 65536, 0, make).unwrap();
            assert!(!again);
            assert!(taken.iter().all(|q| q.block != p.block
                || p.start + p.len <= q.start
                || q.start + q.len <= p.start));
            taken.push(p);
        }
        let blocks = pool.blocks();
        assert!(blocks >= 2);
        // Given back at 10 (no submission runs in a test: the timer
        // decides): not cut again before 10 + REST.
        let first = taken[0];
        pool.give(first, 10);
        let (p, _, _) = pool.take(first.len, 65536, 10 + REST - 1, make).unwrap();
        assert_ne!((p.block, p.start), (first.block, first.start));
        let (q, _, again) = pool.take(first.len, 65536, 10 + REST, make).unwrap();
        assert_eq!((q.block, q.start, again), (first.block, first.start, true));
        // Everything back: the empty blocks past the spare ones go.
        for p in taken.drain(1..).chain([p, q]) {
            pool.give(p, 20);
        }
        pool.reclaim(20 + REST, 0, SPARE);
        assert_eq!(pool.blocks(), blocks.min(SPARE));
        // Too large for a block, and a block that cannot be made.
        assert!(pool.take(BLOCK, 65536, 30 + REST, make).is_none());
        let mut none: Pool<u32> = Pool::new(SPARE, &ALIVE);
        assert!(none.take(65536, 65536, 0, || None).is_none());
    }

    /// (El1xr) `VICTORY_UPLOAD_CAP_MB`: a trim lets go of every empty block,
    /// also those kept as spares and those emptied before, but not of a
    /// block with a piece in use or still resting.
    #[test]
    fn a_trim_lets_go_of_every_empty_block_and_only_those() {
        static ALIVE_HERE: AtomicU64 = AtomicU64::new(0);
        let mut pool: Pool<u32> = Pool::new(SPARE_KEPT, &ALIVE_HERE);
        let made = std::cell::Cell::new(0);
        let make = || {
            made.set(made.get() + 1);
            Some(made.get())
        };
        let half = BLOCK / 2;
        let taken: Vec<Piece> = (0..8).map(|_| pool.take(half, 65536, 0, make).unwrap().0).collect();
        assert_eq!(pool.blocks(), 4);
        // Three blocks emptied and kept as spares; a fourth keeps a piece.
        for p in &taken[..6] {
            pool.give(*p, 10);
        }
        assert!(pool.reclaim(10 + REST, 0, SPARE_KEPT).is_empty());
        assert_eq!(pool.blocks(), 4);
        // Of the last block one piece rests, one is in use.
        pool.give(taken[6], 10 + REST);
        let gone = pool.trim(10 + REST, 0);
        assert_eq!(gone.len(), 3);
        assert_eq!(pool.blocks(), 1);
        assert_eq!(ALIVE_HERE.load(Ordering::Relaxed), 1);
        // Once the last piece is back and rested, its block goes too.
        pool.give(taken[7], 20 + REST);
        assert_eq!(pool.trim(20 + 2 * REST, 0).len(), 1);
        assert_eq!(pool.blocks(), 0);
    }

    /// (El1xr) A fresh pool's usage counts what was cut once and given back as neither held
    /// nor cuttable: the memory the card buffer blocks hold for nothing; a plain pool cuts it
    /// again.
    #[test]
    fn usage_tells_held_cuttable_and_lost() {
        static ALIVE_HERE: AtomicU64 = AtomicU64::new(0);
        let make = || Some(0u32);
        const K64: u64 = 64 << 10;
        for fresh in [true, false] {
            let mut pool: Pool<u32> = if fresh { Pool::fresh(2, &ALIVE_HERE) } else { Pool::new(2, &ALIVE_HERE) };
            let a = pool.take(4 * K64, K64, 0, make).unwrap().0;
            let _b = pool.take(K64, K64, 0, make).unwrap().0;
            assert_eq!(pool.usage(), [5 * K64, BLOCK - 5 * K64]);
            pool.give(a, 0);
            // Resting: neither held nor cuttable.
            assert_eq!(pool.usage(), [K64, BLOCK - 5 * K64]);
            pool.reclaim(REST, 0, 2);
            let lost = if fresh { 4 * K64 } else { 0 };
            assert_eq!(pool.usage(), [K64, BLOCK - K64 - lost]);
        }
    }

    #[test]
    fn a_piece_rests_until_the_submissions_begun_before_have_finished() {
        let given = Resting { at: 1000, begun: 7 };
        assert!(!given.over(1000 + SETTLE, 7 + LAG - 1));
        assert!(!given.over(1000 + SETTLE - 1, 7 + LAG));
        assert!(given.over(1000 + SETTLE, 7 + LAG));
        assert!(given.over(1000 + REST, 0));
    }

    #[test]
    fn pieces_come_from_blocks_older_than_the_lists_that_run() {
        let mut pool: Pool<u64> = Pool::new(SPARE_KEPT, &ALIVE);
        let made = std::cell::Cell::new(0);
        let make = || {
            made.set(made.get() + 1);
            Some(made.get())
        };
        let at = |started, finished| Ripe {
            started,
            finished,
            ahead: 2,
        };
        let take =
            |pool: &mut Pool<u64>, ripe| pool.take_ripe(BLOCK / 2, 65536, 0, ripe, make).unwrap().1;
        // Nothing runs: at once, and two are made ahead, one a time.
        assert_eq!(take(&mut pool, at(0, 0)), 1);
        assert_eq!(made.get(), 2);
        assert_eq!(take(&mut pool, at(0, 0)), 1);
        assert_eq!(made.get(), 3);
        // 4 is made while submission 5 runs.
        assert_eq!(take(&mut pool, at(5, 4)), 2);
        assert_eq!(made.get(), 4);
        assert_eq!(take(&mut pool, at(5, 4)), 2);
        assert_eq!(take(&mut pool, at(6, 4)), 3);
        assert_eq!(take(&mut pool, at(6, 4)), 3);
        // None is ripe: the oldest; it is ripe once 5 has finished.
        assert_eq!(take(&mut pool, at(7, 4)), 4);
        assert_eq!(take(&mut pool, at(7, 5)), 4);
        // A whole block's worth too.
        assert_eq!(
            pool.take_ripe(BLOCK, 65536, 0, at(8, 7), make).unwrap().1,
            5
        );
        // Off: the first with room, whatever its age, none ahead.
        let n = made.get();
        assert_eq!(pool.take(BLOCK / 2, 65536, 0, make).unwrap().1, 6);
        assert_eq!(made.get(), n);
        assert!(pool.take(BLOCK, 65536, 0, make).is_none());
    }

    /// A texture's piece comes back when its last reference goes, not
    /// before; a staging piece used before is zeros again.
    #[test]
    fn a_texture_gives_its_piece_back_when_it_goes() {
        let Some(host) = crate::host::host() else {
            return;
        };
        if !texture_heaps() || !staging_blocks() {
            return; // (run with VICTORY_TEXTURE_HEAPS=1 VICTORY_STAGING_BLOCKS=1)
        }
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Width: 256,
            Height: 256,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R8G8B8A8_UNORM,
            SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            ..Default::default()
        };
        // SAFETY: a valid description.
        let t = unsafe { texture(&host.device, &desc, D3D12_RESOURCE_STATE_COMMON) }
            .expect("a texture in a block");
        assert_eq!(
            crate::host::references(&t),
            1,
            "what the texture holds is no reference to it"
        );
        let resting = || lock(&TEXTURES).resting.len();
        let before = resting();
        let other = t.clone();
        drop(t);
        assert_eq!(resting(), before);
        drop(other);
        assert_eq!(resting(), before + 1);
        // SAFETY: the piece's mapping, 4096 bytes of it ours.
        unsafe {
            let (piece, _, at, data) = staging(host, 4096).unwrap();
            assert!(at.is_multiple_of(512));
            std::ptr::write_bytes(data, 0xAB, 4096);
            drop(piece);
            lock(&STAGING).reclaim(u64::MAX / 2, 0, SPARE);
            let (_piece, _, _, data) = staging(host, 4096).unwrap();
            assert!((0..4096).all(|i| *data.add(i) == 0));
            assert!(host.device.GetDeviceRemovedReason().is_ok());
        }
    }
}

/// How long this PC's card takes to make the allocations a staged texture
/// needs: committed, committed without clearing, and placed in 64 MiB heaps.
/// `cargo test -p victory-gpu allocation_costs -- --ignored --nocapture`.
///
/// Source: DoubleGhosty/cool_fox `texture.rs` `allocation_costs` (42b95b9),
/// GPL-3.0-or-later.
#[cfg(test)]
mod allocation_costs {
    use windows::Win32::Graphics::Direct3D12::*;
    use windows::Win32::Graphics::Dxgi::Common::*;

    fn texture(size: u32) -> D3D12_RESOURCE_DESC {
        D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Width: u64::from(size),
            Height: size,
            DepthOrArraySize: 1,
            MipLevels: 0,
            Format: DXGI_FORMAT_BC7_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            ..Default::default()
        }
    }

    fn buffer(size: u64) -> D3D12_RESOURCE_DESC {
        D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: size,
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            ..Default::default()
        }
    }

    fn committed(
        device: &ID3D12Device,
        kind: D3D12_HEAP_TYPE,
        flags: D3D12_HEAP_FLAGS,
        desc: &D3D12_RESOURCE_DESC,
    ) -> ID3D12Resource {
        let heap = D3D12_HEAP_PROPERTIES {
            Type: kind,
            ..Default::default()
        };
        let state = if kind == D3D12_HEAP_TYPE_UPLOAD {
            D3D12_RESOURCE_STATE_GENERIC_READ
        } else {
            D3D12_RESOURCE_STATE_COMMON
        };
        let mut out = None;
        // SAFETY: plain creation.
        unsafe { device.CreateCommittedResource(&heap, flags, desc, state, None, &mut out) }
            .expect("committed");
        out.unwrap()
    }

    fn timed(
        what: &str,
        n: usize,
        threads: usize,
        make: &(dyn Fn(usize) -> ID3D12Resource + Sync),
    ) {
        let began = std::time::Instant::now();
        let all: Vec<_> = std::thread::scope(|s| {
            (0..threads)
                .map(|t| s.spawn(move || (t..n).step_by(threads).map(make).collect::<Vec<_>>()))
                .collect::<Vec<_>>()
                .into_iter()
                .flat_map(|h| h.join().unwrap())
                .collect()
        });
        let took = began.elapsed().as_secs_f64() * 1e3;
        println!(
            "{what}, {threads} thread(s): {n} in {took:.1} ms, {:.0} us each",
            took * 1e3 / n as f64
        );
        drop(all);
    }

    #[test]
    #[ignore = "a measurement, not a check"]
    fn allocation_costs() {
        let Some(host) = crate::host::host() else {
            return;
        };
        let device = &host.device;
        let not_zeroed = D3D12_HEAP_FLAG_CREATE_NOT_ZEROED;
        const N: usize = 1000;
        for size in [128u32, 512, 1024] {
            let (t, b) = (
                texture(size),
                buffer(u64::from(size * size) * 4 / 3 + 65536),
            );
            println!("-- BC7 {size}x{size}");
            for threads in [1, 4, 8] {
                timed("texture committed", N, threads, &|_| {
                    committed(device, D3D12_HEAP_TYPE_DEFAULT, D3D12_HEAP_FLAG_NONE, &t)
                });
                timed("texture committed not zeroed", N, threads, &|_| {
                    committed(device, D3D12_HEAP_TYPE_DEFAULT, not_zeroed, &t)
                });
                let mapped = |flags| {
                    let b = &b;
                    move |_| {
                        let r = committed(device, D3D12_HEAP_TYPE_UPLOAD, flags, b);
                        let mut data = std::ptr::null_mut();
                        // SAFETY: a live upload buffer.
                        unsafe { r.Map(0, None, Some(&mut data)) }.unwrap();
                        // SAFETY: the first byte of the mapping (its first touch).
                        unsafe { data.cast::<u8>().write_volatile(1) };
                        r
                    }
                };
                timed(
                    "staging committed and mapped",
                    N,
                    threads,
                    &mapped(D3D12_HEAP_FLAG_NONE),
                );
                timed(
                    "staging committed and mapped, not zeroed",
                    N,
                    threads,
                    &mapped(not_zeroed),
                );
            }
            // SAFETY: plain query.
            let info = unsafe { device.GetResourceAllocationInfo(0, &[t]) };
            let per_heap = ((64u64 << 20) / info.SizeInBytes).max(1) as usize;
            let heaps: Vec<ID3D12Heap> = (0..N.div_ceil(per_heap))
                .map(|_| {
                    let desc = D3D12_HEAP_DESC {
                        SizeInBytes: per_heap as u64 * info.SizeInBytes,
                        Properties: D3D12_HEAP_PROPERTIES {
                            Type: D3D12_HEAP_TYPE_DEFAULT,
                            ..Default::default()
                        },
                        Flags: D3D12_HEAP_FLAG_ALLOW_ONLY_NON_RT_DS_TEXTURES | not_zeroed,
                        ..Default::default()
                    };
                    let mut heap = None;
                    // SAFETY: plain creation.
                    unsafe { device.CreateHeap(&desc, &mut heap) }.expect("heap");
                    heap.unwrap()
                })
                .collect();
            let place = |i: usize| {
                let mut out: Option<ID3D12Resource> = None;
                let at = (i % per_heap) as u64 * info.SizeInBytes;
                // SAFETY: a part of a live heap no other texture has.
                unsafe {
                    device.CreatePlacedResource(
                        &heaps[i / per_heap],
                        at,
                        &t,
                        D3D12_RESOURCE_STATE_COMMON,
                        None,
                        &mut out,
                    )
                }
                .expect("placed");
                out.unwrap()
            };
            timed("texture placed in 64 MB heaps", N, 1, &place);
            timed("texture placed in 64 MB heaps", N, 8, &place);
            // What the pool adds: a piece taken under its lock, and the
            // piece's give-back object set on the texture.
            // SAFETY: the description is valid.
            let pooled = |_| {
                unsafe { super::texture(device, &t, D3D12_RESOURCE_STATE_COMMON) }.expect("pooled")
            };
            if super::texture_heaps() {
                timed(
                    "texture from the pool (VICTORY_TEXTURE_HEAPS=1)",
                    N,
                    1,
                    &pooled,
                );
                timed(
                    "texture from the pool (VICTORY_TEXTURE_HEAPS=1)",
                    N,
                    8,
                    &pooled,
                );
            }
        }
    }

    /// The buffers the host makes on hot paths (`Host::uav_buffer`,
    /// `shared_buffer`, `upload_buffer`, `read_back_buffer`): committed, and
    /// placed in 64 MiB heaps (mapped where the CPU maps them).
    /// `cargo test -p victory-gpu buffer_allocation_costs -- --ignored --nocapture`.
    #[test]
    #[ignore = "a measurement, not a check"]
    fn buffer_allocation_costs() {
        let Some(host) = crate::host::host() else { return };
        let device = &host.device;
        let kinds: [(&str, D3D12_HEAP_PROPERTIES, D3D12_RESOURCE_FLAGS, D3D12_RESOURCE_STATES, bool); 4] = [
            (
                "card (uav_buffer)",
                D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() },
                D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
                D3D12_RESOURCE_STATE_COMMON,
                false,
            ),
            (
                "shared (shared_buffer)",
                D3D12_HEAP_PROPERTIES {
                    Type: D3D12_HEAP_TYPE_CUSTOM,
                    CPUPageProperty: D3D12_CPU_PAGE_PROPERTY_WRITE_BACK,
                    MemoryPoolPreference: D3D12_MEMORY_POOL_L0,
                    ..Default::default()
                },
                D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
                D3D12_RESOURCE_STATE_COMMON,
                true,
            ),
            (
                "upload (upload_buffer)",
                D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_UPLOAD, ..Default::default() },
                D3D12_RESOURCE_FLAG_NONE,
                D3D12_RESOURCE_STATE_GENERIC_READ,
                true,
            ),
            (
                "readback (read_back_buffer)",
                D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_READBACK, ..Default::default() },
                D3D12_RESOURCE_FLAG_NONE,
                D3D12_RESOURCE_STATE_COPY_DEST,
                false,
            ),
        ];
        let touch = |r: &ID3D12Resource, map: bool| {
            if map {
                let mut data = std::ptr::null_mut();
                // SAFETY: a live CPU-visible buffer; its first byte.
                unsafe {
                    r.Map(0, None, Some(&mut data)).unwrap();
                    data.cast::<u8>().write_volatile(1);
                }
            }
        };
        const N: usize = 1000;
        const K64: u64 = 64 << 10;
        for size in [4u64 << 10, 64 << 10, 1 << 20] {
            println!("-- buffers of {} KiB", size >> 10);
            for &(what, props, flags, state, map) in &kinds {
                let mut desc = buffer(size);
                desc.Flags = flags;
                let make = |_| {
                    let mut out: Option<ID3D12Resource> = None;
                    // SAFETY: plain creation.
                    unsafe { device.CreateCommittedResource(&props, D3D12_HEAP_FLAG_NONE, &desc, state, None, &mut out) }
                        .expect("committed");
                    let r = out.unwrap();
                    touch(&r, map);
                    r
                };
                for threads in [1, 8] {
                    timed(&format!("{what} committed"), N, threads, &make);
                }
                let len = size.next_multiple_of(K64);
                let per_heap = ((64u64 << 20) / len) as usize;
                let heaps: Vec<ID3D12Heap> = (0..N.div_ceil(per_heap))
                    .map(|_| {
                        let mut heap = None;
                        let desc = D3D12_HEAP_DESC {
                            SizeInBytes: 64 << 20,
                            Properties: props,
                            Flags: D3D12_HEAP_FLAG_ALLOW_ONLY_BUFFERS,
                            ..Default::default()
                        };
                        // SAFETY: plain creation.
                        unsafe { device.CreateHeap(&desc, &mut heap) }.expect("heap");
                        heap.unwrap()
                    })
                    .collect();
                let place = |i: usize| {
                    let mut out: Option<ID3D12Resource> = None;
                    let at = (i % per_heap) as u64 * len;
                    // SAFETY: a part of a live heap no other buffer has.
                    unsafe { device.CreatePlacedResource(&heaps[i / per_heap], at, &desc, state, None, &mut out) }
                        .expect("placed");
                    let r = out.unwrap();
                    touch(&r, map);
                    r
                };
                for threads in [1, 8] {
                    timed(&format!("{what} placed in 64 MB heaps"), N, threads, &place);
                }
            }
            let pools: [(&str, &(dyn Fn(usize) -> ID3D12Resource + Sync)); 3] = [
                ("card from the pool (VICTORY_CARD_BUFFER_BLOCKS=1)", &|_| super::card_buffer(device, size).expect("pooled")),
                ("shared from the pool (VICTORY_SHARED_BUFFER_BLOCKS=1)", &|_| super::shared_buffer(device, size).expect("pooled").0),
                ("upload from the pool (VICTORY_UPLOAD_BLOCKS=1)", &|_| super::upload_buffer(device, size).expect("pooled").0),
            ];
            for (i, (what, make)) in pools.into_iter().enumerate() {
                let on = ["VICTORY_CARD_BUFFER_BLOCKS", "VICTORY_SHARED_BUFFER_BLOCKS", "VICTORY_UPLOAD_BLOCKS"][i];
                if super::switch(on) {
                    for threads in [1, 8] {
                        timed(what, N, threads, make);
                    }
                }
            }
        }
    }
}
