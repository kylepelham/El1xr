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

pub mod model;
pub mod natives;
pub mod queue;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};
use std::sync::{LazyLock, OnceLock};

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
        };
        QUEUE.push(call);
    }
    if QUEUE.idle() {
        return;
    }
    QUEUE.drain(|call| {
        let rva = table.scan.find(call.hash)?;
        // SAFETY: a handler the scan found in the mapped image, on the title's thread; the
        // options pass each native the slots it takes.
        Some(unsafe { natives::invoke(table.base + rva as usize, &call.args) })
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

fn selftest() -> bool {
    static ON: LazyLock<bool> =
        LazyLock::new(|| std::env::var("VICTORY_MOD_MENU_SELFTEST").is_ok_and(|v| v.trim() == "1"));
    *ON
}

/// The trainer's model over the live queue.
pub fn trainer() -> Box<dyn crate::menu::model::MenuModel> {
    Box::new(model::Trainer::new(model::OPTIONS, &QUEUE))
}
