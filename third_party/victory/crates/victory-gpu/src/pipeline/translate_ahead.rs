//! (bootfast) Shaders translated on threads of their own as soon as the
//! title creates a pipeline with them (`VICTORY_TRANSLATE_AHEAD=N`, N of 1
//! to 8 threads; unset or 0: off, as before).
//!
//! A shader is translated on first use ([`Shader::translated`]): the
//! workers that make an early pipeline, or the first draw or dispatch that
//! binds it, on a thread that records the title's frame. With the
//! translator's cache on disk warm, each first use still parses the
//! container and its native programs, reads the cache entry and decodes it
//! (about 1 ms; 3,000-3,800 shaders in the first minute: "shader translate"
//! summed to about 3 s before the first world frame). With a cold cache
//! (the first launch after the translator changed, or after a switch that
//! is part of its key) it is a full translation, and a batch's first runs
//! of each arm had 1,600-1,900 such misses before the bake began.
//!
//! With the switch the shaders of every pipeline the title creates are
//! queued for N low-priority threads, which call `translated` for them. A
//! later first use finds the translation made (or waits for the one under
//! way: `OnceLock`), so nothing is translated twice and the translation is
//! the same; only where and when it runs changes.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Condvar, LazyLock, Mutex, OnceLock};

use super::Shader;

/// The threads (`VICTORY_TRANSLATE_AHEAD`, at most 8; 0 when unset).
fn threads() -> usize {
    static N: OnceLock<usize> = OnceLock::new();
    *N.get_or_init(|| parse_threads(std::env::var("VICTORY_TRANSLATE_AHEAD").ok().as_deref()))
}

fn parse_threads(value: Option<&str>) -> usize {
    value
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(0)
        .min(8)
}

static QUEUE: LazyLock<Mutex<VecDeque<Arc<Shader>>>> = LazyLock::new(|| Mutex::new(VecDeque::new()));
static WORK: Condvar = Condvar::new();

/// Shaders queued, translated here (not found translated already), and the
/// threads' milliseconds, since the start.
static STATS: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

/// Queues `shaders`' translations for the threads (none without the
/// switch, or already translated).
pub(super) fn ahead(shaders: &[Arc<Shader>]) {
    let n = threads();
    if n == 0 {
        return;
    }
    static STARTED: std::sync::Once = std::sync::Once::new();
    STARTED.call_once(|| {
        for i in 0..n {
            if let Err(e) = std::thread::Builder::new()
                .name(format!("victory translate ahead {i}"))
                .spawn(run)
            {
                tracing::warn!(error = %e, "a translate-ahead thread could not be started");
            }
        }
        tracing::info!(
            threads = n,
            "shaders are translated on threads of their own when the title creates their pipelines (VICTORY_TRANSLATE_AHEAD)"
        );
    });
    let mut q = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
    let before = q.len();
    q.extend(shaders.iter().filter(|s| s.translated.get().is_none()).cloned());
    let added = q.len() - before;
    drop(q);
    if added > 0 {
        STATS[0].fetch_add(added as u64, Relaxed);
        WORK.notify_all();
    }
}

fn run() {
    crate::topology::pin_background("victory-translate-ahead");
    {
        use windows_sys::Win32::System::Threading::{
            GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL,
        };
        // SAFETY: a plain call on our own thread.
        unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL) };
    }
    loop {
        let shader = {
            let mut q = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                if let Some(s) = q.pop_front() {
                    break s;
                }
                q = WORK.wait(q).unwrap_or_else(|e| e.into_inner());
            }
        };
        if shader.translated.get().is_some() {
            continue;
        }
        let began = std::time::Instant::now();
        let ok = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            shader.translated();
        }))
        .is_ok();
        if !ok {
            static ONCE: std::sync::Once = std::sync::Once::new();
            ONCE.call_once(|| tracing::error!("a translate-ahead thread failed on a shader"));
            continue;
        }
        let done = STATS[1].fetch_add(1, Relaxed) + 1;
        let ms = STATS[2].fetch_add(began.elapsed().as_millis() as u64, Relaxed);
        if done.is_power_of_two() {
            tracing::info!(
                translated_here = done,
                queued = STATS[0].load(Relaxed),
                ms_so_far = ms,
                "shaders translated ahead (VICTORY_TRANSLATE_AHEAD)"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_switch_is_off_unless_set() {
        use super::parse_threads;
        assert_eq!(parse_threads(None), 0);
        assert_eq!(parse_threads(Some("0")), 0);
        assert_eq!(parse_threads(Some("x")), 0);
        assert_eq!(parse_threads(Some(" 4")), 4);
        assert_eq!(parse_threads(Some("64")), 8);
    }
}
