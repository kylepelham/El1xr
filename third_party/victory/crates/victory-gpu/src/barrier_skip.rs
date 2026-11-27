//! (El1xr) `VICTORY_BARRIER_GROUP_SKIP=1`: a title barrier group whose
//! barriers wait for no shader write is not turned into a wait for every
//! shader write.
//!
//! The title's enhanced barriers (`crate::list`'s `barrier_groups`: 16-byte
//! groups `(type, count, barriers)`, the desktop layout) become one global
//! UAV barrier each, 642 a frame and 0.62 ms of the card ("title barrier
//! group" in `VICTORY_GPU_COST`). A group whose every barrier says, in the
//! title's own sync and access words, that nothing before it is waited for
//! (`SyncBefore` none, `AccessBefore` no access) or that it only orders
//! reads after reads, needs no wait: the layouts of host textures are moved
//! by the layer's own state tracking at their next use. Any group with a
//! write before or after it, with a word outside the known bits, or that
//! does not read as the layout, keeps its barrier.
//! `VICTORY_BARRIER_GROUP_SKIP=count`: keeps every barrier, counts what
//! would be skipped and logs the first groups read.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

/// 0 off (default), 1 skip, 2 count only.
fn mode() -> u8 {
    static MODE: OnceLock<u8> = OnceLock::new();
    *MODE.get_or_init(
        || match std::env::var("VICTORY_BARRIER_GROUP_SKIP").as_deref() {
            Ok("1") => 1,
            Ok("count") => 2,
            _ => 0,
        },
    )
}

const GLOBAL: u32 = 0;
const TEXTURE: u32 = 1;
const BUFFER: u32 = 2;

/// `D3D12_BARRIER_SYNC`: none, split, and every known bit.
const SYNC_NONE: u32 = 0;
const SYNC_KNOWN: u32 = 0x01FF_FFFF | 0x8000_0000;

/// `D3D12_BARRIER_ACCESS`: no access, and every known bit.
const NO_ACCESS: u32 = 0x8000_0000;
const ACCESS_KNOWN: u32 = 0x003F_FFFF | NO_ACCESS;
/// Accesses that only read: vertex, constant and index buffers, depth
/// read, shader resource, indirect argument, copy and resolve source,
/// acceleration structure read, shading rate source.
const READS: u32 = 0x1 | 0x2 | 0x4 | 0x40 | 0x80 | 0x200 | 0x800 | 0x2000 | 0x4000 | 0x1_0000;

/// The most barriers a group may hold to be read.
const MOST: u32 = 4096;

/// The four leading words of a barrier: sync before, sync after, access
/// before, access after.
type Words = [u32; 4];

fn reads_only(access: u32) -> bool {
    access != 0 && access & !READS == 0
}

/// Whether a barrier waits for nothing a global UAV barrier stands for.
fn needs_no_wait([sync_before, sync_after, access_before, access_after]: Words) -> bool {
    if sync_before & !SYNC_KNOWN != 0
        || sync_after & !SYNC_KNOWN != 0
        || access_before & !ACCESS_KNOWN != 0
        || access_after & !ACCESS_KNOWN != 0
    {
        return false;
    }
    if sync_before == SYNC_NONE || access_before == NO_ACCESS {
        return true;
    }
    reads_only(access_before) && (access_after == NO_ACCESS || reads_only(access_after))
}

/// The bytes of one barrier of a group's type.
fn stride(kind: u32) -> Option<usize> {
    match kind {
        GLOBAL => Some(16),
        TEXTURE => Some(64),
        BUFFER => Some(40),
        _ => None,
    }
}

/// Whether `count` groups at `groups` need no wait; `None`: they do not
/// read as groups.
///
/// # Safety
/// `groups` points at `count` groups as the title passed them.
unsafe fn skippable(count: u32, groups: *const u8) -> Option<bool> {
    if groups.is_null() || count == 0 || count > MOST {
        return None;
    }
    let mut all = true;
    for g in 0..count as usize {
        // SAFETY: the caller's groups, 16 bytes each.
        let (kind, n, barriers) = unsafe {
            let at = groups.add(g * 16);
            (
                at.cast::<u32>().read_unaligned(),
                at.add(4).cast::<u32>().read_unaligned(),
                at.add(8).cast::<*const u8>().read_unaligned(),
            )
        };
        let step = stride(kind)?;
        if n > MOST || (n > 0 && barriers.is_null()) {
            return None;
        }
        for b in 0..n as usize {
            // SAFETY: the group's barriers, `step` bytes each.
            let words: Words = unsafe {
                let at = barriers.add(b * step).cast::<u32>();
                [0, 1, 2, 3].map(|i| at.add(i).read_unaligned())
            };
            all &= needs_no_wait(words);
        }
    }
    Some(all)
}

/// Groups seen, groups whose barrier is (or would be) skipped, groups that
/// did not read.
static COUNTS: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

/// Whether the title's `count` groups at `groups` are recorded without a
/// wait (only with `VICTORY_BARRIER_GROUP_SKIP=1`).
pub(crate) fn skip(count: u32, groups: *const u8) -> bool {
    let mode = mode();
    if mode == 0 {
        return false;
    }
    // SAFETY: the title's groups, as passed to the hook.
    let read = unsafe { skippable(count, groups) };
    let seen = COUNTS[0].fetch_add(1, Ordering::Relaxed);
    if seen < 16 && mode == 2 {
        log_groups(count, groups, read);
    }
    match read {
        Some(true) => {
            COUNTS[1].fetch_add(1, Ordering::Relaxed);
        }
        None => {
            COUNTS[2].fetch_add(1, Ordering::Relaxed);
        }
        Some(false) => {}
    }
    if seen % 20_000 == 19_999 {
        let [seen, skipped, unread] = COUNTS.each_ref().map(|c| c.swap(0, Ordering::Relaxed));
        tracing::info!(
            "title barrier groups (VICTORY_BARRIER_GROUP_SKIP={}): of the last {seen}, {skipped} wait for no shader write{}, {unread} did not read as groups",
            if mode == 1 { "1" } else { "count" },
            if mode == 1 { " and were skipped" } else { "" },
        );
    }
    mode == 1 && read == Some(true)
}

fn log_groups(count: u32, groups: *const u8, read: Option<bool>) {
    let mut out = String::new();
    if !groups.is_null() && count <= 8 {
        for g in 0..count as usize {
            // SAFETY: as in `skippable`.
            let (kind, n, barriers) = unsafe {
                let at = groups.add(g * 16);
                (
                    at.cast::<u32>().read_unaligned(),
                    at.add(4).cast::<u32>().read_unaligned(),
                    at.add(8).cast::<*const u8>().read_unaligned(),
                )
            };
            out += &format!(" [type {kind} x{n}");
            if let Some(step) = stride(kind).filter(|_| !barriers.is_null() && n <= 8) {
                for b in 0..n as usize {
                    // SAFETY: as in `skippable`.
                    let w: Words = unsafe {
                        let at = barriers.add(b * step).cast::<u32>();
                        [0, 1, 2, 3].map(|i| at.add(i).read_unaligned())
                    };
                    out += &format!(" ({:#x} {:#x} {:#x} {:#x})", w[0], w[1], w[2], w[3]);
                }
            }
            out += "]";
        }
    }
    tracing::info!("title barrier group read as {read:?}:{out}");
}

#[cfg(test)]
mod tests {
    use super::*;

    const UAV: u32 = 0x10;
    const SRV: u32 = 0x80;
    const RT: u32 = 0x8;
    const COMPUTE: u32 = 0x80;
    const PIXEL: u32 = 0x10;

    #[test]
    fn only_barriers_without_a_write_need_no_wait() {
        // Nothing before: a discard or a first use.
        assert!(needs_no_wait([SYNC_NONE, PIXEL, NO_ACCESS, RT]));
        assert!(needs_no_wait([COMPUTE, PIXEL, NO_ACCESS, SRV]));
        // Reads after reads.
        assert!(needs_no_wait([COMPUTE, PIXEL, SRV, SRV | 0x200]));
        assert!(needs_no_wait([PIXEL, COMPUTE, SRV, NO_ACCESS]));
        // A write before, or a write after reads, or common (any access).
        assert!(!needs_no_wait([COMPUTE, PIXEL, UAV, SRV]));
        assert!(!needs_no_wait([PIXEL, PIXEL, RT, SRV]));
        assert!(!needs_no_wait([COMPUTE, COMPUTE, SRV, UAV]));
        assert!(!needs_no_wait([0x200, PIXEL, 0x400, SRV]));
        assert!(!needs_no_wait([COMPUTE, PIXEL, 0, SRV]));
        assert!(!needs_no_wait([COMPUTE, PIXEL, SRV, 0]));
        // Split ends wait for what their beginning stood behind.
        assert!(!needs_no_wait([0x8000_0000, PIXEL, UAV, SRV]));
        // Unknown bits: kept.
        assert!(!needs_no_wait([0x4000_0000, PIXEL, SRV, SRV]));
        assert!(!needs_no_wait([SYNC_NONE, PIXEL, 0x0100_0000, SRV]));
    }

    fn group(kind: u32, barriers: &[u8], n: u32) -> [u8; 16] {
        let mut g = [0u8; 16];
        g[..4].copy_from_slice(&kind.to_le_bytes());
        g[4..8].copy_from_slice(&n.to_le_bytes());
        g[8..].copy_from_slice(&(barriers.as_ptr() as usize as u64).to_le_bytes());
        g
    }

    fn barriers(step: usize, words: &[Words]) -> Vec<u8> {
        let mut out = vec![0xEEu8; step * words.len()];
        for (i, w) in words.iter().enumerate() {
            for (j, word) in w.iter().enumerate() {
                out[i * step + j * 4..i * step + j * 4 + 4].copy_from_slice(&word.to_le_bytes());
            }
        }
        out
    }

    #[test]
    fn groups_read_at_their_strides() {
        let reads = [COMPUTE, PIXEL, SRV, SRV];
        let write = [COMPUTE, PIXEL, UAV, SRV];
        let textures = barriers(64, &[reads, reads]);
        let buffers = barriers(40, &[reads, write]);
        let global = barriers(16, &[reads]);
        let groups: Vec<u8> = [group(TEXTURE, &textures, 2), group(GLOBAL, &global, 1)].concat();
        // SAFETY: groups and barriers built above.
        assert_eq!(unsafe { skippable(2, groups.as_ptr()) }, Some(true));
        let groups: Vec<u8> = [group(TEXTURE, &textures, 2), group(BUFFER, &buffers, 2)].concat();
        // SAFETY: as above.
        assert_eq!(unsafe { skippable(2, groups.as_ptr()) }, Some(false));
        // Only the first buffer barrier: reads.
        let groups: Vec<u8> = group(BUFFER, &buffers, 1).to_vec();
        // SAFETY: as above.
        assert_eq!(unsafe { skippable(1, groups.as_ptr()) }, Some(true));
        // An unknown type, a null pointer, no groups: not read.
        let groups: Vec<u8> = group(7, &buffers, 1).to_vec();
        // SAFETY: as above.
        assert_eq!(unsafe { skippable(1, groups.as_ptr()) }, None);
        let mut groups = group(BUFFER, &buffers, 1);
        groups[8..].fill(0);
        // SAFETY: as above.
        assert_eq!(unsafe { skippable(1, groups.as_ptr()) }, None);
        // SAFETY: null is refused before it is read.
        assert_eq!(unsafe { skippable(0, std::ptr::null()) }, None);
    }
}
