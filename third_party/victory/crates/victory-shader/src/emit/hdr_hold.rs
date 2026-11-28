//! The brightest colours kept bright: an infinity the NaN scrub meets is held at the largest
//! half float of its sign instead of made 0 (`VICTORY_NAN_SCRUB_INF=1`), and the float colours
//! a pixel program writes are held to what the console's HDR formats hold
//! (`VICTORY_HDR_CLAMP`). Both change translations: the shader cache keys on them (every
//! `VICTORY_*` name read in this crate is part of `translation_fingerprint`).
//!
//! The title renders light into R9G9B9E5 targets, which end at 65408 on the console. With
//! `VICTORY_HDR_STANDIN=10` the host renders them into R16G16B16A16_FLOAT, where a value past
//! 65504 is stored as an infinity; the title's tonemap makes black of an infinity, and the NaN
//! scrub made 0 of a computed one: the brightest pixels (sun disc, glints on water and glass,
//! bright sky) came out black.
//!
//! Ported from ProjectLeonida/mandy `9c31718bc2` (after felicienradio/cold-project-vi
//! `ec62312`) and `1411b6ca38` part 1 (after aki-gta6/aki-runtime `01f6f31`), GPL-3.0.

use super::{Bin, Lower, V};

/// The bits of 65504, the largest finite 16-bit float.
const HALF_MAX: u32 = 0x477F_E000;

/// The largest value [`hdr_clamp`] lets a colour be: R11G11B10_FLOAT's largest, the smallest
/// maximum of the HDR formats a colour target can have here (R9G9B9E5 ends at 65408, a 16-bit
/// float at 65504).
pub(in crate::emit) const HDR_MAX: f32 = 65024.0;

#[cfg(test)]
thread_local! {
    /// A test's choice for [`scrub_holds_infinity`], for its thread only.
    pub(in crate::emit) static TEST_SCRUB_INF: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
    /// A test's choice for [`hdr_clamp`], for its thread only.
    pub(in crate::emit) static TEST_HDR_CLAMP: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

/// `VICTORY_NAN_SCRUB_INF=1` (off by default): [`Lower::scrub_nan`] holds an infinity at
/// ±65504 instead of making it 0. NaN is 0 either way.
pub(super) fn scrub_holds_infinity() -> bool {
    #[cfg(test)]
    if let Some(on) = TEST_SCRUB_INF.get() {
        return on;
    }
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("VICTORY_NAN_SCRUB_INF").is_ok_and(|v| v.trim() == "1"))
}

/// `VICTORY_HDR_CLAMP=1` (off by default): pixel colour outputs are held to ±[`HDR_MAX`]. Meant
/// for `VICTORY_HDR_STANDIN=10` (the 16-bit float stand-in of R9G9B9E5, where a value past
/// 65504 is stored as an infinity); with the default R11G11B10_FLOAT stand-in it changes only
/// values past that format's own end. Not tied to the stand-in switch here, so that switch stays
/// out of the shader cache key.
pub(super) fn hdr_clamp() -> bool {
    #[cfg(test)]
    if let Some(on) = TEST_HDR_CLAMP.get() {
        return on;
    }
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("VICTORY_HDR_CLAMP").is_ok_and(|v| v.trim() == "1"))
}

impl<'a> Lower<'a> {
    /// [`Lower::scrub_nan`] with [`scrub_holds_infinity`]: a NaN (any pattern) is 0, an
    /// infinity the largest finite half float of its sign. An infinity is the brightest a
    /// colour can be; made 0 it was the darkest (the sun's disc came out black). Only an
    /// infinity is changed, not every value past 65504: 32-bit float images come through here
    /// too, and a large finite number in one (a far distance) is data. Tested on the bits, as
    /// [`Lower::nan_guard`] does, so no driver folds the test away under fast math.
    pub(super) fn scrub_holding_infinity(&mut self, f: V) -> V {
        let c = self.cur;
        let bits = self.as_int(f);
        let mag = self.s.m.i32(0x7FFF_FFFF);
        let abs = self.s.m.bin(c, Bin::And, bits, mag);
        let inf_bits = self.s.m.i32(0x7F80_0000);
        let nan = self.s.m.cmp(c, super::pred::IUGT, abs, inf_bits);
        let inf = self.s.m.cmp(c, super::pred::EQ, abs, inf_bits);
        let sign_bit = self.s.m.i32(0x8000_0000);
        let sign = self.s.m.bin(c, Bin::And, bits, sign_bit);
        let half = self.s.m.i32(HALF_MAX);
        let held = self.s.m.bin(c, Bin::Or, sign, half);
        let zero = self.s.m.i32(0);
        let r = self.s.m.select(c, inf, held, bits);
        let r = self.s.m.select(c, nan, zero, r);
        self.as_float(r)
    }

    /// A colour a pixel program writes, held to [-65024, 65024] when [`hdr_clamp`] says so,
    /// so the 16-bit float stand-in never stores an infinity. The view's format is not known
    /// to the translation, so every float colour output is held, a 32-bit float target's too.
    /// A NaN is left as it is (the NaN scrub or guard before this decides about it).
    pub(super) fn clamp_hdr(&mut self, f: V) -> V {
        if !hdr_clamp() {
            return f;
        }
        let c = self.cur;
        let min = self.s.m.f32((-HDR_MAX).to_bits());
        let max = self.s.m.f32(HDR_MAX.to_bits());
        let lo = self.binary(35, f, min); // FMax
        let held = self.binary(36, lo, max); // FMin
        let nan = self.is_nan_f32(f);
        self.s.m.select(c, nan, f, held)
    }
}
