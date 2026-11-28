//! Reads what is there of a span of title memory, page by page, for the
//! pipelines made from loose pieces (`pipeline::pieces`,
//! `VICTORY_PIPELINE_PIECES=1`). Ported from RipcuBAN/victory-fork c590f21
//! (`host::read_there`), kept out of `host.rs`, which is near the file cap.

use crate::host::read_into;

const PAGE: usize = 4096;

/// Reads what is there of the `dst.len()` bytes of title memory at `src`,
/// zeros for the rest; returns how many bytes were there (whole at once,
/// else by 4 KiB pieces through [`read_into`]).
pub(crate) fn read_there(src: *const u8, dst: &mut [u8]) -> usize {
    if read_into(src, dst) {
        return dst.len();
    }
    let mut got = 0;
    let start = src as usize;
    let mut at = 0;
    while at < dst.len() {
        let end = ((start + at) / PAGE + 1) * PAGE - start;
        let end = end.min(dst.len());
        let piece = &mut dst[at..end];
        if read_into((start + at) as *const u8, piece) {
            got += piece.len();
        } else {
            piece.fill(0);
        }
        at = end;
    }
    got
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::System::Memory::*;

    #[test]
    fn what_is_not_there_reads_as_zero() {
        // SAFETY: a private 8 KiB reservation with only its first page
        // committed; released at the end.
        unsafe {
            let base =
                VirtualAlloc(std::ptr::null(), 2 * PAGE, MEM_RESERVE, PAGE_NOACCESS).cast::<u8>();
            assert!(!base.is_null());
            assert_eq!(
                VirtualAlloc(base.cast(), PAGE, MEM_COMMIT, PAGE_READWRITE),
                base.cast()
            );
            std::ptr::write_bytes(base, 0xAB, PAGE);
            // 64 bytes before the end of the readable page and 64 after it
            let mut dst = [0x11u8; 128];
            assert_eq!(read_there(base.add(PAGE - 64), &mut dst), 64);
            assert!(dst[..64].iter().all(|&b| b == 0xAB));
            assert!(dst[64..].iter().all(|&b| b == 0));
            // all there
            let mut whole = [0u8; 32];
            assert_eq!(read_there(base.add(8), &mut whole), 32);
            assert!(whole.iter().all(|&b| b == 0xAB));
            // nothing there
            let mut none = [0x22u8; 16];
            assert_eq!(read_there(base.add(PAGE + 16), &mut none), 0);
            assert_eq!(none, [0; 16]);
            assert_ne!(VirtualFree(base.cast(), 0, MEM_RELEASE), 0);
        }
    }
}
