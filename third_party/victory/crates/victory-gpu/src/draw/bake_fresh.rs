//! `VICTORY_BAKE_FRESH` (`crate::bake_fresh`): a bake draw's input slots checked for freshness
//! before its own pass, and what it reads from them after.

use super::*;
use crate::bake_fresh::{After, C, Fresh, Look, add};

impl State {
    /// Before bake draw's own pass: each input slot in `mine` is looked at, and the stale ones are
    /// made due now so that pass uploads them.
    pub(super) fn bake_fresh_before(
        &self,
        heap: &crate::descriptor::Heap,
        host: &crate::descriptor::host::HostHeap,
        mine: &[u32],
    ) -> Vec<(u32, Fresh)> {
        let frame = crate::frame::current();
        let queued: Vec<u32> = host
            .texture_slots_pick(mine)
            .into_iter()
            .map(|(i, _)| i)
            .collect();
        add(C::Bakes, 1);
        add(C::Inputs, mine.len() as u64);
        mine.iter()
            .map(|&slot| {
                let look = Self::bake_fresh_look(heap, host, slot, queued.contains(&slot), frame);
                let fresh = crate::bake_fresh::classify(&look);
                crate::bake_fresh::count(fresh);
                if fresh.refresh() {
                    host.texture_slot_refresh(slot, frame);
                    add(C::Refreshed, 1);
                }
                (slot, fresh)
            })
            .collect()
    }

    fn bake_fresh_look(
        heap: &crate::descriptor::Heap,
        host: &crate::descriptor::host::HostHeap,
        slot: u32,
        queued: bool,
        frame: u64,
    ) -> Look {
        let mut look = Look {
            queued,
            ..Look::default()
        };
        let Some(handle) = heap.handle(slot) else {
            return look;
        };
        match crate::descriptor::view(handle) {
            Some(View::Placed(false, ..)) => {
                look.texture = true;
                look.mapped = bake_pass::placed(heap, slot)
                    .map_or(0, |(address, len, _)| bake_pass::mapped(address, len));
            }
            Some(View::Shader(object, _))
                if object != 0 && crate::resource::buffer_bytes(object as This).is_none() =>
            {
                look.texture = true;
                look.mapped = 1;
            }
            _ => return look,
        }
        if queued {
            return look;
        }
        look.made_from = host.texture_slot_made_from(slot, handle);
        look.made_null = host.slot_check(slot, frame).0 == 0;
        if look.made_from {
            look.print_changed = host
                .texture_slot_print(slot)
                .map(|((at, len), print, live)| {
                    crate::texture::fingerprint_for(at, len, live) != print
                });
        }
        look
    }

    /// After bake draw `p`'s passes: its input slots that showed null are shown now, and a bake
    /// that still reads an input not current is counted and named.
    pub(super) fn bake_fresh_after(
        &mut self,
        p: &HostPipeline,
        heap: &crate::descriptor::Heap,
        host: &crate::descriptor::host::HostHeap,
        seen: &[(u32, Fresh)],
    ) {
        let heap_id = std::ptr::from_ref(heap) as usize;
        let mut nulls: Vec<u32> = seen
            .iter()
            .filter(|&&(_, f)| f == Fresh::StaleNull)
            .map(|&(slot, _)| slot)
            .collect();
        nulls.sort_unstable();
        let mut published = Vec::new();
        for (h, index, handle, key) in
            crate::bake_publish::take_mine(&mut self.pending_shows, heap_id, &nulls)
        {
            crate::bake_publish::released(h, index);
            if crate::descriptor::host::commit_show_now(h, index, handle, key) {
                published.push(index);
                add(C::Published, 1);
            } else {
                add(C::Moved, 1);
            }
        }
        let slots: Vec<u32> = seen.iter().map(|&(slot, _)| slot).collect();
        let queued: Vec<u32> = host
            .texture_slots_pick(&slots)
            .into_iter()
            .map(|(i, _)| i)
            .collect();
        let read: Vec<(u32, Fresh, After)> = seen
            .iter()
            .filter(|&&(_, f)| f != Fresh::NotTexture)
            .map(|&(slot, f)| {
                let held = self
                    .pending_shows
                    .iter()
                    .any(|&(h, i, _, _)| h == heap_id && i == slot);
                let a = crate::bake_fresh::after(
                    f,
                    published.contains(&slot),
                    held,
                    queued.contains(&slot),
                );
                match a {
                    After::Held => add(C::Held, 1),
                    After::Left => add(C::Left, 1),
                    _ => {}
                }
                (slot, f, a)
            })
            .collect();
        if read.iter().any(|&(_, _, a)| a.incomplete()) {
            crate::bake_fresh::incomplete(crate::frame::current(), bake_pass::bake_hash(p), &read);
        }
    }
}
