//! (El1xr) `VICTORY_STAGING_HOLD_MS=<ms>` (live, off by default; job `memcut`): while
//! `VICTORY_POOL_SLACK_MB` is pressed (the system's free commit under it), the staging buffer a
//! texture keeps after its upload goes once out of use this long and [`FRAMES`] frames,
//! instead of the usual 10 s (`crate::host::ram_idle_ms` with RAM free), so the staging pool's
//! blocks empty and the pool slack trim lets them go.
//!
//! At the run's lowest free commit (`train_b5q/01_bo`, 295 MB) the staging pool held 1600 MB
//! of blocks for 358 MB of pieces: kept staging buffers pin blocks whose other pieces are back.
//!
//! Dark frames (texflicker round 12, `VICTORY_TABLE_NULL_ONCE`): a texture a table reads that
//! has lost its staging buffer reads as never staged, and the table leaves its slot null for a
//! frame while the prepare threads make it again. So:
//! - only buffers no list holds and no list recorded a use of (table reads record one through
//!   `crate::texture::upload_from`) for the hold time and [`FRAMES`] frames go;
//! - a texture whose buffer went here is noted, and a table that meets it again with its host
//!   copy holding the mips it shows reads that copy as it is, its upload recorded now
//!   ([`reads_as_is`], from `crate::table_defer::wait`): never a null slot for it, whether the
//!   switch is still set or not. Its host copy is the texture drawn from; only the next upload
//!   pays (it detiles into a new staging buffer).
//!
//! At every commit line a `staging hold:` line gives what went since the last one and the
//! staging pool's blocks, the process's private bytes and the system's free commit at the
//! last line and now. Not a shader or pipeline key.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{LazyLock, Mutex};

use victory_core::settings::Live;

/// Frames a staging buffer is out of use before the hold lets it go (at 20 to 30 frames a
/// second, 2 to 3 s), never fewer than the sweep's own.
pub(crate) const FRAMES: u64 = 60;

static HOLD: Live = Live::new("VICTORY_STAGING_HOLD_MS", ms_bits);

fn ms_bits(value: Option<&str>) -> u64 {
    value
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(0)
}

/// `VICTORY_STAGING_HOLD_MS`, if set (live).
fn hold() -> Option<u64> {
    Some(HOLD.get()).filter(|&v| v > 0)
}

/// Textures whose staging buffer the hold let go, until the title releases them.
static LET_GO: LazyLock<Mutex<HashSet<usize>>> = LazyLock::new(Default::default);

/// Since the start: buffers let go by the hold, their bytes, table reads of them read as is.
static ACTED: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

/// At the last commit line: buffers let go so far, staging pool blocks, held, process private,
/// free commit (bytes).
static LAST: [AtomicU64; 5] = [const { AtomicU64::new(0) }; 5];

/// The idle time and frames for the staging sweep given its usual ones: while pressed and set,
/// the hold's when shorter, with at least [`FRAMES`] frames.
pub(crate) fn idle(ms: u64, frames: u64) -> (u64, u64) {
    hold_times(
        ms,
        frames,
        hold(),
        hold().is_some() && crate::poolslack::pressed(),
    )
}

fn hold_times(ms: u64, frames: u64, hold: Option<u64>, pressed: bool) -> (u64, u64) {
    match hold {
        Some(hold) if pressed && hold < ms => (hold, frames.max(FRAMES)),
        _ => (ms, frames),
    }
}

/// The sweep let go of `resource`'s staging buffer of `bytes` that its usual time keeps.
pub(crate) fn let_go(resource: usize, bytes: u64) {
    LET_GO
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(resource);
    ACTED[0].fetch_add(1, Relaxed);
    ACTED[1].fetch_add(bytes, Relaxed);
}

/// Whether a table that would leave `resource`'s slot null reads its host copy as it is: its
/// staging buffer went by the hold and the copy holds the mips the view shows (`filled`).
pub(crate) fn reads_as_is(resource: usize, filled: bool) -> bool {
    let read = keeps_slot(filled, || {
        LET_GO
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&resource)
    });
    if read {
        ACTED[2].fetch_add(1, Relaxed);
    }
    read
}

fn keeps_slot(filled: bool, let_go: impl FnOnce() -> bool) -> bool {
    filled && let_go()
}

/// The title released `resource`.
pub(crate) fn forget(resource: usize) {
    let mut set = LET_GO.lock().unwrap_or_else(|e| e.into_inner());
    if !set.is_empty() {
        set.remove(&resource);
    }
}

/// At every commit line, given the staging pool's blocks and held bytes, the process's private
/// bytes and the system's free commit: a line if the hold let go of anything since the last.
pub(crate) fn tick(blocks: u64, held: u64, private: u64, free: u64) {
    let now = [ACTED[0].load(Relaxed), blocks, held, private, free];
    let last = LAST.each_ref().map(|a| a.load(Relaxed));
    for (a, v) in LAST.iter().zip(now) {
        a.store(v, Relaxed);
    }
    let went = now[0].saturating_sub(last[0]);
    if went == 0 {
        return;
    }
    let mb = |v: u64| v >> 20;
    tracing::info!(
        "staging hold: since the last line let go {went} staging buffers of textures out of use {} ms and {FRAMES} frames that the usual time keeps (none held by a list); staging pool blocks {} -> {} MB (held {} -> {} MB), process private {} -> {} MB, free commit {} -> {} MB (last line -> now); since the start {} ({} MB), table reads of them read as is {}",
        hold().unwrap_or(0),
        mb(last[1]),
        mb(blocks),
        mb(last[2]),
        mb(held),
        mb(last[3]),
        mb(private),
        mb(last[4]),
        mb(free),
        ACTED[0].load(Relaxed),
        mb(ACTED[1].load(Relaxed)),
        ACTED[2].load(Relaxed),
    );
}

/// The commit line's part.
pub(crate) fn summary() -> String {
    format!(
        "VICTORY_STAGING_HOLD_MS {} (let go {} ({} MB), read as is {})",
        hold().unwrap_or(0),
        ACTED[0].load(Relaxed),
        ACTED[1].load(Relaxed) >> 20,
        ACTED[2].load(Relaxed),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Live and off by default; shorter only while pressed, never fewer than [`FRAMES`]
    /// frames, never longer than the usual time.
    #[test]
    fn the_hold_shortens_only_while_pressed_and_keeps_its_frames() {
        victory_core::settings::check_live(
            "VICTORY_STAGING_HOLD_MS",
            "2000",
            hold,
            None,
            Some(2000),
        );
        assert_eq!(hold_times(10_000, 4, Some(2_000), true), (2_000, FRAMES));
        assert_eq!(hold_times(10_000, 200, Some(2_000), true), (2_000, 200));
        assert_eq!(hold_times(10_000, 4, Some(2_000), false), (10_000, 4));
        assert_eq!(hold_times(10_000, 4, None, true), (10_000, 4));
        assert_eq!(hold_times(1_000, 4, Some(2_000), true), (1_000, 4));
    }

    /// A texture whose staging buffer the hold let go is never left null by a table while its
    /// host copy holds what the view shows; one the hold did not touch, or whose host copy
    /// is not filled, is left to the table as before.
    #[test]
    fn a_table_reads_a_texture_the_hold_let_go_as_it_is() {
        const T: usize = 0x5ea1_0000;
        assert!(!reads_as_is(T, true));
        let_go(T, 1 << 20);
        assert!(reads_as_is(T, true));
        assert!(!reads_as_is(T, false));
        forget(T);
        assert!(!reads_as_is(T, true));
        assert!(!keeps_slot(false, || panic!("not asked when not filled")));
    }
}
