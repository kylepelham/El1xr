//! (entryhitch, #16) The world's first pass over the heap's buffer views
//! made ahead, on the loading screen (`VICTORY_HEAP_WARM=1`, Live, off by
//! default).
//!
//! The first world frame took 1.5-2.5 s (b5i_stale f515, b5h_cpu f562). Its
//! first heap-buffers pass (`draw::State::heap_buffers`, on one title thread
//! inside a bind) made 59,422 views over 5,274 host copies, 1,850 MB: 604 ms,
//! 425 of them making the copies (`buffer_ahead`). The list it ran in then
//! refreshed all 1,850 MB: the submitting thread's "buffer copies" of that
//! frame (490-508 ms, 340 flushes) and the kept threads' "stale copy"
//! (~1.1 GB at ~0.4 GB/s a thread). The title had written those buffers
//! during its loading screen (10-17 s, the CPU mostly idle).
//!
//! Here, while no world frame has been seen (`load_rate::in_world`), at
//! most once a second a bind that sets a view heap hands the heap (a
//! reference taken on it) to a thread of ours at below-normal priority. It
//! asks each buffer view of the heap for its host copy, as the pass will
//! (`descriptor::host::buffer_copy`: the copy is made if it is not there),
//! and brings each copy's span in from title memory as a refresh would
//! (`shadow::warm::fill`: what changed since the copy last took it). When
//! the world's first pass comes, the copies are there and its refresh takes
//! only the pages written since (write-watched memory; unwatched memory is
//! left to the pass, which copies it whole anyway).
//!
//! Nothing is shown earlier or otherwise: no slot is written here (the pass
//! still makes every view, on the copy it finds). The copies hold the
//! title's bytes as a refresh would have put them; the card side of a copy
//! in the card's memory takes what changed at the next submission, as after
//! any refresh. Copies with shader writes pending, in place, or windows
//! are left alone. Stops at the first world frame. Not in any cache key.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use victory_core::settings::Live;

use crate::descriptor::View;
use crate::object::This;
use crate::shadow::Shadow;

static WARM: Live = Live::flag("VICTORY_HEAP_WARM");

pub(crate) fn on() -> bool {
    WARM.get_bool()
}

/// Passes at most this often (ms).
const EVERY_MS: u64 = 1000;
/// Our threads for a pass.
const THREADS: usize = 4;

/// A pass is running.
static BUSY: AtomicBool = AtomicBool::new(false);
/// When the last pass began (`host::now_ms` plus one; 0: none yet).
static LAST: AtomicU64 = AtomicU64::new(0);
/// Since the start: passes, views asked, copies brought in, bytes copied,
/// microseconds, passes cut short by the world's first frame.
static STATS: [AtomicU64; 6] = [const { AtomicU64::new(0) }; 6];

/// Whether a pass is due at `now` (ms) after one begun at `last` (plus one).
fn due(last: u64, now: u64) -> bool {
    last == 0 || now.saturating_add(1) >= last.saturating_add(EVERY_MS)
}

/// A bind set `heaps` (title heap objects, 0: none): on the loading screen,
/// hands the view heap to a pass if one is due.
pub(crate) fn offer(heaps: &[usize; 2]) {
    if !on() || crate::load_rate::in_world() {
        return;
    }
    let now = crate::host::now_ms();
    if !due(LAST.load(Ordering::Relaxed), now) || BUSY.swap(true, Ordering::AcqRel) {
        return;
    }
    let heap = heaps
        .iter()
        .copied()
        .filter(|&h| h != 0)
        .find(|&h| !crate::descriptor::heap(h as This).samplers());
    // (The bind's list holds the heap now: a reference may be taken on it.)
    let Some(heap) = heap.filter(|&h| {
        crate::descriptor::heap(h as This).host().is_some()
            && crate::object::add_ref_alive(h as This)
    }) else {
        BUSY.store(false, Ordering::Release);
        return;
    };
    LAST.store(now + 1, Ordering::Relaxed);
    let spawned = std::thread::Builder::new()
        .name("victory-heap-warm".into())
        .spawn(move || {
            below_normal();
            pass(heap);
            crate::object::release_boxed::<crate::descriptor::Heap>(heap as This);
            BUSY.store(false, Ordering::Release);
        });
    if spawned.is_err() {
        crate::object::release_boxed::<crate::descriptor::Heap>(heap as This);
        BUSY.store(false, Ordering::Release);
    }
}

fn below_normal() {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Threading::{
            GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL,
        };
        // SAFETY: the calling thread's pseudo handle.
        unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL) };
    }
}

/// Per copy (by its address), the span its views read: one entry each, in
/// the order first seen.
fn spans<T: Clone>(copies: impl IntoIterator<Item = (usize, T, u64, u64)>) -> Vec<(T, u64, u64)> {
    let mut at = std::collections::HashMap::new();
    let mut out: Vec<(T, u64, u64)> = Vec::new();
    for (key, copy, address, len) in copies {
        let end = address.saturating_add(len);
        match at.get(&key) {
            Some(&i) => {
                let s: &mut (T, u64, u64) = &mut out[i];
                s.1 = s.1.min(address);
                s.2 = s.2.max(end);
            }
            None => {
                at.insert(key, out.len());
                out.push((copy, address, end));
            }
        }
    }
    out
}

/// Runs `work` over `items` on [`THREADS`] threads, in order of the
/// pieces; stops handing out pieces at the world's first frame.
fn side_by_side<I: Sync, R: Send>(items: &[I], work: impl Fn(&I) -> Option<R> + Sync) -> Vec<R> {
    let piece = items.len().div_ceil(THREADS).max(1);
    std::thread::scope(|s| {
        let handles: Vec<_> = items
            .chunks(piece)
            .map(|chunk| {
                let work = &work;
                s.spawn(move || {
                    below_normal();
                    let mut out = Vec::new();
                    for (n, item) in chunk.iter().enumerate() {
                        if n % 256 == 255 && crate::load_rate::in_world() {
                            break;
                        }
                        out.extend(work(item));
                    }
                    out
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().unwrap_or_default())
            .collect()
    })
}

fn pass(heap: usize) {
    let began = std::time::Instant::now();
    let heap = crate::descriptor::heap(heap as This);
    let Some(host) = heap.host() else {
        return;
    };
    let views: Vec<View> = host
        .buffer_slots()
        .into_iter()
        .filter_map(|i| crate::descriptor::view(heap.handle(i)?))
        .collect();
    let copies = side_by_side(&views, |view| {
        let (shadow, address, len) = crate::descriptor::host::buffer_copy(view)?;
        let key = Arc::as_ptr(&shadow) as usize;
        Some((key, shadow, address, len))
    });
    let spans: Vec<(Arc<Shadow>, u64, u64)> = spans(copies);
    let filled = side_by_side(&spans, |(shadow, from, to)| {
        crate::shadow::warm::fill(shadow, *from as usize, (*to - *from) as usize)
    });
    let bytes: usize = filled.iter().sum();
    let cut = crate::load_rate::in_world();
    let us = began.elapsed().as_micros() as u64;
    let n = STATS[0].fetch_add(1, Ordering::Relaxed) + 1;
    STATS[1].fetch_add(views.len() as u64, Ordering::Relaxed);
    STATS[2].fetch_add(filled.len() as u64, Ordering::Relaxed);
    STATS[3].fetch_add(bytes as u64, Ordering::Relaxed);
    STATS[4].fetch_add(us, Ordering::Relaxed);
    STATS[5].fetch_add(u64::from(cut), Ordering::Relaxed);
    if n <= 4 || n.is_multiple_of(8) || cut {
        tracing::info!(
            pass = n,
            views = views.len(),
            copies = spans.len(),
            brought_in = filled.len(),
            mb = bytes as f64 / 1e6,
            ms = us as f64 / 1e3,
            cut_by_world = cut,
            "heap warm: buffer views' host copies brought in on the loading screen (VICTORY_HEAP_WARM=1)"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pass_is_due_once_a_second() {
        assert!(due(0, 0));
        assert!(due(0, 5));
        assert!(!due(1, 0));
        assert!(!due(1, 999));
        assert!(due(1, 1000));
        assert!(due(501, 1500));
        assert!(!due(501, 1499));
    }

    #[test]
    fn spans_cover_every_view_of_a_copy_once() {
        let copies = [
            (1, 'a', 0x1000, 0x100),
            (2, 'b', 0x9000, 0x10),
            (1, 'a', 0x800, 0x100),
            (1, 'a', 0x1800, 0x200),
            (2, 'b', 0x9008, 0x100),
        ];
        assert_eq!(spans(copies), [('a', 0x800, 0x1a00), ('b', 0x9000, 0x9108)]);
        assert!(spans(std::iter::empty::<(usize, char, u64, u64)>()).is_empty());
    }

    #[test]
    fn side_by_side_keeps_every_result() {
        let items: Vec<u32> = (0..1000).collect();
        let mut got = side_by_side(&items, |&i| (i % 3 != 0).then_some(i * 2));
        got.sort_unstable();
        let want: Vec<u32> = (0..1000).filter(|i| i % 3 != 0).map(|i| i * 2).collect();
        assert_eq!(got, want);
    }

    #[test]
    fn off_it_takes_nothing() {
        if on() {
            return;
        }
        offer(&[0, 0]);
        assert!(!BUSY.load(Ordering::Relaxed));
        assert_eq!(LAST.load(Ordering::Relaxed), 0);
    }
}
