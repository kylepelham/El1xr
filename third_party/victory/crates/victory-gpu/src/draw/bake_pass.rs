//! `VICTORY_BAKE_INPUTS=1` (`crate::bake_inputs`): a decoration bake draw's own pass over the
//! textures it reads, ahead of its general bake pass or, once the frame's bake budget is spent,
//! in place of it.

use super::*;
use crate::bake_inputs::{C, PARAM_DWORDS, USER_WORD, add, params_offset, slots};

thread_local! {
    /// While a bake draw's passes run: the title slots it reads, served first by
    /// `heap_textures` in a pass whose uploads must be there now.
    pub(super) static PINNED: std::cell::RefCell<Vec<u32>> =
        const { std::cell::RefCell::new(Vec::new()) };
    /// A pass served [`PINNED`].
    pub(super) static SERVED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// The pass is for [`PINNED`] alone.
    pub(super) static ONLY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Reads `n` dwords at title address `at`, if its memory is there.
fn words(at: u64, n: usize) -> Option<Vec<u32>> {
    if at == 0 || n == 0 || !crate::texture::committed(at as *const u8, n * 4) {
        return None;
    }
    // SAFETY: committed title memory, read as it is (the title may be writing it: the words
    // only steer which textures go first).
    Some(
        (0..n)
            .map(|i| unsafe { (at as *const u32).add(i).read_unaligned() })
            .collect(),
    )
}

impl State {
    /// The early pass of a bake draw with the switch: its own textures first.
    pub(super) fn bake_draw_textures(&mut self, list: &HostList, p: &HostPipeline) {
        let Some(mine) = self.bake_input_slots(p) else {
            add(C::Unread, 1);
            return self.early_textures_plain(list, p, false);
        };
        let Some((heap, host)) = self.view_heap() else {
            return self.early_textures_plain(list, p, false);
        };
        let before = host.texture_slots_pick(&mine).len() as u64;
        PINNED.set(mine);
        SERVED.set(false);
        self.early_textures_plain(list, p, false);
        if !SERVED.get() {
            // The frame's bake budget is spent (or the pass would not have served them now):
            // these textures alone, whatever they take.
            let _hitch = crate::hitch::span(crate::hitch::Phase::EarlyTextures);
            let began = std::time::Instant::now();
            ONLY.set(true);
            PASS_BUDGET.set(Some((0, 0, true)));
            self.heap_textures(list, heap, host);
            PASS_BUDGET.set(None);
            ONLY.set(false);
            add(C::OwnUs, began.elapsed().as_micros() as u64);
        }
        let mine = PINNED.take();
        let pending = host.texture_slots_pick(&mine).len() as u64;
        add(C::Bakes, 1);
        add(C::Inputs, mine.len() as u64);
        add(C::Uploaded, before.saturating_sub(pending));
        if pending > 0 {
            add(C::Short, 1);
            add(C::Pending, pending);
        }
    }

    /// The first view heap bound that is not a sampler heap, with its host heap.
    fn view_heap(
        &self,
    ) -> Option<(
        &'static crate::descriptor::Heap,
        &'static crate::descriptor::host::HostHeap,
    )> {
        let heap = self
            .heaps
            .iter()
            .filter(|&&h| h != 0)
            .map(|&h| crate::descriptor::heap(h as This))
            .find(|h| !h.samplers())?;
        Some((heap, heap.host()?))
    }

    /// The title slots bake draw `p` reads textures from (`crate::bake_inputs`): from its
    /// parameter constants and records, found through user data word 9.
    fn bake_input_slots(&self, p: &HostPipeline) -> Option<Vec<u32>> {
        let stage = p
            .stages
            .iter()
            .find(|s| params_offset(s.shader.hash.as_str()).is_some())?;
        let offset = params_offset(stage.shader.hash.as_str())?;
        let t = stage.translated();
        let u = t.host.user_data.iter().find(|u| u.index == USER_WORD)?;
        if matches!(u.root, RootType::Other(_)) {
            return None;
        }
        let word = self.user_value(false, u.slot, u.root, u.dword);
        let (heap, _) = self.view_heap()?;
        let params = match crate::descriptor::view(heap.handle(word.wrapping_add(offset) >> 5)?)? {
            View::Constant(at, size) => {
                let n = (size as usize / 4).min(PARAM_DWORDS);
                let mut p = [0u32; PARAM_DWORDS];
                p[..n].copy_from_slice(&words(at, n)?);
                p
            }
            _ => return None,
        };
        let records = crate::descriptor::view(heap.handle(word >> 5)?)?;
        let (base, len) = match records {
            View::Shader(object, None) => {
                let (at, len) = crate::resource::buffer_bytes(object as This)?;
                (at as u64, len)
            }
            _ => crate::descriptor::host::buffer_range(&records)?,
        };
        Some(slots(&params, |off| {
            (off.saturating_add(4) <= len)
                .then(|| words(base.checked_add(off)?, 1))
                .flatten()
                .map(|w| w[0])
        }))
    }
}
