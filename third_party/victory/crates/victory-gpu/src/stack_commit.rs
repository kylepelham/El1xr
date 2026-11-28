//! (El1xr) `VICTORY_PREPARE_STACK_COMMIT=1` (off by default): the texture
//! prepare threads commit the first MiB of their stack when they start.
//!
//! Windows commits a thread's stack a page at a time, as it first reaches
//! each page; a page it cannot commit is a stack overflow
//! (`STATUS_STACK_OVERFLOW`), however shallow the stack. Batch 5 part I
//! (`VICTORY_RAY_TRACING=on`, `VICTORY_BVH_CARD_WINDOWS_MB=8192`): the PC
//! ran out of commit (5 MB left), allocations failed with `E_OUTOFMEMORY`,
//! and `victory texture prepare 0` died of a stack overflow inside an
//! exception unwind (`RtlpExecuteHandlerForUnwind`, then ucrtbase's
//! `_C_specific_handler`): an unwind takes more stack than the thread's
//! plain work ever did, so it reached pages never committed. Not recursion
//! and not a retry loop: nothing on that path calls itself, and each refusal
//! returns (`Outcome::Done(None)` or `NoRoom`, looked at again by the next
//! frame). Committed while commit is plentiful, the pages stay committed, so
//! an unwind on a full PC finds them there; the threads also reserve 4 MiB
//! (Rust's default is 2 MiB).

use std::sync::OnceLock;

/// The bytes committed at a thread's start.
const COMMIT: usize = 1 << 20;

/// The stack each thread reserves with the switch on.
pub(crate) const RESERVE: usize = 4 << 20;

/// Whether the switch is on (`VICTORY_PREPARE_STACK_COMMIT=1`).
pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("VICTORY_PREPARE_STACK_COMMIT").is_ok_and(|v| v.trim() == "1"))
}

/// Touches [`COMMIT`] bytes of this thread's stack below the caller, so
/// Windows commits those pages now (the frame's stack probe reaches each
/// page in turn, and the writes reach them again).
#[inline(never)]
pub(crate) fn commit() {
    let mut pages = [0u8; COMMIT];
    for at in (0..COMMIT).step_by(4096) {
        pages[at] = 1;
    }
    std::hint::black_box(&mut pages);
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_thread_with_the_reserve_commits_its_first_mib() {
        let t = std::thread::Builder::new()
            .stack_size(super::RESERVE)
            .spawn(|| {
                super::commit();
                true
            })
            .expect("a thread");
        assert!(t.join().expect("no panic"));
    }
}
