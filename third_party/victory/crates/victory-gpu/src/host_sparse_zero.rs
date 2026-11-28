//! (Peppa `13ed564e9c`) `VICTORY_SPARSE_ZERO_SAFE=1`: when a read of title
//! memory into host memory meets pages that are not there, the zeros that
//! stand in for them go only where the host destination is committed.
//! [`super::read_sparse`] zeroed the whole piece with a plain write, which
//! faults on a destination committed only in part (their run RTTR: a 9 MB
//! fill into a copy committed for its first 0.5 MB, a write fault on
//! reserved memory, the game ended). With the switch, a destination that is
//! not all committed is zeroed 4 KiB at a time through the kernel's write,
//! which leaves out the pages that are not there; the same goes for the
//! 4 KiB zeros of [`super::read_pieces`]. Off (default): as before.
//!
//! Not in the cache key: no shader or pipeline output.

use std::sync::OnceLock;

const PAGE: usize = 4 << 10;

/// Whether the switch is on (read once).
pub(super) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_SPARSE_ZERO_SAFE").is_some_and(|v| v == "1"))
}

/// Zeros `[to, to + n)` of host memory: with a plain write if it is all
/// committed, else page by page where the kernel can write.
pub(super) fn zero(to: *mut u8, n: usize) {
    if n == 0 {
        return;
    }
    if crate::texture::committed(to, n) {
        // SAFETY: committed host memory of `n` bytes, ours to write.
        unsafe { to.write_bytes(0, n) };
        return;
    }
    super::for_pieces(to as usize, n, PAGE, |at, k| {
        zero_page(at, k);
    });
}

/// Zeros at most a page at `at` through the kernel; false if it is not
/// there or not writable (then nothing is written).
fn zero_page(at: usize, n: usize) -> bool {
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    static ZEROS: [u8; PAGE] = [0; PAGE];
    debug_assert!(n <= PAGE);
    let mut done = 0;
    // SAFETY: the source is our own zeros; the kernel checks the destination
    // and writes nothing it cannot.
    unsafe {
        super::NtWriteVirtualMemory(
            GetCurrentProcess(),
            at as _,
            ZEROS.as_ptr().cast(),
            n.min(PAGE),
            &mut done,
        ) >= 0
    }
}

#[cfg(test)]
mod tests {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_NOACCESS, PAGE_READWRITE, VirtualAlloc,
        VirtualFree,
    };

    /// A read from memory that is not there into a destination committed
    /// only for its first page: the first page is zeroed and the rest is
    /// left out. Without the switch the plain write of
    /// [`super::super::read_sparse`] faults on the second page.
    #[test]
    fn zeros_into_a_partly_committed_destination_stop_at_its_pages() {
        const LEN: usize = 4 * super::PAGE;
        // SAFETY: plain reservations of our own, released at the end.
        unsafe {
            let src = VirtualAlloc(std::ptr::null(), LEN, MEM_RESERVE, PAGE_NOACCESS).cast::<u8>();
            let dst = VirtualAlloc(std::ptr::null(), LEN, MEM_RESERVE, PAGE_NOACCESS).cast::<u8>();
            assert!(!src.is_null() && !dst.is_null());
            assert_eq!(
                VirtualAlloc(dst.cast(), super::PAGE, MEM_COMMIT, PAGE_READWRITE),
                dst.cast()
            );
            dst.write_bytes(0xab, super::PAGE);
            super::super::read_sparse_with(src, dst, LEN, true);
            assert!(
                std::slice::from_raw_parts(dst, super::PAGE)
                    .iter()
                    .all(|&b| b == 0)
            );
            VirtualFree(src.cast(), 0, MEM_RELEASE);
            VirtualFree(dst.cast(), 0, MEM_RELEASE);
        }
    }

    #[test]
    fn a_committed_destination_is_zeroed_whole() {
        let mut bytes = vec![0x5au8; 3 * super::PAGE + 17];
        super::zero(bytes.as_mut_ptr(), bytes.len());
        assert!(bytes.iter().all(|&b| b == 0));
    }
}
