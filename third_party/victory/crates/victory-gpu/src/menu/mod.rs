//! (El1xr) The in-game settings menu, `VICTORY_MENU=1` (default off). F9 (`VICTORY_MENU_KEY`
//! rebinds it: `F1`..`F24` or a virtual-key number like `0x78`) or View + Menu held for a second
//! opens and closes it. While it is open the HLE input layer gives the title neutral pads, no keys
//! and no mouse, and the cursor shows.
//!
//! The menu is a layered window like the overlay ([`crate::overlay`]), drawn on the CPU with GDI
//! text: it never touches the swap chain, the present path or any GPU state, so closed it costs
//! one key poll every 30 ms, and F12 screenshots (taken from the back buffer) leave it out.
//!
//! It works on the settings registry (`victory_core::settings`) through [`model::Registry`]: tabs
//! are the registry's groups (Display in headed sections), each row's LIVE / RESTART / REBUILD
//! tag is `apply_of`, changes go through `settings::set`, a change that waits for the next start
//! is written to the settings file at once, and SAVE writes the live ones too.
//!
//! The trainer menu ([`crate::modmenu`], `VICTORY_MOD_MENU=1`, F8) is a second window of the same
//! kind over its own model, with its own hotkey and press counters; either one open blocks the
//! title's input.

pub mod display;
pub mod input;
pub mod layout;
pub mod model;
pub mod selftest;
pub mod short;
pub mod state;

#[cfg(windows)]
mod draw;
#[cfg(windows)]
mod window;

use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering::Relaxed};

pub use display::primary_display_size;
use input::{CHORD, Inputs, Key};
use layout::Hit;
use model::MenuModel;
use state::{Action, Menu, Row};

/// The title gets no input (the menu is open, or keys that closed it are still held).
static BLOCKED: AtomicBool = AtomicBool::new(false);
/// The first pad's buttons (with the left stick as d-pad bits), and the presses since the menu
/// last looked: the HLE polls far more often than the menu.
static PAD_DOWN: AtomicU32 = AtomicU32::new(0);
static PAD_PRESSED: AtomicU32 = AtomicU32::new(0);
/// Mouse wheel notches over the game window, in WHEEL_DELTA units.
static WHEEL: AtomicI32 = AtomicI32::new(0);
/// The trainer window's own: blocked, presses and wheel (each window takes its counters).
static TRAINER_BLOCKED: AtomicBool = AtomicBool::new(false);
static TRAINER_PRESSED: AtomicU32 = AtomicU32::new(0);
static TRAINER_WHEEL: AtomicI32 = AtomicI32::new(0);

/// One menu window: its hotkey, model, counters and log name.
pub(crate) struct Spec {
    pub thread: &'static str,
    pub class: &'static str,
    pub what: &'static str,
    pub hotkey: fn() -> u8,
    pub model: fn() -> Box<dyn MenuModel>,
    pub blocked: &'static AtomicBool,
    pub pressed: &'static AtomicU32,
    pub wheel: &'static AtomicI32,
    /// Runs `VICTORY_MENU_SELFTEST`.
    pub selftest: bool,
}

#[cfg(windows)]
static SETTINGS: Spec = Spec {
    thread: "victory-menu",
    class: "El1xrMenu",
    what: "menu on (VICTORY_MENU=1; View + Menu held 1 s also opens it)",
    hotkey,
    model: || Box::new(model::Registry::default()),
    blocked: &BLOCKED,
    pressed: &PAD_PRESSED,
    wheel: &WHEEL,
    selftest: true,
};

#[cfg(windows)]
static TRAINER: Spec = Spec {
    thread: "victory-mod-menu",
    class: "El1xrModMenu",
    what: "mod menu: trainer window on (VICTORY_MOD_MENU=1)",
    hotkey: crate::modmenu::hotkey,
    model: crate::modmenu::trainer,
    blocked: &TRAINER_BLOCKED,
    pressed: &TRAINER_PRESSED,
    wheel: &TRAINER_WHEEL,
    selftest: false,
};

const VK_F9: u8 = 0x78;

/// `VICTORY_MENU=1`.
pub fn enabled() -> bool {
    static ON: LazyLock<bool> =
        LazyLock::new(|| std::env::var("VICTORY_MENU").is_ok_and(|v| v.trim() == "1"));
    *ON
}

/// Whether the title's input is blocked now.
pub fn blocked() -> bool {
    BLOCKED.load(Relaxed) || TRAINER_BLOCKED.load(Relaxed)
}

/// Called by the HLE at each input poll with the first pad's buttons (GameInput bits) and left
/// stick; returns whether the title's input is blocked.
pub fn feed_pad(buttons: u32, stick_x: f32, stick_y: f32) -> bool {
    if !enabled() && !crate::modmenu::enabled() {
        return false;
    }
    let bits = buttons | input::stick_bits(stick_x, stick_y);
    let before = PAD_DOWN.swap(bits, Relaxed);
    PAD_PRESSED.fetch_or(bits & !before, Relaxed);
    TRAINER_PRESSED.fetch_or(bits & !before, Relaxed);
    blocked()
}

/// The buttons the title sees: with the menu on, View and Menu held together are the menu's chord.
pub fn chord_mask(buttons: u32) -> u32 {
    if enabled() && buttons & CHORD == CHORD {
        buttons & !CHORD
    } else {
        buttons
    }
}

/// Whether the menu's hotkey is `vk` (other F-key users stand aside).
pub fn owns_key(vk: u8) -> bool {
    (enabled() && hotkey() == vk) || (crate::modmenu::enabled() && crate::modmenu::hotkey() == vk)
}

/// The game window got wheel movement while the menu is open.
pub fn wheel(delta: i32) {
    WHEEL.fetch_add(delta, Relaxed);
    TRAINER_WHEEL.fetch_add(delta, Relaxed);
}

/// The hotkey's virtual key.
pub fn hotkey() -> u8 {
    static KEY: LazyLock<u8> = LazyLock::new(|| {
        std::env::var("VICTORY_MENU_KEY")
            .ok()
            .and_then(|v| parse_key(&v))
            .unwrap_or(VK_F9)
    });
    *KEY
}

/// `F1`..`F24`, or a virtual-key number (`0x78`, `120`).
pub fn parse_key(v: &str) -> Option<u8> {
    let v = v.trim();
    if let Some(n) = v
        .strip_prefix(['F', 'f'])
        .and_then(|n| n.parse::<u8>().ok())
    {
        return (1..=24).contains(&n).then(|| 0x6F + n);
    }
    let n = match v.strip_prefix("0x").or_else(|| v.strip_prefix("0X")) {
        Some(hex) => u8::from_str_radix(hex, 16).ok()?,
        None => v.parse().ok()?,
    };
    (n >= 0x08).then_some(n)
}

pub fn hotkey_name() -> String {
    key_name(hotkey())
}

pub fn key_name(vk: u8) -> String {
    match vk {
        k @ 0x70..=0x87 => format!("F{}", k - 0x6F),
        k => format!("key 0x{k:02X}"),
    }
}

/// Starts the menus' window threads for the game window `hwnd` (`VICTORY_MENU=1`,
/// `VICTORY_MOD_MENU=1`; first call only).
#[cfg(windows)]
pub(crate) fn start(hwnd: windows::Win32::Foundation::HWND) {
    if enabled() {
        window::start(hwnd.0 as usize, &SETTINGS);
    }
    if crate::modmenu::enabled() {
        window::start(hwnd.0 as usize, &TRAINER);
    }
}

/// The menu with its model and input, with no OS calls: the window feeds it what is held and
/// where the mouse is, and draws when it says so.
pub struct Driver {
    pub menu: Menu,
    pub model: Box<dyn MenuModel>,
    inputs: Inputs,
    /// Closed, but a key or button that was held is not yet let go.
    draining: bool,
}

impl Driver {
    pub fn new(model: Box<dyn MenuModel>) -> Self {
        Driver {
            menu: Menu::for_model(&*model),
            model,
            inputs: Inputs::default(),
            draining: false,
        }
    }

    /// Lays the menu out for a `w` x `h` game picture.
    pub fn layout(&mut self, w: i32, h: i32) -> layout::Layout {
        let tabs = self.menu.tabs(&*self.model);
        let rows = self.menu.rows(&*self.model);
        let pending = !self.model.pending_restart().is_empty();
        let heading = |i: usize| rows.get(i).is_some_and(|r| !state::selectable(r));
        layout::layout(w, h, &mut self.menu, &tabs, rows.len(), &heading, pending)
    }

    /// Whether the title's input is blocked.
    pub fn blocked(&self) -> bool {
        self.menu.open || self.draining
    }

    fn act(&mut self, action: Action) -> bool {
        let was = self.menu.open;
        let changed = self.menu.act(&mut *self.model, action);
        if was && !self.menu.open {
            self.draining = true;
        }
        changed
    }

    /// One poll at `now` (ms): the keys held, the pad bits held (or pressed since the last poll),
    /// and wheel movement. Returns whether the menu's picture changed.
    pub fn tick(&mut self, now: u64, keys: &[Key], pad: u32, wheel: i32) -> bool {
        let mut changed = false;
        for action in self.inputs.update(now, keys, pad) {
            changed |= self.act(action);
        }
        if self.menu.open && wheel != 0 {
            let action = if wheel > 0 { Action::Up } else { Action::Down };
            for _ in 0..(wheel.unsigned_abs() / 120).max(1) {
                changed |= self.act(action);
            }
        }
        if self.draining && keys.is_empty() && pad == 0 {
            self.draining = false;
        }
        changed
    }

    /// A left click on `hit`.
    pub fn click(&mut self, hit: Option<Hit>) -> bool {
        let Some(hit) = hit else { return false };
        let actions: Vec<Action> = match hit {
            Hit::Close => vec![Action::Close],
            Hit::Expert => vec![Action::ToggleExpert],
            Hit::Tab(t) => vec![Action::SelectTab(t)],
            Hit::Row(i) => vec![Action::SelectRow(i)],
            Hit::ValueLeft(i) | Hit::ValueRight(i) => {
                let setting = matches!(self.menu.rows(&*self.model).get(i), Some(Row::Setting(_)));
                let change = match (setting, hit) {
                    (false, _) => Action::Activate,
                    (true, Hit::ValueLeft(_)) => Action::Left,
                    _ => Action::Right,
                };
                vec![Action::SelectRow(i), change]
            }
        };
        actions.into_iter().fold(false, |c, a| self.act(a) | c)
    }
}

#[cfg(test)]
mod tests {
    use super::input::{PAD_A, PAD_B, PAD_DOWN as DOWN};
    use super::model::{Registry, registry_lock};
    use super::*;

    fn driver() -> Driver {
        Driver::new(Box::new(Registry::default()))
    }

    #[test]
    fn hotkeys_parse() {
        assert_eq!(parse_key("F9"), Some(0x78));
        assert_eq!(parse_key("f12"), Some(0x7B));
        assert_eq!(parse_key("F25"), None);
        assert_eq!(parse_key("0x77"), Some(0x77));
        assert_eq!(parse_key("120"), Some(0x78));
        assert_eq!(parse_key("1"), None, "mouse buttons are not hotkeys");
        assert_eq!(parse_key("nope"), None);
    }

    #[test]
    fn input_is_blocked_while_open_and_until_closing_keys_are_let_go() {
        let mut d = driver();
        assert!(!d.blocked());
        assert!(
            !d.tick(0, &[], PAD_A, 0),
            "pad presses do nothing while closed"
        );
        assert!(!d.blocked());
        assert!(d.tick(30, &[Key::Hotkey], 0, 0));
        assert!(d.menu.open && d.blocked());
        d.tick(60, &[], 0, 0);
        assert!(d.blocked(), "open");
        assert!(d.tick(90, &[], PAD_B, 0), "B closes");
        assert!(!d.menu.open);
        assert!(d.blocked(), "B still held: the title must not see it");
        d.tick(120, &[], PAD_B, 0);
        assert!(d.blocked());
        d.tick(150, &[], 0, 0);
        assert!(!d.blocked());
        // The chord opens it after a second, from closed.
        d.tick(200, &[], CHORD, 0);
        assert!(!d.menu.open);
        d.tick(1200, &[], CHORD, 0);
        assert!(d.menu.open);
    }

    #[test]
    fn pad_wheel_and_clicks_navigate() {
        let _lock = registry_lock();
        let mut d = driver();
        d.tick(0, &[Key::Hotkey], 0, 0);
        // Row 0 is the Display tab's first section title: the menu opens on row 1.
        assert_eq!(d.menu.selected, 1);
        assert!(!d.click(Some(Hit::Row(0))), "a section title is never selected");
        d.tick(30, &[], DOWN, 0);
        assert_eq!(d.menu.selected, 2);
        d.tick(60, &[], 0, -240);
        assert_eq!(d.menu.selected, 4);
        d.tick(90, &[], 0, 120);
        assert_eq!(d.menu.selected, 3);
        assert!(d.click(Some(Hit::ValueRight(2))));
        assert_eq!(
            (d.menu.selected, d.model.get("VICTORY_UPSCALE").as_deref()),
            (2, Some("stretch"))
        );
        assert!(d.click(Some(Hit::ValueLeft(2))));
        assert_eq!(d.model.get("VICTORY_UPSCALE").as_deref(), Some("off"));
        assert!(d.click(Some(Hit::Tab(state::Tab::Presets))));
        assert!(d.click(Some(Hit::ValueLeft(0))));
        assert_eq!(d.menu.status.as_deref(), Some("Quality preset applied, saved"));
        let undo = d.menu.rows(&*d.model).len() - 1;
        assert!(d.click(Some(Hit::ValueLeft(undo))));
        assert!(d.model.pending_restart().is_empty());
        assert_eq!(d.model.get("VICTORY_UPSCALE"), d.model.launched("VICTORY_UPSCALE"));
        assert!(!d.click(None));
        assert!(d.click(Some(Hit::Close)));
        assert!(
            !d.menu.open && d.blocked(),
            "closed by click; nothing held, so the next poll lets go"
        );
        d.tick(120, &[], 0, 0);
        assert!(!d.blocked());
    }

    /// The menu has no hold on the GPU: nothing in it names D3D12 or DXGI, and the present path
    /// only starts it.
    #[test]
    fn menu_touches_no_gpu_state() {
        let sources = [
            include_str!("mod.rs"),
            include_str!("draw.rs"),
            include_str!("window.rs"),
            include_str!("layout.rs"),
            include_str!("state.rs"),
            include_str!("input.rs"),
            include_str!("model.rs"),
            include_str!("selftest.rs"),
        ];
        for s in sources {
            let code: String = s
                .split("#[cfg(test)]")
                .next()
                .unwrap_or_default()
                .lines()
                .filter(|l| !l.trim_start().starts_with("//"))
                .collect::<Vec<_>>()
                .join("\n");
            for gpu in [
                "Direct3D12",
                "Dxgi",
                "ID3D12",
                "IDXGI",
                "swap_chain",
                "Present(",
            ] {
                assert!(!code.contains(gpu), "{gpu}");
            }
        }
        let host = include_str!("../host.rs");
        assert_eq!(
            host.matches("crate::menu::").count(),
            1,
            "host.rs only starts the menu"
        );
        assert!(!include_str!("../pacing.rs").contains("menu"));
        // F12 screenshots leave the menu out; only a selftest ask carries its bitmap.
        for s in [include_str!("../screenshot.rs"), include_str!("../menu_shot.rs")] {
            assert!(!s.contains("crate::menu::"), "screenshots do not reach into the menu");
        }
    }
}
