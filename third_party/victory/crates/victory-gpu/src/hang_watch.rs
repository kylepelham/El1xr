//! The hang watch (`VICTORY_HANG_WATCH=<seconds>`; off by default): a thread
//! that looks at the host's fence a few times a second and, when the card
//! has not moved it for that long while work of ours is queued, says what is
//! on the card before the driver's timeout (10 s on the test PC) removes the
//! device: the submissions in flight with their lists, the last title
//! commands each list recorded (kind and pipeline: its shader hashes), and
//! the switches set, those the shader translation reads first. Every line
//! starts with `hang watch:`.
//!
//! It only reads: the fence (free-threaded), atomics, and the notes of
//! `crate::dred` through `try_lock`, so a thread stuck while holding one of
//! them never holds the watch up.

use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// `VICTORY_HANG_WATCH=<seconds>`: how long the fence may stand.
pub(crate) fn seconds() -> Option<f64> {
    static SECONDS: OnceLock<Option<f64>> = OnceLock::new();
    *SECONDS.get_or_init(|| parse(std::env::var("VICTORY_HANG_WATCH").ok().as_deref()))
}

fn parse(value: Option<&str>) -> Option<f64> {
    value?
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|s| s.is_finite() && *s > 0.0)
}

/// How often the fence is looked at.
const LOOK: Duration = Duration::from_millis(200);

/// Starts the watch's thread, once, with the switch set.
pub(crate) fn start() {
    static STARTED: std::sync::Once = std::sync::Once::new();
    let Some(limit) = seconds() else {
        return;
    };
    STARTED.call_once(|| {
        let spawned = std::thread::Builder::new()
            .name("victory-hang-watch".into())
            .spawn(move || watch(Duration::from_secs_f64(limit)));
        match spawned {
            Ok(_) => tracing::info!(seconds = limit, "hang watch: on (VICTORY_HANG_WATCH)"),
            Err(e) => tracing::warn!(error = %e, "hang watch: no thread"),
        }
    });
}

fn watch(limit: Duration) {
    let Some(host) = crate::host::host() else {
        return;
    };
    let mut last = 0;
    let mut since = Instant::now();
    let mut said = false;
    loop {
        std::thread::sleep(LOOK);
        // SAFETY: a plain query of a free-threaded fence.
        let completed = unsafe { host.fence.GetCompletedValue() };
        if completed == u64::MAX {
            tracing::error!("hang watch: the host device is removed (its fence reads all ones)");
            if !said {
                say(last, host.submitted_fence(), since.elapsed());
            }
            return;
        }
        let queued = host.submitted_fence();
        if completed != last || completed >= queued {
            if said {
                tracing::warn!(
                    fence = completed,
                    stood_s = since.elapsed().as_secs_f64(),
                    "hang watch: the card moved on"
                );
            }
            last = completed;
            since = Instant::now();
            said = false;
            continue;
        }
        if !said && since.elapsed() >= limit {
            say(completed, queued, since.elapsed());
            said = true;
        }
    }
}

/// The lines for a fence standing at `completed` with work up to `queued`.
fn say(completed: u64, queued: u64, stood: Duration) {
    let [phase, lists, at] = crate::host::submitting_state();
    let phase = match phase {
        0 => "between submissions",
        1 => "preparing a submission",
        2 => "waiting for the card to run a submission",
        _ => "copying back what a submission wrote",
    };
    tracing::error!(
        "hang watch: the card's fence has stood at {completed} for {:.1} s with work queued up to {queued}; \
         the submitting thread is {phase} ({lists} lists, for {:.1} ms); submissions started {} finished {}",
        stood.as_secs_f64(),
        crate::host::now_us().saturating_sub(at) as f64 / 1000.0,
        crate::host::submissions_started(),
        crate::host::submissions_finished(),
    );
    for line in flights(completed) {
        tracing::error!("hang watch: {line}");
    }
    let (shader, other) = switches();
    tracing::error!("hang watch: shader switches set: {}", shader.join(" "));
    tracing::error!("hang watch: other switches set: {}", other.join(" "));
}

/// The submissions kept that the card has not finished, with their lists'
/// last title commands.
fn flights(completed: u64) -> Vec<String> {
    let mut flights = None;
    for _ in 0..10 {
        if let Ok(f) = crate::dred::FLIGHTS.try_lock() {
            flights = Some(f);
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let Some(flights) = flights else {
        return vec!["the list of submissions sent is held by another thread".to_owned()];
    };
    let mut lines = Vec::new();
    let waiting: Vec<_> = flights.iter().filter(|f| f.fence > completed).collect();
    if waiting.is_empty() {
        lines.push(format!(
            "no title submission kept is past fence {completed}: what stands is our own work (a fetch, compare or copy)"
        ));
    }
    for f in waiting {
        lines.push(format!(
            "in flight: submission {} (fence {}), sent {:.1} s ago, {} lists{}",
            f.submission,
            f.fence,
            f.sent.elapsed().as_secs_f64(),
            f.lists.len(),
            if f.fence == completed + 1 {
                " <- the next the card owes"
            } else {
                ""
            }
        ));
        for (_, ops) in &f.lists {
            lines.push(match ops.recent(6) {
                Some((total, last)) => format!(
                    "  victory list #{}: {total} title commands recorded; the last: {}",
                    ops.id(),
                    last.join(" | ")
                ),
                None => format!("  victory list #{}: being recorded again", ops.id()),
            });
            // (`VICTORY_RAW_ARGS_WATCH`) What the card read for its raw records.
            if f.fence == completed + 1 {
                lines.extend(crate::raw_watch::lines(ops.id(), 16));
            }
        }
    }
    lines
}

/// The `VICTORY_*` switches set: those the shader translation reads, then
/// the others.
fn switches() -> (Vec<String>, Vec<String>) {
    let fingerprint = victory_shader::emit::translation_fingerprint();
    let shader = shader_switches(&fingerprint);
    let mut set: Vec<(String, String)> = std::env::vars()
        .filter(|(k, v)| k.starts_with("VICTORY_") && !v.is_empty())
        .collect();
    set.sort();
    split_switches(&set, &shader)
}

/// The names of the switches the translation fingerprint holds (each
/// `NAME=value` and a zero byte, among the fingerprint's other bytes).
fn shader_switches(fingerprint: &[u8]) -> Vec<&str> {
    fingerprint
        .split(|b| *b == 0)
        .filter_map(|part| {
            let at = part.windows(8).position(|w| w == b"VICTORY_")?;
            let part = &part[at..];
            let end = part.iter().position(|b| *b == b'=')?;
            std::str::from_utf8(&part[..end]).ok()
        })
        .collect()
}

fn split_switches(set: &[(String, String)], shader: &[&str]) -> (Vec<String>, Vec<String>) {
    let (mut a, mut b) = (Vec::new(), Vec::new());
    for (k, v) in set {
        let text = format!("{k}={v}");
        if shader.contains(&k.as_str()) {
            a.push(text);
        } else {
            b.push(text);
        }
    }
    (a, b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_seconds() {
        assert_eq!(parse(None), None);
        assert_eq!(parse(Some("3")), Some(3.0));
        assert_eq!(parse(Some(" 2.5 ")), Some(2.5));
        assert_eq!(parse(Some("0")), None);
        assert_eq!(parse(Some("-1")), None);
        assert_eq!(parse(Some("x")), None);
    }

    #[test]
    fn splits_shader_switches_from_the_others() {
        let set = [
            ("VICTORY_CARD_SIDE".to_owned(), "1".to_owned()),
            ("VICTORY_LDS_FIRST_INIT".to_owned(), "1".to_owned()),
        ];
        let (shader, other) =
            split_switches(&set, &["VICTORY_LDS_FIRST_INIT", "VICTORY_WAVE_COLLAPSE"]);
        assert_eq!(shader, ["VICTORY_LDS_FIRST_INIT=1"]);
        assert_eq!(other, ["VICTORY_CARD_SIDE=1"]);
    }

    #[test]
    fn finds_the_switch_names_in_the_fingerprint() {
        let print = b"0123abcd\x01\x00VICTORY_A=1\0VICTORY_B=\0\x40\x00\x00\x00";
        assert_eq!(shader_switches(print), ["VICTORY_A", "VICTORY_B"]);
        let real = victory_shader::emit::translation_fingerprint();
        assert!(shader_switches(&real).contains(&"VICTORY_LDS_FIRST_INIT"));
    }
}
