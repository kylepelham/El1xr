//! `VICTORY_MAPPED_HOLES=1` (off by default): [`super::mapped_bytes`] counts
//! the blocks of a streamed texture that lie between a missing first page
//! and a missing last page (its only resident mips between two holes), where
//! it used to answer none; and with `VICTORY_COMMITTED_QUERY=1` its walk
//! counts only pages that can be read (no guard pages, no `PAGE_NOACCESS`).
//!
//! The scan between the holes asks the working set only (no kernel read for
//! a block that is not in it), so a slot whose texture is away still costs
//! one `QueryWorkingSetEx` per 16 MiB and no failed reads.
//!
//! Source: mc3yohan-lab/VIctory-RTX-Mc3yohan 581749c560 (from TaynanGT/Vice-Tay #365).

use windows_sys::Win32::System::Memory::{
    MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_EXECUTE_READ, PAGE_EXECUTE_READWRITE,
    PAGE_EXECUTE_WRITECOPY, PAGE_GUARD, PAGE_READONLY, PAGE_READWRITE, PAGE_WRITECOPY,
};
use windows_sys::Win32::System::ProcessStatus::{
    K32QueryWorkingSetEx, PSAPI_WORKING_SET_EX_INFORMATION,
};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

/// The console's page: the title maps its memory in blocks of this size.
const BLOCK: usize = 64 << 10;
/// Protections whose pages can be read.
const READ: u32 = PAGE_READONLY
    | PAGE_READWRITE
    | PAGE_WRITECOPY
    | PAGE_EXECUTE_READ
    | PAGE_EXECUTE_READWRITE
    | PAGE_EXECUTE_WRITECOPY;
/// Where a working-set entry keeps the page's protection (11 bits).
const PROTECTION_SHIFT: u32 = 4;
const PROTECTION_MASK: usize = 0x7ff;

/// Whether the switch is on.
/// Live: each question takes it as it is when it is asked.
pub(super) fn on() -> bool {
    static ON: victory_core::settings::Live =
        victory_core::settings::Live::flag("VICTORY_MAPPED_HOLES");
    static SAID: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    let on = ON.get_bool();
    if on && !SAID.swap(true, std::sync::atomic::Ordering::Relaxed) {
        tracing::info!(
            "VICTORY_MAPPED_HOLES=1: streamed textures count resident mips between two holes; guard pages do not count"
        );
    }
    on
}

/// Whether a region `VirtualQuery` describes counts as there: committed,
/// readable, not a guard page.
pub(super) fn region_counts(info: &MEMORY_BASIC_INFORMATION) -> bool {
    info.State == MEM_COMMIT && info.Protect & READ != 0 && info.Protect & PAGE_GUARD == 0
}

/// Whether a working-set entry's page counts as there: in the working set
/// and not a guard page.
fn entry_counts(flags: usize) -> bool {
    let protection = (flags >> PROTECTION_SHIFT) & PROTECTION_MASK;
    flags & 1 != 0 && protection & PAGE_GUARD as usize == 0
}

/// The bytes of `[start, end)` in blocks after the first whose first page
/// (the block's start) is in the working set, each cut to the range. For a
/// range whose first and last pages are both missing: the first block is
/// left out, as [`super::mapped_bytes`] leaves it out when its first page
/// is missing.
pub(super) fn between(start: usize, end: usize) -> u64 {
    // SAFETY: pseudo-handle for our own process.
    let process = unsafe { GetCurrentProcess() };
    // SAFETY: plain data.
    let mut info: [PSAPI_WORKING_SET_EX_INFORMATION; 256] = unsafe { std::mem::zeroed() };
    let mut spans = [0u64; 256];
    let mut there = 0u64;
    let mut block = (start & !(BLOCK - 1)).saturating_add(BLOCK);
    while block < end {
        let mut n = 0usize;
        while block < end && n < info.len() {
            info[n].VirtualAddress = block as *mut _;
            info[n].VirtualAttributes.Flags = 0;
            let next = block.saturating_add(BLOCK);
            spans[n] = (next.min(end) - block) as u64;
            n += 1;
            block = if next == block { end } else { next };
        }
        // SAFETY: `n` entries of the size given; our own process.
        let asked = unsafe {
            K32QueryWorkingSetEx(
                process,
                info.as_mut_ptr().cast(),
                (n * size_of::<PSAPI_WORKING_SET_EX_INFORMATION>()) as u32,
            )
        } != 0;
        if !asked {
            break;
        }
        for i in 0..n {
            // SAFETY: plain data the query filled in.
            if entry_counts(unsafe { info[i].VirtualAttributes.Flags }) {
                there += spans[i];
            }
        }
    }
    there
}

#[cfg(test)]
mod tests {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_GUARD, PAGE_NOACCESS, PAGE_READWRITE,
        VirtualAlloc, VirtualFree, VirtualProtect,
    };

    const BLOCK: usize = super::BLOCK;

    #[test]
    fn the_switch_is_live() {
        victory_core::settings::check_live("VICTORY_MAPPED_HOLES", "1", super::on, false, true);
    }

    /// A texture of 12 blocks whose first and last blocks are holes and
    /// whose blocks 4..7 are resident: without the switch none of it counts,
    /// with it the resident blocks do; a resident block made a guard page
    /// does not.
    #[test]
    fn resident_mips_between_two_holes_count() {
        const BLOCKS: usize = 12;
        // SAFETY: a reservation owned by this test, released before return;
        // only its own committed pages are written.
        unsafe {
            let base = VirtualAlloc(std::ptr::null(), BLOCKS * BLOCK, MEM_RESERVE, PAGE_NOACCESS)
                .cast::<u8>();
            assert!(!base.is_null());
            let at = base.add(4 * BLOCK);
            assert_eq!(
                VirtualAlloc(at.cast(), 4 * BLOCK, MEM_COMMIT, PAGE_READWRITE),
                at.cast()
            );
            for b in 0..4 {
                at.add(b * BLOCK).write(0x5a);
            }
            let (start, len) = (base as usize + 17, BLOCKS * BLOCK - 17 - 33);
            let end = start + len;
            if std::env::var_os("VICTORY_MAPPED_HOLES").is_none() {
                assert_eq!(super::super::mapped_bytes(start as *const u8, len), 0);
            }
            assert_eq!(super::between(start, end), (4 * BLOCK) as u64);

            // Block 6 becomes a guard page (still in the working set).
            let mut old = 0;
            let guard = base.add(6 * BLOCK);
            assert_ne!(
                VirtualProtect(guard.cast(), 4096, PAGE_READWRITE | PAGE_GUARD, &mut old),
                0
            );
            assert_eq!(super::between(start, end), (3 * BLOCK) as u64);

            // An all-away texture counts none.
            assert_eq!(super::between(base as usize, base as usize + 2 * BLOCK), 0);
            assert_ne!(VirtualFree(base.cast(), 0, MEM_RELEASE), 0);
        }
    }

    #[test]
    fn regions_count_only_when_readable() {
        // SAFETY: plain data.
        let mut info: windows_sys::Win32::System::Memory::MEMORY_BASIC_INFORMATION =
            unsafe { std::mem::zeroed() };
        info.State = MEM_COMMIT;
        info.Protect = PAGE_READWRITE;
        assert!(super::region_counts(&info));
        info.Protect = PAGE_READWRITE | PAGE_GUARD;
        assert!(!super::region_counts(&info));
        info.Protect = PAGE_NOACCESS;
        assert!(!super::region_counts(&info));
        info.State = MEM_RESERVE;
        info.Protect = PAGE_READWRITE;
        assert!(!super::region_counts(&info));
    }
}
