//! (critpath, #88) The title's threads and ours on a processor of two dies.
//!
//! The title places its own threads (`SetThreadGroupAffinity`, `SetThreadPriority`,
//! `SetProcessPriorityBoost`, through `victory_hle`'s `placement` module). Every such call is
//! passed to the system unchanged and the first [`LOGGED`] are logged (`title thread placement:`),
//! so a log says which processors the title's threads are pinned to; `VICTORY_TITLE_WAITS=1`
//! adds a summary to its line ([`crate::title_waits`]).
//!
//! - `VICTORY_OWN_CPUS=auto|<first>-<last>` (Live, off by default): the process's default
//!   processor sets ([`SetProcessDefaultCpuSets`]) are the named processors; `auto`, those of every
//!   last-level cache but processor 0's, where there is more than one. The default holds for every
//!   thread with neither an affinity nor sets of its own: ours (the submitting thread, relay,
//!   wide helpers, texture and pipeline workers), the driver's, and not the title's pinned ones.
//!   The title pins its threads to the console's sixteen processors, here the first sixteen: one
//!   die of a Ryzen 9 9950X3D. Ours had no placement and ran on both dies, our helpers above
//!   normal priority (`VICTORY_HELPER_PRIORITY=1`) taking turns with the title's pinned recording
//!   threads, which cannot move elsewhere. Ported from KyleS98/VIctory-Neo `b50184c687`
//!   (`VICTORY_OWN_CPUS`; measured there on the same processor: the submitting thread had run
//!   48 % of its time on the title's die; prologue cell 37.1 to 38.7-39.1 fps).
//! - `VICTORY_TITLE_BOOST=1` (Live, off by default): the title's request to turn the process's
//!   priority boost off is not passed on (and is undone while the switch is on). A console title
//!   turns it off for its own fixed scheduling; on a desktop it also takes the wake-up boost from
//!   every thread of ours and every title thread woken by an event, which then waits for a
//!   running thread's quantum when its processors are busy.
//!
//! Not in the cache key: no shader or pipeline output.

// The title's own arguments, passed on as the system's functions take them.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::sync::Mutex;
use std::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering::Relaxed};

use victory_core::settings::Live;

static OWN: Live = Live::new("VICTORY_OWN_CPUS", parse_own);
static BOOST: Live = Live::flag("VICTORY_TITLE_BOOST");

/// How many of the title's placement calls are logged one by one.
const LOGGED: u32 = 96;
static CALLS: AtomicU32 = AtomicU32::new(0);

/// `VICTORY_OWN_CPUS` as bits: 0 none, 1 `auto`, else 2 | first << 8 | last << 16.
fn parse_own(v: Option<&str>) -> u64 {
    let Some(v) = v.map(str::trim).filter(|v| !v.is_empty()) else {
        return 0;
    };
    if v.eq_ignore_ascii_case("auto") {
        return 1;
    }
    v.split_once('-')
        .and_then(|(a, b)| Some((a.trim().parse::<u8>().ok()?, b.trim().parse::<u8>().ok()?)))
        .filter(|(a, b)| a <= b)
        .map_or(0, |(a, b)| 2 | u64::from(a) << 8 | u64::from(b) << 16)
}

/// The set ids `bits` ([`parse_own`]) names among `all`; `None`: none, or all of them.
fn own_of(bits: u64, all: &[crate::cores::CpuRecord]) -> Option<Vec<u32>> {
    let chosen: Vec<u32> = match bits {
        0 => return None,
        1 => {
            let first = all.iter().find(|c| c.logical == 0)?.cache;
            all.iter()
                .filter(|c| c.cache != first)
                .map(|c| c.id)
                .collect()
        }
        _ => {
            let (from, to) = ((bits >> 8) as u8, (bits >> 16) as u8);
            all.iter()
                .filter(|c| (from..=to).contains(&c.logical))
                .map(|c| c.id)
                .collect()
        }
    };
    (!chosen.is_empty() && chosen.len() < all.len()).then_some(chosen)
}

/// What [`settle`] last applied: `VICTORY_OWN_CPUS` bits, and the boost (0 untouched, 1 kept on).
static APPLIED_OWN: AtomicU64 = AtomicU64::new(0);
static APPLIED_BOOST: AtomicU8 = AtomicU8::new(0);
/// Whether process default sets of ours are in place.
static OWN_ACTIVE: AtomicU8 = AtomicU8::new(0);

/// Whether our process default sets are in place (`crate::cores` then leaves them alone).
pub(crate) fn own_active() -> bool {
    OWN_ACTIVE.load(Relaxed) != 0
}

/// At every submission (cheap: two live loads): applies a changed `VICTORY_OWN_CPUS` or
/// `VICTORY_TITLE_BOOST`.
pub(crate) fn settle() {
    let own = OWN.get();
    if APPLIED_OWN.load(Relaxed) != own {
        APPLIED_OWN.store(own, Relaxed);
        apply_own(own);
    }
    let boost = u8::from(BOOST.get_bool());
    if APPLIED_BOOST.load(Relaxed) != boost {
        APPLIED_BOOST.store(boost, Relaxed);
        apply_boost(boost != 0);
    }
}

fn apply_own(bits: u64) {
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, SetProcessDefaultCpuSets};
    let all = crate::cores::cpu_records();
    let own = own_of(bits, &all);
    // SAFETY: our own process and the system's own set ids (none: the default again).
    let ok = unsafe {
        match &own {
            Some(own) => {
                SetProcessDefaultCpuSets(GetCurrentProcess(), own.as_ptr(), own.len() as u32)
            }
            None => SetProcessDefaultCpuSets(GetCurrentProcess(), std::ptr::null(), 0),
        }
    } != 0;
    OWN_ACTIVE.store(u8::from(ok && own.is_some()), Relaxed);
    let logical: Vec<u8> = own
        .as_ref()
        .map(|own| {
            all.iter()
                .filter(|c| own.contains(&c.id))
                .map(|c| c.logical)
                .collect()
        })
        .unwrap_or_default();
    tracing::info!(
        asked = bits,
        processors = own.as_ref().map_or(0, Vec::len),
        of = all.len(),
        first = logical.iter().min().copied(),
        last = logical.iter().max().copied(),
        ok,
        "our own threads are kept on these processors (VICTORY_OWN_CPUS; 0 processors: any, as before)"
    );
}

/// The title's last word on the priority boost: 0 none, 1 off, 2 on.
static TITLE_BOOST_WISH: AtomicU8 = AtomicU8::new(0);

fn apply_boost(keep: bool) {
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, SetProcessPriorityBoost};
    let wish = TITLE_BOOST_WISH.load(Relaxed);
    // On: the boost back on. Off: the title's own word again, if it gave one.
    let disable = match (keep, wish) {
        (true, _) => Some(false),
        (false, 1) => Some(true),
        (false, 2) => Some(false),
        _ => None,
    };
    if let Some(disable) = disable {
        // SAFETY: our own process.
        let ok = unsafe { SetProcessPriorityBoost(GetCurrentProcess(), i32::from(disable)) } != 0;
        tracing::info!(
            keep,
            disable,
            ok,
            "the process's priority boost (VICTORY_TITLE_BOOST)"
        );
    }
}

/// The title's `SetProcessPriorityBoost`.
pub fn set_process_priority_boost(process: *mut core::ffi::c_void, disable: i32) -> i32 {
    use windows_sys::Win32::System::Threading::SetProcessPriorityBoost;
    TITLE_BOOST_WISH.store(if disable != 0 { 1 } else { 2 }, Relaxed);
    let kept = disable != 0 && BOOST.get_bool();
    let ok = if kept {
        1
    } else {
        // SAFETY: the title's own arguments.
        unsafe { SetProcessPriorityBoost(process, disable) }
    };
    note(format_args!(
        "SetProcessPriorityBoost disable={disable} passed_on={} ok={ok}",
        !kept
    ));
    ok
}

/// The title's `SetThreadPriority`.
pub fn set_thread_priority(thread: *mut core::ffi::c_void, priority: i32) -> i32 {
    use windows_sys::Win32::System::Threading::{GetThreadId, SetThreadPriority};
    // SAFETY: the title's own arguments.
    let ok = unsafe { SetThreadPriority(thread, priority) };
    // SAFETY: the title's handle; 0 if it is not a thread's.
    let id = unsafe { GetThreadId(thread) };
    note(format_args!(
        "SetThreadPriority thread={id} priority={priority} ok={ok}"
    ));
    ok
}

/// Each thread of the title's last affinity mask (group 0), by thread id.
static PINNED: Mutex<Vec<(u32, u64)>> = Mutex::new(Vec::new());

/// The title's `SetThreadGroupAffinity` (`affinity` a `GROUP_AFFINITY`).
pub fn set_thread_group_affinity(
    thread: *mut core::ffi::c_void,
    affinity: *const core::ffi::c_void,
    previous: *mut core::ffi::c_void,
) -> i32 {
    use windows_sys::Win32::System::SystemInformation::GROUP_AFFINITY;
    use windows_sys::Win32::System::Threading::{GetThreadId, SetThreadGroupAffinity};
    let affinity = affinity.cast::<GROUP_AFFINITY>();
    // SAFETY: the title's own arguments.
    let ok = unsafe { SetThreadGroupAffinity(thread, affinity, previous.cast()) };
    // SAFETY: the title's handle; 0 if it is not a thread's.
    let id = unsafe { GetThreadId(thread) };
    // SAFETY: the title passes a GROUP_AFFINITY (or none).
    let (group, mask) = unsafe { affinity.as_ref() }.map_or((0, 0), |a| (a.Group, a.Mask as u64));
    if ok != 0 && group == 0 {
        let mut pinned = PINNED.lock().unwrap_or_else(|e| e.into_inner());
        match pinned.iter_mut().find(|p| p.0 == id) {
            Some(p) => p.1 = mask,
            None => pinned.push((id, mask)),
        }
    }
    note(format_args!(
        "SetThreadGroupAffinity thread={id} group={group} mask={mask:#x} ok={ok}"
    ));
    ok
}

fn note(what: std::fmt::Arguments) {
    if CALLS.fetch_add(1, Relaxed) < LOGGED {
        tracing::info!("title thread placement: {what}");
    }
}

/// For [`crate::title_waits`]'s line: the title's pinned threads, how many of them only on the
/// processors of `inside` (a mask of logical processors), and all their masks together.
pub(crate) fn pinned_summary(inside: u64) -> (usize, usize, u64) {
    let pinned = PINNED.lock().unwrap_or_else(|e| e.into_inner());
    let within = pinned
        .iter()
        .filter(|p| p.1 != 0 && p.1 & !inside == 0)
        .count();
    let union = pinned.iter().fold(0, |u, p| u | p.1);
    (pinned.len(), within, union)
}

/// The title's boost wish, for the line: 0 none, 1 off, 2 on.
pub(crate) fn title_boost_wish() -> u8 {
    TITLE_BOOST_WISH.load(Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cores::CpuRecord;

    fn two_dies() -> Vec<CpuRecord> {
        (0..8u8)
            .map(|n| CpuRecord {
                id: 256 + u32::from(n),
                class: 0,
                core: n / 2,
                logical: n,
                cache: n / 4,
            })
            .collect()
    }

    #[test]
    fn our_own_processors_are_a_range_or_the_other_caches() {
        let all = two_dies();
        assert_eq!(
            own_of(parse_own(Some("4-7")), &all),
            Some(vec![260, 261, 262, 263])
        );
        assert_eq!(
            own_of(parse_own(Some(" 6 - 9 ")), &all),
            Some(vec![262, 263])
        );
        assert_eq!(
            own_of(parse_own(Some("AUTO")), &all),
            Some(vec![260, 261, 262, 263])
        );
        // All of them, none, nonsense, unset: nothing asked for.
        assert_eq!(own_of(parse_own(Some("0-7")), &all), None);
        assert_eq!(own_of(parse_own(Some("9-12")), &all), None);
        assert_eq!(own_of(parse_own(Some("7-4")), &all), None);
        assert_eq!(own_of(parse_own(Some("fast")), &all), None);
        assert_eq!(own_of(parse_own(None), &all), None);
        // One cache: `auto` names nothing.
        let one: Vec<CpuRecord> = all.iter().map(|c| CpuRecord { cache: 0, ..*c }).collect();
        assert_eq!(own_of(1, &one), None);
        assert!(!crate::cores::cpu_records().is_empty());
    }

    #[test]
    fn own_cpus_is_live_and_settles_back_to_the_default() {
        let read = || OWN.get();
        if !victory_core::settings::check_live("VICTORY_OWN_CPUS", "auto", read, 0, 1) {
            return;
        }
        // Unset again: settling leaves the default sets.
        settle();
        assert!(!own_active());
    }

    #[test]
    fn the_title_s_affinity_calls_pass_through_and_are_kept() {
        use windows_sys::Win32::System::SystemInformation::GROUP_AFFINITY;
        use windows_sys::Win32::System::Threading::{GetCurrentThread, GetCurrentThreadId};
        // SAFETY: no arguments.
        let id = unsafe { GetCurrentThreadId() };
        std::thread::spawn(move || {
            // SAFETY: a zeroed plain struct, filled in below.
            let mut previous: GROUP_AFFINITY = unsafe { std::mem::zeroed() };
            let wanted = GROUP_AFFINITY {
                Mask: 1,
                Group: 0,
                Reserved: [0; 3],
            };
            // SAFETY: no arguments.
            let me = unsafe { GetCurrentThread() };
            let ok = set_thread_group_affinity(
                me,
                (&raw const wanted).cast(),
                (&raw mut previous).cast(),
            );
            assert_ne!(ok, 0);
            assert_ne!(previous.Mask, 0);
            // SAFETY: no arguments.
            let tid = unsafe { GetCurrentThreadId() };
            assert!(PINNED.lock().unwrap().contains(&(tid, 1)));
            let (n, within, union) = pinned_summary(0b1);
            assert!(n >= 1 && within >= 1 && union & 1 == 1);
            assert_ne!(set_thread_priority(me, 0), 0);
        })
        .join()
        .unwrap();
        assert!(!PINNED.lock().unwrap().iter().any(|p| p.0 == id));
    }
}
