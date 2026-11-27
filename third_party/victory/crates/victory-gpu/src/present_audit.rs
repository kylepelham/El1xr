//! (racefix, issue #57) What each present shows (`VICTORY_PRESENT_AUDIT=1`:
//! black and mostly black presents logged with their context, and a summary
//! every 300 presents; `2`: every present logged; off unless set).
//!
//! Whole frames flash black on loading screens, menus and in the world. The
//! pop watch saw the presented texture itself all zeros (the loading
//! screen's grey border too) between two frames of the same art, so it is
//! not the swap chain: either the title's work that drew it had not landed
//! when our copy read it, or the title presented a texture it had not
//! drawn (or had cleared) that frame. Per present this reads back 16 texels
//! of the presented texture on a 4x4 grid in the same list as the present's
//! copy (`Host::present`), and notes:
//! - the swap chain's back buffer index and the title resource presented;
//! - whether that resource was bound as a render target, or cleared (and
//!   whether to zero), by lists recorded since the last present
//!   ([`target_bound`], [`target_cleared`]; copies and unordered writes
//!   into it are not seen);
//! - the submissions begun and finished when the present was recorded, and
//!   the present's own submission number.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Mutex, OnceLock, mpsc};

use windows::Win32::Graphics::Direct3D12::*;

/// Texels read per present: a 4x4 grid.
const SIDE: u32 = 4;
const SAMPLES: usize = (SIDE * SIDE) as usize;
/// Bytes between samples in the read-back buffer (placement alignment).
const STRIDE: u64 = 512;
/// A present is "mostly black" with at least this many black samples.
const MOSTLY: usize = 12;

/// 0 off, 1 black presents and a summary, 2 every present.
pub(crate) fn mode() -> u8 {
    static MODE: OnceLock<u8> = OnceLock::new();
    *MODE.get_or_init(|| {
        let mode = parse(std::env::var("VICTORY_PRESENT_AUDIT").ok().as_deref());
        if mode > 0 {
            tracing::info!(
                mode,
                "present audit: 16 texels of every presented texture read back, black presents logged with their context (VICTORY_PRESENT_AUDIT=1; 2: every present)"
            );
        }
        mode
    })
}

fn parse(value: Option<&str>) -> u8 {
    match value.map(str::trim) {
        Some("1") => 1,
        Some("2") => 2,
        _ => 0,
    }
}

/// What lists recorded since the last present did to a title resource:
/// render target binds, clears, and whether the last clear was to zero.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub(crate) struct Drawn {
    binds: u32,
    clears: u32,
    zero_clear: bool,
}

static DRAWN: Mutex<Option<HashMap<usize, Drawn>>> = Mutex::new(None);

/// A draw bound title resource `resource` as a render target.
pub(crate) fn target_bound(resource: usize) {
    if mode() == 0 {
        return;
    }
    let mut map = DRAWN.lock().unwrap_or_else(|e| e.into_inner());
    map.get_or_insert_with(HashMap::new)
        .entry(resource)
        .or_default()
        .binds += 1;
}

/// The title cleared render target `resource` to `color`.
pub(crate) fn target_cleared(resource: usize, color: &[f32; 4]) {
    if mode() == 0 {
        return;
    }
    let mut map = DRAWN.lock().unwrap_or_else(|e| e.into_inner());
    let d = map
        .get_or_insert_with(HashMap::new)
        .entry(resource)
        .or_default();
    d.clears += 1;
    d.zero_clear = color[..3].iter().all(|&c| c == 0.0);
}

/// The title presents `resource`: what was done to it since the last
/// present, which is forgotten for every resource.
struct Presenting {
    resource: usize,
    drawn: Drawn,
    /// The resource presented last time.
    previous: usize,
}

static PRESENTING: Mutex<Option<Presenting>> = Mutex::new(None);

/// The title presents `resource` (called before `Host::present`).
pub(crate) fn presenting(resource: usize) {
    if mode() == 0 {
        return;
    }
    static PREVIOUS: AtomicU64 = AtomicU64::new(0);
    let drawn = {
        let mut map = DRAWN.lock().unwrap_or_else(|e| e.into_inner());
        let drawn = map
            .as_ref()
            .and_then(|m| m.get(&resource).copied())
            .unwrap_or_default();
        if let Some(m) = map.as_mut() {
            m.clear();
        }
        drawn
    };
    let previous = PREVIOUS.swap(resource as u64, Relaxed) as usize;
    *PRESENTING.lock().unwrap_or_else(|e| e.into_inner()) = Some(Presenting {
        resource,
        drawn,
        previous,
    });
}

/// A read-back recorded with a present.
pub(crate) struct Taken {
    buffer: ID3D12Resource,
    format: i32,
    element: u32,
    meta: Meta,
}

#[derive(Clone, Copy, Debug)]
struct Meta {
    frame: u64,
    back: u32,
    resource: usize,
    previous: usize,
    drawn: Drawn,
    started: u64,
    finished: u64,
    flown: u64,
    title_written: bool,
}

/// Bytes per texel of the formats presented (`host::swap_format`'s).
fn element_bytes(format: i32) -> Option<u32> {
    match format {
        27..=32 | 87 | 90 | 91 | 23..=25 => Some(4),
        9..=14 => Some(8),
        _ => None,
    }
}

/// Whether one texel's colour (alpha left out) is zero.
fn black(format: i32, texel: &[u8]) -> bool {
    match format {
        23..=25 => u32::from_le_bytes([texel[0], texel[1], texel[2], texel[3]]) & 0x3fff_ffff == 0,
        9..=14 => texel[..6].iter().all(|&b| b == 0),
        _ => texel[..3].iter().all(|&b| b == 0),
    }
}

/// Records into `list` (the present's copy list, `texture` a copy source)
/// the read-back of 16 texels of `texture`.
pub(crate) fn take(
    device: &ID3D12Device,
    list: &ID3D12GraphicsCommandList,
    texture: &ID3D12Resource,
    back: u32,
    frame: u64,
) -> Option<Taken> {
    if mode() == 0 {
        return None;
    }
    let presenting = PRESENTING.lock().unwrap_or_else(|e| e.into_inner()).take();
    // SAFETY: a live texture.
    let desc = unsafe { texture.GetDesc() };
    let element = element_bytes(desc.Format.0)?;
    let buffer = FREE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .pop()
        .or_else(|| made(device))?;
    let (w, h) = (desc.Width as u32, desc.Height);
    for i in 0..SIDE * SIDE {
        let (x, y) = (
            (w * (2 * (i % SIDE) + 1)) / (2 * SIDE),
            (h * (2 * (i / SIDE) + 1)) / (2 * SIDE),
        );
        let to = D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: a borrowed pointer the location never releases.
            pResource: unsafe { std::mem::transmute_copy(&buffer) },
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                    Offset: u64::from(i) * STRIDE,
                    Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                        Format: desc.Format,
                        Width: 1,
                        Height: 1,
                        Depth: 1,
                        RowPitch: 256,
                    },
                },
            },
        };
        let from = D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: as above.
            pResource: unsafe { std::mem::transmute_copy(texture) },
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                SubresourceIndex: 0,
            },
        };
        let texel = D3D12_BOX {
            left: x,
            top: y,
            front: 0,
            right: x + 1,
            bottom: y + 1,
            back: 1,
        };
        // SAFETY: an open list, live resources, a box inside the texture.
        unsafe { list.CopyTextureRegion(&to, 0, 0, 0, &from, Some(&texel)) };
    }
    let (resource, drawn, previous) = presenting
        .map(|p| (p.resource, p.drawn, p.previous))
        .unwrap_or_default();
    Some(Taken {
        buffer,
        format: desc.Format.0,
        element,
        meta: Meta {
            frame,
            back,
            resource,
            previous,
            drawn,
            started: crate::host::submissions_started(),
            finished: crate::host::submissions_finished(),
            flown: 0,
            title_written: crate::texture::title_written(texture),
        },
    })
}

static FREE: Mutex<Vec<ID3D12Resource>> = Mutex::new(Vec::new());

fn made(device: &ID3D12Device) -> Option<ID3D12Resource> {
    let heap = D3D12_HEAP_PROPERTIES {
        Type: D3D12_HEAP_TYPE_READBACK,
        ..Default::default()
    };
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: STRIDE * SAMPLES as u64,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        ..Default::default()
    };
    let mut buffer: Option<ID3D12Resource> = None;
    // SAFETY: a committed read-back buffer.
    unsafe {
        device
            .CreateCommittedResource(
                &heap,
                D3D12_HEAP_FLAG_NONE,
                &desc,
                D3D12_RESOURCE_STATE_COPY_DEST,
                None,
                &mut buffer,
            )
            .ok()?;
    }
    buffer
}

/// The list with `taken` went to the card as submission `flown` (0: it has
/// run).
pub(crate) fn sent(mut taken: Taken, flown: u64) {
    static THREAD: OnceLock<Option<mpsc::Sender<Taken>>> = OnceLock::new();
    taken.meta.flown = flown;
    let Some(to) = THREAD.get_or_init(|| {
        let (to, from) = mpsc::channel::<Taken>();
        std::thread::Builder::new()
            .name("victory present audit".into())
            .spawn(move || watch(&from))
            .ok()
            .map(|_| to)
    }) else {
        return;
    };
    let _ = to.send(taken);
}

/// Since the last summary: presents, black, mostly black, black with no
/// render target bind or clear seen, black after a clear to zero, black
/// whose resource differs from the one before.
#[derive(Default)]
struct Counts {
    presents: u64,
    black: u64,
    mostly: u64,
    undrawn: u64,
    zero_cleared: u64,
    switched: u64,
}

fn watch(from: &mpsc::Receiver<Taken>) {
    let mut counts = Counts::default();
    let mut total_black = 0u64;
    let mut last_black_frame = 0u64;
    while let Ok(taken) = from.recv() {
        while crate::host::submissions_finished() < taken.meta.flown {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let zeros = read(&taken);
        FREE.lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(taken.buffer);
        let Some(zeros) = zeros else {
            continue;
        };
        let m = taken.meta;
        counts.presents += 1;
        let all = zeros == SAMPLES;
        let undrawn = m.drawn.binds == 0 && m.drawn.clears == 0;
        if all {
            counts.black += 1;
            total_black += 1;
            counts.undrawn += u64::from(undrawn);
            counts.zero_cleared += u64::from(m.drawn.zero_clear);
            counts.switched += u64::from(m.resource != m.previous);
        } else if zeros >= MOSTLY {
            counts.mostly += 1;
        }
        if zeros >= MOSTLY || mode() == 2 {
            let since_black = m.frame - last_black_frame;
            let line = format!(
                "frame={} black_samples={zeros}/{SAMPLES} back_buffer={} resource={:#x} same_as_last={} rt_binds={} clears={} last_clear_zero={} title_written={} submissions_started={} finished={} present_submission={} frames_since_last_black={since_black}",
                m.frame,
                m.back,
                m.resource,
                m.resource == m.previous,
                m.drawn.binds,
                m.drawn.clears,
                m.drawn.zero_clear,
                m.title_written,
                m.started,
                m.finished,
                m.flown,
            );
            if zeros >= MOSTLY {
                tracing::warn!(
                    "present audit: a {} present: {line}",
                    if all { "black" } else { "mostly black" }
                );
            } else {
                tracing::info!("present audit: {line}");
            }
        }
        if all {
            last_black_frame = m.frame;
        }
        if counts.presents >= 300 {
            let c = std::mem::take(&mut counts);
            tracing::info!(
                presents = c.presents,
                black = c.black,
                mostly_black = c.mostly,
                black_undrawn = c.undrawn,
                black_after_zero_clear = c.zero_cleared,
                black_resource_switched = c.switched,
                black_since_start = total_black,
                "present audit: the last presents (VICTORY_PRESENT_AUDIT)"
            );
        }
    }
}

/// How many of the 16 texels are black; `None` if not mapped.
fn read(taken: &Taken) -> Option<usize> {
    let mut at = std::ptr::null_mut();
    let range = D3D12_RANGE {
        Begin: 0,
        End: (STRIDE * SAMPLES as u64) as usize,
    };
    // SAFETY: a read-back buffer the card has finished writing.
    unsafe { taken.buffer.Map(0, Some(&range), Some(&mut at)) }.ok()?;
    // SAFETY: the mapped buffer holds STRIDE * SAMPLES bytes.
    let bytes = unsafe { std::slice::from_raw_parts(at.cast::<u8>(), range.End) };
    let zeros = (0..SAMPLES)
        .filter(|&i| {
            let start = i * STRIDE as usize;
            black(taken.format, &bytes[start..start + taken.element as usize])
        })
        .count();
    // SAFETY: mapped above; nothing written.
    unsafe {
        taken
            .buffer
            .Unmap(0, Some(&D3D12_RANGE { Begin: 0, End: 0 }))
    };
    Some(zeros)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_switch_is_off_unless_one_or_two() {
        assert_eq!(parse(None), 0);
        assert_eq!(parse(Some("0")), 0);
        assert_eq!(parse(Some("1")), 1);
        assert_eq!(parse(Some("2")), 2);
        assert_eq!(parse(Some("on")), 0);
    }

    #[test]
    fn black_leaves_alpha_out() {
        // R8G8B8A8 with opaque alpha.
        assert!(black(28, &[0, 0, 0, 255]));
        assert!(!black(28, &[0, 1, 0, 255]));
        // B8G8R8A8.
        assert!(black(87, &[0, 0, 0, 255]));
        // R10G10B10A2 with alpha 3.
        assert!(black(24, &0xc000_0000u32.to_le_bytes()));
        assert!(!black(24, &0xc000_0400u32.to_le_bytes()));
        // R16G16B16A16 float, alpha 1.0.
        assert!(black(10, &[0, 0, 0, 0, 0, 0, 0x00, 0x3c]));
        assert!(!black(10, &[0, 0, 0x01, 0, 0, 0, 0x00, 0x3c]));
    }

    #[test]
    fn element_bytes_cover_the_swap_formats() {
        for f in [28, 87, 24, 10] {
            assert!(element_bytes(f).is_some());
        }
        assert_eq!(element_bytes(10), Some(8));
        assert_eq!(element_bytes(2), None);
    }
}
