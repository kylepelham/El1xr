//! (El1xr) `VICTORY_CARD_BLOCK_REUSE=1` (off by default; acts with
//! `VICTORY_CARD_BUFFER_BLOCKS=1`): pieces of card buffer blocks are cut
//! again.
//!
//! The card pool (`buffers`) was fresh: a committed buffer starts as zeros
//! and the card's memory cannot be cleared from the CPU, so a piece was cut
//! only from memory no piece had used, and a used block went only once all
//! of its pieces were back. One piece held kept its whole 64 MiB: in batch 5
//! the card blocks grew from 576 MB to 3.7-4.4 GB while the pieces held
//! stayed 2.6-2.7 GB, and the system charges all of it to commit.
//!
//! Here a piece back is zeroed once every submission begun before it came
//! back has finished (`Resting::done`, no time limit): copies from a buffer
//! of zeros over it, through a buffer placed over its whole block, on a copy
//! queue of ours. It is cut again only once that copy's fence value is seen
//! passed from the CPU (`Pool::cleared`), so a buffer placed in it starts as
//! zeros as a committed one does, and every list that names it is sent after
//! the zeros were written. One send is in flight at a time (one allocator);
//! a block every piece of which is back still goes unzeroed.
//!
//! The buffer of zeros is written with zeros once, from the CPU through an
//! upload buffer, before the first send: a committed buffer only "almost
//! always" starts as zeros (the memory manager may hand a process memory it
//! freed itself back without clearing it). WARP did under commit pressure,
//! and the zeroer then copied a freed buffer's old bytes over the pieces.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use windows::Win32::Graphics::Direct3D12::*;
use windows::core::Interface;

use super::{Piece, Pool, lock};

/// Bytes of the buffer of zeros: a piece takes one copy per this many.
const ZEROS: u64 = 4 << 20;
/// The most bytes of pieces zeroed in one send.
const SEND_MOST: u64 = 512 << 20;

/// Since the start: sends, pieces zeroed, their bytes, sends that failed.
static STATS: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

/// `VICTORY_CARD_BLOCK_REUSE=1`.
pub(super) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| super::switch("VICTORY_CARD_BLOCK_REUSE"))
}

/// A copy queue of ours with one list, and the buffer of zeros it copies.
pub(super) struct Zeroer {
    queue: ID3D12CommandQueue,
    allocator: ID3D12CommandAllocator,
    list: ID3D12GraphicsCommandList,
    fence: ID3D12Fence,
    zeros: ID3D12Resource,
    /// The buffer the zeros are first copied from, until that copy has run.
    first: Option<ID3D12Resource>,
    /// The fence value of the last send (1: the first copy of zeros).
    sent: u64,
}
// SAFETY: D3D12 objects are free-threaded; the list is recorded by the
// holder of the zeroer's lock alone.
unsafe impl Send for Zeroer {}

impl Zeroer {
    pub(super) fn new(device: &ID3D12Device) -> windows::core::Result<Zeroer> {
        // SAFETY: plain object creation; the list is closed before it is kept.
        unsafe {
            let queue: ID3D12CommandQueue =
                device.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                    Type: D3D12_COMMAND_LIST_TYPE_COPY,
                    ..Default::default()
                })?;
            let allocator: ID3D12CommandAllocator =
                device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_COPY)?;
            crate::alloc_count::track(&allocator);
            let list: ID3D12GraphicsCommandList =
                device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_COPY, &allocator, None)?;
            let buffer = |kind: D3D12_HEAP_TYPE, state| -> windows::core::Result<ID3D12Resource> {
                let mut made: Option<ID3D12Resource> = None;
                device.CreateCommittedResource(
                    &D3D12_HEAP_PROPERTIES {
                        Type: kind,
                        ..Default::default()
                    },
                    D3D12_HEAP_FLAG_NONE,
                    &super::buffers::buffer_desc(ZEROS, D3D12_RESOURCE_FLAG_NONE),
                    state,
                    None,
                    &mut made,
                )?;
                made.ok_or_else(windows::core::Error::empty)
            };
            let zeros = buffer(D3D12_HEAP_TYPE_DEFAULT, D3D12_RESOURCE_STATE_COMMON)?;
            // Not taken to start as zeros (see the top): written from the CPU.
            let first = buffer(D3D12_HEAP_TYPE_UPLOAD, D3D12_RESOURCE_STATE_GENERIC_READ)?;
            let mut data = std::ptr::null_mut();
            first.Map(0, None, Some(&mut data))?;
            std::ptr::write_bytes(data.cast::<u8>(), 0, ZEROS as usize);
            first.Unmap(0, None);
            list.CopyBufferRegion(&zeros, 0, &first, 0, ZEROS);
            list.Close()?;
            let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE)?;
            queue.ExecuteCommandLists(&[Some(list.cast()?)]);
            queue.Signal(&fence, 1)?;
            crate::host::name(&queue, || "card block reuse (zeros)".into());
            Ok(Zeroer {
                queue,
                allocator,
                list,
                fence,
                zeros,
                first: Some(first),
                sent: 1,
            })
        }
    }

    fn completed(&self) -> u64 {
        // SAFETY: a plain query; u64::MAX means the device was removed.
        let v = unsafe { self.fence.GetCompletedValue() };
        if v == u64::MAX { 0 } else { v }
    }

    /// Copies of zeros over `pieces` of the buffers placed over their blocks,
    /// sent and followed by a new fence value: returned. The last send has run.
    fn send(&mut self, pieces: &[(Piece, ID3D12Resource)]) -> windows::core::Result<u64> {
        // SAFETY: the last send has run (the caller saw its fence value), so
        // the allocator may be reset; the buffers live until the list has run
        // (the blocks stay while their pieces are out of the free lists).
        self.first = None;
        unsafe {
            self.allocator.Reset()?;
            self.list.Reset(&self.allocator, None)?;
            for (piece, whole) in pieces {
                let mut at = 0;
                while at < piece.len {
                    let n = (piece.len - at).min(ZEROS);
                    self.list
                        .CopyBufferRegion(whole, piece.start + at, &self.zeros, 0, n);
                    at += n;
                }
            }
            self.list.Close()?;
            self.queue.ExecuteCommandLists(&[Some(self.list.cast()?)]);
            self.queue.Signal(&self.fence, self.sent + 1)?;
        }
        self.sent += 1;
        Ok(self.sent)
    }
}

/// One round over `pool`: the pieces zeroed by now may be cut again, and if
/// the last send has run, the pieces waiting are zeroed by a new one
/// (`whole` names the buffer over a block). The blocks let go, to be dropped
/// outside the pool's lock.
pub(super) fn round<B: Clone>(
    pool: &'static Mutex<Pool<B>>,
    zeroer: &mut Zeroer,
    whole: impl Fn(&B) -> Option<ID3D12Resource>,
) -> Vec<B> {
    let completed = zeroer.completed();
    let mut p = lock(pool);
    let spares = p.spares();
    let gone = p.cleared(completed, spares);
    if completed < zeroer.sent || p.dirty.is_empty() {
        return gone;
    }
    let taken = p.take_dirty(SEND_MOST);
    drop(p);
    let mut unsent = Vec::new();
    let mut targets = Vec::with_capacity(taken.len());
    for (piece, memory) in &taken {
        match whole(memory) {
            Some(buffer) => targets.push((*piece, buffer)),
            None => unsent.push(*piece),
        }
    }
    let sent = zeroer.send(&targets);
    let mut p = lock(pool);
    // A piece without a buffer over its block waits until the block goes.
    p.dirty_again(unsent);
    match sent {
        Ok(value) => {
            let bytes: u64 = targets.iter().map(|(piece, _)| piece.len).sum();
            p.clearing(value, targets.iter().map(|(piece, _)| *piece));
            drop(p);
            note(targets.len() as u64, bytes);
        }
        Err(e) => {
            p.dirty_again(targets.iter().map(|(piece, _)| *piece));
            drop(p);
            if STATS[3].fetch_add(1, Ordering::Relaxed) == 0 {
                tracing::warn!(error = %e, "card block reuse: pieces not sent to be zeroed");
            }
        }
    }
    gone
}

/// Counts a send; grep `card block reuse:`.
fn note(pieces: u64, bytes: u64) {
    let n = STATS[0].fetch_add(1, Ordering::Relaxed) + 1;
    let p = STATS[1].fetch_add(pieces, Ordering::Relaxed) + pieces;
    let b = STATS[2].fetch_add(bytes, Ordering::Relaxed) + bytes;
    if n.is_power_of_two() || n.is_multiple_of(1024) {
        tracing::info!(
            "card block reuse: {n} sends, {p} pieces zeroed ({} MB) to be cut again, {} sends failed (VICTORY_CARD_BLOCK_REUSE=1)",
            b >> 20,
            STATS[3].load(Ordering::Relaxed),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::super::{BLOCK, REST, SETTLE};
    use super::*;

    const K64: u64 = 64 << 10;

    /// The submissions begun when the tests' pieces come back: far past any
    /// the crate's other tests start, so only the finished counts these tests
    /// pass (`BEGUN + 1`: not done, `BEGUN + LAG`: done) decide.
    const BEGUN: u64 = 1 << 40;
    const NOT_DONE: u64 = BEGUN + 1;
    const DONE: u64 = BEGUN + super::super::LAG;

    fn reusing(alive: &'static AtomicU64) -> Pool<u32> {
        let mut pool = Pool::fresh(2, alive);
        pool.reuse = true;
        pool
    }

    /// `piece` back at 0 ms, submission [`BEGUN`] begun.
    fn back(pool: &mut Pool<u32>, piece: Piece) {
        pool.resting.push_back((super::super::Resting { at: 0, begun: BEGUN }, piece));
    }

    /// A piece back is not zeroed before every submission begun when it came
    /// back has finished, however long it rests; it is not cut again before
    /// its zeros are written (the fence value passed), then it is, and the
    /// commit line counts it as neither held nor cuttable meanwhile.
    #[test]
    fn a_piece_is_cut_again_only_once_zeroed_after_the_card_is_done_with_it() {
        static ALIVE: AtomicU64 = AtomicU64::new(0);
        let mut pool = reusing(&ALIVE);
        let make = || Some(7);
        let a = pool.take(K64, K64, 0, make).unwrap().0;
        let _b = pool.take(K64, K64, 0, make).unwrap().0;
        back(&mut pool, a);
        // Long past the time limit, but submissions not finished: resting.
        pool.reclaim(10 * REST, NOT_DONE, 2);
        assert!(pool.dirty.is_empty() && pool.resting.len() == 1);
        let c = pool.take(K64, K64, 10 * REST, make).unwrap();
        assert_ne!(c.0.start, a.start, "not cut while the card may use it");
        // Every submission begun then finished: waiting for zeros.
        pool.reclaim(10 * REST, DONE, 2);
        assert_eq!(pool.dirty, vec![a]);
        assert_eq!(pool.usage(), [2 * K64, BLOCK - 3 * K64]);
        let d = pool.take(K64, K64, 10 * REST, make).unwrap();
        assert_ne!(d.0.start, a.start, "not cut before it is zeroed");
        let taken = pool.take_dirty(SEND_MOST);
        assert_eq!(taken, vec![(a, 7)]);
        pool.clearing(5, [a]);
        assert!(pool.cleared(4, 2).is_empty());
        let e = pool.take(K64, K64, 10 * REST, make).unwrap();
        assert_ne!(e.0.start, a.start, "not cut before the zeros are written");
        pool.cleared(5, 2);
        assert!(pool.clearing.is_empty());
        let f = pool.take(K64, K64, 10 * REST, make).unwrap();
        assert_eq!(
            (f.0.block, f.0.start, f.2),
            (a.block, a.start, true),
            "cut again once zeroed"
        );
    }

    /// A block every piece of which is back goes without being zeroed, and so
    /// do the pieces of it waiting for zeros; one with a piece being zeroed
    /// stays until the zeros are written, then stays as a spare (all zeros).
    #[test]
    fn a_block_all_back_goes_unzeroed_and_one_zeroed_stays_as_a_spare() {
        static ALIVE: AtomicU64 = AtomicU64::new(0);
        let mut pool = reusing(&ALIVE);
        let made = std::cell::Cell::new(0);
        let make = || {
            made.set(made.get() + 1);
            Some(made.get())
        };
        let a = pool.take(BLOCK / 2, K64, 0, make).unwrap().0;
        let b = pool.take(BLOCK / 2, K64, 0, make).unwrap().0;
        assert_eq!(a.block, b.block);
        back(&mut pool, a);
        back(&mut pool, b);
        let gone = pool.reclaim(SETTLE, DONE, 2);
        assert_eq!((gone, pool.blocks(), pool.dirty.len()), (vec![1], 0, 0));
        // A second block: one piece out to be zeroed when the other comes back.
        let c = pool.take(BLOCK / 2, K64, 0, make).unwrap().0;
        let d = pool.take(BLOCK / 2, K64, 0, make).unwrap().0;
        back(&mut pool, c);
        pool.reclaim(SETTLE, DONE, 2);
        let out = pool.take_dirty(SEND_MOST);
        pool.clearing(1, out.iter().map(|(p, _)| *p));
        back(&mut pool, d);
        assert!(
            pool.reclaim(SETTLE, DONE, 2).is_empty(),
            "a piece is being zeroed"
        );
        assert_eq!(pool.dirty, vec![d]);
        pool.cleared(1, 2);
        // Still not whole: `d` waits for zeros of its own.
        let out = pool.take_dirty(SEND_MOST);
        pool.clearing(2, out.iter().map(|(p, _)| *p));
        assert!(
            pool.cleared(2, 2).is_empty(),
            "a zeroed block is kept as a spare"
        );
        assert_eq!(pool.blocks(), 1);
        assert!(pool.empty(pool.blocks[c.block].as_ref().unwrap()));
    }

    /// A send takes up to its most bytes, and at least one piece.
    #[test]
    fn a_send_takes_at_most_its_bytes_and_at_least_one_piece() {
        static ALIVE: AtomicU64 = AtomicU64::new(0);
        let mut pool = reusing(&ALIVE);
        pool.dirty = (0..4)
            .map(|i| Piece {
                block: 0,
                start: i * K64,
                len: K64,
            })
            .collect();
        pool.blocks.push(Some(super::super::Block {
            memory: 3,
            free: super::super::Ranges::new(0),
            used_to: BLOCK,
            made: 0,
        }));
        assert_eq!(pool.take_dirty(2 * K64).len(), 2);
        assert_eq!(pool.take_dirty(K64 / 2).len(), 1);
        assert_eq!(pool.dirty.len(), 1);
    }

    /// On the software adapter: the zeroer writes zeros over exactly the
    /// pieces it is given, through a buffer over the whole heap, and its fence
    /// value passes once they are written.
    #[test]
    fn the_zeroer_writes_zeros_over_its_pieces_only() {
        use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
        const SIZE: u64 = 1 << 20;
        // SAFETY: plain object creation and copies on a device of our own;
        // every mapping is of a live buffer of ours, of its size.
        unsafe {
            crate::test_d3d12_debug::enable();
            let Ok(adapter) = crate::host::warp_adapter() else {
                eprintln!("no WARP adapter: skipped");
                return;
            };
            let mut device: Option<ID3D12Device> = None;
            if D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_12_0, &mut device).is_err() {
                eprintln!("no WARP device: skipped");
                return;
            }
            let device = device.unwrap();
            let mut z = Zeroer::new(&device).unwrap();
            while z.completed() < z.sent {
                std::thread::yield_now();
            }
            let heap = {
                let mut heap: Option<ID3D12Heap> = None;
                device
                    .CreateHeap(
                        &D3D12_HEAP_DESC {
                            SizeInBytes: SIZE,
                            Properties: D3D12_HEAP_PROPERTIES {
                                Type: D3D12_HEAP_TYPE_DEFAULT,
                                ..Default::default()
                            },
                            Alignment: 0,
                            Flags: D3D12_HEAP_FLAG_ALLOW_ONLY_BUFFERS,
                        },
                        &mut heap,
                    )
                    .unwrap();
                heap.unwrap()
            };
            let buffer = |kind: D3D12_HEAP_TYPE, state| {
                let mut made: Option<ID3D12Resource> = None;
                device
                    .CreateCommittedResource(
                        &D3D12_HEAP_PROPERTIES {
                            Type: kind,
                            ..Default::default()
                        },
                        D3D12_HEAP_FLAG_NONE,
                        &super::super::buffers::buffer_desc(SIZE, D3D12_RESOURCE_FLAG_NONE),
                        state,
                        None,
                        &mut made,
                    )
                    .unwrap();
                made.unwrap()
            };
            let mut whole: Option<ID3D12Resource> = None;
            device
                .CreatePlacedResource(
                    &heap,
                    0,
                    &super::super::buffers::buffer_desc(SIZE, D3D12_RESOURCE_FLAG_NONE),
                    D3D12_RESOURCE_STATE_COMMON,
                    None,
                    &mut whole,
                )
                .unwrap();
            let whole = whole.unwrap();
            let up = buffer(D3D12_HEAP_TYPE_UPLOAD, D3D12_RESOURCE_STATE_GENERIC_READ);
            let back = buffer(D3D12_HEAP_TYPE_READBACK, D3D12_RESOURCE_STATE_COPY_DEST);
            let mut data = std::ptr::null_mut();
            up.Map(0, None, Some(&mut data)).unwrap();
            std::ptr::write_bytes(data.cast::<u8>(), 0xCD, SIZE as usize);
            // Fill the heap with 0xCD, as a buffer used before leaves it.
            let queue: ID3D12CommandQueue = device
                .CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                    Type: D3D12_COMMAND_LIST_TYPE_COPY,
                    ..Default::default()
                })
                .unwrap();
            let allocator: ID3D12CommandAllocator = device
                .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_COPY)
                .unwrap();
            let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap();
            let run = |value: u64, record: &dyn Fn(&ID3D12GraphicsCommandList)| {
                let list: ID3D12GraphicsCommandList = device
                    .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_COPY, &allocator, None)
                    .unwrap();
                record(&list);
                list.Close().unwrap();
                queue.ExecuteCommandLists(&[Some(list.cast().unwrap())]);
                queue.Signal(&fence, value).unwrap();
                while fence.GetCompletedValue() < value {
                    std::thread::yield_now();
                }
                allocator.Reset().unwrap();
            };
            run(1, &|list| list.CopyBufferRegion(&whole, 0, &up, 0, SIZE));
            // Two pieces, one of more than the buffer of zeros' size would be
            // here if SIZE allowed: 64 KiB at 64 KiB and 192 KiB at 512 KiB.
            let pieces = [
                Piece {
                    block: 0,
                    start: K64,
                    len: K64,
                },
                Piece {
                    block: 0,
                    start: 8 * K64,
                    len: 3 * K64,
                },
            ];
            let value = z.send(&pieces.map(|p| (p, whole.clone()))).unwrap();
            while z.completed() < value {
                std::thread::yield_now();
            }
            run(2, &|list| list.CopyBufferRegion(&back, 0, &whole, 0, SIZE));
            let mut read = std::ptr::null_mut();
            back.Map(0, None, Some(&mut read)).unwrap();
            let bytes = std::slice::from_raw_parts(read.cast::<u8>(), SIZE as usize);
            for (i, &byte) in bytes.iter().enumerate() {
                let i = i as u64;
                let zeroed = pieces
                    .iter()
                    .any(|p| (p.start..p.start + p.len).contains(&i));
                assert_eq!(byte, if zeroed { 0 } else { 0xCD }, "byte {i}");
            }
            // A second send reuses the allocator once the first has run.
            let again = z.send(&[(pieces[0], whole.clone())]).unwrap();
            assert_eq!(again, value + 1);
            while z.completed() < again {
                std::thread::yield_now();
            }
            crate::test_d3d12_debug::assert_clean(&device, "texpool::reuse::pieces");
        }
    }

    /// On the software adapter: the buffer of zeros holds zeros even where
    /// the memory it is made in held other bytes just before. A committed
    /// buffer only "almost always" starts as zeros (the memory manager may
    /// hand a process back memory it freed itself without clearing it): WARP
    /// did, and the zeroer then wrote a test's old bytes over pieces (byte
    /// 65540 = 1, byte 81920 = 90 in loaded gate runs).
    #[test]
    fn the_buffer_of_zeros_is_zeros_over_memory_used_before() {
        use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
        // SAFETY: plain object creation and copies on a device of our own;
        // every mapping is of a live buffer of ours, of its size.
        unsafe {
            crate::test_d3d12_debug::enable();
            let Ok(adapter) = crate::host::warp_adapter() else {
                eprintln!("no WARP adapter: skipped");
                return;
            };
            let mut device: Option<ID3D12Device> = None;
            if D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_12_0, &mut device).is_err() {
                eprintln!("no WARP device: skipped");
                return;
            }
            let device = device.unwrap();
            let buffer = |kind: D3D12_HEAP_TYPE, state| {
                let mut made: Option<ID3D12Resource> = None;
                device
                    .CreateCommittedResource(
                        &D3D12_HEAP_PROPERTIES {
                            Type: kind,
                            ..Default::default()
                        },
                        D3D12_HEAP_FLAG_NONE,
                        &super::super::buffers::buffer_desc(ZEROS, D3D12_RESOURCE_FLAG_NONE),
                        state,
                        None,
                        &mut made,
                    )
                    .unwrap();
                made.unwrap()
            };
            let queue: ID3D12CommandQueue = device
                .CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                    Type: D3D12_COMMAND_LIST_TYPE_COPY,
                    ..Default::default()
                })
                .unwrap();
            let allocator: ID3D12CommandAllocator = device
                .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_COPY)
                .unwrap();
            let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap();
            let next = std::cell::Cell::new(0);
            let run = |record: &dyn Fn(&ID3D12GraphicsCommandList)| {
                let list: ID3D12GraphicsCommandList = device
                    .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_COPY, &allocator, None)
                    .unwrap();
                record(&list);
                list.Close().unwrap();
                queue.ExecuteCommandLists(&[Some(list.cast().unwrap())]);
                next.set(next.get() + 1);
                queue.Signal(&fence, next.get()).unwrap();
                while fence.GetCompletedValue() < next.get() {
                    std::thread::yield_now();
                }
                allocator.Reset().unwrap();
            };
            let up = buffer(D3D12_HEAP_TYPE_UPLOAD, D3D12_RESOURCE_STATE_GENERIC_READ);
            let mut data = std::ptr::null_mut();
            up.Map(0, None, Some(&mut data)).unwrap();
            std::ptr::write_bytes(data.cast::<u8>(), 0xCD, ZEROS as usize);
            let back = buffer(D3D12_HEAP_TYPE_READBACK, D3D12_RESOURCE_STATE_COPY_DEST);
            let mut read = std::ptr::null_mut();
            back.Map(0, None, Some(&mut read)).unwrap();
            let bytes = std::slice::from_raw_parts(read.cast::<u8>(), ZEROS as usize);
            for round in 0..8 {
                // Buffers of the same size filled with other bytes and let go.
                for _ in 0..4 {
                    let used = buffer(D3D12_HEAP_TYPE_DEFAULT, D3D12_RESOURCE_STATE_COMMON);
                    run(&|list| list.CopyBufferRegion(&used, 0, &up, 0, ZEROS));
                }
                let z = Zeroer::new(&device).unwrap();
                while z.completed() < z.sent {
                    std::thread::yield_now();
                }
                run(&|list| list.CopyBufferRegion(&back, 0, &z.zeros, 0, ZEROS));
                if let Some(at) = bytes.iter().position(|&b| b != 0) {
                    panic!("round {round}: byte {at} of the buffer of zeros is {}", bytes[at]);
                }
            }
            crate::test_d3d12_debug::assert_clean(&device, "texpool::reuse::zeros");
        }
    }

    /// (racefix #57) Tests `VICTORY_CARD_BLOCK_REUSE=1`'s exact cross-queue
    /// layout on WARP under `VICTORY_TEST_D3D12_DEBUG=1` + GBV: `Zeroer`
    /// zeroes piece A `[0, 64 KiB)` through the `whole` buffer on the COPY
    /// queue while a disjoint placed buffer B `[64 KiB, 128 KiB)` in the same
    /// 64 MiB `ID3D12Heap` is written on a DIRECT queue.
    #[test]
    fn the_zeroer_on_copy_queue_beside_a_live_placed_buffer_in_the_same_block() {
        use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
        use windows::Win32::System::Threading::{CreateEventW, INFINITE, WaitForSingleObject};
        let _serial = crate::indirect::tests::WARP_SERIAL
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // SAFETY: plain D3D12 object creation and synchronization on a WARP device of our own.
        unsafe {
            crate::test_d3d12_debug::enable();
            let Ok(adapter) = crate::host::warp_adapter() else {
                eprintln!("no WARP adapter: skipped");
                return;
            };
            let mut device: Option<ID3D12Device> = None;
            if D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_12_0, &mut device).is_err() {
                eprintln!("no WARP device: skipped");
                return;
            }
            let device = device.unwrap();
            let mut z = Zeroer::new(&device).unwrap();
            while z.completed() < z.sent {
                std::thread::yield_now();
            }
            let mut heap: Option<ID3D12Heap> = None;
            device
                .CreateHeap(
                    &D3D12_HEAP_DESC {
                        SizeInBytes: 2 * K64,
                        Properties: D3D12_HEAP_PROPERTIES {
                            Type: D3D12_HEAP_TYPE_DEFAULT,
                            ..Default::default()
                        },
                        Alignment: 0,
                        Flags: D3D12_HEAP_FLAG_ALLOW_ONLY_BUFFERS,
                    },
                    &mut heap,
                )
                .unwrap();
            let heap = heap.unwrap();
            let mut whole: Option<ID3D12Resource> = None;
            device
                .CreatePlacedResource(
                    &heap,
                    0,
                    &super::super::buffers::buffer_desc(2 * K64, D3D12_RESOURCE_FLAG_NONE),
                    D3D12_RESOURCE_STATE_COMMON,
                    None,
                    &mut whole,
                )
                .unwrap();
            let whole = whole.unwrap();
            let mut live_b: Option<ID3D12Resource> = None;
            device
                .CreatePlacedResource(
                    &heap,
                    K64,
                    &super::super::buffers::buffer_desc(K64, D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS),
                    D3D12_RESOURCE_STATE_COMMON,
                    None,
                    &mut live_b,
                )
                .unwrap();
            let live_b = live_b.unwrap();
            let mut upload: Option<ID3D12Resource> = None;
            device
                .CreateCommittedResource(
                    &D3D12_HEAP_PROPERTIES {
                        Type: D3D12_HEAP_TYPE_UPLOAD,
                        ..Default::default()
                    },
                    D3D12_HEAP_FLAG_NONE,
                    &super::super::buffers::buffer_desc(K64, D3D12_RESOURCE_FLAG_NONE),
                    D3D12_RESOURCE_STATE_GENERIC_READ,
                    None,
                    &mut upload,
                )
                .unwrap();
            let upload = upload.unwrap();
            let mut ptr = std::ptr::null_mut();
            upload.Map(0, None, Some(&mut ptr)).unwrap();
            std::ptr::write_bytes(ptr.cast::<u8>(), 0x5A, K64 as usize);
            upload.Unmap(0, None);

            let dq: ID3D12CommandQueue = device
                .CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                    Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
                    ..Default::default()
                })
                .unwrap();
            let da: ID3D12CommandAllocator = device
                .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
                .unwrap();
            let dl: ID3D12GraphicsCommandList = device
                .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &da, None)
                .unwrap();
            let df: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap();
            let ev = CreateEventW(None, false, false, None).unwrap();

            // Write disjoint placed buffer B on DIRECT while Zeroer zeroes piece A via `whole` on COPY.
            dl.CopyBufferRegion(&live_b, 0, &upload, 0, K64);
            dl.Close().unwrap();
            dq.ExecuteCommandLists(&[Some(dl.cast().unwrap())]);
            dq.Signal(&df, 1).unwrap();
            let z_done = z
                .send(&[(
                    Piece {
                        block: 0,
                        start: 0,
                        len: K64,
                    },
                    whole.clone(),
                )])
                .unwrap();

            df.SetEventOnCompletion(1, ev).unwrap();
            WaitForSingleObject(ev, INFINITE);
            while z.completed() < z_done {
                std::thread::yield_now();
            }
            let _ = windows::Win32::Foundation::CloseHandle(ev);
            crate::test_d3d12_debug::assert_clean(&device, "texpool::reuse::cross_queue_aliasing");
        }
    }
}
