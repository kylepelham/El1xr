//! Live control of the host upscale (`VICTORY_UPSCALE=stretch|dlss`): what the
//! settings registry's live hooks call to change the upscaler, the DLSS quality
//! mode and its sharpness while the title runs.
//!
//! Nothing changes until something is asked for: with no request every reader
//! keeps its startup value (`VICTORY_UPSCALE`, `VICTORY_DLSS_QUALITY`,
//! `VICTORY_DLSS_SHARPNESS`). A request lands at the next upscale the title
//! dispatches (one a frame), on the thread recording it: a DLSS feature for the
//! new quality is made on that frame's list, and the one it replaces is released
//! [`RETIRE_AFTER`] upscales later, when no list still in flight can use it.
//! The render size stays the title's (`resolution::request_ffx_scale` changes it).
//!
//! `VICTORY_UPSCALE_LIVE_TEST=<secs>:<mode>:<quality>[:<scale>],...` replays
//! requests on a timer, so a batch can test live changes without the menu.

use std::sync::atomic::{AtomicI32, AtomicU8, AtomicU32, Ordering};

/// Which upscale stands in for the title's FidelityFX dispatch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Live {
    /// The title's own `ffxDispatch` (what it does without `VICTORY_UPSCALE`).
    Native,
    Stretch,
    Dlss,
}

impl Live {
    /// `off`/`native`/`fsr`, `stretch` or `dlss`, any case.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "off" | "native" | "fsr" => Some(Self::Native),
            "stretch" => Some(Self::Stretch),
            "dlss" => Some(Self::Dlss),
            _ => None,
        }
    }

    fn code(self) -> u8 {
        match self {
            Self::Native => 1,
            Self::Stretch => 2,
            Self::Dlss => 3,
        }
    }

    fn from_code(c: u8) -> Option<Self> {
        match c {
            1 => Some(Self::Native),
            2 => Some(Self::Stretch),
            3 => Some(Self::Dlss),
            _ => None,
        }
    }
}

/// `NVSDK_NGX_PerfQuality_Value` by name (public DLSS SDK).
pub const QUALITIES: [(&str, i32); 6] = [
    ("performance", 0),
    ("balanced", 1),
    ("quality", 2),
    ("ultra_performance", 3),
    ("ultra_quality", 4),
    ("dlaa", 5),
];

/// A DLSS quality mode by name or number (0 to 5).
pub fn parse_quality(s: &str) -> Option<i32> {
    let s = s.trim().to_ascii_lowercase();
    QUALITIES
        .iter()
        .find(|(name, _)| *name == s)
        .map(|&(_, v)| v)
        .or_else(|| s.parse().ok().filter(|v| (0..=5).contains(v)))
}

/// What one live change asks for; `None` leaves that part as it is.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Request {
    pub mode: Option<Live>,
    pub quality: Option<i32>,
    pub sharpness: Option<f32>,
}

const NONE_U32: u32 = u32::MAX;
static MODE: AtomicU8 = AtomicU8::new(0);
static QUALITY: AtomicI32 = AtomicI32::new(-1);
static SHARPNESS: AtomicU32 = AtomicU32::new(NONE_U32);
/// Bumped on every change of mode, so the next upscale resets its history.
static GENERATION: AtomicU32 = AtomicU32::new(0);

/// Applies `r` from the next upscale on. Out-of-range parts are ignored.
pub fn request(r: Request) {
    if let Some(m) = r.mode
        && MODE.swap(m.code(), Ordering::Relaxed) != m.code()
    {
        GENERATION.fetch_add(1, Ordering::Relaxed);
    }
    if let Some(q) = r.quality.filter(|q| (0..=5).contains(q)) {
        QUALITY.store(q, Ordering::Relaxed);
    }
    if let Some(s) = r.sharpness.filter(|s| (0.0..=1.0).contains(s)) {
        SHARPNESS.store(s.to_bits(), Ordering::Relaxed);
    }
    tracing::info!(?r, "upscale: live change requested");
}

/// The mode asked for at runtime, if any.
pub fn mode() -> Option<Live> {
    Live::from_code(MODE.load(Ordering::Relaxed))
}

/// The DLSS quality asked for at runtime, if any.
pub fn quality() -> Option<i32> {
    Some(QUALITY.load(Ordering::Relaxed)).filter(|q| *q >= 0)
}

/// The DLSS sharpness asked for at runtime, if any.
pub fn sharpness() -> Option<f32> {
    Some(SHARPNESS.load(Ordering::Relaxed))
        .filter(|b| *b != NONE_U32)
        .map(f32::from_bits)
}

/// Changes with every mode change (for the dispatch to reset history).
pub fn generation() -> u32 {
    GENERATION.load(Ordering::Relaxed)
}

/// Whether anything was asked for at runtime (startup behaviour otherwise).
pub fn any() -> bool {
    mode().is_some() || quality().is_some() || sharpness().is_some()
}

/// Upscales a replaced DLSS feature waits before it is released: more than
/// the frames the host keeps in flight.
pub const RETIRE_AFTER: u32 = 8;

/// Things released only after [`RETIRE_AFTER`] more upscales.
pub struct Retired<H> {
    items: Vec<(H, u32)>,
}

impl<H> Default for Retired<H> {
    fn default() -> Self {
        Self::new()
    }
}

impl<H> Retired<H> {
    pub const fn new() -> Self {
        Self { items: Vec::new() }
    }

    pub fn retire(&mut self, h: H) {
        self.items.push((h, RETIRE_AFTER));
    }

    /// One upscale went by: releases what has waited long enough.
    pub fn tick(&mut self, mut release: impl FnMut(H)) {
        let mut kept = Vec::with_capacity(self.items.len());
        for (h, left) in self.items.drain(..) {
            if left <= 1 {
                release(h);
            } else {
                kept.push((h, left - 1));
            }
        }
        self.items = kept;
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

/// One timed step of `VICTORY_UPSCALE_LIVE_TEST`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Step {
    pub at_s: f32,
    pub request: Request,
    pub scale: Option<f32>,
}

/// Parses `<secs>:<mode>:<quality>[:<scale>],...`; `-` leaves a part as it is.
pub fn parse_schedule(spec: &str) -> Option<Vec<Step>> {
    spec.split(',')
        .filter(|s| !s.trim().is_empty())
        .map(|step| {
            let parts: Vec<&str> = step.split(':').map(str::trim).collect();
            if !(3..=4).contains(&parts.len()) {
                return None;
            }
            let keep = |p: &str| p == "-";
            let at_s = parts[0].parse::<f32>().ok().filter(|s| *s >= 0.0)?;
            let mode = if keep(parts[1]) {
                None
            } else {
                Some(Live::parse(parts[1])?)
            };
            let quality = if keep(parts[2]) {
                None
            } else {
                Some(parse_quality(parts[2])?)
            };
            let scale = match parts.get(3) {
                None => None,
                Some(p) if keep(p) => None,
                Some(p) => Some(p.parse::<f32>().ok()?),
            };
            Some(Step {
                at_s,
                request: Request {
                    mode,
                    quality,
                    sharpness: None,
                },
                scale,
            })
        })
        .collect()
}

/// Starts `VICTORY_UPSCALE_LIVE_TEST`'s timer, if set; `scale` applies a step's
/// render scale. Off (nothing started) without it.
pub fn spawn_test_schedule(scale: fn(f32)) {
    let Ok(spec) = std::env::var("VICTORY_UPSCALE_LIVE_TEST") else {
        return;
    };
    let Some(mut steps) = parse_schedule(&spec) else {
        tracing::warn!(
            spec,
            "VICTORY_UPSCALE_LIVE_TEST: expected <secs>:<mode>:<quality>[:<scale>],..."
        );
        return;
    };
    steps.sort_by(|a, b| a.at_s.total_cmp(&b.at_s));
    let start = std::time::Instant::now();
    let _ = std::thread::Builder::new()
        .name("upscale-live-test".into())
        .spawn(move || {
            for step in steps {
                let at = std::time::Duration::from_secs_f32(step.at_s);
                if let Some(wait) = at.checked_sub(start.elapsed()) {
                    std::thread::sleep(wait);
                }
                tracing::info!(at_s = step.at_s, ?step, "VICTORY_UPSCALE_LIVE_TEST step");
                request(step.request);
                if let Some(s) = step.scale {
                    scale(s);
                }
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_and_qualities_parse() {
        assert_eq!(Live::parse(" DLSS "), Some(Live::Dlss));
        assert_eq!(Live::parse("off"), Some(Live::Native));
        assert_eq!(Live::parse("fsr"), Some(Live::Native));
        assert_eq!(Live::parse("stretch"), Some(Live::Stretch));
        assert_eq!(Live::parse("xess"), None);
        for m in [Live::Native, Live::Stretch, Live::Dlss] {
            assert_eq!(Live::from_code(m.code()), Some(m));
        }
        assert_eq!(Live::from_code(0), None);
        assert_eq!(parse_quality("Quality"), Some(2));
        assert_eq!(parse_quality("dlaa"), Some(5));
        assert_eq!(parse_quality("0"), Some(0));
        assert_eq!(parse_quality("6"), None);
        assert_eq!(parse_quality("ultra"), None);
    }

    #[test]
    fn retired_items_wait_their_turn() {
        let mut r = Retired::new();
        r.retire(1);
        let mut released = Vec::new();
        for _ in 0..RETIRE_AFTER - 1 {
            r.tick(|h| released.push(h));
        }
        assert!(released.is_empty());
        r.retire(2);
        r.tick(|h| released.push(h));
        assert_eq!(released, [1]);
        assert_eq!(r.len(), 1);
        for _ in 0..RETIRE_AFTER {
            r.tick(|h| released.push(h));
        }
        assert_eq!(released, [1, 2]);
        assert_eq!(r.len(), 0);
    }

    #[test]
    fn schedule_parses_and_rejects() {
        let s = parse_schedule("60:dlss:performance:0.5, 120:-:quality,180:off:-:-").unwrap();
        assert_eq!(s.len(), 3);
        assert_eq!(s[0].at_s, 60.0);
        assert_eq!(s[0].request.mode, Some(Live::Dlss));
        assert_eq!(s[0].request.quality, Some(0));
        assert_eq!(s[0].scale, Some(0.5));
        assert_eq!(s[1].request.mode, None);
        assert_eq!(s[1].request.quality, Some(2));
        assert_eq!(s[1].scale, None);
        assert_eq!(s[2].request.mode, Some(Live::Native));
        assert_eq!(s[2].request.quality, None);
        assert!(parse_schedule("60:dlss").is_none());
        assert!(parse_schedule("x:dlss:quality").is_none());
        assert!(parse_schedule("60:xess:quality").is_none());
        assert!(parse_schedule("60:dlss:quality:big").is_none());
    }

    /// The only test touching the process-wide state: requests take effect,
    /// bad values are ignored, and only a mode change bumps the generation.
    #[test]
    fn requests_are_read_back() {
        let g = generation();
        request(Request {
            mode: Some(Live::Dlss),
            quality: Some(0),
            sharpness: Some(0.25),
        });
        assert!(any());
        assert_eq!(mode(), Some(Live::Dlss));
        assert_eq!(quality(), Some(0));
        assert_eq!(sharpness(), Some(0.25));
        assert_eq!(generation(), g + 1);
        request(Request {
            mode: Some(Live::Dlss),
            quality: Some(9),
            sharpness: Some(2.0),
        });
        assert_eq!(generation(), g + 1, "same mode: no new generation");
        assert_eq!(quality(), Some(0));
        assert_eq!(sharpness(), Some(0.25));
        request(Request {
            mode: Some(Live::Native),
            ..Request::default()
        });
        assert_eq!(mode(), Some(Live::Native));
        assert_eq!(generation(), g + 2);
    }
}
