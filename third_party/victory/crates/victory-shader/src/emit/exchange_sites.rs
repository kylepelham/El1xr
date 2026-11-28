//! `VICTORY_EXCHANGE_SITES=1` (off by default, in the shader cache key): each static place where
//! the two host waves of a console wave64 exchange through LDS (a lane mask's ballot, a
//! reduction's halves, a lane read across the halves) gets dwords of its own, an LDS array
//! `wave_exchange_<n>` laid out as the program's shared exchange dwords are (per console wave,
//! the turns' sets and the four-dword form included). With one set of dwords for every place, a
//! later place's store overwrites an earlier one's value when the meetings around it do not
//! pair up as the code says (a host GPU that counts barrier arrivals, two host waves standing
//! at different barriers): an RTX 5090 drew black 16x16 tiles outdoors, the tile classes of
//! `2f03a2df10503cef` (`CS_DepthProcessingHBO`) making `9991e29feaa26e3e`
//! (`CS_ShadowTileClassify`) skip tiles. The meetings stay as they are; a place that no longer
//! fits in the host's 32 KiB uses the shared dwords as before.
//!
//! Ported from Barbital11111's PR #246 on trustno1-coder/VIctory-NVIDIA ("each static
//! cross-half exchange site its own LDS dwords"), GPL-3.0.

use super::{Bin, Lower, V};

/// LDS dwords a host thread group has (D3D12: 32 KiB).
const HOST_LDS_DWORDS: u32 = 8192;

/// The prefix of the arrays' names; `bitcode::opt` drops such an array no code reads.
pub(crate) const PREFIX: &str = "wave_exchange_";

/// `VICTORY_EXCHANGE_SITES=1`; a test can choose for its own thread.
fn enabled() -> bool {
    #[cfg(test)]
    if let Some(on) = TEST_SITES.get() {
        return on;
    }
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("VICTORY_EXCHANGE_SITES").is_ok_and(|v| v.trim() == "1"))
}

#[cfg(test)]
thread_local! {
    /// A test's choice for [`enabled`], for its thread only.
    pub(super) static TEST_SITES: std::cell::Cell<Option<bool>> =
        const { std::cell::Cell::new(None) };
    /// Arrays made on this thread since a test set it.
    pub(super) static MADE: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

impl Lower<'_> {
    /// For an exchange whose dwords in the shared exchange region start at `k` (the console
    /// wave's first, with the turn's set added) and whose region starts at `slot`: an array of
    /// its own and `k`'s place in it, both made in block `at`. None with the switch off or
    /// where the array does not fit: the exchange then uses the shared dwords.
    pub(super) fn exchange_site(&mut self, at: usize, slot: u32, k: V) -> Option<(V, V)> {
        if !enabled() {
            return None;
        }
        let per_wave = 1u32 << (u32::from(self.exchange_quad) + 1 + u32::from(self.xpairs.on));
        let threads = self.s.threads.map_or(64, |t| t.iter().product::<u32>());
        let words = threads.div_ceil(64).max(1) * per_wave;
        if self.s.lds_dwords + self.xsites + words > HOST_LDS_DWORDS {
            return None;
        }
        let ty = self.s.m.array(words, self.s.t.i32);
        let g = self.s.m.global(&format!("{PREFIX}{}", self.xsites), ty, 3);
        self.xsites += words;
        #[cfg(test)]
        MADE.set(MADE.get() + 1);
        let base = self.s.m.i32(slot);
        let index = self.s.m.bin(at, Bin::Sub, k, base);
        Some((g, index))
    }
}

#[cfg(test)]
mod tests {
    use super::{MADE, TEST_SITES};
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

    /// Three counts in a row (three exchanges), s16 | s17 << 8 | s18 << 16 to every thread's
    /// dword; 128 threads, two console waves.
    fn three_counts() -> NativeProgram {
        let words = [
            below(0),
            count(16),
            below(1),
            count(17),
            below(2),
            count(18),
            0x8F11_8811, // s_lshl_b32 s17, s17, 8
            0x8F12_9012, // s_lshl_b32 s18, s18, 16
            0x8810_1110, // s_or_b32 s16, s16, s17
            0x8810_1210, // s_or_b32 s16, s16, s18
            0x7E02_0210, // v_mov_b32 v1, s16
            0x3404_0082, // v_lshlrev_b32 v2, 2, v0
            0x38 << 26 | 0x1C << 18 | 1 << 12,
            0x80 << 24 | 2 << 16 | 1 << 8 | 2, // buffer_store_dword v1, v2, s[8:11], 0 offen
        ];
        native_of(&words, true, 128)
    }

    /// Two lane reads (lanes 5 and 40, the second across the halves) and a wave-wide OR of the
    /// thread index, then [lane 5's, lane 40's, the OR, the thread] at the thread's 16 bytes;
    /// 64 threads, one console wave.
    fn reads_and_or() -> NativeProgram {
        let mut words = vec![
            0xD760_000E,
            0x0001_0B00, // v_readlane_b32 s14, v0, 5
            0xD760_000F,
            0x0001_5100, // v_readlane_b32 s15, v0, 40
            0x7E0E_0300, // v_mov_b32 v7, v0
        ];
        words.extend_from_slice(&crate::emit::wave::WAVE_OR_V7_TO_S7);
        words.extend([
            0x7E04_020E, // v_mov_b32 v2, s14
            0x7E06_020F, // v_mov_b32 v3, s15
            0x7E08_0207, // v_mov_b32 v4, s7
            0x341E_0084, // v_lshlrev_b32 v15, 4, v0
        ]);
        for (i, reg) in [2u32, 3, 4, 0].into_iter().enumerate() {
            // buffer_store_dword v<reg>, v15, s[8:11], 0 offen offset:4i
            words.extend([0xE070_1000 | (4 * i as u32), 0x8002_000F | (reg << 8)]);
        }
        native_of(&words, true, 64)
    }

    /// The program for a host of 32-lane waves with the sites on or off, its group barriers and
    /// the arrays it was given.
    fn lowered(native: &NativeProgram, on: bool) -> (HostShader, u32, u32) {
        let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
        let st = structure::structure(&p).expect("structure");
        let res =
            resource::track(&p, &native.layout, native.stage, Some(&st)).expect("resource::track");
        let io = io::raise(&p, native).expect("io::raise");
        let was = super::super::TEST_HOST_WAVE.replace(Some(32));
        let sites = TEST_SITES.replace(Some(on));
        MADE.set(0);
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
        TEST_SITES.set(sites);
        super::super::TEST_HOST_WAVE.set(was);
        let made = MADE.get();
        let l = lowered.expect("lower");
        if let Ok(mut v) =
            validator::Validator::load(validator::Validator::default_path().as_deref())
        {
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
        (shader, barriers, made)
    }

    /// One group with root constants s0..s3: the first `dwords` dwords of the UAV. On WARP
    /// where it takes the program, else on the hardware device; None with neither.
    fn run(shader: &HostShader, constants: [u32; 4], dwords: u32) -> Option<Vec<u32>> {
        let warp = Host::new(true)
            .ok()
            .and_then(|h| h.compute_pipeline(shader).ok().map(|p| (h, p)));
        let card = || {
            Host::new(false)
                .ok()
                .and_then(|h| h.compute_pipeline(shader).ok().map(|p| (h, p)))
        };
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
                    views: &[View::RawUav(dwords)],
                    samplers: &[],
                    groups: [1, 1, 1],
                },
            )
            .unwrap_or_else(|e| panic!("dispatch failed: {e}"));
        let mut words = out.swap_remove(0);
        words.truncate(dwords as usize);
        Some(words)
    }

    #[test]
    fn three_counts_get_three_sites_with_the_same_meetings_and_results() {
        let native = three_counts();
        let (off, off_barriers, off_made) = lowered(&native, false);
        let (on, on_barriers, on_made) = lowered(&native, true);
        assert_eq!(off_made, 0, "off: the shared dwords");
        assert_eq!(on_made, 3, "on: one array per exchange");
        assert_eq!(off_barriers, on_barriers, "the meetings stay");
        assert_ne!(off.dxil, on.dxil);
        assert_eq!(
            lowered(&native, false).0.dxil,
            off.dxil,
            "off translates alike each time"
        );
        // with the turns (`VICTORY_EXCHANGE_PAIRS=1`): each array holds both sets
        let turns = super::super::exchange_pairs::TEST_PAIRS.replace(Some(true));
        let (both, both_barriers, both_made) = lowered(&native, true);
        super::super::exchange_pairs::TEST_PAIRS.set(turns);
        assert_eq!(both_made, 3);
        assert_eq!(both_barriers + 3, on_barriers, "the turns still save a meeting each");
        let _one = crate::emit::valu_tests::ONE_AT_A_TIME
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let lanes = |x: u32, w: u32| x.saturating_sub(64 * w).min(64);
        for s in [[128, 128, 128], [40, 100, 70], [0, 64, 127], [33, 1, 96]] {
            let want: Vec<u32> = (0..128)
                .map(|t| {
                    let w = t / 64;
                    lanes(s[0], w) | lanes(s[1], w) << 8 | lanes(s[2], w) << 16
                })
                .collect();
            for (name, shader) in [("off", &off), ("on", &on), ("on with turns", &both)] {
                let Some(got) = run(shader, [s[0], s[1], s[2], 0], 128) else {
                    return;
                };
                assert_eq!(got, want, "{name}, threads below {s:?}");
            }
        }
    }

    #[test]
    fn lane_reads_and_a_reduction_keep_their_values_in_sites_of_their_own() {
        let native = reads_and_or();
        let (off, off_barriers, _) = lowered(&native, false);
        let (on, on_barriers, on_made) = lowered(&native, true);
        assert!(on_made >= 2, "the lane reads at least: {on_made}");
        assert_eq!(off_barriers, on_barriers, "the meetings stay");
        let want: Vec<u32> = (0..64).flat_map(|t| [5, 40, 63, t]).collect();
        let _one = crate::emit::valu_tests::ONE_AT_A_TIME
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for (name, shader) in [("off", &off), ("on", &on)] {
            let Some(got) = run(shader, [0; 4], 256) else {
                return;
            };
            assert_eq!(got, want, "{name}");
        }
    }
}
