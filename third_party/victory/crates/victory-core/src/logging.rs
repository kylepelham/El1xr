//! One tracing setup for every binary.
//!
//! Filter with `VICTORY_LOG` (falls back to `RUST_LOG`), e.g.
//! `VICTORY_LOG=victory_loader=trace,info`. Debug and trace output exist
//! only in builds with the `diagnostics` feature; a plain release build
//! logs info and above whatever the filter asks for.

use std::sync::OnceLock;

use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Registry, reload};

/// The filter in use, for the in-game menu changing `VICTORY_LOG`.
static FILTER: OnceLock<reload::Handle<EnvFilter, Registry>> = OnceLock::new();

/// The most verbose level this build can log.
const CAP: LevelFilter = if cfg!(feature = "diagnostics") {
    LevelFilter::TRACE
} else {
    LevelFilter::INFO
};

pub fn init(default_level: &str) {
    let filter = std::env::var("VICTORY_LOG")
        .or_else(|_| std::env::var("RUST_LOG"))
        .ok()
        .and_then(|v| EnvFilter::try_new(v).ok())
        .unwrap_or_else(|| EnvFilter::new(default_level));
    let (filter, handle) = reload::Layer::new(filter);
    let _ = FILTER.set(handle);
    crate::settings::on_change("VICTORY_LOG", refilter);
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(CAP)
        .with(
            tracing_subscriber::fmt::layer()
                .with_target(true)
                .with_writer(std::io::stderr),
        )
        .try_init();
}

/// `VICTORY_LOG` changed while running: the new filter from now on (an
/// invalid one is refused and logged; the old one stays).
fn refilter(value: &str) {
    let Some(handle) = FILTER.get() else { return };
    match EnvFilter::try_new(value) {
        Ok(filter) => {
            let _ = handle.reload(filter);
            tracing::info!(filter = value, "log filter changed (VICTORY_LOG)");
        }
        Err(e) => tracing::warn!(filter = value, error = %e, "log filter refused (VICTORY_LOG)"),
    }
}
