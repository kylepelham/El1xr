//! (El1xr) `VICTORY_MOD_MENU_PROBE=1` (default off): checks the trainer's natives without the
//! menu. Every [`EVERY`] it logs the last result of each [`Effect::Read`] row and asks again:
//! `mod menu: probe MOD_CLOCK_HOUR 0x3d4e6e6637d30df7 -> 14 raw [..]`.
//!
//! `VICTORY_MOD_MENU_PROBE_SET=NAME=value[@seconds][;NAME=value[@seconds]...]` also runs those
//! [`Effect::Set`] and [`Effect::Own`] rows (such as `MOD_CLOCK_HOLD=3`) once each, `seconds`
//! after the first game tick, and logs `mod menu: probe set NAME=value`; the reads after it show
//! what changed. A row without its own time runs at [`SET_AFTER`], except in the older form where
//! only the last row has a time (`A=1;B=2@30`): then that time is the whole plan's.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use super::model::{Effect, Opt};
use super::queue::{Call, Outcome, Queue};

pub const EVERY: Duration = Duration::from_secs(10);
pub const SET_AFTER: Duration = Duration::from_secs(90);

/// One set asked for by `VICTORY_MOD_MENU_PROBE_SET`, and when.
#[derive(Debug, PartialEq, Eq)]
pub struct PlannedSet {
    pub name: String,
    pub value: String,
    pub after: Duration,
}

/// The sets asked for, in order.
#[derive(Debug, PartialEq, Eq)]
pub struct SetPlan {
    pub sets: Vec<PlannedSet>,
}

pub fn parse_set(v: &str) -> Option<SetPlan> {
    let mut sets = Vec::new();
    let mut timed = Vec::new();
    for s in v.split(';').filter(|s| !s.trim().is_empty()) {
        let (row, at) = match s.rsplit_once('@') {
            Some((r, t)) => (r, Some(Duration::from_secs(t.trim().parse().ok()?))),
            None => (s, None),
        };
        let (n, v) = row.split_once('=')?;
        let (name, value) = (n.trim(), v.trim());
        if name.is_empty() || value.is_empty() {
            return None;
        }
        timed.push(at.is_some());
        sets.push(PlannedSet {
            name: name.to_string(),
            value: value.to_string(),
            after: at.unwrap_or(SET_AFTER),
        });
    }
    let last = sets.last()?.after;
    if timed.iter().rev().skip(1).all(|t| !t) && timed.last() == Some(&true) {
        for s in &mut sets {
            s.after = last;
        }
    }
    Some(SetPlan { sets })
}

#[derive(Default)]
pub struct Probe {
    first: Option<Instant>,
    last: Option<Instant>,
    /// How many of the plan's sets have run, by index.
    set_done: Vec<bool>,
}

fn probe_name(name: &'static str) -> &'static str {
    // Probe calls keep their results apart from the menu's: one name per row, made once.
    static NAMES: Mutex<Vec<(&'static str, &'static str)>> = Mutex::new(Vec::new());
    let mut names = NAMES.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(&(_, p)) = names.iter().find(|(n, _)| *n == name) {
        return p;
    }
    let p: &'static str = Box::leak(format!("probe:{name}").into_boxed_str());
    names.push((name, p));
    p
}

impl Probe {
    /// One game tick at `now`: queues the reads (and the planned sets once) and returns the log
    /// lines.
    pub fn tick(
        &mut self,
        now: Instant,
        options: &'static [Opt],
        queue: &Queue,
        plan: Option<&SetPlan>,
    ) -> Vec<String> {
        let first = *self.first.get_or_insert(now);
        let mut lines = Vec::new();
        if let Some(plan) = plan {
            self.set_done.resize(plan.sets.len(), false);
            for (i, PlannedSet { name, value, after }) in plan.sets.iter().enumerate() {
                if self.set_done[i] || now - first < *after {
                    continue;
                }
                self.set_done[i] = true;
                let line = match options.iter().find(|o| o.info.name == name) {
                    Some(Opt {
                        info,
                        effect: Effect::Set { hash, args },
                        ..
                    }) if victory_core::settings::valid(info.kind, value) => {
                        let call = Call {
                            name: probe_name(info.name),
                            hash: *hash,
                            args: args(value),
                            script: None,
                        };
                        let queued = queue.push(call);
                        format!("mod menu: probe set {name}={value} {hash:#018x} queued={queued}")
                    }
                    Some(Opt {
                        info,
                        effect: Effect::Own { set, .. },
                        ..
                    }) if victory_core::settings::valid(info.kind, value) => {
                        set(value);
                        format!("mod menu: probe set {name}={value} (own state)")
                    }
                    _ => format!(
                        "mod menu: probe set {name}={value} refused (no such set row or bad value)"
                    ),
                };
                lines.push(line);
            }
        }
        if self.last.is_some_and(|t| now - t < EVERY) {
            return lines;
        }
        self.last = Some(now);
        for o in options {
            let Effect::Read {
                hash,
                args,
                script,
                show,
            } = o.effect
            else {
                continue;
            };
            let name = probe_name(o.info.name);
            match queue.result(name) {
                Some(Outcome::Done(r)) => lines.push(format!(
                    "mod menu: probe {} {hash:#018x} -> {} raw {r:x?}",
                    o.info.name,
                    show(r)
                )),
                Some(Outcome::Unknown) => lines.push(format!(
                    "mod menu: probe {} {hash:#018x} not in this build",
                    o.info.name
                )),
                None => {}
            }
            queue.push(Call {
                name,
                hash,
                args: args.to_vec(),
                script,
            });
        }
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::menu::model::Kind;

    fn hours(v: &str) -> Vec<u64> {
        vec![v.parse().unwrap_or(0), 0]
    }

    const OPTS: &[Opt] = &[
        Opt {
            info: super::super::model::info("P_READ", "World", "Read", "", Kind::Text),
            effect: Effect::Read {
                hash: 0xaa,
                args: &[],
                script: None,
                show: |r| (r[0] as i32).to_string(),
            },
            verified: false,
        },
        Opt {
            info: super::super::model::info(
                "P_SET",
                "World",
                "Set",
                "",
                Kind::Int { min: 0, max: 23 },
            ),
            effect: Effect::Set {
                hash: 0xbb,
                args: hours,
            },
            verified: false,
        },
        Opt {
            info: super::super::model::info("P_OWN", "World", "Own", "", Kind::Bool),
            effect: Effect::Own {
                get: || String::new(),
                set: |v| OWN.store(v == "1", std::sync::atomic::Ordering::Relaxed),
            },
            verified: false,
        },
    ];

    static OWN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

    fn times(v: &str) -> Vec<(String, String, u64)> {
        parse_set(v)
            .unwrap()
            .sets
            .into_iter()
            .map(|s| (s.name, s.value, s.after.as_secs()))
            .collect()
    }

    fn row(n: &str, v: &str, t: u64) -> (String, String, u64) {
        (n.into(), v.into(), t)
    }

    #[test]
    fn set_plans_parse_with_an_optional_delay() {
        assert_eq!(times("A=1; B = 2 @30"), vec![row("A", "1", 30), row("B", "2", 30)]);
        assert_eq!(times("A=1"), vec![row("A", "1", SET_AFTER.as_secs())]);
        assert_eq!(parse_set("A"), None);
        assert_eq!(parse_set(""), None);
        assert_eq!(parse_set("A=1@soon"), None);
        assert_eq!(parse_set("A=@5"), None);
    }

    #[test]
    fn each_row_keeps_its_own_time() {
        // Batch 5 part T's plan: every timed row was refused as `run@60` and the rest ran at 140.
        let plan = "MOD_GET_SWEEP=run;MOD_FIND_STATS=run@60;MOD_HEALTH_REFILL=refill@120;\
                    MOD_WANTED_CLEAR=clear@125;MOD_GOD_MODE=on@130;MOD_TP_FIND=run@140";
        assert_eq!(
            times(plan),
            vec![
                row("MOD_GET_SWEEP", "run", SET_AFTER.as_secs()),
                row("MOD_FIND_STATS", "run", 60),
                row("MOD_HEALTH_REFILL", "refill", 120),
                row("MOD_WANTED_CLEAR", "clear", 125),
                row("MOD_GOD_MODE", "on", 130),
                row("MOD_TP_FIND", "run", 140),
            ]
        );
        assert_eq!(times("A=1@5;B=2"), vec![row("A", "1", 5), row("B", "2", SET_AFTER.as_secs())]);
    }

    #[test]
    fn the_trainer_rows_take_the_part_t_values() {
        use crate::modmenu::model::OPTIONS;
        for (name, value) in [
            ("MOD_GET_SWEEP", "run"),
            ("MOD_FIND_STATS", "run"),
            ("MOD_HEALTH_REFILL", "refill"),
            ("MOD_ARMOUR_REFILL", "refill"),
            ("MOD_WANTED_CLEAR", "clear"),
            ("MOD_GOD_MODE", "on"),
            ("MOD_NEVER_WANTED", "on"),
            ("MOD_TP_FIND", "run"),
            ("MOD_TP_SAVE_1", "save"),
            ("MOD_TP_GOTO_1", "go"),
        ] {
            let o = OPTIONS.iter().find(|o| o.info.name == name).expect(name);
            assert!(matches!(o.effect, Effect::Own { .. }), "{name}");
            assert!(victory_core::settings::valid(o.info.kind, value), "{name}={value}");
        }
    }

    #[test]
    fn timed_sets_run_each_at_its_own_time() {
        let q = Queue::default();
        let mut p = Probe::default();
        let t0 = Instant::now();
        // P_OWN=1 as the other test sets it: the two share its flag and run at once.
        let plan = parse_set("P_OWN=1@10;P_SET=3@20").unwrap();
        let at = |p: &mut Probe, s: u64| -> Vec<String> {
            p.tick(t0 + Duration::from_secs(s), OPTS, &q, Some(&plan))
                .into_iter()
                .filter(|l| l.contains("probe set"))
                .collect()
        };
        assert!(at(&mut p, 0).is_empty());
        assert_eq!(at(&mut p, 11), vec!["mod menu: probe set P_OWN=1 (own state)"]);
        assert!(at(&mut p, 12).is_empty(), "once");
        assert_eq!(at(&mut p, 21).len(), 1);
        assert!(at(&mut p, 40).is_empty());
    }

    #[test]
    fn reads_are_logged_every_period_and_sets_run_once() {
        let q = Queue::default();
        let mut p = Probe::default();
        let t0 = Instant::now();
        let plan = parse_set("P_SET=12;P_READ=1;NOPE=3;P_OWN=1@20").unwrap();
        assert!(
            p.tick(t0, OPTS, &q, Some(&plan)).is_empty(),
            "first tick only asks"
        );
        let mut seen = Vec::new();
        q.drain(|c| {
            seen.push(c.hash);
            Some([14, 0, 0, 0])
        });
        assert_eq!(seen, vec![0xaa], "reads only");
        assert!(
            p.tick(t0 + Duration::from_secs(5), OPTS, &q, Some(&plan))
                .is_empty()
        );
        let lines = p.tick(t0 + Duration::from_secs(21), OPTS, &q, Some(&plan));
        assert_eq!(lines.len(), 5, "{lines:?}");
        assert!(
            lines[0].starts_with("mod menu: probe set P_SET=12 0x00000000000000bb queued=true")
        );
        assert!(lines[1].contains("refused"), "a read is not a set");
        assert!(lines[2].contains("refused"), "unknown row");
        assert_eq!(lines[3], "mod menu: probe set P_OWN=1 (own state)");
        assert!(OWN.load(std::sync::atomic::Ordering::Relaxed));
        assert!(lines[4].starts_with("mod menu: probe P_READ 0x00000000000000aa -> 14 raw"));
        let mut seen = Vec::new();
        q.drain(|c| {
            seen.push((c.hash, c.args.clone()));
            None
        });
        assert_eq!(seen, vec![(0xbb, vec![12, 0]), (0xaa, vec![])]);
        let lines = p.tick(t0 + Duration::from_secs(40), OPTS, &q, Some(&plan));
        assert_eq!(
            lines,
            vec!["mod menu: probe P_READ 0x00000000000000aa not in this build"]
        );
    }
}
