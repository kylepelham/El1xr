//! `VICTORY_SETTINGS_SCHEDULE=<file>`: live settings changed on a timeline,
//! so one launch runs several arms (`dev.py train`'s live arms,
//! `tools/dev_live.py`, write the file).
//!
//! One change per line, `at <t>s set VICTORY_NAME=<value>`; blank lines and
//! `#` comments are skipped. `t` is seconds since the process started (the
//! moment [`super::init`] runs, first thing in `main`), the zero
//! `dev_live.py`'s `start_s` counts from. Every setting must be
//! [`Apply::Live`] and every value one its kind takes: otherwise the whole
//! schedule is refused at load, logged, and no change is made.
//!
//! Changes at the same `t` form one segment, numbered from 1 in time order.
//! Each change, once made through [`super::set`], is logged as
//! `settings: segment <n> <name>=<value> at <t>` (`t` the seconds since the
//! process started when it was made), the line `dev_live.py`'s `SEGMENT_RE`
//! reads.

use super::{Apply, apply_of, info, set, valid};
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// One change of the schedule.
#[derive(Clone, Debug, PartialEq)]
pub struct Step {
    /// Seconds since the process started.
    pub at: f64,
    /// 1, 2, ...: the changes at one `at` share it.
    pub segment: usize,
    pub name: &'static str,
    pub value: String,
}

/// The schedule's changes in time order, or every line that is wrong.
pub fn parse(text: &str) -> Result<Vec<Step>, Vec<String>> {
    let mut steps = Vec::new();
    let mut problems = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        match parse_line(line) {
            Ok(step) => steps.push(step),
            Err(e) => problems.push(format!("line {}: {e}", i + 1)),
        }
    }
    if !problems.is_empty() {
        return Err(problems);
    }
    steps.sort_by(|a, b| a.at.total_cmp(&b.at));
    let mut segment = 0;
    let mut last = None;
    for step in &mut steps {
        if last != Some(step.at) {
            segment += 1;
            last = Some(step.at);
        }
        step.segment = segment;
    }
    Ok(steps)
}

fn parse_line(line: &str) -> Result<Step, String> {
    let form = || format!("{line:?} is not `at <t>s set VICTORY_NAME=<value>`");
    let rest = line.strip_prefix("at ").ok_or_else(form)?;
    let (t, change) = rest.split_once(" set ").ok_or_else(form)?;
    let at = t
        .trim()
        .strip_suffix('s')
        .and_then(|t| t.parse::<f64>().ok())
        .filter(|t| t.is_finite() && *t >= 0.0)
        .ok_or_else(form)?;
    let (name, value) = change.trim().split_once('=').ok_or_else(form)?;
    let (name, value) = (name.trim(), value.trim());
    let info = info(name).ok_or_else(|| format!("no setting is called {name}"))?;
    match apply_of(name) {
        Some(Apply::Live) => {}
        Some(apply) => {
            return Err(format!(
                "{name} is {apply:?}, not Live: it cannot change while running (it needs its own launch)"
            ));
        }
        None => unreachable!("{name} has an entry"),
    }
    if !value.is_empty() && !valid(info.kind, value) {
        return Err(format!("{name} does not take {value:?}"));
    }
    Ok(Step {
        at,
        segment: 0,
        name: info.name,
        value: value.to_string(),
    })
}

/// The line logged for a change made `t` seconds after start.
pub fn segment_line(step: &Step, t: f64) -> String {
    format!(
        "settings: segment {} {}={} at {t:.1}",
        step.segment, step.name, step.value
    )
}

/// Makes each change at its time (`zero` + `at`), waiting with
/// `wait_until`, and gives `log` the line for each.
pub fn run(
    steps: &[Step],
    zero: Instant,
    mut wait_until: impl FnMut(Instant),
    mut log: impl FnMut(String),
) {
    for step in steps {
        wait_until(zero + Duration::from_secs_f64(step.at));
        match set(step.name, &step.value) {
            Ok(_) => log(segment_line(step, zero.elapsed().as_secs_f64())),
            Err(e) => log(format!(
                "settings: schedule: segment {} not applied: {e}",
                step.segment
            )),
        }
    }
}

fn sleep_until(when: Instant) {
    loop {
        let now = Instant::now();
        if now >= when {
            return;
        }
        std::thread::sleep(when - now);
    }
}

/// Reads `VICTORY_SETTINGS_SCHEDULE`, if set, and starts the thread that
/// applies it. Call it once logging is up. A schedule with a wrong line is
/// logged and not started.
pub fn start() {
    let Some(path) = std::env::var_os(super::SCHEDULE_VARIABLE).filter(|v| !v.is_empty()) else {
        return;
    };
    let path = PathBuf::from(path);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) => {
            tracing::error!(
                "settings: schedule {} not read ({e}): no change will be made",
                path.display()
            );
            return;
        }
    };
    let steps = match parse(&text) {
        Ok(steps) => steps,
        Err(problems) => {
            for p in &problems {
                tracing::error!("settings: schedule {}: {p}", path.display());
            }
            tracing::error!(
                "settings: schedule {} refused ({} wrong lines): not started, no change will be made",
                path.display(),
                problems.len()
            );
            return;
        }
    };
    let (Some(first), Some(last)) = (steps.first(), steps.last()) else {
        tracing::warn!("settings: schedule {} has no change", path.display());
        return;
    };
    tracing::info!(
        "settings: schedule {}: {} changes in {} segments, {:.1} s to {:.1} s after start",
        path.display(),
        steps.len(),
        last.segment,
        first.at,
        last.at
    );
    let zero = super::started();
    let spawned = std::thread::Builder::new()
        .name("settings schedule".into())
        .spawn(move || run(&steps, zero, sleep_until, |line| tracing::info!("{line}")));
    if let Err(e) = spawned {
        tracing::error!("settings: schedule thread not started: {e}");
    }
}
