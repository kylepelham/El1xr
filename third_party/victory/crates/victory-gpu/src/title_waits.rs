//! (critpath, #88) `VICTORY_TITLE_WAITS=1` (Live, diagnostics, off by default): where the title's
//! threads spend their time inside our code that is not their own recording, and on which die.
//!
//! The card's clock had 9.6-14.3 ms a frame of "the title had not submitted" (parts O-T): the
//! title's recording threads, which run our binding and take our write faults, had not handed the
//! frame over. One line per frame-rate line:
//!
//! - write faults taken by our handler (nearly all on the title's threads: ours write title memory
//!   through views without guards): count, time in the handler, the longest, and how many took
//!   over 100 us (the kernel's dispatch of each fault, a few microseconds, is not in it);
//! - which die the faults, the binds (on the title's recording threads) and our submissions ran
//!   on: the share on processor 0's last-level cache, the one the title pins its threads to;
//! - the title's pinned threads (`crate::title_threads`): how many, how many on processor 0's
//!   die alone, all their masks together, and its word on the priority boost.
//!
//! Off, each site costs one live load. Not in the cache key.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::Instant;

static ON: victory_core::settings::Live = victory_core::settings::Live::flag("VICTORY_TITLE_WAITS");

pub(crate) fn on() -> bool {
    ON.get_bool()
}

#[derive(Clone, Copy)]
enum C {
    Faults,
    FaultUs,
    FaultMaxUs,
    FaultsSlow,
}
static COUNTS: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

/// Per logical processor (64 of group 0): faults, binds, submissions that ran there.
static FAULTS_AT: [AtomicU64; 64] = [const { AtomicU64::new(0) }; 64];
static BINDS_AT: [AtomicU64; 64] = [const { AtomicU64::new(0) }; 64];
static SUBMITS_AT: [AtomicU64; 64] = [const { AtomicU64::new(0) }; 64];

/// A fault over this many microseconds counts as slow.
const SLOW_US: u64 = 100;

fn processor() -> usize {
    // SAFETY: no arguments.
    (unsafe { windows_sys::Win32::System::Threading::GetCurrentProcessorNumber() } as usize) & 63
}

/// At the top of the write-fault handler: when it started, if counting.
#[inline]
pub(crate) fn fault_start() -> Option<Instant> {
    on().then(Instant::now)
}

/// At the end of a write fault our handler took (`began` from [`fault_start`]).
#[inline]
pub(crate) fn fault_end(began: Option<Instant>) {
    let Some(began) = began else {
        return;
    };
    let us = began.elapsed().as_micros() as u64;
    COUNTS[C::Faults as usize].fetch_add(1, Relaxed);
    COUNTS[C::FaultUs as usize].fetch_add(us, Relaxed);
    COUNTS[C::FaultMaxUs as usize].fetch_max(us, Relaxed);
    if us > SLOW_US {
        COUNTS[C::FaultsSlow as usize].fetch_add(1, Relaxed);
    }
    FAULTS_AT[processor()].fetch_add(1, Relaxed);
}

/// A draw or dispatch bound as its list is recorded (the title's recording threads).
#[inline]
pub(crate) fn bind() {
    if on() {
        BINDS_AT[processor()].fetch_add(1, Relaxed);
    }
}

/// A submission prepared (our submitting thread).
#[inline]
pub(crate) fn submission() {
    if on() {
        SUBMITS_AT[processor()].fetch_add(1, Relaxed);
    }
}

/// The logical processors on processor 0's last-level cache, as a mask (0: not known).
fn first_die() -> u64 {
    static MASK: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *MASK.get_or_init(|| {
        let all = crate::cores::cpu_records();
        let Some(first) = all.iter().find(|c| c.logical == 0).map(|c| c.cache) else {
            return 0;
        };
        all.iter()
            .filter(|c| c.cache == first && c.logical < 64)
            .fold(0, |m, c| m | 1 << c.logical)
    })
}

/// Takes one tally: (all, of them on the processors of `die`).
fn take(at: &[AtomicU64; 64], die: u64) -> (u64, u64) {
    at.iter().enumerate().fold((0, 0), |(all, inside), (p, n)| {
        let n = n.swap(0, Relaxed);
        (all + n, inside + if die >> p & 1 != 0 { n } else { 0 })
    })
}

fn share(t: (u64, u64)) -> f64 {
    if t.0 == 0 {
        0.0
    } else {
        100.0 * t.1 as f64 / t.0 as f64
    }
}

/// The line, per frame over `frames` frames (with the frame-rate line).
pub(crate) fn line(frames: f64) {
    if !on() || frames <= 0.0 {
        return;
    }
    let die = first_die();
    let [faults, fault_us, max_us, slow] = [C::Faults, C::FaultUs, C::FaultMaxUs, C::FaultsSlow]
        .map(|c| COUNTS[c as usize].swap(0, Relaxed));
    let (f, b, s) = (
        take(&FAULTS_AT, die),
        take(&BINDS_AT, die),
        take(&SUBMITS_AT, die),
    );
    let (pinned, within, union) = crate::title_threads::pinned_summary(die);
    tracing::info!(
        "title threads in our code per frame (VICTORY_TITLE_WAITS): write faults {:.1} in {:.2} ms \
         (longest {max_us} us, {:.1} over {SLOW_US} us), {:.0}% on processor 0's die; binds {:.0}, \
         {:.0}% there; our submissions {:.1}, {:.0}% there; the title's pinned threads {pinned}, \
         {within} on that die alone (die mask {die:#x}, their masks together {union:#x}), its \
         priority boost word {} (0 none, 1 off, 2 on); VICTORY_OWN_CPUS on {}",
        faults as f64 / frames,
        fault_us as f64 / 1000.0 / frames,
        slow as f64 / frames,
        share(f),
        b.0 as f64 / frames,
        share(b),
        s.0 as f64 / frames,
        share(s),
        crate::title_threads::title_boost_wish(),
        u8::from(crate::title_threads::own_active()),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tallies_split_by_die_and_empty_out() {
        let at: [AtomicU64; 64] = [const { AtomicU64::new(0) }; 64];
        at[0].store(3, Relaxed);
        at[1].store(1, Relaxed);
        at[20].store(4, Relaxed);
        assert_eq!(take(&at, 0b11), (8, 4));
        assert_eq!(take(&at, 0b11), (0, 0));
        assert_eq!(share((8, 4)), 50.0);
        assert_eq!(share((0, 0)), 0.0);
        assert!(processor() < 64);
        assert_ne!(first_die() & 1, 0, "processor 0 is on its own die");
    }

    #[test]
    fn faults_are_timed_only_when_on() {
        assert!(fault_start().is_none() || on());
        let before = COUNTS[C::Faults as usize].load(Relaxed);
        fault_end(None);
        assert_eq!(COUNTS[C::Faults as usize].load(Relaxed), before);
        let began = Some(Instant::now());
        fault_end(began);
        assert!(COUNTS[C::Faults as usize].load(Relaxed) > before);
        assert!(COUNTS[C::FaultMaxUs as usize].load(Relaxed) < 10_000_000);
    }
}
