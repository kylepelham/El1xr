//! Typed buffer loads that move 16-bit halves without the d16 form: a
//! format load of whole dwords whose result the program reads only one
//! half of, as a half float, moves 16-bit integers, like a d16 access
//! ([`super::super::HostShader::half_views`]).

use std::collections::{HashMap, HashSet};

use super::{
    Atom, Attr, Bin, Bool, EXEC, Fields, Inst, LDS_LOOSE, LaneExit, Lower, NULL, Operand, Program,
    Reg, ResourceType, Resources, Role, RootType, Site, Source, Structured, V, Value, Why, pred,
};

impl<'a> Lower<'a> {
    /// Whether every use of `value`, a typed buffer load's result, reads
    /// one 16-bit half of it as a half float: a `v_fma_mix*` source with
    /// its `op_sel_hi` bit set, or `v_cvt_f32_f16`. A program that takes a
    /// format load apart like that reads a 16-bit integer view on the
    /// console (the load gives the zero-extended integer, whose bits are a
    /// half; a float view's value it would not take apart). A use is a
    /// read as an operand ([`crate::ir::operand_reads`]): a later write of
    /// the register reads the value only to keep it in the lanes EXEC
    /// leaves alone, and a phi carries it on, its own uses counting. False
    /// without a use.
    pub(super) fn read_only_as_halves(&self, value: Value) -> bool {
        let p = self.x.p;
        let mut used = false;
        let mut seen = HashSet::from([value]);
        let mut todo = vec![value];
        while let Some(value) = todo.pop() {
            for b in &p.blocks {
                for phi in b.phis.iter().filter(|phi| phi.args.contains(&value)) {
                    if seen.insert(phi.value) {
                        todo.push(phi.value);
                    }
                }
                for i in &b.insts {
                    if !i.reads.iter().any(|r| r.1 == value) {
                        continue;
                    }
                    let operands = crate::ir::operand_reads(p, i);
                    let regs: Vec<Reg> = i
                        .reads
                        .iter()
                        .filter(|r| r.1 == value && operands.contains(&r.0))
                        .map(|r| r.0)
                        .collect();
                    if regs.is_empty() {
                        continue;
                    }
                    used = true;
                    let Fields::Valu(v) = &i.fields else {
                        return false;
                    };
                    match i.name() {
                        "v_cvt_f32_f16" => {}
                        "v_fma_mix_f32" | "v_fma_mixlo_f16" | "v_fma_mixhi_f16" => {
                            let whole = v.src.iter().enumerate().any(|(k, op)| {
                                matches!(*op, Operand::Vector { reg, .. } if regs.contains(&Reg::V(reg)))
                                    && v.opsel_hi >> k & 1 == 0
                            });
                            if whole {
                                return false;
                            }
                        }
                        _ => return false,
                    }
                }
            }
        }
        used
    }

    /// Whether `inst` is a format load of whole dwords (not d16) whose
    /// every channel is read only as halves ([`Self::read_only_as_halves`]).
    pub(super) fn format_load_read_as_halves(&self, inst: &Inst) -> bool {
        let Fields::Buffer {
            data: Some(Operand::Vector { reg, .. }),
            ..
        } = inst.fields
        else {
            return false;
        };
        let name = inst.name();
        let count = match name
            .strip_prefix("buffer_load_format_")
            .or(name.strip_prefix("tbuffer_load_format_"))
        {
            Some("x") => 1,
            Some("xy") => 2,
            Some("xyz") => 3,
            Some("xyzw") => 4,
            _ => return false,
        };
        (0..count).all(|k| {
            inst.write(Reg::V(reg + k))
                .is_some_and(|v| self.read_only_as_halves(v))
        })
    }

    /// The table places ([`Self::table_place`]) every access through which
    /// moves halves: a d16 access, or a format load read only as halves
    /// ([`Self::format_load_read_as_halves`]). A place the program reaches
    /// otherwise too is left out (a format load of numbers beside one of
    /// halves, a raw or scalar load, a descriptor chosen by control flow),
    /// and so is every place of a table the program also picks from at run
    /// time: what the program makes of such a view is not halves alone.
    pub(super) fn places_read_as_halves(&self) -> std::collections::BTreeSet<(u8, u32)> {
        let mut halves = std::collections::BTreeSet::new();
        let mut other = std::collections::BTreeSet::new();
        let mut picked = HashSet::new();
        for d in &self.x.res.descriptors {
            if d.role == Role::Sampler {
                continue;
            }
            let inst = self
                .x
                .p
                .blocks
                .get(d.site.0)
                .and_then(|b| b.insts.get(d.site.1));
            let half = matches!(d.source, Source::Loaded { .. })
                && inst.is_some_and(|i| {
                    (i.name().starts_with("buffer_") && i.name().contains("_format_d16_"))
                        || self.format_load_read_as_halves(i)
                });
            for leaf in d.source.leaves() {
                let Source::Loaded { load, .. } = leaf else {
                    continue;
                };
                let Some(a) = self.scalar.get(load) else {
                    continue;
                };
                if a.dynamic.is_some() {
                    if let Some(rp) = self.x.native.layout.get(a.param)
                        && rp.root == RootType::Table
                    {
                        picked.insert(rp.slot);
                    }
                } else if let Some(place) = self.table_place(leaf) {
                    if half {
                        halves.insert(place);
                    } else {
                        other.insert(place);
                    }
                }
            }
        }
        halves.retain(|p| !other.contains(p) && !picked.contains(&p.0));
        halves
    }
}

/// `VICTORY_NAN_SCRUB` (on unless `0`): see [`Lower::scrub_nan`].
pub(crate) fn nan_scrub() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_NAN_SCRUB").is_none_or(|v| v != "0"))
}

impl<'a> Lower<'a> {
    /// `VICTORY_NAN_GUARD` (on unless `0`): a float a pixel program exports to a colour
    /// target, or a store writes to a typed image or buffer, with NaN made 0 and an infinity
    /// ±65504, so one bad pixel does not stay in a history the title blends into each frame.
    /// `strict` (float targets): every NaN. Otherwise only the NaNs the GPU makes (0x7FC00000,
    /// either sign) and infinities: a typed store may carry an integer's bits, and all-ones or
    /// other NaN patterns are common integers.
    // shortcut: raw buffer stores are left as they are (no type to go by).
    pub(super) fn nan_guard(&mut self, f: V, strict: bool) -> V {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if !*ON.get_or_init(|| std::env::var_os("VICTORY_NAN_GUARD").is_none_or(|v| v != "0")) {
            return f;
        }
        let c = self.cur;
        let bits = self.as_int(f);
        let mag = self.s.m.i32(0x7FFF_FFFF);
        let abs = self.s.m.bin(c, Bin::And, bits, mag);
        let inf_bits = self.s.m.i32(0x7F80_0000);
        let nan = if strict {
            self.s.m.cmp(c, pred::IUGT, abs, inf_bits)
        } else {
            let quiet = self.s.m.i32(0x7FC0_0000);
            self.s.m.cmp(c, pred::EQ, abs, quiet)
        };
        let inf = self.s.m.cmp(c, pred::EQ, abs, inf_bits);
        let sign_bit = self.s.m.i32(0x8000_0000);
        let sign = self.s.m.bin(c, Bin::And, bits, sign_bit);
        let big = self.s.m.i32(65504f32.to_bits());
        let big = self.s.m.bin(c, Bin::Or, sign, big);
        let zero = self.s.m.i32(0);
        let r = self.s.m.select(c, inf, big, bits);
        let r = self.s.m.select(c, nan, zero, r);
        self.as_float(r)
    }

    /// `VICTORY_NAN_SCRUB` (on unless `0`): `f` or 0 where it is NaN or an
    /// infinity. Used on the colours pixel shaders write and on float image
    /// stores: a NaN or infinity that reaches the title's bloom, blur or
    /// temporal upscaler spreads into black or white patches that stay in its
    /// history (seen on NVIDIA as black blobs on faces and in the air).
    /// Supersedes `nan_guard` on those sites when enabled; part of the shader
    /// cache key.
    pub(super) fn scrub_nan(&mut self, f: V) -> V {
        if !nan_scrub() {
            return f;
        }
        // NaN or an infinity: |f| >= inf, unordered (true for NaN too).
        let c = self.cur;
        let a = self.unary(6, f);
        let inf = self.s.m.f32(0x7f80_0000);
        let bad = self.s.m.cmp(c, pred::UGE, a, inf);
        let zero = self.s.m.f32(0);
        self.s.m.select(c, bad, zero, f)
    }

    /// `count` dwords from raw buffer `h` starting at byte address `addr`.
    pub(super) fn raw_dwords(&mut self, h: V, addr: V, count: u32) -> Vec<V> {
        let c = self.cur;
        let mut out = Vec::with_capacity(count as usize);
        for k in 0..count {
            let off = self.s.m.i32(4 * k);
            let a = self.s.m.bin(c, Bin::Add, addr, off);
            let mask = self.s.m.i8(1);
            let (u, al) = (self.s.m.undef(self.s.t.i32), self.s.m.i32(4));
            let r = self.s.op(
                c,
                "rawBufferLoad.f32",
                Attr::ReadOnly,
                self.s.t.res_ret,
                139,
                &[h, a, u, mask, al],
            );
            let x = self.s.m.extract(c, r, 0);
            let x = self.as_int(x);
            out.push(x);
        }
        out
    }

    /// Renderer port 5 (`VICTORY_PORT_CBV=1`, [`super::super::shader::constants_raw`]):
    /// the handle a constant buffer `src` is read through as raw bytes, or
    /// `None` to read it as a constant buffer, as before.
    ///
    /// On NVIDIA (no raw twin heap):
    /// - a table's (or one picked from the view heap): directly through its
    ///   descriptor slot as a raw SRV `(11, 0)`;
    /// - a root one: as the root SRV `t<slot>` in [`super::super::shader::ROOT_CBV_SPACE`].
    pub(super) fn constants_raw(&mut self, site: Site, src: &Source) -> Result<Option<V>, Why> {
        if !super::super::shader::constants_raw() {
            return Ok(None);
        }
        if let Source::Built { address, .. } = src {
            let Some(&rp) = self.x.native.layout.get(address.param) else {
                return Ok(None);
            };
            let kind = self
                .x
                .native
                .used
                .iter()
                .find(|u| u.user_data == rp.user_data)
                .map(|u| u.kind);
            if rp.root == RootType::Descriptor && matches!(kind, Some(ResourceType::Cbv) | None) {
                return Ok(Some(self.s.root_srv_handle(rp.slot)));
            }
            return Ok(None);
        }
        let x = self.x;
        let scalar = &self.scalar;
        let through_heap = src.leaves().iter().all(|leaf| match leaf {
            Source::Loaded { load, .. } | Source::Fetched { load, .. } => scalar
                .get(load)
                .and_then(|a| x.native.layout.get(a.param))
                .is_some_and(|p| matches!(p.root, RootType::Table | RootType::ResourceHeap)),
            _ => false,
        });
        if !through_heap {
            return Ok(None);
        }
        Ok(Some(self.handle(site, src, false, (11, 0))?))
    }
}

impl<'a> Lower<'a> {
    /// `0 × anything = 0`, as in D3D9. As on RDNA2, a denormal operand counts
    /// as zero, so the test is on the exponent bits rather than a float
    /// compare (whose result would depend on the host's denormal mode).
    pub(super) fn legacy_mul(&mut self, a: V, b: V) -> V {
        let c = self.cur;
        let m = self.s.m.bin(c, Bin::Mul, a, b);
        let exp = self.s.m.i32(0x7F80_0000);
        let izero = self.s.m.i32(0);
        let is_zero = |l: &mut Self, v: V| {
            let bits = l.as_int(v);
            let e = l.s.m.bin(c, Bin::And, bits, exp);
            l.s.m.cmp(c, pred::EQ, e, izero)
        };
        let az = is_zero(self, a);
        let bz = is_zero(self, b);
        let z = self.s.m.bin(c, Bin::Or, az, bz);
        let zero = self.s.m.f32(0);
        self.s.m.select(c, z, zero, m)
    }

    /// `x` with a subnormal flushed to the zero of its sign; every other
    /// value as it is.
    pub(super) fn ftz(&mut self, x: V) -> V {
        let c = self.cur;
        let bits = self.as_int(x);
        let exp = self.s.m.i32(0x7F80_0000);
        let sign = self.s.m.i32(0x8000_0000);
        let izero = self.s.m.i32(0);
        let e = self.s.m.bin(c, Bin::And, bits, exp);
        let sub = self.s.m.cmp(c, pred::EQ, e, izero);
        let signed_zero = self.s.m.bin(c, Bin::And, bits, sign);
        let flushed = self.s.m.select(c, sub, signed_zero, bits);
        self.as_float(flushed)
    }

    /// `v_mad_legacy_f32` and `v_mac_legacy_f32` as RDNA 2 runs them: the
    /// encodings decode as `v_fma_legacy_f32` and `v_fmac_legacy_f32` there,
    /// one fused multiply-add of sources with their subnormals flushed, with
    /// the legacy rule that a zero factor makes the product +0 whatever the
    /// other factor is (an infinity or no number too).
    pub(super) fn legacy_fmad(&mut self, a: V, b: V, acc: V) -> V {
        let (a, b, acc) = (self.ftz(a), self.ftz(b), self.ftz(acc));
        let c = self.cur;
        let fused = self.fmad(a, b, acc);
        let zero = self.s.m.f32(0);
        let az = self.s.m.cmp(c, pred::OEQ, a, zero);
        let bz = self.s.m.cmp(c, pred::OEQ, b, zero);
        let z = self.s.m.bin(c, Bin::Or, az, bz);
        // +0 + acc: the sign of a zero addend comes out of the addition.
        let sum = self.s.m.bin(c, Bin::Add, zero, acc);
        self.s.m.select(c, z, sum, fused)
    }
}

/// Per IR block: every lane of the wave that started the program runs it – no per-lane branch
/// separates it from the entry (it lies in no lane branch's region: the blocks reachable from
/// the branch that do not post-dominate it). None with an indirect exit (not followed).
pub(super) fn all_lane_blocks(st: &Structured) -> Option<Vec<bool>> {
    let n = st.exits.len();
    let mut all = vec![true; n];
    for (_, region) in lane_regions(st)? {
        for b in (0..n).filter(|&b| region[b]) {
            all[b] = false;
        }
    }
    Some(all)
}

/// Every per-lane branch (by its block) with its region: the blocks reachable from it that do
/// not post-dominate it, which only some lanes run. Not the branches the wave takes together
/// ([`Structured::wide`]), nor those on scalar conditions. None with an indirect exit.
fn lane_regions(st: &Structured) -> Option<Vec<(usize, Vec<bool>)>> {
    let n = st.exits.len();
    let mut succ: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (b, e) in st.exits.iter().enumerate() {
        match *e {
            LaneExit::Jump(t) => succ[b].push(t),
            LaneExit::Branch { taken, not_taken, .. } => {
                succ[b].push(taken);
                succ[b].push(not_taken);
            }
            LaneExit::End => {}
            LaneExit::Indirect => return None,
        }
    }
    // post-dominators (bitsets; a virtual exit after every End)
    let words = n.div_ceil(64);
    let full = vec![u64::MAX; words];
    let mut pdom: Vec<Vec<u64>> = (0..n).map(|b| if succ[b].is_empty() { let mut v = vec![0; words]; v[b / 64] |= 1 << (b % 64); v } else { full.clone() }).collect();
    let mut changed = true;
    while changed {
        changed = false;
        for b in (0..n).rev() {
            if succ[b].is_empty() {
                continue;
            }
            let mut v = full.clone();
            for &t in &succ[b] {
                for w in 0..words {
                    v[w] &= pdom[t][w];
                }
            }
            v[b / 64] |= 1 << (b % 64);
            if v != pdom[b] {
                pdom[b] = v;
                changed = true;
            }
        }
    }
    let mut regions = Vec::new();
    for (d, e) in st.exits.iter().enumerate() {
        let LaneExit::Branch { when, .. } = *e else { continue };
        if st.wide.contains(&d) || st.bools.support(when).iter().all(|a| matches!(a, Atom::Uniform(_))) {
            continue; // a wave-wide or uniform branch: all lanes go the same way
        }
        // blocks reachable from the lane branch that do not post-dominate it
        let mut seen = vec![false; n];
        let mut stack: Vec<usize> = succ[d].clone();
        while let Some(b) = stack.pop() {
            if seen[b] || pdom[d][b / 64] >> (b % 64) & 1 == 1 {
                continue;
            }
            seen[b] = true;
            stack.extend(succ[b].iter().copied());
        }
        regions.push((d, seen));
    }
    Some(regions)
}

/// The per-lane branches (by block) whose regions hold a block of `needs` (blocks where two
/// host waves must meet, [`super::take_needs`]): the ones to take wave-wide
/// ([`crate::structure::structure_wide`]) so that every lane runs those blocks. None if one
/// of them is a branch whose wave-wide meaning is not known ([`Structured::wave_any`]).
pub(in super::super) fn branches_around(st: &Structured, needs: &[usize]) -> Option<Vec<usize>> {
    let mut out = Vec::new();
    for (d, region) in lane_regions(st)? {
        if needs.iter().any(|&b| region.get(b).copied().unwrap_or(false)) {
            st.wave_any.get(d).copied().flatten()?;
            out.push(d);
        }
    }
    Some(out)
}

/// Scalar values that hold every lane or none, the same in all of them: a
/// scalar condition made a mask (`s_cselect` of -1 and 0), copies, joins and
/// combinations of such. In wave64 a value is its pair's low dword, and the
/// pair's two dwords are alike. Which lane's bit is read does not matter.
pub(super) fn whole_masks(p: &Program, st: &Structured, wave64: bool) -> HashSet<Value> {
    let is_mask = |v: Value| st.masks.contains_key(&v) || st.highs.contains_key(&v);
    // what each value is made of: dwords of registers by (low, high)
    type Pair = (Value, Option<Value>);
    let mut made: HashMap<Value, (Option<Value>, Vec<Pair>)> = HashMap::new();
    let width = if wave64 { "_b64" } else { "_b32" };
    for block in &p.blocks {
        for phi in &block.phis {
            let Reg::S(c) = phi.reg else { continue };
            if c >= EXEC {
                continue;
            }
            if !wave64 {
                made.insert(phi.value, (None, phi.args.iter().map(|&a| (a, None)).collect()));
            } else if let Some(high) = block.phis.iter().find(|h| h.reg == Reg::S(c + 1)) {
                let args = phi.args.iter().copied().zip(high.args.iter().map(|&h| Some(h)));
                made.insert(phi.value, (Some(high.value), args.collect()));
            }
        }
        for inst in &block.insts {
            let Fields::Sop { dst: Some(Operand::Scalar { code, .. }), src, imm: None } = &inst.fields else {
                continue;
            };
            let name = inst.name();
            let known = ["s_cselect", "s_mov", "s_not", "s_and", "s_or", "s_xor", "s_andn2", "s_orn2", "s_nand", "s_nor", "s_xnor"];
            if *code >= EXEC || !name.strip_suffix(width).is_some_and(|n| known.contains(&n)) {
                continue;
            }
            let Some(lo) = inst.write(Reg::S(*code)) else { continue };
            let hi = if wave64 { inst.write(Reg::S(*code + 1)) } else { None };
            let of: Option<Vec<Pair>> = src
                .iter()
                .filter_map(|op| match *op {
                    // 0 and -1
                    Operand::Const(128 | 193) => None,
                    Operand::Scalar { code, .. } if code < EXEC && code != NULL => {
                        let hi = if wave64 { inst.read(Reg::S(code + 1)) } else { None };
                        Some(inst.read(Reg::S(code)).map(|lo| (lo, hi)))
                    }
                    _ => Some(None),
                })
                .collect();
            if let Some(of) = of {
                made.insert(lo, (hi, of));
            }
        }
    }
    made.retain(|&v, _| !is_mask(v));
    loop {
        let bad: Vec<Value> = made
            .iter()
            .filter(|(_, (_, of))| !of.iter().all(|(lo, hi)| made.get(lo).is_some_and(|m| m.0 == *hi)))
            .map(|x| *x.0)
            .collect();
        if bad.is_empty() {
            return made.into_keys().collect();
        }
        for v in bad {
            made.remove(&v);
        }
    }
}

/// VGPR values that are the same in every lane holding them (`all` is [`all_lane_blocks`]):
/// written by an instruction every arriving lane runs (no guard), lane by lane, from scalars,
/// constants and such values; or loaded from an address made of them. The compiler keeps
/// wave-uniform data in VGPRs and fetches it with `v_readfirstlane`.
/// shortcut: a value joined by a phi counts only where every predecessor is a block all lanes
/// run (no lane branch can have split the arriving lanes); loop-carried values never do.
pub(in super::super) fn lane_uniform(
    p: &Program,
    st: &Structured,
    all: &[bool],
    ordered_lds: bool,
) -> HashSet<Value> {
    let is_mask = |v: Value| st.masks.contains_key(&v) || st.highs.contains_key(&v);
    let mut same: HashSet<Value> = HashSet::new();
    for b in crate::ir::reverse_postorder(&p.blocks) {
        let block = &p.blocks[b];
        let whole = block.preds.iter().all(|&q| all.get(q).copied().unwrap_or(false));
        for phi in &block.phis {
            if matches!(phi.reg, Reg::V(_)) && whole && phi.args.iter().all(|a| same.contains(a)) {
                same.insert(phi.value);
            }
        }
        for (i, inst) in block.insts.iter().enumerate() {
            let name = inst.name();
            let ops: Vec<Operand> = match &inst.fields {
                Fields::Valu(v)
                    if v.dpp.is_none()
                        && v.sdwa.is_none()
                        && !name.starts_with("v_mbcnt")
                        && !name.starts_with("v_cmpx")
                        && !name.contains("lane") =>
                {
                    v.src.clone()
                }
                // (`VICTORY_WAVE32_NVIDIA=1`, [`super::super::nvidia`]) a shared-memory dword read at a
                // lane-uniform address is the same in every lane that runs it: the two host
                // waves read the one word, in order
                Fields::Ds {
                    addr: Some(Operand::Vector { reg, dwords: 1 }),
                    gds: false,
                    ..
                } if ordered_lds && name == "ds_read_b32" => {
                    if st.guards[b][i].is_none()
                        && inst.read(Reg::V(*reg)).is_some_and(|a| same.contains(&a))
                    {
                        same.extend(inst.writes.iter().map(|w| w.1));
                    }
                    continue;
                }
                _ => continue,
            };
            if st.guards[b][i].is_some() {
                continue; // lanes outside EXEC keep another value
            }
            let alike = ops.iter().all(|op| match *op {
                Operand::Vector { reg, .. } => {
                    // every dword of the operand the instruction reads from that register on
                    let read: Vec<Value> =
                        inst.reads.iter().filter(|r| matches!(r.0, Reg::V(x) if x >= reg)).map(|r| r.1).collect();
                    !read.is_empty() && read.iter().all(|x| same.contains(x))
                }
                // a scalar is one value; a lane mask is not, unless a compare of such values made it
                Operand::Scalar { code, .. } if code != NULL => {
                    inst.read(Reg::S(code)).is_none_or(|x| !is_mask(x) || same.contains(&x))
                }
                _ => true,
            });
            if alike {
                same.extend(inst.writes.iter().filter(|w| w.0 != Reg::Scc).map(|w| w.1));
            }
        }
    }
    same
}

/// The paired form: scalar values each console lane has its own of in the lowering, a waterfall
/// loop's first-lane reads (the lane's own dword, `resource::Waterfall`) and what scalar code
/// and joins make of them.
pub(super) fn lane_scalars(p: &Program, res: &Resources) -> HashSet<Value> {
    let mut own: HashSet<Value> = res.waterfall.reads.iter().copied().collect();
    if own.is_empty() {
        return own;
    }
    let order = crate::ir::reverse_postorder(&p.blocks);
    loop {
        let before = own.len();
        for &b in &order {
            let block = &p.blocks[b];
            for phi in &block.phis {
                if !matches!(phi.reg, Reg::V(_)) && phi.args.iter().any(|a| own.contains(a)) {
                    own.insert(phi.value);
                }
            }
            for inst in &block.insts {
                if matches!(inst.fields, Fields::Sop { .. } | Fields::Smem { .. })
                    && inst.reads.iter().any(|r| own.contains(&r.1))
                {
                    own.extend(inst.writes.iter().map(|w| w.1));
                }
            }
        }
        if own.len() == before {
            return own;
        }
    }
}

/// LDS accesses of a console wave run as two host waves, where some sit in blocks that only
/// some lanes run (`all` is [`all_lane_blocks`]): per IR block, whether the host waves meet (a
/// group barrier) at its start and before its exit. `None` if barriers cannot keep the
/// console's order.
///
/// The console runs the wave's lanes in step, so an LDS write is seen by every later read. Two
/// host waves keep that order if they meet between any two accesses of which one writes. A
/// barrier must be run by every thread, so it cannot stand in a lane branch's region; but
/// every way into and out of such a region passes a block all lanes run, and the waves meet
/// there: before the exit of an all-lane block into a region with LDS accesses, and at the
/// start of an all-lane block entered from one. Accesses in all-lane blocks get their own
/// barrier as before. What that cannot order is a write and a read inside one region (blocks
/// joined without an all-lane block between them): refused.
/// shortcut: two writes in one region are taken not to hit the same dword from both halves.
pub(super) fn lds_meetings(p: &crate::ir::Program, st: &Structured, all: &[bool]) -> Option<Vec<(bool, bool)>> {
    let n = st.exits.len();
    let succ = |b: usize| -> Vec<usize> {
        match st.exits[b] {
            LaneExit::Jump(t) => vec![t],
            LaneExit::Branch { taken, not_taken, .. } => vec![taken, not_taken],
            LaneExit::End | LaneExit::Indirect => Vec::new(),
        }
    };
    // regions: blocks not every lane runs, joined by the edges between them
    let mut region: Vec<usize> = (0..n).collect();
    fn find(region: &mut [usize], mut b: usize) -> usize {
        while region[b] != b {
            region[b] = region[region[b]];
            b = region[b];
        }
        b
    }
    for b in 0..n {
        for t in succ(b) {
            if !all[b] && !all[t] {
                let (x, y) = (find(&mut region, b), find(&mut region, t));
                region[x] = y;
            }
        }
    }
    // per region: LDS reads, LDS writes
    let mut reads = vec![false; n];
    let mut writes = vec![false; n];
    for (b, block) in p.blocks.iter().enumerate().take(n) {
        if all[b] {
            continue;
        }
        let r = find(&mut region, b);
        for i in &block.insts {
            let name = i.name();
            if !matches!(i.fields, Fields::Ds { .. }) || name.starts_with("ds_swizzle") || name.contains("permute") {
                continue;
            }
            if name.starts_with("ds_read") {
                reads[r] = true;
            } else {
                writes[r] = true;
            }
        }
    }
    if (0..n).any(|r| reads[r] && writes[r]) && !LDS_LOOSE.get() {
        return None;
    }
    let mut meet = vec![(false, false); n];
    for b in 0..n {
        for t in succ(b) {
            if all[b] && !all[t] {
                let r = find(&mut region, t);
                meet[b].1 |= reads[r] || writes[r];
            } else if !all[b] && all[t] {
                let r = find(&mut region, b);
                meet[t].0 |= reads[r] || writes[r];
            }
        }
    }
    Some(meet)
}

/// Masks held as numbers or constant lane sets in guards or lane branches: the lane tests its
/// own bit by its console lane.
pub(super) fn lane_constant_masks(st: &Structured) -> bool {
    let whens = st.exits.iter().filter_map(|e| match *e {
        LaneExit::Branch { when, .. } => Some(when),
        _ => None,
    });
    let mut work: Vec<Bool> = st.guards.iter().flatten().flatten().copied().chain(whens).collect();
    // Through lane-mask joins too: an edge that brings a number or a constant lane set needs
    // the lane's own bit of it.
    // shortcut: masks only selects read (`v_cndmask_b32`, carries in) are not followed; such
    // a program still stops with "a constant lane mask" or "an opaque lane mask".
    let (mut seen, mut joins) = (HashSet::new(), HashSet::new());
    while let Some(g) = work.pop() {
        if !seen.insert(g) {
            continue;
        }
        for a in st.bools.support(g) {
            match a {
                Atom::Opaque(_) | Atom::Constant(_) => return true,
                Atom::Phi(id) if joins.insert(id) => {
                    if let Some(edges) = st.phi_masks.get(&id) {
                        work.extend(edges.iter().copied());
                    }
                }
                _ => {}
            }
        }
    }
    false
}
