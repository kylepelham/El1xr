//! Keeps the card's clocks up while it has nothing of ours queued
//! (`VICTORY_CARD_AWAKE=1`, off by default; `=turns`: 20 s on, 20 s off, to
//! compare the two in one run). The card stands idle some 6 ms of every
//! frame, in many short stretches while the CPU prepares the next
//! submission; the driver takes that for a light load and lowers the clocks,
//! and what comes next starts slow.
//!
//! Source: foxhound's `crates/victory-gpu/src/host/awake.rs` (GPL-3.0),
//! where a captured frame whose lists take 30 ms sent in one go took
//! 38-40 ms sent with 1.2-2.4 ms gaps between submissions. There one group
//! counts on a compute queue for as long as submissions come; here it
//! counts only in the card's idle stretches as the card's clock
//! (`host::card_clock_watch`) sees them: the clock's thread says when the
//! card has run everything of ours ([`idle`]), the next send of ours says
//! it has work again ([`sent`]).
//!
//! It cannot hold real work up: one group of 64 threads (one or two of the
//! card's wave slots) for under a tenth of a millisecond a dispatch, on a
//! compute queue of the lowest priority, never more than two on the way,
//! and nothing new once a send of ours is queued or the card has stood
//! idle for a quarter of a second (a title that stands still or loads).
//! It stops at the title's exit ([`stop`]) and when the device is removed.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Condvar, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::core::Interface;

/// Counts `turns` times per thread; the root constant is the count.
const COUNT: &str = r#"
#define RS "RootConstants(num32BitConstants=1, b0), UAV(u0)"
cbuffer Turns : register(b0) { uint turns; };
RWByteAddressBuffer sum : register(u0);
[RootSignature(RS)]
[numthreads(64, 1, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    uint x = id.x;
    [loop] for (uint i = 0; i < turns; i++) x = x * 1664525u + 1013904223u;
    sum.Store((id.x & 63u) * 4u, x);
}
"#;

/// Turns of a keep-alive dispatch: 0.086 ms at full clocks on the RTX 5090
/// (the `card_awake_gapped_vs_awake` bench, where 200 workloads of 1 ms sent
/// with 1.5 ms gaps took 1559 ms of card time without and 1447 ms with the
/// card kept awake over six rounds, 1208 ms sent in one go).
const TURNS: u32 = 50_000;

/// Keep-alive dispatches on the way at most: one runs, one waits behind it.
const IN_FLIGHT: u64 = 2;

/// Nothing is sent once the card has stood idle this long.
const QUIET: Duration = Duration::from_millis(250);

/// With `=turns`: this long on, this long off.
const TURN_MS: u128 = 20_000;

#[derive(Clone, Copy, PartialEq, Debug)]
enum Mode {
    Off,
    On,
    ByTurns,
}

fn parse(value: Option<&str>) -> Mode {
    match value {
        Some("1") => Mode::On,
        Some("turns") => Mode::ByTurns,
        _ => Mode::Off,
    }
}

fn mode() -> Mode {
    #[cfg(test)]
    if tests::FORCED.load(Relaxed) {
        return Mode::On;
    }
    static MODE: OnceLock<Mode> = OnceLock::new();
    *MODE.get_or_init(|| parse(std::env::var("VICTORY_CARD_AWAKE").ok().as_deref()))
}

fn on() -> bool {
    mode() != Mode::Off
}

/// With `=turns`, whether this is an on turn (the first 20 s are off).
fn turn_on() -> bool {
    static START: OnceLock<Instant> = OnceLock::new();
    mode() != Mode::ByTurns
        || START.get_or_init(Instant::now).elapsed().as_millis() / TURN_MS % 2 == 1
}

struct State {
    /// Since when the card has had nothing of ours queued; `None` while it has.
    idle_since: Option<Instant>,
    stop: bool,
}

static STATE: Mutex<State> = Mutex::new(State {
    idle_since: None,
    stop: false,
});
static WAKE: Condvar = Condvar::new();
static THREAD: Mutex<Option<std::thread::JoinHandle<()>>> = Mutex::new(None);

fn state() -> MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

/// Since when the thread keeps the current idle stretch covered
/// ([`crate::host::now_us`] plus one; 0: it does not).
static COVERING: AtomicU64 = AtomicU64::new(0);

/// Since the last line: keep-alive dispatches sent, microseconds of idle
/// stretches covered.
static STATS: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

/// The covering of the current idle stretch, if any, ends now.
fn end_cover() {
    let since = COVERING.swap(0, Relaxed);
    if since != 0 {
        STATS[1].fetch_add((crate::host::now_us() + 1).saturating_sub(since), Relaxed);
    }
}

/// Lists of ours went to `device`'s queue: the card has work, nothing new is
/// sent. Starts the thread at the first call. Called under the card clock's
/// lock, as [`idle`] is, so the two come in the order they happened.
pub(crate) fn sent(device: &ID3D12Device) {
    if !on() {
        return;
    }
    static STARTED: std::sync::Once = std::sync::Once::new();
    STARTED.call_once(|| start(device.clone()));
    state().idle_since = None;
    end_cover();
}

/// The card has run everything of ours sent so far.
pub(crate) fn idle() {
    if !on() {
        return;
    }
    let mut s = state();
    if s.idle_since.is_none() {
        s.idle_since = Some(Instant::now());
    }
    drop(s);
    WAKE.notify_one();
}

/// Ends the thread, after what it sent has run: at the title's exit, before
/// Windows ends the threads (a thread ended inside the runtime may hold its
/// locks).
pub fn stop() {
    if !on() {
        return;
    }
    state().stop = true;
    WAKE.notify_one();
    let thread = THREAD.lock().unwrap_or_else(|e| e.into_inner()).take();
    if let Some(thread) = thread {
        let _ = thread.join();
    }
}

fn start(device: ID3D12Device) {
    let spawned = std::thread::Builder::new()
        .name("victory-card-awake".into())
        .spawn(move || {
            let ran = run(&device);
            #[cfg(test)]
            eprintln!("the card's keep-alive thread ended: {ran:?}");
            match ran {
                Ok(()) => tracing::info!("the card is no longer kept awake: the title ends"),
                Err(e) => tracing::warn!(error = %e, "the card is no longer kept awake (VICTORY_CARD_AWAKE)"),
            }
            end_cover();
        });
    match spawned {
        Ok(thread) => *THREAD.lock().unwrap_or_else(|e| e.into_inner()) = Some(thread),
        Err(e) => tracing::warn!(error = %e, "no thread to keep the card awake"),
    }
}

fn run(device: &ID3D12Device) -> windows::core::Result<()> {
    let kit = Kit::new(device, D3D12_COMMAND_LIST_TYPE_COMPUTE, TURNS)?;
    tracing::info!(turns = TURNS, mode = ?mode(), "the card is kept awake in its idle stretches (VICTORY_CARD_AWAKE)");
    let mut was_on = turn_on();
    let mut sent = 0u64;
    let result = loop {
        {
            let s = state();
            if s.stop {
                break Ok(());
            }
            let turn = turn_on();
            if turn != was_on {
                was_on = turn;
                tracing::info!(on = turn, "the card is kept awake, by turns");
            }
            if !(turn && s.idle_since.is_some_and(|t| t.elapsed() < QUIET)) {
                end_cover();
                drop(WAKE.wait_timeout(s, Duration::from_millis(50)));
                continue;
            }
            let _ = COVERING.compare_exchange(0, crate::host::now_us() + 1, Relaxed, Relaxed);
        }
        if sent >= IN_FLIGHT && kit.completed().saturating_add(IN_FLIGHT) <= sent {
            if let Err(e) = kit.wait(sent + 1 - IN_FLIGHT, 100) {
                break Err(e);
            }
            continue;
        }
        if let Err(e) = kit.send(sent + 1) {
            break Err(e);
        }
        sent += 1;
        STATS[0].fetch_add(1, Relaxed);
    };
    kit.finish(sent);
    result
}

/// A queue of its own that runs one closed list of a counting dispatch over
/// and over, and a fence behind each run.
struct Kit {
    device: ID3D12Device,
    queue: ID3D12CommandQueue,
    _root: ID3D12RootSignature,
    _pso: ID3D12PipelineState,
    _sum: ID3D12Resource,
    _allocator: ID3D12CommandAllocator,
    list: Option<ID3D12CommandList>,
    fence: ID3D12Fence,
    event: HANDLE,
}

// SAFETY: free-threaded D3D12 objects and an event, used by one thread at a time.
unsafe impl Send for Kit {}

/// The counting pipeline and a 256-byte buffer it writes.
fn counting(
    device: &ID3D12Device,
) -> windows::core::Result<(ID3D12RootSignature, ID3D12PipelineState, ID3D12Resource)> {
    let code = crate::host::compile(
        COUNT,
        windows::core::s!("main"),
        windows::core::s!("cs_5_1"),
    )?;
    // SAFETY: plain object creation; the code lives through the calls and
    // holds the root signature.
    unsafe {
        let root: ID3D12RootSignature = device.CreateRootSignature(0, &code)?;
        let pso: ID3D12PipelineState =
            device.CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                pRootSignature: std::mem::transmute_copy(&root),
                CS: D3D12_SHADER_BYTECODE {
                    pShaderBytecode: code.as_ptr().cast(),
                    BytecodeLength: code.len(),
                },
                ..Default::default()
            })?;
        let mut sum: Option<ID3D12Resource> = None;
        device.CreateCommittedResource(
            &D3D12_HEAP_PROPERTIES {
                Type: D3D12_HEAP_TYPE_DEFAULT,
                ..Default::default()
            },
            D3D12_HEAP_FLAG_NONE,
            &buffer_desc(256, D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS),
            D3D12_RESOURCE_STATE_COMMON,
            None,
            &mut sum,
        )?;
        Ok((root, pso, sum.ok_or_else(windows::core::Error::empty)?))
    }
}

fn buffer_desc(width: u64, flags: D3D12_RESOURCE_FLAGS) -> D3D12_RESOURCE_DESC {
    D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: width,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        Flags: flags,
        ..Default::default()
    }
}

impl Kit {
    /// One group counting `turns` on a queue of type `kind` (lowest priority).
    fn new(
        device: &ID3D12Device,
        kind: D3D12_COMMAND_LIST_TYPE,
        turns: u32,
    ) -> windows::core::Result<Self> {
        let (root, pso, sum) = counting(device)?;
        // SAFETY: plain object creation and recording of a list that only
        // writes the kit's own buffer.
        unsafe {
            let queue: ID3D12CommandQueue =
                device.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                    Type: kind,
                    Priority: D3D12_COMMAND_QUEUE_PRIORITY_NORMAL.0,
                    ..Default::default()
                })?;
            let allocator: ID3D12CommandAllocator = device.CreateCommandAllocator(kind)?;
            let list: ID3D12GraphicsCommandList =
                device.CreateCommandList(0, kind, &allocator, &pso)?;
            list.SetComputeRootSignature(&root);
            list.SetComputeRoot32BitConstant(0, turns, 0);
            list.SetComputeRootUnorderedAccessView(1, sum.GetGPUVirtualAddress());
            list.Dispatch(1, 1, 1);
            list.Close()?;
            let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE)?;
            let event = CreateEventW(None, false, false, None)?;
            Ok(Kit {
                device: device.clone(),
                queue,
                _root: root,
                _pso: pso,
                _sum: sum,
                _allocator: allocator,
                list: Some(list.cast()?),
                fence,
                event,
            })
        }
    }

    fn completed(&self) -> u64 {
        // SAFETY: a plain query of our fence.
        unsafe { self.fence.GetCompletedValue() }
    }

    /// Runs the list once more; the fence reaches `value` behind it.
    fn send(&self, value: u64) -> windows::core::Result<()> {
        // SAFETY: a closed list whose objects the kit holds; our fence.
        unsafe {
            self.queue
                .ExecuteCommandLists(std::slice::from_ref(&self.list));
            self.queue.Signal(&self.fence, value)
        }
    }

    /// Waits up to `ms` for the fence to reach `value`; an error once the
    /// device is removed.
    fn wait(&self, value: u64, ms: u32) -> windows::core::Result<()> {
        // SAFETY: our fence and event; a plain query.
        unsafe {
            self.fence.SetEventOnCompletion(value, self.event)?;
            WaitForSingleObject(self.event, ms);
            self.device.GetDeviceRemovedReason()
        }
    }

    /// Waits up to a second for `sent` runs to end; the objects are let go
    /// only if they have (or the device is removed).
    fn finish(self, sent: u64) {
        if self.completed() < sent {
            let _ = self.wait(sent, 1000);
        }
        if self.completed() < sent {
            tracing::warn!(
                "keep-alive dispatches still on the card after a second: their objects are kept"
            );
            std::mem::forget(self);
        }
    }
}

impl Drop for Kit {
    fn drop(&mut self) {
        // SAFETY: our event, closed once.
        let _ = unsafe { CloseHandle(self.event) };
    }
}

/// For the frame-rate line, after the card's clock (`idle` ms per frame
/// with nothing of ours queued; `ms` the frame's time).
pub(crate) fn line(frames: f64, ms: f64, idle: f64) {
    if !on() {
        return;
    }
    let [dispatches, covered] = [0, 1].map(|i| STATS[i].swap(0, Relaxed) as f64);
    let seconds = frames * ms / 1000.0;
    tracing::info!(
        "the card kept awake (VICTORY_CARD_AWAKE{}): {:.0} keep-alive dispatches/s ({:.1} per frame), covering {:.1} ms/frame \
         of the {idle:.1} ms/frame the card had nothing of ours queued",
        match mode() {
            Mode::ByTurns if turn_on() => "=turns, on now",
            Mode::ByTurns => "=turns, off now",
            _ => "=1",
        },
        if seconds > 0.0 {
            dispatches / seconds
        } else {
            0.0
        },
        dispatches / frames,
        covered / 1000.0 / frames,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    /// Makes [`mode`] say on, for the tests that run the thread.
    pub(super) static FORCED: AtomicBool = AtomicBool::new(false);

    #[test]
    fn the_switch_reads_one_and_turns() {
        assert_eq!(parse(None), Mode::Off);
        assert_eq!(parse(Some("0")), Mode::Off);
        assert_eq!(parse(Some("1")), Mode::On);
        assert_eq!(parse(Some("turns")), Mode::ByTurns);
        assert_eq!(parse(Some("yes")), Mode::Off);
    }

    /// The kit's list runs and its fence follows, on a compute queue.
    #[test]
    fn the_counting_list_runs_on_a_compute_queue() {
        let Some(host) = crate::host::host() else {
            return;
        };
        let kit = Kit::new(&host.device, D3D12_COMMAND_LIST_TYPE_COMPUTE, 1000).unwrap();
        for value in 1..=3 {
            kit.send(value).unwrap();
        }
        kit.wait(3, 5000).unwrap();
        assert_eq!(kit.completed(), 3);
        kit.finish(3);
    }

    /// Timestamps around `lists` workloads of `groups` groups counting
    /// `turns`, each list its own pair, on a direct queue.
    struct Work {
        queue: ID3D12CommandQueue,
        fence: ID3D12Fence,
        lists: Vec<Option<ID3D12CommandList>>,
        resolve: Option<ID3D12CommandList>,
        back: ID3D12Resource,
        _keep: (
            ID3D12RootSignature,
            ID3D12PipelineState,
            ID3D12Resource,
            ID3D12QueryHeap,
            Vec<ID3D12CommandAllocator>,
        ),
        sent: u64,
    }

    impl Work {
        fn new(device: &ID3D12Device, lists: u32, groups: u32, turns: u32) -> Self {
            let (root, pso, sum) = counting(device).unwrap();
            let kind = D3D12_COMMAND_LIST_TYPE_DIRECT;
            // SAFETY: plain object creation and recording; the lists write
            // the work's own buffer and stamps.
            unsafe {
                let queue: ID3D12CommandQueue = device
                    .CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                        Type: kind,
                        ..Default::default()
                    })
                    .unwrap();
                let mut heap: Option<ID3D12QueryHeap> = None;
                device
                    .CreateQueryHeap(
                        &D3D12_QUERY_HEAP_DESC {
                            Type: D3D12_QUERY_HEAP_TYPE_TIMESTAMP,
                            Count: lists * 2,
                            NodeMask: 0,
                        },
                        &mut heap,
                    )
                    .unwrap();
                let heap = heap.unwrap();
                let mut back: Option<ID3D12Resource> = None;
                device
                    .CreateCommittedResource(
                        &D3D12_HEAP_PROPERTIES {
                            Type: D3D12_HEAP_TYPE_READBACK,
                            ..Default::default()
                        },
                        D3D12_HEAP_FLAG_NONE,
                        &buffer_desc(lists as u64 * 16, D3D12_RESOURCE_FLAG_NONE),
                        D3D12_RESOURCE_STATE_COPY_DEST,
                        None,
                        &mut back,
                    )
                    .unwrap();
                let back = back.unwrap();
                let mut allocators = Vec::new();
                let mut made = Vec::new();
                for i in 0..=lists {
                    let allocator: ID3D12CommandAllocator =
                        device.CreateCommandAllocator(kind).unwrap();
                    let list: ID3D12GraphicsCommandList =
                        device.CreateCommandList(0, kind, &allocator, &pso).unwrap();
                    if i < lists {
                        list.EndQuery(&heap, D3D12_QUERY_TYPE_TIMESTAMP, i * 2);
                        list.SetComputeRootSignature(&root);
                        list.SetComputeRoot32BitConstant(0, turns, 0);
                        list.SetComputeRootUnorderedAccessView(1, sum.GetGPUVirtualAddress());
                        list.Dispatch(groups, 1, 1);
                        // The second stamp after the dispatch has run.
                        list.ResourceBarrier(&[D3D12_RESOURCE_BARRIER {
                            Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
                            ..Default::default()
                        }]);
                        list.EndQuery(&heap, D3D12_QUERY_TYPE_TIMESTAMP, i * 2 + 1);
                    } else {
                        list.ResolveQueryData(
                            &heap,
                            D3D12_QUERY_TYPE_TIMESTAMP,
                            0,
                            lists * 2,
                            &back,
                            0,
                        );
                    }
                    list.Close().unwrap();
                    made.push(Some(list.cast::<ID3D12CommandList>().unwrap()));
                    allocators.push(allocator);
                }
                let resolve = made.pop().unwrap();
                Work {
                    queue,
                    fence: device.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap(),
                    lists: made,
                    resolve,
                    back,
                    _keep: (root, pso, sum, heap, allocators),
                    sent: 0,
                }
            }
        }

        /// Sends `lists` in one go.
        fn send(&mut self, from: usize, to: usize) {
            self.sent += 1;
            // SAFETY: closed lists; our fence.
            unsafe {
                self.queue.ExecuteCommandLists(&self.lists[from..to]);
                self.queue.Signal(&self.fence, self.sent).unwrap();
            }
        }

        /// Spins until what was sent has run.
        fn done(&self) {
            // SAFETY: a plain query of our fence.
            while unsafe { self.fence.GetCompletedValue() } < self.sent {
                std::hint::spin_loop();
            }
        }

        fn run(&mut self, from: usize, to: usize) {
            self.send(from, to);
            self.done();
        }

        /// Milliseconds each workload took on the card, by its stamps.
        fn times(&mut self) -> Vec<f64> {
            self.sent += 1;
            // SAFETY: a closed list; our fence; the read-back buffer holds
            // two stamps per list once the resolve has run.
            unsafe {
                self.queue
                    .ExecuteCommandLists(std::slice::from_ref(&self.resolve));
                self.queue.Signal(&self.fence, self.sent).unwrap();
                while self.fence.GetCompletedValue() < self.sent {
                    std::hint::spin_loop();
                }
                let frequency = self.queue.GetTimestampFrequency().unwrap() as f64;
                let mut data = std::ptr::null_mut();
                self.back.Map(0, None, Some(&mut data)).unwrap();
                let stamps = std::slice::from_raw_parts(data.cast::<u64>(), self.lists.len() * 2);
                let times = stamps
                    .chunks(2)
                    .map(|p| p[1].saturating_sub(p[0]) as f64 * 1000.0 / frequency)
                    .collect();
                self.back.Unmap(0, Some(&D3D12_RANGE::default()));
                times
            }
        }
    }

    fn spin(d: Duration) {
        let t = Instant::now();
        while t.elapsed() < d {
            std::hint::spin_loop();
        }
    }

    fn summary(times: &[f64]) -> String {
        let mut sorted = times.to_vec();
        sorted.sort_by(f64::total_cmp);
        let mean = times.iter().sum::<f64>() / times.len() as f64;
        format!(
            "mean {mean:.3} ms, median {:.3}, max {:.3}",
            sorted[sorted.len() / 2],
            sorted[sorted.len() - 1]
        )
    }

    /// The bench for the switch (not the game; under the `gpu` lock): the
    /// same workloads sent in one go, one by one with gaps, and one by one
    /// with gaps while the card is kept awake through [`sent`] and [`idle`]
    /// as the host calls them. `cargo test -p victory-gpu --release
    /// card_awake_gapped_vs_awake -- --ignored --nocapture`; env
    /// `AWAKE_BENCH=lists,groups,turns,gap_us` (default 30,2048,600000,2000).
    #[test]
    #[ignore]
    fn card_awake_gapped_vs_awake() {
        let Some(host) = crate::host::host() else {
            return;
        };
        let device = &host.device;
        let params: Vec<u64> = std::env::var("AWAKE_BENCH")
            .unwrap_or_else(|_| "30,2048,600000,2000".into())
            .split(',')
            .map(|v| v.parse().unwrap())
            .collect();
        let [lists, groups, turns, gap_us] = [params[0], params[1], params[2], params[3]];
        let gap = Duration::from_micros(gap_us);
        let mut work = Work::new(device, lists as u32, groups as u32, turns as u32);
        let mut one = Work::new(device, 8, 1, TURNS);
        let n = lists as usize;
        let gapped = |work: &mut Work, awake: bool| {
            FORCED.store(awake, Relaxed);
            let started = Instant::now();
            for i in 0..n {
                spin(gap);
                work.send(i, i + 1);
                sent(device);
                work.done();
                idle();
            }
            let wall = started.elapsed().as_secs_f64() * 1000.0;
            sent(device);
            FORCED.store(false, Relaxed);
            let _ = STATS[1].swap(0, Relaxed);
            let dispatches = STATS[0].swap(0, Relaxed);
            (work.times(), wall, dispatches)
        };
        for round in 0..3 {
            spin(Duration::from_millis(300));
            work.run(0, n);
            let together = work.times();
            spin(Duration::from_millis(300));
            let (apart, apart_wall, _) = gapped(&mut work, false);
            spin(Duration::from_millis(300));
            let (awake, awake_wall, dispatches) = gapped(&mut work, true);
            println!(
                "round {round}: {lists} workloads of {groups}x64 threads x {turns} turns, gaps {gap_us} us\n  in one go:      sum {:.2} ms, {}\n  gapped:         sum {:.2} ms, {} (wall {apart_wall:.1} ms)\n  gapped + awake: sum {:.2} ms, {} (wall {awake_wall:.1} ms, {dispatches} keep-alive dispatches)",
                together.iter().sum::<f64>(),
                summary(&together),
                apart.iter().sum::<f64>(),
                summary(&apart),
                awake.iter().sum::<f64>(),
                summary(&awake),
            );
        }
        // One keep-alive dispatch's length, at full clocks right after work.
        work.run(0, n);
        one.run(0, 8);
        println!(
            "one keep-alive dispatch ({TURNS} turns): {}",
            summary(&one.times())
        );
        FORCED.store(true, Relaxed);
        stop();
        FORCED.store(false, Relaxed);
    }
}
