//! Per-wave half-meets for groups of multiple console waves on a 32-lane host:
//! orders the two 32-lane host halves of one console wave without a group
//! barrier when a wave-wide branch means the other console wave of the group
//! may not arrive.

use super::halves::{all_lane_blocks, every_thread_blocks};
use super::{Attr, Bin, Lower, Rmw, Shader, Structured, Term, flags, need, pred};

thread_local! {
    /// See [`with_half_meets`].
    static HALF_MEETS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// The host's group-shared memory, in dwords (32 KiB).
const HOST_LDS_DWORDS: u32 = 8192;

/// Runs `f` while a group of two console waves orders its halves with a per-wave meet
/// instead of a group barrier, where not every thread of the group arrives.
pub(in super::super) fn with_half_meets<R>(f: impl FnOnce() -> R) -> R {
    let was = HALF_MEETS.replace(true);
    let r = f();
    HALF_MEETS.set(was);
    r
}

pub(in super::super) fn half_meets() -> bool {
    HALF_MEETS.get()
}

/// Per-wave half-meet state owned by [`Lower`].
pub(super) struct HalfMeet {
    /// This shader meets the two halves of each console wave on their own.
    pub enabled: bool,
    /// First LDS dword of the per-wave meet table, and how many sites fit.
    pub base: u32,
    pub cap: u32,
    pub waves: u32,
    pub meets: u32,
    /// Set when a meet cannot be placed; [`Lower::run`] then refuses the shader.
    pub refuse: Option<&'static str>,
    /// IR blocks both halves of a console wave run ([`all_lane_blocks`]).
    pub wave_all: Vec<bool>,
    /// IR blocks every thread of the group runs ([`every_thread_blocks`]).
    pub group_all: Vec<bool>,
    /// The open block after the last half meet, when one split the current block.
    pub sync_block: usize,
    #[cfg(test)]
    pub group_barriers: u32,
    #[cfg(test)]
    pub half_meet_count: u32,
    #[cfg(test)]
    pub group_sync_blocks: Vec<usize>,
}

impl HalfMeet {
    /// Reserves room for one dword pair per console wave per meet behind the
    /// program's LDS. A site that does not fit is refused rather than declared
    /// past the host's 32 KiB.
    pub(super) fn new(s: &mut Shader, st: &Structured) -> Self {
        let enabled = HALF_MEETS.get() && s.wave_size == Some(32);
        let (base, cap, waves, wave_all, group_all) = if enabled {
            let group = s.threads.map(|t| t.into_iter().product::<u32>()).unwrap_or(0);
            let waves = group.div_ceil(64).max(1);
            let spare = HOST_LDS_DWORDS.saturating_sub(s.lds_dwords);
            let per = 2 * waves;
            let cap = if per == 0 { 0 } else { spare / per };
            let base = s.lds_dwords;
            s.lds_dwords += cap * per;
            (
                base,
                cap,
                waves,
                all_lane_blocks(st).unwrap_or_default(),
                every_thread_blocks(st).unwrap_or_default(),
            )
        } else {
            (0, 0, 0, Vec::new(), Vec::new())
        };
        Self {
            enabled,
            base,
            cap,
            waves,
            meets: 0,
            refuse: None,
            wave_all,
            group_all,
            sync_block: 0,
            #[cfg(test)]
            group_barriers: 0,
            #[cfg(test)]
            half_meet_count: 0,
            #[cfg(test)]
            group_sync_blocks: Vec::new(),
        }
    }
}

impl<'a> Lower<'a> {
    /// Every thread of the group runs the IR block now being lowered.
    pub(super) fn reached_by_group(&self) -> bool {
        self.meet
            .group_all
            .get(self.cur_ir)
            .copied()
            .unwrap_or(false)
    }

    /// Both halves of each console wave run `block` (a wave-wide branch counts; the other
    /// console wave of the group need not).
    pub(super) fn both_halves(&self, block: usize) -> bool {
        self.meet.wave_all.get(block).copied().unwrap_or(false)
    }

    /// Global UAV memory fence without a thread-group wait (DXIL barrier mode 2).
    pub(super) fn memory_fence(&mut self) {
        let mode = self.s.m.i32(2);
        let void = self.s.t.void;
        self.s
            .op(self.cur, "barrier", Attr::NoDuplicate, void, 80, &[mode]);
    }

    /// A group barrier when every thread arrives, otherwise a meet of this console wave's two
    /// halves. A meet in a block a half does not run is refused: the wait would not finish.
    pub(super) fn order_waves(&mut self, block: usize, everyone: bool) -> usize {
        let done = if !self.meet.enabled || everyone {
            #[cfg(test)]
            self.meet.group_sync_blocks.push(self.cur_ir);
            self.group_sync(block);
            block
        } else if !self.both_halves(self.cur_ir) {
            if self.meet.refuse.is_none() {
                need(self.cur_ir);
                self.meet.refuse = Some("a half-meet where not every lane runs");
            }
            block
        } else {
            self.half_meet_at(block)
        };
        if block == self.cur {
            self.cur = done;
        }
        self.meet.sync_block = done;
        done
    }

    /// One visit of this console wave, in its own pair of LDS dwords, with its own wait block.
    /// The first lane of each half publishes the next count; the other half reads that dword
    /// until the count has reached this visit. A later visit of a loop does not reuse the count.
    fn half_meet_at(&mut self, block: usize) -> usize {
        if self.meet.meets >= self.meet.cap {
            if self.meet.refuse.is_none() {
                self.meet.refuse = Some("more half-meets than the group memory holds");
            }
            return block;
        }
        let site = self.meet.meets;
        self.meet.meets += 1;
        #[cfg(test)]
        {
            self.meet.half_meet_count += 1;
        }
        let thread = self.console_thread();
        let sixty3 = self.s.m.i32(63);
        let in_wave = self.s.m.bin(block, Bin::And, thread, sixty3);
        let five = self.s.m.i32(5);
        let half = self.s.m.bin(block, Bin::LShr, in_wave, five);
        let six = self.s.m.i32(6);
        let wave = self.s.m.bin(block, Bin::LShr, thread, six);
        let per = self.s.m.i32(2 * self.meet.waves);
        let site_v = self.s.m.i32(site);
        let wave_pair = self.s.m.bin(block, Bin::Mul, site_v, per);
        let two = self.s.m.i32(2);
        let base = self.s.m.i32(self.meet.base);
        let at_wave = self.s.m.bin(block, Bin::Add, base, wave_pair);
        let wave_off = self.s.m.bin(block, Bin::Mul, wave, two);
        let at_wave = self.s.m.bin(block, Bin::Add, at_wave, wave_off);
        let index = self.s.m.bin(block, Bin::Add, at_wave, half);
        let one = self.s.m.i32(1);
        let flipped = self.s.m.bin(block, Bin::Xor, half, one);
        let other = self.s.m.bin(block, Bin::Add, at_wave, flipped);
        let thirty1 = self.s.m.i32(31);
        let lane_in_half = self.s.m.bin(block, Bin::And, in_wave, thirty1);
        let zero = self.s.m.i32(0);
        let first = self.s.m.cmp(block, pred::EQ, lane_in_half, zero);
        let add = self.s.m.select(block, first, one, zero);
        let lds = self.s.lds();
        let mine = self.s.m.element(block, lds, index);
        // A loop visits the site again, so the count goes up. The first lane's old
        // value is the one the whole half waits on; a later visit is already enough.
        let old = self.s.m.atomic(block, Rmw::Add, mine, add);
        let mine_old = self.s.m.select(block, first, old, zero);
        let i32t = self.s.t.i32;
        let shared =
            self.s
                .op(block, "waveReadLaneFirst.i32", Attr::NoUnwind, i32t, 118, &[mine_old]);
        self.s.flags |= flags::WAVE_OPS;
        let generation = self.s.m.bin(block, Bin::Add, shared, one);
        let (wait, done) = (self.s.m.new_block(), self.s.m.new_block());
        self.s.m.terminate(block, Term::Br(wait));
        let theirs = self.s.m.element(wait, lds, other);
        let seen = self.s.m.atomic(wait, Rmw::Add, theirs, zero);
        let ready = self.s.m.cmp(wait, pred::IUGE, seen, generation);
        self.s.m.terminate(wait, Term::CondBr(ready, done, wait));
        // The rest of this instruction was aimed at the block the meet closed.
        self.s.m.continues(block, done);
        done
    }

    /// Zeroes the meet table. Every thread runs it, then one group barrier, before any meet.
    pub(super) fn zero_meet_slots(&mut self) {
        if self.meet.cap == 0 {
            return;
        }
        let n = self.meet.cap * 2 * self.meet.waves.max(1);
        let entry = self.s.entry;
        let (head, done) = (self.s.m.new_block(), self.s.m.new_block());
        self.s.m.terminate(entry, Term::Br(head));
        let i = self.s.m.phi(head, self.s.t.i32);
        let base = self.s.m.i32(self.meet.base);
        let idx = self.s.m.bin(head, Bin::Add, base, i);
        let lds = self.s.lds();
        let ptr = self.s.m.element(head, lds, idx);
        let zero = self.s.m.i32(0);
        self.s.m.store(head, ptr, zero);
        let one = self.s.m.i32(1);
        let next = self.s.m.bin(head, Bin::Add, i, one);
        let last = self.s.m.i32(n);
        let more = self.s.m.cmp(head, pred::IULT, next, last);
        self.s.m.terminate(head, Term::CondBr(more, head, done));
        let start = self.s.m.i32(0);
        self.s.m.set_phi(i, vec![(start, entry), (next, head)]);
        self.group_sync(done);
        self.s.entry = done;
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Parts, TEST_HOST_WAVE, lower};
    use super::with_half_meets;
    use crate::native::{ComputeIo, HwStage, NativeProgram, RootParam, RootType, StageIo};
    use crate::{io, ir, resource, structure};

    const S_ENDPGM: u32 = 0xBF81_0000;

    /// A 128-thread group whose divergent region both reads and writes LDS. With that branch
    /// taken by both halves of a wave, the program lowers to wave 32 and the LDS sites meet
    /// per wave. Without the branch marked, no meet is emitted in the divergent block.
    #[test]
    fn half_meet_orders_a_128_thread_lds_region() {
        let words = [
            0x7E02_0281, // v_mov_b32 v1, 1
            0x7D88_0000, // v_cmp_gt_u32 vcc, s0, v0
            0xBE8C_246A, // s_and_saveexec_b64 s[12:13], vcc
            0xBF88_0004, // s_cbranch_execz past the LDS pair
            0x36 << 26 | 0x0D << 18, // ds_write_b32
            1 << 8 | 1,
            0x36 << 26 | 0x36 << 18, // ds_read_b32
            2 << 24 | 1,
            0xBEFE_040C, // s_mov_b64 exec, s[12:13]
            S_ENDPGM,
        ];
        let code: Vec<u8> = words.into_iter().flat_map(u32::to_le_bytes).collect();
        let native = NativeProgram {
            stage: HwStage::Cs,
            wave64: true,
            vgprs: 16,
            layout: vec![RootParam {
                root: RootType::Constants,
                table_offset_dwords: 0,
                sgprs: 2,
                base_register: 0,
                user_data: 0,
                slot: 0,
            }],
            used: vec![],
            io: StageIo::Cs(ComputeIo {
                rsrc2: 1 << 15 | 2 << 1,
                threads: [128, 1, 1],
            }),
            code,
        };
        let p = ir::build(&native.code, true, 16).unwrap();
        let lds_blocks: Vec<usize> = p
            .blocks
            .iter()
            .enumerate()
            .filter(|(_, b)| {
                b.insts
                    .iter()
                    .any(|i| i.name() == "ds_write_b32" || i.name() == "ds_read_b32")
            })
            .map(|(i, _)| i)
            .collect();
        assert!(!lds_blocks.is_empty(), "the LDS pair did not decode");
        let branch = p
            .blocks
            .iter()
            .position(|b| b.insts.iter().any(|i| i.name() == "s_cbranch_execz"))
            .unwrap();
        let mut wide = std::collections::HashSet::new();
        wide.insert(branch);
        let st = structure::structure_wide(&p, &wide).unwrap();
        let res = resource::track(&p, &native.layout, native.stage, Some(&st)).unwrap();
        let io = io::raise(&p, &native).unwrap();
        let was = TEST_HOST_WAVE.replace(Some(32));
        let lowered = with_half_meets(|| {
            lower(&Parts {
                native: &native,
                p: &p,
                st: &st,
                io: &io,
                res: &res,
                inputs: vec![],
                outputs: vec![],
                prims: vec![],
                mesh_stride: 0,
            })
        });
        TEST_HOST_WAVE.set(was);
        let lowered = lowered.expect("wave 32 with a per-wave meet");
        assert_eq!(lowered.wave_size, Some(32));
        assert!(lowered.half_meets >= 1, "the LDS region did not meet per wave");
        assert!(lowered.group_barriers >= 1, "the entry lost its group barrier");
        for b in &lds_blocks {
            assert!(
                !lowered.group_sync_blocks.contains(b),
                "a group barrier landed inside the divergent LDS block {b}"
            );
        }
        let st = structure::structure(&p).unwrap();
        let res = resource::track(&p, &native.layout, native.stage, Some(&st)).unwrap();
        let io = io::raise(&p, &native).unwrap();
        let was = TEST_HOST_WAVE.replace(Some(32));
        let plain = with_half_meets(|| {
            lower(&Parts {
                native: &native,
                p: &p,
                st: &st,
                io: &io,
                res: &res,
                inputs: vec![],
                outputs: vec![],
                prims: vec![],
                mesh_stride: 0,
            })
        });
        TEST_HOST_WAVE.set(was);
        match plain {
            Ok(l) => assert_eq!(
                l.half_meets, 0,
                "a meet was emitted where a half may not arrive"
            ),
            Err(e) => {
                let crate::ShaderError::Unsupported { what, .. } = e else {
                    panic!("unexpected {e}");
                };
                assert!(
                    what == "a half-meet where not every lane runs"
                        || what == "no wave32 lowering for this wave64 program"
                        || what.ends_with("where not every lane runs"),
                    "unexpected refusal: {what}"
                );
            }
        }
    }
}
