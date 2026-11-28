//! (El1xr) What counts as a report press, with no OS calls: the report key going down, or View + RS
//! held together for a second (no other menu uses that pair: View + Menu is F9's, LS + RS the
//! trainer's). `VICTORY_REPORT_SELFTEST` presses through the same detector: its 1st, 3rd, ...
//! passes tap the key, its 2nd, 4th, ... hold the pad chord.

use crate::menu::input::{CHORD_MS, REPORT_CHORD};

/// The report chord.
pub const CHORD: u32 = REPORT_CHORD;
/// How long a selftest pass holds what it presses.
const TAP_MS: u64 = 90;
const HOLD_MS: u64 = CHORD_MS + 200;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Cause {
    Key,
    Pad,
}

impl Cause {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Cause::Key => "key",
            Cause::Pad => "View + RS held",
        }
    }
}

#[derive(Default)]
pub(crate) struct Trigger {
    key_was: bool,
    chord_since: Option<u64>,
    chord_fired: bool,
}

impl Trigger {
    /// One poll at `now` (ms): whether the key is down and which pad bits are held.
    pub(crate) fn tick(&mut self, now: u64, key: bool, pad: u32) -> Option<Cause> {
        let pressed = key && !self.key_was;
        self.key_was = key;
        if pad & CHORD == CHORD {
            let since = *self.chord_since.get_or_insert(now);
            if !self.chord_fired && now.saturating_sub(since) >= CHORD_MS {
                self.chord_fired = true;
                return Some(Cause::Pad);
            }
        } else {
            self.chord_since = None;
            self.chord_fired = false;
        }
        pressed.then_some(Cause::Key)
    }
}

/// `VICTORY_REPORT_SELFTEST=60,90`: seconds after the first present; nothing for 0 or no number.
pub(crate) fn parse_selftest(text: &str) -> Vec<u64> {
    let mut at: Vec<u64> = text
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .filter(|&s| s > 0)
        .collect();
    at.sort_unstable();
    at.dedup();
    at
}

/// What the selftest holds `ms` after the first present: the key, the pad bits, and which pass
/// (its seconds) is pressing.
pub(crate) fn injected(passes: &[u64], ms: u64) -> (bool, u32, Option<u64>) {
    for (i, &s) in passes.iter().enumerate() {
        let start = s * 1000;
        let by_pad = i % 2 == 1;
        let len = if by_pad { HOLD_MS } else { TAP_MS };
        if (start..start + len).contains(&ms) {
            return if by_pad {
                (false, CHORD, Some(s))
            } else {
                (true, 0, Some(s))
            };
        }
    }
    (false, 0, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::menu::input::{CHORD as MENU_CHORD, PAD_A, PAD_LS, PAD_RS, PAD_VIEW, TRAINER_CHORD};

    #[test]
    fn the_key_fires_on_its_press_and_the_chord_after_a_second_held() {
        let mut t = Trigger::default();
        assert_eq!(t.tick(0, true, 0), Some(Cause::Key));
        assert_eq!(t.tick(30, true, 0), None, "held: no repeat");
        assert_eq!(t.tick(60, false, 0), None);
        assert_eq!(t.tick(90, true, 0), Some(Cause::Key));
        assert_eq!(t.tick(100, false, CHORD), None);
        assert_eq!(t.tick(900, false, CHORD | PAD_A), None);
        assert_eq!(t.tick(1100, false, CHORD), Some(Cause::Pad));
        assert_eq!(t.tick(3000, false, CHORD), None, "once per hold");
        assert_eq!(t.tick(3030, false, PAD_VIEW), None);
        assert_eq!(t.tick(3060, false, CHORD), None);
        assert_eq!(t.tick(4100, false, CHORD), Some(Cause::Pad));
    }

    #[test]
    fn the_chord_is_neither_menus_and_their_chords_do_not_press() {
        assert_ne!(CHORD & MENU_CHORD, MENU_CHORD);
        assert_ne!(CHORD & TRAINER_CHORD, TRAINER_CHORD);
        assert_ne!(MENU_CHORD & CHORD, CHORD);
        assert_ne!(TRAINER_CHORD & CHORD, CHORD);
        let mut t = Trigger::default();
        for chord in [MENU_CHORD, TRAINER_CHORD, PAD_RS | PAD_LS] {
            assert_eq!(t.tick(0, false, chord), None);
            assert_eq!(t.tick(2000, false, chord), None);
            t.tick(2030, false, 0);
        }
    }

    #[test]
    fn selftest_passes_alternate_key_and_pad_through_the_detector() {
        assert_eq!(parse_selftest("90, 60,x,0,60"), vec![60, 90]);
        assert!(parse_selftest("").is_empty());
        let passes = [60, 90];
        assert_eq!(injected(&passes, 59_990), (false, 0, None));
        assert_eq!(injected(&passes, 60_010), (true, 0, Some(60)));
        assert_eq!(injected(&passes, 60_200), (false, 0, None));
        assert_eq!(injected(&passes, 91_000), (false, CHORD, Some(90)));
        let mut t = Trigger::default();
        let fired: Vec<(u64, Cause)> = (0..100_000u64)
            .step_by(30)
            .filter_map(|ms| {
                let (key, pad, _) = injected(&passes, ms);
                t.tick(ms, key, pad).map(|c| (ms, c))
            })
            .collect();
        assert_eq!(fired.len(), 2, "{fired:?}");
        assert_eq!(fired[0].1, Cause::Key);
        assert_eq!(fired[1].1, Cause::Pad);
        assert!((91_000..91_200).contains(&fired[1].0));
    }
}
