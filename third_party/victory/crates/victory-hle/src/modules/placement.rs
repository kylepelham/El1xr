//! The title's placement of its own threads (critpath #88): passed to the system unchanged and
//! noted by `victory_gpu::title_threads` (logged, and `VICTORY_TITLE_BOOST`).
#![allow(non_snake_case)]
// Title code calls these through its import table, passing pointers the real
// API would trust.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::ffi::c_void;

use crate::{HleModule, hle_export};

pub extern "system" fn SetThreadGroupAffinity(
    thread: *mut c_void,
    affinity: *const c_void,
    previous: *mut c_void,
) -> i32 {
    victory_gpu::title_threads::set_thread_group_affinity(thread, affinity, previous)
}

pub extern "system" fn SetThreadPriority(thread: *mut c_void, priority: i32) -> i32 {
    victory_gpu::title_threads::set_thread_priority(thread, priority)
}

pub extern "system" fn SetProcessPriorityBoost(process: *mut c_void, disable: i32) -> i32 {
    victory_gpu::title_threads::set_process_priority_boost(process, disable)
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "placement",
        dlls: &[
            "kernel32.dll",
            "kernelbase.dll",
            "api-ms-win-core-processthreads-*",
            "api-ms-win-core-processtopology-*",
        ],
        exports: vec![
            hle_export!(SetThreadGroupAffinity),
            hle_export!(SetThreadPriority),
            hle_export!(SetProcessPriorityBoost),
        ],
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_title_s_placement_calls_are_ours() {
        for (dll, name) in [
            (
                "api-ms-win-core-processtopology-l1-1-0.dll",
                "SetThreadGroupAffinity",
            ),
            (
                "api-ms-win-core-processthreads-l1-1-0.dll",
                "SetThreadPriority",
            ),
            (
                "api-ms-win-core-processthreads-l1-1-2.dll",
                "SetProcessPriorityBoost",
            ),
        ] {
            assert!(crate::registry::lookup(dll, name).is_some(), "{dll} {name}");
        }
        // The rest of those sets still come from the system.
        assert!(
            crate::registry::lookup(
                "api-ms-win-core-processtopology-l1-1-0.dll",
                "GetThreadGroupAffinity"
            )
            .is_none()
        );
        // SAFETY: no arguments.
        let me = unsafe { windows_sys::Win32::System::Threading::GetCurrentThread() };
        assert_ne!(super::SetThreadPriority(me, 0), 0);
    }
}
