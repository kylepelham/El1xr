//! CPU time per frame for the frame line (`VICTORY_FRAME_PHASES=1`): what
//! a thread ran between two of its own laps, and what the whole process
//! ran between two presents, in microseconds.
//!
//! A thread's time comes from `QueryThreadCycleTime`, counted on the
//! time-stamp counter, and turned into microseconds by the counter's rate
//! measured against the wall clock since the first lap.

use std::cell::Cell;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use windows_sys::Win32::Foundation::{FILETIME, HANDLE};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetCurrentThread, GetProcessTimes};

#[link(name = "kernel32")]
unsafe extern "system" {
    /// kernel32's (windows-sys keeps it behind a feature this crate does not use).
    fn QueryThreadCycleTime(thread: HANDLE, cycles: *mut u64) -> i32;
}

thread_local! {
    /// This thread's cycles at its last lap.
    static LAST: Cell<Option<u64>> = const { Cell::new(None) };
}

/// The wall clock and the time-stamp counter when first asked.
fn start() -> (Instant, u64) {
    static START: OnceLock<(Instant, u64)> = OnceLock::new();
    // SAFETY: reads the time-stamp counter.
    *START.get_or_init(|| (Instant::now(), unsafe { core::arch::x86_64::_rdtsc() }))
}

/// Time-stamp counter ticks per microsecond, once some time has passed.
fn ticks_per_us() -> Option<f64> {
    let (at, ticks) = start();
    let us = at.elapsed().as_micros() as f64;
    // SAFETY: reads the time-stamp counter.
    let now = unsafe { core::arch::x86_64::_rdtsc() };
    (us >= 10_000.0).then(|| now.wrapping_sub(ticks) as f64 / us)
}

fn thread_cycles() -> u64 {
    let mut cycles = 0u64;
    // SAFETY: the current-thread pseudo handle and a writable u64.
    unsafe { QueryThreadCycleTime(GetCurrentThread(), &mut cycles) };
    cycles
}

/// Microseconds this thread ran since its last lap (none at the first, or
/// before the counter's rate is known).
pub(crate) fn lap() -> Option<u64> {
    start();
    let now = thread_cycles();
    let last = LAST.with(|last| last.replace(Some(now)))?;
    Some((now.saturating_sub(last) as f64 / ticks_per_us()?) as u64)
}

/// Microseconds the process ran, kernel and user, since the last call.
pub(crate) fn process_lap() -> Option<u64> {
    static LAST: AtomicU64 = AtomicU64::new(0);
    // SAFETY: plain C structs, zero is a valid value for each.
    let [mut creation, mut exit, mut kernel, mut user] =
        unsafe { std::mem::zeroed::<[FILETIME; 4]>() };
    // SAFETY: the current-process pseudo handle and four writable FILETIMEs.
    let ok = unsafe {
        GetProcessTimes(
            GetCurrentProcess(),
            &mut creation,
            &mut exit,
            &mut kernel,
            &mut user,
        )
    };
    if ok == 0 {
        return None;
    }
    let ticks = |f: FILETIME| (u64::from(f.dwHighDateTime) << 32) | u64::from(f.dwLowDateTime);
    let now = (ticks(kernel) + ticks(user)) / 10;
    let last = LAST.swap(now, Ordering::Relaxed);
    (last != 0).then(|| now.saturating_sub(last))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// The bounds are those of a thread that has the core to itself. On a
    /// machine busy building, the spinning thread is raised above the
    /// builds (they run below normal) and the spin is measured again, up to
    /// five times, until one spin had its core; the sleeping thread's bound
    /// holds under any load.
    #[test]
    fn a_busy_thread_laps_its_wall_time_and_a_sleeping_one_little() {
        use windows_sys::Win32::System::Threading::{SetThreadPriority, THREAD_PRIORITY_HIGHEST};
        std::thread::spawn(|| {
            // SAFETY: the current-thread pseudo handle.
            unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST) };
            start();
            std::thread::sleep(Duration::from_millis(20));
            assert_eq!(lap(), None);
            let mut spins = Vec::new();
            for _ in 0..5 {
                let spin = Instant::now();
                while spin.elapsed() < Duration::from_millis(60) {
                    std::hint::spin_loop();
                }
                let busy = lap().unwrap();
                spins.push(busy);
                if (40_000..=90_000).contains(&busy) {
                    break;
                }
            }
            assert!(
                spins.iter().any(|busy| (40_000..=90_000).contains(busy)),
                "busy {spins:?} us"
            );
            std::thread::sleep(Duration::from_millis(60));
            let idle = lap().unwrap();
            assert!(idle < 10_000, "idle {idle} us");
        })
        .join()
        .unwrap();
    }
}
