//! (heapcap) What the `VICTORY_HEAP_CAP` path costs in commit, and the gate
//! on the host copies it makes.
//!
//! Part S: two `HEAP_CAP` launches in a row ran out of commit (`=3` died at
//! 96.4 s, free commit 0.0 GB) where launches without it kept 0.3 GB. A
//! packed view of a texture with no host copy makes one (`make`, through
//! `resource::host_copy`), and each host copy is charged to commit: under
//! `=3` the packed range took the first 65,536 live title slots found, read
//! by a shader or not, and its failed makes were tried again at every look
//! (1.59M of them by frame 902), each trying to make a copy.
//!
//! Now a packed view makes a host copy only while the texture room says so
//! (`resource::room`), free commit is above [`FLOOR`], and the frame's
//! [`NEW_PER_FRAME`] new copies are not spent; otherwise it is left null and
//! made again later (with growing waits: `Packed::made`). What the path
//! holds and made is counted for its log lines ([`fields`]).

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// Free commit under which no packed view makes a host copy, and readying
/// (staging) of packed views waits.
pub(crate) const FLOOR: u64 = 3 << 30;
/// New host copies packed views make per frame, at most.
pub(crate) const NEW_PER_FRAME: u64 = 64;

/// New copies left this frame.
static TOKENS: AtomicU64 = AtomicU64::new(NEW_PER_FRAME);
/// Counts: new copies made and their bytes, copies refused by the gate
/// (since the last line); bytes the path holds itself (tables, fault
/// buffer, its own lists).
static NEW_COPIES: AtomicU64 = AtomicU64::new(0);
static NEW_BYTES: AtomicU64 = AtomicU64::new(0);
static NEW_BYTES_TOTAL: AtomicU64 = AtomicU64::new(0);
static GATED: AtomicU64 = AtomicU64::new(0);
static HELD: AtomicU64 = AtomicU64::new(0);

/// Once a frame (`refresh`): the frame's new copies.
pub(crate) fn new_frame() {
    TOKENS.store(NEW_PER_FRAME, Relaxed);
}

/// Whether free commit is under [`FLOOR`].
pub(crate) fn pressed() -> bool {
    crate::commitline::free_commit() < FLOOR
}

/// Whether a packed view may make a new host copy now (`room`: the texture
/// room says there is some; `free`: free commit), taking one of the frame's.
pub(crate) fn may_make(room: bool, free: u64, tokens: &AtomicU64) -> bool {
    room && free >= FLOOR
        && tokens
            .fetch_update(Relaxed, Relaxed, |t| t.checked_sub(1))
            .is_ok()
}

/// The gate for a packed view of title texture `resource`: true when its
/// host copy is there already, or one may be made now.
pub(crate) fn copy_allowed(resource: crate::object::This) -> bool {
    if crate::resource::host_copy_made(resource) {
        return true;
    }
    let ok = may_make(
        crate::resource::room(),
        crate::commitline::free_commit(),
        &TOKENS,
    );
    if !ok {
        GATED.fetch_add(1, Relaxed);
    }
    ok
}

/// A packed view made the host copy of title texture `resource`.
pub(crate) fn made_copy(resource: crate::object::This) {
    NEW_COPIES.fetch_add(1, Relaxed);
    let bytes = crate::resource::host_copy(resource)
        .and_then(|copy| {
            let host = crate::host::host()?;
            // SAFETY: a live resource and the device that made it.
            let desc = unsafe { copy.texture.GetDesc() };
            // SAFETY: as above.
            Some(unsafe { host.device.GetResourceAllocationInfo(0, &[desc]) }.SizeInBytes)
        })
        .unwrap_or(0);
    NEW_BYTES.fetch_add(bytes, Relaxed);
    NEW_BYTES_TOTAL.fetch_add(bytes, Relaxed);
}

/// The path holds `bytes` more (a table, a buffer, its lists).
pub(crate) fn held(bytes: u64) {
    HELD.fetch_add(bytes, Relaxed);
}

/// For a log line: `[commit_mb, new_copies, new_copy_mb, new_copy_mb_total,
/// gated, free_commit_mb]`: the path's own bytes (what it holds and the
/// host copies its packed views made), the copies made and refused since
/// the last line, and the system's free commit.
pub(crate) fn fields() -> [u64; 6] {
    let total = NEW_BYTES_TOTAL.load(Relaxed);
    [
        (HELD.load(Relaxed) + total) >> 20,
        NEW_COPIES.swap(0, Relaxed),
        NEW_BYTES.swap(0, Relaxed) >> 20,
        total >> 20,
        GATED.swap(0, Relaxed),
        crate::commitline::free_commit() >> 20,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_copies_need_room_commit_and_a_token() {
        let tokens = AtomicU64::new(2);
        assert!(!may_make(false, 8 << 30, &tokens), "no room");
        assert!(
            !may_make(true, FLOOR - 1, &tokens),
            "commit under the floor"
        );
        assert_eq!(tokens.load(Relaxed), 2, "refused: no token taken");
        assert!(may_make(true, FLOOR, &tokens));
        assert!(may_make(true, 8 << 30, &tokens));
        assert!(!may_make(true, 8 << 30, &tokens), "the frame's spent");
        assert_eq!(tokens.load(Relaxed), 0);
    }
}
