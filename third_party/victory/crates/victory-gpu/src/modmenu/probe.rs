//! (El1xr) `VICTORY_MOD_MENU_PROBE=1` (default off): checks the trainer's natives without the
//! menu. Every [`EVERY`] it logs the last result of each [`Effect::Read`] row and asks again:
//! `mod menu: probe MOD_CLOCK_HOUR 0x3d4e6e6637d30df7 -> 14 raw [..]`.
//!
//! `VICTORY_MOD_MENU_PROBE_SET=NAME=value[;NAME=value...][@seconds]` also runs those
//! [`Effect::Set`] rows once, `seconds` (default [`SET_AFTER`]) after the first game tick, and
//! logs `mod menu: probe set NAME=value`; the reads after it show what changed.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use super::model::{Effect, Opt};
use super::queue::{Call, Outcome, Queue};

pub const EVERY: Duration = Duration::from_secs(10);
pub const SET_AFTER: Duration = Duration::from_secs(90);

/// Sets asked for by `VICTORY_MOD_MENU_PROBE_SET`, and when.
#[derive(Debug, PartialEq, Eq)]
pub struct SetPlan {
    pub sets: Vec<(String, String)>,
    pub after: Duration,
}

pub fn parse_set(v: &str) -> Option<SetPlan> {
    let (list, after) = match v.rsplit_once('@') {
        Some((l, s)) => (l, Duration::from_secs(s.trim().parse().ok()?)),
        None => (v, SET_AFTER),
    };
    let sets: Vec<(String, String)> = list
        .split(';')
        .filter(|s| !s.trim().is_empty())
        .map(|s| {
            let (n, v) = s.split_once('=')?;
            Some((n.trim().to_string(), v.trim().to_string()))
        })
        .collect::<Option<_>>()?;
    (!sets.is_empty()).then_some(SetPlan { sets, after })
}

#[derive(Default)]
pub struct Probe {
    first: Option<Instant>,
    last: Option<Instant>,
    set_done: bool,
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
        if let Some(plan) = plan
            && !self.set_done
            && now - first >= plan.after
        {
            self.set_done = true;
            for (name, value) in &plan.sets {
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
                        };
                        let queued = queue.push(call);
                        format!("mod menu: probe set {name}={value} {hash:#018x} queued={queued}")
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
            let Effect::Read { hash, args, show } = o.effect else {
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
    ];

    #[test]
    fn set_plans_parse_with_an_optional_delay() {
        assert_eq!(
            parse_set("A=1; B = 2 @30"),
            Some(SetPlan {
                sets: vec![("A".into(), "1".into()), ("B".into(), "2".into())],
                after: Duration::from_secs(30)
            })
        );
        assert_eq!(parse_set("A=1").unwrap().after, SET_AFTER);
        assert_eq!(parse_set("A"), None);
        assert_eq!(parse_set(""), None);
        assert_eq!(parse_set("A=1@soon"), None);
    }

    #[test]
    fn reads_are_logged_every_period_and_sets_run_once() {
        let q = Queue::default();
        let mut p = Probe::default();
        let t0 = Instant::now();
        let plan = parse_set("P_SET=12;P_READ=1;NOPE=3@20").unwrap();
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
        assert_eq!(lines.len(), 4, "{lines:?}");
        assert!(
            lines[0].starts_with("mod menu: probe set P_SET=12 0x00000000000000bb queued=true")
        );
        assert!(lines[1].contains("refused"), "a read is not a set");
        assert!(lines[2].contains("refused"), "unknown row");
        assert!(lines[3].starts_with("mod menu: probe P_READ 0x00000000000000aa -> 14 raw"));
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
