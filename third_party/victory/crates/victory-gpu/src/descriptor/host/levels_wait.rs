//! (texflicker, #96) `VICTORY_LEVELS_WAIT_SHOWN=N` (off by default; `1`: 60
//! frames, else `N` frames, at most 600): a slot the title writes with a view
//! of a texture made level by level (`crate::tiles`) that has no level with
//! memory yet goes on showing the texture it showed until the new texture's
//! first level has landed, at most `N` frames.
//!
//! Made at once, such a slot showed the new texture with nothing behind it:
//! zeros, a surface black until the texture pass had uploaded it and made the
//! slot again from its first level ([`super::HostHeap::texture_views_remade`]).
//! Only a texture that can stand for the new one is kept (after trustno1
//! `d1c412c`'s kin test): a view placed in memory of the same format and
//! dimension, of another texture. The title's view is noted as made, so its
//! draws, the texture pass and the levels' remaking find it; the slot is made
//! from it when a draw reads it (the draw uploads the levels before it reads
//! them), when the pass shows it after its upload ran (`commit_show`), when a
//! pass finds its first level there, or when its frames are over.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use super::View;

/// Frames a slot may wait (`VICTORY_LEVELS_WAIT_SHOWN`); 0 off.
pub(super) fn frames() -> u64 {
    static N: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *N.get_or_init(|| parse_frames(std::env::var("VICTORY_LEVELS_WAIT_SHOWN").ok().as_deref()))
}

fn parse_frames(value: Option<&str>) -> u64 {
    match value.and_then(|v| v.trim().parse::<u64>().ok()) {
        None | Some(0) => 0,
        Some(1) => 60,
        Some(n) => n.min(600),
    }
}

/// Slots waiting at once, at most: past it a slot is made at once, as before.
const MOST: usize = 65_536;

/// A slot noted as made from `key` that still shows what it showed, until
/// frame `frame`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Pending {
    key: u64,
    frame: u64,
}

/// By host heap and slot.
static PENDING: Mutex<BTreeMap<(usize, u32), Pending>> = Mutex::new(BTreeMap::new());

/// Since the start: slots that kept what they showed, made from their view
/// by a draw's read or a show, by a pass that found the level there, at
/// their last frame, and let go as the title wrote them again.
static COUNTS: [AtomicU64; 5] = [const { AtomicU64::new(0) }; 5];

#[derive(Clone, Copy)]
enum Tally {
    Kept,
    Read,
    Landed,
    Over,
    Rewritten,
}

fn tally(what: Tally) {
    let before = COUNTS[what as usize].fetch_add(1, Relaxed);
    if matches!(what, Tally::Kept) && (before + 1).is_multiple_of(1024) {
        let [kept, read, landed, over, rewritten] = COUNTS.each_ref().map(|c| c.load(Relaxed));
        tracing::info!(
            "levels wait shown, since the start: {kept} slots kept the texture they showed while the new one had no level; \
             made from the new one by a draw or a show {read}, by a pass that found its level {landed}, at their last frame {over}; \
             {rewritten} written again meanwhile (VICTORY_LEVELS_WAIT_SHOWN)"
        );
    }
}

/// Whether `old`, which a slot shows, can stand for `new`: both views placed
/// in memory of textures (not buffers), of the same format and dimension,
/// and not of the same texture.
fn stands_for(old: &View, new: &View) -> bool {
    const BUFFER: u32 = 1;
    match (old, new) {
        (View::Placed(false, a, from), View::Placed(false, b, to)) => {
            a[0] != BUFFER
                && b[0] != BUFFER
                && from[0] == to[0]
                && from[1] == to[1]
                && !super::same_placed_texture(old, new)
        }
        _ => false,
    }
}

/// Whether `view` is of a texture made level by level with no level that
/// has memory: a view of it reads zeros.
fn empty(view: &View) -> bool {
    let Some(View::Shader(r, _)) = super::placed_texture(view) else {
        return false;
    };
    let this = r as crate::object::This;
    crate::resource::host_copy(this).is_some_and(|copy| copy.tiled.is_some())
        && crate::resource::first_shown(this).is_none()
}

impl super::HostHeap {
    fn me(&self) -> usize {
        std::ptr::from_ref(self) as usize
    }

    /// A title write (not a draw's read) of slot `index` with `view`, of key
    /// `key`, that is to be made now: true if the slot goes on showing what
    /// it shows (noted as made from `key`; the caller leaves it).
    pub(super) fn levels_wait(&self, made: &mut [u64], index: u32, key: u64, view: &View) -> bool {
        let n = frames();
        if n == 0 || self.sampler {
            return false;
        }
        let old = made[index as usize];
        if old == 0 {
            return false;
        }
        let Some(shown) = super::view_at(old as u32, (old >> 32) as u32) else {
            return false;
        };
        let held = self
            .kept
            .get(index as usize)
            .is_some_and(|k| k.lock().unwrap_or_else(|e| e.into_inner()).is_some());
        if !held || !stands_for(&shown, view) || !empty(view) {
            return false;
        }
        self.hold_levels(made, index, key, crate::frame::current() + n)
    }

    /// [`super::HostHeap::levels_wait`] with the slot found to wait: until
    /// frame `frame`.
    fn hold_levels(&self, made: &mut [u64], index: u32, key: u64, frame: u64) -> bool {
        {
            let mut pending = PENDING.lock().unwrap_or_else(|e| e.into_inner());
            if pending.len() >= MOST {
                return false;
            }
            pending.insert((self.me(), index), Pending { key, frame });
        }
        made[index as usize] = key;
        // Not shown yet: every look at the slot takes the records' lock and
        // comes here ([`super::HostHeap::levels_read`]).
        if let Some(too) = self.made_too.get(index as usize) {
            too.store(0, std::sync::atomic::Ordering::Release);
        }
        tally(Tally::Kept);
        true
    }

    /// A draw's read of slot `index`, noted as made from `key` already: if
    /// it waits for its texture's levels, it is made now (the draw uploads
    /// them before it reads them; a show runs after its upload).
    pub(super) fn levels_read(&self, index: u32, key: u64, view: &View) {
        if frames() == 0 || self.sampler {
            return;
        }
        self.levels_read_now(index, key, view);
    }

    /// [`super::HostHeap::levels_read`] with the switch on.
    fn levels_read_now(&self, index: u32, key: u64, view: &View) {
        let shown = self
            .made_too
            .get(index as usize)
            .is_none_or(|too| too.load(std::sync::atomic::Ordering::Acquire) == key);
        if shown {
            return;
        }
        {
            let mut pending = PENDING.lock().unwrap_or_else(|e| e.into_inner());
            match pending.get(&(self.me(), index)) {
                Some(p) if p.key == key => {
                    pending.remove(&(self.me(), index));
                }
                _ => return,
            }
        }
        self.make_waited(index, key, view);
        tally(Tally::Read);
    }

    /// Makes slot `index` from `view` (of key `key`) and shows it.
    fn make_waited(&self, index: u32, key: u64, view: &View) {
        match super::make(view, self.cpu(index)) {
            Some((kept, _)) => self.keep(index, kept),
            None => {
                super::null_for(view, self.cpu(index));
                self.keep(index, None);
            }
        }
        if let Some(too) = self.made_too.get(index as usize) {
            too.store(key, std::sync::atomic::Ordering::Release);
        }
    }

    /// At a texture pass: the waiting slots of this heap whose new texture
    /// has its first level now, or whose frames are over, are made from
    /// their view; those the title wrote again since are let go. How many
    /// were made.
    pub(super) fn levels_due(&self) -> usize {
        if frames() == 0 || self.sampler {
            return 0;
        }
        self.levels_due_at(crate::frame::current())
    }

    /// [`super::HostHeap::levels_due`] at frame `frame`, with the switch on.
    fn levels_due_at(&self, frame: u64) -> usize {
        let me = self.me();
        let made = self.made.lock().unwrap_or_else(|e| e.into_inner());
        let mine: Vec<(u32, Pending)> = {
            let pending = PENDING.lock().unwrap_or_else(|e| e.into_inner());
            pending
                .range((me, 0)..=(me, u32::MAX))
                .map(|(&(_, index), &p)| (index, p))
                .collect()
        };
        if mine.is_empty() {
            return 0;
        }
        let mut done = Vec::new();
        let mut makes = Vec::new();
        for (index, p) in mine {
            let view = super::view_at(p.key as u32, (p.key >> 32) as u32);
            match view {
                Some(view) if made.get(index as usize) == Some(&p.key) => {
                    let landed = !empty(&view);
                    if landed || frame >= p.frame {
                        done.push((index, p));
                        makes.push((index, p.key, view, landed));
                    }
                }
                _ => {
                    done.push((index, p));
                    tally(Tally::Rewritten);
                }
            }
        }
        {
            let mut pending = PENDING.lock().unwrap_or_else(|e| e.into_inner());
            for (index, p) in done {
                if pending.get(&(me, index)) == Some(&p) {
                    pending.remove(&(me, index));
                }
            }
        }
        let n = makes.len();
        for (index, key, view, landed) in makes {
            self.make_waited(index, key, &view);
            tally(if landed { Tally::Landed } else { Tally::Over });
        }
        drop(made);
        n
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wait_is_off_unless_asked_and_bounded() {
        assert_eq!(parse_frames(None), 0);
        assert_eq!(parse_frames(Some("0")), 0);
        assert_eq!(parse_frames(Some("x")), 0);
        assert_eq!(parse_frames(Some("1")), 60);
        assert_eq!(parse_frames(Some(" 12 ")), 12);
        assert_eq!(parse_frames(Some("100000")), 600);
    }

    fn placed(resource: [u32; 14], format: u32, dimension: u32, address: u32) -> View {
        let mut desc = [0u32; 16];
        (desc[0], desc[1], desc[10], desc[11]) = (format, dimension, address, 0xb);
        View::Placed(false, resource, desc)
    }

    /// What may stand for a texture with no level yet: another texture's
    /// view of the same format and dimension; not the same texture, not a
    /// buffer, not another format or dimension, not a view to write.
    #[test]
    fn a_slot_keeps_only_a_texture_that_can_stand_for_the_new_one() {
        let texture = [3, 0, 0, 0, 512, 0, 256, 0x8_0001, 98, 1, 0, 0x105, 0, 0];
        let mut larger = texture;
        (larger[4], larger[6]) = (1024, 512);
        let mut buffer = texture;
        buffer[0] = 1;
        let old = placed(texture, 98, 4, 0x1000_0000);
        assert!(stands_for(&old, &placed(larger, 98, 4, 0x2000_0000)));
        assert!(stands_for(&old, &placed(texture, 98, 4, 0x2000_0000)));
        assert!(
            !stands_for(&old, &placed(texture, 98, 4, 0x1000_0000)),
            "the same texture"
        );
        assert!(
            !stands_for(&old, &placed(larger, 83, 4, 0x2000_0000)),
            "another format"
        );
        assert!(
            !stands_for(&old, &placed(larger, 98, 9, 0x2000_0000)),
            "another dimension"
        );
        assert!(
            !stands_for(&old, &placed(buffer, 98, 4, 0x2000_0000)),
            "a buffer"
        );
        let View::Placed(_, r, d) = placed(larger, 98, 4, 0x2000_0000) else {
            unreachable!()
        };
        assert!(
            !stands_for(&old, &View::Placed(true, r, d)),
            "a view to write"
        );
    }

    /// A waiting slot is noted as made from the new key and keeps its host
    /// copy; a draw's read makes it and lets it go; a slot the title wrote
    /// again is let go at the pass without a make.
    #[test]
    fn a_waiting_slot_is_made_at_its_read_and_let_go_once_rewritten() {
        let Some(heap) = super::super::HostHeap::new(false, 8) else {
            return; // no host GPU
        };
        let Some(host) = crate::host::host() else {
            return;
        };
        let shown = |i: u32| {
            heap.kept[i as usize]
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                .map(|r| windows::core::Interface::as_raw(r) as usize)
        };
        let (a, _) = host.upload_buffer(256).unwrap();
        let raw = windows::core::Interface::as_raw(&a) as usize;
        let (old, new) = (5u64 << 32 | 1, 6u64 << 32 | 2);
        let mut made = vec![0u64; 8];
        for i in [1, 2] {
            made[i] = old;
            heap.keep(i as u32, Some(a.clone()));
            heap.made_too[i].store(old, Relaxed);
        }
        assert!(heap.hold_levels(&mut made, 1, new, u64::MAX));
        assert!(heap.hold_levels(&mut made, 2, new, u64::MAX));
        assert_eq!(made[1], new, "noted as made from the new view");
        assert_eq!(
            heap.made_too[1].load(Relaxed),
            0,
            "not shown: looks take the lock"
        );
        assert_eq!(shown(1), Some(raw), "the old host copy is still shown");
        // A read with another key leaves it waiting.
        // A view that makes nothing on the card (no buffer is there): a
        // made-up placed texture could take the shared device down.
        let view = View::Constant(0x7fff_0000, 256);
        heap.levels_read_now(1, old, &view);
        assert!(PENDING.lock().unwrap().contains_key(&(heap.me(), 1)));
        // The read of the new key makes it (null here).
        heap.levels_read_now(1, new, &view);
        assert!(!PENDING.lock().unwrap().contains_key(&(heap.me(), 1)));
        assert_eq!(heap.made_too[1].load(Relaxed), new, "shown now");
        assert_ne!(shown(1), Some(raw), "the old host copy is let go");
        // Slot 2 written again meanwhile: let go at the pass, left as it is.
        *heap.made.lock().unwrap().get_mut(2).unwrap() = 7u64 << 32 | 3;
        assert_eq!(heap.levels_due_at(10), 0);
        assert!(!PENDING.lock().unwrap().contains_key(&(heap.me(), 2)));
        assert_eq!(shown(2), Some(raw));
    }
}
