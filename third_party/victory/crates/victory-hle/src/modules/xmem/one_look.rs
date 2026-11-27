//! (bootfast) `VICTORY_DSTORAGE_ONE_LOOK=1` (off by default): a
//! DirectStorage read into physical pages looks at the page table once
//! before its write and once after, not four times.
//!
//! Every such read took the page table's lock (the one the title's own
//! mapping calls take) to find its page runs, again for the title addresses
//! that show them (to flush and hold the lazy GPU pages there), again after
//! the write for the same addresses (`written`), and a fourth time for the
//! same addresses byte by byte (`shown_at`). At 5,000-11,000 requests a
//! second over 32 workers that is up to 44,000 takes of one lock a second,
//! beside the title's mapping. Here the first two are one look and the last
//! two another; what each finds and what it calls, and in what order, is
//! the same: the holds are taken outside the lock and before the write, the
//! page-wise notes go out while they are held, the byte-wise ones after.

use std::sync::Arc;
use std::sync::OnceLock;

use super::{
    PAGE, PageWindow, Slice, View, merge_pieces, notify_shown, physical, shown_pages, shown_pieces,
};

pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_DSTORAGE_ONE_LOOK").is_some_and(|v| v == "1"))
}

/// As `page_window`: the bytes `[offset, offset + len)` of `pages` when they
/// are consecutive pages of one allocation, writable in place, with the
/// lazy GPU pages that show them flushed and held; one look at the table.
pub(crate) fn page_window(pages: &[usize], offset: usize, len: usize) -> Option<PageWindow> {
    let end = offset.checked_add(len)?;
    let range = pages.get(offset / PAGE..end.div_ceil(PAGE))?;
    let (slice, n, mut shown) = {
        let physical = physical();
        let (slice, n) = one_run(range, |p| physical.pages.get(p).cloned())?;
        let shown = shown_pages(&physical, &slice, n);
        (slice, n, shown)
    };
    slice.section.mark_touched(slice.index, n);
    shown.sort_unstable();
    shown.dedup();
    let holds = shown
        .chunk_by(|a, b| a + PAGE == *b)
        .map(|run| victory_gpu::kernel_will_write_hold(run[0], run.len() * PAGE))
        .collect();
    Some(PageWindow {
        view: View::map(&slice, n)?,
        within: offset % PAGE,
        len,
        slice,
        _holds: holds,
        told: false,
    })
}

/// The one run of consecutive slices of one section that `range` names, or
/// `None` (a page unknown, or more than one run).
fn one_run(range: &[usize], slice_of: impl Fn(&usize) -> Option<Slice>) -> Option<(Slice, usize)> {
    let first = slice_of(range.first()?)?;
    for (i, page) in range.iter().enumerate().skip(1) {
        let s = slice_of(page)?;
        if !Arc::ptr_eq(&s.section, &first.section) || s.index != first.index + i {
            return None;
        }
    }
    Some((first, range.len()))
}

/// After the write through `window` into bytes `[offset, offset + len)` of
/// `pages`: as dropping the window and then `pages_written`, with one look
/// at the table.
pub(crate) fn done(mut window: PageWindow, pages: &[usize], offset: usize, len: usize) {
    let n = window.view.len / PAGE;
    let end = offset.saturating_add(len);
    let first = offset / PAGE;
    let (shown, pieces) = {
        let physical = physical();
        let shown = shown_pages(&physical, &window.slice, n);
        let pieces = pages
            .get(first..end.div_ceil(PAGE))
            .map_or_else(Vec::new, |touched| {
                shown_pieces(&physical, touched, first, offset, end)
            });
        (shown, pieces)
    };
    // Page-wise, while the holds stand (as the window's drop does).
    notify_shown(shown);
    window.told = true;
    drop(window);
    for (at, bytes) in merge_pieces(pieces) {
        victory_gpu::title_memory_written(at, bytes, true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_switch_is_off_unless_set() {
        if std::env::var_os("VICTORY_DSTORAGE_ONE_LOOK").is_none() {
            assert!(!on());
        }
    }
}
