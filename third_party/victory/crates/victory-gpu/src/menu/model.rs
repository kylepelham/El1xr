//! (El1xr) What the in-game menu shows and changes: the settings registry
//! (`victory_core::settings`) behind the small [`MenuModel`] trait the menu's state works on, the
//! presets, and how some values read.

pub use victory_core::settings::{Applied, Apply, Info, Kind, SetError};

use victory_core::settings;

/// A named set of values (Quality / Balanced / Performance).
#[derive(Clone, Copy, Debug)]
pub struct Preset {
    pub name: &'static str,
    pub help: &'static str,
    pub values: &'static [(&'static str, &'static str)],
}

/// The menu's view of the settings registry.
pub trait MenuModel: Send {
    fn infos(&self) -> &[Info];
    /// The value chosen: a change made while running, else the launch value.
    fn get(&self, name: &str) -> Option<String>;
    /// The value the process started with.
    fn launched(&self, name: &str) -> Option<String>;
    /// When a change of `name` takes effect.
    fn apply_of(&self, name: &str) -> Apply;
    fn set(&mut self, name: &str, value: &str) -> Result<Applied, SetError>;
    /// Changed settings that wait for a restart.
    fn pending_restart(&self) -> Vec<&'static str>;
    /// Writes the settings file.
    fn save(&mut self) -> std::io::Result<()>;
    fn presets(&self) -> &[Preset] {
        &[]
    }
    /// What the menu is, after the name in its header.
    fn title(&self) -> &'static str {
        "Settings"
    }
}

/// The live registry. `save` is the registry's unless a test swaps it.
pub struct Registry {
    save: fn() -> std::io::Result<()>,
}

impl Default for Registry {
    fn default() -> Self {
        Registry {
            save: settings::save,
        }
    }
}

impl Registry {
    #[cfg(test)]
    pub fn with_save(save: fn() -> std::io::Result<()>) -> Self {
        Registry { save }
    }
}

impl MenuModel for Registry {
    fn infos(&self) -> &[Info] {
        settings::infos()
    }

    fn get(&self, name: &str) -> Option<String> {
        settings::get(name)
    }

    fn launched(&self, name: &str) -> Option<String> {
        settings::launched(name)
    }

    fn apply_of(&self, name: &str) -> Apply {
        settings::apply_of(name).unwrap_or(Apply::Restart)
    }

    fn set(&mut self, name: &str, value: &str) -> Result<Applied, SetError> {
        settings::set(name, value)
    }

    fn pending_restart(&self) -> Vec<&'static str> {
        settings::pending_restart()
    }

    fn save(&mut self) -> std::io::Result<()> {
        (self.save)()
    }

    fn presets(&self) -> &[Preset] {
        PRESETS
    }
}

/// DLSS presets over the upscaler and its quality mode (`upscale_live`'s numbers).
pub const PRESETS: &[Preset] = &[
    Preset {
        name: "Quality",
        help: "DLSS at full resolution (DLAA).",
        values: &[("VICTORY_UPSCALE", "dlss"), ("VICTORY_DLSS_QUALITY", "5")],
    },
    Preset {
        name: "Balanced",
        help: "DLSS Quality mode.",
        values: &[("VICTORY_UPSCALE", "dlss"), ("VICTORY_DLSS_QUALITY", "2")],
    },
    Preset {
        name: "Performance",
        help: "DLSS Performance mode.",
        values: &[("VICTORY_UPSCALE", "dlss"), ("VICTORY_DLSS_QUALITY", "0")],
    },
];

/// Settings listed first in their tab, in this order; the rest follow in the registry's.
pub const FIRST: &[&str] = &[
    "VICTORY_UPSCALE",
    "VICTORY_DLSS_QUALITY",
    "VICTORY_FFX_SCALE",
    "VICTORY_DLSS_SHARPNESS",
    "VICTORY_FRAME_GEN",
    "VICTORY_PRESENT_CAP",
    "VICTORY_PRESENT_SYNC",
    "VICTORY_PRESENT_SMOOTH",
    "VICTORY_OVERLAY",
];

/// Values that read better by name, in the order the menu steps through them.
const NAMED: &[(&str, &[(&str, &str)])] = &[
    (
        "VICTORY_UPSCALE",
        &[
            ("off", "Title's own"),
            ("stretch", "Stretch"),
            ("dlss", "DLSS"),
        ],
    ),
    (
        "VICTORY_DLSS_QUALITY",
        &[
            ("3", "Ultra performance"),
            ("0", "Performance"),
            ("1", "Balanced"),
            ("2", "Quality"),
            ("4", "Ultra quality"),
            ("5", "DLAA"),
        ],
    ),
];

/// The values of `name` by name, in stepping order, if it has them.
pub fn named(name: &str) -> Option<&'static [(&'static str, &'static str)]> {
    NAMED.iter().find(|(n, _)| *n == name).map(|&(_, v)| v)
}

/// Tests that change the process-wide registry take this first.
#[cfg(test)]
pub static REGISTRY_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
pub fn registry_lock() -> std::sync::MutexGuard<'static, ()> {
    REGISTRY_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(name: &str) -> &'static Info {
        settings::info(name).unwrap_or_else(|| panic!("{name} is not registered"))
    }

    #[test]
    fn presets_names_and_first_rows_are_registered_and_valid() {
        for preset in PRESETS {
            for (name, value) in preset.values {
                assert!(settings::valid(info(name).kind, value), "{name}={value}");
            }
        }
        for name in FIRST {
            info(name);
        }
        for (name, values) in NAMED {
            for (value, _) in *values {
                assert!(settings::valid(info(name).kind, value), "{name}={value}");
            }
        }
    }

    #[test]
    fn the_display_settings_have_the_kinds_the_menu_edits() {
        assert_eq!(
            info("VICTORY_UPSCALE").kind,
            Kind::Choice(&["off", "stretch", "dlss"])
        );
        assert_eq!(
            info("VICTORY_DLSS_QUALITY").kind,
            Kind::Int { min: 0, max: 5 }
        );
        assert!(matches!(
            info("VICTORY_FFX_SCALE").kind,
            Kind::Float { min, max, .. } if min == 0.5 && max == 1.0
        ));
        assert!(matches!(info("VICTORY_FRAME_GEN").kind, Kind::Choice(_)));
        for name in [
            "VICTORY_UPSCALE",
            "VICTORY_DLSS_QUALITY",
            "VICTORY_FFX_SCALE",
            "VICTORY_FRAME_GEN",
        ] {
            assert_eq!(info(name).group, "Display", "{name}");
            assert!(!info(name).expert, "{name}");
        }
        assert_eq!(info("VICTORY_FRAME_GEN").apply, Apply::Restart);
    }

    /// The round 1 demo's made-up names (nothing in the tree reads them) are gone from the menu
    /// and the registry. Spelled in pieces so the registry's scan of the tree does not take them
    /// for reads.
    #[test]
    fn no_demo_names_remain() {
        let demo = ["FRAME_CAP", "DLSS_MODE", "NOPE"].map(|n| format!("VICTORY_{n}"));
        let sources = [
            include_str!("mod.rs"),
            include_str!("draw.rs"),
            include_str!("window.rs"),
            include_str!("layout.rs"),
            include_str!("state.rs"),
            include_str!("input.rs"),
            include_str!("model.rs"),
        ];
        for name in &demo {
            let quoted = format!("\"{name}\"");
            for s in sources {
                assert!(!s.contains(&quoted), "{name}");
            }
            assert!(settings::info(name).is_none(), "{name} is not a setting");
        }
        assert!(sources.iter().all(|s| !s.contains(&["Demo", "Model"].concat())));
    }
}
