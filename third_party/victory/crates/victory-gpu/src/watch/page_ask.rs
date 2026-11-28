//! (feedcard, #88) `VICTORY_PAGE_ASK=1` (Live, off by default): a write
//! fault that finds its block already opened (by another thread, or not
//! guarded any more) asks whether its one page is writable with
//! `QueryWorkingSetEx`, not `VirtualQuery`.
//!
//! `VirtualQuery` answers for the whole run of alike pages around the
//! address and walks to its end: on a view of hundreds of megabytes of
//! read-only blocks that is hundreds of microseconds, with the address space
//! held against every protection change. Neo measured 91 such asks a frame
//! of 245 us each on the same 9950X3D (title threads' fault time 15.5-17.3
//! -> 2.8-3.3 ms a frame). The working-set answer covers one page and walks
//! nothing; a page not in memory (or no answer) is asked the old way, so
//! the answer is the same: committed (a resident page is) and read-write.
//!
//! After KyleS98/VIctory-Neo `0fb1e38eee` (`VICTORY_PAGE_ASK`).
//! Line: write fault page asks per frame.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

static ON: victory_core::settings::Live = victory_core::settings::Live::flag("VICTORY_PAGE_ASK");

/// Asks answered by the page, asks sent to `VirtualQuery`, and the
/// microseconds of each kind, since the last line.
static COUNTS: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

/// The protection of the page `at` lies in, if the system has that page in
/// the process's memory (`None`: it has not, or would not say).
pub(super) fn page_protection(at: usize) -> Option<u32> {
    use windows_sys::Win32::System::ProcessStatus::{
        K32QueryWorkingSetEx, PSAPI_WORKING_SET_EX_INFORMATION,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    // SAFETY: plain data; the address asked about goes in it.
    let mut info: PSAPI_WORKING_SET_EX_INFORMATION = unsafe { std::mem::zeroed() };
    info.VirtualAddress = at as *mut _;
    // SAFETY: our own process; an array of one record of that size.
    let ok = unsafe {
        K32QueryWorkingSetEx(
            GetCurrentProcess(),
            std::ptr::from_mut(&mut info).cast(),
            size_of_val(&info) as u32,
        )
    } != 0;
    // SAFETY: every form of the answer is these bits.
    let flags = unsafe { info.VirtualAttributes.Flags };
    // Bit 0: the page is valid in the working set; bits 4-14: its protection.
    (ok && flags & 1 != 0).then(|| ((flags >> 4) & 0x7ff) as u32)
}

/// Whether `at` is committed read-write memory now: by its page with the
/// switch on and the page in memory, else by `VirtualQuery`.
pub(super) fn writable(at: usize) -> bool {
    writable_with(ON.get_bool(), at)
}

pub(super) fn writable_with(on: bool, at: usize) -> bool {
    use windows_sys::Win32::System::Memory::PAGE_READWRITE;
    let began = std::time::Instant::now();
    if on && let Some(protection) = page_protection(at) {
        COUNTS[0].fetch_add(1, Relaxed);
        COUNTS[2].fetch_add(began.elapsed().as_micros() as u64, Relaxed);
        return protection == PAGE_READWRITE;
    }
    let answer = region_writable(at);
    COUNTS[1].fetch_add(1, Relaxed);
    COUNTS[3].fetch_add(began.elapsed().as_micros() as u64, Relaxed);
    answer
}

/// [`writable`] by `VirtualQuery` over the region `at` lies in.
fn region_writable(at: usize) -> bool {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_READWRITE, VirtualQuery,
    };
    // SAFETY: plain data filled in by the query.
    let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: a valid out pointer of the right size.
    let n = unsafe { VirtualQuery(at as *const _, &mut info, size_of_val(&info)) };
    n != 0 && info.State == MEM_COMMIT && info.Protect == PAGE_READWRITE
}

/// One line a frame, once a fault asked.
pub(super) fn line(frames: f64) {
    let c = COUNTS.each_ref().map(|a| a.swap(0, Relaxed));
    if frames <= 0.0 || c[0] + c[1] == 0 {
        return;
    }
    tracing::info!(
        "write fault page asks per frame (VICTORY_PAGE_ASK={}): {:.1} by the page in {:.3} ms, \
         {:.1} by VirtualQuery in {:.3} ms",
        u8::from(ON.get_bool()),
        c[0] as f64 / frames,
        c[2] as f64 / frames / 1e3,
        c[1] as f64 / frames,
        c[3] as f64 / frames / 1e3,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_NOACCESS, PAGE_READONLY, PAGE_READWRITE,
        VirtualAlloc, VirtualFree, VirtualProtect,
    };

    const PAGE: usize = 4096;

    /// The page answer and the region answer agree on every state a fault
    /// meets: read-write and touched, read-only, no access, reserved only,
    /// committed but never touched (not in memory: asked the old way), and
    /// a read-write page inside a run of read-only ones.
    #[test]
    fn the_page_answer_is_the_regions() {
        // SAFETY: our own fresh allocation of 16 pages, written and
        // re-protected only inside it, released at the end.
        unsafe {
            let base =
                VirtualAlloc(std::ptr::null(), 16 * PAGE, MEM_RESERVE, PAGE_NOACCESS) as usize;
            assert_ne!(base, 0);
            assert!(
                !VirtualAlloc(base as *const _, 12 * PAGE, MEM_COMMIT, PAGE_READWRITE).is_null()
            );
            // Pages 0-9 touched; 10 and 11 committed, never touched; 12-15 reserved.
            for p in 0..10 {
                ((base + p * PAGE) as *mut u8).write_volatile(1);
            }
            let mut old = 0;
            assert_ne!(
                VirtualProtect(
                    (base + 2 * PAGE) as *const _,
                    4 * PAGE,
                    PAGE_READONLY,
                    &mut old
                ),
                0
            );
            // A read-write page among the read-only ones.
            assert_ne!(
                VirtualProtect(
                    (base + 4 * PAGE) as *const _,
                    PAGE,
                    PAGE_READWRITE,
                    &mut old
                ),
                0
            );
            assert_ne!(
                VirtualProtect((base + 7 * PAGE) as *const _, PAGE, PAGE_NOACCESS, &mut old),
                0
            );
            let want = [
                true, true, false, false, true, false, true, false, true, true, true, true, false,
                false, false, false,
            ];
            for (p, &w) in want.iter().enumerate() {
                let at = base + p * PAGE + 123;
                assert_eq!(region_writable(at), w, "region, page {p}");
                assert_eq!(writable_with(true, at), w, "by the page, page {p}");
                assert_eq!(writable_with(false, at), w, "off, page {p}");
            }
            // Touched pages answer by the page; reserved ones do not.
            assert_eq!(page_protection(base), Some(PAGE_READWRITE));
            assert_eq!(page_protection(base + 2 * PAGE), Some(PAGE_READONLY));
            assert_eq!(page_protection(base + 13 * PAGE), None);
            assert_ne!(VirtualFree(base as *mut _, 0, MEM_RELEASE), 0);
        }
    }
}
