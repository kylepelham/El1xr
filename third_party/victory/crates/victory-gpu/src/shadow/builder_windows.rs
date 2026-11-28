//! (El1xr) `VICTORY_BVH_CARD_WINDOWS_MB=<MB>` (off by default; only with
//! `VICTORY_BVH_BUILDER=1`): windows outside buffers go to the card up to
//! this many MB more than `window_card_most` gives them otherwise.
//!
//! The structure builder and the traces read and write title memory by
//! address (`crate::memory`): every list with a builder dispatch declares
//! every numbered copy written. A window with a card side is compared on the
//! card (`Host::card_before`) and only the pieces that changed come back. A
//! window without one is snapshotted on the CPU at each such submission and
//! compared on the CPU when it lands. Batch 5 part F (R3, RTX 5090): the
//! windows grew from 1.7 GB to 6-8 GB while the launcher's
//! `VICTORY_CARD_WINDOWS_MB=1536` put 1.5 GB of them on the card. Per frame,
//! 9.7 GB were snapshotted (`snapshots` 580-770 ms of preparing) and 9.7-13 GB
//! compared (the relay 690 ms landing), with the card about 5 % busy. That is
//! where the 0.75 fps went.
//!
//! The card's own line (`placement::window_line`, eight tenths of its
//! budget) still holds: a window that would pass it stays in the PC's memory
//! as before.

use std::sync::OnceLock;

/// The bytes the switch adds: `switch_mb` MB with the builder on, else none.
fn extra(switch_mb: Option<u64>, builder: bool) -> u64 {
    match switch_mb {
        Some(mb) if builder => mb.saturating_mul(1 << 20),
        _ => 0,
    }
}

/// The bytes of windows the card holds beyond `window_card_most`'s own
/// total ([`extra`] of the switches), read once and said once.
pub(super) fn more() -> u64 {
    static MORE: OnceLock<u64> = OnceLock::new();
    *MORE.get_or_init(|| {
        let mb = std::env::var("VICTORY_BVH_CARD_WINDOWS_MB")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok());
        let builder = std::env::var("VICTORY_BVH_BUILDER").is_ok_and(|v| v.trim() == "1");
        let more = extra(mb, builder);
        if more > 0 {
            tracing::info!(
                "the structure builder's windows: windows outside buffers on the card up to {} MB more \
                 (VICTORY_BVH_CARD_WINDOWS_MB): their shader writes are compared on the card, not snapshotted",
                more >> 20
            );
        } else if mb.is_some_and(|mb| mb > 0) {
            tracing::info!("VICTORY_BVH_CARD_WINDOWS_MB is set but the structure builder is off: windows placed as before");
        }
        more
    })
}

#[cfg(test)]
mod tests {
    use super::extra;

    #[test]
    fn adds_only_with_the_builder_on() {
        assert_eq!(extra(Some(8192), true), 8192 << 20);
        assert_eq!(extra(Some(8192), false), 0);
        assert_eq!(extra(None, true), 0);
        assert_eq!(extra(Some(0), true), 0);
        assert_eq!(extra(Some(u64::MAX), true), u64::MAX);
    }
}
