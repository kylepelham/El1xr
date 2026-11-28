//! (worldstream, #26) What one `UpdateTileMappings` costs on the card, the
//! title not running: alone, with the flag that says no work in flight
//! touches the tiles, with ranges joined, and beside what the loading bundle
//! runs at the same time (the texture block thread's `CreateHeap` +
//! `MakeResident` + wait, a heap made and dropped, placed textures made,
//! other threads mapping, copies running on the same queue).
//!
//! Run by name, alone, under the `gpu` lock, no game:
//! `VICTORY_TILES_TEST_CARD=1 cargo test -p victory-gpu --release --lib tiles::bench -- --ignored --nocapture --test-threads=1`
//! Without `VICTORY_TILES_TEST_CARD=1` it does nothing. It sends only tile
//! mappings and copies of a few megabytes; every wait is bounded.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{HANDLE, WAIT_OBJECT_0};
use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::core::Interface;

const HEAP_TILES: u32 = super::HEAP_TILES;
const TEXTURES: usize = 120;
const SIDE: u32 = 2048;
const LEVELS: u16 = 12;

struct Card {
    device: ID3D12Device,
    queue: ID3D12CommandQueue,
}

// SAFETY: D3D12 devices and queues are free-threaded.
unsafe impl Send for Card {}
unsafe impl Sync for Card {}

fn card() -> Option<Arc<Card>> {
    if !std::env::var_os("VICTORY_TILES_TEST_CARD").is_some_and(|v| v == "1") {
        eprintln!("VICTORY_TILES_TEST_CARD=1 not set: the bench runs only on the card; skipped");
        return None;
    }
    // SAFETY: plain object creation.
    unsafe {
        let mut device: Option<ID3D12Device> = None;
        D3D12CreateDevice(None, D3D_FEATURE_LEVEL_12_0, &mut device).ok()?;
        let device = device?;
        let queue: ID3D12CommandQueue = device
            .CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
                ..Default::default()
            })
            .ok()?;
        Some(Arc::new(Card { device, queue }))
    }
}

fn heap(device: &ID3D12Device, flags: D3D12_HEAP_FLAGS) -> ID3D12Heap {
    let desc = D3D12_HEAP_DESC {
        SizeInBytes: u64::from(HEAP_TILES) * super::TILE,
        Properties: D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_DEFAULT,
            ..Default::default()
        },
        Alignment: 0,
        Flags: D3D12_HEAP_FLAG_ALLOW_ONLY_NON_RT_DS_TEXTURES | flags,
    };
    let mut made: Option<ID3D12Heap> = None;
    // SAFETY: plain creation.
    unsafe { device.CreateHeap(&desc, &mut made) }.expect("a heap");
    made.expect("a heap")
}

fn desc(reserved: bool) -> D3D12_RESOURCE_DESC {
    D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
        Width: u64::from(SIDE),
        Height: SIDE,
        DepthOrArraySize: 1,
        MipLevels: LEVELS,
        Format: DXGI_FORMAT_BC7_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Layout: if reserved {
            D3D12_TEXTURE_LAYOUT_64KB_UNDEFINED_SWIZZLE
        } else {
            D3D12_TEXTURE_LAYOUT_UNKNOWN
        },
        ..Default::default()
    }
}

/// A reserved texture and its standard levels' tiles and packed tiles.
fn reserved(device: &ID3D12Device) -> (ID3D12Resource, Vec<u32>, u32) {
    let mut made: Option<ID3D12Resource> = None;
    // SAFETY: plain creation.
    unsafe {
        device.CreateReservedResource(&desc(true), D3D12_RESOURCE_STATE_COPY_DEST, None, &mut made)
    }
    .expect("reserved");
    let texture = made.expect("reserved");
    let tiled = super::Tiled::make(device, &texture, u32::from(LEVELS)).expect("tiling");
    let standard = tiled.standard.clone();
    let packed = tiled.packed;
    drop(tiled);
    (texture, standard, packed)
}

/// How one texture's levels are named in the call.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Shape {
    /// As `map_locked` does: a region a level, a range a level.
    PerLevel,
    /// One region from level 1's first tile over every level after it,
    /// one range (the tiles taken in one run).
    Joined,
}

struct Set {
    textures: Vec<(ID3D12Resource, Vec<u32>, u32)>,
    heaps: Vec<ID3D12Heap>,
}

fn set(device: &ID3D12Device, mappers: usize) -> Vec<Set> {
    (0..mappers)
        .map(|_| Set {
            textures: (0..TEXTURES).map(|_| reserved(device)).collect(),
            heaps: (0..12)
                .map(|_| heap(device, D3D12_HEAP_FLAG_NONE))
                .collect(),
        })
        .collect()
}

/// Maps levels 1.. of every texture of `set` (as a texture made level by
/// level holds its smaller levels), one call a texture, then takes every
/// mapping away again (one call a texture, not timed). The microseconds
/// of each mapping call.
fn burst(
    queue: &ID3D12CommandQueue,
    set: &Set,
    shape: Shape,
    flags: D3D12_TILE_MAPPING_FLAGS,
    serial: bool,
) -> Vec<u64> {
    let mut times = Vec::with_capacity(set.textures.len());
    let (mut heap_at, mut tile) = (0usize, 0u32);
    for (texture, standard, packed) in &set.textures {
        let tiles: u32 = standard[1..].iter().sum::<u32>() + packed;
        if tile + tiles > HEAP_TILES {
            heap_at += 1;
            tile = 0;
        }
        let heap = &set.heaps[heap_at % set.heaps.len()];
        let mut coords = Vec::new();
        let mut sizes = Vec::new();
        let (mut starts, mut counts) = (Vec::new(), Vec::new());
        match shape {
            Shape::PerLevel => {
                let mut at = tile;
                for (level, &n) in standard.iter().enumerate().skip(1) {
                    coords.push(D3D12_TILED_RESOURCE_COORDINATE {
                        Subresource: level as u32,
                        ..Default::default()
                    });
                    sizes.push(D3D12_TILE_REGION_SIZE {
                        NumTiles: n,
                        ..Default::default()
                    });
                    starts.push(at);
                    counts.push(n);
                    at += n;
                }
                if *packed > 0 {
                    coords.push(D3D12_TILED_RESOURCE_COORDINATE {
                        Subresource: standard.len() as u32,
                        ..Default::default()
                    });
                    sizes.push(D3D12_TILE_REGION_SIZE {
                        NumTiles: *packed,
                        ..Default::default()
                    });
                    starts.push(at);
                    counts.push(*packed);
                }
            }
            Shape::Joined => {
                coords.push(D3D12_TILED_RESOURCE_COORDINATE {
                    Subresource: 1,
                    ..Default::default()
                });
                sizes.push(D3D12_TILE_REGION_SIZE {
                    NumTiles: tiles,
                    ..Default::default()
                });
                starts.push(tile);
                counts.push(tiles);
            }
        }
        let range_flags = vec![D3D12_TILE_RANGE_FLAG_NONE; starts.len()];
        let began = Instant::now();
        let held = serial.then(|| SERIAL.lock().unwrap_or_else(|e| e.into_inner()));
        // SAFETY: a live reserved texture and heap; arrays of the lengths named.
        unsafe {
            queue.UpdateTileMappings(
                texture,
                coords.len() as u32,
                Some(coords.as_ptr()),
                Some(sizes.as_ptr()),
                heap,
                starts.len() as u32,
                Some(range_flags.as_ptr()),
                Some(starts.as_ptr()),
                Some(counts.as_ptr()),
                flags,
            );
        }
        drop(held);
        times.push(began.elapsed().as_micros() as u64);
        tile += tiles;
    }
    times
}

/// Takes every mapping of `set` away again (one call a texture).
fn unmap(queue: &ID3D12CommandQueue, set: &Set) {
    for (texture, standard, packed) in &set.textures {
        let tiles: u32 = standard[1..].iter().sum::<u32>() + packed;
        let coord = D3D12_TILED_RESOURCE_COORDINATE {
            Subresource: 1,
            ..Default::default()
        };
        let size = D3D12_TILE_REGION_SIZE {
            NumTiles: tiles,
            ..Default::default()
        };
        let null = D3D12_TILE_RANGE_FLAG_NULL;
        // SAFETY: as above; a NULL range takes the mappings away.
        unsafe {
            queue.UpdateTileMappings(
                texture,
                1,
                Some(&coord),
                Some(&size),
                None,
                1,
                Some(&null),
                None,
                Some(&tiles),
                D3D12_TILE_MAPPING_FLAG_NONE,
            );
        }
    }
}

struct Fenced {
    fence: ID3D12Fence,
    event: HANDLE,
    value: u64,
}

impl Fenced {
    fn new(device: &ID3D12Device) -> Fenced {
        // SAFETY: plain creation.
        unsafe {
            Fenced {
                fence: device.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap(),
                event: CreateEventW(None, false, false, None).unwrap(),
                value: 0,
            }
        }
    }

    /// Waits (at most 1.5 s) for everything on `queue` so far.
    fn drain(&mut self, queue: &ID3D12CommandQueue) {
        self.value += 1;
        // SAFETY: our fence and event.
        unsafe {
            queue.Signal(&self.fence, self.value).unwrap();
            self.fence
                .SetEventOnCompletion(self.value, self.event)
                .unwrap();
            assert_eq!(
                WaitForSingleObject(self.event, 1500),
                WAIT_OBJECT_0,
                "the queue did not finish in 1.5 s"
            );
        }
    }
}

/// What runs beside the mapping threads.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Beside {
    Nothing,
    /// The texture block thread: a 64 MiB heap made not resident,
    /// `MakeResident`, a copy on a queue of its own waited for; eight kept,
    /// the oldest released.
    Blocks,
    /// A 64 MiB heap made (resident) and released, in a loop.
    Heaps,
    /// Four threads making placed textures in heaps and releasing them
    /// (texture heaps on the prepare threads).
    Placed,
}

/// How the mapping calls and the kernel memory work beside them meet.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Gate {
    /// Not at all.
    Free,
    /// One lock: every mapping call, and every `CreateHeap` +
    /// `MakeResident` (or placed texture made) beside, holds it.
    Lock,
    /// One thread maps every set in turn (nothing beside waits).
    Seq,
}

static SERIAL: Mutex<()> = Mutex::new(());

/// Runs `f`, the kernel memory work beside, under `gate`.
fn gated<R>(gate: Gate, f: impl FnOnce() -> R) -> R {
    let began = Instant::now();
    let out = gated_inner(gate, f);
    BESIDE_US.fetch_add(began.elapsed().as_micros() as u64, Ordering::Relaxed);
    out
}

static BESIDE_US: AtomicU64 = AtomicU64::new(0);

fn gated_inner<R>(gate: Gate, f: impl FnOnce() -> R) -> R {
    match gate {
        Gate::Free | Gate::Seq => f(),
        Gate::Lock => {
            let _held = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
            f()
        }
    }
}

fn beside(card: Arc<Card>, what: Beside, gate: Gate, stop: Arc<AtomicBool>) -> Vec<std::thread::JoinHandle<u64>> {
    let threads = if what == Beside::Placed { 4 } else { 1 };
    (0..threads)
        .map(|_| {
            let (card, stop) = (card.clone(), stop.clone());
            std::thread::spawn(move || beside_one(&card, what, gate, &stop))
        })
        .collect()
}

fn beside_one(card: &Card, what: Beside, gate: Gate, stop: &AtomicBool) -> u64 {
    let device = &card.device;
    let mut n = 0u64;
    match what {
        Beside::Nothing => {}
        Beside::Blocks => {
            // SAFETY: plain creation and a copy between buffers of ours.
            unsafe {
                let queue: ID3D12CommandQueue = device
                    .CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                        Type: D3D12_COMMAND_LIST_TYPE_COPY,
                        ..Default::default()
                    })
                    .unwrap();
                let (list, _keep) = copy_list(device, D3D12_COMMAND_LIST_TYPE_COPY, 256);
                let mut fenced = Fenced::new(device);
                let mut kept = std::collections::VecDeque::new();
                while !stop.load(Ordering::Relaxed) {
                    let made = gated(gate, || {
                        let made = heap(device, D3D12_HEAP_FLAG_CREATE_NOT_RESIDENT | D3D12_HEAP_FLAG_CREATE_NOT_ZEROED);
                        let pageable: ID3D12Pageable = made.cast().unwrap();
                        device.MakeResident(&[Some(pageable)]).unwrap();
                        made
                    });
                    queue.ExecuteCommandLists(&[Some(list.clone())]);
                    fenced.drain(&queue);
                    kept.push_back(made);
                    if kept.len() > 8 {
                        let old = kept.pop_front();
                        gated(gate, || drop(old));
                    }
                    n += 1;
                }
            }
        }
        Beside::Heaps => {
            while !stop.load(Ordering::Relaxed) {
                let made = gated(gate, || heap(device, D3D12_HEAP_FLAG_NONE));
                gated(gate, || drop(made));
                n += 1;
            }
        }
        Beside::Placed => {
            let block = heap(device, D3D12_HEAP_FLAG_NONE);
            let plain = desc(false);
            // SAFETY: plain query.
            let info = unsafe { device.GetResourceAllocationInfo(0, &[plain]) };
            while !stop.load(Ordering::Relaxed) {
                let mut made = Vec::new();
                let mut at = 0u64;
                while at + info.SizeInBytes <= u64::from(HEAP_TILES) * super::TILE {
                    let mut t: Option<ID3D12Resource> = None;
                    // SAFETY: a texture placed inside the heap, aligned.
                    gated(gate, || unsafe {
                        device.CreatePlacedResource(&block, at, &plain, D3D12_RESOURCE_STATE_COPY_DEST, None, &mut t)
                    })
                    .unwrap();
                    made.push(t);
                    at += info.SizeInBytes.next_multiple_of(info.Alignment);
                    n += 1;
                }
            }
        }
    }
    n
}

/// A closed list copying `bytes` between two buffers of its own.
fn copy_list(
    device: &ID3D12Device,
    kind: D3D12_COMMAND_LIST_TYPE,
    bytes: u64,
) -> (
    ID3D12CommandList,
    (ID3D12CommandAllocator, [ID3D12Resource; 2]),
) {
    let buffer = || {
        let mut made: Option<ID3D12Resource> = None;
        // SAFETY: plain creation.
        unsafe {
            device
                .CreateCommittedResource(
                    &D3D12_HEAP_PROPERTIES {
                        Type: D3D12_HEAP_TYPE_DEFAULT,
                        ..Default::default()
                    },
                    D3D12_HEAP_FLAG_NONE,
                    &D3D12_RESOURCE_DESC {
                        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                        Width: bytes,
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
                    D3D12_RESOURCE_STATE_COMMON,
                    None,
                    &mut made,
                )
                .unwrap();
        }
        made.unwrap()
    };
    let (a, b) = (buffer(), buffer());
    // SAFETY: plain creation and recording.
    unsafe {
        let allocator: ID3D12CommandAllocator = device.CreateCommandAllocator(kind).unwrap();
        let list: ID3D12GraphicsCommandList =
            device.CreateCommandList(0, kind, &allocator, None).unwrap();
        list.CopyBufferRegion(&b, 0, &a, 0, bytes);
        list.Close().unwrap();
        (list.cast().unwrap(), (allocator, [a, b]))
    }
}

#[derive(Default)]
struct Pooled {
    times: Vec<u64>,
    wall: Duration,
    beside: u64,
    beside_us: u64,
}

impl Pooled {
    fn line(&mut self, label: &str) {
        let times = &mut self.times;
        times.sort_unstable();
        let n = times.len().max(1);
        let sum = times.iter().sum::<u64>();
        let at = |p: f64| times.get(((n as f64 - 1.0) * p) as usize).copied().unwrap_or(0);
        eprintln!(
            "BENCH {label:<52} calls {:>5} mean {:>7.1} us p50 {:>5} p90 {:>5} p99 {:>6} max {:>7} | in calls >2ms {:>4.0}% | wall/call {:>6.1} us | beside {} ops, {:.0} us an op",
            times.len(),
            sum as f64 / n as f64,
            at(0.5),
            at(0.9),
            at(0.99),
            times.last().copied().unwrap_or(0),
            100.0 * times.iter().filter(|&&t| t > 2000).sum::<u64>() as f64 / sum.max(1) as f64,
            self.wall.as_secs_f64() * 1e6 / n as f64,
            self.beside,
            self.beside_us as f64 / self.beside.max(1) as f64
        );
    }
}

/// One rep: `mappers` threads map their sets (a burst each) while `what`
/// runs beside them under `gate`.
fn rep(card: &Arc<Card>, sets: &Arc<Vec<Set>>, case: &Case, into: &mut Pooled) {
    let stop = Arc::new(AtomicBool::new(false));
    let side = beside(card.clone(), case.beside, case.gate, stop.clone());
    std::thread::sleep(Duration::from_millis(20));
    let began = Instant::now();
    let (shape, flags, serial) = (case.shape, case.flags, case.gate == Gate::Lock);
    let handles: Vec<_> = if case.gate == Gate::Seq {
        let (card, sets, n) = (card.clone(), sets.clone(), case.mappers);
        vec![std::thread::spawn(move || {
            (0..n).flat_map(|m| burst(&card.queue, &sets[m], shape, flags, false)).collect::<Vec<_>>()
        })]
    } else {
        (0..case.mappers)
            .map(|m| {
                let (card, sets) = (card.clone(), sets.clone());
                std::thread::spawn(move || burst(&card.queue, &sets[m], shape, flags, serial))
            })
            .collect()
    };
    into.times.extend(handles.into_iter().flat_map(|h| h.join().unwrap()));
    into.wall += began.elapsed();
    stop.store(true, Ordering::Relaxed);
    into.beside += side.into_iter().map(|h| h.join().unwrap()).sum::<u64>();
    into.beside_us += BESIDE_US.swap(0, Ordering::Relaxed);
    for set in &sets[..case.mappers] {
        unmap(&card.queue, set);
    }
    Fenced::new(&card.device).drain(&card.queue);
}

struct Case {
    mappers: usize,
    shape: Shape,
    flags: D3D12_TILE_MAPPING_FLAGS,
    beside: Beside,
    gate: Gate,
}

impl Case {
    fn label(&self) -> String {
        let flag = if self.flags == D3D12_TILE_MAPPING_FLAG_NO_HAZARD { "NO_HAZARD" } else { "NONE" };
        format!("{}x {:?} {flag} beside {:?} gate {:?}", self.mappers, self.shape, self.beside, self.gate)
    }
}

#[test]
#[ignore = "on the card, under the gpu lock: run it by name with --ignored --nocapture"]
fn tile_mapping_costs() {
    let Some(card) = card() else {
        return;
    };
    let sets = Arc::new(set(&card.device, 4));
    let (s, p) = (&sets[0].textures[0].1, sets[0].textures[0].2);
    eprintln!("texture {SIDE}x{SIDE} BC7 {LEVELS} levels: standard tiles {s:?}, packed {p}; levels 1.. mapped a call");
    let reps = 10;
    let (none, no_hazard) = (D3D12_TILE_MAPPING_FLAG_NONE, D3D12_TILE_MAPPING_FLAG_NO_HAZARD);
    let case = |mappers, shape, flags, beside, gate| Case { mappers, shape, flags, beside, gate };
    use {Beside as B, Gate as G, Shape as S};
    let cases = [
        case(1, S::PerLevel, none, B::Nothing, G::Free),
        case(1, S::Joined, none, B::Nothing, G::Free),
        case(1, S::PerLevel, no_hazard, B::Nothing, G::Free),
        case(1, S::PerLevel, none, B::Blocks, G::Free),
        case(1, S::PerLevel, none, B::Blocks, G::Lock),
        case(1, S::PerLevel, none, B::Placed, G::Free),
        case(1, S::PerLevel, none, B::Placed, G::Lock),
        case(1, S::PerLevel, none, B::Heaps, G::Free),
        case(2, S::PerLevel, none, B::Nothing, G::Free),
        case(2, S::PerLevel, none, B::Nothing, G::Lock),
        case(4, S::PerLevel, none, B::Nothing, G::Free),
        case(4, S::PerLevel, none, B::Nothing, G::Lock),
        case(4, S::PerLevel, none, B::Nothing, G::Seq),
        case(4, S::Joined, none, B::Nothing, G::Lock),
        case(4, S::PerLevel, none, B::Blocks, G::Free),
        case(4, S::PerLevel, none, B::Blocks, G::Lock),
    ];
    let mut pooled: Vec<Pooled> = cases.iter().map(|_| Pooled::default()).collect();
    // Warm up.
    rep(&card, &sets, &cases[0], &mut Pooled::default());
    // Interleaved: every case once a rep, so the machine's load falls on all alike.
    for _ in 0..reps {
        for (c, into) in cases.iter().zip(pooled.iter_mut()) {
            rep(&card, &sets, c, into);
        }
    }
    for (c, p) in cases.iter().zip(pooled.iter_mut()) {
        p.line(&c.label());
    }
}
