//! (El1xr) What the in-game menu shows and changes: the [`MenuModel`] trait, shaped like the
//! settings registry's API (`victory-core` `settings`, see `ingame_tuning.md`), and a demo model
//! the menu runs on until the registry is wired in.

use std::collections::HashMap;

/// When a change takes effect.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Apply {
    /// Within a frame or two.
    Live,
    /// Read once at start.
    Restart,
    /// In the shader/pipeline cache key: a restart and a shader rebuild.
    Retranslate,
}

/// A setting's value type and range.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Kind {
    Bool,
    Int { min: i64, max: i64 },
    Float { min: f64, max: f64, step: f64 },
    Choice(&'static [&'static str]),
    Text,
}

/// One setting's metadata.
#[derive(Clone, Copy, Debug)]
pub struct Info {
    pub name: &'static str,
    pub group: &'static str,
    pub label: &'static str,
    pub help: &'static str,
    pub kind: Kind,
    pub default: &'static str,
    pub apply: Apply,
    pub expert: bool,
}

/// What a successful `set` did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Applied {
    Now,
    OnRestart,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SetError {
    Unknown,
    Invalid(String),
}

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
    /// The current effective value.
    fn get(&self, name: &str) -> Option<String>;
    fn set(&mut self, name: &str, value: &str) -> Result<Applied, SetError>;
    /// Changed settings that wait for a restart.
    fn pending_restart(&self) -> Vec<&'static str>;
    /// Writes the settings file.
    fn save(&mut self) -> std::io::Result<()>;
    fn presets(&self) -> &[Preset] {
        &[]
    }
}

/// Whether `value` is valid for `kind`.
pub fn valid(kind: Kind, value: &str) -> bool {
    match kind {
        Kind::Bool => matches!(value, "0" | "1"),
        Kind::Int { min, max } => value.parse::<i64>().is_ok_and(|v| (min..=max).contains(&v)),
        Kind::Float { min, max, .. } => value.parse::<f64>().is_ok_and(|v| v >= min && v <= max),
        Kind::Choice(options) => options.contains(&value),
        Kind::Text => true,
    }
}

const UPSCALERS: &[&str] = &["off", "dlss", "fsr"];
const DLSS_MODES: &[&str] = &[
    "dlaa",
    "quality",
    "balanced",
    "performance",
    "ultra performance",
];
const LOG_LEVELS: &[&str] = &["error", "warn", "info", "debug", "trace"];

/// Demo settings: real switch names, made-up metadata where the registry is not in yet.
pub const DEMO_INFOS: &[Info] = &[
    Info {
        name: "VICTORY_UPSCALE",
        group: "Display",
        label: "Upscaler",
        help: "Off, NVIDIA DLSS, or the title's FSR passed through.",
        kind: Kind::Choice(UPSCALERS),
        default: "off",
        apply: Apply::Live,
        expert: false,
    },
    Info {
        name: "VICTORY_DLSS_MODE",
        group: "Display",
        label: "DLSS quality",
        help: "Render resolution preset for DLSS.",
        kind: Kind::Choice(DLSS_MODES),
        default: "quality",
        apply: Apply::Live,
        expert: false,
    },
    Info {
        name: "VICTORY_RENDER_SCALE",
        group: "Display",
        label: "Render scale",
        help: "Fraction of the output resolution the title renders at.",
        kind: Kind::Float {
            min: 0.33,
            max: 1.0,
            step: 0.01,
        },
        default: "0.67",
        apply: Apply::Live,
        expert: false,
    },
    Info {
        name: "VICTORY_FRAME_GEN",
        group: "Display",
        label: "Frame generation",
        help: "DLSS Frame Generation (one generated frame per rendered one).",
        kind: Kind::Bool,
        default: "0",
        apply: Apply::Restart,
        expert: false,
    },
    Info {
        name: "VICTORY_FRAME_CAP",
        group: "Display",
        label: "Frame cap",
        help: "Highest frame rate presented; 0 is uncapped.",
        kind: Kind::Int { min: 0, max: 240 },
        default: "0",
        apply: Apply::Live,
        expert: false,
    },
    Info {
        name: "VICTORY_OVERLAY",
        group: "Display",
        label: "Performance overlay",
        help: "The FPS panel at the top left (F10 also toggles it).",
        kind: Kind::Bool,
        default: "1",
        apply: Apply::Live,
        expert: false,
    },
    Info {
        name: "VICTORY_ASYNC_SUBMIT",
        group: "Performance",
        label: "Async submit",
        help: "Submit command lists from a worker thread.",
        kind: Kind::Bool,
        default: "1",
        apply: Apply::Restart,
        expert: false,
    },
    Info {
        name: "VICTORY_BUFFER_AHEAD",
        group: "Performance",
        label: "Buffer ahead",
        help: "Upload buffers one frame before the title asks.",
        kind: Kind::Bool,
        default: "1",
        apply: Apply::Restart,
        expert: false,
    },
    Info {
        name: "VICTORY_PIPELINE_RECIPES",
        group: "Performance",
        label: "Pipeline recipes",
        help: "Build pipelines from recorded recipes at boot.",
        kind: Kind::Bool,
        default: "1",
        apply: Apply::Restart,
        expert: false,
    },
    Info {
        name: "VICTORY_LIST_POOL_MB",
        group: "Performance",
        label: "List pool",
        help: "Command list pool size in MB.",
        kind: Kind::Int { min: 64, max: 2048 },
        default: "512",
        apply: Apply::Restart,
        expert: false,
    },
    Info {
        name: "VICTORY_PIPELINE_WAIT_MS",
        group: "Performance",
        label: "Pipeline wait",
        help: "Longest a draw waits for its pipeline, in ms.",
        kind: Kind::Int { min: 0, max: 100 },
        default: "8",
        apply: Apply::Live,
        expert: false,
    },
    Info {
        name: "VICTORY_NAN_SCRUB",
        group: "Performance",
        label: "NaN scrub",
        help: "Replace NaNs in render targets before they spread.",
        kind: Kind::Bool,
        default: "1",
        apply: Apply::Retranslate,
        expert: false,
    },
    Info {
        name: "VICTORY_LOG",
        group: "Diagnostics",
        label: "Log level",
        help: "How much goes into victory.log.",
        kind: Kind::Choice(LOG_LEVELS),
        default: "info",
        apply: Apply::Restart,
        expert: false,
    },
    Info {
        name: "VICTORY_GPU_COST",
        group: "Diagnostics",
        label: "GPU cost",
        help: "Time each pass on the GPU and log the table.",
        kind: Kind::Bool,
        default: "0",
        apply: Apply::Live,
        expert: false,
    },
    Info {
        name: "VICTORY_HANG_WATCH",
        group: "Diagnostics",
        label: "Hang watch",
        help: "Log what the GPU was doing when a frame stalls.",
        kind: Kind::Bool,
        default: "0",
        apply: Apply::Restart,
        expert: false,
    },
    Info {
        name: "VICTORY_WAVE_PAIRS",
        group: "Performance",
        label: "Wave pairs",
        help: "Paired-wave shader lowering (hangs this card; keep 0).",
        kind: Kind::Bool,
        default: "0",
        apply: Apply::Retranslate,
        expert: true,
    },
    Info {
        name: "VICTORY_THREAD_TICKET",
        group: "Performance",
        label: "Thread ticket",
        help: "Ticketed thread ordering (DEVICE_HUNG on this card; keep 0).",
        kind: Kind::Bool,
        default: "0",
        apply: Apply::Retranslate,
        expert: true,
    },
    Info {
        name: "VICTORY_HEAP_SCAN_EVERY",
        group: "Performance",
        label: "Heap scan every",
        help: "Frames between heap scans.",
        kind: Kind::Int { min: 1, max: 600 },
        default: "60",
        apply: Apply::Live,
        expert: true,
    },
    Info {
        name: "VICTORY_DATA_DIR",
        group: "Diagnostics",
        label: "Data folder",
        help: "Where saves and caches go.",
        kind: Kind::Text,
        default: "%LOCALAPPDATA%\\victory",
        apply: Apply::Restart,
        expert: true,
    },
];

const DEMO_PRESETS: &[Preset] = &[
    Preset {
        name: "Quality",
        help: "DLAA, no frame generation, no cap.",
        values: &[
            ("VICTORY_UPSCALE", "dlss"),
            ("VICTORY_DLSS_MODE", "dlaa"),
            ("VICTORY_RENDER_SCALE", "1.00"),
            ("VICTORY_FRAME_GEN", "0"),
        ],
    },
    Preset {
        name: "Balanced",
        help: "DLSS Quality with frame generation.",
        values: &[
            ("VICTORY_UPSCALE", "dlss"),
            ("VICTORY_DLSS_MODE", "quality"),
            ("VICTORY_RENDER_SCALE", "0.67"),
            ("VICTORY_FRAME_GEN", "1"),
        ],
    },
    Preset {
        name: "Performance",
        help: "DLSS Performance with frame generation.",
        values: &[
            ("VICTORY_UPSCALE", "dlss"),
            ("VICTORY_DLSS_MODE", "performance"),
            ("VICTORY_RENDER_SCALE", "0.50"),
            ("VICTORY_FRAME_GEN", "1"),
        ],
    },
];

/// Values in memory: live settings apply at once, the rest are pending until a restart. `save`
/// does nothing but count.
pub struct DemoModel {
    values: HashMap<&'static str, String>,
    started: HashMap<&'static str, String>,
    pub saves: usize,
}

impl Default for DemoModel {
    fn default() -> Self {
        let values: HashMap<_, _> = DEMO_INFOS
            .iter()
            .map(|i| (i.name, i.default.to_string()))
            .collect();
        DemoModel {
            started: values.clone(),
            values,
            saves: 0,
        }
    }
}

impl MenuModel for DemoModel {
    fn infos(&self) -> &[Info] {
        DEMO_INFOS
    }

    fn get(&self, name: &str) -> Option<String> {
        self.values.get(name).cloned()
    }

    fn set(&mut self, name: &str, value: &str) -> Result<Applied, SetError> {
        let info = DEMO_INFOS
            .iter()
            .find(|i| i.name == name)
            .ok_or(SetError::Unknown)?;
        if !valid(info.kind, value) {
            return Err(SetError::Invalid(format!(
                "{value} is not valid for {name}"
            )));
        }
        self.values.insert(info.name, value.to_string());
        Ok(if info.apply == Apply::Live {
            Applied::Now
        } else {
            Applied::OnRestart
        })
    }

    fn pending_restart(&self) -> Vec<&'static str> {
        DEMO_INFOS
            .iter()
            .filter(|i| {
                i.apply != Apply::Live && self.values.get(i.name) != self.started.get(i.name)
            })
            .map(|i| i.name)
            .collect()
    }

    fn save(&mut self) -> std::io::Result<()> {
        self.saves += 1;
        Ok(())
    }

    fn presets(&self) -> &[Preset] {
        DEMO_PRESETS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demo_defaults_are_valid_and_names_unique() {
        for info in DEMO_INFOS {
            assert!(valid(info.kind, info.default), "{}", info.name);
            assert_eq!(DEMO_INFOS.iter().filter(|i| i.name == info.name).count(), 1);
        }
        for preset in DEMO_PRESETS {
            for (name, value) in preset.values {
                let info = DEMO_INFOS.iter().find(|i| i.name == *name).unwrap();
                assert!(valid(info.kind, value), "{name}={value}");
            }
        }
    }

    #[test]
    fn restart_settings_become_pending_and_live_ones_do_not() {
        let mut m = DemoModel::default();
        assert_eq!(m.set("VICTORY_FRAME_CAP", "60"), Ok(Applied::Now));
        assert_eq!(m.set("VICTORY_FRAME_GEN", "1"), Ok(Applied::OnRestart));
        assert_eq!(m.pending_restart(), vec!["VICTORY_FRAME_GEN"]);
        m.set("VICTORY_FRAME_GEN", "0").unwrap();
        assert!(m.pending_restart().is_empty());
        assert_eq!(
            m.set("VICTORY_FRAME_CAP", "999"),
            Err(SetError::Invalid(
                "999 is not valid for VICTORY_FRAME_CAP".into()
            ))
        );
        assert_eq!(m.set("VICTORY_NOPE", "1"), Err(SetError::Unknown));
    }
}
