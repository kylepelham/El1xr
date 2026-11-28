//! (critpath, #88) `VICTORY_PAGE_ASK=1` (Live, off by default): a write fault asks about its one
//! page, not the whole run of pages around it.
//!
//! [`super::writable`] answered with `VirtualQuery`, which walks to the end of the run of alike
//! pages around the address to say how long it is: on a view of hundreds of megabytes whose blocks
//! are all read-only, hundreds of microseconds with the address space held against every change of
//! protection. The title's write faults ask it whenever the block was opened by another thread
//! first, or a 4 KiB page of it was (part T 06_live, FLUSH_PROF: about 3,000 faults a frame,
//! 12.8 ms a frame in the handler on the title's threads, 8.8k `VirtualQuery` a frame in 11.9 ms on
//! all threads). `QueryWorkingSetEx` answers for the one page and walks nothing. A page not in
//! memory (or a refused answer) is asked about as before.
//!
//! Measured upstream (KyleS98/VIctory-Neo, Ryzen 9 9950X3D): the title's threads' fault time
//! 15.5-17.3 ms to 2.8-3.3 ms a frame. Not in the cache key: no shader or pipeline output.

static ON: victory_core::settings::Live = victory_core::settings::Live::flag("VICTORY_PAGE_ASK");

#[cfg(test)]
thread_local! {
    pub(super) static FORCED: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

pub(super) fn on() -> bool {
    #[cfg(test)]
    if let Some(forced) = FORCED.with(std::cell::Cell::get) {
        return forced;
    }
    ON.get_bool()
}

/// The Win32 protection of the page `at` lies in, where the page is in the working set (`None`:
/// it is not, or the system would not say).
pub(super) fn protection(at: usize) -> Option<u32> {
    use windows_sys::Win32::System::ProcessStatus::{
        K32QueryWorkingSetEx, PSAPI_WORKING_SET_EX_INFORMATION,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    // SAFETY: plain data; the address asked about is set in it.
    let mut info: PSAPI_WORKING_SET_EX_INFORMATION = unsafe { std::mem::zeroed() };
    info.VirtualAddress = at as *mut _;
    // SAFETY: our own process; an array of one record of the size given.
    let ok = unsafe {
        K32QueryWorkingSetEx(
            GetCurrentProcess(),
            std::ptr::from_mut(&mut info).cast(),
            size_of_val(&info) as u32,
        )
    } != 0;
    // SAFETY: every form of the answer is these bits.
    let flags = unsafe { info.VirtualAttributes.Flags };
    // Bit 0: the page is valid (in the working set); bits 4..15: its Win32 protection.
    (ok && flags & 1 != 0).then_some(((flags >> 4) & 0x7ff) as u32)
}

#[cfg(test)]
mod tests {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READONLY, PAGE_READWRITE, VirtualAlloc,
        VirtualFree, VirtualProtect,
    };

    #[test]
    fn one_page_is_asked_about_and_agrees_with_the_region() {
        const PAGE: usize = 4096;
        // SAFETY: a fresh allocation of our own, freed at the end.
        let base = unsafe {
            VirtualAlloc(
                std::ptr::null(),
                16 * PAGE,
                MEM_RESERVE | MEM_COMMIT,
                PAGE_READWRITE,
            )
        } as usize;
        assert_ne!(base, 0);
        // Touched: in the working set.
        for p in 0..16 {
            // SAFETY: committed read-write memory of ours.
            unsafe { std::ptr::write_volatile((base + p * PAGE) as *mut u8, 1) };
        }
        let mut old = 0;
        // SAFETY: pages 4..8 of our allocation.
        assert_ne!(
            unsafe { VirtualProtect((base + 4 * PAGE) as _, 4 * PAGE, PAGE_READONLY, &mut old) },
            0
        );
        assert_eq!(super::protection(base), Some(PAGE_READWRITE));
        assert_eq!(super::protection(base + 5 * PAGE + 17), Some(PAGE_READONLY));
        assert_eq!(super::protection(base + 8 * PAGE), Some(PAGE_READWRITE));
        for forced in [false, true] {
            super::FORCED.with(|f| f.set(Some(forced)));
            assert!(super::super::writable(base + 3 * PAGE + 9));
            assert!(!super::super::writable(base + 7 * PAGE));
            assert!(super::super::writable(base + 15 * PAGE));
        }
        super::FORCED.with(|f| f.set(None));
        // SAFETY: the allocation made above.
        unsafe { VirtualFree(base as _, 0, MEM_RELEASE) };
        // Released: not in the working set, so asked about as before.
        assert_eq!(super::protection(base), None);
    }
}
