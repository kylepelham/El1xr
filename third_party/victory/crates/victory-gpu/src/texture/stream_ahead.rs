//! (worldstream, #26) `VICTORY_STREAM_AHEAD=1`: a placed texture is handed
//! to the prepare threads (`super::prepare`) as soon as a DirectStorage
//! request (or another kernel/runtime write the guards do not see) has
//! finished writing its bytes, not when the pass over the heap's texture
//! views first finds one of its slots due. Off (unset or 0): nothing here
//! runs, as before.
//!
//! Without it a texture streamed in waits for that pass twice: the written
//! range is kept until the next pass, which makes the slots under it due
//! two frames on (`descriptor::host` `PAGE_WRITE_AFTER`), and the pass then
//! hands the texture out and passes its slot over ("passed over waiting for
//! them", 13-45 slots a frame while driving in b5q) until a thread has made
//! the host copy and detiled it, a frame or more later. With the switch the
//! thread starts while the title is still busy with the request's
//! completion, and the pass finds the texture made ready.
//!
//! Where: `victory_hle`'s `dstorage` `into_destination` -> `xmem`
//! `pages_written` (or `one_look::done`, or a plain address) ->
//! `crate::watch::title_memory_written`, which ends with [`Arrived`]'s
//! drop, once the guards and the lazy pages have been told.
//!
//! The lookup: the textures the title placed in its memory and views
//! without a resource object (`crate::resource` `PLACED_TEXTURES`, the
//! title's streamed textures, kept for the run), by address
//! ([`walk`]). Which mips: a title that streams textures loads the small
//! ones first, and the smaller a mip the nearer the texture's start it lies
//! (`Layout::bytes_from`); so the bytes written from the texture's start on
//! are followed ([`cover`]) and the texture is handed out from the largest
//! mip they hold whole ([`first_whole`]). A write that starts at the
//! texture's start begins it again (the title loads it anew). A texture with
//! no whole mip from its start yet is left to the pass.
//!
//! The prepare threads make the host copy as they always do: in a 64 MiB
//! block with `VICTORY_TEXTURE_HEAPS=1`, a committed texture without. What
//! they make is checked at the list's submission like everything they make
//! (write watch, guards, hash), so a write after the detile is not lost.
//! Needs `VICTORY_TEXTURE_PREPARE_THREADS` (none: nothing is handed out).

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Mutex, OnceLock};

use super::prepare::{self, Ahead};
use crate::object::This;

/// Whether the switch is on (read once).
pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        let on = std::env::var_os("VICTORY_STREAM_AHEAD").is_some_and(|v| v == "1");
        if on {
            tracing::info!(
                threads = prepare::threads(),
                "textures handed to the prepare threads as their bytes arrive (VICTORY_STREAM_AHEAD=1)"
            );
        }
        on
    })
}

/// Since the last line ([`STATS`] by index).
pub(crate) const WRITES: usize = 0;
pub(crate) const UNDER: usize = 1;
pub(crate) const HANDED: usize = 2;
pub(crate) const HANDED_BYTES: usize = 3;
pub(crate) const PARTIAL: usize = 4;
pub(crate) const HELD: usize = 5;
pub(crate) const STAGED: usize = 6;
pub(crate) const FULL: usize = 7;
pub(crate) const NOT: usize = 8;
/// Moved up when a slot asked for them (`prepare`).
pub(crate) const PROMOTED: usize = 9;
/// Entries no slot waited for, taken by the pass, and let go untaken
/// (`prepare`; these also count `VICTORY_BAKE_STAGE_AHEAD`'s).
pub(crate) const TAKEN: usize = 10;
pub(crate) const UNTAKEN: usize = 11;
pub(crate) static STATS: [AtomicU64; 12] = [const { AtomicU64::new(0) }; 12];

/// Ends [`crate::watch::title_memory_written`]: `[at, at + len)` holds the
/// bytes of a finished write.
pub(crate) struct Arrived(pub(crate) usize, pub(crate) usize);

impl Drop for Arrived {
    fn drop(&mut self) {
        if on() {
            written(self.0 as u64, self.0.saturating_add(self.1) as u64);
        }
    }
}

/// Placed textures looked at for one write, at most.
const MOST: usize = 64;
/// Textures whose bytes are followed, at most (then forgotten all at once).
const FOLLOWED: usize = 1 << 16;

/// The bytes of a texture written from its start on, and the first mip it
/// was handed out from since its last load began.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Cover {
    bytes: u64,
    handed: Option<u32>,
}

static COVERS: Mutex<Option<HashMap<usize, Cover>>> = Mutex::new(None);

/// A write `[start, end)` has finished.
fn written(start: u64, end: u64) {
    STATS[WRITES].fetch_add(1, Relaxed);
    for (resource, at, desc) in crate::resource::placed_textures_under(start, end, MOST) {
        STATS[UNDER].fetch_add(1, Relaxed);
        look_at(resource, at, &desc, (start, end));
    }
    line();
}

fn look_at(resource: This, at: u64, desc: &[u32; 14], write: (u64, u64)) {
    let Some(size) = crate::resource::placed_bytes(desc) else {
        return;
    };
    let key = resource as usize;
    let now = {
        let mut covers = COVERS.lock().unwrap_or_else(|e| e.into_inner());
        let covers = covers.get_or_insert_with(HashMap::new);
        if covers.len() >= FOLLOWED {
            covers.clear();
        }
        let now = cover(covers.get(&key).copied(), (at, size), write);
        match now {
            Some(c) => covers.insert(key, c),
            None => covers.remove(&key),
        };
        now
    };
    let Some(now) = now else {
        STATS[PARTIAL].fetch_add(1, Relaxed);
        return;
    };
    // SAFETY: 56 bytes in the description's layout.
    let d = unsafe {
        desc.as_ptr()
            .cast::<crate::resource::ResourceDesc>()
            .read_unaligned()
    };
    let texture = crate::resource::texture_layout(&d).filter(|t| t.planes.len() == 1);
    let first = match &texture {
        Some(t) => first_whole(
            |k| t.planes[0].layout.bytes_from(k),
            d.mip_levels(),
            now.bytes,
        ),
        None => first_whole(|_| size, 1, now.bytes),
    };
    let Some(first) = first else {
        STATS[PARTIAL].fetch_add(1, Relaxed);
        return;
    };
    if now.handed.is_some_and(|h| h <= first) {
        STATS[HELD].fetch_add(1, Relaxed);
        return;
    }
    let handed = match prepare::hand_ahead(resource, first) {
        Ahead::Handed(bytes) => {
            STATS[HANDED].fetch_add(1, Relaxed);
            STATS[HANDED_BYTES].fetch_add(bytes, Relaxed);
            true
        }
        Ahead::Held => {
            STATS[HELD].fetch_add(1, Relaxed);
            true
        }
        Ahead::Staged => {
            STATS[STAGED].fetch_add(1, Relaxed);
            false
        }
        Ahead::Full => {
            STATS[FULL].fetch_add(1, Relaxed);
            false
        }
        Ahead::Not => {
            STATS[NOT].fetch_add(1, Relaxed);
            false
        }
    };
    if handed {
        let mut covers = COVERS.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(c) = covers.get_or_insert_with(HashMap::new).get_mut(&key) {
            c.handed = Some(c.handed.map_or(first, |h| h.min(first)));
        }
    }
}

/// The bytes of a texture of `size` bytes at `start` (`texture`) written
/// from its start on after a write `[ws, we)`, from what they were
/// (`prev`). A write from its start (or before) begins them again; one
/// that starts past them leaves them as they are; `None`: none yet.
fn cover(prev: Option<Cover>, texture: (u64, u64), write: (u64, u64)) -> Option<Cover> {
    let (start, size) = texture;
    let end = start.saturating_add(size);
    let (ws, we) = write;
    if we <= start || ws >= end {
        return prev;
    }
    let reach = we.min(end) - start;
    if ws <= start {
        return Some(Cover {
            bytes: reach,
            handed: None,
        });
    }
    let prev = prev?;
    if ws - start > prev.bytes {
        return Some(prev);
    }
    Some(Cover {
        bytes: prev.bytes.max(reach),
        handed: prev.handed,
    })
}

/// The largest mip (the smallest number) whose bytes and those of the
/// smaller ones lie within the first `covered` bytes of the texture
/// (`bytes_from(k)`: the bytes from the start that mip `k` and the smaller
/// ones take); `None` if not even the smallest does.
fn first_whole(bytes_from: impl Fn(u32) -> u64, mips: u32, covered: u64) -> Option<u32> {
    (0..mips.max(1)).find(|&k| bytes_from(k) <= covered)
}

/// The values of `map` (by address and description) whose memory meets
/// `[start, end)`, `most` at most, by `bytes` (a description's bytes).
/// Walks down from `end`; stops after 8 in a row end below `start` (a
/// texture lying over several smaller ones is the one case that can be
/// missed, and it is then left to the pass, as before).
pub(crate) fn walk<V: Copy>(
    map: &BTreeMap<(u64, [u32; 14]), V>,
    start: u64,
    end: u64,
    most: usize,
    bytes: impl Fn(&[u32; 14]) -> Option<u64>,
) -> Vec<(V, u64, [u32; 14])> {
    const MISSES: u32 = 8;
    let mut out = Vec::new();
    let mut misses = 0;
    for (&(at, desc), &value) in map.range(..(end, [0; 14])).rev() {
        let Some(len) = bytes(&desc) else {
            continue;
        };
        if at.saturating_add(len) <= start {
            misses += 1;
            if misses >= MISSES {
                break;
            }
            continue;
        }
        misses = 0;
        out.push((value, at, desc));
        if out.len() >= most {
            break;
        }
    }
    out
}

/// Every 5 s, a line of [`STATS`] (taken back to 0).
fn line() {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let now = crate::host::now_ms();
    let next = NEXT.load(Relaxed);
    if next == 0 {
        let _ = NEXT.compare_exchange(0, now + 5000, Relaxed, Relaxed);
        return;
    }
    if now < next
        || NEXT
            .compare_exchange(next, now + 5000, Relaxed, Relaxed)
            .is_err()
    {
        return;
    }
    let s = STATS.each_ref().map(|s| s.swap(0, Relaxed));
    tracing::info!(
        "stream ahead (VICTORY_STREAM_AHEAD=1) over 5 s: {} finished writes looked at, {} placed textures under them; \
         {} handed to the prepare threads ({} MB), {} without a whole mip from their start yet, \
         {} handed out or made ready already, {} staged already, {} line full, {} not handed (no threads, not filled, refused); \
         {} moved up when a slot asked, {} made ahead and taken by the pass, {} made ahead and let go untaken",
        s[WRITES],
        s[UNDER],
        s[HANDED],
        s[HANDED_BYTES] >> 20,
        s[PARTIAL],
        s[HELD],
        s[STAGED],
        s[FULL],
        s[NOT],
        s[PROMOTED],
        s[TAKEN],
        s[UNTAKEN],
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_are_followed_from_the_start_only() {
        let t = (1000, 400);
        // Nothing before the first write from its start.
        assert_eq!(cover(None, t, (1100, 1200)), None);
        let c = cover(None, t, (900, 1100)).unwrap();
        assert_eq!(
            c,
            Cover {
                bytes: 100,
                handed: None
            }
        );
        // Joined to what is there, not across a hole.
        let c = cover(Some(c), t, (1100, 1250)).unwrap();
        assert_eq!(c.bytes, 250);
        assert_eq!(cover(Some(c), t, (1300, 1400)).unwrap().bytes, 250);
        // Writes elsewhere change nothing; past the end it stops at the end.
        assert_eq!(cover(Some(c), t, (0, 1000)), Some(c));
        let handed = Cover {
            bytes: 250,
            handed: Some(2),
        };
        assert_eq!(
            cover(Some(handed), t, (1200, 9000)).unwrap(),
            Cover {
                bytes: 400,
                handed: Some(2)
            }
        );
        // A load from the start begins it again.
        assert_eq!(
            cover(Some(handed), t, (1000, 1050)).unwrap(),
            Cover {
                bytes: 50,
                handed: None
            }
        );
    }

    #[test]
    fn the_largest_mip_held_whole_from_the_start() {
        // Mips 0..4 of 4096, 1024, 256, 64, 16 bytes, smaller ones first.
        let from = |k: u32| [5456u64, 1360, 336, 80, 16][k as usize];
        assert_eq!(first_whole(from, 5, 10), None);
        assert_eq!(first_whole(from, 5, 16), Some(4));
        assert_eq!(first_whole(from, 5, 400), Some(2));
        assert_eq!(first_whole(from, 5, 5456), Some(0));
        // A layout held only whole.
        assert_eq!(first_whole(|_| 100, 3, 99), None);
        assert_eq!(first_whole(|_| 100, 3, 100), Some(0));
    }

    #[test]
    fn the_walk_finds_what_meets_the_range() {
        let desc = |n: u32| {
            let mut d = [0u32; 14];
            d[0] = n;
            d
        };
        let mut map = BTreeMap::new();
        for i in 0..20u32 {
            // 100 bytes each, every 100 bytes from 0.
            map.insert((u64::from(i) * 100, desc(100)), i);
        }
        // An alias at 500 of 300 bytes.
        map.insert((500, desc(300)), 99);
        let bytes = |d: &[u32; 14]| Some(u64::from(d[0]));
        let found: Vec<u32> = walk(&map, 650, 720, 64, bytes)
            .into_iter()
            .map(|f| f.0)
            .collect();
        assert_eq!(found, vec![7, 6, 99]);
        // A range of its own end is not met by the texture that starts there.
        let found: Vec<u32> = walk(&map, 250, 300, 64, bytes)
            .into_iter()
            .map(|f| f.0)
            .collect();
        assert_eq!(found, vec![2]);
        assert_eq!(walk(&map, 0, 2000, 3, bytes).len(), 3);
        assert!(walk(&map, 5000, 6000, 64, bytes).is_empty());
    }

    const CHILD: &str = "VICTORY_STREAM_AHEAD_CHILD";
    const TEST: &str =
        "texture::stream_ahead::tests::a_finished_write_hands_its_placed_texture_to_the_threads";

    /// End to end, with and without `VICTORY_TEXTURE_HEAPS`: a finished write
    /// over a placed texture hands it to the prepare threads, which make
    /// its host copy; a second write of the same bytes hands nothing more.
    #[test]
    fn a_finished_write_hands_its_placed_texture_to_the_threads() {
        if std::env::var_os(CHILD).is_none() {
            for heaps in ["0", "1"] {
                let output = std::process::Command::new(std::env::current_exe().unwrap())
                    .args([TEST, "--exact", "--nocapture", "--test-threads=1"])
                    .env(CHILD, "1")
                    .env("VICTORY_TEXTURE_PREPARE_THREADS", "1")
                    .env("VICTORY_STREAM_AHEAD", "1")
                    .env("VICTORY_TEXTURE_HEAPS", heaps)
                    .output()
                    .unwrap();
                print!("{}", String::from_utf8_lossy(&output.stdout));
                eprint!("{}", String::from_utf8_lossy(&output.stderr));
                assert!(
                    output.status.success(),
                    "stream ahead probe (heaps {heaps}) failed: {}",
                    output.status
                );
            }
            return;
        }
        assert!(on());
        if crate::host::host().is_none() {
            eprintln!("Skipped stream ahead probe: no D3D12 device");
            return;
        }
        const SIDE: u32 = 256;
        const RGBA8: u32 = 28;
        let words: [u32; 14] = [
            3,
            0,
            0,
            0,
            SIDE,
            0,
            SIDE,
            (1 << 16) | 1,
            RGBA8,
            1,
            0,
            0x105,
            0,
            0,
        ];
        let size =
            crate::resource::placed_bytes(&words).expect("a placed texture's bytes") as usize;
        // Title memory: 64 KiB aligned, as the title's pages are.
        let memory = Box::leak(vec![0x5au8; size + (64 << 10)].into_boxed_slice());
        let address = (memory.as_ptr() as u64).next_multiple_of(64 << 10);
        let resource = crate::resource::texture_at(address, &words).expect("a placed texture");
        assert!(!crate::resource::host_copy_made(resource));
        // Half of it: no whole mip from the start yet, nothing handed.
        written(address, address + size as u64 / 2);
        assert_eq!(prepare::held(resource, 0), prepare::Held::Nothing);
        assert_eq!(STATS[PARTIAL].load(Relaxed), 1);
        // The rest arrives: handed now.
        written(address + size as u64 / 2, address + size as u64);
        assert_eq!(
            STATS[HANDED].load(Relaxed),
            1,
            "handed as the write finished"
        );
        assert_ne!(prepare::held(resource, 0), prepare::Held::Nothing);
        assert!(prepare::wait_idle(std::time::Duration::from_secs(20)));
        assert_eq!(prepare::held(resource, 0), prepare::Held::Ready);
        assert!(
            crate::resource::host_copy_made(resource),
            "the thread made the host copy"
        );
        // The same bytes again (not from the start): held already.
        written(address + 64, address + size as u64);
        assert_eq!(STATS[HANDED].load(Relaxed), 1);
    }
}
