//! (`VICTORY_MASK_ELSEWHERE=1`) A lane mask read as a number where it may hold lanes outside
//! EXEC where it is written. A mask read as data is the ballot of its lanes' bits, taken in the
//! block that writes it, which misses lanes that do not run there (`Lower::mask_dword`
//! refuses those). A mask a scalar instruction writes is also a number of that instruction's
//! operands, and the console computes it so: here it is computed so, at the end of the
//! writer's block (where the ballot would be), each operand a number in turn (data, a ballot
//! of a mask that holds only lanes that run where it is written, a join the lowering already
//! has as a number, or another such scalar result). FullAdaptive's candidate test compares
//! lanes with `s35 = s32 | s31`, where `s31 = s27 & 0x1fffff` and `s27` joins a loaded number
//! with an EXEC-derived mask left from an earlier walk on another path: the structure takes
//! `s31` and `s35` for masks, but each is a number.
//! Compute programs only (host lanes are console lanes), not with two host waves to a console
//! wave.

use super::{Fields, Lower, MaskData, Reg, V, Val, Value};

#[cfg(test)]
thread_local! {
    /// Tests: the switch on for this thread.
    pub(in crate::emit) static FORCED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether such masks are lowered (`VICTORY_MASK_ELSEWHERE=1`).
pub fn on() -> bool {
    #[cfg(test)]
    if FORCED.get() {
        return true;
    }
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_MASK_ELSEWHERE").is_some_and(|v| v == "1"))
}

impl Lower<'_> {
    /// The number scalar instruction-written mask `v` holds, computed from the writer's operands
    /// at the end of its block; `None` with the switch off, for a mask nothing scalar writes
    /// (a compare, EXEC, a join) or one an operand of which has no number.
    pub(super) fn mask_by_writer(&mut self, v: Value) -> Option<V> {
        if !on()
            || self.mask_data != MaskData::Console
            || self.twin.is_some()
            || self.exchange.is_some()
            || self.local
        {
            return None;
        }
        let b = *self.def_block.get(&v)?;
        let inst = self.x.p.blocks[b]
            .insts
            .iter()
            .find(|i| i.writes.iter().any(|w| w.1 == v))?;
        if !matches!(inst.fields, Fields::Sop { .. }) || inst.write(Reg::S(super::EXEC)).is_some() {
            return None;
        }
        let at = if b == self.cur_ir { self.cur } else { self.tail[b]? };
        // What the writer's lowering left for its other results stays.
        let kept: Vec<(Value, Option<Val>)> =
            inst.writes.iter().map(|w| (w.1, self.vals.get(&w.1).copied())).collect();
        for &(w, _) in &kept {
            self.vals.remove(&w);
        }
        let (cur, numeric) = (self.cur, self.numeric_sop);
        self.cur = at;
        self.numeric_sop = true;
        let r = self.sop(inst);
        self.cur = cur;
        self.numeric_sop = numeric;
        let x = match (r, self.vals.get(&v)) {
            (Ok(()), Some(&Val::I32(x))) => Some(x),
            _ => None,
        };
        for (w, old) in kept {
            // Mask dwords keep their number: it is defined where they are, so it serves every
            // later read.
            let mask_dword = w != v && self.x.st.masks.contains_key(&w);
            if w == v && x.is_some() || mask_dword && matches!(self.vals.get(&w), Some(Val::I32(_))) {
                continue;
            }
            match old {
                Some(old) => self.vals.insert(w, old),
                None => self.vals.remove(&w),
            };
        }
        x
    }
}

#[cfg(test)]
#[path = "mask_elsewhere_tests.rs"]
mod tests;
