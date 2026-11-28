//! Every `VICTORY_*` setting in one table, with what the in-game menu needs
//! to show and change it (`ingame_tuning.md`).
//!
//! A setting's value comes from, highest first: the environment (the
//! launcher, `dev.py`), the settings file, the setting's default.
//!
//! The settings file is `%LOCALAPPDATA%\victory\settings.toml`, or the path
//! in `VICTORY_SETTINGS_FILE` (`none`: no file, as `dev.py`'s runs use). It
//! holds flat `VICTORY_NAME = "value"` lines. [`init`], first thing in
//! `main`, copies its values into the environment for every setting the
//! environment does not set, so every reader (and the shader cache key)
//! sees them as before; without a file nothing changes.
//!
//! Settings read once at start are [`Apply::Restart`]; those in the shader
//! or pipeline cache key [`Apply::Retranslate`]; [`Apply::Live`] ones are
//! read through a [`Live`] cell, which re-reads its value after [`set`].

mod expert;
pub mod ray_tracing;
pub mod schedule;
mod table;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

/// When a change takes effect.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Apply {
    /// Within a frame or two.
    Live,
    /// At the next start: read once.
    Restart,
    /// At the next start, and the shaders or pipelines it changes are made
    /// again: it is part of the shader or pipeline cache key.
    Retranslate,
}

/// The values a setting takes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Kind {
    /// `0` or `1`.
    Bool,
    Int {
        min: i64,
        max: i64,
    },
    Float {
        min: f64,
        max: f64,
        step: f64,
    },
    Choice(&'static [&'static str]),
    /// Anything (lists, paths, hashes).
    Text,
}

/// One setting.
#[derive(Clone, Copy, Debug)]
pub struct Info {
    pub name: &'static str,
    /// `Display`, `Memory`, `Streaming`, `Threads`, `Submission`, `Shaders`,
    /// `Fixes`, `Diagnostics`, `Launch` or `Experimental`.
    pub group: &'static str,
    pub label: &'static str,
    pub help: &'static str,
    pub kind: Kind,
    /// The value the code uses when the variable is not set; empty when it
    /// is the code's own built-in behaviour with no single value.
    pub default: &'static str,
    pub apply: Apply,
    /// Shown only behind the menu's expert toggle.
    pub expert: bool,
}

/// Variables of the tree that are not settings: build-time values, test
/// names and process plumbing between our own processes.
pub const NOT_SETTINGS: &[&str] = &[
    "VICTORY_BUILD",
    "VICTORY_SHADER_SOURCE_HASH",
    "VICTORY_SHADER_SWITCHES",
    "VICTORY_TRANSLATOR",
    "VICTORY_A",
    "VICTORY_B",
    "VICTORY_RELAUNCHED_FROM",
    "VICTORY_AUDIO_CAPTURE_OF",
    "VICTORY_TEST_EXIT_PROCESS_CHILD",
    "VICTORY_PREPARE_DEDUP_CHILD",
    "VICTORY_STREAM_AHEAD_CHILD",
    "VICTORY_SETTINGS_SCHEDULE",
];

/// The variable naming the settings file (`none`: no file).
pub const FILE_VARIABLE: &str = "VICTORY_SETTINGS_FILE";

/// The variable naming a timeline of live changes ([`schedule`]).
pub const SCHEDULE_VARIABLE: &str = "VICTORY_SETTINGS_SCHEDULE";

/// When the process started: the first call, which [`init`] makes first
/// thing in `main`.
pub fn started() -> Instant {
    static STARTED: OnceLock<Instant> = OnceLock::new();
    *STARTED.get_or_init(Instant::now)
}

/// Every setting: the described ones first, then the rest as expert
/// entries named after their variable.
pub fn infos() -> &'static [Info] {
    static ALL: OnceLock<Vec<Info>> = OnceLock::new();
    ALL.get_or_init(|| {
        let mut all: Vec<Info> = table::TABLE.to_vec();
        all.extend(expert::EXPERT.iter().map(|&(name, apply)| Info {
            name,
            group: "Experimental",
            label: name,
            help: "",
            kind: Kind::Text,
            default: "",
            apply,
            expert: true,
        }));
        all
    })
}

/// The setting called `name`.
pub fn info(name: &str) -> Option<&'static Info> {
    static INDEX: OnceLock<BTreeMap<&'static str, usize>> = OnceLock::new();
    let index = INDEX.get_or_init(|| {
        infos()
            .iter()
            .enumerate()
            .map(|(i, s)| (s.name, i))
            .collect()
    });
    index.get(name).map(|&i| &infos()[i])
}

/// What [`set`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Applied {
    Now,
    OnRestart,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SetError {
    #[error("no setting is called {0}")]
    Unknown(String),
    #[error("{name} does not take {value:?}")]
    Invalid { name: String, value: String },
}

struct Store {
    /// The settings file's values, as read at start (or last saved).
    file: BTreeMap<String, String>,
    /// The settings file's values as read at start: what a cleared change
    /// goes back to.
    start: BTreeMap<String, String>,
    /// Changes made while running; an empty value clears the setting back
    /// to its launch value.
    changed: BTreeMap<String, String>,
    hooks: Vec<(&'static str, fn(&str))>,
}

static STORE: Mutex<Store> = Mutex::new(Store {
    file: BTreeMap::new(),
    start: BTreeMap::new(),
    changed: BTreeMap::new(),
    hooks: Vec::new(),
});

/// Bumped by every [`set`] of a live setting: [`Live`] cells read again.
static GENERATION: AtomicU64 = AtomicU64::new(0);

fn store() -> std::sync::MutexGuard<'static, Store> {
    STORE.lock().unwrap_or_else(|e| e.into_inner())
}

/// The settings file in use: `None` for `VICTORY_SETTINGS_FILE=none` or
/// when no user folder is known.
pub fn file_path() -> Option<PathBuf> {
    file_path_from(|k| std::env::var_os(k))
}

fn file_path_from(env: impl Fn(&str) -> Option<std::ffi::OsString>) -> Option<PathBuf> {
    match env(FILE_VARIABLE) {
        Some(v) if v.eq_ignore_ascii_case("none") => None,
        Some(v) if !v.is_empty() => Some(PathBuf::from(v)),
        _ => env("LOCALAPPDATA")
            .map(|base| PathBuf::from(base).join("victory").join("settings.toml")),
    }
}

/// What [`init`] found, for the log once logging is up.
#[derive(Debug, Default)]
pub struct Loaded {
    pub path: Option<PathBuf>,
    /// Values taken from the file (the environment did not set them).
    pub applied: Vec<String>,
    /// Values of the file the environment overrides.
    pub overridden: Vec<String>,
    /// Lines of the file that were not used, and why.
    pub problems: Vec<String>,
    /// What `VICTORY_RAY_TRACING=on` added ([`ray_tracing`]).
    pub ray_tracing: Option<String>,
}

impl Loaded {
    /// The lines to log once logging is up; none when there was no file.
    pub fn lines(&self) -> Vec<String> {
        let mut out: Vec<String> = self.ray_tracing.iter().cloned().collect();
        let Some(path) = &self.path else {
            return out;
        };
        if !self.applied.is_empty() || !self.overridden.is_empty() {
            out.push(format!(
                "settings: {} values from {} ({}), {} set by the environment instead ({})",
                self.applied.len(),
                path.display(),
                self.applied.join(", "),
                self.overridden.len(),
                self.overridden.join(", ")
            ));
        }
        out.extend(
            self.problems
                .iter()
                .map(|p| format!("settings: {}: {p}", path.display())),
        );
        out
    }
}

/// Reads the settings file and puts its values into the environment for
/// the settings the environment leaves unset, then what
/// `VICTORY_RAY_TRACING=on` adds ([`ray_tracing`]). Call it first in `main`,
/// before any thread starts. Without a file and that switch nothing changes.
pub fn init() -> Loaded {
    let mut loaded = read_file();
    let value = std::env::var(ray_tracing::NAME).ok();
    let added = ray_tracing::expansion(value.as_deref(), |name| std::env::var_os(name).is_some());
    for (name, v) in &added {
        // SAFETY: as in `read_file`: first in `main`, before any other thread.
        unsafe { std::env::set_var(name, v) };
    }
    loaded.ray_tracing = ray_tracing::line(value.as_deref(), &added);
    loaded
}

fn read_file() -> Loaded {
    started();
    let Some(path) = file_path() else {
        return Loaded::default();
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Loaded::default(),
        Err(e) => {
            return Loaded {
                problems: vec![format!("not read: {e}")],
                path: Some(path),
                ..Loaded::default()
            };
        }
    };
    let (file, problems) = parse_file(&text);
    let (applied, overridden) = split(&file, |name| std::env::var_os(name).is_some());
    for name in &applied {
        // SAFETY: called first in `main`, before any other thread runs
        // or reads the environment.
        unsafe { std::env::set_var(name, &file[name]) };
    }
    let loaded = Loaded {
        path: Some(path),
        applied,
        overridden,
        problems,
        ray_tracing: None,
    };
    let mut s = store();
    s.start = file.clone();
    s.file = file;
    drop(s);
    loaded
}

/// The settings whose value came from the settings file: those `init` put
/// into the environment, and those the environment holds with the file's
/// value (the launcher copies the file into it first).
pub fn from_file() -> BTreeSet<String> {
    let start = store().start.clone();
    file_values_in_env(&start, |name| std::env::var(name).ok())
}

fn file_values_in_env(
    file: &BTreeMap<String, String>,
    env: impl Fn(&str) -> Option<String>,
) -> BTreeSet<String> {
    file.iter()
        .filter(|(name, value)| env(name).is_none_or(|v| v == **value))
        .map(|(name, _)| name.clone())
        .collect()
}

/// The file's names the environment leaves unset (to put there) and those
/// it sets (the environment wins).
fn split(
    file: &BTreeMap<String, String>,
    in_env: impl Fn(&str) -> bool,
) -> (Vec<String>, Vec<String>) {
    file.keys().cloned().partition(|name| !in_env(name))
}

/// The file's `VICTORY_*` values, and what was left out. Booleans become
/// `1`/`0`, numbers their text.
fn parse_file(text: &str) -> (BTreeMap<String, String>, Vec<String>) {
    let mut out = BTreeMap::new();
    let mut problems = Vec::new();
    let table: toml::Table = match toml::from_str(text) {
        Ok(t) => t,
        Err(e) => return (out, vec![format!("not TOML, ignored: {e}")]),
    };
    for (key, value) in table {
        if !is_variable(&key) {
            problems.push(format!("{key}: not a VICTORY_* name, ignored"));
            continue;
        }
        let text = match value {
            toml::Value::String(s) => s,
            toml::Value::Integer(i) => i.to_string(),
            toml::Value::Float(f) => f.to_string(),
            toml::Value::Boolean(b) => if b { "1" } else { "0" }.to_string(),
            other => {
                problems.push(format!(
                    "{key}: a {} is not a value, ignored",
                    other.type_str()
                ));
                continue;
            }
        };
        if info(&key).is_none() {
            problems.push(format!("{key}: not a setting of this build (kept)"));
        }
        out.insert(key, text);
    }
    (out, problems)
}

fn is_variable(name: &str) -> bool {
    name.strip_prefix("VICTORY_").is_some_and(|rest| {
        !rest.is_empty()
            && rest
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
    })
}

/// The value `name` has had since start: the environment (with the file's
/// values in it), else the default; `None` when unset with no default.
pub fn launched(name: &str) -> Option<String> {
    std::env::var(name).ok().or_else(|| {
        info(name)
            .map(|i| i.default)
            .filter(|d| !d.is_empty())
            .map(str::to_string)
    })
}

/// The value chosen for `name`: a change made while running, else
/// [`launched`]. For a live setting that is the value in effect; for the
/// others the one the next start takes.
pub fn get(name: &str) -> Option<String> {
    let changed = store().changed.get(name).cloned();
    match changed {
        Some(v) if !v.is_empty() => Some(v),
        _ => launched(name),
    }
}

/// What a [`Live`] reader parses: a change made while running, else the
/// environment as a `std::env::var` reader would see it.
fn live_value(name: &str) -> Option<String> {
    let changed = store().changed.get(name).cloned();
    match changed {
        Some(v) if !v.is_empty() => Some(v),
        _ => std::env::var(name).ok(),
    }
}

/// What `std::env::var(name).ok()` gives a reader, with a change of a live
/// setting made while running on top: for the builders of [`Derived`]
/// values.
pub fn live_var(name: &str) -> Option<String> {
    live_value(name)
}

/// When a change of `name` takes effect: live when its table entry says
/// so or a hook ([`on_change`]) applies it.
pub fn apply_of(name: &str) -> Option<Apply> {
    let info = info(name)?;
    if info.apply == Apply::Live || store().hooks.iter().any(|(n, _)| *n == name) {
        Some(Apply::Live)
    } else {
        Some(info.apply)
    }
}

/// Whether `value` is one `kind` takes.
pub fn valid(kind: Kind, value: &str) -> bool {
    match kind {
        Kind::Bool => matches!(value, "0" | "1"),
        Kind::Int { min, max } => value
            .trim()
            .parse::<i64>()
            .is_ok_and(|v| (min..=max).contains(&v)),
        Kind::Float { min, max, .. } => value
            .trim()
            .parse::<f64>()
            .is_ok_and(|v| v >= min && v <= max),
        Kind::Choice(choices) => choices.contains(&value),
        Kind::Text => true,
    }
}

/// Changes `name` to `value` (empty: back to its launch value). A live
/// setting takes it within a frame or two (and its hooks run); the others
/// at the next start ([`pending_restart`]). Kept in the file by [`save`].
pub fn set(name: &str, value: &str) -> Result<Applied, SetError> {
    let info = info(name).ok_or_else(|| SetError::Unknown(name.to_string()))?;
    if !value.is_empty() && !valid(info.kind, value) {
        return Err(SetError::Invalid {
            name: name.to_string(),
            value: value.to_string(),
        });
    }
    let live = apply_of(name) == Some(Apply::Live);
    let hooks: Vec<fn(&str)> = {
        let mut s = store();
        s.changed.insert(name.to_string(), value.to_string());
        s.hooks
            .iter()
            .filter(|(n, _)| *n == name)
            .map(|&(_, f)| f)
            .collect()
    };
    if !live {
        return Ok(Applied::OnRestart);
    }
    GENERATION.fetch_add(1, Ordering::SeqCst);
    let now = get(name).unwrap_or_default();
    for hook in hooks {
        hook(&now);
    }
    Ok(Applied::Now)
}

/// Settings changed while running that wait for a restart.
pub fn pending_restart() -> Vec<&'static str> {
    let changed: Vec<(String, String)> = store()
        .changed
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    changed
        .into_iter()
        .filter_map(|(name, _)| {
            let info = info(&name)?;
            (apply_of(&name) != Some(Apply::Live) && get(&name) != launched(&name))
                .then_some(info.name)
        })
        .collect()
}

/// Calls `f` with the new value after every [`set`] of `name`, which then
/// applies at once (for settings whose change needs work at a safe point,
/// such as the DLSS mode).
pub fn on_change(name: &'static str, f: fn(&str)) {
    store().hooks.push((name, f));
}

/// Writes the settings file: the file's values with this run's changes on
/// top (a cleared setting goes back to the file's value at start, or leaves
/// the file). Only values someone chose are written, never the launcher's or
/// the defaults.
pub fn save() -> std::io::Result<()> {
    let path = file_path().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "no settings file (VICTORY_SETTINGS_FILE=none)",
        )
    })?;
    let merged = {
        let s = store();
        merged(&s.file, &s.start, &s.changed)
    };
    write_file(&path, &merged)?;
    store().file = merged;
    Ok(())
}

fn merged(
    file: &BTreeMap<String, String>,
    start: &BTreeMap<String, String>,
    changed: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let mut out = file.clone();
    for (k, v) in changed {
        if v.is_empty() {
            match start.get(k) {
                Some(was) => out.insert(k.clone(), was.clone()),
                None => out.remove(k),
            };
        } else {
            out.insert(k.clone(), v.clone());
        }
    }
    out
}

fn render(values: &BTreeMap<String, String>) -> String {
    let mut text = String::from(
        "# El1xr settings (VIctory). One VICTORY_* setting per line; the environment\n\
         # (the launcher's -Override, dev.py) wins over this file.\n",
    );
    for (k, v) in values {
        let escaped = v.replace('\\', "\\\\").replace('"', "\\\"");
        text.push_str(&format!("{k} = \"{escaped}\"\n"));
    }
    text
}

fn write_file(path: &Path, values: &BTreeMap<String, String>) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, render(values))?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// A live setting's parsed value, kept in an atomic: [`Live::get`] costs two
/// atomic loads, and parses again after a [`set`]. The parse function gets
/// what `std::env::var(name).ok()` gave the reader it replaces (or the
/// value set while running) and returns the value as `u64` bits (an `f64`
/// through `to_bits`).
pub struct Live {
    name: &'static str,
    parse: fn(Option<&str>) -> u64,
    /// The generation the value was parsed at, plus one (0: never).
    seen: AtomicU64,
    bits: AtomicU64,
}

impl Live {
    pub const fn new(name: &'static str, parse: fn(Option<&str>) -> u64) -> Self {
        Live {
            name,
            parse,
            seen: AtomicU64::new(0),
            bits: AtomicU64::new(0),
        }
    }

    /// A switch on only when its value is exactly `1`: what
    /// `std::env::var_os(name).is_some_and(|v| v == "1")` read.
    pub const fn flag(name: &'static str) -> Self {
        Live::new(name, flag_bits)
    }

    pub fn name(&self) -> &'static str {
        self.name
    }

    #[inline]
    pub fn get(&self) -> u64 {
        let generation = GENERATION.load(Ordering::Acquire) + 1;
        if self.seen.load(Ordering::Acquire) == generation {
            return self.bits.load(Ordering::Relaxed);
        }
        let bits = (self.parse)(live_value(self.name).as_deref());
        self.bits.store(bits, Ordering::Relaxed);
        self.seen.store(generation, Ordering::Release);
        bits
    }

    pub fn get_bool(&self) -> bool {
        self.get() != 0
    }

    pub fn get_f64(&self) -> f64 {
        f64::from_bits(self.get())
    }
}

/// For the tests of a reader made live: `name` is live and a schedule takes
/// it; `read` (the reader on its hot path) gives `launch` as launched, then
/// `changed` after `set(name, value)`, then `launch` again once cleared.
/// Skipped (false) when the environment sets `name`.
#[doc(hidden)]
pub fn check_live<T: PartialEq + std::fmt::Debug>(
    name: &str,
    value: &str,
    read: impl Fn() -> T,
    launch: T,
    changed: T,
) -> bool {
    if std::env::var_os(name).is_some() {
        return false;
    }
    assert_eq!(apply_of(name), Some(Apply::Live), "{name}");
    let line = format!("at 1s set {name}={value}\n");
    assert!(schedule::parse(&line).is_ok(), "a schedule refuses {line}");
    assert_eq!(read(), launch, "{name} as launched");
    assert_eq!(set(name, value), Ok(Applied::Now), "{name}={value}");
    assert_eq!(read(), changed, "{name}={value}");
    assert_eq!(set(name, ""), Ok(Applied::Now));
    assert_eq!(read(), launch, "{name} cleared");
    true
}

fn flag_bits(value: Option<&str>) -> u64 {
    u64::from(value == Some("1"))
}

/// A value built from live settings (read through [`live_var`]) that does
/// not fit a [`Live`] cell's 64 bits: built at its first use and again after
/// a [`set`] of a live setting. [`Derived::get`] takes a lock, so keep it off
/// paths run per draw; a [`Live`] cell is lock-free.
pub struct Derived<T: Copy> {
    make: fn() -> T,
    /// The generation the value was built at, plus one (0: never).
    seen: AtomicU64,
    value: Mutex<Option<T>>,
}

impl<T: Copy> Derived<T> {
    pub const fn new(make: fn() -> T) -> Self {
        Derived {
            make,
            seen: AtomicU64::new(0),
            value: Mutex::new(None),
        }
    }

    pub fn get(&self) -> T {
        let generation = GENERATION.load(Ordering::Acquire) + 1;
        let mut value = self.value.lock().unwrap_or_else(|e| e.into_inner());
        if self.seen.load(Ordering::Relaxed) == generation
            && let Some(v) = *value
        {
            return v;
        }
        let made = (self.make)();
        *value = Some(made);
        self.seen.store(generation, Ordering::Relaxed);
        made
    }
}

#[cfg(test)]
mod tests;
