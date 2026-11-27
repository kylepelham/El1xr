//! Shared plumbing for every VIctory crate and tool.
//!
//! * [`config`]: where the user's files live (never inside the repository).
//! * [`policy`]: the runtime half of rule #1, refusing to write
//!   game-derived data into the source tree.
//! * [`logging`]: one tracing setup for all binaries.
//! * [`settings`]: every `VICTORY_*` setting, the settings file, live changes.

pub mod config;
pub mod heapcount;
pub mod logging;
pub mod policy;
pub mod settings;

pub use config::Config;
pub use policy::{PolicyError, ensure_outside_repo};

/// Set once the title is through its loading (no bake of character skin
/// for a while: `victory-gpu`'s `load_rate`); the loader then gives the
/// title its playing frame rate (`VICTORY_LOAD_FPS`, avatarmanz PR #449).
pub static LOADING_OVER: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
