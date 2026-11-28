//! (critpath, #88) The recording of `Host::card_before`'s two lists, from
//! what the plan read when it was made, and `VICTORY_CARD_RECORD_SEND=1`
//! (Live; off by default; takes effect with `VICTORY_SEND_THREAD=1`).
//!
//! `card_before` is `card_before` on the preparing breakdown line, 2.0-3.1 ms
//! a frame on the submitting thread, of which the recording of the sync list
//! (copies up, masks, rehash) is 1.1-1.8 ms and the compare's list (with the
//! fetch inside the flight) 0.5-0.8 ms (`card_before split per frame`, b5g and
//! b5h). Everything those lists record is fixed when the plan is made: which
//! copies changed, their ranges, the masks, the compare's slots in the result
//! area, the slots done with, the card sides of each copy. [`SyncRec`] and
//! [`CheckRec`] hold all of it, so the recording reads nothing that can move
//! after the plan is made, and the same function records with the switch on
//! and off.
//!
//! With the switch, a submission sent ahead hands the two recordings to the
//! thread that sends (`crate::host::send_thread`), which records them right
//! before the submission's `ExecuteCommandLists`. The lists are already in
//! their places in the submission (the sync list first, the compare last or
//! in a call of its own): a list is the same object before and after it is
//! recorded. Order:
//!
//! 1. The lists are the plan's own (taken from `CardWork` by this plan and
//!    given back by `Host::card_done` only once its fence completed), so no
//!    other recording or run of them is in the way.
//! 2. The thread records a job's lists before it sends that job's lists, and
//!    jobs run in the order they were handed over; everything else that puts
//!    work on the host queue waits for the thread first (`wait_idle`), so it
//!    also waits for the recording.
//! 3. A recording that fails sends nothing of that job, signals its fence
//!    and fails the run (as a failure inline did, with the same text).
//! 4. A submission the thread does not take (switch off, no thread, not
//!    sent ahead) records inline before its lists go to the queue.
//!
//! Not in the cache key: no shader or pipeline output changes.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use super::*;

static ON: victory_core::settings::Live =
    victory_core::settings::Live::flag("VICTORY_CARD_RECORD_SEND");

/// The switch, read once per submission.
pub(super) fn on() -> bool {
    ON.get_bool()
}

/// Recordings handed to the thread since the last line.
static HANDED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Of them, those recorded inline because the thread did not take the job.
static INLINE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// One changed copy's part of the sync list.
pub(super) struct SyncItem {
    pub(super) shadow: Arc<crate::shadow::Shadow>,
    pub(super) staging: ID3D12Resource,
    pub(super) before: Option<ID3D12Resource>,
    pub(super) size: u64,
    /// What changed on its CPU side.
    pub(super) ranges: Vec<(usize, usize)>,
    /// Of them, what goes up as copies (`None`: all of them).
    pub(super) copies: Option<Vec<(usize, usize)>>,
    /// What goes up by mask: (table at, blocks, unused).
    pub(super) masked: Vec<(usize, usize, usize)>,
    /// Declared for the first time: hashed whole.
    pub(super) whole: bool,
}

/// Everything the sync list records.
pub(super) struct SyncRec {
    pub(super) items: Vec<SyncItem>,
    /// Transient windows: the copy's buffer and, per chunk, offsets and where
    /// the bytes are (buffer, offset).
    pub(super) uploads: Vec<(ID3D12Resource, Vec<(usize, usize, ID3D12Resource, u64)>)>,
    /// The masked pass and the plan's mask buffer, if both are there.
    pub(super) patch: Option<(ID3D12RootSignature, ID3D12PipelineState, ID3D12Resource)>,
    pub(super) root: ID3D12RootSignature,
    pub(super) pso: ID3D12PipelineState,
    pub(super) differs: ID3D12Resource,
    /// Rehash runs joined (`rehash_runs`, `under_writes_runs`).
    pub(super) join: bool,
}

impl SyncRec {
    /// Reads the changed copies' card sides now: a copy without its CPU-side
    /// staging is left out, as the recording always left it out. The ranges
    /// of those taken move here (the plan keeps only the copies).
    pub(super) fn new(
        work: &CardWork,
        dirty: &mut [(Arc<crate::shadow::Shadow>, Vec<(usize, usize)>)],
        sends: Vec<(Option<Vec<(usize, usize)>>, Vec<(usize, usize, usize)>)>,
        uploads: &[crate::shadow::Uploads],
        masks: Option<&(ID3D12Resource, usize, u64)>,
        whole: &[Arc<crate::shadow::Shadow>],
    ) -> SyncRec {
        let mut items = Vec::with_capacity(dirty.len());
        for ((shadow, ranges), (copies, masked)) in dirty.iter_mut().zip(sends) {
            let Some((Some(staging), before, size)) = shadow.card_sides() else {
                continue;
            };
            items.push(SyncItem {
                shadow: shadow.clone(),
                staging: staging.clone(),
                before,
                size,
                ranges: std::mem::take(ranges),
                copies,
                masked,
                whole: whole.iter().any(|s| Arc::ptr_eq(s, shadow)),
            });
        }
        SyncRec {
            items,
            uploads: uploads
                .iter()
                .map(|(shadow, parts)| {
                    let parts = parts
                        .iter()
                        .map(|(a, b, chunk)| (*a, *b, chunk.buffer().clone(), chunk.offset))
                        .collect();
                    (shadow.buffer.clone(), parts)
                })
                .collect(),
            patch: match (work.patch.as_ref(), masks) {
                (Some((root, pso)), Some((buffer, _, _))) => {
                    Some((root.clone(), pso.clone(), buffer.clone()))
                }
                _ => None,
            },
            root: work.root.clone(),
            pso: work.pso.clone(),
            differs: work.differs.0.clone(),
            join: rehash_runs() || under_writes_runs() > 512,
        }
    }

    /// Records the sync list into `sync`.
    ///
    /// # Safety
    /// The list's last run finished; the buffers are live and the ranges lie
    /// in them.
    pub(super) unsafe fn record(&self, sync: &Prelude) -> windows::core::Result<ID3D12CommandList> {
        let (allocator, list) = sync;
        let mut t = crate::prep_cut::now();
        // SAFETY: as the caller says.
        unsafe {
            allocator.Reset()?;
            list.Reset(allocator, None)?;
            crate::prep_cut::lap(crate::prep_cut::F::SyncOpen, &mut t);
            crate::gpucost::begin(list, crate::gpucost::CARD_SYNC);
            // Resolve each dirty copy's card sides once and execute in
            // 3 batched phases (Copy -> CARD_PATCH -> Rehash) to
            // collapse per-buffer barriers and compute PSO binds.
            let mut states = Vec::with_capacity(self.items.len());
            let mut total_bytes = 0u64;
            let mut total_ranges = 0u64;
            for item in &self.items {
                let copies = item.copies.as_deref().unwrap_or(item.ranges.as_slice());
                crate::prep_cut::add(crate::prep_cut::F::SyncCopies, copies.len() as u64);
                for &(a, b) in copies {
                    list.CopyBufferRegion(
                        &item.shadow.buffer,
                        a as u64,
                        &item.staging,
                        a as u64,
                        (b - a) as u64,
                    );
                }
                total_bytes += item
                    .ranges
                    .iter()
                    .map(|&(a, b)| (b - a) as u64)
                    .sum::<u64>();
                total_ranges += item.ranges.len() as u64;
                states.push((!copies.is_empty()).then_some(D3D12_RESOURCE_STATE_COPY_DEST));
            }
            // (Neptune) Transient windows: from their chunks, in the order copied. No
            // masks, no compare, nothing hashed: shaders do not write them.
            for (buffer, parts) in &self.uploads {
                crate::prep_cut::add(crate::prep_cut::F::SyncCopies, parts.len() as u64);
                for (a, b, chunk, offset) in parts {
                    list.CopyBufferRegion(buffer, *a as u64, chunk, *offset, (b - a) as u64);
                    crate::windowup::SENT.fetch_add((b - a) as u64, Ordering::Relaxed);
                }
                total_bytes += parts
                    .iter()
                    .map(|&(a, b, _, _)| (b - a) as u64)
                    .sum::<u64>();
                total_ranges += parts.len() as u64;
            }
            if total_bytes > 0 || total_ranges > 0 {
                crate::shadow::CARD_STATS[0].fetch_add(total_bytes, Ordering::Relaxed);
                crate::shadow::CARD_STATS[1].fetch_add(total_ranges, Ordering::Relaxed);
            }
            // Phase 2: masked CARD_PATCH dispatches (batched barriers + single PSO bind).
            crate::gpucost::at(list, crate::gpucost::CARD_PATCH);
            if let Some((root, pso, mask_buffer)) = &self.patch {
                let mut to_uav: Vec<D3D12_RESOURCE_BARRIER> = Vec::new();
                let mut any_masked = false;
                for (item, state) in self.items.iter().zip(&mut states) {
                    if !item.masked.is_empty() {
                        any_masked = true;
                        if let Some(from) = *state {
                            to_uav.push(transition(
                                &item.shadow.buffer,
                                from,
                                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                            ));
                        }
                        *state = Some(D3D12_RESOURCE_STATE_UNORDERED_ACCESS);
                    }
                }
                if !to_uav.is_empty() {
                    list.ResourceBarrier(&to_uav);
                }
                if any_masked {
                    list.SetComputeRootSignature(root);
                    list.SetPipelineState(pso);
                    list.SetComputeRootShaderResourceView(2, mask_buffer.GetGPUVirtualAddress());
                    for item in &self.items {
                        if item.masked.is_empty() {
                            continue;
                        }
                        list.SetComputeRootShaderResourceView(
                            1,
                            item.staging.GetGPUVirtualAddress(),
                        );
                        list.SetComputeRootUnorderedAccessView(
                            3,
                            item.shadow.buffer.GetGPUVirtualAddress(),
                        );
                        for &(table_at, blocks, _) in &item.masked {
                            let constants = [table_at as u32, blocks as u32];
                            list.SetComputeRoot32BitConstants(0, 2, constants.as_ptr().cast(), 0);
                            list.Dispatch(16 * blocks as u32, 1, 1);
                        }
                    }
                }
            }
            // Phase 3: batched barriers (`COPY_DEST`/`UAV` -> `COMMON` for read-only
            // copies and transient uploads, since `sync` is batched into the same
            // `ExecuteCommandLists` as the title's lists; `COPY_DEST`/`UAV` ->
            // `NON_PIXEL_SHADER_RESOURCE` for copies with `before`) followed by a
            // single rehash PSO bind and a final `NON_PIXEL_SHADER_RESOURCE` -> `COMMON`.
            crate::gpucost::at(list, crate::gpucost::CARD_REHASH);
            let mut post_barriers: Vec<D3D12_RESOURCE_BARRIER> = Vec::new();
            let mut any_rehash = false;
            for (item, state) in self.items.iter().zip(&states) {
                if item.before.is_some() {
                    any_rehash = true;
                    if let Some(from) = *state {
                        post_barriers.push(transition(
                            &item.shadow.buffer,
                            from,
                            D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
                        ));
                    }
                } else if let Some(from) = *state {
                    post_barriers.push(transition(
                        &item.shadow.buffer,
                        from,
                        D3D12_RESOURCE_STATE_COMMON,
                    ));
                }
            }
            for (buffer, parts) in &self.uploads {
                if !parts.is_empty() {
                    post_barriers.push(transition(
                        buffer,
                        D3D12_RESOURCE_STATE_COPY_DEST,
                        D3D12_RESOURCE_STATE_COMMON,
                    ));
                }
            }
            if !post_barriers.is_empty() {
                list.ResourceBarrier(&post_barriers);
            }
            if any_rehash {
                list.SetComputeRootSignature(&self.root);
                list.SetPipelineState(&self.pso);
                list.SetComputeRootUnorderedAccessView(3, self.differs.GetGPUVirtualAddress());
                let mut pass_count = 0u64;
                let mut piece_count = 0u64;
                let mut rehash_done: Vec<D3D12_RESOURCE_BARRIER> = Vec::new();
                for (item, state) in self.items.iter().zip(&states) {
                    let Some(before) = &item.before else {
                        continue;
                    };
                    let shadow = &item.shadow;
                    list.SetComputeRootShaderResourceView(1, shadow.buffer.GetGPUVirtualAddress());
                    list.SetComputeRootUnorderedAccessView(2, before.GetGPUVirtualAddress());
                    let runs: Vec<crate::shadow::CardSpan> = if item.whole {
                        vec![shadow.card_run(0, usize::MAX)]
                    } else if self.join {
                        crate::shadow::joined_piece_runs_iter(
                            item.ranges.iter().map(|&(a, b)| shadow.card_run(a, b)),
                        )
                    } else {
                        item.ranges
                            .iter()
                            .map(|&(a, b)| shadow.card_run(a, b))
                            .collect()
                    };
                    let touched = state.is_some() || !runs.is_empty();
                    pass_count += runs.len() as u64;
                    piece_count += runs.iter().map(|r| u64::from(r.pieces)).sum::<u64>();
                    for run in runs {
                        let constants = [run.first, run.skew, item.size as u32, run.pieces, 0, 0];
                        list.SetComputeRoot32BitConstants(0, 6, constants.as_ptr().cast(), 0);
                        list.Dispatch(run.pieces.min(65_535), 1, 1);
                    }
                    if touched {
                        rehash_done.push(transition(
                            &shadow.buffer,
                            D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
                            D3D12_RESOURCE_STATE_COMMON,
                        ));
                    }
                }
                if !rehash_done.is_empty() {
                    list.ResourceBarrier(&rehash_done);
                }
                CARD_PASSES[0].fetch_add(pass_count, Ordering::Relaxed);
                CARD_PASSES[1].fetch_add(piece_count, Ordering::Relaxed);
            }
            crate::gpucost::end(list);
            crate::prep_cut::restart(&mut t);
            list.Close()?;
            crate::prep_cut::lap(crate::prep_cut::F::SyncClose, &mut t);
            list.cast()
        }
    }
}

/// One range of the compare, with its copy's card sides as read when the
/// plan was made.
pub(super) struct CheckRange {
    /// The copy (its address), for the compare's passes.
    copy: usize,
    buffer: ID3D12Resource,
    span: crate::shadow::CardSpan,
    at: u32,
    compared: bool,
    /// The copy's "before" twin and size, if it has card sides.
    sides: Option<(Option<ID3D12Resource>, u64)>,
}

/// The compare that keeps what it flags ([`relay_own_queue`]): root, pipeline,
/// its buffer, its slots, the slots done with when the plan was made.
type Keep = (
    ID3D12RootSignature,
    ID3D12PipelineState,
    ID3D12Resource,
    u32,
    u32,
);

/// The fetch inside the flight ([`inflight_fetch`]): root, pipeline, table,
/// where the pieces come down and its slots, the tickets landed.
type Inflight = (
    ID3D12RootSignature,
    ID3D12PipelineState,
    ID3D12Resource,
    ID3D12Resource,
    u32,
    u32,
);

/// Everything the compare's list records.
pub(super) struct CheckRec {
    ranges: Vec<CheckRange>,
    root: ID3D12RootSignature,
    pso: ID3D12PipelineState,
    differs: ID3D12Resource,
    keep: Option<Keep>,
    inflight: Option<Inflight>,
    flown: bool,
}

impl CheckRec {
    pub(super) fn new(
        work: &CardWork,
        ranges: &[CardRange],
        keep: Option<(&CardSnap, u32)>,
        flown: bool,
    ) -> CheckRec {
        CheckRec {
            ranges: ranges
                .iter()
                .map(|r| CheckRange {
                    copy: Arc::as_ptr(&r.shadow) as usize,
                    buffer: r.shadow.buffer.clone(),
                    span: r.span,
                    at: r.at,
                    compared: r.compared,
                    sides: r
                        .shadow
                        .card_sides()
                        .map(|(_, before, size)| (before, size)),
                })
                .collect(),
            root: work.root.clone(),
            pso: work.pso.clone(),
            differs: work.differs.0.clone(),
            keep: keep.map(|(snap, base)| {
                (
                    snap.root.clone(),
                    snap.pso.clone(),
                    snap.buffer.clone(),
                    snap.slots,
                    base,
                )
            }),
            inflight: work.inflight.as_ref().filter(|_| flown).map(|i| {
                (
                    i.root.clone(),
                    i.pso.clone(),
                    i.table.0.clone(),
                    i.fetched.0.clone(),
                    i.fetched.2,
                    i.landed,
                )
            }),
            flown,
        }
    }

    /// With the fetch inside the flight: whether every compared range has it,
    /// and the tickets landed it was told.
    pub(super) fn inflight(&self) -> Option<(bool, u32)> {
        self.inflight.as_ref().map(|i| {
            let every = self
                .ranges
                .iter()
                .filter(|r| r.compared)
                .all(|r| matches!(r.sides, Some((Some(_), _))));
            (every, i.5)
        })
    }

    /// Records the compare into `check`.
    ///
    /// # Safety
    /// The list's last run finished; live buffers.
    pub(super) unsafe fn record(
        &self,
        check: &Prelude,
    ) -> windows::core::Result<ID3D12CommandList> {
        let (allocator, list) = check;
        let mut t = crate::prep_cut::now();
        let uav_all = || D3D12_RESOURCE_BARRIER {
            Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
            Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
            Anonymous: D3D12_RESOURCE_BARRIER_0 {
                UAV: std::mem::ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER {
                    pResource: std::mem::ManuallyDrop::new(None),
                }),
            },
        };
        // SAFETY: as the caller says; the compare's own root signature.
        unsafe {
            allocator.Reset()?;
            list.Reset(allocator, None)?;
            crate::prep_cut::lap(crate::prep_cut::F::CheckOpen, &mut t);
            crate::gpucost::begin(list, crate::gpucost::CARD_CHECK);
            // What the submission's shaders wrote, first.
            list.ResourceBarrier(&[uav_all()]);
            match &self.keep {
                Some((root, pso, buffer, _, _)) => {
                    list.SetComputeRootSignature(root);
                    list.SetPipelineState(pso);
                    list.SetComputeRootUnorderedAccessView(4, buffer.GetGPUVirtualAddress());
                }
                None => {
                    list.SetComputeRootSignature(&self.root);
                    list.SetPipelineState(&self.pso);
                }
            }
            let (snap_base, snap_slots, words) = self
                .keep
                .as_ref()
                .map_or((0, 0, 6), |&(_, _, _, slots, base)| (base, slots, 8));
            list.SetComputeRootUnorderedAccessView(3, self.differs.GetGPUVirtualAddress());
            let key = |r: &CheckRange| {
                r.compared
                    .then_some((r.copy, r.span.first, r.span.pieces, r.at))
            };
            for (i, first, pieces, at) in crate::card_share::passes(self.ranges.iter().map(key)) {
                let r = &self.ranges[i];
                let Some((Some(before), size)) = &r.sides else {
                    continue;
                };
                let constants = [
                    first,
                    r.span.skew,
                    *size as u32,
                    pieces,
                    at * 4,
                    if self.flown { 2 } else { 1 },
                    snap_base,
                    snap_slots,
                ];
                list.SetComputeRoot32BitConstants(0, words, constants.as_ptr().cast(), 0);
                list.SetComputeRootShaderResourceView(1, r.buffer.GetGPUVirtualAddress());
                list.SetComputeRootUnorderedAccessView(2, before.GetGPUVirtualAddress());
                list.Dispatch(pieces.min(65_535), 1, 1);
                CARD_PASSES[2].fetch_add(1, Ordering::Relaxed);
            }
            // The fetch inside the flight (`inflight_fetch`): behind
            // every compare of the list, the pieces they flagged go
            // to slots in the PC's memory.
            if let Some((root, pso, table, fetched, slots, landed)) = &self.inflight {
                list.ResourceBarrier(&[uav_all()]);
                list.SetComputeRootSignature(root);
                list.SetPipelineState(pso);
                list.SetComputeRootUnorderedAccessView(2, self.differs.GetGPUVirtualAddress());
                list.SetComputeRootUnorderedAccessView(3, table.GetGPUVirtualAddress());
                list.SetComputeRootUnorderedAccessView(4, fetched.GetGPUVirtualAddress());
                for r in self.ranges.iter().filter(|r| r.compared) {
                    let Some((Some(_), size)) = &r.sides else {
                        continue;
                    };
                    let groups = r.span.pieces.clamp(1, 65535);
                    let constants = [
                        r.span.first,
                        r.span.skew,
                        *size as u32,
                        r.span.pieces,
                        r.at * 4,
                        *landed,
                        *slots,
                        groups,
                    ];
                    list.SetComputeRoot32BitConstants(0, 8, constants.as_ptr().cast(), 0);
                    list.SetComputeRootShaderResourceView(1, r.buffer.GetGPUVirtualAddress());
                    list.Dispatch(groups, 1, 1);
                }
            }
            crate::gpucost::end(list);
            crate::prep_cut::restart(&mut t);
            list.Close()?;
            crate::prep_cut::lap(crate::prep_cut::F::CheckClose, &mut t);
            list.cast()
        }
    }
}

/// A plan's lists left to record: the sync list and the compare, each with
/// its list (a second reference to the plan's own).
pub(super) struct Pending {
    pub(super) sync: Option<(SyncRec, Prelude)>,
    pub(super) check: Option<(CheckRec, Prelude)>,
}

// SAFETY: D3D12 objects (free-threaded) and copies kept alive by their
// `Arc`s; the lists are the plan's own, recorded by one thread (the one the
// pending recording is handed to) before they go to the queue.
unsafe impl Send for Pending {}

impl Pending {
    /// Records both lists, the sync list first; `Err` with the text a failure
    /// inline gave.
    pub(super) fn record(&self) -> Result<(), String> {
        if let Some((rec, sync)) = &self.sync {
            // SAFETY: the plan's own list, given back only after its run
            // (`Host::card_done`); the plan keeps the copies alive.
            if let Err(e) = unsafe { rec.record(sync) } {
                return Err(format!(
                    "host copies in the card's memory could not be brought up to date ({e})"
                ));
            }
        }
        if let Some((rec, check)) = &self.check {
            // SAFETY: as above.
            if let Err(e) = unsafe { rec.record(check) } {
                return Err(format!(
                    "the compare of host copies in the card's memory could not be recorded ({e})"
                ));
            }
        }
        Ok(())
    }

    /// Recorded on the thread that sends (`inline` false) or here.
    pub(super) fn counted(inline: bool) {
        HANDED.fetch_add(1, Ordering::Relaxed);
        if inline {
            INLINE.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// The line, per frame, while the switch is on or anything was handed.
pub(super) fn line(frames: f64) {
    let handed = HANDED.swap(0, Ordering::Relaxed);
    let inline = INLINE.swap(0, Ordering::Relaxed);
    if handed == 0 && !on() {
        return;
    }
    let frames = frames.max(1.0);
    tracing::info!(
        "card lists recorded by the thread that sends per frame (VICTORY_CARD_RECORD_SEND=1): {:.1} plans, {:.1} of them recorded inline",
        (handed - inline) as f64 / frames,
        inline as f64 / frames,
    );
}
