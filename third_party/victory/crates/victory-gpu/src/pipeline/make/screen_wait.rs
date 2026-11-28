//! (texflicker, issue #96) Passes over the screen: triangles with no vertex
//! input and no depth or stencil test ([`GraphicsState::screen_pass`]), the
//! title's lighting, composition and post passes.
//!
//! - `VICTORY_SCREEN_PIPELINE_WAIT=<ms>[,<frame ms>]` (off by default,
//!   live): a screen pass whose pipeline is not made goes to the front of
//!   the workers' queue, and after the frame's usual wait
//!   ([`super::wait`], 25 ms for graphics) its draw waits on for it, at most
//!   `<ms>` (3000 with `=1`, no more than [`EACH_MOST`]) per draw and
//!   `<frame ms>` (5000, no more than [`FRAME_MOST`]) for all of one
//!   frame's, however long the driver has held it. Without it such a draw
//!   is dropped, and the frame shows without that light: a lit picture that
//!   goes dark for a frame or a few while its pipeline compiles.
//! - Always: screen-pass draws dropped because their pipeline was still
//!   being made (or was refused) are counted per presented frame, and a
//!   frame with any logs one line `screen passes dropped without a
//!   pipeline: frame=N ...` (the frame number of the flicker catcher's
//!   lines), so a flicker can be matched to them. The flicker catcher
//!   counts them as `screen_unmade`.
//!
//! Upstream waits up to 20 s a frame on two workers of their own; this tree
//! has 28 workers, so the pass goes to the front of their queue instead,
//! and the wait is bounded below the title's 30 s watchdog with the
//! loading windows (4 s + 12 s) before it.
//!
//! Source: fxceless/victory-fork #892 = uiliannn/VItoriaBr #25, by
//! sagouindupm (after Belobug 18e57db8); carried by darkworldhm.
//!
//! [`GraphicsState::screen_pass`]: super::GraphicsState::screen_pass

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::Pending;
use super::compute_wait::{FrameTake, Limits, to_front, wait_taken};

/// The longest one draw may wait, whatever is asked.
const EACH_MOST: u64 = 10_000;
/// The longest all draws of one frame may wait together.
const FRAME_MOST: u64 = 10_000;
/// `=1`: the defaults.
const EACH_DEFAULT: u64 = 3_000;
const FRAME_DEFAULT: u64 = 5_000;
/// Lines logged at most: waits (made and still not made) and dropped frames
/// each.
const LINES_MOST: u64 = 4_096;

/// The limits `VICTORY_SCREEN_PIPELINE_WAIT` asks for; `None`: off.
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

#[cfg(test)]
thread_local! {
    /// A test's own limits, on its thread alone.
    static TEST_LIMITS: std::cell::Cell<Option<Option<Limits>>> = const { std::cell::Cell::new(None) };
}

/// Live: each wait takes the limits as they are when it begins; the line is
/// logged whenever they change.
fn limits() -> Option<Limits> {
    #[cfg(test)]
    if let Some(limits) = TEST_LIMITS.get() {
        return limits;
    }
    static ON: victory_core::settings::Live =
        victory_core::settings::Live::new("VICTORY_SCREEN_PIPELINE_WAIT", |v| {
            // Both at most 10,000 ms: 32 bits each; 0 off (`each` is never 0).
            parse(v).map_or(0, |l| {
                (l.each.as_millis() as u64) << 32 | l.frame.as_millis() as u64
            })
        });
    static LOGGED: AtomicU64 = AtomicU64::new(0);
    let bits = ON.get();
    if bits == 0 {
        return None;
    }
    let limits = Limits {
        each: Duration::from_millis(bits >> 32),
        frame: Duration::from_millis(bits & u64::from(u32::MAX)),
    };
    if LOGGED.swap(bits, Ordering::Relaxed) != bits {
        tracing::info!(
            each_ms = limits.each.as_millis() as u64,
            frame_ms = limits.frame.as_millis() as u64,
            "VICTORY_SCREEN_PIPELINE_WAIT: a screen pass waits for its pipeline instead of being dropped, bounded per draw and per frame"
        );
    }
    Some(limits)
}

/// At a first use of `p` not made yet: a screen pass goes to the front of
/// the workers' queue while the switch is on.
pub(super) fn front(p: &Arc<Pending>) {
    if p.screen && limits().is_some() {
        to_front(p);
    }
}

/// Draws that got their pipeline by waiting on, and draws dropped after all.
static MADE: AtomicU64 = AtomicU64::new(0);
static GAVE_UP: AtomicU64 = AtomicU64::new(0);
static WAIT_LINES: AtomicU64 = AtomicU64::new(0);

/// After the usual wait of a draw for `p` in `frame`: waits on if the
/// switch is on and `p` is a screen pass not made yet.
pub(super) fn after(p: &Arc<Pending>, frame: u64) {
    if !p.screen || p.made.get().is_some() {
        return;
    }
    let Some(l) = limits() else { return };
    static FRAMES: Mutex<FrameTake> = Mutex::new(FrameTake::new());
    let result = wait_taken(p, frame, l, &FRAMES);
    let line = WAIT_LINES.fetch_add(1, Ordering::Relaxed) < LINES_MOST;
    match result {
        Ok(waited) => {
            let made = MADE.fetch_add(1, Ordering::Relaxed) + 1;
            if line {
                tracing::info!(
                    shaders = ?p.shaders,
                    frame,
                    waited_ms = waited.as_millis() as u64,
                    made,
                    "VICTORY_SCREEN_PIPELINE_WAIT: a screen pass's pipeline was made while its draw waited"
                );
            }
        }
        Err((_, waited)) => {
            let gave_up = GAVE_UP.fetch_add(1, Ordering::Relaxed) + 1;
            if line {
                tracing::warn!(
                    shaders = ?p.shaders,
                    frame,
                    waited_ms = waited.as_millis() as u64,
                    gave_up,
                    "VICTORY_SCREEN_PIPELINE_WAIT: a screen pass's pipeline was not made in time: its draw is dropped"
                );
            }
        }
    }
}

/// Screen-pass draws dropped since the last present: the pipeline being
/// made, refused; and in the run.
static DROPPED: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];
static DROPPED_RUN: AtomicU64 = AtomicU64::new(0);
static DROP_LINES: AtomicU64 = AtomicU64::new(0);

/// A draw of `p` was dropped: counted if `p` is a screen pass without its
/// host pipeline (not left out by a switch).
pub(super) fn dropped(p: &Pending) {
    if !p.screen || p.left_out.is_some() {
        return;
    }
    let refused = match p.made.get() {
        None => false,
        Some(None) => true,
        Some(Some(_)) => return,
    };
    DROPPED[usize::from(refused)].fetch_add(1, Ordering::Relaxed);
    DROPPED_RUN.fetch_add(1, Ordering::Relaxed);
    if !refused {
        crate::flicker_catch::count(crate::flicker_catch::Kind::ScreenUnmade);
    }
}

/// The counts since the last present: (being made, refused), taken.
fn take() -> (u64, u64) {
    (
        DROPPED[0].swap(0, Ordering::Relaxed),
        DROPPED[1].swap(0, Ordering::Relaxed),
    )
}

/// At present `frame`: the line for a frame that dropped screen passes.
pub(super) fn presented(frame: u64) {
    let (unmade, refused) = take();
    if unmade + refused == 0 || DROP_LINES.fetch_add(1, Ordering::Relaxed) >= LINES_MOST {
        return;
    }
    tracing::info!(
        frame,
        unmade,
        refused,
        run = DROPPED_RUN.load(Ordering::Relaxed),
        pipelines_waiting = super::waiting(),
        wait_on = limits().is_some(),
        "screen passes dropped without a pipeline"
    );
}

#[cfg(test)]
mod tests {
    use super::super::{GraphicsState, ready};
    use super::*;
    use windows::Win32::Graphics::Direct3D12::{
        D3D12_PRIMITIVE_TOPOLOGY_TYPE_LINE, D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
    };

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    fn pending(
        topology: windows::Win32::Graphics::Direct3D12::D3D12_PRIMITIVE_TOPOLOGY_TYPE,
    ) -> Arc<Pending> {
        let mut state = GraphicsState::default();
        state.topology = topology;
        Arc::new(Pending::new(Vec::new(), Some(Box::new(state))))
    }

    #[test]
    fn the_switch_parses_with_its_bounds() {
        assert_eq!(parse(None), None);
        assert_eq!(parse(Some("0")), None);
        assert_eq!(parse(Some("x")), None);
        assert_eq!(
            parse(Some("1")),
            Some(Limits {
                each: ms(3000),
                frame: ms(5000)
            })
        );
        assert_eq!(
            parse(Some("8000")),
            Some(Limits {
                each: ms(8000),
                frame: ms(8000)
            })
        );
        assert_eq!(
            parse(Some("50000,90000")),
            Some(Limits {
                each: ms(10_000),
                frame: ms(10_000)
            })
        );
        assert_eq!(
            parse(Some(" 200 , 400 ")),
            Some(Limits {
                each: ms(200),
                frame: ms(400)
            })
        );
    }

    #[test]
    fn the_switch_is_live() {
        let on = Some(Limits {
            each: ms(EACH_DEFAULT),
            frame: ms(FRAME_DEFAULT),
        });
        victory_core::settings::check_live("VICTORY_SCREEN_PIPELINE_WAIT", "1", limits, None, on);
    }

    /// With no frame budget at all, a screen pass's first use still waits
    /// until a worker has answered while the switch is on.
    #[test]
    fn a_screen_pass_waits_for_its_pipeline_with_the_switch() {
        super::super::TEST_BUDGET.set(Some(Duration::ZERO));
        TEST_LIMITS.set(Some(Some(Limits {
            each: ms(10_000),
            frame: ms(10_000),
        })));
        let p = pending(D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE);
        assert!(p.screen);
        assert!(ready(&p).is_none());
        assert!(
            matches!(p.made.get(), Some(None)),
            "the draw waits until the worker has answered"
        );
    }

    /// Only screen passes without a host pipeline are counted, and a
    /// present takes the counts.
    #[test]
    fn dropped_screen_passes_are_counted_per_frame() {
        let _ = take();
        let screen = pending(D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE);
        let lines = pending(D3D12_PRIMITIVE_TOPOLOGY_TYPE_LINE);
        let compute = Pending::new(Vec::new(), None);
        assert!(!lines.screen && !compute.screen);
        dropped(&screen);
        dropped(&screen);
        dropped(&lines);
        dropped(&compute);
        screen.make_with(|| None);
        dropped(&screen);
        // Other tests may drop screen passes at the same time.
        let (unmade, refused) = take();
        assert!(
            unmade >= 2 && refused >= 1,
            "unmade {unmade} refused {refused}"
        );
    }
}
