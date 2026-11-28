//! (feedcard, #88) `VICTORY_HEAP_PASS_SPLIT`: the read-only half of
//! [`HostHeap::mirror_heap_buffer`]'s fast path, for threads that look at
//! the heap-buffers pass's slots beside it (`crate::heap_pass_split`).
//!
//! `mirror_inner` returns at once, without a lock or a make, when the slot
//! shows the record's view already (`made_too` holds its entry and
//! generation and the entry is current): nearly every slot of the pass's
//! full looks. Here that test alone; a slot that fails it is left to
//! `mirror_heap_buffer` on the pass's own thread, in order.

use std::sync::atomic::Ordering::Acquire;

use super::{HostHeap, View, mirror_known, view_at};

impl HostHeap {
    /// The view slot `index` shows already for the record at `handle`, and
    /// the slot's key (`made_too`), as `mirror_inner`'s fast path finds it;
    /// `None` where `mirror_heap_buffer` would make or let go something.
    pub(crate) fn shown_already(&self, index: u32, handle: u64) -> Option<(u64, View)> {
        if index >= self.count {
            return None;
        }
        let record = crate::descriptor::record_at(handle);
        let shown = self.made_too[index as usize].load(Acquire);
        if !mirror_known::shows(record, shown) {
            return None;
        }
        let [_, i, generation] = record;
        view_at(i, generation).map(|view| (shown, view))
    }

    /// The record at `handle`, if slot `index` still shows key `key` for it.
    pub(crate) fn shows_key(&self, index: u32, handle: u64, key: u64) -> Option<[u32; 3]> {
        if index >= self.count || self.made_too[index as usize].load(Acquire) != key {
            return None;
        }
        let record = crate::descriptor::record_at(handle);
        mirror_known::shows(record, key).then_some(record)
    }
}
