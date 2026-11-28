//! A draw's slot mirror makes its texture's host copy before it locks the
//! heap's slot records (`VICTORY_MIRROR_COPY_FIRST=1`; off by default).
//!
//! Ported from JozefDeTremmerie/CCS03 `4e6864c0ea`. Making a host copy is a
//! call into the graphics kernel, a quarter of a millisecond to
//! milliseconds; [`super::make`] did it with `HostHeap::made` held, so
//! while the world streamed in every other thread mirroring a slot of the
//! heap stood waiting (CCS03 measured 450 ms a frame of waiting for that
//! lock on the recording threads in a cutscene's first seconds). With the
//! switch the copy is made first, and `make` only looks it up. Same views,
//! same slots: only where the copy is made moves.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use crate::descriptor::View;

static ON: victory_core::settings::Live =
    victory_core::settings::Live::new("VICTORY_MIRROR_COPY_FIRST", |v| u64::from(parse(v)));

/// Copies made ahead of the lock that took long enough to be a make
/// ([`SLOW_NS`] or more), and their total time.
static SLOW: AtomicU64 = AtomicU64::new(0);
static SLOW_NS_TOTAL: AtomicU64 = AtomicU64::new(0);
/// A lookup of a copy already made takes microseconds; a make, a quarter
/// of a millisecond or more.
const SLOW_NS: u64 = 100_000;
/// Every this many slow copies, one line.
const LOG_EVERY: u64 = 256;

fn parse(value: Option<&str>) -> bool {
    value.is_some_and(|v| v.trim() == "1")
}

/// Whether a mirror should make the copy before the lock: the switch is
/// on, a draw reads the slot, and the slot does not show this view yet.
pub(super) fn wanted(on: bool, for_draw: bool, shown: u64, key: u64) -> bool {
    on && for_draw && shown != key
}

pub(super) fn on() -> bool {
    ON.get_bool()
}

/// The texture object whose host copy [`super::make`] would make for
/// `view`; `None` for constant buffers, samplers and placed buffer views.
fn texture_of(view: &View) -> Option<crate::object::This> {
    match view {
        &View::Shader(r, _) | &View::Unordered(r, _, _) => Some(r as crate::object::This),
        View::Placed(false, ..) => match super::placed_texture(view) {
            Some(View::Shader(r, _)) => Some(r as crate::object::This),
            _ => None,
        },
        _ => None,
    }
}

/// Makes the host copy of the texture `view` shows, if it has none yet.
/// Buffers and other views: nothing (`hosted` answers `None` for them).
pub(super) fn make_copy(view: &View) {
    let Some(texture) = texture_of(view) else {
        return;
    };
    let start = std::time::Instant::now();
    let _ = crate::resource::host_copy(texture);
    let ns = u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX);
    if ns >= SLOW_NS {
        let total = SLOW_NS_TOTAL.fetch_add(ns, Relaxed) + ns;
        let slow = SLOW.fetch_add(1, Relaxed) + 1;
        if slow == 1 || slow.is_multiple_of(LOG_EVERY) {
            tracing::info!(
                slow,
                total_ms = total / 1_000_000,
                "copy first: host copies made before the heap lock (VICTORY_MIRROR_COPY_FIRST=1; CCS03 4e6864c0ea)"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_draw_on_a_changed_slot_with_the_switch() {
        assert!(wanted(true, true, 0, 7));
        assert!(wanted(true, true, 3, 7));
        assert!(!wanted(true, true, 7, 7));
        assert!(!wanted(true, false, 0, 7));
        assert!(!wanted(false, true, 0, 7));
    }

    #[test]
    fn switch_reads_one_only() {
        assert!(parse(Some("1")));
        assert!(parse(Some(" 1 ")));
        assert!(!parse(Some("0")));
        assert!(!parse(None));
    }

    #[test]
    fn copies_only_for_texture_views() {
        assert_eq!(
            texture_of(&View::Shader(0x1000, None)),
            Some(0x1000 as crate::object::This)
        );
        assert_eq!(
            texture_of(&View::Unordered(0x2000, 0, None)),
            Some(0x2000 as crate::object::This)
        );
        assert_eq!(texture_of(&View::Constant(0x3000, 256)), None);
        assert_eq!(texture_of(&View::Sampler([0; 13])), None);
        assert_eq!(texture_of(&View::Placed(true, [0; 14], [0; 16])), None);
    }
}
