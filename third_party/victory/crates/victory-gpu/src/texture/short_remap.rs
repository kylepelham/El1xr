//! `VICTORY_SHORT_REMAP_LOCK=1` (off by default): a texture read through
//! the copy (a partly mapped one, or every one with `VICTORY_TEXTURE_COPY=1`)
//! lets the runtime remap title memory again as soon as its bytes are in the
//! thread's read buffer, not after they are hashed and detiled. A read in
//! place keeps the guard throughout: it needs the pages to stay mapped.
//!
//! Source: 3cvil/M1LK b29bcff394 (after kelvaroq native/patches/0004).

use crate::watch::Reading;

/// Whether the switch is on.
/// Live: each read takes it as it is when it begins.
pub(super) fn on() -> bool {
    static ON: victory_core::settings::Live = victory_core::settings::Live::flag("VICTORY_SHORT_REMAP_LOCK");
    static SAID: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    let on = ON.get_bool();
    if on && !SAID.swap(true, std::sync::atomic::Ordering::Relaxed) {
        tracing::info!(
            "VICTORY_SHORT_REMAP_LOCK=1: texture reads through the copy release the remap lock right after the copy"
        );
    }
    on
}

/// [`super::with_title`] for a caller that holds `mapping`: with
/// `release_early` the guard is dropped once the bytes are copied, before
/// `consume` hashes and detiles them; otherwise it is held until `consume`
/// returns, as before.
pub(super) fn with_title_snapshot<R>(
    memory: *const u8,
    size: usize,
    mapping: Reading,
    release_early: bool,
    consume: impl FnOnce(&[u8]) -> R,
) -> R {
    if !release_early {
        let result = super::with_title(memory, size, consume);
        drop(mapping);
        return result;
    }
    super::with_title(memory, size, |copied| {
        drop(mapping);
        consume(copied)
    })
}

#[cfg(test)]
mod tests {
    use super::with_title_snapshot;

    #[test]
    fn the_switch_is_live() {
        victory_core::settings::check_live("VICTORY_SHORT_REMAP_LOCK", "1", super::on, false, true);
    }
    use crate::watch::{title_memory_reading, title_memory_remapping};

    /// The bytes are those of the memory at the copy, partly mapped or not;
    /// with the switch a remap goes on while the bytes are consumed,
    /// without it the remap waits until they are.
    #[test]
    fn snapshot_preserves_the_bytes_and_the_chosen_remap_lock_lifetime() {
        use std::sync::mpsc;
        use std::time::Duration;
        use windows_sys::Win32::System::Memory::*;
        // No lazy test meanwhile: the remap on a thread of this test's own
        // would bar every page from arming in it (#78).
        let _lazy = crate::shadow::lazy::test_disable_lazy_copyback();

        const PAGE: usize = 4096;
        for (sparse, release_early) in [(false, false), (false, true), (true, false), (true, true)]
        {
            // SAFETY: this test owns the entire allocation. Only the writer
            // remaps it, and only while holding the remapping guard.
            unsafe {
                let base = VirtualAlloc(
                    std::ptr::null(),
                    3 * PAGE,
                    MEM_RESERVE | MEM_COMMIT,
                    PAGE_READWRITE,
                )
                .cast::<u8>();
                assert!(!base.is_null());
                base.write_bytes(0x31, 3 * PAGE);
                if sparse {
                    assert_ne!(VirtualFree(base.add(PAGE).cast(), PAGE, MEM_DECOMMIT), 0);
                }
                let mut original = vec![0x31; 3 * PAGE];
                if sparse {
                    original[PAGE..2 * PAGE].fill(0);
                }
                let address = base as usize;
                std::thread::scope(|scope| {
                    let (begin, wait) = mpsc::channel();
                    let (attempt, started) = mpsc::channel();
                    let (finished, remapped) = mpsc::channel();
                    scope.spawn(move || {
                        wait.recv().unwrap();
                        attempt.send(()).unwrap();
                        let _changing = title_memory_remapping();
                        let first = address as *mut u8;
                        assert_ne!(VirtualFree(first.cast(), PAGE, MEM_DECOMMIT), 0);
                        assert_eq!(
                            VirtualAlloc(first.cast(), PAGE, MEM_COMMIT, PAGE_READWRITE),
                            first.cast()
                        );
                        first.write_bytes(0xa7, PAGE);
                        let _ = finished.send(());
                    });
                    with_title_snapshot(
                        base,
                        3 * PAGE,
                        title_memory_reading(),
                        release_early,
                        |copied| {
                            begin.send(()).unwrap();
                            started.recv().unwrap();
                            if release_early {
                                remapped
                                    .recv_timeout(Duration::from_secs(5))
                                    .expect("writer blocked by the snapshot consumer");
                            } else {
                                std::thread::sleep(Duration::from_millis(20));
                                assert_eq!(remapped.try_recv(), Err(mpsc::TryRecvError::Empty));
                            }
                            assert_eq!(copied, original);
                        },
                    );
                });
                original[..PAGE].fill(0xa7);
                with_title_snapshot(
                    base,
                    3 * PAGE,
                    title_memory_reading(),
                    release_early,
                    |fresh| assert_eq!(fresh, original),
                );
                assert_ne!(VirtualFree(base.cast(), 0, MEM_RELEASE), 0);
            }
        }
    }
}
