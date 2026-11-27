//! `VICTORY_PIPELINE_COMPUTE_WAIT=<ms>[,<frame ms>]` (off by default): a
//! dispatch whose compute pipeline is still not made after the frame's
//! usual wait ([`super::wait`]) waits a little longer for it, at most
//! `<ms>` (50 with `=1`, no more than [`EACH_MOST`]) per dispatch and
//! `<frame ms>` (200, no more than [`FRAME_MOST`]) for all dispatches of
//! one frame together. The pipeline goes to the front of the workers'
//! queue first.
//!
//! A dropped draw is drawn again in the next frame; a dropped dispatch may
//! not be: the title runs some compute passes once, when a scene changes,
//! and what they leave is read for as long as the scene lasts (in fxceless'
//! run 195 a first-time compute program took 4.4 s, its one dispatch was
//! dropped, and the room before stayed blended over the picture for
//! minutes). Upstream waits for the pipeline without a bound; this tree
//! knows a compute program that takes 85 s to make, so the wait is bounded
//! and a pipeline the driver has held for long ([`Pending::slow`]) is not
//! waited for. Every dispatch still dropped is logged (once per pipeline
//! and reason).
//!
//! Source: fxceless/victory-fork a42b627e78 (#1312), by Exopus.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::{Pending, QUEUE, REQUESTED};

/// The longest one dispatch may wait, whatever is asked.
const EACH_MOST: u64 = 1_000;
/// The longest all dispatches of one frame may wait together.
const FRAME_MOST: u64 = 2_000;
/// `=1`: the defaults.
const EACH_DEFAULT: u64 = 50;
const FRAME_DEFAULT: u64 = 200;
/// Lines logged at most, made and dropped together.
const LINES_MOST: u64 = 256;

/// What the switch allows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Limits {
    pub(super) each: Duration,
    pub(super) frame: Duration,
}

/// The limits `VICTORY_PIPELINE_COMPUTE_WAIT` asks for; `None`: off.
pub(super) fn parse(value: Option<&str>) -> Option<Limits> {
    let value = value?.trim();
    let (each, frame) = match value.split_once(',') {
        Some((each, frame)) => (
            each.trim().parse::<u64>().ok()?,
            Some(frame.trim().parse::<u64>().ok()?),
        ),
        None => (value.parse::<u64>().ok()?, None),
    };
    if each == 0 {
        return None;
    }
    let each = if each == 1 && frame.is_none() {
        EACH_DEFAULT
    } else {
        each.min(EACH_MOST)
    };
    let frame = frame.unwrap_or(FRAME_DEFAULT.max(each)).min(FRAME_MOST);
    Some(Limits {
        each: Duration::from_millis(each),
        frame: Duration::from_millis(frame),
    })
}

/// Live: each wait takes the limits as they are when it begins; the line is
/// logged whenever they change to new ones.
fn limits() -> Option<Limits> {
    static ON: victory_core::settings::Live = victory_core::settings::Live::new("VICTORY_PIPELINE_COMPUTE_WAIT", |v| {
        // Both at most 2,000 ms: 32 bits each; 0 off (`each` is never 0).
        parse(v).map_or(0, |l| (l.each.as_millis() as u64) << 32 | l.frame.as_millis() as u64)
    });
    static LOGGED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let bits = ON.get();
    if bits == 0 {
        return None;
    }
    let limits = Limits {
        each: Duration::from_millis(bits >> 32),
        frame: Duration::from_millis(bits & u64::from(u32::MAX)),
    };
    if LOGGED.swap(bits, std::sync::atomic::Ordering::Relaxed) != bits {
        tracing::info!(
            each_ms = limits.each.as_millis() as u64,
            frame_ms = limits.frame.as_millis() as u64,
            "VICTORY_PIPELINE_COMPUTE_WAIT: a dispatch waits longer for its compute pipeline, bounded per dispatch and per frame"
        );
    }
    Some(limits)
}

/// One frame's extra waiting: each wait takes its share before it begins
/// and gives back what it did not use, so the threads' waits together stay
/// within the frame's limit.
#[derive(Debug)]
pub(super) struct FrameTake {
    frame: u64,
    spent: Duration,
}

impl FrameTake {
    pub(super) const fn new() -> FrameTake {
        FrameTake {
            frame: u64::MAX,
            spent: Duration::ZERO,
        }
    }

    pub(super) fn take(&mut self, frame: u64, l: Limits) -> Duration {
        if self.frame != frame {
            *self = FrameTake {
                frame,
                spent: Duration::ZERO,
            };
        }
        let got = l.each.min(l.frame.saturating_sub(self.spent));
        self.spent += got;
        got
    }

    pub(super) fn give_back(&mut self, frame: u64, unused: Duration) {
        if self.frame == frame {
            self.spent = self.spent.saturating_sub(unused);
        }
    }
}

/// Why a dispatch was dropped after all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum Dropped {
    /// Not made within the dispatch's limit.
    Each,
    /// The frame's limit was spent before it could wait.
    Frame,
    /// The driver has held it for long ([`Pending::slow`]).
    HeldTooLong,
}

impl Dropped {
    fn what(self) -> &'static str {
        match self {
            Dropped::Each => "not made within the dispatch's limit",
            Dropped::Frame => "the frame's limit was spent",
            Dropped::HeldTooLong => "held by the driver for long, not waited for",
        }
    }
}

/// Dispatches that got their pipeline by waiting longer, and dispatches
/// dropped after all, by reason.
static MADE: AtomicU64 = AtomicU64::new(0);
static DROPPED: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];
static LINES: AtomicU64 = AtomicU64::new(0);

/// After the usual wait of a dispatch for `p` in `frame`: waits longer if
/// the switch is on and `p` is a compute pipeline not made yet.
pub(super) fn after(p: &Arc<Pending>, frame: u64) {
    let Some(l) = limits() else { return };
    if p.made.get().is_some() || !p.computes() {
        return;
    }
    static FRAMES: Mutex<FrameTake> = Mutex::new(FrameTake::new());
    match wait_more(p, frame, l, &FRAMES) {
        Ok(waited) => made(p, frame, waited),
        Err((why, waited)) => dropped(p, frame, why, waited),
    }
}

/// Waits for `p` within `l`, taken from `frames`: how long, or why it is
/// still not made.
pub(super) fn wait_more(
    p: &Arc<Pending>,
    frame: u64,
    l: Limits,
    frames: &Mutex<FrameTake>,
) -> Result<Duration, (Dropped, Duration)> {
    if p.slow() {
        return Err((Dropped::HeldTooLong, Duration::ZERO));
    }
    to_front(p);
    let allowed = frames
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take(frame, l);
    if allowed.is_zero() {
        return Err((Dropped::Frame, Duration::ZERO));
    }
    let _hitch = crate::hitch::span(crate::hitch::Phase::PipelineWait);
    let started = Instant::now();
    let (lock, done) = &p.completion;
    let mut held = lock.lock().unwrap_or_else(|e| e.into_inner());
    while p.made.get().is_none() {
        let rest = allowed.saturating_sub(started.elapsed());
        if rest.is_zero() {
            break;
        }
        held = done
            .wait_timeout(held, rest)
            .unwrap_or_else(|e| e.into_inner())
            .0;
    }
    drop(held);
    let waited = started.elapsed().min(allowed);
    frames
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .give_back(frame, allowed - waited);
    if p.made.get().is_some() {
        Ok(waited)
    } else {
        Err((Dropped::Each, waited))
    }
}

/// Moves `p` to the front of the workers' queue if it still waits there.
pub(super) fn to_front(p: &Arc<Pending>) {
    if p.queue_state.load(Ordering::Relaxed) != REQUESTED {
        return;
    }
    let mut queue = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
    queue.to_front(p);
}

fn line() -> bool {
    LINES.fetch_add(1, Ordering::Relaxed) < LINES_MOST
}

fn made(p: &Pending, frame: u64, waited: Duration) {
    let made = MADE.fetch_add(1, Ordering::Relaxed) + 1;
    if line() {
        tracing::info!(
            shaders = ?p.shaders,
            frame,
            waited_ms = waited.as_millis() as u64,
            made,
            "VICTORY_PIPELINE_COMPUTE_WAIT: a compute pipeline was made while its dispatch waited longer"
        );
    }
}

fn dropped(p: &Pending, frame: u64, why: Dropped, waited: Duration) {
    DROPPED[why as usize].fetch_add(1, Ordering::Relaxed);
    static SEEN: Mutex<Option<HashSet<(String, Dropped)>>> = Mutex::new(None);
    let first = SEEN
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_or_insert_with(HashSet::new)
        .insert((p.key.clone(), why));
    if first && line() {
        let [each, spent, slow] = DROPPED.each_ref().map(|d| d.load(Ordering::Relaxed));
        tracing::warn!(
            shaders = ?p.shaders,
            frame,
            waited_ms = waited.as_millis() as u64,
            made = MADE.load(Ordering::Relaxed),
            dropped_each = each,
            dropped_frame = spent,
            dropped_held = slow,
            "VICTORY_PIPELINE_COMPUTE_WAIT: a dispatch dropped after all: {}",
            why.what()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::super::{MAKING, Queue};
    use super::*;

    #[test]
    fn the_switch_is_live() {
        let on = Some(Limits { each: ms(EACH_DEFAULT), frame: ms(FRAME_DEFAULT) });
        victory_core::settings::check_live("VICTORY_PIPELINE_COMPUTE_WAIT", "1", limits, None, on);
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn the_switch_is_bounded_per_dispatch_and_per_frame() {
        assert_eq!(parse(None), None);
        assert_eq!(parse(Some("0")), None);
        assert_eq!(parse(Some("off")), None);
        assert_eq!(
            parse(Some("1")),
            Some(Limits {
                each: ms(50),
                frame: ms(200)
            })
        );
        assert_eq!(
            parse(Some("80")),
            Some(Limits {
                each: ms(80),
                frame: ms(200)
            })
        );
        assert_eq!(
            parse(Some("500")),
            Some(Limits {
                each: ms(500),
                frame: ms(500)
            })
        );
        assert_eq!(
            parse(Some("90000")),
            Some(Limits {
                each: ms(1000),
                frame: ms(1000)
            })
        );
        assert_eq!(
            parse(Some("40,100")),
            Some(Limits {
                each: ms(40),
                frame: ms(100)
            })
        );
        assert_eq!(
            parse(Some("1,10")),
            Some(Limits {
                each: ms(1),
                frame: ms(10)
            })
        );
        assert_eq!(
            parse(Some("50,99999")),
            Some(Limits {
                each: ms(50),
                frame: ms(2000)
            })
        );
        assert_eq!(parse(Some("50,x")), None);
    }

    #[test]
    fn waits_of_one_frame_share_its_limit_and_give_back_the_rest() {
        let l = Limits {
            each: ms(50),
            frame: ms(120),
        };
        let mut f = FrameTake::new();
        assert_eq!(f.take(3, l), ms(50));
        assert_eq!(f.take(3, l), ms(50));
        assert_eq!(f.take(3, l), ms(20));
        assert_eq!(f.take(3, l), ms(0));
        f.give_back(3, ms(30));
        assert_eq!(f.take(3, l), ms(30));
        // A late give-back of an earlier frame does not count in this one.
        assert_eq!(f.take(4, l), ms(50));
        f.give_back(3, ms(50));
        assert_eq!(f.take(4, l), ms(50));
        assert_eq!(f.take(4, l), ms(20));
    }

    /// A pipeline nobody makes is waited for no longer than the dispatch's
    /// limit, and once the frame's limit is spent not at all.
    #[test]
    fn a_pipeline_never_made_is_dropped_after_the_limit() {
        let p = Pending::new(Vec::new(), None);
        p.queue_state.store(MAKING, Ordering::Relaxed);
        let p = Arc::new(p);
        let frames = Mutex::new(FrameTake::new());
        let l = Limits {
            each: ms(30),
            frame: ms(50),
        };
        let started = Instant::now();
        let first = wait_more(&p, 9, l, &frames);
        assert!(
            matches!(first, Err((Dropped::Each, w)) if w == ms(30)),
            "{first:?}"
        );
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(matches!(wait_more(&p, 9, l, &frames), Err((Dropped::Each, w)) if w == ms(20)));
        assert!(matches!(wait_more(&p, 9, l, &frames), Err((Dropped::Frame, w)) if w.is_zero()));
        assert!(matches!(
            wait_more(&p, 10, l, &frames),
            Err((Dropped::Each, _))
        ));
    }

    /// A pipeline made while the dispatch waits ends the wait at once, and
    /// what was not waited goes back to the frame.
    #[test]
    fn a_pipeline_made_meanwhile_ends_the_wait() {
        let p = Pending::new(Vec::new(), None);
        p.queue_state.store(MAKING, Ordering::Relaxed);
        let p = Arc::new(p);
        let frames = Mutex::new(FrameTake::new());
        let l = Limits {
            each: ms(1000),
            frame: ms(1000),
        };
        let maker = p.clone();
        let worker = std::thread::spawn(move || {
            std::thread::sleep(ms(20));
            maker.make_with(|| None);
        });
        let got = wait_more(&p, 1, l, &frames);
        worker.join().unwrap();
        let Ok(waited) = got else {
            panic!("not made: {got:?}")
        };
        assert!(waited < ms(1000));
        assert_eq!(
            frames.lock().unwrap().take(1, l),
            ms(1000).min(ms(1000) - waited)
        );
    }

    #[test]
    fn a_waited_for_pipeline_goes_to_the_front_of_the_queue() {
        let first = Arc::new(Pending::new(Vec::new(), None));
        let second = Arc::new(Pending::new(Vec::new(), None));
        let third = Arc::new(Pending::new(Vec::new(), None));
        let mut queue = Queue::new();
        for p in [&first, &second, &third] {
            assert_eq!(queue.push(p, REQUESTED, 0), Some(true));
        }
        queue.to_front(&third);
        queue.to_front(&Arc::new(Pending::new(Vec::new(), None)));
        assert!(Arc::ptr_eq(&queue.pop().unwrap(), &third));
        assert!(Arc::ptr_eq(&queue.pop().unwrap(), &first));
        assert!(Arc::ptr_eq(&queue.pop().unwrap(), &second));
        assert!(queue.pop().is_none());
    }
}
