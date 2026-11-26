//! RDNA2 LDS bounds-checking, narrow-wave single-console-wave barrier policy,
//! and scalar quadmask helpers extracted from `lower.rs` to keep `lower.rs`
//! well under the file-size policy limit.

use super::{Bin, Lower, V, pred};

/// The title's barrier retains its memory fences even when group sync
/// would wait for a host wave that never visits it. Explicit overrides
/// keep their old meanings; automatic mode is a targeted compatibility
/// workaround for narrow-wave hosts and is restricted to single-console-wave
/// groups (`group_threads <= console_wave`), so multi-console-wave groups
/// never lose cross-wave synchronization.
pub(in super::super) fn barrier_mode(
    setting: u8,
    host_wave: u32,
    console_wave: u32,
    group_threads: Option<u32>,
    divergent: bool,
    writes_uavs: bool,
) -> u32 {
    let single_console_wave = group_threads.is_some_and(|t| t <= console_wave);
    let waits = match setting {
        0 => false,
        1 => !divergent,
        2 => true,
        _ => !(host_wave != 0 && host_wave < console_wave && single_console_wave && divergent),
    };
    8 | if writes_uavs { 2 } else { 0 } | u32::from(waits)
}

impl<'a> Lower<'a> {
    /// Bounds-checked console LDS dword pointer and in-bounds predicate.
    /// Out-of-bounds console accesses read 0 and drop writes without touching
    /// host wave-exchange or half-meet scratch slots or trapping on OOB GEP.
    pub(super) fn lds_dword(&mut self, addr: V, bytes: u32) -> (V, V) {
        let c = self.cur;
        let o = self.s.m.i32(bytes);
        let at = self.s.m.bin(c, Bin::Add, addr, o);
        let two = self.s.m.i32(2);
        let index = self.s.m.bin(c, Bin::LShr, at, two);
        let len = self.s.m.i32(self.lds_dwords);
        let ok = self.s.m.cmp(c, pred::IULT, index, len);
        let z = self.s.m.i32(0);
        let safe_index = self.s.m.select(c, ok, index, z);
        let lds = self.s.lds();
        (self.s.m.element(c, lds, safe_index), ok)
    }

    /// Combines an optional lane execution guard with an LDS bounds predicate.
    pub(super) fn lds_guard(&mut self, guard: Option<V>, ok: V) -> V {
        match guard {
            Some(g) => self.s.m.bin(self.cur, Bin::And, g, ok),
            None => ok,
        }
    }

    /// Loads an LDS dword and returns 0 when out of bounds.
    pub(super) fn lds_load_checked(&mut self, ptr: V, ok: V) -> V {
        let raw = self.s.m.load(self.cur, ptr);
        let z = self.s.m.i32(0);
        self.s.m.select(self.cur, ok, raw, z)
    }

    /// Packs whether each consecutive group of four pixels has a set bit.
    pub(super) fn quad_mask(&mut self, pixels: V) -> V {
        let c = self.cur;
        let one = self.s.m.i32(1);
        let shifted = self.s.m.bin(c, Bin::LShr, pixels, one);
        let mut mask = self.s.m.bin(c, Bin::Or, pixels, shifted);
        // Fold each nibble into its bottom bit, then gather those bits into
        // the low byte in three stages without needing a 64-bit operation.
        for (distance, keep) in [
            (2, 0x1111_1111),
            (3, 0x0303_0303),
            (6, 0x000f_000f),
            (12, 0x0000_00ff),
        ] {
            let distance = self.s.m.i32(distance);
            let shifted = self.s.m.bin(c, Bin::LShr, mask, distance);
            let combined = self.s.m.bin(c, Bin::Or, mask, shifted);
            let keep = self.s.m.i32(keep);
            mask = self.s.m.bin(c, Bin::And, combined, keep);
        }
        mask
    }
}
