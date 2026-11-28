//! (El1xr) `VICTORY_BUDGET_ON_CARD=1` (restart, off by default): a list's
//! indirect budget words (`HostList::budget`: the work its clamps let run,
//! and what they note) live in the card's memory instead of the PC's.
//!
//! Every clamp of an indirect record (in place, ahead in the prelude, the
//! sanitizer's note) reads the list's budget word and swaps it with an
//! atomic compare-exchange; on the PC's memory each is a round trip over
//! PCIe, and the records of one prelude contend on the one word. On the
//! RTX 5090 (`bench_budget_on_the_card`): 19 preludes of 88 records took
//! 49-55 ms of the card with the budget in the PC's memory and 0.31-0.33 ms
//! with it on the card. In place (one record between two waits) the
//! timing runs had 290 a frame at 1.34 ms (4.6 us each).
//!
//! The words are a 64-byte slot of one card buffer per list (by the list's
//! own budget buffer, whose words stay the mirror the CPU reads): zeroed
//! when the buffer is made, and at the end of each run of a list that used
//! them a pass of ours ([`end_run`]) copies them to the mirror and zeroes
//! them, so the next run starts from zero as the CPU's zeroing made it, and
//! the CPU reads the same words it read before. A run that used the slot
//! and closed without that pass loses it: the list goes back to the PC's
//! words, counted on the frame line.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Mutex, OnceLock};

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;
use windows::core::s;

use super::{Clamp, host, make_from, uav_barrier};

/// Whether the budget is on the card (`VICTORY_BUDGET_ON_CARD=1`).
pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_BUDGET_ON_CARD").is_some_and(|v| v == "1"))
}

/// Bytes of one list's words (the clamp notes up to word 9).
pub(super) const SLOT: u64 = 64;
/// Slots of the card buffer (1 MiB).
pub(super) const SLOTS: u32 = 16_384;

/// The end-of-run pass: each thread moves one word to the mirror and
/// zeroes it on the card.
pub(super) const END: &str = r#"
#define RS "UAV(u0), UAV(u1)"
RWByteAddressBuffer card : register(u0);
RWByteAddressBuffer mirror : register(u1);
[RootSignature(RS)]
[numthreads(16, 1, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    uint at = id.x * 4;
    mirror.Store(at, card.Load(at));
    card.Store(at, 0u);
}
"#;

/// Where a list's slot stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum State {
    /// Zero on the card (made, or its last run ended with the pass).
    Clean,
    /// Used by the run being recorded.
    Used,
    /// A run used it and closed without the pass: back to the PC's words.
    Lost,
}

/// Slots by the GPU address of a list's own budget buffer.
#[derive(Default)]
pub(super) struct Slots {
    next: u32,
    of: HashMap<u64, (u32, State)>,
}

impl Slots {
    /// A run of the list whose budget buffer is at `mirror` uses its words:
    /// its slot (taking one if it has none), `None` when there is no slot
    /// left or the slot was lost.
    pub(super) fn begin(&mut self, mirror: u64) -> Option<u32> {
        if !self.of.contains_key(&mirror) {
            if self.next >= SLOTS {
                return None;
            }
            self.of.insert(mirror, (self.next, State::Clean));
            self.next += 1;
        }
        let e = self.of.get_mut(&mirror)?;
        e.1 = match e.1 {
            State::Clean => State::Used,
            State::Used | State::Lost => {
                if e.1 == State::Used {
                    COUNTS[1].fetch_add(1, Relaxed);
                }
                State::Lost
            }
        };
        (e.1 == State::Used).then_some(e.0)
    }

    /// The list's slot, unless it was lost (the prelude, recorded after
    /// the list's close, clamps into it too: it runs before the list).
    pub(super) fn used(&self, mirror: u64) -> Option<u32> {
        self.of.get(&mirror).filter(|e| e.1 != State::Lost).map(|e| e.0)
    }

    /// The run's end pass is recorded: the slot's words will be zero.
    pub(super) fn end(&mut self, mirror: u64) -> Option<u32> {
        let e = self.of.get_mut(&mirror).filter(|e| e.1 == State::Used)?;
        e.1 = State::Clean;
        Some(e.0)
    }
}

static SLOTS_OF: Mutex<Option<Slots>> = Mutex::new(None);

fn slots<R>(f: impl FnOnce(&mut Slots) -> R) -> R {
    f(SLOTS_OF
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_or_insert_with(Slots::default))
}

/// Since the last frame line: runs that used the card's words, slots lost
/// (a run closed without its end pass), and runs that found no slot.
static COUNTS: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

/// The card buffer of the slots (a committed buffer: zeroed when made).
fn buffer() -> Option<(&'static ID3D12Resource, u64)> {
    static BUFFER: OnceLock<Option<(ID3D12Resource, u64)>> = OnceLock::new();
    BUFFER
        .get_or_init(|| {
            let device = &host()?.device;
            let made = zeroed_buffer(device, u64::from(SLOTS) * SLOT);
            if made.is_none() {
                tracing::warn!("VICTORY_BUDGET_ON_CARD: no card buffer for the budgets; they stay in the PC's memory");
            }
            made
        })
        .as_ref()
        .map(|(b, gpu)| (b, *gpu))
}

/// A committed default-heap buffer of `size` bytes (zeroed by D3D12) that
/// shaders write, and its GPU address.
pub(super) fn zeroed_buffer(device: &ID3D12Device, size: u64) -> Option<(ID3D12Resource, u64)> {
    let desc = D3D12_RESOURCE_DESC {
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
        Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
        ..Default::default()
    };
    let mut buffer: Option<ID3D12Resource> = None;
    // SAFETY: plain creation of a device-local buffer.
    unsafe {
        device
            .CreateCommittedResource(
                &D3D12_HEAP_PROPERTIES {
                    Type: D3D12_HEAP_TYPE_DEFAULT,
                    ..Default::default()
                },
                D3D12_HEAP_FLAG_NONE,
                &desc,
                D3D12_RESOURCE_STATE_COMMON,
                None,
                &mut buffer,
            )
            .ok()?;
    }
    let buffer = buffer?;
    crate::host::name(&buffer, || "indirect budgets on the card".to_owned());
    // SAFETY: a live buffer.
    let gpu = unsafe { buffer.GetGPUVirtualAddress() };
    Some((buffer, gpu))
}

fn end_pass() -> Option<&'static Clamp> {
    static END_PASS: OnceLock<Option<Clamp>> = OnceLock::new();
    END_PASS
        .get_or_init(|| {
            let device = &host()?.device;
            make_from(device, END, s!("budget end"))
                .map_err(|e| tracing::warn!(error = %e, "VICTORY_BUDGET_ON_CARD: no end pass"))
                .ok()
        })
        .as_ref()
}

/// A run of the list whose own budget words are at `mirror` begins to use
/// them (`HostList::budget`'s first call in a run).
pub(crate) fn begin_run(mirror: u64) {
    if !on() || buffer().is_none() || end_pass().is_none() {
        return;
    }
    let got = slots(|s| s.begin(mirror));
    COUNTS[if got.is_some() { 0 } else { 2 }].fetch_add(1, Relaxed);
}

/// The GPU address the list's clamps use for its budget words at `mirror`:
/// its slot on the card while its run uses one.
pub(crate) fn card(mirror: u64) -> Option<u64> {
    if !on() {
        return None;
    }
    let (_, base) = buffer()?;
    slots(|s| s.used(mirror)).map(|slot| base + u64::from(slot) * SLOT)
}

/// Records, at the end of `list` (about to close), the move of the run's
/// words from the card to the mirror at `mirror`, and their zeroing.
pub(crate) fn end_run(list: &ID3D12GraphicsCommandList, mirror: u64) {
    if !on() {
        return;
    }
    let (Some((_, base)), Some(pass)) = (buffer(), end_pass()) else {
        return;
    };
    let Some(slot) = slots(|s| s.end(mirror)) else {
        return;
    };
    record_end(list, pass, base + u64::from(slot) * SLOT, mirror);
}

/// [`end_run`] with the pass `pass`, the words at `card`.
pub(super) fn record_end(list: &ID3D12GraphicsCommandList, pass: &Clamp, card: u64, mirror: u64) {
    // SAFETY: an open list; our root signature and pipeline; the card
    // buffer and the list's own budget buffer, live.
    unsafe {
        // Every clamp of the run first.
        list.ResourceBarrier(&[uav_barrier()]);
        list.SetComputeRootSignature(&pass.root);
        list.SetPipelineState(&pass.pso);
        list.SetComputeRootUnorderedAccessView(0, card);
        list.SetComputeRootUnorderedAccessView(1, mirror);
        list.Dispatch(1, 1, 1);
    }
}

/// The frame line, with the switch on.
pub(crate) fn line(frames: f64) {
    if !on() || frames <= 0.0 {
        return;
    }
    let [used, lost, none] = COUNTS
        .each_ref()
        .map(|c| c.swap(0, Relaxed) as f64 / frames);
    tracing::info!(
        "indirect budgets on the card per frame (VICTORY_BUDGET_ON_CARD=1): {used:.0} runs used the card's words, {lost:.2} slots lost (closed without the end pass), {none:.2} runs on the PC's words (no slot left, or lost)"
    );
}
