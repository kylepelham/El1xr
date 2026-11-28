//! The flicker catcher's read-back ([`super`], `VICTORY_FLICKER_CATCH`):
//! a compute pass in the present's list draws the presented texture
//! [`WIDE`] x [`HIGH`] into a buffer of ours, copied into a read-back
//! buffer; a thread of its own waits for the submission, judges the frame
//! and, on an event, logs [`super::report`] and saves the pictures.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Mutex, OnceLock, mpsc};

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::s;

use super::detect::{self, Detector, Event, HIGH, WIDE};
use crate::host::transition;

/// Bytes of a small picture (one `u32` a pixel).
const BYTES: u64 = (WIDE * HIGH * 4) as u64;
/// Read-back buffers at most (in flight and kept by the thread).
const BUFFERS: usize = 16;
/// Pictures the thread keeps for the PNGs of an event.
const KEPT: usize = 8;
/// Views of presented textures in the pass's heap, used round.
const VIEWS: u32 = 64;
/// Events whose pictures are saved, and events logged, at most.
const SAVED: u64 = 64;
const LOGGED: u64 = 400;
/// Frames between the lines of what was seen so far.
const TOLD_EVERY: u64 = 600;

const SHADER: &str = r#"
#define RS "RootConstants(num32BitConstants = 4, b0), UAV(u0), DescriptorTable(SRV(t0))"
cbuffer Params : register(b0) { uint width; uint height; uint srgb; uint unused; };
Texture2D<float4> shown : register(t0);
RWByteAddressBuffer picture : register(u0);

[RootSignature(RS)]
[numthreads(8, 8, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    if (id.x >= 640 || id.y >= 360) {
        return;
    }
    float3 sum = 0;
    for (uint s = 0; s < 4; s++) {
        uint px = min(((id.x * 2 + (s & 1)) * 2 + 1) * width / 2560, width - 1);
        uint py = min(((id.y * 2 + (s >> 1)) * 2 + 1) * height / 1440, height - 1);
        sum += saturate(shown.Load(int3(px, py, 0)).rgb);
    }
    float3 c = sum / 4;
    if (srgb != 0) {
        c = pow(c, 1.0 / 2.2);
    }
    uint3 b = (uint3)(c * 255.0 + 0.5);
    picture.Store((id.y * 640 + id.x) * 4, b.r | (b.g << 8) | (b.b << 16));
}
"#;

struct Readback(ID3D12Resource);
// SAFETY: D3D12 objects are free-threaded.
unsafe impl Send for Readback {}

/// Read-back buffers not in use.
static FREE: Mutex<Vec<Readback>> = Mutex::new(Vec::new());
/// Read-back buffers made.
static MADE: AtomicU64 = AtomicU64::new(0);
/// Presents not read back (no buffer free, a format not read).
static MISSED: AtomicU64 = AtomicU64::new(0);

struct Pass {
    root: ID3D12RootSignature,
    pipeline: ID3D12PipelineState,
    views: ID3D12DescriptorHeap,
    step: u32,
    next: u32,
    small: ID3D12Resource,
    /// Whether `small` was used: at rest it is a copy source.
    used: bool,
}

// SAFETY: D3D12 objects are free-threaded; the pass is used under a lock.
unsafe impl Send for Pass {}

impl Pass {
    fn new(device: &ID3D12Device) -> windows::core::Result<Pass> {
        let code = crate::host::compile(SHADER, s!("main"), s!("cs_5_1"))?;
        // SAFETY: plain object creation on a live device.
        unsafe {
            let root: ID3D12RootSignature = device.CreateRootSignature(0, &code[..])?;
            let pipeline =
                device.CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                    pRootSignature: std::mem::transmute_copy(&root),
                    CS: D3D12_SHADER_BYTECODE {
                        pShaderBytecode: code.as_ptr().cast(),
                        BytecodeLength: code.len(),
                    },
                    ..Default::default()
                })?;
            let views: ID3D12DescriptorHeap =
                device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                    NumDescriptors: VIEWS,
                    Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                    NodeMask: 0,
                })?;
            let step =
                device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV);
            let small = buffer(
                device,
                D3D12_HEAP_TYPE_DEFAULT,
                D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
                D3D12_RESOURCE_STATE_COMMON,
            )?;
            Ok(Pass {
                root,
                pipeline,
                views,
                step,
                next: 0,
                small,
                used: false,
            })
        }
    }

    /// Records the small picture of `texture` (readable by compute) and its
    /// copy into `into`.
    fn record(
        &mut self,
        device: &ID3D12Device,
        list: &ID3D12GraphicsCommandList,
        texture: &ID3D12Resource,
        desc: &D3D12_RESOURCE_DESC,
        format: DXGI_FORMAT,
        into: &ID3D12Resource,
    ) {
        let slot = self.next % VIEWS;
        self.next = self.next.wrapping_add(1);
        let srgb = u32::from(matches!(desc.Format.0, 29 | 91));
        // SAFETY: an open direct list; live resources; our heap and pipeline.
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
                &self.small,
                rest,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
            )]);
            list.SetComputeRootSignature(&self.root);
            list.SetDescriptorHeaps(&[Some(self.views.clone())]);
            let constants = [desc.Width as u32, desc.Height, srgb, 0];
            list.SetComputeRoot32BitConstants(0, 4, constants.as_ptr().cast(), 0);
            list.SetComputeRootUnorderedAccessView(1, self.small.GetGPUVirtualAddress());
            list.SetComputeRootDescriptorTable(2, gpu);
            list.SetPipelineState(&self.pipeline);
            list.Dispatch((WIDE as u32).div_ceil(8), (HIGH as u32).div_ceil(8), 1);
            list.ResourceBarrier(&[transition(
                &self.small,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                D3D12_RESOURCE_STATE_COPY_SOURCE,
            )]);
            list.CopyBufferRegion(into, 0, &self.small, 0, BYTES);
        }
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
        Width: BYTES,
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

/// The pass, made on first use (`None` inside: it could not be).
static PASS: Mutex<Option<Option<Pass>>> = Mutex::new(None);

/// A frame's picture on its way back.
pub(crate) struct Taken {
    buffer: Readback,
    frame: u64,
}

/// At a present of frame `frame`: closes the frame's counts and, while
/// frames are caught, records into `list` (the present's, `texture` in
/// `crate::hold_frame::READ`) the read-back of its small picture.
pub(crate) fn take(
    device: &ID3D12Device,
    list: &ID3D12GraphicsCommandList,
    texture: &ID3D12Resource,
    frame: u64,
) -> Option<Taken> {
    if !super::on() {
        return None;
    }
    super::frame_done(frame);
    // SAFETY: a live texture.
    let desc = unsafe { texture.GetDesc() };
    let Some(format) = crate::hold_frame::view_format(desc.Format).filter(|_| {
        desc.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE2D
            && desc.SampleDesc.Count == 1
            && !desc
                .Flags
                .contains(D3D12_RESOURCE_FLAG_DENY_SHADER_RESOURCE)
    }) else {
        MISSED.fetch_add(1, Relaxed);
        return None;
    };
    let mut pass = PASS.lock().unwrap_or_else(|e| e.into_inner());
    let pass = pass
        .get_or_insert_with(|| match Pass::new(device) {
            Ok(pass) => {
                tracing::info!("flicker catch: presented frames are drawn small, read back and judged (VICTORY_FLICKER_CATCH)");
                Some(pass)
            }
            Err(e) => {
                tracing::warn!(error = %e, "flicker catch: no pass, frames not judged (VICTORY_FLICKER_CATCH)");
                None
            }
        })
        .as_mut()?;
    let free = FREE.lock().unwrap_or_else(|e| e.into_inner()).pop();
    let buffer = match free {
        Some(b) => b,
        None if MADE.load(Relaxed) < BUFFERS as u64 => {
            let made = buffer(
                device,
                D3D12_HEAP_TYPE_READBACK,
                D3D12_RESOURCE_FLAG_NONE,
                D3D12_RESOURCE_STATE_COPY_DEST,
            )
            .ok()?;
            MADE.fetch_add(1, Relaxed);
            Readback(made)
        }
        None => {
            MISSED.fetch_add(1, Relaxed);
            return None;
        }
    };
    pass.record(device, list, texture, &desc, format, &buffer.0);
    Some(Taken { buffer, frame })
}

struct Job {
    taken: Taken,
    /// The submission that copies it (0: it has run).
    flown: u64,
}

/// The list with `taken` went to the card as submission `flown` (0: it has
/// run).
pub(crate) fn sent(taken: Taken, flown: u64) {
    static THREAD: OnceLock<Option<mpsc::Sender<Job>>> = OnceLock::new();
    let Some(to) = THREAD.get_or_init(|| {
        let (to, from) = mpsc::channel::<Job>();
        std::thread::Builder::new()
            .name("victory flicker catch".into())
            .spawn(move || watch(&from))
            .ok()
            .map(|_| to)
    }) else {
        FREE.lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(taken.buffer);
        return;
    };
    if let Err(mpsc::SendError(job)) = to.send(Job { taken, flown }) {
        FREE.lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(job.taken.buffer);
    }
}

/// The small picture in a read-back buffer the card has written.
fn pixels_of(buffer: &ID3D12Resource) -> Option<Vec<u32>> {
    let mut at = std::ptr::null_mut();
    let range = D3D12_RANGE {
        Begin: 0,
        End: BYTES as usize,
    };
    // SAFETY: a read-back buffer the card has finished writing.
    unsafe { buffer.Map(0, Some(&range), Some(&mut at)) }.ok()?;
    let mut out = vec![0u32; WIDE * HIGH];
    // SAFETY: the mapped buffer holds `BYTES` bytes.
    unsafe { std::ptr::copy_nonoverlapping(at.cast::<u32>(), out.as_mut_ptr(), WIDE * HIGH) };
    // SAFETY: mapped above; nothing written.
    unsafe { buffer.Unmap(0, Some(&D3D12_RANGE { Begin: 0, End: 0 })) };
    Some(out)
}

/// The snap folder (`VICTORY_SNAP_DIR`, default `snaps`).
fn folder() -> Option<&'static PathBuf> {
    static FOLDER: OnceLock<Option<PathBuf>> = OnceLock::new();
    FOLDER
        .get_or_init(|| {
            let folder = std::env::var_os("VICTORY_SNAP_DIR")
                .map_or_else(|| PathBuf::from("snaps"), PathBuf::from);
            std::fs::create_dir_all(&folder).ok()?;
            Some(folder)
        })
        .as_ref()
}

/// Saves the kept pictures of frames `frames` (with their roles) as PNGs:
/// the names written.
fn save(kept: &VecDeque<(u64, Vec<u32>)>, event_frame: u64, frames: &[(u64, &str)]) -> Vec<String> {
    let Some(folder) = folder() else {
        return Vec::new();
    };
    frames
        .iter()
        .filter_map(|&(frame, role)| {
            let (_, pixels) = kept.iter().find(|(f, _)| *f == frame)?;
            let png =
                crate::screenshot::encode_png(WIDE as u32, HIGH as u32, &detect::rgb_of(pixels));
            let name = format!("flicker-f{event_frame}-{role}-f{frame}.png");
            std::fs::write(folder.join(&name), png).ok()?;
            Some(name)
        })
        .collect()
}

fn watch(from: &mpsc::Receiver<Job>) {
    let mut detector = Detector::default();
    let mut kept: VecDeque<(u64, Vec<u32>)> = VecDeque::new();
    let (mut judged, mut flickers, mut blacks, mut logged, mut saved) =
        (0u64, 0u64, 0u64, 0u64, 0u64);
    let mut regions = 0u64;
    while let Ok(job) = from.recv() {
        while crate::host::submissions_finished() < job.flown {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let pixels = pixels_of(&job.taken.buffer.0);
        FREE.lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(job.taken.buffer);
        let Some(pixels) = pixels else {
            continue;
        };
        let frame = job.taken.frame;
        detector.region = super::region::tiles();
        let events = detector.push(frame, detect::tiles_of(&pixels));
        kept.push_back((frame, pixels));
        while kept.len() > KEPT {
            kept.pop_front();
        }
        judged += 1;
        for event in events {
            let (first, back) = match event {
                Event::Flicker { frame, frames, .. } => {
                    flickers += 1;
                    (frame, frame + u64::from(frames))
                }
                Event::Black { frame, frames } => {
                    blacks += 1;
                    (frame, frame + u64::from(frames))
                }
                Event::Region { frame, .. } => {
                    regions += 1;
                    (frame, frame + 1)
                }
            };
            if logged >= LOGGED {
                continue;
            }
            logged += 1;
            let pngs = if saved < SAVED {
                saved += 1;
                save(
                    &kept,
                    first,
                    &[(first - 1, "before"), (first, "dark"), (back, "after")],
                )
            } else {
                Vec::new()
            };
            tracing::info!("{}; pngs=[{}]", super::report(&event), pngs.join(" "));
        }
        if judged % TOLD_EVERY == 0 {
            tracing::info!(
                "flicker catch, since the start: {judged} frames judged, {flickers} flickers, {blacks} black stretches, {regions} regions, {} presents not read back",
                MISSED.load(Relaxed)
            );
        }
    }
}

#[cfg(test)]
mod tests;
