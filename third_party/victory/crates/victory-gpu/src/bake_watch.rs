//! `VICTORY_BAKE_WATCH=1`: a cheap watch, meant to stay on in every run of
//! a batch, on what a character's baked skin and the face blendshape chain
//! went through in each frame (a face drawn black in about one run in five).
//!
//! Counted per frame, with atomics: bake draws, bake texture passes that
//! left textures due (the bake recorded reading textures not uploaded yet),
//! first host writes (a bake's copy into a texture), prelude uploads left
//! out over a host-written texture, prelude uploads into the memory of
//! another texture the host wrote (an alias of a baked texture), zeros or
//! lost writes of levels on demand in host-written textures, the face
//! chain's dispatches, chain lists submitted while a bake list recorded
//! before them was still not submitted, and the buffer views the chain
//! read. A frame with any of these gives one `bake watch frame` line; so
//! does every 600th frame. The events behind them give their own
//! `bake watch:` lines (at most [`MAX_LINES`] in all). Off, every hook is
//! one cached boolean test.

use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Mutex, OnceLock};

/// The face blendshape chain: `CS_BlendshapeTranslationsHSSGraphOutputExtract`,
/// `CS_ComputeStickyLips`, `CS_ReconstructBlendshapeNormals`, and the ML
/// Gather whose buffer the first reads.
const CHAIN: [&str; 4] = [
    "970c4fbb3b017c0e",
    "068c90badc86f1fd",
    "e4a7e088c7780ce8",
    "aaf7da7a94dc72a9",
];
const MAX_LINES: u64 = 20_000;
const HEARTBEAT_FRAMES: u64 = 600;

#[derive(Clone, Copy)]
#[repr(usize)]
pub(crate) enum C {
    BakeDraws,
    BakeShort,
    DueLeftMost,
    Marks,
    PreludeSkipped,
    AliasUploads,
    ZerosOverBaked,
    LostNoUpload,
    LostNoTiles,
    ChainDispatches,
    ChainAhead,
}
const N: usize = 11;
const NAMES: [&str; N] = [
    "bake_draws",
    "bake_short",
    "due_left_most",
    "marks",
    "prelude_skipped",
    "alias_uploads",
    "zeros_over_baked",
    "lost_no_upload",
    "lost_no_tiles",
    "chain_dispatches",
    "chain_ahead_of_bake",
];

static COUNTS: [AtomicU64; N] = [const { AtomicU64::new(0) }; N];
static FRAME: AtomicU64 = AtomicU64::new(0);
static LINES: AtomicU64 = AtomicU64::new(0);
/// The fingerprint of the chain's views this frame, and the last frame's.
static VIEWS_PRINT: AtomicU64 = AtomicU64::new(0);
static VIEWS_LAST: AtomicU64 = AtomicU64::new(0);

#[derive(Default)]
struct Lists {
    /// Lists that recorded a bake draw and are not submitted yet, with the
    /// frame they were recorded in.
    bakes: Vec<(usize, u64)>,
    /// Lists that recorded a dispatch of the chain, not submitted yet.
    chains: Vec<(usize, u64)>,
    /// The chain's buffer views this frame: address, length, written.
    views: Vec<(u64, u64, bool)>,
}
static LISTS: Mutex<Lists> = Mutex::new(Lists {
    bakes: Vec::new(),
    chains: Vec::new(),
    views: Vec::new(),
});

thread_local! {
    /// The pipeline being bound on this thread is one of the chain's.
    static IN_CHAIN: Cell<bool> = const { Cell::new(false) };
}

pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_BAKE_WATCH").is_some_and(|v| v == "1"))
}

fn lists() -> std::sync::MutexGuard<'static, Lists> {
    LISTS.lock().unwrap_or_else(|e| e.into_inner())
}

/// Whether one more event line may be written.
fn say() -> bool {
    LINES.fetch_add(1, Relaxed) < MAX_LINES
}

pub(crate) fn add(c: C, n: u64) {
    if on() {
        roll();
        COUNTS[c as usize].fetch_add(n, Relaxed);
    }
}

fn most(c: C, n: u64) {
    COUNTS[c as usize].fetch_max(n, Relaxed);
}

/// Whether a frame's counts are worth a line: anything beyond the chain's
/// own dispatches and the bake draws themselves.
fn notable(c: &[u64; N], views_changed: bool) -> bool {
    views_changed
        || c.iter()
            .enumerate()
            .any(|(i, &n)| n > 0 && i != C::BakeDraws as usize && i != C::ChainDispatches as usize)
}

/// At the first hook of a new frame: the line for the frame before.
fn roll() {
    let now = crate::frame::current();
    let prev = FRAME.load(Relaxed);
    if prev == now || FRAME.compare_exchange(prev, now, Relaxed, Relaxed).is_err() {
        return;
    }
    let c: [u64; N] = std::array::from_fn(|i| COUNTS[i].swap(0, Relaxed));
    let print = VIEWS_PRINT.swap(0, Relaxed);
    let last = VIEWS_LAST.swap(print, Relaxed);
    let views_changed = print != 0 && print != last;
    let views = std::mem::take(&mut lists().views);
    if !(notable(&c, views_changed) || prev % HEARTBEAT_FRAMES == 0) || !say() {
        return;
    }
    let counts = NAMES
        .iter()
        .zip(c)
        .filter(|&(_, n)| n > 0)
        .map(|(name, n)| format!("{name}={n}"))
        .collect::<Vec<_>>()
        .join(" ");
    if views_changed {
        let views = views
            .iter()
            .map(|&(at, len, uav)| format!("{at:#x}+{len:#x}{}", if uav { "w" } else { "r" }))
            .collect::<Vec<_>>()
            .join(",");
        tracing::info!(frame = prev, %counts, chain_views = %views, "bake watch frame (the chain read other buffers)");
    } else {
        tracing::info!(frame = prev, %counts, "bake watch frame");
    }
}

/// A pipeline is bound for a draw or a dispatch into `list`.
pub(crate) fn binding<'a>(list: usize, mut stages: impl Iterator<Item = &'a str>, is_bake: bool) {
    if !on() {
        return;
    }
    roll();
    let chain = stages.any(|h| CHAIN.contains(&h));
    IN_CHAIN.set(chain);
    if is_bake {
        COUNTS[C::BakeDraws as usize].fetch_add(1, Relaxed);
        let frame = crate::frame::current();
        let mut l = lists();
        if !l.bakes.iter().any(|&(at, _)| at == list) {
            l.bakes.push((list, frame));
        }
    }
}

/// A dispatch whose pipeline [`binding`] found to be the chain's was bound.
pub(crate) fn dispatched(list: usize) {
    if !on() || !IN_CHAIN.get() {
        return;
    }
    COUNTS[C::ChainDispatches as usize].fetch_add(1, Relaxed);
    let frame = crate::frame::current();
    let mut l = lists();
    if !l.chains.iter().any(|&(at, _)| at == list) {
        l.chains.push((list, frame));
    }
}

/// A buffer view is bound for the pipeline being bound.
pub(crate) fn view(address: u64, len: u64, uav: bool) {
    if !on() || !IN_CHAIN.get() {
        return;
    }
    let mut l = lists();
    if l.views.contains(&(address, len, uav)) {
        return;
    }
    l.views.push((address, len, uav));
    drop(l);
    // Order-free: the views as a set.
    let mix = (address ^ len.rotate_left(32) ^ u64::from(uav)).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    VIEWS_PRINT.fetch_add(mix | 1, Relaxed);
}

/// Of `chains` submitted in `submitted`, how many go to the card while a
/// bake list recorded no later than them is not in front of them: still
/// waiting (not in this submission) or behind them in it.
fn chain_ahead(bakes: &[(usize, u64)], chains: &[(usize, u64)], submitted: &[usize]) -> u64 {
    let place = |list: usize| submitted.iter().position(|&l| l == list);
    let mut n = 0;
    for &(chain, frame) in chains {
        let Some(at) = place(chain) else {
            continue;
        };
        let ahead = bakes
            .iter()
            .filter(|&&(_, f)| f <= frame)
            .any(|&(bake, _)| place(bake).is_none_or(|b| b > at));
        n += u64::from(ahead);
    }
    n
}

/// A queue submits `submitted` (host lists, in order).
pub(crate) fn submit(submitted: &[usize]) {
    if !on() {
        return;
    }
    roll();
    let frame = crate::frame::current();
    let mut l = lists();
    let ahead = chain_ahead(&l.bakes, &l.chains, submitted);
    let gone =
        |&(list, f): &(usize, u64)| !submitted.contains(&list) && frame.saturating_sub(f) < 8;
    l.bakes.retain(gone);
    l.chains.retain(gone);
    let waiting = l.bakes.len();
    drop(l);
    if ahead > 0 {
        COUNTS[C::ChainAhead as usize].fetch_add(ahead, Relaxed);
        if say() {
            tracing::warn!(
                frame,
                chain_lists = ahead,
                bake_lists_waiting = waiting,
                "bake watch: face chain submitted before a bake list recorded with or before it"
            );
        }
    }
}

/// A bake draw's texture pass ended with `due` textures picked by index
/// still due, after `used_ms` of `budget_ms`.
pub(crate) fn bake_pass(frame: u64, due: u64, used_ms: u64, budget_ms: u64) {
    if !on() || due == 0 {
        return;
    }
    roll();
    COUNTS[C::BakeShort as usize].fetch_add(1, Relaxed);
    most(C::DueLeftMost, due);
    if say() {
        tracing::warn!(
            frame,
            due_left = due,
            used_ms,
            budget_ms,
            "bake watch: a bake draw recorded with textures picked by index still due"
        );
    }
}

/// The host wrote `resource` for the first time (a copy or a resolve).
pub(crate) fn marked(resource: usize, width: u32, height: u32, format: u32) {
    if !on() {
        return;
    }
    add(C::Marks, 1);
    if say() {
        tracing::info!(
            frame = crate::frame::current(),
            resource = format_args!("{resource:#x}"),
            width,
            height,
            format,
            "bake watch: the host wrote a texture (first time)"
        );
    }
}

/// A prelude upload over `resource` went to the card although its memory
/// lies in that of `baked`, which the host wrote.
pub(crate) fn alias_upload(resource: usize, address: usize, baked: usize, baked_at: u64) {
    add(C::AliasUploads, 1);
    if on() && say() {
        tracing::warn!(
            frame = crate::frame::current(),
            resource = format_args!("{resource:#x}"),
            address = format_args!("{address:#x}"),
            baked = format_args!("{baked:#x}"),
            baked_at = format_args!("{baked_at:#x}"),
            "bake watch: a prelude upload from the memory of a texture the host wrote"
        );
    }
}

/// Levels on demand of `resource`: `what` happened to `levels` (bits).
pub(crate) fn levels(c: C, resource: usize, levels: u32) {
    add(c, 1);
    if on() && say() {
        let what = NAMES[c as usize];
        tracing::warn!(
            frame = crate::frame::current(),
            resource = format_args!("{resource:#x}"),
            levels = format_args!("{levels:#x}"),
            host_wrote = crate::texture::host_wrote(resource),
            what,
            "bake watch: levels on demand of a texture the card writes"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{C, N, chain_ahead, notable};

    #[test]
    fn a_chain_list_ahead_of_a_bake_list_is_counted() {
        let bakes = [(1, 10), (2, 12)];
        let chains = [(5, 10), (6, 11)];
        // Bake 1 goes first: chain 5 is fine; bake 1 is in front of 6 too.
        assert_eq!(chain_ahead(&bakes, &chains, &[1, 5, 6]), 0);
        // Bake 1 behind chain 5 in the same submission.
        assert_eq!(chain_ahead(&bakes, &chains, &[5, 1]), 1);
        // Bake 1 not submitted yet: both chains are ahead of it.
        assert_eq!(chain_ahead(&bakes, &chains, &[5, 6]), 2);
        // Bake 2 was recorded after both chains: it does not count.
        assert_eq!(chain_ahead(&bakes[1..], &chains, &[5, 6]), 0);
        // Chains not in the submission are not counted.
        assert_eq!(chain_ahead(&bakes, &chains, &[1]), 0);
    }

    #[test]
    fn plain_frames_are_not_notable() {
        let mut c = [0u64; N];
        c[C::BakeDraws as usize] = 4;
        c[C::ChainDispatches as usize] = 9;
        assert!(!notable(&c, false));
        assert!(notable(&c, true));
        c[C::BakeShort as usize] = 1;
        assert!(notable(&c, false));
    }
}
