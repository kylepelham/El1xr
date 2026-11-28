//! Two of [`super::isa_fixes`]' buffer fixes, each behind its switch (off by default):
//!
//! - `VICTORY_NORM_NEAREST=1` ([`Lower::norm_nearest`]): a UNORM or SNORM field of up to 16
//!   bits loads as the float nearest its exact quotient, as the reference interpreter's
//!   `ratio_bits` (`PrototypeScarlett/emu/rdna2_buffer_format.py`) and the console's buffer
//!   unit give it, built in integers. The host's `fdiv` may be an ulp off, and a title that
//!   truncates `x * (2^n - 1)` back to the code it stored then reads the code below.
//! - `VICTORY_TBUFFER_D16=1` ([`Lower::d16_pack`]): a `tbuffer_load_format_d16_*` result packs
//!   two 16-bit channels a VGPR, as `rdna2_buffer_d16.py` does.

use super::{Bin, Cast, Inst, Lower, Numeric, Reg, V, Why, buffer_format, pred};

impl<'a> Lower<'a> {
    /// The bits of the float nearest `a / d`, `d = 2^p - 1`, `1 <= p <= 16`, for `0 <= a <= d`
    /// (1.0 for `a = d`, 0 for 0).
    ///
    /// In base `2^p`, `a / d` is `0.aaaa...`: the `p`-bit pattern of `a` repeated without end
    /// (`a / (2^p - 1) = a 2^-p (1 + 2^-p + 2^-2p + ...)`). With `lz` the pattern's leading
    /// zeros, the quotient is `0.1fff... 2^-lz`, its bits after the leading zeros the pattern
    /// rotated left by `lz` and repeated. The first 24 of them are the significand; it rounds
    /// up when the 25th is set, never a tie (the bits after it are the pattern again, and
    /// `a != 0`). Adding that bit to the assembled float's bits carries into the exponent when
    /// the significand overflows. Checked against `ratio_bits` for every code of every width
    /// from 1 to 16.
    pub(super) fn norm_nearest(&mut self, a: V, p: u32) -> V {
        debug_assert!((1..=16).contains(&p), "{p}-bit quotient");
        let c = self.cur;
        let k = |l: &mut Self, x: u32| l.s.m.i32(x);
        let (zero, one) = (k(self, 0), k(self, 1));
        // The highest bit of `a | 1` is `a`'s for every `a >= 1`; for 0 it keeps the shifts
        // below in range (the result is chosen away).
        let a1 = self.s.m.bin(c, Bin::Or, a, one);
        let f = self.s.m.cast(c, Cast::UiToFp, a1, self.s.t.f32);
        let f = self.as_int(f);
        let k23 = k(self, 23);
        let e = self.s.m.bin(c, Bin::LShr, f, k23);
        // lz = p - 1 - (e - 127), 0 to p - 1.
        let top = k(self, p - 1 + 127);
        let lz = self.s.m.bin(c, Bin::Sub, top, e);
        let pv = k(self, p);
        let back = self.s.m.bin(c, Bin::Sub, pv, lz);
        let left = self.s.m.bin(c, Bin::Shl, a, lz);
        let right = self.s.m.bin(c, Bin::LShr, a, back);
        let rot = self.s.m.bin(c, Bin::Or, left, right);
        let mask = k(self, (1u32 << p) - 1);
        let rot = self.s.m.bin(c, Bin::And, rot, mask);
        // The first 32 bits of the rotated pattern repeated.
        let mut bits = None;
        let mut at = 32 - p as i32;
        while at > -(p as i32) {
            let part = match at {
                0 => rot,
                1.. => {
                    let s = k(self, at as u32);
                    self.s.m.bin(c, Bin::Shl, rot, s)
                }
                _ => {
                    let s = k(self, at.unsigned_abs());
                    self.s.m.bin(c, Bin::LShr, rot, s)
                }
            };
            bits = Some(match bits {
                Some(b) => self.s.m.bin(c, Bin::Or, b, part),
                None => part,
            });
            at -= p as i32;
        }
        let bits = bits.expect("at least one part");
        let (k7, k8) = (k(self, 7), k(self, 8));
        let up = self.s.m.bin(c, Bin::LShr, bits, k7);
        let up = self.s.m.bin(c, Bin::And, up, one);
        let mant = self.s.m.bin(c, Bin::LShr, bits, k8);
        // The significand's leading bit adds 1 to the exponent field: (126 - lz) << 23 in all.
        let base = k(self, 125);
        let exp = self.s.m.bin(c, Bin::Sub, base, lz);
        let exp = self.s.m.bin(c, Bin::Shl, exp, k23);
        let r = self.s.m.bin(c, Bin::Add, exp, mant);
        let r = self.s.m.bin(c, Bin::Add, r, up);
        let d = k(self, (1u32 << p) - 1);
        let full = self.s.m.cmp(c, pred::IUGE, a, d);
        let unit = k(self, 1.0f32.to_bits());
        let r = self.s.m.select(c, full, unit, r);
        let none = self.s.m.cmp(c, pred::EQ, a, zero);
        self.s.m.select(c, none, zero, r)
    }

    /// [`Self::norm_nearest`] for a signed field already sign-extended (`s`) of `w` bits:
    /// the magnitude's quotient over `2^(w-1) - 1` with the sign; the two lowest codes -1.0.
    pub(super) fn snorm_nearest(&mut self, s: V, w: u32) -> V {
        let c = self.cur;
        let zero = self.s.m.i32(0);
        let neg = self.s.m.cmp(c, pred::SLT, s, zero);
        let minus = self.s.m.bin(c, Bin::Sub, zero, s);
        let m = self.s.m.select(c, neg, minus, s);
        let d = self.s.m.i32((1u32 << (w - 1)) - 1);
        let over = self.s.m.cmp(c, pred::IUGT, m, d);
        let m = self.s.m.select(c, over, d, m);
        let q = self.norm_nearest(m, w - 1);
        let sign = self.s.m.i32(0x8000_0000);
        let sign = self.s.m.select(c, neg, sign, zero);
        self.s.m.bin(c, Bin::Or, q, sign)
    }

    /// The channels of a format load ([`Self::format_load`]'s, as integers) as d16 data: 16
    /// bits each, two a VGPR from `reg`, low half first. Integer formats keep their low 16
    /// bits, the others become f16. An odd last channel fills the low half of its VGPR and
    /// keeps the high half (`rdna2_buffer_d16.py`: "A load writes only the halves it fills").
    // shortcut: a float result narrows to f16 as the host's `legacyF32ToF16` rounds, where
    // the reference rounds a normalized quotient once (four 16-bit codes differ by an ulp of
    // f16) and a 32-bit float toward zero; a 16-bit float field comes back as it was.
    pub(super) fn d16_pack(
        &mut self,
        inst: &Inst,
        reg: u8,
        format: u8,
        xs: Vec<V>,
    ) -> Result<Vec<V>, Why> {
        let (_, numeric) = buffer_format(format).ok_or("a buffer format")?;
        let c = self.cur;
        let low = self.s.m.i32(0xFFFF);
        let mut halves = Vec::new();
        for x in xs {
            halves.push(if matches!(numeric, Numeric::Uint | Numeric::Sint) {
                self.s.m.bin(c, Bin::And, x, low)
            } else {
                let (f, i32t) = (self.as_float(x), self.s.t.i32);
                self.s
                    .op(c, "legacyF32ToF16", super::Attr::ReadNone, i32t, 130, &[f])
            });
        }
        let k16 = self.s.m.i32(16);
        let mut out = Vec::new();
        for (k, pair) in halves.chunks(2).enumerate() {
            out.push(match *pair {
                [lo, hi] => {
                    let hi = self.s.m.bin(c, Bin::Shl, hi, k16);
                    self.s.m.bin(c, Bin::Or, lo, hi)
                }
                [lo] => {
                    let old = match inst.read(Reg::V(reg + k as u8)) {
                        Some(v) => self.i32_of(v)?,
                        None => self.s.m.i32(0),
                    };
                    let high = self.s.m.i32(0xFFFF_0000);
                    let old = self.s.m.bin(c, Bin::And, old, high);
                    self.s.m.bin(c, Bin::Or, old, lo)
                }
                _ => unreachable!("chunks of one or two"),
            });
        }
        Ok(out)
    }
}
