//! `VICTORY_SKIN_WATCH` (`crate::skin_watch`): which passes that light skin ran, what they were
//! bound with, what the skin lighting and resolve draws changed in their first target, and
//! whether skin's probe inputs read zero.

use super::*;
use crate::skin_watch::{LIGHTING, RESOLVE};
use std::sync::Arc;

const COPY_SOURCE: u32 = 0x800;

/// A few rows of one texture copied into a read-back buffer of ours.
pub(super) struct Rows {
    buffer: ID3D12Resource,
    /// The buffer's bytes.
    bytes: u64,
    /// Per row its offset in the buffer and its texels.
    rows: Vec<(u64, u32)>,
    texel: u32,
    format: u32,
    /// Width, height and depth (or slices).
    size: [u32; 3],
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
            let all = std::slice::from_raw_parts(data.cast::<u8>(), self.bytes as usize);
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
/// rows read before it with what is told of them.
pub(super) struct Watch {
    kind: &'static str,
    frame: u64,
    before: Option<(usize, Arc<Rows>, crate::skin_watch::Output)>,
}


/// Records the copies of a few rows (inside `rect`, x y w h; all of it when empty) of
/// subresource 0 of `copy` into a read-back buffer, from whatever state the list holds it in
/// and back. A volume (the tiled lighting's light-range volume) has the rows of each of its
/// first [`crate::skin_watch::SLICES`] slices read.
fn read_rows(
    list: &HostList,
    copy: &Arc<crate::resource::HostTexture>,
    rect: [u32; 4],
) -> Option<Rows> {
    let host = crate::host::host()?;
    // SAFETY: a live texture.
    let desc = unsafe { copy.texture.GetDesc() };
    let volume = desc.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE3D;
    if !volume && desc.Dimension != D3D12_RESOURCE_DIMENSION_TEXTURE2D
        || desc.SampleDesc.Count > 1
    {
        return None;
    }
    let size = [desc.Width as u32, desc.Height, u32::from(desc.DepthOrArraySize)];
    let slices = if volume { size[2].min(crate::skin_watch::SLICES) } else { 1 };
    let rows: Vec<_> = (0..slices)
        .flat_map(|z| {
            crate::skin_watch::rows(rect, [size[0], size[1]])
                .into_iter()
                .map(move |(x, y, n)| (x, y, z, n))
        })
        .collect();
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
    let pitch = (u64::from(rows[0].3) * u64::from(texel)).next_multiple_of(512);
    let bytes = rows.len() as u64 * pitch;
    let buffer = bake_check::read_back_buffer(&host.device, bytes)?;
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
    for (k, &(x, y, z, n)) in rows.iter().enumerate() {
        let offset = k as u64 * pitch;
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
            front: z,
            right: x + n,
            bottom: y + 1,
            back: z + 1,
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
        bytes,
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
    /// first target's rows read (every skin lighting draw, the first resolve draws a frame).
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
            && let Some(index) = crate::skin_watch::take_output(frame, kind)
            && let Some(p) = crate::pipeline::host_pipeline(self.pipeline as This)
        {
            let output = crate::skin_watch::Output {
                frame,
                kind,
                index,
                pixel: p
                    .stages
                    .iter()
                    .find(|s| s.shader.stage == "pixel")
                    .map_or_else(String::new, |s| s.shader.hash.clone()),
                blended: p.state.contains("blend true"),
            };
            before = read_rows(list, &copy, self.viewport_rect())
                .map(|r| (target, Arc::new(r), output));
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
        if let Some((target, before, output)) = w.before
            && let Some(copy) = crate::resource::host_copy(target as This)
            && let Some(after) = read_rows(list, &copy, self.viewport_rect())
        {
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
                crate::skin_watch::output_landed(&output, &shape, c, rgb);
            });
        }
        if w.kind == LIGHTING && crate::skin_watch::take(w.frame, "inputs", 1).is_some() {
            self.skin_watch_inputs(list, w.frame);
        }
    }

    /// Skin's probe atlas (`crate::skin_fill::ATLAS`, its all-zero texels counted) and the
    /// tiled lighting's light-range volume (`crate::skin_fill::LAYERS`, its tiles without
    /// lights and widest light range counted): a few rows of each read, counted once the list
    /// has run.
    fn skin_watch_inputs(&self, list: &HostList, frame: u64) {
        let read = |(slot, offset): (u8, u32)| {
            let Some(&Root::Table(table)) = self.roots_ref(false).get(usize::from(slot)) else {
                return Err("no_table".to_owned());
            };
            let r = match crate::descriptor::view(table + u64::from(offset) * 4) {
                Some(View::Shader(r, _)) if r != 0 => r,
                other => return Err(format!("no_view {}", view_kind(other.as_ref()))),
            };
            let copy = crate::resource::host_copy(r as This)
                .ok_or_else(|| format!("no_copy {r:#x}"))?;
            read_rows(list, &copy, [0; 4]).ok_or_else(|| {
                // SAFETY: a live texture.
                let d = unsafe { copy.texture.GetDesc() };
                format!(
                    "unreadable dim {} f{} {}x{}x{} samples {}",
                    d.Dimension.0,
                    d.Format.0,
                    d.Width,
                    d.Height,
                    d.DepthOrArraySize,
                    d.SampleDesc.Count
                )
            })
        };
        for (name, place) in [
            ("atlas", crate::skin_fill::ATLAS),
            ("light_ranges", crate::skin_fill::LAYERS),
        ] {
            let rows = match read(place) {
                Ok(rows) => rows,
                Err(why) => {
                    crate::skin_watch::input_landed(frame, name, &why, (0, 0));
                    continue;
                }
            };
            list.defer_after(move || {
                let bytes = rows.bytes().unwrap_or_default();
                let counts = crate::skin_watch::zeros(&bytes, rows.texel as usize);
                let [w, h, d] = rows.size;
                let mut shape = format!("f{} {w}x{h}x{d}", rows.format);
                if name == "light_ranges" && rows.texel == 4 {
                    let r = crate::skin_watch::light_ranges(&bytes);
                    shape.push_str(&format!(
                        " no_lights={} widest={} bad={}",
                        r.none, r.widest, r.bad
                    ));
                }
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

/// What a table entry that is not a texture view holds, for the log.
fn view_kind(view: Option<&View>) -> &'static str {
    match view {
        None => "none",
        Some(View::Shader(..)) => "null_shader",
        Some(_) => "other",
    }
}
