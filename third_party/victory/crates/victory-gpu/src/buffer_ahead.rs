//! Host copies of a big heap-buffers pass made side by side
//! (`VICTORY_BUFFER_AHEAD=1`).
//!
//! The world's first pass over the buffer views of a heap makes some 60,000
//! views, one after the other, and with them every host copy they read:
//! 5,255 copies, 1.85 GB, 2.6-3.8 s of one frame. Its time goes into making
//! the copies' buffers (`CreateCommittedResource`, which waits for the
//! system to make each one resident), not the views. Here the pass first has
//! the copies its slots will read made on the kept threads, with the tables
//! let go while a buffer is made (`shadow::lookup_tables`); then it makes
//! its views one after the other as before, finding the copies there.
//!
//! Nothing is shown earlier or otherwise: a slot shows a view only when the
//! pass makes it, on the copy that pass would have made itself, so no slot
//! is left with a view of stale or missing memory (the hang `draw.rs`
//! records of streamed geometry drawn from stale buffers).

use std::collections::HashSet;
use std::sync::Arc;

use crate::descriptor::View;

/// Passes over fewer slots make their copies as they go: the threads cost
/// more than they save.
pub(crate) const LEAST: usize = 4096;

/// Whether big passes make their copies ahead (`VICTORY_BUFFER_AHEAD=1`).
pub(crate) fn on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_BUFFER_AHEAD").is_some_and(|v| v == "1"))
}

/// One of each: views of the same buffer range read the same copies.
fn distinct(
    views: impl IntoIterator<Item = View>,
    key: impl Fn(&View) -> Option<(u64, u64)>,
) -> Vec<View> {
    let mut seen = HashSet::new();
    views
        .into_iter()
        .filter(|view| key(view).is_some_and(|k| seen.insert(k)))
        .collect()
}

/// Makes the host copies `views` (those of a pass's slots) read, on the
/// kept threads and this one, and returns when all are made. First each
/// buffer's whole copy, once per buffer (two threads asking for the same
/// buffer would both make one), then what each view reads of it (a copy of
/// its own bytes, `shadow::exact`, if it has one).
pub(crate) fn make(views: Vec<View>) {
    use crate::descriptor::host::{buffer_copy, buffer_extent, buffer_range};
    let began = std::time::Instant::now();
    let n = views.len();
    let buffers = Arc::new(distinct(views.iter().cloned(), buffer_extent));
    let ranges = Arc::new(distinct(views, buffer_range));
    let (b, r) = (buffers.len(), ranges.len());
    {
        let buffers = buffers.clone();
        crate::shadow::wide_chunks(b, 16, move |from, to| {
            for view in &buffers[from..to] {
                if let Some((base, width)) = buffer_extent(view) {
                    let _ = crate::shadow::find(base, width);
                }
            }
        });
    }
    crate::shadow::wide_chunks(r, 64, move |from, to| {
        for view in &ranges[from..to] {
            let _ = buffer_copy(view);
        }
    });
    static SAID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    if SAID.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 8 {
        tracing::info!(
            views = n,
            buffers = b,
            ranges = r,
            ms = began.elapsed().as_millis() as u64,
            "host copies of a heap-buffers pass made ahead, side by side (VICTORY_BUFFER_AHEAD)"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distinct_keeps_the_first_of_each_key_and_drops_keyless() {
        let views = vec![
            View::Constant(0x1000, 16),
            View::Constant(0x1000, 32),
            View::Constant(0x2000, 16),
            View::Sampler([0; 13]),
            View::Constant(0x1000, 16),
        ];
        let key = |v: &View| match v {
            View::Constant(at, _) => Some((*at, 0)),
            _ => None,
        };
        assert_eq!(
            distinct(views, key),
            [View::Constant(0x1000, 16), View::Constant(0x2000, 16)]
        );
    }
}
