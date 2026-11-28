//! (El1xr) `VICTORY_MENU_SELFTEST=<seconds>[,<seconds>...][:<SETTING>]`: a scripted pass over the
//! menu for runs nobody watches. At each time (seconds since the menu's window was made, about
//! the first present, as timed snaps count) it presses the menu's keys through the same path a
//! held key takes ([`super::Driver::tick`]): the hotkey opens the menu, the next-tab key steps
//! through every tab, Right then Left changes one Live Display setting and puts it back
//! (`VICTORY_OVERLAY` unless `:<SETTING>` names another), and Esc closes it. Each step logs a
//! `menu selftest:` line, and after each tab, the change and the close the window asks the present
//! for a picture ([`crate::menu_shot`]): the next presented frame read back from the swap chain's
//! texture with the menu's own bitmap laid over it, never the screen. The keys go through the
//! driver whatever window is in front. With no key in its plan nothing here runs; it has no OS
//! calls.

use std::collections::VecDeque;

use super::Driver;
use super::input::Key;
use super::model::{Apply, Kind};
use super::state::{Row, Tab};

/// How long a step settles before its picture is asked for (the menu drawn, the title's frame
/// caught up).
const SETTLE_MS: u64 = 500;
/// How long the closing keys may take to let the title have its input back.
const RETURN_TIMEOUT_MS: u64 = 3000;
/// The setting changed and put back unless the plan names one: Live, and harmless to flip.
const PREFERRED: &str = "VICTORY_OVERLAY";

#[derive(Clone, Debug, PartialEq)]
pub struct Plan {
    /// When each pass starts, in ms.
    pub at_ms: Vec<u64>,
    pub setting: Option<String>,
}

/// `60,120` or `60,120:VICTORY_FFX_SCALE`; `None` for nothing usable.
pub fn parse(text: &str) -> Option<Plan> {
    let (times, setting) = match text.split_once(':') {
        Some((t, s)) => (t, Some(s.trim().to_string()).filter(|s| !s.is_empty())),
        None => (text, None),
    };
    let mut at_ms: Vec<u64> = times
        .split([',', ' '])
        .filter(|t| !t.trim().is_empty())
        .filter_map(|t| t.trim().parse::<f64>().ok())
        .filter(|s| s.is_finite() && *s >= 0.0)
        .map(|s| (s * 1000.0) as u64)
        .collect();
    at_ms.sort_unstable();
    at_ms.dedup();
    (!at_ms.is_empty()).then_some(Plan { at_ms, setting })
}

/// What the window does for a step.
#[derive(Clone, Debug, PartialEq)]
pub enum Out {
    Log(String),
    Warn(String),
    /// A picture of the game's next presented frame with the menu over it, into this file name.
    Grab(String),
}

#[derive(Debug, Default)]
pub struct Tick {
    /// Keys held this poll, on top of the real ones.
    pub keys: Vec<Key>,
    pub out: Vec<Out>,
}

#[derive(Clone, Debug)]
enum Step {
    /// The key held for one poll, then let go for one.
    Press(Key),
    Settle,
    Opened,
    OnTab {
        i: usize,
        n: usize,
        tab: Tab,
        rows: usize,
    },
    Grab(String),
    BackOnDisplay,
    Selected {
        row: usize,
        name: &'static str,
    },
    Changed {
        name: &'static str,
        from: String,
    },
    Restored {
        name: &'static str,
        from: String,
    },
    Closed,
    InputBack,
    /// A warning that does not stop the pass.
    Note(String),
}

pub struct Selftest {
    plan: Plan,
    next: usize,
    steps: VecDeque<Step>,
    /// The pass's start in ms, for its log lines and file names.
    pass: Option<u64>,
    /// The key of the `Press` at the front went down on the last poll.
    pressed: bool,
    last_press: u64,
    closed_at: u64,
}

fn secs(ms: u64) -> String {
    format!("{}", ms as f64 / 1000.0)
}

fn file_part(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect()
}

impl Selftest {
    pub fn new(plan: Plan) -> Self {
        Selftest {
            plan,
            next: 0,
            steps: VecDeque::new(),
            pass: None,
            pressed: false,
            last_press: 0,
            closed_at: 0,
        }
    }

    /// The pass's times, for the log at start.
    pub fn plan(&self) -> &Plan {
        &self.plan
    }

    /// One poll at `now` (ms), before the driver's own: the keys to hold and what to do.
    /// `blocked` is what the HLE reads ([`super::blocked`]).
    pub fn tick(&mut self, now: u64, driver: &mut Driver, blocked: bool, front: bool) -> Tick {
        let mut t = Tick::default();
        if self.pass.is_none() {
            let Some(&at) = self.plan.at_ms.get(self.next) else {
                return t;
            };
            if now < at {
                return t;
            }
            self.next += 1;
            if driver.menu.open {
                t.out.push(Out::Warn(format!(
                    "menu selftest: {}s skipped, the menu is already open",
                    secs(at)
                )));
                return t;
            }
            t.out.push(Out::Log(format!(
                "menu selftest: pass at {}s starts (game window in front={front}, title input blocked={blocked})",
                secs(at)
            )));
            self.pass = Some(at);
            self.steps = [Step::Press(Key::Hotkey), Step::Opened].into();
        }
        while let Some(step) = self.steps.front().cloned() {
            if !self.run(step, now, driver, blocked, &mut t) {
                return t;
            }
            self.steps.pop_front();
        }
        let at = self.pass.take().unwrap_or_default();
        t.out.push(Out::Log(format!(
            "menu selftest: pass at {}s done",
            secs(at)
        )));
        t
    }

    /// Runs `step`; whether it is done (else the poll ends with it).
    fn run(&mut self, step: Step, now: u64, d: &mut Driver, blocked: bool, t: &mut Tick) -> bool {
        let at = secs(self.pass.unwrap_or_default());
        let log = |t: &mut Tick, s: String| t.out.push(Out::Log(format!("menu selftest: {s}")));
        let warn = |t: &mut Tick, s: String| t.out.push(Out::Warn(format!("menu selftest: {s}")));
        match step {
            Step::Press(key) => {
                if self.pressed {
                    self.pressed = false;
                    self.steps.pop_front();
                } else {
                    t.keys.push(key);
                    (self.pressed, self.last_press) = (true, now);
                }
                return false;
            }
            Step::Settle => return now >= self.last_press + SETTLE_MS,
            Step::Opened => {
                if !d.menu.open {
                    warn(t, format!("{at}s: the hotkey did not open the menu"));
                    self.steps.clear();
                    return false;
                }
                log(t, format!("opened (title input blocked={blocked})"));
                self.plan_pass(d, &at);
            }
            Step::OnTab { i, n, tab, rows } => {
                if d.menu.tab == tab {
                    log(
                        t,
                        format!("tab {}/{n} {} drawn, {rows} rows", i + 1, tab.title()),
                    );
                } else {
                    warn(
                        t,
                        format!(
                            "tab {}/{n}: expected {}, the menu shows {}",
                            i + 1,
                            tab.title(),
                            d.menu.tab.title()
                        ),
                    );
                }
            }
            Step::Grab(name) => t.out.push(Out::Grab(name)),
            Step::BackOnDisplay => {
                if d.menu.tab != Tab::DISPLAY {
                    warn(
                        t,
                        format!(
                            "after the last tab the menu shows {}, not Display",
                            d.menu.tab.title()
                        ),
                    );
                }
            }
            Step::Selected { row, name } => {
                if d.menu.selected != row {
                    warn(
                        t,
                        format!(
                            "{name}: row {row} not selected (row {} is)",
                            d.menu.selected
                        ),
                    );
                }
            }
            Step::Changed { name, from } => {
                let now_v = d.model.get(name).unwrap_or_default();
                let apply = d.model.apply_of(name);
                let status = d.menu.status.clone().unwrap_or_default();
                if now_v == from {
                    warn(
                        t,
                        format!("value not set: {name} stayed {from:?} ({status})"),
                    );
                } else {
                    log(
                        t,
                        format!(
                            "value set {name} {from:?} -> {now_v:?} (apply {apply:?}; menu says \"{status}\")"
                        ),
                    );
                }
            }
            Step::Restored { name, from } => {
                let now_v = d.model.get(name).unwrap_or_default();
                if now_v == from {
                    log(t, format!("value restored {name} -> {from:?}"));
                } else {
                    warn(
                        t,
                        format!(
                            "value not restored by Left: {name} is {now_v:?}, was {from:?}; set back directly"
                        ),
                    );
                    let _ = d.model.set(name, &from);
                }
            }
            Step::Note(why) => warn(t, why),
            Step::Closed => {
                if d.menu.open {
                    warn(t, "Esc did not close the menu".into());
                    self.steps.clear();
                    return false;
                }
                log(t, "closed".into());
                self.closed_at = now;
            }
            Step::InputBack => {
                if blocked && now < self.closed_at + RETURN_TIMEOUT_MS {
                    return false;
                }
                if blocked {
                    warn(
                        t,
                        format!(
                            "input NOT returned to title {RETURN_TIMEOUT_MS} ms after close (blocked=true)"
                        ),
                    );
                } else {
                    log(
                        t,
                        format!(
                            "input returned to title {} ms after close (blocked=false)",
                            now - self.closed_at
                        ),
                    );
                }
            }
        }
        true
    }

    /// The rest of a pass once the menu is open: every tab, the change and its undo, the close.
    fn plan_pass(&mut self, d: &Driver, at: &str) {
        let model = &*d.model;
        let tabs = d.menu.tabs(model);
        let n = tabs.len();
        let mut steps = Vec::new();
        for (i, &tab) in tabs.iter().enumerate() {
            let rows = super::state::rows(model, tab, d.menu.expert).len();
            steps.push(Step::Settle);
            steps.push(Step::OnTab { i, n, tab, rows });
            steps.push(Step::Grab(format!(
                "menu-{at}s-{:02}-{}.png",
                i + 1,
                file_part(tab.title())
            )));
            steps.push(Step::Press(Key::NextTab));
        }
        steps.push(Step::BackOnDisplay);
        match self.target(d) {
            Ok((row, name)) => {
                let from = model.get(name).unwrap_or_default();
                let display = super::state::rows(model, Tab::DISPLAY, d.menu.expert);
                let downs = super::state::downs_to(&display, row);
                steps.extend((0..downs).map(|_| Step::Press(Key::Down)));
                steps.push(Step::Selected { row, name });
                steps.push(Step::Press(Key::Right));
                steps.push(Step::Changed {
                    name,
                    from: from.clone(),
                });
                steps.push(Step::Settle);
                steps.push(Step::Grab(format!(
                    "menu-{at}s-live-{}.png",
                    file_part(name)
                )));
                steps.push(Step::Press(Key::Left));
                steps.push(Step::Restored { name, from });
                steps.extend((0..downs).map(|_| Step::Press(Key::Up)));
            }
            Err(why) => steps.push(Step::Note(why)),
        }
        steps.push(Step::Press(Key::Esc));
        steps.push(Step::Closed);
        steps.push(Step::InputBack);
        steps.push(Step::Settle);
        steps.push(Step::Grab(format!("menu-{at}s-closed.png")));
        self.steps.extend(steps);
    }

    /// The Display row to change and its setting: the plan's, else [`PREFERRED`] if Live, else
    /// the first Live on/off or choice setting.
    fn target(&self, d: &Driver) -> Result<(usize, &'static str), String> {
        let model = &*d.model;
        let infos = model.infos();
        let rows = super::state::rows(model, Tab::DISPLAY, d.menu.expert);
        let at = |name: &str| {
            rows.iter().enumerate().find_map(|(r, row)| match *row {
                Row::Setting(i) if infos[i].name == name => Some((r, infos[i].name)),
                _ => None,
            })
        };
        if let Some(name) = &self.plan.setting {
            return at(name).ok_or_else(|| format!("{name} is not on the Display tab: no value changed"));
        }
        let live = |name: &str| model.apply_of(name) == Apply::Live;
        if live(PREFERRED)
            && let Some(found) = at(PREFERRED)
        {
            return Ok(found);
        }
        rows.iter()
            .enumerate()
            .find_map(|(r, row)| match *row {
                Row::Setting(i)
                    if live(infos[i].name) && matches!(infos[i].kind, Kind::Bool | Kind::Choice(_)) =>
                {
                    Some((r, infos[i].name))
                }
                _ => None,
            })
            .ok_or_else(|| "no Live on/off or choice setting on the Display tab: no value changed".into())
    }
}

#[cfg(test)]
mod tests {
    use super::super::model::{Registry, registry_lock};
    use super::*;

    #[test]
    fn plans_parse() {
        assert_eq!(
            parse("120, 60,60"),
            Some(Plan { at_ms: vec![60_000, 120_000], setting: None })
        );
        assert_eq!(
            parse("30.5:VICTORY_FFX_SCALE"),
            Some(Plan { at_ms: vec![30_500], setting: Some("VICTORY_FFX_SCALE".into()) })
        );
        assert_eq!(parse(""), None);
        assert_eq!(parse("soon"), None);
        assert_eq!(parse(":VICTORY_OVERLAY"), None);
    }

    /// Polls the selftest and the driver as the window does, every 30 ms up to `until`.
    fn run(test: &mut Selftest, d: &mut Driver, until: u64) -> Vec<Out> {
        let mut out = Vec::new();
        for now in (0..until).step_by(30) {
            let blocked = d.blocked();
            let t = test.tick(now, d, blocked, false);
            d.tick(now, &t.keys, 0, 0);
            out.extend(t.out);
        }
        out
    }

    #[test]
    fn a_pass_opens_steps_every_tab_flips_a_live_setting_back_and_gives_the_input_back() {
        let _lock = registry_lock();
        let mut d = Driver::new(Box::new(Registry::default()));
        let before = d.model.get(PREFERRED);
        let tabs = d.menu.tabs(&*d.model);
        let mut test = Selftest::new(parse("1").unwrap());
        let out = run(&mut test, &mut d, 30_000);
        let logs: Vec<&str> = out
            .iter()
            .filter_map(|o| match o {
                Out::Log(s) => Some(s.as_str()),
                _ => None,
            })
            .collect();
        let warns: Vec<&Out> = out.iter().filter(|o| matches!(o, Out::Warn(_))).collect();
        assert!(warns.is_empty(), "{warns:?}");
        let has = |s: &str| logs.iter().any(|l| l.contains(s));
        assert!(has("menu selftest: pass at 1s starts"));
        assert!(has("menu selftest: opened (title input blocked=true)"));
        for (i, tab) in tabs.iter().enumerate() {
            assert!(has(&format!("tab {}/{} {} drawn", i + 1, tabs.len(), tab.title())), "{tab:?}");
        }
        assert!(has("value set VICTORY_OVERLAY \"1\" -> \"0\" (apply Live"), "{logs:#?}");
        assert!(has("value restored VICTORY_OVERLAY -> \"1\""));
        assert!(has("menu selftest: closed"));
        assert!(has("input returned to title"));
        assert!(has("pass at 1s done"));
        let grabs: Vec<&str> = out
            .iter()
            .filter_map(|o| match o {
                Out::Grab(s) => Some(s.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(grabs.len(), tabs.len() + 2);
        assert_eq!(grabs[0], "menu-1s-01-display.png");
        assert_eq!(grabs[tabs.len()], "menu-1s-live-victory_overlay.png");
        assert_eq!(*grabs.last().unwrap(), "menu-1s-closed.png");
        assert!(!d.menu.open && !d.blocked());
        assert_eq!(d.model.get(PREFERRED), before);
        assert!(d.model.pending_restart().is_empty());
        let _ = d.model.set(PREFERRED, "");
    }

    #[test]
    fn a_pass_with_the_menu_open_is_skipped_and_a_named_setting_off_display_is_reported() {
        let _lock = registry_lock();
        let mut d = Driver::new(Box::new(Registry::default()));
        d.tick(0, &[Key::Hotkey], 0, 0);
        d.tick(30, &[], 0, 0);
        let mut test = Selftest::new(parse("0.1,5:VICTORY_MENU").unwrap());
        let out = run(&mut test, &mut d, 300);
        assert!(matches!(&out[..], [Out::Warn(w)] if w.contains("0.1s skipped")), "{out:?}");
        d.tick(400, &[Key::Esc], 0, 0);
        d.tick(430, &[], 0, 0);
        let out = run(&mut test, &mut d, 30_000);
        assert!(out.iter().any(|o| matches!(o, Out::Warn(w) if w.contains("VICTORY_MENU is not on the Display tab"))));
        assert!(out.iter().any(|o| matches!(o, Out::Log(l) if l.contains("input returned to title"))));
        assert!(!d.blocked());
    }
}
