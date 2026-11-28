//! (worldstream, #26) `VICTORY_SHADER_PACK=1`: translated shaders are read
//! from a pack beside the shader cache (`<cache>/pack`, `victory_pipeline::Pack`)
//! before its loose files: one positional read from a handle kept open,
//! instead of a file look-up, open, read and close among about a million
//! files in 256 folders. What the loose files serve, or a translation
//! stores there, is put into the pack on a thread of its own, so the run
//! after reads it from the pack. The loose files stay as they are (the
//! switch off reads them as before). Restart.
//!
//! Microbenchmark (`victory-pipeline` `pack::tests`, debug build, 50,000
//! lookups): 10,000 entries of the real cache (42 KiB average) loose 60.5 us
//! each, pack 20.7 us each with the record's hash checked (2.9x); 16 KiB
//! made-up entries 53.9 against 14.4 us (3.8x).

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use victory_pipeline::{Cache, Pack};

/// A pack past this is started anew when it opens (stale translations of
/// older translators pile up in it; about 1.1 GB a translator).
const MOST: u64 = 4 << 30;

/// The pack, opened on the first look-up with the switch on.
pub(crate) fn pack(cache: &Cache) -> Option<&'static Pack> {
    static PACK: OnceLock<Option<Pack>> = OnceLock::new();
    let pack = PACK
        .get_or_init(|| {
            if std::env::var_os("VICTORY_SHADER_PACK").is_none_or(|v| v != "1") {
                return None;
            }
            let dir = cache.root().join("pack");
            match Pack::open(&dir, MOST) {
                Ok(pack) => {
                    let s = pack.stats();
                    tracing::info!(
                        dir = %dir.display(),
                        records = s.records_at_open,
                        mb = s.bytes >> 20,
                        open_ms = s.open_ms,
                        index_used = s.index_used,
                        torn_bytes = s.torn_bytes,
                        read_only = s.read_only,
                        started_anew = s.started_anew,
                        "shader pack: opened (VICTORY_SHADER_PACK=1)"
                    );
                    Some(pack)
                }
                Err(e) => {
                    tracing::warn!(error = %e, dir = %dir.display(), "shader pack: not opened; loose files only");
                    None
                }
            }
        })
        .as_ref();
    if let Some(p) = pack {
        line(p);
    }
    pack
}

/// Waits for what is queued to be in the pack (`victory warm`'s end).
pub(crate) fn sync() {
    if let Some(cache) = crate::shader_cache::cache()
        && let Some(p) = pack(cache)
    {
        p.sync();
        line_now(p);
    }
}

/// Every 5 s while look-ups come, the pack's counts so far.
fn line(p: &Pack) {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let now = crate::host::now_ms();
    let next = NEXT.load(Relaxed);
    if now < next
        || NEXT
            .compare_exchange(next, now + 5000, Relaxed, Relaxed)
            .is_err()
    {
        return;
    }
    if next != 0 {
        line_now(p);
    }
}

fn line_now(p: &Pack) {
    let s = p.stats();
    let lookups = s.hits + s.misses;
    tracing::info!(
        "shader pack: {} hits, {} misses (to the loose files), {} bad records; look-ups {:.1} ms ({:.1} us each); \
         {} put in ({} appended, {} MB, {} not: read-only, full or queue full), index written {} times; \
         now {} records ({} MB); opened in {} ms with {} records (index {}, {} torn bytes cut{}{})",
        s.hits,
        s.misses,
        s.bad,
        s.lookup_us as f64 / 1000.0,
        if lookups == 0 {
            0.0
        } else {
            s.lookup_us as f64 / lookups as f64
        },
        s.queued,
        s.appended,
        s.appended_bytes >> 20,
        s.dropped,
        s.index_writes,
        s.records,
        s.bytes >> 20,
        s.open_ms,
        s.records_at_open,
        if s.index_used { "used" } else { "rebuilt" },
        s.torn_bytes,
        if s.read_only {
            ", read-only: another process writes it"
        } else {
            ""
        },
        if s.started_anew { ", started anew" } else { "" },
    );
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicU64;

    use super::*;
    use crate::shader_cache::{cached, tests::shader};

    const CHILD: &str = "VICTORY_SHADER_PACK_CHILD";
    const TEST: &str = "shader_pack::tests::translations_come_back_from_the_pack";

    /// With the switch (a child process: it is read once): a translation
    /// stored in the loose files goes into the pack, and comes back from it
    /// with the loose files gone.
    #[test]
    fn translations_come_back_from_the_pack() {
        let Some(dir) = std::env::var_os(CHILD) else {
            let dir = tempfile::tempdir().unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([TEST, "--exact", "--test-threads=1"])
                .env(CHILD, dir.path())
                .env("VICTORY_SHADER_PACK", "1")
                .output()
                .unwrap();
            let out = String::from_utf8_lossy(&output.stdout);
            assert!(output.status.success() && out.contains("1 passed"), "{out}");
            assert!(dir.path().join("cache/pack/shaders.pack").is_file());
            return;
        };
        let root = std::path::PathBuf::from(dir).join("cache");
        let cache = Cache::open(&root).unwrap();
        let runs = AtomicU64::new(0);
        let translate = |_: &[u8]| {
            runs.fetch_add(1, Relaxed);
            Ok(shader())
        };
        let build = [3u8; 32];
        assert_eq!(cached(&cache, &build, b"container", translate), Ok(shader()));
        let p = pack(&cache).expect("the pack");
        p.sync();
        assert_eq!(p.stats().appended, 1, "the translation went in");
        // The loose files gone: from the pack, not translated again.
        for e in std::fs::read_dir(&root).unwrap().flatten() {
            if e.file_name() != "pack" {
                std::fs::remove_dir_all(e.path()).unwrap();
            }
        }
        assert_eq!(cached(&cache, &build, b"container", translate), Ok(shader()));
        assert_eq!(runs.load(Relaxed), 1);
        assert_eq!(p.stats().hits, 1);
        // Another build misses the pack and translates.
        assert_eq!(cached(&cache, &[4; 32], b"container", translate), Ok(shader()));
        assert_eq!(runs.load(Relaxed), 2);
    }
}
