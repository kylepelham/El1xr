use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

use anyhow::{Context, Result};

use crate::Converter;

/// Identifies one conversion result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CacheKey(blake3::Hash);

impl CacheKey {
    pub fn new(converter: &dyn Converter, input: &[u8]) -> Self {
        let mut h = blake3::Hasher::new();
        h.update(converter.id().as_bytes());
        h.update(&[0]);
        h.update(&converter.version().to_le_bytes());
        h.update(input);
        CacheKey(h.finalize())
    }

    /// The key's 32 bytes (the pack's index, `crate::pack`).
    pub fn bytes(&self) -> &[u8; 32] {
        self.0.as_bytes()
    }

    pub fn to_hex(&self) -> String {
        self.0.to_hex().to_string()
    }
}

/// (Vice-Tay 511a950) The conversions running, by key: a second miss for
/// a key being converted waits for that conversion instead of converting
/// it too (`VICTORY_SHARED_CONVERSIONS=1`).
type Running = Mutex<HashMap<CacheKey, Weak<Mutex<()>>>>;

/// Misses that waited for another thread's conversion of their key and
/// took its result ([`shared_conversions`]).
static SHARED: AtomicU64 = AtomicU64::new(0);

/// Misses served by another thread's conversion of the same key since the
/// start (`VICTORY_SHARED_CONVERSIONS=1`; 0 without it).
pub fn shared_conversions() -> u64 {
    SHARED.load(Ordering::Relaxed)
}

/// Whether concurrent misses for one key share one conversion
/// (`VICTORY_SHARED_CONVERSIONS=1`; off unless set).
fn sharing() -> bool {
    std::env::var_os("VICTORY_SHARED_CONVERSIONS").is_some_and(|v| v == "1")
}

/// Content-addressed store under the user's cache directory.
#[derive(Debug, Clone)]
pub struct Cache {
    root: PathBuf,
    /// With `VICTORY_SHARED_CONVERSIONS=1`: the conversions running, shared
    /// by the clones of this cache.
    running: Option<Arc<Running>>,
}

impl Cache {
    /// The cache's folder.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Opens (creating) a cache at `root`. Refuses locations inside the repo.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        Self::open_sharing(root, sharing())
    }

    /// [`Cache::open`], with concurrent misses for one key sharing one
    /// conversion or not.
    fn open_sharing(root: impl Into<PathBuf>, share: bool) -> Result<Self> {
        let root = root.into();
        victory_core::ensure_outside_repo(&root)?;
        std::fs::create_dir_all(&root).with_context(|| format!("creating {}", root.display()))?;
        Ok(Cache {
            root,
            running: share.then(Arc::default),
        })
    }

    /// The lock of `key`'s conversion, made if none is running
    /// (`VICTORY_SHARED_CONVERSIONS=1`).
    fn conversion(running: &Running, key: CacheKey) -> Arc<Mutex<()>> {
        let mut running = running.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(conversion) = running.get(&key).and_then(Weak::upgrade) {
            return conversion;
        }
        running.retain(|_, conversion| conversion.strong_count() != 0);
        let conversion = Arc::new(Mutex::new(()));
        running.insert(key, Arc::downgrade(&conversion));
        conversion
    }

    pub fn path_for(&self, key: &CacheKey) -> PathBuf {
        let hex = key.to_hex();
        self.root.join(&hex[..2]).join(&hex[2..])
    }

    /// Returns the cached output for `input`, converting it first if needed.
    pub fn get_or_convert(&self, converter: &dyn Converter, input: &[u8]) -> Result<PathBuf> {
        self.get_or_convert_keyed(&CacheKey::new(converter, input), converter, input)
    }

    /// [`Cache::get_or_convert`] with the key worked out already
    /// (`CacheKey::new(converter, input)`).
    pub fn get_or_convert_keyed(&self, key: &CacheKey, converter: &dyn Converter, input: &[u8]) -> Result<PathBuf> {
        let key = *key;
        let path = self.path_for(&key);
        if path.is_file() {
            tracing::trace!(converter = converter.id(), key = %key.to_hex(), "cache hit");
            return Ok(path);
        }
        let conversion = self.running.as_deref().map(|running| Self::conversion(running, key));
        let _converting = conversion.as_deref().map(|c| c.lock().unwrap_or_else(|e| e.into_inner()));
        if conversion.is_some() && path.is_file() {
            // Converted by the thread this one waited for.
            SHARED.fetch_add(1, Ordering::Relaxed);
            return Ok(path);
        }
        let output = converter
            .convert(input)
            .with_context(|| format!("converter {} v{}", converter.id(), converter.version()))?;
        let dir = path.parent().expect("hashed path has a parent");
        std::fs::create_dir_all(dir)?;
        // Write-then-rename so readers never see partial output.
        let tmp = tempfile_in(dir);
        // A process killed before it removed its staging file leaves the
        // name behind, and a later process can get the same id (Windows
        // reuses process ids) and counter: `create_new` then fails and the
        // conversion, already done, is lost for the whole run (one shader
        // never translates). Only the writer reads such a file, and it is
        // gone, so an existing one is written again.
        let mut file = match std::fs::OpenOptions::new().write(true).create_new(true).open(&tmp) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => std::fs::OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(&tmp)
                .with_context(|| format!("reusing the staging file {}", tmp.display()))?,
            Err(e) => return Err(e.into()),
        };
        let written = file.write_all(&output);
        drop(file);
        let published = written.and_then(|()| std::fs::rename(&tmp, &path));
        if let Err(e) = published {
            let _ = std::fs::remove_file(&tmp);
            // (Vice-Tay 511a950) Another process published the key first (or
            // a reader held the file open): its output is the same.
            if self.running.is_none() || !path.is_file() {
                return Err(e.into());
            }
        }
        Ok(path)
    }
}

/// Names the staging files: the process id and a counter, which a killed
/// process can leave behind for a later one to meet ([`get_or_convert`]).
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn tempfile_in(dir: &Path) -> PathBuf {
    let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    dir.join(format!(".tmp-{}-{n}", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// The tests share the process-wide staging-file counter (and so the
    /// name the next file gets): one at a time keeps them predictable.
    fn one_at_a_time() -> std::sync::MutexGuard<'static, ()> {
        static ONE: std::sync::Mutex<()> = std::sync::Mutex::new(());
        ONE.lock().unwrap_or_else(|e| e.into_inner())
    }

    struct Upper {
        version: u32,
        runs: AtomicUsize,
    }

    impl Converter for Upper {
        fn id(&self) -> &'static str {
            "test.upper"
        }
        fn version(&self) -> u32 {
            self.version
        }
        fn convert(&self, input: &[u8]) -> Result<Vec<u8>> {
            self.runs.fetch_add(1, Ordering::Relaxed);
            Ok(input.to_ascii_uppercase())
        }
    }

    #[test]
    fn converts_once_then_hits() {
        let _one = one_at_a_time();
        let tmp = tempfile::tempdir().unwrap();
        let cache = Cache::open(tmp.path().join("cache")).unwrap();
        let conv = Upper {
            version: 1,
            runs: AtomicUsize::new(0),
        };
        let a = cache.get_or_convert(&conv, b"abc").unwrap();
        let b = cache.get_or_convert(&conv, b"abc").unwrap();
        assert_eq!(a, b);
        assert_eq!(std::fs::read(&a).unwrap(), b"ABC");
        assert_eq!(conv.runs.load(Ordering::Relaxed), 1);
    }

    /// [`Upper`], slow enough that the threads of a test all miss.
    struct SlowUpper(Upper);

    impl Converter for SlowUpper {
        fn id(&self) -> &'static str {
            self.0.id()
        }
        fn version(&self) -> u32 {
            self.0.version()
        }
        fn convert(&self, input: &[u8]) -> Result<Vec<u8>> {
            std::thread::sleep(std::time::Duration::from_millis(30));
            self.0.convert(input)
        }
    }

    /// `threads` threads miss on one key at once, sharing conversions; the
    /// conversions run.
    fn misses_at_once(threads: usize) -> usize {
        let tmp = tempfile::tempdir().unwrap();
        let cache = Cache::open_sharing(tmp.path().join("cache"), true).unwrap();
        let conv = SlowUpper(Upper {
            version: 1,
            runs: AtomicUsize::new(0),
        });
        let start = std::sync::Barrier::new(threads);
        std::thread::scope(|scope| {
            for _ in 0..threads {
                let (cache, start, conv) = (&cache, &start, &conv);
                scope.spawn(move || {
                    start.wait();
                    let path = cache.get_or_convert(conv, b"abc").unwrap();
                    assert_eq!(std::fs::read(path).unwrap(), b"ABC");
                });
            }
        });
        conv.0.runs.load(Ordering::Relaxed)
    }

    #[test]
    fn concurrent_misses_for_one_key_share_a_conversion() {
        let _one = one_at_a_time();
        let shared = shared_conversions();
        assert_eq!(misses_at_once(8), 1);
        assert_eq!(shared_conversions() - shared, 7);
    }

    #[test]
    fn failed_publication_removes_the_staging_file() {
        let _one = one_at_a_time();
        let tmp = tempfile::tempdir().unwrap();
        let cache = Cache::open(tmp.path().join("cache")).unwrap();
        let conv = Upper {
            version: 1,
            runs: AtomicUsize::new(0),
        };
        let path = cache.path_for(&CacheKey::new(&conv, b"abc"));
        std::fs::create_dir_all(&path).unwrap();

        assert!(cache.get_or_convert(&conv, b"abc").is_err());
        assert!(path.is_dir());
        assert_eq!(
            std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
            1
        );
    }

    /// A staging file a killed process left behind - the next process has
    /// the same id and counter, which is how the name repeats - does not
    /// fail the conversion: it is written again and renamed into place.
    #[test]
    fn a_stale_staging_file_does_not_fail_a_conversion() {
        let _one = one_at_a_time();
        let tmp = tempfile::tempdir().unwrap();
        let cache = Cache::open(tmp.path().join("cache")).unwrap();
        let conv = Upper {
            version: 1,
            runs: AtomicUsize::new(0),
        };
        let path = cache.path_for(&CacheKey::new(&conv, b"abc"));
        let dir = path.parent().unwrap();
        std::fs::create_dir_all(dir).unwrap();
        // The name the next staging file gets ([`tempfile_in`]): write a
        // half-finished one there first, as a killed process would leave it.
        let stale = dir.join(format!(".tmp-{}-0", std::process::id()));
        TEMP_COUNTER.store(0, Ordering::Relaxed);
        std::fs::write(&stale, b"half-written").unwrap();
        let converted = cache.get_or_convert(&conv, b"abc").unwrap();
        assert_eq!(converted, path);
        assert_eq!(std::fs::read(&converted).unwrap(), b"ABC");
        assert!(!stale.exists(), "the stale staging file is reused");
    }

    #[test]
    fn version_bump_invalidates() {
        let _one = one_at_a_time();
        let v1 = Upper {
            version: 1,
            runs: AtomicUsize::new(0),
        };
        let v2 = Upper {
            version: 2,
            runs: AtomicUsize::new(0),
        };
        assert_ne!(CacheKey::new(&v1, b"x"), CacheKey::new(&v2, b"x"));
    }

    #[test]
    fn refuses_cache_inside_repo() {
        let _one = one_at_a_time();
        let inside = Path::new(env!("CARGO_MANIFEST_DIR")).join("cache");
        assert!(Cache::open(inside).is_err());
    }
}
