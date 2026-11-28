//! (El1xr) The output resolution and the upscaler's quality mode
//! (`VICTORY_RESOLUTION`, `VICTORY_UPSCALE_QUALITY`; nothing without them):
//! what the title renders for and at, turned into the switches that already
//! do it.
//!
//! - `VICTORY_RESOLUTION=<w>x<h>|native` is the output (target) size: the
//!   title's screen size and back buffers (`VICTORY_OUTPUT`), which the swap
//!   chain follows. `native` is the primary monitor's mode, else the largest
//!   of the title's modes inside it. With none chosen,
//!   `VICTORY_NATIVE_DEFAULT=1` takes `native` (the title's 3840x2160
//!   without it).
//! - `VICTORY_UPSCALE` with `VICTORY_UPSCALE_QUALITY` gives the render size:
//!   with `stretch` or `dlss` the title's FidelityFX upscaler
//!   (`VICTORY_UPSCALER=7`) renders at the target times the mode's scale
//!   (`VICTORY_FFX_SCALE`) and DLSS gets the matching NGX mode
//!   (`VICTORY_DLSS_QUALITY`); with `off` it renders at the target
//!   (`VICTORY_RENDER_SCALE=1`, the title's own upscaler at 1.0).
//!
//! A switch the launcher or `dev.py` set in the environment wins over the
//! plan; one from the settings file gives way to it (the plan is what the menu
//! shows). The title computes its render size as `trunc(size * scale)` in
//! f32 (FidelityFX's +0x28ef680), so the scale is picked to land on an even
//! height with an even width.

use std::collections::BTreeSet;

/// Sizes the title has a display mode for (its own list, +0x615c0c0).
pub const MODES: [(u32, u32); 5] = [
    (1280, 720),
    (1664, 936),
    (1920, 1080),
    (2560, 1440),
    (3840, 2160),
];

/// The output size the title picks without a switch (Series X settings).
pub const TITLE_OUTPUT: (u32, u32) = (3840, 2160);

/// `VICTORY_RESOLUTION`'s values, as the menu steps through them.
pub const RESOLUTIONS: &[&str] = &[
    "native",
    "3840x2160",
    "2560x1440",
    "1920x1080",
    "1664x936",
    "1280x720",
];

/// `VICTORY_UPSCALE_QUALITY`'s values, best first.
pub const QUALITIES: &[&str] = &[
    "dlaa",
    "quality",
    "balanced",
    "performance",
    "ultra_performance",
];

/// A quality mode: its name for the menu, its render scale, its NGX mode
/// (`NVSDK_NGX_PerfQuality_Value`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Quality {
    pub label: &'static str,
    pub scale: f32,
    pub ngx: u32,
}

/// The mode called `name` (one of [`QUALITIES`]).
pub fn quality(name: &str) -> Option<Quality> {
    let (label, scale, ngx) = match name.trim() {
        "dlaa" => ("DLAA", 1.0, 5),
        "quality" => ("Quality", 2.0 / 3.0, 2),
        "balanced" => ("Balanced", 0.58, 1),
        "performance" => ("Performance", 0.5, 0),
        "ultra_performance" => ("Ultra performance", 1.0 / 3.0, 3),
        _ => return None,
    };
    Some(Quality { label, scale, ngx })
}

/// With it at `1`, no `VICTORY_RESOLUTION` means `native`.
pub const NATIVE_DEFAULT: &str = "VICTORY_NATIVE_DEFAULT";

/// `VICTORY_RESOLUTION` as chosen, else `native` when [`NATIVE_DEFAULT`] is
/// on; the flag says it was defaulted.
pub fn chosen_resolution(get: impl Fn(&str) -> Option<String>) -> Option<(String, bool)> {
    if let Some(r) = get("VICTORY_RESOLUTION").filter(|v| !v.trim().is_empty()) {
        return Some((r, false));
    }
    (get(NATIVE_DEFAULT).as_deref().map(str::trim) == Some("1")).then(|| ("native".to_string(), true))
}

/// The mode used when an upscaler is on and none is chosen.
pub const DEFAULT_QUALITY: &str = "quality";

/// `<w>x<h>` if it is one of [`MODES`].
pub fn parse_mode(spec: &str) -> Option<(u32, u32)> {
    let (w, h) = spec.trim().split_once(['x', 'X'])?;
    let size = (w.trim().parse().ok()?, h.trim().parse().ok()?);
    MODES.contains(&size).then_some(size)
}

/// The title's mode for a monitor of `size`: the same, else the largest
/// inside it, else the smallest.
pub fn fit_mode(size: (u32, u32)) -> (u32, u32) {
    MODES
        .iter()
        .rev()
        .copied()
        .find(|&(w, h)| w <= size.0 && h <= size.1)
        .unwrap_or(MODES[0])
}

/// `VICTORY_RESOLUTION`'s size: `native` asks `native` for the monitor's.
pub fn parse_resolution(
    spec: &str,
    native: impl FnOnce() -> Option<(u32, u32)>,
) -> Option<(u32, u32)> {
    if spec.trim().eq_ignore_ascii_case("native") {
        return native().map(fit_mode);
    }
    parse_mode(spec)
}

/// The title's render size for `target` at `scale`: f32 multiply, truncate.
pub fn title_render_size(target: (u32, u32), scale: f32) -> (u32, u32) {
    let at = |v: u32| (v as f32 * scale) as u32;
    (at(target.0), at(target.1))
}

/// The smallest f32 scale near `height / target_h` the title turns into
/// exactly `height`.
fn scale_for_height(target_h: u32, height: u32) -> f32 {
    let mut s = height as f32 / target_h as f32;
    for _ in 0..64 {
        let got = (target_h as f32 * s) as u32;
        if got == height {
            return s;
        }
        s = if got < height {
            f32::from_bits(s.to_bits() + 1)
        } else {
            f32::from_bits(s.to_bits() - 1)
        };
    }
    s
}

/// The scale to give the title for `target` and a nominal `scale`, and the
/// render size it makes: the even height nearest the nominal one whose width
/// is even too (a scale of 1 stays 1).
pub fn render_scale(target: (u32, u32), scale: f32) -> (f32, (u32, u32)) {
    if scale >= 1.0 {
        return (1.0, target);
    }
    let nominal = (target.1 as f32 * scale / 2.0).round() as i64 * 2;
    let mut best = None;
    for k in 0..=8i64 {
        for h in [nominal + 2 * k, nominal - 2 * k] {
            if h < 2 || h > target.1 as i64 {
                continue;
            }
            let s = scale_for_height(target.1, h as u32);
            let size = title_render_size(target, s);
            if size.1 == h as u32 && size.0 % 2 == 0 {
                return (s, size);
            }
            best.get_or_insert((s, size));
        }
    }
    best.unwrap_or((scale, title_render_size(target, scale)))
}

/// The switches a plan sets, and the sizes it gives.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Plan {
    /// `(name, value)` for each switch the plan decides.
    pub switches: Vec<(&'static str, String)>,
    pub output: Option<(u32, u32)>,
    pub render: Option<(u32, u32)>,
    /// No resolution was chosen: the output is the monitor's by [`NATIVE_DEFAULT`].
    pub defaulted: bool,
}

/// The menu's and the log's words for what an upscaler and quality give at
/// `target`: `Quality: renders 2560x1440 -> 3840x2160`.
pub fn describe(target: (u32, u32), upscale: &str, quality_name: Option<&str>) -> String {
    let (w, h) = target;
    match upscale {
        "stretch" | "dlss" => {
            let q = quality(quality_name.unwrap_or(DEFAULT_QUALITY))
                .unwrap_or_else(|| quality(DEFAULT_QUALITY).unwrap());
            let (_, (rw, rh)) = render_scale(target, q.scale);
            format!("{}: renders {rw}x{rh} -> {w}x{h}", q.label)
        }
        _ => format!("Upscaler off: renders {w}x{h}, the target"),
    }
}

/// The plan for these values (`get` gives a switch's value); `None` when
/// neither `VICTORY_RESOLUTION` nor `VICTORY_UPSCALE_QUALITY` is set (nor
/// [`NATIVE_DEFAULT`] with a monitor size known).
pub fn plan(
    get: impl Fn(&str) -> Option<String>,
    native: impl FnOnce() -> Option<(u32, u32)>,
) -> Option<Plan> {
    let resolution = chosen_resolution(&get);
    let quality_name = get("VICTORY_UPSCALE_QUALITY").filter(|v| !v.trim().is_empty());
    if resolution.is_none() && quality_name.is_none() {
        return None;
    }
    let defaulted = resolution.as_ref().is_some_and(|r| r.1);
    let mut plan = Plan {
        defaulted,
        ..Plan::default()
    };
    let output = resolution
        .as_ref()
        .and_then(|(r, _)| parse_resolution(r, native));
    // A defaulted resolution with no monitor size and no quality changes nothing.
    if defaulted && output.is_none() && quality_name.is_none() {
        return None;
    }
    if let Some((w, h)) = output {
        plan.switches.push(("VICTORY_OUTPUT", format!("{w}x{h}")));
    }
    plan.output = output;
    let target = output.unwrap_or(TITLE_OUTPUT);
    let upscale = get("VICTORY_UPSCALE")
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    match upscale.as_str() {
        "stretch" | "dlss" => {
            let q = quality_name
                .as_deref()
                .and_then(quality)
                .unwrap_or_else(|| quality(DEFAULT_QUALITY).unwrap());
            let (scale, render) = render_scale(target, q.scale);
            plan.switches.push(("VICTORY_UPSCALER", "7".into()));
            plan.switches
                .push(("VICTORY_FFX_SCALE", format!("{scale}")));
            if upscale == "dlss" {
                plan.switches
                    .push(("VICTORY_DLSS_QUALITY", q.ngx.to_string()));
            }
            plan.render = Some(render);
        }
        _ => {
            plan.switches.push(("VICTORY_RENDER_SCALE", "1".into()));
            plan.render = Some(target);
        }
    }
    Some(plan)
}

/// What [`derive_env`] did, for the log.
#[derive(Debug, Default)]
pub struct Derived {
    pub plan: Option<Plan>,
    /// Switches put into the environment.
    pub set: Vec<String>,
    /// Switches the environment sets, left as they are.
    pub kept: Vec<String>,
}

impl Derived {
    pub fn lines(&self) -> Vec<String> {
        let Some(plan) = &self.plan else {
            return Vec::new();
        };
        let size = |s: Option<(u32, u32)>| {
            s.map_or("the title's".to_string(), |(w, h)| format!("{w}x{h}"))
        };
        vec![format!(
            "resolution plan: output {}{}, render {} (VICTORY_RESOLUTION / VICTORY_UPSCALE_QUALITY); set {}; kept from the environment {}",
            size(plan.output),
            if plan.defaulted {
                " (this monitor's: no resolution chosen, VICTORY_NATIVE_DEFAULT=1)"
            } else {
                ""
            },
            size(plan.render),
            if self.set.is_empty() {
                "nothing".into()
            } else {
                self.set.join(", ")
            },
            if self.kept.is_empty() {
                "nothing".into()
            } else {
                self.kept.join(", ")
            },
        )]
    }
}

/// Puts the plan's switches into the environment (call it in `main` right
/// after `settings::init`, before any thread starts). A switch the
/// environment sets stays, unless it came from the settings file
/// (`from_file`). Nothing without `VICTORY_RESOLUTION` or
/// `VICTORY_UPSCALE_QUALITY` (or [`NATIVE_DEFAULT`]).
pub fn derive_env(
    from_file: &BTreeSet<String>,
    native: impl FnOnce() -> Option<(u32, u32)>,
) -> Derived {
    let Some(plan) = plan(|n| std::env::var(n).ok(), native) else {
        return Derived::default();
    };
    let mut derived = Derived::default();
    for (name, value) in &plan.switches {
        let explicit = std::env::var_os(name).is_some() && !from_file.contains(*name);
        if explicit {
            derived.kept.push(format!(
                "{name}={}",
                std::env::var(name).unwrap_or_default()
            ));
            continue;
        }
        // SAFETY: called first in `main`, before any other thread runs or
        // reads the environment (as `settings::init`).
        unsafe { std::env::set_var(name, value) };
        derived.set.push(format!("{name}={value}"));
    }
    derived.plan = Some(plan);
    derived
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        let map: BTreeMap<&str, &str> = pairs.iter().copied().collect();
        move |n| map.get(n).map(|v| v.to_string())
    }

    fn switches(p: &Plan) -> Vec<(&str, &str)> {
        p.switches.iter().map(|(n, v)| (*n, v.as_str())).collect()
    }

    #[test]
    fn unset_is_no_plan() {
        assert_eq!(plan(env(&[]), || panic!("not asked")), None);
        assert_eq!(
            plan(
                env(&[("VICTORY_UPSCALE", "dlss"), ("VICTORY_RESOLUTION", "")]),
                || None
            ),
            None
        );
    }

    #[test]
    fn target_times_mode_lands_on_even_sizes() {
        let cases: &[((u32, u32), &str, (u32, u32))] = &[
            ((3840, 2160), "dlaa", (3840, 2160)),
            ((3840, 2160), "quality", (2560, 1440)),
            ((3840, 2160), "balanced", (2222, 1250)),
            ((3840, 2160), "performance", (1920, 1080)),
            ((3840, 2160), "ultra_performance", (1280, 720)),
            ((2560, 1440), "quality", (1706, 960)),
            ((2560, 1440), "balanced", (1486, 836)),
            ((2560, 1440), "performance", (1280, 720)),
            ((2560, 1440), "ultra_performance", (856, 482)),
            ((1920, 1080), "quality", (1280, 720)),
            ((1920, 1080), "balanced", (1112, 626)),
            ((1920, 1080), "ultra_performance", (640, 360)),
        ];
        for &(target, mode, want) in cases {
            let q = quality(mode).unwrap();
            let (scale, size) = render_scale(target, q.scale);
            assert_eq!(size, want, "{target:?} {mode}");
            assert_eq!(
                title_render_size(target, scale),
                want,
                "the title's own arithmetic, {target:?} {mode}"
            );
            assert!(size.0 % 2 == 0 && size.1 % 2 == 0, "{target:?} {mode}");
            assert!((scale - q.scale).abs() < 0.01, "{target:?} {mode}: {scale}");
            // The value written into the environment reads back as the same f32.
            assert_eq!(format!("{scale}").parse::<f32>().unwrap(), scale);
        }
    }

    #[test]
    fn dlss_quality_at_1440_sets_the_existing_switches() {
        let p = plan(
            env(&[
                ("VICTORY_RESOLUTION", "2560x1440"),
                ("VICTORY_UPSCALE", "dlss"),
                ("VICTORY_UPSCALE_QUALITY", "quality"),
            ]),
            || None,
        )
        .unwrap();
        assert_eq!(
            switches(&p),
            [
                ("VICTORY_OUTPUT", "2560x1440"),
                ("VICTORY_UPSCALER", "7"),
                ("VICTORY_FFX_SCALE", "0.6666667"),
                ("VICTORY_DLSS_QUALITY", "2"),
            ]
        );
        assert_eq!(
            (p.output, p.render),
            (Some((2560, 1440)), Some((1706, 960)))
        );
        assert_eq!(
            describe((2560, 1440), "dlss", Some("quality")),
            "Quality: renders 1706x960 -> 2560x1440"
        );
        assert_eq!(
            describe((3840, 2160), "dlss", Some("quality")),
            "Quality: renders 2560x1440 -> 3840x2160"
        );
    }

    #[test]
    fn modes_map_to_ngx_and_stretch_skips_dlss() {
        let ngx: Vec<u32> = QUALITIES.iter().map(|q| quality(q).unwrap().ngx).collect();
        assert_eq!(ngx, [5, 2, 1, 0, 3]);
        let p = plan(
            env(&[
                ("VICTORY_UPSCALE", "stretch"),
                ("VICTORY_UPSCALE_QUALITY", "performance"),
            ]),
            || None,
        )
        .unwrap();
        assert_eq!(
            switches(&p),
            [("VICTORY_UPSCALER", "7"), ("VICTORY_FFX_SCALE", "0.5")]
        );
        assert_eq!(
            p.render,
            Some((1920, 1080)),
            "the title's 4K without a resolution"
        );
        // An upscaler with no mode chosen takes Quality.
        let p = plan(
            env(&[
                ("VICTORY_RESOLUTION", "3840x2160"),
                ("VICTORY_UPSCALE", "dlss"),
            ]),
            || None,
        )
        .unwrap();
        assert_eq!(p.render, Some((2560, 1440)));
    }

    #[test]
    fn upscaler_off_renders_at_the_target() {
        let p = plan(env(&[("VICTORY_RESOLUTION", "1920x1080")]), || None).unwrap();
        assert_eq!(
            switches(&p),
            [
                ("VICTORY_OUTPUT", "1920x1080"),
                ("VICTORY_RENDER_SCALE", "1")
            ]
        );
        assert_eq!(p.render, Some((1920, 1080)));
        assert_eq!(
            describe((1920, 1080), "off", Some("quality")),
            "Upscaler off: renders 1920x1080, the target"
        );
    }

    #[test]
    fn native_is_the_monitors_mode_or_the_largest_inside_it() {
        assert_eq!(
            parse_resolution("native", || Some((2560, 1440))),
            Some((2560, 1440))
        );
        assert_eq!(
            parse_resolution("Native", || Some((3440, 1440))),
            Some((2560, 1440))
        );
        assert_eq!(
            parse_resolution("native", || Some((7680, 4320))),
            Some((3840, 2160))
        );
        assert_eq!(
            parse_resolution("native", || Some((1024, 768))),
            Some((1280, 720))
        );
        assert_eq!(parse_resolution("native", || None), None);
        assert_eq!(parse_resolution("1600x900", || None), None);
        for r in RESOLUTIONS.iter().filter(|r| **r != "native") {
            assert!(parse_resolution(r, || None).is_some(), "{r}");
        }
        // An unknown size leaves the output alone but still plans the render.
        let p = plan(env(&[("VICTORY_RESOLUTION", "native")]), || None).unwrap();
        assert_eq!(p.output, None);
        assert_eq!(switches(&p), [("VICTORY_RENDER_SCALE", "1")]);
    }

    /// Kyle: with nothing chosen the target is the primary monitor's (behind
    /// `VICTORY_NATIVE_DEFAULT=1`), and DLSS's mode sizes the render from it.
    #[test]
    fn no_resolution_chosen_takes_the_monitor_with_native_default() {
        let p = plan(env(&[("VICTORY_NATIVE_DEFAULT", "1")]), || Some((2560, 1440))).unwrap();
        assert!(p.defaulted);
        assert_eq!(
            switches(&p),
            [("VICTORY_OUTPUT", "2560x1440"), ("VICTORY_RENDER_SCALE", "1")]
        );
        let p = plan(
            env(&[("VICTORY_NATIVE_DEFAULT", "1"), ("VICTORY_UPSCALE", "dlss")]),
            || Some((2560, 1440)),
        )
        .unwrap();
        assert_eq!(
            switches(&p),
            [
                ("VICTORY_OUTPUT", "2560x1440"),
                ("VICTORY_UPSCALER", "7"),
                ("VICTORY_FFX_SCALE", "0.6666667"),
                ("VICTORY_DLSS_QUALITY", "2"),
            ]
        );
        assert_eq!((p.output, p.render), (Some((2560, 1440)), Some((1706, 960))));
        let p = plan(
            env(&[
                ("VICTORY_NATIVE_DEFAULT", "1"),
                ("VICTORY_UPSCALE", "dlss"),
                ("VICTORY_UPSCALE_QUALITY", "performance"),
            ]),
            || Some((3440, 1440)),
        )
        .unwrap();
        assert_eq!((p.output, p.render), (Some((2560, 1440)), Some((1280, 720))));
        // A chosen resolution wins and is not "defaulted".
        let p = plan(
            env(&[("VICTORY_NATIVE_DEFAULT", "1"), ("VICTORY_RESOLUTION", "1920x1080")]),
            || panic!("not asked"),
        )
        .unwrap();
        assert_eq!((p.output, p.defaulted), (Some((1920, 1080)), false));
        // Off, or no monitor size: today's (the title's 3840x2160, nothing set).
        assert_eq!(plan(env(&[("VICTORY_NATIVE_DEFAULT", "0")]), || Some((2560, 1440))), None);
        assert_eq!(plan(env(&[]), || Some((2560, 1440))), None);
        assert_eq!(plan(env(&[("VICTORY_NATIVE_DEFAULT", "1")]), || None), None);
        let d = Derived {
            plan: plan(env(&[("VICTORY_NATIVE_DEFAULT", "1")]), || Some((2560, 1440))),
            ..Derived::default()
        };
        assert!(d.lines()[0].starts_with("resolution plan: output 2560x1440 (this monitor's"), "{:?}", d.lines());
    }

    /// `derive_env` touches the process environment: names of its own.
    #[test]
    fn the_environment_wins_but_the_file_gives_way() {
        let names = [
            "VICTORY_NATIVE_DEFAULT",
            "VICTORY_RESOLUTION",
            "VICTORY_UPSCALE",
            "VICTORY_UPSCALE_QUALITY",
            "VICTORY_OUTPUT",
            "VICTORY_UPSCALER",
            "VICTORY_FFX_SCALE",
            "VICTORY_DLSS_QUALITY",
            "VICTORY_RENDER_SCALE",
        ];
        let saved: Vec<_> = names.iter().map(|n| (n, std::env::var_os(n))).collect();
        // SAFETY: test-only; no other test reads these.
        unsafe {
            for n in names {
                std::env::remove_var(n);
            }
            assert!(
                derive_env(&BTreeSet::new(), || None).lines().is_empty(),
                "unset: nothing"
            );
            assert!(names.iter().all(|n| std::env::var_os(n).is_none()));
            std::env::set_var("VICTORY_UPSCALE", "dlss");
            std::env::set_var("VICTORY_UPSCALE_QUALITY", "performance");
            std::env::set_var("VICTORY_FFX_SCALE", "0.75"); // the launcher's
            std::env::set_var("VICTORY_DLSS_QUALITY", "5"); // the settings file's
            let from_file = BTreeSet::from(["VICTORY_DLSS_QUALITY".to_string()]);
            let d = derive_env(&from_file, || None);
            assert_eq!(std::env::var("VICTORY_FFX_SCALE").unwrap(), "0.75");
            assert_eq!(std::env::var("VICTORY_DLSS_QUALITY").unwrap(), "0");
            assert_eq!(std::env::var("VICTORY_UPSCALER").unwrap(), "7");
            assert_eq!(d.kept, ["VICTORY_FFX_SCALE=0.75"]);
            assert_eq!(d.set, ["VICTORY_UPSCALER=7", "VICTORY_DLSS_QUALITY=0"]);
            assert!(d.lines()[0].contains("render 1920x1080"), "{:?}", d.lines());
            for (n, v) in saved {
                match v {
                    Some(v) => std::env::set_var(n, v),
                    None => std::env::remove_var(n),
                }
            }
        }
    }
}
