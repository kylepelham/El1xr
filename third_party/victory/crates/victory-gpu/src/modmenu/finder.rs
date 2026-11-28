//! (El1xr) Finds natives in game by what they do, for the rows offline analysis could not pin.
//!
//! Find teleport (`MOD_TP_FIND=run`): for each [`candidates::TELEPORT`] native in turn it reads
//! the player's position, calls the candidate with the player and a spot [`OFFSET`] away, and
//! reads the position again at once and [`WAIT_TICKS`] later. The first that puts the player on
//! the spot becomes the trainer's teleport native ([`teleport::set_native`]); the finder then
//! moves the player back and stops. Each try is logged before the call
//! (`mod menu: teleport find try i/N <hash>`), so a crash names the candidate:
//! `VICTORY_MOD_MENU_TP_SKIP=<hash>,...` leaves it out next time.
//!
//! Getter sweep (`MOD_GET_SWEEP=run`): calls every [`candidates::ENTITY_GETTERS`] native with the
//! player ped and every [`candidates::PLAYER_GETTERS`] native with player 0 and 255, logs each
//! result (`mod menu: get sweep 1 ...`), and [`PASS_GAP`] later logs those that changed
//! (`mod menu: get sweep 2 ...`). Health, armour and wanted level are read by some of them; the
//! values (and what changes between passes) name them.
//!
//! Find stats (`MOD_FIND_STATS=run`): the pair finder ([`stats::PairFind`]) on the player ped, then
//! on player 0; what it finds drives the health, armour and wanted rows.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::time::{Duration, Instant};

use super::{candidates, stats};
use super::teleport::{self, Native, coords, dist, move_to, player, show};

/// Where a teleport candidate is asked to put the player, from where the player stands.
pub const OFFSET: [f32; 3] = [3.0, 0.0, 0.5];
/// Game ticks to wait before the second read (a warp the title runs on a later frame).
pub const WAIT_TICKS: u32 = 15;
/// Getters called per game tick.
pub const CHUNK: usize = 24;
/// Time between the getter sweep's two passes.
pub const PASS_GAP: Duration = Duration::from_secs(60);

fn add(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}

/// On the spot: within `xy` metres across and 2 m up or down (the player may fall or step).
fn landed(now: [f32; 3], target: [f32; 3], xy: f32) -> bool {
    let across = ((now[0] - target[0]).powi(2) + (now[1] - target[1]).powi(2)).sqrt();
    across <= xy && (now[2] - target[2]).abs() <= 2.0
}

struct Wait {
    ticks: u32,
    before: [f32; 3],
    target: [f32; 3],
}

/// Find teleport's state, one step per game tick.
pub struct TpFind {
    list: Vec<(u64, usize)>,
    idx: usize,
    wait: Option<Wait>,
    back: Option<(u32, [f32; 3])>,
    no_player_logged: bool,
    pub found: Option<(u64, usize)>,
    pub done: bool,
}

impl TpFind {
    pub fn new(list: &[(u64, u8)], skip: &[u64]) -> Self {
        Self {
            list: list
                .iter()
                .filter(|(h, _)| !skip.contains(h))
                .map(|&(h, s)| (h, usize::from(s)))
                .collect(),
            idx: 0,
            wait: None,
            back: None,
            no_player_logged: false,
            found: None,
            done: false,
        }
    }

    pub fn status(&self) -> String {
        match (self.found, self.done) {
            (Some((h, _)), _) => format!("found {h:#018x}"),
            (None, true) => format!("none of {} moved the player", self.list.len()),
            (None, false) => format!("trying {}/{}", self.idx + 1, self.list.len()),
        }
    }

    fn found(&mut self, call: &mut Native, log: &mut dyn FnMut(String), how: &str, w: &Wait) {
        let (hash, slots) = self.list[self.idx];
        log(format!(
            "teleport find found {hash:#018x} slots={slots} ({how}): from {} to {}",
            show(w.before),
            show(w.target)
        ));
        self.found = Some((hash, slots));
        if let Some(ped) = player(call) {
            move_to(call, hash, slots, ped, w.before);
        }
        self.back = Some((WAIT_TICKS, w.before));
    }

    /// One game tick. `log` gets each line as it happens (a try before its call).
    pub fn tick(&mut self, call: &mut Native, log: &mut dyn FnMut(String)) {
        if self.done {
            return;
        }
        if let Some((ticks, start)) = self.back.as_mut() {
            *ticks = ticks.saturating_sub(1);
            if *ticks == 0 {
                let start = *start;
                let now = player(call).and_then(|p| coords(call, p));
                log(match now {
                    Some(now) => format!(
                        "teleport find back at {} (off by {:.2} m from the start)",
                        show(now),
                        dist(now, start)
                    ),
                    None => "teleport find back: no position".into(),
                });
                self.done = true;
            }
            return;
        }
        if let Some(w) = self.wait.as_mut() {
            w.ticks = w.ticks.saturating_sub(1);
            if w.ticks > 0 {
                return;
            }
            let w = self.wait.take().expect("waiting");
            let now = player(call).and_then(|p| coords(call, p));
            match now {
                Some(now) if landed(now, w.target, 1.0) => {
                    return self.found(call, log, "after a wait", &w);
                }
                Some(now) => log(format!(
                    "teleport find no {:#018x}: moved {:.2} m, {:.2} m off the spot",
                    self.list[self.idx].0,
                    dist(now, w.before),
                    dist(now, w.target)
                )),
                None => log("teleport find: position lost".into()),
            }
            self.idx += 1;
            return;
        }
        if self.idx >= self.list.len() {
            log(format!(
                "teleport find: none of {} candidates moved the player",
                self.list.len()
            ));
            self.done = true;
            return;
        }
        let Some((ped, before)) = player(call).and_then(|p| Some((p, coords(call, p)?))) else {
            if !self.no_player_logged {
                log("teleport find: no player yet, waiting".into());
                self.no_player_logged = true;
            }
            return;
        };
        let (hash, slots) = self.list[self.idx];
        let target = add(before, OFFSET);
        log(format!(
            "teleport find try {}/{} {hash:#018x} slots={slots} from {}",
            self.idx + 1,
            self.list.len(),
            show(before)
        ));
        move_to(call, hash, slots, ped, target);
        let w = Wait {
            ticks: WAIT_TICKS,
            before,
            target,
        };
        match coords(call, ped) {
            Some(now) if landed(now, target, 0.25) => self.found(call, log, "at once", &w),
            _ => self.wait = Some(w),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Arg {
    Ped,
    Player(u8),
}

/// The getter sweep's state.
pub struct GetSweep {
    list: Vec<(u64, Arg)>,
    pass: u8,
    next: usize,
    pass_end: Option<Instant>,
    seen: HashMap<(u64, Arg), u64>,
    changed: usize,
    pub done: bool,
}

/// A result slot as an int, a float and raw.
fn show_raw(v: u64) -> String {
    let f = f32::from_bits(v as u32);
    let f = if f.is_finite() && f.abs() < 1e7 {
        format!("{f:.3}")
    } else {
        format!("{f:e}")
    };
    format!("i32 {} f32 {f} raw {v:#x}", v as u32 as i32)
}

impl GetSweep {
    pub fn new(entity: &[u64], players: &[u64]) -> Self {
        let mut list: Vec<(u64, Arg)> = entity.iter().map(|&h| (h, Arg::Ped)).collect();
        for &p in &[0u8, 0xff] {
            list.extend(players.iter().map(|&h| (h, Arg::Player(p))));
        }
        Self {
            list,
            pass: 1,
            next: 0,
            pass_end: None,
            seen: HashMap::new(),
            changed: 0,
            done: false,
        }
    }

    pub fn status(&self) -> String {
        if self.done {
            format!("done: {} changed between passes", self.changed)
        } else {
            format!("pass {} at {}/{}", self.pass, self.next, self.list.len())
        }
    }

    pub fn tick(&mut self, now: Instant, call: &mut Native, log: &mut dyn FnMut(String)) {
        if self.done || self.pass_end.is_some_and(|t| now - t < PASS_GAP) {
            return;
        }
        let Some(ped) = player(call) else { return };
        let end = (self.next + CHUNK).min(self.list.len());
        for &(hash, arg) in &self.list[self.next..end] {
            let args: Vec<u64> = match arg {
                Arg::Ped => ped.to_vec(),
                Arg::Player(p) => vec![u64::from(p)],
            };
            let Some([v, ..]) = call(hash, &args) else {
                continue;
            };
            let what = match arg {
                Arg::Ped => "ped".to_string(),
                Arg::Player(p) => format!("player {p}"),
            };
            if self.pass == 1 {
                self.seen.insert((hash, arg), v);
                log(format!("get sweep 1 {hash:016x} {what} -> {}", show_raw(v)));
            } else if let Some(&old) = self.seen.get(&(hash, arg))
                && old != v
            {
                self.changed += 1;
                log(format!(
                    "get sweep 2 {hash:016x} {what} changed {} -> {}",
                    show_raw(old),
                    show_raw(v)
                ));
            }
        }
        self.next = end;
        if self.next < self.list.len() {
            return;
        }
        if self.pass == 1 {
            log(format!(
                "get sweep 1 done: {} natives; pass 2 in {} s logs what changed",
                self.list.len(),
                PASS_GAP.as_secs()
            ));
            self.pass = 2;
            self.next = 0;
            self.pass_end = Some(now);
        } else {
            log(format!("get sweep 2 done: {} changed", self.changed));
            self.done = true;
        }
    }
}

static ACTIVE: AtomicBool = AtomicBool::new(false);
static TP: Mutex<Option<TpFind>> = Mutex::new(None);
static GET: Mutex<Option<GetSweep>> = Mutex::new(None);
/// Find stats: the ped pass, then the player pass ([`stats::PairFind`]).
static PAIRS: Mutex<Vec<stats::PairFind>> = Mutex::new(Vec::new());

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// `VICTORY_MOD_MENU_TP_SKIP=<hash>,...`.
pub fn parse_skip(v: &str) -> Vec<u64> {
    v.split(',')
        .filter_map(|h| {
            u64::from_str_radix(
                h.trim().trim_start_matches("0x").replace('_', "").as_str(),
                16,
            )
            .ok()
        })
        .collect()
}

pub fn tp_set(v: &str) {
    *lock(&TP) = (v == "run").then(|| {
        let skip = std::env::var("VICTORY_MOD_MENU_TP_SKIP")
            .map_or_else(|_| Vec::new(), |v| parse_skip(&v));
        TpFind::new(candidates::TELEPORT, &skip)
    });
    refresh();
}

/// One lock at a time: the menu thread and the game thread both set and tick the finders.
fn refresh() {
    let tp = lock(&TP).is_some();
    let get = lock(&GET).is_some();
    ACTIVE.store(tp || get || !lock(&PAIRS).is_empty(), Relaxed);
}

pub fn stats_set(v: &str) {
    *lock(&PAIRS) = if v == "run" {
        vec![stats::PairFind::ped(), stats::PairFind::player()]
    } else {
        Vec::new()
    };
    refresh();
}

pub fn stats_get() -> String {
    let pairs = lock(&PAIRS);
    if pairs.is_empty() {
        return "off".into();
    }
    let s: Vec<String> = pairs.iter().map(stats::PairFind::status).collect();
    format!("ped {}; player {}", s[0], s[1])
}

pub fn tp_get() -> String {
    lock(&TP)
        .as_ref()
        .map_or_else(|| "off".into(), TpFind::status)
}

pub fn get_set(v: &str) {
    *lock(&GET) =
        (v == "run").then(|| GetSweep::new(candidates::ENTITY_GETTERS, candidates::PLAYER_GETTERS));
    refresh();
}

pub fn get_get() -> String {
    lock(&GET)
        .as_ref()
        .map_or_else(|| "off".into(), GetSweep::status)
}

/// The game-thread step; one atomic load while nothing runs.
pub fn tick(call: &mut Native) {
    if !ACTIVE.load(Relaxed) {
        return;
    }
    let mut log = |s: String| tracing::info!("mod menu: {s}");
    if let Some(tp) = lock(&TP).as_mut()
        && !tp.done
    {
        tp.tick(call, &mut log);
        if let Some((h, s)) = tp.found
            && teleport::native() != Some((h, s))
        {
            teleport::set_native(h, s);
        }
    }
    if let Some(get) = lock(&GET).as_mut() {
        get.tick(Instant::now(), call, &mut log);
    }
    if let Some(pass) = lock(&PAIRS).iter_mut().find(|p| !p.done) {
        pass.tick(call, &mut log);
        if pass.done {
            for line in stats::adopt(pass) {
                log(line);
            }
            if let Some(line) = stats::restore(pass, call) {
                log(line);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modmenu::model::{NATIVE_GET_ENTITY_COORDS, NATIVE_PLAYER_PED_ID};
    use crate::modmenu::teleport::{pack, unpack};

    /// A world with one player and natives: 0xa does nothing, 0xb crawls a bit, 0xc warps a frame
    /// late, 0xd warps at once.
    struct World {
        pos: [f32; 3],
        pending: Option<[f32; 3]>,
        calls: Vec<u64>,
    }

    impl World {
        fn call(&mut self, h: u64, a: &[u64]) -> Option<[u64; 4]> {
            self.calls.push(h);
            match h {
                NATIVE_PLAYER_PED_ID => Some([5, 6, 0, 0]),
                NATIVE_GET_ENTITY_COORDS => {
                    assert_eq!(&a[..2], &[5, 6]);
                    let [x, y] = pack(self.pos);
                    Some([x, y, 0, 0])
                }
                0xa => Some([0; 4]),
                0xb => {
                    self.pos[0] += 0.3;
                    Some([0; 4])
                }
                0xc => {
                    self.pending = Some(unpack([a[2], a[3], 0, 0]));
                    Some([0; 4])
                }
                0xd => {
                    assert_eq!(a.len(), 7);
                    self.pos = unpack([a[2], a[3], 0, 0]);
                    Some([0; 4])
                }
                0x100.. if a.len() == 2 => Some([u64::from(200f32.to_bits()), 0, 0, 0]),
                0x100.. => Some([a[0] + h, 0, 0, 0]),
                _ => None,
            }
        }

        fn frame(&mut self) {
            if let Some(p) = self.pending.take() {
                self.pos = p;
            }
        }
    }

    fn run(list: &[(u64, u8)], skip: &[u64]) -> (TpFind, World, Vec<String>) {
        let mut w = World {
            pos: [100.0, 200.0, 10.0],
            pending: None,
            calls: Vec::new(),
        };
        let mut f = TpFind::new(list, skip);
        let mut lines = Vec::new();
        for _ in 0..200 {
            f.tick(&mut |h, a| w.call(h, a), &mut |s| lines.push(s));
            w.frame();
            if f.done {
                break;
            }
        }
        (f, w, lines)
    }

    #[test]
    fn find_teleport_names_the_first_native_that_lands_the_player_and_moves_back() {
        let (f, w, lines) = run(&[(0xa, 4), (0xb, 5), (0xd, 7), (0xc, 4)], &[]);
        assert_eq!(f.found, Some((0xd, 7)));
        assert!(f.done);
        assert!(lines[0].starts_with("teleport find try 1/4 0x000000000000000a slots=4"));
        assert!(lines[1].starts_with("teleport find no 0x000000000000000a: moved 0.00 m"));
        assert!(
            lines[3].contains("no 0x000000000000000b"),
            "a crawl is not a warp: {lines:?}"
        );
        assert!(lines[5].starts_with("teleport find found 0x000000000000000d slots=7 (at once)"));
        assert!(lines[6].starts_with("teleport find back at"), "{lines:?}");
        assert!(lines[6].contains("off by 0.00 m"), "{lines:?}");
        assert!(
            dist(w.pos, [100.3, 200.0, 10.0]) < 0.01,
            "back where the sweep found it"
        );
        assert!(!w.calls.contains(&0xc), "stops at the first");
        assert_eq!(f.status(), "found 0x000000000000000d");
    }

    #[test]
    fn a_warp_on_a_later_frame_is_found_after_the_wait_and_skips_are_left_out() {
        let (f, _, lines) = run(&[(0xd, 7), (0xc, 4)], &[0xd]);
        assert_eq!(f.found, Some((0xc, 4)));
        assert!(lines[0].contains("try 1/1 0x000000000000000c"));
        assert!(lines[1].contains("(after a wait)"), "{lines:?}");
        let (f, _, lines) = run(&[(0xa, 4)], &[]);
        assert_eq!((f.found, f.done), (None, true));
        assert_eq!(
            lines.last().unwrap(),
            "teleport find: none of 1 candidates moved the player"
        );
        assert_eq!(
            parse_skip("0xd, 4a65_60c6_18a1_1572,zz"),
            vec![0xd, 0x4a65_60c6_18a1_1572]
        );
    }

    #[test]
    fn the_getter_sweep_logs_every_value_then_only_what_changed() {
        let mut w = World {
            pos: [0.0; 3],
            pending: None,
            calls: Vec::new(),
        };
        let entity: Vec<u64> = (0x100..0x100 + 30).collect();
        let mut g = GetSweep::new(&entity, &[0x200]);
        let mut lines = Vec::new();
        let t0 = Instant::now();
        g.tick(t0, &mut |h, a| w.call(h, a), &mut |s| lines.push(s));
        assert_eq!(lines.len(), CHUNK, "a chunk per tick");
        assert_eq!(
            lines[0],
            "get sweep 1 0000000000000100 ped -> i32 1128792064 f32 200.000 raw 0x43480000"
        );
        g.tick(t0, &mut |h, a| w.call(h, a), &mut |s| lines.push(s));
        assert_eq!(
            lines.len(),
            30 + 2 + 1,
            "30 entity, the player getter twice, done"
        );
        assert!(lines[30].starts_with("get sweep 1 0000000000000200 player 0 -> i32 512"));
        assert!(lines[31].starts_with("get sweep 1 0000000000000200 player 255 -> i32 767"));
        assert!(lines[32].starts_with("get sweep 1 done: 32 natives"));
        lines.clear();
        g.tick(
            t0 + Duration::from_secs(10),
            &mut |h, a| w.call(h, a),
            &mut |s| lines.push(s),
        );
        assert!(lines.is_empty(), "waits for pass 2");
        for _ in 0..2 {
            g.tick(t0 + PASS_GAP, &mut |h, a| w.call(h, a), &mut |s| {
                lines.push(s)
            });
        }
        assert_eq!(lines, vec!["get sweep 2 done: 0 changed"]);
        assert!(g.done);
        assert_eq!(g.status(), "done: 0 changed between passes");
        assert_eq!(show_raw(1), "i32 1 f32 0.000 raw 0x1", "an int reads as a tiny float");
        assert!(show_raw(0x7f7f_ffff).contains("f32 3.4028235e38"), "no 39-digit floats");
    }

    #[test]
    fn shipped_candidates_fit_the_invoker() {
        assert!(candidates::TELEPORT.len() > 10);
        for &(h, s) in candidates::TELEPORT {
            assert!(
                (4..=crate::modmenu::natives::MAX_ARGS as u8).contains(&s),
                "{h:x}"
            );
        }
        let mut all: Vec<u64> = candidates::TELEPORT.iter().map(|c| c.0).collect();
        all.extend(candidates::ENTITY_GETTERS);
        all.extend(candidates::PLAYER_GETTERS);
        let n = all.len();
        all.sort_unstable();
        all.dedup();
        assert_eq!(all.len(), n, "each candidate once");
    }
}
