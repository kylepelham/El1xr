//! The list scratch chunks no list holds (`crate::host`'s `SPARE`): how many
//! bytes of them are kept, and which kept chunk a list's small first chunk
//! may be.
//!
//! Taken from fxceless/victory-fork #1367. On an RTX 5080 (Jason's balcony,
//! Series S) the spares sat at the old 128 MiB bound the whole run, all of
//! them 4 MiB chunks, while some 35 small first chunks a frame were made new
//! (about 1 ms each to free, a committed upload buffer) and some 12 chunks
//! of 4 MiB a frame came back past the bound and were freed (about 2 ms
//! each). Measured there: 63-84 ms frames before, 52-56 ms after.

use std::sync::OnceLock;

/// The most bytes of chunks kept while no list holds them, unless
/// `VICTORY_SCRATCH_SPARE_MB` says otherwise ([`spare_most`]); it was
/// 128 MiB. The lists hold some 140 MB of chunks at once on the balcony.
const SPARE_MOST: u64 = 512 << 20;

/// The most bytes of scratch chunks kept while no list holds them:
/// [`SPARE_MOST`], or `VICTORY_SCRATCH_SPARE_MB`.
pub(crate) fn spare_most() -> u64 {
    static MOST: OnceLock<u64> = OnceLock::new();
    *MOST.get_or_init(|| spare_bytes(std::env::var("VICTORY_SCRATCH_SPARE_MB").ok().as_deref()))
}

/// [`spare_most`] for the switch's value `mb`.
fn spare_bytes(mb: Option<&str>) -> u64 {
    mb.and_then(|v| v.trim().parse::<u64>().ok())
        .map_or(SPARE_MOST, |mb| mb << 20)
}

/// Whether a list's small first chunk may be a kept larger one
/// (`VICTORY_SCRATCH_ANY_SIZE`, on; `=0`: only one of its own size).
fn any_size() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_SCRATCH_ANY_SIZE").is_none_or(|v| v != "0"))
}

/// A kept chunk of `size` bytes from `take`; or, for a list's small first
/// chunk (`size` is `first`) while none of that size is kept, a kept one of
/// `chunk` bytes. With its size: the list uses all of it and gives it back
/// at that size.
pub(crate) fn take_kept<T>(
    take: impl FnMut(u64) -> Option<T>,
    size: u64,
    first: u64,
    chunk: u64,
) -> Option<(T, u64)> {
    take_kept_with(take, size, first, chunk, any_size())
}

fn take_kept_with<T>(
    mut take: impl FnMut(u64) -> Option<T>,
    size: u64,
    first: u64,
    chunk: u64,
    any: bool,
) -> Option<(T, u64)> {
    if let Some(kept) = take(size) {
        return Some((kept, size));
    }
    if any && size == first {
        return take(chunk).map(|kept| (kept, chunk));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIRST: u64 = 256 << 10;
    const CHUNK: u64 = 4 << 20;

    /// A small first chunk with none of its size kept is a kept 4 MiB
    /// one, at that size; with the switch off, or for a 4 MiB chunk with
    /// none kept, nothing is taken.
    #[test]
    fn a_small_first_chunk_may_be_a_kept_large_one() {
        let mut kept = vec![CHUNK];
        let mut take = |size: u64| {
            let at = kept.iter().position(|&s| s == size)?;
            Some(kept.remove(at))
        };
        assert_eq!(take_kept_with(&mut take, FIRST, FIRST, CHUNK, false), None);
        assert_eq!(
            take_kept_with(&mut take, FIRST, FIRST, CHUNK, true),
            Some((CHUNK, CHUNK))
        );
        assert_eq!(take_kept_with(&mut take, CHUNK, FIRST, CHUNK, true), None);
    }

    /// A kept chunk of the size asked for comes first.
    #[test]
    fn a_chunk_of_the_size_asked_for_comes_first() {
        let mut kept = vec![CHUNK, FIRST];
        let mut take = |size: u64| {
            let at = kept.iter().position(|&s| s == size)?;
            Some(kept.remove(at))
        };
        assert_eq!(
            take_kept_with(&mut take, FIRST, FIRST, CHUNK, true),
            Some((FIRST, FIRST))
        );
    }

    /// The bound is 512 MiB unless the switch names a number of MiB.
    #[test]
    fn the_spare_bound_follows_its_switch() {
        assert_eq!(spare_bytes(None), 512 << 20);
        assert_eq!(spare_bytes(Some(" 128 ")), 128 << 20);
        assert_eq!(spare_bytes(Some("lots")), 512 << 20);
    }
}
