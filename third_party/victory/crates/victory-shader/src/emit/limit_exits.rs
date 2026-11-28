//! (portscout, deepscout a #8) `VICTORY_LIMIT_EXITS=1`: in a compute program whose code asks
//! the wave (`console_lanes`), a loop closed by a plain jump tests the iteration limit at its
//! own exits, and its latch jumps back with no test of its own.
//!
//! Ported by hand from darkworldhm 08fdf0b (`LimitExits`, GPL-3.0). Testing the limit in the
//! latch gives the loop a second way out, to the end of the program (`abort_block`), so no
//! block after the loop is one every lane of it passes. A wave operation after the loop (a
//! ballot, a first lane, the mask two host waves exchange) then did not cover the wave's lanes
//! on NVIDIA. darkworldhm measured it on a GeForce card with a title program that appends each
//! thread's tile to one of two lists after such a loop, numbered per wave: of 14,260 threads
//! 442 took a slot another had and 53 took none (its water's dark blocks); with the limit
//! tested at the loop's exits every one had its own.
//!
//! A lane taken out at the limit goes on with the code after the loop, as one in a loop closed
//! by a branch does. A latch some path from the header reaches without passing an exit keeps
//! its test (a lane could loop there for ever). The title's barriers of such a program are no
//! longer made fences for the plain latches' sake (`fence_only`). With `VICTORY_LOOP_KEEP=1`
//! as well, its tests at the exits are the same ones. Retranslates (shader cache key).

use std::collections::{HashMap, HashSet};

use super::{Bin, LaneExit, Lower, Structured, Tag, V, pred};
use crate::ir::Program;

#[cfg(test)]
thread_local! {
    /// Tests: the switch for this thread.
    pub(in crate::emit) static FORCED: std::cell::Cell<Option<bool>> =
        const { std::cell::Cell::new(None) };
}

/// `VICTORY_LIMIT_EXITS=1`.
pub fn on() -> bool {
    #[cfg(test)]
    if let Some(on) = FORCED.get() {
        return on;
    }
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_LIMIT_EXITS").is_some_and(|v| v == "1"))
}

/// The loops closed by a plain jump whose own exits test the iteration limit.
#[derive(Default)]
pub(super) struct LimitExits {
    /// By block ending in a branch out of such a loop: the loops' headers.
    pub(super) tests: HashMap<usize, Vec<usize>>,
    /// By header: the loop's blocks.
    bodies: HashMap<usize, HashSet<usize>>,
    /// The latches whose jump stays a plain one: a lane passes a testing branch on every way
    /// from the header to them.
    pub(super) plain: HashSet<usize>,
}

/// The loops of `p` (as `st` has its lanes go) to test at their exits.
pub(super) fn limit_exits(p: &Program, st: &Structured) -> LimitExits {
    let order = crate::ir::reverse_postorder(&p.blocks);
    let mut place = vec![usize::MAX; p.blocks.len()];
    for (n, &b) in order.iter().enumerate() {
        place[b] = n;
    }
    let mut preds = vec![Vec::new(); p.blocks.len()];
    for &b in &order {
        let to = match st.exits[b] {
            LaneExit::Jump(t) => [Some(t), None],
            LaneExit::Branch {
                taken, not_taken, ..
            } => [Some(taken), Some(not_taken)],
            LaneExit::End | LaneExit::Indirect => [None, None],
        };
        for t in to.into_iter().flatten().filter(|&t| place[t] != usize::MAX) {
            preds[t].push(b);
        }
    }
    let jumps = |b: usize| matches!(st.exits[b], LaneExit::Jump(_));
    let mut found = LimitExits::default();
    for &h in &order {
        let latches: Vec<usize> = preds[h]
            .iter()
            .copied()
            .filter(|&q| place[h] <= place[q])
            .collect();
        if !latches.iter().any(|&q| jumps(q)) {
            continue;
        }
        // the loop: its header and what reaches a latch without passing the header
        let mut body = HashSet::from([h]);
        let mut stack = latches.clone();
        while let Some(b) = stack.pop() {
            if body.insert(b) {
                stack.extend(preds[b].iter().copied());
            }
        }
        // (entered at a second place: the header's count is not known in all of it)
        if h != order[0] && body.contains(&order[0]) {
            continue;
        }
        let exiting: HashSet<usize> = body
            .iter()
            .copied()
            .filter(|&b| {
                matches!(st.exits[b], LaneExit::Branch { taken, not_taken, .. }
                    if body.contains(&taken) != body.contains(&not_taken))
            })
            .collect();
        let mut any = false;
        for &latch in latches.iter().filter(|&&q| jumps(q)) {
            // back from the latch to the header through no exiting block?
            let (mut seen, mut stack, mut free) = (HashSet::new(), vec![latch], false);
            while let Some(b) = stack.pop() {
                if exiting.contains(&b) || !seen.insert(b) {
                    continue;
                }
                if b == h {
                    free = true;
                    break;
                }
                stack.extend(preds[b].iter().copied().filter(|q| body.contains(q)));
            }
            if !free {
                found.plain.insert(latch);
                any = true;
            }
        }
        if any {
            for &b in &exiting {
                found.tests.entry(b).or_default().push(h);
            }
            found.bodies.insert(h, body);
        }
    }
    found
}

impl Lower<'_> {
    /// Branch condition `cond` of block `b` (to `taken` where it holds), forced out of each
    /// loop of [`LimitExits`] it leaves, on the iteration its latch would have stopped it on.
    pub(super) fn limit_exit(&mut self, b: usize, taken: usize, cond: V) -> V {
        let Some(headers) = self.limit.tests.get(&b).cloned() else {
            return cond;
        };
        let mut cond = cond;
        for h in headers {
            let Some(&(count, _)) = self.counters.get(&h) else {
                continue;
            };
            let last = self.iteration_limit(h).saturating_sub(1);
            let last = self.s.m.i32(last);
            let c = self.cur;
            cond = if self.limit.bodies[&h].contains(&taken) {
                let under = self.s.m.cmp(c, pred::IULT, count, last);
                self.s.m.tag(under, Tag::Counter);
                self.s.m.bin(c, Bin::And, cond, under)
            } else {
                let over = self.s.m.cmp(c, pred::IUGE, count, last);
                self.s.m.tag(over, Tag::Counter);
                self.s.m.bin(c, Bin::Or, cond, over)
            };
        }
        cond
    }
}

#[cfg(test)]
mod tests {
    use super::{FORCED, limit_exits};
    use crate::emit::wave64_tests::native_of;
    use crate::native::NativeProgram;
    use crate::{io, ir, resource, structure};

    /// A wave of 32 counting v1 up to its thread index round a loop closed by `s_branch`, its
    /// exit a `s_cbranch_vccnz` in the body, then the count of lanes below each (`v_mbcnt`,
    /// a program that asks the wave) stored.
    fn counting() -> NativeProgram {
        native_of(
            &[
            0x7E02_0280, // v_mov_b32 v1, 0
            0x7D84_0300, // loop: v_cmp_eq_u32 vcc, v0, v1
            0xBF87_0002, // s_cbranch_vccnz past the loop
            0x4A02_0281, // v_add_nc_u32 v1, 1, v1
            0xBF82_FFFC, // s_branch loop
            0xD765_0002, // v_mbcnt_lo_u32_b32 v2, -1, 0
            0x0001_00C1,
            0xE070_0000, // buffer_store_dword v2, off, s[8:11], 0
            0x8002_0200,
            ],
            false,
            32,
        )
    }

    #[test]
    fn the_latch_stays_a_jump_and_the_exit_tests_the_limit() {
        let native = counting();
        let p = ir::build(&native.code, false, 16).unwrap();
        let st = structure::structure(&p).unwrap();
        let found = limit_exits(&p, &st);
        assert_eq!(found.plain.len(), 1, "one latch, every way to it through the exit");
        let latch = *found.plain.iter().next().unwrap();
        assert!(matches!(st.exits[latch], structure::LaneExit::Jump(_)));
        assert_eq!(found.tests.len(), 1, "one exit branch");
        assert_eq!(found.tests.values().next().unwrap().len(), 1);
    }

    /// Off, the translation is today's; on, it differs (no latch test), and both validate.
    #[test]
    fn the_switch_changes_the_translation_and_it_validates() {
        let native = counting();
        let p = ir::build(&native.code, false, 16).unwrap();
        let st = structure::structure(&p).unwrap();
        let res = resource::track(&p, &native.layout, native.stage, Some(&st)).unwrap();
        let io = io::raise(&p, &native).unwrap();
        let bytes = crate::dxbc::build(&[]);
        let c = crate::emit::Container::parse(&bytes).unwrap();
        let wave = super::super::TEST_HOST_WAVE.replace(Some(32));
        let translate = |on: bool| {
            let was = FORCED.replace(Some(on));
            let r = crate::emit::emit(&c, &native, &p, &st, &io, &res);
            FORCED.set(was);
            r.unwrap()
        };
        let (off, on) = (translate(false), translate(true));
        super::super::TEST_HOST_WAVE.set(wave);
        assert_ne!(off.dxil, on.dxil);
        assert_eq!(translate(false).dxil, off.dxil);
        #[cfg(windows)]
        if let Ok(mut v) = crate::emit::validator::Validator::load(
            crate::emit::validator::Validator::default_path().as_deref(),
        ) {
            v.validate(&mut on.dxil.clone()).expect("on validates");
            v.validate(&mut off.dxil.clone()).expect("off validates");
        }
    }
}
