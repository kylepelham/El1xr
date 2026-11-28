//! (feedcard, #88) `VICTORY_STALE_DIFF_WIDE=1` (Live, off by default): the stale copy's compare of
//! title memory with a card copy's CPU side (`diff_copy_direct`, `VICTORY_COPY_CHANGED=1`) takes
//! 128 bytes a step and asks for the bytes 1 KiB ahead on both sides, instead of 32 bytes a step.
//!
//! Why: the stale copy is about 8 ms of the final flush's busy time a frame (part Q: 25.8 MB),
//! about 4 GB/s a thread. Most of it is card copies (copy137mb: 69 of 82 MB, a third differing),
//! read beside their CPU side and written only where they differ. Both sides are cold (last read a
//! frame ago) and lie in 4 KiB pages, where the core's own prefetcher stops at every page edge;
//! and every 32 bytes was a compare, a mask, a branch. Here four lanes are compared and joined into
//! one branch, the slow per-lane path is taken only where something differs, and a prefetch 1 KiB
//! ahead runs over the page edges (a prefetch never faults, also on a page that is not there).
//!
//! Same bytes, same changed runs, same `done` on a fault: every load of a step comes before any of
//! its stores, so a fault on a step leaves it whole for the caller's fallback, as a fault on one
//! 32-byte step did. Not in the cache key: no shader or pipeline output.

static ON: victory_core::settings::Live =
    victory_core::settings::Live::flag("VICTORY_STALE_DIFF_WIDE");

#[cfg(test)]
thread_local! {
    pub(crate) static FORCED: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

pub(super) fn on() -> bool {
    #[cfg(test)]
    if let Some(forced) = FORCED.with(std::cell::Cell::get) {
        return forced;
    }
    ON.get_bool()
}

/// [`super::diff_copy_direct_avx2`], 128 bytes a step with prefetches ahead: same arguments
/// and answer (`done`, a multiple of 32, and whether a source page faulted).
#[cfg(all(target_arch = "x86_64", windows))]
#[inline(never)]
#[target_feature(enable = "avx2")]
pub(super) unsafe fn diff_copy_wide_avx2(
    src: *const u8,
    dst: *mut u8,
    vec_len: usize,
    mask: *mut u64,
    fault_slot: *mut u64,
) -> (usize, bool) {
    let done: usize;
    let faulted: usize;
    // SAFETY: as `diff_copy_direct_avx2`: `dst` holds `vec_len` writable bytes (a multiple of 32,
    // at most 64 KiB), `mask` 32 zeroed words, `fault_slot` this thread's `FAST_FAULT_RIP` cell and
    // `FAST_COPY` covers `[src, src + vec_len)`, so a source fault resumes at `9:` with `rdx` the
    // offset of the step that faulted (none of whose stores ran: its four loads come first), which is
    // then redone 32 bytes at a time so that a second fault resumes at `19:` with `rdx` exact.
    // Prefetches past the end or onto a missing page do not fault.
    unsafe {
        core::arch::asm!(
            "lea rax, [rip + 9f]",
            "mov qword ptr [r9], rax",
            "mov r12, rcx",
            "and r12, -128",
            "cmp rdx, r12",
            "jae 5f",
            "2:",
            "prefetcht0 [rsi + rdx + 1024]",
            "prefetcht0 [rdi + rdx + 1024]",
            "vmovdqu ymm0, ymmword ptr [rsi + rdx]",
            "vmovdqu ymm1, ymmword ptr [rsi + rdx + 32]",
            "vmovdqu ymm2, ymmword ptr [rsi + rdx + 64]",
            "vmovdqu ymm3, ymmword ptr [rsi + rdx + 96]",
            "vpcmpeqb ymm4, ymm0, ymmword ptr [rdi + rdx]",
            "vpcmpeqb ymm5, ymm1, ymmword ptr [rdi + rdx + 32]",
            "vpcmpeqb ymm6, ymm2, ymmword ptr [rdi + rdx + 64]",
            "vpcmpeqb ymm7, ymm3, ymmword ptr [rdi + rdx + 96]",
            "vpand ymm8, ymm4, ymm5",
            "vpand ymm9, ymm6, ymm7",
            "vpand ymm8, ymm8, ymm9",
            "vpmovmskb eax, ymm8",
            "cmp eax, -1",
            "je 4f",
            // Lane 0.
            "vpmovmskb eax, ymm4",
            "cmp eax, -1",
            "je 22f",
            "vmovdqu ymmword ptr [rdi + rdx], ymm0",
            "mov r10, rdx",
            "shr r10, 5",
            "mov r11, r10",
            "shr r11, 6",
            "xor eax, eax",
            "bts rax, r10",
            "or qword ptr [r8 + r11*8], rax",
            "22:",
            "vpmovmskb eax, ymm5",
            "cmp eax, -1",
            "je 23f",
            "vmovdqu ymmword ptr [rdi + rdx + 32], ymm1",
            "lea r10, [rdx + 32]",
            "shr r10, 5",
            "mov r11, r10",
            "shr r11, 6",
            "xor eax, eax",
            "bts rax, r10",
            "or qword ptr [r8 + r11*8], rax",
            "23:",
            "vpmovmskb eax, ymm6",
            "cmp eax, -1",
            "je 24f",
            "vmovdqu ymmword ptr [rdi + rdx + 64], ymm2",
            "lea r10, [rdx + 64]",
            "shr r10, 5",
            "mov r11, r10",
            "shr r11, 6",
            "xor eax, eax",
            "bts rax, r10",
            "or qword ptr [r8 + r11*8], rax",
            "24:",
            "vpmovmskb eax, ymm7",
            "cmp eax, -1",
            "je 4f",
            "vmovdqu ymmword ptr [rdi + rdx + 96], ymm3",
            "lea r10, [rdx + 96]",
            "shr r10, 5",
            "mov r11, r10",
            "shr r11, 6",
            "xor eax, eax",
            "bts rax, r10",
            "or qword ptr [r8 + r11*8], rax",
            "4:",
            "add rdx, 128",
            "cmp rdx, r12",
            "jb 2b",
            // The last steps of 32 bytes, as `diff_copy_direct_avx2`.
            "5:",
            "cmp rdx, rcx",
            "jae 7f",
            "6:",
            "vmovdqu ymm0, ymmword ptr [rsi + rdx]",
            "vpcmpeqb ymm1, ymm0, ymmword ptr [rdi + rdx]",
            "vpmovmskb eax, ymm1",
            "cmp eax, -1",
            "je 26f",
            "vmovdqu ymmword ptr [rdi + rdx], ymm0",
            "mov r10, rdx",
            "shr r10, 5",
            "mov r11, r10",
            "shr r11, 6",
            "xor eax, eax",
            "bts rax, r10",
            "or qword ptr [r8 + r11*8], rax",
            "26:",
            "add rdx, 32",
            "cmp rdx, rcx",
            "jb 6b",
            "7:",
            "xor eax, eax",
            "jmp 8f",
            // A fault in a step: none of its stores ran. Redo from its start 32 bytes at a time,
            // so a second fault names the very step of 32 bytes that faulted, as the narrow form
            // does (the page stays out, or its guard was armed again by the handler).
            "9:",
            "lea rax, [rip + 19f]",
            "mov qword ptr [r9], rax",
            "jmp 6b",
            "19:",
            "mov eax, 1",
            "8:",
            "mov qword ptr [r9], 0",
            "vzeroupper",
            in("rsi") src,
            in("rdi") dst,
            in("rcx") vec_len,
            in("r8") mask,
            in("r9") fault_slot,
            inout("rdx") 0usize => done,
            out("rax") faulted,
            out("r10") _,
            out("r11") _,
            out("r12") _,
            out("ymm0") _,
            out("ymm1") _,
            out("ymm2") _,
            out("ymm3") _,
            out("ymm4") _,
            out("ymm5") _,
            out("ymm6") _,
            out("ymm7") _,
            out("ymm8") _,
            out("ymm9") _,
            options(nostack),
        );
    }
    (done, faulted != 0)
}

#[cfg(test)]
mod tests {
    use super::FORCED;

    /// Title-like memory of `pages` 4 KiB pages, `hole` (if any) a page that is not readable.
    fn memory(pages: usize, hole: Option<usize>) -> *mut u8 {
        use windows_sys::Win32::System::Memory::{
            MEM_COMMIT, MEM_RESERVE, PAGE_NOACCESS, PAGE_READWRITE, VirtualAlloc, VirtualProtect,
        };
        // SAFETY: a fresh allocation of our own, never freed (tests).
        unsafe {
            let at = VirtualAlloc(
                std::ptr::null(),
                pages * 4096,
                MEM_COMMIT | MEM_RESERVE,
                PAGE_READWRITE,
            )
            .cast::<u8>();
            assert!(!at.is_null());
            if let Some(h) = hole {
                let mut old = 0;
                assert!(
                    VirtualProtect(at.add(h * 4096).cast(), 4096, PAGE_NOACCESS, &mut old) != 0
                );
            }
            at
        }
    }

    fn rng(seed: &mut u64) -> u64 {
        *seed ^= *seed << 13;
        *seed ^= *seed >> 7;
        *seed ^= *seed << 17;
        *seed
    }

    /// Both forms over random lengths, offsets, differences and a missing page: the same copy,
    /// the same changed runs, the same answer.
    #[test]
    fn the_wide_compare_copies_and_tells_the_same() {
        let src = memory(17, None);
        let holed = memory(17, Some(9));
        let mut seed = 0x1234_5678_9abc_def1u64;
        let mut faults = 0;
        for round in 0..400 {
            let base = if round % 5 == 4 { holed } else { src };
            let off = (rng(&mut seed) as usize % 4096) & !31;
            let len = 1 + rng(&mut seed) as usize % (64 * 1024 - off);
            let mut held = vec![0u8; len];
            for i in 0..len {
                // SAFETY: inside our allocation; the hole is skipped.
                let at = off + i;
                if base == holed && at / 4096 == 9 {
                    continue;
                }
                let v = rng(&mut seed) as u8;
                unsafe { *base.add(at) = v };
                held[i] = if rng(&mut seed) % 64 == 0 { v ^ 1 } else { v };
            }
            // A stretch all equal and one all differing, so both branches run.
            if len > 600 {
                for i in 200..400 {
                    held[i] = unsafe { *base.add(off + i) }.wrapping_add(u8::from(i >= 300));
                }
            }
            let mut out = Vec::new();
            for wide in [false, true] {
                FORCED.set(Some(wide));
                let mut h = held.clone();
                let mut runs = Vec::new();
                // SAFETY: `base + off` .. `+ len` lies in our allocation.
                let answer = crate::host::diff_copy_direct(
                    unsafe { base.add(off) },
                    &mut h,
                    7 * 4096,
                    &mut runs,
                );
                out.push((h, runs, answer));
            }
            FORCED.set(None);
            assert_eq!(out[0], out[1], "round {round}: off {off}, len {len}");
            faults += usize::from(matches!(out[1].2, Err(d) if d < len & !31));
        }
        assert!(faults > 20, "only {faults} rounds met the missing page");
    }

    /// (#88) The stale copy's compare over cold memory (256 MiB each side, read in runs of about
    /// 25 KiB as the final flush does, each run once a pass), a third of the 4 KiB pages
    /// differing in a few places, 1 and 16 threads: GB/s, 32-byte and wide forms alternated.
    /// `cargo test --release -p victory-gpu --lib bench_diff_wide -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn bench_diff_wide() {
        const BYTES: usize = 256 << 20;
        const RUN: usize = 25 << 10;
        let src = memory(BYTES / 4096, None) as usize;
        let held = memory(BYTES / 4096, None) as usize;
        let mut seed = 99u64;
        for page in 0..BYTES / 4096 {
            for i in (0..4096).step_by(8) {
                let v = rng(&mut seed);
                // SAFETY: inside our allocations.
                unsafe {
                    *((src + page * 4096 + i) as *mut u64) = v;
                    *((held + page * 4096 + i) as *mut u64) = if page % 3 == 0 && i % 1024 == 0 {
                        !v
                    } else {
                        v
                    };
                }
            }
        }
        let runs: Vec<usize> = (0..BYTES / RUN).map(|r| r * RUN).collect();
        for round in 0..3 {
            for threads in [1usize, 16] {
                for wide in [false, true] {
                    let t = std::time::Instant::now();
                    std::thread::scope(|s| {
                        for k in 0..threads {
                            let runs = &runs;
                            s.spawn(move || {
                                FORCED.set(Some(wide));
                                let mut out = Vec::new();
                                for &r in runs.iter().skip(k).step_by(threads) {
                                    // Undo the last pass's copy where it differed, so it differs again.
                                    // SAFETY: inside our allocations.
                                    unsafe {
                                        for i in (0..RUN).step_by(1024) {
                                            if (r + i) / 4096 % 3 == 0 && (r + i) % 1024 == 0 {
                                                let p = (held + r + i) as *mut u64;
                                                *p = !*((src + r + i) as *const u64);
                                            }
                                        }
                                    }
                                    out.clear();
                                    let h = unsafe {
                                        std::slice::from_raw_parts_mut((held + r) as *mut u8, RUN)
                                    };
                                    let _ = crate::host::diff_copy_direct(
                                        (src + r) as *const u8,
                                        h,
                                        0,
                                        &mut out,
                                    );
                                }
                            });
                        }
                    });
                    let s = t.elapsed().as_secs_f64();
                    eprintln!(
                        "round {round} threads={threads} wide={wide}: {:.2} GB/s of title memory compared ({:.1} ms for 256 MiB)",
                        BYTES as f64 / s / 1e9,
                        s * 1e3
                    );
                }
            }
        }
    }
}
