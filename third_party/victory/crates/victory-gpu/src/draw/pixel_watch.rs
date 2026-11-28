//! `VICTORY_PIXEL_WATCH` (`crate::pixel_watch`) command-list recording hooks on [`State`].
//!
//! Records batched 1x1 `CopyTextureRegion` samples of every screen-sized render target,
//! depth/stencil target and compute UAV on watched frames, together with the draws/dispatches
//! covering the watched pixel(s), their shader hashes and bound texture slots/formats.

use std::sync::Arc;

use victory_shader::{ResourceType, RootType};

use super::{Root, State};
use crate::descriptor::View;
use crate::host::HostList;
use crate::object::This;
use crate::pipeline::host::HostPipeline;
use crate::pixel_watch::{
    self, BATCH_BYTES, BATCH_SLOTS, BoundTex, DrawCover, LandedSample, PassKind, WatchPoint, gpu,
};

const COPY_SOURCE: u32 = 0x800;
/// Cap on distinct covering pipelines recorded per pass.
const MAX_COVERS: usize = 12;
/// Cap on bound SRV/UAV textures recorded per covering pipeline.
const MAX_TEXTURES: usize = 8;
/// Cap on mid-pass pipeline-switch flushes inside one single-RT pass.
const MAX_SINGLE_RT_SWITCHES: u32 = 24;
/// Cap on total samples batched per command list.
const MAX_LIST_SAMPLES: usize = 512;

struct GfxPass {
    frame: u64,
    targets: Vec<usize>,
    depth: usize,
    last_pipeline: usize,
    switches: u32,
    draw_count: u32,
    point_mask: u8,
    covers: Vec<DrawCover>,
}

struct UavPass {
    frame: u64,
    pipeline: usize,
    cs_tag: u64,
    uavs: Vec<(u16, usize)>,
    dispatches: u32,
    covers: Vec<DrawCover>,
}

struct RecordedSample {
    slot: u32,
    stencil_slot: Option<u32>,
    bytes: usize,
    point_idx: u8,
    point: WatchPoint,
    kind: PassKind,
    target_slot: u16,
    resource: usize,
    memory: u64,
    width: u32,
    height: u32,
    format: u32,
    tx: u32,
    ty: u32,
    tag: u64,
    draw_count: u32,
    covers: Vec<DrawCover>,
}

struct RecordedBatch {
    buf: gpu::ReadbackBuf,
    samples: Vec<RecordedSample>,
}

/// Per-command-list pixel-watch state held inside `draw::State`.
#[derive(Default)]
pub(super) struct ListState {
    frame: u64,
    active_gfx: Option<GfxPass>,
    active_uav: Option<UavPass>,
    batches: Vec<RecordedBatch>,
    cur_buf: Option<gpu::ReadbackBuf>,
    cur_slots: u32,
    cur_samples: Vec<RecordedSample>,
    total_samples: usize,
}

impl ListState {
    pub(super) fn reset(&mut self) {
        self.frame = 0;
        self.active_gfx = None;
        self.active_uav = None;
        self.batches.clear();
        self.cur_buf = None;
        self.cur_slots = 0;
        self.cur_samples.clear();
        self.total_samples = 0;
    }

    pub(super) fn has_gfx(&self) -> bool {
        self.active_gfx.is_some()
    }

    fn seal_cur(&mut self) {
        if let Some(buf) = self.cur_buf.take() {
            if self.cur_samples.is_empty() {
                gpu::return_buffer(buf);
            } else {
                let samples = std::mem::take(&mut self.cur_samples);
                self.batches.push(RecordedBatch { buf, samples });
            }
        }
        self.cur_slots = 0;
    }

    fn alloc_slots(
        &mut self,
        device: &windows::Win32::Graphics::Direct3D12::ID3D12Device,
        need: u32,
    ) -> Option<(&windows::Win32::Graphics::Direct3D12::ID3D12Resource, u32)> {
        if self.total_samples >= MAX_LIST_SAMPLES || need == 0 || need > BATCH_SLOTS {
            return None;
        }
        if self.cur_buf.is_none() || self.cur_slots + need > BATCH_SLOTS {
            self.seal_cur();
            self.cur_buf = gpu::take_buffer(device);
            self.cur_slots = 0;
        }
        let buf = self.cur_buf.as_ref()?;
        let slot = self.cur_slots;
        self.cur_slots += need;
        Some((&buf.0, slot))
    }
}

fn roots_sig(roots: &[Root]) -> u64 {
    roots.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, r| match r {
        Root::Table(t) => (h ^ t).wrapping_mul(0x0100_0000_01b3),
        _ => h,
    })
}

fn primary_shader_hash(p: &HostPipeline, compute: bool) -> String {
    let want = if compute { "compute" } else { "pixel" };
    p.stages
        .iter()
        .find(|s| s.shader.stage == want)
        .or_else(|| p.stages.last())
        .map_or_else(String::new, |s| s.shader.hash.clone())
}

impl State {
    /// Collects SRV/UAV textures bound via root tables for the current pipeline.
    fn pixel_watch_textures(&self, p: &HostPipeline, compute: bool) -> (Vec<BoundTex>, bool) {
        use crate::resource::find_facts::{object_brief, words_brief};
        let roots = if compute {
            &self.compute
        } else {
            &self.graphics
        };
        let heap = self
            .heaps
            .iter()
            .filter(|&&h| h != 0)
            .map(|&h| crate::descriptor::heap(h as This))
            .find(|h| !h.samplers());
        let mut textures = Vec::new();
        let mut heap_indexed = false;
        for stage in &p.stages {
            if !compute && stage.shader.stage == "vertex" {
                continue;
            }
            let used = &stage.translated().used;
            heap_indexed |= used.iter().any(|u| u.root == RootType::ResourceHeap);
            for u in used.iter().filter(|u| {
                matches!(u.kind, ResourceType::Srv | ResourceType::Uav) && u.root == RootType::Table
            }) {
                if textures.len() >= MAX_TEXTURES {
                    break;
                }
                let Some(Root::Table(table)) = roots.get(usize::from(u.slot)) else {
                    continue;
                };
                let handle = table + u64::from(u.table_offset_dwords) * 4;
                let (resource, memory, width, height, format) =
                    match crate::descriptor::view(handle) {
                        Some(View::Shader(r, _) | View::Unordered(r, _, _)) => {
                            let Some((m, w, h, f, dim)) = object_brief(r as This) else {
                                continue;
                            };
                            if dim == 1 {
                                continue;
                            }
                            (r, m, w, h, f)
                        }
                        Some(View::Placed(_, res, view)) => {
                            let (w, h, f, dim) = words_brief(&res);
                            if dim == 1 {
                                continue;
                            }
                            (0, u64::from(view[10]) | u64::from(view[11]) << 32, w, h, f)
                        }
                        _ => continue,
                    };
                let kind = if u.kind == ResourceType::Uav {
                    b'u'
                } else {
                    b't'
                };
                let register = u.lower_bound as u16;
                if textures.iter().any(|t: &BoundTex| {
                    t.kind == kind && t.register == register && t.memory == memory
                }) {
                    continue;
                }
                let record = crate::descriptor::record_at(handle);
                let slot = heap.and_then(|h| h.index(handle));
                let host = match (heap, slot) {
                    (Some(h), Some(s)) => crate::descriptor::host_slot_state(h.start, s, record),
                    _ => "not_in_heap",
                };
                textures.push(BoundTex {
                    kind,
                    register,
                    resource,
                    memory,
                    width,
                    height,
                    format,
                    slot,
                    host,
                });
            }
        }
        (textures, heap_indexed)
    }

    /// Collects screen-sized 2D UAV texture resources written by a compute dispatch.
    fn pixel_watch_compute_uavs(&self, p: &HostPipeline) -> Vec<(u16, usize)> {
        use crate::resource::find_facts::object_brief;
        let mut out = Vec::new();
        for stage in &p.stages {
            for u in stage
                .translated()
                .used
                .iter()
                .filter(|u| u.kind == ResourceType::Uav && u.root == RootType::Table)
            {
                let Some(Root::Table(table)) = self.compute.get(usize::from(u.slot)) else {
                    continue;
                };
                let handle = table + u64::from(u.table_offset_dwords) * 4;
                if let Some(View::Unordered(r, _, _)) = crate::descriptor::view(handle)
                    && r != 0
                    && let Some((_, w, h, _, dim)) = object_brief(r as This)
                    && dim != 1
                    && pixel_watch::screen_shaped(w, h)
                {
                    let reg = u.lower_bound as u16;
                    if !out.iter().any(|&(k, res)| k == reg && res == r) {
                        out.push((reg, r));
                    }
                }
            }
        }
        out
    }

    /// Records 1x1 texel copy(s) of `copy` for every watched point bit set in `point_mask`.
    #[allow(clippy::too_many_arguments)]
    fn record_target_sample(
        &mut self,
        list: &HostList,
        copy: &Arc<crate::resource::HostTexture>,
        kind: PassKind,
        target_slot: u16,
        resource: usize,
        tag: u64,
        draw_count: u32,
        covers: &[DrawCover],
        point_mask: u8,
    ) {
        let Some(host) = crate::host::host() else {
            return;
        };
        // SAFETY: live host texture.
        let desc = unsafe { copy.texture.GetDesc() };
        if desc.Dimension
            != windows::Win32::Graphics::Direct3D12::D3D12_RESOURCE_DIMENSION_TEXTURE2D
            || desc.SampleDesc.Count > 1
        {
            return;
        }
        let (w, h) = (desc.Width as u32, desc.Height);
        if !pixel_watch::screen_shaped(desc.Width, h) {
            return;
        }
        let format = desc.Format.0 as u32;
        let Some(info) = pixel_watch::format_info(format) else {
            return;
        };
        let Some((cfmt0, pitch0)) = gpu::plane_footprint(&host.device, &desc, 0, info.copy_format)
        else {
            return;
        };
        let stencil_info = if info.has_stencil_plane {
            gpu::plane_footprint(&host.device, &desc, 1, 62).map(|(cf, p)| {
                let sub = u32::from(desc.MipLevels) * u32::from(desc.DepthOrArraySize);
                (cf, p, sub)
            })
        } else {
            None
        };
        let cfg = pixel_watch::config();
        let active = cfg.active_points();
        if active.is_empty() || point_mask == 0 {
            return;
        }
        let memory = crate::resource::find_facts::object_brief(resource as This)
            .map_or(0, |(m, _, _, _, _)| m);

        let back = list.transition_held(copy, COPY_SOURCE);
        if let Some((before, now)) = back {
            let barrier = crate::host::transition(
                &copy.texture,
                windows::Win32::Graphics::Direct3D12::D3D12_RESOURCE_STATES(before as i32),
                windows::Win32::Graphics::Direct3D12::D3D12_RESOURCE_STATES(now as i32),
            );
            // SAFETY: open list and live texture.
            unsafe { list.list.ResourceBarrier(&[barrier]) };
        }

        let need = if stencil_info.is_some() { 2 } else { 1 };
        for (idx, &pt) in active.iter().enumerate() {
            if point_mask & (1u8 << idx) == 0 {
                continue;
            }
            let Some((buf, slot0)) = self.pixel_watch.alloc_slots(&host.device, need) else {
                break;
            };
            let (tx, ty) = pixel_watch::map_pixel(pt.u, pt.v, w, h);
            gpu::copy_texel(
                &list.list,
                buf,
                slot0,
                &copy.texture,
                0,
                cfmt0,
                pitch0,
                tx,
                ty,
            );
            let stencil_slot = if let Some((cfmt1, pitch1, sub1)) = stencil_info {
                gpu::copy_texel(
                    &list.list,
                    buf,
                    slot0 + 1,
                    &copy.texture,
                    sub1,
                    cfmt1,
                    pitch1,
                    tx,
                    ty,
                );
                Some(slot0 + 1)
            } else {
                None
            };
            self.pixel_watch.cur_samples.push(RecordedSample {
                slot: slot0,
                stencil_slot,
                bytes: info.bytes as usize,
                point_idx: idx as u8,
                point: pt,
                kind,
                target_slot,
                resource,
                memory,
                width: w,
                height: h,
                format,
                tx,
                ty,
                tag,
                draw_count,
                covers: covers.to_vec(),
            });
            self.pixel_watch.total_samples += 1;
        }

        if let Some((before, _)) = back {
            crate::list::ensure_held_state(list, copy, before);
        }
    }

    fn flush_gfx(&mut self, list: &HostList) {
        let Some(pass) = self.pixel_watch.active_gfx.take() else {
            return;
        };
        if pass.draw_count == 0 || pass.point_mask == 0 {
            return;
        }
        self.pixel_watch.frame = pass.frame;
        for (i, &rt) in pass.targets.iter().enumerate() {
            if rt == 0 {
                continue;
            }
            if let Some(copy) = crate::resource::host_copy(rt as This) {
                self.record_target_sample(
                    list,
                    &copy,
                    PassKind::Rt,
                    i as u16,
                    rt,
                    0,
                    pass.draw_count,
                    &pass.covers,
                    pass.point_mask,
                );
            }
        }
        if pass.depth != 0
            && let Some(copy) = crate::resource::host_copy(pass.depth as This)
        {
            self.record_target_sample(
                list,
                &copy,
                PassKind::Dsv,
                0,
                pass.depth,
                0,
                pass.draw_count,
                &pass.covers,
                pass.point_mask,
            );
        }
    }

    fn flush_uav(&mut self, list: &HostList) {
        let Some(pass) = self.pixel_watch.active_uav.take() else {
            return;
        };
        if pass.dispatches == 0 || pass.uavs.is_empty() {
            return;
        }
        self.pixel_watch.frame = pass.frame;
        let cfg = pixel_watch::config();
        let all_mask = ((1u16 << cfg.count) - 1) as u8;
        for &(reg, res) in &pass.uavs {
            // Compute shaders writing a depth+UAV texture write its colour twin.
            let copy = match crate::resource::host_side(res as This) {
                Some((_, Some(twin))) => twin.colour.clone(),
                Some((copy, None)) => copy,
                None => continue,
            };
            self.record_target_sample(
                list,
                &copy,
                PassKind::Uav,
                reg,
                res,
                pass.cs_tag,
                pass.dispatches,
                &pass.covers,
                all_mask,
            );
        }
    }

    /// Called from `record_targets` right before the bound RTVs/DSV change.
    pub(super) fn pixel_watch_targets_changing(&mut self, list: &HostList) {
        if self.pixel_watch.has_gfx() {
            self.flush_gfx(list);
        }
    }

    /// Called from `bind()` right before a graphics draw or compute dispatch is recorded:
    /// flushes the previous pass BEFORE the new pass overwrites its target(s).
    pub(super) fn pixel_watch_before_bind(&mut self, list: &HostList, compute: bool) {
        if compute {
            if self.pixel_watch.active_gfx.is_some() {
                self.flush_gfx(list);
            }
            if self
                .pixel_watch
                .active_uav
                .as_ref()
                .is_some_and(|u| u.pipeline != self.pipeline)
            {
                self.flush_uav(list);
            }
        } else {
            if self.pixel_watch.active_uav.is_some() {
                self.flush_uav(list);
            }
            // In single-RT lighting/post passes, flush on pipeline switches (up to a cap) so
            // individual lighting stages (Sun, TiledLighting, Ambient, Sky, Post) get distinct
            // samples even when sharing one `OMSetRenderTargets` call.
            let switch = self.pixel_watch.active_gfx.as_ref().is_some_and(|g| {
                g.targets.len() == 1
                    && g.last_pipeline != self.pipeline
                    && g.switches < MAX_SINGLE_RT_SWITCHES
            });
            if switch {
                let prev_switches = self
                    .pixel_watch
                    .active_gfx
                    .as_ref()
                    .map_or(0, |g| g.switches);
                self.flush_gfx(list);
                // Remember switches across the pass by seeding an empty pass with incremented
                // counter if the next draw covers a watched point (we carry it in a fresh GfxPass).
                if let Some(frame) =
                    Some(crate::frame::current()).filter(|&f| pixel_watch::this_frame(f))
                {
                    self.pixel_watch.active_gfx = Some(GfxPass {
                        frame,
                        targets: self.targets.clone(),
                        depth: self.depth,
                        last_pipeline: self.pipeline,
                        switches: prev_switches + 1,
                        draw_count: 0,
                        point_mask: 0,
                        covers: Vec::new(),
                    });
                }
            }
        }
    }

    /// Called after a graphics draw (`DrawInstanced`, `DrawIndexedInstanced`, `DispatchMesh`,
    /// or indirect draw/mesh) ran.
    pub(super) fn pixel_watch_draw(&mut self, list: &HostList) {
        let frame = crate::frame::current();
        if !pixel_watch::this_frame(frame) {
            return;
        }
        pixel_watch::announce_if_needed();
        if self.pixel_watch.active_uav.is_some() {
            self.flush_uav(list);
        }
        // Find the first screen-shaped bound target (RTV or DSV) to test viewport/scissor coverage.
        let primary = self
            .targets
            .iter()
            .copied()
            .chain((self.depth != 0).then_some(self.depth))
            .find_map(|r| {
                let (_, w, h, _, dim) = crate::resource::find_facts::object_brief(r as This)?;
                (dim != 1 && pixel_watch::screen_shaped(w, h)).then_some((w as u32, h))
            });
        let Some((w, h)) = primary else {
            return;
        };
        let cfg = pixel_watch::config();
        let viewport = self
            .viewports
            .first()
            .map(|v| [v.TopLeftX, v.TopLeftY, v.Width, v.Height]);
        let scissor = self
            .scissors
            .first()
            .map(|r| [r.left, r.top, r.right, r.bottom]);
        let mut mask = 0u8;
        for (i, pt) in cfg.active_points().iter().enumerate() {
            let (tx, ty) = pixel_watch::map_pixel(pt.u, pt.v, w, h);
            if pixel_watch::point_covered(tx, ty, viewport, scissor) {
                mask |= 1u8 << i;
            }
        }
        if mask == 0 {
            return;
        }
        let Some(p) = crate::pipeline::host_pipeline(self.pipeline as This) else {
            return;
        };
        let hash = primary_shader_hash(&p, false);
        let sig = roots_sig(&self.graphics);

        // If the existing active_gfx belongs to different targets, flush it first.
        if self
            .pixel_watch
            .active_gfx
            .as_ref()
            .is_some_and(|g| g.targets != self.targets || g.depth != self.depth)
        {
            self.flush_gfx(list);
        }

        let need_walk = {
            let pass = self.pixel_watch.active_gfx.get_or_insert_with(|| GfxPass {
                frame,
                targets: self.targets.clone(),
                depth: self.depth,
                last_pipeline: self.pipeline,
                switches: 0,
                draw_count: 0,
                point_mask: 0,
                covers: Vec::new(),
            });
            pass.last_pipeline = self.pipeline;
            pass.draw_count += 1;
            pass.point_mask |= mask;
            if let Some(existing) = pass
                .covers
                .iter_mut()
                .find(|c| c.shader_hash == hash && c.sig == sig)
            {
                existing.draws += 1;
                false
            } else {
                pass.covers.len() < MAX_COVERS
            }
        };
        if need_walk {
            let (textures, heap_indexed) = self.pixel_watch_textures(&p, false);
            if let Some(pass) = self.pixel_watch.active_gfx.as_mut() {
                pass.covers.push(DrawCover {
                    shader_hash: hash,
                    pipeline_name: p.name.clone(),
                    draws: 1,
                    sig,
                    heap_indexed,
                    textures,
                });
            }
        }
    }

    /// Called after a compute dispatch (`Dispatch` or indirect dispatch) ran.
    pub(super) fn pixel_watch_dispatch(&mut self, list: &HostList) {
        let frame = crate::frame::current();
        if !pixel_watch::this_frame(frame) {
            return;
        }
        pixel_watch::announce_if_needed();
        if self.pixel_watch.active_gfx.is_some() {
            self.flush_gfx(list);
        }
        let Some(p) = crate::pipeline::host_pipeline(self.pipeline as This) else {
            return;
        };
        let uavs = self.pixel_watch_compute_uavs(&p);
        if uavs.is_empty() {
            return;
        }
        let hash = primary_shader_hash(&p, true);
        let cs_tag = pixel_watch::fnv64(hash.as_bytes());
        let sig = roots_sig(&self.compute);

        if self
            .pixel_watch
            .active_uav
            .as_ref()
            .is_some_and(|u| u.pipeline != self.pipeline || u.uavs != uavs)
        {
            self.flush_uav(list);
        }

        let need_walk = {
            let pass = self.pixel_watch.active_uav.get_or_insert_with(|| UavPass {
                frame,
                pipeline: self.pipeline,
                cs_tag,
                uavs: uavs.clone(),
                dispatches: 0,
                covers: Vec::new(),
            });
            pass.dispatches += 1;
            if let Some(existing) = pass
                .covers
                .iter_mut()
                .find(|c| c.shader_hash == hash && c.sig == sig)
            {
                existing.draws += 1;
                false
            } else {
                pass.covers.len() < MAX_COVERS
            }
        };
        if need_walk {
            let (textures, heap_indexed) = self.pixel_watch_textures(&p, true);
            if let Some(pass) = self.pixel_watch.active_uav.as_mut() {
                pass.covers.push(DrawCover {
                    shader_hash: hash,
                    pipeline_name: p.name.clone(),
                    draws: 1,
                    sig,
                    heap_indexed,
                    textures,
                });
            }
        }
    }

    /// Called right before `HostList::close`: flushes any open graphics/compute pass and schedules
    /// one batched `defer_after` callback to read back all 1x1 copies recorded in this list.
    pub(crate) fn pixel_watch_close(&mut self, list: &HostList) {
        self.flush_gfx(list);
        self.flush_uav(list);
        self.pixel_watch.seal_cur();
        if self.pixel_watch.batches.is_empty() {
            return;
        }
        let frame = self.pixel_watch.frame;
        let list_id = list.ops.id();
        let batches = std::mem::take(&mut self.pixel_watch.batches);
        self.pixel_watch.total_samples = 0;

        // `defer_after` expects `Fn()`, so move ownership via `Mutex<Option<...>>`.
        let payload = std::sync::Mutex::new(Some(batches));
        list.defer_after(move || {
            let Some(batches) = payload.lock().unwrap_or_else(|e| e.into_inner()).take() else {
                return;
            };
            let mut landed = Vec::new();
            for batch in batches {
                let mut data = std::ptr::null_mut();
                // SAFETY: our read-back buffer, after its command list has landed on the GPU.
                unsafe {
                    if batch.buf.0.Map(0, None, Some(&mut data)).is_ok() && !data.is_null() {
                        let all =
                            std::slice::from_raw_parts(data.cast::<u8>(), BATCH_BYTES as usize);
                        for s in batch.samples {
                            if let Some(raw) = gpu::read_slot(all, s.slot, s.bytes) {
                                let stencil = s
                                    .stencil_slot
                                    .and_then(|st| gpu::read_slot(all, st, 1))
                                    .and_then(|v| v.first().copied());
                                landed.push(LandedSample {
                                    point_idx: s.point_idx,
                                    point: s.point,
                                    kind: s.kind,
                                    slot: s.target_slot,
                                    resource: s.resource,
                                    memory: s.memory,
                                    width: s.width,
                                    height: s.height,
                                    format: s.format,
                                    tx: s.tx,
                                    ty: s.ty,
                                    tag: s.tag,
                                    draw_count: s.draw_count,
                                    covers: s.covers,
                                    raw,
                                    stencil,
                                });
                            }
                        }
                        batch.buf.0.Unmap(0, None);
                    }
                }
                gpu::return_buffer(batch.buf);
            }
            pixel_watch::landed(frame, list_id, &landed);
        });
    }
}
