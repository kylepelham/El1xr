//! (El1xr) Weather: the getters and setters pinned offline, the type table read from the title,
//! and Find weather ([`WeatherFind`]), which proves a setter in game by reading the weather back.
//!
//! Pinned on 1.0.2.1: the title keeps its weather types in a table of 0x38-byte entries, each
//! starting with the type's name hash ([`TYPES_RVA`] points at it, [`COUNT_RVA`] holds the count;
//! `+0x1cd5210` maps a hash to its index and returns -1 for an unknown one). The previous and next
//! type indices sit at `+0xab46190` / `+0xab46194`; [`NATIVE_WEATHER_PREV`] and
//! [`NATIVE_WEATHER_NEXT`] return those types' hashes. The setters ([`SETTERS`]) look the hash up
//! and index the table with the result, so an unknown hash reads out of bounds: the trainer only
//! ever passes a hash that is in the table it read.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};

use super::queue::{Call, Queue};
use super::teleport::Native;

/// `GET_NEXT_WEATHER_TYPE_HASH_NAME`-like: () -> the next type's hash.
pub const NATIVE_WEATHER_NEXT: u64 = 0x09d5_557c_aefd_e2c9;
/// `GET_PREV_WEATHER_TYPE_HASH_NAME`-like.
pub const NATIVE_WEATHER_PREV: u64 = 0x4358_cd16_b414_bd51;
/// The weather type count (u32) and the pointer to the type table, image offsets.
pub const COUNT_RVA: usize = 0xab4_5e80;
pub const TYPES_RVA: usize = 0xab4_5eb8;
pub const TYPE_SIZE: usize = 0x38;
/// More types than this means the table was not read right.
pub const MAX_TYPES: u32 = 128;

/// How a setter takes its arguments.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Layout {
    /// (previous hash, next hash, mix, flag): sets both types at once.
    Pair,
    /// (hash, now, persist, over time, seconds, flag, flag): the combined setter.
    Now,
    /// (hash, seconds, seconds): a transition.
    Over,
}

/// Setter candidates, most likely first.
pub const SETTERS: &[(u64, Layout)] = &[
    (0x5e21_92a2_05c1_c357, Layout::Pair),
    (0x01f6_a4a5_a6e8_b0e5, Layout::Now),
    (0xb271_e5bc_174b_ba1f, Layout::Over),
];

pub fn args(layout: Layout, hash: u32) -> Vec<u64> {
    let h = u64::from(hash);
    match layout {
        Layout::Pair => vec![h, h, 0, 0],
        Layout::Now => vec![h, 1, 1, 0, 0, 0, 0],
        Layout::Over => vec![h, 0, 0],
    }
}

/// The name hash the title uses (Jenkins one-at-a-time over the lower-case name).
pub const fn joaat(name: &str) -> u32 {
    let b = name.as_bytes();
    let mut h: u32 = 0;
    let mut i = 0;
    while i < b.len() {
        h = h.wrapping_add(b[i].to_ascii_lowercase() as u32);
        h = h.wrapping_add(h << 10);
        h ^= h >> 6;
        i += 1;
    }
    h = h.wrapping_add(h << 3);
    h ^= h >> 11;
    h.wrapping_add(h << 15)
}

/// Names tried for the title's types (GTA V's and likely new ones); the table names which exist.
pub const NAMES: &[&str] = &[
    "EXTRASUNNY",
    "CLEAR",
    "CLOUDS",
    "OVERCAST",
    "RAIN",
    "THUNDER",
    "CLEARING",
    "SMOG",
    "FOGGY",
    "NEUTRAL",
    "SNOW",
    "SNOWLIGHT",
    "BLIZZARD",
    "XMAS",
    "HALLOWEEN",
    "SUNNY",
    "CLOUDY",
    "STORM",
    "HURRICANE",
    "FOG",
    "DRIZZLE",
    "HEATWAVE",
    "HAZE",
    "MIST",
];

/// The Weather row's values: the next type in the table, or a name.
pub const CHOICES: &[&str] = &[
    "next",
    "EXTRASUNNY",
    "CLEAR",
    "CLOUDS",
    "OVERCAST",
    "RAIN",
    "THUNDER",
    "CLEARING",
    "SMOG",
    "FOGGY",
    "NEUTRAL",
    "SNOW",
    "BLIZZARD",
    "SUNNY",
    "STORM",
    "FOG",
];

/// A type hash as a name when one of [`NAMES`] has it.
pub fn name(hash: u32) -> String {
    NAMES
        .iter()
        .find(|n| joaat(n) == hash)
        .map_or_else(|| format!("{hash:#010x}"), |n| (*n).to_string())
}

pub fn show([v, ..]: [u64; 4]) -> String {
    name(v as u32)
}

static TYPES: Mutex<Vec<u32>> = Mutex::new(Vec::new());
static WANTED: AtomicBool = AtomicBool::new(false);
static FOUND: AtomicU64 = AtomicU64::new(0);
static LAST: Mutex<String> = Mutex::new(String::new());

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The types read last.
pub fn types() -> Vec<u32> {
    lock(&TYPES).clone()
}

/// Whether anything asked for the types (the game thread reads them only then).
pub fn wanted() -> bool {
    WANTED.load(Relaxed)
}

pub fn want() {
    WANTED.store(true, Relaxed);
}

/// Whether `len` bytes at `at` are committed and readable.
fn readable(at: usize, len: usize) -> bool {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_GUARD, PAGE_NOACCESS, VirtualQuery,
    };
    let mut at = at;
    let end = at + len;
    while at < end {
        // SAFETY: VirtualQuery only fills `info`.
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        let n = unsafe {
            VirtualQuery(
                at as *const _,
                &mut info,
                std::mem::size_of::<MEMORY_BASIC_INFORMATION>(),
            )
        };
        if n == 0
            || info.State != MEM_COMMIT
            || info.Protect & (PAGE_NOACCESS | PAGE_GUARD) != 0
            || info.Protect == 0
        {
            return false;
        }
        at = info.BaseAddress as usize + info.RegionSize;
    }
    true
}

/// Reads the type table of the image mapped at `base`, checking each read.
///
/// # Safety
/// `base` is the mapped title image.
pub unsafe fn read_table(base: usize) -> Option<Vec<u32>> {
    let count_at = base + COUNT_RVA;
    let ptr_at = base + TYPES_RVA;
    if !readable(count_at, 4) || !readable(ptr_at, 8) {
        return None;
    }
    // SAFETY: checked readable above.
    let (count, table) = unsafe {
        (
            (count_at as *const u32).read_volatile(),
            (ptr_at as *const usize).read_volatile(),
        )
    };
    if count == 0 || count > MAX_TYPES || table == 0 {
        return None;
    }
    let len = count as usize * TYPE_SIZE;
    if !readable(table, len) {
        return None;
    }
    // SAFETY: checked readable above.
    Some(
        (0..count as usize)
            .map(|i| unsafe { ((table + i * TYPE_SIZE) as *const u32).read_volatile() })
            .collect(),
    )
}

/// Reads the table again (the game thread, while [`wanted`]); logs it the first time it changes.
pub fn refresh(read: impl FnOnce() -> Option<Vec<u32>>) {
    let Some(t) = read() else { return };
    let mut types = lock(&TYPES);
    if *types != t {
        let named: Vec<String> = t.iter().map(|&h| name(h)).collect();
        tracing::info!("mod menu: weather types {}: {}", t.len(), named.join(" "));
        *types = t;
    }
}

/// The weather setter, if found: (hash, layout).
pub fn found() -> Option<(u64, Layout)> {
    let h = FOUND.load(Relaxed);
    SETTERS.iter().find(|s| s.0 == h).copied()
}

/// Takes `hash` as the setter if it is one of [`SETTERS`].
pub fn set_found(hash: u64) -> bool {
    let known = SETTERS.iter().any(|s| s.0 == hash);
    if known {
        FOUND.store(hash, Relaxed);
    }
    known
}

fn note(s: String) {
    tracing::info!("mod menu: {s}");
    *lock(&LAST) = s;
}

/// The type a value names: `next` is the one after the current type in the table.
pub fn pick(value: &str, types: &[u32], current: u32) -> Result<u32, String> {
    if types.is_empty() {
        return Err("weather types not read yet".into());
    }
    let hash = if value == "next" {
        let at = types
            .iter()
            .position(|&t| t == current)
            .map_or(0, |i| i + 1);
        types[at % types.len()]
    } else {
        joaat(value)
    };
    if types.contains(&hash) {
        Ok(hash)
    } else {
        Err(format!("{value} is not a weather type in this build"))
    }
}

/// Script: sets the weather named by `args[0]` (an index into [`CHOICES`]).
pub fn set_script(call: &mut Native, a: &[u64]) -> Option<[u64; 4]> {
    let value = CHOICES.get(*a.first()? as usize)?;
    let Some((setter, layout)) = found() else {
        note("weather setter not found yet: run Find weather".into());
        return None;
    };
    let current = call(NATIVE_WEATHER_NEXT, &[])?[0] as u32;
    let hash = match pick(value, &types(), current) {
        Ok(h) => h,
        Err(e) => {
            note(e);
            return None;
        }
    };
    call(setter, &args(layout, hash))?;
    let now = call(NATIVE_WEATHER_NEXT, &[])?[0] as u32;
    note(format!(
        "weather {} -> {} via {setter:#018x}: reads {}",
        name(current),
        name(hash),
        name(now)
    ));
    Some([u64::from(now), 0, 0, 0])
}

pub fn queue_set(queue: &Queue, value: &str) -> bool {
    let Some(i) = CHOICES.iter().position(|c| *c == value) else {
        return false;
    };
    want();
    queue.push(Call {
        name: "weather",
        hash: NATIVE_WEATHER_NEXT,
        args: vec![i as u64],
        script: Some(set_script),
    })
}

pub fn row_set(value: &str) {
    queue_set(&super::QUEUE, value);
}

pub fn row_get() -> String {
    let last = lock(&LAST);
    if !last.is_empty() {
        return last.clone();
    }
    match found() {
        Some((h, _)) => format!("via {h:#018x}"),
        None => "not found yet: run Find weather".into(),
    }
}

/// Ticks to wait before reading a setter's result again (a change applied on a later frame).
pub const WAIT_TICKS: u32 = 5;

/// Find weather: one setter per try, read back through the next and previous getters, then the
/// weather put back.
pub struct WeatherFind {
    idx: usize,
    start: Option<u32>,
    target: u32,
    wait: Option<u32>,
    waited_for_types: u32,
    pub found: Option<(u64, Layout)>,
    pub done: bool,
}

impl Default for WeatherFind {
    fn default() -> Self {
        Self::new()
    }
}

impl WeatherFind {
    pub fn new() -> Self {
        want();
        Self {
            idx: 0,
            start: None,
            target: 0,
            wait: None,
            waited_for_types: 0,
            found: None,
            done: false,
        }
    }

    pub fn status(&self) -> String {
        match (self.found, self.done) {
            (Some((h, _)), _) => format!("found {h:#018x}"),
            (None, true) => "no setter moved the weather".into(),
            (None, false) => format!("setter {}/{}", self.idx + 1, SETTERS.len()),
        }
    }

    fn reads(call: &mut Native) -> Option<(u32, u32)> {
        let next = call(NATIVE_WEATHER_NEXT, &[])?[0] as u32;
        let prev = call(NATIVE_WEATHER_PREV, &[])?[0] as u32;
        Some((next, prev))
    }

    fn finish(&mut self, call: &mut Native, log: &mut dyn FnMut(String), how: &str) {
        let (hash, layout) = SETTERS[self.idx];
        log(format!(
            "weather find found {hash:#018x} {layout:?} ({how}): {} -> {}",
            name(self.start.unwrap_or(0)),
            name(self.target)
        ));
        self.found = Some((hash, layout));
        if let Some(start) = self.start {
            call(hash, &args(layout, start));
            let back = Self::reads(call).map_or(0, |r| r.0);
            log(format!(
                "weather find put back {}: reads {}",
                name(start),
                name(back)
            ));
        }
        self.done = true;
    }

    /// One game tick with the types read so far.
    pub fn tick(&mut self, call: &mut Native, types: &[u32], log: &mut dyn FnMut(String)) {
        if self.done {
            return;
        }
        if types.is_empty() {
            self.waited_for_types += 1;
            if self.waited_for_types == 1 {
                log("weather find: weather types not read yet, waiting".into());
            }
            if self.waited_for_types > 600 {
                log("weather find: no weather types; stopped".into());
                self.done = true;
            }
            return;
        }
        if let Some(w) = self.wait.as_mut() {
            *w = w.saturating_sub(1);
            if *w > 0 {
                return;
            }
            self.wait = None;
            match Self::reads(call) {
                Some((n, p)) if n == self.target || p == self.target => {
                    return self.finish(call, log, "after a wait");
                }
                r => log(format!(
                    "weather find no {:#018x}: reads {}",
                    SETTERS[self.idx].0,
                    r.map_or("nothing".into(), |(n, p)| format!(
                        "{} / {}",
                        name(n),
                        name(p)
                    ))
                )),
            }
            self.idx += 1;
            return;
        }
        let Some((next, prev)) = Self::reads(call) else {
            log("weather find: the weather getters did not answer".into());
            self.done = true;
            return;
        };
        let start = *self.start.get_or_insert(next);
        if self.idx >= SETTERS.len() {
            log(format!(
                "weather find: none of {} setters moved the weather",
                SETTERS.len()
            ));
            self.done = true;
            return;
        }
        if self.target == 0 {
            let other = |&&t: &&u32| t != start && t != prev;
            self.target = types
                .iter()
                .filter(other)
                .find(|&&t| NAMES.iter().any(|n| joaat(n) == t))
                .or_else(|| types.iter().find(other))
                .or_else(|| types.iter().find(|&&t| t != start))
                .copied()
                .unwrap_or(start);
        }
        let (hash, layout) = SETTERS[self.idx];
        log(format!(
            "weather find try {}/{} {hash:#018x} {layout:?}: {} -> {}",
            self.idx + 1,
            SETTERS.len(),
            name(start),
            name(self.target)
        ));
        if call(hash, &args(layout, self.target)).is_none() {
            log(format!(
                "weather find skip {hash:#018x}: no call (faulted or not in this build)"
            ));
            self.idx += 1;
            return;
        }
        match Self::reads(call) {
            Some((n, p)) if n == self.target || p == self.target => {
                self.finish(call, log, "at once")
            }
            _ => self.wait = Some(WAIT_TICKS),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_hash_as_the_title_does() {
        // Found as immediates in the title's ambient-speech weather pick (`+0x245ef5f`).
        assert_eq!(joaat("FOG"), 0xd61b_de01);
        assert_eq!(joaat("RAIN"), 0x54a6_9840);
        assert_eq!(joaat("rain"), joaat("RAIN"));
        assert_eq!(name(joaat("EXTRASUNNY")), "EXTRASUNNY");
        assert_eq!(name(0x1234), "0x00001234");
        for c in CHOICES.iter().skip(1) {
            assert!(NAMES.contains(c), "{c}");
        }
    }

    #[test]
    fn only_types_in_the_table_are_ever_passed() {
        let types = [joaat("CLEAR"), joaat("RAIN"), 0x5555];
        assert_eq!(pick("RAIN", &types, 0), Ok(joaat("RAIN")));
        assert!(
            pick("SNOW", &types, 0)
                .unwrap_err()
                .contains("not a weather type")
        );
        assert_eq!(pick("next", &types, joaat("RAIN")), Ok(0x5555));
        assert_eq!(pick("next", &types, 0x5555), Ok(joaat("CLEAR")), "wraps");
        assert_eq!(
            pick("next", &types, 7),
            Ok(joaat("CLEAR")),
            "unknown current: the first"
        );
        assert!(pick("next", &[], 0).is_err());
        assert_eq!(args(Layout::Pair, 9), vec![9, 9, 0, 0]);
        assert_eq!(args(Layout::Now, 9).len(), 7);
    }

    /// A world where setter `ok` (with layout `Pair`) sets both types, possibly a tick late.
    struct Sky {
        next: u32,
        prev: u32,
        pending: Option<u32>,
        late: bool,
        ok: u64,
    }

    impl Sky {
        fn call(&mut self, h: u64, a: &[u64]) -> Option<[u64; 4]> {
            match h {
                NATIVE_WEATHER_NEXT => Some([u64::from(self.next), 0, 0, 0]),
                NATIVE_WEATHER_PREV => Some([u64::from(self.prev), 0, 0, 0]),
                h if h == self.ok => {
                    let t = a[0] as u32;
                    if self.late {
                        self.pending = Some(t);
                    } else {
                        self.next = t;
                        self.prev = t;
                    }
                    Some([0; 4])
                }
                0xb271_e5bc_174b_ba1f => None,
                _ => Some([0; 4]),
            }
        }
        fn frame(&mut self) {
            if let Some(t) = self.pending.take() {
                self.next = t;
                self.prev = t;
            }
        }
    }

    fn run(sky: &mut Sky, types: &[u32]) -> (WeatherFind, Vec<String>) {
        let mut f = WeatherFind::new();
        let mut lines = Vec::new();
        for _ in 0..50 {
            f.tick(&mut |h, a| sky.call(h, a), types, &mut |s| lines.push(s));
            sky.frame();
            if f.done {
                break;
            }
        }
        (f, lines)
    }

    #[test]
    fn find_weather_proves_a_setter_by_reading_back_and_puts_the_weather_back() {
        let clear = joaat("CLEAR");
        let types = [clear, 0x77, joaat("RAIN")];
        let mut sky = Sky {
            next: clear,
            prev: clear,
            pending: None,
            late: false,
            ok: SETTERS[1].0,
        };
        let (f, lines) = run(&mut sky, &types);
        assert_eq!(f.found, Some(SETTERS[1]));
        assert!(
            lines[0].contains("try 1/3") && lines[0].ends_with("CLEAR -> RAIN"),
            "{lines:?}"
        );
        assert!(
            lines[1].starts_with("weather find no 0x5e2192a205c1c357"),
            "{lines:?}"
        );
        assert!(lines[2].contains("try 2/3"));
        assert!(lines[3].contains("(at once): CLEAR -> RAIN"));
        assert_eq!(lines[4], "weather find put back CLEAR: reads CLEAR");
        assert_eq!(sky.next, clear);
        let mut sky = Sky {
            next: clear,
            prev: clear,
            pending: None,
            late: true,
            ok: SETTERS[0].0,
        };
        let (f, lines) = run(&mut sky, &types);
        assert_eq!(f.found, Some(SETTERS[0]));
        assert!(lines[1].contains("(after a wait)"), "{lines:?}");
    }

    #[test]
    fn find_weather_waits_for_the_table_and_skips_what_cannot_be_called() {
        let mut sky = Sky {
            next: 1,
            prev: 1,
            pending: None,
            late: false,
            ok: SETTERS[2].0,
        };
        let mut f = WeatherFind::new();
        let mut lines = Vec::new();
        f.tick(&mut |h, a| sky.call(h, a), &[], &mut |s| lines.push(s));
        assert_eq!(
            lines,
            vec!["weather find: weather types not read yet, waiting"]
        );
        sky.ok = 0;
        let (f, lines) = run(&mut sky, &[1, 2]);
        assert_eq!((f.found, f.done), (None, true));
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("weather find skip 0xb271e5bc174bba1f"))
        );
        assert_eq!(
            lines.last().unwrap(),
            "weather find: none of 3 setters moved the weather"
        );
    }

    #[test]
    fn the_weather_row_uses_the_found_setter_and_names_what_it_reads() {
        assert!(set_found(SETTERS[0].0));
        assert!(!set_found(0x1234), "only a known setter");
        *lock(&TYPES) = vec![joaat("CLEAR"), joaat("RAIN")];
        let mut sky = Sky {
            next: joaat("CLEAR"),
            prev: joaat("CLEAR"),
            pending: None,
            late: false,
            ok: SETTERS[0].0,
        };
        let rain = CHOICES.iter().position(|c| *c == "RAIN").unwrap() as u64;
        assert!(set_script(&mut |h, a| sky.call(h, a), &[rain]).is_some());
        assert_eq!(sky.next, joaat("RAIN"));
        assert!(row_get().ends_with("reads RAIN"), "{}", row_get());
        let snow = CHOICES.iter().position(|c| *c == "SNOW").unwrap() as u64;
        assert!(set_script(&mut |h, a| sky.call(h, a), &[snow]).is_none());
        assert!(row_get().contains("not a weather type"));
    }
}
