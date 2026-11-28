//! (El1xr) Survives a native that faults. Batch 5 part T crashed in the getter sweep: native
//! `0xf272ca4d7752933e` read through a null (`+0x1479bd0`, read 0x2b8) for the player ped.
//!
//! [`call`] runs a handler through a small trampoline that saves the caller's non-volatile
//! registers; a vectored handler, registered after every other one (write watches, missing pages
//! and the crash reporter's first-chance line run first), takes a hardware fault raised while a
//! guarded call is on the thread and resumes the trampoline as if the handler had returned. The
//! handler's own frames are dropped, so the native's script lock (a recursive lock the handler
//! takes in its prologue) is given back here ([`HandlerLock`]); anything else the native held stays
//! as it was, which is why the trainer never calls a native that faulted again.

use std::cell::Cell;
use std::sync::Once;

use windows_sys::Win32::Foundation::{
    EXCEPTION_ACCESS_VIOLATION, EXCEPTION_ILLEGAL_INSTRUCTION, EXCEPTION_INT_DIVIDE_BY_ZERO,
    EXCEPTION_PRIV_INSTRUCTION, NTSTATUS,
};
use windows_sys::Win32::System::Diagnostics::Debug::{
    AddVectoredExceptionHandler, EXCEPTION_CONTINUE_EXECUTION, EXCEPTION_CONTINUE_SEARCH,
    EXCEPTION_POINTERS,
};

/// What the trampoline saves and the fault handler restores, then what the fault was.
#[repr(C, align(16))]
#[derive(Default)]
struct Buf {
    /// rbx, rbp, rdi, rsi, r12, r13, r14, r15.
    gpr: [u64; 8],
    rsp: u64,
    resume: u64,
    mxcsr: u32,
    pad: [u32; 3],
    xmm: [[u64; 2]; 10],
    code: u32,
    rip: u64,
    access: u64,
    addr: u64,
}

const _: () = assert!(std::mem::offset_of!(Buf, rsp) == 64);
const _: () = assert!(std::mem::offset_of!(Buf, resume) == 72);
const _: () = assert!(std::mem::offset_of!(Buf, mxcsr) == 80);
const _: () = assert!(std::mem::offset_of!(Buf, xmm) == 96);

std::arch::global_asm!(
    ".text",
    ".globl victory_mm_guarded_call",
    ".p2align 4",
    ".seh_proc victory_mm_guarded_call",
    "victory_mm_guarded_call:",
    "sub rsp, 0x28",
    ".seh_stackalloc 0x28",
    ".seh_endprologue",
    "mov [r8], rbx",
    "mov [r8 + 8], rbp",
    "mov [r8 + 16], rdi",
    "mov [r8 + 24], rsi",
    "mov [r8 + 32], r12",
    "mov [r8 + 40], r13",
    "mov [r8 + 48], r14",
    "mov [r8 + 56], r15",
    "mov [r8 + 64], rsp",
    "lea rax, [rip + victory_mm_guard_resume]",
    "mov [r8 + 72], rax",
    "stmxcsr [r8 + 80]",
    "movaps [r8 + 96], xmm6",
    "movaps [r8 + 112], xmm7",
    "movaps [r8 + 128], xmm8",
    "movaps [r8 + 144], xmm9",
    "movaps [r8 + 160], xmm10",
    "movaps [r8 + 176], xmm11",
    "movaps [r8 + 192], xmm12",
    "movaps [r8 + 208], xmm13",
    "movaps [r8 + 224], xmm14",
    "movaps [r8 + 240], xmm15",
    "mov rax, rcx",
    "mov rcx, rdx",
    "call rax",
    "xor eax, eax",
    "add rsp, 0x28",
    "ret",
    "victory_mm_guard_resume:",
    "mov eax, 1",
    "add rsp, 0x28",
    "ret",
    ".seh_endproc",
);

unsafe extern "C" {
    /// Calls `entry(ctx)`; 0 when it returned, 1 when a fault resumed here.
    fn victory_mm_guarded_call(entry: usize, ctx: *mut u8, buf: *mut Buf) -> u32;
}

thread_local! {
    /// The guarded call running on this thread (null: none).
    static ACTIVE: Cell<*mut Buf> = const { Cell::new(std::ptr::null_mut()) };
}

const CAUGHT: [i32; 4] = [
    EXCEPTION_ACCESS_VIOLATION,
    EXCEPTION_ILLEGAL_INSTRUCTION,
    EXCEPTION_PRIV_INSTRUCTION,
    EXCEPTION_INT_DIVIDE_BY_ZERO,
];

unsafe extern "system" fn on_fault(info: *mut EXCEPTION_POINTERS) -> i32 {
    // SAFETY: the system passes the exception being dispatched and the faulting thread's context.
    let (record, context) = unsafe { (&*(*info).ExceptionRecord, &mut *(*info).ContextRecord) };
    if !CAUGHT.contains(&(record.ExceptionCode as NTSTATUS)) {
        return EXCEPTION_CONTINUE_SEARCH;
    }
    let buf = ACTIVE
        .try_with(|a| a.replace(std::ptr::null_mut()))
        .unwrap_or(std::ptr::null_mut());
    if buf.is_null() {
        return EXCEPTION_CONTINUE_SEARCH;
    }
    // SAFETY: `buf` is the live guarded call's, on this thread's stack below the faulting frames.
    let buf = unsafe { &mut *buf };
    buf.code = record.ExceptionCode as u32;
    buf.rip = context.Rip;
    if record.NumberParameters >= 2 {
        buf.access = record.ExceptionInformation[0] as u64;
        buf.addr = record.ExceptionInformation[1] as u64;
    }
    let [rbx, rbp, rdi, rsi, r12, r13, r14, r15] = buf.gpr;
    context.Rbx = rbx;
    context.Rbp = rbp;
    context.Rdi = rdi;
    context.Rsi = rsi;
    context.R12 = r12;
    context.R13 = r13;
    context.R14 = r14;
    context.R15 = r15;
    context.Rsp = buf.rsp;
    context.Rip = buf.resume;
    context.EFlags &= !0x400;
    context.MxCsr = buf.mxcsr;
    // SAFETY: the XMM view of the context's floating-point state.
    unsafe {
        context.Anonymous.FltSave.MxCsr = buf.mxcsr;
        let x = &mut context.Anonymous.Anonymous;
        for (dst, src) in [
            &mut x.Xmm6,
            &mut x.Xmm7,
            &mut x.Xmm8,
            &mut x.Xmm9,
            &mut x.Xmm10,
            &mut x.Xmm11,
            &mut x.Xmm12,
            &mut x.Xmm13,
            &mut x.Xmm14,
            &mut x.Xmm15,
        ]
        .into_iter()
        .zip(buf.xmm)
        {
            dst.Low = src[0];
            dst.High = src[1] as i64;
        }
    }
    EXCEPTION_CONTINUE_EXECUTION
}

fn install() {
    static ONCE: Once = Once::new();
    // SAFETY: registers a handler that only acts on faults inside a guarded call on its thread.
    ONCE.call_once(|| unsafe {
        AddVectoredExceptionHandler(0, Some(on_fault));
    });
}

/// A fault a guarded native raised.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fault {
    pub code: u32,
    /// Where it faulted.
    pub rip: u64,
    /// 0 read, 1 write, 8 execute (access violations).
    pub access: u64,
    pub addr: u64,
    /// Script lock holds given back for the dropped handler frames.
    pub released: u32,
}

impl Fault {
    pub fn describe(&self, image: usize) -> String {
        let at = match (self.rip as usize).checked_sub(image) {
            Some(off) if off < 0x1_0000_0000 => format!("image+{off:#x}"),
            _ => format!("{:#x}", self.rip),
        };
        let what = match (self.code as i32, self.access) {
            (EXCEPTION_ACCESS_VIOLATION, 0) => format!(" read {:#x}", self.addr),
            (EXCEPTION_ACCESS_VIOLATION, 1) => format!(" write {:#x}", self.addr),
            (EXCEPTION_ACCESS_VIOLATION, _) => format!(" execute {:#x}", self.addr),
            _ => String::new(),
        };
        format!(
            "{:#x} at {at}{what}, lock released {}",
            self.code, self.released
        )
    }
}

/// The recursive script lock a handler takes in its prologue (`lea rdi, [rip+lock]`,
/// `mov rcx, rdi`, `call acquire`) and gives back in its tail (`add rsp`, pops, `jmp release`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HandlerLock {
    pub lock: usize,
    pub acquire: usize,
    pub release: usize,
}

fn rel32(code: &[u8], at: usize) -> Option<i64> {
    Some(i64::from(i32::from_le_bytes(
        code.get(at..at + 4)?.try_into().ok()?,
    )))
}

fn target(base: usize, next: usize, rel: i64) -> usize {
    (base as i64 + next as i64 + rel) as usize
}

/// Finds the handler lock in `code`, the handler's first bytes, mapped at `base`.
pub fn handler_lock(code: &[u8], base: usize) -> Option<HandlerLock> {
    const LEA_RDI: [u8; 3] = [0x48, 0x8d, 0x3d];
    const MOV_RCX_RDI: [u8; 3] = [0x48, 0x89, 0xf9];
    let i = code.windows(3).take(0x40).position(|w| w == LEA_RDI)?;
    let lock = target(base, i + 7, rel32(code, i + 3)?);
    if code.get(i + 7..i + 10)? != MOV_RCX_RDI || *code.get(i + 10)? != 0xe8 {
        return None;
    }
    let acquire = target(base, i + 15, rel32(code, i + 11)?);
    let tail = i + 15 + code[i + 15..].windows(3).position(|w| w == [0x48, 0x83, 0xc4])?;
    let mut k = tail + 4;
    loop {
        match code.get(k..k + 2)? {
            [0x5b | 0x5d | 0x5e | 0x5f, _] => k += 1,
            [0x41, 0x5c..=0x5f] => k += 2,
            _ => break,
        }
    }
    if *code.get(k)? != 0xe9 {
        return None;
    }
    let release = target(base, k + 5, rel32(code, k + 1)?);
    Some(HandlerLock {
        lock,
        acquire,
        release,
    })
}

/// Where the acquire function reads the title's thread index: `mov rax, gs:[0x58]`, then
/// `mov rax, [rax]` and `mov edi, [rax+disp32]`.
pub fn thread_index_offset(acquire: &[u8]) -> Option<u32> {
    const GS_TLS: [u8; 9] = [0x65, 0x48, 0x8b, 0x04, 0x25, 0x58, 0x00, 0x00, 0x00];
    let g = acquire.windows(9).take(0x40).position(|w| w == GS_TLS)?;
    let rest = &acquire[g + 9..];
    let j = rest
        .windows(5)
        .take(0x10)
        .position(|w| w == [0x48, 0x8b, 0x00, 0x8b, 0xb8])?;
    u32::try_from(rel32(rest, j + 5)?).ok()
}

/// The title's thread index for this thread, read as the acquire function reads it.
///
/// # Safety
/// A title thread, whose TLS slot 0 holds the title's thread block.
unsafe fn thread_index(offset: u32) -> u32 {
    let tls: *const *const u8;
    // SAFETY: reads this thread's TEB TLS pointer.
    unsafe { std::arch::asm!("mov {}, gs:[0x58]", out(reg) tls, options(nostack, readonly)) };
    // SAFETY: the caller's contract.
    unsafe { (*tls).add(offset as usize).cast::<u32>().read() }
}

/// How the lock stands for this thread: (held by this thread, recursion count).
///
/// # Safety
/// `lock` is a live title lock; a title thread.
unsafe fn holds(lock: usize, offset: u32) -> u32 {
    // SAFETY: the caller's contract.
    unsafe {
        let word = (lock as *const u64).read_volatile();
        let count = ((lock + 0x10) as *const u32).read_volatile();
        if (word >> 32) as u32 == thread_index(offset) {
            count
        } else {
            0
        }
    }
}

/// Calls the native handler at `entry` with `ctx`; a fault comes back as `Err`.
///
/// # Safety
/// `entry` is a native handler in the mapped title image and `ctx` its call context, on a title
/// thread.
pub unsafe fn call(entry: usize, ctx: *mut u8) -> Result<(), Fault> {
    install();
    // SAFETY: the handler's first bytes and its acquire function's are mapped code.
    let lock = unsafe {
        let lock = handler_lock(std::slice::from_raw_parts(entry as *const u8, 0x100), entry);
        lock.and_then(|l| {
            let off =
                thread_index_offset(std::slice::from_raw_parts(l.acquire as *const u8, 0x60))?;
            Some((l, off))
        })
    };
    // SAFETY: the caller's contract.
    let held = lock.map(|(l, off)| unsafe { holds(l.lock, off) });
    let mut buf = Buf::default();
    let outer = ACTIVE.replace(&mut buf);
    // SAFETY: the caller's contract; `buf` outlives the call.
    let faulted = unsafe { victory_mm_guarded_call(entry, ctx, &mut buf) };
    ACTIVE.set(outer);
    if faulted == 0 {
        return Ok(());
    }
    let mut released = 0;
    if let (Some((l, off)), Some(held)) = (lock, held) {
        // SAFETY: the handler took this lock on this thread and its frames are gone; the release
        // function takes the lock in rcx.
        unsafe {
            let release: unsafe extern "C" fn(usize) = std::mem::transmute(l.release);
            while holds(l.lock, off) > held && released < 4 {
                release(l.lock);
                released += 1;
            }
        }
    }
    Err(Fault {
        code: buf.code,
        rip: buf.rip,
        access: buf.access,
        addr: buf.addr,
        released,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    std::arch::global_asm!(
        ".text",
        // Writes args[0] + 1 to ret[0], as a getter does.
        "victory_mm_test_ok:",
        "mov rax, [rcx]",
        "mov rax, [rax]",
        "inc rax",
        "mov rdx, [rcx + 8]",
        "mov [rdx], rax",
        "ret",
        // Spoils every non-volatile register, then reads through a null, as part T's getter did.
        "victory_mm_test_fault:",
        "push rbx",
        "push rbp",
        "mov rbx, 0x1111",
        "mov rbp, 0x2222",
        "mov rdi, 0x3333",
        "mov rsi, 0x4444",
        "mov r12, 0x5555",
        "mov r13, 0x6666",
        "mov r14, 0x7777",
        "mov r15, 0x8888",
        "xorps xmm6, xmm6",
        "xorps xmm15, xmm15",
        "xor eax, eax",
        "mov eax, [rax + 0x2b8]",
        "ud2",
    );

    unsafe extern "C" {
        fn victory_mm_test_ok();
        fn victory_mm_test_fault();
    }

    #[repr(C)]
    struct Ctx {
        args: *mut u64,
        ret: *mut u64,
    }

    #[test]
    fn a_native_that_faults_comes_back_as_an_error_and_the_caller_goes_on() {
        let mut args = [41u64];
        let mut ret = [0u64];
        let mut ctx = Ctx {
            args: args.as_mut_ptr(),
            ret: ret.as_mut_ptr(),
        };
        let ok = victory_mm_test_ok as *const () as usize;
        let bad = victory_mm_test_fault as *const () as usize;
        // SAFETY: the test handlers read `ctx` as a native handler would.
        assert_eq!(unsafe { call(ok, (&raw mut ctx).cast()) }, Ok(()));
        assert_eq!(ret[0], 42);
        let keep: Vec<f64> = (0..64).map(|i| f64::from(i) * 1.5).collect();
        let sum: f64 = std::hint::black_box(keep.iter().sum());
        for round in 0..3 {
            // SAFETY: as above; the fault is caught.
            let f = unsafe { call(bad, (&raw mut ctx).cast()) }.unwrap_err();
            assert_eq!(f.code as i32, EXCEPTION_ACCESS_VIOLATION, "round {round}");
            assert_eq!((f.access, f.addr), (0, 0x2b8));
            assert_eq!(f.released, 0, "no lock in a test handler");
            assert!(
                f.describe(bad).starts_with("0xc0000005 at image+0x"),
                "{}",
                f.describe(bad)
            );
            assert!(f.describe(bad).ends_with("read 0x2b8, lock released 0"));
        }
        assert_eq!(
            std::hint::black_box(keep.iter().sum::<f64>()),
            sum,
            "registers came back"
        );
        // SAFETY: as above.
        assert_eq!(
            unsafe { call(ok, (&raw mut ctx).cast()) },
            Ok(()),
            "and calls go on"
        );
        assert!(ACTIVE.get().is_null());
    }

    #[test]
    fn the_handler_lock_is_read_from_the_prologue_and_tail() {
        // GET_ENTITY_COORDS's handler at 0x3c09d00 (1.0.2.1), first 0x71 bytes.
        let code: Vec<u8> = [
            "56 57 53 48 83 ec 40 83 79 10 00 48 89 ce 78 4f 48 8d 3d c9 a7 ec 06 48 89 f9",
            "e8 21 ae 3f fc 48 8b 06 48 8d 5c 24 20 48 8d 54 24 30 48 89 d9 c5 f8 10 00 83",
            "78 10 00 41 0f 95 c0 c5 f8 29 02 e8 bc 63 74 fc c5 f8 28 03 48 8b 46 08 48 89",
            "f9 c5 f8 11 00 48 83 c4 40 5b 5f 5e e9 51 3d 19 01",
        ]
        .join(" ")
        .split(' ')
        .map(|b| u8::from_str_radix(b, 16).unwrap())
        .collect();
        let base = 0x3c0_9d00;
        let l = handler_lock(&code, base).unwrap();
        assert_eq!(l.lock, 0x3c09d17 + 0x6eca7c9);
        assert_eq!(l.acquire, 0x4b40);
        assert_eq!(l.release, 0x4d9dab0);
        assert_eq!(handler_lock(&code[..20], base), None);
        // The acquire function at 0x4b40: `mov rax, gs:[0x58]`, `mov rsi, rcx`, `mov rax, [rax]`,
        // `mov edi, [rax+0x1454]`.
        let acquire = [
            0x41, 0x57, 0x41, 0x56, 0x56, 0x57, 0x55, 0x53, 0x48, 0x83, 0xec, 0x28, 0x8b, 0x69,
            0x1c, 0x48, 0x8b, 0x19, 0x65, 0x48, 0x8b, 0x04, 0x25, 0x58, 0x00, 0x00, 0x00, 0x48,
            0x89, 0xce, 0x48, 0x8b, 0x00, 0x8b, 0xb8, 0x54, 0x14, 0x00, 0x00,
        ];
        assert_eq!(thread_index_offset(&acquire), Some(0x1454));
        assert_eq!(thread_index_offset(&acquire[..30]), None);
    }
}
