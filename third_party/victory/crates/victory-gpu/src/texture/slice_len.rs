//! The bytes of a staging buffer one subresource may be written through.
//!
//! The host's `GetCopyableFootprints` does not pad the last row of the last
//! subresource, so its `pitch * rows * depth` can end past the buffer the
//! total sizes. Detiling only needs `(rows - 1) * pitch + row bytes`, which
//! always lies inside, but the slice made for it, and the zero fill of a
//! level that is not the title's (`VICTORY_LEVELS_ON_DEMAND=1`), went to the
//! advertised end: past the mapped buffer. The slice stops at the buffer's
//! end; every byte inside it is written as before.
//!
//! Source: fxceless/victory-fork PR #1471 (CwWwWwW).

/// The bytes of a subresource's footprint at `offset` that lie in a staging
/// buffer of `allocated` bytes.
pub(super) fn subresource_len(
    offset: usize,
    pitch: usize,
    rows: usize,
    depth: usize,
    allocated: usize,
) -> usize {
    let advertised = pitch.saturating_mul(rows).saturating_mul(depth);
    advertised.min(allocated.saturating_sub(offset))
}

#[cfg(test)]
mod tests {
    use super::subresource_len;

    #[test]
    fn the_last_subresource_stops_at_the_buffers_end() {
        // 8 rows of 128 bytes, 256 apart, in the 1,920 bytes the host sizes:
        // advertised, the slice would be 2,048.
        assert_eq!(subresource_len(0, 256, 8, 1, 1920), 1920);
        // Four 1-row levels of a 784-byte buffer: the last begins at 768.
        assert_eq!(subresource_len(768, 256, 1, 1, 784), 16);
        assert_eq!(subresource_len(512, 256, 4, 1, 784), 272);
    }

    #[test]
    fn subresources_inside_the_buffer_keep_their_length() {
        assert_eq!(subresource_len(0, 256, 4, 1, 4096), 1024);
        assert_eq!(subresource_len(1024, 512, 2, 3, 4096), 3072);
        assert_eq!(subresource_len(4096, 256, 1, 1, 4096), 0);
    }

    /// What detiling needs of the last subresource still fits the clamped
    /// slice, so no level the title wrote loses a byte.
    #[test]
    fn detiling_still_fits_the_clamped_slice() {
        let (pitch, rows, row_bytes) = (256, 8, 128);
        let need = (rows - 1) * pitch + row_bytes;
        let allocated = need;
        assert!(subresource_len(0, pitch, rows, 1, allocated) >= need);
    }
}
