//! (El1xr) Turns held keys and pad buttons into menu actions, with no OS calls: presses fire once,
//! navigation and value steps repeat while held, and a chord held for a second toggles the menu
//! (View + Menu the settings menu, both stick clicks the trainer: neither is a move the title
//! makes by holding them together).
//!
//! Pad: d-pad or left stick move and change, A changes or applies, B closes, LB/RB change tabs,
//! LT/RT step the value down/up, Y shows the expert settings.

use super::state::Action;

/// GameInput's gamepad button bits (the HLE passes them through as the title sees them).
pub const PAD_MENU: u32 = 0x1;
pub const PAD_VIEW: u32 = 0x2;
pub const PAD_A: u32 = 0x4;
pub const PAD_B: u32 = 0x8;
pub const PAD_X: u32 = 0x10;
pub const PAD_Y: u32 = 0x20;
pub const PAD_UP: u32 = 0x40;
pub const PAD_DOWN: u32 = 0x80;
pub const PAD_LEFT: u32 = 0x100;
pub const PAD_RIGHT: u32 = 0x200;
pub const PAD_LB: u32 = 0x400;
pub const PAD_RB: u32 = 0x800;
pub const PAD_LS: u32 = 0x1000;
pub const PAD_RS: u32 = 0x2000;
/// The triggers past half way, as bits of ours above GameInput's.
pub const PAD_LT: u32 = 0x1_0000;
pub const PAD_RT: u32 = 0x2_0000;
/// The settings menu's chord.
pub const CHORD: u32 = PAD_VIEW | PAD_MENU;
/// The trainer's chord: both stick clicks.
pub const TRAINER_CHORD: u32 = PAD_LS | PAD_RS;
/// The report key's chord (`VICTORY_REPORT_KEY`): View + RS.
pub const REPORT_CHORD: u32 = PAD_VIEW | PAD_RS;
/// Bits [`Inputs::update`] reads.
const PAD_BITS: u32 = 18;

/// How long View + Menu are held to toggle the menu.
pub const CHORD_MS: u64 = 1000;
const REPEAT_DELAY_MS: u64 = 350;
const REPEAT_EVERY_MS: u64 = 75;

/// A keyboard key, as the menu means it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Hotkey,
    Esc,
    Up,
    Down,
    Left,
    Right,
    Enter,
    PrevTab,
    NextTab,
    Expert,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Input {
    Key(Key),
    Pad(u32),
    Chord,
}

impl Input {
    fn repeats(self) -> bool {
        matches!(
            self,
            Input::Key(Key::Up | Key::Down | Key::Left | Key::Right)
                | Input::Pad(PAD_UP | PAD_DOWN | PAD_LEFT | PAD_RIGHT | PAD_LT | PAD_RT)
        )
    }

    fn action(self) -> Option<Action> {
        Some(match self {
            Input::Key(Key::Hotkey) | Input::Chord => Action::Toggle,
            Input::Key(Key::Esc) | Input::Pad(PAD_B) => Action::Close,
            Input::Key(Key::Up) | Input::Pad(PAD_UP) => Action::Up,
            Input::Key(Key::Down) | Input::Pad(PAD_DOWN) => Action::Down,
            Input::Key(Key::Left) | Input::Pad(PAD_LEFT | PAD_LT) => Action::Left,
            Input::Key(Key::Right) | Input::Pad(PAD_RIGHT | PAD_RT) => Action::Right,
            Input::Key(Key::Enter) | Input::Pad(PAD_A) => Action::Activate,
            Input::Key(Key::PrevTab) | Input::Pad(PAD_LB) => Action::PrevTab,
            Input::Key(Key::NextTab) | Input::Pad(PAD_RB) => Action::NextTab,
            Input::Key(Key::Expert) | Input::Pad(PAD_Y) => Action::ToggleExpert,
            Input::Pad(_) => return None,
        })
    }
}

/// The pad's left stick as d-pad bits (past half way).
pub fn stick_bits(x: f32, y: f32) -> u32 {
    let mut bits = 0;
    if y > 0.5 {
        bits |= PAD_UP;
    }
    if y < -0.5 {
        bits |= PAD_DOWN;
    }
    if x < -0.5 {
        bits |= PAD_LEFT;
    }
    if x > 0.5 {
        bits |= PAD_RIGHT;
    }
    bits
}

/// The triggers (0 to 1) as [`PAD_LT`] / [`PAD_RT`] bits (past half way).
pub fn trigger_bits(left: f32, right: f32) -> u32 {
    (if left > 0.5 { PAD_LT } else { 0 }) | (if right > 0.5 { PAD_RT } else { 0 })
}

/// Which device the last action came from, for the footer's hints.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Device {
    #[default]
    Keys,
    Pad,
}

/// What is held, and since when.
pub struct Inputs {
    /// Input, when it went down, when it last fired.
    held: Vec<(Input, u64, Option<u64>)>,
    /// The pad buttons that toggle this menu when held together for [`CHORD_MS`].
    chord: u32,
    /// Where the last action came from.
    pub last: Device,
}

impl Default for Inputs {
    fn default() -> Self {
        Inputs::with_chord(CHORD)
    }
}

impl Inputs {
    pub fn with_chord(chord: u32) -> Self {
        Inputs {
            held: Vec::new(),
            chord,
            last: Device::Keys,
        }
    }

    /// The actions for what is held at `now` (ms): `keys` and pad bits `pad`.
    pub fn update(&mut self, now: u64, keys: &[Key], pad: u32) -> Vec<Action> {
        let mut down: Vec<Input> = keys.iter().map(|&k| Input::Key(k)).collect();
        let chord = self.chord;
        if pad & chord == chord {
            // The chord's buttons are the chord's alone while both are held.
            down.push(Input::Chord);
        }
        let single = if pad & chord == chord {
            pad & !chord
        } else {
            pad
        };
        down.extend(
            (0..PAD_BITS)
                .map(|b| 1u32 << b)
                .filter(|b| single & b != 0)
                .map(Input::Pad),
        );
        self.held.retain(|(i, ..)| down.contains(i));
        for &i in &down {
            if !self.held.iter().any(|(h, ..)| *h == i) {
                self.held.push((i, now, None));
            }
        }
        let mut fired = Vec::new();
        for (input, since, last) in &mut self.held {
            let fire = match (*input, *last) {
                (Input::Chord, None) => now - *since >= CHORD_MS,
                (Input::Chord, Some(_)) => false,
                (_, None) => true,
                (i, Some(l)) => {
                    i.repeats() && now - *since >= REPEAT_DELAY_MS && now - l >= REPEAT_EVERY_MS
                }
            };
            if fire {
                *last = Some(now);
                if let Some(action) = input.action() {
                    fired.push(action);
                    self.last = match input {
                        Input::Key(_) => Device::Keys,
                        Input::Pad(_) | Input::Chord => Device::Pad,
                    };
                }
            }
        }
        fired
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presses_fire_once_and_navigation_repeats() {
        let mut i = Inputs::default();
        assert_eq!(i.update(0, &[Key::Hotkey], 0), vec![Action::Toggle]);
        assert!(
            i.update(500, &[Key::Hotkey], 0).is_empty(),
            "no repeat for the hotkey"
        );
        assert!(i.update(530, &[], 0).is_empty());
        assert_eq!(i.update(560, &[Key::Hotkey], 0), vec![Action::Toggle]);
        assert_eq!(i.update(1000, &[Key::Down], 0), vec![Action::Down]);
        assert!(i.update(1300, &[Key::Down], 0).is_empty());
        assert_eq!(i.update(1360, &[Key::Down], 0), vec![Action::Down]);
        assert!(i.update(1400, &[Key::Down], 0).is_empty());
        assert_eq!(i.update(1440, &[Key::Down], 0), vec![Action::Down]);
        assert_eq!(
            i.update(1500, &[], PAD_A | PAD_RB),
            vec![Action::Activate, Action::NextTab]
        );
    }

    #[test]
    fn chord_toggles_after_a_second_and_hides_its_buttons() {
        let mut i = Inputs::default();
        assert!(
            i.update(0, &[], CHORD).is_empty(),
            "neither Menu nor View acts alone"
        );
        assert!(i.update(990, &[], CHORD).is_empty());
        assert_eq!(i.update(1000, &[], CHORD), vec![Action::Toggle]);
        assert!(i.update(3000, &[], CHORD).is_empty(), "once per hold");
        i.update(3010, &[], 0);
        assert!(
            i.update(3020, &[], CHORD | PAD_A)
                .contains(&Action::Activate)
        );
        assert_eq!(i.update(4020, &[], CHORD), vec![Action::Toggle]);
    }

    /// The triggers step the value and repeat like the d-pad; the trainer's chord is both stick
    /// clicks and leaves View + Menu alone; the last device is noted for the hints.
    #[test]
    fn triggers_step_and_repeat_and_each_menu_has_its_chord() {
        let mut i = Inputs::default();
        assert_eq!(trigger_bits(0.9, 0.1), PAD_LT);
        assert_eq!(trigger_bits(0.2, 0.6), PAD_RT);
        assert_eq!(i.update(0, &[], PAD_RT), vec![Action::Right]);
        assert_eq!(i.last, Device::Pad);
        assert!(i.update(300, &[], PAD_RT).is_empty());
        assert_eq!(i.update(360, &[], PAD_RT), vec![Action::Right], "repeats after the delay");
        assert_eq!(i.update(400, &[], PAD_LT), vec![Action::Left]);
        assert_eq!(i.update(450, &[Key::Down], 0), vec![Action::Down]);
        assert_eq!(i.last, Device::Keys);
        assert!(i.update(500, &[], TRAINER_CHORD).is_empty(), "stick clicks do nothing here");
        assert!(i.update(1600, &[], TRAINER_CHORD).is_empty());
        let mut t = Inputs::with_chord(TRAINER_CHORD);
        assert!(t.update(0, &[], CHORD).is_empty());
        assert!(t.update(1100, &[], CHORD).is_empty(), "View + Menu is the settings menu's");
        t.update(1130, &[], 0);
        assert!(t.update(1200, &[], TRAINER_CHORD).is_empty());
        assert_eq!(t.update(2200, &[], TRAINER_CHORD), vec![Action::Toggle]);
        assert_eq!(t.update(2260, &[], TRAINER_CHORD | PAD_B), vec![Action::Close]);
    }

    #[test]
    fn stick_acts_as_dpad() {
        assert_eq!(stick_bits(0.0, 0.9), PAD_UP);
        assert_eq!(stick_bits(-0.8, -0.8), PAD_DOWN | PAD_LEFT);
        assert_eq!(stick_bits(0.3, -0.3), 0);
    }
}
