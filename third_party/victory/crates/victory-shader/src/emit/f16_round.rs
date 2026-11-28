//! (`VICTORY_F16_ROUND_MODE=1`) 16-bit float results rounded as the console's MODE register
//! says where they are made. The title sets the 16/64-bit rounding field
//! (`s_setreg_imm32_b32 hwreg(HW_REG_MODE, 2, 2)`) to toward zero (3) in 6,694 programs, around
//! its f16 interpolation, and back to nearest (0) in 2,723; many never set it back, so every
//! later `v_fma_mixlo_f16`, `v_cvt_f16_f32` and 16-bit float operation rounds toward zero on
//! the console. The lowering ignores MODE: those results take the host's `legacyF32ToF16`
//! (toward zero on NVIDIA, nearest elsewhere), or nearest everywhere with
//! `VICTORY_F16_NEAREST=1`. With the switch on, the mode in force at each instruction is
//! followed through the program (nearest at entry, the MODE default) and the result is
//! rounded to nearest or toward zero by it; where paths meet with different modes, or the
//! mode is written from a register, the host's conversion stays.

use std::collections::HashMap;

use super::{Fields, Inst, Lower, Operand, Program, V};

#[cfg(test)]
thread_local! {
    /// Tests: the switch on for this thread.
    pub(in crate::emit) static FORCED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether f16 results follow MODE (`VICTORY_F16_ROUND_MODE=1`).
pub fn on() -> bool {
    #[cfg(test)]
    if FORCED.get() {
        return true;
    }
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_F16_ROUND_MODE").is_some_and(|v| v == "1"))
}

/// The 16/64-bit rounding field (MODE bits 2-3): 0 nearest even, 1 up, 2 down, 3 toward zero;
/// `None` where it is not known.
type Round = Option<u8>;

const HW_REG_MODE: u16 = 1;

/// The rounding field after `inst`, from `round` before it.
fn after(inst: &Inst, round: Round) -> Round {
    match (inst.name(), &inst.fields) {
        (
            "s_setreg_imm32_b32",
            Fields::Sop {
                imm: Some(h), src, ..
            },
        ) => {
            let (id, offset, size) = (h & 0x3F, (h >> 6) & 0x1F, ((h >> 11) & 0x1F) + 1);
            if id != HW_REG_MODE || offset > 3 || offset + size <= 2 {
                return round;
            }
            let Some(Operand::Literal(value)) = src.first() else {
                return None;
            };
            let field = if size >= 32 {
                u32::MAX
            } else {
                (1u32 << size) - 1
            } << offset;
            let old = u32::from(round.unwrap_or(0)) << 2;
            let new = (old & !field) | ((value << offset) & field);
            // A partial write keeps a bit of an unknown field unknown.
            if round.is_none() && field & 0b1100 != 0b1100 {
                return None;
            }
            Some(((new >> 2) & 3) as u8)
        }
        ("s_setreg_b32", Fields::Sop { imm: Some(h), .. }) => {
            let (id, offset, size) = (h & 0x3F, (h >> 6) & 0x1F, ((h >> 11) & 0x1F) + 1);
            if id != HW_REG_MODE || offset > 3 || offset + size <= 2 {
                round
            } else {
                None
            }
        }
        // `s_round_mode imm`: bits 2-3 of the immediate are the 16/64-bit field.
        ("s_round_mode", Fields::Sop { imm: Some(h), .. }) => Some(((h >> 2) & 3) as u8),
        ("s_round_mode" | "s_setreg_b32" | "s_setreg_imm32_b32" | "s_swappc_b64", _) => None,
        _ => round,
    }
}

/// The rounding field in force at each instruction, by its byte offset (a block's copies
/// share offsets: they meet there too).
pub(super) fn modes(p: &Program) -> HashMap<usize, Round> {
    let n = p.blocks.len();
    // `None`: not reached yet; `Some(round)`: the field at the block's end.
    let mut exits: Vec<Option<Round>> = vec![None; n];
    let mut at: HashMap<usize, Round> = HashMap::new();
    let meet = |a: Round, b: Round| if a == b { a } else { None };
    loop {
        let mut changed = false;
        for (b, block) in p.blocks.iter().enumerate() {
            let mut entry = (b == 0).then_some(Some(0u8));
            for &pred in &block.preds {
                if let Some(r) = exits.get(pred).copied().flatten() {
                    entry = Some(entry.map_or(r, |e| meet(e, r)));
                }
            }
            let Some(mut round) = entry else {
                continue;
            };
            for inst in &block.insts {
                at.entry(inst.machine.offset)
                    .and_modify(|r| *r = meet(*r, round))
                    .or_insert(round);
                round = after(inst, round);
            }
            if exits[b] != Some(round) {
                exits[b] = Some(round);
                changed = true;
            }
        }
        if !changed {
            return at;
        }
    }
}

impl Lower<'_> {
    /// `f` as an f16 in the low bits, rounded as MODE says at `inst` (nearest or toward
    /// zero); `None` with the switch off or where the mode is not one of those or not known.
    pub(super) fn f16_by_mode(&mut self, inst: &Inst, f: V) -> Option<V> {
        if !on() {
            return None;
        }
        let p = self.x.p;
        let round = *self
            .f16_modes
            .get_or_insert_with(|| modes(p))
            .get(&inst.machine.offset)?;
        match round? {
            0 => Some(self.f16_nearest(f)),
            3 => Some(self.f16_toward_zero(f)),
            _ => None,
        }
    }
}

#[cfg(test)]
#[test]
fn the_mode_follows_setreg() {
    let inst = |h: u16, value: u32| {
        let words = [0xBA80_0000u32 | u32::from(h), value, 0xBF81_0000];
        let code: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        let p = crate::ir::build(&code, false, 1).expect("ir::build");
        p.blocks[0].insts[0].clone()
    };
    // hwreg(HW_REG_MODE, 2, 2) = 3: toward zero; = 0: nearest.
    assert_eq!(after(&inst(0x0881, 3), Some(0)), Some(3));
    assert_eq!(after(&inst(0x0881, 0), Some(3)), Some(0));
    // hwreg(HW_REG_MODE, 0, 8) = 0xC0: the whole low byte, rounding fields 0.
    assert_eq!(after(&inst(0x3801, 0xC0), Some(3)), Some(0));
    // hwreg(HW_REG_MODE, 4, 4): denormals only; the rounding stays.
    assert_eq!(after(&inst(0x1901, 0xF), Some(3)), Some(3));
    // hwreg(HW_REG_MODE, 3, 1) = 1 on an unknown field: still unknown.
    assert_eq!(after(&inst(0x00C1, 1), None), None);
    // Another hardware register: no change.
    assert_eq!(after(&inst(0x0882, 3), Some(0)), Some(0));
}
