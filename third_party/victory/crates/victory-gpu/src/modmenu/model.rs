//! (El1xr) What the trainer menu (F8) shows and does, as data: each option is a row of the menu
//! (name, tab, label, help, kind) and the native it calls. A [`Effect::Read`] row shows a
//! native's result, asked for again while the menu draws it; a [`Effect::Set`] row turns the
//! value chosen into argument slots and queues the call. More options are more rows in
//! [`OPTIONS`], once their natives' hashes are known (see `AnyXboxX-coord/modmenu/STATE.md`).

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicI64, Ordering::Relaxed};
use std::time::{Duration, Instant};

use super::queue::{Call, Outcome, Queue, Script};
use super::{finder, stats, teleport};
use crate::menu::model::{Applied, Apply, Info, Kind, MenuModel, SetError};

/// What an option does.
#[derive(Clone, Copy)]
pub enum Effect {
    /// Shows our own state (`status` gives the text); no native.
    Status(fn() -> String),
    /// Shows a native's result, read again at most every [`READ_EVERY`] while it is drawn. With
    /// a `script`, the script runs instead (`hash` is its first native).
    Read {
        hash: u64,
        args: &'static [u64],
        script: Option<Script>,
        show: fn([u64; 4]) -> String,
    },
    /// Calls a native with the value chosen, as argument slots.
    Set {
        hash: u64,
        args: fn(&str) -> Vec<u64>,
    },
    /// Our own state that drives natives from the game thread (`get` shows it, `set` takes a
    /// valid value), such as the clock hold.
    Own { get: fn() -> String, set: fn(&str) },
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

// The player, mapped offline on 1.0.2.1 from the batch 5 census (`AnyXboxX-coord/modmenu/STATE.md`).
// Entity handles are 128-bit ids (two slots) and vectors are four packed floats (two slots).
/// `PLAYER_PED_ID`-like: 0 args; the local player global (`+0xab56e38`) through the title's
/// entity-to-id function (`+0x2a6520`, mode 2), a 128-bit handle in result slots 0-1. Called in
/// every census window.
pub const NATIVE_PLAYER_PED_ID: u64 = 0x6471_f11f_f69d_8a77;
/// `GET_ENTITY_COORDS`-like: (handle in slots 0-1, flag in slot 2); resolves the handle to an
/// entity (`+0x350170`, the type check `0xb1389614`), reads its matrix translation (`+0x70`) and
/// adds the world cell (`+0x4dc3790`): x, y, z, w floats in result slots 0-1.
pub const NATIVE_GET_ENTITY_COORDS: u64 = 0x6c66_0bb2_5792_a0f9;

/// Hours the clock rows take, as choices (an `Int` 0 would read "Off" in the menu).
pub const HOURS: &[&str] = &[
    "0", "1", "2", "3", "4", "5", "6", "7", "8", "9", "10", "11", "12", "13", "14", "15", "16",
    "17", "18", "19", "20", "21", "22", "23",
];
/// The clock hold's values: off, or the hour to keep.
pub const HOLD: &[&str] = &[
    "off", "0", "1", "2", "3", "4", "5", "6", "7", "8", "9", "10", "11", "12", "13", "14", "15",
    "16", "17", "18", "19", "20", "21", "22", "23",
];

/// The finders' one value: a button that starts (or restarts) them; the row shows their progress.
pub const RUN: &[&str] = &["run"];
/// One-press rows: the row shows what they will use, so each press is a change.
pub const REFILL: &[&str] = &["refill"];
pub const CLEAR: &[&str] = &["clear"];
pub const ON_OFF: &[&str] = &["off", "on"];

/// The hour the clock is held at (`-1`: no hold). The game-thread hook sets the clock to it on
/// every tick, so the title's own scripts (the prologue puts 7:30 back within 10 s) cannot move it.
static HOLD_HOUR: AtomicI64 = AtomicI64::new(-1);

/// The held hour, if any.
pub fn held_hour() -> Option<u64> {
    u64::try_from(HOLD_HOUR.load(Relaxed)).ok()
}

fn hold_get() -> String {
    held_hour().map_or_else(|| "off".into(), |h| h.to_string())
}

fn hold_set(v: &str) {
    HOLD_HOUR.store(v.parse().unwrap_or(-1), Relaxed);
}

/// The player's position: the player ped's handle, then that entity's coordinates.
fn player_coords(
    call: &mut dyn FnMut(u64, &[u64]) -> Option<[u64; 4]>,
    _: &[u64],
) -> Option<[u64; 4]> {
    let [lo, hi, ..] = call(NATIVE_PLAYER_PED_ID, &[])?;
    call(NATIVE_GET_ENTITY_COORDS, &[lo, hi, 0])
}

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

fn working_status() -> String {
    let natives = OPTIONS
        .iter()
        .filter(|o| !matches!(o.effect, Effect::Status(_)));
    let (ok, all) = natives.fold((0, 0), |(ok, all), o| {
        (ok + usize::from(o.verified), all + 1)
    });
    format!("{ok} of {all} confirmed in game; the rest say (unverified)")
}

fn show_int([v, ..]: [u64; 4]) -> String {
    (v as i64 as i32).to_string()
}

fn show_handle([lo, hi, ..]: [u64; 4]) -> String {
    format!("{hi:016x}{lo:016x}")
}

/// x y z of four packed floats in slots 0-1.
fn show_xyz([a, b, ..]: [u64; 4]) -> String {
    let f = |v: u64| f32::from_bits(v as u32);
    format!("{:.1} {:.1} {:.1}", f(a), f(a >> 32), f(b))
}

fn clock_time(v: &str) -> Vec<u64> {
    vec![v.parse().unwrap_or(0), 0, 0, 0]
}

// Rows in menu order: per tab, what works first. `verified`: batch 5 part I read the clock (12:00,
// 8:00, 7:30 as the prologue sets it), the frame rate (60 while loading, 30 in game) and set the
// hour (read 3 right after; the prologue's script put 7:30 back within 10 s, hence the hold);
// batch 5 item 5 held the clock at 3 to the end and read a constant player handle and a position
// that moved with the scene.
pub const OPTIONS: &[Opt] = &[
    Opt {
        info: info(
            "MOD_CLOCK_HOUR",
            "World",
            "Hour",
            "The in-game clock's hour, read through the title's clock getter.",
            Kind::Text,
        ),
        effect: Effect::Read {
            hash: NATIVE_CLOCK_HOURS,
            args: &[],
            script: None,
            show: show_int,
        },
        verified: true,
    },
    Opt {
        info: info(
            "MOD_CLOCK_MINUTE",
            "World",
            "Minute",
            "The in-game clock's minute.",
            Kind::Text,
        ),
        effect: Effect::Read {
            hash: NATIVE_CLOCK_MINUTES,
            args: &[],
            script: None,
            show: show_int,
        },
        verified: true,
    },
    Opt {
        info: info(
            "MOD_CLOCK_SET_HOUR",
            "World",
            "Set hour",
            "Sets the in-game clock to this hour, minute and second 0, once. Mission scripts may set \
             their own time again; use Hold hour to keep it.",
            Kind::Choice(HOURS),
        ),
        effect: Effect::Set {
            hash: NATIVE_SET_CLOCK_TIME,
            args: clock_time,
        },
        verified: true,
    },
    Opt {
        info: info(
            "MOD_CLOCK_HOLD",
            "World",
            "Hold hour",
            "Sets the clock to this hour on every game tick, so scripts cannot move it. Off lets the \
             clock run again.",
            Kind::Choice(HOLD),
        ),
        effect: Effect::Own {
            get: hold_get,
            set: hold_set,
        },
        verified: true,
    },
    Opt {
        info: info(
            "MOD_PLAYER_COORDS",
            "Player",
            "Position",
            "The player's x y z: the player ped's handle, then that entity's coordinates.",
            Kind::Text,
        ),
        effect: Effect::Read {
            hash: NATIVE_PLAYER_PED_ID,
            args: &[],
            script: Some(player_coords),
            show: show_xyz,
        },
        verified: true,
    },
    Opt {
        info: info(
            "MOD_PLAYER_PED",
            "Player",
            "Player ped handle",
            "The player ped's 128-bit entity handle; stays the same while the player does.",
            Kind::Text,
        ),
        effect: Effect::Read {
            hash: NATIVE_PLAYER_PED_ID,
            args: &[],
            script: None,
            show: show_handle,
        },
        verified: true,
    },
    Opt {
        info: info(
            "MOD_HEALTH",
            "Player",
            "Health (unverified)",
            "The player's health through the title's health getter (peds: 1000 is full).",
            Kind::Text,
        ),
        effect: Effect::Read {
            hash: NATIVE_PLAYER_PED_ID,
            args: &[],
            script: Some(stats::health_script),
            show: show_int,
        },
        verified: false,
    },
    Opt {
        info: info(
            "MOD_HEALTH_MAX",
            "Player",
            "Max health (unverified)",
            "The player's maximum health.",
            Kind::Text,
        ),
        effect: Effect::Read {
            hash: NATIVE_PLAYER_PED_ID,
            args: &[],
            script: Some(stats::max_health_script),
            show: show_int,
        },
        verified: false,
    },
    Opt {
        info: info(
            "MOD_HEALTH_REFILL",
            "Player",
            "Refill health (unverified)",
            "Sets the player's health to its maximum through the setter Find stats found.",
            Kind::Choice(REFILL),
        ),
        effect: Effect::Own {
            get: stats::refill_get,
            set: stats::refill_set,
        },
        verified: false,
    },
    Opt {
        info: info(
            "MOD_GOD_MODE",
            "Player",
            "God mode (unverified)",
            "Refills the player's health whenever it is below the maximum (checked every 10 game ticks).",
            Kind::Choice(ON_OFF),
        ),
        effect: Effect::Own {
            get: stats::god_get,
            set: stats::god_set,
        },
        verified: false,
    },
    Opt {
        info: info(
            "MOD_ARMOUR_REFILL",
            "Player",
            "Refill armour (unverified)",
            "Sets the player's armour to 100 through the setter Find stats took for armour.",
            Kind::Choice(REFILL),
        ),
        effect: Effect::Own {
            get: stats::armour_get,
            set: stats::armour_set,
        },
        verified: false,
    },
    Opt {
        info: info(
            "MOD_WANTED_CLEAR",
            "Player",
            "Clear wanted level (unverified)",
            "Sets the wanted level to 0 through the setter Find stats found.",
            Kind::Choice(CLEAR),
        ),
        effect: Effect::Own {
            get: stats::clear_wanted_get,
            set: stats::clear_wanted_set,
        },
        verified: false,
    },
    Opt {
        info: info(
            "MOD_NEVER_WANTED",
            "Player",
            "Never wanted (unverified)",
            "Keeps the wanted level at 0 (set every 10 game ticks).",
            Kind::Choice(ON_OFF),
        ),
        effect: Effect::Own {
            get: stats::never_wanted_get,
            set: stats::never_wanted_set,
        },
        verified: false,
    },
    Opt {
        info: info(
            "MOD_FIND_STATS",
            "Player",
            "Find stats (unverified)",
            "Calls each candidate health, armour and wanted setter with a test value and keeps the ones a getter reads back; then refills health. Logged line by line.",
            Kind::Choice(RUN),
        ),
        effect: Effect::Own {
            get: finder::stats_get,
            set: finder::stats_set,
        },
        verified: false,
    },
    Opt {
        info: info(
            "MOD_TP_NATIVE",
            "Teleport",
            "Teleport native",
            "The native the teleport rows call: found in game by Find teleport, or named by \
             VICTORY_MOD_MENU_TP_NATIVE.",
            Kind::Text,
        ),
        effect: Effect::Status(teleport::native_status),
        verified: true,
    },
    Opt {
        info: info(
            "MOD_TP_LAST",
            "Teleport",
            "Last teleport",
            "What the last save or teleport did, with the position read back after it.",
            Kind::Text,
        ),
        effect: Effect::Status(teleport::last_get),
        verified: true,
    },
    Opt {
        info: info(
            "MOD_TP_SAVE_1",
            "Teleport",
            "Save spot 1 (unverified)",
            "Saves where the player stands as spot 1 (kept until the game closes). Shows the spot.",
            Kind::Choice(teleport::SAVE),
        ),
        effect: Effect::Own {
            get: teleport::spot_1,
            set: teleport::save_1,
        },
        verified: false,
    },
    Opt {
        info: info(
            "MOD_TP_SAVE_2",
            "Teleport",
            "Save spot 2 (unverified)",
            "Saves where the player stands as spot 2 (kept until the game closes). Shows the spot.",
            Kind::Choice(teleport::SAVE),
        ),
        effect: Effect::Own {
            get: teleport::spot_2,
            set: teleport::save_2,
        },
        verified: false,
    },
    Opt {
        info: info(
            "MOD_TP_SAVE_3",
            "Teleport",
            "Save spot 3 (unverified)",
            "Saves where the player stands as spot 3 (kept until the game closes). Shows the spot.",
            Kind::Choice(teleport::SAVE),
        ),
        effect: Effect::Own {
            get: teleport::spot_3,
            set: teleport::save_3,
        },
        verified: false,
    },
    Opt {
        info: info(
            "MOD_TP_GOTO_1",
            "Teleport",
            "Go to spot 1 (unverified)",
            "Moves the player to spot 1 through the teleport native and reads the position back \
             (Last teleport shows how close it landed).",
            Kind::Choice(teleport::GO),
        ),
        effect: Effect::Own {
            get: teleport::spot_1,
            set: teleport::goto_1,
        },
        verified: false,
    },
    Opt {
        info: info(
            "MOD_TP_GOTO_2",
            "Teleport",
            "Go to spot 2 (unverified)",
            "Moves the player to spot 2 through the teleport native and reads the position back \
             (Last teleport shows how close it landed).",
            Kind::Choice(teleport::GO),
        ),
        effect: Effect::Own {
            get: teleport::spot_2,
            set: teleport::goto_2,
        },
        verified: false,
    },
    Opt {
        info: info(
            "MOD_TP_GOTO_3",
            "Teleport",
            "Go to spot 3 (unverified)",
            "Moves the player to spot 3 through the teleport native and reads the position back \
             (Last teleport shows how close it landed).",
            Kind::Choice(teleport::GO),
        ),
        effect: Effect::Own {
            get: teleport::spot_3,
            set: teleport::goto_3,
        },
        verified: false,
    },
    Opt {
        info: info(
            "MOD_TP_FIND",
            "Teleport",
            "Find teleport (unverified)",
            "Tries each candidate native on the player with a spot 3 m away and keeps the first \
             that lands the player there, then moves the player back. Logged line by line.",
            Kind::Choice(RUN),
        ),
        effect: Effect::Own {
            get: finder::tp_get,
            set: finder::tp_set,
        },
        verified: false,
    },
    Opt {
        info: info(
            "MOD_WORKING",
            "Status",
            "Working options",
            "Rows confirmed by a game run; the others are mapped offline and await a check.",
            Kind::Text,
        ),
        effect: Effect::Status(working_status),
        verified: true,
    },
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
            "Title frame rate",
            "Read through the title's own native on its thread: proves calls work (60 while \
             loading, 30 in game).",
            Kind::Text,
        ),
        effect: Effect::Read {
            hash: NATIVE_TITLE_FPS,
            args: &[],
            script: None,
            show: show_int,
        },
        verified: true,
    },
    Opt {
        info: info(
            "MOD_GET_SWEEP",
            "Status",
            "Getter sweep (unverified)",
            "Logs every candidate getter's value for the player (health, armour and wanted level \
             are among them), then 60 s later the ones that changed. For mapping, not play.",
            Kind::Choice(RUN),
        ),
        effect: Effect::Own {
            get: finder::get_get,
            set: finder::get_set,
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
            Effect::Read {
                hash,
                args,
                script,
                show,
            } => {
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
                        script,
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
            Effect::Own { get, .. } => Some(get()),
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
        if !victory_core::settings::valid(opt.info.kind, value) {
            return Err(invalid());
        }
        let (hash, args) = match opt.effect {
            Effect::Set { hash, args } => (hash, args),
            Effect::Own { set, .. } => {
                set(value);
                return Ok(Applied::Now);
            }
            Effect::Status(_) | Effect::Read { .. } => return Err(invalid()),
        };
        let call = Call {
            name: opt.info.name,
            hash,
            args: args(value),
            script: None,
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
                script: None,
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
        assert_eq!(
            args("13"),
            vec![13, 0, 0, 0],
            "hour, minute 0, second 0, flag 0"
        );
        assert!(victory_core::settings::valid(set.info.kind, "0"));
        assert!(!victory_core::settings::valid(set.info.kind, "24"));
        let t = Trainer::new(OPTIONS, queue());
        assert_eq!(
            tabs(&t, false),
            vec![
                Tab::Group("World"),
                Tab::Group("Player"),
                Tab::Group("Teleport"),
                Tab::Group("Status")
            ],
            "opens on the clock, which works"
        );
        for o in OPTIONS {
            let tab: Vec<_> = OPTIONS
                .iter()
                .filter(|p| p.info.group == o.info.group)
                .collect();
            let first_unverified = tab.iter().position(|p| !p.verified).unwrap_or(tab.len());
            assert!(
                tab[first_unverified..].iter().all(|p| !p.verified),
                "working rows first in {}",
                o.info.group
            );
        }
    }

    #[test]
    fn teleport_rows_are_buttons_that_queue_their_spot_on_the_game_thread() {
        use crate::menu::state::step_value;
        let mut t = Trainer::new(OPTIONS, queue());
        let save = OPTIONS.iter().find(|o| o.info.name == "MOD_TP_SAVE_2").unwrap();
        let shown = t.get("MOD_TP_SAVE_2").unwrap();
        assert_eq!(shown, "empty", "the row shows the spot");
        assert_eq!(
            step_value(save.info.name, save.info.kind, &shown, 1).as_deref(),
            Some("save"),
            "each press is a change, so each press saves"
        );
        assert_eq!(t.set("MOD_TP_SAVE_2", "save"), Ok(Applied::Now));
        assert_eq!(t.set("MOD_TP_GOTO_3", "go"), Ok(Applied::Now));
        assert_eq!(t.set("MOD_TP_FIND", "run"), Ok(Applied::Now));
        assert!(t.get("MOD_TP_FIND").unwrap().starts_with("trying 1/"));
        finder::tp_set("off");
        let mut seen = Vec::new();
        super::super::QUEUE.drain(|c| {
            seen.push((c.name, c.args.clone()));
            None
        });
        assert!(seen.contains(&("tp save", vec![2])), "{seen:?}");
        assert!(seen.contains(&("tp goto", vec![3])), "{seen:?}");
    }

    #[test]
    fn the_clock_hold_is_our_own_state() {
        let q = queue();
        let mut t = Trainer::new(OPTIONS, q);
        assert_eq!(t.get("MOD_CLOCK_HOLD").as_deref(), Some("off"));
        assert_eq!(t.set("MOD_CLOCK_HOLD", "3"), Ok(Applied::Now));
        assert_eq!(held_hour(), Some(3));
        assert_eq!(t.get("MOD_CLOCK_HOLD").as_deref(), Some("3"));
        assert!(t.set("MOD_CLOCK_HOLD", "24").is_err());
        assert_eq!(held_hour(), Some(3), "a bad value changes nothing");
        assert_eq!(t.set("MOD_CLOCK_HOLD", "off"), Ok(Applied::Now));
        assert_eq!(held_hour(), None);
        assert!(
            q.idle(),
            "the game-thread hook, not the menu, calls the native"
        );
    }

    #[test]
    fn player_position_reads_the_ped_then_its_coordinates() {
        let q = queue();
        let t = Trainer::new(OPTIONS, q);
        assert_eq!(t.get("MOD_PLAYER_COORDS").as_deref(), Some("reading"));
        let xyz = |x: f32, y: f32, z: f32| {
            [
                u64::from(x.to_bits()) | u64::from(y.to_bits()) << 32,
                u64::from(z.to_bits()),
                0,
                0,
            ]
        };
        let mut natives = Vec::new();
        q.drain(|c| {
            let script = c.script.expect("a script row");
            script(
                &mut |hash, args| {
                    natives.push((hash, args.to_vec()));
                    match hash {
                        NATIVE_PLAYER_PED_ID => Some([0x11, 0x22, 0, 0]),
                        NATIVE_GET_ENTITY_COORDS => Some(xyz(-1.5, 1024.0, 30.5)),
                        _ => None,
                    }
                },
                &c.args,
            )
        });
        assert_eq!(
            natives,
            vec![
                (NATIVE_PLAYER_PED_ID, vec![]),
                (NATIVE_GET_ENTITY_COORDS, vec![0x11, 0x22, 0])
            ],
            "the handle's two slots, then the flag"
        );
        assert_eq!(
            t.get("MOD_PLAYER_COORDS").as_deref(),
            Some("-1.5 1024.0 30.5")
        );
        assert_eq!(
            show_handle([0x11, 0x22, 0, 0]),
            "00000000000000220000000000000011"
        );
        assert_eq!(
            player_coords(&mut |_, _| None, &[]),
            None,
            "no ped, no read"
        );
        assert!(
            working_status().starts_with("7 of 23 confirmed"),
            "{}",
            working_status()
        );
    }
}
