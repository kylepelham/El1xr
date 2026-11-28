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
    /// A line that says more than the help under `name`'s row (what the next start renders at).
    fn detail(&self, _name: &str) -> Option<String> {
        None
    }
    /// The value an unset `name` stands for, when it is one of its own values (the monitor's
    /// resolution, DLSS's default mode).
    fn effective(&self, _name: &str) -> Option<String> {
        None
    }
    /// What the menu is, after the name in its header.
    fn title(&self) -> &'static str {
        "Settings"
    }
}

/// The live registry. `save` is the registry's; in tests it never writes (the menu saves restart
/// changes at once, and a test must not touch the user's settings file).
pub struct Registry {
    save: fn() -> std::io::Result<()>,
}

impl Default for Registry {
    fn default() -> Self {
        #[cfg(not(test))]
        let save = settings::save;
        #[cfg(test)]
        let save = || Ok(());
        Registry { save }
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

    fn detail(&self, name: &str) -> Option<String> {
        super::display::detail(name, settings::get, super::display::primary_display_size)
    }

    fn effective(&self, name: &str) -> Option<String> {
        super::display::effective(name, settings::get, super::display::primary_display_size)
    }
}

/// DLSS presets over the upscaler and its quality mode (`victory_core::render_plan`): the render
/// size follows the Resolution row.
pub const PRESETS: &[Preset] = &[
    Preset {
        name: "Quality",
        help: "DLSS Quality: renders at 0.667 of the resolution.",
        values: &[("VICTORY_UPSCALE", "dlss"), ("VICTORY_UPSCALE_QUALITY", "quality")],
    },
    Preset {
        name: "Balanced",
        help: "DLSS Balanced: renders at 0.58 of the resolution.",
        values: &[("VICTORY_UPSCALE", "dlss"), ("VICTORY_UPSCALE_QUALITY", "balanced")],
    },
    Preset {
        name: "Performance",
        help: "DLSS Performance: renders at half the resolution.",
        values: &[("VICTORY_UPSCALE", "dlss"), ("VICTORY_UPSCALE_QUALITY", "performance")],
    },
];

/// A tab split into headed sections: each lists its settings in order; the tab's other settings
/// follow under [`OTHER`] in the registry's order.
pub type Sections = &'static [(&'static str, &'static [&'static str])];

/// The groups whose tab is split into sections.
pub const SECTIONS: &[(&str, Sections)] = &[(
    "Display",
    &[
        (
            "Picture",
            &[
                "VICTORY_RESOLUTION",
                "VICTORY_UPSCALE",
                "VICTORY_UPSCALE_QUALITY",
                "VICTORY_RAY_TRACING",
                "VICTORY_FRAME_GEN",
                "VICTORY_DLSS_SHARPNESS",
                "VICTORY_DLSS_PRESET",
            ],
        ),
        (
            "Frame pacing",
            &[
                "VICTORY_PRESENT_CAP",
                "VICTORY_PRESENT_SYNC",
                "VICTORY_PRESENT_TEARING",
                "VICTORY_PRESENT_SMOOTH",
                "VICTORY_PRESENT_SMOOTH_READY",
            ],
        ),
        (
            "Screen and loading",
            &[
                "VICTORY_OVERLAY",
                "VICTORY_LOAD_FPS",
                "VICTORY_LOAD_END",
                "VICTORY_LOAD_UNPACED",
                "VICTORY_HOLD_LAST_FRAME",
            ],
        ),
        (
            "Lighting",
            &[
                "VICTORY_AMBIENT",
                "VICTORY_AMBIENT_AIR",
                "VICTORY_AMBIENT_MOST",
                "VICTORY_AMBIENT_ONCE",
                "VICTORY_AMBIENT_SPECULAR",
            ],
        ),
        (
            "Overrides",
            &[
                "VICTORY_NATIVE_DEFAULT",
                "VICTORY_DLSS_QUALITY",
                "VICTORY_FFX_SCALE",
                "VICTORY_UPSCALER",
                "VICTORY_DLSS_REACTIVE",
                "VICTORY_HDR_STANDIN",
            ],
        ),
    ],
)];

/// The heading of a sectioned tab's remaining settings.
pub const OTHER: &str = "Other";

/// `group`'s sections, if its tab has them.
pub fn sections(group: &str) -> Option<Sections> {
    SECTIONS.iter().find(|(g, _)| *g == group).map(|&(_, s)| s)
}

/// Values that read better by name, in the order the menu steps through them.
const NAMED: &[(&str, &[(&str, &str)])] = &[
    (
        "VICTORY_UPSCALE",
        &[
            ("off", "Off"),
            ("stretch", "Stretch"),
            ("dlss", "DLSS"),
        ],
    ),
    (
        "VICTORY_UPSCALE_QUALITY",
        &[
            ("ultra_performance", "Ultra performance"),
            ("performance", "Performance"),
            ("balanced", "Balanced"),
            ("quality", "Quality"),
            ("dlaa", "DLAA"),
        ],
    ),
    (
        "VICTORY_RESOLUTION",
        &[
            ("1280x720", "1280 x 720"),
            ("1664x936", "1664 x 936"),
            ("1920x1080", "1920 x 1080"),
            ("2560x1440", "2560 x 1440"),
            ("3840x2160", "3840 x 2160"),
            ("native", "Native"),
        ],
    ),
    (
        "VICTORY_UPSCALER",
        &[("6", "Title's own"), ("7", "FidelityFX")],
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

/// The value an unset `name` steps to first, either way (else the first named one).
pub fn first_pick(name: &str) -> Option<&'static str> {
    match name {
        "VICTORY_UPSCALE_QUALITY" => Some(victory_core::render_plan::DEFAULT_QUALITY),
        "VICTORY_RESOLUTION" => Some("native"),
        _ => None,
    }
}

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
        for (group, sections) in SECTIONS {
            let mut seen = std::collections::BTreeSet::new();
            for (_, names) in *sections {
                for name in *names {
                    assert_eq!(info(name).group, *group, "{name}");
                    assert!(seen.insert(*name), "{name} is in two sections");
                }
            }
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
