//! (El1xr) The trainer menu, `VICTORY_MOD_MENU=1` (default off): single-player options that call
//! the title's own script natives. F8 (`VICTORY_MOD_MENU_KEY` rebinds it) opens and closes it;
//! it is the settings menu's window, drawing and input shielding ([`crate::menu`]) over its own
//! model ([`model::Trainer`]).
//!
//! At start [`init`] scans the mapped title for its native handlers ([`natives`]) and logs
//! `mod menu: natives found N`. Calls are queued ([`queue`]) and run by [`game_tick`], which the
//! HLE calls on the title's own thread each time it reads its input (`GetCurrentReading`, the
//! frame's input update): never on our threads, never under our locks. Each handler takes the
//! title's global recursive native lock itself.
//!
//! `VICTORY_MOD_MENU_SELFTEST=1` queues one read of the title's frame-rate native at the first
//! game tick after the table is built and logs `mod menu: selftest ...` with the result.
//! `VICTORY_MOD_MENU_CENSUS=1` counts calls per native ([`census`]) and logs the busiest every
//! [`CENSUS_EVERY`]; `VICTORY_MOD_MENU_PROBE=1` logs the trainer's reads ([`probe`]).
//!
//! The clock hold (`MOD_CLOCK_HOLD`) sets the clock on every game tick while it is on and logs
//! `mod menu: clock hold at H` / `mod menu: clock hold off` when it changes.

pub mod census;
pub mod model;
pub mod natives;
pub mod probe;
pub mod queue;

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering::Relaxed};
use std::sync::{LazyLock, Mutex, OnceLock};
use std::time::{Duration, Instant};

use natives::Scan;
use queue::{Call, Outcome, Queue};

const VK_F8: u8 = 0x77;

/// `VICTORY_MOD_MENU=1`.
pub fn enabled() -> bool {
    static ON: LazyLock<bool> =
        LazyLock::new(|| std::env::var("VICTORY_MOD_MENU").is_ok_and(|v| v.trim() == "1"));
    *ON
}

/// The trainer's hotkey (`VICTORY_MOD_MENU_KEY`, F8 by default).
pub fn hotkey() -> u8 {
    static KEY: LazyLock<u8> = LazyLock::new(|| {
        std::env::var("VICTORY_MOD_MENU_KEY")
            .ok()
            .and_then(|v| crate::menu::parse_key(&v))
            .unwrap_or(VK_F8)
    });
    *KEY
}

struct Table {
    base: usize,
    scan: Scan,
}

static TABLE: OnceLock<Result<Table, String>> = OnceLock::new();
pub(crate) static QUEUE: LazyLock<Queue> = LazyLock::new(Queue::default);
static SELFTEST_QUEUED: AtomicBool = AtomicBool::new(false);
static TICKS: AtomicUsize = AtomicUsize::new(0);
static CENSUS: OnceLock<census::Census> = OnceLock::new();
static CENSUS_LAST: Mutex<Option<Instant>> = Mutex::new(None);
static PROBE: LazyLock<Mutex<probe::Probe>> = LazyLock::new(Mutex::default);
/// The hold the hook last logged (`-1`: off).
static HOLD_LOGGED: AtomicI64 = AtomicI64::new(-1);

/// How often the census logs, and how many natives each time.
pub const CENSUS_EVERY: Duration = Duration::from_secs(30);
const CENSUS_TOP: usize = 40;

fn flag(name: &str) -> bool {
    std::env::var(name).is_ok_and(|v| v.trim() == "1")
}

/// Builds the native table from the title image mapped at `base` (`size` bytes) and logs what it
/// found. Call once, after mapping and before the title runs.
///
/// # Safety
/// `base..base + size` must be the mapped title image, readable for the whole run.
pub unsafe fn init(base: usize, size: usize) {
    if !enabled() {
        return;
    }
    // SAFETY: the caller's contract.
    let image = unsafe { std::slice::from_raw_parts(base as *const u8, size) };
    let table = match natives::scan(image) {
        Ok(scan) => {
            tracing::info!(
                repeated = scan.repeated,
                unplaced = scan.unplaced,
                title_fps = scan
                    .find(model::NATIVE_TITLE_FPS)
                    .map(|r| format!("{r:#x}")),
                "mod menu: natives found {}",
                scan.natives.len()
            );
            if flag("VICTORY_MOD_MENU_CENSUS") {
                // SAFETY: the caller's contract (the title has not started).
                match unsafe { census::install(base, size, &scan.natives) } {
                    Ok(c) => {
                        tracing::info!(
                            skipped = c.skipped,
                            "mod menu: census hooked {} natives",
                            c.hooked
                        );
                        let _ = CENSUS.set(c);
                    }
                    Err(e) => tracing::warn!("mod menu: census off ({e})"),
                }
            }
            Ok(Table { base, scan })
        }
        Err(e) => {
            tracing::warn!("mod menu: natives not found ({e}); the trainer menu has no options");
            Err(e.to_string())
        }
    };
    let _ = TABLE.set(table);
}

/// The table's state for the menu.
pub fn status() -> String {
    match TABLE.get() {
        None => "not scanned".into(),
        Some(Ok(t)) => format!("{} found", t.scan.natives.len()),
        Some(Err(e)) => format!("none: {e}"),
    }
}

/// The game-thread hook: runs the queued native calls. Cheap when nothing waits.
pub fn game_tick() {
    if !enabled() {
        return;
    }
    let Some(Ok(table)) = TABLE.get() else { return };
    if TICKS.fetch_add(1, Relaxed) == 0 && selftest() && !SELFTEST_QUEUED.swap(true, Relaxed) {
        let call = Call {
            name: "selftest",
            hash: model::NATIVE_TITLE_FPS,
            args: Vec::new(),
            script: None,
        };
        QUEUE.push(call);
    }
    side_logs();
    hold_clock();
    if QUEUE.idle() {
        return;
    }
    let mut native = |hash: u64, args: &[u64]| {
        let rva = table.scan.find(hash)?;
        // SAFETY: a handler the scan found in the mapped image, on the title's thread; the
        // options and scripts pass each native the slots it takes.
        Some(unsafe { natives::invoke(table.base + rva as usize, args) })
    };
    QUEUE.drain(|call| match call.script {
        Some(script) => script(&mut native, &call.args),
        None => native(call.hash, &call.args),
    });
    if SELFTEST_QUEUED.load(Relaxed)
        && let Some(outcome) = QUEUE.result("selftest")
        && SELFTEST_QUEUED.swap(false, Relaxed)
    {
        match outcome {
            Outcome::Done([v, ..]) => tracing::info!(
                thread = ?std::thread::current().id(),
                "mod menu: selftest native {:#018x} (title frame rate) -> {}",
                model::NATIVE_TITLE_FPS,
                v as i64 as i32
            ),
            Outcome::Unknown => tracing::warn!(
                "mod menu: selftest native {:#018x} is not in the table",
                model::NATIVE_TITLE_FPS
            ),
        }
    }
}

/// While the clock hold is on, queues the clock set for this tick; logs when the hold changes.
fn hold_clock() {
    let held = model::held_hour();
    let now = held.map_or(-1, |h| h as i64);
    if HOLD_LOGGED.swap(now, Relaxed) != now {
        match held {
            Some(h) => tracing::info!("mod menu: clock hold at {h}"),
            None => tracing::info!("mod menu: clock hold off"),
        }
    }
    if let Some(h) = held {
        QUEUE.push(Call {
            name: "hold",
            hash: model::NATIVE_SET_CLOCK_TIME,
            args: vec![h, 0, 0, 0],
            script: None,
        });
    }
}

/// The census and probe lines, on their own clocks.
fn side_logs() {
    if CENSUS.get().is_none() && !probe_on() {
        return;
    }
    let now = Instant::now();
    if let Some(c) = CENSUS.get() {
        let mut last = CENSUS_LAST.lock().unwrap_or_else(|e| e.into_inner());
        if last.is_none_or(|t| now - t >= CENSUS_EVERY) {
            let first = last.is_none();
            *last = Some(now);
            let (calls, ran, top) = c.report(CENSUS_TOP);
            if !first {
                let top: Vec<String> = top.iter().map(|(h, n)| format!("{h:016x}={n}")).collect();
                tracing::info!(
                    "mod menu: census {}s calls={calls} natives={ran} top {}",
                    CENSUS_EVERY.as_secs(),
                    top.join(" ")
                );
            }
        }
    }
    if probe_on() {
        static PLAN: LazyLock<Option<probe::SetPlan>> = LazyLock::new(|| {
            std::env::var("VICTORY_MOD_MENU_PROBE_SET")
                .ok()
                .and_then(|v| probe::parse_set(&v))
        });
        let lines = PROBE.lock().unwrap_or_else(|e| e.into_inner()).tick(
            now,
            model::OPTIONS,
            &QUEUE,
            PLAN.as_ref(),
        );
        for line in lines {
            tracing::info!("{line}");
        }
    }
}

fn probe_on() -> bool {
    static ON: LazyLock<bool> = LazyLock::new(|| flag("VICTORY_MOD_MENU_PROBE"));
    *ON
}

fn selftest() -> bool {
    static ON: LazyLock<bool> =
        LazyLock::new(|| std::env::var("VICTORY_MOD_MENU_SELFTEST").is_ok_and(|v| v.trim() == "1"));
    *ON
}

/// The trainer's model over the live queue.
pub fn trainer() -> Box<dyn crate::menu::model::MenuModel> {
    Box::new(model::Trainer::new(model::OPTIONS, &QUEUE))
}
