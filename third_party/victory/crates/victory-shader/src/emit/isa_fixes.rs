//! Translation fixes to RDNA 2 semantics, each behind its own switch (off by default;
//! every `VICTORY_*` name in this crate is part of the translation fingerprint, so the shader
//! cache keeps the two outputs apart):
//!
//! - `VICTORY_SCC_MINMAX=1`: `s_min_*`/`s_max_*` set SCC when the first operand is chosen
//!   (`D = S0 < S1 ? S0 : S1; SCC = S0 < S1`, the RDNA 2 ISA), not `D != 0`. Source:
//!   3cvil/M1LK `7f1f395cc2`, from Biskwit/VIctory-reloaded (TaynanGT/Vice-Tay `3c83a339`).
//! - `VICTORY_CMP_SAMPLER_SPLIT=1`: an immediate sampler constant used both with and without
//!   comparison takes a slot for each, instead of one comparison sampler for both. Source:
//!   3cvil/M1LK `339ff04049`, from Biskwit/VIctory-reloaded `26a801e2`.
//! - `VICTORY_WRITELANE_MASK=1`: `v_writelane_b32` takes its lane from S1 masked to the
//!   console wave (S1[4:0] in wave32, S1[5:0] in wave64), in every form, not only the paired
//!   one. Source: Belobug/VIctory-Belobug-AMD PR #650 (11run11), GPL-3.0.
//! - `VICTORY_DOT2_ACC_NEG=1`: `v_dot2_f32_f16` (VOP3P) negates its accumulator once for
//!   `neg_lo[2]`, not twice (`fsrc` already applies it). Source: uiliannn/VItoriaBr PR #26.
//! - `VICTORY_SCC_SHIFT_ADD=1`: `s_lshl{1-4}_add_u32` set SCC to the carry out of
//!   `(S0 << n) + S1` taken in 64 bits (the bits shifted out included), not `D != 0`. Source:
//!   uiliannn/VItoriaBr PR #88 = TaynanGT/Vice-Tay PR #345; the shifted-out bits as the
//!   reference interpreter (`rdna2_exec_bits.py` `lshl_add`) and the RDNA 3 pseudo-code.
//! - `VICTORY_TBUFFER_D16=1`: `tbuffer_load_format_d16_*` pack two 16-bit channels a VGPR
//!   (low half first; an odd last channel keeps the VGPR's high half), instead of a whole
//!   VGPR a channel over the registers after the destination. Source: Marvisak/VIctory
//!   `c430b1d` (Duff); the kept half as the reference interpreter (`rdna2_buffer_d16.py`).
//! - `VICTORY_NORM_NEAREST=1`: UNORM and SNORM buffer fields of up to 16 bits load as the
//!   float nearest their exact quotient (the reference interpreter's `ratio_bits`), built in
//!   integers, not by the host's division (which may be an ulp off). Source: Marvisak/VIctory
//!   `da82e44` (Marvis); a different construction (`isa_bundle.rs`).
//! - `VICTORY_COMPUTE_QUAD_LOD=1`: a compute program's implicit-level samples take their
//!   gradients from the console's lane quads, not level 0 (`quad_lod.rs`). Source:
//!   hello3928/Project-VI `653b1197`, `78bcfdc3` (Marvisak/VIctory PR #2999).
//! - `VICTORY_FIXED_TRIPS=1`: a loop whose trip count the code fixes (a constant counter, one
//!   step, one exit) runs to its end instead of being cut at the stage's loop limit
//!   (`trips.rs`). Source: DexNom/VIctory PR #137 (11run11).
//! - `VICTORY_WREXEC_SCC=1`: the SCC that the `s_andn2_wrexec` closing a waterfall loop writes
//!   is false after the loop (it is left only with no lane), instead of the program being
//!   refused for a lane mask read as data (`sop.rs`). Source: Marvisak/VIctory PR #3016.
//! - `VICTORY_U32_CLAMP=1`: unsigned VOP3 add/sub (`_nc`, `_co`, `_co_ci`, `v_add3_u32`) honour
//!   their clamp bit: the vector result saturates at 0 or `u32::MAX`, the carry out is the
//!   unclamped one (`valu.rs`); refused before. Source: LooperSalty/AnyXboxX `ca0c86b9d2`
//!   (Mauro Alvarez), from Marvisak/VIctory PR #1398.
//! - `VICTORY_MASK_MOVES=1`: a wave64 mask pair restored one dword at a time through `s_mov_b32`
//!   copies (as the spill pass writes a pair parked in VGPR lanes) is still the mask it was
//!   copied from, when both words come from that mask in their order (`structure.rs`). Source:
//!   jzvgrow-dot/Victory-4080Super `4eea7337d8`, from DexNom/VIctory `76749ce58d`.

use std::sync::OnceLock;

const COUNT: usize = 12;

#[derive(Clone, Copy, Debug)]
pub(crate) enum Fix {
    SccMinMax,
    CmpSamplerSplit,
    WritelaneMask,
    Dot2AccNeg,
    SccShiftAdd,
    TbufferD16,
    NormNearest,
    ComputeQuadLod,
    FixedTrips,
    WrexecScc,
    U32Clamp,
    MaskMoves,
}

impl Fix {
    fn name(self) -> &'static str {
        match self {
            Fix::SccMinMax => "VICTORY_SCC_MINMAX",
            Fix::CmpSamplerSplit => "VICTORY_CMP_SAMPLER_SPLIT",
            Fix::WritelaneMask => "VICTORY_WRITELANE_MASK",
            Fix::Dot2AccNeg => "VICTORY_DOT2_ACC_NEG",
            Fix::SccShiftAdd => "VICTORY_SCC_SHIFT_ADD",
            Fix::TbufferD16 => "VICTORY_TBUFFER_D16",
            Fix::NormNearest => "VICTORY_NORM_NEAREST",
            Fix::ComputeQuadLod => "VICTORY_COMPUTE_QUAD_LOD",
            Fix::FixedTrips => "VICTORY_FIXED_TRIPS",
            Fix::WrexecScc => "VICTORY_WREXEC_SCC",
            Fix::U32Clamp => "VICTORY_U32_CLAMP",
            Fix::MaskMoves => "VICTORY_MASK_MOVES",
        }
    }

    /// Whether the fix is asked for (`=1`). A test can choose for its own thread.
    pub(crate) fn on(self) -> bool {
        #[cfg(test)]
        if let Some(on) = TEST.with(|t| t.get()[self as usize]) {
            return on;
        }
        static ON: [OnceLock<bool>; COUNT] = [const { OnceLock::new() }; COUNT];
        *ON[self as usize].get_or_init(|| std::env::var(self.name()).is_ok_and(|v| v.trim() == "1"))
    }
}

#[cfg(test)]
thread_local! {
    /// A test's choices for [`Fix::on`], for its thread only.
    static TEST: std::cell::Cell<[Option<bool>; COUNT]> = const { std::cell::Cell::new([None; COUNT]) };
}

/// Sets `fix` for this thread's lowering (`None`: as the environment says).
#[cfg(test)]
pub(crate) fn set_for_test(fix: Fix, on: Option<bool>) {
    TEST.with(|t| {
        let mut all = t.get();
        all[fix as usize] = on;
        t.set(all);
    });
}
