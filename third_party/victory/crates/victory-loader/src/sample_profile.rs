//! Where the process spends its CPU: `VICTORY_SAMPLE_PROFILE=<ms>` samples
//! every thread's stack every that many milliseconds for a window and writes
//! the counts to a text file that `tools/dev.py profile` names and reports.
//!
//! `VICTORY_SAMPLE_PROFILE_START_S` (default 150) is when the window opens,
//! in seconds from [`start`]; `VICTORY_SAMPLE_PROFILE_SECONDS` (default 60)
//! how long it lasts; `VICTORY_SAMPLE_PROFILE_OUT` the file (default
//! `victory_profile.txt` in the working folder).
//!
//! At each tick a thread whose cycle count has barely moved since its last
//! sample ([`RAN_CYCLES`]) did not run, so its stack is the one sampled last (counted with
//! `ran` 0, no suspend). A thread that ran is suspended, its context read and
//! unwound into a buffer allocated beforehand (it may hold the heap lock),
//! and resumed. A watchdog resumes a thread left suspended over
//! [`STUCK_MS`] (the unwinder waiting on a lock the thread holds) and counts
//! it. The file holds raw return addresses and the module list; names come
//! offline from dbghelp against the PDBs.

use std::collections::HashMap;
use std::ffi::c_void;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicPtr, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use windows_sys::Wdk::System::Threading::{
    NtQueryInformationThread, ThreadBasicInformation, ThreadQuerySetWin32StartAddress,
};
use windows_sys::Win32::Foundation::{
    CloseHandle, FILETIME, HANDLE, INVALID_HANDLE_VALUE, LocalFree,
};
use windows_sys::Win32::System::Diagnostics::Debug::{
    CONTEXT, CONTEXT_FULL_AMD64, GetThreadContext,
};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, MODULEENTRY32W, Module32FirstW, Module32NextW, TH32CS_SNAPMODULE,
    TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcessId, GetCurrentThreadId, GetExitCodeThread, GetThreadDescription,
    GetThreadTimes, OpenThread, ResumeThread, SuspendThread, THREAD_GET_CONTEXT,
    THREAD_QUERY_INFORMATION, THREAD_SUSPEND_RESUME,
};
use windows_sys::Win32::System::WindowsProgramming::QueryThreadCycleTime;

use crate::crash::{MAX_FRAMES, locate, unwind_checks_enabled, unwind_into};

/// A thread left suspended this long is resumed by the watchdog.
const STUCK_MS: u64 = 200;
/// Cycles since the last tick past which a thread counts as having run:
/// above what our own suspend and resume cost a waiting thread (its kernel
/// wakes it to take the suspend), about 10 us.
const RAN_CYCLES: u64 = 50_000;
/// How often the thread list is read again.
const REFRESH: Duration = Duration::from_millis(500);
/// `GetExitCodeThread` of a live thread.
const STILL_ACTIVE: u32 = 259;

/// What `VICTORY_SAMPLE_PROFILE*` ask for.
#[derive(Debug, PartialEq)]
pub(crate) struct Config {
    pub(crate) interval: Duration,
    pub(crate) start: Duration,
    pub(crate) window: Duration,
    pub(crate) out: PathBuf,
}

impl Config {
    /// `None` unless `VICTORY_SAMPLE_PROFILE` is a positive number of milliseconds.
    pub(crate) fn parse(get: impl Fn(&str) -> Option<String>) -> Option<Config> {
        let ms: f64 = get("VICTORY_SAMPLE_PROFILE")?.trim().parse().ok()?;
        if !(ms > 0.0 && ms.is_finite()) {
            return None;
        }
        let secs = |name: &str, default: f64| {
            get(name)
                .and_then(|v| v.trim().parse::<f64>().ok())
                .filter(|s| s.is_finite() && *s >= 0.0)
                .unwrap_or(default)
        };
        Some(Config {
            interval: Duration::from_secs_f64(ms / 1e3),
            start: Duration::from_secs_f64(secs("VICTORY_SAMPLE_PROFILE_START_S", 150.0)),
            window: Duration::from_secs_f64(secs("VICTORY_SAMPLE_PROFILE_SECONDS", 60.0)),
            out: get("VICTORY_SAMPLE_PROFILE_OUT")
                .filter(|p| !p.is_empty())
                .map_or_else(|| PathBuf::from("victory_profile.txt"), PathBuf::from),
        })
    }
}

/// Starts the sampler if `VICTORY_SAMPLE_PROFILE` asks for it.
pub fn start() {
    let Some(config) = Config::parse(|name| std::env::var(name).ok()) else {
        return;
    };
    println!(
        "sample profile: every {:?} from {:.0} s for {:.0} s into {} (VICTORY_SAMPLE_PROFILE)",
        config.interval,
        config.start.as_secs_f64(),
        config.window.as_secs_f64(),
        config.out.display()
    );
    let started = std::thread::Builder::new()
        .name("victory-sample-profile".into())
        .spawn(move || {
            std::thread::sleep(config.start);
            let profile = sample(&config);
            match write(&config, &profile) {
                Ok(()) => println!(
                    "sample profile: {} samples of {} threads ({} stacks) in {:.1} s to {}; \
                     suspended {:.1} us mean, {} us most; {} resumed by the watchdog",
                    profile.samples.values().sum::<u64>(),
                    profile.threads.len(),
                    profile.stacks.len(),
                    profile.window_s,
                    config.out.display(),
                    profile.suspended_us_mean(),
                    profile.suspended_us_max,
                    profile.stuck,
                ),
                Err(e) => println!("sample profile: cannot write {}: {e}", config.out.display()),
            }
        });
    if let Err(e) = started {
        println!("sample profile: no thread to sample with: {e}");
    }
}

/// One thread's samples so far.
struct Watched {
    handle: HANDLE,
    teb: *const usize,
    cycles: u64,
    last: Option<u32>,
    cpu_start: u64,
    alive: bool,
}

/// What a window gathered.
#[derive(Default)]
pub(crate) struct Profile {
    pub(crate) interval_ms: f64,
    pub(crate) window_s: f64,
    pub(crate) ticks: u64,
    pub(crate) stacks: Vec<Vec<u64>>,
    /// (thread, stack, ran) -> samples.
    pub(crate) samples: HashMap<(u32, u32, bool), u64>,
    /// (thread, cpu ms in the window, start address, name).
    pub(crate) threads: Vec<(u32, f64, String, String)>,
    pub(crate) modules: Vec<(u64, u64, String)>,
    pub(crate) suspended_us_total: u64,
    pub(crate) suspended_count: u64,
    pub(crate) suspended_us_max: u64,
    pub(crate) stuck: u64,
}

impl Profile {
    fn suspended_us_mean(&self) -> f64 {
        self.suspended_us_total as f64 / self.suspended_count.max(1) as f64
    }
}

/// The thread the sampler holds suspended now (null: none), and since when
/// (microseconds of [`EPOCH`]).
static SUSPENDED: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
static SUSPENDED_SINCE_US: AtomicU64 = AtomicU64::new(0);
static STUCK: AtomicUsize = AtomicUsize::new(0);

fn epoch_us() -> u64 {
    static EPOCH: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    EPOCH.get_or_init(Instant::now).elapsed().as_micros() as u64
}

/// Resumes the thread held in [`SUSPENDED`] if it is still there: exactly
/// one of the sampler and the watchdog takes it.
fn resume_held() -> bool {
    let held = SUSPENDED.swap(std::ptr::null_mut(), Ordering::AcqRel);
    if held.is_null() {
        return false;
    }
    // SAFETY: the sampler suspended this open thread once.
    unsafe { ResumeThread(held) };
    true
}

fn watchdog(done: &std::sync::atomic::AtomicBool) {
    while !done.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(STUCK_MS / 4));
        let since = SUSPENDED_SINCE_US.load(Ordering::Acquire);
        if !SUSPENDED.load(Ordering::Acquire).is_null()
            && epoch_us().saturating_sub(since) > STUCK_MS * 1000
            && SUSPENDED_SINCE_US.load(Ordering::Acquire) == since
            && resume_held()
        {
            STUCK.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// `GetThreadContext` wants a 16-byte aligned context.
#[repr(C, align(16))]
struct Aligned(CONTEXT);

/// `THREAD_BASIC_INFORMATION` (ntddk).
#[repr(C)]
struct BasicInfo {
    exit_status: i32,
    teb: *const usize,
    client_id: [usize; 2],
    affinity: usize,
    priority: i32,
    base_priority: i32,
}

fn teb_of(thread: HANDLE) -> *const usize {
    let mut info = BasicInfo {
        exit_status: 0,
        teb: std::ptr::null(),
        client_id: [0; 2],
        affinity: 0,
        priority: 0,
        base_priority: 0,
    };
    // SAFETY: writes `info`, whose layout and size are the system's.
    let status = unsafe {
        NtQueryInformationThread(
            thread,
            ThreadBasicInformation,
            (&raw mut info).cast(),
            size_of::<BasicInfo>() as u32,
            std::ptr::null_mut(),
        )
    };
    if status != 0 {
        std::ptr::null()
    } else {
        info.teb
    }
}

/// Kernel plus user time of `thread`, in 100 ns units.
fn cpu_of(thread: HANDLE) -> u64 {
    let zero = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
    // SAFETY: writes four FILETIMEs of an open thread.
    if unsafe { GetThreadTimes(thread, &mut created, &mut exited, &mut kernel, &mut user) } == 0 {
        return 0;
    }
    let t = |f: FILETIME| (u64::from(f.dwHighDateTime) << 32) | u64::from(f.dwLowDateTime);
    t(kernel) + t(user)
}

fn cycles_of(thread: HANDLE) -> Option<u64> {
    let mut cycles = 0u64;
    // SAFETY: writes one counter of an open thread.
    (unsafe { QueryThreadCycleTime(thread, &mut cycles) } != 0).then_some(cycles)
}

fn alive(thread: HANDLE) -> bool {
    let mut code = 0u32;
    // SAFETY: writes one exit code of an open thread.
    unsafe { GetExitCodeThread(thread, &mut code) != 0 && code == STILL_ACTIVE }
}

/// This process's threads but `skip`.
fn thread_ids(skip: &[u32]) -> Vec<u32> {
    // SAFETY: plain queries and a snapshot of the system's threads.
    let (process, snapshot) = unsafe {
        (
            GetCurrentProcessId(),
            CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0),
        )
    };
    let mut ids = Vec::new();
    if snapshot == INVALID_HANDLE_VALUE {
        return ids;
    }
    // SAFETY: zeroed is a valid THREADENTRY32; its size is set before use.
    let mut entry: THREADENTRY32 = unsafe { std::mem::zeroed() };
    entry.dwSize = size_of::<THREADENTRY32>() as u32;
    // SAFETY: `snapshot` is open; `entry` is sized.
    let mut more = unsafe { Thread32First(snapshot, &mut entry) } != 0;
    while more {
        if entry.th32OwnerProcessID == process && !skip.contains(&entry.th32ThreadID) {
            ids.push(entry.th32ThreadID);
        }
        // SAFETY: as above.
        more = unsafe { Thread32Next(snapshot, &mut entry) } != 0;
    }
    // SAFETY: we opened it.
    unsafe { CloseHandle(snapshot) };
    ids
}

fn modules() -> Vec<(u64, u64, String)> {
    // SAFETY: a snapshot of this process's modules.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPMODULE, 0) };
    let mut out = Vec::new();
    if snapshot == INVALID_HANDLE_VALUE {
        return out;
    }
    let mut entry = MODULEENTRY32W {
        dwSize: size_of::<MODULEENTRY32W>() as u32,
        ..Default::default()
    };
    // SAFETY: `snapshot` is open; `entry` is sized.
    let mut more = unsafe { Module32FirstW(snapshot, &mut entry) } != 0;
    while more {
        let len = entry
            .szExePath
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(entry.szExePath.len());
        out.push((
            entry.modBaseAddr as u64,
            u64::from(entry.modBaseSize),
            String::from_utf16_lossy(&entry.szExePath[..len]),
        ));
        // SAFETY: as above.
        more = unsafe { Module32NextW(snapshot, &mut entry) } != 0;
    }
    // SAFETY: we opened it.
    unsafe { CloseHandle(snapshot) };
    out
}

fn name_of(thread: HANDLE) -> String {
    let mut name = std::ptr::null_mut();
    // SAFETY: the system allocates the string; we free it with LocalFree.
    unsafe {
        if GetThreadDescription(thread, &mut name) < 0 || name.is_null() {
            return String::new();
        }
        let len = (0..).take_while(|&i| *name.add(i) != 0).count();
        let text = String::from_utf16_lossy(std::slice::from_raw_parts(name, len));
        LocalFree(name.cast::<c_void>());
        text
    }
}

fn start_of(thread: HANDLE) -> usize {
    let mut start = 0usize;
    // SAFETY: writes one pointer-sized start address.
    unsafe {
        NtQueryInformationThread(
            thread,
            ThreadQuerySetWin32StartAddress,
            (&raw mut start).cast(),
            size_of::<usize>() as u32,
            std::ptr::null_mut(),
        )
    };
    start
}

/// Suspends `thread`, unwinds it into `frames` (capacity [`MAX_FRAMES`],
/// so nothing allocates while it is held) and resumes it; the microseconds
/// it was held, or `None` if it could not be read.
fn unwind_thread(
    thread: HANDLE,
    teb: *const usize,
    frames: &mut Vec<u64>,
    checks: bool,
) -> Option<u64> {
    frames.clear();
    // SAFETY: zeroed is a valid CONTEXT.
    let mut context = Aligned(unsafe { std::mem::zeroed() });
    context.0.ContextFlags = CONTEXT_FULL_AMD64;
    let since = epoch_us();
    SUSPENDED_SINCE_US.store(since, Ordering::Release);
    // SAFETY: suspends and reads a thread of this process that is not this
    // one; between suspend and resume nothing allocates or takes a lock the
    // thread could hold besides the unwinder's own (the watchdog's case).
    unsafe {
        if SuspendThread(thread) == u32::MAX {
            return None;
        }
        SUSPENDED.store(thread, Ordering::Release);
        if GetThreadContext(thread, &mut context.0) != 0 {
            // The TEB's NT_TIB: stack base at +8, current limit at +0x10.
            let (high, low) = (teb.add(1).read_volatile(), teb.add(2).read_volatile());
            unwind_into(context.0, low, high, frames, checks);
        }
        resume_held();
    }
    Some(epoch_us().saturating_sub(since))
}

fn sample(config: &Config) -> Profile {
    let done = std::sync::atomic::AtomicBool::new(false);
    let checks = unwind_checks_enabled();
    // SAFETY: plain query.
    let me = unsafe { GetCurrentThreadId() };
    let mut profile = Profile {
        interval_ms: config.interval.as_secs_f64() * 1e3,
        ..Profile::default()
    };
    std::thread::scope(|scope| {
        let (dog_tx, dog_rx) = std::sync::mpsc::channel();
        let done = &done;
        scope.spawn(move || {
            // SAFETY: plain query.
            dog_tx.send(unsafe { GetCurrentThreadId() }).ok();
            watchdog(done);
        });
        let skip = [me, dog_rx.recv().unwrap_or(0)];
        let mut watched: HashMap<u32, Watched> = HashMap::new();
        let mut interned: HashMap<Vec<u64>, u32> = HashMap::new();
        let mut frames: Vec<u64> = Vec::with_capacity(MAX_FRAMES);
        let began = Instant::now();
        let mut next_refresh = began;
        let mut tick_at = began;
        while began.elapsed() < config.window {
            if Instant::now() >= next_refresh {
                next_refresh += REFRESH;
                for w in watched.values_mut().filter(|w| w.alive) {
                    w.alive = alive(w.handle);
                }
                for id in thread_ids(&skip) {
                    if watched.contains_key(&id) {
                        continue;
                    }
                    // SAFETY: opens a thread of our own process.
                    let handle = unsafe {
                        OpenThread(
                            THREAD_SUSPEND_RESUME | THREAD_GET_CONTEXT | THREAD_QUERY_INFORMATION,
                            0,
                            id,
                        )
                    };
                    if handle.is_null() {
                        continue;
                    }
                    let teb = teb_of(handle);
                    watched.insert(
                        id,
                        Watched {
                            handle,
                            teb,
                            cycles: cycles_of(handle).unwrap_or(0),
                            last: None,
                            cpu_start: cpu_of(handle),
                            alive: !teb.is_null(),
                        },
                    );
                }
            }
            profile.ticks += 1;
            for (&id, w) in watched.iter_mut().filter(|(_, w)| w.alive) {
                let Some(cycles) = cycles_of(w.handle) else {
                    continue;
                };
                let ran = cycles.saturating_sub(w.cycles) > RAN_CYCLES;
                w.cycles = cycles;
                let stack = match w.last {
                    Some(last) if !ran => last,
                    _ => {
                        let Some(us) = unwind_thread(w.handle, w.teb, &mut frames, checks) else {
                            continue;
                        };
                        profile.suspended_us_total += us;
                        profile.suspended_count += 1;
                        profile.suspended_us_max = profile.suspended_us_max.max(us);
                        let next = profile.stacks.len() as u32;
                        let at = *interned.entry(frames.clone()).or_insert(next);
                        if at == next {
                            profile.stacks.push(frames.clone());
                        }
                        w.last = Some(at);
                        at
                    }
                };
                *profile.samples.entry((id, stack, ran)).or_default() += 1;
            }
            tick_at += config.interval;
            let now = Instant::now();
            if tick_at > now {
                std::thread::sleep(tick_at - now);
            } else {
                // Behind: skip the ticks missed rather than sample in a burst.
                tick_at = now;
            }
        }
        profile.window_s = began.elapsed().as_secs_f64();
        done.store(true, Ordering::Relaxed);
        let mut ids: Vec<u32> = watched.keys().copied().collect();
        ids.sort_unstable();
        for id in ids {
            let w = &watched[&id];
            let cpu_ms = cpu_of(w.handle).saturating_sub(w.cpu_start) as f64 / 1e4;
            profile
                .threads
                .push((id, cpu_ms, locate(start_of(w.handle)), name_of(w.handle)));
            // SAFETY: opened above.
            unsafe { CloseHandle(w.handle) };
        }
    });
    profile.stuck = STUCK.load(Ordering::Relaxed) as u64;
    profile.modules = modules();
    profile
}

/// The file `tools/dev_profile.py` reads: one record per line, tab separated.
pub(crate) fn render(profile: &Profile, out: &mut impl std::io::Write) -> std::io::Result<()> {
    writeln!(out, "victory-sample-profile\t1")?;
    writeln!(out, "interval_ms\t{}", profile.interval_ms)?;
    writeln!(out, "window_s\t{:.3}", profile.window_s)?;
    writeln!(out, "ticks\t{}", profile.ticks)?;
    writeln!(out, "suspended_us_mean\t{:.1}", profile.suspended_us_mean())?;
    writeln!(out, "suspended_us_max\t{}", profile.suspended_us_max)?;
    writeln!(out, "stuck\t{}", profile.stuck)?;
    for (base, size, path) in &profile.modules {
        writeln!(out, "module\t{base:#x}\t{size:#x}\t{path}")?;
    }
    for (id, cpu_ms, start, name) in &profile.threads {
        writeln!(
            out,
            "thread\t{id}\t{cpu_ms:.3}\t{start}\t{}",
            name.replace(['\t', '\n'], " ")
        )?;
    }
    for (i, frames) in profile.stacks.iter().enumerate() {
        let pcs: Vec<String> = frames.iter().map(|pc| format!("{pc:x}")).collect();
        writeln!(out, "stack\t{i}\t{}", pcs.join(" "))?;
    }
    let mut samples: Vec<_> = profile.samples.iter().collect();
    samples.sort_unstable();
    for ((id, stack, ran), n) in samples {
        writeln!(out, "sample\t{id}\t{stack}\t{}\t{n}", u8::from(*ran))?;
    }
    Ok(())
}

fn write(config: &Config, profile: &Profile) -> std::io::Result<()> {
    if let Some(dir) = config.out.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    let mut out = std::io::BufWriter::new(std::fs::File::create(&config.out)?);
    render(profile, &mut out)?;
    out.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            pairs
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| (*v).to_string())
        }
    }

    #[test]
    fn config_reads_the_switches() {
        assert_eq!(Config::parse(env(&[])), None);
        assert_eq!(Config::parse(env(&[("VICTORY_SAMPLE_PROFILE", "0")])), None);
        assert_eq!(Config::parse(env(&[("VICTORY_SAMPLE_PROFILE", "x")])), None);
        let c = Config::parse(env(&[
            ("VICTORY_SAMPLE_PROFILE", "2"),
            ("VICTORY_SAMPLE_PROFILE_START_S", "10"),
            ("VICTORY_SAMPLE_PROFILE_SECONDS", "0.5"),
            ("VICTORY_SAMPLE_PROFILE_OUT", "a/p.txt"),
        ]))
        .unwrap();
        assert_eq!(c.interval, Duration::from_millis(2));
        assert_eq!(c.start, Duration::from_secs(10));
        assert_eq!(c.window, Duration::from_millis(500));
        assert_eq!(c.out, PathBuf::from("a/p.txt"));
        let d = Config::parse(env(&[("VICTORY_SAMPLE_PROFILE", "5")])).unwrap();
        assert_eq!(
            (d.start, d.window),
            (Duration::from_secs(150), Duration::from_secs(60))
        );
        assert_eq!(d.out, PathBuf::from("victory_profile.txt"));
    }

    #[inline(never)]
    fn spin(stop: &std::sync::atomic::AtomicBool) -> u64 {
        let mut x = 0u64;
        while !stop.load(Ordering::Relaxed) {
            x = std::hint::black_box(x.wrapping_mul(6364136223846793005).wrapping_add(1));
        }
        x
    }

    #[test]
    fn a_window_samples_a_busy_and_a_parked_thread() {
        let stop = std::sync::atomic::AtomicBool::new(false);
        let (park_tx, park_rx) = std::sync::mpsc::channel::<()>();
        std::thread::scope(|scope| {
            let busy = scope.spawn(|| spin(&stop));
            let parked = scope.spawn(move || park_rx.recv().ok());
            let config = Config {
                interval: Duration::from_millis(2),
                start: Duration::ZERO,
                window: Duration::from_millis(600),
                out: PathBuf::new(),
            };
            let profile = sample(&config);
            stop.store(true, Ordering::Relaxed);
            drop(park_tx);
            busy.join().unwrap();
            parked.join().unwrap();
            // Loose: a loaded machine (the gate builds beside it) slows the ticks down.
            assert!(profile.ticks >= 3, "{}", profile.ticks);
            assert!(profile.threads.len() >= 3, "{:?}", profile.threads);
            let ran: u64 = profile
                .samples
                .iter()
                .filter(|((_, _, r), _)| *r)
                .map(|(_, n)| n)
                .sum();
            let idle: u64 = profile
                .samples
                .iter()
                .filter(|((_, _, r), _)| !*r)
                .map(|(_, n)| n)
                .sum();
            assert!(ran > 0 && idle > 0, "ran {ran} idle {idle}");
            assert!(profile.stacks.iter().any(|s| s.len() >= 3));
            assert!(
                profile.threads.iter().any(|t| t.1 > 10.0),
                "{:?}",
                profile.threads
            );
            assert!(
                profile
                    .modules
                    .iter()
                    .any(|m| m.2.to_ascii_lowercase().ends_with("ntdll.dll"))
            );
            assert_eq!(profile.stuck, 0);
            let mut text = Vec::new();
            render(&profile, &mut text).unwrap();
            let text = String::from_utf8(text).unwrap();
            assert!(text.starts_with("victory-sample-profile\t1\n"));
            assert!(text.lines().any(|l| l.starts_with("sample\t")));
            assert!(text.lines().any(|l| l.starts_with("module\t")));
            // Left to try `dev.py profile --report` on (names this test binary's frames).
            std::fs::write(std::env::temp_dir().join("victory_sample_profile_test.txt"), &text).ok();
        });
    }
}
