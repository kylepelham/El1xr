//! `VICTORY_BAKE_REPLAY=1|2` (#35, after Vaximous/Victory-Blueyes `48f4385a70`): a decoration
//! bake that `VICTORY_BAKE_FRESH` found `incomplete` (it read an input whose show was held back,
//! still queued or unmapped) is drawn again into the same target once those inputs have arrived.
//!
//! Vaximous keeps every bake draw of a frame whose pass ran out of time and draws them all again
//! after 90 frames at the latest, whether or not their textures came. Here only the bakes named
//! `incomplete` are kept, with the bake draws of the same frame that share a target with them
//! (a bake draws its layers onto what the ones before drew), and they are drawn again only when:
//! - every input slot of every kept draw is current now (`crate::bake_fresh::classify`: `ok`);
//! - the parameter constants, the whole run of records and the title's views in the input slots
//!   hash as they did when the draw was recorded (the title may reuse that memory: then the
//!   draw is dropped, never drawn again with other inputs);
//! - no later bake draw or whole clear wrote one of its targets (the title baked again);
//! - with `1`, the title cleared every target whole in that frame, before the first draw (it is
//!   cleared again first, so blended layers are not added twice); `2` draws again without.
//!
//! Inputs that have not arrived after [`MAX_WAIT`] frames: dropped. One host list a frame is
//! sent and waited for with every group that is ready. Needs `VICTORY_BAKE_INPUTS=1` and
//! `VICTORY_BAKE_FRESH=1` (which names the incomplete bakes).

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// Live: read at every bake draw, every whole clear and the first draw of a frame.
pub(crate) static MODE: victory_core::settings::Live =
    victory_core::settings::Live::new("VICTORY_BAKE_REPLAY", parse);

fn parse(v: Option<&str>) -> u64 {
    match v.map(str::trim) {
        Some("1") => 1,
        Some("2") => 2,
        _ => 0,
    }
}

/// 0 off, 1 only targets the title cleared whole in that frame, 2 also without a clear.
pub(crate) fn mode() -> u64 {
    MODE.get()
}

/// Frames a kept bake waits for its inputs at most.
pub(crate) const MAX_WAIT: u64 = 60;
/// Frames after a bake before it may be drawn again (held shows wait for their list to run).
const MIN_AGE: u64 = 2;
/// Bake draws kept per frame, and waiting, at most.
pub(crate) const MAX_FRAME: usize = 256;
pub(crate) const MAX_WAITING: usize = 1024;

/// A bake draw as kept: its targets, the hash of what it read (None: not read), whether
/// `VICTORY_BAKE_FRESH` named it incomplete, its input slots, and what is needed to draw it again.
pub(crate) struct Shot<T> {
    pub(crate) targets: Vec<usize>,
    pub(crate) hash: Option<u64>,
    pub(crate) incomplete: bool,
    pub(crate) slots: Vec<u32>,
    pub(crate) payload: T,
}

/// The draws of a frame's bakes worth drawing again: the incomplete ones and every one sharing a
/// target with a kept one (to a fixed point), in their order. Empty if none was incomplete.
pub(crate) fn group<T>(draws: Vec<Shot<T>>) -> Vec<Shot<T>> {
    let mut keep: Vec<bool> = draws.iter().map(|d| d.incomplete).collect();
    if !keep.contains(&true) {
        return Vec::new();
    }
    loop {
        let targets: Vec<usize> = draws
            .iter()
            .zip(&keep)
            .filter(|&(_, &k)| k)
            .flat_map(|(d, _)| d.targets.iter().copied())
            .collect();
        let mut grew = false;
        for (d, k) in draws.iter().zip(keep.iter_mut()) {
            if !*k && d.targets.iter().any(|t| targets.contains(t)) {
                *k = true;
                grew = true;
            }
        }
        if !grew {
            break;
        }
    }
    draws
        .into_iter()
        .zip(keep)
        .filter_map(|(d, k)| k.then_some(d))
        .collect()
}

/// What becomes of a kept group at frame `now`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    Wait,
    Replay,
    /// What it read is not what it reads now.
    Changed,
    /// The title wrote one of its targets since.
    Superseded,
    /// Its inputs did not arrive in time.
    Expired,
    /// A target was not cleared whole in its frame (mode 1).
    NoClear,
}

/// What is known of a kept group (made at `frame`) at frame `now`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Seen {
    pub(crate) now: u64,
    pub(crate) frame: u64,
    /// Every input slot of every draw is current.
    pub(crate) arrived: bool,
    /// Every draw's hash is the one it had (a draw not read counts as changed).
    pub(crate) unchanged: bool,
    pub(crate) superseded: bool,
    /// Every target was cleared whole in its frame.
    pub(crate) cleared: bool,
    pub(crate) mode: u64,
}

pub(crate) fn decide(s: &Seen) -> Verdict {
    if s.superseded {
        Verdict::Superseded
    } else if !s.unchanged {
        Verdict::Changed
    } else if s.mode < 2 && !s.cleared {
        Verdict::NoClear
    } else if s.now < s.frame + MIN_AGE {
        Verdict::Wait
    } else if !s.arrived {
        if s.now >= s.frame + MAX_WAIT {
            Verdict::Expired
        } else {
            Verdict::Wait
        }
    } else {
        Verdict::Replay
    }
}

/// Whether every slot read is current (`ok`, or not a texture).
pub(crate) fn arrived(slots: &[crate::bake_fresh::Fresh]) -> bool {
    use crate::bake_fresh::Fresh;
    slots
        .iter()
        .all(|f| matches!(f, Fresh::Ok | Fresh::NotTexture))
}

/// FNV-1a over dwords.
#[derive(Clone, Copy)]
pub(crate) struct Hash(u64);

impl Default for Hash {
    fn default() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }
}

impl Hash {
    pub(crate) fn add(&mut self, w: u32) {
        for b in w.to_le_bytes() {
            self.0 = (self.0 ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
        }
    }

    pub(crate) fn add_all(&mut self, words: &[u32]) {
        self.add(words.len() as u32);
        words.iter().for_each(|&w| self.add(w));
    }

    pub(crate) fn add64(&mut self, w: u64) {
        self.add(w as u32);
        self.add((w >> 32) as u32);
    }

    pub(crate) fn get(self) -> u64 {
        self.0
    }
}

/// What the title last did to a render target: the frame of its last write by a bake draw or a
/// whole clear (one frame later for a clear after a bake draw in the same frame: it wrote over
/// that bake), the frame of its last bake draw, and the view and colour of its last whole clear
/// with that clear's frame.
#[derive(Clone, Copy, Default)]
struct Target {
    written: u64,
    baked: u64,
    clear: Option<(u64, Clear)>,
}

/// A whole clear: the view's values and the colour.
#[derive(Clone, Copy)]
pub(crate) struct Clear {
    /// (None only in the tests.)
    #[cfg(windows)]
    pub(crate) view: Option<crate::descriptor::RtvSnapshot>,
    pub(crate) color: [f32; 4],
}

static TARGETS: Mutex<Option<HashMap<usize, Target>>> = Mutex::new(None);

fn with_targets<R>(f: impl FnOnce(&mut HashMap<usize, Target>) -> R) -> R {
    let mut t = TARGETS.lock().unwrap_or_else(|e| e.into_inner());
    let t = t.get_or_insert_with(HashMap::new);
    if t.len() > 4096 {
        let frame = crate::frame::current();
        t.retain(|_, v| v.written + 2 * MAX_WAIT > frame);
    }
    f(t)
}

/// The title cleared render target `resource` whole (with the switch).
pub(crate) fn cleared(resource: usize, frame: u64, clear: Clear) {
    with_targets(|t| {
        let e = t.entry(resource).or_default();
        let after_bake = e.baked == frame && frame > 0;
        e.written = e.written.max(frame + u64::from(after_bake));
        e.clear = Some((frame, clear));
    });
}

/// A bake draw of the title wrote `targets` in `frame`.
pub(crate) fn written(targets: &[usize], frame: u64) {
    with_targets(|t| {
        for &r in targets {
            let e = t.entry(r).or_default();
            e.written = e.written.max(frame);
            e.baked = frame;
        }
    });
}

/// Whether one of `targets` was written after `frame`.
pub(crate) fn superseded(targets: &[usize], frame: u64) -> bool {
    with_targets(|t| {
        targets
            .iter()
            .any(|r| t.get(r).is_some_and(|e| e.written > frame))
    })
}

/// The whole clear of `target` in `frame`, if there was one.
pub(crate) fn clear_of(target: usize, frame: u64) -> Option<Clear> {
    with_targets(|t| {
        t.get(&target)
            .and_then(|e| e.clear)
            .and_then(|(at, c)| (at == frame).then_some(c))
    })
}

#[derive(Clone, Copy)]
#[repr(usize)]
pub(crate) enum C {
    Kept,
    Groups,
    Waiting,
    Drawn,
    Changed,
    Superseded,
    Expired,
    NoClear,
    Stale,
    Unread,
    Full,
    ReplayIncomplete,
    Us,
}
const N: usize = 13;
const NAMES: [&str; N] = [
    "kept",
    "groups",
    "waiting",
    "drawn",
    "changed",
    "superseded",
    "expired",
    "no_clear",
    "stale",
    "unread",
    "full",
    "replay_incomplete",
    "us",
];
static COUNTS: [AtomicU64; N] = [const { AtomicU64::new(0) }; N];
static FRAME: AtomicU64 = AtomicU64::new(0);
static LINES: AtomicU64 = AtomicU64::new(0);
const MAX_LINES: u64 = 20_000;

pub(crate) fn add(c: C, n: u64) {
    roll();
    COUNTS[c as usize].fetch_add(n, Relaxed);
}

/// The number of groups waiting, as of now (not summed).
pub(crate) fn waiting(n: u64) {
    roll();
    COUNTS[C::Waiting as usize].store(n, Relaxed);
}

/// The line for a frame where something was kept, drawn or dropped, at the first count of the
/// next (a frame with only groups waiting gives none).
fn roll() {
    let now = crate::frame::current();
    let prev = FRAME.load(Relaxed);
    if prev == now || FRAME.compare_exchange(prev, now, Relaxed, Relaxed).is_err() {
        return;
    }
    let c: [u64; N] = std::array::from_fn(|i| {
        if i == C::Waiting as usize {
            COUNTS[i].load(Relaxed)
        } else {
            COUNTS[i].swap(0, Relaxed)
        }
    });
    let quiet = c
        .iter()
        .enumerate()
        .all(|(i, &n)| i == C::Waiting as usize || n == 0);
    if quiet || LINES.fetch_add(1, Relaxed) >= MAX_LINES {
        return;
    }
    tracing::info!(frame = prev, counts = %line(&c), "bake replay frame");
}

fn line(c: &[u64; N]) -> String {
    NAMES
        .iter()
        .zip(c)
        .map(|(name, n)| format!("{name}={n}"))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bake_fresh::Fresh;

    fn shot(targets: &[usize], incomplete: bool, id: u32) -> Shot<u32> {
        Shot {
            targets: targets.to_vec(),
            hash: Some(1),
            incomplete,
            slots: Vec::new(),
            payload: id,
        }
    }

    fn seen() -> Seen {
        Seen {
            now: 14,
            frame: 10,
            arrived: true,
            unchanged: true,
            superseded: false,
            cleared: true,
            mode: 1,
        }
    }

    /// The bake is drawn again only once its inputs arrived and what it read is unchanged.
    #[test]
    fn replayed_only_when_inputs_arrived_and_params_unchanged() {
        assert_eq!(decide(&seen()), Verdict::Replay);
        let waiting = Seen {
            arrived: false,
            ..seen()
        };
        assert_eq!(decide(&waiting), Verdict::Wait);
        assert_eq!(
            decide(&Seen {
                now: 10 + MAX_WAIT,
                ..waiting
            }),
            Verdict::Expired
        );
        assert_eq!(
            decide(&Seen { now: 11, ..seen() }),
            Verdict::Wait,
            "its list has not run"
        );
    }

    /// Params, records or the title's views changed: never drawn again, arrived or not.
    #[test]
    fn never_replayed_when_what_it_read_changed() {
        for arrived in [false, true] {
            for now in [11, 14, 10 + MAX_WAIT] {
                let s = Seen {
                    now,
                    arrived,
                    unchanged: false,
                    ..seen()
                };
                assert_eq!(decide(&s), Verdict::Changed, "arrived={arrived} now={now}");
            }
        }
        let rebaked = Seen {
            superseded: true,
            ..seen()
        };
        assert_eq!(decide(&rebaked), Verdict::Superseded);
    }

    /// Mode 1 needs the title's whole clear of every target in that frame; mode 2 does not.
    #[test]
    fn without_a_clear_only_in_mode_2() {
        let s = Seen {
            cleared: false,
            ..seen()
        };
        assert_eq!(decide(&s), Verdict::NoClear);
        assert_eq!(decide(&Seen { mode: 2, ..s }), Verdict::Replay);
    }

    #[test]
    fn inputs_arrive_when_every_slot_is_current() {
        assert!(arrived(&[Fresh::Ok, Fresh::NotTexture]));
        for f in [
            Fresh::Queued,
            Fresh::Unmapped,
            Fresh::StaleNull,
            Fresh::StaleOther,
            Fresh::OtherBytes,
        ] {
            assert!(!arrived(&[Fresh::Ok, f]), "{f:?}");
        }
    }

    /// The incomplete draw comes with the frame's draws into its targets, in order, and draws
    /// into other targets are left out.
    #[test]
    fn a_group_is_the_incomplete_bake_and_its_targets_layers() {
        let g = group(vec![
            shot(&[1], false, 0),
            shot(&[2], false, 1),
            shot(&[1, 3], true, 2),
            shot(&[3], false, 3),
            shot(&[4], false, 4),
        ]);
        assert_eq!(g.iter().map(|s| s.payload).collect::<Vec<_>>(), [0, 2, 3]);
        assert!(group(vec![shot(&[1], false, 0), shot(&[1], false, 1)]).is_empty());
    }

    #[test]
    fn a_later_write_supersedes_and_clears_are_kept_per_frame() {
        written(&[9001], 20);
        assert!(!superseded(&[9001], 20));
        assert!(superseded(&[9001], 19));
        cleared(
            9002,
            30,
            Clear {
                #[cfg(windows)]
                view: None,
                color: [0.0; 4],
            },
        );
        assert!(clear_of(9002, 30).is_some());
        assert!(clear_of(9002, 29).is_none());
        assert!(superseded(&[9002], 29));
        assert!(!superseded(&[9002], 30), "cleared before its bake");
        written(&[9003], 40);
        cleared(9003, 40, Clear { #[cfg(windows)] view: None, color: [0.0; 4] });
        assert!(superseded(&[9003], 40), "cleared after its bake in the same frame");
    }

    #[test]
    fn the_hash_sees_every_word_and_the_length() {
        let h = |w: &[u32]| {
            let mut h = Hash::default();
            h.add_all(w);
            h.get()
        };
        assert_eq!(h(&[1, 2, 3]), h(&[1, 2, 3]));
        assert_ne!(h(&[1, 2, 3]), h(&[1, 2, 4]));
        assert_ne!(h(&[1, 2]), h(&[1, 2, 0]));
    }

    #[test]
    fn the_line_names_every_count() {
        let mut c = [0u64; N];
        c[C::Drawn as usize] = 3;
        assert_eq!(
            line(&c),
            "kept=0 groups=0 waiting=0 drawn=3 changed=0 superseded=0 expired=0 no_clear=0 stale=0 unread=0 full=0 replay_incomplete=0 us=0"
        );
    }
}
