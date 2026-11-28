//! `VICTORY_SKIN_WATCH` (`crate::skin_watch`): which passes that light skin ran, what they were
//! bound with, what the skin lighting and resolve draws changed in their first target, and
//! whether skin's probe inputs read zero.

use super::*;
use crate::skin_watch::{LIGHTING, RESOLVE};
use std::sync::Arc;

const COPY_SOURCE: u32 = 0x800;
/// Bytes of the read-back buffer per row: [`crate::bake_check::ROW_TEXELS`] of the widest
/// format, 512-aligned.
const ROW_BYTES: u64 = crate::bake_check::ROW_TEXELS as u64 * 16;

/// A few rows of one texture copied into a read-back buffer of ours.
pub(super) struct Rows {
    buffer: ID3D12Resource,
    /// Per row its offset in the buffer and its texels.
    rows: Vec<(u64, u32)>,
    texel: u32,
    format: u32,
    size: [u32; 2],
}

impl Rows {
    /// The rows' bytes, once the list that copied them has run.
    fn bytes(&self) -> Option<Vec<u8>> {
        let mut data = std::ptr::null_mut();
        // SAFETY: a read-back buffer of ours the list's copies wrote, mapped whole while read.
        unsafe {
            if self.buffer.Map(0, None, Some(&mut data)).is_err() || data.is_null() {
                return None;
            }
            let all = std::slice::from_raw_parts(
                data.cast::<u8>(),
                (u64::from(crate::bake_check::ROWS) * ROW_BYTES) as usize,
            );
            let mut out = Vec::new();
            for &(offset, n) in &self.rows {
                let at = offset as usize;
                if let Some(row) = all.get(at..at + (n * self.texel) as usize) {
                    out.extend_from_slice(row);
                }
            }
            self.buffer.Unmap(0, None);
            Some(out)
        }
    }
}

/// A skin lighting or resolve draw being recorded: its kind, frame, and its first target's
/// rows read before it.
pub(super) struct Watch {
    kind: &'static str,
    frame: u64,
    before: Option<(usize, Arc<Rows>)>,
}

/// Records the copies of a few rows (inside `rect`, x y w h; all of it when empty) of
/// subresource 0 of `copy` into a read-back buffer, from whatever state the list holds it in
/// and back.
fn read_rows(
    list: &HostList,
    copy: &Arc<crate::resource::HostTexture>,
    rect: [u32; 4],
) -> Option<Rows> {
    let host = crate::host::host()?;
    // SAFETY: a live texture.
    let desc = unsafe { copy.texture.GetDesc() };
    if desc.Dimension != D3D12_RESOURCE_DIMENSION_TEXTURE2D || desc.SampleDesc.Count > 1 {
        return None;
    }
    let size = [desc.Width as u32, desc.Height];
    let rows = crate::bake_check::rows(rect, size);
    if rows.is_empty() {
        return None;
    }
    // Bytes a texel, from the host's own layout of one row of the first subresource.
    let (mut row_bytes, mut one_row) = (0u64, desc);
    one_row.Height = 1;
    one_row.DepthOrArraySize = 1;
    one_row.MipLevels = 1;
    // SAFETY: a plain query; the out pointer is ours.
    unsafe {
        host.device.GetCopyableFootprints(
            &one_row,
            0,
            1,
            0,
            None,
            None,
            Some(&mut row_bytes),
            None,
        );
    }
    let texel = (row_bytes / desc.Width.max(1)) as u32;
    if texel == 0 || texel > 16 || u64::from(texel) * desc.Width != row_bytes {
        return None;
    }
    let buffer =
        bake_check::read_back_buffer(&host.device, u64::from(crate::bake_check::ROWS) * ROW_BYTES)?;
    let back = list.transition_held(copy, COPY_SOURCE);
    if let Some((before, now)) = back {
        let barrier = crate::host::transition(
            &copy.texture,
            D3D12_RESOURCE_STATES(before as i32),
            D3D12_RESOURCE_STATES(now as i32),
        );
        // SAFETY: an open list and a live texture.
        unsafe { list.list.ResourceBarrier(&[barrier]) };
    }
    let mut out = Vec::new();
    for (k, &(x, y, n)) in rows.iter().enumerate() {
        let offset = k as u64 * ROW_BYTES;
        let to = D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: a borrowed pointer the location never releases.
            pResource: unsafe { std::mem::transmute_copy(&buffer) },
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                    Offset: offset,
                    Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                        Format: desc.Format,
                        Width: n,
                        Height: 1,
                        Depth: 1,
                        RowPitch: (n * texel).next_multiple_of(256),
                    },
                },
            },
        };
        let from = D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: as above.
            pResource: unsafe { std::mem::transmute_copy(&copy.texture) },
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                SubresourceIndex: 0,
            },
        };
        let area = D3D12_BOX {
            left: x,
            top: y,
            front: 0,
            right: x + n,
            bottom: y + 1,
            back: 1,
        };
        // SAFETY: an open list, live resources, a box inside the texture.
        unsafe {
            list.list
                .CopyTextureRegion(&to, 0, 0, 0, &from, Some(&area))
        };
        out.push((offset, n));
    }
    if let Some((before, _)) = back {
        crate::list::ensure_held_state(list, copy, before);
    }
    Some(Rows {
        buffer,
        rows: out,
        texel,
        format: desc.Format.0 as u32,
        size,
    })
}

impl State {
    /// The watched pass the bound pipeline is part of, if any.
    fn skin_watch_kind(&self, compute: bool) -> Option<&'static str> {
        let p = crate::pipeline::host_pipeline(self.pipeline as This)?;
        if p.compute != compute {
            return None;
        }
        if !compute && p.stages.iter().any(|s| s.shader.skin()) {
            return Some(LIGHTING);
        }
        p.stages
            .iter()
            .find_map(|s| crate::skin_watch::kind(&s.shader.hash, || s.shader.camera_name()))
    }

    /// What a draw is bound with: its depth target (host format, a colour twin),
    /// depth function and stencil, and how its rectangles are drawn.
    fn skin_watch_bound(&self) -> Option<String> {
        let p = crate::pipeline::host_pipeline(self.pipeline as This)?;
        let depth = self.depth as This;
        let host = crate::resource::host_copy(depth)
            // SAFETY: a live texture.
            .map_or(0, |c| unsafe { c.texture.GetDesc() }.Format.0);
        let rects = match (self.topology == RECTANGLES, p.rects) {
            (false, _) => "no",
            (true, true) => "gs",
            (true, false) => "first_triangle",
        };
        Some(format!(
            "depth=f{host} twin={} {} rects={rects} targets={}",
            crate::resource::depth_and_unordered(depth),
            crate::skin_watch::depth_state(&p.state),
            self.targets.len(),
        ))
    }

    /// Before an indirect draw: when it is a skin lighting or resolve draw, its watch, and the
    /// first target's rows read for the first two of each kind a frame.
    pub(super) fn skin_watch_before(&self, list: &HostList, kind: u32) -> Option<Watch> {
        const DRAW: u32 = 0;
        const INDEXED: u32 = 1;
        if !matches!(kind, DRAW | INDEXED) {
            return self.skin_watch_kind(true).map(|kind| Watch {
                kind,
                frame: crate::frame::current(),
                before: None,
            });
        }
        let kind = self.skin_watch_kind(false)?;
        let frame = crate::frame::current();
        let mut before = None;
        if (kind == LIGHTING || kind == RESOLVE)
            && let Some(&target) = self.targets.first()
            && let Some(copy) = crate::resource::host_copy(target as This)
            && crate::skin_watch::take_output(frame, kind)
        {
            before = read_rows(list, &copy, self.viewport_rect()).map(|r| (target, Arc::new(r)));
        }
        Some(Watch {
            kind,
            frame,
            before,
        })
    }

    /// The first viewport as x, y, w, h (all zero without one).
    fn viewport_rect(&self) -> [u32; 4] {
        self.viewports.first().map_or([0; 4], |v| {
            [v.TopLeftX, v.TopLeftY, v.Width, v.Height].map(|f| f.max(0.0) as u32)
        })
    }

    /// After an indirect draw or dispatch with a watch: counted; a draw that ran has its first
    /// target read again (compared once the list has run), and the frame's first skin lighting
    /// draw its probe inputs.
    pub(super) fn skin_watch_after(&self, list: &HostList, w: Watch, ran: bool) {
        let why = crate::drop_why::why_now();
        let draw = w.kind == LIGHTING || w.kind == RESOLVE || w.before.is_some();
        crate::skin_watch::note(w.frame, w.kind, ran, why, || {
            if draw { self.skin_watch_bound() } else { None }
        });
        if !ran {
            return;
        }
        if let Some((target, before)) = w.before
            && let Some(copy) = crate::resource::host_copy(target as This)
            && let Some(after) = read_rows(list, &copy, self.viewport_rect())
        {
            let (frame, kind) = (w.frame, w.kind);
            let pixel = crate::pipeline::host_pipeline(self.pipeline as This)
                .and_then(|p| p.stages.iter().find(|s| s.shader.stage == "pixel"))
                .map_or_else(String::new, |s| s.shader.hash.clone());
            list.defer_after(move || {
                let texel = after.texel as usize;
                let (c, rgb) = match (before.bytes(), after.bytes()) {
                    (Some(b), Some(a)) => (
                        crate::skin_watch::change(&b, &a, texel, |t| {
                            crate::skin_watch::black_texel(after.format, t)
                        }),
                        crate::skin_watch::changed_rgb(&b, &a, texel, after.format),
                    ),
                    _ => (crate::skin_watch::Change::default(), None),
                };
                let shape = format!("f{} {}x{}", after.format, after.size[0], after.size[1]);
                crate::skin_watch::output_landed(frame, kind, &pixel, &shape, c, rgb);
            });
        }
        if w.kind == LIGHTING && crate::skin_watch::take(w.frame, "inputs", 1) {
            self.skin_watch_inputs(list, w.frame);
        }
    }

    /// Skin's probe inputs (`crate::skin_fill::ATLAS`, `LAYERS`): a few rows of each read, the
    /// all-zero texels counted once the list has run.
    fn skin_watch_inputs(&self, list: &HostList, frame: u64) {
        let view = |(slot, offset): (u8, u32)| match self.roots_ref(false).get(usize::from(slot)) {
            Some(&Root::Table(table)) => {
                match crate::descriptor::view(table + u64::from(offset) * 4) {
                    Some(View::Shader(r, _)) if r != 0 => Some(r),
                    _ => None,
                }
            }
            _ => None,
        };
        for (name, place) in [
            ("atlas", crate::skin_fill::ATLAS),
            ("layers", crate::skin_fill::LAYERS),
        ] {
            let Some(rows) = view(place)
                .and_then(|r| crate::resource::host_copy(r as This))
                .and_then(|copy| read_rows(list, &copy, [0; 4]))
            else {
                crate::skin_watch::input_landed(frame, name, "no_view", (0, 0));
                continue;
            };
            list.defer_after(move || {
                let counts = rows.bytes().map_or((0, 0), |b| {
                    crate::skin_watch::zeros(&b, rows.texel as usize)
                });
                let shape = format!("f{} {}x{}", rows.format, rows.size[0], rows.size[1]);
                crate::skin_watch::input_landed(frame, name, &shape, counts);
            });
        }
    }

    /// After a draw or dispatch that is not indirect: counted when it is part of a watched pass.
    pub(super) fn skin_watch_plain(&self, compute: bool, ran: bool) {
        let Some(kind) = self.skin_watch_kind(compute) else {
            return;
        };
        let why = crate::drop_why::why_now();
        crate::skin_watch::note(crate::frame::current(), kind, ran, why, || {
            if compute {
                None
            } else {
                self.skin_watch_bound()
            }
        });
    }
}
