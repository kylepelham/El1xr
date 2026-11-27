//! What `VICTORY_SOFT_VIEWS=copies` (foxhound 9f8838b109) would save, asked
//! of a run without changing it (`VICTORY_SOFT_VIEWS_PROBE=1`; off unless
//! set).
//!
//! Soft views keep the views the GPU's page-mapping copies
//! (`CopyPageMappingsBatchX`: [`super::map_alias`],
//! [`super::map_alias_constant`]) make in our books alone, with no view of
//! the host's until something touches them: a view nothing touches before
//! the title unmaps it costs no map and no unmap (foxhound: 45 and 93 us,
//! each waiting for the address-space lock). Anything that does touch one
//! needs it made then: a fault, or one of the GPU side's reads, guards and
//! questions about title memory (some 80 places here, several of which read
//! a placeholder as zeros or as not there instead of faulting). Before
//! porting that, this asks how many views are never touched at all.
//!
//! When a view is taken away, Windows is asked which of its 4 KiB pages are
//! in the working set (`QueryWorkingSetEx`, the valid bit): a page the CPU
//! (the title's, ours, or the driver locking it for the card) never touched
//! since the view was made is not. A view none of whose pages were touched
//! is one soft views would never have made. The line in the log, every 5 s
//! with the remapped line: views taken away by kind (copies, constant
//! fills, the title's own maps), how many of them untouched, and how long
//! they lived. (A page the system trimmed from the working set reads as
//! untouched too: an upper bound, close under no memory pressure.)

use super::*;

/// Whether the probe runs (`VICTORY_SOFT_VIEWS_PROBE=1`).
pub(super) fn on() -> bool {
    static ON: LazyLock<bool> = LazyLock::new(|| {
        let on = std::env::var_os("VICTORY_SOFT_VIEWS_PROBE").is_some_and(|v| v == "1");
        if on {
            tracing::info!(
                "views taken away are asked whether anything touched them (VICTORY_SOFT_VIEWS_PROBE=1)"
            );
        }
        on
    });
    *ON
}

/// Who makes a view.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Kind {
    /// The title's own mapping calls (and anything else).
    Title = 0,
    /// The GPU's page-mapping copies.
    Copy = 1,
    /// Their fills of one constant page.
    Constant = 2,
}

thread_local! {
    static MAKING: std::cell::Cell<Kind> = const { std::cell::Cell::new(Kind::Title) };
}

/// Views made on this thread until it drops are of `kind`.
pub(super) struct Making(Kind);

impl Making {
    pub(super) fn begin(kind: Kind) -> Making {
        Making(MAKING.with(|m| m.replace(kind)))
    }
}

impl Drop for Making {
    fn drop(&mut self) {
        MAKING.with(|m| m.set(self.0));
    }
}

/// Views made and not yet taken away: when, and by whom.
static MADE: LazyLock<Mutex<HashMap<usize, (std::time::Instant, Kind)>>> =
    LazyLock::new(Default::default);

/// Since the last line, by kind: views taken away, of them untouched, and
/// the milliseconds the untouched and the touched ones lived.
static SEEN: [[AtomicU64; 4]; 3] = [const { [const { AtomicU64::new(0) }; 4] }; 3];

/// A view was made at `base`.
pub(super) fn made(base: usize) {
    if !on() {
        return;
    }
    let kind = MAKING.with(|m| m.get());
    MADE.lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(base, (std::time::Instant::now(), kind));
}

/// The view at `[base, base + len)` is about to be taken away: whether
/// anything touched it is counted.
pub(super) fn gone(base: usize, len: usize) {
    if !on() {
        return;
    }
    let Some((when, kind)) = MADE.lock().unwrap_or_else(|e| e.into_inner()).remove(&base) else {
        return;
    };
    let lived = when.elapsed().as_millis() as u64;
    let touched = touched(base, len);
    let seen = &SEEN[kind as usize];
    seen[0].fetch_add(1, Ordering::Relaxed);
    if touched {
        seen[3].fetch_add(lived, Ordering::Relaxed);
    } else {
        seen[1].fetch_add(1, Ordering::Relaxed);
        seen[2].fetch_add(lived, Ordering::Relaxed);
    }
}

/// Whether a 4 KiB page of `[base, base + len)` is in the working set.
/// (The first 256 pages: a view of the copies is mostly 16.)
fn touched(base: usize, len: usize) -> bool {
    use windows_sys::Win32::System::ProcessStatus::{
        K32QueryWorkingSetEx, PSAPI_WORKING_SET_EX_INFORMATION,
    };
    let n = (len / 4096).clamp(1, 256);
    // SAFETY: plain data; zero is a valid value.
    let mut info: Vec<PSAPI_WORKING_SET_EX_INFORMATION> = (0..n)
        .map(|k| PSAPI_WORKING_SET_EX_INFORMATION {
            VirtualAddress: (base + k * 4096) as *mut c_void,
            ..unsafe { std::mem::zeroed() }
        })
        .collect();
    let bytes = (n * size_of::<PSAPI_WORKING_SET_EX_INFORMATION>()) as u32;
    // SAFETY: `n` entries of ours, each with its address filled in.
    if unsafe { K32QueryWorkingSetEx(GetCurrentProcess(), info.as_mut_ptr().cast(), bytes) } == 0 {
        // Not known: counted as touched (soft views would gain nothing).
        return true;
    }
    // SAFETY: the union's flags word; bit 0 is "valid".
    info.iter()
        .any(|i| unsafe { i.VirtualAttributes.Flags } & 1 != 0)
}

/// The probe's line since the last one, `secs` long; `None` with the probe
/// off or nothing taken away.
pub(super) fn line(secs: f64) -> Option<String> {
    if !on() {
        return None;
    }
    let s: [[u64; 4]; 3] =
        std::array::from_fn(|k| std::array::from_fn(|i| SEEN[k][i].swap(0, Ordering::Relaxed)));
    if s.iter().all(|k| k[0] == 0) {
        return None;
    }
    let part = |k: usize, name: &str| {
        let [all, untouched, untouched_ms, touched_ms] = s[k];
        let touched = all - untouched;
        format!(
            "{name}: {:.0} taken away, {:.0} untouched ({:.0}%, lived {:.0} ms on average), {:.0} touched (lived {:.0} ms)",
            all as f64 / secs,
            untouched as f64 / secs,
            100.0 * untouched as f64 / all.max(1) as f64,
            untouched_ms as f64 / untouched.max(1) as f64,
            touched as f64 / secs,
            touched_ms as f64 / touched.max(1) as f64,
        )
    };
    Some(format!(
        "views never touched before they were taken away, a second over {secs:.0} s (VICTORY_SOFT_VIEWS_PROBE): {}; {}; {}; {} views made and not taken away",
        part(Kind::Copy as usize, "page-mapping copies"),
        part(Kind::Constant as usize, "constant fills"),
        part(Kind::Title as usize, "the title's own maps"),
        MADE.lock().unwrap_or_else(|e| e.into_inner()).len(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A view nobody touched is not in the working set; one read once is.
    #[test]
    fn a_view_read_once_is_touched_and_one_never_read_is_not() {
        let mut pages = [0usize; 2];
        let mut n = 2;
        assert_eq!(XMemAllocatePhysicalPages(0, &mut n, pages.as_mut_ptr()), 1);
        let a = XMemVirtualAlloc(
            std::ptr::null_mut(),
            2 * PAGE,
            MEM_RESERVE | PAGES_64K,
            0,
            PAGE_READWRITE,
        );
        assert!(!a.is_null());
        let b = a.wrapping_byte_add(PAGE);
        assert_eq!(XMemMapPhysicalPages(a, 1, &pages[0]), a);
        assert_eq!(XMemMapPhysicalPages(b, 1, &pages[1]), b);
        assert!(!touched(a as usize, PAGE));
        // SAFETY: mapped read-write.
        assert_eq!(unsafe { b.cast::<u8>().byte_add(5000).read_volatile() }, 0);
        assert!(touched(b as usize, PAGE));
        assert!(!touched(a as usize, PAGE));
        assert_eq!(XMemMapPhysicalPages(a, 2, std::ptr::null()), a);
        assert_eq!(XMemFreePhysicalPages(2, pages.as_ptr()), 1);
    }

    #[test]
    fn views_made_in_a_copy_are_of_the_copies() {
        assert_eq!(MAKING.with(|m| m.get()), Kind::Title);
        {
            let _copy = Making::begin(Kind::Copy);
            assert_eq!(MAKING.with(|m| m.get()), Kind::Copy);
            {
                let _fill = Making::begin(Kind::Constant);
                assert_eq!(MAKING.with(|m| m.get()), Kind::Constant);
            }
            assert_eq!(MAKING.with(|m| m.get()), Kind::Copy);
        }
        assert_eq!(MAKING.with(|m| m.get()), Kind::Title);
    }
}
