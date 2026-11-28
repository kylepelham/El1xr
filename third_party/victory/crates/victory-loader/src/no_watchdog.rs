//! `VICTORY_NO_WATCHDOG=1` (off without it): the title's own hang watchdog
//! no longer ends the run.
//!
//! A thread of the title (game_scarlett_master.exe 1.0.2.1, image offsets)
//! counts down 30 one-second ticks while its main thread does not advance
//! (+0x2215bfd `mov ebp, 1Eh`). At zero it calls a handler (the pointer at
//! +0x71b6348, loaded at +0x2215c27), whose end is a store to address 0
//! (+0x23cf683, `mov dword [0], 4D8BD421h`); if the handler returns or is
//! unset, the store after the call does it (+0x2215c35, then `int3`). Our
//! crash log names that store `write 0x0` at +0x23cf683 (`dev_greps.py`
//! `WATCHDOG_RVAS`): a frame or stall of 30 s on our side, not a fault.
//!
//! Here the load of the handler becomes a jump over the call and both
//! faults to +0x2215c41, the loop's own continuation, and the handler's store
//! becomes a NOP for any other caller. Each site is compared byte for byte
//! first and left alone, said so, if it differs (another build).
//!
//! The hang watch (`VICTORY_HANG_WATCH`) still names a long GPU stall; this
//! only keeps the title from ending itself after one.
//!
//! Source: MAXAWER/victory-merged-amd 739a51785b (from vitutu1/victory-perf,
//! after ZigZagBoi1/ScarlettPort).

use crate::resolution::{Patched, Site};

/// The load of the handler pointer that starts the call-and-fault block.
pub const CALL_BLOCK: Site = Site {
    at: 0x2215c27,
    what: "title watchdog: the handler call and the fault after it",
    // mov rax, [rip + 4FA071Ah]
    bytes: &[0x48, 0x8b, 0x05, 0x1a, 0x07, 0xfa, 0x04],
};

/// `jmp +0x18` (to +0x2215c41), then NOPs.
pub const CALL_BLOCK_CODE: [u8; 7] = [0xeb, 0x18, 0x90, 0x90, 0x90, 0x90, 0x90];

/// The handler's deliberate store to address 0.
pub const HANDLER_FAULT: Site = Site {
    at: 0x23cf683,
    what: "title watchdog: the handler's store to address 0",
    // mov dword [0], 4D8BD421h
    bytes: &[0xc7, 0x04, 0x25, 0, 0, 0, 0, 0x21, 0xd4, 0x8b, 0x4d],
};

/// An eleven-byte NOP.
pub const HANDLER_FAULT_CODE: [u8; 11] = [0x66, 0x66, 0x2e, 0x0f, 0x1f, 0x84, 0, 0, 0, 0, 0];

/// Where the call-and-fault block ends: the loop's continuation.
pub const CONTINUATION: usize = 0x2215c41;

/// Whether `VICTORY_NO_WATCHDOG` asks for it.
pub fn asked() -> bool {
    std::env::var_os("VICTORY_NO_WATCHDOG").is_some_and(|v| !v.is_empty() && v != "0")
}

/// The sites and what goes over each.
pub fn patches() -> [(&'static Site, &'static [u8]); 2] {
    [
        (&CALL_BLOCK, &CALL_BLOCK_CODE),
        (&HANDLER_FAULT, &HANDLER_FAULT_CODE),
    ]
}

/// Disarms the watchdog if asked; a line each in the log and on standard
/// output. Returns how many sites were patched.
///
/// # Safety
/// The image of `size` bytes at `base` stays mapped and no thread runs its
/// code yet.
#[cfg(windows)]
pub unsafe fn apply_asked(base: usize, size: usize) -> usize {
    if !asked() {
        return 0;
    }
    let mut done = 0;
    for (site, code) in patches() {
        // SAFETY: the caller's promise.
        let patched = unsafe { crate::resolution::write(base, size, site, code) };
        let offset = format!("{:#x}", site.at);
        match patched {
            Patched::Done => {
                done += 1;
                tracing::info!(site = %offset, what = site.what, "no watchdog: patched")
            }
            Patched::Differs => {
                tracing::warn!(site = %offset, what = site.what, "no watchdog: not patched, the title's bytes there are not the ones expected (another build?)")
            }
            Patched::Failed(why) => {
                tracing::warn!(site = %offset, what = site.what, why, "no watchdog: not patched")
            }
        }
        println!("no watchdog: {} at title+{offset}: {patched:?}", site.what);
    }
    done
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replacements_fit_the_bytes_compared() {
        for (site, code) in patches() {
            assert_eq!(code.len(), site.bytes.len(), "{}", site.what);
        }
    }

    /// The jump lands right after the fallback's `mov dword [0], 0; int3`
    /// (load 7, `test rax, rax` 3, `je` 2, `call rax` 2, store 11, `int3` 1).
    #[test]
    fn the_jump_lands_on_the_continuation() {
        let next = CALL_BLOCK.at + 2;
        assert_eq!(CALL_BLOCK_CODE[0], 0xeb);
        assert_eq!(next + CALL_BLOCK_CODE[1] as usize, CONTINUATION);
        assert_eq!(CALL_BLOCK.at + 7 + 3 + 2 + 2 + 11 + 1, CONTINUATION);
    }

    /// The handler's store, the one `dev_greps.py` names in crash logs.
    #[test]
    fn the_handler_site_is_the_crash_logs_store() {
        assert_eq!(HANDLER_FAULT.at, 0x23cf683);
        assert_eq!(HANDLER_FAULT.bytes[..3], [0xc7, 0x04, 0x25]);
        assert_eq!(HANDLER_FAULT.bytes[3..7], [0, 0, 0, 0], "address 0");
    }

    /// A fake image holding the expected bytes is patched; one holding other
    /// bytes is left as it was.
    #[test]
    fn only_the_expected_bytes_match() {
        let size = HANDLER_FAULT.at + HANDLER_FAULT.bytes.len();
        let mut image = vec![0xccu8; size];
        image[CALL_BLOCK.at..CALL_BLOCK.at + 7].copy_from_slice(CALL_BLOCK.bytes);
        let base = image.as_ptr() as usize;
        // SAFETY: `image` is `size` bytes at `base`.
        assert_eq!(
            unsafe { crate::resolution::matches(base, size, &CALL_BLOCK) },
            Patched::Done
        );
        // SAFETY: as above.
        assert_eq!(
            unsafe { crate::resolution::matches(base, size, &HANDLER_FAULT) },
            Patched::Differs
        );
        // SAFETY: as above; a smaller image.
        assert_eq!(
            unsafe { crate::resolution::matches(base, CALL_BLOCK.at + 3, &CALL_BLOCK) },
            Patched::Failed("outside the image")
        );
    }
}
