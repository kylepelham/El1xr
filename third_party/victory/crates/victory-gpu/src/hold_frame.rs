//! (racefix, issue #57) A present whose texture is all black shows the last
//! frame that was not, for at most a run of frames
//! (`VICTORY_HOLD_LAST_FRAME=1`: at most 150 in a row; `N` from 2: at most
//! `N`; off unless set; live).
//!
//! The present audit saw whole black presents on the boot and loading
//! screens and one at world entry (batch 4: 77 in the first 90 s). They are
//! the title's own texture, drawn and presented in order on our one queue,
//! so nothing of ours is read early; the title shows black there. This
//! shows the last good frame again instead, decided on the card in the
//! present's own list, so nothing waits and nothing is a frame late:
//! - `begin` clears the frame's "lit" word;
//! - `scan` reads every texel of the presented texture and sets "lit" if
//!   any colour (alpha left out) is not zero;
//! - `decide` sets "hold" when the frame is black, a good frame is kept and
//!   fewer than the most frames in a row were held, and counts;
//! - the copy into the back buffer is predicated on "hold": the presented
//!   texture when 0, the kept frame when 1; a lit frame is copied into the
//!   kept one (predicated on "lit").
//!
//! A black run longer than the most is shown black from then on, and a
//! black frame with no good frame before it (boot) is shown as it is. Not
//! with a second plane (its UI over a black game plane would sit over a
//! stale one).

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::Mutex;

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::s;

use crate::host::transition;

/// The most frames held in a row with `VICTORY_HOLD_LAST_FRAME=1`: the
/// longest black run of batch 4's loading screens was 44 frames.
const MOST_DEFAULT: u32 = 150;
/// Views of presented textures in the pass's heap, used round.
const VIEWS: u32 = 64;
/// Read-backs of the counters in flight.
const RING: usize = 8;
/// Bytes of the state buffer: lit (u64), hold (u64), kept, run, held,
/// lit frames, black shown.
const STATE_BYTES: u64 = 64;
const AT_LIT: u64 = 0;
const AT_HOLD: u64 = 8;
const AT_COUNTS: u64 = 24;
/// The scan's group side.
const SIDE: u32 = 16;

const SHADER: &str = r#"
#define RS "RootConstants(num32BitConstants = 4, b0), UAV(u0), DescriptorTable(SRV(t0))"
cbuffer Params : register(b0) { uint width; uint height; uint most; uint fresh; };
Texture2D<float4> shown : register(t0);
RWByteAddressBuffer state : register(u0);
groupshared uint group_lit;

[RootSignature(RS)]
[numthreads(1, 1, 1)]
void begin() {
    state.Store2(0, uint2(0, 0));
    if (fresh != 0) {
        state.Store2(16, uint2(0, 0));
    }
}

[RootSignature(RS)]
[numthreads(16, 16, 1)]
void scan(uint3 id : SV_DispatchThreadID, uint index : SV_GroupIndex) {
    if (index == 0) {
        group_lit = 0;
    }
    GroupMemoryBarrierWithGroupSync();
    if (id.x < width && id.y < height && any(shown.Load(int3(id.xy, 0)).rgb != 0.0)) {
        group_lit = 1;
    }
    GroupMemoryBarrierWithGroupSync();
    if (index == 0 && group_lit != 0) {
        state.Store(0, 1);
    }
}

[RootSignature(RS)]
[numthreads(1, 1, 1)]
void decide() {
    uint lit = state.Load(0);
    uint kept = state.Load(16);
    uint run = state.Load(20);
    uint hold = 0;
    if (lit != 0) {
        kept = 1;
        run = 0;
        state.Store(28, state.Load(28) + 1);
    } else if (kept != 0 && run < most) {
        hold = 1;
        run += 1;
        state.Store(24, state.Load(24) + 1);
    } else {
        state.Store(32, state.Load(32) + 1);
    }
    state.Store2(8, uint2(hold, 0));
    state.Store2(16, uint2(kept, run));
}
"#;

/// Live: a change applies from the next present; a frame kept before a
/// present the pass did not see is forgotten.
static MOST: victory_core::settings::Live =
    victory_core::settings::Live::new("VICTORY_HOLD_LAST_FRAME", |v| u64::from(parse(v)));

/// The most frames held in a row; 0: off.
pub(crate) fn most() -> u32 {
    static LOGGED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let most = MOST.get() as u32;
    if LOGGED.swap(most, Relaxed) != most && most > 0 {
        tracing::info!(
            most,
            "a present whose texture is all black shows the last good frame again, at most this many in a row (VICTORY_HOLD_LAST_FRAME)"
        );
    }
    most
}

fn parse(value: Option<&str>) -> u32 {
    match value.map(str::trim).map(str::parse::<u32>) {
        Some(Ok(1)) => MOST_DEFAULT,
        Some(Ok(n)) => n,
        _ => 0,
    }
}

/// The state the presented texture is read in while the switch is on: a
/// copy source and read by the scan.
pub(crate) const READ: D3D12_RESOURCE_STATES = D3D12_RESOURCE_STATES(
    D3D12_RESOURCE_STATE_COPY_SOURCE.0 | D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE.0,
);

/// The format a texture of `format` is read through; `None` for formats
/// the scan cannot read as floats.
pub(crate) fn view_format(format: DXGI_FORMAT) -> Option<DXGI_FORMAT> {
    Some(match format.0 {
        9 => DXGI_FORMAT_R16G16B16A16_FLOAT,
        10 | 11 | 13 | 24 | 28 | 29 | 31 | 87 | 91 => format,
        23 => DXGI_FORMAT_R10G10B10A2_UNORM,
        27 => DXGI_FORMAT_R8G8B8A8_UNORM,
        90 => DXGI_FORMAT_B8G8R8A8_UNORM,
        _ => return None,
    })
}

/// A read-back of the counters: free, recorded in a list not sent yet, or
/// sent as submission `n` (0: it ran and was waited for).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Slot {
    Free,
    Recorded,
    Flown(u64),
}

/// The pass: its pipelines, heap, state on the card, the kept frame and
/// the counters' read-backs.
pub(crate) struct Hold {
    most: u32,
    root: ID3D12RootSignature,
    begin: ID3D12PipelineState,
    scan: ID3D12PipelineState,
    decide: ID3D12PipelineState,
    views: ID3D12DescriptorHeap,
    step: u32,
    next: u32,
    state: ID3D12Resource,
    /// Whether `state` was used: at rest it is a copy source.
    used: bool,
    /// The last good frame and the description it was made for.
    kept: Option<(ID3D12Resource, D3D12_RESOURCE_DESC)>,
    ring: Vec<(ID3D12Resource, Slot)>,
    /// The counters last read: held, lit, black shown.
    counts: [u32; 3],
    /// The kept frame is not the last present's (the pass missed one).
    stale: bool,
}

// SAFETY: D3D12 objects are free-threaded; the pass is used under a lock.
unsafe impl Send for Hold {}

impl Hold {
    pub(crate) fn new(device: &ID3D12Device, most: u32) -> windows::core::Result<Hold> {
        let code = |entry| crate::host::compile(SHADER, entry, s!("cs_5_1"));
        let begin = code(s!("begin"))?;
        let scan = code(s!("scan"))?;
        let decide = code(s!("decide"))?;
        // SAFETY: plain object creation on a live device.
        unsafe {
            let root: ID3D12RootSignature = device.CreateRootSignature(0, &begin[..])?;
            let pipeline = |code: &[u8]| -> windows::core::Result<ID3D12PipelineState> {
                device.CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                    pRootSignature: std::mem::transmute_copy(&root),
                    CS: D3D12_SHADER_BYTECODE {
                        pShaderBytecode: code.as_ptr().cast(),
                        BytecodeLength: code.len(),
                    },
                    ..Default::default()
                })
            };
            let (begin, scan, decide) = (pipeline(&begin)?, pipeline(&scan)?, pipeline(&decide)?);
            let views: ID3D12DescriptorHeap =
                device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                    NumDescriptors: VIEWS,
                    Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                    NodeMask: 0,
                })?;
            let step =
                device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV);
            let state = buffer(
                device,
                D3D12_HEAP_TYPE_DEFAULT,
                D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
                D3D12_RESOURCE_STATE_COMMON,
            )?;
            let mut ring = Vec::with_capacity(RING);
            for _ in 0..RING {
                ring.push((
                    buffer(
                        device,
                        D3D12_HEAP_TYPE_READBACK,
                        D3D12_RESOURCE_FLAG_NONE,
                        D3D12_RESOURCE_STATE_COPY_DEST,
                    )?,
                    Slot::Free,
                ));
            }
            Ok(Hold {
                most,
                root,
                begin,
                scan,
                decide,
                views,
                step,
                next: 0,
                state,
                used: false,
                kept: None,
                ring,
                counts: [0; 3],
                stale: false,
            })
        }
    }

    /// Records into `list` the present of `texture` (in [`READ`]) into
    /// `back` (a copy destination, left one): the texture, or the kept frame
    /// if the texture is black. `finished`: the submissions finished, for
    /// the read-backs. `None` if not recorded (a texture the pass cannot
    /// read): the caller copies as before. Else the read-back slot used, if
    /// any, for [`Hold::sent`].
    pub(crate) fn record(
        &mut self,
        device: &ID3D12Device,
        list: &ID3D12GraphicsCommandList,
        texture: &ID3D12Resource,
        back: &ID3D12Resource,
        finished: u64,
    ) -> Option<Option<usize>> {
        // SAFETY: a live texture.
        let desc = unsafe { texture.GetDesc() };
        let format = view_format(desc.Format)?;
        if desc.Dimension != D3D12_RESOURCE_DIMENSION_TEXTURE2D
            || desc.SampleDesc.Count != 1
            || desc.DepthOrArraySize != 1
            || desc
                .Flags
                .contains(D3D12_RESOURCE_FLAG_DENY_SHADER_RESOURCE)
        {
            return None;
        }
        let fresh = self.keep_for(device, &desc)?;
        self.harvest(finished);
        let (width, height) = (desc.Width as u32, desc.Height);
        let kept = &self.kept.as_ref()?.0;
        let slot = self.next % VIEWS;
        self.next = self.next.wrapping_add(1);
        // SAFETY: an open direct list; live resources; our heap and pipelines.
        unsafe {
            let cpu = D3D12_CPU_DESCRIPTOR_HANDLE {
                ptr: self.views.GetCPUDescriptorHandleForHeapStart().ptr
                    + (slot * self.step) as usize,
            };
            let gpu = D3D12_GPU_DESCRIPTOR_HANDLE {
                ptr: self.views.GetGPUDescriptorHandleForHeapStart().ptr
                    + u64::from(slot * self.step),
            };
            device.CreateShaderResourceView(
                texture,
                Some(&D3D12_SHADER_RESOURCE_VIEW_DESC {
                    Format: format,
                    ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2D,
                    Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
                    Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                        Texture2D: D3D12_TEX2D_SRV {
                            MostDetailedMip: 0,
                            MipLevels: 1,
                            ..Default::default()
                        },
                    },
                }),
                cpu,
            );
            let rest = if self.used {
                D3D12_RESOURCE_STATE_COPY_SOURCE
            } else {
                D3D12_RESOURCE_STATE_COMMON
            };
            self.used = true;
            list.ResourceBarrier(&[transition(
                &self.state,
                rest,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
            )]);
            list.SetComputeRootSignature(&self.root);
            list.SetDescriptorHeaps(&[Some(self.views.clone())]);
            let constants = [width, height, self.most, u32::from(fresh)];
            list.SetComputeRoot32BitConstants(0, 4, constants.as_ptr().cast(), 0);
            list.SetComputeRootUnorderedAccessView(1, self.state.GetGPUVirtualAddress());
            list.SetComputeRootDescriptorTable(2, gpu);
            list.SetPipelineState(&self.begin);
            list.Dispatch(1, 1, 1);
            uav_barrier(list);
            list.SetPipelineState(&self.scan);
            list.Dispatch(width.div_ceil(SIDE), height.div_ceil(SIDE), 1);
            uav_barrier(list);
            list.SetPipelineState(&self.decide);
            list.Dispatch(1, 1, 1);
            list.ResourceBarrier(&[
                transition(
                    &self.state,
                    D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                    D3D12_RESOURCE_STATE_PREDICATION,
                ),
                transition(
                    kept,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                    D3D12_RESOURCE_STATE_COPY_SOURCE,
                ),
            ]);
            // Skipped when "hold" is not 0, and when it is 0.
            list.SetPredication(&self.state, AT_HOLD, D3D12_PREDICATION_OP_NOT_EQUAL_ZERO);
            list.CopyResource(back, texture);
            list.SetPredication(&self.state, AT_HOLD, D3D12_PREDICATION_OP_EQUAL_ZERO);
            list.CopyResource(back, kept);
            list.SetPredication(None, 0, D3D12_PREDICATION_OP_EQUAL_ZERO);
            list.ResourceBarrier(&[transition(
                kept,
                D3D12_RESOURCE_STATE_COPY_SOURCE,
                D3D12_RESOURCE_STATE_COPY_DEST,
            )]);
            // A lit frame is kept.
            list.SetPredication(&self.state, AT_LIT, D3D12_PREDICATION_OP_EQUAL_ZERO);
            list.CopyResource(kept, texture);
            list.SetPredication(None, 0, D3D12_PREDICATION_OP_EQUAL_ZERO);
            list.ResourceBarrier(&[transition(
                &self.state,
                D3D12_RESOURCE_STATE_PREDICATION,
                D3D12_RESOURCE_STATE_COPY_SOURCE,
            )]);
            let read = self.ring.iter().position(|(_, s)| *s == Slot::Free);
            if let Some(i) = read {
                list.CopyBufferRegion(&self.ring[i].0, 0, &self.state, AT_COUNTS, 12);
                self.ring[i].1 = Slot::Recorded;
            }
            Some(read)
        }
    }

    /// The list with read-back `slot` went to the card as submission
    /// `flown` (0: it ran and was waited for).
    pub(crate) fn sent(&mut self, slot: Option<usize>, flown: u64) {
        if let Some((_, s)) = slot.and_then(|i| self.ring.get_mut(i)) {
            *s = Slot::Flown(flown);
        }
    }

    /// Reads the counters of every read-back the card has run.
    fn harvest(&mut self, finished: u64) {
        for (buffer, slot) in &mut self.ring {
            let Slot::Flown(flown) = *slot else {
                continue;
            };
            if flown > finished {
                continue;
            }
            *slot = Slot::Free;
            if let Some(counts) = read(buffer) {
                // Cumulative on the card: the latest read is the largest.
                for (c, n) in self.counts.iter_mut().zip(counts) {
                    *c = (*c).max(n);
                }
            }
        }
    }

    /// The counters last read: frames held, lit, black shown.
    pub(crate) fn counts(&self) -> [u32; 3] {
        self.counts
    }

    /// The kept frame for textures of `desc`, made again if the size or
    /// format changed: true if it is new (nothing kept yet).
    fn keep_for(&mut self, device: &ID3D12Device, desc: &D3D12_RESOURCE_DESC) -> Option<bool> {
        let same = |k: &D3D12_RESOURCE_DESC| {
            (k.Width, k.Height, k.Format, k.MipLevels)
                == (desc.Width, desc.Height, desc.Format, desc.MipLevels)
        };
        if self.kept.as_ref().is_some_and(|(_, k)| same(k)) {
            return Some(std::mem::take(&mut self.stale));
        }
        self.stale = false;
        let mut made = *desc;
        made.Flags = D3D12_RESOURCE_FLAG_NONE;
        made.Layout = D3D12_TEXTURE_LAYOUT_UNKNOWN;
        made.Alignment = 0;
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_DEFAULT,
            ..Default::default()
        };
        let mut texture: Option<ID3D12Resource> = None;
        // SAFETY: a committed texture of our own.
        unsafe {
            device
                .CreateCommittedResource(
                    &heap,
                    D3D12_HEAP_FLAG_NONE,
                    &made,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                    None,
                    &mut texture,
                )
                .ok()?;
        }
        self.kept = Some((texture?, *desc));
        Some(true)
    }
}

fn buffer(
    device: &ID3D12Device,
    kind: D3D12_HEAP_TYPE,
    flags: D3D12_RESOURCE_FLAGS,
    state: D3D12_RESOURCE_STATES,
) -> windows::core::Result<ID3D12Resource> {
    let heap = D3D12_HEAP_PROPERTIES {
        Type: kind,
        ..Default::default()
    };
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: STATE_BYTES,
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
    };
    let mut made: Option<ID3D12Resource> = None;
    // SAFETY: a committed buffer of our own.
    unsafe {
        device.CreateCommittedResource(
            &heap,
            D3D12_HEAP_FLAG_NONE,
            &desc,
            state,
            None,
            &mut made,
        )?
    };
    made.ok_or_else(windows::core::Error::empty)
}

fn uav_barrier(list: &ID3D12GraphicsCommandList) {
    let barrier = D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
        ..Default::default()
    };
    // SAFETY: an open list; a barrier on all unordered access.
    unsafe { list.ResourceBarrier(&[barrier]) };
}

/// The three counters of a read-back the card has run.
fn read(buffer: &ID3D12Resource) -> Option<[u32; 3]> {
    let mut at = std::ptr::null_mut();
    let range = D3D12_RANGE { Begin: 0, End: 12 };
    // SAFETY: a read-back buffer the card has finished writing.
    unsafe { buffer.Map(0, Some(&range), Some(&mut at)) }.ok()?;
    let mut counts = [0u32; 3];
    // SAFETY: the mapped buffer holds at least 12 bytes.
    unsafe { std::ptr::copy_nonoverlapping(at.cast::<u32>(), counts.as_mut_ptr(), 3) };
    // SAFETY: mapped above; nothing written.
    unsafe { buffer.Unmap(0, Some(&D3D12_RANGE { Begin: 0, End: 0 })) };
    Some(counts)
}

/// The game's pass, made on first use (`None` inside: it could not be).
static PASS: Mutex<Option<Option<Hold>>> = Mutex::new(None);

/// [`Hold::record`] with the game's pass: `None` if the switch is off or
/// the pass cannot present this texture.
pub(crate) fn record(
    device: &ID3D12Device,
    list: &ID3D12GraphicsCommandList,
    texture: &ID3D12Resource,
    back: &ID3D12Resource,
) -> Option<Option<usize>> {
    let most = most();
    if most == 0 {
        return None;
    }
    let mut pass = PASS.lock().unwrap_or_else(|e| e.into_inner());
    let hold = pass
        .get_or_insert_with(|| match Hold::new(device, most) {
            Ok(hold) => Some(hold),
            Err(e) => {
                tracing::warn!(error = %e, "no pass to hold the last good frame: presents go as before (VICTORY_HOLD_LAST_FRAME)");
                None
            }
        })
        .as_mut()?;
    hold.most = most;
    // A present went by without the pass (off, or a second plane).
    static LAST: AtomicU64 = AtomicU64::new(0);
    let now = crate::queue::presented();
    if LAST.swap(now, Relaxed) + 1 != now {
        hold.stale = true;
    }
    let before = hold.counts();
    let recorded = hold.record(
        device,
        list,
        texture,
        back,
        crate::host::submissions_finished(),
    );
    note(before, hold.counts());
    recorded
}

/// [`Hold::sent`] with the game's pass.
pub(crate) fn sent(slot: Option<usize>, flown: u64) {
    if let Some(Some(hold)) = PASS.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        hold.sent(slot, flown);
    }
}

/// Logs frames held since the last read (the first 64 times), and the
/// counters every 300 presents.
fn note(before: [u32; 3], now: [u32; 3]) {
    static LINES: AtomicU64 = AtomicU64::new(0);
    static PRESENTS: AtomicU64 = AtomicU64::new(0);
    let [held, lit, black] = now;
    if held > before[0] && LINES.fetch_add(1, Relaxed) < 64 {
        tracing::info!(
            held_now = held - before[0],
            held,
            lit,
            black_shown = black,
            "hold last frame: black presents showed the last good frame (VICTORY_HOLD_LAST_FRAME)"
        );
    }
    if PRESENTS.fetch_add(1, Relaxed) % 300 == 299 {
        tracing::info!(
            held,
            lit,
            black_shown = black,
            "hold last frame: presents so far (held: the last good frame shown again; black_shown: black with nothing to hold or past the most in a row)"
        );
    }
}

#[cfg(test)]
mod tests;
