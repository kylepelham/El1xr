//! The hit record of the title's reflection traces, as
//! `CS_RaytraceVBuffer_RayBinning_ShaderBinning_Mixed_WithFar`
//! (`6d7fe7745ab9c6d8`) writes it and `CS_RaytracingResolve*` reads it: the
//! CPU reference a host kernel that writes the same records (the hardware
//! glass trace, `VICTORY_NATIVE_RT_GLASS`) is checked against.
//!
//! Read from the title's programs (writer at 0xb40c-0xb544 of the trace;
//! reader at 0x900-0xb50 of `standard_pv:CS_RaytracingResolve_14a87528`):
//! three 32-bit words per ray, at `3 * ray`, `3 * ray + 1`, `3 * ray + 2`
//! of an R32_UINT buffer.
//!
//! - word 0: bits 0-14 a positive half float (`t`, see [`Hit::t`]), bit 15
//!   a flag, bits 16-26 the barycentric `b0` × 2047, bits 27-31 a 5-bit
//!   field (31 when unset).
//! - word 1: bits 0-9 the geometry index, bits 10-16 a 7-bit field, bits
//!   17-31 the instance index. The reader adds the instance's
//!   `InstanceContributionToHitGroupIndex` (its D3D12 instance description,
//!   dword 13) to the geometry index to find the material record (116
//!   bytes), whose first word's bits 22-31 name the resolve program.
//! - word 2: bits 0-10 the barycentric `b1` × 2047, bits 11-27 the
//!   primitive index, bit 28 and bit 30 flags, bit 29 which instance list
//!   (the second structure), bit 31 set on a hit the resolve programs
//!   shade (they skip records without it). The writer sets bit 31 from a
//!   per-hit condition not pinned yet; [`pack_hit`] always sets it.
//!
//! Still unknown: the meaning of word 0's flag and 5-bit field, word 1's
//! 7-bit field, word 2's bits 28 and 30, whether the half float is the hit
//! distance (the trace also writes the distance as a float to a second
//! buffer), and which of `b0`/`b1` is DXR's `barycentrics.x`.
//!
//! A miss writes a distance as a float in word 0 (-1000 for the sky) and 0
//! in word 1, and leaves word 2.

/// What a ray hit, in the record's terms.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Hit {
    /// The half-float field of word 0 (the writer converts a positive
    /// float; the reader takes it back with the sign bit clear).
    pub t: f32,
    /// Word 0 bit 15.
    pub flag15: bool,
    /// Word 0 bits 27-31.
    pub field27: u32,
    pub b0: f32,
    pub b1: f32,
    pub instance: u32,
    pub geometry: u32,
    /// Word 1 bits 10-16.
    pub field10: u32,
    pub primitive: u32,
    /// Word 2 bit 28.
    pub flag28: bool,
    /// Word 2 bit 29: the instance is in the second structure's list.
    pub second: bool,
    /// Word 2 bit 30.
    pub flag30: bool,
}

/// A barycentric as the record keeps it: × 2047, truncated, 11 bits.
fn unorm11(b: f32) -> u32 {
    // `v_mul_legacy_f32` by 2047.0 then `v_cvt_u32_f32` (truncates, clamps
    // below at 0); the result is masked to its field.
    ((b.max(0.0) * 2047.0) as u32) & 0x7ff
}

/// f32 to f16 bits, rounding to nearest even (`v_cvt_f16_f32` with the
/// rounding mode the writer sets), for finite values.
pub fn f16_bits(x: f32) -> u16 {
    let bits = x.to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let exp = ((bits >> 23) & 0xff) as i32;
    let man = bits & 0x7f_ffff;
    if exp == 0xff {
        return sign | 0x7c00 | if man != 0 { 0x200 } else { 0 };
    }
    let e = exp - 127 + 15;
    if e >= 0x1f {
        return sign | 0x7c00;
    }
    if e <= 0 {
        if e < -10 {
            return sign;
        }
        let m = man | 0x80_0000;
        let shift = (14 - e) as u32;
        let half = 1 << (shift - 1);
        let rest = m & ((1 << shift) - 1);
        let mut out = m >> shift;
        if rest > half || (rest == half && out & 1 == 1) {
            out += 1;
        }
        return sign | out as u16;
    }
    let rest = man & 0x1fff;
    let mut out = ((e as u32) << 10) | (man >> 13);
    if rest > 0x1000 || (rest == 0x1000 && out & 1 == 1) {
        out += 1;
    }
    sign | out as u16
}

/// The three words the trace writes for `hit`.
pub fn pack_hit(hit: &Hit) -> [u32; 3] {
    let w0 = (u32::from(f16_bits(hit.t)) & 0x7fff)
        | u32::from(hit.flag15) << 15
        | unorm11(hit.b0) << 16
        | hit.field27 << 27;
    let w1 = (hit.geometry & 0x3ff) | (hit.field10 & 0x7f) << 10 | hit.instance << 17;
    let w2 = unorm11(hit.b1)
        | (hit.primitive << 11) & 0x0fff_f800
        | u32::from(hit.flag28) << 28
        | u32::from(hit.second) << 29
        | u32::from(hit.flag30) << 30
        | 1 << 31;
    [w0, w1, w2]
}

/// The two words a miss writes (word 2 is left as it was).
pub fn pack_miss(distance: f32) -> [u32; 2] {
    [distance.to_bits(), 0]
}

/// What the resolve programs read back from `words`; `None` unless word 2
/// marks a hit.
pub fn unpack(words: [u32; 3]) -> Option<Hit> {
    let [w0, w1, w2] = words;
    if w2 >> 31 == 0 {
        return None;
    }
    Some(Hit {
        t: f16_to_f32((w0 & 0x7fff) as u16),
        flag15: w0 & 0x8000 != 0,
        field27: w0 >> 27,
        b0: ((w0 >> 16) & 0x7ff) as f32 * (1.0 / 2047.0),
        b1: (w2 & 0x7ff) as f32 * (1.0 / 2047.0),
        instance: w1 >> 17,
        geometry: w1 & 0x3ff,
        field10: (w1 >> 10) & 0x7f,
        primitive: (w2 >> 11) & 0x1_ffff,
        flag28: w2 & 1 << 28 != 0,
        second: w2 & 1 << 29 != 0,
        flag30: w2 & 1 << 30 != 0,
    })
}

/// No bin: the trace's "nothing to shade" value for either bin.
pub const NO_BIN: u32 = 0xffff;
/// The pixel image's mark for a value from bin `a`.
pub const BIN_A_MARK: u32 = 0x8000;

/// The two shading bins of a ray, read from the trace's tail (0xb734-0xba88).
/// `a` is counted in the table's UAV at dword 88 (`u42`), `b` in the one at
/// dword 120 (`u40`), one `buffer_atomic_add` per bin and wave (the sum is
/// what a per-ray add of 1 gives); either may be [`NO_BIN`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bins {
    pub a: u32,
    pub b: u32,
}

/// What decides a hit's bins (0x9b7c-0x9c4c of the trace).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BinInputs {
    /// `InstanceContributionToHitGroupIndex + geometry`: the material
    /// record's index (`t339`, 116 bytes); the bin is this, `& 0x3fff`.
    pub material: u32,
    /// The material record's word 0; bits 22-31 == 0x3ff: never shaded.
    pub material_word0: u32,
    /// The hit was taken from the screen instead (a reprojection test on
    /// the previous frame's depth): no bin at all.
    pub from_screen: bool,
    /// Scalar `s8` at 0x9be8 (not traced): when clear, the material goes
    /// to bin `b`.
    pub split: bool,
    /// When `split`: the lane condition `s2` and the per-layer bit of the
    /// frame CB at 0x2c0 (`1 << (group z & 31)`) both set send it to `b`,
    /// anything else to `a`.
    pub layer_to_b: bool,
}

/// The bins of a hit.
pub fn hit_bins(i: &BinInputs) -> Bins {
    let none = Bins {
        a: NO_BIN,
        b: NO_BIN,
    };
    if i.from_screen || i.material_word0 >= 0xffc0_0000 {
        return none;
    }
    let m = i.material & 0x3fff;
    if !i.split || i.layer_to_b {
        Bins { a: NO_BIN, b: m }
    } else {
        Bins { a: m, b: NO_BIN }
    }
}

/// The bins of a miss: `b` is the sky bin, the frame CB's dword at 0x2f0.
pub fn miss_bins(sky: u32) -> Bins {
    Bins { a: NO_BIN, b: sky }
}

/// The value the trace stores at the ray's pixel of the 2D image at table
/// dword 152 (`u36`, the value in all four channels); `None` leaves it.
pub fn pixel_bin(bins: Bins) -> Option<u32> {
    if bins.a != NO_BIN {
        Some(bins.a | BIN_A_MARK)
    } else if bins.b != NO_BIN {
        Some(bins.b)
    } else {
        None
    }
}

/// The trace's per-object stamp in the UAV at table dword 104 (`u50`,
/// stride 8), written on a hit that is not taken from the screen and has a
/// shaded material (0x9c54-0x9d28). Not a bin list: a "seen by a
/// reflection this frame" mark keyed by the instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stamp {
    /// Element index: bits 10-22 of the D3D12 instance description's dword
    /// 12 (`InstanceID`), from the description table of word 2's bit 29.
    pub slot: u32,
    /// Word 0: dword 6 (+24) of the 68-byte record `slot` of `t346`.
    /// Word 1: the frame CB's dword at 0x308 (a frame number).
    pub words: [u32; 2],
}

/// The stamp a hit writes, if any. `instance_dword12` is the instance
/// description's dword 12 (`InstanceID | InstanceMask << 24`); `hint` is a
/// lane value from before the walk's tail (`v13`, not traced) that also
/// allows the stamp; `record6` reads dword 6 of `t346[slot]`.
pub fn hit_stamp(
    instance_dword12: u32,
    material_word0: u32,
    material_word1: u32,
    hint: bool,
    frame: u32,
    record6: impl Fn(u32) -> u32,
) -> Option<Stamp> {
    let wanted = (material_word1 & 0x200 != 0 || hint) && material_word1 & 0x110 == 0;
    let marked = instance_dword12 & 0x4000_0000 != 0 || material_word0 & 1 != 0;
    if !wanted || !marked {
        return None;
    }
    let slot = (instance_dword12 >> 10) & 0x1fff;
    Some(Stamp {
        slot,
        words: [record6(slot), frame],
    })
}

/// The counter changes, pixel value and record words when a ray the title
/// traced as a miss (all-miss today) is found to hit opaque geometry: what
/// the hardware glass kernel must do after the title's pass, so that bins
/// match a title run that hit. The title's miss already counted the ray in
/// the sky bin (`b` = `sky`) and stored `sky` at its pixel.
#[derive(Debug, Clone, PartialEq)]
pub struct MissToHit {
    /// (counter: `false` = `a`/`u42`, `true` = `b`/`u40`, bin, delta).
    pub counters: Vec<(bool, u32, i32)>,
    /// The pixel image's new value. A hit without a bin leaves the pixel in
    /// the title (its earlier clear value, not read yet), but the miss has
    /// already stored `sky` there: [`NO_BIN`] is a guess at that clear.
    pub pixel: u32,
    pub record: [u32; 3],
}

/// See [`MissToHit`].
pub fn miss_to_hit(sky: u32, hit: &Hit, bins: Bins) -> MissToHit {
    let mut counters = Vec::new();
    let was = miss_bins(sky);
    if was.b != NO_BIN {
        counters.push((true, was.b, -1));
    }
    if bins.a != NO_BIN {
        counters.push((false, bins.a, 1));
    }
    if bins.b != NO_BIN {
        counters.push((true, bins.b, 1));
    }
    MissToHit {
        counters,
        pixel: pixel_bin(bins).unwrap_or(NO_BIN),
        record: pack_hit(hit),
    }
}

/// f16 bits to f32.
pub fn f16_to_f32(h: u16) -> f32 {
    let sign = u32::from(h & 0x8000) << 16;
    let exp = u32::from((h >> 10) & 0x1f);
    let man = u32::from(h & 0x3ff);
    let bits = match (exp, man) {
        (0, 0) => sign,
        // Subnormal: m × 2^-24, exact in f32.
        (0, m) => sign | ((m as f32) * f32::powi(2.0, -24)).to_bits(),
        (0x1f, m) => sign | 0x7f80_0000 | m << 13,
        (e, m) => sign | (e + 127 - 15) << 23 | m << 13,
    };
    f32::from_bits(bits)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit() -> Hit {
        Hit {
            t: 12.5,
            flag15: true,
            field27: 3,
            b0: 0.25,
            b1: 0.5,
            instance: 0x5432,
            geometry: 0x2aa,
            field10: 0x55,
            primitive: 0x1_2345,
            flag28: false,
            second: true,
            flag30: false,
        }
    }

    #[test]
    fn fields_sit_where_the_title_puts_them() {
        let [w0, w1, w2] = pack_hit(&hit());
        // 12.5 as a half: 0x4a40.
        assert_eq!(w0 & 0x7fff, 0x4a40);
        assert_eq!(w0 & 0x8000, 0x8000);
        // 0.25 × 2047 = 511.75: 511.
        assert_eq!((w0 >> 16) & 0x7ff, 511);
        assert_eq!(w0 >> 27, 3);
        assert_eq!(w1, 0x2aa | 0x55 << 10 | 0x5432 << 17);
        // 0.5 × 2047 = 1023.5: 1023.
        assert_eq!(w2 & 0x7ff, 1023);
        assert_eq!((w2 >> 11) & 0x1_ffff, 0x1_2345);
        assert_eq!(w2 >> 28, 0b1010);
    }

    #[test]
    fn the_reader_gets_back_what_was_written() {
        let h = hit();
        let back = unpack(pack_hit(&h)).expect("a hit");
        assert_eq!(back.t, 12.5);
        assert_eq!(
            (back.instance, back.geometry, back.primitive),
            (h.instance, h.geometry, h.primitive)
        );
        assert_eq!((back.field10, back.field27), (h.field10, h.field27));
        assert_eq!(
            (back.flag15, back.flag28, back.second, back.flag30),
            (true, false, true, false)
        );
        // Within one step of 1/2047.
        assert!((back.b0 - h.b0).abs() <= 1.0 / 2047.0);
        assert!((back.b1 - h.b1).abs() <= 1.0 / 2047.0);
    }

    #[test]
    fn indices_wider_than_their_fields_are_cut() {
        let h = Hit {
            geometry: 0x7ff,
            primitive: 0x3_ffff,
            field10: 0xff,
            b0: 1.5,
            ..hit()
        };
        let [w0, w1, w2] = pack_hit(&h);
        assert_eq!(w1 & 0x3ff, 0x3ff);
        assert_eq!((w1 >> 10) & 0x7f, 0x7f);
        assert_eq!(
            w1 >> 17,
            0x5432,
            "the geometry index does not reach the instance"
        );
        assert_eq!(
            w2 & 0xf000_0000,
            0xa000_0000,
            "the primitive index does not reach the flags"
        );
        // 1.5 × 2047 = 3070, masked to 11 bits as the writer's field is.
        assert_eq!((w0 >> 16) & 0x7ff, 3070 & 0x7ff);
    }

    #[test]
    fn misses_are_not_resolved() {
        let [w0, w1] = pack_miss(-1000.0);
        assert_eq!(w0, 0xc47a_0000);
        assert_eq!(w1, 0);
        // Word 2 as a miss leaves it (cleared here): no hit bit.
        assert_eq!(unpack([w0, w1, 0]), None);
    }

    fn bin_inputs() -> BinInputs {
        BinInputs {
            material: 0x1_2345,
            material_word0: 0x0040_0000,
            from_screen: false,
            split: false,
            layer_to_b: false,
        }
    }

    #[test]
    fn a_hit_goes_to_one_bin_by_its_material() {
        let i = bin_inputs();
        assert_eq!(
            hit_bins(&i),
            Bins {
                a: NO_BIN,
                b: 0x2345
            }
        );
        let a = hit_bins(&BinInputs { split: true, ..i });
        assert_eq!(
            a,
            Bins {
                a: 0x2345,
                b: NO_BIN
            }
        );
        assert_eq!(pixel_bin(a), Some(0x2345 | BIN_A_MARK));
        let b = BinInputs {
            split: true,
            layer_to_b: true,
            ..i
        };
        assert_eq!(
            hit_bins(&b),
            Bins {
                a: NO_BIN,
                b: 0x2345
            }
        );
        assert_eq!(pixel_bin(hit_bins(&b)), Some(0x2345));
        // Resolve id 0x3ff, or a hit taken from the screen: no bin, the
        // pixel is left.
        let never = BinInputs {
            material_word0: 0xffc0_0000,
            ..i
        };
        assert_eq!(pixel_bin(hit_bins(&never)), None);
        let screen = BinInputs {
            from_screen: true,
            ..i
        };
        assert_eq!(
            hit_bins(&screen),
            Bins {
                a: NO_BIN,
                b: NO_BIN
            }
        );
    }

    #[test]
    fn a_miss_turned_into_a_hit_moves_its_count_off_the_sky() {
        let bins = hit_bins(&bin_inputs());
        let m = miss_to_hit(7, &hit(), bins);
        assert_eq!(m.counters, vec![(true, 7, -1), (true, 0x2345, 1)]);
        assert_eq!(m.pixel, 0x2345);
        assert_eq!(m.record, pack_hit(&hit()));
        // No sky bin: nothing to take back.
        let m = miss_to_hit(NO_BIN, &hit(), Bins { a: 3, b: NO_BIN });
        assert_eq!(m.counters, vec![(false, 3, 1)]);
        assert_eq!(m.pixel, 3 | BIN_A_MARK);
    }

    #[test]
    fn stamps_key_on_the_instance_id() {
        // InstanceID 0x12_3456, mask 0x40 (dword bit 30).
        let dword12 = 0x40 << 24 | 0x12_3456;
        let s = hit_stamp(dword12, 0, 0x200, false, 99, |slot| slot * 2).expect("a stamp");
        assert_eq!(s.slot, (0x12_3456 >> 10) & 0x1fff);
        assert_eq!(s.words, [s.slot * 2, 99]);
        // Material word 1 bit 4 or 8 forbids it; neither 0x200 nor the
        // hint: no stamp; no mask bit 30 and no material bit 0: none.
        assert_eq!(hit_stamp(dword12, 0, 0x210, false, 99, |_| 0), None);
        assert_eq!(hit_stamp(dword12, 0, 0, false, 99, |_| 0), None);
        assert!(hit_stamp(dword12, 0, 0, true, 99, |_| 0).is_some());
        assert_eq!(hit_stamp(0x12_3456, 0, 0x200, false, 99, |_| 0), None);
        assert!(hit_stamp(0x12_3456, 1, 0x200, false, 99, |_| 0).is_some());
    }

    #[test]
    fn halves_round_to_nearest_even() {
        assert_eq!(f16_bits(1.0), 0x3c00);
        assert_eq!(f16_bits(65504.0), 0x7bff);
        assert_eq!(f16_bits(1e6), 0x7c00);
        assert_eq!(f16_bits(-2.0), 0xc000);
        // 1 + 2^-11 is halfway between 1 and 1 + 2^-10: to even (1.0).
        assert_eq!(f16_bits(1.0 + f32::powi(2.0, -11)), 0x3c00);
        // 1 + 3 × 2^-11: halfway, to even (1 + 2^-9).
        assert_eq!(f16_bits(1.0 + 3.0 * f32::powi(2.0, -11)), 0x3c02);
        // Subnormal: 2^-24 is the smallest.
        assert_eq!(f16_bits(f32::powi(2.0, -24)), 0x0001);
        for h in [0x0001u16, 0x03ff, 0x0400, 0x3c00, 0x4a40, 0x7bff] {
            assert_eq!(f16_bits(f16_to_f32(h)), h);
        }
    }
}
