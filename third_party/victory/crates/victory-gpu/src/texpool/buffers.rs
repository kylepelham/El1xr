//! Host buffers cut from blocks of 64 MiB instead of a committed allocation
//! each, by kind, each behind its own switch (all off by default):
//!
//! - `VICTORY_CARD_BUFFER_BLOCKS=1`: `Host::uav_buffer`, buffers in the
//!   card's memory: the card sides of title buffers (`crate::shadow`
//!   `card_for`, about 5,200 made in the first pass over the heap's buffers
//!   at world entry) and of windows, and the others asked for there.
//! - `VICTORY_SHARED_BUFFER_BLOCKS=1`: `Host::shared_buffer`, buffers in the
//!   PC's memory the CPU maps and shaders read and write: the CPU sides of
//!   the same copies (about 4,650 in that pass) and views of title memory.
//! - `VICTORY_UPLOAD_BLOCKS=1`: `Host::upload_buffer`: about 3,100 a run
//!   besides texture staging, and the staging buffers too while
//!   `VICTORY_STAGING_BLOCKS` (which cuts them from blocks of its own) is
//!   off (about 42,000 a run, most of them kept by `VICTORY_STAGE_POOL_MB`).
//!
//! Each piece is a placed buffer of its own, as the committed one was: the
//! callers see the same description, state and address range, mapped the
//! same way. A committed buffer starts as zeros ("almost always": a pass
//! that reads before it writes zeros its buffer itself, `crate::zero_fill`),
//! so a piece does too: a
//! piece of the card's memory is cut only from memory no piece used before
//! (heaps are made cleared; a block is let go once all its pieces are back),
//! or (El1xr, `VICTORY_CARD_BLOCK_REUSE=1`) zeroed since on a copy queue
//! (`super::reuse`),
//! and a piece of mapped memory used before is cleared through its mapping.
//! Pieces come back with the buffer's last reference and rest as texture
//! pieces do; blocks are made ahead and made resident before use as texture
//! blocks are (`VICTORY_RIPE_BLOCKS`, `VICTORY_WAIT_RESIDENT`).
//!
//! Measured with `allocation_costs` (`super`): see `ready.md` of job
//! `texpool`.
//!
//! Source: the pool and its guards as in `super` (uiliannn/VItoriaBr
//! 5668b8bbdc; DoubleGhosty/cool_fox 42b95b9, 0322b39abd, f1d3eb2b64,
//! 3b73cb0eec), here for buffers. GPL-3.0-or-later.

use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;

use super::{
    BLOCK, Piece, Pool, Ripe, give_back_with, lock, made_resident, now, reuse, switch, wait_resident,
};

/// The kinds of buffer, as `crate::host` makes them.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Kind {
    Card,
    Shared,
    Upload,
}

/// Empty blocks kept, and so made ahead at most, per kind.
const KEPT: usize = 4;

static ALIVE: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];
static CARD: Mutex<Pool<Mem>> = Mutex::new(Pool::fresh(KEPT, &ALIVE[0]));
static SHARED: Mutex<Pool<Mem>> = Mutex::new(Pool::new(KEPT, &ALIVE[1]).capped());
static UPLOAD: Mutex<Pool<Mem>> = Mutex::new(Pool::new(KEPT, &ALIVE[2]).capped());

/// (El1xr) Blocks alive: card, shared, upload.
pub(super) fn blocks_alive() -> [u64; 3] {
    ALIVE.each_ref().map(|a| a.load(Ordering::Relaxed))
}

/// (El1xr) Card, shared, upload: bytes held by pieces in use and bytes still cuttable.
pub(super) fn usage() -> [[u64; 2]; 3] {
    [&CARD, &SHARED, &UPLOAD].map(|pool| lock(pool).usage())
}

/// (El1xr) Empty blocks: card, shared, upload.
pub(super) fn empties() -> [u64; 3] {
    [&CARD, &SHARED, &UPLOAD].map(|pool| lock(pool).empties())
}

/// (El1xr) `VICTORY_POOL_SLACK_MB`: every kind's rested pieces freed and its empty
/// blocks past `keep` let go: blocks let go per kind.
pub(super) fn trim_slack(at: u64, finished: u64, keep: usize) -> [u64; 3] {
    [Kind::Card, Kind::Shared, Kind::Upload].map(|kind| {
        let freed = lock(kind.pool()).slack_trim(at, finished, keep);
        let n = freed.len() as u64;
        super::let_go(kind.which(), freed);
        n
    })
}

/// (El1xr) The shared and upload pools' rested pieces freed and every empty
/// block let go (`VICTORY_UPLOAD_CAP_MB`): the blocks let go.
pub(super) fn trim(at: u64, finished: u64) -> u64 {
    let mut n = 0;
    for kind in [Kind::Shared, Kind::Upload] {
        let freed = lock(kind.pool()).trim(at, finished);
        n += freed.len() as u64;
        super::let_go(kind.which(), freed);
    }
    n
}

/// A block: its heap, and for the kinds the CPU maps a buffer over all of
/// it, mapped once, that the pieces' mappings are cut from (a `Map` of each
/// piece of upload memory cost about as much as the allocation it spared:
/// 523 us a MiB on the 5090).
///
/// (El1xr) A card block has the buffer over all of it too, not mapped, while
/// its pieces are zeroed and cut again (`VICTORY_CARD_BLOCK_REUSE`): the copy
/// queue writes the zeros through it.
#[derive(Clone)]
struct Mem {
    heap: ID3D12Heap,
    whole: Option<ID3D12Resource>,
    data: *mut u8,
}
// SAFETY: D3D12 objects are free-threaded; the mapping is written by the
// owners of its pieces, each its own.
unsafe impl Send for Mem {}

/// Per kind since the start: buffers placed, their microseconds, blocks
/// made, buffers the blocks did not take.
static STATS: [[AtomicU64; 4]; 3] = [const { [const { AtomicU64::new(0) }; 4] }; 3];

impl Kind {
    fn at(self) -> usize {
        self as usize
    }

    fn name(self) -> &'static str {
        ["card", "shared", "upload"][self.at()]
    }

    fn on(self) -> bool {
        static ON: [OnceLock<bool>; 3] = [const { OnceLock::new() }; 3];
        *ON[self.at()].get_or_init(|| {
            let name = [
                "VICTORY_CARD_BUFFER_BLOCKS",
                "VICTORY_SHARED_BUFFER_BLOCKS",
                "VICTORY_UPLOAD_BLOCKS",
            ][self.at()];
            let on = switch(name);
            if on {
                tracing::info!(
                    ahead = super::ahead().min(KEPT),
                    resident = wait_resident(),
                    "host {} buffers are pieces of 64 MiB blocks ({name}=1)",
                    self.name()
                );
            }
            on
        })
    }

    fn which(self) -> super::Which {
        [super::Which::Card, super::Which::Shared, super::Which::Upload][self.at()]
    }

    fn pool(self) -> &'static Mutex<Pool<Mem>> {
        [&CARD, &SHARED, &UPLOAD][self.at()]
    }

    fn heap(self) -> D3D12_HEAP_PROPERTIES {
        match self {
            Kind::Card => D3D12_HEAP_PROPERTIES {
                Type: D3D12_HEAP_TYPE_DEFAULT,
                ..Default::default()
            },
            Kind::Shared => D3D12_HEAP_PROPERTIES {
                Type: D3D12_HEAP_TYPE_CUSTOM,
                CPUPageProperty: D3D12_CPU_PAGE_PROPERTY_WRITE_BACK,
                MemoryPoolPreference: D3D12_MEMORY_POOL_L0,
                ..Default::default()
            },
            Kind::Upload => D3D12_HEAP_PROPERTIES {
                Type: D3D12_HEAP_TYPE_UPLOAD,
                ..Default::default()
            },
        }
    }

    /// The flags and state `crate::host` makes this kind with.
    fn made_as(self) -> (D3D12_RESOURCE_FLAGS, D3D12_RESOURCE_STATES) {
        match self {
            Kind::Card | Kind::Shared => (
                D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
                D3D12_RESOURCE_STATE_COMMON,
            ),
            Kind::Upload => (D3D12_RESOURCE_FLAG_NONE, D3D12_RESOURCE_STATE_GENERIC_READ),
        }
    }

    fn give(self) -> fn(Piece) {
        [give_card, give_shared, give_upload][self.at()]
    }
}

fn give_card(piece: Piece) {
    lock(&CARD).give(piece, now());
}

fn give_shared(piece: Piece) {
    lock(&SHARED).give(piece, now());
}

fn give_upload(piece: Piece) {
    lock(&UPLOAD).give(piece, now());
}

/// A block of `kind`, cleared, resident and mapped by the card when it is
/// returned (`super::wait_resident`).
fn block(device: &ID3D12Device, kind: Kind) -> Option<Mem> {
    if crate::alloc_backoff::refused() {
        return None;
    }
    let resident = wait_resident();
    let desc = D3D12_HEAP_DESC {
        SizeInBytes: BLOCK,
        Properties: kind.heap(),
        Alignment: 0,
        Flags: D3D12_HEAP_FLAG_ALLOW_ONLY_BUFFERS
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
        tracing::warn!(error = %e, kind = kind.name(), "no block of buffer memory");
        return None;
    }
    let heap = heap?;
    let began = std::time::Instant::now();
    if resident && let Err(e) = made_resident(device, &heap) {
        tracing::warn!(error = %e, kind = kind.name(), "a block of buffer memory is not resident");
        return None;
    }
    super::time(kind.which(), super::T_RESIDENT_US, super::us(began));
    let (mut whole, mut data) = (None, std::ptr::null_mut());
    if kind != Kind::Card {
        let mut made: Option<ID3D12Resource> = None;
        // SAFETY: a buffer over the whole live heap, which the pieces
        // alias (buffers have no layout); mapped as the committed ones are.
        let mapped = unsafe {
            device
                .CreatePlacedResource(
                    &heap,
                    0,
                    &buffer_desc(BLOCK, D3D12_RESOURCE_FLAG_NONE),
                    kind.made_as().1,
                    None,
                    &mut made,
                )
                .and_then(|()| {
                    let b = made.as_ref().ok_or_else(windows::core::Error::empty)?;
                    b.Map(0, None, Some(&mut data))
                })
        };
        if let Err(e) = mapped {
            tracing::warn!(error = %e, kind = kind.name(), "a block of buffer memory is not mapped");
            return None;
        }
        whole = made;
    } else if reuse::on() {
        let mut made: Option<ID3D12Resource> = None;
        // SAFETY: a buffer over the whole live heap, which the pieces alias;
        // written only by the zeros over pieces no buffer holds (`reuse`).
        let placed = unsafe {
            device.CreatePlacedResource(
                &heap,
                0,
                &buffer_desc(BLOCK, D3D12_RESOURCE_FLAG_NONE),
                D3D12_RESOURCE_STATE_COMMON,
                None,
                &mut made,
            )
        };
        if let Err(e) = placed {
            tracing::warn!(error = %e, "a block of card buffer memory has no buffer over it to zero its pieces");
            return None;
        }
        whole = made;
    }
    STATS[kind.at()][2].fetch_add(1, Ordering::Relaxed);
    crate::host::name(&heap, || format!("host {} buffers (a block)", kind.name()));
    Some(Mem {
        heap,
        whole,
        data: data.cast(),
    })
}

/// (El1xr) A round of card block reuse at most this often (ms).
const REUSE_ROUND_MS: u64 = 10;

/// (El1xr) `VICTORY_CARD_BLOCK_REUSE=1`: the zeroer is made with the first
/// card piece asked for, before any card block, and the card pool reuses
/// from then on; a round of it at most every [`REUSE_ROUND_MS`], by one
/// thread at a time (`reuse::round`).
fn card_reuse(device: &ID3D12Device) {
    static ZEROER: OnceLock<Option<Mutex<reuse::Zeroer>>> = OnceLock::new();
    static LAST: AtomicU64 = AtomicU64::new(0);
    if !reuse::on() {
        return;
    }
    let zeroer = ZEROER.get_or_init(|| match reuse::Zeroer::new(device) {
        Ok(zeroer) => {
            lock(&CARD).reuse = true;
            tracing::info!("card buffer blocks: pieces back are zeroed on a copy queue and cut again (VICTORY_CARD_BLOCK_REUSE=1)");
            Some(Mutex::new(zeroer))
        }
        Err(e) => {
            tracing::warn!(error = %e, "card block reuse: no copy queue to zero pieces; card blocks stay fresh-only");
            None
        }
    });
    let Some(zeroer) = zeroer else {
        return;
    };
    let at = now();
    let last = LAST.load(Ordering::Relaxed);
    if at < last + REUSE_ROUND_MS || LAST.compare_exchange(last, at, Ordering::Relaxed, Ordering::Relaxed).is_err() {
        return;
    }
    let Ok(mut zeroer) = zeroer.try_lock() else {
        return;
    };
    let gone = reuse::round(&CARD, &mut zeroer, |mem: &Mem| mem.whole.clone());
    drop(zeroer);
    super::let_go(super::Which::Card, gone);
}

pub(crate) fn buffer_desc(size: u64, flags: D3D12_RESOURCE_FLAGS) -> D3D12_RESOURCE_DESC {
    D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: size.max(1),
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        Flags: flags,
        ..Default::default()
    }
}

/// A buffer of `size` bytes of `kind` placed in a piece of a block, all
/// zeros, with its mapping for the kinds the CPU maps (cut from the
/// block's: the buffer itself is not mapped). `None`: switched off, larger
/// than half a block, or the host refused; the caller makes a committed one.
fn placed(device: &ID3D12Device, kind: Kind, size: u64) -> Option<(ID3D12Resource, *mut u8)> {
    if !kind.on() {
        return None;
    }
    let len = size
        .max(1)
        .next_multiple_of(u64::from(D3D12_DEFAULT_RESOURCE_PLACEMENT_ALIGNMENT));
    if len > BLOCK / 2 {
        STATS[kind.at()][3].fetch_add(1, Ordering::Relaxed);
        return None;
    }
    let started = std::time::Instant::now();
    if kind == Kind::Card {
        card_reuse(device);
    }
    let taken = super::take_from(
        kind.pool(),
        kind.which(),
        len,
        u64::from(D3D12_DEFAULT_RESOURCE_PLACEMENT_ALIGNMENT),
        Ripe::ahead(if kind != Kind::Card && crate::commitline::upload_over_cap() {
            1
        } else {
            crate::poolslack::spares(KEPT)
        }),
        || block(device, kind),
    );
    let Some((piece, mem, again)) = taken else {
        STATS[kind.at()][3].fetch_add(1, Ordering::Relaxed);
        return None;
    };
    let (flags, state) = kind.made_as();
    let mut made: Option<ID3D12Resource> = None;
    // SAFETY: the piece is no one else's; the heap lives while a buffer
    // placed in it does.
    let result = unsafe {
        device.CreatePlacedResource(
            &mem.heap,
            piece.start,
            &buffer_desc(size, flags),
            state,
            None,
            &mut made,
        )
    };
    let buffer = match (result, made) {
        (Ok(()), Some(made)) => made,
        (result, _) => {
            if let Err(e) = result {
                tracing::warn!(error = %e, kind = kind.name(), size, "no host buffer in a block");
            }
            kind.give()(piece);
            STATS[kind.at()][3].fetch_add(1, Ordering::Relaxed);
            return None;
        }
    };
    give_back_with(&buffer, piece, kind.give());
    let mut data = std::ptr::null_mut::<u8>();
    if kind != Kind::Card {
        // SAFETY: the piece lies in the block's mapping.
        data = unsafe { mem.data.add(piece.start as usize) };
        if again {
            // As a committed buffer starts.
            let began = std::time::Instant::now();
            // SAFETY: the piece is ours alone.
            unsafe { std::ptr::write_bytes(data, 0, size.max(1) as usize) };
            super::time(kind.which(), super::T_ZERO_BYTES, size.max(1));
            super::time(kind.which(), super::T_ZERO_US, super::us(began));
        }
    } else {
        debug_assert!(!again || reuse::on(), "a card piece is cut only from fresh or zeroed memory");
    }
    let us = started.elapsed().as_micros() as u64;
    let n = STATS[kind.at()][0].fetch_add(1, Ordering::Relaxed) + 1;
    STATS[kind.at()][1].fetch_add(us, Ordering::Relaxed);
    if n.is_power_of_two() || n.is_multiple_of(4096) {
        log_stats(kind, n);
    }
    Some((buffer, data))
}

/// `Host::uav_buffer` in a block (`VICTORY_CARD_BUFFER_BLOCKS=1`).
pub(crate) fn card_buffer(device: &ID3D12Device, size: u64) -> Option<ID3D12Resource> {
    placed(device, Kind::Card, size).map(|(buffer, _)| buffer)
}

/// `Host::shared_buffer` in a block (`VICTORY_SHARED_BUFFER_BLOCKS=1`).
pub(crate) fn shared_buffer(device: &ID3D12Device, size: u64) -> Option<(ID3D12Resource, *mut u8)> {
    placed(device, Kind::Shared, size)
}

/// `Host::upload_buffer` in a block (`VICTORY_UPLOAD_BLOCKS=1`).
pub(crate) fn upload_buffer(device: &ID3D12Device, size: u64) -> Option<(ID3D12Resource, *mut u8)> {
    placed(device, Kind::Upload, size)
}

/// The kind's line, for grepping a run's log.
fn log_stats(kind: Kind, placed: u64) {
    let s = STATS[kind.at()]
        .each_ref()
        .map(|s| s.load(Ordering::Relaxed));
    tracing::info!(
        "buffer blocks ({}): {placed} buffers placed, {:.1} us each, {} blocks made, {} alive, {} not placed",
        kind.name(),
        s[1] as f64 / placed.max(1) as f64,
        s[2],
        ALIVE[kind.at()].load(Ordering::Relaxed),
        s[3],
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    static TEST_ALIVE: AtomicU64 = AtomicU64::new(0);

    /// A fresh pool never cuts memory used before, and lets a used block go
    /// once every piece of it is back.
    #[test]
    fn a_fresh_pool_cuts_only_unused_memory() {
        let mut pool: Pool<u32> = Pool::fresh(2, &TEST_ALIVE);
        let made = std::cell::Cell::new(0);
        let make = || {
            made.set(made.get() + 1);
            Some(made.get())
        };
        const K64: u64 = 64 << 10;
        let a = pool.take(K64, K64, 0, make).unwrap();
        let b = pool.take(K64, K64, 0, make).unwrap();
        assert_eq!((a.0.start, b.0.start), (0, K64));
        pool.give(a.0, 0);
        pool.reclaim(super::super::REST, 0, 2);
        // Back and rested, but not cut again: past it instead.
        let c = pool.take(K64, K64, super::super::REST, make).unwrap();
        assert_eq!((c.0.block, c.0.start, c.2), (a.0.block, 2 * K64, false));
        // Half a block left: a piece of more goes to a new block.
        let d = pool.take(BLOCK / 2, K64, super::super::REST, make).unwrap();
        assert_eq!((d.0.block, d.0.start), (a.0.block, 3 * K64));
        let e = pool.take(BLOCK / 2, K64, super::super::REST, make).unwrap();
        assert_ne!(e.0.block, a.0.block);
        assert_eq!(e.2, false);
        // Every piece of the first block back: it goes, used as it is.
        let blocks = pool.blocks();
        for p in [b.0, c.0, d.0] {
            pool.give(p, 10);
        }
        pool.reclaim(10 + super::super::REST, 0, 2);
        assert_eq!(pool.blocks(), blocks - 1);
    }

    /// With every switch on, each kind's buffer has its asked size, the
    /// state its committed one starts in, zeros where the CPU sees it, and
    /// its piece back when it goes; a piece of mapped memory used again is
    /// zeros again.
    #[test]
    fn placed_buffers_look_like_committed_ones() {
        let Some(host) = crate::host::host() else {
            return;
        };
        if !(Kind::Card.on() && Kind::Shared.on() && Kind::Upload.on()) {
            return; // (run with VICTORY_CARD_BUFFER_BLOCKS=1 VICTORY_SHARED_BUFFER_BLOCKS=1 VICTORY_UPLOAD_BLOCKS=1)
        }
        let device = &host.device;
        let card = card_buffer(device, 1000).expect("a card buffer");
        // SAFETY: plain queries.
        unsafe {
            assert_eq!(card.GetDesc().Width, 1000);
            assert_eq!(
                card.GetDesc().Flags,
                D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS
            );
        }
        let resting = |kind: Kind| lock(kind.pool()).resting.len();
        let before = resting(Kind::Card);
        drop(card);
        assert_eq!(resting(Kind::Card), before + 1);
        for kind in [Kind::Shared, Kind::Upload] {
            let (buffer, data) = placed(device, kind, 5000).expect("a mapped buffer");
            // SAFETY: the mapping of 5,000 bytes, ours.
            unsafe {
                assert!((0..5000).all(|i| *data.add(i) == 0));
                std::ptr::write_bytes(data, 0xCD, 5000);
            }
            drop(buffer);
            lock(kind.pool()).reclaim(u64::MAX / 2, 0, KEPT);
            let (_again, data) = placed(device, kind, 5000).expect("a mapped buffer");
            // SAFETY: as above.
            unsafe { assert!((0..5000).all(|i| *data.add(i) == 0)) };
        }
        // SAFETY: a plain query.
        assert!(unsafe { device.GetDeviceRemovedReason() }.is_ok());
    }
}
