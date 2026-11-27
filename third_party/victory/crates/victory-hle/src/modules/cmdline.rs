//! Title command line and C-runtime argument vectors.
//!
//! `victory run <exe> --execute -- <title_args...>` passes the title's own
//! arguments after `--`. Without overriding these APIs, the title's CRT and
//! `GetCommandLine` calls pass through to the host and see `victory`'s own
//! process command line (`victory run <exe> --execute -- ...`), so `argv[1]`
//! is `"run"` rather than the title's first argument (breaking both CLI flags
//! and `XLaunchNewGame` relaunches).
#![allow(non_snake_case)]

use std::ffi::{CString, c_char};
use std::path::Path;
use std::sync::atomic::{AtomicI32, AtomicPtr, Ordering};
use std::sync::{LazyLock, Mutex};

use crate::{HleModule, hle_export};

/// Backing storage kept alive for the lifetime of the process so pointers
/// already handed to title code remain valid even if updated (e.g. in tests).
struct Storage {
    _cmdline_a: CString,
    _cmdline_w: Vec<u16>,
    _args_a: Vec<CString>,
    _args_w: Vec<Vec<u16>>,
    _argv_a: Vec<*mut c_char>,
    _argv_w: Vec<*mut u16>,
}

// SAFETY: storage owns heap buffers whose raw pointers are read-only once published.
unsafe impl Send for Storage {}

static KEPT: Mutex<Vec<Storage>> = Mutex::new(Vec::new());

static ARGC: AtomicI32 = AtomicI32::new(0);
static ARGV_A: AtomicPtr<*mut c_char> = AtomicPtr::new(std::ptr::null_mut());
static ARGV_W: AtomicPtr<*mut u16> = AtomicPtr::new(std::ptr::null_mut());
static CMDLINE_A: AtomicPtr<u8> = AtomicPtr::new(std::ptr::null_mut());
static CMDLINE_W: AtomicPtr<u16> = AtomicPtr::new(std::ptr::null_mut());
static WINMAIN_A: AtomicPtr<u8> = AtomicPtr::new(std::ptr::null_mut());
static WINMAIN_W: AtomicPtr<u16> = AtomicPtr::new(std::ptr::null_mut());

/// Default empty command line if `set_title_command_line` was never called.
static DEFAULT_INIT: LazyLock<()> = LazyLock::new(|| {
    if ARGC.load(Ordering::Acquire) == 0 {
        set_title_command_line(Path::new("title.exe"), &[]);
    }
});

/// Quotes one argument using Windows `CommandLineToArgvW` / MSVC CRT rules.
fn quote_arg(arg: &str) -> String {
    if !arg.is_empty() && !arg.bytes().any(|b| matches!(b, b' ' | b'\t' | b'"')) {
        return arg.to_owned();
    }
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('"');
    let mut backslashes = 0usize;
    for ch in arg.chars() {
        match ch {
            '\\' => backslashes += 1,
            '"' => {
                for _ in 0..backslashes * 2 + 1 {
                    out.push('\\');
                }
                out.push('"');
                backslashes = 0;
            }
            _ => {
                for _ in 0..backslashes {
                    out.push('\\');
                }
                backslashes = 0;
                out.push(ch);
            }
        }
    }
    for _ in 0..backslashes * 2 {
        out.push('\\');
    }
    out.push('"');
    out
}

/// Formats `exe` and `args` into `(full_command_line, args_byte_offset)` where
/// `args_byte_offset` is where `_get_narrow_winmain_command_line` starts.
fn format_command_line(exe: &str, args: &[String]) -> (String, usize) {
    let mut line = quote_arg(exe);
    let args_offset = if args.is_empty() {
        line.len()
    } else {
        line.push(' ');
        let offset = line.len();
        for (i, arg) in args.iter().enumerate() {
            if i > 0 {
                line.push(' ');
            }
            line.push_str(&quote_arg(arg));
        }
        offset
    };
    (line, args_offset)
}

/// Whether `VICTORY_SKIP_INTRO` asks to prepend `-skipToIIS` as `argv[1]`.
pub fn skip_intro_enabled() -> bool {
    std::env::var("VICTORY_SKIP_INTRO")
        .ok()
        .map(|v| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

fn effective_args(args: &[String], skip_intro: bool) -> Vec<String> {
    let mut out = Vec::with_capacity(args.len() + usize::from(skip_intro));
    if skip_intro {
        let already_first = args
            .first()
            .is_some_and(|a| a.eq_ignore_ascii_case("-skipToIIS"));
        if !already_first {
            out.push("-skipToIIS".to_owned());
        }
    }
    for arg in args {
        if skip_intro && arg.eq_ignore_ascii_case("-skipToIIS") && !out.is_empty() {
            continue;
        }
        out.push(arg.clone());
    }
    out
}

/// Installs the title's executable path (`argv[0]`) and arguments (`argv[1..]`)
/// for `GetCommandLineA/W` and the UCRT argument-vector APIs.
///
/// When `VICTORY_SKIP_INTRO=1` is set, `-skipToIIS` is placed at `argv[1]` so
/// titles that check `argv[1]` for intro-skip bypass startup logo movies while
/// leaving in-game story cutscenes untouched. Returns the effective argument
/// list (`argv[1..]`).
pub fn set_title_command_line(exe: &Path, args: &[String]) -> Vec<String> {
    set_title_command_line_with_skip(exe, args, skip_intro_enabled())
}

fn set_title_command_line_with_skip(exe: &Path, args: &[String], skip_intro: bool) -> Vec<String> {
    let effective = effective_args(args, skip_intro);
    let exe_str = std::path::absolute(exe)
        .unwrap_or_else(|_| exe.to_owned())
        .to_string_lossy()
        .into_owned();
    let (full, args_byte_offset) = format_command_line(&exe_str, &effective);
    let args_utf16_offset = full[..args_byte_offset].encode_utf16().count();

    let cmdline_a = CString::new(full.replace('\0', "")).unwrap_or_default();
    let cmdline_w: Vec<u16> = full.encode_utf16().chain([0]).collect();

    let mut all_args = Vec::with_capacity(1 + effective.len());
    all_args.push(exe_str);
    all_args.extend(effective.iter().cloned());

    let args_a: Vec<CString> = all_args
        .iter()
        .map(|s| CString::new(s.replace('\0', "")).unwrap_or_default())
        .collect();
    let args_w: Vec<Vec<u16>> = all_args
        .iter()
        .map(|s| s.encode_utf16().chain([0]).collect())
        .collect();

    let mut argv_a: Vec<*mut c_char> = args_a.iter().map(|s| s.as_ptr().cast_mut()).collect();
    argv_a.push(std::ptr::null_mut());
    let mut argv_w: Vec<*mut u16> = args_w.iter().map(|s| s.as_ptr().cast_mut()).collect();
    argv_w.push(std::ptr::null_mut());

    let cmd_a_ptr = cmdline_a.as_ptr().cast_mut().cast::<u8>();
    // SAFETY: `args_byte_offset <= cmdline_a.as_bytes().len()` (NUL-terminated).
    let winmain_a_ptr = unsafe { cmd_a_ptr.add(args_byte_offset.min(cmdline_a.as_bytes().len())) };
    let cmd_w_ptr = cmdline_w.as_ptr().cast_mut();
    // SAFETY: `args_utf16_offset < cmdline_w.len()`.
    let winmain_w_ptr = unsafe { cmd_w_ptr.add(args_utf16_offset.min(cmdline_w.len() - 1)) };
    let argv_a_ptr = argv_a.as_mut_ptr();
    let argv_w_ptr = argv_w.as_mut_ptr();
    let argc = all_args.len() as i32;

    let mut kept = KEPT.lock().unwrap_or_else(|e| e.into_inner());
    kept.push(Storage {
        _cmdline_a: cmdline_a,
        _cmdline_w: cmdline_w,
        _args_a: args_a,
        _args_w: args_w,
        _argv_a: argv_a,
        _argv_w: argv_w,
    });

    CMDLINE_A.store(cmd_a_ptr, Ordering::Release);
    CMDLINE_W.store(cmd_w_ptr, Ordering::Release);
    WINMAIN_A.store(winmain_a_ptr, Ordering::Release);
    WINMAIN_W.store(winmain_w_ptr, Ordering::Release);
    ARGV_A.store(argv_a_ptr, Ordering::Release);
    ARGV_W.store(argv_w_ptr, Ordering::Release);
    ARGC.store(argc, Ordering::Release);
    effective
}

fn ensure_init() {
    LazyLock::force(&DEFAULT_INIT);
}

pub extern "system" fn GetCommandLineA() -> *const u8 {
    ensure_init();
    CMDLINE_A.load(Ordering::Acquire).cast_const()
}

pub extern "system" fn GetCommandLineW() -> *const u16 {
    ensure_init();
    CMDLINE_W.load(Ordering::Acquire).cast_const()
}

pub extern "C" fn _get_narrow_winmain_command_line() -> *mut u8 {
    ensure_init();
    WINMAIN_A.load(Ordering::Acquire)
}

pub extern "C" fn _get_wide_winmain_command_line() -> *mut u16 {
    ensure_init();
    WINMAIN_W.load(Ordering::Acquire)
}

pub extern "C" fn _configure_narrow_argv(_mode: i32) -> i32 {
    ensure_init();
    0
}

pub extern "C" fn _configure_wide_argv(_mode: i32) -> i32 {
    ensure_init();
    0
}

pub extern "C" fn __p___argc() -> *mut i32 {
    ensure_init();
    ARGC.as_ptr()
}

pub extern "C" fn __p___argv() -> *mut *mut *mut c_char {
    ensure_init();
    ARGV_A.as_ptr()
}

pub extern "C" fn __p___wargv() -> *mut *mut *mut u16 {
    ensure_init();
    ARGV_W.as_ptr()
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "cmdline",
        dlls: &[
            "kernel32.dll",
            "kernelbase.dll",
            "api-ms-win-core-processenvironment-*",
            "api-ms-win-crt-runtime-*",
            "ucrtbase.dll",
        ],
        exports: vec![
            hle_export!(GetCommandLineA),
            hle_export!(GetCommandLineW),
            hle_export!(_get_narrow_winmain_command_line),
            hle_export!(_get_wide_winmain_command_line),
            hle_export!(_configure_narrow_argv),
            hle_export!(_configure_wide_argv),
            hle_export!(__p___argc),
            hle_export!(__p___argv),
            hle_export!(__p___wargv),
        ],
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::CStr;

    use super::*;

    fn wide_to_string(ptr: *const u16) -> String {
        // SAFETY: NUL-terminated UTF-16 buffer from this module.
        unsafe {
            let len = (0..).take_while(|&i| *ptr.add(i) != 0).count();
            String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len))
        }
    }

    static TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn title_args_reach_argc_argv_and_get_command_line() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let exe = Path::new(r"E:\Game Dir\title.exe");
        let args = vec!["-skipToIIS".to_owned(), "-flag=value".to_owned()];
        set_title_command_line(exe, &args);

        assert_eq!(_configure_narrow_argv(1), 0);
        assert_eq!(_configure_wide_argv(1), 0);

        // SAFETY: pointers published by `set_title_command_line`.
        unsafe {
            let argc = *__p___argc();
            assert_eq!(argc, 3);
            let argv = *__p___argv();
            let a0 = CStr::from_ptr(*argv).to_str().unwrap();
            let a1 = CStr::from_ptr(*argv.add(1)).to_str().unwrap();
            let a2 = CStr::from_ptr(*argv.add(2)).to_str().unwrap();
            assert!(a0.ends_with(r"Game Dir\title.exe"), "a0={a0}");
            assert_eq!(a1, "-skipToIIS");
            assert_eq!(a2, "-flag=value");
            assert!((*argv.add(3)).is_null());

            let wargv = *__p___wargv();
            assert_eq!(wide_to_string(*wargv.add(1)), "-skipToIIS");
            assert!((*wargv.add(3)).is_null());

            let cmd_a = CStr::from_ptr(GetCommandLineA().cast()).to_str().unwrap();
            assert!(
                cmd_a.starts_with('"'),
                "exe with space must be quoted: {cmd_a}"
            );
            assert!(cmd_a.ends_with(" -skipToIIS -flag=value"), "cmd_a={cmd_a}");
            assert_eq!(wide_to_string(GetCommandLineW()), cmd_a);

            let wm_a = CStr::from_ptr(_get_narrow_winmain_command_line().cast())
                .to_str()
                .unwrap();
            assert_eq!(wm_a, "-skipToIIS -flag=value");
            assert_eq!(
                wide_to_string(_get_wide_winmain_command_line()),
                "-skipToIIS -flag=value"
            );
        }
    }

    #[test]
    fn empty_title_args_yields_argc_one_and_empty_winmain_tail() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        set_title_command_line(Path::new(r"E:\Game\title.exe"), &[]);
        // SAFETY: pointers published by `set_title_command_line`.
        unsafe {
            assert_eq!(*__p___argc(), 1);
            let wm_a = CStr::from_ptr(_get_narrow_winmain_command_line().cast())
                .to_str()
                .unwrap();
            assert_eq!(wm_a, "");
            assert_eq!(wide_to_string(_get_wide_winmain_command_line()), "");
        }
    }

    #[test]
    fn skip_intro_prepends_flag_at_argv_1_without_duplicating() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let effective = set_title_command_line_with_skip(
            Path::new(r"E:\Game\title.exe"),
            &["-flag=1".to_owned()],
            true,
        );
        assert_eq!(effective, vec!["-skipToIIS", "-flag=1"]);
        // SAFETY: pointers published by `set_title_command_line_with_skip`.
        unsafe {
            assert_eq!(*__p___argc(), 3);
            let argv = *__p___argv();
            assert_eq!(CStr::from_ptr(*argv.add(1)).to_str().unwrap(), "-skipToIIS");
            assert_eq!(CStr::from_ptr(*argv.add(2)).to_str().unwrap(), "-flag=1");
        }
        // Moving an existing -skipToIIS from argv[2] to argv[1] avoids duplicates.
        let dedup = effective_args(&["-foo".to_owned(), "-skipToIIS".to_owned()], true);
        assert_eq!(dedup, vec!["-skipToIIS", "-foo"]);
    }

    #[test]
    fn registry_resolves_cmdline_apis() {
        assert!(
            crate::registry::lookup(
                "api-ms-win-core-processenvironment-l1-1-0.dll",
                "GetCommandLineA"
            )
            .is_some()
        );
        assert!(crate::registry::lookup("kernel32.dll", "GetCommandLineW").is_some());
        assert!(
            crate::registry::lookup("api-ms-win-crt-runtime-l1-1-0.dll", "__p___argc").is_some()
        );
        assert!(
            crate::registry::lookup("api-ms-win-crt-runtime-l1-1-0.dll", "__p___argv").is_some()
        );
        assert!(
            crate::registry::lookup(
                "api-ms-win-crt-runtime-l1-1-0.dll",
                "_configure_narrow_argv"
            )
            .is_some()
        );
        assert!(
            crate::registry::lookup(
                "api-ms-win-crt-runtime-l1-1-0.dll",
                "_get_narrow_winmain_command_line"
            )
            .is_some()
        );
    }
}
