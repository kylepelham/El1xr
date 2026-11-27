//! Where the Rust side's blocks come from: mimalloc (per-thread caches)
//! unless `VICTORY_ALLOC=system` asks for the process heap, which the title
//! and the drivers share. The choice is read once, at the first allocation,
//! before any block is handed out, so every block goes back to the
//! allocator it came from.
//!
//! mimalloc under our allocator is trustno1-coder/VIctory-NVIDIA PR 11
//! (Dastano); the way back to the process heap is from
//! Peppacodes/playablegames-peppa (`cd950293a1`).

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering::Relaxed};

/// 0: not read yet, 1: mimalloc, 2: the process heap.
static CHOICE: AtomicU8 = AtomicU8::new(0);

/// True when `VICTORY_ALLOC=system` was set when the process started.
#[inline]
fn system() -> bool {
    match CHOICE.load(Relaxed) {
        1 => false,
        2 => true,
        _ => read(),
    }
}

/// The first allocation reads the switch; a thread that loses the race
/// takes the winner's answer (both read the same environment).
#[cold]
fn read() -> bool {
    let wanted = if asked_system() { 2 } else { 1 };
    match CHOICE.compare_exchange(0, wanted, Relaxed, Relaxed) {
        Ok(_) => wanted == 2,
        Err(set) => set == 2,
    }
}

/// `name` as a wide string ending in a 0.
const fn wide<const N: usize>(name: &str) -> [u16; N] {
    let bytes = name.as_bytes();
    let mut out = [0u16; N];
    let mut i = 0;
    while i < bytes.len() {
        out[i] = bytes[i] as u16;
        i += 1;
    }
    out
}

/// Whether `value[..n]` spells `"system"` in ASCII, ignoring case.
fn is_system_value(value: &[u16], n: usize) -> bool {
    n == 6
        && value.len() >= 6
        && value[..6]
            .iter()
            .zip(b"system")
            .all(|(&w, &b)| w == u16::from(b) || w == u16::from(b.to_ascii_uppercase()))
}

/// `VICTORY_ALLOC` is `system` (any case). Read without allocating: this
/// runs inside the allocator.
#[cfg(windows)]
fn asked_system() -> bool {
    use windows_sys::Win32::System::Environment::GetEnvironmentVariableW;
    const NAME: [u16; 14] = wide::<14>("VICTORY_ALLOC");
    let mut value = [0u16; 16];
    // SAFETY: NAME ends in a 0 and the buffer's length is the one passed.
    let n =
        unsafe { GetEnvironmentVariableW(NAME.as_ptr(), value.as_mut_ptr(), value.len() as u32) };
    is_system_value(&value, n as usize)
}

#[cfg(not(windows))]
fn asked_system() -> bool {
    let _ = (wide::<1>, is_system_value);
    false
}

/// Which allocator the Rust side uses, for the log.
pub(crate) fn name() -> &'static str {
    if system() {
        "the process heap (VICTORY_ALLOC=system)"
    } else {
        "mimalloc (VICTORY_ALLOC=system gives the process heap)"
    }
}

/// Logs once which allocator the Rust side is using.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn log_once() {
    static TOLD: AtomicBool = AtomicBool::new(false);
    if !TOLD.swap(true, Relaxed) {
        tracing::info!("the Rust side's blocks come from {}", name());
    }
}

/// mimalloc, or the process heap when `VICTORY_ALLOC=system`.
pub struct Chosen;

// SAFETY: every call goes unchanged to one allocator, the same one for the
// whole run (picked before the first block), so each block is released and
// grown by the allocator that made it.
unsafe impl GlobalAlloc for Chosen {
    #[inline]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller's contract, passed on.
        unsafe {
            if system() {
                System.alloc(layout)
            } else {
                mimalloc::MiMalloc.alloc(layout)
            }
        }
    }

    #[inline]
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller's contract, passed on.
        unsafe {
            if system() {
                System.alloc_zeroed(layout)
            } else {
                mimalloc::MiMalloc.alloc_zeroed(layout)
            }
        }
    }

    #[inline]
    unsafe fn dealloc(&self, at: *mut u8, layout: Layout) {
        // SAFETY: the caller's contract, passed on.
        unsafe {
            if system() {
                System.dealloc(at, layout)
            } else {
                mimalloc::MiMalloc.dealloc(at, layout)
            }
        }
    }

    #[inline]
    unsafe fn realloc(&self, at: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        // SAFETY: the caller's contract, passed on.
        unsafe {
            if system() {
                System.realloc(at, layout, size)
            } else {
                mimalloc::MiMalloc.realloc(at, layout, size)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_name_is_a_wide_string_ending_in_a_zero() {
        let name = wide::<14>("VICTORY_ALLOC");
        assert_eq!(name[0], u16::from(b'V'));
        assert_eq!(name[12], u16::from(b'C'));
        assert_eq!(name[13], 0);
    }

    #[test]
    fn system_env_value_matches_case_insensitively() {
        let to_wide = |s: &str| -> Vec<u16> { s.encode_utf16().collect() };
        for yes in ["system", "SYSTEM", "System", "SyStEm"] {
            let w = to_wide(yes);
            assert!(is_system_value(&w, w.len()), "{yes} should match");
        }
        for no in ["", "sys", "system2", "mimalloc", "1"] {
            let w = to_wide(no);
            assert!(!is_system_value(&w, w.len()), "{no} must not match");
        }
    }

    #[test]
    fn blocks_go_back_to_the_allocator_that_made_them() {
        let layout = Layout::from_size_align(4096, 64).unwrap();
        // SAFETY: a non-zero layout; the block is grown and released with
        // the layout it has.
        unsafe {
            let at = Chosen.alloc_zeroed(layout);
            assert!(!at.is_null());
            assert_eq!(at as usize % 64, 0);
            assert!(std::slice::from_raw_parts(at, 4096).iter().all(|&b| b == 0));
            let grown = Chosen.realloc(at, layout, 1 << 20);
            assert!(!grown.is_null());
            Chosen.dealloc(grown, Layout::from_size_align(1 << 20, 64).unwrap());
        }
        assert_ne!(CHOICE.load(Relaxed), 0);
    }
}
