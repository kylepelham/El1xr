//! Opt-in process audio mute (`VICTORY_MUTE=1` or `VICTORY_MUTE=true`).
//!
//! Uses Windows Core Audio's session manager (`IMMDeviceEnumerator` +
//! `ISimpleAudioVolume`) to mute only audio sessions belonging to this
//! process, without creating an `IAudioClient` stream so timer resolution and
//! XAudio2/game audio timing stay untouched.

#[cfg(windows)]
use std::sync::Mutex;
#[cfg(windows)]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(windows)]
use std::thread::JoinHandle;
#[cfg(windows)]
use std::time::Duration;

#[cfg(windows)]
static STARTED: AtomicBool = AtomicBool::new(false);
#[cfg(windows)]
static STOP: AtomicBool = AtomicBool::new(false);
#[cfg(windows)]
static HANDLE: Mutex<Option<JoinHandle<()>>> = Mutex::new(None);

/// Whether a `VICTORY_MUTE` value enables muting (`"1"` or `"true"`, case-insensitive).
pub fn is_enabled_value(value: Option<&str>) -> bool {
    value.is_some_and(|v| {
        let trimmed = v.trim();
        trimmed == "1" || trimmed.eq_ignore_ascii_case("true")
    })
}

fn is_enabled() -> bool {
    is_enabled_value(std::env::var("VICTORY_MUTE").ok().as_deref())
}

/// Starts the background mute daemon if `VICTORY_MUTE` is enabled.
/// Safe to call more than once; spawns at most one daemon thread.
pub fn start() {
    if !is_enabled() {
        return;
    }
    #[cfg(windows)]
    {
        if STARTED.swap(true, Ordering::AcqRel) {
            return;
        }
        STOP.store(false, Ordering::Release);
        match std::thread::Builder::new()
            .name("victory-mute".into())
            .spawn(|| run_daemon_until(&STOP))
        {
            Ok(handle) => {
                *HANDLE.lock().unwrap_or_else(|e| e.into_inner()) = Some(handle);
            }
            Err(e) => {
                STARTED.store(false, Ordering::Release);
                tracing::warn!("audio mute thread not started: {e}");
            }
        }
    }
}

/// Signals the background mute daemon to stop, releases COM, and waits for it to exit.
/// Safe to call even if `start` was not called or `VICTORY_MUTE` was not enabled.
pub fn stop() {
    #[cfg(windows)]
    {
        STOP.store(true, Ordering::Release);
        let handle = HANDLE.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(handle) = handle {
            handle.thread().unpark();
            let _ = handle.join();
            STARTED.store(false, Ordering::Release);
        }
    }
}

/// RAII guard for a thread's COM apartment: declared before any COM interface
/// so all interfaces drop before `CoUninitialize()` runs.
#[cfg(windows)]
struct ComGuard;

#[cfg(windows)]
impl ComGuard {
    fn init_mta() -> windows::core::Result<Self> {
        use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
        // SAFETY: initializes COM (MTA) on the calling thread; both `S_OK` and
        // `S_FALSE` succeed via `.ok()` and require a matching `CoUninitialize`.
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.ok()?;
        Ok(Self)
    }
}

#[cfg(windows)]
impl Drop for ComGuard {
    fn drop(&mut self) {
        use windows::Win32::System::Com::CoUninitialize;
        // SAFETY: matches the successful `CoInitializeEx` in `ComGuard::init_mta`.
        unsafe { CoUninitialize() };
    }
}

#[cfg(windows)]
fn create_enumerator() -> windows::core::Result<windows::Win32::Media::Audio::IMMDeviceEnumerator> {
    use windows::Win32::Media::Audio::MMDeviceEnumerator;
    use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance};
    // SAFETY: COM is initialized on this thread; standard Core Audio CLSID.
    unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
}

#[cfg(windows)]
fn run_daemon_until(stop: &AtomicBool) {
    let _com = match ComGuard::init_mta() {
        Ok(guard) => guard,
        Err(e) => {
            tracing::warn!("audio mute COM init failed: {e}");
            return;
        }
    };

    let pid = std::process::id();
    let mut enumerator = match create_enumerator() {
        Ok(e) => Some(e),
        Err(e) => {
            tracing::warn!("audio mute device enumerator failed: {e}");
            return;
        }
    };

    let mut logged = false;
    let mut muted_once = false;
    while !stop.load(Ordering::Acquire) {
        if enumerator.is_none() {
            match create_enumerator() {
                Ok(e) => enumerator = Some(e),
                Err(e) => {
                    tracing::debug!("audio mute enumerator recreate failed: {e}");
                }
            }
        }
        if let Some(ref current) = enumerator {
            match mute_process_sessions(current, pid) {
                Ok(true) => {
                    muted_once = true;
                    if !logged {
                        tracing::info!("audio session muted (VICTORY_MUTE=1)");
                        logged = true;
                    }
                }
                Ok(false) => {}
                Err(e) => {
                    tracing::debug!("audio mute poll failed (will recreate enumerator): {e}");
                    // Drop the potentially stale COM proxy (e.g. audio service restart).
                    enumerator = None;
                }
            }
        }
        if stop.load(Ordering::Acquire) {
            break;
        }
        let interval = if muted_once {
            Duration::from_secs(1)
        } else {
            Duration::from_millis(100)
        };
        std::thread::park_timeout(interval);
    }
    // Drop `enumerator` explicitly before `_com` runs `CoUninitialize`.
    drop(enumerator);
}

#[cfg(windows)]
fn mute_process_sessions(
    enumerator: &windows::Win32::Media::Audio::IMMDeviceEnumerator,
    pid: u32,
) -> windows::core::Result<bool> {
    use windows::Win32::Media::Audio::{
        DEVICE_STATE_ACTIVE, IAudioSessionControl2, IAudioSessionEnumerator, IAudioSessionManager2,
        ISimpleAudioVolume, eRender,
    };
    use windows::Win32::System::Com::CLSCTX_ALL;
    use windows::core::Interface;

    // SAFETY: COM calls on interfaces owned by this MTA thread.
    unsafe {
        let devices = enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)?;
        let device_count = devices.GetCount()?;
        let mut muted_any = false;
        for d in 0..device_count {
            let Ok(device) = devices.Item(d) else {
                continue;
            };
            let Ok(manager) = device.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None) else {
                continue;
            };
            let Ok(sessions): windows::core::Result<IAudioSessionEnumerator> =
                manager.GetSessionEnumerator()
            else {
                continue;
            };
            let Ok(session_count) = sessions.GetCount() else {
                continue;
            };
            for s in 0..session_count {
                let Ok(control) = sessions.GetSession(s) else {
                    continue;
                };
                let Ok(control2) = control.cast::<IAudioSessionControl2>() else {
                    continue;
                };
                if control2.GetProcessId().ok() != Some(pid) {
                    continue;
                }
                let Ok(volume) = control.cast::<ISimpleAudioVolume>() else {
                    continue;
                };
                let already_muted = volume.GetMute().map(|b| b.as_bool()).unwrap_or(false);
                if already_muted || volume.SetMute(true, std::ptr::null()).is_ok() {
                    muted_any = true;
                }
            }
        }
        Ok(muted_any)
    }
}

#[cfg(test)]
mod tests {
    use super::is_enabled_value;

    #[test]
    fn env_parsing_accepts_one_and_true_case_insensitively() {
        assert!(is_enabled_value(Some("1")));
        assert!(is_enabled_value(Some(" 1 ")));
        assert!(is_enabled_value(Some("true")));
        assert!(is_enabled_value(Some("TRUE")));
        assert!(is_enabled_value(Some("True")));
        assert!(is_enabled_value(Some(" tRuE ")));

        assert!(!is_enabled_value(None));
        assert!(!is_enabled_value(Some("")));
        assert!(!is_enabled_value(Some("0")));
        assert!(!is_enabled_value(Some("false")));
        assert!(!is_enabled_value(Some("FALSE")));
        assert!(!is_enabled_value(Some("yes")));
        assert!(!is_enabled_value(Some("2")));
    }

    #[cfg(windows)]
    #[test]
    fn enum_audio_endpoints_scans_all_active_render_devices_cleanly() {
        use super::{ComGuard, create_enumerator, mute_process_sessions};
        // Run on a dedicated thread so COM MTA init does not conflict with any
        // STA apartment that another test might set on the test runner thread.
        let handle = std::thread::spawn(|| {
            let _com = ComGuard::init_mta().expect("COM MTA init should succeed");
            let enumerator = create_enumerator().expect("MMDeviceEnumerator should create");
            let muted = mute_process_sessions(&enumerator, std::process::id())
                .expect("EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE) should succeed");
            // This unit test process creates no audio client, so no session belongs to it.
            assert!(!muted);
            drop(enumerator);
        });
        handle.join().expect("mute endpoint scan thread panicked");
    }

    #[cfg(windows)]
    #[test]
    fn daemon_wakes_and_exits_cleanly_when_stopped() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::{Duration, Instant};

        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = Arc::clone(&stop);
        let handle = std::thread::spawn(move || super::run_daemon_until(&stop_flag));
        // Give the daemon time to complete its first poll and park.
        std::thread::sleep(Duration::from_millis(50));
        let start = Instant::now();
        stop.store(true, Ordering::Release);
        handle.thread().unpark();
        handle.join().expect("mute daemon panicked on shutdown");
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "daemon should wake promptly on stop, took {:?}",
            start.elapsed()
        );
    }
}
