//! A packed store beside the content-addressed [`crate::Cache`] (worldstream,
//! #26, `VICTORY_SHADER_PACK` in victory-gpu): one append-only pack file of
//! records and an index of where each lies, read once when the pack opens.
//! A hit is one positional read from a handle kept open, not a file open,
//! read and close in a folder of thousands (the shader cache holds about a
//! million loose files in 256 folders).
//!
//! Records are `magic, key (32), len (u32), payload, blake3(key, len,
//! payload) (32)`. The index (`shaders.idx`) names the pack length it
//! covers and is written whole to a staging file and renamed into place,
//! with a hash over it; a missing or broken index is rebuilt by reading the
//! pack. Records past the index (appended after it was last written) are
//! found by reading on from where it ends, up to the first record that does
//! not check (a torn append from a killed process), which the writer cuts
//! off. Every hit checks its record's hash again.
//!
//! One process writes (the one holding `shaders.lock`, opened with no
//! sharing; Windows only); others read what was there when they opened and
//! leave new entries to the loose files. Appends run on a thread of the
//! writer's own ([`Pack::put`] only queues). A pack grown past its most
//! bytes is started anew when the writer opens it (the old one kept as
//! `shaders.pack.old` until the next time), its entries coming back from the
//! loose files as they are hit.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Condvar, Mutex, RwLock, Weak};
use std::time::{Duration, Instant};

const RECORD: &[u8; 4] = b"VPR1";
const INDEX: &[u8; 4] = b"VPI1";
const HEAD: u64 = 4 + 32 + 4;
const TAIL: u64 = 32;
const INDEX_ENTRY: usize = 32 + 8 + 4;
/// Bytes queued for appending at most (more are not packed this time).
const QUEUED_MOST: u64 = 256 << 20;
/// The index is written again after this many appends, or when the queue
/// runs dry this long after the last write.
const INDEX_EVERY: u64 = 4096;
const INDEX_AFTER: Duration = Duration::from_secs(2);

type Key = [u8; 32];

/// What a pack did since it opened.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PackStats {
    /// Records found when it opened, the milliseconds that took, whether
    /// the index was there and right, the bytes of a torn tail cut off.
    pub records_at_open: u64,
    pub open_ms: u64,
    pub index_used: bool,
    pub torn_bytes: u64,
    /// Another process writes it; this one only reads.
    pub read_only: bool,
    /// Started anew when it opened (past its most bytes).
    pub started_anew: bool,
    pub hits: u64,
    pub misses: u64,
    /// Records whose hash did not check on a hit (then misses).
    pub bad: u64,
    pub lookup_us: u64,
    pub queued: u64,
    pub appended: u64,
    pub appended_bytes: u64,
    /// Not queued: the queue was full, the pack at its most, or read-only.
    pub dropped: u64,
    pub index_writes: u64,
    pub records: u64,
    pub bytes: u64,
}

/// A pack in `dir` (see the module).
#[derive(Clone)]
pub struct Pack {
    inner: Arc<Inner>,
}

struct Inner {
    /// `None`: read-only with no pack made yet.
    read: Option<File>,
    index: RwLock<HashMap<Key, (u64, u32)>>,
    writer: Option<Mutex<Writer>>,
    /// The queue, held by the writer's thread too (which holds the rest
    /// only while it appends, so a dropped pack lets go of its lock).
    shared: Arc<Shared>,
    most: u64,
    at_open: PackStats,
    counts: [AtomicU64; 9],
    thread: AtomicBool,
}

struct Writer {
    _lock: File,
    file: File,
    dir: PathBuf,
    end: u64,
    since_index: u64,
    last_index: Instant,
}

#[derive(Default)]
struct Shared {
    queue: Mutex<Queue>,
    bell: Condvar,
    idle: Condvar,
}

#[derive(Default)]
struct Queue {
    items: VecDeque<(Key, Vec<u8>)>,
    keys: HashSet<Key>,
    bytes: u64,
    busy: bool,
}

const HITS: usize = 0;
const MISSES: usize = 1;
const BAD: usize = 2;
const LOOKUP_US: usize = 3;
const QUEUED: usize = 4;
const APPENDED: usize = 5;
const APPENDED_BYTES: usize = 6;
const DROPPED: usize = 7;
const INDEX_WRITES: usize = 8;

fn pack_path(dir: &Path) -> PathBuf {
    dir.join("shaders.pack")
}

fn index_path(dir: &Path) -> PathBuf {
    dir.join("shaders.idx")
}

impl Pack {
    /// Opens (creating) the pack in `dir`, started anew past `most` bytes
    /// if this process writes it.
    pub fn open(dir: &Path, most: u64) -> std::io::Result<Pack> {
        let began = Instant::now();
        std::fs::create_dir_all(dir)?;
        let lock = writer_lock(dir);
        let mut stats = PackStats {
            read_only: lock.is_none(),
            ..PackStats::default()
        };
        let path = pack_path(dir);
        if lock.is_some() && std::fs::metadata(&path).is_ok_and(|m| m.len() > most) {
            let old = dir.join("shaders.pack.old");
            let _ = std::fs::remove_file(&old);
            std::fs::rename(&path, &old)?;
            let _ = std::fs::remove_file(index_path(dir));
            stats.started_anew = true;
        }
        let writer_file = match lock {
            Some(_) => Some(
                shared(
                    OpenOptions::new()
                        .read(true)
                        .write(true)
                        .create(true)
                        .truncate(false),
                )
                .open(&path)?,
            ),
            None => None,
        };
        let read = match shared(OpenOptions::new().read(true)).open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Read-only before any writer made it: an empty pack.
                return Ok(Pack::empty(most, stats, began));
            }
            Err(e) => return Err(e),
        };
        let len = read.metadata()?.len();
        let (mut index, covered) = match load_index(&index_path(dir), len) {
            Some(loaded) => {
                stats.index_used = true;
                loaded
            }
            None => (HashMap::new(), 0),
        };
        let end = scan(&read, covered, len, &mut index);
        stats.torn_bytes = len - end;
        let writer = match (lock, writer_file) {
            (Some(lock), Some(file)) => {
                if end < len {
                    file.set_len(end)?;
                }
                Some(Mutex::new(Writer {
                    _lock: lock,
                    file,
                    dir: dir.to_path_buf(),
                    end,
                    since_index: u64::from(end > covered),
                    last_index: Instant::now(),
                }))
            }
            _ => None,
        };
        stats.records_at_open = index.len() as u64;
        stats.open_ms = began.elapsed().as_millis() as u64;
        Ok(Pack {
            inner: Arc::new(Inner {
                read: Some(read),
                index: RwLock::new(index),
                writer,
                shared: Arc::default(),
                most,
                at_open: stats,
                counts: [const { AtomicU64::new(0) }; 9],
                thread: AtomicBool::new(false),
            }),
        })
    }

    fn empty(most: u64, mut stats: PackStats, began: Instant) -> Pack {
        stats.open_ms = began.elapsed().as_millis() as u64;
        Pack {
            inner: Arc::new(Inner {
                read: None,
                index: RwLock::new(HashMap::new()),
                writer: None,
                shared: Arc::default(),
                most,
                at_open: stats,
                counts: [const { AtomicU64::new(0) }; 9],
                thread: AtomicBool::new(false),
            }),
        }
    }

    /// The payload filed under `key`, if the pack holds it and it checks.
    pub fn get(&self, key: &Key) -> Option<Vec<u8>> {
        let began = Instant::now();
        let found = self.inner.read_record(key);
        let c = &self.inner.counts;
        c[LOOKUP_US].fetch_add(began.elapsed().as_micros() as u64, Relaxed);
        c[if found.is_some() { HITS } else { MISSES }].fetch_add(1, Relaxed);
        found
    }

    /// Files `payload` under `key` on the writer's thread (nothing if this
    /// process does not write the pack, it holds the key or the queue is
    /// full).
    pub fn put(&self, key: &Key, payload: Vec<u8>) {
        let inner = &self.inner;
        if inner.writer.is_none() || payload.len() > u32::MAX as usize {
            inner.counts[DROPPED].fetch_add(1, Relaxed);
            return;
        }
        if inner
            .index
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(key)
        {
            return;
        }
        let mut q = inner.shared.queue.lock().unwrap_or_else(|e| e.into_inner());
        if q.keys.contains(key) {
            return;
        }
        if q.bytes + payload.len() as u64 > QUEUED_MOST {
            inner.counts[DROPPED].fetch_add(1, Relaxed);
            return;
        }
        q.bytes += payload.len() as u64;
        q.keys.insert(*key);
        q.items.push_back((*key, payload));
        drop(q);
        inner.counts[QUEUED].fetch_add(1, Relaxed);
        if !inner.thread.swap(true, Relaxed) {
            let weak = Arc::downgrade(inner);
            let shared = Arc::clone(&inner.shared);
            let spawned = std::thread::Builder::new()
                .name("victory-shader-pack".into())
                .spawn(move || run(&shared, &weak));
            if spawned.is_err() {
                inner.thread.store(false, Relaxed);
            }
        }
        inner.shared.bell.notify_one();
    }

    /// Waits until everything queued is appended and the index written
    /// (tests, benches, and the end of `victory warm`).
    pub fn sync(&self) {
        let inner = &self.inner;
        let mut q = inner.shared.queue.lock().unwrap_or_else(|e| e.into_inner());
        while !q.items.is_empty() || q.busy {
            q = inner.shared.idle.wait(q).unwrap_or_else(|e| e.into_inner());
        }
        drop(q);
        if let Some(w) = &inner.writer {
            let mut w = w.lock().unwrap_or_else(|e| e.into_inner());
            if w.since_index > 0 {
                inner.write_index(&mut w);
            }
        }
    }

    pub fn stats(&self) -> PackStats {
        let inner = &self.inner;
        let c = inner.counts.each_ref().map(|c| c.load(Relaxed));
        let index = inner.index.read().unwrap_or_else(|e| e.into_inner());
        PackStats {
            hits: c[HITS],
            misses: c[MISSES],
            bad: c[BAD],
            lookup_us: c[LOOKUP_US],
            queued: c[QUEUED],
            appended: c[APPENDED],
            appended_bytes: c[APPENDED_BYTES],
            dropped: c[DROPPED],
            index_writes: c[INDEX_WRITES],
            records: index.len() as u64,
            bytes: index.values().map(|&(_, l)| u64::from(l)).sum(),
            ..inner.at_open
        }
    }
}

impl Inner {
    fn read_record(&self, key: &Key) -> Option<Vec<u8>> {
        let (at, len) = *self
            .index
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(key)?;
        let mut buf = vec![0u8; (HEAD + u64::from(len) + TAIL) as usize];
        let ok = self.read.as_ref().is_some_and(|f| read_at(f, &mut buf, at - HEAD).is_ok())
            && check(&buf).is_some_and(|(k, l)| k == *key && l == len);
        if !ok {
            self.counts[BAD].fetch_add(1, Relaxed);
            self.index
                .write()
                .unwrap_or_else(|e| e.into_inner())
                .remove(key);
            return None;
        }
        buf.truncate((HEAD + u64::from(len)) as usize);
        buf.drain(..HEAD as usize);
        Some(buf)
    }

    /// Appends one record (the writer's thread).
    fn append(&self, w: &mut Writer, key: &Key, payload: &[u8]) {
        if w.end + HEAD + payload.len() as u64 + TAIL > self.most {
            self.counts[DROPPED].fetch_add(1, Relaxed);
            return;
        }
        let record = record(key, payload);
        if write_at(&w.file, &record, w.end).is_err() {
            self.counts[DROPPED].fetch_add(1, Relaxed);
            return;
        }
        let at = w.end + HEAD;
        w.end += record.len() as u64;
        w.since_index += 1;
        self.index
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(*key, (at, payload.len() as u32));
        self.counts[APPENDED].fetch_add(1, Relaxed);
        self.counts[APPENDED_BYTES].fetch_add(payload.len() as u64, Relaxed);
    }

    fn write_index(&self, w: &mut Writer) {
        let entries: Vec<(Key, (u64, u32))> = self
            .index
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|&(_, &(at, len))| at + u64::from(len) + TAIL <= w.end)
            .map(|(k, v)| (*k, *v))
            .collect();
        let _ = w.file.sync_data();
        if store_index(&index_path(&w.dir), w.end, &entries).is_ok() {
            w.since_index = 0;
            w.last_index = Instant::now();
            self.counts[INDEX_WRITES].fetch_add(1, Relaxed);
        }
    }
}

/// The writer's thread: appends what is queued, writes the index now and
/// then; ends with the pack.
fn run(shared: &Shared, weak: &Weak<Inner>) {
    loop {
        let item = {
            let mut q = shared.queue.lock().unwrap_or_else(|e| e.into_inner());
            if q.items.is_empty() {
                q.busy = false;
                shared.idle.notify_all();
                if weak.strong_count() == 0 {
                    return;
                }
                q = shared
                    .bell
                    .wait_timeout(q, Duration::from_millis(250))
                    .unwrap_or_else(|e| e.into_inner())
                    .0;
            }
            let item = q.items.pop_front();
            q.busy = item.is_some();
            item
        };
        let Some(inner) = weak.upgrade() else {
            let mut q = shared.queue.lock().unwrap_or_else(|e| e.into_inner());
            q.busy = false;
            shared.idle.notify_all();
            return;
        };
        let Some(w) = &inner.writer else {
            return;
        };
        let mut w = w.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((key, payload)) = &item {
            inner.append(&mut w, key, payload);
        }
        let due = w.since_index >= INDEX_EVERY
            || (item.is_none() && w.since_index > 0 && w.last_index.elapsed() >= INDEX_AFTER);
        if due {
            inner.write_index(&mut w);
        }
        drop(w);
        drop(inner);
        if let Some((key, payload)) = item {
            let mut q = shared.queue.lock().unwrap_or_else(|e| e.into_inner());
            q.keys.remove(&key);
            q.bytes -= payload.len() as u64;
        }
    }
}

fn record(key: &Key, payload: &[u8]) -> Vec<u8> {
    let len = (payload.len() as u32).to_le_bytes();
    let mut out = Vec::with_capacity(payload.len() + (HEAD + TAIL) as usize);
    out.extend_from_slice(RECORD);
    out.extend_from_slice(key);
    out.extend_from_slice(&len);
    out.extend_from_slice(payload);
    let hash = record_hash(key, &len, payload);
    out.extend_from_slice(hash.as_bytes());
    out
}

fn record_hash(key: &Key, len: &[u8; 4], payload: &[u8]) -> blake3::Hash {
    let mut h = blake3::Hasher::new();
    h.update(key);
    h.update(len);
    h.update(payload);
    h.finalize()
}

/// A whole record's key and length, if it checks.
fn check(buf: &[u8]) -> Option<(Key, u32)> {
    if buf.len() < (HEAD + TAIL) as usize || &buf[..4] != RECORD {
        return None;
    }
    let key: Key = buf[4..36].try_into().ok()?;
    let len_bytes: [u8; 4] = buf[36..40].try_into().ok()?;
    let len = u32::from_le_bytes(len_bytes);
    let end = HEAD as usize + len as usize;
    if buf.len() != end + TAIL as usize {
        return None;
    }
    let hash = record_hash(&key, &len_bytes, &buf[HEAD as usize..end]);
    (hash.as_bytes()[..] == buf[end..]).then_some((key, len))
}

/// Reads records from `from` up to `len`, filing them in `index`, until one
/// does not check: where the good ones end.
fn scan(file: &File, from: u64, len: u64, index: &mut HashMap<Key, (u64, u32)>) -> u64 {
    let mut at = from;
    let mut head = [0u8; HEAD as usize];
    while at + HEAD + TAIL <= len {
        if read_at(file, &mut head, at).is_err() || &head[..4] != RECORD {
            break;
        }
        let size = u64::from(u32::from_le_bytes(
            head[36..40].try_into().unwrap_or_default(),
        ));
        let whole = HEAD + size + TAIL;
        if at + whole > len {
            break;
        }
        let mut buf = vec![0u8; whole as usize];
        if read_at(file, &mut buf, at).is_err() {
            break;
        }
        let Some((key, size)) = check(&buf) else {
            break;
        };
        index.insert(key, (at + HEAD, size));
        at += whole;
    }
    at
}

/// The index file's entries and the pack length they cover, if it checks
/// and covers no more than the pack's `len`.
fn load_index(path: &Path, len: u64) -> Option<(HashMap<Key, (u64, u32)>, u64)> {
    let bytes = std::fs::read(path).ok()?;
    if bytes.len() < 16 + 32 || &bytes[..4] != INDEX {
        return None;
    }
    let (body, hash) = bytes.split_at(bytes.len() - 32);
    if blake3::hash(body).as_bytes()[..] != *hash {
        return None;
    }
    let covered = u64::from_le_bytes(body[4..12].try_into().ok()?);
    let count = u32::from_le_bytes(body[12..16].try_into().ok()?) as usize;
    if covered > len || body.len() != 16 + count * INDEX_ENTRY {
        return None;
    }
    let mut index = HashMap::with_capacity(count);
    for e in body[16..].chunks_exact(INDEX_ENTRY) {
        let key: Key = e[..32].try_into().ok()?;
        let at = u64::from_le_bytes(e[32..40].try_into().ok()?);
        let size = u32::from_le_bytes(e[40..44].try_into().ok()?);
        if at + u64::from(size) + TAIL > covered {
            return None;
        }
        index.insert(key, (at, size));
    }
    Some((index, covered))
}

/// Writes the index whole to a staging file and renames it into place.
fn store_index(path: &Path, covered: u64, entries: &[(Key, (u64, u32))]) -> std::io::Result<()> {
    let mut body = Vec::with_capacity(16 + entries.len() * INDEX_ENTRY + 32);
    body.extend_from_slice(INDEX);
    body.extend_from_slice(&covered.to_le_bytes());
    body.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for (key, (at, size)) in entries {
        body.extend_from_slice(key);
        body.extend_from_slice(&at.to_le_bytes());
        body.extend_from_slice(&size.to_le_bytes());
    }
    let hash = blake3::hash(&body);
    body.extend_from_slice(hash.as_bytes());
    let staging = path.with_extension(format!("idx.tmp-{}", std::process::id()));
    let written = std::fs::write(&staging, &body).and_then(|()| std::fs::rename(&staging, path));
    if written.is_err() {
        let _ = std::fs::remove_file(&staging);
    }
    written
}

#[cfg(windows)]
fn shared(o: &mut OpenOptions) -> &mut OpenOptions {
    use std::os::windows::fs::OpenOptionsExt;
    // FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE: the writer
    // appends and renames while readers hold theirs.
    o.share_mode(1 | 2 | 4)
}

#[cfg(not(windows))]
fn shared(o: &mut OpenOptions) -> &mut OpenOptions {
    o
}

/// The writer's lock: `shaders.lock` opened with no sharing, held while the
/// pack is; `None` when another process holds it.
#[cfg(windows)]
fn writer_lock(dir: &Path) -> Option<File> {
    use std::os::windows::fs::OpenOptionsExt;
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .share_mode(0)
        .open(dir.join("shaders.lock"))
        .ok()
}

#[cfg(not(windows))]
fn writer_lock(_: &Path) -> Option<File> {
    None
}

#[cfg(windows)]
fn read_at(file: &File, buf: &mut [u8], mut at: u64) -> std::io::Result<()> {
    use std::os::windows::fs::FileExt;
    let mut done = 0;
    while done < buf.len() {
        let n = file.seek_read(&mut buf[done..], at)?;
        if n == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        done += n;
        at += n as u64;
    }
    Ok(())
}

#[cfg(windows)]
fn write_at(file: &File, buf: &[u8], mut at: u64) -> std::io::Result<()> {
    use std::os::windows::fs::FileExt;
    let mut done = 0;
    while done < buf.len() {
        let n = file.seek_write(&buf[done..], at)?;
        if n == 0 {
            return Err(std::io::ErrorKind::WriteZero.into());
        }
        done += n;
        at += n as u64;
    }
    Ok(())
}

#[cfg(unix)]
fn read_at(file: &File, buf: &mut [u8], at: u64) -> std::io::Result<()> {
    std::os::unix::fs::FileExt::read_exact_at(file, buf, at)
}

#[cfg(unix)]
fn write_at(file: &File, buf: &[u8], at: u64) -> std::io::Result<()> {
    std::os::unix::fs::FileExt::write_all_at(file, buf, at)
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    fn key(n: u32) -> Key {
        *blake3::hash(&n.to_le_bytes()).as_bytes()
    }

    fn payload(n: u32, len: usize) -> Vec<u8> {
        (0..len)
            .map(|i| (i as u32).wrapping_mul(31).wrapping_add(n) as u8)
            .collect()
    }

    #[test]
    fn puts_come_back_in_this_process_and_the_next() {
        let tmp = tempfile::tempdir().unwrap();
        let pack = Pack::open(tmp.path(), 1 << 30).unwrap();
        assert!(!pack.stats().read_only);
        for n in 0..100 {
            pack.put(&key(n), payload(n, 100 + n as usize));
        }
        pack.put(&key(0), payload(0, 100));
        pack.sync();
        assert_eq!(pack.get(&key(7)), Some(payload(7, 107)));
        assert_eq!(pack.get(&key(1000)), None);
        let s = pack.stats();
        assert_eq!((s.appended, s.records, s.hits, s.misses), (100, 100, 1, 1));
        drop(pack);
        let again = Pack::open(tmp.path(), 1 << 30).unwrap();
        let s = again.stats();
        assert!(s.index_used);
        assert_eq!((s.records_at_open, s.torn_bytes), (100, 0));
        assert_eq!(again.get(&key(99)), Some(payload(99, 199)));
    }

    /// A torn append (a killed writer) is cut off; records past the index
    /// are found by reading on; a broken index is rebuilt from the pack.
    #[test]
    fn torn_tails_are_cut_and_a_broken_index_is_rebuilt() {
        let tmp = tempfile::tempdir().unwrap();
        let pack = Pack::open(tmp.path(), 1 << 30).unwrap();
        for n in 0..10 {
            pack.put(&key(n), payload(n, 1000));
        }
        pack.sync();
        drop(pack);
        // Two records past the index, the second torn.
        let path = pack_path(tmp.path());
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.extend(record(&key(10), &payload(10, 1000)));
        let torn = record(&key(11), &payload(11, 1000));
        bytes.extend(&torn[..torn.len() - 5]);
        std::fs::write(&path, &bytes).unwrap();
        let pack = Pack::open(tmp.path(), 1 << 30).unwrap();
        let s = pack.stats();
        assert_eq!(s.records_at_open, 11);
        assert_eq!(s.torn_bytes, torn.len() as u64 - 5);
        assert_eq!(pack.get(&key(10)), Some(payload(10, 1000)));
        assert_eq!(pack.get(&key(11)), None);
        // Appends go where the good records end.
        pack.put(&key(12), payload(12, 50));
        pack.sync();
        drop(pack);
        // A broken index: rebuilt from the pack.
        let idx = index_path(tmp.path());
        let mut i = std::fs::read(&idx).unwrap();
        let last = i.len() - 1;
        i[last] ^= 1;
        std::fs::write(&idx, &i).unwrap();
        let pack = Pack::open(tmp.path(), 1 << 30).unwrap();
        let s = pack.stats();
        assert!(!s.index_used);
        assert_eq!((s.records_at_open, s.torn_bytes), (12, 0));
        assert_eq!(pack.get(&key(12)), Some(payload(12, 50)));
    }

    /// A record changed on disk fails its hash on the hit: a miss, and it
    /// leaves the index.
    #[test]
    fn a_changed_record_is_a_miss() {
        let tmp = tempfile::tempdir().unwrap();
        let pack = Pack::open(tmp.path(), 1 << 30).unwrap();
        pack.put(&key(1), payload(1, 64));
        pack.sync();
        let path = pack_path(tmp.path());
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[HEAD as usize + 3] ^= 0xff;
        let f = shared(OpenOptions::new().write(true)).open(&path).unwrap();
        write_at(&f, &bytes, 0).unwrap();
        assert_eq!(pack.get(&key(1)), None);
        assert_eq!(pack.stats().bad, 1);
        assert_eq!(pack.get(&key(1)), None);
        assert_eq!(pack.stats().bad, 1, "gone from the index");
    }

    /// A second opener while the first holds the lock reads what is there
    /// and writes nothing.
    #[test]
    fn a_second_process_reads_only() {
        let tmp = tempfile::tempdir().unwrap();
        let first = Pack::open(tmp.path(), 1 << 30).unwrap();
        first.put(&key(1), payload(1, 64));
        first.sync();
        let second = Pack::open(tmp.path(), 1 << 30).unwrap();
        assert!(second.stats().read_only);
        assert_eq!(second.get(&key(1)), Some(payload(1, 64)));
        second.put(&key(2), payload(2, 64));
        second.sync();
        assert_eq!(second.stats().dropped, 1);
        // The first keeps writing; the second does not see it until it
        // opens again.
        first.put(&key(3), payload(3, 64));
        first.sync();
        assert_eq!(second.get(&key(3)), None);
        drop(first);
        drop(second);
        let third = Pack::open(tmp.path(), 1 << 30).unwrap();
        assert!(!third.stats().read_only);
        assert_eq!(third.get(&key(3)), Some(payload(3, 64)));
    }

    /// Read-only before any pack exists: empty, no failure.
    #[test]
    fn read_only_with_no_pack_yet_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let _lock = writer_lock(tmp.path()).unwrap();
        let pack = Pack::open(tmp.path(), 1 << 30).unwrap();
        assert!(pack.stats().read_only);
        assert_eq!(pack.get(&key(1)), None);
    }

    /// Past its most bytes the writer starts it anew; within it, appends
    /// stop at the most.
    #[test]
    fn a_full_pack_stops_and_starts_anew() {
        let tmp = tempfile::tempdir().unwrap();
        let most = 3 * (HEAD + 1000 + TAIL) + 10;
        let pack = Pack::open(tmp.path(), most).unwrap();
        for n in 0..5 {
            pack.put(&key(n), payload(n, 1000));
        }
        pack.sync();
        let s = pack.stats();
        assert_eq!((s.appended, s.dropped), (3, 2));
        drop(pack);
        let smaller = Pack::open(tmp.path(), most / 2).unwrap();
        let s = smaller.stats();
        assert!(s.started_anew);
        assert_eq!(s.records_at_open, 0);
        assert!(tmp.path().join("shaders.pack.old").is_file());
    }

    /// `cargo test -p victory-pipeline pack_against_loose -- --ignored --nocapture`:
    /// 50,000 lookups from 10,000 entries of 16 KiB, the pack against loose
    /// files laid out as the cache lays them (256 folders).
    #[test]
    #[ignore = "benchmark"]
    fn pack_against_loose() {
        const ENTRIES: u32 = 10_000;
        const LOOKUPS: u32 = 50_000;
        const SIZE: usize = 16 << 10;
        let tmp = tempfile::tempdir().unwrap();
        let loose = tmp.path().join("loose");
        let pack = Pack::open(&tmp.path().join("pack"), 8 << 30).unwrap();
        for n in 0..ENTRIES {
            let hex = blake3::Hash::from(key(n)).to_hex();
            let dir = loose.join(&hex[..2]);
            std::fs::create_dir_all(&dir).unwrap();
            let p = payload(n, SIZE);
            std::fs::write(dir.join(&hex[2..]), &p).unwrap();
            pack.put(&key(n), p);
        }
        pack.sync();
        let order: Vec<u32> = (0..LOOKUPS)
            .map(|i| i.wrapping_mul(7919) % ENTRIES)
            .collect();
        let began = Instant::now();
        let mut bytes = 0usize;
        for &n in &order {
            let hex = blake3::Hash::from(key(n)).to_hex();
            let path = loose.join(&hex[..2]).join(&hex[2..]);
            assert!(path.is_file());
            bytes += std::fs::read(&path).unwrap().len();
        }
        let loose_s = began.elapsed().as_secs_f64();
        drop(pack);
        let began = Instant::now();
        let pack = Pack::open(&tmp.path().join("pack"), 8 << 30).unwrap();
        let open_s = began.elapsed().as_secs_f64();
        let began = Instant::now();
        for &n in &order {
            bytes += pack.get(&key(n)).unwrap().len();
        }
        let pack_s = began.elapsed().as_secs_f64();
        assert_eq!(bytes, 2 * LOOKUPS as usize * SIZE);
        println!(
            "shader pack bench: {LOOKUPS} lookups of {ENTRIES} entries x {} KiB: loose {:.0} ms ({:.1} us each), \
             pack {:.0} ms ({:.1} us each, hash checked) + open {:.1} ms; {:.1}x",
            SIZE >> 10,
            loose_s * 1e3,
            loose_s * 1e6 / f64::from(LOOKUPS),
            pack_s * 1e3,
            pack_s * 1e6 / f64::from(LOOKUPS),
            open_s * 1e3,
            loose_s / pack_s,
        );
    }

    /// `PACK_BENCH_LOOSE=<the shader cache folder> cargo test -p victory-pipeline
    /// real_cache_pack_against_loose -- --ignored --nocapture`: 50,000 lookups of
    /// 10,000 entries of the real cache (16 of its folders), read in place,
    /// against a pack of the same entries. Reads only from the cache.
    #[test]
    #[ignore = "benchmark"]
    fn real_cache_pack_against_loose() {
        const ENTRIES: usize = 10_000;
        const LOOKUPS: usize = 50_000;
        let Some(root) = std::env::var_os("PACK_BENCH_LOOSE").map(PathBuf::from) else {
            println!("shader pack bench (real cache): PACK_BENCH_LOOSE not set");
            return;
        };
        let mut files = Vec::new();
        for shard in 0..16u32 {
            let Ok(dir) = std::fs::read_dir(root.join(format!("{shard:02x}"))) else { continue };
            files.extend(
                dir.filter_map(Result::ok)
                    .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
                    .take(ENTRIES / 16)
                    .map(|e| e.path()),
            );
        }
        let tmp = tempfile::tempdir().unwrap();
        let pack = Pack::open(tmp.path(), 8 << 30).unwrap();
        let keys: Vec<Key> = files.iter().map(|f| *blake3::hash(f.to_string_lossy().as_bytes()).as_bytes()).collect();
        let mut total = 0u64;
        for (f, k) in files.iter().zip(&keys) {
            let b = std::fs::read(f).unwrap();
            total += b.len() as u64;
            pack.put(k, b);
            pack.sync();
        }
        drop(pack);
        let n = files.len();
        let order: Vec<usize> = (0..LOOKUPS).map(|i| i.wrapping_mul(7919) % n).collect();
        let began = Instant::now();
        for &i in &order {
            assert!(files[i].is_file());
            std::hint::black_box(std::fs::read(&files[i]).unwrap());
        }
        let loose_s = began.elapsed().as_secs_f64();
        let began = Instant::now();
        let pack = Pack::open(tmp.path(), 8 << 30).unwrap();
        let open_s = began.elapsed().as_secs_f64();
        let began = Instant::now();
        for &i in &order {
            std::hint::black_box(pack.get(&keys[i]).unwrap());
        }
        let pack_s = began.elapsed().as_secs_f64();
        println!(
            "shader pack bench (real cache): {LOOKUPS} lookups of {n} entries ({:.0} KiB average): loose {:.0} ms ({:.1} us each), \
             pack {:.0} ms ({:.1} us each, hash checked) + open {:.1} ms; {:.1}x",
            total as f64 / n as f64 / 1024.0,
            loose_s * 1e3,
            loose_s * 1e6 / LOOKUPS as f64,
            pack_s * 1e3,
            pack_s * 1e6 / LOOKUPS as f64,
            open_s * 1e3,
            loose_s / pack_s,
        );
    }
}
