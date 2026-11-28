//! (worldstream, #26) `VICTORY_SHADER_PREFETCH=N`: the translated shaders
//! the last boot looked up are read ahead on N threads (1 to 32) from the
//! moment the shader cache opens, before the title creates the pipelines
//! that need them. Restart; unset or 0: off, nothing read or written.
//!
//! Why: with the operating system's file cache cold (the first launch after
//! a reboot, or after a run under another translator key), each cache hit
//! costs a file look-up, open and read among about a million files: part T
//! and U's OFF launches spent 36-92 thread-seconds in "shader translate"
//! (all hits, misses=0) before the first world frame against 12 in a warm
//! one, with the boot frames 2.4-2.9 s long and 1.4-1.8 s of "pipeline
//! wait" on the presenting thread. The four translate-ahead threads wait on
//! the disk one file at a time. Here N threads read the files (or the pack's
//! records with `VICTORY_SHADER_PACK=1`) in the order the last boot asked
//! for them and drop the bytes: the lookups that follow find them in the
//! file cache. Nothing is kept in our memory, so commit does not grow.
//!
//! The order comes from `<cache>/prefetch/boot_keys`: each lookup's key,
//! first use only, during the first [`RECORD_S`] seconds after the cache
//! opened (at most [`MOST_KEYS`]), written once at the end of that window
//! (staging file and rename). A key the cache no longer holds is skipped.

use std::collections::HashSet;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use victory_pipeline::Cache;

/// Keys are noted this long after the cache opened.
const RECORD_S: u64 = 90;
/// And at most this many.
const MOST_KEYS: usize = 1 << 16;
const KEY: usize = 32;

/// The threads (`VICTORY_SHADER_PREFETCH`, at most 32; 0 when unset).
fn threads() -> usize {
    static N: OnceLock<usize> = OnceLock::new();
    *N.get_or_init(|| parse(std::env::var("VICTORY_SHADER_PREFETCH").ok().as_deref()))
}

fn parse(value: Option<&str>) -> usize {
    value
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(0)
        .min(32)
}

/// The keys looked up since the cache opened, first use only.
struct Noted {
    since: Instant,
    seen: HashSet<[u8; KEY]>,
    order: Vec<[u8; KEY]>,
}

static NOTED: Mutex<Option<Noted>> = Mutex::new(None);

fn file(root: &Path) -> PathBuf {
    root.join("prefetch").join("boot_keys")
}

/// The cache opened: the last boot's keys are read ahead, and this boot's
/// are noted (nothing without the switch).
pub(crate) fn opened(cache: &'static Cache) {
    let n = threads();
    if n == 0 {
        return;
    }
    *NOTED.lock().unwrap_or_else(|e| e.into_inner()) = Some(Noted {
        since: Instant::now(),
        seen: HashSet::new(),
        order: Vec::new(),
    });
    let keys = std::fs::read(file(cache.root()))
        .map(|b| keys(&b))
        .unwrap_or_default();
    let spawned = std::thread::Builder::new()
        .name("victory shader prefetch".into())
        .spawn(move || {
            read_ahead(cache, Arc::new(keys), n);
            let left = Duration::from_secs(RECORD_S).saturating_sub(started_ago());
            std::thread::sleep(left);
            write(cache.root());
        });
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "shader prefetch: its thread could not be started");
    }
}

fn started_ago() -> Duration {
    NOTED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map_or(Duration::ZERO, |n| n.since.elapsed())
}

/// The keys of a `boot_keys` file (a torn tail dropped).
fn keys(bytes: &[u8]) -> Vec<[u8; KEY]> {
    bytes
        .chunks_exact(KEY)
        .take(MOST_KEYS)
        .filter_map(|c| c.try_into().ok())
        .collect()
}

/// A lookup of `key` (`crate::shader_cache::cached`).
pub(crate) fn noted(key: &[u8; KEY]) {
    if threads() == 0 {
        return;
    }
    let mut noted = NOTED.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(n) = noted.as_mut() {
        LOOKUPS.fetch_add(1, Relaxed);
        if n.order.len() < MOST_KEYS
            && n.since.elapsed().as_secs() < RECORD_S
            && n.seen.insert(*key)
        {
            n.order.push(*key);
        }
    }
}

/// Lookups since the cache opened.
static LOOKUPS: AtomicU64 = AtomicU64::new(0);

/// Reads `keys` on `n` threads, in order (thread `i` takes `i`, `i + n`, ...
/// so the first keys go first), dropping the bytes.
fn read_ahead(cache: &'static Cache, keys: Arc<Vec<[u8; KEY]>>, n: usize) {
    if keys.is_empty() {
        tracing::info!("shader prefetch: no keys from the last boot yet (VICTORY_SHADER_PREFETCH)");
        return;
    }
    let began = Instant::now();
    let next = Arc::new(AtomicUsize::new(0));
    let [read, bytes, missing] = [(); 3].map(|_| Arc::new(AtomicU64::new(0)));
    let workers: Vec<_> = (0..n)
        .filter_map(|i| {
            let (keys, next) = (keys.clone(), next.clone());
            let (read, bytes, missing) = (read.clone(), bytes.clone(), missing.clone());
            std::thread::Builder::new()
                .name(format!("victory shader prefetch {i}"))
                .spawn(move || {
                    crate::topology::pin_background("victory-shader-prefetch");
                    below_normal();
                    let pack = crate::shader_pack::pack(cache);
                    loop {
                        let at = next.fetch_add(1, Relaxed);
                        let Some(key) = keys.get(at) else { break };
                        match fetch(cache, pack, key) {
                            Some(len) => {
                                read.fetch_add(1, Relaxed);
                                bytes.fetch_add(len, Relaxed);
                            }
                            None => {
                                missing.fetch_add(1, Relaxed);
                            }
                        }
                    }
                })
                .ok()
        })
        .collect();
    for w in workers {
        let _ = w.join();
    }
    tracing::info!(
        keys = keys.len(),
        read = read.load(Relaxed),
        mb = bytes.load(Relaxed) >> 20,
        missing = missing.load(Relaxed),
        threads = n,
        ms = began.elapsed().as_millis() as u64,
        lookups_meanwhile = LOOKUPS.load(Relaxed),
        "shader prefetch: the last boot's shaders read ahead (VICTORY_SHADER_PREFETCH)"
    );
}

/// Reads one entry and drops it: its length, or `None` when the cache does
/// not hold it.
fn fetch(cache: &Cache, pack: Option<&victory_pipeline::Pack>, key: &[u8; KEY]) -> Option<u64> {
    if let Some(bytes) = pack.and_then(|p| p.get(key)) {
        return Some(bytes.len() as u64);
    }
    let path = cache.path_for(&victory_pipeline::CacheKey::from_bytes(*key));
    std::fs::read(path).ok().map(|b| b.len() as u64)
}

fn below_normal() {
    use windows_sys::Win32::System::Threading::{
        GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL,
    };
    // SAFETY: a plain call on our own thread.
    unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL) };
}

/// Writes this boot's keys for the next one (staging file and rename).
fn write(root: &Path) {
    let order = {
        let noted = NOTED.lock().unwrap_or_else(|e| e.into_inner());
        noted.as_ref().map(|n| n.order.clone()).unwrap_or_default()
    };
    if order.is_empty() {
        return;
    }
    let path = file(root);
    match write_keys(&path, &order) {
        Ok(()) => tracing::info!(
            keys = order.len(),
            path = %path.display(),
            "shader prefetch: this boot's shaders noted for the next (VICTORY_SHADER_PREFETCH)"
        ),
        Err(e) => {
            tracing::warn!(error = %e, path = %path.display(), "shader prefetch: the boot's keys were not written")
        }
    }
}

fn write_keys(path: &Path, order: &[[u8; KEY]]) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!("boot_keys.{}.tmp", std::process::id()));
    let mut f = std::fs::File::create(&tmp)?;
    for key in order {
        f.write_all(key)?;
    }
    f.sync_all()?;
    drop(f);
    let done = std::fs::rename(&tmp, path);
    if done.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    done
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_switch_is_off_unless_set() {
        assert_eq!(parse(None), 0);
        assert_eq!(parse(Some("x")), 0);
        assert_eq!(parse(Some(" 16")), 16);
        assert_eq!(parse(Some("99")), 32);
    }

    #[test]
    fn keys_round_trip_and_a_torn_tail_is_dropped() {
        let tmp = tempfile::tempdir().unwrap();
        let path = file(tmp.path());
        let order = [[1u8; KEY], [2u8; KEY], [3u8; KEY]];
        write_keys(&path, &order).unwrap();
        let mut bytes = std::fs::read(&path).unwrap();
        assert_eq!(keys(&bytes), order);
        bytes.extend([9u8; 7]);
        assert_eq!(keys(&bytes), order, "a torn tail");
    }

    #[test]
    fn an_entry_is_read_from_its_loose_file_and_a_missing_one_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = Cache::open(tmp.path().join("cache")).unwrap();
        let key = [7u8; KEY];
        let path = cache.path_for(&victory_pipeline::CacheKey::from_bytes(key));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"abcd").unwrap();
        assert_eq!(fetch(&cache, None, &key), Some(4));
        assert_eq!(fetch(&cache, None, &[8u8; KEY]), None);
    }
}
