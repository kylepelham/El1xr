//! Opt-in process audio mute (`VICTORY_MUTE=1` or `VICTORY_MUTE=true`).
//!
//! Uses Windows Core Audio's session manager (`IMMDeviceEnumerator` +
//! `ISimpleAudioVolume`) to mute only audio sessions belonging to this
//! process, without creating an `IAudioClient` stream so timer resolution and
//! XAudio2/game audio timing stay untouched.

#[cfg(windows)]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(windows)]
use std::time::Duration;

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
        static STARTED: AtomicBool = AtomicBool::new(false);
        if STARTED.swap(true, Ordering::AcqRel) {
            return;
        }
        if let Err(e) = std::thread::Builder::new()
            .name("victory-mute".into())
            .spawn(run_daemon)
        {
            tracing::warn!("audio mute thread not started: {e}");
        }
    }
}

#[cfg(windows)]
fn run_daemon() {
    use windows::Win32::Media::Audio::{IMMDeviceEnumerator, MMDeviceEnumerator};
    use windows::Win32::System::Com::{
        CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
    };

    let pid = std::process::id();
    // SAFETY: initializes COM (MTA) on this dedicated daemon thread.
    let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    if let Err(e) = hr.ok() {
        tracing::warn!("audio mute COM init failed: {e}");
        return;
    }

    // SAFETY: COM is initialized on this thread; standard Core Audio CLSID.
    let enumerator: IMMDeviceEnumerator =
        match unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) } {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!("audio mute device enumerator failed: {e}");
                // SAFETY: matches successful `CoInitializeEx` above.
                unsafe { CoUninitialize() };
                return;
            }
        };

    let mut logged = false;
    let mut muted_once = false;
    loop {
        match mute_process_sessions(&enumerator, pid) {
            Ok(true) => {
                muted_once = true;
                if !logged {
                    tracing::info!("audio session muted (VICTORY_MUTE=1)");
                    logged = true;
                }
            }
            Ok(false) => {}
            Err(e) => {
                tracing::debug!("audio mute poll failed: {e}");
            }
        }
        let interval = if muted_once {
            Duration::from_secs(1)
        } else {
            Duration::from_millis(100)
        };
        std::thread::sleep(interval);
    }
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
}
