//! `VICTORY_LOOP_CAP_WATCH=1`: a translated program counts the lanes its loops stop at their
//! iteration limits ([`super::loop_keep`]) in a buffer of ours, so a run names the programs
//! that lose pixels (or a thread's later stores) to the limit: a loop bounded only by data
//! cannot be judged from the code.
//!
//! The root UAV [`SLOT_LOOP_CAP`] holds two dwords a program, at the number the host gives it
//! in the user-data dword of kind [`USER_LOOP_CAP_ID`]:
//!
//! - [`ENDED`]: a lane round a loop closed by a plain jump reached the limit and left the
//!   program (a pixel discarded before its export, a compute thread returning);
//! - [`KEPT`]: a lane left a loop at the limit by the loop's own exit with the values it had (a
//!   loop closed by a branch, or one closed by a plain jump with `VICTORY_LOOP_KEEP=1`). It
//!   also counts a lane whose loop meets its own exit on the very iteration the limit falls on.
//!
//! One atomic add on the path a lane at the limit takes, nothing on the others. Changes the
//! translation: every `VICTORY_*` name in this crate is in its fingerprint.

use super::super::shader::{LOOP_CAP_PROGRAMS, SLOT_LOOP_CAP, USER_LOOP_CAP_ID};
use super::{Attr, Bin, Lower, Term, V, pred};
use crate::native::RootType;

#[cfg(test)]
thread_local! {
    /// Tests: the switch for this thread.
    pub(in crate::emit) static FORCED: std::cell::Cell<Option<bool>> =
        const { std::cell::Cell::new(None) };
}

/// Dword of a program's pair: lanes that left the program at the limit.
pub const ENDED: u32 = 0;
/// Dword of a program's pair: lanes that left a loop at the limit, keeping their values.
pub const KEPT: u32 = 1;

/// `VICTORY_LOOP_CAP_WATCH=1`.
pub fn on() -> bool {
    #[cfg(test)]
    if let Some(on) = FORCED.get() {
        return on;
    }
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_LOOP_CAP_WATCH").is_some_and(|v| v == "1"))
}

impl Lower<'_> {
    /// Adds 1 to dword `which` ([`ENDED`], [`KEPT`]) of this program's pair, in `block`.
    pub(super) fn cap_hit(&mut self, block: usize, which: u32) {
        let id = self.host_word(RootType::Other(USER_LOOP_CAP_ID), 0, 0);
        let mask = self.s.m.i32(LOOP_CAP_PROGRAMS - 1);
        let id = self.s.m.bin(block, Bin::And, id, mask);
        let three = self.s.m.i32(3);
        let at = self.s.m.bin(block, Bin::Shl, id, three);
        let at = if which == 0 {
            at
        } else {
            let off = self.s.m.i32(which * 4);
            self.s.m.bin(block, Bin::Add, at, off)
        };
        let h = self.s.root_uav_handle(SLOT_LOOP_CAP);
        let i32t = self.s.t.i32;
        let iu = self.s.m.undef(i32t);
        let add = self.s.m.i32(0);
        let one = self.s.m.i32(1);
        self.s.op(
            block,
            "atomicBinOp.i32",
            Attr::NoUnwind,
            i32t,
            78,
            &[h, add, at, iu, iu, one],
        );
    }

    /// Whether IR edge `b` -> `to` goes back to a counted loop header.
    fn back_to(&self, b: usize, to: usize) -> bool {
        self.counters.contains_key(&to) && self.rpo[to] <= self.rpo[b]
    }

    /// For each side of block `b`'s branch (`taken`, `not_taken`), the condition that a lane
    /// leaving by it left a loop at its iteration limit (none: that side never does). After the
    /// back edge's count ([`Self::count_iteration`]) and [`Self::keep_exit`].
    fn cap_exits(&mut self, b: usize, taken: usize, not_taken: usize) -> [Option<V>; 2] {
        let c = self.cur;
        let mut hits = [None, None];
        let limit = self.loop_cap();
        // A loop closed by this branch: the side that leaves it, when the count reached it.
        for (back, side) in [(taken, 1), (not_taken, 0)] {
            if !self.back_to(b, back) {
                continue;
            }
            let Some(&(next, _)) = self.counters[&back].1.last() else {
                continue;
            };
            let limit = self.s.m.i32(limit);
            hits[side] = Some(self.s.m.cmp(c, pred::IUGE, next, limit));
            break;
        }
        // Loops closed by a plain jump that `VICTORY_LOOP_KEEP` makes leave here.
        if super::loop_keep::keep() {
            let left: Vec<(usize, usize)> = self
                .jump_loops()
                .iter()
                .filter(|(_, inside)| inside[b])
                .filter_map(|(h, inside)| match (inside[taken], inside[not_taken]) {
                    (false, true) => Some((*h, 0)),
                    (true, false) => Some((*h, 1)),
                    _ => None,
                })
                .collect();
            let last = limit.saturating_sub(1);
            for (h, side) in left {
                let phi = self.counters[&h].0;
                let last = self.s.m.i32(last);
                let hit = self.s.m.cmp(c, pred::IUGE, phi, last);
                hits[side] = Some(match hits[side] {
                    Some(was) => self.s.m.bin(c, Bin::Or, was, hit),
                    None => hit,
                });
            }
        }
        hits
    }

    /// (`VICTORY_LOOP_CAP_WATCH`) The DXIL targets of block `b`'s branch to `taken` (by DXIL
    /// block `t`) and `not_taken` (by `f`), with the block each side then enters its IR target
    /// from where that differs from the plain rule: a side that can leave a loop at its limit
    /// goes through a test that adds to [`KEPT`] when it did. Back edges are left alone.
    pub(super) fn watch_exits(
        &mut self,
        b: usize,
        (taken, t): (usize, usize),
        (not_taken, f): (usize, usize),
    ) -> [(usize, Option<usize>); 2] {
        let mut out = [(t, None), (f, None)];
        if !on() {
            return out;
        }
        let hits = self.cap_exits(b, taken, not_taken);
        for (side, (to, via)) in [(taken, t), (not_taken, f)].into_iter().enumerate() {
            let Some(hit) = hits[side] else {
                continue;
            };
            if self.back_to(b, to) {
                continue;
            }
            // The block the IR target is entered from: the edge block, or one made here.
            let from = if Some(via) == self.block[to] {
                let j = self.s.m.new_block();
                self.s.m.terminate(j, Term::Br(via));
                self.edges.insert((b, to), j);
                j
            } else {
                via
            };
            let (test, bump) = (self.s.m.new_block(), self.s.m.new_block());
            self.s.m.terminate(test, Term::CondBr(hit, bump, from));
            self.cap_hit(bump, KEPT);
            self.s.m.terminate(bump, Term::Br(from));
            out[side] = (test, Some(from));
        }
        out
    }
}

#[cfg(test)]
#[path = "loop_watch_tests.rs"]
mod tests;
