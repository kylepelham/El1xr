//! Frame pacing at the present, measured always (release builds too) and steerable.
//!
//! Every present's interval since the one before goes into a window of about 5 s; when the
//! window ends one `frame pacing:` line gives the frame time's p50/p95/p99/max, the frames over
//! 50 and 100 ms, the spread (standard deviation) of the present intervals and their jitter (the
//! mean change from one interval to the next). `tools/dev.py bench` reads the line.
//!
//! How frames are presented (the title paces itself through its frame interval clock,
//! `frame.rs`: 33.3 ms for GTA VI, so it is capped at an even 30 fps whenever it keeps up):
//! - `VICTORY_PRESENT_SYNC=<0..4>`: the sync interval (default 0: no wait for a blank).
//! - `VICTORY_PRESENT_TEARING=1`: a swap chain and presents that allow tearing (variable refresh
//!   displays show each frame when it is ready), when the system supports it and the sync
//!   interval is 0.
//! - `VICTORY_PRESENT_CAP=<fps>`: at most that many presents a second, each held until its turn
//!   on an even grid (a late present goes at once and the grid starts again from it).
//! - `VICTORY_PRESENT_SMOOTH=<fraction>`: each present held until at least that fraction of the
//!   recent mean interval has passed since the one before (a quick frame after a slow one waits).
//!   Default 0.9, `0` turns it off. Measured on GTA VI at 19-21 fps (200 s benches): present
//!   interval jitter 12.7-15.3 ms to 6.2 ms and its spread 11.3 ms to 7.2 ms, at the same fps.
//! - (bootfast) `VICTORY_PRESENT_SMOOTH_READY=1`: the running mean takes each frame's interval
//!   up to when its present was ready, not the interval after the hold. With the held interval
//!   the mean can fall by at most 1% a frame (a held frame is 0.9 of the mean, weighted 0.1), so
//!   after the boot frames (capped at 100 ms) the loading screen was held at 14-24 fps for about
//!   6 s with the title waiting on its present turn (batch 4: 0.991-0.993 a frame, frames 25-200).
//!   `=2`: also no hold before the world's first frame (`crate::load_rate::in_world`).

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use windows::Win32::Graphics::Dxgi::*;
use windows::core::Interface;

/// How long a window of present intervals lasts before its line is logged.
const WINDOW: Duration = Duration::from_secs(5);
/// A frame slower than this is a visible hitch at a 30 fps target.
const SLOW_US: u32 = 50_000;
/// A frame slower than this is a stutter.
const STUTTER_US: u32 = 100_000;
/// `VICTORY_PRESENT_SMOOTH` when not set.
const SMOOTH_DEFAULT: f64 = 0.9;
/// `VICTORY_PRESENT_SMOOTH`: the weight of the newest interval in the running mean, and the
/// longest interval taken into it (a load's stall would hold presents for long after).
const SMOOTH_WEIGHT: f64 = 0.1;
const SMOOTH_MAX_US: f64 = 100_000.0;

struct Config {
    sync: u32,
    tearing: bool,
    cap: Option<Duration>,
    smooth: Option<f64>,
}

fn config() -> &'static Config {
    static CONFIG: OnceLock<Config> = OnceLock::new();
    CONFIG.get_or_init(|| {
        let var = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
        let sync = parse_sync(var("VICTORY_PRESENT_SYNC").as_deref());
        let tearing = var("VICTORY_PRESENT_TEARING").is_some_and(|v| v == "1");
        let cap = parse_cap(var("VICTORY_PRESENT_CAP").as_deref());
        let smooth = parse_smooth(var("VICTORY_PRESENT_SMOOTH").as_deref());
        tracing::info!(
            sync,
            tearing,
            cap = ?cap,
            smooth = ?smooth,
            "presenting (VICTORY_PRESENT_SYNC, _TEARING, _CAP, _SMOOTH)"
        );
        Config {
            sync,
            tearing,
            cap,
            smooth,
        }
    })
}

fn parse_sync(v: Option<&str>) -> u32 {
    v.filter(|v| !v.is_empty())
        .and_then(|v| v.parse::<u32>().ok())
        .map_or(0, |v| v.min(4))
}

fn parse_cap(v: Option<&str>) -> Option<Duration> {
    v.filter(|v| !v.is_empty())
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|fps| *fps > 0.0)
        .map(|fps| Duration::from_secs_f64(1.0 / fps))
}

fn parse_smooth(v: Option<&str>) -> Option<f64> {
    v.filter(|v| !v.is_empty())
        .and_then(|v| v.parse::<f64>().ok())
        .map_or(Some(SMOOTH_DEFAULT), |f| (f > 0.0).then_some(f.min(1.0)))
}

/// The sync interval, cap and smoothing, live: a settings change (`set`) reaches
/// them while the title runs (`victory_core::settings`). Parsed as at
/// start; a cap of 0 ns and a smoothing of 0 stand for none.
static SYNC: victory_core::settings::Live =
    victory_core::settings::Live::new("VICTORY_PRESENT_SYNC", |v| u64::from(parse_sync(v)));
static CAP: victory_core::settings::Live =
    victory_core::settings::Live::new("VICTORY_PRESENT_CAP", |v| {
        parse_cap(v).map_or(0, |d| d.as_nanos() as u64)
    });
static SMOOTH: victory_core::settings::Live =
    victory_core::settings::Live::new("VICTORY_PRESENT_SMOOTH", |v| {
        parse_smooth(v).unwrap_or(0.0).to_bits()
    });

/// `VICTORY_PRESENT_SMOOTH_READY` (0, 1 or 2), live.
static SMOOTH_READY: victory_core::settings::Live =
    victory_core::settings::Live::new("VICTORY_PRESENT_SMOOTH_READY", |v| {
        u64::from(parse_smooth_ready(v))
    });

fn parse_smooth_ready(v: Option<&str>) -> u8 {
    match v.map(str::trim) {
        Some("1") => 1,
        Some("2") => 2,
        _ => 0,
    }
}

/// Microseconds presents were held by `VICTORY_PRESENT_SMOOTH` since the last take
/// (`crate::boot_prof`).
static HELD_US: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub(crate) fn take_held_us() -> u64 {
    HELD_US.swap(0, Ordering::Relaxed)
}

/// The interval the running mean takes: the held one, or with `ready` (the switch on) the one
/// up to when the present was ready.
fn mean_sample_us(held_us: u32, ready_us: u32, ready: bool) -> f64 {
    f64::from(if ready { ready_us } else { held_us }).min(SMOOTH_MAX_US)
}

/// The running mean after a sample.
fn next_mean(mean_us: f64, sample_us: f64) -> f64 {
    if mean_us == 0.0 {
        sample_us
    } else {
        mean_us + SMOOTH_WEIGHT * (sample_us - mean_us)
    }
}

/// What [`present`] paces by now: the start's values until a setting
/// changes one (logged when it does).
fn live() -> (u32, Option<Duration>, Option<f64>) {
    let c = config();
    let sync = SYNC.get() as u32;
    let cap = Some(CAP.get())
        .filter(|&ns| ns > 0)
        .map(Duration::from_nanos);
    let smooth = Some(SMOOTH.get_f64()).filter(|&f| f > 0.0);
    static LOGGED: Mutex<Option<(u32, Option<Duration>, Option<f64>)>> = Mutex::new(None);
    let now = (sync, cap, smooth);
    let mut logged = LOGGED.lock().unwrap_or_else(|e| e.into_inner());
    if *logged.get_or_insert((c.sync, c.cap, c.smooth)) != now {
        *logged = Some(now);
        tracing::info!(sync, cap = ?cap, smooth = ?smooth, "presenting changed (in-game settings)");
    }
    now
}

/// The swap chain flags chosen when it was made, for `ResizeBuffers` (which must keep them).
static CHAIN_FLAGS: AtomicU32 = AtomicU32::new(0);

/// The flags to make the swap chain with: tearing allowed when asked for, the sync interval is
/// 0 and the system supports it.
pub(crate) fn swap_chain_flags(factory: &IDXGIFactory2) -> DXGI_SWAP_CHAIN_FLAG {
    let c = config();
    let mut flags = 0;
    if c.tearing && c.sync == 0 {
        let mut allowed = windows::core::BOOL(0);
        // SAFETY: a feature query into a BOOL of the size given.
        let supported = factory.cast::<IDXGIFactory5>().is_ok_and(|f| unsafe {
            f.CheckFeatureSupport(
                DXGI_FEATURE_PRESENT_ALLOW_TEARING,
                (&raw mut allowed).cast(),
                std::mem::size_of::<windows::core::BOOL>() as u32,
            )
            .is_ok()
        }) && allowed.as_bool();
        tracing::info!(
            supported,
            "presents that allow tearing (VICTORY_PRESENT_TEARING=1)"
        );
        if supported {
            flags = DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING.0 as u32;
        }
    }
    CHAIN_FLAGS.store(flags, Ordering::Relaxed);
    DXGI_SWAP_CHAIN_FLAG(flags as i32)
}

/// The flags the swap chain was made with.
pub(crate) fn chain_flags() -> DXGI_SWAP_CHAIN_FLAG {
    DXGI_SWAP_CHAIN_FLAG(CHAIN_FLAGS.load(Ordering::Relaxed) as i32)
}

/// One window of presents, and what pacing them keeps across windows.
struct Window {
    since: Instant,
    drawn: u64,
    last: Option<Instant>,
    /// Present-to-present intervals, microseconds, in order.
    intervals: Vec<u32>,
    /// The next present's turn under `VICTORY_PRESENT_CAP`.
    turn: Option<Instant>,
    /// The running mean interval for `VICTORY_PRESENT_SMOOTH`, microseconds.
    mean_us: f64,
    /// Microseconds held by `VICTORY_PRESENT_SMOOTH` in this window, and the presents held.
    held_us: u64,
    held: u32,
}

static WINDOW_STATE: Mutex<Option<Window>> = Mutex::new(None);

/// Presents the swap chain's current buffer as configured and times it.
pub(crate) fn present(swap_chain: &IDXGISwapChain3) {
    if crate::camera::on() {
        crate::camera::frame_end(|at, out| crate::host::read_into(at as *const u8, out));
    }
    let (sync, cap, smooth) = live();
    let mut guard = WINDOW_STATE.lock().unwrap_or_else(|e| e.into_inner());
    let w = guard.get_or_insert_with(|| Window {
        since: Instant::now(),
        drawn: crate::draw::drawn(),
        last: None,
        intervals: Vec::with_capacity(512),
        turn: None,
        mean_us: 0.0,
        held_us: 0,
        held: 0,
    });
    let ready_mode = SMOOTH_READY.get() as u8;
    let ready = Instant::now();
    // Frame generation's proxy swap chain paces its own presents.
    let paced_by_proxy = crate::framegen::active();
    if let Some(period) = cap.filter(|_| !paced_by_proxy) {
        let now = Instant::now();
        let at = match w.turn {
            Some(turn) if turn > now => {
                std::thread::sleep(turn - now);
                turn
            }
            _ => now,
        };
        w.turn = Some(at + period);
    }
    let loading_free = ready_mode >= 2 && !crate::load_rate::in_world();
    if let (Some(fraction), Some(last), false, false) = (smooth, w.last, paced_by_proxy, loading_free)
    {
        let hold = last + Duration::from_micros((w.mean_us * fraction) as u64);
        let now = Instant::now();
        if hold > now {
            std::thread::sleep(hold - now);
            let us = (hold - now).as_micros() as u64;
            w.held_us += us;
            w.held += 1;
            if crate::boot_prof::on() {
                HELD_US.fetch_add(us, Ordering::Relaxed);
            }
        }
    }
    // Tearing goes with a sync interval of 0 only (DXGI refuses the present otherwise).
    let flags = if CHAIN_FLAGS.load(Ordering::Relaxed) != 0 && sync == 0 {
        DXGI_PRESENT_ALLOW_TEARING
    } else {
        DXGI_PRESENT(0)
    };
    // The present goes to the host queue behind the frame's lists (`VICTORY_SEND_THREAD`).
    crate::host::send_thread::wait_idle();
    let dlssg = crate::dlssg::active();
    if dlssg {
        crate::dlssg::before_present();
    }
    // SAFETY: our swap chain; its buffer was written and moved to the present state.
    let _ = unsafe { swap_chain.Present(sync, flags) };
    if dlssg {
        crate::dlssg::after_present();
    }
    let now = Instant::now();
    if let Some(last) = w.last {
        let us = now
            .duration_since(last)
            .as_micros()
            .min(u128::from(u32::MAX)) as u32;
        w.intervals.push(us);
        let ready_us = ready
            .saturating_duration_since(last)
            .as_micros()
            .min(u128::from(u32::MAX)) as u32;
        w.mean_us = next_mean(w.mean_us, mean_sample_us(us, ready_us, ready_mode > 0));
    }
    w.last = Some(now);
    let elapsed = w.since.elapsed();
    if elapsed < WINDOW {
        return;
    }
    let drawn = crate::draw::drawn();
    let frames = w.intervals.len();
    let draws = if frames > 0 {
        (drawn - w.drawn) as f64 / frames as f64
    } else {
        0.0
    };
    if let Some(s) = summarize(&mut w.intervals) {
        tracing::info!("{}", line(elapsed.as_secs_f64(), &s, draws));
    }
    if ready_mode > 0 || crate::boot_prof::on() {
        tracing::info!(
            held_ms = w.held_us / 1000,
            held = w.held,
            frames,
            mean_ms = format_args!("{:.1}", w.mean_us / 1000.0),
            ready_mode,
            world = crate::load_rate::in_world(),
            "present smoothing in the window: presents held to the recent mean (VICTORY_PRESENT_SMOOTH_READY)"
        );
    }
    (w.held_us, w.held) = (0, 0);
    (w.since, w.drawn) = (Instant::now(), drawn);
    w.intervals.clear();
}

/// A window's frame times, in milliseconds.
#[derive(Debug, PartialEq)]
pub(crate) struct Summary {
    pub frames: usize,
    pub p50: f64,
    pub p95: f64,
    pub p99: f64,
    pub max: f64,
    pub over_50: usize,
    pub over_100: usize,
    pub sd: f64,
    /// The mean change from one interval to the next.
    pub jitter: f64,
}

/// The percentiles (nearest rank), the slow frames, the standard deviation and the jitter of
/// `us` (microseconds, in present order); sorts it. `None` when empty.
pub(crate) fn summarize(us: &mut [u32]) -> Option<Summary> {
    if us.is_empty() {
        return None;
    }
    let n = us.len();
    let jitter = us
        .windows(2)
        .map(|p| f64::from(p[0].abs_diff(p[1])))
        .sum::<f64>()
        / (n.max(2) - 1) as f64
        / 1e3;
    us.sort_unstable();
    let rank = |p: f64| f64::from(us[((p * n as f64).ceil() as usize).clamp(1, n) - 1]) / 1e3;
    let mean = us.iter().map(|&v| f64::from(v)).sum::<f64>() / n as f64;
    let var = us
        .iter()
        .map(|&v| (f64::from(v) - mean).powi(2))
        .sum::<f64>()
        / n as f64;
    Some(Summary {
        frames: n,
        p50: rank(0.50),
        p95: rank(0.95),
        p99: rank(0.99),
        max: f64::from(us[n - 1]) / 1e3,
        over_50: us.iter().filter(|&&v| v > SLOW_US).count(),
        over_100: us.iter().filter(|&&v| v > STUTTER_US).count(),
        sd: var.sqrt() / 1e3,
        jitter,
    })
}

/// The line logged for a window of `secs` seconds (`tools/dev.py` parses it).
pub(crate) fn line(secs: f64, s: &Summary, draws: f64) -> String {
    format!(
        "frame pacing: {} frames in {secs:.1} s ({:.1} fps), frame time p50 {:.1} ms p95 {:.1} ms \
         p99 {:.1} ms max {:.1} ms, {} over 50 ms, {} over 100 ms, present interval sd {:.1} ms \
         jitter {:.1} ms; {draws:.0} draws/frame",
        s.frames,
        s.frames as f64 / secs,
        s.p50,
        s.p95,
        s.p99,
        s.max,
        s.over_50,
        s.over_100,
        s.sd,
        s.jitter,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_live_cells_hold_what_the_start_parsed() {
        // Unset, empty, junk and real values: the live cells' bits read back as
        // the start's `Config` fields, so nothing changes until a setting does.
        for v in [
            None,
            Some(""),
            Some("x"),
            Some("0"),
            Some("-3"),
            Some("1"),
            Some("2"),
            Some("9"),
            Some("30"),
            Some("59.94"),
            Some("0.5"),
            Some("1.7"),
        ] {
            let cap = parse_cap(v).map_or(0, |d| d.as_nanos() as u64);
            assert_eq!(
                Some(cap).filter(|&ns| ns > 0).map(Duration::from_nanos),
                parse_cap(v),
                "{v:?}"
            );
            let smooth = f64::from_bits(parse_smooth(v).unwrap_or(0.0).to_bits());
            assert_eq!(Some(smooth).filter(|&f| f > 0.0), parse_smooth(v), "{v:?}");
            assert_eq!(u64::from(parse_sync(v)) as u32, parse_sync(v), "{v:?}");
        }
        assert_eq!(parse_smooth(None), Some(SMOOTH_DEFAULT));
        assert_eq!(parse_smooth(Some("0")), None);
        assert_eq!(parse_sync(Some("9")), 4);
    }

    #[test]
    fn summary_takes_nearest_rank_percentiles_and_counts_slow_frames() {
        // 97 frames of 33.3 ms, one of 60, one of 120, one of 200.
        let mut us: Vec<u32> = std::iter::repeat_n(33_300, 97).collect();
        us.extend([200_000, 60_000, 120_000]);
        let s = summarize(&mut us).unwrap();
        assert_eq!(s.frames, 100);
        assert_eq!(s.p50, 33.3);
        assert_eq!(s.p95, 33.3);
        assert_eq!(s.p99, 120.0);
        assert_eq!(s.max, 200.0);
        assert_eq!((s.over_50, s.over_100), (3, 2));
        assert!(s.sd > 15.0 && s.sd < 25.0, "{}", s.sd);
        assert!(summarize(&mut []).is_none());
    }

    #[test]
    fn even_frames_have_no_spread_or_jitter() {
        let s = summarize(&mut [33_333; 10]).unwrap();
        assert_eq!((s.sd, s.jitter), (0.0, 0.0));
        assert_eq!((s.over_50, s.over_100), (0, 0));
        assert_eq!(summarize(&mut [40_000]).unwrap().jitter, 0.0);
    }

    #[test]
    fn jitter_follows_present_order_not_sorted_order() {
        // Same intervals: alternating is jittery, a slow ramp is not.
        let alternating = summarize(&mut [30_000, 50_000, 30_000, 50_000]).unwrap();
        let ramp = summarize(&mut [30_000, 30_000, 50_000, 50_000]).unwrap();
        assert_eq!(alternating.sd, ramp.sd);
        assert_eq!(alternating.jitter, 20.0);
        assert!((ramp.jitter - 20.0 / 3.0).abs() < 1e-9);
    }

    /// Frames ready every 20 ms after boot frames that left the mean at 100 ms: held to the
    /// mean, the mean falls 1% a frame (still over 60 ms after 50 frames); from the ready
    /// intervals it is near 20 ms within 30 frames.
    #[test]
    fn the_mean_follows_ready_frames_down_with_the_switch() {
        let run = |ready: bool, frames: usize| {
            let mut mean: f64 = 100_000.0;
            for _ in 0..frames {
                let natural = 20_000u32;
                let held = (mean * 0.9).max(f64::from(natural)) as u32;
                mean = next_mean(mean, mean_sample_us(held, natural, ready));
            }
            mean
        };
        assert!(run(false, 50) > 60_000.0, "{}", run(false, 50));
        assert!(run(true, 30) < 25_000.0, "{}", run(true, 30));
        // Off: the held interval, capped, as before.
        assert_eq!(mean_sample_us(150_000, 20_000, false), SMOOTH_MAX_US);
        assert_eq!(mean_sample_us(40_000, 20_000, true), 20_000.0);
        assert_eq!(next_mean(0.0, 33_000.0), 33_000.0);
        assert_eq!(parse_smooth_ready(Some("2")), 2);
        assert_eq!(parse_smooth_ready(Some("x")), 0);
        assert_eq!(parse_smooth_ready(None), 0);
    }

    /// `tools/dev.py` and `tests/test_dev_pacing.py` read this exact shape.
    #[test]
    fn line_has_the_shape_dev_py_reads() {
        let s = summarize(&mut [40_000, 45_000, 52_000, 110_000]).unwrap();
        assert_eq!(
            line(5.0, &s, 4830.4),
            "frame pacing: 4 frames in 5.0 s (0.8 fps), frame time p50 45.0 ms p95 110.0 ms \
             p99 110.0 ms max 110.0 ms, 2 over 50 ms, 1 over 100 ms, present interval sd 28.2 ms \
             jitter 23.3 ms; 4830 draws/frame"
        );
    }
}
