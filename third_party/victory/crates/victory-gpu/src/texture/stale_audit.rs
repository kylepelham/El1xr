//! (racefix) A watch over the host copies of textures drawn now: is what
//! their staging buffer was last detiled from still what title memory
//! holds? (`VICTORY_STALE_AUDIT=1`: count and log; `2`: also repair;
//! off unless set.)
//!
//! Lucia's diner outfit shows another outfit's texture in about half the
//! runs (batch 0-2: grey hoodie drawn black, red, or as the prison top; the
//! prison top drawn as a dark jacket with red piping). A whole, coherent
//! other texture points at a streamed texture placed where another of the
//! same description lay (one placed resource, one host copy:
//! `resource.rs` `texture_at`) whose new bytes never reached the host
//! copy: every path that should notice (guards, page writes, remaps, the
//! hash turn) said nothing changed. The logs cannot tell which.
//!
//! A thread of its own looks, every half second, at the staging buffers a
//! list used in the last 1.5 s (biggest first, at most
//! [`ROUND_BYTES`] a round): it reads the texture's memory through the
//! kernel (unmapped pages read as zeros, as the refresh reads them) and
//! hashes it. A texture whose memory hash differs from the hash its staging
//! buffer was detiled from, both unchanged for [`CONFIRM_MS`], is stale on
//! the screen: logged with its shape and address. With `2` its next refresh
//! is made to read it (`last` forgotten, hashed at every check for a while)
//! and its slots are made due as by a page write.
//!
//! Only textures whose refresh keeps a hash (view memory: guarded or
//! hashed, the streamed textures) are looked at; write-watched ones keep
//! none.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use super::{STAGING, Staging};

/// Bytes of title memory read in one round.
const ROUND_BYTES: u64 = 128 << 20;
/// Between rounds.
const ROUND_MS: u64 = 500;
/// How long a mismatch must hold, both hashes unchanged, to count.
const CONFIRM_MS: u64 = 2000;
/// Textures smaller than this are not looked at.
const LEAST_BYTES: usize = 64 << 10;
/// A staging buffer a list used this recently is drawn now.
const USED_MS: u64 = 1500;

/// 0 off, 1 count and log, 2 also repair.
pub(super) fn mode() -> u8 {
    static MODE: std::sync::OnceLock<u8> = std::sync::OnceLock::new();
    *MODE.get_or_init(|| parse(std::env::var("VICTORY_STALE_AUDIT").ok().as_deref()))
}

fn parse(value: Option<&str>) -> u8 {
    match value.map(str::trim) {
        Some("1") => 1,
        Some("2") => 2,
        _ => 0,
    }
}

/// Since the start: rounds, textures read, bytes read, mismatches seen,
/// confirmed stale, repairs, confirmed stale again after a repair.
static STATS: [AtomicU64; 7] = [const { AtomicU64::new(0) }; 7];

/// Called once a second (`super::let_go_idle`): starts the thread once.
pub(super) fn tick() {
    if mode() == 0 {
        return;
    }
    static STARTED: std::sync::Once = std::sync::Once::new();
    STARTED.call_once(|| {
        let started = std::thread::Builder::new()
            .name("victory stale audit".into())
            .spawn(run);
        match started {
            Ok(_) => tracing::info!(
                mode = mode(),
                round_mb = ROUND_BYTES >> 20,
                confirm_ms = CONFIRM_MS,
                "stale audit: host copies of textures drawn now are compared with title memory (VICTORY_STALE_AUDIT=1: log, 2: also repair)"
            ),
            Err(e) => tracing::warn!(error = %e, "stale audit: no thread"),
        }
    });
}

/// What one look found for one texture.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Verdict {
    /// Memory and staging agree (or nothing to compare).
    Same,
    /// They differ, not long enough yet.
    Seen,
    /// They differ and have for [`CONFIRM_MS`]: said once per pair of hashes.
    Stale,
    /// Still stale, said already.
    Known,
}

/// A mismatch being watched: the memory's hash, the staging's, since when,
/// and whether it was said.
struct Suspect {
    memory: [u8; 32],
    staged: [u8; 32],
    since: u64,
    said: bool,
}

/// The mismatches being watched, by texture.
#[derive(Default)]
pub(super) struct Book {
    suspects: HashMap<usize, Suspect>,
}

impl Book {
    /// Texture `key` read now (`now_ms`): its memory hashes to `memory`, its
    /// staging buffer was detiled from `staged`.
    pub(super) fn observe(
        &mut self,
        key: usize,
        memory: [u8; 32],
        staged: [u8; 32],
        now_ms: u64,
    ) -> Verdict {
        if memory == staged {
            self.suspects.remove(&key);
            return Verdict::Same;
        }
        let s = self.suspects.entry(key).or_insert(Suspect {
            memory,
            staged,
            since: now_ms,
            said: false,
        });
        if s.memory != memory || s.staged != staged {
            *s = Suspect {
                memory,
                staged,
                since: now_ms,
                said: false,
            };
            return Verdict::Seen;
        }
        if now_ms.saturating_sub(s.since) < CONFIRM_MS {
            return Verdict::Seen;
        }
        if s.said {
            return Verdict::Known;
        }
        s.said = true;
        Verdict::Stale
    }

    /// Forgets textures not looked at in this round's set.
    pub(super) fn keep_only(&mut self, alive: &std::collections::HashSet<usize>) {
        self.suspects.retain(|k, _| alive.contains(k));
    }
}

fn run() {
    crate::topology::pin_background("victory-stale-audit");
    let mut book = Book::default();
    let mut read = Vec::<u8>::new();
    let mut repaired: HashMap<usize, u64> = HashMap::new();
    let mut last_line = crate::host::now_ms();
    loop {
        std::thread::sleep(std::time::Duration::from_millis(ROUND_MS));
        let round = candidates();
        STATS[0].fetch_add(1, Relaxed);
        let alive: std::collections::HashSet<usize> = round.iter().map(|(k, _)| *k).collect();
        book.keep_only(&alive);
        repaired.retain(|k, _| alive.contains(k));
        let mut bytes = 0u64;
        for (key, staging) in round {
            if bytes >= ROUND_BYTES {
                break;
            }
            let Some(staged) = staged_hash(&staging) else {
                continue;
            };
            let size = staging.size;
            if read.len() < size {
                read.resize(size, 0);
            }
            crate::host::copy_mapped(read.as_mut_ptr(), staging.memory, size, false);
            let memory = *blake3::hash(&read[..size]).as_bytes();
            bytes += size as u64;
            STATS[1].fetch_add(1, Relaxed);
            STATS[2].fetch_add(size as u64, Relaxed);
            // The staging may have been detiled meanwhile: compare with
            // its hash as it is after the read.
            let Some(staged) = staged_hash(&staging).filter(|h| *h == staged) else {
                continue;
            };
            match book.observe(key, memory, staged, crate::host::now_ms()) {
                Verdict::Same | Verdict::Known => {}
                Verdict::Seen => {
                    STATS[3].fetch_add(1, Relaxed);
                }
                Verdict::Stale => {
                    let again = repaired.contains_key(&key);
                    let n = STATS[4].fetch_add(1, Relaxed) + 1;
                    if again {
                        STATS[6].fetch_add(1, Relaxed);
                    }
                    if n <= 32 || n.is_power_of_two() {
                        say(n, &staging, again);
                    }
                    if mode() == 2 {
                        repair(&staging);
                        repaired.insert(key, crate::host::now_ms());
                        STATS[5].fetch_add(1, Relaxed);
                    }
                }
            }
        }
        let now = crate::host::now_ms();
        if now.saturating_sub(last_line) >= 30_000 {
            last_line = now;
            let [rounds, textures, read_bytes, seen, stale, fixed, again] =
                STATS.each_ref().map(|c| c.load(Relaxed));
            tracing::info!(
                rounds,
                textures,
                read_mb = read_bytes >> 20,
                mismatches_seen = seen,
                stale,
                repaired = fixed,
                stale_again_after_repair = again,
                "stale audit: since the start (VICTORY_STALE_AUDIT)"
            );
        }
    }
}

/// The staging buffers a list used lately that keep a hash, biggest first.
fn candidates() -> Vec<(usize, Arc<Staging>)> {
    let mut out: Vec<(usize, Arc<Staging>)> = {
        let map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
        let Some(map) = map.as_ref() else {
            return Vec::new();
        };
        map.iter()
            .filter(|(_, s)| {
                s.size >= LEAST_BYTES
                    && s.uploaded.load(Relaxed)
                    && !crate::host::long_ago(s.used.load(Relaxed), USED_MS, 0)
            })
            .map(|(&k, s)| (k, s.clone()))
            .collect()
    };
    out.sort_unstable_by(|a, b| b.1.size.cmp(&a.1.size).then(a.0.cmp(&b.0)));
    out
}

/// The hash the staging buffer was last detiled from, if its refresh keeps
/// one; `None` while another thread refreshes it.
fn staged_hash(staging: &Staging) -> Option<[u8; 32]> {
    let last = staging.last.try_lock().ok()?;
    last.hash.map(|h| *h.as_bytes())
}

fn say(n: u64, staging: &Staging, again: bool) {
    let d = &staging.layout.desc;
    tracing::warn!(
        n,
        again,
        resource = format_args!("{:#x}", staging.resource),
        address = format_args!("{:#x}", staging.memory as usize),
        bytes = staging.size,
        width = d.width,
        height = d.height,
        mips = d.mips,
        element_bytes = d.element_bytes,
        block = d.block,
        first = staging.first,
        on_demand = staging.on_demand,
        frame = crate::frame::current(),
        "stale audit: a texture drawn now shows bytes its title memory no longer holds (for 2 s, nothing made it read again)"
    );
}

/// Has the next refresh read the texture whatever its watch says, and its
/// slots made due as by a write behind the guards.
fn repair(staging: &Staging) {
    {
        let mut last = staging.last.lock().unwrap_or_else(|e| e.into_inner());
        last.hash = None;
        last.asked = 0;
        last.unguarded = last.unguarded.max(crate::frame::current() + 16);
    }
    crate::descriptor::host::page_written(staging.memory as usize, staging.size);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_switch_is_off_unless_one_or_two() {
        assert_eq!(parse(None), 0);
        assert_eq!(parse(Some("0")), 0);
        assert_eq!(parse(Some("yes")), 0);
        assert_eq!(parse(Some("1")), 1);
        assert_eq!(parse(Some(" 2 ")), 2);
    }

    #[test]
    fn a_mismatch_counts_only_once_it_held_two_seconds() {
        let mut book = Book::default();
        let (m, s) = ([1; 32], [2; 32]);
        assert_eq!(book.observe(7, m, s, 1000), Verdict::Seen);
        assert_eq!(book.observe(7, m, s, 2500), Verdict::Seen);
        assert_eq!(book.observe(7, m, s, 3000), Verdict::Stale);
        assert_eq!(book.observe(7, m, s, 3500), Verdict::Known);
    }

    #[test]
    fn a_change_of_either_hash_starts_the_wait_again() {
        let mut book = Book::default();
        let (m, s) = ([1; 32], [2; 32]);
        assert_eq!(book.observe(7, m, s, 0), Verdict::Seen);
        // Memory still arriving.
        assert_eq!(book.observe(7, [3; 32], s, 2500), Verdict::Seen);
        assert_eq!(book.observe(7, [3; 32], s, 4000), Verdict::Seen);
        assert_eq!(book.observe(7, [3; 32], s, 4500), Verdict::Stale);
        // Detiled anew from other bytes: watched afresh.
        assert_eq!(book.observe(7, [3; 32], [4; 32], 5000), Verdict::Seen);
    }

    #[test]
    fn agreeing_hashes_forget_the_suspect() {
        let mut book = Book::default();
        let (m, s) = ([1; 32], [2; 32]);
        assert_eq!(book.observe(7, m, s, 0), Verdict::Seen);
        assert_eq!(book.observe(7, m, m, 1000), Verdict::Same);
        assert_eq!(book.observe(7, m, s, 2500), Verdict::Seen);
        assert_eq!(book.observe(7, m, s, 4500), Verdict::Stale);
    }

    #[test]
    fn textures_out_of_the_round_are_forgotten() {
        let mut book = Book::default();
        book.observe(7, [1; 32], [2; 32], 0);
        book.observe(8, [1; 32], [2; 32], 0);
        book.keep_only(&[8].into_iter().collect());
        assert_eq!(book.observe(7, [1; 32], [2; 32], 3000), Verdict::Seen);
        assert_eq!(book.observe(8, [1; 32], [2; 32], 3000), Verdict::Stale);
    }
}
