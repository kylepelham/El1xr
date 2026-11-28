//! [`Host`]'s side of host copies in the card's memory: making the
//! compare and its lists, the compare before a submission
//! ([`Host::card_before`]), and the relay's fetch of what it kept
//! ([`Host::relay_card_back`], [`Host::relay_card_inflight`]).

use super::*;

impl Host {
    pub(super) fn make_card_work(&self) -> windows::core::Result<CardWork> {
        use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
        use windows::Win32::Graphics::Direct3D::ID3DBlob;
        let mut code: Option<ID3DBlob> = None;
        // SAFETY: the source and names live through the call; the out
        // pointer is ours.
        unsafe {
            D3DCompile(
                CARD_COMPARE.as_ptr().cast(),
                CARD_COMPARE.len(),
                windows::core::s!("card compare"),
                None,
                None,
                windows::core::s!("main"),
                windows::core::s!("cs_5_1"),
                0,
                0,
                &mut code,
                None,
            )?;
        }
        let code = code.ok_or_else(windows::core::Error::empty)?;
        let list = || self.make_prelude().ok_or_else(windows::core::Error::empty);
        // A megabyte of results: 262,144 pieces, 16 GB of declared ranges
        // in one submission.
        const RESULTS: u64 = 1 << 20;
        let (differs, data) = self
            .shared_buffer(RESULTS)
            .ok_or_else(windows::core::Error::empty)?;
        name(&differs, || "card compare results".into());
        // SAFETY: the blob's own bytes; the root signature is inside the
        // shader.
        unsafe {
            let bytes = std::slice::from_raw_parts(
                code.GetBufferPointer().cast::<u8>(),
                code.GetBufferSize(),
            );
            let root: ID3D12RootSignature = self.device.CreateRootSignature(0, bytes)?;
            let pso: ID3D12PipelineState =
                self.device
                    .CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                        pRootSignature: std::mem::transmute_copy(&root),
                        CS: D3D12_SHADER_BYTECODE {
                            pShaderBytecode: bytes.as_ptr().cast(),
                            BytecodeLength: bytes.len(),
                        },
                        ..Default::default()
                    })?;
            let snap = relay_own_queue().then(|| self.make_card_snap()).flatten();
            let patch = under_writes_masks()
                .then(|| self.make_card_patch())
                .flatten();
            Ok(CardWork {
                sync: vec![list()?],
                check: vec![list()?],
                fetch: list()?,
                root,
                pso,
                differs: (differs, data as usize, RESULTS),
                fetched: None,
                taken: crate::result_ring::Ring::new((RESULTS / 4) as u32),
                out: 0,
                snap,
                patch,
                masks: Vec::new(),
                inflight: if inflight_fetch() {
                    self.make_card_inflight(RESULTS)
                } else {
                    None
                },
            })
        }
    }

    /// [`CardInflight`] for `results` bytes of the compare's results; `None`
    /// (and the old fetch) if the shader or the memory cannot be had.
    fn make_card_inflight(&self, results: u64) -> Option<CardInflight> {
        use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
        use windows::Win32::Graphics::Direct3D::ID3DBlob;
        let made = (|| -> windows::core::Result<CardInflight> {
            let mut code: Option<ID3DBlob> = None;
            // SAFETY: the source and names live through the call; the out
            // pointer is ours.
            unsafe {
                D3DCompile(
                    CARD_FETCH.as_ptr().cast(),
                    CARD_FETCH.len(),
                    windows::core::s!("card fetch in flight"),
                    None,
                    None,
                    windows::core::s!("main"),
                    windows::core::s!("cs_5_1"),
                    0,
                    0,
                    &mut code,
                    None,
                )?;
            }
            let code = code.ok_or_else(windows::core::Error::empty)?;
            let slots = inflight_slots(inflight_fetch_bytes());
            let (table, table_data) = self
                .shared_buffer(4 + results)
                .ok_or_else(windows::core::Error::empty)?;
            name(&table, || "card fetch in flight: tickets".into());
            let fetched_bytes = u64::from(slots) * INFLIGHT_SLOT;
            let (fetched, fetched_data) = self
                .shared_buffer(fetched_bytes)
                .ok_or_else(windows::core::Error::empty)?;
            name(&fetched, || "card fetch in flight: pieces".into());
            // SAFETY: the mapping holds the counter; nothing of ours ran
            // on the card with it yet.
            unsafe { table_data.cast::<u32>().write_volatile(0) };
            // SAFETY: the blob's own bytes; the root signature is inside
            // the shader.
            unsafe {
                let bytes = std::slice::from_raw_parts(
                    code.GetBufferPointer().cast::<u8>(),
                    code.GetBufferSize(),
                );
                let root: ID3D12RootSignature = self.device.CreateRootSignature(0, bytes)?;
                let pso: ID3D12PipelineState =
                    self.device
                        .CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                            pRootSignature: std::mem::transmute_copy(&root),
                            CS: D3D12_SHADER_BYTECODE {
                                pShaderBytecode: bytes.as_ptr().cast(),
                                BytecodeLength: bytes.len(),
                            },
                            ..Default::default()
                        })?;
                Ok(CardInflight {
                    root,
                    pso,
                    table: (table, table_data as usize, 4 + results),
                    fetched: (fetched, fetched_data as usize, slots),
                    landed: 0,
                    brought: Vec::new(),
                })
            }
        })();
        match made {
            Ok(inflight) => {
                tracing::info!(
                    slots = inflight.fetched.2,
                    "card copies' flagged pieces come down inside their flights (VICTORY_CARD_FETCH_INFLIGHT=1)"
                );
                Some(inflight)
            }
            Err(e) => {
                tracing::warn!(error = %e, "no fetch inside flights (VICTORY_CARD_FETCH_INFLIGHT=1): pieces are fetched the old way");
                None
            }
        }
    }

    /// (Neptune) The masked pass ([`under_writes_masks`]); `None`, with a
    /// warning, if it cannot be made: every range then goes up as a copy.
    fn make_card_patch(&self) -> Option<(ID3D12RootSignature, ID3D12PipelineState)> {
        use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
        use windows::Win32::Graphics::Direct3D::ID3DBlob;
        let made = (|| -> windows::core::Result<(ID3D12RootSignature, ID3D12PipelineState)> {
            let mut code: Option<ID3DBlob> = None;
            // SAFETY: the source and names live through the call; the out
            // pointer is ours. The blob's own bytes; the root signature is
            // inside the shader.
            unsafe {
                D3DCompile(
                    CARD_PATCH.as_ptr().cast(),
                    CARD_PATCH.len(),
                    windows::core::s!("card patch"),
                    None,
                    None,
                    windows::core::s!("main"),
                    windows::core::s!("cs_5_1"),
                    0,
                    0,
                    &mut code,
                    None,
                )?;
                let code = code.ok_or_else(windows::core::Error::empty)?;
                let bytes = std::slice::from_raw_parts(
                    code.GetBufferPointer().cast::<u8>(),
                    code.GetBufferSize(),
                );
                let root: ID3D12RootSignature = self.device.CreateRootSignature(0, bytes)?;
                let pso: ID3D12PipelineState =
                    self.device
                        .CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                            pRootSignature: std::mem::transmute_copy(&root),
                            CS: D3D12_SHADER_BYTECODE {
                                pShaderBytecode: bytes.as_ptr().cast(),
                                BytecodeLength: bytes.len(),
                            },
                            ..Default::default()
                        })?;
                Ok((root, pso))
            }
        })();
        match made {
            Ok(patch) => Some(patch),
            Err(e) => {
                tracing::warn!(error = %e, "VICTORY_UNDER_WRITES_MASKS=1: the masked pass could not be made; every range goes up as a copy");
                None
            }
        }
    }

    /// (Neptune) The compare that keeps the pieces it flags, and their
    /// buffer on the card ([`relay_own_queue`]); `None`, with a warning, if
    /// either cannot be made: pieces are then fetched as before.
    fn make_card_snap(&self) -> Option<CardSnap> {
        use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
        use windows::Win32::Graphics::Direct3D::ID3DBlob;
        let source = format!("#define SNAP 1\n{CARD_COMPARE}");
        let slots = snap_slots();
        let made = (|| -> windows::core::Result<CardSnap> {
            let mut code: Option<ID3DBlob> = None;
            // SAFETY: the source and names live through the call; the out
            // pointer is ours. The blob's own bytes; the root signature is
            // inside the shader.
            unsafe {
                D3DCompile(
                    source.as_ptr().cast(),
                    source.len(),
                    windows::core::s!("card compare, keeping"),
                    None,
                    None,
                    windows::core::s!("main"),
                    windows::core::s!("cs_5_1"),
                    0,
                    0,
                    &mut code,
                    None,
                )?;
                let code = code.ok_or_else(windows::core::Error::empty)?;
                let bytes = std::slice::from_raw_parts(
                    code.GetBufferPointer().cast::<u8>(),
                    code.GetBufferSize(),
                );
                let root: ID3D12RootSignature = self.device.CreateRootSignature(0, bytes)?;
                let pso: ID3D12PipelineState =
                    self.device
                        .CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                            pRootSignature: std::mem::transmute_copy(&root),
                            CS: D3D12_SHADER_BYTECODE {
                                pShaderBytecode: bytes.as_ptr().cast(),
                                BytecodeLength: bytes.len(),
                            },
                            ..Default::default()
                        })?;
                // The count starts at 0, as `SNAP_DONE` does: written, a
                // new buffer is only "almost always" zeros (`zero_fill`).
                let buffer = self
                    .uav_buffer(SNAP_AT + u64::from(slots) * SNAP_SLOT)
                    .ok_or_else(windows::core::Error::empty)?;
                crate::zero_fill::zero_buffer(&self.device, &buffer, SNAP_AT)?;
                name(&buffer, || "card compare, pieces kept for the fetch".into());
                Ok(CardSnap {
                    root,
                    pso,
                    buffer,
                    slots,
                })
            }
        })();
        match made {
            Ok(snap) => {
                tracing::info!(
                    slots,
                    mb = (SNAP_AT + u64::from(slots) * SNAP_SLOT) >> 20,
                    "the compare keeps the pieces it flags on the card, for a fetch on a queue of its own (VICTORY_RELAY_OWN_QUEUE=1)"
                );
                Some(snap)
            }
            Err(e) => {
                tracing::warn!(error = %e, "VICTORY_RELAY_OWN_QUEUE=1: the compare that keeps pieces could not be made; pieces are fetched as before");
                None
            }
        }
    }

    /// Before a submission's lists run, for copies in the card's memory
    /// ([`crate::shadow::cards`]): what changed on their CPU side since
    /// the last submission goes to their card side (a list of ours, run
    /// now; the submission's lists follow it on the queue), and the ranges
    /// this submission's shaders may write get a compare to run after its
    /// lists. `None` if there is nothing to do.
    ///
    /// `flown`: the submission is sent ahead with [`relay`]. Its compare
    /// then stores the hashes it makes (see [`flagged_pieces`] and
    /// `relay`).
    ///
    /// `defer` (`VICTORY_CARD_RECORD_SEND`): the two lists are put in the
    /// plan unrecorded and their recordings given back, to be recorded
    /// before the submission's lists go to the queue ([`super::card_rec`]).
    pub(super) fn card_before(
        &self,
        runs: &[&HostList],
        flown: bool,
        defer: bool,
    ) -> (
        Option<CardPlan>,
        Option<std::sync::MutexGuard<'static, u64>>,
        Option<super::card_rec::Pending>,
    ) {
        use super::card_rec::{CheckRec, Pending, SyncRec};
        use std::sync::Arc;
        if !crate::shadow::cards() {
            return (None, None, None);
        }
        use crate::card_prof::P;
        let mut lap = crate::card_prof::Lap::new();
        // The ranges this submission's shaders may write, per copy.
        let mut declared: Vec<(Arc<crate::shadow::Shadow>, Vec<(usize, usize)>)> = Vec::new();
        let mut decl_index: std::collections::HashMap<usize, usize> =
            std::collections::HashMap::new();
        // Each copy's ranges sorted and joined here, once (`decl_join`).
        let joined_decl = crate::card_prof::decl_join();
        if joined_decl {
            let mut join = crate::card_prof::DeclJoin::new();
            let mut gathered = 0u64;
            for list in runs {
                if step_index() {
                    for (shadow, ranges) in &list.deferred().gpu_writes {
                        if !shadow.carded() || shadow.transient() {
                            continue;
                        }
                        gathered += ranges.len() as u64;
                        join.add(Arc::as_ptr(shadow) as usize, || shadow.clone(), ranges);
                    }
                } else {
                    for step in &list.deferred().copies {
                        if let Step::GpuWrite(shadow, address, len) = step
                            && shadow.carded()
                            && !shadow.transient()
                        {
                            gathered += 1;
                            let range = (*address, *address + *len);
                            join.add(Arc::as_ptr(shadow) as usize, || shadow.clone(), &[range]);
                        }
                    }
                }
            }
            declared = join.finish();
            crate::card_prof::add(P::DeclIn, gathered);
        } else if step_index() {
            for list in runs {
                for (shadow, ranges) in &list.deferred().gpu_writes {
                    if !shadow.carded() || shadow.transient() {
                        continue;
                    }
                    let key = Arc::as_ptr(shadow) as usize;
                    if let Some(&i) = decl_index.get(&key) {
                        let dst = &mut declared[i].1;
                        for &range in ranges {
                            if !dst.contains(&range) {
                                dst.push(range);
                            }
                        }
                    } else {
                        decl_index.insert(key, declared.len());
                        declared.push((shadow.clone(), ranges.clone()));
                    }
                }
            }
        } else {
            for list in runs {
                for step in &list.deferred().copies {
                    if let Step::GpuWrite(shadow, address, len) = step
                        && shadow.carded()
                        && !shadow.transient()
                    {
                        let range = (*address, *address + *len);
                        match declared.iter_mut().find(|(s, _)| Arc::ptr_eq(s, shadow)) {
                            Some((_, ranges)) => {
                                if !ranges.contains(&range) {
                                    ranges.push(range);
                                }
                            }
                            None => declared.push((shadow.clone(), vec![range])),
                        }
                    }
                }
            }
        }
        if !joined_decl && crate::card_prof::on() {
            let gathered: usize = declared.iter().map(|(_, r)| r.len()).sum();
            crate::card_prof::add(P::DeclIn, gathered as u64);
        }
        lap.mark(P::Declared);
        // With the relay: the order of the queue is held from here until
        // what is taken is on the queue, and the taking is numbered.
        let mut order = relay().then(|| QUEUE_ORDER.lock().unwrap_or_else(|e| e.into_inner()));
        lap.mark(P::Order);
        let sent_as = match order.as_mut() {
            Some(order) => {
                **order += 1;
                **order
            }
            None => 0,
        };
        let (mut dirty, uploads) = crate::shadow::card_dirty(sent_as);
        lap.mark(P::Dirty);
        if declared.is_empty() && dirty.is_empty() && uploads.is_empty() {
            return (None, None, None);
        }
        let mut work = CARD_WORK.lock().unwrap_or_else(|e| e.into_inner());
        lap.mark(P::Work);
        let Some(work) = work.as_mut() else {
            return (None, None, None);
        };
        if crate::card_prof::on() {
            crate::card_prof::add(P::Plans, 1);
            crate::card_prof::add(P::DirtyCopies, dirty.len() as u64);
            let ranges: usize = dirty.iter().map(|(_, r)| r.len()).sum();
            crate::card_prof::add(P::DirtyRanges, ranges as u64);
        }
        // A copy whose shader writes are declared for the first time gets
        // its bytes from before, whole.
        let mut whole: Vec<Arc<crate::shadow::Shadow>> = Vec::new();
        for (shadow, _) in &declared {
            if shadow.card_make_before(|size| {
                let before = self.uav_buffer(crate::shadow::card_hash_bytes(size))?;
                name(&before, || {
                    "title buffer on the card, hashes of its pieces".into()
                });
                Some(before)
            }) {
                whole.push(shadow.clone());
                if !dirty.iter().any(|(s, _)| Arc::ptr_eq(s, shadow)) {
                    dirty.push((shadow.clone(), Vec::new()));
                }
            }
        }
        lap.mark(P::Before);
        // (entryhitch) `VICTORY_CARD_CHECK_ASIDE`: the compare's ranges placed
        // now, and its list recorded on a kept thread beside the sync list.
        let aside = crate::prep_cut::check_aside() && (!dirty.is_empty() || !uploads.is_empty());
        let mut early: Option<(Vec<CardRange>, Option<Prelude>)> = None;
        let mut results = (0, 0);
        if aside {
            let ranges;
            (ranges, results) = place_ranges(&mut work.taken, work.out, &declared, joined_decl);
            lap.mark(P::Spans);
            let check = ranges.iter().any(|r| r.compared).then(|| {
                work.check.pop().or_else(|| self.make_prelude()).unwrap_or_else(|| {
                    fail(format_args!(
                        "no host list for the compare of host copies in the card's memory"
                    ))
                })
            });
            early = Some((ranges, check));
        }
        let early_keep = early.as_ref().and_then(|_| keep_of(&work.snap, flown));
        let mut checked: Option<CheckRecorded> = None;
        let mut used: Vec<(bool, Prelude)> = Vec::new();
        let mut sync_list: Option<ID3D12CommandList> = None;
        let mut pending_sync: Option<(SyncRec, Prelude)> = None;
        let mut pending_check: Option<(CheckRec, Prelude)> = None;
        // (Neptune) The masks this plan's send reads (`under_writes_masks`).
        let mut masks: Option<(ID3D12Resource, usize, u64)> = None;
        if !dirty.is_empty() || !uploads.is_empty() {
            let Some(sync) = work.sync.pop().or_else(|| self.make_prelude()) else {
                fail(format_args!(
                    "no host list to bring host copies in the card's memory up to date"
                ));
            };
            // (Neptune) Per copy, what goes up as copies and what by mask
            // (`under_writes_masks`): the blocks in runs that follow one
            // another (first block, blocks, where their masks begin), the
            // masks written one after the other into a buffer of the plan's.
            type Sends = (Option<Vec<(usize, usize)>>, Vec<(usize, usize, usize)>);
            let mut sends: Vec<Sends> = Vec::with_capacity(dirty.len());
            if work.patch.is_some() {
                let least = crate::prep_cut::masks_least().unwrap_or_else(masks_least);
                let split: Vec<_> = dirty
                    .iter()
                    .map(|(_, ranges)| masked_blocks_slices(ranges, least))
                    .collect();
                let blocks: usize = split.iter().map(|s| s.1.len()).sum();
                if blocks > 0 {
                    // A mask and a table entry for each block.
                    let need = (blocks as u64 * (512 + 8)).next_multiple_of(64 << 10);
                    let have = match work.masks.iter().position(|m| m.2 >= need) {
                        Some(i) => Some(work.masks.swap_remove(i)),
                        None => self.shared_buffer(need).map(|(buffer, data)| {
                            name(&buffer, || "card patch masks".into());
                            (buffer, data as usize, need)
                        }),
                    };
                    if let Some((buffer, data, bytes)) = have {
                        let mut at = 0usize;
                        for (copies, masked) in split {
                            let mut entries: Vec<[u32; 2]> = Vec::with_capacity(masked.len());
                            for &(block, ranges) in &masked {
                                // SAFETY: the mapping is cached write-back memory
                                // holding `bytes`, and `blocks` masks and table
                                // entries fit in it (`need`); no list on the card
                                // reads this buffer (it came back with `card_done`,
                                // or is new).
                                let dst = unsafe { &mut *((data + at) as *mut [u8; 512]) };
                                block_mask_into(block, ranges, dst);
                                entries.push([(block * 4096) as u32, at as u32]);
                                at += 512;
                                CARD_MASKED[1].fetch_add(ranges.len() as u64, Ordering::Relaxed);
                            }
                            let mut runs: Vec<(usize, usize, usize)> = Vec::new();
                            for pass in entries.chunks(4000) {
                                // SAFETY: as above; the entries are plain words.
                                unsafe {
                                    std::ptr::copy_nonoverlapping(
                                        pass.as_ptr().cast::<u8>(),
                                        (data + at) as *mut u8,
                                        pass.len() * 8,
                                    )
                                };
                                runs.push((at, pass.len(), 0));
                                at += pass.len() * 8;
                            }
                            CARD_MASKED[0].fetch_add(masked.len() as u64, Ordering::Relaxed);
                            crate::card_prof::add(P::MaskBlocks, masked.len() as u64);
                            CARD_MASKED_PASSES.fetch_add(runs.len() as u64, Ordering::Relaxed);
                            sends.push((Some(copies), runs));
                        }
                        debug_assert!(at as u64 <= bytes);
                        masks = Some((buffer, data, bytes));
                    }
                }
            }
            if sends.is_empty() {
                sends.resize_with(dirty.len(), || (None, Vec::new()));
            }
            lap.mark(P::Masks);
            let rec = SyncRec::new(work, &mut dirty, sends, &uploads, masks.as_ref(), &whole);
            // SAFETY (both recordings): the list's last run finished (it ran
            // ahead of a submission that was waited for, and came back after
            // that); live buffers, and the ranges lie in them.
            let recorded = match &early {
                _ if defer => {
                    pending_sync = Some((rec, sync.clone()));
                    sync.1.cast()
                }
                Some((ranges, Some(check))) => {
                    crate::prep_cut::add(crate::prep_cut::F::Aside, 1);
                    let check_rec = CheckRec::new(work, ranges, early_keep, flown);
                    let inflight = check_rec.inflight();
                    let (recorded, check) = crate::prep_cut::both_out(
                        || unsafe { rec.record(&sync) },
                        || (unsafe { check_rec.record(check) }, inflight),
                    );
                    checked = Some(check);
                    recorded
                }
                _ => unsafe { rec.record(&sync) },
            };
            match recorded {
                Ok(cmd) => sync_list = Some(cmd),
                Err(e) => fail(format_args!(
                    "host copies in the card's memory could not be brought up to date ({e})"
                )),
            }
            used.push((false, sync));
        }
        lap.mark(P::Sync);
        let mut plan = CardPlan {
            ranges: Vec::new(),
            sync: sync_list,
            check: None,
            _synced: dirty
                .into_iter()
                .map(|(shadow, _)| shadow)
                .chain(uploads.iter().map(|(shadow, _)| shadow.clone()))
                .collect(),
            _uploads: uploads
                .into_iter()
                .flat_map(|(_, parts)| parts.into_iter().map(|p| p.2))
                .collect(),
            lists: used,
            sent_as,
            snap: None,
            masks,
            inflight: false,
            inflight_landed: 0,
            results: (0, 0),
        };
        // Each byte once, as the copy-back joins the declared ranges. The
        // results go behind those of plans still in the air.
        let (ranges, early_check) = match early {
            Some((ranges, check)) => (ranges, check),
            None => {
                let ranges;
                (ranges, results) = place_ranges(&mut work.taken, work.out, &declared, joined_decl);
                (ranges, None)
            }
        };
        plan.ranges = ranges;
        plan.results = results;
        lap.mark(P::Spans);
        if plan.ranges.iter().any(|r| r.compared) {
            let Some(check) = early_check
                .or_else(|| work.check.pop())
                .or_else(|| self.make_prelude())
            else {
                fail(format_args!(
                    "no host list for the compare of host copies in the card's memory"
                ));
            };
            // (Neptune) Sent ahead, with a queue of the fetch's own: the
            // compare that keeps what it flags, told which slots are free
            // (`relay_own_queue`). The count of slots done with only grows,
            // so one read a moment ago is on the safe side.
            let keep = if checked.is_some() { early_keep } else { keep_of(&work.snap, flown) };
            let (recorded, inflight) = match checked {
                Some(done) => done,
                None => {
                    let rec = CheckRec::new(work, &plan.ranges, keep, flown);
                    let inflight = rec.inflight();
                    if defer {
                        pending_check = Some((rec, check.clone()));
                        (check.1.cast(), inflight)
                    } else {
                        // SAFETY: the list's last run finished (its submission
                        // was waited for, and it came back after that); live
                        // buffers; the compare's own root signature.
                        (unsafe { rec.record(&check) }, inflight)
                    }
                }
            };
            match recorded {
                Ok(list) => plan.check = Some(list),
                Err(e) => fail(format_args!(
                    "the compare of host copies in the card's memory could not be recorded ({e})"
                )),
            }
            if let Some((every, landed)) = inflight {
                plan.inflight = every;
                plan.inflight_landed = landed;
            }
            plan.snap = keep.map(|(_, base)| base);
            plan.lists.push((true, check));
        }
        lap.mark(P::Check);
        work.out += 1;
        let pending = (pending_sync.is_some() || pending_check.is_some()).then_some(Pending {
            sync: pending_sync,
            check: pending_check,
        });
        (Some(plan), order, pending)
    }

    /// With [`relay`]: whether the compare's result area is half taken by
    /// plans in the air. It starts from the beginning only when no plan is
    /// out, so the caller lets the flights land first.
    /// ([`crate::result_ring`]: three quarters, as a ring.)
    pub(super) fn card_results_low(&self) -> bool {
        CARD_WORK
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_some_and(|work| work.taken.wants_landing(work.out, crate::result_ring::on()))
    }

    /// The plan's submission has run and its results were read
    /// ([`Host::card_back`]): its lists are free for the next plan. Called
    /// when nothing of ours is on the card, so the compare's results start
    /// from the beginning again.
    pub(super) fn card_done(&self, plan: CardPlan) {
        let mut work = CARD_WORK.lock().unwrap_or_else(|e| e.into_inner());
        let Some(work) = work.as_mut() else {
            return;
        };
        for (check, list) in plan.lists {
            if check {
                work.check.push(list)
            } else {
                work.sync.push(list)
            }
        }
        work.masks.extend(plan.masks);
        work.out = work.out.saturating_sub(1);
        // With the relay other plans may be on the card, their results
        // not read yet: the area starts again only when none is out (as a
        // ring, the plan's own stretch comes back now).
        work.taken.give(plan.results, !relay() || work.out == 0);
    }

    /// The relay's own list, fence, event for [`Host::relay_card_back`].
    fn make_relay_fetch(&self) -> Option<RelayFetch> {
        let list = self.make_prelude()?;
        // SAFETY: plain objects of our own.
        unsafe {
            let fence: ID3D12Fence = self.device.CreateFence(0, D3D12_FENCE_FLAG_NONE).ok()?;
            let event = CreateEventW(None, false, false, None).ok()?;
            Some(RelayFetch {
                list,
                fence,
                event,
                sent: 0,
                buffer: None,
                own: relay_own_queue().then(|| self.make_own_queue()).flatten(),
            })
        }
    }

    /// (Neptune) The queue of the fetch's own ([`relay_own_queue`]);
    /// `None`, with a warning, if the host gives none: pieces are then
    /// fetched as before.
    fn make_own_queue(&self) -> Option<OwnQueue> {
        // SAFETY: plain objects of our own.
        let made = unsafe {
            (|| -> windows::core::Result<OwnQueue> {
                let queue: ID3D12CommandQueue =
                    self.device.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                        Type: D3D12_COMMAND_LIST_TYPE_COPY,
                        ..Default::default()
                    })?;
                name(&queue, || {
                    "the queue that fetches the compare's kept pieces".into()
                });
                let allocator: ID3D12CommandAllocator = self
                    .device
                    .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_COPY)?;
                crate::alloc_count::track(&allocator);
                let list: ID3D12GraphicsCommandList = self.device.CreateCommandList(
                    0,
                    D3D12_COMMAND_LIST_TYPE_COPY,
                    &allocator,
                    None,
                )?;
                list.Close()?;
                let fence: ID3D12Fence = self.device.CreateFence(0, D3D12_FENCE_FLAG_NONE)?;
                let event = CreateEventW(None, false, false, None)?;
                Ok(OwnQueue {
                    queue,
                    list: (allocator, list),
                    fence,
                    event,
                    sent: 0,
                    buffer: None,
                })
            })()
        };
        match made {
            Ok(own) => Some(own),
            Err(e) => {
                tracing::warn!(error = %e, "VICTORY_RELAY_OWN_QUEUE=1: no copy queue for the fetch; pieces are fetched as before");
                None
            }
        }
    }

    /// [`relay`], on the thread that lands flights: for the plans of the
    /// flights being landed (the card has run them), the pieces their
    /// compares flagged are fetched, once each, with a list and a wait of
    /// this thread's own, and what shaders wrote in them goes into title
    /// memory (`Shadow::card_merge_relay`). As [`Host::card_back`], but:
    /// no submission lock; a piece flagged under one range of a plan is
    /// fetched under all ([`flagged_pieces`]); the fetch runs behind
    /// whatever was sent meanwhile, so it may bring a later flight's
    /// bytes, which that flight's landing then finds merged already; and
    /// nothing is sent back up.
    ///
    /// (Neptune) With [`relay_own_queue`] a piece its compare kept is
    /// fetched from the kept copy, on the queue of this thread's own, and
    /// merged with that plan's send number: it is the card's bytes right
    /// behind that flight, whatever the first queue has run since. Of
    /// several landed plans that flagged a piece the last one's copy is
    /// taken (it holds what the earlier flights wrote and what was sent up
    /// between them). A piece whose last compare found no slot comes from
    /// the live buffer on the first queue, as before.
    ///
    /// With [`inflight_fetch`] the pieces are first looked for where the
    /// flights' own compare lists put them ([`Host::relay_card_inflight`]);
    /// only a landing that cannot take them from there fetches, as here.
    pub(super) fn relay_card_back(&self, plans: &[&CardPlan], fetch: &mut Option<RelayFetch>) {
        use std::sync::Arc;
        if inflight_fetch() {
            let landed = self.relay_card_inflight(plans);
            inflight_tickets_landed(plans);
            if landed {
                return;
            }
        }
        let (differs, snaps) = {
            let work = CARD_WORK.lock().unwrap_or_else(|e| e.into_inner());
            match work.as_ref() {
                Some(work) => (
                    Some((work.differs.1, (work.differs.2 / 4) as usize)),
                    work.snap
                        .as_ref()
                        .map(|snap| (snap.buffer.clone(), snap.slots)),
                ),
                None => (None, None),
            }
        };
        // (Neptune) Compares keep pieces: plans in the air may hold some,
        // to be merged with their own send numbers.
        let keeping = snaps.is_some();
        if keeping && fetch.is_none() {
            *fetch = self.make_relay_fetch();
        }
        let own = keeping
            && fetch.as_ref().is_some_and(|f| f.own.is_some())
            && !SNAP_OFF.load(Ordering::Acquire);
        // From the live buffers: copy, offsets, where the bytes come down.
        let mut pieces: Vec<(Arc<crate::shadow::Shadow>, usize, usize, u64)> = Vec::new();
        let mut total = 0u64;
        // (Neptune) From the compare's kept copies: copy, offsets, where the
        // bytes come down, where they are kept, their plan's send number.
        let mut kept: Vec<(Arc<crate::shadow::Shadow>, usize, usize, u64, u64, u64)> = Vec::new();
        let mut kept_total = 0u64;
        // (Neptune) The slots these plans' compares asked for.
        let mut handed = 0u32;
        if let Some((differs, dwords)) = differs {
            // SAFETY: the mapping of the results. The compares of these
            // plans have run, and their dwords are given to no other plan
            // before these are done with (`card_done`).
            let differs = unsafe { std::slice::from_raw_parts(differs as *const u32, dwords) };
            // Per plan, the pieces to fetch; and for each piece the last
            // plan that flagged it, with the number of the slot its compare
            // kept it in, if it did.
            let mut flagged: Vec<std::collections::HashSet<(usize, u32)>> =
                Vec::with_capacity(plans.len());
            let mut last: std::collections::HashMap<(usize, u32), (usize, Option<u32>)> =
                std::collections::HashMap::new();
            let mut done = SNAP_DONE.load(Ordering::Acquire);
            let mut adds_up = true;
            for (k, plan) in plans.iter().enumerate() {
                let mut flags: Vec<(usize, u32, bool)> = Vec::new();
                let mut numbers: Vec<((usize, u32), u32)> = Vec::new();
                let mut count = 0u32;
                let mut counted = std::collections::HashSet::new();
                for r in &plan.ranges {
                    let copy = Arc::as_ptr(&r.shadow) as usize;
                    for p in 0..r.span.pieces {
                        let d = if r.compared {
                            differs.get((r.at + p) as usize).copied().unwrap_or(1)
                        } else {
                            1
                        };
                        flags.push((copy, r.span.first + p, d != 0));
                        if let Some(base) = plan.snap
                            && r.compared
                            && d != 0
                            && counted.insert(r.at + p)
                        {
                            count = count.wrapping_add(1);
                            if d >= 2 {
                                numbers.push(((copy, r.span.first + p), base.wrapping_add(d - 2)));
                            }
                        }
                    }
                }
                let fetched = flagged_pieces(&flags);
                for key in &fetched {
                    last.insert(*key, (k, None));
                }
                // The card counts one for each piece a keeping compare
                // flags, in the queue's order, which is the order flights
                // land in: this plan's numbers are the `count` from `done`.
                adds_up &= numbers
                    .iter()
                    .all(|(_, number)| number.wrapping_sub(done) < count);
                for (key, number) in numbers {
                    last.insert(key, (k, Some(number)));
                }
                done = done.wrapping_add(count);
                handed = handed.wrapping_add(count);
                flagged.push(fetched);
            }
            if !adds_up && !SNAP_OFF.swap(true, Ordering::AcqRel) {
                tracing::warn!(
                    "VICTORY_RELAY_OWN_QUEUE=1: the numbers of the compare's kept pieces do not add up; \
                     no compare keeps pieces from here on, and pieces are fetched as before"
                );
            }
            let own = own && adds_up;
            let slots = snaps.as_ref().map_or(0, |s| s.1);
            let mut seen = std::collections::HashSet::new();
            for (plan, fetched) in plans.iter().zip(&flagged) {
                for r in &plan.ranges {
                    let copy = Arc::as_ptr(&r.shadow) as usize;
                    for p in 0..r.span.pieces {
                        let key = (copy, r.span.first + p);
                        if !fetched.contains(&key) {
                            continue;
                        }
                        let (lo, hi) = r.shadow.card_piece(&r.span, p);
                        if lo >= hi
                            || crate::wc_skip::skip_back(r.declared, (hi - lo) as u64)
                            || !seen.insert((copy, lo, hi))
                        {
                            continue;
                        }
                        // Where the piece's last compare kept these bytes,
                        // and the send number of that compare's plan.
                        let slot = last.get(&key).and_then(|&(k, number)| {
                            let number = number.filter(|_| own)?;
                            let within =
                                lo.checked_sub(snap_piece_from(key.1, r.span.skew))? as u64;
                            (within + (hi - lo) as u64 <= SNAP_SLOT).then(|| {
                                (
                                    SNAP_AT
                                        + u64::from(number & slots.wrapping_sub(1)) * SNAP_SLOT
                                        + within,
                                    plans[k].sent_as,
                                )
                            })
                        });
                        match slot {
                            Some((from, after)) => {
                                kept.push((r.shadow.clone(), lo, hi, kept_total, from, after));
                                kept_total += (hi - lo) as u64;
                            }
                            None => {
                                pieces.push((r.shadow.clone(), lo, hi, total));
                                total += (hi - lo) as u64;
                            }
                        }
                    }
                    crate::shadow::CARD_STATS[2]
                        .fetch_add(u64::from(r.span.pieces), Ordering::Relaxed);
                }
            }
        }
        crate::shadow::lazy::filter_card_pieces(&mut pieces, &mut total);
        crate::shadow::lazy::filter_card_kept(&mut kept, &mut kept_total);
        // (Neptune) With kept pieces about, the notes of the sends behind
        // the last flight landed stay: the flights in the air merge their
        // kept pieces with their own send numbers (`relay_own_queue`, 4).
        let last_sent = plans.iter().map(|plan| plan.sent_as).max();
        if pieces.is_empty() && kept.is_empty() {
            if keeping || inflight_fetch() {
                if let Some(sent) = last_sent {
                    crate::shadow::relay_fetched(sent);
                }
            } else {
                // Nothing to fetch: every send so far is older than the next fetch.
                crate::shadow::relay_fetched(
                    *QUEUE_ORDER.lock().unwrap_or_else(|e| e.into_inner()),
                );
            }
            SNAP_DONE.fetch_add(handed, Ordering::AcqRel);
            return;
        }
        if inflight_fetch() {
            INFLIGHT_STATS[3].fetch_add(1, Ordering::Relaxed);
        }
        if fetch.is_none() {
            *fetch = self.make_relay_fetch();
        }
        let Some(f) = fetch.as_mut() else {
            fail(format_args!(
                "no list, fence or event to fetch what shaders wrote to the card's memory (VICTORY_RELAY=1)"
            ));
        };
        let waiting = std::time::Instant::now();
        // (Neptune) The kept pieces, on the queue of their own: nothing of
        // the first queue is waited for, and no order is taken (the slots
        // are written by nothing until `SNAP_DONE` passes them, below).
        let mut kept_down: Option<(usize, HANDLE)> = None;
        if !kept.is_empty() {
            let (Some((snaps, _)), Some(own)) = (snaps.as_ref(), f.own.as_mut()) else {
                fail(format_args!(
                    "pieces kept by the compare and no queue to fetch them with (VICTORY_RELAY_OWN_QUEUE=1)"
                ));
            };
            if own.buffer.as_ref().is_none_or(|b| b.2 < kept_total) {
                let size = kept_total.next_multiple_of(16 << 20);
                own.buffer = self
                    .shared_buffer(size)
                    .map(|(buffer, data)| (buffer, data as usize, size));
            }
            let Some((buffer, data, _)) = own.buffer.clone() else {
                fail(format_args!(
                    "no memory to fetch the pieces the compare kept ({kept_total} bytes, VICTORY_RELAY_OWN_QUEUE=1)"
                ));
            };
            own.sent += 1;
            let (allocator, list) = &own.list;
            // SAFETY: the list's last run was waited for by this thread;
            // live buffers; the queue, the fence and the event are this
            // thread's.
            let ran = unsafe {
                (|| -> windows::core::Result<()> {
                    allocator.Reset()?;
                    list.Reset(allocator, None)?;
                    for (at, from, len) in
                        crate::relay_runs::relay_kept_runs(&kept, crate::relay_runs::enabled())
                    {
                        list.CopyBufferRegion(&buffer, at, snaps, from, len);
                    }
                    list.Close()?;
                    own.queue
                        .ExecuteCommandLists(&[Some(list.cast::<ID3D12CommandList>()?)]);
                    own.queue.Signal(&own.fence, own.sent)?;
                    own.fence.SetEventOnCompletion(own.sent, own.event)
                })()
            };
            if let Err(e) = ran {
                fail(format_args!(
                    "the pieces the compare kept could not be fetched ({e}, VICTORY_RELAY_OWN_QUEUE=1)"
                ));
            }
            kept_down = Some((data, own.event));
        }
        let mut live_down: Option<(usize, HANDLE, u64)> = None;
        if !pieces.is_empty() {
            if f.buffer.as_ref().is_none_or(|b| b.2 < total) {
                let size = total.next_multiple_of(16 << 20);
                f.buffer = self
                    .shared_buffer(size)
                    .map(|(buffer, data)| (buffer, data as usize, size));
            }
            let Some((buffer, data, _)) = f.buffer.clone() else {
                fail(format_args!(
                    "no memory to fetch what shaders wrote to the card's memory ({total} bytes, VICTORY_RELAY=1)"
                ));
            };
            f.sent += 1;
            let ((allocator, list), fence, event, sent) =
                (f.list.clone(), f.fence.clone(), f.event, f.sent);
            // SAFETY: the list's last run was waited for by this thread; live
            // buffers (the copies are held by the plans); the fence and the
            // event are this thread's.
            let ran = unsafe {
                (|| -> windows::core::Result<u64> {
                    allocator.Reset()?;
                    list.Reset(&allocator, None)?;
                    crate::gpucost::begin(&list, crate::gpucost::FETCH_RELAY);
                    for (shadow, lo, hi, at) in
                        crate::relay_runs::relay_copy_runs(&pieces, crate::relay_runs::enabled())
                    {
                        list.CopyBufferRegion(
                            &buffer,
                            at,
                            &shadow.buffer,
                            lo as u64,
                            (hi - lo) as u64,
                        );
                    }
                    crate::gpucost::end(&list);
                    list.Close()?;
                    let lists = [Some(list.cast::<ID3D12CommandList>()?)];
                    // In the order of the queue: the sends of title bytes so
                    // far are ahead of this fetch, every later one behind it.
                    let order = QUEUE_ORDER.lock().unwrap_or_else(|e| e.into_inner());
                    // Sends handed to the thread that sends (`VICTORY_SEND_THREAD`) are counted in `order`.
                    super::send_thread::wait_idle();
                    self.queue.ExecuteCommandLists(&lists);
                    let after = *order;
                    drop(order);
                    self.card_sent();
                    self.queue.Signal(&fence, sent)?;
                    fence.SetEventOnCompletion(sent, event)?;
                    Ok(after)
                })()
            };
            match ran {
                Ok(after) => live_down = Some((data, event, after)),
                Err(e) => fail(format_args!(
                    "what shaders wrote to the card's memory could not be fetched ({e}, VICTORY_RELAY=1)"
                )),
            }
        }
        let mut kept_us = 0;
        if let Some((_, event)) = kept_down {
            // SAFETY: this thread's event, set by its fence above.
            unsafe { WaitForSingleObject(event, INFINITE) };
            kept_us = waiting.elapsed().as_micros() as u64;
        }
        // (Neptune) `own.queue` has finished copying from `snaps` into
        // `own.buffer` (or no kept pieces were read from `snaps`): their
        // slots are free for compares recorded from here on.
        SNAP_DONE.fetch_add(handed, Ordering::AcqRel);
        if let Some((_, event, _)) = live_down {
            // SAFETY: this thread's event, set by its fence above.
            unsafe { WaitForSingleObject(event, INFINITE) };
        }
        // This fetch may bring bytes of flights in the air now: their own
        // slots are older than what it puts in the CPU's side there
        // (`relay_card_inflight`). Sends up to `after` are numbered, and the
        // submission numbered `after` may not have its lists on the queue
        // yet; every one before it has (the submission lock).
        if inflight_fetch()
            && let Some((_, _, after)) = live_down
            && after > 1
            && let Some(inflight) = CARD_WORK
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_mut()
                .and_then(|w| w.inflight.as_mut())
        {
            inflight.brought.extend(
                pieces
                    .iter()
                    .map(|(shadow, lo, hi, _)| (shadow.clone(), *lo, *hi, after - 1)),
            );
        }
        let us = waiting.elapsed().as_micros() as u64;
        RELAY_STATS[3].fetch_add(us, Ordering::Relaxed);
        RELAY_STATS[5].fetch_add(kept_us, Ordering::Relaxed);
        RELAY_STATS[6].fetch_add(kept.len() as u64, Ordering::Relaxed);
        RELAY_STATS[7].fetch_add(pieces.len() as u64, Ordering::Relaxed);
        {
            use crate::shadow::CARD_STATS;
            CARD_STATS[3].fetch_add((pieces.len() + kept.len()) as u64, Ordering::Relaxed);
            CARD_STATS[4].fetch_add(total + kept_total, Ordering::Relaxed);
            CARD_STATS[5].fetch_add(1, Ordering::Relaxed);
            CARD_STATS[6].fetch_add(us, Ordering::Relaxed);
        }
        // (fps-neptune) `VICTORY_RELAY_WIDE_MERGE=1`: the merges of one landing's pieces run on the
        // copy-back thread's kept threads (`VICTORY_WIDE_MERGE` only reaches the non-relay path).
        let mut jobs_in: Vec<(
            std::sync::Arc<crate::shadow::Shadow>,
            usize,
            usize,
            usize,
            u64,
        )> = Vec::new();
        if let Some((data, _)) = kept_down {
            for (shadow, lo, hi, at, _, after) in &kept {
                jobs_in.push((shadow.clone(), *lo, *hi, data + *at as usize, *after));
            }
        }
        if let Some((data, _, after)) = live_down {
            for (shadow, lo, hi, at) in &pieces {
                jobs_in.push((shadow.clone(), *lo, *hi, data + *at as usize, after));
            }
        }
        let written = merge_landed(jobs_in);
        COPY_BACK[1].fetch_add(written as u64, Ordering::Relaxed);
        match (keeping, last_sent, live_down) {
            (true, Some(sent), _) => crate::shadow::relay_fetched(sent),
            (true, None, _) => {}
            // Sends up to `after` are of no use to any later merge.
            (false, _, Some((_, _, after))) => crate::shadow::relay_fetched(fallback_fetched_mark(
                inflight_fetch(),
                after,
                last_sent,
            )),
            (false, _, None) => {}
        }
    }

    /// [`Host::relay_card_back`] with [`inflight_fetch`]: the pieces these
    /// plans' compares flagged are taken from the slots their own compare
    /// lists copied them to ([`CARD_FETCH`]) and merged, plan by plan in the
    /// line's order, with no list or wait of ours. `false` (nothing merged)
    /// if any plan cannot land so; the caller then lands them all the old
    /// way, as without the switch.
    ///
    /// The merge's "fetched after" (`Shadow::card_merge_relay`) is the
    /// plan's own `sent_as`. `QUEUE_ORDER` is raised only in
    /// `Host::card_before`, under the submission lock, and that lock is held
    /// until the submission's lists and its compare's list are on the queue
    /// behind its sync list: so when the compare's list ran, the card had
    /// received exactly the sends 1..=`sent_as` of title bytes and no later
    /// one, which is what the merge's skip of later sends asks. (The old
    /// fetch, by contrast, ran whenever it reached the queue, and took the
    /// count at its own ExecuteCommandLists.)
    ///
    /// When a landing goes the old way instead, all of it does: a fetch the
    /// old way brings the card's bytes as they are now, newer than the slots
    /// of plans in the air then, and merging an older slot after it would
    /// put a flight's bytes back over a later flight's. For the same reason
    /// the bytes such a fetch brought (`CardInflight::brought`) are left
    /// out of the slots of flights whose lists were on the queue ahead of
    /// it: the CPU's side holds what the card held after them there. The
    /// rest of their slots is merged, newer than anything merged there
    /// before. (Left out whole, a flight sent before such a fetch would have
    /// had to land the old way, its fetch taking a later count still: one
    /// overflow kept every landing on the old way while flights were in
    /// the air.) Plans land in the line's order, each plan's slot as it was
    /// right behind its own lists, so every merge here is of bytes at least
    /// as new as the CPU's side holds.
    ///
    /// shortcut: all or nothing per landing, and a plan whose pieces overflow
    /// the slots lands the old way for all its pieces, not only the ones
    /// left out.
    fn relay_card_inflight(&self, plans: &[&CardPlan]) -> bool {
        use std::sync::Arc;
        type Piece = (Arc<crate::shadow::Shadow>, usize, usize, usize, u64);
        let mut plan_pieces: Vec<Vec<Piece>> = Vec::with_capacity(plans.len());
        {
            let work = CARD_WORK.lock().unwrap_or_else(|e| e.into_inner());
            let Some(work) = work.as_ref() else {
                return false;
            };
            let Some(inflight) = &work.inflight else {
                return false;
            };
            let dwords = (work.differs.2 / 4) as usize;
            // SAFETY: the mappings of the results and the tickets. The
            // compares and fetches of these plans have run, and their dwords
            // are given to no other plan before these are done with
            // (`card_done`).
            let (differs, table) = unsafe {
                (
                    std::slice::from_raw_parts(work.differs.1 as *const u32, dwords),
                    std::slice::from_raw_parts((inflight.table.1 as *const u32).add(1), dwords),
                )
            };
            let slots = inflight.fetched.2;
            for plan in plans {
                if plan.ranges.is_empty() {
                    continue;
                }
                if !plan.inflight || plan.ranges.iter().any(|r| !r.compared) {
                    return false;
                }
                // Pieces (copy, number) the compare flagged, with their ticket.
                let mut tickets: std::collections::HashMap<(usize, u32), u32> =
                    std::collections::HashMap::new();
                let mut flags: Vec<(usize, u32, bool)> = Vec::new();
                for r in &plan.ranges {
                    let copy = Arc::as_ptr(&r.shadow) as usize;
                    for p in 0..r.span.pieces {
                        let at = (r.at + p) as usize;
                        let (Some(&flag), Some(&entry)) = (differs.get(at), table.get(at)) else {
                            return false;
                        };
                        if flag != 0 {
                            if entry == INFLIGHT_NONE {
                                return false;
                            }
                            if !inflight_gets_slot(entry, plan.inflight_landed, slots) {
                                INFLIGHT_STATS[2].fetch_add(1, Ordering::Relaxed);
                                return false;
                            }
                            tickets.insert((copy, r.span.first + p), entry);
                        }
                        flags.push((copy, r.span.first + p, flag != 0));
                    }
                }
                let fetched = flagged_pieces(&flags);
                let mut seen = std::collections::HashSet::new();
                let mut current: Vec<Piece> = Vec::new();
                for r in &plan.ranges {
                    let copy = Arc::as_ptr(&r.shadow) as usize;
                    for p in 0..r.span.pieces {
                        let piece = r.span.first + p;
                        if !fetched.contains(&(copy, piece)) {
                            continue;
                        }
                        let (lo, hi) = r.shadow.card_piece(&r.span, p);
                        if lo >= hi
                            || crate::wc_skip::skip_back(r.declared, (hi - lo) as u64)
                            || !seen.insert((copy, lo, hi))
                        {
                            continue;
                        }
                        let from = inflight_piece_from(&r.span, p);
                        let Some(&k) = tickets.get(&(copy, piece)) else {
                            return false;
                        };
                        if lo < from || (hi - from) as u64 > INFLIGHT_SLOT {
                            return false;
                        }
                        let slot =
                            inflight.fetched.1 + (k % slots) as usize * INFLIGHT_SLOT as usize;
                        // Bytes a fetch the old way brought after this flight ran.
                        let newer: Vec<(usize, usize)> = inflight
                            .brought
                            .iter()
                            .filter(|b| {
                                b.3 >= plan.sent_as
                                    && Arc::ptr_eq(&b.0, &r.shadow)
                                    && b.1 < hi
                                    && b.2 > lo
                            })
                            .map(|b| (b.1, b.2))
                            .collect();
                        for (a, b) in crate::shadow::outside(lo, hi, newer) {
                            current.push((r.shadow.clone(), a, b, slot + (a - from), plan.sent_as));
                        }
                    }
                }
                if !current.is_empty() {
                    plan_pieces.push(current);
                }
            }
        }
        // Outside the lock: no slot of these plans is written over before
        // their tickets are landed (`inflight_tickets_landed`, after this).
        // Plans merge in chronological order; disjoint pieces within each
        // plan run in parallel when `VICTORY_RELAY_WIDE_MERGE=1`.
        let (mut written, mut count, mut bytes) = (0usize, 0u64, 0u64);
        for pieces in plan_pieces {
            let pieces = crate::shadow::lazy::filter_inflight_pieces(pieces);
            count += pieces.len() as u64;
            bytes += pieces
                .iter()
                .map(|(_, lo, hi, _, _)| (hi - lo) as u64)
                .sum::<u64>();
            written += merge_landed(pieces);
        }
        for plan in plans {
            for r in &plan.ranges {
                crate::shadow::CARD_STATS[2].fetch_add(u64::from(r.span.pieces), Ordering::Relaxed);
            }
        }
        COPY_BACK[1].fetch_add(written as u64, Ordering::Relaxed);
        INFLIGHT_STATS[0].fetch_add(count, Ordering::Relaxed);
        INFLIGHT_STATS[1].fetch_add(bytes, Ordering::Relaxed);
        // The next merge is of a later plan, or a fetch the old way sent now.
        if let Some(last) = plans.iter().map(|p| p.sent_as).max() {
            crate::shadow::relay_fetched(last);
        }
        true
    }
}

/// The compare's list recorded ([`record_check`]): the list, and with the
/// fetch inside the flight, whether every range has it and the slots landed.
type CheckRecorded = (windows::core::Result<ID3D12CommandList>, Option<(bool, u32)>);

/// [`card_ranges`] placed where the result area has room for the plan
/// ([`crate::result_ring`]), and the stretch of it they took.
fn place_ranges(
    taken: &mut crate::result_ring::Ring,
    out: u32,
    declared: &[(std::sync::Arc<crate::shadow::Shadow>, Vec<(usize, usize)>)],
    joined_decl: bool,
) -> (Vec<CardRange>, (u32, u32)) {
    let (start, limit) = taken.room(out, crate::result_ring::on());
    let mut at = start;
    let ranges = card_ranges(declared, joined_decl, &mut at, u64::from(limit));
    (ranges, taken.took(start, at))
}

/// `card_before`'s compare ranges: each copy's declared ranges joined, each
/// placed in the compare's results from `taken` on (moved past them), below
/// the dword `most`.
fn card_ranges(
    declared: &[(std::sync::Arc<crate::shadow::Shadow>, Vec<(usize, usize)>)],
    joined_decl: bool,
    taken: &mut u32,
    most: u64,
) -> Vec<CardRange> {
    let mut at = *taken;
    let mut out: Vec<CardRange> = Vec::new();
    for (shadow, ranges) in declared {
        let joined_here;
        let spans: &[(usize, usize)] = if joined_decl {
            ranges
        } else {
            let mut sorted = ranges.clone();
            sorted.sort_unstable();
            let mut spans: Vec<(usize, usize)> = Vec::new();
            for (from, to) in sorted {
                match spans.last_mut() {
                    Some(last) if from <= last.1 => last.1 = last.1.max(to),
                    _ => spans.push((from, to)),
                }
            }
            joined_here = spans;
            &joined_here
        };
        crate::card_prof::add(crate::card_prof::P::DeclOut, spans.len() as u64);
        let twin = shadow
            .card_sides()
            .is_some_and(|(_, before, _)| before.is_some());
        let mut run = crate::card_share::Run::new(crate::card_share::on() && !inflight_fetch());
        for &(from, to) in spans {
            let Some(span) = shadow.card_span(from, to) else {
                continue;
            };
            let slot = twin
                .then(|| run.slot(span.first, span.pieces, &mut at, most))
                .flatten();
            out.push(CardRange {
                shadow: shadow.clone(),
                declared: (from, to),
                span,
                at: slot.unwrap_or(at),
                compared: slot.is_some(),
            });
        }
    }
    *taken = at;
    out
}

/// The snapshot the compare keeps what it flags in, if it does, and the
/// slots done with as read now.
fn keep_of(snap: &Option<CardSnap>, flown: bool) -> Option<(&CardSnap, u32)> {
    snap.as_ref()
        .filter(|_| flown && !SNAP_OFF.load(Ordering::Acquire))
        .map(|snap| (snap, SNAP_DONE.load(Ordering::Acquire)))
}
