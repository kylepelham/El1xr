//! (El1xr) The Display tab's resolution rows: the primary monitor's size for
//! `VICTORY_RESOLUTION=native`, and the line under Resolution, Upscaler and
//! Quality saying what the next start renders at
//! (`victory_core::render_plan`); Ray tracing's line in plain words.

use victory_core::render_plan::{self, TITLE_OUTPUT};

/// The primary monitor's mode in pixels (whatever the process's DPI
/// awareness); `None` if Windows does not say.
#[cfg(windows)]
pub fn primary_display_size() -> Option<(u32, u32)> {
    use windows_sys::Win32::Graphics::Gdi::{
        DEVMODEW, ENUM_CURRENT_SETTINGS, EnumDisplaySettingsW,
    };
    // SAFETY: a zeroed DEVMODEW with its size set, as the call asks.
    unsafe {
        let mut mode: DEVMODEW = std::mem::zeroed();
        mode.dmSize = std::mem::size_of::<DEVMODEW>() as u16;
        (EnumDisplaySettingsW(std::ptr::null(), ENUM_CURRENT_SETTINGS, &mut mode) != 0
            && mode.dmPelsWidth > 0
            && mode.dmPelsHeight > 0)
            .then_some((mode.dmPelsWidth, mode.dmPelsHeight))
    }
}

#[cfg(not(windows))]
pub fn primary_display_size() -> Option<(u32, u32)> {
    None
}

/// The line under `name`'s row for the values `get` gives (`None`: the
/// row's help).
pub fn detail(
    name: &str,
    get: impl Fn(&str) -> Option<String>,
    native: impl Fn() -> Option<(u32, u32)>,
) -> Option<String> {
    if name == victory_core::settings::ray_tracing::NAME {
        return Some(ray_tracing_line(get(name).as_deref()).to_string());
    }
    if !matches!(
        name,
        "VICTORY_RESOLUTION" | "VICTORY_UPSCALE" | "VICTORY_UPSCALE_QUALITY"
    ) {
        return None;
    }
    let plan = render_plan::plan(&get, &native);
    let target = plan
        .as_ref()
        .and_then(|p| p.output)
        .or_else(|| {
            get("VICTORY_OUTPUT")
                .as_deref()
                .and_then(render_plan::parse_mode)
        })
        .unwrap_or(TITLE_OUTPUT);
    let (w, h) = target;
    if name == "VICTORY_RESOLUTION" {
        let chosen = get(name).unwrap_or_default();
        if chosen.trim().is_empty() && plan.as_ref().is_some_and(|p| p.defaulted) {
            let monitor = native().map_or(String::new(), |(mw, mh)| format!(" {mw}x{mh}"));
            return Some(format!(
                "Not chosen: this monitor's{monitor}, output {w}x{h}; a choice is kept for the next start"
            ));
        }
        return Some(if chosen.trim().eq_ignore_ascii_case("native") {
            match native() {
                Some((mw, mh)) => format!(
                    "Native: output {w}x{h} (this monitor {mw}x{mh}); the swap chain follows"
                ),
                None => format!("Native: the monitor's size is unknown, output {w}x{h}"),
            }
        } else if chosen.is_empty() {
            format!("Not chosen: the title's {w}x{h}; a choice is kept for the next start")
        } else {
            format!("Output {w}x{h}; the swap chain follows")
        });
    }
    let upscale = get("VICTORY_UPSCALE")
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    if plan.is_none() {
        // Today's: the title's own upscale at its 2/3 (or `VICTORY_RENDER_SCALE`).
        let own = matches!(upscale.as_str(), "" | "off")
            && get("VICTORY_UPSCALER").is_none_or(|u| u.trim() != "7");
        if name != "VICTORY_UPSCALE" || !own {
            return None;
        }
        let scale = get("VICTORY_RENDER_SCALE")
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(2.0 / 3.0);
        let (rw, rh) = render_plan::title_render_size(target, scale);
        return Some(format!("Off: the title's own upscale renders {rw}x{rh} -> {w}x{h}"));
    }
    let quality = get("VICTORY_UPSCALE_QUALITY");
    Some(render_plan::describe(
        target,
        &upscale,
        quality.as_deref().filter(|q| !q.is_empty()),
    ))
}

/// Ray tracing's line for its value.
fn ray_tracing_line(value: Option<&str>) -> &'static str {
    if victory_core::settings::ray_tracing::is_on(value) {
        "On: glass, puddles and car paint reflect the scene through ray tracing; costs frame rate"
    } else {
        "Off: reflections come from the ambient stand-in; On traces glass, puddles and car paint"
    }
}

/// The value an unset `name` stands for: Resolution's output size, and Quality's mode while an
/// upscaler is on.
pub fn effective(
    name: &str,
    get: impl Fn(&str) -> Option<String>,
    native: impl Fn() -> Option<(u32, u32)>,
) -> Option<String> {
    if get(name).is_some_and(|v| !v.trim().is_empty()) {
        return None;
    }
    match name {
        "VICTORY_RESOLUTION" => {
            let plan = render_plan::plan(&get, &native);
            let (w, h) = plan
                .and_then(|p| p.output)
                .or_else(|| get("VICTORY_OUTPUT").as_deref().and_then(render_plan::parse_mode))
                .unwrap_or(TITLE_OUTPUT);
            Some(format!("{w}x{h}"))
        }
        "VICTORY_UPSCALE_QUALITY" => {
            let upscale = get("VICTORY_UPSCALE").unwrap_or_default();
            matches!(upscale.trim(), "stretch" | "dlss").then(|| render_plan::DEFAULT_QUALITY.to_string())
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn with(pairs: &[(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        let map: BTreeMap<&str, &str> = pairs.iter().copied().collect();
        move |n| map.get(n).map(|v| v.to_string())
    }

    const MONITOR: fn() -> Option<(u32, u32)> = || Some((2560, 1440));

    #[test]
    fn rows_say_what_the_next_start_renders() {
        let v = with(&[
            ("VICTORY_RESOLUTION", "native"),
            ("VICTORY_UPSCALE", "dlss"),
            ("VICTORY_UPSCALE_QUALITY", "quality"),
        ]);
        assert_eq!(
            detail("VICTORY_UPSCALE_QUALITY", &v, MONITOR).unwrap(),
            "Quality: renders 1706x960 -> 2560x1440"
        );
        assert_eq!(
            detail("VICTORY_UPSCALE", &v, MONITOR).unwrap(),
            "Quality: renders 1706x960 -> 2560x1440"
        );
        assert_eq!(
            detail("VICTORY_RESOLUTION", &v, MONITOR).unwrap(),
            "Native: output 2560x1440 (this monitor 2560x1440); the swap chain follows"
        );
        let v = with(&[
            ("VICTORY_RESOLUTION", "3840x2160"),
            ("VICTORY_UPSCALE", "dlss"),
            ("VICTORY_UPSCALE_QUALITY", "performance"),
        ]);
        assert_eq!(
            detail("VICTORY_UPSCALE_QUALITY", &v, MONITOR).unwrap(),
            "Performance: renders 1920x1080 -> 3840x2160"
        );
        let v = with(&[
            ("VICTORY_RESOLUTION", "2560x1440"),
            ("VICTORY_UPSCALE", "off"),
        ]);
        assert_eq!(
            detail("VICTORY_UPSCALE", &v, MONITOR).unwrap(),
            "Upscaler off: renders 2560x1440, the target"
        );
        assert_eq!(detail("VICTORY_FFX_SCALE", &v, MONITOR), None);
    }

    #[test]
    fn without_a_plan_the_rows_say_todays_sizes() {
        let v = with(&[]);
        assert_eq!(
            detail("VICTORY_UPSCALE", &v, MONITOR).unwrap(),
            "Off: the title's own upscale renders 2560x1440 -> 3840x2160"
        );
        let v = with(&[("VICTORY_OUTPUT", "1920x1080"), ("VICTORY_RENDER_SCALE", "1")]);
        assert_eq!(
            detail("VICTORY_UPSCALE", &v, MONITOR).unwrap(),
            "Off: the title's own upscale renders 1920x1080 -> 1920x1080"
        );
        let v = with(&[("VICTORY_UPSCALE", "dlss")]);
        assert_eq!(detail("VICTORY_UPSCALE", &v, MONITOR), None);
        assert_eq!(detail("VICTORY_UPSCALE_QUALITY", &v, MONITOR), None);
        assert_eq!(
            detail("VICTORY_RESOLUTION", &v, MONITOR).unwrap(),
            "Not chosen: the title's 3840x2160; a choice is kept for the next start"
        );
    }

    /// Unset values read as what they stand for: the title's 4K, the monitor's size with
    /// `VICTORY_NATIVE_DEFAULT=1`, DLSS's Quality while it is on; a chosen value is its own.
    #[test]
    fn unset_rows_read_as_the_size_and_mode_they_give() {
        assert_eq!(effective("VICTORY_RESOLUTION", with(&[]), MONITOR).as_deref(), Some("3840x2160"));
        let auto = with(&[("VICTORY_NATIVE_DEFAULT", "1"), ("VICTORY_UPSCALE", "dlss")]);
        assert_eq!(effective("VICTORY_RESOLUTION", &auto, MONITOR).as_deref(), Some("2560x1440"));
        assert_eq!(effective("VICTORY_UPSCALE_QUALITY", &auto, MONITOR).as_deref(), Some("quality"));
        assert_eq!(
            detail("VICTORY_RESOLUTION", &auto, MONITOR).unwrap(),
            "Not chosen: this monitor's 2560x1440, output 2560x1440; a choice is kept for the next start"
        );
        assert_eq!(
            detail("VICTORY_UPSCALE_QUALITY", &auto, MONITOR).unwrap(),
            "Quality: renders 1706x960 -> 2560x1440"
        );
        let chosen = with(&[("VICTORY_NATIVE_DEFAULT", "1"), ("VICTORY_RESOLUTION", "1920x1080")]);
        assert_eq!(effective("VICTORY_RESOLUTION", &chosen, MONITOR), None);
        assert_eq!(effective("VICTORY_UPSCALE_QUALITY", with(&[("VICTORY_UPSCALE", "off")]), MONITOR), None);
        assert_eq!(effective("VICTORY_OVERLAY", with(&[]), MONITOR), None);
    }

    #[test]
    fn ray_tracing_reads_in_plain_words() {
        let off = detail("VICTORY_RAY_TRACING", with(&[]), MONITOR).unwrap();
        assert!(off.starts_with("Off: "), "{off}");
        let on = detail("VICTORY_RAY_TRACING", with(&[("VICTORY_RAY_TRACING", "on")]), MONITOR).unwrap();
        assert!(on.starts_with("On: "), "{on}");
        assert!(!format!("{off}{on}").contains("VICTORY_"));
    }

    #[cfg(windows)]
    #[test]
    fn the_primary_monitor_has_a_size() {
        let (w, h) = primary_display_size().expect("a desktop session");
        assert!(w >= 640 && h >= 480, "{w}x{h}");
    }
}
