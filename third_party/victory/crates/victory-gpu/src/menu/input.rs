//! (El1xr) Turns held keys and pad buttons into menu actions, with no OS calls: presses fire once,
//! navigation repeats while held, and View + Menu held for a second toggles the menu.

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
pub const CHORD: u32 = PAD_VIEW | PAD_MENU;

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
                | Input::Pad(PAD_UP | PAD_DOWN | PAD_LEFT | PAD_RIGHT)
        )
    }

    fn action(self) -> Option<Action> {
        Some(match self {
            Input::Key(Key::Hotkey) | Input::Chord => Action::Toggle,
            Input::Key(Key::Esc) | Input::Pad(PAD_B) => Action::Close,
            Input::Key(Key::Up) | Input::Pad(PAD_UP) => Action::Up,
            Input::Key(Key::Down) | Input::Pad(PAD_DOWN) => Action::Down,
            Input::Key(Key::Left) | Input::Pad(PAD_LEFT) => Action::Left,
            Input::Key(Key::Right) | Input::Pad(PAD_RIGHT) => Action::Right,
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

/// What is held, and since when.
#[derive(Default)]
pub struct Inputs {
    /// Input, when it went down, when it last fired.
    held: Vec<(Input, u64, Option<u64>)>,
}

impl Inputs {
    /// The actions for what is held at `now` (ms): `keys` and pad bits `pad`.
    pub fn update(&mut self, now: u64, keys: &[Key], pad: u32) -> Vec<Action> {
        let mut down: Vec<Input> = keys.iter().map(|&k| Input::Key(k)).collect();
        if pad & CHORD == CHORD {
            // The chord's buttons are the chord's alone while both are held.
            down.push(Input::Chord);
        }
        let single = if pad & CHORD == CHORD {
            pad & !CHORD
        } else {
            pad
        };
        down.extend(
            (0..16)
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
                fired.extend(input.action());
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

    #[test]
    fn stick_acts_as_dpad() {
        assert_eq!(stick_bits(0.0, 0.9), PAD_UP);
        assert_eq!(stick_bits(-0.8, -0.8), PAD_DOWN | PAD_LEFT);
        assert_eq!(stick_bits(0.3, -0.3), 0);
    }
}
