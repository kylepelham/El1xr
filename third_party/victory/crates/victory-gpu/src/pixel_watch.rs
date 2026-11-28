//! `VICTORY_PIXEL_WATCH=<x>,<y>[,<every_n>][;<x2>,<y2>...]` and
//! `VICTORY_PIXEL_WATCH_FRAMES=<start>..<end>|<start>s..<end>s` (diagnostic, live, off by default):
//! watches one to four screen pixels across every screen-sized render-target, depth/stencil,
//! compute UAV and present pass on watched frames.
//!
//! Each watched pixel (4K `3840x2160` coordinates or normalized `0..1` coordinates) is mapped
//! proportionally to every 16:9 screen-sized target (`w >= 160`, `h >= 90`, within 5% of 16:9).
//! On watched frames, a 1x1 texel copy per target/pass is batched into a single pooled 64 KiB
//! read-back buffer per command list and read back once the list has landed. The first watched
//! frame logs every pass (`pixel watch: first`); subsequent watched frames diff each logical
//! pass against the previous watched frame and log `pixel watch: CHANGE` whenever a texel value
//! (or stencil) changes significantly, together with the draws/dispatches that covered the pixel,
//! their pixel/compute shader hashes and names, and their bound texture slots, formats and host
//! descriptor states.

use std::collections::{HashMap, VecDeque};
use std::fmt::Write as _;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// Reference coordinate width/height when non-normalized pixel coords are given.
pub(crate) const REF_WIDTH: f64 = 3840.0;
pub(crate) const REF_HEIGHT: f64 = 2160.0;
/// Maximum simultaneous watched pixels (separated by `;`).
pub(crate) const MAX_POINTS: usize = 4;
/// Bytes per 1x1 texel slot in a batched read-back buffer (D3D12 placement alignment).
pub(crate) const SLOT_STRIDE: u64 = 512;
/// Slots per 64 KiB pooled read-back buffer.
pub(crate) const BATCH_SLOTS: u32 = 128;
pub(crate) const BATCH_BYTES: u64 = BATCH_SLOTS as u64 * SLOT_STRIDE;
/// Cap on `CHANGE` log lines per run so an animated pixel cannot flood the log.
const MAX_CHANGE_LOGS: u64 = 4000;
/// Cap on `CHANGE` log lines per frame.
const MAX_CHANGES_PER_FRAME: u32 = 256;
/// Heartbeat interval (in watched frames) when nothing changes.
const HEARTBEAT_EVERY: u64 = 30;
/// Maximum pooled 64 KiB read-back buffers kept at rest.
#[cfg(windows)]
const MAX_POOLED_BUFFERS: usize = 32;

/// One watched screen pixel.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct WatchPoint {
    /// Normalized `[0, 1]` coordinates (for 4K integer coords `(x, y)`, `(x + 0.5) / 3840`).
    pub(crate) u: f64,
    pub(crate) v: f64,
    /// Original 4K-equivalent pixel coords for readable labels.
    pub(crate) x4k: u16,
    pub(crate) y4k: u16,
    pub(crate) normalized: bool,
}

impl WatchPoint {
    pub(crate) fn label(&self) -> String {
        if self.normalized {
            format!("({:.4},{:.4})", self.u, self.v)
        } else {
            format!("({},{})", self.x4k, self.y4k)
        }
    }
}

/// Parsed live configuration of `VICTORY_PIXEL_WATCH`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Config {
    pub(crate) points: [WatchPoint; MAX_POINTS],
    pub(crate) count: usize,
    pub(crate) every: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            points: [WatchPoint::default(); MAX_POINTS],
            count: 0,
            every: 0,
        }
    }
}

impl Config {
    pub(crate) fn active_points(&self) -> &[WatchPoint] {
        &self.points[..self.count]
    }
}

/// Mode of `VICTORY_PIXEL_WATCH_FRAMES`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Window {
    All,
    Frames { start: u64, end: u64 },
    Seconds { start: u64, end: u64 },
}

const WIN_FRAMES: u64 = 1;
const WIN_SECONDS: u64 = 2;
const WIN_MAX: u64 = (1 << 31) - 1;

fn pack_window(w: Window) -> u64 {
    match w {
        Window::All => 0,
        Window::Frames { start, end } => {
            (WIN_FRAMES << 62) | ((start.min(WIN_MAX)) << 31) | end.min(WIN_MAX)
        }
        Window::Seconds { start, end } => {
            (WIN_SECONDS << 62) | ((start.min(WIN_MAX)) << 31) | end.min(WIN_MAX)
        }
    }
}

fn unpack_window(bits: u64) -> Window {
    let mode = bits >> 62;
    let start = (bits >> 31) & WIN_MAX;
    let end = bits & WIN_MAX;
    match mode {
        WIN_FRAMES => Window::Frames { start, end },
        WIN_SECONDS => Window::Seconds { start, end },
        _ => Window::All,
    }
}

/// Parses `VICTORY_PIXEL_WATCH_FRAMES`: `<start>..<end>`, `<start>-<end>`, `<start>..`,
/// `<start>s..<end>s`, `<start>s-<end>s`, or `<start>s..`.
pub(crate) fn parse_window(value: Option<&str>) -> Window {
    let Some(raw) = value.map(str::trim) else {
        return Window::All;
    };
    if raw.is_empty() || raw == "0" {
        return Window::All;
    }
    let (lhs, rhs) = if let Some((a, b)) = raw.split_once("..") {
        (a.trim(), b.trim())
    } else if let Some((a, b)) = raw.split_once('-') {
        (a.trim(), b.trim())
    } else {
        return Window::All;
    };
    let is_secs =
        lhs.ends_with('s') || lhs.ends_with('S') || rhs.ends_with('s') || rhs.ends_with('S');
    let lhs_clean = lhs.trim_end_matches(['s', 'S']);
    let rhs_clean = rhs.trim_end_matches(['s', 'S']);
    let Ok(start) = lhs_clean.parse::<u64>() else {
        return Window::All;
    };
    let end = if rhs_clean.is_empty() {
        WIN_MAX
    } else {
        let Ok(e) = rhs_clean.parse::<u64>() else {
            return Window::All;
        };
        if e < start {
            return Window::All;
        }
        e
    };
    if is_secs {
        Window::Seconds { start, end }
    } else {
        Window::Frames { start, end }
    }
}

fn parse_point(xs: &str, ys: &str) -> Option<WatchPoint> {
    let xs = xs.trim();
    let ys = ys.trim();
    let x = xs.parse::<f64>().ok()?;
    let y = ys.parse::<f64>().ok()?;
    if !x.is_finite() || !y.is_finite() || x < 0.0 || y < 0.0 {
        return None;
    }
    let has_dot = xs.contains('.') || ys.contains('.');
    if has_dot && x <= 1.0 && y <= 1.0 {
        let u = x.clamp(0.0, 1.0);
        let v = y.clamp(0.0, 1.0);
        let x4k = ((u * REF_WIDTH).floor() as i64).clamp(0, REF_WIDTH as i64 - 1) as u16;
        let y4k = ((v * REF_HEIGHT).floor() as i64).clamp(0, REF_HEIGHT as i64 - 1) as u16;
        Some(WatchPoint {
            u,
            v,
            x4k,
            y4k,
            normalized: true,
        })
    } else {
        let xi = x.floor().clamp(0.0, REF_WIDTH - 1.0);
        let yi = y.floor().clamp(0.0, REF_HEIGHT - 1.0);
        Some(WatchPoint {
            u: (xi + 0.5) / REF_WIDTH,
            v: (yi + 0.5) / REF_HEIGHT,
            x4k: xi as u16,
            y4k: yi as u16,
            normalized: false,
        })
    }
}

/// Parses `VICTORY_PIXEL_WATCH` (`<x>,<y>[,<every_n>][;<x2>,<y2>...]`).
/// `window_set` determines the default `every_n` when omitted (`1` inside a frame/time window,
/// `30` otherwise).
pub(crate) fn parse_config(value: Option<&str>, window_set: bool) -> Config {
    let Some(raw) = value.map(str::trim) else {
        return Config::default();
    };
    if raw.is_empty() || raw == "0" {
        return Config::default();
    }
    let mut cfg = Config {
        every: if window_set { 1 } else { 30 },
        ..Config::default()
    };
    for (idx, part) in raw.split(';').enumerate() {
        if cfg.count >= MAX_POINTS {
            break;
        }
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let fields: Vec<&str> = part.split(',').collect();
        if fields.len() < 2 || fields.len() > 3 {
            if idx == 0 {
                return Config::default();
            }
            continue;
        }
        let Some(pt) = parse_point(fields[0], fields[1]) else {
            if idx == 0 {
                return Config::default();
            }
            continue;
        };
        if idx == 0 && fields.len() == 3 {
            let Ok(n) = fields[2].trim().parse::<u64>() else {
                return Config::default();
            };
            if n == 0 {
                return Config::default();
            }
            cfg.every = n;
        }
        cfg.points[cfg.count] = pt;
        cfg.count += 1;
    }
    if cfg.count == 0 {
        return Config::default();
    }
    cfg
}

/// Lock-free `every_n` (>0 when `VICTORY_PIXEL_WATCH` has a valid pixel; 0 when off).
/// Bit 63 marks that `every_n` was explicitly given so `this_frame` knows whether to use
/// `1` when `VICTORY_PIXEL_WATCH_FRAMES` is active.
const EXPLICIT_EVERY: u64 = 1 << 63;

fn parse_on(value: Option<&str>) -> u64 {
    let Some(raw) = value.map(str::trim) else {
        return 0;
    };
    if raw.is_empty() || raw == "0" {
        return 0;
    }
    let first = raw.split(';').next().unwrap_or("").trim();
    let explicit = first.split(',').count() == 3;
    let cfg = parse_config(Some(raw), false);
    if cfg.count == 0 {
        0
    } else if explicit {
        cfg.every | EXPLICIT_EVERY
    } else {
        cfg.every
    }
}

static ON: victory_core::settings::Live =
    victory_core::settings::Live::new("VICTORY_PIXEL_WATCH", parse_on);
static WINDOW: victory_core::settings::Live =
    victory_core::settings::Live::new("VICTORY_PIXEL_WATCH_FRAMES", |v| {
        pack_window(parse_window(v))
    });
static CONFIG: victory_core::settings::Derived<Config> =
    victory_core::settings::Derived::new(|| {
        let win = unpack_window(WINDOW.get());
        let val = victory_core::settings::get("VICTORY_PIXEL_WATCH");
        parse_config(val.as_deref(), win != Window::All)
    });

/// Fast lock-free check: whether `VICTORY_PIXEL_WATCH` is enabled.
#[inline]
pub(crate) fn on() -> bool {
    ON.get() != 0
}

/// Pure cadence predicate (tested directly without relying on process uptime).
pub(crate) fn frame_in_window(frame: u64, elapsed_secs: u64, every: u64, window: Window) -> bool {
    if every == 0 {
        return false;
    }
    match window {
        Window::All => frame.is_multiple_of(every),
        Window::Frames { start, end } => {
            frame >= start && frame <= end && (frame - start).is_multiple_of(every)
        }
        Window::Seconds { start, end } => {
            elapsed_secs >= start && elapsed_secs <= end && frame.is_multiple_of(every)
        }
    }
}

/// Lock-free hot-path check: whether `frame` should be sampled.
#[inline]
pub(crate) fn this_frame(frame: u64) -> bool {
    let bits = ON.get();
    if bits == 0 {
        return false;
    }
    let window = unpack_window(WINDOW.get());
    let every = if bits & EXPLICIT_EVERY != 0 {
        bits & !EXPLICIT_EVERY
    } else if window != Window::All {
        1
    } else {
        bits
    };
    let secs = match window {
        Window::Seconds { .. } => victory_core::settings::started().elapsed().as_secs(),
        _ => 0,
    };
    frame_in_window(frame, secs, every, window)
}

/// Current live watch configuration (points + cadence).
pub(crate) fn config() -> Config {
    CONFIG.get()
}

/// Whether a 2D target `w` x `h` is screen-shaped (16:9 within 5%, at least 160x90).
pub(crate) fn screen_shaped(w: u64, h: u32) -> bool {
    w >= 160 && h >= 90 && ((w as f64 / f64::from(h)) / (16.0 / 9.0) - 1.0).abs() < 0.05
}

/// Maps normalized `(u, v)` to integer texel `(tx, ty)` inside `width x height`.
pub(crate) fn map_pixel(u: f64, v: f64, width: u32, height: u32) -> (u32, u32) {
    if width == 0 || height == 0 {
        return (0, 0);
    }
    let tx = ((u * f64::from(width)).floor() as i64).clamp(0, i64::from(width) - 1) as u32;
    let ty = ((v * f64::from(height)).floor() as i64).clamp(0, i64::from(height) - 1) as u32;
    (tx, ty)
}

/// Whether texel `(tx, ty)` (its center `(tx + 0.5, ty + 0.5)`) lies inside `viewport` and
/// `scissor` (when set).
pub(crate) fn point_covered(
    tx: u32,
    ty: u32,
    viewport: Option<[f32; 4]>,
    scissor: Option<[i32; 4]>,
) -> bool {
    let px = tx as f32 + 0.5;
    let py = ty as f32 + 0.5;
    if let Some([x, y, w, h]) = viewport {
        if w <= 0.0 || h <= 0.0 || px < x || px >= x + w || py < y || py >= y + h {
            return false;
        }
    }
    if let Some([l, t, r, b]) = scissor {
        if r <= l
            || b <= t
            || (tx as i32) < l
            || (tx as i32) >= r
            || (ty as i32) < t
            || (ty as i32) >= b
        {
            return false;
        }
    }
    true
}

/// How a format's decoded values are compared for significant changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ValueKind {
    ColorFloat,
    Depth,
    Integer,
}

/// Copy & decode metadata for a DXGI format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FormatInfo {
    /// Concrete non-typeless DXGI format for `CopyTextureRegion` plane 0.
    pub(crate) copy_format: u32,
    /// Bytes per texel on plane 0.
    pub(crate) bytes: u32,
    /// Whether this format has a second plane (stencil) in D3D12.
    pub(crate) has_stencil_plane: bool,
    pub(crate) name: &'static str,
    pub(crate) kind: ValueKind,
}

/// Metadata for uncompressed RTV/DSV/UAV/swapchain DXGI formats; `None` for BC/unsupported.
pub(crate) fn format_info(dxgi: u32) -> Option<FormatInfo> {
    use ValueKind::*;
    let (copy_format, bytes, has_stencil_plane, name, kind) = match dxgi {
        // R32G32B32A32
        1 => (2, 16, false, "rgba32_t", ColorFloat),
        2 => (2, 16, false, "rgba32f", ColorFloat),
        3 => (3, 16, false, "rgba32u", Integer),
        4 => (4, 16, false, "rgba32i", Integer),
        // R16G16B16A16
        9 => (10, 8, false, "rgba16_t", ColorFloat),
        10 => (10, 8, false, "rgba16f", ColorFloat),
        11 => (11, 8, false, "rgba16un", ColorFloat),
        12 => (12, 8, false, "rgba16u", Integer),
        13 => (13, 8, false, "rgba16sn", ColorFloat),
        14 => (14, 8, false, "rgba16i", Integer),
        // R32G32
        15 => (16, 8, false, "rg32_t", ColorFloat),
        16 => (16, 8, false, "rg32f", ColorFloat),
        17 => (17, 8, false, "rg32u", Integer),
        18 => (18, 8, false, "rg32i", Integer),
        // D32_FLOAT_S8X24_UINT / R32G8X24_TYPELESS
        19 => (21, 8, true, "d32s8_t", Depth),
        20 => (21, 8, true, "d32s8", Depth),
        21 => (21, 8, true, "r32x8", Depth),
        // R10G10B10A2
        23 => (24, 4, false, "rgb10a2_t", ColorFloat),
        24 => (24, 4, false, "rgb10a2", ColorFloat),
        25 => (25, 4, false, "rgb10a2u", Integer),
        // R11G11B10_FLOAT
        26 => (26, 4, false, "r11g11b10f", ColorFloat),
        // R8G8B8A8
        27 => (28, 4, false, "rgba8_t", ColorFloat),
        28 => (28, 4, false, "rgba8", ColorFloat),
        29 => (29, 4, false, "rgba8_srgb", ColorFloat),
        30 => (30, 4, false, "rgba8u", Integer),
        31 => (31, 4, false, "rgba8sn", ColorFloat),
        32 => (32, 4, false, "rgba8i", Integer),
        // R16G16
        33 => (34, 4, false, "rg16_t", ColorFloat),
        34 => (34, 4, false, "rg16f", ColorFloat),
        35 => (35, 4, false, "rg16un", ColorFloat),
        36 => (36, 4, false, "rg16u", Integer),
        37 => (37, 4, false, "rg16sn", ColorFloat),
        38 => (38, 4, false, "rg16i", Integer),
        // R32 / D32_FLOAT
        39 => (41, 4, false, "r32_t", Depth),
        40 => (41, 4, false, "d32f", Depth),
        41 => (41, 4, false, "r32f", ColorFloat),
        42 => (42, 4, false, "r32u", Integer),
        43 => (43, 4, false, "r32i", Integer),
        // D24_UNORM_S8_UINT / R24G8_TYPELESS
        44 => (46, 4, true, "d24s8_t", Depth),
        45 => (46, 4, true, "d24s8", Depth),
        46 => (46, 4, true, "r24x8", Depth),
        // R8G8
        48 => (49, 2, false, "rg8_t", ColorFloat),
        49 => (49, 2, false, "rg8", ColorFloat),
        50 => (50, 2, false, "rg8u", Integer),
        51 => (51, 2, false, "rg8sn", ColorFloat),
        52 => (52, 2, false, "rg8i", Integer),
        // R16 / D16_UNORM
        53 => (56, 2, false, "r16_t", ColorFloat),
        54 => (54, 2, false, "r16f", ColorFloat),
        55 => (56, 2, false, "d16", Depth),
        56 => (56, 2, false, "r16un", ColorFloat),
        57 => (57, 2, false, "r16u", Integer),
        58 => (58, 2, false, "r16sn", ColorFloat),
        59 => (59, 2, false, "r16i", Integer),
        // R8
        60 => (61, 1, false, "r8_t", ColorFloat),
        61 => (61, 1, false, "r8", ColorFloat),
        62 => (62, 1, false, "r8u", Integer),
        63 => (63, 1, false, "r8sn", ColorFloat),
        64 => (64, 1, false, "r8i", Integer),
        // R9G9B9E5_SHAREDEXP
        67 => (67, 4, false, "rgb9e5", ColorFloat),
        // B8G8R8A8 / B8G8R8X8
        87 => (87, 4, false, "bgra8", ColorFloat),
        88 => (88, 4, false, "bgrx8", ColorFloat),
        90 => (87, 4, false, "bgra8_t", ColorFloat),
        91 => (91, 4, false, "bgra8_srgb", ColorFloat),
        92 => (88, 4, false, "bgrx8_t", ColorFloat),
        93 => (93, 4, false, "bgrx8_srgb", ColorFloat),
        _ => return None,
    };
    Some(FormatInfo {
        copy_format,
        bytes,
        has_stencil_plane,
        name,
        kind,
    })
}

/// IEEE 754 binary16 -> binary32.
pub(crate) fn f16_to_f32(bits: u16) -> f32 {
    let sign = u32::from(bits >> 15) << 31;
    let exp = u32::from((bits >> 10) & 0x1f);
    let mant = u32::from(bits & 0x03ff);
    let out = if exp == 0 {
        if mant == 0 {
            sign
        } else {
            // Subnormal: normalize.
            let mut m = mant;
            let mut e = 127 - 14;
            while (m & 0x0400) == 0 {
                m <<= 1;
                e -= 1;
            }
            sign | (e << 23) | ((m & 0x03ff) << 13)
        }
    } else if exp == 31 {
        sign | (0xff << 23) | (mant << 13)
    } else {
        sign | ((exp + (127 - 15)) << 23) | (mant << 13)
    };
    f32::from_bits(out)
}

/// Unsigned 11-bit float (5-bit exponent, 6-bit mantissa) -> f32.
pub(crate) fn f11_to_f32(bits: u32) -> f32 {
    let exp = (bits >> 6) & 0x1f;
    let mant = bits & 0x3f;
    let out = if exp == 0 {
        if mant == 0 {
            0
        } else {
            let mut m = mant;
            let mut e = 127 - 14;
            while (m & 0x40) == 0 {
                m <<= 1;
                e -= 1;
            }
            (e << 23) | ((m & 0x3f) << 17)
        }
    } else if exp == 31 {
        (0xff << 23) | (mant << 17)
    } else {
        ((exp + (127 - 15)) << 23) | (mant << 17)
    };
    f32::from_bits(out)
}

/// Unsigned 10-bit float (5-bit exponent, 5-bit mantissa) -> f32.
pub(crate) fn f10_to_f32(bits: u32) -> f32 {
    let exp = (bits >> 5) & 0x1f;
    let mant = bits & 0x1f;
    let out = if exp == 0 {
        if mant == 0 {
            0
        } else {
            let mut m = mant;
            let mut e = 127 - 14;
            while (m & 0x20) == 0 {
                m <<= 1;
                e -= 1;
            }
            (e << 23) | ((m & 0x1f) << 18)
        }
    } else if exp == 31 {
        (0xff << 23) | (mant << 18)
    } else {
        ((exp + (127 - 15)) << 23) | (mant << 18)
    };
    f32::from_bits(out)
}

/// `DXGI_FORMAT_R9G9B9E5_SHAREDEXP` -> `[r, g, b]`.
pub(crate) fn rgb9e5_to_f32(word: u32) -> [f32; 3] {
    let r = (word & 0x1ff) as f32;
    let g = ((word >> 9) & 0x1ff) as f32;
    let b = ((word >> 18) & 0x1ff) as f32;
    let exp = ((word >> 27) & 0x1f) as i32;
    let scale = 2.0f32.powi(exp - 15 - 9);
    [r * scale, g * scale, b * scale]
}

fn snorm8(b: u8) -> f32 {
    ((b as i8) as f32 / 127.0).max(-1.0)
}

fn snorm16(v: u16) -> f32 {
    ((v as i16) as f32 / 32767.0).max(-1.0)
}

/// Decoded single texel value.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Decoded {
    Float([f32; 4], u8),
    Int([i64; 4], u8),
}

impl Decoded {
    pub(crate) fn format_compact(&self) -> String {
        match *self {
            Decoded::Float(v, n) => {
                let mut s = String::from("(");
                for (i, &c) in v.iter().take(usize::from(n)).enumerate() {
                    if i > 0 {
                        s.push(',');
                    }
                    if c.is_nan() {
                        s.push_str("NaN");
                    } else if c.is_infinite() {
                        s.push_str(if c.is_sign_positive() { "+Inf" } else { "-Inf" });
                    } else if n == 1 {
                        let _ = write!(s, "{c:.5}");
                    } else {
                        let _ = write!(s, "{c:.3}");
                    }
                }
                s.push(')');
                s
            }
            Decoded::Int(v, n) => {
                let mut s = String::from("(");
                for (i, &c) in v.iter().take(usize::from(n)).enumerate() {
                    if i > 0 {
                        s.push(',');
                    }
                    let _ = write!(s, "{c}");
                }
                s.push(')');
                s
            }
        }
    }
}

/// Decodes raw little-endian texel bytes for `dxgi` format.
pub(crate) fn decode_texel(dxgi: u32, raw: &[u8]) -> Option<Decoded> {
    let u16_at = |i: usize| u16::from_le_bytes([raw[i], raw[i + 1]]);
    let u32_at = |i: usize| u32::from_le_bytes([raw[i], raw[i + 1], raw[i + 2], raw[i + 3]]);
    let f32_at = |i: usize| f32::from_bits(u32_at(i));
    let info = format_info(dxgi)?;
    if raw.len() < info.bytes as usize {
        return None;
    }
    Some(match dxgi {
        1 | 2 => Decoded::Float([f32_at(0), f32_at(4), f32_at(8), f32_at(12)], 4),
        3 => Decoded::Int(
            [
                i64::from(u32_at(0)),
                i64::from(u32_at(4)),
                i64::from(u32_at(8)),
                i64::from(u32_at(12)),
            ],
            4,
        ),
        4 => Decoded::Int(
            [
                i64::from(u32_at(0) as i32),
                i64::from(u32_at(4) as i32),
                i64::from(u32_at(8) as i32),
                i64::from(u32_at(12) as i32),
            ],
            4,
        ),
        9 | 10 => Decoded::Float(
            [
                f16_to_f32(u16_at(0)),
                f16_to_f32(u16_at(2)),
                f16_to_f32(u16_at(4)),
                f16_to_f32(u16_at(6)),
            ],
            4,
        ),
        11 => Decoded::Float(
            [
                f32::from(u16_at(0)) / 65535.0,
                f32::from(u16_at(2)) / 65535.0,
                f32::from(u16_at(4)) / 65535.0,
                f32::from(u16_at(6)) / 65535.0,
            ],
            4,
        ),
        12 => Decoded::Int(
            [
                i64::from(u16_at(0)),
                i64::from(u16_at(2)),
                i64::from(u16_at(4)),
                i64::from(u16_at(6)),
            ],
            4,
        ),
        13 => Decoded::Float(
            [
                snorm16(u16_at(0)),
                snorm16(u16_at(2)),
                snorm16(u16_at(4)),
                snorm16(u16_at(6)),
            ],
            4,
        ),
        14 => Decoded::Int(
            [
                i64::from(u16_at(0) as i16),
                i64::from(u16_at(2) as i16),
                i64::from(u16_at(4) as i16),
                i64::from(u16_at(6) as i16),
            ],
            4,
        ),
        15 | 16 => Decoded::Float([f32_at(0), f32_at(4), 0.0, 0.0], 2),
        17 => Decoded::Int([i64::from(u32_at(0)), i64::from(u32_at(4)), 0, 0], 2),
        18 => Decoded::Int(
            [
                i64::from(u32_at(0) as i32),
                i64::from(u32_at(4) as i32),
                0,
                0,
            ],
            2,
        ),
        19..=21 => Decoded::Float([f32_at(0), 0.0, 0.0, 0.0], 1),
        23 | 24 => {
            let w = u32_at(0);
            Decoded::Float(
                [
                    (w & 0x3ff) as f32 / 1023.0,
                    ((w >> 10) & 0x3ff) as f32 / 1023.0,
                    ((w >> 20) & 0x3ff) as f32 / 1023.0,
                    ((w >> 30) & 0x3) as f32 / 3.0,
                ],
                4,
            )
        }
        25 => {
            let w = u32_at(0);
            Decoded::Int(
                [
                    i64::from(w & 0x3ff),
                    i64::from((w >> 10) & 0x3ff),
                    i64::from((w >> 20) & 0x3ff),
                    i64::from((w >> 30) & 0x3),
                ],
                4,
            )
        }
        26 => {
            let w = u32_at(0);
            Decoded::Float(
                [
                    f11_to_f32(w & 0x7ff),
                    f11_to_f32((w >> 11) & 0x7ff),
                    f10_to_f32((w >> 22) & 0x3ff),
                    0.0,
                ],
                3,
            )
        }
        27..=29 => Decoded::Float(
            [
                f32::from(raw[0]) / 255.0,
                f32::from(raw[1]) / 255.0,
                f32::from(raw[2]) / 255.0,
                f32::from(raw[3]) / 255.0,
            ],
            4,
        ),
        30 => Decoded::Int(
            [
                i64::from(raw[0]),
                i64::from(raw[1]),
                i64::from(raw[2]),
                i64::from(raw[3]),
            ],
            4,
        ),
        31 => Decoded::Float(
            [
                snorm8(raw[0]),
                snorm8(raw[1]),
                snorm8(raw[2]),
                snorm8(raw[3]),
            ],
            4,
        ),
        32 => Decoded::Int(
            [
                i64::from(raw[0] as i8),
                i64::from(raw[1] as i8),
                i64::from(raw[2] as i8),
                i64::from(raw[3] as i8),
            ],
            4,
        ),
        33 | 34 => Decoded::Float([f16_to_f32(u16_at(0)), f16_to_f32(u16_at(2)), 0.0, 0.0], 2),
        35 => Decoded::Float(
            [
                f32::from(u16_at(0)) / 65535.0,
                f32::from(u16_at(2)) / 65535.0,
                0.0,
                0.0,
            ],
            2,
        ),
        36 => Decoded::Int([i64::from(u16_at(0)), i64::from(u16_at(2)), 0, 0], 2),
        37 => Decoded::Float([snorm16(u16_at(0)), snorm16(u16_at(2)), 0.0, 0.0], 2),
        38 => Decoded::Int(
            [
                i64::from(u16_at(0) as i16),
                i64::from(u16_at(2) as i16),
                0,
                0,
            ],
            2,
        ),
        39..=41 => Decoded::Float([f32_at(0), 0.0, 0.0, 0.0], 1),
        42 => Decoded::Int([i64::from(u32_at(0)), 0, 0, 0], 1),
        43 => Decoded::Int([i64::from(u32_at(0) as i32), 0, 0, 0], 1),
        44..=46 => {
            let d = (u32_at(0) & 0x00ff_ffff) as f32 / 16_777_215.0;
            Decoded::Float([d, 0.0, 0.0, 0.0], 1)
        }
        48 | 49 => Decoded::Float(
            [
                f32::from(raw[0]) / 255.0,
                f32::from(raw[1]) / 255.0,
                0.0,
                0.0,
            ],
            2,
        ),
        50 => Decoded::Int([i64::from(raw[0]), i64::from(raw[1]), 0, 0], 2),
        51 => Decoded::Float([snorm8(raw[0]), snorm8(raw[1]), 0.0, 0.0], 2),
        52 => Decoded::Int([i64::from(raw[0] as i8), i64::from(raw[1] as i8), 0, 0], 2),
        53 | 55 | 56 => Decoded::Float([f32::from(u16_at(0)) / 65535.0, 0.0, 0.0, 0.0], 1),
        54 => Decoded::Float([f16_to_f32(u16_at(0)), 0.0, 0.0, 0.0], 1),
        57 => Decoded::Int([i64::from(u16_at(0)), 0, 0, 0], 1),
        58 => Decoded::Float([snorm16(u16_at(0)), 0.0, 0.0, 0.0], 1),
        59 => Decoded::Int([i64::from(u16_at(0) as i16), 0, 0, 0], 1),
        60 | 61 => Decoded::Float([f32::from(raw[0]) / 255.0, 0.0, 0.0, 0.0], 1),
        62 => Decoded::Int([i64::from(raw[0]), 0, 0, 0], 1),
        63 => Decoded::Float([snorm8(raw[0]), 0.0, 0.0, 0.0], 1),
        64 => Decoded::Int([i64::from(raw[0] as i8), 0, 0, 0], 1),
        67 => {
            let [r, g, b] = rgb9e5_to_f32(u32_at(0));
            Decoded::Float([r, g, b, 0.0], 3)
        }
        87 | 88 | 90..=93 => Decoded::Float(
            [
                f32::from(raw[2]) / 255.0,
                f32::from(raw[1]) / 255.0,
                f32::from(raw[0]) / 255.0,
                f32::from(raw[3]) / 255.0,
            ],
            4,
        ),
        _ => return None,
    })
}

/// Returns `Some(delta)` when `now` differs significantly from `prev` (filtering out minor
/// TAA/dithering jitter while catching lighting, albedo, normal, shadow, depth and stencil jumps).
pub(crate) fn significant_change(prev: &Decoded, now: &Decoded, kind: ValueKind) -> Option<f32> {
    match (*prev, *now) {
        (Decoded::Int(a, n), Decoded::Int(b, _)) => {
            let max_abs = a[..usize::from(n)]
                .iter()
                .zip(&b[..usize::from(n)])
                .map(|(x, y)| (x - y).abs())
                .max()
                .unwrap_or(0);
            (max_abs > 0).then_some(max_abs as f32)
        }
        (Decoded::Float(a, n), Decoded::Float(b, _)) => {
            let mut max_abs = 0.0f32;
            let mut max_rel = 0.0f32;
            for (&x, &y) in a[..usize::from(n)].iter().zip(&b[..usize::from(n)]) {
                if x.is_nan() != y.is_nan() || x.is_infinite() != y.is_infinite() {
                    return Some(f32::INFINITY);
                }
                if x.is_nan() && y.is_nan() {
                    continue;
                }
                let d = (y - x).abs();
                let mag = x.abs().max(y.abs());
                max_abs = max_abs.max(d);
                if d > 0.0 {
                    max_rel = max_rel.max(d / mag.max(0.01));
                }
            }
            match kind {
                ValueKind::Depth => (max_abs >= 1e-4).then_some(max_abs),
                ValueKind::Integer => (max_abs >= 0.5).then_some(max_abs),
                ValueKind::ColorFloat => {
                    let big = max_abs >= 0.03 && max_rel >= 0.10;
                    let dim_drop = max_abs >= 0.015 && max_rel >= 0.35;
                    (big || dim_drop).then_some(max_abs)
                }
            }
        }
        _ => Some(f32::INFINITY),
    }
}

fn hex_of(raw: &[u8]) -> String {
    let mut s = String::with_capacity(raw.len() * 2);
    for &b in raw {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Kind of watched pass target.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum PassKind {
    Rt,
    Dsv,
    Uav,
    Present,
}

impl PassKind {
    pub(crate) fn label(self, slot: u16) -> String {
        match self {
            PassKind::Rt => format!("rt{slot}"),
            PassKind::Dsv => "dsv".to_owned(),
            PassKind::Uav => format!("u{slot}"),
            PassKind::Present => "present".to_owned(),
        }
    }
}

/// One bound SRV/UAV texture of a covering draw or compute dispatch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BoundTex {
    /// `t` (SRV) or `u` (UAV).
    pub(crate) kind: u8,
    pub(crate) register: u16,
    pub(crate) resource: usize,
    pub(crate) memory: u64,
    pub(crate) width: u64,
    pub(crate) height: u32,
    pub(crate) format: u32,
    pub(crate) slot: Option<u32>,
    pub(crate) host: &'static str,
}

impl BoundTex {
    pub(crate) fn format_compact(&self) -> String {
        let mut s = format!(
            "{}{}:f{} {}x{}",
            self.kind as char, self.register, self.format, self.width, self.height
        );
        if self.resource != 0 {
            let _ = write!(s, " r={:#x}", self.resource);
        } else if self.memory != 0 {
            let _ = write!(s, " m={:#x}", self.memory);
        }
        if let Some(slot) = self.slot {
            let _ = write!(s, " s={slot}({})", self.host);
        }
        s
    }
}

/// One unique pipeline/table set that covered the watched pixel in a pass.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DrawCover {
    pub(crate) shader_hash: String,
    pub(crate) pipeline_name: String,
    pub(crate) draws: u32,
    pub(crate) sig: u64,
    pub(crate) heap_indexed: bool,
    pub(crate) textures: Vec<BoundTex>,
}

impl DrawCover {
    pub(crate) fn format_compact(&self) -> String {
        let short_hash = self.shader_hash.get(..8).unwrap_or(&self.shader_hash);
        let mut s = format!("{short_hash}");
        if !self.pipeline_name.is_empty() {
            let clean: String = self
                .pipeline_name
                .chars()
                .take(40)
                .map(|c| if c.is_whitespace() { '_' } else { c })
                .collect();
            let _ = write!(s, "({clean})");
        }
        let _ = write!(s, "x{}", self.draws);
        if self.heap_indexed {
            s.push_str("[heap_idx]");
        }
        if !self.textures.is_empty() {
            s.push('{');
            for (i, t) in self.textures.iter().take(6).enumerate() {
                if i > 0 {
                    s.push(',');
                }
                s.push_str(&t.format_compact());
            }
            if self.textures.len() > 6 {
                let _ = write!(s, ",+{}", self.textures.len() - 6);
            }
            s.push('}');
        }
        s
    }
}

/// Metadata + bytes for one landed 1x1 texel sample.
#[derive(Clone, Debug)]
pub(crate) struct LandedSample {
    pub(crate) point_idx: u8,
    pub(crate) point: WatchPoint,
    pub(crate) kind: PassKind,
    pub(crate) slot: u16,
    pub(crate) resource: usize,
    pub(crate) memory: u64,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) format: u32,
    pub(crate) tx: u32,
    pub(crate) ty: u32,
    /// Tag differentiating compute passes (`uav` uses CS hash FNV; `rt`/`dsv`/`present` use 0).
    pub(crate) tag: u64,
    /// Total draws (or dispatches) in the pass that covered this pixel.
    pub(crate) draw_count: u32,
    /// Covering pipelines + bound textures.
    pub(crate) covers: Vec<DrawCover>,
    /// Raw little-endian bytes of plane 0 (`1..=16` bytes).
    pub(crate) raw: Vec<u8>,
    /// Optional stencil byte (plane 1 of planar depth/stencil).
    pub(crate) stencil: Option<u8>,
}

/// Stable logical pass key across consecutive watched frames (resilient to double-buffering).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct PassKey {
    pub(crate) point_idx: u8,
    pub(crate) kind: PassKind,
    pub(crate) slot: u16,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) format: u32,
    pub(crate) tag: u64,
    pub(crate) occurrence: u16,
}

#[derive(Clone, Debug)]
struct PrevSample {
    frame: u64,
    raw: Vec<u8>,
    stencil: Option<u8>,
    decoded: Decoded,
    cover_sig: u64,
}

#[derive(Default)]
struct FrameSummary {
    frame: u64,
    lists: u32,
    sampled: u32,
    firsts: u32,
    changed: u32,
    unchanged_tags: Vec<String>,
    occ: HashMap<(u8, PassKind, u16, u32, u32, u32, u64), u16>,
}

#[derive(Default)]
struct Tracker {
    prev: HashMap<PassKey, PrevSample>,
    cur: FrameSummary,
    watched_frames: u64,
    total_changes: u64,
}

static TRACKER: Mutex<Option<Tracker>> = Mutex::new(None);
static LOGGED_ON: AtomicU64 = AtomicU64::new(0);

/// Logs the active watch config once when enabled (or when config changes).
pub(crate) fn announce_if_needed() {
    let bits = ON.get() ^ WINDOW.get().rotate_left(7);
    if bits == 0 {
        return;
    }
    if LOGGED_ON.swap(bits, Relaxed) != bits {
        let cfg = config();
        let pts: Vec<String> = cfg
            .active_points()
            .iter()
            .enumerate()
            .map(|(i, p)| format!("p{i}={}", p.label()))
            .collect();
        tracing::info!(
            points = %pts.join(" "),
            every = cfg.every,
            window = ?unpack_window(WINDOW.get()),
            "pixel watch active (VICTORY_PIXEL_WATCH / VICTORY_PIXEL_WATCH_FRAMES)"
        );
    }
}

pub(crate) fn fnv64(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    })
}

fn cover_sig(covers: &[DrawCover]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for c in covers {
        for &b in c.shader_hash.as_bytes() {
            h = (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
        }
        h = (h ^ c.sig).wrapping_mul(0x0100_0000_01b3);
        h = (h ^ u64::from(c.draws)).wrapping_mul(0x0100_0000_01b3);
    }
    h
}

fn format_covers(covers: &[DrawCover]) -> String {
    if covers.is_empty() {
        return "-".to_owned();
    }
    let mut s = String::new();
    for (i, c) in covers.iter().take(6).enumerate() {
        if i > 0 {
            s.push_str(" | ");
        }
        s.push_str(&c.format_compact());
    }
    if covers.len() > 6 {
        let _ = write!(s, " | +{}", covers.len() - 6);
    }
    s
}

/// Evaluates one batch of landed samples into log lines (pure logic, unit-tested).
fn evaluate_batch(
    t: &mut Tracker,
    frame: u64,
    list_id: u64,
    samples: &[LandedSample],
    out: &mut Vec<String>,
) {
    if t.cur.frame != frame {
        if t.cur.frame != 0 && t.cur.sampled > 0 {
            t.watched_frames += 1;
            if t.cur.changed > 0
                || t.cur.firsts > 0
                || t.watched_frames.is_multiple_of(HEARTBEAT_EVERY)
            {
                let unchanged = if t.cur.changed > 0 {
                    let shown: Vec<&str> = t
                        .cur
                        .unchanged_tags
                        .iter()
                        .take(24)
                        .map(String::as_str)
                        .collect();
                    let extra = t.cur.unchanged_tags.len().saturating_sub(24);
                    if extra > 0 {
                        format!(" unchanged=[{}, +{} more]", shown.join(", "), extra)
                    } else {
                        format!(" unchanged=[{}]", shown.join(", "))
                    }
                } else {
                    String::new()
                };
                out.push(format!(
                    "pixel watch: frame={} lists={} sampled={} first={} changed={}{unchanged}",
                    t.cur.frame, t.cur.lists, t.cur.sampled, t.cur.firsts, t.cur.changed
                ));
            }
        }
        t.cur = FrameSummary {
            frame,
            ..FrameSummary::default()
        };
    }
    t.cur.lists += 1;

    for s in samples {
        let Some(info) = format_info(s.format) else {
            continue;
        };
        let Some(decoded) = decode_texel(s.format, &s.raw) else {
            continue;
        };
        let base = (
            s.point_idx,
            s.kind,
            s.slot,
            s.width,
            s.height,
            s.format,
            s.tag,
        );
        let occ = {
            let e = t.cur.occ.entry(base).or_insert(0);
            let cur = *e;
            *e = cur.saturating_add(1);
            cur
        };
        let key = PassKey {
            point_idx: s.point_idx,
            kind: s.kind,
            slot: s.slot,
            width: s.width,
            height: s.height,
            format: s.format,
            tag: s.tag,
            occurrence: occ,
        };
        let csig = cover_sig(&s.covers);
        let kind_label = s.kind.label(s.slot);
        let pt_label = format!("p{}{}", s.point_idx, s.point.label());
        t.cur.sampled += 1;

        match t.prev.get(&key) {
            None => {
                t.cur.firsts += 1;
                let stencil_txt = s
                    .stencil
                    .map_or_else(String::new, |st| format!(" st={st:#04x}"));
                out.push(format!(
                    "pixel watch: first frame={frame} list=#{list_id} pt={pt_label} pass={kind_label}#{occ} res={:#x} mem={:#x} fmt=f{}({}) {}x{} texel=({},{}) val={}{stencil_txt} hex={} draws={} covers=[{}]",
                    s.resource,
                    s.memory,
                    s.format,
                    info.name,
                    s.width,
                    s.height,
                    s.tx,
                    s.ty,
                    decoded.format_compact(),
                    hex_of(&s.raw),
                    s.draw_count,
                    format_covers(&s.covers),
                ));
                t.prev.insert(
                    key,
                    PrevSample {
                        frame,
                        raw: s.raw.clone(),
                        stencil: s.stencil,
                        decoded,
                        cover_sig: csig,
                    },
                );
            }
            Some(prev) => {
                let val_delta = significant_change(&prev.decoded, &decoded, info.kind);
                let stencil_changed = prev.stencil != s.stencil;
                if val_delta.is_some() || stencil_changed {
                    t.cur.changed += 1;
                    if t.cur.changed <= MAX_CHANGES_PER_FRAME && t.total_changes < MAX_CHANGE_LOGS {
                        t.total_changes += 1;
                        let draws_diff = if prev.cover_sig != csig { "yes" } else { "no" };
                        let delta_txt = match val_delta {
                            Some(d) if d.is_infinite() => "inf".to_owned(),
                            Some(d) => format!("{d:.4}"),
                            None => "stencil".to_owned(),
                        };
                        let stencil_txt = match (prev.stencil, s.stencil) {
                            (Some(a), Some(b)) if a != b => format!(" st={a:#04x}->{b:#04x}"),
                            (_, Some(b)) => format!(" st={b:#04x}"),
                            _ => String::new(),
                        };
                        out.push(format!(
                            "pixel watch: CHANGE frame={frame} (prev_f={}) list=#{list_id} pt={pt_label} pass={kind_label}#{occ} res={:#x} mem={:#x} fmt=f{}({}) {}x{} texel=({},{}) prev={} now={} delta={delta_txt}{stencil_txt} hex={}->{} draws={} draws_diff={draws_diff} covers=[{}]",
                            prev.frame,
                            s.resource,
                            s.memory,
                            s.format,
                            info.name,
                            s.width,
                            s.height,
                            s.tx,
                            s.ty,
                            prev.decoded.format_compact(),
                            decoded.format_compact(),
                            hex_of(&prev.raw),
                            hex_of(&s.raw),
                            s.draw_count,
                            format_covers(&s.covers),
                        ));
                    }
                    // Update baseline only when significantly changed (or every frame? Updating
                    // every frame catches sudden jumps while staying quiet after the transition!).
                    t.prev.insert(
                        key,
                        PrevSample {
                            frame,
                            raw: s.raw.clone(),
                            stencil: s.stencil,
                            decoded,
                            cover_sig: csig,
                        },
                    );
                } else {
                    let short_tag = format!(
                        "p{}:{kind_label}#{occ}:f{}({}x{})",
                        s.point_idx, s.format, s.width, s.height
                    );
                    if t.cur.unchanged_tags.len() < 48 {
                        t.cur.unchanged_tags.push(short_tag);
                    }
                    // Keep `frame` fresh so prev_f on a later jump is the last watched frame.
                    if let Some(p) = t.prev.get_mut(&key) {
                        p.frame = frame;
                        p.raw.clone_from(&s.raw);
                        p.stencil = s.stencil;
                        p.decoded = decoded;
                        p.cover_sig = csig;
                    }
                }
            }
        }
    }
}

/// Called from `HostList::defer_after` (and `present_sent`) once a list's 1x1 copies have landed.
pub(crate) fn landed(frame: u64, list_id: u64, samples: &[LandedSample]) {
    if samples.is_empty() {
        return;
    }
    let mut lines = Vec::new();
    {
        let mut guard = TRACKER.lock().unwrap_or_else(|e| e.into_inner());
        let t = guard.get_or_insert_with(Tracker::default);
        evaluate_batch(t, frame, list_id, samples, &mut lines);
    }
    for line in lines {
        tracing::info!("{line}");
    }
}

#[cfg(windows)]
pub(crate) mod gpu {
    use super::*;
    use windows::Win32::Graphics::Direct3D12::*;
    use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_SAMPLE_DESC};

    /// Pooled 64 KiB read-back buffer wrapper (`Send` for storage on `draw::State` and in pools).
    pub(crate) struct ReadbackBuf(pub(crate) ID3D12Resource);
    // SAFETY: D3D12 resources are free-threaded; ownership is transferred exclusively.
    unsafe impl Send for ReadbackBuf {}

    static POOL: Mutex<Vec<ReadbackBuf>> = Mutex::new(Vec::new());

    /// Takes a pooled 64 KiB read-back buffer or creates one on `device`.
    pub(crate) fn take_buffer(device: &ID3D12Device) -> Option<ReadbackBuf> {
        if let Some(buf) = POOL.lock().unwrap_or_else(|e| e.into_inner()).pop() {
            return Some(buf);
        }
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_READBACK,
            ..Default::default()
        };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: BATCH_BYTES,
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            ..Default::default()
        };
        let mut out: Option<ID3D12Resource> = None;
        // SAFETY: plain committed read-back buffer creation on a live device.
        unsafe {
            device
                .CreateCommittedResource(
                    &heap,
                    D3D12_HEAP_FLAG_NONE,
                    &desc,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                    None,
                    &mut out,
                )
                .ok()?;
        }
        out.map(ReadbackBuf)
    }

    /// Returns a read-back buffer to the pool once the GPU list that wrote it has finished.
    pub(crate) fn return_buffer(buf: ReadbackBuf) {
        let mut pool = POOL.lock().unwrap_or_else(|e| e.into_inner());
        if pool.len() < MAX_POOLED_BUFFERS {
            pool.push(buf);
        }
    }

    /// Queries D3D12's legal 1x1 `PlacedFootprint` format and row pitch for `desc` at `plane`
    /// (`0` = color/depth, `1` = stencil on planar depth-stencil).
    pub(crate) fn plane_footprint(
        device: &ID3D12Device,
        desc: &D3D12_RESOURCE_DESC,
        plane: u32,
        fallback_format: u32,
    ) -> Option<(DXGI_FORMAT, u32)> {
        let mut one = *desc;
        one.Width = 1;
        one.Height = 1;
        one.DepthOrArraySize = 1;
        one.MipLevels = 1;
        // Map typeless formats to concrete ones before querying so D3D12 always returns a
        // concrete copyable footprint format.
        if plane == 0 {
            one.Format = DXGI_FORMAT(fallback_format as i32);
        }
        let mut fp = D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default();
        let mut row_bytes = 0u64;
        // SAFETY: plain read-only layout query into local structs.
        unsafe {
            device.GetCopyableFootprints(
                &one,
                plane,
                1,
                0,
                Some(&mut fp),
                None,
                Some(&mut row_bytes),
                None,
            );
        }
        if fp.Footprint.RowPitch == 0 || row_bytes == 0 || row_bytes > 16 {
            return None;
        }
        let fmt = if fp.Footprint.Format.0 == 0 {
            DXGI_FORMAT(fallback_format as i32)
        } else {
            fp.Footprint.Format
        };
        Some((fmt, fp.Footprint.RowPitch.next_multiple_of(256)))
    }

    /// Records a 1x1 `CopyTextureRegion` of `(tx, ty)` from `subresource` of `src` into `dst` at
    /// `slot * SLOT_STRIDE`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn copy_texel(
        list: &ID3D12GraphicsCommandList,
        dst: &ID3D12Resource,
        slot: u32,
        src: &ID3D12Resource,
        subresource: u32,
        format: DXGI_FORMAT,
        row_pitch: u32,
        tx: u32,
        ty: u32,
    ) {
        let to = D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: borrowed pointer; `CopyTextureRegion` does not release it.
            pResource: unsafe { std::mem::transmute_copy(dst) },
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                    Offset: u64::from(slot) * SLOT_STRIDE,
                    Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                        Format: format,
                        Width: 1,
                        Height: 1,
                        Depth: 1,
                        RowPitch: row_pitch,
                    },
                },
            },
        };
        let from = D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: borrowed pointer; `CopyTextureRegion` does not release it.
            pResource: unsafe { std::mem::transmute_copy(src) },
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                SubresourceIndex: subresource,
            },
        };
        let area = D3D12_BOX {
            left: tx,
            top: ty,
            front: 0,
            right: tx + 1,
            bottom: ty + 1,
            back: 1,
        };
        // SAFETY: caller ensures `list` is open, `src` is in `COPY_SOURCE`, `dst` is `COPY_DEST`,
        // and `(tx, ty)` lies inside `src`.
        unsafe {
            list.CopyTextureRegion(&to, 0, 0, 0, &from, Some(&area));
        }
    }

    /// Reads `len` bytes from `slot` of a mapped slice of a batched read-back buffer.
    pub(crate) fn read_slot(mapped: &[u8], slot: u32, len: usize) -> Option<Vec<u8>> {
        let at = slot as usize * SLOT_STRIDE as usize;
        mapped.get(at..at + len).map(<[u8]>::to_vec)
    }

    /// Pending present-time 1x1 texel read-back (`Host::present`).
    pub(crate) struct PresentTaken {
        buf: ReadbackBuf,
        frame: u64,
        width: u32,
        height: u32,
        format: u32,
        bytes: usize,
        entries: Vec<(u8, WatchPoint, u32, u32, u32)>,
    }

    static PRESENT_IN_FLIGHT: Mutex<VecDeque<(PresentTaken, u64)>> = Mutex::new(VecDeque::new());

    /// Called from `Host::present` while `texture` is in `COPY_SOURCE` (or `read`).
    pub(crate) fn present_take(
        device: &ID3D12Device,
        list: &ID3D12GraphicsCommandList,
        texture: &ID3D12Resource,
        frame: u64,
    ) -> Option<PresentTaken> {
        if !this_frame(frame) {
            return None;
        }
        announce_if_needed();
        // SAFETY: live texture.
        let desc = unsafe { texture.GetDesc() };
        if desc.Dimension != D3D12_RESOURCE_DIMENSION_TEXTURE2D || desc.SampleDesc.Count > 1 {
            return None;
        }
        let format = desc.Format.0 as u32;
        let info = format_info(format)?;
        let (copy_fmt, row_pitch) = plane_footprint(device, &desc, 0, info.copy_format)?;
        let cfg = config();
        if cfg.count == 0 {
            return None;
        }
        let buf = take_buffer(device)?;
        let (w, h) = (desc.Width as u32, desc.Height);
        let mut entries = Vec::with_capacity(cfg.count);
        for (i, &pt) in cfg.active_points().iter().enumerate() {
            let (tx, ty) = map_pixel(pt.u, pt.v, w, h);
            let slot = i as u32;
            copy_texel(list, &buf.0, slot, texture, 0, copy_fmt, row_pitch, tx, ty);
            entries.push((i as u8, pt, tx, ty, slot));
        }
        Some(PresentTaken {
            buf,
            frame,
            width: w,
            height: h,
            format,
            bytes: info.bytes as usize,
            entries,
        })
    }

    fn finish_present(taken: PresentTaken) {
        let mut data = std::ptr::null_mut();
        // SAFETY: our read-back buffer, after its submission finished.
        let samples = unsafe {
            if taken.buf.0.Map(0, None, Some(&mut data)).is_err() || data.is_null() {
                Vec::new()
            } else {
                let all = std::slice::from_raw_parts(data.cast::<u8>(), BATCH_BYTES as usize);
                let mut out = Vec::with_capacity(taken.entries.len());
                for &(point_idx, point, tx, ty, slot) in &taken.entries {
                    if let Some(raw) = read_slot(all, slot, taken.bytes) {
                        out.push(LandedSample {
                            point_idx,
                            point,
                            kind: PassKind::Present,
                            slot: 0,
                            resource: 0,
                            memory: 0,
                            width: taken.width,
                            height: taken.height,
                            format: taken.format,
                            tx,
                            ty,
                            tag: 0,
                            draw_count: 1,
                            covers: Vec::new(),
                            raw,
                            stencil: None,
                        });
                    }
                }
                taken.buf.0.Unmap(0, None);
                out
            }
        };
        return_buffer(taken.buf);
        landed(taken.frame, 0, &samples);
    }

    /// Called from `Host::present` right after the present list is submitted (`flown == 0` when
    /// executed synchronously). Drains any finished in-flight present read-backs.
    pub(crate) fn present_sent(taken: PresentTaken, flown: u64) {
        let finished = crate::host::submissions_finished();
        let mut ready = Vec::new();
        {
            let mut q = PRESENT_IN_FLIGHT.lock().unwrap_or_else(|e| e.into_inner());
            if flown == 0 || flown <= finished {
                ready.push(taken);
            } else {
                q.push_back((taken, flown));
            }
            while q.front().is_some_and(|(_, f)| *f <= finished) {
                if let Some((t, _)) = q.pop_front() {
                    ready.push(t);
                }
            }
            // Bound queue size if frames are never finished.
            while q.len() > 16 {
                if let Some((t, _)) = q.pop_front() {
                    return_buffer(t.buf);
                }
            }
        }
        for t in ready {
            finish_present(t);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_4k_and_normalized_coords_and_every() {
        let cfg = parse_config(Some("2188,1872,5"), false);
        assert_eq!(cfg.count, 1);
        assert_eq!(cfg.every, 5);
        assert!(!cfg.points[0].normalized);
        assert_eq!(cfg.points[0].x4k, 2188);
        assert_eq!(cfg.points[0].y4k, 1872);
        // Maps accurately to 4K, 1080p, 1440p, 720p, 540p.
        assert_eq!(
            map_pixel(cfg.points[0].u, cfg.points[0].v, 3840, 2160),
            (2188, 1872)
        );
        assert_eq!(
            map_pixel(cfg.points[0].u, cfg.points[0].v, 1920, 1080),
            (1094, 936)
        );
        assert_eq!(
            map_pixel(cfg.points[0].u, cfg.points[0].v, 2560, 1440),
            (1459, 1248)
        );
        assert_eq!(
            map_pixel(cfg.points[0].u, cfg.points[0].v, 1280, 720),
            (729, 624)
        );
        assert_eq!(
            map_pixel(cfg.points[0].u, cfg.points[0].v, 960, 540),
            (547, 468)
        );

        // Normalized coordinates + multi-point.
        let norm = parse_config(Some("0.5698,0.8667,1;956,260;1920,972"), false);
        assert_eq!(norm.count, 3);
        assert_eq!(norm.every, 1);
        assert!(norm.points[0].normalized);
        assert!(!norm.points[1].normalized);
        assert_eq!(
            map_pixel(norm.points[0].u, norm.points[0].v, 1920, 1080),
            (1094, 936)
        );
        assert_eq!(
            map_pixel(norm.points[1].u, norm.points[1].v, 1920, 1080),
            (478, 130)
        );
        assert_eq!(
            map_pixel(norm.points[2].u, norm.points[2].v, 1920, 1080),
            (960, 486)
        );

        // Default every_n is 30 without window, 1 with window.
        assert_eq!(parse_config(Some("2188,1872"), false).every, 30);
        assert_eq!(parse_config(Some("2188,1872"), true).every, 1);

        // Off / invalid inputs.
        assert_eq!(parse_config(None, false).count, 0);
        assert_eq!(parse_config(Some(""), false).count, 0);
        assert_eq!(parse_config(Some("0"), false).count, 0);
        assert_eq!(parse_config(Some("2188,1872,0"), false).count, 0);
        assert_eq!(parse_config(Some("bad"), false).count, 0);
    }

    #[test]
    fn parse_frame_and_second_windows() {
        assert_eq!(parse_window(None), Window::All);
        assert_eq!(parse_window(Some("")), Window::All);
        assert_eq!(parse_window(Some("0")), Window::All);
        assert_eq!(
            parse_window(Some("2750..2810")),
            Window::Frames {
                start: 2750,
                end: 2810
            }
        );
        assert_eq!(
            parse_window(Some("1740-1780")),
            Window::Frames {
                start: 1740,
                end: 1780
            }
        );
        assert_eq!(
            parse_window(Some("95s..145s")),
            Window::Seconds {
                start: 95,
                end: 145
            }
        );
        assert_eq!(
            parse_window(Some("100s-120s")),
            Window::Seconds {
                start: 100,
                end: 120
            }
        );
        // Round-trip pack/unpack.
        for w in [
            Window::All,
            Window::Frames {
                start: 2750,
                end: 2810,
            },
            Window::Seconds {
                start: 95,
                end: 145,
            },
        ] {
            assert_eq!(unpack_window(pack_window(w)), w);
        }

        // Cadence predicate.
        let fw = Window::Frames {
            start: 2750,
            end: 2760,
        };
        assert!(!frame_in_window(2749, 0, 1, fw));
        assert!(frame_in_window(2750, 0, 1, fw));
        assert!(frame_in_window(2760, 0, 1, fw));
        assert!(!frame_in_window(2761, 0, 1, fw));
        assert!(frame_in_window(2750, 0, 5, fw));
        assert!(!frame_in_window(2751, 0, 5, fw));
        assert!(frame_in_window(2755, 0, 5, fw));

        let sw = Window::Seconds {
            start: 95,
            end: 145,
        };
        assert!(!frame_in_window(100, 94, 1, sw));
        assert!(frame_in_window(100, 95, 1, sw));
        assert!(frame_in_window(105, 145, 5, sw));
        assert!(!frame_in_window(106, 145, 5, sw));
        assert!(!frame_in_window(105, 146, 5, sw));
    }

    #[test]
    fn target_filtering_and_coverage() {
        assert!(screen_shaped(3840, 2160));
        assert!(screen_shaped(2560, 1440));
        assert!(screen_shaped(1920, 1080));
        assert!(screen_shaped(1280, 720));
        assert!(screen_shaped(1280, 736)); // tile-aligned 16:9
        assert!(screen_shaped(192, 108));
        assert!(!screen_shaped(1024, 1024)); // square shadow atlas / cube
        assert!(!screen_shaped(64, 36)); // too small
        assert!(!screen_shaped(2048, 512)); // 4:1 LUT / atlas

        assert!(point_covered(
            1094,
            936,
            Some([0.0, 0.0, 1920.0, 1080.0]),
            Some([0, 0, 1920, 1080])
        ));
        assert!(!point_covered(
            1094,
            936,
            Some([0.0, 0.0, 1000.0, 1080.0]),
            None
        ));
        assert!(!point_covered(1094, 936, None, Some([0, 0, 1094, 1080])));
        assert!(point_covered(1094, 936, None, Some([1094, 936, 1095, 937])));
    }

    #[test]
    fn decode_formats_and_significant_delta() {
        // RGBA8_UNORM
        let lit = decode_texel(28, &[241, 183, 80, 255]).unwrap();
        let dark = decode_texel(28, &[50, 30, 21, 255]).unwrap();
        let jitter = decode_texel(28, &[240, 182, 81, 255]).unwrap();
        assert!(significant_change(&lit, &dark, ValueKind::ColorFloat).is_some());
        assert!(significant_change(&lit, &jitter, ValueKind::ColorFloat).is_none());

        // BGRA8_UNORM swizzles to RGBA
        let bgra = decode_texel(87, &[80, 183, 241, 255]).unwrap();
        assert_eq!(bgra, lit);

        // R9G9B9E5_SHAREDEXP: mantissa=256 (0.5 after /512), exp=16 (2^1) => 1.0
        let word = 256u32 | (128u32 << 9) | (64u32 << 18) | (16u32 << 27);
        let Decoded::Float(v, 3) = decode_texel(67, &word.to_le_bytes()).unwrap() else {
            panic!("expected rgb9e5 float3");
        };
        assert!((v[0] - 1.0).abs() < 1e-4);
        assert!((v[1] - 0.5).abs() < 1e-4);
        assert!((v[2] - 0.25).abs() < 1e-4);

        // R11G11B10_FLOAT: exp=15 (1.0) in each channel
        let w11 = (15u32 << 6) | ((15u32 << 6) << 11) | ((15u32 << 5) << 22);
        let Decoded::Float(v11, 3) = decode_texel(26, &w11.to_le_bytes()).unwrap() else {
            panic!("expected r11g11b10f float3");
        };
        assert!((v11[0] - 1.0).abs() < 1e-4);
        assert!((v11[1] - 1.0).abs() < 1e-4);
        assert!((v11[2] - 1.0).abs() < 1e-4);

        // R16G16B16A16_FLOAT: 1.0 (0x3c00), 0.5 (0x3800), -2.0 (0xc000), 0.0 (0x0000)
        let mut h = Vec::new();
        for bits in [0x3c00u16, 0x3800, 0xc000, 0x0000] {
            h.extend_from_slice(&bits.to_le_bytes());
        }
        let Decoded::Float(vf16, 4) = decode_texel(10, &h).unwrap() else {
            panic!("expected f16 float4");
        };
        assert_eq!(vf16, [1.0, 0.5, -2.0, 0.0]);

        // D24_UNORM_S8_UINT
        let d24_word = 0x0080_0000u32;
        let Decoded::Float(vd, 1) = decode_texel(45, &d24_word.to_le_bytes()).unwrap() else {
            panic!("expected d24 float1");
        };
        assert!((vd[0] - 0.5).abs() < 1e-4);

        // BC1 (71) is rejected cleanly.
        assert!(format_info(71).is_none());
    }

    #[test]
    fn tracker_emits_first_quiet_and_change_with_unchanged_summary() {
        let mut t = Tracker::default();
        let pt = parse_point("2188", "1872").unwrap();
        let mk = |kind, slot, fmt, raw: Vec<u8>, ps: &str| LandedSample {
            point_idx: 0,
            point: pt,
            kind,
            slot,
            resource: 0x1000,
            memory: 0x2000,
            width: 3840,
            height: 2160,
            format: fmt,
            tx: 2188,
            ty: 1872,
            tag: 0,
            draw_count: 3,
            covers: vec![DrawCover {
                shader_hash: ps.to_owned(),
                pipeline_name: "PS_Test".to_owned(),
                draws: 3,
                sig: 42,
                heap_indexed: false,
                textures: vec![BoundTex {
                    kind: b't',
                    register: 0,
                    resource: 0x3000,
                    memory: 0x4000,
                    width: 2560,
                    height: 1440,
                    format: 67,
                    slot: Some(123),
                    host: "made",
                }],
            }],
            raw,
            stencil: None,
        };

        // Frame 100: first baseline (albedo + lighting).
        let mut out = Vec::new();
        evaluate_batch(
            &mut t,
            100,
            1,
            &[
                mk(
                    PassKind::Rt,
                    0,
                    28,
                    vec![200, 150, 100, 255],
                    "aaaaaaaa11111111",
                ),
                mk(
                    PassKind::Rt,
                    0,
                    26,
                    ((15u32 << 6) | ((15u32 << 6) << 11) | ((15u32 << 5) << 22))
                        .to_le_bytes()
                        .to_vec(),
                    "bbbbbbbb22222222",
                ),
            ],
            &mut out,
        );
        assert_eq!(out.len(), 2);
        assert!(out[0].starts_with("pixel watch: first "));
        assert!(out[1].starts_with("pixel watch: first "));

        // Frame 101: tiny jitter only -> silent (except frame 100 summary on rollover).
        out.clear();
        evaluate_batch(
            &mut t,
            101,
            2,
            &[
                mk(
                    PassKind::Rt,
                    0,
                    28,
                    vec![201, 150, 99, 255],
                    "aaaaaaaa11111111",
                ),
                mk(
                    PassKind::Rt,
                    0,
                    26,
                    ((15u32 << 6) | ((15u32 << 6) << 11) | ((15u32 << 5) << 22))
                        .to_le_bytes()
                        .to_vec(),
                    "bbbbbbbb22222222",
                ),
            ],
            &mut out,
        );
        assert_eq!(out.len(), 1);
        assert!(out[0].contains("frame=100"));

        // Frame 102: albedo unchanged, lighting drops to zero -> CHANGE on lighting only!
        out.clear();
        evaluate_batch(
            &mut t,
            102,
            3,
            &[
                mk(
                    PassKind::Rt,
                    0,
                    28,
                    vec![200, 150, 100, 255],
                    "aaaaaaaa11111111",
                ),
                mk(
                    PassKind::Rt,
                    0,
                    26,
                    0u32.to_le_bytes().to_vec(),
                    "bbbbbbbb22222222",
                ),
            ],
            &mut out,
        );
        assert_eq!(out.len(), 1);
        assert!(out[0].starts_with("pixel watch: CHANGE frame=102"));
        assert!(out[0].contains("bbbbbbbb"));
        assert!(out[0].contains("t0:f67 2560x1440"));

        // Frame 103 rollover flushes frame 102 summary listing the unchanged albedo pass!
        out.clear();
        evaluate_batch(
            &mut t,
            103,
            4,
            &[mk(
                PassKind::Rt,
                0,
                28,
                vec![200, 150, 100, 255],
                "aaaaaaaa11111111",
            )],
            &mut out,
        );
        assert_eq!(out.len(), 1);
        assert!(out[0].contains("frame=102"));
        assert!(out[0].contains("changed=1"));
        assert!(out[0].contains("unchanged=[p0:rt0#0:f28(3840x2160)]"));
    }

    #[test]
    fn live_settings_round_trip() {
        victory_core::settings::check_live(
            "VICTORY_PIXEL_WATCH",
            "2188,1872,5",
            || (on(), config().every, config().count),
            (false, 0, 0),
            (true, 5, 1),
        );
        victory_core::settings::check_live(
            "VICTORY_PIXEL_WATCH_FRAMES",
            "2750..2810",
            || unpack_window(WINDOW.get()),
            Window::All,
            Window::Frames {
                start: 2750,
                end: 2810,
            },
        );
    }

    /// Real D3D12 (host GPU / WARP) round-trip test: clears/fills textures in RGBA8, RGBA16F,
    /// R11G11B10F, RGB9E5, R10G10B10A2, D32F (`R32_TYPELESS`), and D24S8 (`R24G8_TYPELESS` depth +
    /// stencil planes), copies 1x1 texels with `gpu::copy_texel`, and verifies exact decoded values!
    #[cfg(windows)]
    #[test]
    fn d3d12_1x1_copy_and_decode_color_and_depth_stencil() {
        use windows::Win32::Graphics::Direct3D12::*;
        use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_SAMPLE_DESC};

        let Some(host) = crate::host::host() else {
            return;
        };
        let device = &host.device;
        let Ok(list) = host.new_list() else {
            return;
        };
        let buf = gpu::take_buffer(device).expect("read-back buffer");
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_DEFAULT,
            ..Default::default()
        };
        let mk_tex = |fmt: i32, flags: D3D12_RESOURCE_FLAGS, init: D3D12_RESOURCE_STATES| {
            let desc = D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
                Width: 320,
                Height: 180,
                DepthOrArraySize: 1,
                MipLevels: 1,
                Format: DXGI_FORMAT(fmt),
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Layout: D3D12_TEXTURE_LAYOUT_UNKNOWN,
                Flags: flags,
                ..Default::default()
            };
            let mut res: Option<ID3D12Resource> = None;
            // SAFETY: plain texture creation.
            unsafe {
                device
                    .CreateCommittedResource(
                        &heap,
                        D3D12_HEAP_FLAG_NONE,
                        &desc,
                        init,
                        None,
                        &mut res,
                    )
                    .expect("create test texture");
            }
            (res.unwrap(), desc)
        };

        // 1) RGBA8_UNORM RTV cleared to [0.5, 0.25, 0.75, 1.0].
        let (rt_rgba8, desc_rgba8) = mk_tex(
            28,
            D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
            D3D12_RESOURCE_STATE_RENDER_TARGET,
        );
        // 2) R16G16B16A16_FLOAT RTV cleared to [1.5, 0.5, 0.25, 1.0].
        let (rt_f16, desc_f16) = mk_tex(
            10,
            D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
            D3D12_RESOURCE_STATE_RENDER_TARGET,
        );
        // 3) R11G11B10_FLOAT RTV cleared to [2.0, 0.5, 0.25, 0.0].
        let (rt_11, desc_11) = mk_tex(
            26,
            D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
            D3D12_RESOURCE_STATE_RENDER_TARGET,
        );
        // 4) R32_TYPELESS DSV (cleared via D32_FLOAT view to 0.375).
        let (ds_32, desc_32) = mk_tex(
            39,
            D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL,
            D3D12_RESOURCE_STATE_DEPTH_WRITE,
        );
        // 5) R24G8_TYPELESS DSV (cleared via D24_UNORM_S8_UINT view to depth=0.625, stencil=0x2a).
        let (ds_24, desc_24) = mk_tex(
            44,
            D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL,
            D3D12_RESOURCE_STATE_DEPTH_WRITE,
        );
        // 6) R32G8X24_TYPELESS DSV (cleared via D32_FLOAT_S8X24_UINT to depth=0.125, stencil=0x7b).
        let (ds_32s8, desc_32s8) = mk_tex(
            19,
            D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL,
            D3D12_RESOURCE_STATE_DEPTH_WRITE,
        );
        // 7) R10G10B10A2_UNORM RTV cleared to [0.25, 0.5, 0.75, 1.0].
        let (rt_10, desc_10) = mk_tex(
            24,
            D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
            D3D12_RESOURCE_STATE_RENDER_TARGET,
        );

        // Create tiny RTV and DSV heaps to clear them on the GPU.
        let rtv_heap: ID3D12DescriptorHeap = unsafe {
            device
                .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: D3D12_DESCRIPTOR_HEAP_TYPE_RTV,
                    NumDescriptors: 4,
                    ..Default::default()
                })
                .unwrap()
        };
        let dsv_heap: ID3D12DescriptorHeap = unsafe {
            device
                .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: D3D12_DESCRIPTOR_HEAP_TYPE_DSV,
                    NumDescriptors: 3,
                    ..Default::default()
                })
                .unwrap()
        };
        let rtv_step =
            unsafe { device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_RTV) }
                as usize;
        let dsv_step =
            unsafe { device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_DSV) }
                as usize;
        let rtv0 = unsafe { rtv_heap.GetCPUDescriptorHandleForHeapStart() };
        let rtv1 = D3D12_CPU_DESCRIPTOR_HANDLE {
            ptr: rtv0.ptr + rtv_step,
        };
        let rtv2 = D3D12_CPU_DESCRIPTOR_HANDLE {
            ptr: rtv0.ptr + 2 * rtv_step,
        };
        let rtv3 = D3D12_CPU_DESCRIPTOR_HANDLE {
            ptr: rtv0.ptr + 3 * rtv_step,
        };
        let dsv0 = unsafe { dsv_heap.GetCPUDescriptorHandleForHeapStart() };
        let dsv1 = D3D12_CPU_DESCRIPTOR_HANDLE {
            ptr: dsv0.ptr + dsv_step,
        };
        let dsv2 = D3D12_CPU_DESCRIPTOR_HANDLE {
            ptr: dsv0.ptr + 2 * dsv_step,
        };
        unsafe {
            device.CreateRenderTargetView(&rt_rgba8, None, rtv0);
            device.CreateRenderTargetView(&rt_f16, None, rtv1);
            device.CreateRenderTargetView(&rt_11, None, rtv2);
            device.CreateRenderTargetView(&rt_10, None, rtv3);
            device.CreateDepthStencilView(
                &ds_32,
                Some(&D3D12_DEPTH_STENCIL_VIEW_DESC {
                    Format: DXGI_FORMAT(40),
                    ViewDimension: D3D12_DSV_DIMENSION_TEXTURE2D,
                    ..Default::default()
                }),
                dsv0,
            );
            device.CreateDepthStencilView(
                &ds_24,
                Some(&D3D12_DEPTH_STENCIL_VIEW_DESC {
                    Format: DXGI_FORMAT(45),
                    ViewDimension: D3D12_DSV_DIMENSION_TEXTURE2D,
                    ..Default::default()
                }),
                dsv1,
            );
            device.CreateDepthStencilView(
                &ds_32s8,
                Some(&D3D12_DEPTH_STENCIL_VIEW_DESC {
                    Format: DXGI_FORMAT(20),
                    ViewDimension: D3D12_DSV_DIMENSION_TEXTURE2D,
                    ..Default::default()
                }),
                dsv2,
            );
            list.list
                .ClearRenderTargetView(rtv0, &[0.5, 0.25, 0.75, 1.0], None);
            list.list
                .ClearRenderTargetView(rtv1, &[1.5, 0.5, 0.25, 1.0], None);
            list.list
                .ClearRenderTargetView(rtv2, &[2.0, 0.5, 0.25, 0.0], None);
            list.list
                .ClearRenderTargetView(rtv3, &[0.25, 0.5, 0.75, 1.0], None);
            list.list
                .ClearDepthStencilView(dsv0, D3D12_CLEAR_FLAG_DEPTH, 0.375, 0, None);
            list.list.ClearDepthStencilView(
                dsv1,
                D3D12_CLEAR_FLAG_DEPTH | D3D12_CLEAR_FLAG_STENCIL,
                0.625,
                0x2a,
                None,
            );
            list.list.ClearDepthStencilView(
                dsv2,
                D3D12_CLEAR_FLAG_DEPTH | D3D12_CLEAR_FLAG_STENCIL,
                0.125,
                0x7b,
                None,
            );
            list.list.ResourceBarrier(&[
                crate::host::transition(
                    &rt_rgba8,
                    D3D12_RESOURCE_STATE_RENDER_TARGET,
                    D3D12_RESOURCE_STATE_COPY_SOURCE,
                ),
                crate::host::transition(
                    &rt_f16,
                    D3D12_RESOURCE_STATE_RENDER_TARGET,
                    D3D12_RESOURCE_STATE_COPY_SOURCE,
                ),
                crate::host::transition(
                    &rt_11,
                    D3D12_RESOURCE_STATE_RENDER_TARGET,
                    D3D12_RESOURCE_STATE_COPY_SOURCE,
                ),
                crate::host::transition(
                    &rt_10,
                    D3D12_RESOURCE_STATE_RENDER_TARGET,
                    D3D12_RESOURCE_STATE_COPY_SOURCE,
                ),
                crate::host::transition(
                    &ds_32,
                    D3D12_RESOURCE_STATE_DEPTH_WRITE,
                    D3D12_RESOURCE_STATE_COPY_SOURCE,
                ),
                crate::host::transition(
                    &ds_24,
                    D3D12_RESOURCE_STATE_DEPTH_WRITE,
                    D3D12_RESOURCE_STATE_COPY_SOURCE,
                ),
                crate::host::transition(
                    &ds_32s8,
                    D3D12_RESOURCE_STATE_DEPTH_WRITE,
                    D3D12_RESOURCE_STATE_COPY_SOURCE,
                ),
            ]);
        }

        let (tx, ty) = map_pixel(0.5698, 0.8667, 320, 180);
        let specs: [(&ID3D12Resource, &D3D12_RESOURCE_DESC, u32, u32, u32); 9] = [
            (&rt_rgba8, &desc_rgba8, 0, 28, 0),
            (&rt_f16, &desc_f16, 0, 10, 1),
            (&rt_11, &desc_11, 0, 26, 2),
            (&ds_32, &desc_32, 0, 39, 3),
            (&ds_24, &desc_24, 0, 44, 4),
            (&ds_24, &desc_24, 1, 62, 5), // plane 1 stencil!
            (&ds_32s8, &desc_32s8, 0, 19, 6),
            (&ds_32s8, &desc_32s8, 1, 62, 7), // plane 1 stencil!
            (&rt_10, &desc_10, 0, 24, 8),
        ];
        for (res, desc, plane, fmt, slot) in specs {
            let info = format_info(fmt).unwrap();
            let (cfmt, pitch) = gpu::plane_footprint(device, desc, plane, info.copy_format)
                .unwrap_or_else(|| panic!("footprint failed for fmt={fmt} plane={plane}"));
            gpu::copy_texel(&list.list, &buf.0, slot, res, plane, cfmt, pitch, tx, ty);
        }
        list.close();
        host.execute(&[&list]);

        let mut data = std::ptr::null_mut();
        unsafe {
            buf.0.Map(0, None, Some(&mut data)).unwrap();
            let all = std::slice::from_raw_parts(data.cast::<u8>(), BATCH_BYTES as usize);
            let Decoded::Float(c8, 4) =
                decode_texel(28, &gpu::read_slot(all, 0, 4).unwrap()).unwrap()
            else {
                panic!()
            };
            assert!(
                (c8[0] - 0.5).abs() < 0.01
                    && (c8[1] - 0.25).abs() < 0.01
                    && (c8[2] - 0.75).abs() < 0.01
            );

            let Decoded::Float(c16, 4) =
                decode_texel(10, &gpu::read_slot(all, 1, 8).unwrap()).unwrap()
            else {
                panic!()
            };
            assert!(
                (c16[0] - 1.5).abs() < 0.01
                    && (c16[1] - 0.5).abs() < 0.01
                    && (c16[2] - 0.25).abs() < 0.01
            );

            let Decoded::Float(c11, 3) =
                decode_texel(26, &gpu::read_slot(all, 2, 4).unwrap()).unwrap()
            else {
                panic!()
            };
            assert!(
                (c11[0] - 2.0).abs() < 0.01
                    && (c11[1] - 0.5).abs() < 0.01
                    && (c11[2] - 0.25).abs() < 0.01
            );

            let Decoded::Float(d32, 1) =
                decode_texel(39, &gpu::read_slot(all, 3, 4).unwrap()).unwrap()
            else {
                panic!()
            };
            assert!((d32[0] - 0.375).abs() < 1e-4, "d32={d32:?}");

            let Decoded::Float(d24, 1) =
                decode_texel(44, &gpu::read_slot(all, 4, 4).unwrap()).unwrap()
            else {
                panic!()
            };
            assert!((d24[0] - 0.625).abs() < 1e-4, "d24={d24:?}");

            let st = gpu::read_slot(all, 5, 1).unwrap()[0];
            assert_eq!(st, 0x2a, "d24s8 stencil mismatch");

            let Decoded::Float(d32s8, 1) =
                decode_texel(19, &gpu::read_slot(all, 6, 8).unwrap()).unwrap()
            else {
                panic!()
            };
            assert!((d32s8[0] - 0.125).abs() < 1e-4, "d32s8={d32s8:?}");

            let st32 = gpu::read_slot(all, 7, 1).unwrap()[0];
            assert_eq!(st32, 0x7b, "d32s8 stencil mismatch");

            let Decoded::Float(c10, 4) =
                decode_texel(24, &gpu::read_slot(all, 8, 4).unwrap()).unwrap()
            else {
                panic!()
            };
            assert!(
                (c10[0] - 0.25).abs() < 0.01
                    && (c10[1] - 0.5).abs() < 0.01
                    && (c10[2] - 0.75).abs() < 0.01
                    && (c10[3] - 1.0).abs() < 0.01,
                "c10={c10:?}"
            );
            buf.0.Unmap(0, None);
        }
        gpu::return_buffer(buf);
    }
}
