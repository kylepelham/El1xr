//! `VICTORY_FACE_WATCH` (`crate::face_watch`): what each texture a watched character material
//! draw picks from its record shows when it is bound.

use super::*;
use crate::face_watch::Tex;

impl State {
    /// For a draw whose pixel shader is watched: each record slot's state, told by
    /// [`crate::face_watch::drawn`].
    pub(super) fn face_watch(&self, p: &HostPipeline) {
        let frame = crate::frame::current();
        let Some((stage, fields)) = p
            .stages
            .iter()
            .find_map(|s| crate::face_watch::fields(s.shader.hash.as_str(), frame).map(|f| (s, f)))
        else {
            return;
        };
        let hash = crate::face_watch::intern(stage.shader.hash.as_str());
        let read = self
            .material_record(stage, fields)
            .and_then(|r| self.view_heap().map(|(heap, host)| (r, heap, host)));
        let Some(((table, record, slots), heap, host)) = read else {
            crate::face_watch::unread(frame);
            return;
        };
        let heap_id = std::ptr::from_ref(heap) as usize;
        let texs: Vec<Tex> = slots
            .iter()
            .map(|&slot| self.face_watch_slot(heap, host, heap_id, slot, frame))
            .collect();
        crate::face_watch::drawn(frame, hash, table, record, &texs);
    }

    fn face_watch_slot(
        &self,
        heap: &crate::descriptor::Heap,
        host: &crate::descriptor::host::HostHeap,
        heap_id: usize,
        slot: u32,
        frame: u64,
    ) -> Tex {
        let s = self.bake_check_slot(heap, host, heap_id, slot, frame);
        let look = Self::bake_fresh_look(heap, host, slot, s.queued.is_some(), frame);
        let mut t = Tex {
            slot,
            flags: crate::bake_check::flags(&s),
            text: crate::bake_check::slot_text(&s),
            fresh: crate::bake_fresh::classify(&look).name(),
            ..Tex::default()
        };
        let object = match heap.handle(slot).and_then(crate::descriptor::view) {
            Some(View::Placed(false, resource, _)) => {
                if let Some((address, len, _)) = bake_pass::placed(heap, slot) {
                    t.zero = bake_zero::reads_zero(address, len);
                    t.note_bake(crate::face_watch::in_result(address, len));
                    t.over_target = crate::resource::placed_inside::target_over(address, len);
                    crate::resource::placed_known(address, &resource)
                } else {
                    None
                }
            }
            Some(View::Shader(object, _)) if object != 0 => Some(object as This),
            _ => None,
        };
        if let Some(this) = object {
            t.read_again = crate::target_reuse::read_again(this as usize, frame);
            t.card_written = crate::resource::host_written_mark(this);
            t.stand_in = crate::resource::stands_in(this);
            t.baked = crate::bake_check::result_of(this as usize);
            if t.baked.is_none()
                && !t.in_bake
                && let Some((at, desc)) = crate::resource::texture_memory(this)
                && let Some(len) = crate::resource::texture_layout(&desc).map(|l| l.size)
            {
                t.note_bake(crate::face_watch::in_result(at as u64, len));
            }
        }
        t
    }
}
