//! Where the presenting thread stands while a frame runs late
//! (`VICTORY_HITCH_STACKS=1`, on by default in the diagnostics build).
//!
//! A thread of ours looks every few milliseconds at how long ago the last
//! present was. Past [`SAMPLE_AFTER_MS`] it suspends the presenting thread,
//! unwinds its stack into a fixed buffer, and resumes it. When the late
//! frame ends at [`SAMPLE_FROM_MS`] or more, the same thread groups the
//! samples by their innermost frames, names them through `dbghelp`
//! (VIctory's own functions from its PDB, system DLLs by export, title code
//! as a bare address) and logs the most frequent ones.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::System::Diagnostics::Debug::{
    CONTEXT, CONTEXT_FULL_AMD64, GetThreadContext, RtlLookupFunctionEntry, RtlVirtualUnwind,
    SYMBOL_INFO, SYMOPT_DEFERRED_LOADS, SYMOPT_UNDNAME, SymFromAddr, SymInitialize, SymSetOptions,
    UNW_FLAG_NHANDLER,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentThreadStackLimits, OpenThread, ResumeThread, SuspendThread,
    THREAD_GET_CONTEXT, THREAD_QUERY_INFORMATION, THREAD_SUSPEND_RESUME,
};

/// A frame this late starts being sampled.
pub(crate) const SAMPLE_AFTER_MS: u64 = 60;
/// A frame this long or longer has its samples named.
pub(crate) const SAMPLE_FROM_MS: u64 = 250;
/// Frames unwound per sample.
const DEPTH: usize = 32;
/// Innermost frames a group is keyed by.
const KEY_FRAMES: usize = 12;
/// Samples kept per late frame (about 20 s at the sampling rate).
const MOST_SAMPLES: usize = 4000;

/// Whether the sampler runs.
fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| match std::env::var("VICTORY_HITCH_STACKS") {
        Ok(v) => v == "1",
        Err(_) => cfg!(feature = "diagnostics"),
    })
}

/// The presenting thread, its stack's bounds and its last present
/// (milliseconds of [`epoch`]).
static PRESENTER: AtomicU32 = AtomicU32::new(0);
static STACK: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];
static LAST_PRESENT_MS: AtomicU64 = AtomicU64::new(0);
static STARTED: AtomicBool = AtomicBool::new(false);

/// Samples of the frame running late, and frames ended that the sampler is
/// to name: (frame, its time in microseconds, its samples).
struct Shared {
    samples: Vec<[u64; DEPTH]>,
    ended: Vec<(u64, u64, Vec<[u64; DEPTH]>)>,
}

static SHARED: Mutex<Shared> = Mutex::new(Shared {
    samples: Vec::new(),
    ended: Vec::new(),
});

fn epoch() -> Instant {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}

fn now_ms() -> u64 {
    epoch().elapsed().as_millis() as u64
}

/// The present that ends a frame of `frame_us`, on thread `me`.
pub(crate) fn frame_ended(frame: u64, frame_us: u64, me: u32) {
    if !enabled() {
        return;
    }
    if PRESENTER.swap(me, Ordering::Relaxed) != me {
        let (mut low, mut high) = (0usize, 0usize);
        // SAFETY: writes this thread's stack bounds.
        unsafe { GetCurrentThreadStackLimits(&mut low, &mut high) };
        STACK[0].store(low as u64, Ordering::Relaxed);
        STACK[1].store(high as u64, Ordering::Relaxed);
    }
    LAST_PRESENT_MS.store(now_ms(), Ordering::Relaxed);
    {
        let mut shared = SHARED.lock().unwrap_or_else(|e| e.into_inner());
        let samples = std::mem::take(&mut shared.samples);
        if frame_us >= SAMPLE_FROM_MS * 1000 && !samples.is_empty() {
            shared.ended.push((frame, frame_us, samples));
        }
    }
    if !STARTED.swap(true, Ordering::Relaxed) {
        let started = std::thread::Builder::new()
            .name("victory-hitch".into())
            .spawn(sampler);
        if let Err(e) = started {
            tracing::warn!(error = %e, "no thread to sample late frames (VICTORY_HITCH_STACKS)");
        }
    }
}

/// `GetThreadContext` wants a 16-byte aligned context.
#[repr(C, align(16))]
struct Aligned(CONTEXT);

fn sampler() {
    let symbols = Symbols::new();
    tracing::info!(
        symbols = symbols.ready,
        "late frames: the presenting thread's stack is sampled past {SAMPLE_AFTER_MS} ms and named for frames of {SAMPLE_FROM_MS} ms or more (VICTORY_HITCH_STACKS)"
    );
    let mut open: Option<(u32, HANDLE)> = None;
    loop {
        std::thread::sleep(Duration::from_millis(5));
        let ended = std::mem::take(&mut SHARED.lock().unwrap_or_else(|e| e.into_inner()).ended);
        for (frame, frame_us, samples) in ended {
            report(&symbols, frame, frame_us, &samples);
        }
        let late = now_ms().saturating_sub(LAST_PRESENT_MS.load(Ordering::Relaxed));
        if late < SAMPLE_AFTER_MS {
            continue;
        }
        let id = PRESENTER.load(Ordering::Relaxed);
        if open.is_none_or(|(had, _)| had != id) {
            if let Some((_, h)) = open.take() {
                // SAFETY: opened below.
                unsafe { CloseHandle(h) };
            }
            // SAFETY: opens a thread of this process.
            let h = unsafe {
                OpenThread(
                    THREAD_SUSPEND_RESUME | THREAD_GET_CONTEXT | THREAD_QUERY_INFORMATION,
                    0,
                    id,
                )
            };
            if h.is_null() {
                continue;
            }
            open = Some((id, h));
        }
        let Some((_, thread)) = open else { continue };
        let low = STACK[0].load(Ordering::Relaxed) as usize;
        let high = STACK[1].load(Ordering::Relaxed) as usize;
        let frames = sample(thread, low, high);
        let mut shared = SHARED.lock().unwrap_or_else(|e| e.into_inner());
        if shared.samples.len() < MOST_SAMPLES {
            shared.samples.push(frames);
        }
    }
}

/// The thread's frames, innermost first (zeros past the last).
fn sample(thread: HANDLE, low: usize, high: usize) -> [u64; DEPTH] {
    let mut frames = [0u64; DEPTH];
    // SAFETY: zeroed is a valid CONTEXT.
    let mut context = Aligned(unsafe { std::mem::zeroed() });
    context.0.ContextFlags = CONTEXT_FULL_AMD64;
    // SAFETY: suspends, reads and resumes a thread that is not this one;
    // nothing in between allocates or takes a lock the thread could hold
    // besides the unwinder's own.
    unsafe {
        if SuspendThread(thread) == u32::MAX {
            return frames;
        }
        if GetThreadContext(thread, &mut context.0) != 0 {
            unwind(&mut context.0, low, high, &mut frames);
        }
        ResumeThread(thread);
    }
    frames
}

/// Unwinds `context` into `frames` (as `victory-loader`'s crash reports do).
fn unwind(context: &mut CONTEXT, low: usize, high: usize, frames: &mut [u64; DEPTH]) {
    frames[0] = context.Rip;
    for slot in frames.iter_mut().skip(1) {
        let rsp = context.Rsp as usize;
        if rsp < low || rsp + 8 > high {
            break;
        }
        let mut image = 0;
        // SAFETY: looks up the function table entry covering `Rip`, if any.
        let entry =
            unsafe { RtlLookupFunctionEntry(context.Rip, &mut image, std::ptr::null_mut()) };
        if entry.is_null() {
            // A leaf: the return address is at the top of the stack.
            // SAFETY: `rsp` is inside the thread's stack.
            context.Rip = unsafe { (rsp as *const u64).read() };
            context.Rsp += 8;
        } else {
            let (mut data, mut frame) = (std::ptr::null_mut(), 0);
            // SAFETY: `entry` belongs to `image`; the context is our copy.
            unsafe {
                RtlVirtualUnwind(
                    UNW_FLAG_NHANDLER,
                    image,
                    context.Rip,
                    entry,
                    context,
                    &mut data,
                    &mut frame,
                    std::ptr::null_mut(),
                )
            };
        }
        if context.Rip == 0 {
            break;
        }
        *slot = context.Rip;
    }
}

/// Names of code addresses through `dbghelp`, used by the sampler alone.
struct Symbols {
    ready: bool,
}

impl Symbols {
    fn new() -> Self {
        // SAFETY: dbghelp is used by this thread alone; the process's
        // modules are loaded (their symbols on first use).
        let ready = unsafe {
            SymSetOptions(SYMOPT_UNDNAME | SYMOPT_DEFERRED_LOADS);
            SymInitialize(GetCurrentProcess(), std::ptr::null(), 1) != 0
        };
        Symbols { ready }
    }

    /// The function at `address`, or the bare address.
    fn name(&self, address: u64) -> String {
        const NAME_BYTES: usize = 512;
        if self.ready {
            #[repr(C, align(8))]
            struct Buffer([u8; size_of::<SYMBOL_INFO>() + NAME_BYTES]);
            let mut buffer = Buffer([0; size_of::<SYMBOL_INFO>() + NAME_BYTES]);
            let info = buffer.0.as_mut_ptr().cast::<SYMBOL_INFO>();
            let mut displacement = 0u64;
            // SAFETY: `info` heads a buffer of `MaxNameLen` more bytes.
            let found = unsafe {
                (*info).SizeOfStruct = size_of::<SYMBOL_INFO>() as u32;
                (*info).MaxNameLen = NAME_BYTES as u32;
                SymFromAddr(GetCurrentProcess(), address, &mut displacement, info) != 0
            };
            if found {
                // SAFETY: dbghelp wrote `NameLen` bytes of the name.
                let name = unsafe {
                    let len = ((*info).NameLen as usize).min(NAME_BYTES - 1);
                    let bytes = std::slice::from_raw_parts((*info).Name.as_ptr().cast::<u8>(), len);
                    String::from_utf8_lossy(bytes).into_owned()
                };
                return shorten(&name);
            }
        }
        format!("{address:#x}")
    }
}

/// A function's path without its crate's generic noise: `a::b::c<T>::d`
/// keeps `c::d` and the hash suffix goes.
fn shorten(name: &str) -> String {
    let mut depth = 0;
    let mut plain = String::with_capacity(name.len());
    for c in name.chars() {
        match c {
            '<' => depth += 1,
            '>' => depth -= 1,
            _ if depth == 0 => plain.push(c),
            _ => {}
        }
    }
    let parts: Vec<&str> = plain
        .split("::")
        .filter(|p| !(p.len() == 17 && p.starts_with('h')))
        .collect();
    let keep = parts.len().saturating_sub(3);
    parts[keep..].join("::")
}

/// Logs the most frequent stacks of a late frame.
fn report(symbols: &Symbols, frame: u64, frame_us: u64, samples: &[[u64; DEPTH]]) {
    use std::collections::HashMap;
    let mut names: HashMap<u64, String> = HashMap::new();
    let mut name = |a: u64| names.entry(a).or_insert_with(|| symbols.name(a)).clone();
    let mut groups: HashMap<Vec<String>, usize> = HashMap::new();
    for frames in samples {
        let mut key: Vec<String> = Vec::with_capacity(KEY_FRAMES);
        for &pc in frames.iter().take_while(|&&pc| pc != 0) {
            let n = name(pc);
            if key.last() != Some(&n) {
                key.push(n);
            }
            if key.len() == KEY_FRAMES {
                break;
            }
        }
        *groups.entry(key).or_default() += 1;
    }
    let mut groups: Vec<(Vec<String>, usize)> = groups.into_iter().collect();
    groups.sort_by(|a, b| b.1.cmp(&a.1));
    let total = samples.len();
    for (key, n) in groups.iter().take(4) {
        tracing::info!(
            "hitch stacks: frame {frame} took {:.1} ms; {n} of {total} samples of the presenting thread in: {}",
            frame_us as f64 / 1e3,
            key.join(" <- ")
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_lose_generics_and_hashes() {
        assert_eq!(
            shorten("victory_gpu::draw::State::heap_buffers<victory_gpu::Foo>::h0123456789abcdef"),
            "draw::State::heap_buffers"
        );
        assert_eq!(shorten("NtWaitForSingleObject"), "NtWaitForSingleObject");
    }

    #[test]
    fn a_parked_thread_unwinds_to_several_frames() {
        let (tx, rx) = std::sync::mpsc::channel();
        let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
        let parked = std::thread::spawn(move || {
            let (mut low, mut high) = (0usize, 0usize);
            // SAFETY: writes this thread's stack bounds.
            unsafe { GetCurrentThreadStackLimits(&mut low, &mut high) };
            tx.send((super::super::os_thread_id(), low, high)).unwrap();
            stop_rx.recv().ok();
        });
        let (id, low, high) = rx.recv().unwrap();
        // SAFETY: a thread of this process.
        let h = unsafe {
            OpenThread(
                THREAD_SUSPEND_RESUME | THREAD_GET_CONTEXT | THREAD_QUERY_INFORMATION,
                0,
                id,
            )
        };
        assert!(!h.is_null());
        std::thread::sleep(Duration::from_millis(50));
        let frames = sample(h, low, high);
        assert!(
            frames.iter().filter(|&&f| f != 0).count() >= 3,
            "{frames:?}"
        );
        let symbols = Symbols::new();
        let named: Vec<String> = frames
            .iter()
            .take_while(|&&f| f != 0)
            .map(|&f| symbols.name(f))
            .collect();
        assert!(!named.is_empty());
        // SAFETY: opened above.
        unsafe { CloseHandle(h) };
        stop_tx.send(()).unwrap();
        parked.join().unwrap();
    }
}
