//! (bootfast) `VICTORY_INPUT_TRACE=1` (off by default): a diagnostic line
//! each second in which the title's way of reading input changed, to find
//! the moment the opening cutscene's skip prompt is up (the harness taps
//! Enter at a fixed cadence; one that presses A when the prompt shows needs
//! a signal for it). Per second it counts the title's current-reading calls
//! by the input kinds asked for, the gamepad states handed to it, and of
//! those the ones with A held; a line is written when the kinds asked for
//! change, the calls per second move by more than half, or A is first seen
//! held or released, so a run's log holds a few dozen lines, not one a
//! second.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_INPUT_TRACE").is_some_and(|v| v == "1"))
}

/// One second's reading of input.
#[derive(Clone, Default, Debug, PartialEq)]
struct Second {
    /// Current-reading calls by the kinds asked for.
    kinds: BTreeMap<u32, u64>,
    /// Gamepad states handed out, and of them with A held.
    pads: u64,
    pads_a: u64,
}

impl Second {
    fn calls(&self) -> u64 {
        self.kinds.values().sum()
    }
}

/// Whether `now` is worth a line after `last`: other kinds asked for, the
/// calls or pad reads per second moved by more than half, or A newly held
/// or released.
fn changed(last: &Second, now: &Second) -> bool {
    let moved = |a: u64, b: u64| a.max(b) > 2 && a.abs_diff(b) * 2 > a.max(b);
    !last.kinds.keys().eq(now.kinds.keys())
        || moved(last.calls(), now.calls())
        || moved(last.pads, now.pads)
        || (last.pads_a == 0) != (now.pads_a == 0)
}

struct Trace {
    began: Instant,
    second: u64,
    now: Second,
    last: Second,
}

static TRACE: Mutex<Option<Trace>> = Mutex::new(None);
static FIRST: AtomicBool = AtomicBool::new(true);

fn with(f: impl FnOnce(&mut Second)) {
    let mut guard = TRACE.lock().unwrap_or_else(|e| e.into_inner());
    let t = guard.get_or_insert_with(|| Trace {
        began: Instant::now(),
        second: 0,
        now: Second::default(),
        last: Second::default(),
    });
    let second = t.began.elapsed().as_secs();
    if second != t.second {
        let done = std::mem::take(&mut t.now);
        if FIRST.swap(false, Relaxed) || changed(&t.last, &done) {
            tracing::info!(
                second = t.second,
                calls = done.calls(),
                kinds = ?done.kinds.iter().map(|(k, n)| format!("{k:#x}x{n}")).collect::<Vec<_>>(),
                pads = done.pads,
                pads_with_a = done.pads_a,
                "title input reading changed (VICTORY_INPUT_TRACE)"
            );
        }
        t.last = done;
        t.second = second;
    }
    f(&mut t.now);
}

/// The title asked for the current reading of `kind`.
pub(super) fn current(kind: u32) {
    if on() {
        with(|s| *s.kinds.entry(kind).or_default() += 1);
    }
}

/// A gamepad state was handed to the title, with A held or not.
pub(super) fn pad(a: bool) {
    if on() {
        with(|s| {
            s.pads += 1;
            s.pads_a += u64::from(a);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn second(kinds: &[(u32, u64)], pads: u64, pads_a: u64) -> Second {
        Second {
            kinds: kinds.iter().copied().collect(),
            pads,
            pads_a,
        }
    }

    #[test]
    fn a_line_only_when_the_reading_changes() {
        let base = second(&[(0x8, 60)], 60, 0);
        assert!(!changed(&base, &second(&[(0x8, 55)], 58, 0)));
        assert!(changed(&base, &second(&[(0x8, 60), (0x4, 30)], 60, 0)));
        assert!(changed(&base, &second(&[(0x8, 20)], 60, 0)));
        assert!(changed(&base, &second(&[(0x8, 60)], 10, 0)));
        assert!(changed(&base, &second(&[(0x8, 60)], 60, 3)));
        assert!(!changed(&second(&[], 1, 0), &second(&[], 2, 0)));
    }
}
