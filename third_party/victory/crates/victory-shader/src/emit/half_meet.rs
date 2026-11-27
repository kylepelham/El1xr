//! Per-wave half-meets for groups of multiple console waves on a 32-lane host:
//! orders the two 32-lane host halves of one console wave without a group
//! barrier when a wave-wide branch means the other console wave of the group
//! may not arrive.

use std::sync::atomic::{AtomicU32, Ordering};

use super::halves::{all_lane_blocks, every_thread_blocks};
use super::{Attr, Bin, Lower, Rmw, Shader, Structured, Term, V, flags, need, pred};

thread_local! {
    /// See [`with_half_meets`].
    static HALF_MEETS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// A test's choice for [`meet_poll_cap`], for its thread only.
    #[cfg(test)]
    static TEST_POLL_CAP: std::cell::Cell<Option<Option<u32>>> = const { std::cell::Cell::new(None) };
}

/// `VICTORY_MEET_POLLS=<n>`: a half that has polled its partner's dword `n` times goes on
/// without it (a partner cut out of a loop at the loop limit then costs wrong values, not a
/// hung card), and its first lane counts the give-up in a fourth tally dword the host logs.
/// Unset or 0: the wait has no cap, as before. Part of the translation (the shader cache
/// keys on it). After trustno1-coder/VIctory-NVIDIA `4f49709` (`MEETING_POLLS`, 4096).
fn meet_poll_cap() -> Option<u32> {
    #[cfg(test)]
    if let Some(cap) = TEST_POLL_CAP.get() {
        return cap;
    }
    static CAP: std::sync::OnceLock<Option<u32>> = std::sync::OnceLock::new();
    *CAP.get_or_init(|| parse_poll_cap(std::env::var("VICTORY_MEET_POLLS").ok().as_deref()))
}

/// [`meet_poll_cap`]'s reading of the switch. Pure, for testing.
fn parse_poll_cap(value: Option<&str>) -> Option<u32> {
    value?.trim().parse::<u32>().ok().filter(|&n| n > 0)
}

/// The host's group-shared memory, in dwords (32 KiB).
const HOST_LDS_DWORDS: u32 = 8192;

/// Meet sites emitted for the face-relocation compute, one number for the whole process.
static MEET_SITES: AtomicU32 = AtomicU32::new(0);
/// Of those sites, the ones that order a shader LDS read, and a shader LDS write.
static MEET_LDS_READS: AtomicU32 = AtomicU32::new(0);
static MEET_LDS_WRITES: AtomicU32 = AtomicU32::new(0);

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

pub(in super::super) fn meet_sites() -> u32 {
    MEET_SITES.load(Ordering::Relaxed)
}

/// Shader LDS reads and writes that got their own half-meet, or `(0, 0)` before translation.
pub(in super::super) fn meet_lds_sites() -> (u32, u32) {
    (
        MEET_LDS_READS.load(Ordering::Relaxed),
        MEET_LDS_WRITES.load(Ordering::Relaxed),
    )
}

/// Per-wave half-meet state owned by [`Lower`].
pub(super) struct HalfMeet {
    /// This shader meets the two halves of each console wave on their own.
    pub enabled: bool,
    /// First LDS dword of the per-wave meet table, and how many sites fit.
    pub base: u32,
    pub cap: u32,
    pub waves: u32,
    /// First of three LDS dwords: meetings, wait polls, wait atomics; a fourth, waits that
    /// gave up, with [`meet_poll_cap`].
    pub tally: u32,
    /// Tally dwords: 3, or 4 with a poll cap.
    pub tallies: u32,
    /// [`meet_poll_cap`], read once per shader.
    pub poll_cap: Option<u32>,
    pub meets: u32,
    /// Half-meet sites that order a shader LDS read, and those that order a write.
    pub lds_reads: u32,
    pub lds_writes: u32,
    /// The counter UAV, made once in the entry.
    pub uav: Option<V>,
    /// The last LDS access in this block since a meet: `false` a read, `true` a write.
    pub lds_run: Option<bool>,
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
    /// program's LDS, plus three tally dwords. A site that does not fit is
    /// refused rather than declared past the host's 32 KiB.
    pub(super) fn new(s: &mut Shader, st: &Structured) -> Self {
        let enabled = HALF_MEETS.get() && s.wave_size == Some(32);
        let poll_cap = if enabled { meet_poll_cap() } else { None };
        let tallies = 3 + u32::from(poll_cap.is_some());
        let (base, cap, waves, tally, wave_all, group_all) = if enabled {
            let group = s.threads.map(|t| t.into_iter().product::<u32>()).unwrap_or(0);
            let waves = group.div_ceil(64).max(1);
            // Three dwords past the table count meetings, wait polls and wait atomics
            // (a fourth the waits that gave up, with a poll cap).
            let spare = HOST_LDS_DWORDS
                .saturating_sub(s.lds_dwords)
                .saturating_sub(tallies);
            let per = 2 * waves;
            let cap = if per == 0 { 0 } else { spare / per };
            let base = s.lds_dwords;
            s.lds_dwords += cap * per;
            let tally = s.lds_dwords;
            s.lds_dwords += tallies;
            (
                base,
                cap,
                waves,
                tally,
                all_lane_blocks(st).unwrap_or_default(),
                every_thread_blocks(st).unwrap_or_default(),
            )
        } else {
            (0, 0, 0, 0, Vec::new(), Vec::new())
        };
        Self {
            enabled,
            base,
            cap,
            waves,
            tally,
            tallies,
            poll_cap,
            meets: 0,
            lds_reads: 0,
            lds_writes: 0,
            uav: None,
            lds_run: None,
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

    /// Publishes process-wide meet/LDS counts after lowering finishes.
    pub(super) fn record_counts(&self) {
        if self.enabled {
            MEET_SITES.store(self.meets, Ordering::Relaxed);
            MEET_LDS_READS.store(self.lds_reads, Ordering::Relaxed);
            MEET_LDS_WRITES.store(self.lds_writes, Ordering::Relaxed);
        }
    }
}

impl<'a> Lower<'a> {
    /// Entry-block setup when per-wave half-meets are active: zeroes the meet
    /// table + tally slots, initialises the wave counter, and allocates the
    /// telemetry root UAV handle.
    pub(super) fn init_half_meet(&mut self) {
        if self.meet.enabled {
            // The meet slots start at zero, then the wave counter, both before any thread leaves.
            self.zero_meet_slots();
            let _ = self.wave_base();
            self.meet.uav = Some(self.s.root_uav_handle(super::super::shader::SLOT_MEET));
        }
    }

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

    /// Orders an LDS instruction when `split_lds` is active. Consecutive reads
    /// in one block already saw earlier writes at the first read; writes still
    /// meet so the other half finishes prior accesses and two writes stay ordered.
    pub(super) fn lds_access_meet(&mut self, name: &str) {
        let write = !name.starts_with("ds_read");
        let owed = !self.meet.enabled || write || self.meet.lds_run != Some(false);
        if owed {
            if self.meet.enabled && !self.reached_by_group() && self.both_halves(self.cur_ir) {
                if write {
                    self.meet.lds_writes += 1;
                } else {
                    self.meet.lds_reads += 1;
                }
            }
            self.cur = self.order_waves(self.cur, self.reached_by_group());
        }
        if self.meet.enabled {
            self.meet.lds_run = Some(write);
        }
    }

    /// A group barrier when every thread arrives, otherwise a meet of this console wave's two
    /// halves. A meet in a block a half does not run is refused: the wait would not finish.
    pub(super) fn order_waves(&mut self, block: usize, everyone: bool) -> usize {
        // A meet or a group barrier ends a run of reads: the next access orders again.
        self.meet.lds_run = None;
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
    /// with a volatile load until the count has reached this visit.
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
        // Publish on this half's dword, then wait on the other half's. One lane of the
        // half does each: the other lanes take that lane's value. A plain volatile load
        // waits, so the publisher is the only writer of its dword.
        let (yes, no, join) = (
            self.s.m.new_block(),
            self.s.m.new_block(),
            self.s.m.new_block(),
        );
        self.s.m.terminate(block, Term::CondBr(first, yes, no));
        let lds = self.s.lds();
        let mine = self.s.m.element(yes, lds, index);
        let old_yes = self.s.m.atomic(yes, Rmw::Add, mine, one);
        self.s.m.terminate(yes, Term::Br(join));
        self.s.m.terminate(no, Term::Br(join));
        let i32t = self.s.t.i32;
        let old = self.s.m.phi(join, i32t);
        self.s.m.set_phi(old, vec![(old_yes, yes), (zero, no)]);
        let shared =
            self.s
                .op(join, "waveReadLaneFirst.i32", Attr::NoUnwind, i32t, 118, &[old]);
        self.s.flags |= flags::WAVE_OPS;
        let generation = self.s.m.bin(join, Bin::Add, shared, one);
        let (wait, done) = (self.s.m.new_block(), self.s.m.new_block());
        self.s.m.terminate(join, Term::Br(wait));
        let polls = self.s.m.phi(wait, i32t);
        let (lyes, lno, ljoin) = (
            self.s.m.new_block(),
            self.s.m.new_block(),
            self.s.m.new_block(),
        );
        self.s.m.terminate(wait, Term::CondBr(first, lyes, lno));
        let theirs = self.s.m.element(lyes, lds, other);
        let seen_yes = self.s.m.load_volatile(lyes, theirs);
        self.s.m.terminate(lyes, Term::Br(ljoin));
        self.s.m.terminate(lno, Term::Br(ljoin));
        let seen_lane = self.s.m.phi(ljoin, i32t);
        self.s
            .m
            .set_phi(seen_lane, vec![(seen_yes, lyes), (zero, lno)]);
        let seen = self.s.op(
            ljoin,
            "waveReadLaneFirst.i32",
            Attr::NoUnwind,
            i32t,
            118,
            &[seen_lane],
        );
        let ready = self.s.m.cmp(ljoin, pred::IUGE, seen, generation);
        let polled = self.s.m.bin(ljoin, Bin::Add, polls, one);
        let leave = match self.meet.poll_cap {
            Some(cap) => {
                let cap = self.s.m.i32(cap);
                let spent = self.s.m.cmp(ljoin, pred::IUGE, polled, cap);
                self.s.m.bin(ljoin, Bin::Or, ready, spent)
            }
            None => ready,
        };
        self.s.m.terminate(ljoin, Term::CondBr(leave, done, wait));
        let start = self.s.m.i32(0);
        self.s.m.set_phi(polls, vec![(start, join), (polled, ljoin)]);
        self.cur = done;
        let who = self.tally_lane(done);
        let guarded = self.begin_guarded(Some(who));
        let here = self.cur;
        self.lds_tally(here, 0, one);
        self.lds_tally(here, 1, polled);
        self.end_guarded(guarded, &[]);
        if self.meet.poll_cap.is_some() {
            // The half went on without its partner: its first lane counts it.
            let here = self.cur;
            let short = self.s.m.cmp(here, pred::IULT, seen, generation);
            let gave_up = self.s.m.bin(here, Bin::And, first, short);
            let guarded = self.begin_guarded(Some(gave_up));
            let here = self.cur;
            self.lds_tally(here, 3, one);
            self.end_guarded(guarded, &[]);
        }
        let after = self.cur;
        // The rest of this instruction was aimed at the block the meet closed.
        self.s.m.continues(block, after);
        after
    }

    /// Thread 0 of the group: wave 0, half 0, its first lane.
    fn tally_lane(&mut self, block: usize) -> V {
        let thread = self.console_thread();
        let zero = self.s.m.i32(0);
        self.s.m.cmp(block, pred::EQ, thread, zero)
    }

    /// Adds `value` into tally dword `slot` (0 meetings, 1 polls, 2 wait atomics, 3 waits that
    /// gave up).
    fn lds_tally(&mut self, block: usize, slot: u32, value: V) {
        let lds = self.s.lds();
        let idx = self.s.m.i32(self.meet.tally + slot);
        let ptr = self.s.m.element(block, lds, idx);
        let _ = self.s.m.atomic(block, Rmw::Add, ptr, value);
    }

    /// Adds this group's tally into the counter UAV and clears it, so a second
    /// return from the same group adds nothing.
    pub(super) fn flush_meet_totals(&mut self) {
        let Some(h) = self.meet.uav else {
            return;
        };
        let who = self.tally_lane(self.cur);
        let guarded = self.begin_guarded(Some(who));
        let block = self.cur;
        let lds = self.s.lds();
        let i32t = self.s.t.i32;
        let iu = self.s.m.undef(i32t);
        let add = self.s.m.i32(0);
        let zero = self.s.m.i32(0);
        for slot in 0..self.meet.tallies {
            let idx = self.s.m.i32(self.meet.tally + slot);
            let ptr = self.s.m.element(block, lds, idx);
            let value = self.s.m.load(block, ptr);
            let at = self.s.m.i32(slot * 4);
            self.s.op(
                block,
                "atomicBinOp.i32",
                Attr::NoUnwind,
                i32t,
                78,
                &[h, add, at, iu, iu, value],
            );
            self.s.m.store(block, ptr, zero);
        }
        self.end_guarded(guarded, &[]);
    }

    /// Zeroes the meet table and tally dwords. Every thread runs it, then one
    /// group barrier, before any meet.
    fn zero_meet_slots(&mut self) {
        if self.meet.cap == 0 {
            return;
        }
        let n = self.meet.cap * 2 * self.meet.waves.max(1) + self.meet.tallies;
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
    use super::super::{Lowered, Parts, TEST_HOST_WAVE, lower};
    use super::{TEST_POLL_CAP, parse_poll_cap, with_half_meets};
    use crate::native::{ComputeIo, HwStage, NativeProgram, RootParam, RootType, StageIo};
    use crate::{io, ir, resource, structure};

    const S_ENDPGM: u32 = 0xBF81_0000;

    fn make_128_lds_program(lds_words: &[u32]) -> NativeProgram {
        let mut words = vec![
            0x7E02_0281, // v_mov_b32 v1, 1
            0x7D88_0000, // v_cmp_gt_u32 vcc, s0, v0
            0xBE8C_246A, // s_and_saveexec_b64 s[12:13], vcc
            0xBF88_0000 | (lds_words.len() as u32), // s_cbranch_execz past the LDS sequence
        ];
        words.extend_from_slice(lds_words);
        words.push(0xBEFE_040C); // s_mov_b64 exec, s[12:13]
        words.push(S_ENDPGM);
        let code: Vec<u8> = words.into_iter().flat_map(u32::to_le_bytes).collect();
        NativeProgram {
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
        }
    }

    fn lower_wide_half_meet(native: &NativeProgram) -> (Lowered, Vec<usize>) {
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
        let io = io::raise(&p, native).unwrap();
        let was = TEST_HOST_WAVE.replace(Some(32));
        let lowered = with_half_meets(|| {
            lower(&Parts {
                native,
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
        (lowered.expect("wave 32 with a per-wave meet"), lds_blocks)
    }

    /// A 128-thread group whose divergent region both reads and writes LDS. With that branch
    /// taken by both halves of a wave, the program lowers to wave 32 and the LDS sites meet
    /// per wave. Without the branch marked, no meet is emitted in the divergent block.
    #[test]
    fn half_meet_orders_a_128_thread_lds_region() {
        let native = make_128_lds_program(&[
            0x36 << 26 | 0x0D << 18, // ds_write_b32
            1 << 8 | 1,
            0x36 << 26 | 0x36 << 18, // ds_read_b32
            2 << 24 | 1,
        ]);
        let (lowered, lds_blocks) = lower_wide_half_meet(&native);
        assert_eq!(lowered.wave_size, Some(32));
        assert!(lowered.half_meets >= 1, "the LDS region did not meet per wave");
        assert!(lowered.group_barriers >= 1, "the entry lost its group barrier");
        assert!(
            lowered.root_uavs.contains(&super::super::super::shader::SLOT_MEET),
            "half-meet shader must declare SLOT_MEET root UAV"
        );
        for b in &lds_blocks {
            assert!(
                !lowered.group_sync_blocks.contains(b),
                "a group barrier landed inside the divergent LDS block {b}"
            );
        }
        #[cfg(windows)]
        {
            use super::super::super::validator::Validator;
            if let Ok(mut v) = Validator::load(Validator::default_path().as_deref()) {
                v.validate(&mut lowered.dxil.clone())
                    .expect("half-meet DXIL failed validation");
            }
        }

        let p = ir::build(&native.code, true, 16).unwrap();
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

    /// `VICTORY_MEET_POLLS`: unset, 0 or unreadable is no cap; a number is the cap.
    #[test]
    fn meet_poll_cap_reads_the_switch() {
        assert_eq!(parse_poll_cap(None), None);
        assert_eq!(parse_poll_cap(Some("0")), None);
        assert_eq!(parse_poll_cap(Some("lots")), None);
        assert_eq!(parse_poll_cap(Some(" 4096 ")), Some(4096));
    }

    /// With a poll cap the wait has a second way out and the program a fourth tally dword;
    /// it still validates. Without one the translation is the uncapped one, byte for byte.
    #[test]
    fn meet_poll_cap_bounds_the_wait() {
        let native = make_128_lds_program(&[
            0x36 << 26 | 0x0D << 18, // ds_write_b32
            1 << 8 | 1,
            0x36 << 26 | 0x36 << 18, // ds_read_b32
            2 << 24 | 1,
        ]);
        let with = |cap: Option<u32>| {
            let was = TEST_POLL_CAP.replace(Some(cap));
            let (l, _) = lower_wide_half_meet(&native);
            TEST_POLL_CAP.set(was);
            l
        };
        let open = with(None);
        let again = with(None);
        let capped = with(Some(4096));
        assert_eq!(open.dxil, again.dxil, "the uncapped translation is not stable");
        assert_ne!(open.dxil, capped.dxil, "the cap left the translation as it was");
        assert_eq!(open.half_meets, capped.half_meets);
        #[cfg(windows)]
        {
            use super::super::super::validator::Validator;
            if let Ok(mut v) = Validator::load(Validator::default_path().as_deref()) {
                v.validate(&mut capped.dxil.clone())
                    .expect("capped half-meet DXIL failed validation");
            }
        }
    }

    /// Consecutive `ds_read_b32` instructions in the same divergent block share
    /// one half-meet, while a subsequent `ds_write_b32` introduces another meet.
    #[test]
    fn half_meet_coalesces_consecutive_ds_reads() {
        let two_reads = make_128_lds_program(&[
            0x36 << 26 | 0x0D << 18, // ds_write_b32
            1 << 8 | 1,
            0x36 << 26 | 0x36 << 18, // ds_read_b32 #1
            2 << 24 | 1,
            0x36 << 26 | 0x36 << 18, // ds_read_b32 #2 (coalesced with #1)
            3 << 24 | 1,
        ]);
        let one_read = make_128_lds_program(&[
            0x36 << 26 | 0x0D << 18, // ds_write_b32
            1 << 8 | 1,
            0x36 << 26 | 0x36 << 18, // ds_read_b32 #1
            2 << 24 | 1,
        ]);
        let write_read_write = make_128_lds_program(&[
            0x36 << 26 | 0x0D << 18, // ds_write_b32 #1
            1 << 8 | 1,
            0x36 << 26 | 0x36 << 18, // ds_read_b32
            2 << 24 | 1,
            0x36 << 26 | 0x0D << 18, // ds_write_b32 #2 (not coalesced)
            1 << 8 | 1,
        ]);
        let (l_one, _) = lower_wide_half_meet(&one_read);
        let (l_two, _) = lower_wide_half_meet(&two_reads);
        let (l_wrw, _) = lower_wide_half_meet(&write_read_write);
        assert_eq!(
            l_two.half_meets, l_one.half_meets,
            "second consecutive ds_read should share the first read's half-meet"
        );
        assert_eq!(
            l_wrw.half_meets,
            l_one.half_meets + 1,
            "ds_write after ds_read must emit its own half-meet"
        );
    }
}
