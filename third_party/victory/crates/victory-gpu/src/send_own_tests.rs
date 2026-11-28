//! `VICTORY_SEND_OWN` (after rorodev6/victory-megafork-amd `18c01292113e`):
//! the layout of a plan's owned bytes and the masked pass's source. The pass
//! itself runs on WARP in `indirect::tests::send_own_warp_tests`.

use super::*;

/// Off, the masked pass's source is the one it always was; on, a block count
/// with the bit set reads block `slot` at `slot * 4096`, and without the bit
/// the old address.
#[test]
fn the_patch_source_changes_only_with_the_switch() {
    let source = crate::host::CARD_PATCH;
    assert!(matches!(patch_source(source, false), Cow::Borrowed(s) if s == source));
    let on = patch_source(source, true);
    assert!(on.contains("slot * 4096u + word * 4u : a;"));
    assert!(on.contains("from.Load(src)"));
    assert!(on.contains("if (slot >= (blocks & 0x7FFFFFFFu)) return;"));
    assert!(!on.contains("from.Load(a)"));
}

/// Blocks first, each 4 KiB, then the ranges back to back.
#[test]
fn owned_bytes_are_laid_out_blocks_first() {
    let (blocks, ranges, all) = layout(2, &[(10, 14), (100, 103), (4096, 8192)]);
    assert_eq!(blocks, [0, 4096]);
    assert_eq!(ranges, [8192, 8196, 8199]);
    assert_eq!(all, 8199 + 4096);
    assert_eq!(layout(0, &[]), (Vec::new(), Vec::new(), 0));
}
