//! (El1xr) `VICTORY_IDLE_EVICT_S=<s>` (live, off by default; job `memcut`, #190): while the
//! system's free commit is under `VICTORY_IDLE_EVICT_MB` (6144 unless set; 0: whatever it is),
//! host copies of textures the title fills that a slot still shows but no table and no list
//! has read for `<s>` seconds and [`frames`] frames, and whose title memory is unchanged since
//! their last upload, are let go with their slots; the next table read makes them again from
//! title memory and uploads them at once.
//!
//! The commit census (`train_b5q/01_bo`) puts 2.8 GB of the card's 8.8 GB of host textures in
//! copies idle for the usual time but still held by a slot (`filled idle but held` on the ram
//! line): the idle sweep (`crate::resource::let_go_idle`) takes only copies no slot shows, and
//! the card's memory is charged to commit whole (WDDM backing store).
//!
//! What may go (`crate::resource::idle_sweep`):
//! - a copy of a texture the title fills, never written by the host, not made level by level,
//!   not made whole from the store, without a twin;
//! - held by no list (`Arc` count) and asked for by nothing for the idle time and frames;
//! - shown only by title slots a draw read through a table within the last 240 frames (which
//!   that table read makes again by itself), or, with `VICTORY_IDLE_EVICT_WIDE=1`, by title
//!   slots a table read at some time; never by a slot no table ever read (shaders pick those
//!   by index: nothing would make them again) nor by a dynamic one;
//! - its title memory unchanged since its upload: its staging buffer was uploaded and not
//!   detiled since, and the write watch (or the guard of view memory) saw no write to it after
//!   that detiling ([`crate::texture::unchanged_since_upload`]). So the copy made again holds
//!   exactly what the one let go held. A texture whose staging buffer is gone, or whose
//!   memory is neither watched nor guarded, cannot tell and stays.
//!
//! Never a null slot on a re-read (`VICTORY_TABLE_NULL_ONCE` semantics): a texture let go here
//! is noted until the title releases it, and a table that meets it again makes and uploads it
//! where the draw is recorded instead of leaving its slot null while the prepare threads make
//! it ([`reads_now`], from `crate::table_defer::wait`), whether the switch is still set or not.
//!
//! The scan runs on a thread of its own every [`EVERY_MS`] (a pass over every slot of every
//! heap is too long for the submitting thread), at most [`STEP`] bytes a pass. At every commit
//! line an `idle evict:` line gives what went and came back since the last one, the census of
//! idle copies by what holds them, and the process's private bytes and the system's free
//! commit at the last line and now. Not a shader or pipeline key.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::{LazyLock, Mutex};

use victory_core::settings::Live;

/// Most bytes one pass lets go.
pub(crate) const STEP: u64 = 256 << 20;
/// How often the thread looks.
pub(crate) const EVERY_MS: u64 = 1_000;
/// A texture made again within this long after it went was a bad pick.
const FAST_BACK_MS: u64 = 2_000;
/// A texture that came back is not picked again for this long.
pub(crate) const PROTECT_MS: u64 = 60_000;
/// Free commit under which it acts unless `VICTORY_IDLE_EVICT_MB` says otherwise.
const DEFAULT_FLOOR_MB: u64 = 6_144;

static IDLE: Live = Live::new("VICTORY_IDLE_EVICT_S", secs_bits);
static FLOOR: Live = Live::new("VICTORY_IDLE_EVICT_MB", floor_bits);
static WIDE: Live = Live::flag("VICTORY_IDLE_EVICT_WIDE");

fn secs_bits(value: Option<&str>) -> u64 {
    value
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(0)
}

fn floor_bits(value: Option<&str>) -> u64 {
    value
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_FLOOR_MB)
        .saturating_mul(1 << 20)
}

/// `VICTORY_IDLE_EVICT_S`, if set (live).
fn idle_s() -> Option<u64> {
    Some(IDLE.get()).filter(|&v| v > 0)
}

/// Frames a copy must be out of use besides the seconds: twenty a second, at least 60.
pub(crate) fn frames(secs: u64) -> u64 {
    secs.saturating_mul(20).max(60)
}

/// What one pass does: the idle time and frames, how many bytes it lets go at most, and
/// whether slots a table read at some time count.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Plan {
    pub(crate) idle_ms: u64,
    pub(crate) frames: u64,
    pub(crate) need: u64,
    pub(crate) wide: bool,
}

/// The pass to make now, if set and pressed.
pub(crate) fn plan() -> Option<Plan> {
    plan_for(
        idle_s(),
        FLOOR.get(),
        crate::commitline::free_commit(),
        WIDE.get_bool(),
    )
}

fn plan_for(secs: Option<u64>, floor: u64, free: u64, wide: bool) -> Option<Plan> {
    let secs = secs?;
    let need = if floor == 0 {
        STEP
    } else {
        floor.checked_sub(free).filter(|&n| n > 0)?.min(STEP)
    };
    Some(Plan {
        idle_ms: secs.saturating_mul(1000),
        frames: frames(secs),
        need,
        wide,
    })
}

/// Starts the thread once the switch is set (called by the idle sweep, at every submission).
pub(crate) fn start() {
    static STARTED: AtomicBool = AtomicBool::new(false);
    if idle_s().is_none() || STARTED.swap(true, Relaxed) {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("victory idle evict".into())
        .spawn(|| {
            loop {
                std::thread::sleep(std::time::Duration::from_millis(EVERY_MS));
                if let Some(plan) = plan() {
                    crate::resource::idle_sweep::sweep(plan);
                }
            }
        });
    if spawned.is_err() {
        STARTED.store(false, Relaxed);
    }
}

/// Textures let go here, until the title releases them: when they went (`crate::host::moment`,
/// 0 once made again) and when they last came back (0: not yet).
static GONE: LazyLock<Mutex<HashMap<usize, (u64, u64)>>> = LazyLock::new(Default::default);
/// Something was ever let go here (spares the lock on every host copy made).
static ANY: AtomicBool = AtomicBool::new(false);

/// Since the start: copies let go, their bytes, slots put back to waiting, made again, of
/// them within [`FAST_BACK_MS`], table reads made at once, passes.
static ACTED: [AtomicU64; 7] = [const { AtomicU64::new(0) }; 7];

/// What the last pass found of the copies idle for its time (bytes), by what keeps them:
/// a list, nothing the pass minds (may go: slots a table makes again, memory unchanged),
/// slots a table read only earlier (`VICTORY_IDLE_EVICT_WIDE` off), slots no
/// table read or dynamic ones, title memory changed since the upload, cannot tell (no
/// staging buffer, memory neither watched nor guarded), came back lately; and the pass's
/// microseconds.
pub(crate) static CENSUS: [AtomicU64; 8] = [const { AtomicU64::new(0) }; 8];
pub(crate) const BY_LIST: usize = 0;
pub(crate) const LATELY: usize = 1;
pub(crate) const EARLIER: usize = 2;
pub(crate) const NEVER: usize = 3;
pub(crate) const CHANGED: usize = 4;
pub(crate) const UNKNOWN: usize = 5;
pub(crate) const PROTECTED: usize = 6;
pub(crate) const PASS_US: usize = 7;

/// At the last commit line: let go so far, made again so far, process private, free commit.
static LAST: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

/// A pass ran.
pub(crate) fn passed() {
    ACTED[6].fetch_add(1, Relaxed);
}

/// Whether `resource` came back within [`PROTECT_MS`] (not picked again yet).
pub(crate) fn protected(resource: usize) -> bool {
    if !ANY.load(Relaxed) {
        return false;
    }
    let back = GONE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&resource)
        .map_or(0, |&(_, back)| back);
    back != 0 && !crate::host::long_ago(back, PROTECT_MS, 0)
}

/// `resource`'s host copy of `bytes` went with `slots` slots put back to waiting.
pub(crate) fn evicted(resource: usize, bytes: u64, slots: u64) {
    let now = crate::host::moment().max(1);
    let mut gone = GONE.lock().unwrap_or_else(|e| e.into_inner());
    let back = gone.get(&resource).map_or(0, |&(_, back)| back);
    gone.insert(resource, (now, back));
    ANY.store(true, Relaxed);
    ACTED[0].fetch_add(1, Relaxed);
    ACTED[1].fetch_add(bytes, Relaxed);
    ACTED[2].fetch_add(slots, Relaxed);
}

/// `resource` was given a host copy (`crate::resource` `hosted`, under its lock).
pub(crate) fn made_again(resource: usize) {
    if !ANY.load(Relaxed) {
        return;
    }
    let mut gone = GONE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(entry) = gone.get_mut(&resource)
        && entry.0 != 0
    {
        let now = crate::host::moment().max(1);
        let fast = !crate::host::long_ago(entry.0, FAST_BACK_MS, 0);
        *entry = (0, now);
        ACTED[3].fetch_add(1, Relaxed);
        ACTED[4].fetch_add(u64::from(fast), Relaxed);
    }
}

/// Whether a table that would leave `resource`'s slot null makes and uploads it now instead:
/// its host copy went here once (until the title releases it).
pub(crate) fn reads_now(resource: usize) -> bool {
    if !ANY.load(Relaxed) {
        return false;
    }
    let read = GONE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains_key(&resource);
    if read {
        ACTED[5].fetch_add(1, Relaxed);
    }
    read
}

/// The title released `resource`.
pub(crate) fn forget(resource: usize) {
    if !ANY.load(Relaxed) {
        return;
    }
    GONE.lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&resource);
}

/// At every commit line, given the process's private bytes and the system's free commit: a
/// line if anything went or came back since the last.
pub(crate) fn tick(private: u64, free: u64) {
    let now = [
        ACTED[0].load(Relaxed),
        ACTED[3].load(Relaxed),
        private,
        free,
    ];
    let last = LAST.each_ref().map(|a| a.load(Relaxed));
    for (a, v) in LAST.iter().zip(now) {
        a.store(v, Relaxed);
    }
    let went = now[0].saturating_sub(last[0]);
    let back = now[1].saturating_sub(last[1]);
    if went == 0 && back == 0 {
        return;
    }
    let mb = |v: u64| v >> 20;
    let c = CENSUS.each_ref().map(|a| a.load(Relaxed));
    tracing::info!(
        "idle evict: since the last line let go {went} host copies of textures no table or list read for {} s and {} frames, title memory unchanged since their upload, with their slots; made again {back}; process private {} -> {} MB, free commit {} -> {} MB (last line -> now); last pass ({} us): idle copies held by a list {} MB, that may go (only slots a table makes again, title memory unchanged) {} MB, held by slots a table read only earlier {} MB (VICTORY_IDLE_EVICT_WIDE {}), by slots no table read or dynamic ones {} MB, title memory changed {} MB, cannot tell {} MB, came back lately {} MB; {}",
        idle_s().unwrap_or(0),
        frames(idle_s().unwrap_or(0)),
        mb(last[2]),
        mb(private),
        mb(last[3]),
        mb(free),
        c[PASS_US],
        mb(c[BY_LIST]),
        mb(c[LATELY]),
        mb(c[EARLIER]),
        u8::from(WIDE.get_bool()),
        mb(c[NEVER]),
        mb(c[CHANGED]),
        mb(c[UNKNOWN]),
        mb(c[PROTECTED]),
        summary(),
    );
}

/// The commit line's part.
pub(crate) fn summary() -> String {
    format!(
        "VICTORY_IDLE_EVICT_S {} under {} MB free commit (let go {} ({} MB, {} slots), made again {} ({} within 2 s), table reads made at once {}, passes {})",
        idle_s().unwrap_or(0),
        FLOOR.get() >> 20,
        ACTED[0].load(Relaxed),
        ACTED[1].load(Relaxed) >> 20,
        ACTED[2].load(Relaxed),
        ACTED[3].load(Relaxed),
        ACTED[4].load(Relaxed),
        ACTED[5].load(Relaxed),
        ACTED[6].load(Relaxed),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Live and off by default; acts only under the free commit set (any with 0), at most a
    /// step a pass, never fewer than 60 frames.
    #[test]
    fn it_acts_only_when_set_and_under_the_floor() {
        victory_core::settings::check_live("VICTORY_IDLE_EVICT_S", "5", idle_s, None, Some(5));
        const MB: u64 = 1 << 20;
        assert_eq!(plan_for(None, 6144 * MB, 0, false), None);
        assert_eq!(plan_for(Some(5), 6144 * MB, 6144 * MB, false), None);
        assert_eq!(plan_for(Some(5), 6144 * MB, 7000 * MB, false), None);
        let p = plan_for(Some(5), 6144 * MB, 6000 * MB, true).unwrap();
        assert_eq!(
            p,
            Plan {
                idle_ms: 5000,
                frames: 100,
                need: 144 * MB,
                wide: true
            }
        );
        assert_eq!(plan_for(Some(5), 6144 * MB, 0, false).unwrap().need, STEP);
        assert_eq!(plan_for(Some(5), 0, 50_000 * MB, false).unwrap().need, STEP);
        assert_eq!(frames(1), 60);
        assert_eq!(floor_bits(None), DEFAULT_FLOOR_MB * MB);
        assert_eq!(floor_bits(Some("0")), 0);
    }

    /// A texture let go here is made at once by the next table read until the title releases
    /// it, made again or not; one never let go is left to the table as before. One that came
    /// back is not picked again for a while.
    #[test]
    fn a_table_never_leaves_a_texture_let_go_here_null() {
        const T: usize = 0x1d1e_0000;
        assert!(!reads_now(T));
        evicted(T, 1 << 20, 2);
        assert!(reads_now(T));
        assert!(!protected(T));
        made_again(T);
        assert!(reads_now(T), "still made at once after it came back");
        assert!(protected(T));
        forget(T);
        assert!(!reads_now(T));
        assert!(!protected(T));
    }
}
