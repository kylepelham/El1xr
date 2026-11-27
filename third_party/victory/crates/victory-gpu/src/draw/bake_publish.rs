//! `VICTORY_BAKE_PUBLISH` (`crate::bake_publish`): a bake draw's input slots whose descriptors
//! `VICTORY_SHOW_AFTER` still holds back are shown before the draw.

use super::*;
use crate::bake_publish::{C, add};

impl State {
    /// Slot `index`'s live descriptor waits until this list has run (`VICTORY_SHOW_AFTER`).
    pub(super) fn hold_show(&mut self, heap: usize, index: u32, handle: u64, key: u64) {
        crate::bake_publish::held(heap, index, handle, key);
        self.pending_shows.push((heap, index, handle, key));
    }

    /// Before bake draw `p`, after its texture passes: its input slots still held back are
    /// shown now (with `2` also those another list holds back).
    pub(super) fn bake_publish(&mut self, p: &HostPipeline, mode: u64) {
        let Some(mut mine) = self.bake_input_slots(p) else {
            add(C::Unread, 1);
            return;
        };
        let Some((heap, _)) = self.view_heap() else {
            return;
        };
        let heap = std::ptr::from_ref(heap) as usize;
        add(C::Bakes, 1);
        add(C::Inputs, mine.len() as u64);
        mine.sort_unstable();
        let (mut same, mut other, mut moved) = (0, 0, 0);
        let shown = crate::bake_publish::take_mine(&mut self.pending_shows, heap, &mine);
        for (heap, index, handle, key) in shown {
            crate::bake_publish::released(heap, index);
            if crate::descriptor::host::commit_show_now(heap, index, handle, key) {
                same += 1;
            } else {
                moved += 1;
            }
        }
        if mode == 2 {
            for &index in &mine {
                let Some((handle, key)) = crate::bake_publish::take_held(heap, index) else {
                    continue;
                };
                if crate::descriptor::host::commit_show_now(heap, index, handle, key) {
                    other += 1;
                } else {
                    moved += 1;
                }
            }
        }
        add(C::SameList, same);
        add(C::OtherList, other);
        add(C::Moved, moved);
    }
}
