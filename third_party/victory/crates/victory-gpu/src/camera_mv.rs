//! `VICTORY_CAMERA_MV_CHECK=1` (off by default; needs
//! `VICTORY_CAMERA_CAPTURE=1`): checks the title's motion vectors against
//! the camera live. A few times a second, right after a velocity pass, 64
//! pixels of its depth and motion vectors (an 8x8 grid) are copied into a
//! read-back buffer of a ring; a later frame reads the slot once its
//! submission has finished (nothing waits) and counts the pixels whose
//! motion the camera's clip-to-previous-clip predicts
//! (`crate::camera::motion_agreement`). Logged every 3 s:
//! `camera: mv agreement X% (n=..)`.
//!
//! A depth texture with the depth-stencil flag can only be copied whole: it
//! is copied into a plain texture of ours first (one full copy), and the
//! pixels from there.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;

use crate::camera::{Mat4, MotionSample};
use crate::camera_copy::{COPY_DEST, COPY_SOURCE, move_to};
use crate::host::HostList;
use crate::object::This;
use crate::resource::HostTexture;

/// `VICTORY_CAMERA_MV_CHECK=1`.
pub fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        crate::camera::on() && std::env::var_os("VICTORY_CAMERA_MV_CHECK").is_some_and(|v| v == "1")
    })
}

/// Pixels a side of the grid.
pub const GRID: usize = 8;
pub const SAMPLES: usize = GRID * GRID;
/// Bytes between two pixels in the read-back buffer (a placed footprint's
/// alignment).
const PLACEMENT: u64 = D3D12_TEXTURE_DATA_PLACEMENT_ALIGNMENT as u64;
const RING: usize = 4;
/// Between two samples.
const EVERY_MS: u64 = 250;
/// Within so many pixels the motion agrees.
pub const TOLERANCE_PX: f32 = 1.0;

/// The grid's pixels in a `size` texture: cell centres.
pub fn grid(size: [u32; 2]) -> Vec<[u32; 2]> {
    let mut out = Vec::with_capacity(SAMPLES);
    for j in 0..GRID as u32 {
        for i in 0..GRID as u32 {
            let x = ((2 * i + 1) * size[0]) / (2 * GRID as u32);
            let y = ((2 * j + 1) * size[1]) / (2 * GRID as u32);
            out.push([
                x.min(size[0].saturating_sub(1)),
                y.min(size[1].saturating_sub(1)),
            ]);
        }
    }
    out
}

fn half(bits: u16) -> f32 {
    let sign = if bits & 0x8000 != 0 { -1.0 } else { 1.0 };
    let exp = i32::from((bits >> 10) & 0x1f);
    let man = f32::from(bits & 0x3ff);
    match exp {
        0 => sign * man * 2f32.powi(-24),
        31 => {
            if man == 0.0 {
                sign * f32::INFINITY
            } else {
                f32::NAN
            }
        }
        _ => sign * (1.0 + man / 1024.0) * 2f32.powi(exp - 15),
    }
}

/// A depth texel of footprint format `format` (0..1), if read.
pub fn depth_texel(format: i32, p: &[u8]) -> Option<f32> {
    let u32_at = |p: &[u8]| u32::from_le_bytes([p[0], p[1], p[2], p[3]]);
    match DXGI_FORMAT(format) {
        DXGI_FORMAT_R32_TYPELESS
        | DXGI_FORMAT_R32_FLOAT
        | DXGI_FORMAT_D32_FLOAT
        | DXGI_FORMAT_R32G8X24_TYPELESS
        | DXGI_FORMAT_D32_FLOAT_S8X24_UINT
        | DXGI_FORMAT_R32_FLOAT_X8X24_TYPELESS => Some(f32::from_bits(u32_at(p))),
        DXGI_FORMAT_R24G8_TYPELESS
        | DXGI_FORMAT_D24_UNORM_S8_UINT
        | DXGI_FORMAT_R24_UNORM_X8_TYPELESS => {
            Some((u32_at(p) & 0x00ff_ffff) as f32 / 16_777_215.0)
        }
        DXGI_FORMAT_R16_TYPELESS | DXGI_FORMAT_R16_UNORM | DXGI_FORMAT_D16_UNORM => {
            Some(f32::from(u16::from_le_bytes([p[0], p[1]])) / 65_535.0)
        }
        _ => None,
    }
}

/// A motion texel of footprint format `format` (its first two channels), if
/// read.
pub fn motion_texel(format: i32, p: &[u8]) -> Option<[f32; 2]> {
    let h = |at: usize| half(u16::from_le_bytes([p[at], p[at + 1]]));
    let f = |at: usize| f32::from_le_bytes([p[at], p[at + 1], p[at + 2], p[at + 3]]);
    match DXGI_FORMAT(format) {
        DXGI_FORMAT_R16G16_TYPELESS
        | DXGI_FORMAT_R16G16_FLOAT
        | DXGI_FORMAT_R16G16B16A16_TYPELESS
        | DXGI_FORMAT_R16G16B16A16_FLOAT => Some([h(0), h(2)]),
        DXGI_FORMAT_R32G32_TYPELESS
        | DXGI_FORMAT_R32G32_FLOAT
        | DXGI_FORMAT_R32G32B32A32_TYPELESS
        | DXGI_FORMAT_R32G32B32A32_FLOAT => Some([f(0), f(4)]),
        _ => None,
    }
}

/// What one check found.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Verdict {
    pub n: usize,
    /// Share agreeing as written (previous minus current, pixels).
    pub agree: f32,
    /// Share agreeing with the sign flipped (current minus previous).
    pub flipped: f32,
    /// Median motion length, pixels.
    pub median_px: f32,
}

/// Judges the samples (depth 0: sky, left out) against `clip_to_prev_clip`.
pub fn judge(clip_to_prev_clip: &Mat4, samples: &[MotionSample], size: [u32; 2]) -> Verdict {
    let kept: Vec<MotionSample> = samples
        .iter()
        .copied()
        .filter(|s| s.depth > 0.0 && s.depth.is_finite() && s.motion.iter().all(|m| m.is_finite()))
        .collect();
    if kept.is_empty() {
        return Verdict::default();
    }
    let agree =
        crate::camera::motion_agreement(clip_to_prev_clip, &kept, size, [1.0, 1.0], TOLERANCE_PX);
    let flipped =
        crate::camera::motion_agreement(clip_to_prev_clip, &kept, size, [-1.0, -1.0], TOLERANCE_PX);
    let mut lengths: Vec<f32> = kept
        .iter()
        .map(|s| s.motion[0].hypot(s.motion[1]))
        .collect();
    lengths.sort_by(f32::total_cmp);
    Verdict {
        n: kept.len(),
        agree,
        flipped,
        median_px: lengths[lengths.len() / 2],
    }
}

/// One source of the samples: the texture the pixels are copied from and
/// the footprint format they land in.
struct Source {
    format: i32,
    size: [u32; 2],
}

struct Slot {
    buffer: ID3D12Resource,
    /// Recorded in frame, its camera's clip-to-previous-clip (at the frame's
    /// end), and the submission count that has run it.
    frame: u64,
    camera: Option<Mat4>,
    after: u64,
    depth: Option<Source>,
    motion: Option<Source>,
    busy: bool,
}

#[derive(Default)]
struct Ring {
    slots: Vec<Slot>,
    /// Our plain copy of a depth-stencil texture, and its state.
    plain: Option<(ID3D12Resource, D3D12_RESOURCE_DESC, u32)>,
    last_ms: u64,
    /// Since the last log: agreeing, flipped, judged, checks, median sum.
    sums: (f32, f32, usize, usize, f32),
    told_ms: u64,
}

static RING_: Mutex<Option<Ring>> = Mutex::new(None);

/// A velocity pass was recorded: the next bind of the same state on the
/// same list samples it.
struct After {
    state: usize,
    list: usize,
    depth: Option<u64>,
    motion: Option<u64>,
}

static AFTER: Mutex<Option<After>> = Mutex::new(None);
static AFTER_SET: AtomicBool = AtomicBool::new(false);

/// After a velocity pass's bind (state `state`, host list `list`).
pub(crate) fn after_velocity(state: usize, list: usize, depth: Option<u64>, motion: Option<u64>) {
    if !on() || motion.is_none() {
        return;
    }
    let now = crate::host::now_ms();
    {
        let ring = RING_.lock().unwrap_or_else(|e| e.into_inner());
        if ring.as_ref().is_some_and(|r| now < r.last_ms + EVERY_MS) {
            return;
        }
    }
    *AFTER.lock().unwrap_or_else(|e| e.into_inner()) = Some(After {
        state,
        list,
        depth,
        motion,
    });
    AFTER_SET.store(true, Ordering::Release);
}

/// At every bind: if a velocity pass of this state on this list was just
/// recorded, its pixels are copied now (it is behind us in the list).
pub(crate) fn at_bind(state: usize, list: &HostList) {
    if !AFTER_SET.load(Ordering::Acquire) {
        return;
    }
    let after = {
        let mut a = AFTER.lock().unwrap_or_else(|e| e.into_inner());
        if a.as_ref().is_none_or(|a| a.state != state) {
            return;
        }
        AFTER_SET.store(false, Ordering::Release);
        a.take()
    };
    let Some(after) = after else {
        return;
    };
    if after.list != std::ptr::from_ref(list) as usize {
        return;
    }
    sample(list, after.depth, after.motion);
}

fn readback(device: &ID3D12Device) -> Option<ID3D12Resource> {
    let heap = D3D12_HEAP_PROPERTIES {
        Type: D3D12_HEAP_TYPE_READBACK,
        ..Default::default()
    };
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: PLACEMENT * 2 * SAMPLES as u64,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        ..Default::default()
    };
    let mut made: Option<ID3D12Resource> = None;
    // SAFETY: a plain committed buffer.
    unsafe {
        device.CreateCommittedResource(
            &heap,
            D3D12_HEAP_FLAG_NONE,
            &desc,
            D3D12_RESOURCE_STATE_COPY_DEST,
            None,
            &mut made,
        )
    }
    .ok()?;
    made
}

/// The footprint format of subresource 0 of `desc` (a depth-stencil
/// texture's depth plane).
fn footprint_format(device: &ID3D12Device, desc: &D3D12_RESOURCE_DESC) -> DXGI_FORMAT {
    let mut fp = D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default();
    // SAFETY: a plain query.
    unsafe { device.GetCopyableFootprints(desc, 0, 1, 0, Some(&mut fp), None, None, None) };
    fp.Footprint.Format
}

/// Copies the grid's pixels of `from` (in a copy-source state) into
/// `buffer` from byte `base`, one placed footprint a pixel.
fn copy_pixels(
    l: &ID3D12GraphicsCommandList,
    buffer: &ID3D12Resource,
    base: u64,
    from: &ID3D12Resource,
    format: DXGI_FORMAT,
    points: &[[u32; 2]],
) {
    let src = crate::camera_copy::location(from);
    for (k, p) in points.iter().enumerate() {
        let dst = D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: a borrowed pointer the location never releases.
            pResource: unsafe { std::mem::transmute_copy(buffer) },
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                    Offset: base + k as u64 * PLACEMENT,
                    Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                        Format: format,
                        Width: 1,
                        Height: 1,
                        Depth: 1,
                        RowPitch: D3D12_TEXTURE_DATA_PITCH_ALIGNMENT,
                    },
                },
            },
        };
        let b = D3D12_BOX {
            left: p[0],
            top: p[1],
            front: 0,
            right: p[0] + 1,
            bottom: p[1] + 1,
            back: 1,
        };
        // SAFETY: an open list; the box is inside level 0 and the offset
        // inside the buffer.
        unsafe { l.CopyTextureRegion(&dst, 0, 0, 0, &src, Some(&b)) };
    }
}

fn sample(list: &HostList, depth: Option<u64>, motion: Option<u64>) {
    let Some(host) = crate::host::host() else {
        return;
    };
    let device = &host.device;
    let copy_of = |t: Option<u64>| t.and_then(|t| crate::resource::host_copy(t as This));
    let (Some(mv), depth) = (copy_of(motion), copy_of(depth)) else {
        return;
    };
    let mut guard = RING_.lock().unwrap_or_else(|e| e.into_inner());
    let ring = guard.get_or_insert_with(Ring::default);
    let free = match ring.slots.iter().position(|s| !s.busy) {
        Some(i) => i,
        None if ring.slots.len() < RING => {
            let Some(buffer) = readback(device) else {
                return;
            };
            ring.slots.push(Slot {
                buffer,
                frame: 0,
                camera: None,
                after: 0,
                depth: None,
                motion: None,
                busy: false,
            });
            ring.slots.len() - 1
        }
        None => return,
    };
    ring.last_ms = crate::host::now_ms();
    // SAFETY: live host textures.
    let mv_desc = unsafe { mv.texture.GetDesc() };
    let size = [mv_desc.Width as u32, mv_desc.Height];
    let points = grid(size);
    let l = &list.list;
    crate::gpucost::mark(
        l,
        || "ours: camera mv check copies (VICTORY_CAMERA_MV_CHECK)",
    );
    let buffer = ring.slots[free].buffer.clone();
    let motion_source = sample_one(list, &mv, &mut ring.plain, device, &buffer, 0, &points);
    let depth_source = depth.as_ref().and_then(|d| {
        sample_one(
            list,
            d,
            &mut ring.plain,
            device,
            &buffer,
            PLACEMENT * SAMPLES as u64,
            &points,
        )
    });
    list.keep(&buffer);
    let slot = &mut ring.slots[free];
    slot.frame = crate::camera::frame();
    slot.camera = None;
    slot.after = u64::MAX;
    slot.motion = motion_source;
    slot.depth = depth_source;
    slot.busy = slot.motion.is_some();
}

/// Copies the grid of one texture; its footprint format, if copied.
fn sample_one(
    list: &HostList,
    copy: &Arc<HostTexture>,
    plain: &mut Option<(ID3D12Resource, D3D12_RESOURCE_DESC, u32)>,
    device: &ID3D12Device,
    buffer: &ID3D12Resource,
    base: u64,
    points: &[[u32; 2]],
) -> Option<Source> {
    // SAFETY: a live host texture.
    let desc = unsafe { copy.texture.GetDesc() };
    let format = footprint_format(device, &desc);
    let size = [desc.Width as u32, desc.Height];
    let depth_stencil = desc.Flags.contains(D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL);
    if depth_texel(format.0, &[0; 4]).is_none() && motion_texel(format.0, &[0; 16]).is_none() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            tracing::warn!("camera: mv check: format {} not read", format.0);
        });
        return None;
    }
    let l = &list.list;
    let back = move_to(list, copy, COPY_SOURCE);
    if depth_stencil {
        let mut want = crate::camera_copy::copy_desc(&desc);
        want.Format = format;
        if !plain
            .as_ref()
            .is_some_and(|p| crate::camera_copy::fits(&p.1, &want))
        {
            if let Some(old) = plain.take() {
                crate::host::retire(old.0);
            }
            *plain =
                crate::camera_copy::texture(device, &want, COPY_DEST).map(|t| (t, want, COPY_DEST));
        }
        let Some((ours, _, state)) = plain.as_mut() else {
            if let Some(before) = back {
                move_to(list, copy, before);
            }
            return None;
        };
        let to = crate::camera_copy::location(ours);
        let from = crate::camera_copy::location(&copy.texture);
        // SAFETY: an open list; the depth plane (subresource 0) whole into
        // a plain texture of its footprint format and size.
        unsafe {
            if *state != COPY_DEST {
                l.ResourceBarrier(&[crate::host::transition(
                    ours,
                    D3D12_RESOURCE_STATES(*state as i32),
                    D3D12_RESOURCE_STATE_COPY_DEST,
                )]);
            }
            l.CopyTextureRegion(&to, 0, 0, 0, &from, None);
            l.ResourceBarrier(&[crate::host::transition(
                ours,
                D3D12_RESOURCE_STATE_COPY_DEST,
                D3D12_RESOURCE_STATE_COPY_SOURCE,
            )]);
        }
        *state = COPY_SOURCE;
        list.keep(ours);
        copy_pixels(l, buffer, base, ours, format, points);
    } else {
        copy_pixels(l, buffer, base, &copy.texture, format, points);
    }
    if let Some(before) = back {
        move_to(list, copy, before);
    }
    Some(Source {
        format: format.0,
        size,
    })
}

/// End of a frame: the slots it recorded learn their camera and the
/// submission that runs them; slots that have run are read and judged.
pub(crate) fn frame_end(cam: &crate::camera::FrameCamera) {
    crate::camera_copy::frame_end(cam);
    if !on() {
        return;
    }
    let mut guard = RING_.lock().unwrap_or_else(|e| e.into_inner());
    let Some(ring) = guard.as_mut() else {
        return;
    };
    let started = crate::host::submissions_started();
    let finished = crate::host::submissions_finished();
    let mut judged = Vec::new();
    for slot in ring.slots.iter_mut().filter(|s| s.busy) {
        if slot.frame == cam.frame {
            slot.camera = cam.camera.map(|c| c.clip_to_prev_clip);
            continue;
        }
        if slot.after == u64::MAX {
            // The frame's lists were all handed in by the next frame's end:
            // every submission started by then has run them.
            slot.after = started;
            continue;
        }
        if finished < slot.after || cam.frame < slot.frame + 2 {
            continue;
        }
        slot.busy = false;
        if let Some(v) = read(slot) {
            judged.push(v);
        }
    }
    for v in judged {
        let s = &mut ring.sums;
        s.0 += v.agree * v.n as f32;
        s.1 += v.flipped * v.n as f32;
        s.2 += v.n;
        s.3 += 1;
        s.4 += v.median_px;
    }
    let now = crate::host::now_ms();
    if ring.sums.3 > 0 && now >= ring.told_ms + 3000 {
        let (agree, flipped, n, checks, median) = std::mem::take(&mut ring.sums);
        ring.told_ms = now;
        let share = |x: f32| if n > 0 { 100.0 * x / n as f32 } else { 0.0 };
        tracing::info!(
            n,
            checks,
            flipped = share(flipped),
            median_px = median / checks as f32,
            tolerance_px = TOLERANCE_PX,
            "camera: mv agreement {:.1}% (n={n})",
            share(agree)
        );
    }
}

/// Reads a slot that has run: its pixels judged against its camera.
fn read(slot: &Slot) -> Option<Verdict> {
    let camera = slot.camera?;
    let motion = slot.motion.as_ref()?;
    let len = (PLACEMENT * 2 * SAMPLES as u64) as usize;
    let mut data: *mut std::ffi::c_void = std::ptr::null_mut();
    let range = D3D12_RANGE { Begin: 0, End: len };
    // SAFETY: a read-back buffer the card has finished writing.
    unsafe { slot.buffer.Map(0, Some(&range), Some(&mut data)) }.ok()?;
    // SAFETY: mapped, `len` bytes.
    let bytes = unsafe { std::slice::from_raw_parts(data.cast::<u8>(), len) };
    let points = grid(motion.size);
    let samples: Vec<MotionSample> = points
        .iter()
        .enumerate()
        .filter_map(|(k, p)| {
            let at = k * PLACEMENT as usize;
            let mv = motion_texel(motion.format, &bytes[at..at + 16])?;
            let depth = match &slot.depth {
                Some(d) if d.size == motion.size => {
                    let at = (SAMPLES + k) * PLACEMENT as usize;
                    depth_texel(d.format, &bytes[at..at + 4])?
                }
                _ => return None,
            };
            Some(MotionSample {
                pixel: [p[0] as f32 + 0.5, p[1] as f32 + 0.5],
                depth,
                motion: mv,
            })
        })
        .collect();
    // SAFETY: mapped above; nothing written.
    unsafe {
        slot.buffer
            .Unmap(0, Some(&D3D12_RANGE { Begin: 0, End: 0 }))
    };
    if samples.is_empty() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            tracing::warn!(
                motion_format = motion.format,
                motion_size = ?motion.size,
                depth = ?slot.depth.as_ref().map(|d| (d.format, d.size)),
                "camera: mv check: no samples (no depth, or depth and motion of other sizes)"
            );
        });
        return None;
    }
    Some(judge(&camera, &samples, motion.size))
}

#[cfg(test)]
#[path = "camera_mv_tests.rs"]
mod tests;

#[cfg(test)]
pub(crate) fn bench_readback(device: &ID3D12Device) -> ID3D12Resource {
    readback(device).expect("readback buffer")
}

#[cfg(test)]
pub(crate) fn bench_pixels(
    l: &ID3D12GraphicsCommandList,
    buffer: &ID3D12Resource,
    from: &ID3D12Resource,
    format: DXGI_FORMAT,
    size: [u32; 2],
) {
    copy_pixels(l, buffer, 0, from, format, &grid(size));
}
