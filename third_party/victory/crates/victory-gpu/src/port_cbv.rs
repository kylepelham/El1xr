//! Renderer port #5: the title's constant buffers read by the card where the
//! title has them (`VICTORY_PORT_CBV=1`, off by default; read in [`on`]).
//!
//! What the console allows: a constant buffer anywhere on 16 bytes. What
//! D3D12 asks: a constant buffer view on 256. So the layer made every
//! constant buffer a draw's tables name afresh at each submission: a copy of
//! its bytes (out of title memory, on the CPU) into upload memory of its own
//! on 256, and a view of that ([`crate::draw`]'s tables, `defer_constants`).
//! With renderer port 1 (`VICTORY_PORT_CONSTANTS=1`) the memory they are
//! copied out of is the card's to read in place already.
//!
//! What is done here instead (NVIDIA-clean, no raw twin heap): shaders are
//! translated to read constant buffers as raw bytes
//! (`victory_shader::emit::constants_raw`): a table's directly through its
//! descriptor slot, a root one as a root SRV. A raw view starts on 16 bytes,
//! a root SRV on 4. So a constant buffer of a table in title memory in place
//! is made a raw view of the title's bytes directly in its slot, and nothing
//! is copied or done at submission; a root one is bound at its address as
//! root SRVs are. Constant buffers anywhere else are copied as before and
//! viewed as raw bytes directly in their slot. A title slot's constant buffer
//! view is likewise made a raw view of its bytes.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// Whether renderer port #5 is on: the shaders' side
/// (`victory_shader::emit::constants_raw`) and the host's views follow the
/// same switch.
pub(crate) fn on() -> bool {
    victory_shader::emit::constants_raw()
}

/// Whether a table's constant buffer in title memory in place is given to the
/// card where it is (on with the port; `VICTORY_PORT_CBV_PLACE=0`: copied as
/// before, and read as raw bytes all the same).
pub(crate) fn in_place_on() -> bool {
    static PLACE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    on() && *PLACE
        .get_or_init(|| std::env::var_os("VICTORY_PORT_CBV_PLACE").is_none_or(|v| v != "0"))
}

/// Since the last line: tables' constant buffers given in place, their bytes;
/// copied as before because not in place, because not on 16 bytes; title
/// slots' constant buffers made as raw views.
static STATS: [AtomicU64; 5] = [const { AtomicU64::new(0) }; 5];

/// A table's constant buffer of `size` bytes was given in place.
pub(crate) fn in_place(size: u32) {
    STATS[0].fetch_add(1, Relaxed);
    STATS[1].fetch_add(u64::from(size), Relaxed);
}

/// A table's constant buffer was copied as before: `aligned` false when it
/// is in place but not on 16 bytes.
pub(crate) fn copied(aligned: bool) {
    STATS[if aligned { 2 } else { 3 }].fetch_add(1, Relaxed);
}

/// A title slot's constant buffer view was made as a raw view.
pub(crate) fn twinned() {
    STATS[4].fetch_add(1, Relaxed);
}

/// Whether a constant buffer at host address `gpu` can be a raw view of
/// those bytes: on 16 bytes.
pub(crate) fn raw_ok(gpu: u64) -> bool {
    gpu.is_multiple_of(16)
}

/// For the frame-rate line.
pub(crate) fn log(frames: f64) {
    if !on() {
        return;
    }
    let s: Vec<f64> = STATS
        .iter()
        .map(|s| s.swap(0, Relaxed) as f64 / frames.max(1e-9))
        .collect();
    tracing::info!(
        "renderer port 5 (VICTORY_PORT_CBV=1) per frame: {:.0} tables' constant buffers read by the card in place ({:.1} KB not copied, \
         no step at submission); copied as before: {:.0} not in place, {:.0} not on 16 bytes; {:.0} title slots' constant buffers as raw views",
        s[0],
        s[1] / 1024.0,
        s[2],
        s[3],
        s[4],
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_views_need_sixteen_bytes() {
        assert!(raw_ok(0x3500_0010));
        assert!(raw_ok(0x3500_0100));
        assert!(!raw_ok(0x3500_0008));
        assert!(!raw_ok(0x3500_0004));
    }
}
