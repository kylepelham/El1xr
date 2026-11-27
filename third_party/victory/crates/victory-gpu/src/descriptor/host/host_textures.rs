//! (trustno1 `5dda41d`) `VICTORY_QUEUE_HOST_TEXTURES=0`: a slot's record of a
//! texture object the host renders to or writes (a target, depth or
//! unordered access texture: not [`crate::resource::title_fills`]) leaves the
//! queue of textures picked by index, as a buffer view's does. Unset: it is
//! queued as any texture, as before.
//!
//! The pass over that queue takes such a slot, finds nothing to bring up to
//! date (`draw.rs` `heap_textures`: "a texture object the host renders to or
//! writes holds the host's own contents") and takes it out. The title writes
//! such views into its tables' slots every frame, so they come back, each
//! time a take of the queue's lock and a place among the slots due (upstream:
//! 58,978 of 90,699 slots the pass took in 5 s of a cutscene found nothing to
//! do). Not behaviour-neutral: fewer slots are due, so a pass reaches others
//! sooner, the catch-up budget (by the count queued) starts less often, and
//! the queue's write count, which the bake pass asks, moves less.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use victory_core::settings::Live;

/// Whether records of textures the host writes are queued (unset or
/// anything but `0`).
pub(crate) fn queued() -> bool {
    static ON: Live = Live::new("VICTORY_QUEUE_HOST_TEXTURES", |v| {
        u64::from(v.is_none_or(|v| v.trim() != "0"))
    });
    ON.get_bool()
}

/// Records left out since the last line.
static LEFT_OUT: AtomicU64 = AtomicU64::new(0);

/// Whether a slot's record of texture object `object` goes into the queue.
pub(super) fn queues(object: crate::object::This) -> bool {
    keeps(queued(), || crate::resource::title_fills(object))
}

fn keeps(queued: bool, title_fills: impl FnOnce() -> bool) -> bool {
    if queued || title_fills() {
        return true;
    }
    LEFT_OUT.fetch_add(1, Relaxed);
    false
}

/// Logs [`LEFT_OUT`] on the frame-meter cadence while the switch is on, and
/// starts it over.
pub(crate) fn log(frames: f64) {
    let n = LEFT_OUT.swap(0, Relaxed);
    if queued() || frames <= 0.0 {
        return;
    }
    tracing::info!(
        "records of textures the host writes left out of the queue of textures picked by index (VICTORY_QUEUE_HOST_TEXTURES=0): \
         {:.1} per frame",
        n as f64 / frames
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_switch_is_live() {
        victory_core::settings::check_live("VICTORY_QUEUE_HOST_TEXTURES", "0", queued, true, false);
    }

    /// Unset, every texture record is queued without asking what it is;
    /// with the switch, only those the title fills.
    #[test]
    fn only_textures_the_title_fills_are_queued_with_the_switch() {
        assert!(keeps(true, || panic!("asked")));
        assert!(keeps(false, || true));
        assert!(!keeps(false, || false));
    }
}
