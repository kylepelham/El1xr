//! The shadow splice made without a hand-written table (`VICTORY_NATIVE_SHADOW_AUTO=1`, with
//! `VICTORY_NATIVE_SHADOW=1`): for a program [`super::shadow_patch`] does not know, each walk
//! [`super::walk_find`] finds gets the same second arm as a hand-found one, when its resume, its
//! results and its top level can all be read from the program:
//!
//! - **resume:** where the loop goes when it is done (the code after its last backward branch,
//!   or that code's `s_branch`). Every branch in the loop stays in the loop or goes there, and
//!   the skip's target reaches it through plain copies only (what the skip path sets, the arm
//!   sets itself);
//! - **results:** the registers live at the resume that the loop writes. Each must be one the
//!   arm can give: the extent ([`Out::HitT`]), the hit leaf's node pointer (read after the walk
//!   only by a test of bit 2 or against 0: [`Out::Hit`] 1, a triangle), the iteration count
//!   (only incremented in the loop and compared with 0xfffe or 0xffff after it:
//!   [`Out::Always`] 0). Any other register (the hit's barycentrics, instance or node addresses
//!   read from console memory after the walk), any scalar but exec, or SCC refuses the walk;
//! - **ray:** the registers the first node test's origin and direction are copied from at the
//!   skip are not written in the loop (a walk that makes its ray in the loop is refused; the
//!   copies themselves may be: a bottom level's transformed ray, put back for the top level);
//! - **top level:** where the node test's descriptor takes its first dword (the structure's
//!   address >> 8) from a scalar the walk does not write, the arm compares that scalar with the
//!   host's two candidates as for the per-wave variants ([`Walk::choose`]): a walk whose
//!   structure is neither takes the title's walk, so no binding can be wrong. Otherwise the arm
//!   runs on the first host top level, and the host binds the program only when it has one
//!   structure ([`ShadowBinding::any`]).
//!
//! Hand-found programs keep their table rows; walks that are refused keep the title's walk.

use super::glass_patch::{ONE, ZERO};
use super::shadow_patch::{self, Out, ShadowBinding, Walk};
use super::walk_find::{self, Found};
use crate::gfx10::{self, Encoding, Fields, Instruction, Operand};
use crate::ir::{self, Reg};
use crate::native::{HwStage, NativeProgram};

/// Whether walks found in programs without a table row are spliced too.
pub fn native_shadow_auto() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var_os("VICTORY_NATIVE_SHADOW_AUTO").is_some_and(|v| v == "1")
            && super::shadow_splice::native_shadow()
    })
}

/// A walk the splice can take: what [`Walk`] needs beyond [`Found`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Derived {
    pub found: Found,
    pub resume: usize,
    pub choose: Option<u32>,
    pub outs: Vec<Out>,
}

/// Why a found walk is not spliced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Refused {
    /// The program has no SSA form here.
    Program,
    /// A branch in the loop leaves it elsewhere than the resume.
    Exit(usize),
    /// The skip's target reaches the resume through more than copies.
    SkipPath(usize),
    /// The loop writes the ray's origin or direction.
    Ray(u8),
    /// A VGPR live at the resume, written in the loop, that the arm cannot give.
    Output(u8),
    /// A scalar (or SCC, 255) live at the resume and written in the loop.
    Scalar(u8),
}

impl Derived {
    fn walk(&self) -> Walk<'_> {
        let f = &self.found;
        Walk {
            cmp: f.cmp,
            skip: f.skip,
            past: f.past,
            head: f.head,
            resume: self.resume,
            lanes: f.lanes,
            structure: 0,
            choose: self.choose,
            origin: f.origin,
            direction: f.direction,
            extent: f.extent,
            mask: 0xff,
            outs: &self.outs,
        }
    }
}

const EXEC: [u8; 2] = [ir::EXEC, ir::EXEC + 1];

/// The program's instructions in code order, each once (the IR copies some blocks).
type Ordered<'p> = Vec<&'p ir::Inst>;

fn ordered(p: &ir::Program) -> Ordered<'_> {
    let mut seen = std::collections::BTreeMap::new();
    for b in &p.blocks {
        for i in &b.insts {
            seen.entry(i.machine.offset).or_insert(i);
        }
    }
    seen.into_values().collect()
}

/// The instructions of `code` with offsets in `lo..=hi`.
fn within<'a, 'p>(code: &'a Ordered<'p>, lo: usize, hi: usize) -> impl Iterator<Item = &'p ir::Inst> + 'a {
    code.iter().copied().filter(move |i| lo <= i.machine.offset && i.machine.offset <= hi)
}

/// Every walk of `native`'s program, derived or refused.
pub(super) fn derive(native: &NativeProgram) -> Vec<Result<Derived, (usize, Refused)>> {
    let found = walk_find::walks(&native.code);
    if found.is_empty() {
        return Vec::new();
    }
    let Ok(insts) = gfx10::decode(&native.code) else {
        return Vec::new();
    };
    let program = ir::build(&native.code, native.wave64, native.vgprs).ok();
    let code = program.as_ref().map(ordered);
    found
        .into_iter()
        .map(|f| {
            let skip = f.skip;
            let (Some(p), Some(code)) = (program.as_ref(), code.as_ref()) else {
                return Err((skip, Refused::Program));
            };
            derive_one(&insts, p, code, f).map_err(|r| (skip, r))
        })
        .collect()
}

fn derive_one(insts: &[Instruction], p: &ir::Program, code: &Ordered<'_>, f: Found) -> Result<Derived, Refused> {
    let resume = resume(insts, &f)?;
    let written: std::collections::BTreeSet<Reg> =
        within(code, f.head, f.end).flat_map(|i| i.writes.iter().map(|w| w.0)).collect();
    let first = insts.iter().find(|i| i.offset == f.tests[0]).ok_or(Refused::Program)?;
    for &r in f.origin.iter().chain(&f.direction) {
        if written.contains(&Reg::V(r)) {
            return Err(Refused::Ray(r));
        }
    }
    let live = live_at(p, resume).ok_or(Refused::Program)?;
    let mut outs = Vec::new();
    for reg in live.iter().filter(|r| written.contains(r)) {
        match *reg {
            Reg::V(r) if r == f.extent => outs.push(Out::HitT(r)),
            Reg::V(r) if is_leaf(p, code, resume, r) => outs.push(Out::Hit(r, ONE)),
            Reg::V(r) if is_count(p, code, &f, resume, r) => outs.push(Out::Always(r, ZERO)),
            Reg::V(r) => return Err(Refused::Output(r)),
            Reg::S(c) if EXEC.contains(&c) => {}
            Reg::S(c) => return Err(Refused::Scalar(c)),
            Reg::Scc => return Err(Refused::Scalar(255)),
        }
    }
    let choose = base(code, &f, first);
    Ok(Derived { found: f, resume, choose, outs })
}

/// Where the loop of `f` goes when it is done (past its last backward branch and any `s_nop`,
/// or where an `s_branch` there goes), if every way out of it goes there and the skip path
/// reaches it through copies only.
fn resume(insts: &[Instruction], f: &Found) -> Result<usize, Refused> {
    let mut after = insts.iter().position(|i| i.offset == f.end).ok_or(Refused::Program)? + 1;
    while insts.get(after).is_some_and(|i| i.name() == Some("s_nop")) {
        after += 1;
    }
    let next = insts.get(after).ok_or(Refused::Program)?;
    let exit = if next.name() == Some("s_branch") { walk_find::target(next) } else { next.offset };
    for i in insts.iter().filter(|i| f.head <= i.offset && i.offset <= f.end) {
        if walk_find::is_branch(i) {
            let t = walk_find::target(i);
            if !(f.head <= t && t <= next.offset) && t != exit {
                return Err(Refused::Exit(i.offset));
            }
        }
    }
    if f.past > exit {
        return Err(Refused::SkipPath(f.past));
    }
    for i in insts.iter().filter(|i| f.past <= i.offset && i.offset < exit) {
        let copy = i.encoding == Encoding::Vop1 && i.name() == Some("v_mov_b32");
        if !copy && i.name() != Some("s_nop") {
            return Err(Refused::SkipPath(i.offset));
        }
    }
    Ok(exit)
}

/// One bit per register: scalar codes 0-255, VGPRs, SCC.
type Set = [u64; 9];

fn index(r: Reg) -> usize {
    match r {
        Reg::S(c) => usize::from(c),
        Reg::V(v) => 256 + usize::from(v),
        Reg::Scc => 512,
    }
}

fn reg_at(i: usize) -> Reg {
    match i {
        0..256 => Reg::S(i as u8),
        256..512 => Reg::V((i - 256) as u8),
        _ => Reg::Scc,
    }
}

fn has(s: &Set, r: Reg) -> bool {
    s[index(r) / 64] >> (index(r) % 64) & 1 != 0
}

fn put(s: &mut Set, r: Reg) {
    s[index(r) / 64] |= 1 << (index(r) % 64);
}

/// The registers live at the start of the block at byte `at` (its copies' together), reads
/// counted as operands only (a VALU write's old value is not a use here).
fn live_at(p: &ir::Program, at: usize) -> Option<Vec<Reg>> {
    let n = p.blocks.len();
    let mut uses = vec![[0u64; 9]; n];
    let mut defs = vec![[0u64; 9]; n];
    for (b, block) in p.blocks.iter().enumerate() {
        for inst in &block.insts {
            for r in ir::operand_reads(p, inst) {
                if !has(&defs[b], r) {
                    put(&mut uses[b], r);
                }
            }
            for &(r, _) in &inst.writes {
                put(&mut defs[b], r);
            }
        }
    }
    let order = ir::reverse_postorder(&p.blocks);
    let mut live_in = vec![[0u64; 9]; n];
    let mut changed = true;
    while changed {
        changed = false;
        for &b in order.iter().rev() {
            let mut out = [0u64; 9];
            for s in p.blocks[b].exit.successors() {
                for (o, i) in out.iter_mut().zip(&live_in[s]) {
                    *o |= i;
                }
            }
            let mut inn = [0u64; 9];
            for w in 0..9 {
                inn[w] = uses[b][w] | out[w] & !defs[b][w];
            }
            if inn != live_in[b] {
                live_in[b] = inn;
                changed = true;
            }
        }
    }
    let mut set = [0u64; 9];
    let mut any = false;
    for (b, block) in p.blocks.iter().enumerate() {
        if block.offset == at {
            any = true;
            for (s, l) in set.iter_mut().zip(&live_in[b]) {
                *s |= l;
            }
        }
    }
    any.then(|| (0..9 * 64).filter(|&i| set[i / 64] >> (i % 64) & 1 != 0).map(reg_at).collect())
}

fn is_v(o: &Operand, r: u8) -> bool {
    matches!(*o, Operand::Vector { reg, .. } if reg == r)
}

fn is_const(o: &Operand, value: u32) -> bool {
    match *o {
        Operand::Const(c) => (128..=192).contains(&c) && u32::from(c) - 128 == value,
        Operand::Literal(l) => l == value,
        _ => false,
    }
}

/// The instructions from `at` on, in code order, that read VGPR `r`, up to the first that
/// writes it (looking at most 64 instructions on, stopping at the program's end); `None` if
/// none reads it there.
fn reads_after<'p>(p: &ir::Program, code: &Ordered<'p>, at: usize, r: u8) -> Option<Vec<&'p ir::Inst>> {
    let start = code.iter().position(|i| i.machine.offset == at)?;
    let mut found = Vec::new();
    for &i in code[start..].iter().take(64) {
        if i.name() == "s_endpgm" {
            break;
        }
        if ir::operand_reads(p, i).contains(&Reg::V(r)) {
            found.push(i);
        }
        if i.writes.iter().any(|w| w.0 == Reg::V(r)) {
            break;
        }
    }
    (!found.is_empty()).then_some(found)
}

/// The VALU sources of `i` if it is a VALU instruction with two, exactly one of them `r`: the
/// other one.
fn other_source(i: &ir::Inst, r: u8) -> Option<Operand> {
    let Fields::Valu(v) = &i.fields else {
        return None;
    };
    (v.src.len() == 2 && v.src.iter().filter(|o| is_v(o, r)).count() == 1)
        .then(|| v.src.iter().find(|o| !is_v(o, r)).copied())
        .flatten()
}

/// Whether the code after the walk reads `r` only as a leaf node pointer: its bit 2
/// (`v_and_b32 _, 4, r`) or against 0 (`v_cmp*_{eq,ne}_u32 _, 0, r`).
fn is_leaf(p: &ir::Program, code: &Ordered<'_>, at: usize, r: u8) -> bool {
    let Some(reads) = reads_after(p, code, at, r) else {
        return false;
    };
    reads.iter().all(|i| {
        let name = i.name();
        let Some(other) = other_source(i, r) else {
            return false;
        };
        match name {
            "v_and_b32" => is_const(&other, 4),
            n if n.starts_with("v_cmp") && (n.ends_with("_eq_u32") || n.ends_with("_ne_u32")) => {
                is_const(&other, 0)
            }
            _ => false,
        }
    })
}

/// Whether `r` is the walk's iteration count: the loop only adds 1 to it, the code after the
/// walk only compares it with 0xfffe or 0xffff.
fn is_count(p: &ir::Program, code: &Ordered<'_>, f: &Found, at: usize, r: u8) -> bool {
    let mut steps = within(code, f.head, f.end).filter(|i| i.writes.iter().any(|w| w.0 == Reg::V(r))).peekable();
    if steps.peek().is_none() {
        return false;
    }
    let adds = steps.all(|i| i.name() == "v_add_nc_u32" && other_source(i, r).is_some_and(|o| is_const(&o, 1)));
    let Some(reads) = reads_after(p, code, at, r) else {
        return false;
    };
    adds && reads.iter().all(|i| {
        i.name().starts_with("v_cmp")
            && i.name().ends_with("_u32")
            && other_source(i, r).is_some_and(|o| is_const(&o, 0xfffe) || is_const(&o, 0xffff))
    })
}

/// The scalar the walk's top-level descriptor takes its first dword from, where every write of
/// that dword in the walk is a copy of it (`s_mov_b32 sD, sX`) and the walk writes no sX.
fn base(code: &Ordered<'_>, f: &Found, first: &Instruction) -> Option<u32> {
    let Fields::Image { rsrc: Operand::Scalar { code: d, .. }, .. } = first.fields(false) else {
        return None;
    };
    let walk = || within(code, f.skip + 4, f.end);
    let mut from = None;
    for i in walk().filter(|i| i.writes.iter().any(|w| w.0 == Reg::S(d))) {
        let Fields::Sop { src, .. } = &i.fields else {
            return None;
        };
        match (i.name(), src.first()) {
            ("s_mov_b32", Some(&Operand::Scalar { code: x, dwords: 1 })) if x < 106 && from.is_none_or(|y| y == x) => {
                from = Some(x);
            }
            _ => return None,
        }
    }
    let x = from?;
    let writes_x = walk().any(|i| i.writes.iter().any(|w| w.0 == Reg::S(x)));
    (!writes_x).then_some(u32::from(x))
}

/// The walks spliced in `native`, if it is a compute program without a table row.
fn derived(native: &NativeProgram) -> Vec<Derived> {
    if native.stage != HwStage::Cs {
        return Vec::new();
    }
    derive(native).into_iter().flatten().collect()
}

/// Splices every walk derived in `native` (a program [`shadow_patch`] does not know); returns
/// whether it changed the program.
pub(super) fn rewrite(native: &mut NativeProgram) -> bool {
    let walks = derived(native);
    if walks.is_empty() {
        return false;
    }
    let walks: Vec<Walk<'_>> = walks.iter().map(Derived::walk).collect();
    shadow_patch::apply_walks(&walks, native);
    true
}

/// For the host: how a program the automatic splice changes is bound (with
/// `VICTORY_NATIVE_SHADOW_AUTO=1`), from its container; `None` for any other program.
pub fn shadow_auto_binding(container: &[u8]) -> Option<ShadowBinding> {
    if !native_shadow_auto() {
        return None;
    }
    binding_of(container)
}

fn binding_of(container: &[u8]) -> Option<ShadowBinding> {
    let c = crate::dxbc::Container::parse(container).ok()?;
    let part = super::program_part(&c)?;
    let native = crate::native_programs(part.data)
        .ok()?
        .into_iter()
        .find(|p| p.stage == HwStage::Cs)?;
    if shadow_patch::known(&c.digest, &native).is_some() {
        return None;
    }
    let walks = derived(&native);
    (!walks.is_empty()).then(|| ShadowBinding {
        hash: "auto",
        registers: [None, None],
        per_wave: walks.iter().all(|w| w.choose.is_some()),
        any: true,
    })
}

#[cfg(test)]
#[path = "walk_splice_tests.rs"]
mod tests;
