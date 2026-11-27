//! The frame's textures for frame generation (with `VICTORY_CAMERA_CAPTURE=1`):
//! - `VICTORY_CAMERA_HUDLESS=1` (off by default): just before the title's
//!   UI blit (`PS_BlitUISDRToBackbuffer`) draws, its render target (the
//!   HUD-less picture) is copied into a texture of ours, once a frame
//!   ([`copy_hudless`]); [`hudless`] gives it to frame generation, at rest in
//!   [`HUDLESS_STATE`];
//! - [`resources`]: the last finished frame's depth, motion vectors and UI
//!   texture (the UI mask: alpha is coverage) as the title's host textures,
//!   to be moved to the state a consumer needs with
//!   `crate::list::ensure_held_state` on a host list.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;

use crate::camera::Surface;
use crate::host::HostList;
use crate::object::This;
use crate::resource::HostTexture;

/// `VICTORY_CAMERA_HUDLESS=1` (needs `VICTORY_CAMERA_CAPTURE=1`).
pub fn hudless_on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        crate::camera::on() && std::env::var_os("VICTORY_CAMERA_HUDLESS").is_some_and(|v| v == "1")
    })
}

pub(crate) const COPY_SOURCE: u32 = D3D12_RESOURCE_STATE_COPY_SOURCE.0 as u32;
pub(crate) const COPY_DEST: u32 = D3D12_RESOURCE_STATE_COPY_DEST.0 as u32;
/// The state the HUD-less copy rests in between frames.
pub const HUDLESS_STATE: D3D12_RESOURCE_STATES = D3D12_RESOURCE_STATE_ALL_SHADER_RESOURCE;

/// The HUD-less copy of the last frame that made one.
#[derive(Clone, Debug)]
pub struct Hudless {
    pub resource: ID3D12Resource,
    pub format: DXGI_FORMAT,
    pub width: u32,
    pub height: u32,
    /// `FrameCamera::frame` of the frame it was copied in.
    pub frame: u64,
}

struct Ours {
    resource: ID3D12Resource,
    desc: D3D12_RESOURCE_DESC,
    /// Its state at the end of what has been recorded so far.
    state: u32,
    frame: u64,
}

static OURS: Mutex<Option<Ours>> = Mutex::new(None);
static COPIES: AtomicU64 = AtomicU64::new(0);

/// The last HUD-less copy, for frame generation: in [`HUDLESS_STATE`] once
/// the list that copied it has run (same queue, recorded before the present
/// of `frame`).
pub fn hudless() -> Option<Hudless> {
    let ours = OURS.lock().unwrap_or_else(|e| e.into_inner());
    ours.as_ref().map(|o| Hudless {
        resource: o.resource.clone(),
        format: o.desc.Format,
        width: o.desc.Width as u32,
        height: o.desc.Height,
        frame: o.frame,
    })
}

/// The description our copy of a texture like `desc` takes: its first level
/// only, readable by shaders, nothing else.
pub(crate) fn copy_desc(desc: &D3D12_RESOURCE_DESC) -> D3D12_RESOURCE_DESC {
    D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
        Alignment: 0,
        Width: desc.Width,
        Height: desc.Height,
        DepthOrArraySize: 1,
        MipLevels: 1,
        Format: desc.Format,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Layout: D3D12_TEXTURE_LAYOUT_UNKNOWN,
        Flags: D3D12_RESOURCE_FLAG_NONE,
    }
}

/// Whether our copy made for `made` serves a texture like `wanted`.
pub(crate) fn fits(made: &D3D12_RESOURCE_DESC, wanted: &D3D12_RESOURCE_DESC) -> bool {
    (made.Width, made.Height, made.Format) == (wanted.Width, wanted.Height, wanted.Format)
}

pub(crate) fn texture(
    device: &ID3D12Device,
    desc: &D3D12_RESOURCE_DESC,
    state: u32,
) -> Option<ID3D12Resource> {
    let heap = D3D12_HEAP_PROPERTIES {
        Type: D3D12_HEAP_TYPE_DEFAULT,
        ..Default::default()
    };
    let mut made: Option<ID3D12Resource> = None;
    // SAFETY: a plain committed texture.
    unsafe {
        device.CreateCommittedResource(
            &heap,
            D3D12_HEAP_FLAG_NONE,
            desc,
            D3D12_RESOURCE_STATES(state as i32),
            None,
            &mut made,
        )
    }
    .ok()?;
    made
}

/// Moves a title texture's host copy to `state` in `list`; the state it
/// left, if the list had it in another one (to move it back after).
pub(crate) fn move_to(list: &HostList, copy: &Arc<HostTexture>, state: u32) -> Option<u32> {
    let (before, after) = list.transition_held(copy, state)?;
    let barrier = crate::host::transition(
        &copy.texture,
        D3D12_RESOURCE_STATES(before as i32),
        D3D12_RESOURCE_STATES(after as i32),
    );
    // SAFETY: an open list and a live texture the list holds (`states`).
    unsafe { list.list.ResourceBarrier(&[barrier]) };
    Some(before)
}

/// A title texture as a surface: its host format and first level's size.
pub(crate) fn surface_of(title: u64) -> Option<(Surface, Arc<HostTexture>)> {
    let t = title as This;
    if t.is_null() || !crate::resource::is_texture(t) {
        return None;
    }
    let copy = crate::resource::host_copy(t)?;
    // SAFETY: a live host texture.
    let desc = unsafe { copy.texture.GetDesc() };
    Some((
        Surface {
            title,
            format: desc.Format.0,
            size: [desc.Width as u32, desc.Height],
        },
        copy,
    ))
}

/// The frame being built's depth, motion and UI textures, held from when
/// their views were read (the title may let them go before the frame ends).
static NOTED: Mutex<[Option<Arc<HostTexture>>; 3]> = Mutex::new([None, None, None]);

/// Holds the velocity pass's depth and motion, the blit's UI texture.
pub(crate) fn note_held(depth: Option<Arc<HostTexture>>, motion: Option<Arc<HostTexture>>, ui: Option<Arc<HostTexture>>) {
    let mut n = NOTED.lock().unwrap_or_else(|e| e.into_inner());
    for (slot, v) in n.iter_mut().zip([depth, motion, ui]) {
        if v.is_some() {
            *slot = v;
        }
    }
}

/// Records the copy of render target `target` (a title texture) into ours,
/// once a frame, before the UI blit draws over it. Every state is as it was
/// after it.
pub(crate) fn copy_hudless(list: &HostList, target: u64) {
    if crate::camera::hudless_noted() {
        return;
    }
    let Some(copy) = crate::resource::host_copy(target as This) else {
        return;
    };
    let Some(host) = crate::host::host() else {
        return;
    };
    // SAFETY: a live host texture.
    let desc = unsafe { copy.texture.GetDesc() };
    let frame = crate::camera::frame();
    let mut ours = OURS.lock().unwrap_or_else(|e| e.into_inner());
    if !ours.as_ref().is_some_and(|o| fits(&o.desc, &desc)) {
        let want = copy_desc(&desc);
        let Some(resource) = texture(&host.device, &want, COPY_DEST) else {
            static ONCE: std::sync::Once = std::sync::Once::new();
            ONCE.call_once(|| {
                tracing::warn!(
                    format = desc.Format.0,
                    w = desc.Width,
                    h = desc.Height,
                    "camera: hudless copy texture not made"
                );
            });
            return;
        };
        if let Some(old) = ours.take() {
            crate::host::retire(old.resource);
        }
        tracing::info!(
            format = desc.Format.0,
            w = desc.Width,
            h = desc.Height,
            "camera: hudless copy texture made (DXGI format {}, {}x{})",
            desc.Format.0,
            desc.Width,
            desc.Height
        );
        *ours = Some(Ours {
            resource,
            desc: want,
            state: COPY_DEST,
            frame,
        });
    }
    let Some(o) = ours.as_mut() else {
        return;
    };
    let back = move_to(list, &copy, COPY_SOURCE);
    crate::gpucost::mark(
        &list.list,
        || "ours: camera hudless copy (VICTORY_CAMERA_HUDLESS)",
    );
    let l = &list.list;
    let mut barriers = Vec::with_capacity(1);
    if o.state != COPY_DEST {
        barriers.push(crate::host::transition(
            &o.resource,
            D3D12_RESOURCE_STATES(o.state as i32),
            D3D12_RESOURCE_STATE_COPY_DEST,
        ));
    }
    let to = location(&o.resource);
    let from = location(&copy.texture);
    // SAFETY: an open list; both textures live (ours kept by the list, the
    // title's held through `states`), same format and size of level 0.
    unsafe {
        if !barriers.is_empty() {
            l.ResourceBarrier(&barriers);
        }
        l.CopyTextureRegion(&to, 0, 0, 0, &from, None);
        l.ResourceBarrier(&[crate::host::transition(
            &o.resource,
            D3D12_RESOURCE_STATE_COPY_DEST,
            HUDLESS_STATE,
        )]);
    }
    list.keep(&o.resource);
    o.state = HUDLESS_STATE.0 as u32;
    o.frame = frame;
    if let Some(before) = back {
        move_to(list, &copy, before);
    }
    COPIES.fetch_add(1, Ordering::Relaxed);
    crate::camera::note_hudless(Surface {
        title: 0,
        format: o.desc.Format.0,
        size: [o.desc.Width as u32, o.desc.Height],
    });
}

/// Subresource 0 of `resource` as a copy location.
pub(crate) fn location(resource: &ID3D12Resource) -> D3D12_TEXTURE_COPY_LOCATION {
    D3D12_TEXTURE_COPY_LOCATION {
        // SAFETY: a borrowed pointer the location never releases.
        pResource: unsafe { std::mem::transmute_copy(resource) },
        Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
            SubresourceIndex: 0,
        },
    }
}

/// The last finished frame's textures, as host textures.
#[derive(Clone, Default)]
pub(crate) struct Resources {
    pub frame: u64,
    pub depth: Option<Arc<HostTexture>>,
    pub motion: Option<Arc<HostTexture>>,
    pub ui_mask: Option<Arc<HostTexture>>,
    pub hudless: Option<Hudless>,
}

static RESOURCES: Mutex<Option<Resources>> = Mutex::new(None);

/// The textures of the last finished frame ([`crate::camera::latest`] has
/// their formats, sizes, the motion-vector scale and the jitter).
pub(crate) fn resources() -> Resources {
    RESOURCES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .unwrap_or_default()
}

/// At the end of a frame: keeps its textures for [`resources`].
pub(crate) fn frame_end(cam: &crate::camera::FrameCamera) {
    let [depth, motion, ui_mask] = std::mem::take(&mut *NOTED.lock().unwrap_or_else(|e| e.into_inner()));
    let r = Resources {
        frame: cam.frame,
        depth,
        motion,
        ui_mask,
        hudless: cam.hudless.and_then(|_| hudless()),
    };
    *RESOURCES.lock().unwrap_or_else(|e| e.into_inner()) = Some(r);
}

/// HUD-less copies recorded so far.
pub fn copies() -> u64 {
    COPIES.load(Ordering::Relaxed)
}

#[cfg(test)]
#[path = "camera_copy_bench.rs"]
mod bench;
