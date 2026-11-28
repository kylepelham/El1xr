//! (El1xr) Health, armour and wanted level: the getters pinned offline, the setters found in game
//! by the pair finder ([`PairFind`]), and the rows' scripts (refill, god mode, wanted clear and
//! never wanted).
//!
//! Pinned on 1.0.2.1 (see `AnyXboxX-coord/modmenu/STATE.md`): [`NATIVE_HEALTH`] resolves the
//! entity and reads its health through the title's health accessor (`+0x2ba4e0`: for peds
//! `(1 - damage) * max`, rounded so a living ped never reads 0); [`NATIVE_MAX_HEALTH`] reads the
//! other output of the same accessor (batch 5 part T read 130 / 130 for the player). The entity
//! health setters found offline skip peds, so the ped setter, armour and wanted are found by the pair finder: each candidate setter
//! is called with [`VALUE`] and the getters that read it back afterwards name the pair.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};

use super::candidates;
use super::queue::{Call, Queue};
use super::teleport::{Native, player};

/// `GET_ENTITY_HEALTH`-like: entity handle (slots 0-1) -> int.
pub const NATIVE_HEALTH: u64 = 0x158c_3f08_7dc9_7878;
/// `GET_ENTITY_MAX_HEALTH`-like: entity handle (slots 0-1) -> int.
pub const NATIVE_MAX_HEALTH: u64 = 0x92e8_f3c3_ea28_a89f;
/// Reads a level from the wanted manager by key (slot 0); a wanted getter candidate.
pub const NATIVE_WANTED_BY_KEY: u64 = 0xb649_eb46_5f68_c63d;
/// What the ped pass sets: below the caps (player health 130 in part T; armour 100 in GTA V) and unusual.
pub const VALUE: i32 = 73;
/// What the player pass sets: a wanted level (0..=6).
pub const WANTED_VALUE: i32 = 2;
/// Armour a refill sets.
pub const FULL_ARMOUR: i32 = 100;

/// A found setter: hash (0: none) and `slots | float << 8`.
struct Found(AtomicU64, AtomicU64);

impl Found {
    const fn new() -> Self {
        Self(AtomicU64::new(0), AtomicU64::new(0))
    }
    fn get(&self) -> Option<(u64, usize, bool)> {
        let h = self.0.load(Relaxed);
        let m = self.1.load(Relaxed);
        (h != 0).then_some((h, (m & 0xff) as usize, m >> 8 != 0))
    }
    fn set(&self, hash: u64, slots: usize, float: bool) {
        self.1.store(slots as u64 | u64::from(float) << 8, Relaxed);
        self.0.store(hash, Relaxed);
    }
}

static HEALTH_SET: Found = Found::new();
static ARMOUR_SET: Found = Found::new();
static ARMOUR_GET: AtomicU64 = AtomicU64::new(0);
static WANTED_SET: Found = Found::new();
static WANTED_GET: AtomicU64 = AtomicU64::new(0);
static GOD: AtomicBool = AtomicBool::new(false);
static NEVER_WANTED: AtomicBool = AtomicBool::new(false);

/// Argument slots for a setter: the target's slots, the value (as float bits when the setter
/// reads a float), zeros up to `slots`.
pub fn setter_args(target: &[u64], value: i32, slots: usize, float: bool) -> Vec<u64> {
    let mut args = target.to_vec();
    args.push(if float {
        u64::from((value as f32).to_bits())
    } else {
        u64::from(value as u32)
    });
    args.resize(slots.max(args.len()), 0);
    args
}

/// Whether a getter's result reads `value` (an int, or a float within 1).
pub fn reads(raw: u64, value: i32) -> bool {
    let i = raw as u32 as i32;
    let f = f32::from_bits(raw as u32);
    i == value || (f.is_finite() && (f - value as f32).abs() <= 1.0 && f != 0.0)
}

/// Which target a pair finder pass sets.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Target {
    Ped,
    Player,
}

/// The pair finder: one setter per game tick, every getter read before and after it.
pub struct PairFind {
    target: Target,
    setters: Vec<(u64, usize, bool)>,
    getters: Vec<u64>,
    value: i32,
    idx: usize,
    pub pairs: Vec<(u64, u64)>,
    pub done: bool,
}

impl PairFind {
    pub fn new(
        target: Target,
        setters: Vec<(u64, usize, bool)>,
        getters: Vec<u64>,
        value: i32,
    ) -> Self {
        Self {
            target,
            setters,
            getters,
            value,
            idx: 0,
            pairs: Vec::new(),
            done: false,
        }
    }

    pub fn ped() -> Self {
        let setters = candidates::ENTITY_SETTERS
            .iter()
            .map(|&(h, s, f)| (h, usize::from(s), f))
            .collect();
        Self::new(
            Target::Ped,
            setters,
            candidates::ENTITY_GETTERS.to_vec(),
            VALUE,
        )
    }

    pub fn player() -> Self {
        let setters = candidates::PLAYER_SETTERS
            .iter()
            .map(|&(h, s)| (h, usize::from(s), false))
            .collect();
        let mut getters = candidates::PLAYER_GETTERS.to_vec();
        getters.push(NATIVE_WANTED_BY_KEY);
        Self::new(Target::Player, setters, getters, WANTED_VALUE)
    }

    pub fn status(&self) -> String {
        if self.done {
            format!(
                "{} pairs from {} setters",
                self.pairs.len(),
                self.setters.len()
            )
        } else {
            format!("setter {}/{}", self.idx + 1, self.setters.len())
        }
    }

    fn read_all(&self, call: &mut Native, target: &[u64]) -> Vec<Option<u64>> {
        self.getters
            .iter()
            .map(|&g| call(g, target).map(|r| r[0]))
            .collect()
    }

    /// One game tick; `log` gets each line as it happens (a try before its call).
    pub fn tick(&mut self, call: &mut Native, log: &mut dyn FnMut(String)) {
        if self.done {
            return;
        }
        let Some((hash, slots, float)) = self.setters.get(self.idx).copied() else {
            log(format!(
                "pair find {:?} done: {} pairs",
                self.target,
                self.pairs.len()
            ));
            self.done = true;
            return;
        };
        let target: Vec<u64> = match self.target {
            Target::Ped => match player(call) {
                Some(p) => p.to_vec(),
                None => return,
            },
            Target::Player => vec![0],
        };
        let before = self.read_all(call, &target);
        log(format!(
            "pair find {:?} try {}/{} {hash:#018x} = {}",
            self.target,
            self.idx + 1,
            self.setters.len(),
            self.value
        ));
        call(hash, &setter_args(&target, self.value, slots, float));
        let after = self.read_all(call, &target);
        for ((&g, b), a) in self.getters.iter().zip(before).zip(after) {
            if let (Some(b), Some(a)) = (b, a)
                && a != b
                && reads(a, self.value)
            {
                log(format!(
                    "pair find {:?} found setter {hash:#018x} getter {g:#018x}: {b:#x} -> {a:#x}",
                    self.target
                ));
                self.pairs.push((hash, g));
            }
        }
        self.idx += 1;
    }

    /// The setter of the first pair whose getter passes `keep`.
    pub fn setter(&self, keep: impl Fn(u64, u64) -> bool) -> Option<(u64, usize, bool, u64)> {
        let &(s, g) = self.pairs.iter().find(|&&(s, g)| keep(s, g))?;
        let &(_, slots, float) = self.setters.iter().find(|c| c.0 == s)?;
        Some((s, slots, float, g))
    }
}

/// Takes what a finished pass found: the ped pass names health (its getter is [`NATIVE_HEALTH`])
/// and armour (the next pair with another getter and setter); the player pass names wanted.
/// Returns log lines.
pub fn adopt(f: &PairFind) -> Vec<String> {
    let mut lines = Vec::new();
    match f.target {
        Target::Ped => {
            let health = f.setter(|_, g| g == NATIVE_HEALTH);
            if let Some((s, n, fl, _)) = health {
                HEALTH_SET.set(s, n, fl);
                lines.push(format!("health setter {s:#018x} slots={n} float={fl}"));
            }
            let hs = health.map(|h| h.0);
            if let Some((s, n, fl, g)) = f.setter(|s, g| g != NATIVE_HEALTH && Some(s) != hs) {
                ARMOUR_SET.set(s, n, fl);
                ARMOUR_GET.store(g, Relaxed);
                lines.push(format!(
                    "armour setter {s:#018x} slots={n} getter {g:#018x} (unverified)"
                ));
            }
        }
        Target::Player => {
            if let Some((s, n, fl, g)) = f.setter(|_, _| true) {
                WANTED_SET.set(s, n, fl);
                WANTED_GET.store(g, Relaxed);
                lines.push(format!(
                    "wanted setter {s:#018x} slots={n} getter {g:#018x}"
                ));
            }
        }
    }
    lines
}

/// After a pass: puts back what its test value changed and the found setter can set (health to
/// its maximum, the wanted level to 0). Returns the log line.
pub fn restore(f: &PairFind, call: &mut Native) -> Option<String> {
    match f.target {
        Target::Ped => {
            let was = refill_script(call, &[])?[0] as u32 as i32;
            let ped = player(call)?;
            let now = call(NATIVE_HEALTH, &ped)?[0] as u32 as i32;
            Some(format!("health refilled after the pass: {was} -> {now}"))
        }
        Target::Player => {
            let now = clear_wanted_script(call, &[])?[0] as u32 as i32;
            Some(format!("wanted level cleared after the pass: reads {now}"))
        }
    }
}

/// Script: the player's health (`args` unused).
pub fn health_script(call: &mut Native, _: &[u64]) -> Option<[u64; 4]> {
    let ped = player(call)?;
    call(NATIVE_HEALTH, &ped)
}

pub fn max_health_script(call: &mut Native, _: &[u64]) -> Option<[u64; 4]> {
    let ped = player(call)?;
    call(NATIVE_MAX_HEALTH, &ped)
}

/// Script: sets the player's health to its maximum if it is below (refill, and god mode's tick).
pub fn refill_script(call: &mut Native, _: &[u64]) -> Option<[u64; 4]> {
    let (hash, slots, float) = HEALTH_SET.get()?;
    let ped = player(call)?;
    let max = call(NATIVE_MAX_HEALTH, &ped)?[0] as u32 as i32;
    let now = call(NATIVE_HEALTH, &ped)?[0] as u32 as i32;
    if max > 0 && now < max {
        call(hash, &setter_args(&ped, max, slots, float))?;
    }
    Some([now as u32 as u64, 0, 0, 0])
}

pub fn armour_script(call: &mut Native, _: &[u64]) -> Option<[u64; 4]> {
    let (hash, slots, float) = ARMOUR_SET.get()?;
    let ped = player(call)?;
    call(hash, &setter_args(&ped, FULL_ARMOUR, slots, float))
}

/// Script: wanted level 0 through the found setter, then its getter.
pub fn clear_wanted_script(call: &mut Native, _: &[u64]) -> Option<[u64; 4]> {
    let (hash, slots, float) = WANTED_SET.get()?;
    call(hash, &setter_args(&[0], 0, slots, float))?;
    match WANTED_GET.load(Relaxed) {
        0 => Some([0; 4]),
        g => call(g, &[0]),
    }
}

fn push(queue: &Queue, name: &'static str, script: super::queue::Script) -> bool {
    queue.push(Call {
        name,
        hash: super::model::NATIVE_PLAYER_PED_ID,
        args: Vec::new(),
        script: Some(script),
    })
}

pub fn refill_set(_: &str) {
    push(&super::QUEUE, "refill", refill_script);
}
pub fn armour_set(_: &str) {
    push(&super::QUEUE, "armour", armour_script);
}
pub fn clear_wanted_set(_: &str) {
    push(&super::QUEUE, "clear wanted", clear_wanted_script);
}

fn found_text(f: &Found, what: &str) -> String {
    match f.get() {
        Some((h, _, _)) => format!("{what} via {h:#018x}"),
        None => "not found yet: run Find stats".into(),
    }
}
pub fn refill_get() -> String {
    found_text(&HEALTH_SET, "refill")
}
pub fn armour_get() -> String {
    found_text(&ARMOUR_SET, "armour 100")
}
pub fn clear_wanted_get() -> String {
    found_text(&WANTED_SET, "level 0")
}

fn on_off(b: &AtomicBool) -> String {
    if b.load(Relaxed) { "on" } else { "off" }.into()
}
pub fn god_get() -> String {
    on_off(&GOD)
}
pub fn god_set(v: &str) {
    GOD.store(v == "on", Relaxed);
}
pub fn never_wanted_get() -> String {
    on_off(&NEVER_WANTED)
}
pub fn never_wanted_set(v: &str) {
    NEVER_WANTED.store(v == "on", Relaxed);
}

/// Game ticks between the holds' calls.
pub const HOLD_EVERY: usize = 10;

/// The holds, on the game thread every [`HOLD_EVERY`] ticks: god mode refills health when it is
/// below the maximum, never wanted sets the wanted level to 0.
pub fn hold(tick: usize, queue: &Queue) {
    if tick % HOLD_EVERY != 0 {
        return;
    }
    if GOD.load(Relaxed) && HEALTH_SET.get().is_some() {
        push(queue, "god", refill_script);
    }
    if NEVER_WANTED.load(Relaxed) && WANTED_SET.get().is_some() {
        push(queue, "never wanted", clear_wanted_script);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modmenu::model::NATIVE_PLAYER_PED_ID;

    /// A ped with health (setter 0x51 takes an int, getter is the real hash), armour (setter 0x52
    /// takes a float, getter 0x62), and a setter 0x53 that changes nothing.
    struct Ped {
        health: i32,
        armour: f32,
    }

    impl Ped {
        fn call(&mut self, h: u64, a: &[u64]) -> Option<[u64; 4]> {
            let me = [7u64, 8];
            match h {
                NATIVE_PLAYER_PED_ID => Some([me[0], me[1], 0, 0]),
                NATIVE_HEALTH => Some([self.health as u32 as u64, 0, 0, 0]),
                NATIVE_MAX_HEALTH => Some([1000, 0, 0, 0]),
                0x62 => Some([u64::from(self.armour.to_bits()), 0, 0, 0]),
                0x51 => {
                    assert_eq!(&a[..2], &me);
                    self.health = a[2] as u32 as i32;
                    Some([0; 4])
                }
                0x52 => {
                    self.armour = f32::from_bits(a[2] as u32);
                    Some([0; 4])
                }
                0x53 => Some([0; 4]),
                _ => None,
            }
        }
    }

    #[test]
    fn the_pair_finder_names_health_and_armour_by_reading_the_value_back() {
        let mut p = Ped {
            health: 1000,
            armour: 0.0,
        };
        let mut f = PairFind::new(
            Target::Ped,
            vec![(0x53, 3, false), (0x52, 3, true), (0x51, 4, false)],
            vec![NATIVE_HEALTH, 0x62],
            VALUE,
        );
        let mut lines = Vec::new();
        for _ in 0..5 {
            f.tick(&mut |h, a| p.call(h, a), &mut |s| lines.push(s));
        }
        assert!(f.done);
        assert_eq!(f.pairs, vec![(0x52, 0x62), (0x51, NATIVE_HEALTH)]);
        assert!(lines[0].starts_with("pair find Ped try 1/3 0x0000000000000053 = 73"));
        assert!(
            lines[2].contains("found setter 0x0000000000000052 getter 0x0000000000000062"),
            "{lines:?}"
        );
        assert_eq!(lines.last().unwrap(), "pair find Ped done: 2 pairs");
        let adopted = adopt(&f);
        assert_eq!(adopted.len(), 2, "{adopted:?}");
        assert!(adopted[0].starts_with("health setter 0x0000000000000051 slots=4"));
        assert!(adopted[1].starts_with("armour setter 0x0000000000000052"));
        // Refill and god mode go through the found setter, up to the maximum.
        assert_eq!(p.health, 73);
        assert_eq!(
            restore(&f, &mut |h, a| p.call(h, a)).as_deref(),
            Some("health refilled after the pass: 73 -> 1000")
        );
        p.health = 500;
        assert!(refill_script(&mut |h, a| p.call(h, a), &[]).is_some());
        assert_eq!(p.health, 1000);
        assert!(armour_script(&mut |h, a| p.call(h, a), &[]).is_some());
        assert_eq!(p.armour, 100.0, "a float setter gets float bits");
        let q = Queue::default();
        god_set("on");
        hold(HOLD_EVERY, &q);
        hold(HOLD_EVERY + 1, &q);
        god_set("off");
        let mut names = Vec::new();
        q.drain(|c| {
            names.push(c.name);
            None
        });
        assert_eq!(names, vec!["god"], "every HOLD_EVERY ticks");
    }

    #[test]
    fn setter_arguments_and_read_backs() {
        assert_eq!(setter_args(&[1, 2], 73, 4, false), vec![1, 2, 73, 0]);
        assert_eq!(setter_args(&[0], -1, 2, false), vec![0, 0xffff_ffff]);
        assert_eq!(
            setter_args(&[1, 2], 73, 3, true),
            vec![1, 2, u64::from(73f32.to_bits())]
        );
        assert!(reads(73, 73));
        assert!(reads(u64::from(72.6f32.to_bits()), 73));
        assert!(!reads(0, 73));
        assert!(!reads(74 << 32, 73), "the low 32 bits only");
        let p = PairFind::player();
        assert_eq!(
            p.setters[0].0, 0x99f1_dcca_3222_b48b,
            "the wanted setters first"
        );
        assert!(p.getters.contains(&NATIVE_WANTED_BY_KEY));
        assert!(candidates::ENTITY_GETTERS.contains(&NATIVE_HEALTH));
        for &(_, s, _) in candidates::ENTITY_SETTERS {
            assert!((3..=4).contains(&s));
        }
    }
}
