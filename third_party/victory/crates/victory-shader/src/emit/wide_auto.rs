//! `VICTORY_WAVE_WIDE_AUTO=1` (off by default, in the shader cache key): a console wave64
//! compute program on a host of 32-lane waves whose forms without skips stop at an exchange
//! of the two host waves that only a deep-only skip keeps from every lane
//! ([`crate::ir::deep_skip_holds`]) tries the wave-wide form before "without skips, scalars
//! followed", which places that exchange after per-lane branching with no group barrier since.
//! The 18 cascade-shadow raster variants (`cascadeshadows_rendering:CS_ShadowRenderTiled*`)
//! drew an 8-pixel sun-shadow staircase and light through walls and hair in that form.
//!
//! The wave-wide form is refused where it would put a group barrier of the translator's own
//! into a loop ([`watch_loop_barriers`]): two host waves that leave such a loop on different
//! rounds wait there for threads that never come, which lost the card on
//! `ptfx_CS_lightAtlas_Sprite/_Trail`. The usual order then stands.
//!
//! Ported from Mikey970's PR #281 on trustno1-coder/VIctory-NVIDIA (`5e82577e`), GPL-3.0,
//! default off here (on there).

use super::Lower;
use crate::structure::{LaneExit, Structured};

/// `VICTORY_WAVE_WIDE_AUTO=1`; a test can choose for its own thread.
pub(in crate::emit) fn enabled() -> bool {
    #[cfg(test)]
    if let Some(on) = TEST_WIDE_AUTO.get() {
        return on;
    }
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("VICTORY_WAVE_WIDE_AUTO").is_ok_and(|v| v.trim() == "1"))
}

thread_local! {
    /// See [`watch_loop_barriers`]: `None` while nobody watches, then whether a barrier went
    /// into a loop.
    static LOOP_BARRIER: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
thread_local! {
    /// A test's choice for [`enabled`], for its thread only.
    pub(in crate::emit) static TEST_WIDE_AUTO: std::cell::Cell<Option<bool>> =
        const { std::cell::Cell::new(None) };
}

#[cfg(test)]
thread_local! {
    /// Whether the last program that tried the wave-wide form ahead on this thread took it.
    pub(in crate::emit) static TAKEN: std::cell::Cell<Option<bool>> =
        const { std::cell::Cell::new(None) };
}

/// Notes for a test whether a program took the wave-wide form ahead.
pub(in crate::emit) fn note_taken(taken: bool) {
    #[cfg(test)]
    TAKEN.set(Some(taken));
    #[cfg(not(test))]
    let _ = taken;
}

/// Runs `f` (lowerings) and says whether the last lowering in it put a group barrier of the
/// translator's own (a half-wave exchange, a split-LDS meeting) into a block of a loop.
/// [`reset_loop_barriers`] starts each lowering afresh.
pub(in crate::emit) fn watch_loop_barriers<R>(f: impl FnOnce() -> R) -> (R, bool) {
    let was = LOOP_BARRIER.replace(Some(false));
    let r = f();
    let looped = LOOP_BARRIER.replace(was).unwrap_or(false);
    (r, looped)
}

/// Inside [`watch_loop_barriers`]: forgets what an earlier lowering put where.
pub(in crate::emit) fn reset_loop_barriers() {
    if LOOP_BARRIER.get().is_some() {
        LOOP_BARRIER.set(Some(false));
    }
}

/// Whether block `b` lies on a cycle of `st`'s control flow.
fn in_cycle(st: &Structured, b: usize) -> bool {
    let n = st.exits.len();
    let succ = |x: usize| -> Vec<usize> {
        match st.exits[x] {
            LaneExit::Jump(t) => vec![t],
            LaneExit::Branch {
                taken, not_taken, ..
            } => vec![taken, not_taken],
            LaneExit::End | LaneExit::Indirect => Vec::new(),
        }
    };
    if b >= n {
        return false;
    }
    let mut seen = vec![false; n];
    let mut stack = succ(b);
    while let Some(x) = stack.pop() {
        if x == b {
            return true;
        }
        if x < n && !seen[x] {
            seen[x] = true;
            stack.extend(succ(x));
        }
    }
    false
}

impl Lower<'_> {
    /// For [`watch_loop_barriers`]: a group barrier of the translator's own goes into DXIL block
    /// `block`, which is the end of an earlier IR block (a ballot taken where its mask is
    /// written) or a block of the one being lowered.
    pub(super) fn note_barrier(&self, block: usize) {
        if LOOP_BARRIER.get() != Some(false) {
            return;
        }
        let ir = (0..self.tail.len())
            .find(|&d| d != self.cur_ir && self.tail[d] == Some(block))
            .unwrap_or(self.cur_ir);
        if in_cycle(self.x.st, ir) {
            LOOP_BARRIER.set(Some(true));
        }
    }
}
