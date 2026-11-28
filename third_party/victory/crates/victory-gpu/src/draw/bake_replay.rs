//! `VICTORY_BAKE_REPLAY` (`crate::bake_replay`): bake draws `VICTORY_BAKE_FRESH` named
//! incomplete kept with what they had set, checked at the first draw of each later frame, and
//! drawn again into their targets in a host list of their own once their inputs arrived.

use super::*;
use crate::bake_replay::{C, Seen, Shot, Verdict, add};

thread_local! {
    /// A bake draw is bound (with the switch), and whether `VICTORY_BAKE_FRESH` named it
    /// incomplete.
    pub(super) static BAKE: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
    /// Kept bake draws are being drawn again: not kept again.
    static REPLAYING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// The draw call of a kept bake draw.
#[derive(Clone, Copy)]
pub(super) enum Again {
    Draw(u32, u32, u32, u32),
    Indexed(u32, u32, u32, i32, u32),
    Mesh(u32),
}

/// What a bake draw had set, to draw it again.
pub(super) struct Snap {
    pipeline: usize,
    hashes: Vec<String>,
    heaps: [usize; 2],
    graphics: Vec<Root>,
    vertex: [VertexView; 32],
    index: IndexView,
    topology: u32,
    topology_set: bool,
    target_views: Vec<Option<crate::descriptor::RtvSnapshot>>,
    depth_view: Option<crate::descriptor::RtvSnapshot>,
    targets_set: bool,
    viewports: Vec<D3D12_VIEWPORT>,
    scissors: Vec<RECT>,
    blend: Option<[f32; 4]>,
    stencil: Option<u32>,
    depth_bounds: Option<[f32; 2]>,
    /// Per target of the shot, its kind then.
    kinds: Vec<Option<(u64, u32, u32)>>,
    again: Again,
}

type Kept = Shot<Snap>;

/// The bake draws of the frame being recorded: its number, the draws.
static FRAME_BAKES: std::sync::Mutex<(u64, Vec<Kept>)> = std::sync::Mutex::new((0, Vec::new()));
/// Groups waiting for their inputs: the frame they were drawn in, the draws in order.
static GROUPS: std::sync::Mutex<Vec<(u64, Vec<Kept>)>> = std::sync::Mutex::new(Vec::new());
static CHECKED: AtomicU64 = AtomicU64::new(u64::MAX);

/// The bake draws of a frame before `frame` become a group if one was incomplete.
fn settle(frame: u64) {
    let mut current = FRAME_BAKES.lock().unwrap_or_else(|e| e.into_inner());
    if current.0 == frame {
        return;
    }
    let at = current.0;
    let draws = std::mem::take(&mut current.1);
    current.0 = frame;
    drop(current);
    let group = crate::bake_replay::group(draws);
    if group.is_empty() {
        return;
    }
    let mut groups = GROUPS.lock().unwrap_or_else(|e| e.into_inner());
    let n: usize = groups.iter().map(|(_, g)| g.len()).sum();
    if n + group.len() > crate::bake_replay::MAX_WAITING {
        add(C::Full, group.len() as u64);
        return;
    }
    add(C::Groups, 1);
    add(C::Kept, group.len() as u64);
    groups.push((at, group));
}

/// A plain state with `snap`'s heaps and roots, to read what its draw reads.
fn reader(snap: &Snap) -> State {
    let mut s = State::default();
    s.heaps = snap.heaps;
    s.graphics = snap.graphics.clone();
    s
}

fn same_pipeline(snap: &Snap) -> bool {
    crate::pipeline::host_pipeline(snap.pipeline as This).is_some_and(|p| {
        p.stages.len() == snap.hashes.len()
            && p.stages
                .iter()
                .zip(&snap.hashes)
                .all(|(s, h)| &s.shader.hash == h)
    })
}

/// Clears `target` in `list` as the title cleared it whole.
fn clear_again(
    list: &HostList,
    host: &crate::host::Host,
    target: usize,
    clear: &crate::bake_replay::Clear,
) {
    let Some(view) = clear.view else { return };
    let record = view.restore();
    if record.resource as usize != target {
        return;
    }
    let Some(texture) = crate::resource::host_resource(record.resource) else {
        return;
    };
    const RENDER_TARGET: u32 = 0x4;
    // SAFETY: an open list of ours, a fresh view of a live texture.
    unsafe {
        if let Some((_, before)) = list.transition(record.resource, RENDER_TARGET) {
            use windows::Win32::Graphics::Direct3D12::D3D12_RESOURCE_STATES as St;
            list.list.ResourceBarrier(&[crate::host::transition(
                texture,
                St(before as i32),
                St(RENDER_TARGET as i32),
            )]);
        }
        host.output_views(|views| {
            let rtv = views.rtv(Some(texture), record.desc().as_ref());
            list.list.ClearRenderTargetView(rtv, &clear.color, None);
        });
    }
}

impl State {
    /// After a bake draw was recorded (`ran`) or dropped, with the switch on.
    pub(super) fn bake_replay_drawn(&self, ran: bool, again: Again) {
        if REPLAYING.get() {
            return;
        }
        let Some(incomplete) = BAKE.take() else {
            return;
        };
        if !ran {
            return;
        }
        let Some(p) = crate::pipeline::host_pipeline(self.pipeline as This) else {
            return;
        };
        let frame = crate::frame::current();
        crate::bake_replay::written(&self.targets, frame);
        if !crate::bake_fresh::on() || !crate::bake_inputs::on() {
            return;
        }
        settle(frame);
        let read = self.bake_input_hash(&p);
        if read.is_none() {
            add(C::Unread, 1);
        }
        let (slots, hash) = read.map_or((Vec::new(), None), |(s, h)| (s, Some(h)));
        let shot = Shot {
            targets: self.targets.clone(),
            hash,
            incomplete,
            slots,
            payload: Snap {
                pipeline: self.pipeline,
                hashes: p.stages.iter().map(|s| s.shader.hash.clone()).collect(),
                heaps: self.heaps,
                graphics: self.graphics.clone(),
                vertex: self.vertex,
                index: self.index,
                topology: self.topology,
                topology_set: self.topology_set,
                target_views: self.target_views.clone(),
                depth_view: self.depth_view,
                targets_set: self.targets_set,
                viewports: self.viewports.clone(),
                scissors: self.scissors.clone(),
                blend: self.blend,
                stencil: self.stencil,
                depth_bounds: self.depth_bounds,
                kinds: self
                    .targets
                    .iter()
                    .map(|&t| crate::resource::texture_kind(t as This))
                    .collect(),
                again,
            },
        };
        let mut current = FRAME_BAKES.lock().unwrap_or_else(|e| e.into_inner());
        if current.1.len() >= crate::bake_replay::MAX_FRAME {
            add(C::Full, 1);
            return;
        }
        current.1.push(shot);
    }

    /// Bake draw `p`'s input slots and the hash of what it reads: its parameter dwords, its run
    /// of records and the title's views in those slots (None: not all of it can be read).
    fn bake_input_hash(&self, p: &HostPipeline) -> Option<(Vec<u32>, u64)> {
        let (params, base, len) = self.bake_input_params(p)?;
        let slots = self.bake_input_slots(p)?;
        let (heap, _) = self.view_heap()?;
        let mut h = crate::bake_replay::Hash::default();
        h.add_all(&params);
        let (off, n) = crate::bake_inputs::records_bytes(&params);
        let n = n.min(len.saturating_sub(off));
        h.add64(off);
        if n >= 4 {
            h.add_all(&bake_pass::words(base.checked_add(off)?, (n / 4) as usize)?);
        }
        for &slot in &slots {
            h.add(slot);
            h.add64(
                heap.handle(slot)
                    .map_or(0, crate::descriptor::host::title_key),
            );
        }
        Some((slots, h.get()))
    }

    /// Whether every input slot of every draw of a group is current at `frame`.
    fn bake_replay_arrived(group: &[Kept], frame: u64) -> bool {
        group.iter().all(|shot| {
            let s = reader(&shot.payload);
            let Some((heap, host)) = s.view_heap() else {
                return false;
            };
            let queued: Vec<u32> = host
                .texture_slots_pick(&shot.slots)
                .into_iter()
                .map(|(i, _)| i)
                .collect();
            let fresh: Vec<crate::bake_fresh::Fresh> = shot
                .slots
                .iter()
                .map(|&slot| {
                    crate::bake_fresh::classify(&Self::bake_fresh_look(
                        heap,
                        host,
                        slot,
                        queued.contains(&slot),
                        frame,
                    ))
                })
                .collect();
            // `VICTORY_BAKE_ZERO_WAIT`: an input that still reads zeros has not arrived.
            if crate::bake_zero::wait_ms() > 0
                && Self::bake_zero_still(heap, host, &shot.slots, &fresh, frame)
            {
                return false;
            }
            crate::bake_replay::arrived(&fresh)
        })
    }

    fn bake_replay_unchanged(group: &[Kept]) -> bool {
        group.iter().all(|shot| {
            shot.hash.is_some()
                && crate::pipeline::host_pipeline(shot.payload.pipeline as This).is_some_and(|p| {
                    reader(&shot.payload).bake_input_hash(&p).map(|(_, h)| h) == shot.hash
                })
        })
    }

    /// At the first draw of a frame that is not a bake, with the switch on: the waiting groups
    /// are looked at, and the ready ones drawn again.
    pub(super) fn bake_replay_run() {
        if REPLAYING.get() {
            return;
        }
        let frame = crate::frame::current();
        if CHECKED.load(Ordering::Relaxed) == frame
            || CHECKED.swap(frame, Ordering::Relaxed) == frame
        {
            return;
        }
        settle(frame);
        let groups = std::mem::take(&mut *GROUPS.lock().unwrap_or_else(|e| e.into_inner()));
        if groups.is_empty() {
            return;
        }
        let mode = crate::bake_replay::mode();
        let (mut ready, mut wait) = (Vec::new(), Vec::new());
        for (at, group) in groups {
            let mut targets: Vec<usize> = group
                .iter()
                .flat_map(|s| s.targets.iter().copied())
                .collect();
            targets.sort_unstable();
            targets.dedup();
            let seen = Seen {
                now: frame,
                frame: at,
                arrived: Self::bake_replay_arrived(&group, frame),
                unchanged: Self::bake_replay_unchanged(&group),
                superseded: crate::bake_replay::superseded(&targets, at),
                cleared: targets
                    .iter()
                    .all(|&t| crate::bake_replay::clear_of(t, at).is_some()),
                mode,
            };
            match crate::bake_replay::decide(&seen) {
                Verdict::Wait => wait.push((at, group)),
                Verdict::Replay => ready.push((at, group)),
                Verdict::Changed => add(C::Changed, 1),
                Verdict::Superseded => add(C::Superseded, 1),
                Verdict::Expired => add(C::Expired, 1),
                Verdict::NoClear => add(C::NoClear, 1),
            }
        }
        {
            let mut groups = GROUPS.lock().unwrap_or_else(|e| e.into_inner());
            groups.extend(wait);
            crate::bake_replay::waiting(groups.len() as u64);
        }
        if !ready.is_empty() {
            Self::bake_replay_draw(frame, ready);
        }
    }

    /// The ready groups drawn again, in one host list sent and waited for.
    fn bake_replay_draw(frame: u64, ready: Vec<(u64, Vec<Kept>)>) {
        let Some(host) = crate::host::host() else {
            return;
        };
        let list = match host.new_list() {
            Ok(list) => list,
            Err(e) => {
                tracing::warn!(error = %e, "no host list to draw bakes again");
                return;
            }
        };
        let began = std::time::Instant::now();
        let dropped_before = DROPPED.load(Ordering::Relaxed);
        let (mut groups, mut drawn, mut replay_incomplete) = (0u64, 0u64, 0u64);
        let mut states = Vec::new();
        REPLAYING.set(true);
        for (at, group) in ready {
            let same = group.iter().all(|s| {
                same_pipeline(&s.payload)
                    && s.targets
                        .iter()
                        .zip(&s.payload.kinds)
                        .all(|(&t, &k)| crate::resource::texture_kind(t as This) == k)
            });
            if !same {
                add(C::Stale, 1);
                continue;
            }
            groups += 1;
            let mut cleared = Vec::new();
            for shot in group {
                for &t in &shot.targets {
                    if !cleared.contains(&t) {
                        cleared.push(t);
                        if let Some(c) = crate::bake_replay::clear_of(t, at) {
                            clear_again(&list, host, t, &c);
                        }
                    }
                }
                let snap = shot.payload;
                let mut s = State::default();
                s.pipeline = snap.pipeline;
                s.heaps = snap.heaps;
                s.graphics = snap.graphics;
                s.vertex = snap.vertex;
                s.index = snap.index;
                s.topology = snap.topology;
                s.topology_set = snap.topology_set;
                s.target_views = snap.target_views;
                s.depth_view = snap.depth_view;
                s.targets_set = snap.targets_set;
                s.viewports = snap.viewports;
                s.scissors = snap.scissors;
                s.blend = snap.blend;
                s.stencil = snap.stencil;
                s.depth_bounds = snap.depth_bounds;
                s.replay(&list);
                BAKE.set(None);
                match snap.again {
                    Again::Draw(v, i, f, fi) => s.draw(&list, v, i, f, fi),
                    Again::Indexed(n, i, f, b, fi) => s.draw_indexed(&list, n, i, f, b, fi),
                    Again::Mesh(x) => s.dispatch_mesh(&list, x),
                }
                if BAKE.take() == Some(true) {
                    replay_incomplete += 1;
                }
                drawn += 1;
                // (Its dynamic slots and held shows stay until the list has run.)
                states.push(s);
            }
        }
        if drawn > 0 {
            list.close();
            host.execute(&[&list]);
        }
        REPLAYING.set(false);
        drop(states);
        let dropped = DROPPED
            .load(Ordering::Relaxed)
            .saturating_sub(dropped_before);
        let us = began.elapsed().as_micros() as u64;
        add(C::Drawn, drawn.saturating_sub(dropped));
        add(C::ReplayIncomplete, replay_incomplete);
        add(C::Us, us);
        tracing::info!(
            "bake replay: frame {frame} groups={groups} drawn={drawn} dropped={dropped} replay_incomplete={replay_incomplete} ms={} (incomplete bakes drawn again: inputs arrived, params and records unchanged; VICTORY_BAKE_REPLAY=0: not)",
            us / 1000
        );
    }
}
