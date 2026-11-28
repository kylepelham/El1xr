//! (mandy 6e80eb8648) The runtime's half of `VICTORY_ALIAS_LATER=1`
//! (`victory_gpu::alias_later`): a page-mapping copy in two steps. The call
//! looks up the pages each range is to show, while the pool shows them (the
//! title maps other pages into its pool right after); the mapping thread then
//! maps them, each range as [`map_alias`] / [`map_alias_constant`] would have
//! in the call (the same pages left as they are, the same unmaps, the same
//! views, the same failures).

use victory_gpu::alias_later::{AliasOp, LaterWork};

use super::*;

/// One range, looked up.
enum Planned {
    /// `[to, to + len)` shows `slices` page for page, or nothing (`unmap`).
    /// Fewer slices than pages: the source ended at a page not mapped; the
    /// pages after are left unmapped and the range fails.
    Copy {
        to: usize,
        len: usize,
        unmap: bool,
        slices: Vec<Slice>,
    },
    /// Every page of `[to, to + len)` shows `source`.
    Constant {
        to: usize,
        len: usize,
        source: Slice,
    },
    /// A constant range whose source page is not mapped: nothing is done.
    Failed,
}

/// What the page at `at` shows, as a key of `Physical::shown`.
fn shown_key(physical: &Physical, at: usize) -> Option<(usize, usize)> {
    physical
        .views
        .range(..=at)
        .next_back()
        .filter(|(base, (size, _))| at < *base + size)
        .map(|(base, (_, s))| s.key((at - base) / PAGE))
}

/// The slice shown at `at`.
fn slice_at(physical: &Physical, at: usize) -> Option<Slice> {
    physical
        .views
        .range(..=at)
        .next_back()
        .filter(|(base, (size, _))| at < *base + size)
        .map(|(base, (_, s))| Slice {
            index: s.index + (at - base) / PAGE,
            section: s.section.clone(),
        })
}

/// Whether sorted, joined `ranges` meet `[lo, hi)`.
fn meets(ranges: &[(u64, u64)], lo: u64, hi: u64) -> bool {
    let i = ranges.partition_point(|r| r.1 <= lo);
    ranges.get(i).is_some_and(|r| r.0 < hi)
}

/// The first step of a page-mapping copy behind the title's call
/// (`victory_gpu::alias_later::LaterPageMapper`): looks up what `ops` are to
/// show and hands back the mapping. `None` where that would not end as the call
/// did: ranges off 64 KiB pages, a source that a destination of the same call
/// meets (the call reads what it maps), or the title's maps side by side.
pub fn map_aliases_later(ops: &[AliasOp]) -> Option<LaterWork> {
    let aligned = |v: u64| v as usize % PAGE == 0;
    if side_by_side::all_on()
        || ops
            .iter()
            .any(|op| !aligned(op.to) || !aligned(op.from) || !aligned(op.len))
    {
        return None;
    }
    let source = |op: &AliasOp| (op.from, if op.constant { PAGE as u64 } else { op.len });
    let destinations = victory_gpu::alias_later::touched(ops);
    if ops.iter().any(|op| {
        let (from, len) = source(op);
        op.from != 0 && len > 0 && meets(&destinations, from, from.saturating_add(len))
    }) {
        return None;
    }
    // A source an earlier batch still maps into: as it will show then.
    let sources: Vec<(u64, u64)> = ops.iter().filter(|op| op.from != 0).map(source).collect();
    victory_gpu::alias_later::wait_touching(&sources);
    let plans: Vec<Planned> = {
        let physical = physical();
        ops.iter()
            .map(|op| {
                let (to, from, len) = (op.to as usize, op.from as usize, op.len as usize);
                if op.constant {
                    return match slice_at(&physical, from) {
                        Some(source) => Planned::Constant { to, len, source },
                        None => Planned::Failed,
                    };
                }
                let slices = match from {
                    0 => Vec::new(),
                    _ => (0..len / PAGE)
                        .map_while(|k| slice_at(&physical, from + k * PAGE))
                        .collect(),
                };
                Planned::Copy {
                    to,
                    len,
                    unmap: from == 0,
                    slices,
                }
            })
            .collect()
    };
    Some(Box::new(move || {
        plans.into_iter().fold(true, |ok, plan| apply(plan) & ok)
    }))
}

/// The second step, for one range: what [`map_alias`] or
/// [`map_alias_constant`] does after looking up the source.
fn apply(plan: Planned) -> bool {
    match plan {
        Planned::Copy {
            to,
            len,
            unmap,
            slices,
        } => apply_copy(to, len, unmap, &slices),
        Planned::Constant { to, len, source } => apply_constant(to, len, source),
        Planned::Failed => {
            let _call = Call::begin();
            false
        }
    }
}

/// [`map_alias`] with the source's slices looked up (ranges apart).
fn apply_copy(to: usize, len: usize, unmap_only: bool, slices: &[Slice]) -> bool {
    use std::sync::atomic::Ordering::Relaxed;
    let _making = probe::Making::begin(probe::Kind::Copy);
    let _call = Call::begin();
    let mut physical = physical();
    let pages = len / PAGE;
    let complete = unmap_only || slices.len() == pages;
    WORK[4].fetch_add(pages as u64, Relaxed);
    let mode = remap_same();
    if mode != 0 {
        let same = if unmap_only {
            shows_nothing(&physical, to, len)
        } else {
            complete
                && slices
                    .iter()
                    .enumerate()
                    .all(|(k, s)| shown_key(&physical, to + k * PAGE) == Some(s.key(0)))
        };
        if same {
            WORK[10].fetch_add(pages as u64, Relaxed);
            if mode == 1 && !unmap_only {
                victory_gpu::kernel_will_write(to, len);
            }
            return true;
        }
    }
    let _changing = changing(&physical, to, len);
    unmap(&mut physical, to, len);
    if unmap_only {
        return true;
    }
    if alias_runs() {
        let mut k = 0;
        while k < slices.len() {
            let first = &slices[k];
            let run = 1 + slices[k + 1..]
                .iter()
                .zip(1..)
                .take_while(|&(s, i)| {
                    Arc::ptr_eq(&s.section, &first.section) && s.index == first.index + i
                })
                .count();
            if !map_run(&mut physical, to + k * PAGE, run, first.clone()) {
                return false;
            }
            k += run;
        }
        return complete;
    }
    for (k, slice) in slices.iter().enumerate() {
        if !map(&mut physical, to + k * PAGE, PAGE, slice.clone()) {
            return false;
        }
    }
    complete
}

/// [`map_alias_constant`] with the source's slice looked up.
fn apply_constant(to: usize, len: usize, source: Slice) -> bool {
    use std::sync::atomic::Ordering::Relaxed;
    let pages = len / PAGE;
    if pages == 0 {
        return true;
    }
    let _making = probe::Making::begin(probe::Kind::Constant);
    let _call = Call::begin();
    let mut physical = physical();
    WORK[4].fetch_add(pages as u64, Relaxed);
    let mode = remap_same();
    let key = source.key(0);
    let shows_source =
        |physical: &Physical, at: usize| mode != 0 && shown_key(physical, at) == Some(key);
    let mut i = 0;
    while i < pages {
        let at = to + i * PAGE;
        if shows_source(&physical, at) {
            WORK[10].fetch_add(1, Relaxed);
            if mode == 1 {
                victory_gpu::kernel_will_write(at, PAGE);
            }
            i += 1;
            continue;
        }
        let lo = i;
        i += 1;
        while i < pages && !shows_source(&physical, to + i * PAGE) {
            i += 1;
        }
        let (start, sub_len) = (to + lo * PAGE, (i - lo) * PAGE);
        let _changing = changing(&physical, start, sub_len);
        unmap(&mut physical, start, sub_len);
        for k in 0..(i - lo) {
            if !map(&mut physical, start + k * PAGE, PAGE, source.clone()) {
                return false;
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::super::tests::{allocate_whole, query};
    use super::*;

    /// One test at a time sends batches to the mapping thread.
    static ONE: Mutex<()> = Mutex::new(());

    fn reserve(pages: usize) -> *mut c_void {
        let at = XMemVirtualAlloc(
            std::ptr::null_mut(),
            pages * PAGE,
            MEM_RESERVE | PAGES_64K,
            0,
            PAGE_READWRITE,
        );
        assert!(!at.is_null());
        at
    }

    fn at(p: *mut c_void, page: usize) -> u64 {
        p as u64 + (page * PAGE) as u64
    }

    fn copy(to: u64, from: u64, pages: usize) -> AliasOp {
        AliasOp {
            to,
            from,
            len: (pages * PAGE) as u64,
            constant: false,
        }
    }

    /// What each page of `[base, base + pages)` shows.
    fn shown(base: *mut c_void, pages: usize) -> Vec<Option<(usize, usize)>> {
        let physical = physical();
        (0..pages)
            .map(|k| shown_key(&physical, base as usize + k * PAGE))
            .collect()
    }

    /// The ordering the switch must keep: what a range is to show is looked up
    /// at the call, so the pool mapped to other pages and written before the
    /// mapping is done changes nothing of it.
    #[test]
    fn mappings_done_later_show_what_the_pool_showed_at_the_call() {
        let pages = allocate_whole(4);
        let (pool, heap) = (reserve(2), reserve(3));
        assert_eq!(XMemMapPhysicalPages(pool, 2, pages.as_ptr()), pool);
        // SAFETY: mapped read-write.
        unsafe {
            pool.cast::<u8>().write(11);
            (at(pool, 1) as *mut u8).write(22);
        }
        let later = map_aliases_later(&[copy(heap as u64, pool as u64, 2)]).expect("split");
        // The title goes on: other pages in its pool, other bytes in them.
        assert_eq!(XMemMapPhysicalPages(pool, 2, pages[2..].as_ptr()), pool);
        // SAFETY: mapped read-write.
        unsafe {
            pool.cast::<u8>().write(33);
            (at(pool, 1) as *mut u8).write(44);
        }
        assert_eq!(query(heap).host.State, MEM_RESERVE);
        assert!(later());
        // SAFETY: mapped by the copy.
        unsafe {
            assert_eq!(heap.cast::<u8>().read(), 11);
            assert_eq!((at(heap, 1) as *const u8).read(), 22);
        }
        assert_eq!(query(at(heap, 2) as *const c_void).host.State, MEM_RESERVE);
    }

    /// The same copies, in the call on one heap and in two steps on another,
    /// end with every page showing the same, and fail alike: runs of pages,
    /// pages shown already, unmaps (of something and of nothing), constant
    /// pages over a mapped run, a source that ends at a page not mapped, a
    /// constant source not mapped.
    #[test]
    fn mappings_done_later_end_as_mappings_in_the_call() {
        let pages = allocate_whole(6);
        let pool = reserve(8);
        assert_eq!(XMemMapPhysicalPages(pool, 4, pages.as_ptr()), pool);
        assert_eq!(
            XMemMapPhysicalPages(at(pool, 5) as *mut c_void, 2, pages[4..].as_ptr()),
            at(pool, 5) as *mut c_void
        );
        // pool: 0-3 pages 0-3, 4 nothing, 5-6 pages 4-5, 7 nothing.
        let steps = |heap: *mut c_void| {
            let h = |k: usize| at(heap, k);
            vec![
                vec![copy(h(0), pool as u64, 4)],
                vec![copy(h(0), pool as u64, 4), copy(h(6), at(pool, 5), 2)],
                vec![
                    copy(h(1), 0, 2),
                    AliasOp {
                        to: h(4),
                        from: at(pool, 6),
                        len: (3 * PAGE) as u64,
                        constant: true,
                    },
                ],
                vec![copy(h(8), 0, 2)],
                vec![copy(h(2), at(pool, 3), 3)],
                vec![AliasOp {
                    to: h(9),
                    from: at(pool, 4),
                    len: PAGE as u64,
                    constant: true,
                }],
                vec![
                    copy(h(0), at(pool, 1), 2),
                    copy(h(3), at(pool, 2), 1),
                    copy(h(7), 0, 1),
                ],
            ]
        };
        let (now, two) = (reserve(10), reserve(10));
        for (in_call, later) in steps(now).into_iter().zip(steps(two)) {
            let ok_now = in_call.iter().fold(true, |ok, op| {
                let done = if op.constant {
                    map_alias_constant(op.to, op.from, op.len)
                } else {
                    map_alias(op.to, op.from, op.len)
                };
                ok & done
            });
            let ok_later = map_aliases_later(&later).expect("split")();
            assert_eq!(ok_now, ok_later, "{later:?}");
            assert_eq!(shown(now, 10), shown(two, 10), "{later:?}");
        }
    }

    /// Not split: a source that a destination of the same call meets.
    #[test]
    fn a_copy_that_reads_what_it_maps_is_not_split() {
        let heap = reserve(4);
        assert!(map_aliases_later(&[copy(at(heap, 0), at(heap, 1), 2)]).is_none());
        assert!(
            map_aliases_later(&[copy(at(heap, 2), 0, 1), copy(at(heap, 0), at(heap, 2), 1)])
                .is_none()
        );
        assert!(map_aliases_later(&[copy(at(heap, 0) + 0x1000, 0, 1)]).is_none());
    }

    /// The title's own mapping of an address a pending batch maps waits for it:
    /// the title's pages are what the address shows in the end.
    #[test]
    fn the_titles_mapping_waits_for_a_pending_batch_there() {
        let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
        let pages = allocate_whole(2);
        let (pool, heap) = (reserve(1), reserve(1));
        assert_eq!(XMemMapPhysicalPages(pool, 1, pages.as_ptr()), pool);
        let ops = [copy(heap as u64, pool as u64, 1)];
        let work = map_aliases_later(&ops).expect("split");
        let (go, wait) = std::sync::mpsc::channel::<()>();
        let held: LaterWork = Box::new(move || {
            wait.recv().unwrap();
            work()
        });
        let number = victory_gpu::alias_later::send(held, victory_gpu::alias_later::touched(&ops))
            .unwrap_or_else(|_| panic!("no mapping thread"));
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(30));
            go.send(()).unwrap();
        });
        // The title maps its second page there: after the batch.
        assert_eq!(XMemMapPhysicalPages(heap, 1, pages[1..].as_ptr()), heap);
        assert!(victory_gpu::alias_later::done(number));
        releaser.join().unwrap();
        let physical_key = {
            let physical = physical();
            physical.pages.get(&pages[1]).map(|s| s.key(0))
        };
        assert_eq!(shown(heap, 1), vec![physical_key]);
    }
}
