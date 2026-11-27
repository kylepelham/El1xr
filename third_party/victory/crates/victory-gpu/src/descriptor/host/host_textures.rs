//! (readmet3xt-lab, trustno1-coder/VIctory-NVIDIA `5dda41d`)
//! `VICTORY_QUEUE_HOST_TEXTURES=0`: a slot's record of a texture object the
//! host renders to or writes (a target, depth or unordered access texture:
//! not `resource::title_fills`) leaves the queue of textures picked by index,
//! as a buffer view's does, instead of being queued as any texture.
//!
//! The pass takes such a slot and finds nothing to bring up to date
//! (`draw.rs` `heap_textures`: "A texture object the host renders to or
//! writes holds the host's own contents"); the title writes such views into
//! its tables' slots every frame, so they come back each time, take the
//! queue's lock and stand among the slots due (upstream: 58,978 of 90,699
//! slots the pass took in 5 s of a cutscene found nothing to do). With the
//! switch the slots due are fewer, so a pass reaches streamed textures
//! sooner, the catch-up budget (by the count due) starts less often, and a
//! bake's pass is asked less often (it runs when the queue's write count
//! changed). Unset: queued, as before.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// Records of textures the host writes left out of the queue since the
/// start.
static LEFT_OUT: AtomicU64 = AtomicU64::new(0);

fn left_out_on() -> bool {
    static ON: victory_core::settings::Live =
        victory_core::settings::Live::new("VICTORY_QUEUE_HOST_TEXTURES", |v| {
            u64::from(v.map(str::trim) == Some("0"))
        });
    ON.get_bool()
}

/// Whether a slot written with a view of texture object `this` is queued
/// for the pass over textures picked by index.
pub(super) fn queued(this: crate::object::This) -> bool {
    queued_as(left_out_on(), || crate::resource::title_fills(this))
}

fn queued_as(left_out: bool, title_fills: impl FnOnce() -> bool) -> bool {
    if !left_out || title_fills() {
        return true;
    }
    let n = LEFT_OUT.fetch_add(1, Relaxed) + 1;
    if n.is_power_of_two() && n >= 4096 {
        tracing::info!(
            "records of textures the host writes left out of the texture queue, since the start: {n} (VICTORY_QUEUE_HOST_TEXTURES=0)"
        );
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_switch_is_live() {
        victory_core::settings::check_live(
            "VICTORY_QUEUE_HOST_TEXTURES",
            "0",
            left_out_on,
            false,
            true,
        );
    }

    /// Unset, every texture object's record is queued and nothing is asked
    /// of the object; with the switch, only those the title fills.
    #[test]
    fn only_textures_the_title_fills_are_queued_with_the_switch() {
        assert!(queued_as(false, || panic!(
            "not asked unless the switch is on"
        )));
        assert!(queued_as(true, || true));
        let before = LEFT_OUT.load(Relaxed);
        assert!(!queued_as(true, || false));
        assert!(LEFT_OUT.load(Relaxed) > before);
    }
}
