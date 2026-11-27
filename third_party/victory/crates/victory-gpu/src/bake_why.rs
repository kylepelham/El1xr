//! Why a bake draw's own textures were still not uploaded when it was recorded
//! (`VICTORY_BAKE_WHY=1`), and a wait for the ones whose memory the title had not mapped yet
//! (`VICTORY_BAKE_WAIT_MS=N`). Both act only with `VICTORY_BAKE_INPUTS=1`
//! ([`crate::bake_inputs`]), which finds those textures.
//!
//! Batch 4 (#36): with `VICTORY_BAKE_INPUTS=1` the waitress's shirt came out pink in both runs
//! and dark in all three without (the bake read its inputs as zeros), but Lucia's prison top was
//! still red instead of orange at its first shot in one of two runs, right after a bake draw
//! recorded with 7 of its textures still pending (`bake inputs frame` frame 430: `pending=7`).
//! A texture the title has not mapped when we record the draw cannot be uploaded; the console's
//! card reads it only when it runs the draw, later. The title's streaming threads may map it in
//! between: the wait gives them that time, the diagnostic tells whether and when they do.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::Instant;

/// `VICTORY_BAKE_WHY=1`: one `bake why: pending` line per texture a bake draw still lacks, and a
/// `bake why: landed` line when it is uploaded later.
static WHY: victory_core::settings::Live =
    victory_core::settings::Live::new("VICTORY_BAKE_WHY", |v| u64::from(v == Some("1")));

/// `VICTORY_BAKE_WAIT_MS=N`: a bake draw whose own textures are not (all) mapped waits up to N
/// ms for the title to map them, then uploads what came. 0 or unset: no wait.
static WAIT: victory_core::settings::Live =
    victory_core::settings::Live::new("VICTORY_BAKE_WAIT_MS", |v| {
        v.and_then(|v| v.trim().parse().ok()).unwrap_or(0)
    });

/// The longest a frame's bake draws wait in all: a frame has dozens of them.
const FRAME_WAIT_TIMES: u64 = 4;
/// The step between looks at the memory while waiting.
const STEP: std::time::Duration = std::time::Duration::from_millis(1);
/// Lines of each kind at most.
const MAX_LINES: u64 = 4_000;
/// Pending textures remembered for their `landed` line at most.
const MAX_TRACKED: usize = 4_096;

pub(crate) fn why() -> bool {
    WHY.get_bool()
}

/// The wait for one bake draw, in ms (0: none).
pub(crate) fn wait_ms() -> u64 {
    WAIT.get().min(1_000)
}

/// What is missing of a texture a bake draw reads.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Lack {
    /// Placed texture with none of its memory mapped.
    Unmapped,
    /// Placed texture with part of its memory mapped (`mapped` of `len` bytes).
    Partly,
    /// Its memory is mapped but the upload did not happen (no room on the card, or the queue
    /// looks at it again later).
    Other,
}

impl Lack {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Lack::Unmapped => "unmapped",
            Lack::Partly => "partly_mapped",
            Lack::Other => "mapped_not_uploaded",
        }
    }

    /// Whether the title mapping more of it could let it be uploaded.
    pub(crate) fn waits_for_title(self) -> bool {
        matches!(self, Lack::Unmapped | Lack::Partly)
    }
}

/// From the bytes of a placed texture's memory mapped and its length.
pub(crate) fn lack(mapped: u64, len: u64) -> Lack {
    if mapped == 0 {
        Lack::Unmapped
    } else if mapped < len {
        Lack::Partly
    } else {
        Lack::Other
    }
}

/// A texture a bake draw read before it was uploaded: its slot, and the frame and time of the
/// first bake draw that found it pending.
struct Pending {
    frame: u64,
    at: Instant,
    shader: &'static str,
}

fn tracked() -> &'static Mutex<HashMap<u32, Pending>> {
    static T: std::sync::OnceLock<Mutex<HashMap<u32, Pending>>> = std::sync::OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

static PENDING_LINES: AtomicU64 = AtomicU64::new(0);
static LANDED_LINES: AtomicU64 = AtomicU64::new(0);

fn say(lines: &AtomicU64) -> bool {
    lines.fetch_add(1, Relaxed) < MAX_LINES
}

/// What a pending texture is, for its line.
pub(crate) struct Seen {
    pub(crate) slot: u32,
    pub(crate) lack: Lack,
    pub(crate) mapped: u64,
    pub(crate) len: u64,
    pub(crate) address: u64,
    /// Width, height, mips and format of a placed texture (zeros for another view).
    pub(crate) shape: [u32; 4],
    pub(crate) state: String,
}

/// The width, height, mip count and format of a placed texture's description.
pub(crate) fn shape(desc: &[u32; 14]) -> [u32; 4] {
    [desc[4], desc[6], desc[7] >> 16, desc[8]]
}

/// A bake draw (pixel shader `shader`, target `target`) was recorded at `frame` with `seen`
/// still pending, after `waited_ms` of waiting.
pub(crate) fn pending(
    frame: u64,
    shader: &'static str,
    target: usize,
    waited_ms: u64,
    seen: &[Seen],
) {
    let mut t = tracked().lock().unwrap_or_else(|e| e.into_inner());
    for s in seen {
        if t.len() < MAX_TRACKED {
            t.entry(s.slot).or_insert(Pending {
                frame,
                at: Instant::now(),
                shader,
            });
        }
        if say(&PENDING_LINES) {
            tracing::warn!(
                frame,
                shader,
                target = format_args!("{target:#x}"),
                slot = s.slot,
                lack = s.lack.name(),
                mapped = s.mapped,
                len = s.len,
                address = format_args!("{:#x}", s.address),
                width = s.shape[0],
                height = s.shape[1],
                mips = s.shape[2],
                format = s.shape[3],
                waited_ms,
                state = %s.state,
                "bake why: pending"
            );
        }
    }
}

/// Of the textures found pending before, those `queued` no longer names were uploaded since:
/// a `landed` line each, with the frames and time since the bake draw that lacked them.
pub(crate) fn landed(frame: u64, queued: impl Fn(&[u32]) -> Vec<u32>) {
    let mut t = tracked().lock().unwrap_or_else(|e| e.into_inner());
    if t.is_empty() {
        return;
    }
    let slots: Vec<u32> = t.keys().copied().collect();
    let still = queued(&slots);
    for slot in slots.into_iter().filter(|s| !still.contains(s)) {
        let Some(p) = t.remove(&slot) else { continue };
        if say(&LANDED_LINES) {
            tracing::info!(
                frame,
                slot,
                baked_at = p.frame,
                frames_after = frame.saturating_sub(p.frame),
                ms_after = p.at.elapsed().as_millis() as u64,
                shader = p.shader,
                "bake why: landed"
            );
        }
    }
}

static WAIT_FRAME: AtomicU64 = AtomicU64::new(u64::MAX);
static WAIT_SPENT: AtomicU64 = AtomicU64::new(0);

/// How long this bake draw may still wait in `frame`, given `ms` per draw: at most
/// [`FRAME_WAIT_TIMES`] times that per frame.
pub(crate) fn wait_left(frame: u64, ms: u64) -> u64 {
    if WAIT_FRAME.swap(frame, Relaxed) != frame {
        WAIT_SPENT.store(0, Relaxed);
    }
    ms.min((ms * FRAME_WAIT_TIMES).saturating_sub(WAIT_SPENT.load(Relaxed)))
}

/// `ms` of this frame's waiting were spent.
pub(crate) fn waited(ms: u64) {
    WAIT_SPENT.fetch_add(ms, Relaxed);
}

/// Waits until `mapped_all` says every texture's memory is there or `ms` pass; whether it was.
pub(crate) fn wait_for(ms: u64, mut mapped_all: impl FnMut() -> bool) -> (bool, u64) {
    let began = Instant::now();
    let limit = std::time::Duration::from_millis(ms);
    loop {
        if mapped_all() {
            return (true, began.elapsed().as_millis() as u64);
        }
        if began.elapsed() >= limit {
            return (false, began.elapsed().as_millis() as u64);
        }
        std::thread::sleep(STEP);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_lack_follows_the_mapped_bytes() {
        assert_eq!(lack(0, 4096), Lack::Unmapped);
        assert_eq!(lack(1024, 4096), Lack::Partly);
        assert_eq!(lack(4096, 4096), Lack::Other);
        assert!(Lack::Unmapped.waits_for_title() && Lack::Partly.waits_for_title());
        assert!(!Lack::Other.waits_for_title());
    }

    #[test]
    fn a_frame_waits_at_most_four_draws_worth() {
        let frame = u64::MAX - 7;
        assert_eq!(wait_left(frame, 10), 10);
        waited(35);
        assert_eq!(wait_left(frame, 10), 5);
        waited(5);
        assert_eq!(wait_left(frame, 10), 0);
        assert_eq!(wait_left(frame + 1, 10), 10);
    }

    #[test]
    fn the_wait_ends_when_the_memory_is_there_or_at_its_limit() {
        let mut n = 0;
        let (ok, _) = wait_for(1_000, || {
            n += 1;
            n == 3
        });
        assert!(ok);
        assert_eq!(n, 3);
        let (ok, ms) = wait_for(5, || false);
        assert!(!ok && ms >= 5);
    }

    #[test]
    fn a_pending_texture_lands_once() {
        pending(
            10,
            "c0a7fe77b1862e10",
            0,
            0,
            &[Seen {
                slot: 0x7ff_fff0,
                lack: Lack::Unmapped,
                mapped: 0,
                len: 4096,
                address: 0,
                shape: [256, 256, 9, 71],
                state: String::new(),
            }],
        );
        assert!(tracked().lock().unwrap().contains_key(&0x7ff_fff0));
        landed(12, |s| {
            s.iter().copied().filter(|&s| s != 0x7ff_fff0).collect()
        });
        assert!(!tracked().lock().unwrap().contains_key(&0x7ff_fff0));
    }

    #[test]
    fn the_shape_is_read_from_the_description() {
        let mut d = [0u32; 14];
        d[4] = 2048;
        d[6] = 1024;
        d[7] = (12 << 16) | 1;
        d[8] = 78;
        assert_eq!(shape(&d), [2048, 1024, 12, 78]);
    }
}
