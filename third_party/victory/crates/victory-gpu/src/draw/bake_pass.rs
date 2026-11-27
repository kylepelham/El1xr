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

/// Whether a pass over the due slots stops before its `i`th: never for the first `pinned` (a
/// bake draw's own textures, `VICTORY_BAKE_INPUTS`: outside the budgets, whatever
/// `VICTORY_BAKE_ONE_BUDGET` left of the pass's time), else once the frame's bytes are spent
/// (`bytes_spent`) or, after an upload (or at every 8th slot looked at), past `budget`.
pub(super) fn stops_at(
    i: usize,
    pinned: usize,
    bytes_spent: bool,
    uploaded: u64,
    checked: u64,
    budget: std::time::Duration,
    elapsed: impl FnOnce() -> std::time::Duration,
) -> bool {
    let looked_enough = uploaded > 0 || (checked > 0 && checked.is_multiple_of(8));
    i >= pinned && (bytes_spent || (looked_enough && elapsed() >= budget))
}

/// Reads `n` dwords at title address `at`, if its memory is there.
pub(super) fn words(at: u64, n: usize) -> Option<Vec<u32>> {
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
        let waited_ms = self.bake_wait(list, heap, host);
        let mine = PINNED.take();
        let left = host.texture_slots_pick(&mine);
        if crate::bake_why::why() {
            self.bake_why(p, heap, host, &left, waited_ms);
        }
        let pending = left.len() as u64;
        add(C::Bakes, 1);
        add(C::Inputs, mine.len() as u64);
        add(C::Uploaded, before.saturating_sub(pending));
        if pending > 0 {
            add(C::Short, 1);
            add(C::Pending, pending);
        }
    }

    /// (`VICTORY_BAKE_WAIT_MS=N`) The bake draw's own textures whose memory the title has not
    /// (all) mapped: waits up to N ms for all of it, then uploads them alone. The ms waited.
    fn bake_wait(
        &mut self,
        list: &HostList,
        heap: &'static crate::descriptor::Heap,
        host: &'static crate::descriptor::host::HostHeap,
    ) -> u64 {
        let ms = crate::bake_why::wait_ms();
        if ms == 0 {
            return 0;
        }
        let waiting: Vec<(u64, u64)> = PINNED.with_borrow(|mine| {
            host.texture_slots_pick(mine)
                .iter()
                .filter_map(|&(index, _)| {
                    let (address, len, _) = placed(heap, index)?;
                    let mapped = mapped(address, len);
                    crate::bake_why::lack(mapped, len)
                        .waits_for_title()
                        .then_some((address, len))
                })
                .collect()
        });
        if waiting.is_empty() {
            return 0;
        }
        let ms = crate::bake_why::wait_left(crate::frame::current(), ms);
        if ms == 0 {
            return 0;
        }
        let _hitch = crate::hitch::span(crate::hitch::Phase::EarlyTextures);
        let (_, took) = crate::bake_why::wait_for(ms, || {
            waiting.iter().all(|&(address, len)| mapped(address, len) >= len)
        });
        crate::bake_why::waited(took.max(1));
        ONLY.set(true);
        PASS_BUDGET.set(Some((0, 0, true)));
        self.heap_textures(list, heap, host);
        PASS_BUDGET.set(None);
        ONLY.set(false);
        took
    }

    /// (`VICTORY_BAKE_WHY=1`) A line for each of the bake draw's textures still pending, and
    /// for each found pending before that has been uploaded since.
    fn bake_why(
        &self,
        p: &HostPipeline,
        heap: &'static crate::descriptor::Heap,
        host: &'static crate::descriptor::host::HostHeap,
        left: &[(u32, u64)],
        waited_ms: u64,
    ) {
        let frame = crate::frame::current();
        crate::bake_why::landed(frame, |slots| {
            host.texture_slots_pick(slots).into_iter().map(|(i, _)| i).collect()
        });
        if left.is_empty() {
            return;
        }
        let seen: Vec<crate::bake_why::Seen> = left
            .iter()
            .map(|&(slot, _)| {
                let (address, len, shape) = placed(heap, slot).unwrap_or((0, 0, [0; 4]));
                let mapped = if len == 0 { 0 } else { mapped(address, len) };
                crate::bake_why::Seen {
                    slot,
                    lack: if len == 0 {
                        crate::bake_why::Lack::Other
                    } else {
                        crate::bake_why::lack(mapped, len)
                    },
                    mapped,
                    len,
                    address,
                    shape,
                    state: host.texture_slot_state(slot, frame),
                }
            })
            .collect();
        crate::bake_why::pending(
            frame,
            bake_hash(p),
            self.targets.first().copied().unwrap_or(0),
            waited_ms,
            &seen,
        );
    }

    /// The first view heap bound that is not a sampler heap, with its host heap.
    pub(super) fn view_heap(
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
    pub(super) fn bake_input_slots(&self, p: &HostPipeline) -> Option<Vec<u32>> {
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

/// The title address, the bytes from the first mip shown and the shape of the texture placed in
/// memory that slot `index` views (None: another kind of view).
fn placed(heap: &crate::descriptor::Heap, index: u32) -> Option<(u64, u64, [u32; 4])> {
    let Some(View::Placed(false, resource, desc)) =
        heap.handle(index).and_then(crate::descriptor::view)
    else {
        return None;
    };
    let address = u64::from(desc[10]) | u64::from(desc[11]) << 32;
    let len =
        crate::resource::placed_bytes_from(&resource, crate::resource::first_mip(&desc)).unwrap_or(1);
    Some((address, len, crate::bake_why::shape(&resource)))
}

fn mapped(address: u64, len: u64) -> u64 {
    crate::texture::mapped_bytes(address as *const u8, len as usize)
}

/// The bake pixel shader of `p`, as [`BAKE_PIXEL_SHADERS`] names it.
fn bake_hash(p: &HostPipeline) -> &'static str {
    p.stages
        .iter()
        .find_map(|s| BAKE_PIXEL_SHADERS.iter().find(|&&h| h == s.shader.hash.as_str()))
        .copied()
        .unwrap_or("?")
}

#[cfg(test)]
mod tests {
    use super::stops_at;
    use std::time::Duration;

    /// Batch 4 part C: `bake inputs frame ... short=1` in the world with
    /// `VICTORY_BAKE_ONE_BUDGET=1`. The switch only shortens the pass's
    /// `budget`; a bake draw's own slots (the first `pinned`) are uploaded
    /// whatever the budget or bytes say, so a short bake is a slot the pass
    /// could not upload (no memory yet, part of it, or no room), not one it
    /// ran out of time for.
    #[test]
    fn a_bake_draws_own_slots_are_outside_every_budget() {
        let none = Duration::ZERO;
        let late = || Duration::from_secs(10);
        for i in 0..3 {
            assert!(!stops_at(i, 3, true, 5, 8, none, late), "pinned slot {i}");
        }
        assert!(stops_at(3, 3, true, 0, 1, none, late), "past the bytes");
        assert!(stops_at(3, 3, false, 1, 4, none, late), "past the time after an upload");
        assert!(!stops_at(3, 3, false, 0, 3, none, late), "nothing uploaded, not the 8th look");
        assert!(stops_at(3, 3, false, 0, 8, none, late), "the 8th look");
        assert!(
            !stops_at(3, 3, false, 1, 4, Duration::from_secs(11), late),
            "within the budget"
        );
    }
}
