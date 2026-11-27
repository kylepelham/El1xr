//! Three translation fixes to RDNA 2 semantics, each behind its own switch (off by default;
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

use std::sync::OnceLock;

#[derive(Clone, Copy, Debug)]
pub(crate) enum Fix {
    SccMinMax,
    CmpSamplerSplit,
    WritelaneMask,
}

impl Fix {
    fn name(self) -> &'static str {
        match self {
            Fix::SccMinMax => "VICTORY_SCC_MINMAX",
            Fix::CmpSamplerSplit => "VICTORY_CMP_SAMPLER_SPLIT",
            Fix::WritelaneMask => "VICTORY_WRITELANE_MASK",
        }
    }

    /// Whether the fix is asked for (`=1`). A test can choose for its own thread.
    pub(crate) fn on(self) -> bool {
        #[cfg(test)]
        if let Some(on) = TEST.with(|t| t.get()[self as usize]) {
            return on;
        }
        static ON: [OnceLock<bool>; 3] = [const { OnceLock::new() }; 3];
        *ON[self as usize].get_or_init(|| std::env::var(self.name()).is_ok_and(|v| v.trim() == "1"))
    }
}

#[cfg(test)]
thread_local! {
    /// A test's choices for [`Fix::on`], for its thread only.
    static TEST: std::cell::Cell<[Option<bool>; 3]> = const { std::cell::Cell::new([None; 3]) };
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
