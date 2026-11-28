//! (`VICTORY_CARD_KEPT_WATCH=1`, [`crate::texture::own_copy`]) The census of host copies of
//! textures the title fills that the card wrote: the sweeps never let one go (`let_go_idle`,
//! `slot_evict_with`, `idle_sweep::sweep` skip them), and none is made again from title
//! memory. Of them, those the card wrote only some subresources of and that were never filled
//! from title memory hold only what the card wrote; the rest is empty for good.

//!
//! Also (item 3 of issue #55): placed textures filled by a card copy from a render target at
//! their memory (`alias_of`, `crate::texture::alias`) instead of from title memory. Counted, and
//! of them those whose target nothing else has used for [`STALE_MS`] and [`STALE_FRAMES`]: the
//! title may have reused that memory for streamed texels the view never shows.

use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering::{Acquire, Relaxed};

use super::{Resource, fills, resource, textures};
use crate::object::This;
use crate::texture::own_copy;

/// A target unused this long (and this many frames) when a placed view is filled from it.
const STALE_MS: u64 = 2_000;
const STALE_FRAMES: u64 = 60;
/// `card kept watch: alias` lines in a run.
const ALIAS_EXAMPLES: u64 = 24;

/// Alias fills from a target since the last line, of them from a stale target; examples said.
static ALIAS: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];
/// Targets alias fills read, by address: the last use by anything else (a [`crate::host::moment`])
/// and the moment of the last alias fill (the fill's own use of the target marks it used).
static TARGET_USES: std::sync::Mutex<Option<std::collections::HashMap<usize, (u64, u64)>>> =
    std::sync::Mutex::new(None);
/// Targets [`TARGET_USES`] keeps before it starts over.
const MOST_TARGETS: usize = 4096;

/// (`VICTORY_CARD_KEPT_WATCH`) Placed texture `placed` is about to be filled by a card copy
/// from target `source`; asked before the copy takes the target (which marks it used).
pub(crate) fn alias_fill(placed: This, source: This) {
    if !own_copy::watch() {
        return;
    }
    ALIAS[0].fetch_add(1, Relaxed);
    let used = resource(source).host.used.load(Relaxed);
    let used = other_use(source as usize, used, crate::host::moment());
    if used == 0 || !crate::host::long_ago(used, STALE_MS, STALE_FRAMES) {
        return;
    }
    ALIAS[1].fetch_add(1, Relaxed);
    if ALIAS[2].load(Relaxed) < ALIAS_EXAMPLES && ALIAS[2].fetch_add(1, Relaxed) < ALIAS_EXAMPLES {
        let idle_frames = (crate::host::moment() >> 32).saturating_sub(used >> 32);
        tracing::info!(
            placed = format_args!("{:#x}", placed as usize),
            placed_texture = %desc_line(resource(placed)),
            target = format_args!("{:#x}", source as usize),
            target_texture = %desc_line(resource(source)),
            idle_frames,
            frame = crate::frame::current(),
            "card kept watch: alias (a placed texture filled from a render target at its memory that nothing else used for 2 s and 60 frames: title memory may hold texels the view never shows)"
        );
    }
}

/// The last use of target `target` by anything but an alias fill, given its last use `used` and
/// the moment `now` of this fill.
fn other_use(target: usize, used: u64, now: u64) -> u64 {
    let mut map = TARGET_USES.lock().unwrap_or_else(|e| e.into_inner());
    let map = map.get_or_insert_with(std::collections::HashMap::new);
    if map.len() >= MOST_TARGETS && !map.contains_key(&target) {
        map.clear();
    }
    let entry = map.entry(target).or_insert((used, 0));
    if used != entry.1 {
        entry.0 = used;
    }
    entry.1 = now;
    entry.0
}

/// A title texture's size, format, mips and layers, for a log line.
fn desc_line(r: &Resource) -> String {
    format!(
        "{}x{} fmt {} mips {} layers {}",
        r.desc.width, r.desc.height, r.desc.format, r.desc.mip_levels, r.desc.depth_or_array_size
    )
}

/// Every 30 frames with the watch on, on the submitting thread before the sweep: one
/// `card kept watch:` line.
pub(crate) fn tick() {
    if !own_copy::watch() {
        return;
    }
    let frame = crate::frame::current();
    if !own_copy::line_due(frame) {
        return;
    }
    let started = std::time::Instant::now();
    let [kept, kept_bytes, empty, empty_bytes] = census();
    let [own, other, never_filled, let_through] = own_copy::take_counts();
    let alias_fills = ALIAS[0].swap(0, Relaxed);
    let alias_stale = ALIAS[1].swap(0, Relaxed);
    tracing::info!(
        frame,
        kept,
        kept_mib = kept_bytes >> 20,
        never_filled_kept = empty,
        never_filled_kept_mib = empty_bytes >> 20,
        left_out_own = own,
        left_out_other = other,
        left_out_never_filled = never_filled,
        let_through,
        alias_fills,
        alias_stale,
        census_us = started.elapsed().as_micros() as u64,
        "card kept watch: card-written textures the sweeps keep (never refilled from title memory), and prelude uploads VICTORY_PRELUDE_RECHECK left out and placed textures filled from a target (of them from a stale one) since the last line"
    );
}

/// Card-written host copies of textures the title fills and their bytes; of them, those never
/// filled from title memory and their bytes. Names up to `own_copy`'s examples of the latter.
fn census() -> [u64; 4] {
    let mut c = [0u64; 4];
    let all = textures();
    for &at in all.iter() {
        // SAFETY: a texture leaves the table before it is freed, under the lock held here.
        let r = unsafe { &*(at as *const Resource) };
        if !fills(&r.desc) || !r.host.host_written.load(Relaxed) {
            continue;
        }
        let made = r.host.made.lock().unwrap_or_else(|e| e.into_inner());
        let Some((host, _)) = made.as_ref() else {
            continue;
        };
        c[0] += 1;
        c[1] += host.bytes;
        if host.uploaded_from.load(Acquire) == u32::MAX
            && crate::texture::card_wrote_part(at, &host.texture)
        {
            c[2] += 1;
            c[3] += host.bytes;
            own_copy::example(
                at,
                "kept by the sweeps, never filled from title memory",
                || crate::texture::describe(&host.texture),
            );
        }
    }
    c
}

#[cfg(test)]
mod tests {
    use super::other_use;

    #[test]
    fn a_targets_own_alias_fills_do_not_count_as_its_use() {
        const T: usize = 0xC0DE_0201;
        let at = |frame: u64, tenths: u64| frame << 32 | tenths;
        // The title drew to it at frame 10; alias fills at 11 and 12 (each marks it used).
        assert_eq!(other_use(T, at(10, 100), at(11, 101)), at(10, 100));
        assert_eq!(other_use(T, at(11, 101), at(12, 102)), at(10, 100), "only the fill used it");
        // Drawn to again at frame 13.
        assert_eq!(other_use(T, at(13, 103), at(14, 104)), at(13, 103));
    }
}
