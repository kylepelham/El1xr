//! (El1xr) Writes a report bundle on a thread of its own, below normal priority: the presented
//! frames read back before and at the press as PNGs, `frametimes.csv` (the last ten seconds, one
//! row per present), `switches.txt` (every `VICTORY_*` / `ANYXBOXX_*` in effect, and the settings
//! changed in the menu while running) and `summary.txt`. Pictures come only from the presented
//! texture ([`crate::screenshot::to_rgb`]); nothing reads the screen.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use windows::Win32::Graphics::Direct3D12::*;

use super::ring::{self, Entry};
use super::trigger::Cause;

/// How long a picture's copy may take to land before it is skipped.
const LAND_TIMEOUT: Duration = Duration::from_secs(10);
const LAND_POLL: Duration = Duration::from_millis(5);

pub(crate) struct Buffer(pub ID3D12Resource);
// SAFETY: D3D12 objects are free-threaded.
unsafe impl Send for Buffer {}

pub(crate) struct Picture {
    /// `before-2`, `before-1`, `at`.
    pub name: &'static str,
    pub frame: u64,
    /// Seconds before the press (0 for the frame at it).
    pub before_s: f64,
    pub buffer: Buffer,
    pub footprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT,
    /// The submission that copies it (0: already waited for).
    pub flight: u64,
}

pub(crate) struct Job {
    pub folder: PathBuf,
    pub cause: Cause,
    pub frame: u64,
    pub pressed: Instant,
    pub pictures: Vec<Picture>,
    pub frames: Vec<Entry>,
    /// Why the frame at the press is missing, when it is.
    pub note: Option<String>,
    /// The selftest pass (its seconds) that pressed.
    pub selftest: Option<u64>,
}

/// `report-<date>_<time>-f<frame>` under `root`, with `-2`, `-3`... when taken.
pub(crate) fn folder_in(root: &Path, stamp: &str, frame: u64) -> PathBuf {
    let stem = format!("report-{stamp}-f{frame}");
    let mut path = root.join(&stem);
    for n in 2.. {
        if !path.exists() {
            break;
        }
        path = root.join(format!("{stem}-{n}"));
    }
    path
}

pub(crate) fn stamp() -> String {
    use windows_sys::Win32::System::SystemInformation::GetLocalTime;
    // SAFETY: a plain struct filled by the call.
    let t = unsafe {
        let mut t = std::mem::zeroed();
        GetLocalTime(&mut t);
        t
    };
    format!(
        "{:04}-{:02}-{:02}_{:02}-{:02}-{:02}",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
    )
}

/// `frame,t_s,ms` per present, `t_s` the seconds from the press (negative before it).
pub(crate) fn csv(frames: &[Entry], pressed: Instant) -> String {
    let mut out = String::from("frame,t_s,ms\n");
    for e in frames {
        let t = if e.at >= pressed {
            e.at.duration_since(pressed).as_secs_f64()
        } else {
            -pressed.duration_since(e.at).as_secs_f64()
        };
        let _ = writeln!(out, "{},{t:.4},{:.3}", e.frame, e.ms);
    }
    out
}

/// `NAME=value` for every `VICTORY_*` and `ANYXBOXX_*` in `env`, sorted; then each setting the menu
/// changed while running (`changed`: name, value now, whether live).
pub(crate) fn switches(
    env: impl Iterator<Item = (String, String)>,
    changed: &[(String, String, bool)],
) -> String {
    let mut vars: Vec<(String, String)> = env
        .filter(|(k, _)| k.starts_with("VICTORY_") || k.starts_with("ANYXBOXX_"))
        .collect();
    vars.sort();
    let mut out = String::from("# switches in the environment at start\n");
    for (k, v) in &vars {
        let _ = writeln!(out, "{k}={v}");
    }
    if !changed.is_empty() {
        out.push_str("# changed in the menu while running\n");
        for (k, v, live) in changed {
            let when = if *live {
                "in effect now"
            } else {
                "at the next start"
            };
            let _ = writeln!(out, "{k}={v} ({when})");
        }
    }
    out
}

/// The settings whose value now differs from the one the game started with.
pub(crate) fn changed_settings() -> Vec<(String, String, bool)> {
    use victory_core::settings;
    settings::infos()
        .iter()
        .filter_map(|i| {
            let now = settings::get(i.name)?;
            (Some(&now) != settings::launched(i.name).as_ref()).then(|| {
                (
                    i.name.to_string(),
                    now,
                    settings::apply_of(i.name) == Some(settings::Apply::Live),
                )
            })
        })
        .collect()
}

pub(crate) fn summary(job: &Job, written: &[(String, u64, f64)]) -> String {
    let s = ring::summarise(&job.frames);
    let mut out = String::new();
    let _ = writeln!(
        out,
        "report key: frame={} folder={}",
        job.frame,
        job.folder.display()
    );
    let _ = writeln!(
        out,
        "pressed by: {}{}",
        job.cause.name(),
        job.selftest.map_or(String::new(), |s| format!(
            " (VICTORY_REPORT_SELFTEST pass at {s}s)"
        ))
    );
    if let Some(note) = &job.note {
        let _ = writeln!(out, "note: {note}");
    }
    let _ = writeln!(
        out,
        "build: {}",
        option_env!("VICTORY_BUILD").unwrap_or("?")
    );
    let _ = writeln!(
        out,
        "pictures (the presented frame read back, no screen capture):"
    );
    for (name, frame, before) in written {
        let _ = writeln!(
            out,
            "  {name}: frame {frame}, {before:.2} s before the press"
        );
    }
    let _ = writeln!(
        out,
        "last {} s: {} frames, avg {:.1} ms, p50 {:.1} ms, p99 {:.1} ms, worst {:.1} ms, {} over 33.3 ms",
        ring::WINDOW.as_secs(),
        s.frames,
        s.avg_ms,
        s.p50_ms,
        s.p99_ms,
        s.worst_ms,
        s.over_33
    );
    let limit = ring::hitch_threshold(&job.frames.iter().map(|e| e.ms).collect::<Vec<_>>());
    let _ = writeln!(out, "hitches (at least {limit:.1} ms): {}", s.hitches.len());
    for (frame, ms) in &s.hitches {
        let _ = writeln!(out, "  frame {frame}: {ms:.1} ms");
    }
    out
}

/// What a selftest checks of the bundle it pressed for: `None` when whole, else what is missing.
pub(crate) fn missing(folder: &Path, pictures: usize, frames: usize) -> Option<String> {
    let mut gone: Vec<String> = ["summary.txt", "switches.txt", "frametimes.csv"]
        .iter()
        .filter(|f| !folder.join(f).is_file())
        .map(|f| f.to_string())
        .collect();
    let pngs = std::fs::read_dir(folder)
        .map(|d| {
            d.flatten()
                .filter(|e| e.path().extension().is_some_and(|x| x == "png"))
                .count()
        })
        .unwrap_or(0);
    if pngs == 0 || pngs < pictures {
        gone.push(format!("pictures ({pngs} of {pictures})"));
    }
    if frames == 0 {
        gone.push("frame times".into());
    }
    (!gone.is_empty()).then(|| gone.join(", "))
}

/// Starts the writer, which calls `done` when finished; whether it started.
pub(crate) fn spawn(job: Job, done: fn()) -> bool {
    let spawned = std::thread::Builder::new()
        .name("victory-report".into())
        .spawn(move || {
            use windows_sys::Win32::System::Threading::{
                GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL,
            };
            // SAFETY: this thread's own priority.
            unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL) };
            write(job);
            done();
        });
    if spawned.is_err() {
        tracing::warn!("report key: the writer thread did not start");
    }
    spawned.is_ok()
}

fn landed(flight: u64) -> bool {
    let deadline = Instant::now() + LAND_TIMEOUT;
    while flight != 0 && crate::host::submissions_finished() < flight {
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(LAND_POLL);
    }
    true
}

/// The picture as 8-bit RGB.
fn rgb_of(p: &Picture) -> Option<(Vec<u8>, usize, usize)> {
    let f = p.footprint.Footprint;
    let (w, h, pitch) = (f.Width as usize, f.Height as usize, f.RowPitch as usize);
    let mut data = std::ptr::null_mut();
    // SAFETY: the copy has landed and no copy into this buffer runs while a report is written;
    // the mapping spans the footprint's rows.
    unsafe {
        if p.buffer.0.Map(0, None, Some(&mut data)).is_err() || data.is_null() {
            return None;
        }
        let rows = std::slice::from_raw_parts(data.cast::<u8>(), pitch * h);
        let rgb = crate::screenshot::to_rgb(f.Format.0 as u32, w, h, pitch, rows);
        p.buffer.0.Unmap(0, None);
        rgb.map(|rgb| (rgb, w, h))
    }
}

fn write(job: Job) {
    let folder = &job.folder;
    if let Err(e) = std::fs::create_dir_all(folder) {
        tracing::warn!("report key: {} not made: {e}", folder.display());
        return;
    }
    let mut written = Vec::new();
    for p in &job.pictures {
        let name = format!("{}-f{}.png", p.name, p.frame);
        if !landed(p.flight) {
            tracing::warn!("report key: {name} skipped: its copy did not land");
            continue;
        }
        let Some((rgb, w, h)) = rgb_of(p) else {
            tracing::warn!("report key: {name} skipped: the presented format is not converted");
            continue;
        };
        match std::fs::write(
            folder.join(&name),
            crate::screenshot::encode_png(w as u32, h as u32, &rgb),
        ) {
            Ok(()) => written.push((name, p.frame, p.before_s)),
            Err(e) => tracing::warn!("report key: {name} not written: {e}"),
        }
    }
    let files = [
        ("frametimes.csv", csv(&job.frames, job.pressed)),
        (
            "switches.txt",
            switches(std::env::vars(), &changed_settings()),
        ),
        ("summary.txt", summary(&job, &written)),
    ];
    for (name, text) in files {
        if let Err(e) = std::fs::write(folder.join(name), text) {
            tracing::warn!("report key: {name} not written: {e}");
        }
    }
    let s = ring::summarise(&job.frames);
    tracing::info!(
        "report key: wrote {} (pictures={}, frames={}, avg={:.1} ms, worst={:.1} ms, hitches={})",
        folder.display(),
        written.len(),
        s.frames,
        s.avg_ms,
        s.worst_ms,
        s.hitches.len()
    );
    if let Some(pass) = job.selftest {
        match missing(folder, job.pictures.len(), s.frames) {
            None => tracing::info!(
                "report selftest: pass at {pass}s ok by {} (pictures={}, frames={})",
                job.cause.name(),
                written.len(),
                s.frames
            ),
            Some(what) => tracing::warn!(
                "report selftest: pass at {pass}s by {}: missing {what} in {}",
                job.cause.name(),
                folder.display()
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries(ms: &[f32], pressed: Instant) -> Vec<Entry> {
        let n = ms.len();
        ms.iter()
            .enumerate()
            .map(|(i, &m)| Entry {
                frame: 100 + i as u64,
                at: pressed - Duration::from_millis(40 * (n - 1 - i) as u64),
                ms: m,
            })
            .collect()
    }

    #[test]
    fn frame_times_are_rows_timed_from_the_press() {
        let pressed = Instant::now() + Duration::from_secs(1);
        let text = csv(&entries(&[40.0, 85.5, 41.0], pressed), pressed);
        assert_eq!(
            text,
            "frame,t_s,ms\n100,-0.0800,40.000\n101,-0.0400,85.500\n102,0.0000,41.000\n"
        );
    }

    #[test]
    fn switches_list_ours_sorted_and_the_menus_changes() {
        let env = [
            ("VICTORY_B", "2"),
            ("PATH", "x"),
            ("ANYXBOXX_JOB", "j"),
            ("VICTORY_A", "1"),
        ]
        .map(|(k, v)| (k.to_string(), v.to_string()));
        let text = switches(
            env.into_iter(),
            &[("VICTORY_OVERLAY".into(), "0".into(), true)],
        );
        assert_eq!(
            text,
            "# switches in the environment at start\nANYXBOXX_JOB=j\nVICTORY_A=1\nVICTORY_B=2\n# changed in the menu while running\nVICTORY_OVERLAY=0 (in effect now)\n"
        );
    }

    #[test]
    fn folders_are_unique_and_a_bundle_is_checked_whole() {
        let dir = tempfile::tempdir().unwrap();
        let a = folder_in(dir.path(), "2026-11-28_10-00-00", 7);
        assert!(a.ends_with("report-2026-11-28_10-00-00-f7"));
        std::fs::create_dir_all(&a).unwrap();
        assert!(
            folder_in(dir.path(), "2026-11-28_10-00-00", 7)
                .ends_with("report-2026-11-28_10-00-00-f7-2")
        );
        assert_eq!(
            missing(&a, 1, 10).as_deref(),
            Some("summary.txt, switches.txt, frametimes.csv, pictures (0 of 1)")
        );
        for f in ["summary.txt", "switches.txt", "frametimes.csv", "at-f7.png"] {
            std::fs::write(a.join(f), "x").unwrap();
        }
        assert_eq!(missing(&a, 1, 10), None);
        assert_eq!(
            missing(&a, 3, 0).as_deref(),
            Some("pictures (1 of 3), frame times")
        );
    }

    #[test]
    fn the_summary_starts_with_the_marker_and_lists_hitches() {
        let pressed = Instant::now() + Duration::from_secs(1);
        let mut ms = vec![40.0; 50];
        ms[20] = 120.0;
        let job = Job {
            folder: PathBuf::from("r"),
            cause: Cause::Pad,
            frame: 149,
            pressed,
            pictures: Vec::new(),
            frames: entries(&ms, pressed),
            note: None,
            selftest: Some(90),
        };
        let text = summary(&job, &[("at-f149.png".into(), 149, 0.0)]);
        assert!(text.starts_with("report key: frame=149 folder=r\npressed by: View + RS held (VICTORY_REPORT_SELFTEST pass at 90s)\n"), "{text}");
        assert!(
            text.contains("  at-f149.png: frame 149, 0.00 s before the press\n"),
            "{text}"
        );
        assert!(
            text.contains("hitches (at least 80.0 ms): 1\n  frame 120: 120.0 ms\n"),
            "{text}"
        );
        assert!(text.contains("50 frames, avg 41.6 ms"), "{text}");
    }
}
