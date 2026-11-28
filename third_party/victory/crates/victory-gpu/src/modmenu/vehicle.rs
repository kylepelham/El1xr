//! (El1xr) Vehicle spawn: Find vehicle ([`VehicleFind`]) names the request, loaded and create
//! natives in game, and the Spawn row ([`spawn_set`]) uses them.
//!
//! Offline (1.0.2.1) the candidates are the natives that reach the title's model lookup by name
//! hash (`+0x4f1f810`, a hash map of 16-byte entries): [`REQUEST`] take a hash and return nothing
//! (`REQUEST_MODEL`-like), [`LOADED`] take a hash and return one slot (`HAS_MODEL_LOADED`-like),
//! [`CREATE`] take a hash and a position and return an entity handle. No vehicle model names are
//! in the title, so the models tried are guesses ([`MODELS`], GTA V names) plus any named by
//! `VICTORY_MOD_MENU_VEHICLE_MODELS=<name or hash hex>,...`, which come first.
//!
//! Per model: each request candidate is called and the loaded candidates are read for up to
//! [`LOAD_TICKS`]; the first that flips one from 0 to 1 names both. Then each create candidate is
//! called with the model and a spot [`AHEAD`] from the player; it is the one when the handle it
//! returns reads back (through the confirmed coords native) within [`NEAR`] m of the spot. A model
//! that loads through no request is still tried with the create candidates (it may be resident).

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};

use super::teleport::{Native, coords, dist, pack, player, show};
use super::weather::joaat;

pub const REQUEST: &[(u64, u8)] = &[
    (0xddea_d184_3b42_ba2c, 2),
    (0xfd5d_0406_e0a8_1c8a, 2),
    (0xb075_52fe_99d2_dbbd, 2),
    (0xd914_996c_7138_7ff6, 2),
    (0xe630_4994_2b4f_0b55, 2),
    (0xc38b_72a5_4503_65b5, 2),
    (0xaff5_9058_e307_8cb7, 2),
];

pub const LOADED: &[u64] = &[
    0x5336_7505_dc19_7c72,
    0x8f09_3a0f_1346_1ec2,
    0x8d0c_84ee_e94b_8c82,
    0x17fa_81cf_6b91_7402,
    0x0915_253d_05ec_4541,
    0x729b_f23d_f0f3_5d0d,
    0x9a14_672a_29c1_b934,
];

/// (hash, argument slots): the model in slot 0, the position in slots 1-2, zeros after.
pub const CREATE: &[(u64, u8)] = &[
    (0x4549_74ad_8f84_0a43, 3),
    (0xc22a_05a7_045f_73af, 12),
    (0x81f5_6748_76e7_fb8d, 14),
];

pub const MODELS: &[&str] = &[
    "sultan",
    "blista",
    "asea",
    "faggio",
    "bmx",
    "police",
    "taxi",
    "issi2",
    "dominator",
    "infernus",
    "banshee",
    "buffalo",
    "washington",
    "emperor",
    "rebel",
    "sanchez",
    "tornado",
    "adder",
];

pub const LOAD_TICKS: u32 = 60;
pub const AHEAD: [f32; 3] = [4.0, 0.0, 0.5];
pub const NEAR: f32 = 5.0;

/// A name or a hash in hex (`0x...`).
pub fn model_hash(v: &str) -> Option<u32> {
    let v = v.trim();
    if v.is_empty() {
        return None;
    }
    match v.strip_prefix("0x") {
        Some(h) => u32::from_str_radix(&h.replace('_', ""), 16).ok(),
        None => Some(joaat(v)),
    }
}

/// The models to try: `VICTORY_MOD_MENU_VEHICLE_MODELS` first, then [`MODELS`].
pub fn models(env: Option<&str>) -> Vec<u32> {
    let mut out: Vec<u32> = env
        .unwrap_or("")
        .split(',')
        .filter_map(model_hash)
        .collect();
    for m in MODELS {
        let h = joaat(m);
        if !out.contains(&h) {
            out.push(h);
        }
    }
    out
}

/// What Find vehicle found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Found {
    pub create: u64,
    pub slots: usize,
    pub model: u32,
    /// 0: none needed or none found.
    pub request: u64,
    pub loaded: u64,
}

static FOUND: Mutex<Option<Found>> = Mutex::new(None);
static LAST: Mutex<String> = Mutex::new(String::new());

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub fn found() -> Option<Found> {
    *lock(&FOUND)
}

pub fn set_found(f: Found) {
    *lock(&FOUND) = Some(f);
}

fn ahead(at: [f32; 3]) -> [f32; 3] {
    [at[0] + AHEAD[0], at[1] + AHEAD[1], at[2] + AHEAD[2]]
}

fn create_args(model: u32, slots: usize, at: [f32; 3]) -> Vec<u64> {
    let mut a = vec![0u64; slots.max(3)];
    a[0] = u64::from(model);
    a[1..3].copy_from_slice(&pack(at));
    a
}

/// Calls `create` and reads the new entity back: Some((handle, position)) when it stands within
/// [`NEAR`] m of `at`.
fn try_create(
    call: &mut Native,
    create: u64,
    slots: usize,
    model: u32,
    at: [f32; 3],
) -> Result<([u64; 2], [f32; 3]), String> {
    let Some([lo, hi, ..]) = call(create, &create_args(model, slots, at)) else {
        return Err("no call (faulted or not in this build)".into());
    };
    if lo | hi == 0 {
        return Err("no entity".into());
    }
    let Some(pos) = coords(call, [lo, hi]) else {
        return Err("no position".into());
    };
    if dist(pos, at) <= NEAR {
        Ok(([lo, hi], pos))
    } else {
        Err(format!(
            "entity at {} ({:.1} m off)",
            show(pos),
            dist(pos, at)
        ))
    }
}

fn loaded_bits(call: &mut Native, model: u32) -> Vec<bool> {
    LOADED
        .iter()
        .map(|&l| call(l, &[u64::from(model)]).is_some_and(|r| r[0] & 0xff == 1))
        .collect()
}

enum Phase {
    Request {
        r: usize,
        wait: u32,
        before: Vec<bool>,
    },
    Create {
        c: usize,
    },
}

/// Find vehicle's state, one step per game tick.
pub struct VehicleFind {
    models: Vec<u32>,
    m: usize,
    phase: Phase,
    request: u64,
    loaded: u64,
    pub found: Option<Found>,
    pub done: bool,
}

impl VehicleFind {
    pub fn new(models: Vec<u32>) -> Self {
        Self {
            models,
            m: 0,
            phase: Phase::Request {
                r: 0,
                wait: 0,
                before: Vec::new(),
            },
            request: 0,
            loaded: 0,
            found: None,
            done: false,
        }
    }

    pub fn status(&self) -> String {
        match (self.found, self.done) {
            (Some(f), _) => format!("found {:#018x} with model {:#010x}", f.create, f.model),
            (None, true) => format!("none of {} models spawned", self.models.len()),
            (None, false) => format!("model {}/{}", self.m + 1, self.models.len()),
        }
    }

    fn next_model(&mut self) {
        self.m += 1;
        self.request = 0;
        self.loaded = 0;
        self.phase = Phase::Request {
            r: 0,
            wait: 0,
            before: Vec::new(),
        };
    }

    pub fn tick(&mut self, call: &mut Native, log: &mut dyn FnMut(String)) {
        if self.done {
            return;
        }
        let Some(&model) = self.models.get(self.m) else {
            log(format!(
                "vehicle find: none of {} models spawned",
                self.models.len()
            ));
            self.done = true;
            return;
        };
        match &mut self.phase {
            Phase::Request { r, wait, before } => {
                let Some(&(req, slots)) = REQUEST.get(*r) else {
                    self.phase = Phase::Create { c: 0 };
                    return;
                };
                if *wait == 0 {
                    *before = loaded_bits(call, model);
                    log(format!(
                        "vehicle find model {:#010x} request try {}/{} {req:#018x}",
                        model,
                        *r + 1,
                        REQUEST.len()
                    ));
                    let mut a = vec![0u64; usize::from(slots)];
                    a[0] = u64::from(model);
                    if call(req, &a).is_none() {
                        log(format!("vehicle find skip {req:#018x}: no call"));
                        *r += 1;
                        return;
                    }
                }
                *wait += 1;
                let now = loaded_bits(call, model);
                let flipped = before.iter().zip(&now).position(|(b, n)| !*b && *n);
                if let Some(i) = flipped {
                    self.request = req;
                    self.loaded = LOADED[i];
                    log(format!(
                        "vehicle find request {req:#018x} loaded {:#018x} after {} ticks",
                        LOADED[i], wait
                    ));
                    self.phase = Phase::Create { c: 0 };
                } else if *wait >= LOAD_TICKS {
                    *r += 1;
                    *wait = 0;
                }
            }
            Phase::Create { c } => {
                let Some(&(create, slots)) = CREATE.get(*c) else {
                    log(format!(
                        "vehicle find model {model:#010x}: no create native spawned it"
                    ));
                    self.next_model();
                    return;
                };
                let Some(at) = player(call).and_then(|p| coords(call, p)).map(ahead) else {
                    return;
                };
                log(format!(
                    "vehicle find model {model:#010x} create try {}/{} {create:#018x} at {}",
                    *c + 1,
                    CREATE.len(),
                    show(at)
                ));
                match try_create(call, create, usize::from(slots), model, at) {
                    Ok((h, pos)) => {
                        let f = Found {
                            create,
                            slots: usize::from(slots),
                            model,
                            request: self.request,
                            loaded: self.loaded,
                        };
                        log(format!(
                            "vehicle find found create {create:#018x} model {model:#010x} \
                             request {:#018x} loaded {:#018x}: entity {:016x}{:016x} at {}",
                            f.request,
                            f.loaded,
                            h[1],
                            h[0],
                            show(pos)
                        ));
                        self.found = Some(f);
                        self.done = true;
                    }
                    Err(e) => {
                        log(format!("vehicle find no {create:#018x}: {e}"));
                        *c += 1;
                    }
                }
            }
        }
    }
}

/// The Spawn row's job: request, wait for loaded, create ahead of the player.
struct Spawn {
    wait: u32,
}

static SPAWN: Mutex<Option<Spawn>> = Mutex::new(None);
static SPAWNING: AtomicBool = AtomicBool::new(false);

pub const SPAWN_CHOICES: &[&str] = &["spawn"];

pub fn spawn_set(_: &str) {
    if found().is_none() {
        *lock(&LAST) = "vehicle spawn not found yet: run Find vehicle".into();
        return;
    }
    *lock(&SPAWN) = Some(Spawn { wait: 0 });
    SPAWNING.store(true, Relaxed);
}

pub fn spawn_get() -> String {
    let last = lock(&LAST);
    if !last.is_empty() {
        return last.clone();
    }
    match found() {
        Some(f) => format!("model {:#010x} via {:#018x}", f.model, f.create),
        None => "not found yet: run Find vehicle".into(),
    }
}

/// The game-thread step of the Spawn row; one atomic load while idle.
pub fn spawn_tick(call: &mut Native) {
    if !SPAWNING.load(Relaxed) {
        return;
    }
    let mut job = lock(&SPAWN);
    let (Some(s), Some(f)) = (job.as_mut(), found()) else {
        SPAWNING.store(false, Relaxed);
        return;
    };
    let model = u64::from(f.model);
    if s.wait == 0 && f.request != 0 {
        call(f.request, &[model, 0]);
    }
    s.wait += 1;
    let ready = f.loaded == 0 || call(f.loaded, &[model]).is_some_and(|r| r[0] & 0xff == 1);
    if !ready && s.wait < LOAD_TICKS {
        return;
    }
    let line = match player(call).and_then(|p| coords(call, p)).map(ahead) {
        Some(at) => match try_create(call, f.create, f.slots, f.model, at) {
            Ok((h, pos)) => format!(
                "vehicle spawned {:016x}{:016x} at {} ({:.1} m from the spot)",
                h[1],
                h[0],
                show(pos),
                dist(pos, at)
            ),
            Err(e) => format!("vehicle spawn failed: {e}"),
        },
        None => "vehicle spawn: no player".into(),
    };
    tracing::info!("mod menu: {line}");
    *lock(&LAST) = line;
    *job = None;
    SPAWNING.store(false, Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modmenu::model::{NATIVE_GET_ENTITY_COORDS, NATIVE_PLAYER_PED_ID};
    use crate::modmenu::teleport::unpack;

    /// One model (`good`) streams in a few ticks after request `REQUEST[2]` and spawns through
    /// `CREATE[1]`; `CREATE[0]` returns an entity far away.
    struct World {
        good: u32,
        requested_at: Option<u32>,
        tick: u32,
        loaded: bool,
        spawned: Vec<[f32; 3]>,
    }

    impl World {
        fn call(&mut self, h: u64, a: &[u64]) -> Option<[u64; 4]> {
            match h {
                NATIVE_PLAYER_PED_ID => Some([1, 2, 0, 0]),
                NATIVE_GET_ENTITY_COORDS => {
                    let at = match a[0] {
                        1 => [10.0, 20.0, 1.0],
                        9 => [500.0, 0.0, 0.0],
                        i => self.spawned[(i - 100) as usize],
                    };
                    let [x, y] = pack(at);
                    Some([x, y, 0, 0])
                }
                _ if h == REQUEST[2].0 => {
                    if a[0] as u32 == self.good {
                        self.requested_at = Some(self.tick);
                    }
                    Some([0; 4])
                }
                _ if h == LOADED[4] => {
                    Some([u64::from(self.loaded && a[0] as u32 == self.good), 0, 0, 0])
                }
                _ if h == CREATE[0].0 => Some([9, 9, 0, 0]),
                _ if h == CREATE[1].0 => {
                    if !self.loaded || a[0] as u32 != self.good {
                        return Some([0; 4]);
                    }
                    assert_eq!(a.len(), 12);
                    self.spawned.push(unpack([a[1], a[2], 0, 0]));
                    Some([99 + self.spawned.len() as u64, 7, 0, 0])
                }
                _ if REQUEST.iter().any(|r| r.0 == h) || LOADED.contains(&h) => Some([0; 4]),
                _ => None,
            }
        }

        fn frame(&mut self) {
            self.tick += 1;
            if self.requested_at.is_some_and(|t| self.tick >= t + 3) {
                self.loaded = true;
            }
        }
    }

    #[test]
    fn find_vehicle_names_request_loaded_and_create_by_reading_the_entity_back() {
        let good = joaat("sultan");
        let mut w = World {
            good,
            requested_at: None,
            tick: 0,
            loaded: false,
            spawned: Vec::new(),
        };
        let mut f = VehicleFind::new(vec![0x1111, good]);
        let mut lines = Vec::new();
        for _ in 0..2000 {
            f.tick(&mut |h, a| w.call(h, a), &mut |s| lines.push(s));
            w.frame();
            if f.done {
                break;
            }
        }
        let found = f.found.unwrap_or_else(|| panic!("{lines:?}"));
        assert_eq!(
            found,
            Found {
                create: CREATE[1].0,
                slots: 12,
                model: good,
                request: REQUEST[2].0,
                loaded: LOADED[4],
            }
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("model 0x00001111: no create native spawned it"))
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("entity at 500.0 0.0 0.0 (")),
            "far is not it"
        );
        assert!(dist(w.spawned[0], [14.0, 20.0, 1.5]) < 0.01);
        // The Spawn row then goes through what was found.
        set_found(found);
        w.loaded = false;
        w.requested_at = None;
        spawn_set("spawn");
        for _ in 0..20 {
            spawn_tick(&mut |h, a| w.call(h, a));
            w.frame();
        }
        assert_eq!(w.spawned.len(), 2, "{}", spawn_get());
        assert!(
            spawn_get().starts_with("vehicle spawned"),
            "{}",
            spawn_get()
        );
    }

    #[test]
    fn models_come_from_the_setting_first() {
        let m = models(Some("0xDEAD_BEEF, sultan ,"));
        assert_eq!(m[0], 0xdead_beef);
        assert_eq!(m[1], joaat("sultan"));
        assert_eq!(m.iter().filter(|&&h| h == joaat("sultan")).count(), 1);
        assert_eq!(models(None).len(), MODELS.len());
        assert_eq!(model_hash("0xzz"), None);
    }
}
