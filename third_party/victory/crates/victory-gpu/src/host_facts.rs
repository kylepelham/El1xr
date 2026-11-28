//! (bootfast) The host GPU's facts the shader translator keys its cache on
//! (`crate::shader_cache`): the most lanes of a wave and the view heap's
//! null slot, which a run learns from its D3D12 device. An offline warm-up
//! that only translates (`victory warm --no-pipelines` with
//! `VICTORY_HOST_WAVE` and `VICTORY_VIEW_LIMIT`, as `dev.py prewarm` runs
//! it) gives them here instead, so it files every translation under the key
//! a run on that GPU looks for without making a device on the card.

use std::sync::atomic::{AtomicBool, Ordering::Relaxed};

static ASSUMED: AtomicBool = AtomicBool::new(false);

/// From now on the translator takes `max_wave` and `view_null_slot` as the
/// host's, and no device is made to find the view heap's size.
pub fn assume_host_facts(max_wave: u32, view_null_slot: u32) {
    victory_shader::emit::set_host_max_wave(max_wave);
    victory_shader::emit::set_view_heap_limit(view_null_slot);
    #[cfg(windows)]
    crate::descriptor::host::heap_cap::tell_translator(view_null_slot);
    ASSUMED.store(true, Relaxed);
}

/// Whether [`assume_host_facts`] gave them.
pub(crate) fn assumed() -> bool {
    ASSUMED.load(Relaxed)
}

/// Shaders found in the translator's cache and translated since the start
/// (for `victory warm`'s summary; `shader_cache.rs` is part of the cache
/// key, so it is left as it is).
#[cfg(windows)]
pub fn shader_cache_counts() -> (u64, u64) {
    crate::shader_cache::counts()
}

/// With `VICTORY_SHADER_PACK=1`: waits until what was queued for the shader
/// pack is in it (the end of `victory warm`).
#[cfg(windows)]
pub fn shader_pack_sync() {
    crate::shader_pack::sync();
}
