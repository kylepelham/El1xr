//! (cool_fox `5e7faabe7b`, `42b95b990f`) `VICTORY_PIPELINE_RESCUE=1`: a
//! pipeline whose maker thread ended while making it is made again, so no
//! one waits for it for ever.
//!
//! Off (default), [`Pending::make_with`] makes a pipeline inside `made`'s
//! `get_or_init`. A thread that ends inside it without unwinding (a stack
//! overflow in vkd3d-proton under Wine; `ExitThread` from a driver) holds
//! that for good, and every later `make_with` of it waits for ever: the
//! structure builder's `now`, on the title's thread. With the switch, the
//! first caller claims the making with its own thread handle and sets
//! `made` when done; a later caller waits on the pipeline's completion and
//! asks every 50 ms whether the maker has ended. If it has, the pipeline is
//! made once more on a thread with [`RESCUE_STACK`]; if that thread ends
//! too, the pipeline is given up (`made` = `None`: its draws and dispatches
//! go without it) and every waiter wakes.
//!
//! Not in the cache key: no shader or pipeline output.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::OnceLock;
use std::sync::atomic::Ordering;
use std::time::Duration;

use windows::Win32::Foundation::{
    CloseHandle, DUPLICATE_SAME_ACCESS, DuplicateHandle, HANDLE, WAIT_OBJECT_0,
};
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentThread, INFINITE, WaitForSingleObject,
};

use super::{HostPipeline, Pending};

/// The stack of the thread that makes a pipeline again ([`rescue`]):
/// reserved only, its pages committed as they are used. vkd3d-proton
/// converts DXIL recursively over its control flow, and one compute program
/// used all 256 MiB of a worker's (cool_fox).
const RESCUE_STACK: usize = 4 << 30;

/// How often a waiter asks whether the maker has ended.
const ASK: Duration = Duration::from_millis(50);

/// Whether the switch is on (read once).
pub(super) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_PIPELINE_RESCUE").is_some_and(|v| v == "1"))
}

/// [`Pending::make_with`] with the switch on: makes `p` with `build` unless
/// it is made or another thread is making it; then waits until it is made,
/// made again or given up.
pub(super) fn make_with(p: &Pending, build: impl FnOnce() -> Option<HostPipeline>) {
    make_or_wait(p, build, |p| {
        HostPipeline::new(&p.shaders, p.graphics.as_deref())
    });
}

/// [`make_with`]; `rebuild`: how a rescue makes it.
fn make_or_wait(
    p: &Pending,
    build: impl FnOnce() -> Option<HostPipeline>,
    rebuild: impl FnOnce(&Pending) -> Option<HostPipeline> + Send + 'static,
) {
    if p.made.get().is_some() {
        return;
    }
    if p.building.swap(true, Ordering::AcqRel) {
        wait(p, rebuild);
        return;
    }
    *p.maker.lock().unwrap_or_else(|e| e.into_inner()) = this_thread();
    let made = match catch_unwind(AssertUnwindSafe(build)) {
        Ok(pipeline) => pipeline,
        Err(_) => {
            tracing::warn!(shaders = ?p.shaders, "host pipeline build panicked");
            None
        }
    };
    let _ = p.made.set(made);
    let mut maker = p.maker.lock().unwrap_or_else(|e| e.into_inner());
    close(std::mem::take(&mut *maker));
}

/// Waits for the thread making `p`; makes it again if that thread ended.
fn wait(p: &Pending, rebuild: impl FnOnce(&Pending) -> Option<HostPipeline> + Send + 'static) {
    let mut rebuild = Some(rebuild);
    loop {
        {
            let (lock, done) = &p.completion;
            let held = lock.lock().unwrap_or_else(|e| e.into_inner());
            if p.made.get().is_some() {
                return;
            }
            // (timed: a notification may come between the test and the wait)
            let _ = done.wait_timeout(held, ASK);
        }
        if p.made.get().is_some() {
            return;
        }
        if maker_ended(p) && !p.rescued.swap(true, Ordering::AcqRel) {
            if let Some(rebuild) = rebuild.take() {
                rescue(p, rebuild);
            }
            return;
        }
    }
}

/// Whether the thread that claimed `p` has ended without making it.
fn maker_ended(p: &Pending) -> bool {
    let maker = p.maker.lock().unwrap_or_else(|e| e.into_inner());
    // SAFETY: a handle of ours, open while `maker` holds it.
    *maker != 0 && unsafe { WaitForSingleObject(HANDLE(*maker as _), 0) } == WAIT_OBJECT_0
}

/// Makes `p` again on a thread with [`RESCUE_STACK`] and waits for that
/// thread to end; gives `p` up if it ended without making it. Every waiter
/// wakes either way.
fn rescue(p: &Pending, rebuild: impl FnOnce(&Pending) -> Option<HostPipeline> + Send + 'static) {
    let begun = p.begun.get().map(|t| t.elapsed().as_millis() as u64);
    tracing::warn!(
        shaders = ?p.shaders,
        key = %p.key,
        after_ms = ?begun,
        "the thread making a pipeline ended while making it (a stack overflow in the host's compiler?): \
         made again on a thread with a stack of 4 GiB (VICTORY_PIPELINE_RESCUE)"
    );
    let at = p as *const Pending as usize;
    let spawned = std::thread::Builder::new()
        .name("victory-pipeline-rescue".to_owned())
        .stack_size(RESCUE_STACK)
        .spawn(move || {
            // SAFETY: the waiter that started this thread holds `p` until
            // the thread has ended (below).
            let p = unsafe { &*(at as *const Pending) };
            let made = catch_unwind(AssertUnwindSafe(|| rebuild(p))).unwrap_or(None);
            let _ = p.made.set(made);
        });
    match spawned {
        Ok(thread) => {
            use std::os::windows::io::AsRawHandle;
            // Not `join`: a thread that ends without unwinding leaves its
            // result unset, and `join` would panic on it.
            // SAFETY: the thread's own handle, open while `thread` lives.
            unsafe { WaitForSingleObject(HANDLE(thread.as_raw_handle()), INFINITE) };
        }
        Err(e) => tracing::warn!(error = %e, "no thread to make a pipeline again"),
    }
    if p.made.set(None).is_ok() {
        tracing::warn!(
            shaders = ?p.shaders,
            key = %p.key,
            "making a pipeline ended its thread again: its draws and dispatches go without it"
        );
    }
    let (lock, done) = &p.completion;
    let _held = lock.lock().unwrap_or_else(|e| e.into_inner());
    done.notify_all();
}

/// A handle of the calling thread that stays valid after it ends (0 if
/// none: then it is never asked about).
fn this_thread() -> usize {
    let mut handle = HANDLE::default();
    // SAFETY: duplicates this thread's pseudo handle into a real one.
    let ok = unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            GetCurrentThread(),
            GetCurrentProcess(),
            &mut handle,
            0,
            false,
            DUPLICATE_SAME_ACCESS,
        )
    };
    if ok.is_ok() { handle.0 as usize } else { 0 }
}

fn close(handle: usize) {
    if handle != 0 {
        // SAFETY: a handle from `this_thread`, closed once.
        let _ = unsafe { CloseHandle(HANDLE(handle as _)) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Instant;

    /// A thread that ends while making a pipeline without unwinding
    /// (`ExitThread`, what a stack overflow in vkd3d-proton does under
    /// Wine) leaves it unmade. A second caller does not make it with its
    /// own `build` or wait for ever: it makes it again on a rescue thread,
    /// or gives it up if the rescue ends too.
    #[test]
    fn a_pipeline_whose_maker_ended_is_made_again_or_given_up() {
        for rescue_ends_too in [false, true] {
            let p = Arc::new(Pending::new(Vec::new(), None));
            let maker = p.clone();
            std::thread::spawn(move || {
                make_or_wait(
                    &maker,
                    || {
                        // SAFETY: ends this test thread, which holds nothing but its claim.
                        unsafe { windows::Win32::System::Threading::ExitThread(0) }
                    },
                    |_| None,
                )
            });
            let started = Instant::now();
            while !maker_ended(&p) && started.elapsed() < Duration::from_secs(10) {
                std::thread::sleep(Duration::from_millis(5));
            }
            assert!(maker_ended(&p), "the maker ended");
            let (tell, told) = std::sync::mpsc::channel();
            let waiter = p.clone();
            std::thread::spawn(move || {
                make_or_wait(
                    &waiter,
                    || panic!("the second caller must not make it itself"),
                    move |_| {
                        if rescue_ends_too {
                            // SAFETY: as above, the rescue thread.
                            unsafe { windows::Win32::System::Threading::ExitThread(0) }
                        }
                        None
                    },
                );
                let _ = tell.send(waiter.made.get().is_some());
            });
            assert_eq!(
                told.recv_timeout(Duration::from_secs(10)),
                Ok(true),
                "the waiter returns"
            );
            assert!(p.rescued.load(Ordering::Acquire));
            assert!(matches!(p.made.get(), Some(None)));
        }
    }

    /// A maker that finishes wakes a waiter, which never rescues it.
    #[test]
    fn a_waiter_returns_when_the_maker_finishes() {
        let p = Arc::new(Pending::new(Vec::new(), None));
        let (go, gate) = std::sync::mpsc::channel::<()>();
        let maker = p.clone();
        let making = std::thread::spawn(move || {
            make_or_wait(
                &maker,
                || {
                    gate.recv().unwrap();
                    None
                },
                |_| panic!("not rescued"),
            )
        });
        while !p.building.load(Ordering::Acquire) {
            std::thread::yield_now();
        }
        let waiter = p.clone();
        let waiting = std::thread::spawn(move || {
            make_or_wait(
                &waiter,
                || panic!("not made twice"),
                |_| panic!("not rescued"),
            )
        });
        std::thread::sleep(Duration::from_millis(120));
        assert!(!waiting.is_finished(), "the waiter waits for the maker");
        go.send(()).unwrap();
        making.join().unwrap();
        waiting.join().unwrap();
        assert!(!p.rescued.load(Ordering::Acquire));
        assert_eq!(*p.maker.lock().unwrap(), 0, "the maker's handle is closed");
    }
}
