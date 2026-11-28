//! Branches on a lane mask that the console takes with the whole wave (`VICTORY_LANE_MASK_WIDE`).
//!
//! `s_and_b64 s, vcc, exec` sets SCC when some active lane's bit is set, and `s_cbranch_scc0`
//! on it sends the whole wave one way. The structure takes such a branch per lane (each lane
//! by its own bit), which is right where each arm narrows EXEC to the lanes that went its way
//! (`s_and_saveexec_b64` and `s_cbranch_execz`), and wrong where an arm keeps EXEC: on the
//! console every active lane runs the arm the wave chose, split per lane those whose own bit
//! went the other way ran the other arm. cool_fox issue #66 measured it on a 32-lane host (RTX
//! 5080 and WARP): 188 of 576 words wrong; with the branch the wave's, all right.
//!
//! With the switch on, [`crate::structure`] keeps every such branch as the console has it (one
//! decision for the wave, EXEC a guard in both arms: `Lanes::lane_split_loses`). On a host of
//! 32-lane waves a console wave64 program then needs every lane where such a branch stands; a
//! program none of whose forms gives it that is translated again as before, split per lane
//! ([`again`]): inexact there, but drawn. `VICTORY_LANE_MASK_WIDE=exact` refuses it instead.
//! Unset or `0`: off (the structure as before). Part of the translation: the shader cache keys
//! on it.
//!
//! Ported from DoubleGhosty/cool_fox `34e99a33b4` (Hasusmogus, #66: `lane_split_loses`,
//! `structure_per_lane`), `d2f23973f6` and `a4af70be0f` (the per-lane retry, `VICTORY_LANE_SPLIT`,
//! in the cache key), with the store-guard test of `bd8b4f524a` (steven-seagull27), GPL-3.0.

use std::cell::Cell;

use crate::ShaderError;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    Off,
    /// Wave-wide where splitting loses, and the per-lane form for a program that refuses it.
    On,
    /// Wave-wide where splitting loses, and no per-lane retry.
    Exact,
}

fn parse(value: Option<&str>) -> Mode {
    match value.map(str::trim) {
        Some("1") => Mode::On,
        Some(v) if v.eq_ignore_ascii_case("exact") => Mode::Exact,
        _ => Mode::Off,
    }
}

fn mode() -> Mode {
    #[cfg(test)]
    if let Some(on) = TEST_LANE_MASK_WIDE.get() {
        return if on { Mode::On } else { Mode::Off };
    }
    static MODE: std::sync::OnceLock<Mode> = std::sync::OnceLock::new();
    *MODE.get_or_init(|| parse(std::env::var("VICTORY_LANE_MASK_WIDE").ok().as_deref()))
}

#[cfg(test)]
thread_local! {
    /// A test's choice for the switch (`On` or `Off`), for its thread only.
    pub(crate) static TEST_LANE_MASK_WIDE: Cell<Option<bool>> = const { Cell::new(None) };
}

thread_local! {
    /// Inside [`per_lane`]: the structure as with the switch off.
    static PER_LANE: Cell<bool> = const { Cell::new(false) };
}

/// Whether the structure keeps the branches splitting loses as the console has them.
pub(crate) fn on() -> bool {
    mode() != Mode::Off && !PER_LANE.get()
}

/// `f` with the structure as with the switch off, on this thread.
fn per_lane<T>(f: impl FnOnce() -> T) -> T {
    struct Reset(bool);
    impl Drop for Reset {
        fn drop(&mut self) {
            PER_LANE.set(self.0);
        }
    }
    let _reset = Reset(PER_LANE.replace(true));
    f()
}

/// What a translation gives: the program, and the wave size it requires of the host.
type Emitted<T> = Result<(T, Option<u32>), ShaderError>;

/// `first`, the translation whose structure took `wide` branches wave-wide (none: nothing this
/// switch changed); where it was refused, or needs 64-lane waves a host of `host_wave` lanes
/// does not have, the translation `retry` makes with every branch split per lane, as with the
/// switch off, if that one does better; for a refused one, that translation's result, error
/// and all.
pub(crate) fn again<T>(
    first: Emitted<T>,
    wide: usize,
    host_wave: u32,
    retry: impl FnOnce() -> Emitted<T>,
) -> Emitted<T> {
    let narrow = |w: &Option<u32>| *w == Some(64) && host_wave == 32;
    let refused = match &first {
        Ok((_, w)) => narrow(w),
        Err(e) => matches!(e, ShaderError::Unsupported { .. }),
    };
    if !refused || wide == 0 || mode() != Mode::On || PER_LANE.get() {
        return first;
    }
    let again = per_lane(retry);
    // (a refused program takes the per-lane result whatever it is, its error too: the caller's
    // next forms go on from the error the switch-off form gives)
    let better = match (&first, &again) {
        (Err(_), _) => true,
        (Ok(_), Err(_)) => false,
        (Ok(_), Ok((_, w))) => !narrow(w),
    };
    if std::env::var_os("VICTORY_WAVE_WHY").is_some() {
        let what = match &first {
            Ok((_, w)) => format!("wave {w:?}"),
            Err(e) => e.to_string(),
        };
        eprintln!(
            "LANE-MASK-WIDE {wide} branches wave-wide: {what}; split per lane {}",
            if better {
                "taken (inexact there)"
            } else {
                "no better"
            }
        );
    }
    if better { again } else { first }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_parse_to_a_mode() {
        assert_eq!(parse(None), Mode::Off);
        assert_eq!(parse(Some("0")), Mode::Off);
        assert_eq!(parse(Some(" 1 ")), Mode::On);
        assert_eq!(parse(Some("exact")), Mode::Exact);
    }

    #[test]
    fn a_refused_program_is_split_per_lane_once() {
        let refused = || -> Emitted<u8> {
            Err(ShaderError::Unsupported {
                offset: 0,
                what: "a wave-wide branch where not every lane runs",
            })
        };
        let was = TEST_LANE_MASK_WIDE.replace(Some(true));
        let r = again(refused(), 1, 32, || {
            assert!(!on(), "the retry's structure splits per lane");
            Ok((1, Some(32)))
        });
        assert!(matches!(r, Ok((1, Some(32)))));
        // the per-lane error replaces the wave-wide one
        let other = again(refused(), 1, 32, || -> Emitted<u8> {
            Err(ShaderError::Unsupported { offset: 4, what: "an opaque lane mask" })
        });
        assert!(matches!(other, Err(ShaderError::Unsupported { what: "an opaque lane mask", .. })));
        // nothing to split: no retry
        assert!(again(refused(), 0, 32, || unreachable!()).is_err());
        // a program that runs keeps its form
        assert!(matches!(
            again(Ok((2, Some(32))), 3, 32, || unreachable!()),
            Ok((2, _))
        ));
        // a retry still at 64 lanes is no better
        assert!(matches!(
            again(Ok((2, Some(64))), 3, 32, || Ok((1, Some(64)))),
            Ok((2, _))
        ));
        assert!(on());
        TEST_LANE_MASK_WIDE.set(Some(false));
        assert!(again(refused(), 1, 32, || unreachable!()).is_err());
        TEST_LANE_MASK_WIDE.set(was);
    }
}
