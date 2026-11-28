//! (El1xr) What the trainer menu (F8) shows and does, as data: each option is a row of the menu
//! (name, tab, label, help, kind) and the native it calls. A [`Effect::Read`] row shows a
//! native's result, asked for again while the menu draws it; a [`Effect::Set`] row turns the
//! value chosen into argument slots and queues the call. More options are more rows in
//! [`OPTIONS`], once their natives' hashes are known (see `AnyXboxX-coord/modmenu/STATE.md`).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use super::queue::{Call, Outcome, Queue};
use crate::menu::model::{Applied, Apply, Info, Kind, MenuModel, SetError};

/// What an option does.
#[derive(Clone, Copy)]
pub enum Effect {
    /// Shows our own state (`status` gives the text); no native.
    Status(fn() -> String),
    /// Shows a native's result, read again at most every [`READ_EVERY`] while it is drawn.
    Read {
        hash: u64,
        args: &'static [u64],
        show: fn([u64; 4]) -> String,
    },
    /// Calls a native with the value chosen, as argument slots.
    Set {
        hash: u64,
        args: fn(&str) -> Vec<u64>,
    },
}

pub struct Opt {
    pub info: Info,
    pub effect: Effect,
    /// Whether a run confirmed the native does what the row says. Hashes mapped offline by
    /// behaviour start unverified and say so in their label.
    pub verified: bool,
}

pub const READ_EVERY: Duration = Duration::from_millis(500);

/// `MISC`-like: the title's frame rate as scripts see it (`1e6 / frame_us`, 30 on Series X; the
/// function the handler calls is `+0x3eaf5d0`, see `victory-loader`'s `frame_rate.rs`).
pub const NATIVE_TITLE_FPS: u64 = 0x0bb2_0f74_b5a2_5613;

// The clock, mapped offline on 1.0.2.1 (see `AnyXboxX-coord/modmenu/STATE.md`): seven 0-arg int
// getters side by side each return one dword of the clock's date-time block; the title computes
// `h * 3600 + m * 60 + s` from three of them, which names the hour, minute and second.
/// `GET_CLOCK_HOURS`-like: 0 args, int.
pub const NATIVE_CLOCK_HOURS: u64 = 0x3d4e_6e66_37d3_0df7;
/// `GET_CLOCK_MINUTES`-like: 0 args, int.
pub const NATIVE_CLOCK_MINUTES: u64 = 0x47dc_2440_b1e9_ddce;
/// `SET_CLOCK_TIME`-like: (hour, minute, second, flag); calls the clock's own time setter (the
/// one the title's "advance to" native also ends in). The flag (1) first runs a step of its own,
/// unknown; the trainer passes 0.
pub const NATIVE_SET_CLOCK_TIME: u64 = 0x04d2_1127_c5ad_3a60;

pub(crate) const fn info(
    name: &'static str,
    group: &'static str,
    label: &'static str,
    help: &'static str,
    kind: Kind,
) -> Info {
    Info {
        name,
        group,
        label,
        help,
        kind,
        default: "",
        apply: Apply::Live,
        expert: false,
    }
}

fn natives_status() -> String {
    super::status()
}

fn show_int([v, ..]: [u64; 4]) -> String {
    (v as i64 as i32).to_string()
}

fn clock_time(v: &str) -> Vec<u64> {
    vec![v.parse().unwrap_or(0), 0, 0, 0]
}

pub const OPTIONS: &[Opt] = &[
    Opt {
        info: info(
            "MOD_NATIVES",
            "Status",
            "Natives",
            "Script native handlers found in the title by their shape.",
            Kind::Text,
        ),
        effect: Effect::Status(natives_status),
        verified: true,
    },
    Opt {
        info: info(
            "MOD_TITLE_FPS",
            "Status",
            "Title frame rate (unverified)",
            "Read through the title's own native on its thread: proves calls work.",
            Kind::Text,
        ),
        effect: Effect::Read {
            hash: NATIVE_TITLE_FPS,
            args: &[],
            show: show_int,
        },
        verified: false,
    },
    Opt {
        info: info(
            "MOD_CLOCK_HOUR",
            "World",
            "Hour (unverified)",
            "The in-game clock's hour, read through the title's clock getter.",
            Kind::Text,
        ),
        effect: Effect::Read {
            hash: NATIVE_CLOCK_HOURS,
            args: &[],
            show: show_int,
        },
        verified: false,
    },
    Opt {
        info: info(
            "MOD_CLOCK_MINUTE",
            "World",
            "Minute (unverified)",
            "The in-game clock's minute.",
            Kind::Text,
        ),
        effect: Effect::Read {
            hash: NATIVE_CLOCK_MINUTES,
            args: &[],
            show: show_int,
        },
        verified: false,
    },
    Opt {
        info: info(
            "MOD_CLOCK_SET_HOUR",
            "World",
            "Set hour (unverified)",
            "Sets the in-game clock to this hour, minute and second 0.",
            Kind::Int { min: 0, max: 23 },
        ),
        effect: Effect::Set {
            hash: NATIVE_SET_CLOCK_TIME,
            args: clock_time,
        },
        verified: false,
    },
];

/// The trainer's options behind [`MenuModel`].
pub struct Trainer {
    options: &'static [Opt],
    infos: Vec<Info>,
    queue: &'static Queue,
    /// Values chosen for `Set` rows, and when each `Read` row last asked.
    chosen: HashMap<&'static str, String>,
    asked: Mutex<HashMap<&'static str, Instant>>,
}

impl Trainer {
    pub fn new(options: &'static [Opt], queue: &'static Queue) -> Self {
        Trainer {
            options,
            infos: options.iter().map(|o| o.info).collect(),
            queue,
            chosen: HashMap::new(),
            asked: Mutex::new(HashMap::new()),
        }
    }

    fn opt(&self, name: &str) -> Option<&'static Opt> {
        self.options.iter().find(|o| o.info.name == name)
    }
}

impl MenuModel for Trainer {
    fn infos(&self) -> &[Info] {
        &self.infos
    }

    fn get(&self, name: &str) -> Option<String> {
        let opt = self.opt(name)?;
        match opt.effect {
            Effect::Status(status) => Some(status()),
            Effect::Read { hash, args, show } => {
                let now = Instant::now();
                let mut asked = self.asked.lock().unwrap_or_else(|e| e.into_inner());
                let due = asked
                    .get(opt.info.name)
                    .is_none_or(|&t| now - t >= READ_EVERY);
                if due
                    && self.queue.push(Call {
                        name: opt.info.name,
                        hash,
                        args: args.to_vec(),
                    })
                {
                    asked.insert(opt.info.name, now);
                }
                Some(match self.queue.result(opt.info.name) {
                    Some(Outcome::Done(r)) => show(r),
                    Some(Outcome::Unknown) => "not in this build".into(),
                    None => "reading".into(),
                })
            }
            Effect::Set { .. } => Some(
                self.chosen
                    .get(opt.info.name)
                    .cloned()
                    .unwrap_or_else(|| opt.info.default.to_string()),
            ),
        }
    }

    fn launched(&self, name: &str) -> Option<String> {
        self.get(name)
    }

    fn apply_of(&self, _: &str) -> Apply {
        Apply::Live
    }

    fn set(&mut self, name: &str, value: &str) -> Result<Applied, SetError> {
        let invalid = || SetError::Invalid {
            name: name.to_string(),
            value: value.to_string(),
        };
        let opt = self
            .opt(name)
            .ok_or_else(|| SetError::Unknown(name.to_string()))?;
        let Effect::Set { hash, args } = opt.effect else {
            return Err(invalid());
        };
        if !victory_core::settings::valid(opt.info.kind, value) {
            return Err(invalid());
        }
        let call = Call {
            name: opt.info.name,
            hash,
            args: args(value),
        };
        if !self.queue.push(call) {
            return Err(invalid());
        }
        self.chosen.insert(opt.info.name, value.to_string());
        Ok(Applied::Now)
    }

    fn pending_restart(&self) -> Vec<&'static str> {
        Vec::new()
    }

    fn save(&mut self) -> std::io::Result<()> {
        Ok(())
    }

    fn title(&self) -> &'static str {
        "Trainer"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::menu::state::{Menu, Tab, rows, tabs};

    fn queue() -> &'static Queue {
        Box::leak(Box::default())
    }

    fn hours(v: &str) -> Vec<u64> {
        vec![v.parse().unwrap_or(0), 0, 0]
    }

    const TEST_OPTIONS: &[Opt] = &[
        Opt {
            info: info("T_READ", "World", "Read", "", Kind::Text),
            effect: Effect::Read {
                hash: 0xaa,
                args: &[7],
                show: show_int,
            },
            verified: true,
        },
        Opt {
            info: info("T_HOUR", "World", "Hour", "", Kind::Int { min: 0, max: 23 }),
            effect: Effect::Set {
                hash: 0xbb,
                args: hours,
            },
            verified: true,
        },
        Opt {
            info: info("T_STATUS", "Status", "Status", "", Kind::Text),
            effect: Effect::Status(|| "fine".into()),
            verified: true,
        },
    ];

    #[test]
    fn options_are_rows_under_their_tabs_without_presets() {
        let t = Trainer::new(TEST_OPTIONS, queue());
        assert_eq!(
            tabs(&t, false),
            vec![Tab::Group("World"), Tab::Group("Status")]
        );
        assert_eq!(rows(&t, Tab::Group("World"), false).len(), 2);
        let mut menu = Menu::for_model(&t);
        assert_eq!(menu.tab, Tab::Group("World"), "opens on its first tab");
        menu.open = true;
        assert_eq!(t.title(), "Trainer");
    }

    #[test]
    fn reads_queue_a_call_at_most_every_interval_and_show_the_result() {
        let q = queue();
        let t = Trainer::new(TEST_OPTIONS, q);
        assert_eq!(t.get("T_READ").as_deref(), Some("reading"));
        assert_eq!(t.get("T_READ").as_deref(), Some("reading"));
        let mut seen = Vec::new();
        q.drain(|c| {
            seen.push((c.hash, c.args.clone()));
            Some([u64::from(u32::MAX), 0, 0, 0])
        });
        assert_eq!(seen, vec![(0xaa, vec![7])], "one call for two draws");
        assert_eq!(t.get("T_READ").as_deref(), Some("-1"), "an int result");
        assert_eq!(t.get("T_STATUS").as_deref(), Some("fine"));
        assert_eq!(t.get("NOPE"), None);
    }

    #[test]
    fn sets_queue_the_native_with_the_value_as_slots() {
        let q = queue();
        let mut t = Trainer::new(TEST_OPTIONS, q);
        assert_eq!(t.set("T_HOUR", "13"), Ok(Applied::Now));
        assert_eq!(t.get("T_HOUR").as_deref(), Some("13"));
        assert!(t.set("T_HOUR", "24").is_err(), "out of range");
        assert!(t.set("T_READ", "1").is_err(), "a read takes no value");
        assert!(matches!(t.set("NOPE", "1"), Err(SetError::Unknown(_))));
        let mut seen = Vec::new();
        q.drain(|c| {
            seen.push((c.name, c.hash, c.args.clone()));
            None
        });
        assert_eq!(seen, vec![("T_HOUR", 0xbb, vec![13, 0, 0])]);
        assert_eq!(q.result("T_HOUR"), Some(Outcome::Unknown));
    }

    #[test]
    fn shipped_options_are_unique_and_valid() {
        let mut names: Vec<_> = OPTIONS.iter().map(|o| o.info.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), OPTIONS.len());
        for o in OPTIONS {
            assert!(
                !o.info.name.starts_with("VICTORY_"),
                "not settings: {}",
                o.info.name
            );
            let native = !matches!(o.effect, Effect::Status(_));
            assert_eq!(
                native && !o.verified,
                o.info.label.contains("(unverified)"),
                "unverified natives say so: {}",
                o.info.name
            );
        }
        let set = OPTIONS
            .iter()
            .find(|o| o.info.name == "MOD_CLOCK_SET_HOUR")
            .unwrap();
        let Effect::Set { hash, args } = set.effect else {
            panic!("a set row")
        };
        assert_eq!(hash, NATIVE_SET_CLOCK_TIME);
        assert_eq!(args("13"), vec![13, 0, 0, 0], "hour, minute 0, second 0, flag 0");
    }
}
