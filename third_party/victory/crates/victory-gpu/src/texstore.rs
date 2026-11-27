//! A store of the larger levels of textures the title streams
//! (`VICTORY_TEXTURE_STORE=<folder>`; off).
//!
//! The title streams a texture's levels smallest first, and views it from
//! the first level it holds: a new texture is blurred until its larger
//! levels have been read (its streaming, and ours of its files) and shown,
//! seconds later, and again each time it is streamed out and back in.
//!
//! Here, once a texture has been uploaded whole (all levels, from level 0),
//! its levels as the host lays them out are kept, in memory and in the
//! folder, under keys made of its shape and of the bytes of its smaller
//! levels: for each level `k` from which on its levels fill at least
//! `MIN_PREFIX` bytes, the digest of the memory those levels take (the
//! title lays a texture's smaller levels out first, so that is the start of
//! its memory). When a texture of that shape is later first viewed from
//! level `k` with those very bytes there, in this run or a later one, its
//! host copy is made whole at once: levels from `k` on from title memory as
//! always, the larger ones from the store, and its views show them all.
//!
//! What it assumes: two textures of one shape whose smaller levels hold the
//! same bytes (at least `MIN_PREFIX` of them) have the same larger levels.
//! A texture shown wrong would be one of two such. Files are never trimmed.

use std::collections::{HashMap, HashSet};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex, OnceLock, mpsc};

/// Least bytes of the smaller levels a key is made of.
pub(crate) const MIN_PREFIX: u64 = 32 << 10;
/// Bytes of levels kept in memory at most (the rest is read from the folder
/// when asked).
const KEPT: u64 = 4 << 30;
/// Maximum whole textures queued for background indexing/writing so concurrent
/// prepare threads cannot bloat RAM if disk I/O lags behind.
const MAX_QUEUED_JOBS: usize = 64;

/// A texture's shape: width, height, format, levels.
pub(crate) type Shape = [u32; 4];

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct Key {
    shape: Shape,
    level: u32,
    digest: u128,
}

/// Levels `0..levels` of a texture as the host lays them out, by the
/// digest of the whole texture they came from.
#[derive(Clone, Copy)]
struct Entry {
    blob: u128,
    levels: u32,
}

struct Store {
    folder: PathBuf,
    index: Mutex<HashMap<Key, Entry>>,
    kept: Mutex<HashMap<u128, Arc<Vec<u8>>>>,
    kept_bytes: AtomicU64,
    jobs: Mutex<mpsc::SyncSender<Job>>,
    /// Contents given to it this run (digests of whole textures).
    seen: Mutex<HashSet<u128>>,
}

/// Since the start: entries found at startup, textures kept, keys asked,
/// found, blobs read from the folder, bytes written.
pub(crate) static STATS: [AtomicU64; 6] = [const { AtomicU64::new(0) }; 6];

/// Host copies whose larger levels a list copied from the store.
pub(crate) static PREFILLED: AtomicU64 = AtomicU64::new(0);

/// 128-bit digest of `bytes` (BLAKE3 prefix, deterministic across runs).
pub(crate) fn digest(bytes: &[u8]) -> u128 {
    let hash = blake3::hash(bytes);
    let mut head = [0u8; 16];
    head.copy_from_slice(&hash.as_bytes()[..16]);
    u128::from_le_bytes(head)
}

/// 128-bit digest from an already-computed `blake3::Hash`.
pub(crate) fn digest_hash(hash: blake3::Hash) -> u128 {
    let mut head = [0u8; 16];
    head.copy_from_slice(&hash.as_bytes()[..16]);
    u128::from_le_bytes(head)
}

fn line_of(key: &Key, entry: &Entry) -> String {
    format!(
        "{} {} {} {} {} {:032x} {:032x} {}",
        key.shape[0],
        key.shape[1],
        key.shape[2],
        key.shape[3],
        key.level,
        key.digest,
        entry.blob,
        entry.levels
    )
}

fn parse_line(line: &str) -> Option<(Key, Entry)> {
    let mut p = line.split_whitespace();
    let mut n = || p.next();
    let shape = [
        n()?.parse().ok()?,
        n()?.parse().ok()?,
        n()?.parse().ok()?,
        n()?.parse().ok()?,
    ];
    let level = n()?.parse().ok()?;
    let digest = u128::from_str_radix(n()?, 16).ok()?;
    let blob = u128::from_str_radix(n()?, 16).ok()?;
    let levels = n()?.parse().ok()?;
    Some((
        Key {
            shape,
            level,
            digest,
        },
        Entry { blob, levels },
    ))
}

fn load_index(folder: &Path) -> HashMap<Key, Entry> {
    let mut index = HashMap::new();
    if let Ok(text) = std::fs::read_to_string(folder.join("index.txt")) {
        for (key, entry) in text.lines().filter_map(parse_line) {
            // (A blob whose file is gone is no entry when read.)
            index.insert(key, entry);
        }
    }
    index
}

fn write_blob(
    folder: &Path,
    index: &mut std::fs::File,
    blob: u128,
    bytes: &[u8],
    keys: &[(Key, Entry)],
) {
    let path = folder.join(format!("{blob:032x}.lv"));
    if !path.exists() {
        let part = folder.join(format!("{blob:032x}.part"));
        if std::fs::write(&part, bytes).is_err() || std::fs::rename(&part, &path).is_err() {
            return;
        }
        STATS[5].fetch_add(bytes.len() as u64, Relaxed);
    }
    let text: String = keys.iter().map(|(k, e)| line_of(k, e) + "\n").collect();
    let _ = index.write_all(text.as_bytes());
    let _ = index.flush();
}

fn store() -> Option<&'static Store> {
    static STORE: OnceLock<Option<Store>> = OnceLock::new();
    STORE
        .get_or_init(|| {
            let folder = PathBuf::from(
                std::env::var_os("VICTORY_TEXTURE_STORE").filter(|v| !v.is_empty() && v != "0")?,
            );
            std::fs::create_dir_all(&folder).ok()?;
            let index = load_index(&folder);
            STATS[0].store(index.len() as u64, Relaxed);
            tracing::info!(
                folder = %folder.display(),
                keys = index.len(),
                "texture store: larger levels kept from earlier runs (VICTORY_TEXTURE_STORE)"
            );
            let (send, receive) = mpsc::sync_channel::<Job>(MAX_QUEUED_JOBS);
            let to = folder.clone();
            std::thread::Builder::new()
                .name("texture store".into())
                .spawn(move || {
                    let Ok(mut index) = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(to.join("index.txt"))
                    else {
                        return;
                    };
                    for job in receive {
                        // (The store is made before this thread's first job.)
                        let Some(store) = store() else {
                            continue;
                        };
                        let Some((blob, bytes, keys)) = kept(store, job) else {
                            continue;
                        };
                        write_blob(&to, &mut index, blob, &bytes[..], &keys);
                    }
                })
                .ok()?;
            Some(Store {
                folder,
                index: Mutex::new(index),
                kept: Mutex::new(HashMap::new()),
                kept_bytes: AtomicU64::new(0),
                jobs: Mutex::new(send),
                seen: Mutex::new(HashSet::new()),
            })
        })
        .as_ref()
}

/// Whether the store is on.
pub(crate) fn on() -> bool {
    store().is_some()
}

/// The levels a key can be made from: those from which on the levels fill
/// at least [`MIN_PREFIX`] bytes and are the start of the memory, with
/// those bytes (`level`, bytes). Not level 0: that is the whole texture.
pub(crate) fn key_levels(layout: &victory_formats::texture::Layout, mips: u32) -> Vec<(u32, u64)> {
    if layout.slices != 1 {
        return Vec::new();
    }
    (1..mips)
        .filter(|&k| crate::resource::read_from(layout, k) == k)
        .map(|k| (k, layout.bytes_from(k)))
        .filter(|&(_, bytes)| bytes >= MIN_PREFIX && bytes < layout.size)
        .collect()
}

/// A texture of `shape` was read whole: `tiled` is its memory (all levels),
/// `parts` where the host lays out each of its levels (offset, row pitch,
/// rows). Detiled and kept on the store's thread, unless that content is
/// known.
pub(crate) fn keep(
    shape: Shape,
    layout: &victory_formats::texture::Layout,
    tiled: &[u8],
    parts: Vec<(u64, u32, u32)>,
    precomputed_blob: Option<u128>,
) {
    let Some(store) = store() else {
        return;
    };
    let blob = precomputed_blob.unwrap_or_else(|| digest(tiled));
    if !store
        .seen
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(blob)
    {
        return;
    }
    let job = Job {
        shape,
        layout: layout.clone(),
        tiled: tiled.to_vec(),
        parts,
        blob,
    };
    enqueue_job(store, job);
}

fn enqueue_job(store: &Store, job: Job) -> bool {
    let blob = job.blob;
    let sent = store
        .jobs
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .try_send(job)
        .is_ok();
    if !sent {
        // Queue is full (or worker exited): allow a later upload to retry.
        store
            .seen
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&blob);
    }
    sent
}

/// Inserts `bytes` under `blob` into `store.kept` at most once while staying
/// within [`KEPT`], avoiding double-counting `kept_bytes` when several
/// prepare threads miss the cache for the same blob concurrently.
fn cache_kept(store: &Store, blob: u128, bytes: &Arc<Vec<u8>>) -> bool {
    let len = bytes.len() as u64;
    let mut kept = store.kept.lock().unwrap_or_else(|e| e.into_inner());
    if kept.contains_key(&blob) || store.kept_bytes.load(Relaxed) + len > KEPT {
        return false;
    }
    kept.insert(blob, bytes.clone());
    store.kept_bytes.fetch_add(len, Relaxed);
    true
}

/// A texture to keep ([`keep`]).
struct Job {
    shape: Shape,
    layout: victory_formats::texture::Layout,
    tiled: Vec<u8>,
    parts: Vec<(u64, u32, u32)>,
    blob: u128,
}

/// [`keep`]'s work, on the store's thread: the levels detiled as the host
/// lays them out, the keys, memory, and what goes to the folder.
fn kept(store: &Store, job: Job) -> Option<(u128, Arc<Vec<u8>>, Vec<(Key, Entry)>)> {
    let Job {
        shape,
        layout,
        tiled,
        parts,
        blob,
    } = job;
    let keys = key_levels(&layout, shape[3]);
    let &(top, _) = keys.iter().max_by_key(|(k, _)| *k)?;
    let end = parts.get(top as usize)?.0 as usize;
    let made: Vec<Key> = keys
        .iter()
        .filter(|&&(_, bytes)| bytes as usize <= tiled.len())
        .map(|&(level, bytes)| Key {
            shape,
            level,
            digest: digest(&tiled[..bytes as usize]),
        })
        .collect();
    if made.is_empty() {
        return None;
    }
    {
        let index = store.index.lock().unwrap_or_else(|e| e.into_inner());
        if made.iter().all(|k| index.contains_key(k)) {
            return None;
        }
    }
    // Levels `0..top`, the ones a key can stand for.
    let mut levels = vec![0u8; end];
    for (i, &(at, pitch, rows)) in parts.iter().take(top as usize).enumerate() {
        let (at, pitch, rows) = (at as usize, pitch as usize, rows as usize);
        let len = (pitch * rows).min(end.saturating_sub(at));
        layout
            .detile(&tiled, i as u32, 0, &mut levels[at..at + len], pitch, len)
            .ok()?;
    }
    let bytes = Arc::new(levels);
    let entries: Vec<(Key, Entry)> = made
        .into_iter()
        .map(|k| {
            (
                k,
                Entry {
                    blob,
                    levels: k.level,
                },
            )
        })
        .collect();
    {
        let mut index = store.index.lock().unwrap_or_else(|e| e.into_inner());
        for (k, e) in &entries {
            index.insert(*k, *e);
        }
    }
    cache_kept(store, blob, &bytes);
    STATS[1].fetch_add(1, Relaxed);
    Some((blob, bytes, entries))
}

/// The larger levels of a texture of `shape` first viewed from `level`,
/// whose memory from that level on holds `smaller`: levels `0..level` in
/// the host's layout, if the store has them.
pub(crate) fn find(shape: Shape, level: u32, smaller: &[u8]) -> Option<Arc<Vec<u8>>> {
    find_in(store()?, shape, level, smaller)
}

fn find_in(store: &Store, shape: Shape, level: u32, smaller: &[u8]) -> Option<Arc<Vec<u8>>> {
    STATS[2].fetch_add(1, Relaxed);
    let key = Key {
        shape,
        level,
        digest: digest(smaller),
    };
    let entry = *store
        .index
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&key)?;
    if entry.levels != level {
        return None;
    }
    let kept = store
        .kept
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&entry.blob)
        .cloned();
    let bytes = match kept {
        Some(bytes) => bytes,
        None => {
            let bytes =
                Arc::new(std::fs::read(store.folder.join(format!("{:032x}.lv", entry.blob))).ok()?);
            STATS[4].fetch_add(1, Relaxed);
            cache_kept(store, entry.blob, &bytes);
            bytes
        }
    };
    STATS[3].fetch_add(1, Relaxed);
    Some(bytes)
}

/// The line for the memory lines.
pub(crate) fn line() -> Option<String> {
    let store = store()?;
    let [at_start, kept, asked, found, read, written] =
        [0, 1, 2, 3, 4, 5].map(|i| STATS[i].load(Relaxed));
    Some(format!(
        "texture store (VICTORY_TEXTURE_STORE), since the start: {at_start} keys from earlier runs, {kept} textures kept \
         ({} MB written, {} MB held in memory), {asked} first views from a smaller level asked, {found} found ({read} read \
         from the folder), {} host copies made whole from it",
        written >> 20,
        store.kept_bytes.load(Relaxed) >> 20,
        PREFILLED.load(Relaxed)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use victory_formats::texture::{Dimension, Swizzle, Texture, TextureDesc};

    #[test]
    fn index_lines_read_back() {
        let key = Key {
            shape: [1024, 512, 71, 11],
            level: 3,
            digest: 0x0123_4567_89ab_cdef_0011_2233_4455_6677,
        };
        let entry = Entry {
            blob: 0xffee_ddcc_bbaa_9988_7766_5544_3322_1100,
            levels: 3,
        };
        let (k, e) = parse_line(&line_of(&key, &entry)).expect("a line");
        assert_eq!(k, key);
        assert_eq!((e.blob, e.levels), (entry.blob, entry.levels));
        assert!(parse_line("1 2 3").is_none());
    }

    #[test]
    fn key_levels_and_disk_round_trip() {
        let desc = TextureDesc {
            dimension: Dimension::Tex2d,
            width: 1024,
            height: 1024,
            depth_or_array: 1,
            mips: 9,
            format: 71, // BC1_UNORM
            samples: 1,
            swizzle: Swizzle::SW_4KB_S,
        };
        let layout = Texture::new(&desc).unwrap().planes[0].layout.clone();
        let levels = key_levels(&layout, 9);
        assert!(
            !levels.is_empty(),
            "1024x1024 BC1 should produce prefix key levels"
        );
        assert!(
            levels
                .iter()
                .all(|&(k, bytes)| k >= 1 && bytes >= MIN_PREFIX && bytes < layout.size)
        );

        // Build synthetic tiled data and host subresource layout for mips 0..9.
        let tiled: Vec<u8> = (0..layout.size as usize)
            .map(|i| ((i.wrapping_mul(31) ^ (i >> 7)) & 0xff) as u8)
            .collect();
        let mut parts = Vec::new();
        let mut offset = 0u64;
        for mip in 0..9u32 {
            let w = (1024u32 >> mip).max(1);
            let h = (1024u32 >> mip).max(1);
            let blocks_w = w.div_ceil(4);
            let blocks_h = h.div_ceil(4);
            let pitch = (blocks_w * 8).next_multiple_of(256);
            parts.push((offset, pitch, blocks_h));
            offset = (offset + u64::from(pitch) * u64::from(blocks_h)).next_multiple_of(512);
        }

        let dir =
            std::env::temp_dir().join(format!("victory-texstore-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let (tx, _rx) = mpsc::sync_channel(MAX_QUEUED_JOBS);
        let st = Store {
            folder: dir.clone(),
            index: Mutex::new(HashMap::new()),
            kept: Mutex::new(HashMap::new()),
            kept_bytes: AtomicU64::new(0),
            jobs: Mutex::new(tx),
            seen: Mutex::new(HashSet::new()),
        };

        let shape = [1024, 1024, 71, 9];
        let blob = digest(&tiled);
        let (written_blob, bytes, entries) = kept(
            &st,
            Job {
                shape,
                layout: layout.clone(),
                tiled: tiled.clone(),
                parts: parts.clone(),
                blob,
            },
        )
        .expect("kept should detile and index texture");
        assert_eq!(written_blob, blob);
        assert!(!entries.is_empty());

        let mut index_file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("index.txt"))
            .unwrap();
        write_blob(&dir, &mut index_file, written_blob, &bytes, &entries);
        drop(index_file);

        // Reload a fresh store from disk (empty RAM cache) and find by prefix.
        let (tx2, _rx2) = mpsc::sync_channel(MAX_QUEUED_JOBS);
        let reloaded = Store {
            folder: dir.clone(),
            index: Mutex::new(load_index(&dir)),
            kept: Mutex::new(HashMap::new()),
            kept_bytes: AtomicU64::new(0),
            jobs: Mutex::new(tx2),
            seen: Mutex::new(HashSet::new()),
        };
        let &(level, prefix_len) = levels.first().unwrap();
        let found = find_in(&reloaded, shape, level, &tiled[..prefix_len as usize])
            .expect("reloaded store should find texture on disk");
        assert_eq!(&*found, &*bytes);

        // Mismatched prefix bytes must not hit.
        let mut altered = tiled[..prefix_len as usize].to_vec();
        altered[0] ^= 0xff;
        assert!(find_in(&reloaded, shape, level, &altered).is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cache_kept_deduplicates_bytes_and_full_queue_releases_seen() {
        let desc = TextureDesc {
            dimension: Dimension::Tex2d,
            width: 64,
            height: 64,
            depth_or_array: 1,
            mips: 1,
            format: 71,
            samples: 1,
            swizzle: Swizzle::SW_4KB_S,
        };
        let layout = Texture::new(&desc).unwrap().planes[0].layout.clone();
        let (tx, _rx) = mpsc::sync_channel(1);
        let st = Store {
            folder: PathBuf::new(),
            index: Mutex::new(HashMap::new()),
            kept: Mutex::new(HashMap::new()),
            kept_bytes: AtomicU64::new(0),
            jobs: Mutex::new(tx),
            seen: Mutex::new(HashSet::new()),
        };

        // Inserting the same blob twice only counts its bytes once.
        let bytes = Arc::new(vec![7u8; 4096]);
        assert!(cache_kept(&st, 42, &bytes));
        assert!(!cache_kept(&st, 42, &bytes));
        assert_eq!(st.kept_bytes.load(Relaxed), 4096);

        // Filling the bounded channel rejects overflow without blocking and clears `seen`.
        let make_job = |blob| Job {
            shape: [64, 64, 71, 1],
            layout: layout.clone(),
            tiled: Vec::new(),
            parts: Vec::new(),
            blob,
        };
        st.seen.lock().unwrap().insert(1);
        assert!(enqueue_job(&st, make_job(1)));
        assert!(st.seen.lock().unwrap().contains(&1));

        st.seen.lock().unwrap().insert(2);
        assert!(!enqueue_job(&st, make_job(2)));
        assert!(
            !st.seen.lock().unwrap().contains(&2),
            "rejected job must clear seen so a later upload can store it"
        );
    }
}
