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
//!
//! `VICTORY_HITCH_STACKS_TITLE=1` (with `VICTORY_HITCH_STACKS=1`) also
//! samples the title's thread that calls the present at every look, late
//! or not, and every [`TITLE_SAMPLES`] samples logs where it stood most
//! ([`report_title`]): what a normal frame's title thread waits in.

use std::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::HANDLE;
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
/// A thread of ours in a span this old is sampled too while a frame is late.
const OTHER_AFTER_MS: u64 = 40;
/// Frames unwound per sample.
const DEPTH: usize = 48;
/// Innermost frames a group is keyed by.
const KEY_FRAMES: usize = 22;
/// Samples kept per late frame.
const MOST_SAMPLES: usize = 8000;
/// Threads whose open spans are known.
const THREADS: usize = 128;
/// Samples of the title's presenting thread per report.
const TITLE_SAMPLES: usize = 2000;
/// Innermost frames a title sample is grouped by.
const TITLE_KEY_FRAMES: usize = 6;

fn title_enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| enabled() && std::env::var("VICTORY_HITCH_STACKS_TITLE").is_ok_and(|v| v == "1"))
}

/// The title's thread that calls the present, and its stack's bounds.
static TITLE: AtomicU32 = AtomicU32::new(0);
static TITLE_STACK: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

/// The title calls a present on thread `me`.
pub(crate) fn title_presents(me: u32) {
    if !title_enabled() || TITLE.load(Ordering::Relaxed) == me {
        return;
    }
    let (mut low, mut high) = (0usize, 0usize);
    // SAFETY: writes this thread's stack bounds.
    unsafe { GetCurrentThreadStackLimits(&mut low, &mut high) };
    TITLE_STACK[0].store(low as u64, Ordering::Relaxed);
    TITLE_STACK[1].store(high as u64, Ordering::Relaxed);
    TITLE.store(me, Ordering::Relaxed);
}

/// Whether the sampler runs.
fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("VICTORY_HITCH_STACKS").is_ok_and(|v| v == "1"))
}

/// A thread that opened spans: its id, its stack's bounds, the
/// milliseconds (of [`epoch`], plus one) its outermost open span began (0:
/// none open) and the innermost open phase.
struct Known {
    id: AtomicU32,
    stack: [AtomicU64; 2],
    since_ms: AtomicU64,
    phase: AtomicU32,
}

static KNOWN: [Known; THREADS] = [const {
    Known {
        id: AtomicU32::new(0),
        stack: [const { AtomicU64::new(0) }; 2],
        since_ms: AtomicU64::new(0),
        phase: AtomicU32::new(0),
    }
}; THREADS];
static KNOWN_TAKEN: AtomicU32 = AtomicU32::new(0);

thread_local! {
    /// This thread's entry in [`KNOWN`] (`usize::MAX`: none left).
    static MINE: Cell<Option<usize>> = const { Cell::new(None) };
}

fn mine() -> Option<&'static Known> {
    let at = MINE.with(|mine| {
        if let Some(at) = mine.get() {
            return at;
        }
        let at = {
            let at = KNOWN_TAKEN.fetch_add(1, Ordering::Relaxed) as usize;
            if let Some(k) = KNOWN.get(at) {
                let (mut low, mut high) = (0usize, 0usize);
                // SAFETY: writes this thread's stack bounds.
                unsafe { GetCurrentThreadStackLimits(&mut low, &mut high) };
                k.stack[0].store(low as u64, Ordering::Relaxed);
                k.stack[1].store(high as u64, Ordering::Relaxed);
                k.id.store(super::thread_id(), Ordering::Relaxed);
            }
            at
        };
        mine.set(Some(at));
        at
    });
    KNOWN.get(at)
}

/// A span opened (`outermost`: none was open on this thread).
pub(crate) fn opened(phase: super::Phase, outermost: bool, now: Instant) {
    if !enabled() {
        return;
    }
    let Some(k) = mine() else { return };
    k.phase.store(phase as u32, Ordering::Relaxed);
    if outermost {
        let ms = now.saturating_duration_since(epoch()).as_millis() as u64 + 1;
        k.since_ms.store(ms, Ordering::Relaxed);
    }
}

/// A span closed; `parent` is open again, or none.
pub(crate) fn closed(parent: Option<super::Phase>) {
    if !enabled() {
        return;
    }
    let Some(k) = mine() else { return };
    match parent {
        Some(p) => k.phase.store(p as u32, Ordering::Relaxed),
        None => k.since_ms.store(0, Ordering::Relaxed),
    }
}

/// The presenting thread, its stack's bounds and its last present
/// (milliseconds of [`epoch`]).
static PRESENTER: AtomicU32 = AtomicU32::new(0);
static STACK: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];
static LAST_PRESENT_MS: AtomicU64 = AtomicU64::new(0);
static STARTED: AtomicBool = AtomicBool::new(false);

/// One sample: whose (`u32::MAX`: the presenting thread; else the phase a
/// thread of ours was in) and its frames.
type Sample = (u32, [u64; DEPTH]);

/// Samples of the frame running late, and frames ended that the sampler is
/// to name: (frame, its time in microseconds, its samples).
struct Shared {
    samples: Vec<Sample>,
    ended: Vec<(u64, u64, Vec<Sample>)>,
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

/// Handles of the threads sampled, kept open.
struct Handles(std::collections::HashMap<u32, HANDLE>);

impl Handles {
    fn get(&mut self, id: u32) -> Option<HANDLE> {
        if let Some(&h) = self.0.get(&id) {
            return Some(h);
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
            return None;
        }
        self.0.insert(id, h);
        Some(h)
    }
}

fn sampler() {
    let symbols = Symbols::new();
    tracing::info!(
        symbols = symbols.ready,
        "late frames: the presenting thread's stack, and those of our threads in a call of {OTHER_AFTER_MS} ms or more, are sampled past {SAMPLE_AFTER_MS} ms and named for frames of {SAMPLE_FROM_MS} ms or more (VICTORY_HITCH_STACKS)"
    );
    let mut handles = Handles(std::collections::HashMap::new());
    let me = super::thread_id();
    let mut title: Vec<[u64; DEPTH]> = Vec::with_capacity(TITLE_SAMPLES);
    loop {
        std::thread::sleep(Duration::from_millis(5));
        let ended = std::mem::take(&mut SHARED.lock().unwrap_or_else(|e| e.into_inner()).ended);
        for (frame, frame_us, samples) in ended {
            report(&symbols, frame, frame_us, &samples);
        }
        let id = TITLE.load(Ordering::Relaxed);
        if id != 0
            && let Some(h) = handles.get(id)
        {
            let low = TITLE_STACK[0].load(Ordering::Relaxed) as usize;
            let high = TITLE_STACK[1].load(Ordering::Relaxed) as usize;
            title.push(sample(h, low, high));
            if title.len() == TITLE_SAMPLES {
                report_title(&symbols, &title);
                title.clear();
            }
        }
        let now = now_ms();
        let late = now.saturating_sub(LAST_PRESENT_MS.load(Ordering::Relaxed));
        if late < SAMPLE_AFTER_MS {
            continue;
        }
        let presenter = PRESENTER.load(Ordering::Relaxed);
        let mut taken: Vec<Sample> = Vec::new();
        if let Some(h) = handles.get(presenter) {
            let low = STACK[0].load(Ordering::Relaxed) as usize;
            let high = STACK[1].load(Ordering::Relaxed) as usize;
            taken.push((u32::MAX, sample(h, low, high)));
        }
        let known = (KNOWN_TAKEN.load(Ordering::Relaxed) as usize).min(THREADS);
        for k in &KNOWN[..known] {
            let id = k.id.load(Ordering::Relaxed);
            let since = k.since_ms.load(Ordering::Relaxed);
            if id == 0 || id == presenter || id == me || since == 0 || now + 1 < since + OTHER_AFTER_MS {
                continue;
            }
            let phase = k.phase.load(Ordering::Relaxed);
            if let Some(h) = handles.get(id) {
                let low = k.stack[0].load(Ordering::Relaxed) as usize;
                let high = k.stack[1].load(Ordering::Relaxed) as usize;
                taken.push((phase, sample(h, low, high)));
            }
        }
        let mut shared = SHARED.lock().unwrap_or_else(|e| e.into_inner());
        let room = MOST_SAMPLES.saturating_sub(shared.samples.len());
        shared.samples.extend(taken.into_iter().take(room));
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
            // Rust links its PDB by bare name: look beside the executable.
            let search = std::env::current_exe()
                .ok()
                .and_then(|e| e.parent().map(|d| format!("{}\0", d.display())));
            SymInitialize(
                GetCurrentProcess(),
                search.as_ref().map_or(std::ptr::null(), |s| s.as_ptr()),
                1,
            ) != 0
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
        module_offset(address)
    }
}

/// `module+0xoffset` for code without a name, or the bare address.
fn module_offset(address: u64) -> String {
    use windows_sys::Win32::System::LibraryLoader::{
        GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
        GetModuleFileNameW, GetModuleHandleExW,
    };
    let mut module = std::ptr::null_mut();
    let mut path = [0u16; 260];
    // SAFETY: looks up the module holding `address` without holding it;
    // the name is written into `path`.
    let len = unsafe {
        if GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            address as usize as *const u16,
            &mut module,
        ) == 0
        {
            return format!("{address:#x}");
        }
        GetModuleFileNameW(module, path.as_mut_ptr(), path.len() as u32) as usize
    };
    let path = String::from_utf16_lossy(&path[..len]);
    let file = path.rsplit('\\').next().unwrap_or(&path);
    format!("{file}+{:#x}", address - module as u64)
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

/// Logs where the title's presenting thread stood most in `samples`, by
/// their innermost [`TITLE_KEY_FRAMES`] frames.
fn report_title(symbols: &Symbols, samples: &[[u64; DEPTH]]) {
    use std::collections::HashMap;
    let mut names: HashMap<u64, String> = HashMap::new();
    let mut groups: HashMap<Vec<String>, usize> = HashMap::new();
    for frames in samples {
        let mut key: Vec<String> = Vec::with_capacity(TITLE_KEY_FRAMES);
        for &pc in frames.iter().take_while(|&&pc| pc != 0) {
            let n = names.entry(pc).or_insert_with(|| symbols.name(pc)).clone();
            if key.last() != Some(&n) {
                key.push(n);
            }
            if key.len() == TITLE_KEY_FRAMES {
                break;
            }
        }
        *groups.entry(key).or_default() += 1;
    }
    let mut groups: Vec<(Vec<String>, usize)> = groups.into_iter().collect();
    groups.sort_by(|a, b| b.1.cmp(&a.1));
    for (key, n) in groups.iter().take(8) {
        tracing::info!(
            "title stacks: {n} of {} samples of the title's presenting thread: {}",
            samples.len(),
            key.join(" <- ")
        );
    }
}

/// Logs the most frequent stacks of a late frame: the presenting
/// thread's, and those of our threads in a long call.
fn report(symbols: &Symbols, frame: u64, frame_us: u64, samples: &[Sample]) {
    use std::collections::HashMap;
    let mut names: HashMap<u64, String> = HashMap::new();
    let mut name = |a: u64| names.entry(a).or_insert_with(|| symbols.name(a)).clone();
    let mut groups: HashMap<(u32, Vec<String>), usize> = HashMap::new();
    let mut totals: HashMap<u32, usize> = HashMap::new();
    for (whose, frames) in samples {
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
        *groups.entry((*whose, key)).or_default() += 1;
        *totals.entry(*whose).or_default() += 1;
    }
    let mut groups: Vec<((u32, Vec<String>), usize)> = groups.into_iter().collect();
    groups.sort_by(|a, b| b.1.cmp(&a.1));
    let mut shown: HashMap<u32, usize> = HashMap::new();
    for ((whose, key), n) in &groups {
        let count = shown.entry(*whose).or_default();
        let most = if *whose == u32::MAX { 4 } else { 3 };
        if *count >= most {
            continue;
        }
        *count += 1;
        let who = super::NAMES
            .get(*whose as usize)
            .map_or("the presenting thread".to_owned(), |p| format!("threads in {p}"));
        tracing::info!(
            "hitch stacks: frame {frame} took {:.1} ms; {n} of {} samples of {who}: {}",
            frame_us as f64 / 1e3,
            totals[whose],
            key.join(" <- ")
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::Foundation::CloseHandle;

    #[test]
    fn a_thread_keeps_one_entry() {
        let first = mine().map(std::ptr::from_ref);
        let taken = KNOWN_TAKEN.load(Ordering::Relaxed);
        assert!(first.is_some());
        assert_eq!(mine().map(std::ptr::from_ref), first);
        assert_eq!(KNOWN_TAKEN.load(Ordering::Relaxed), taken);
    }

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
