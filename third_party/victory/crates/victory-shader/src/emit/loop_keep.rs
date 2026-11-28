//! What a translated loop does at its iteration limit ([`super::LOOP_LIMIT`]).
//!
//! The limits are low (a pixel loop 128 iterations, a vertex or mesh loop 256, a compute loop
//! 4,096) because a loop over data the host has not got yet ran for seconds and lost the card.
//! A loop closed by a branch leaves by that branch at the limit, keeping what it computed; one
//! closed by a plain jump (`s_branch` back, its exit somewhere in the body) has no such exit,
//! and a lane still in it at the limit leaves the program, a pixel discarded. A genuine loop
//! longer than the limit (a skin light loop over garbage ranges, a long ray march) then draws
//! nothing there.
//!
//! - `VICTORY_LOOP_KEEP=1`: a loop closed by a plain jump leaves at its own exit branch on the
//!   iteration the limit would end it on, with the values it has computed, as a loop closed by
//!   a branch does. A path round the loop that passes no exit branch still ends the program.
//! - `VICTORY_LOOP_ROOM=<n>`: pixel, vertex and mesh loops run up to `n` iterations (at least
//!   their limits above), compute loops up to `max(n, 4096)`; ray walks keep theirs. TDR is
//!   10 s on the target PC: a runaway loop over a 4K screen at 1,024 costs about 8 times what
//!   it does at 128.
//!
//! Both change the translation: every `VICTORY_*` name in this crate is in its fingerprint.

use super::{Bin, LOOP_LIMIT, LaneExit, Lower, TRACE_LOOP_LIMIT, V, pred};

#[cfg(test)]
thread_local! {
    /// Tests: `(keep, room)` for this thread.
    pub(in crate::emit) static FORCED: std::cell::Cell<Option<(bool, u32)>> =
        const { std::cell::Cell::new(None) };
}

/// `VICTORY_LOOP_KEEP=1`.
pub fn keep() -> bool {
    #[cfg(test)]
    if let Some((k, _)) = FORCED.get() {
        return k;
    }
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_LOOP_KEEP").is_some_and(|v| v == "1"))
}

/// `VICTORY_LOOP_ROOM=<n>` (0 unset or unreadable).
pub fn room() -> u32 {
    #[cfg(test)]
    if let Some((_, r)) = FORCED.get() {
        return r;
    }
    static ROOM: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *ROOM.get_or_init(|| parse_room(std::env::var("VICTORY_LOOP_ROOM").ok().as_deref()))
}

fn parse_room(v: Option<&str>) -> u32 {
    v.and_then(|s| s.trim().parse().ok()).unwrap_or(0)
}

/// A stage's iteration limit: `pixel`, `narrow` (vertex or mesh), `traces` (walks trees),
/// with `named` the diagnostics override ([`super::super::loop_limit`]) and `room`
/// (`VICTORY_LOOP_ROOM`).
pub(super) fn cap(named: u32, pixel: bool, narrow: bool, traces: bool, room: u32) -> u32 {
    match named {
        LOOP_LIMIT if pixel => (1 << 7).max(room),
        LOOP_LIMIT if narrow => (1 << 8).max(room),
        LOOP_LIMIT if traces => TRACE_LOOP_LIMIT,
        LOOP_LIMIT => (1 << 12).max(room),
        named => named,
    }
}

impl Lower<'_> {
    /// This program's iteration limit.
    pub(super) fn loop_cap(&self) -> u32 {
        cap(
            super::super::loop_limit(),
            self.s.pixel,
            self.s.mesh.is_some() || self.s.threads.is_none(),
            self.traces,
            room(),
        )
    }

    /// Loops closed by a plain jump: each header with the blocks of its loop.
    pub(super) fn jump_loops(&mut self) -> &[(usize, Vec<bool>)] {
        if self.jump_loops.is_none() {
            let p = self.x.p;
            let mut out = Vec::new();
            let mut headers: Vec<usize> = self.counters.keys().copied().collect();
            headers.sort_unstable();
            for h in headers {
                let latches: Vec<usize> = p.blocks[h]
                    .preds
                    .iter()
                    .copied()
                    .filter(|&q| self.block[q].is_some() && self.rpo[h] <= self.rpo[q])
                    .collect();
                if !latches
                    .iter()
                    .any(|&q| self.x.st.exits[q] == LaneExit::Jump(h))
                {
                    continue;
                }
                let mut inside = vec![false; p.blocks.len()];
                inside[h] = true;
                let mut work = latches;
                while let Some(b) = work.pop() {
                    if inside[b] || self.block[b].is_none() {
                        continue;
                    }
                    inside[b] = true;
                    work.extend(p.blocks[b].preds.iter().copied());
                }
                out.push((h, inside));
            }
            self.jump_loops = Some(out);
        }
        self.jump_loops.as_deref().unwrap_or_default()
    }

    /// (`VICTORY_LOOP_KEEP`) Branch condition `cond` of block `b` (to `taken` where it holds),
    /// forced to leave each loop closed by a plain jump that one side leaves, on the iteration
    /// its latch would stop it on (the count at the limit less one).
    pub(super) fn keep_exit(&mut self, b: usize, taken: usize, not_taken: usize, cond: V) -> V {
        if !keep() {
            return cond;
        }
        let left: Vec<(usize, bool)> = self
            .jump_loops()
            .iter()
            .filter(|(_, inside)| inside[b])
            .filter_map(|(h, inside)| match (inside[taken], inside[not_taken]) {
                (false, true) => Some((*h, true)),
                (true, false) => Some((*h, false)),
                _ => None,
            })
            .collect();
        let cap = self.loop_cap();
        let mut cond = cond;
        for (h, by_taken) in left {
            let trips = self.trips.get(&h).copied();
            let last = super::trips::counted_limit(cap, super::super::loop_limit(), trips).saturating_sub(1);
            let phi = self.counters[&h].0;
            let c = self.cur;
            let limit = self.s.m.i32(last);
            cond = if by_taken {
                let stop = self.s.m.cmp(c, pred::IUGE, phi, limit);
                self.s.m.bin(c, Bin::Or, cond, stop)
            } else {
                let go = self.s.m.cmp(c, pred::IULT, phi, limit);
                self.s.m.bin(c, Bin::And, cond, go)
            };
        }
        cond
    }
}

#[cfg(test)]
#[path = "loop_keep_tests.rs"]
mod tests;
