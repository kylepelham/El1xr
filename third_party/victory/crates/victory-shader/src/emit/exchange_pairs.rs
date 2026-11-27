//! `VICTORY_EXCHANGE_PAIRS=1` (off by default, in the shader cache key): the two host waves of a
//! console wave64 exchange a value through their console wave's LDS dwords as store, meeting,
//! load, meeting; the second meeting only keeps the next exchange from writing the dwords before
//! the other half has read them. With two sets of dwords that take turns, the next exchange
//! writes the other set, and its own meeting (which neither half passes before both have done
//! their loads of the last exchange) orders the set it leaves for the one after: the trailing
//! meeting goes, one of two per exchange.
//!
//! The set is chosen as the code is lowered, in reverse postorder: what each block may leave
//! unread at its end (its exit's sets) joins at the next block's start. A block entered from
//! paths that leave different sets unread, or from a back edge not lowered yet, may have both
//! sets unread: its first exchange meets once more before its store, in that exchange's own
//! block, where the halves meet anyway (no meeting where they might not). An exchange placed
//! into a block lowered earlier (a mask's ballot taken where the mask was written) keeps its
//! trailing meeting, so what the blocks after it were lowered for stays true. A program the
//! turns leave with as many meetings as before is lowered again without them, so it comes out
//! as with the switch off.

use std::collections::HashMap;

use super::{Bin, Lower};

/// Sets that may still be read: bit 0 the first, bit 1 the second.
const BOTH: u8 = 3;

/// Whether the exchanges take turns (`VICTORY_EXCHANGE_PAIRS=1`); a test can choose for its
/// own thread.
pub fn enabled() -> bool {
    #[cfg(test)]
    if let Some(on) = TEST_PAIRS.get() {
        return on;
    }
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("VICTORY_EXCHANGE_PAIRS").is_ok_and(|v| v.trim() == "1"))
}

#[cfg(test)]
thread_local! {
    /// A test's choice for [`enabled`], for its thread only.
    pub(crate) static TEST_PAIRS: std::cell::Cell<Option<bool>> =
        const { std::cell::Cell::new(None) };
}

/// The state of the turns, owned by [`Lower`].
#[derive(Debug, Default)]
pub(super) struct ExchangePairs {
    /// This shader's exchanges take turns.
    pub on: bool,
    /// Dwords of one set (2, or 4 for a four-dword exchange).
    pub stride: u32,
    /// The sets the code lowered so far may leave unread where it ends.
    unread: u8,
    /// Per IR block, the sets its exit may leave unread (`None`: not lowered yet).
    exits: Vec<Option<u8>>,
    /// The emitted block that ends a lowered IR block, and the sets it may leave unread.
    tails: HashMap<usize, u8>,
    /// Meetings added where both sets may be unread, and trailing meetings left out.
    pub added: u32,
    pub saved: u32,
}

impl ExchangePairs {
    pub(super) fn new(on: bool, stride: u32, blocks: usize) -> Self {
        Self {
            on,
            stride,
            exits: if on { vec![None; blocks] } else { Vec::new() },
            ..Self::default()
        }
    }
}

/// Where an exchange stands: its set's first dword past the console wave's, and whether it
/// may leave out its trailing meeting.
#[derive(Clone, Copy)]
pub(super) struct Turn {
    pub offset: u32,
    pub(super) set: u8,
    pub(super) current: bool,
}

impl Lower<'_> {
    /// At the start of IR block `b`: the sets its reachable predecessors may leave unread, both
    /// where one of them is not lowered yet (a back edge). The entry goes on from what the code
    /// before it (the program's inputs) left.
    pub(super) fn exchange_entry(&mut self, b: usize) {
        if !self.xpairs.on {
            return;
        }
        let mut reached = false;
        let mut unread = 0;
        for &q in &self.x.p.blocks[b].preds {
            if self.block[q].is_some() {
                reached = true;
                unread |= self.xpairs.exits[q].unwrap_or(BOTH);
            }
        }
        if reached {
            self.xpairs.unread = unread;
        }
    }

    /// After IR block `b` is lowered (its exit too), ending in `self.cur`.
    pub(super) fn exchange_exit(&mut self, b: usize) {
        if !self.xpairs.on {
            return;
        }
        self.xpairs.exits[b] = Some(self.xpairs.unread);
        self.xpairs.tails.insert(self.cur, self.xpairs.unread);
    }

    /// Before an exchange's store in `at`: the block to store in (after one more meeting where
    /// both sets may be unread) and the set to use.
    pub(super) fn exchange_turn(&mut self, at: usize) -> (usize, Turn) {
        let off = Turn {
            offset: 0,
            set: 0,
            current: false,
        };
        if !self.xpairs.on {
            return (at, off);
        }
        let current = at == self.cur;
        let mut unread = if current {
            self.xpairs.unread
        } else {
            self.xpairs.tails.get(&at).copied().unwrap_or(BOTH)
        };
        let mut at = at;
        if unread == BOTH {
            at = self.order_waves(at, self.reached_by_group());
            unread = 0;
            self.xpairs.added += 1;
        }
        let set = u8::from(unread & 1 != 0);
        let turn = Turn {
            offset: u32::from(set) * self.xpairs.stride,
            set,
            current,
        };
        (at, turn)
    }

    /// `pair` (the console wave's first exchange dword) moved to the turn's set.
    pub(super) fn exchange_set(&mut self, at: usize, pair: super::V, turn: Turn) -> super::V {
        if turn.offset == 0 {
            return pair;
        }
        let k = self.s.m.i32(turn.offset);
        self.s.m.bin(at, Bin::Add, pair, k)
    }

    /// After the exchange's loads in `at`: its trailing meeting, unless the turns make it
    /// unneeded. The block the code goes on in.
    pub(super) fn exchange_done(&mut self, at: usize, turn: Turn) -> usize {
        if self.xpairs.on && turn.current {
            self.xpairs.unread = 1 << turn.set;
            self.xpairs.saved += 1;
            return at;
        }
        self.order_waves(at, self.reached_by_group())
    }
}

#[cfg(test)]
mod tests {
    use super::TEST_PAIRS;
    use crate::emit::host::{Dispatch, Host, Root, View};
    use crate::emit::wave64_tests::native_of;
    use crate::emit::{HostShader, validator};
    use crate::native::{HwStage, NativeProgram, RootType};
    use crate::{io, ir, resource, structure};

    /// `v_cmp_gt_u32 vcc, s<n>, v0`: the lanes whose thread is below s<n>.
    const fn below(n: u32) -> u32 {
        0x7D88_0000 | n
    }
    /// `s_bcnt1_i32_b64 s<n>, vcc`: how many lanes of the console wave.
    const fn count(n: u32) -> u32 {
        0xBE80_106A | n << 16
    }
    /// s16 | s17 << 8 | s18 << 16 to every thread's dword.
    const STORE: [u32; 9] = [
        0x8F11_8811, // s_lshl_b32 s17, s17, 8
        0x8F12_9012, // s_lshl_b32 s18, s18, 16
        0x8810_1110, // s_or_b32 s16, s16, s17
        0x8810_1210, // s_or_b32 s16, s16, s18
        0x7E02_0210, // v_mov_b32 v1, s16
        0x3404_0082, // v_lshlrev_b32 v2, 2, v0
        0x38 << 26 | 0x1C << 18 | 1 << 12,
        0x80 << 24 | 2 << 16 | 1 << 8 | 2, // buffer_store_dword v1, v2, s[8:11], 0 offen
        0xBF80_0000,                       // s_nop 0
    ];

    /// Three counts in a row, each an exchange of the two host waves of a console wave.
    fn in_a_row() -> NativeProgram {
        let mut words = vec![below(0), count(16), below(1), count(17), below(2), count(18)];
        words.extend_from_slice(&STORE);
        native_of(&words, true, 128)
    }

    /// A count, then a second one only where s3 is not 0 (a branch the whole group takes
    /// together), then a third after the join: the two ways into it leave different sets unread.
    fn out_of_step() -> NativeProgram {
        let mut words = vec![
            0xBE91_0380, // s_mov_b32 s17, 0
            below(0),
            count(16),
            0xBF06_8003, // s_cmp_eq_u32 s3, 0
            0xBF85_0002, // s_cbranch_scc1 over the second count
            below(1),
            count(17),
            below(2),
            count(18),
        ];
        words.extend_from_slice(&STORE);
        native_of(&words, true, 128)
    }

    /// The console's lanes of wave `w` whose thread is below `x`.
    fn lanes(x: u32, w: u32) -> u32 {
        x.saturating_sub(64 * w).min(64)
    }

    /// The program for a host of 32-lane waves with the turns on or off, and its group barriers.
    fn lowered(native: &NativeProgram, on: bool) -> (HostShader, u32) {
        let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
        let st = structure::structure(&p).expect("structure");
        let res = resource::track(&p, &native.layout, native.stage, Some(&st)).expect("resource::track");
        let io = io::raise(&p, native).expect("io::raise");
        let was = super::super::TEST_HOST_WAVE.replace(Some(32));
        let pairs = TEST_PAIRS.replace(Some(on));
        let lowered = super::super::lower(&super::super::Parts {
            native,
            p: &p,
            st: &st,
            io: &io,
            res: &res,
            inputs: Vec::new(),
            outputs: Vec::new(),
            prims: Vec::new(),
            mesh_stride: 0,
        });
        TEST_PAIRS.set(pairs);
        super::super::TEST_HOST_WAVE.set(was);
        let l = lowered.expect("lower");
        if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
            let mut copy = l.dxil.clone();
            v.validate(&mut copy).expect("DXIL validation failed");
        }
        let barriers = l.group_barriers;
        let shader = HostShader {
            stage: HwStage::Cs,
            dxil: l.dxil,
            user_data: l.user_data,
            samplers: l.samplers,
            static_samplers: Vec::new(),
            root_cbvs: l.root_cbvs,
            root_srvs: l.root_srvs,
            root_uavs: l.root_uavs,
            root_indexed_views: Vec::new(),
            half_views: l.half_views,
        };
        (shader, barriers)
    }

    /// One group with root constants s0..s3: the 128 threads' dwords. On WARP where it takes
    /// the program, else (`WaveSize(32)`, which WARP refuses) on the hardware device as the
    /// other wave64 tests run; None with neither.
    fn on_warp(shader: &HostShader, constants: [u32; 4]) -> Option<Vec<u32>> {
        let warp = Host::new(true).ok().and_then(|h| h.compute_pipeline(shader).ok().map(|p| (h, p)));
        let card = || Host::new(false).ok().and_then(|h| h.compute_pipeline(shader).ok().map(|p| (h, p)));
        let Some((host, (root, pso))) = warp.or_else(card) else {
            eprintln!("neither WARP nor the hardware device takes the program; not run");
            return None;
        };
        let mut ud = vec![0u32; shader.user_data_dwords().max(1) as usize];
        for u in &shader.user_data {
            if let (RootType::Constants, 0, d @ 0..=3) = (u.root, u.slot, u.dword) {
                ud[u.index as usize] = constants[d as usize];
            }
        }
        let mut out = host
            .dispatch(
                &root,
                &pso,
                &Dispatch {
                    roots: vec![(0, Root::Constants(ud))],
                    views: &[View::RawUav(128)],
                    samplers: &[],
                    groups: [1, 1, 1],
                },
            )
            .unwrap_or_else(|e| panic!("dispatch failed: {e}"));
        Some(out.swap_remove(0))
    }

    #[test]
    fn exchanges_in_a_row_take_turns_and_count_both_halves() {
        let native = in_a_row();
        let (off, off_barriers) = lowered(&native, false);
        let (on, on_barriers) = lowered(&native, true);
        assert_eq!(off_barriers, on_barriers + 3, "one meeting fewer per exchange");
        assert_eq!(lowered(&native, false).0.dxil, off.dxil, "off translates alike each time");
        let _one = crate::emit::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
        for s in [[128, 128, 128], [40, 100, 70], [0, 64, 127], [33, 1, 96], [63, 65, 31]] {
            let want: Vec<u32> = (0..128)
                .map(|t| {
                    let w = t / 64;
                    lanes(s[0], w) | lanes(s[1], w) << 8 | lanes(s[2], w) << 16
                })
                .collect();
            for (name, shader) in [("off", &off), ("on", &on)] {
                let Some(got) = on_warp(shader, [s[0], s[1], s[2], 0]) else {
                    return;
                };
                assert_eq!(got, want, "{name}, threads below {s:?}");
            }
        }
    }

    #[test]
    fn a_join_reached_out_of_step_meets_once_more_and_counts_both_halves() {
        let native = out_of_step();
        let (off, off_barriers) = lowered(&native, false);
        let (on, on_barriers) = lowered(&native, true);
        // three exchanges lose their trailing meeting, the join's first gains one before it
        assert_eq!(off_barriers, on_barriers + 3 - 1);
        let _one = crate::emit::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
        for s3 in [0u32, 1] {
            for s in [[128, 128, 128], [40, 100, 70], [0, 64, 127], [33, 1, 96]] {
                let want: Vec<u32> = (0..128)
                    .map(|t| {
                        let w = t / 64;
                        let second = if s3 == 0 { 0 } else { lanes(s[1], w) };
                        lanes(s[0], w) | second << 8 | lanes(s[2], w) << 16
                    })
                    .collect();
                for (name, shader) in [("off", &off), ("on", &on)] {
                    let Some(got) = on_warp(shader, [s[0], s[1], s[2], s3]) else {
                        return;
                    };
                    assert_eq!(got, want, "{name}, s3 {s3}, threads below {s:?}");
                }
            }
        }
    }
}
