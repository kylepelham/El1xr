//! `VICTORY_DISPATCHX_MASK=1` (off by default): the lanes past a `DispatchX`
//! thread count store nothing.
//!
//! The console's `DispatchX` with `COMPUTE_DISPATCH_INITIATOR` bit 5
//! (`USE_THREAD_DIMENSIONS`) counts threads, not groups: the last group's
//! lanes past the count are never started. `VICTORY_DISPATCHX_THREADS`
//! (`victory-gpu`'s `rt_diag`) launches the groups that hold the count,
//! rounded up, so up to a group less one lane run past it. In an offline
//! replay of the structure builder's submissions (Marvisak/VIctory issue
//! #3486, Slay326) those lanes wrote outside the build's ranges until they
//! were masked too.
//!
//! The structure builder's programs (compute programs with a spilled
//! user-data table, as `victory-gpu`'s `of_builder` tells them) ask for a
//! host word, [`USER_THREAD_LIMIT`]: the dispatch's thread count in x, or
//! `u32::MAX` when it counts groups. Each buffer, global and image store or
//! atomic is made only by the lanes whose thread number in x
//! (`group * width + thread`) is below it.
//! shortcut: those lanes still run everything else: they take part in wave
//! operations (a ballot or count counts them) and in LDS writes, where the
//! console leaves them out of EXEC from the start. Only x is limited.

use super::{Bin, Input, Lower, RootType, StageIo, V, Val, Why, pred};
use crate::native::NativeProgram;

use super::super::shader::USER_THREAD_LIMIT;

/// Whether `VICTORY_DISPATCHX_MASK=1` (read once). For the shader cache key.
pub fn on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("VICTORY_DISPATCHX_MASK").is_ok_and(|v| v.trim() == "1"))
}

#[cfg(test)]
thread_local! {
    /// A test's choice, for its thread only: masks any compute program.
    pub(super) static TEST_ON: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether `native`'s stores are masked: a builder program with the switch on.
fn applies(native: &NativeProgram) -> bool {
    #[cfg(test)]
    if TEST_ON.get() {
        return matches!(native.io, StageIo::Cs(_));
    }
    on() && matches!(native.io, StageIo::Cs(_)) && crate::spill::is_spilled(native)
}

/// Whether instruction `name` writes memory a buffer or image holds.
fn stores(name: &str) -> bool {
    name.contains("store") || name.contains("atomic")
}

impl Lower<'_> {
    /// At the program's start, while the entry block is still open: whether
    /// each console lane of the thread is below the thread limit, when the
    /// program's stores are masked ([`applies`]).
    pub(super) fn thread_limit_init(&mut self) {
        let p = self.x.p;
        let any_store = || p.blocks.iter().any(|b| b.insts.iter().any(|i| stores(i.name())));
        if !applies(self.x.native) || !any_store() {
            return;
        }
        let StageIo::Cs(cs) = self.x.native.io else {
            return;
        };
        let width = cs.threads[0];
        let live = |l: &mut Self| -> Result<V, Why> {
            let (Val::I32(t), Val::I32(g)) = (l.thread_id(0), l.input(Input::GroupId(0))) else {
                return Err("a thread number");
            };
            let e = l.s.entry;
            let width = l.s.m.i32(width);
            let first = l.s.m.bin(e, Bin::Mul, g, width);
            let id = l.s.m.bin(e, Bin::Add, first, t);
            let limit = l.host_word(RootType::Other(USER_THREAD_LIMIT), 0, 0);
            Ok(l.s.m.cmp(e, pred::IULT, id, limit))
        };
        self.thread_live = if self.twin.is_some() {
            self.both(live).ok()
        } else {
            live(self).ok().map(|v| [v, v])
        };
    }

    /// `guard` of instruction `name`, with the lanes past the dispatch's
    /// thread count left out when it stores ([`Self::thread_limit_init`]).
    pub(super) fn thread_limit_guard(&mut self, name: &str, guard: Option<V>) -> Option<V> {
        let Some(live) = self.thread_live.filter(|_| stores(name)) else {
            return guard;
        };
        let live = live[usize::from(self.upper)];
        Some(match guard {
            Some(g) => self.s.m.bin(self.cur, Bin::And, g, live),
            None => live,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Lowered, Parts, lower};
    use super::{TEST_ON, USER_THREAD_LIMIT, stores};
    use crate::native::{
        ComputeIo, HwStage, NativeProgram, ResourceType, RootParam, RootType, StageIo, UsedResource,
    };
    use crate::{io, ir, resource, structure};

    const S_ENDPGM: u32 = 0xBF81_0000;

    /// 64 threads in x; `store`: each writes its thread number to dword 0 of
    /// the raw view at heap slot 0 (`buffer_store_dword v0, off, s[8:11]`),
    /// else it loads that dword.
    fn program(store: bool) -> NativeProgram {
        let mut words = vec![
            0xBE85_0380,                               // s_mov_b32 s5, 0
            0x3D << 26 | 0x02 << 18 | 8 << 6 | 4 >> 1, // s_load_dwordx4 s[8:11], s[4:5], 0
            125 << 25,
        ];
        if store {
            words.extend([0x38 << 26 | 0x1C << 18, 0x80 << 24 | 2 << 16]);
        } else {
            words.extend([0x38 << 26 | 0x0C << 18, 0x80 << 24 | 2 << 16 | 1 << 8]);
        }
        words.push(S_ENDPGM);
        NativeProgram {
            stage: HwStage::Cs,
            wave64: false,
            vgprs: 8,
            layout: vec![RootParam {
                root: RootType::Table,
                table_offset_dwords: 0,
                sgprs: 1,
                base_register: 0,
                user_data: 4,
                slot: 1,
            }],
            used: vec![UsedResource {
                kind: ResourceType::Uav,
                root: RootType::Table,
                table_offset_dwords: 0,
                lower_bound: 0,
                stride: 0,
                dwords: 4,
                user_data: 4,
                slot: 1,
            }],
            io: StageIo::Cs(ComputeIo {
                rsrc2: 5 << 1,
                threads: [64, 1, 1],
            }),
            code: words.into_iter().flat_map(u32::to_le_bytes).collect(),
        }
    }

    fn lowered(native: &NativeProgram, on: bool) -> Lowered {
        let p = ir::build(&native.code, native.wave64, native.vgprs).unwrap();
        let st = structure::structure(&p).unwrap();
        let res = resource::track(&p, &native.layout, native.stage, Some(&st)).unwrap();
        let io = io::raise(&p, native).unwrap();
        let was = TEST_ON.replace(on);
        let lowered = lower(&Parts {
            native,
            p: &p,
            st: &st,
            io: &io,
            res: &res,
            inputs: vec![],
            outputs: vec![],
            prims: vec![],
            mesh_stride: 0,
        });
        TEST_ON.set(was);
        lowered.unwrap_or_else(|e| panic!("{e}"))
    }

    fn asks_limit(l: &Lowered) -> bool {
        l.user_data
            .iter()
            .any(|u| u.root == RootType::Other(USER_THREAD_LIMIT))
    }

    #[test]
    fn only_stores_and_atomics_are_masked() {
        assert!(stores("buffer_store_dword"));
        assert!(stores("global_atomic_add"));
        assert!(stores("image_store"));
        assert!(stores("buffer_atomic_cmpswap"));
        assert!(!stores("buffer_load_dword"));
        assert!(!stores("image_sample_lz"));
    }

    /// On, a store asks for the thread limit and is made under it; off, or a
    /// program that only loads, translates as before.
    #[test]
    fn a_store_is_made_below_the_thread_limit() {
        let store = program(true);
        let off = lowered(&store, false);
        let on = lowered(&store, true);
        assert!(!asks_limit(&off));
        assert!(asks_limit(&on));
        assert_ne!(off.dxil, on.dxil);
        if let Ok(mut v) = crate::emit::validator::Validator::load(
            crate::emit::validator::Validator::default_path().as_deref(),
        ) {
            let mut copy = on.dxil.clone();
            v.validate(&mut copy).expect("DXIL validation failed");
        }
        let load = program(false);
        let (off, on) = (lowered(&load, false), lowered(&load, true));
        assert!(!asks_limit(&on));
        assert_eq!(off.dxil, on.dxil);
    }

    #[test]
    fn off_by_default() {
        assert!(!super::on());
    }
}
