//! `VICTORY_BAKE_ZERO_WAIT` (`crate::bake_zero`): a bake draw whose input texture's memory still
//! reads zeros held until the title writes it, and the inputs that filled uploaded before it.

use super::*;
use crate::bake_zero::Seen;

thread_local! {
    /// The bake draw being bound reads an input still zeros that may fill (`VICTORY_BAKE_REPLAY`
    /// keeps it as incomplete).
    pub(super) static DRAWN_ZERO: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether the texture at title address `address` (`len` bytes) reads zeros in every sampled run
/// (None: a run's memory is not there).
pub(super) fn reads_zero(address: u64, len: u64) -> Option<bool> {
    crate::bake_zero::reads_zero(len, |off, n| {
        let at = address.checked_add(off)? as *const u8;
        if !crate::texture::committed(at, n as usize) {
            return None;
        }
        // SAFETY: committed title memory, read as it is (the title may be writing it: the
        // answer only steers whether the bake waits).
        let bytes = unsafe { std::slice::from_raw_parts(at, n as usize) };
        Some(crate::bake_zero::all_zero(bytes))
    })
}

/// The address and length of the texture placed in memory that slot `slot` views, if every
/// sampled run of it reads zeros.
pub(super) fn slot_zero(heap: &crate::descriptor::Heap, slot: u32) -> Option<Option<(u64, u64)>> {
    let (address, len, _) = bake_pass::placed(heap, slot)?;
    // A length that could not be read (`placed` gives 1) says nothing.
    if address == 0 || len < 16 {
        return None;
    }
    let zero = reads_zero(address, len)?;
    Some(zero.then_some((address, len)))
}

impl State {
    /// (`VICTORY_BAKE_ZERO_WAIT=<ms>`) While bake draw `p`'s own pass holds its input slots
    /// ([`bake_pass::PINNED`]): an input reading zeros holds the draw up to the bound, and the
    /// ones that filled are uploaded alone.
    pub(super) fn bake_zero_wait(
        &mut self,
        list: &HostList,
        p: &HostPipeline,
        heap: &'static crate::descriptor::Heap,
        host: &'static crate::descriptor::host::HostHeap,
    ) {
        DRAWN_ZERO.set(false);
        let ms = crate::bake_zero::wait_ms();
        if ms == 0 {
            return;
        }
        let frame = crate::frame::current();
        let mine = bake_pass::PINNED.with_borrow(Clone::clone);
        let mut seen = Seen::default();
        let mut zero: Vec<(u32, u64, u64)> = Vec::new();
        for slot in mine {
            if let Some(z) = slot_zero(heap, slot) {
                seen.checked += 1;
                if let Some((address, len)) = z {
                    zero.push((slot, address, len));
                }
            }
        }
        seen.zero = zero.len() as u64;
        let unknown: Vec<(u32, u64, u64)> = zero
            .iter()
            .copied()
            .filter(|&(_, address, len)| !crate::bake_zero::known(address, len, frame))
            .collect();
        let left = if zero.is_empty() || unknown.is_empty() {
            0
        } else {
            crate::bake_zero::wait_left(frame, ms)
        };
        let outcome = crate::bake_zero::decide(zero.len(), unknown.is_empty(), left, |left| {
            let _hitch = crate::hitch::span(crate::hitch::Phase::EarlyTextures);
            let (filled, took) = crate::bake_why::wait_for(left, || {
                unknown
                    .iter()
                    .all(|&(_, address, len)| reads_zero(address, len) == Some(false))
            });
            let took = took.max(1);
            crate::bake_zero::waited(took);
            seen.ms = took;
            filled
        });
        if outcome.held() {
            for &(slot, address, len) in &unknown {
                if reads_zero(address, len) == Some(false) {
                    host.texture_slot_refresh(slot, frame);
                    seen.refreshed += 1;
                } else {
                    crate::bake_zero::expired(address, len, frame);
                }
            }
            if seen.refreshed > 0 {
                bake_pass::ONLY.set(true);
                PASS_BUDGET.set(Some((0, 0, true)));
                self.heap_textures(list, heap, host);
                PASS_BUDGET.set(None);
                bake_pass::ONLY.set(false);
            }
            crate::bake_zero::bake_line(frame, bake_pass::bake_hash(p), outcome, seen.ms, &unknown);
        }
        DRAWN_ZERO.set(outcome.replays());
        crate::bake_zero::record(frame, bake_pass::bake_hash(p), outcome, seen);
    }

    /// (`VICTORY_BAKE_ZERO_WAIT`, for `VICTORY_BAKE_REPLAY`) Whether one of `slots` still reads
    /// zeros; one whose memory changed since its upload is made due, so it is current later.
    pub(super) fn bake_zero_still(
        heap: &crate::descriptor::Heap,
        host: &crate::descriptor::host::HostHeap,
        slots: &[u32],
        fresh: &[crate::bake_fresh::Fresh],
        frame: u64,
    ) -> bool {
        for (&slot, &f) in slots.iter().zip(fresh) {
            if f == crate::bake_fresh::Fresh::OtherBytes {
                host.texture_slot_refresh(slot, frame);
            }
        }
        slots
            .iter()
            .any(|&slot| matches!(slot_zero(heap, slot), Some(Some(_))))
    }
}
