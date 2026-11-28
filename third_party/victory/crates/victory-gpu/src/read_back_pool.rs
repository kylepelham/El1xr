//! (feedcard, #88; port of ProjectLeonida/mandy `d06b2be257`, deepscout C #21)
//! `VICTORY_READ_BACK_POOL=1` (Live; off by default): the read-back buffers a list records the
//! title's texture-to-buffer copies into (`crate::list`, `HostList::defer_read_back`) are kept,
//! when the list is reset, for the next read backs of their sizes.
//!
//! Each was a committed resource made for the one copy (`Host::read_back_buffer`,
//! `CreateCommittedResource`) on the thread that records, and released at its list's reset. In
//! mandy's CPU profile (Series X, about 9,500 draws a frame) that was 5 % of the recording threads.
//! The texture copy-backs (`crate::texback`) already keep theirs.
//!
//! With the switch a buffer is made in whole 64 KiB (the card gives a committed buffer its own
//! whole 64 KiB either way), a list's buffers go to a table at its reset, and a read back takes
//! one of its size from there before one is made. Why a buffer in the table is nobody's: a list's
//! read-backs are read when its submission has run (`host::read_backs`, under the list's lock), a
//! list is reset only after its last submission finished, and a buffer goes to the table only
//! then, and only if the list's reference to it is the last ([`sole`]). What differs for whoever
//! reads one: the bytes its copy did not write (the rows' padding) are what the buffer held
//! before, not zeros; `host::read_backs` copies only each row's bytes. Not in the cache key.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use windows::Win32::Graphics::Direct3D12::ID3D12Resource;
use windows::core::Interface;

static ON: victory_core::settings::Live =
    victory_core::settings::Live::new("VICTORY_READ_BACK_POOL", |v| u64::from(v == Some("1")));

/// Live: read at each read back and each list reset.
pub(crate) fn on() -> bool {
    ON.get_bool()
}

/// The most bytes of read-back buffers kept while no list holds them, and the biggest one kept.
const MOST: u64 = 16 << 20;
const BIGGEST: u64 = 4 << 20;

/// The bytes a read-back buffer for `size` bytes is made with when they are kept: whole 64 KiB,
/// so that read backs of nearly one size share their buffers.
pub(crate) fn size(size: u64) -> u64 {
    size.max(1).next_multiple_of(1 << 16)
}

struct Kept(ID3D12Resource, u64);

// SAFETY: a D3D12 resource (free-threaded) that nothing else names.
unsafe impl Send for Kept {}

struct Table {
    kept: Vec<Kept>,
    bytes: u64,
}

static TABLE: Mutex<Table> = Mutex::new(Table {
    kept: Vec::new(),
    bytes: 0,
});

/// Since the last line: buffers made, taken from the table, kept at a list's reset, let go there
/// (the table full, a buffer something else still names, or one not made in whole 64 KiB).
static COUNTS: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

/// Whether this reference to `buffer` is the only one.
fn sole(buffer: &ID3D12Resource) -> bool {
    // SAFETY: a live resource: a reference is taken and given back at once, and giving it back
    // says how many are left.
    unsafe {
        let unknown = &Interface::vtable(buffer).base__.base__.base__.base__;
        (unknown.AddRef)(buffer.as_raw());
        (unknown.Release)(buffer.as_raw()) == 1
    }
}

/// A read-back buffer of at least `bytes` for a list's read back: one a list was done with, if
/// one of its size is kept, else a new one (in whole 64 KiB with the switch, as before without).
pub(crate) fn buffer(host: &crate::host::Host, bytes: u64) -> Option<ID3D12Resource> {
    if !on() {
        return host.read_back_buffer(bytes);
    }
    let want = size(bytes);
    let kept = {
        let mut t = TABLE.lock().unwrap_or_else(|e| e.into_inner());
        t.kept.iter().rposition(|k| k.1 == want).map(|at| {
            t.bytes -= want;
            t.kept.remove(at).0
        })
    };
    if kept.is_some() {
        COUNTS[1].fetch_add(1, Ordering::Relaxed);
        return kept;
    }
    COUNTS[0].fetch_add(1, Ordering::Relaxed);
    host.read_back_buffer(want)
}

/// A list is reset (its submissions ran): its read-back buffers (`backs`, `buffer` of each) go to
/// the table with the switch on, or are let go with the list as before.
pub(crate) fn give<B>(backs: &mut Vec<B>, buffer: impl Fn(B) -> ID3D12Resource) {
    if backs.is_empty() || !on() {
        return;
    }
    // (Asked of each before the table is taken.)
    let sized: Vec<(ID3D12Resource, Option<u64>)> = backs
        .drain(..)
        .map(|b| {
            let b = buffer(b);
            // SAFETY: a live buffer.
            let width = unsafe { b.GetDesc() }.Width;
            let ours = width == size(width) && width <= BIGGEST && sole(&b);
            (b, ours.then_some(width))
        })
        .collect();
    let mut gone = Vec::new();
    {
        let mut t = TABLE.lock().unwrap_or_else(|e| e.into_inner());
        for (b, width) in sized {
            match width {
                Some(w) if t.bytes + w <= MOST => {
                    t.bytes += w;
                    t.kept.push(Kept(b, w));
                    COUNTS[2].fetch_add(1, Ordering::Relaxed);
                }
                _ => {
                    COUNTS[3].fetch_add(1, Ordering::Relaxed);
                    gone.push(b);
                }
            }
        }
    }
    // Released outside the lock.
    drop(gone);
}

/// One line a frame of what the table did, if there were read backs with the switch on.
pub(crate) fn line(frames: f64) {
    let c: Vec<u64> = COUNTS
        .iter()
        .map(|a| a.swap(0, Ordering::Relaxed))
        .collect();
    if frames <= 0.0 || c.iter().all(|&n| n == 0) {
        return;
    }
    let (kept, bytes) = {
        let t = TABLE.lock().unwrap_or_else(|e| e.into_inner());
        (t.kept.len(), t.bytes)
    };
    tracing::info!(
        "read-back buffers per frame (VICTORY_READ_BACK_POOL={}): {:.1} made, {:.1} taken from \
         those kept; at their lists' resets {:.1} kept and {:.1} let go; {kept} kept now, {:.1} MB",
        u8::from(on()),
        c[0] as f64 / frames,
        c[1] as f64 / frames,
        c[2] as f64 / frames,
        c[3] as f64 / frames,
        bytes as f64 / (1 << 20) as f64,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_read_back_buffer_is_made_in_whole_64_kib() {
        assert_eq!(size(0), 1 << 16);
        assert_eq!(size(1), 1 << 16);
        assert_eq!(size(512), 1 << 16);
        assert_eq!(size(1 << 16), 1 << 16);
        assert_eq!(size((1 << 16) + 1), 2 << 16);
        assert_eq!(size(350_000), 6 << 16);
    }

    /// A reset list's read-back buffer is the next read back's of its size, once; one something
    /// else still names at the reset is let go, not kept; off, nothing is kept.
    #[test]
    fn a_reset_lists_read_back_buffer_is_the_next_read_backs() {
        let Some(host) = crate::host::host() else {
            eprintln!("no D3D12 device; the read-back buffers' table not tested");
            return;
        };
        if std::env::var_os("VICTORY_READ_BACK_POOL").is_some() {
            return;
        }
        // (A size no other read back here asks for: the table is the process's.)
        let bytes = (7 << 16) - 100;
        victory_core::settings::set("VICTORY_READ_BACK_POOL", "1").unwrap();
        let first = buffer(host, bytes).unwrap();
        // SAFETY: a live buffer.
        assert_eq!(unsafe { first.GetDesc() }.Width, 7 << 16);
        let raw = first.as_raw() as usize;
        let mut backs = vec![(first, 0usize)];
        // The list holds it: another read back gets another buffer.
        let other = buffer(host, bytes).unwrap();
        assert_ne!(other.as_raw() as usize, raw);
        give(&mut backs, |b| b.0);
        assert!(backs.is_empty());
        // Nobody's now: the next read back of that size takes it, once.
        let again = buffer(host, bytes - 4096).unwrap();
        assert_eq!(again.as_raw() as usize, raw);
        let third = buffer(host, bytes).unwrap();
        assert_ne!(third.as_raw() as usize, raw);
        // Still named elsewhere at the reset: let go, not kept.
        let named = again.clone();
        let mut backs = vec![(again, 0usize)];
        give(&mut backs, |b| b.0);
        assert!(sole(&named));
        let fourth = buffer(host, bytes).unwrap();
        assert_ne!(fourth.as_raw() as usize, raw);
        // Off: given back with the list as before (left in place), made at the size asked.
        victory_core::settings::set("VICTORY_READ_BACK_POOL", "").unwrap();
        let mut backs = vec![(fourth, 0usize)];
        give(&mut backs, |b| b.0);
        assert_eq!(backs.len(), 1);
        let off = buffer(host, bytes).unwrap();
        // SAFETY: a live buffer.
        assert_eq!(unsafe { off.GetDesc() }.Width, bytes);
        drop((other, third, backs, named, off));
    }
}

#[cfg(test)]
mod bench {
    use super::*;

    /// (#88) Sizes the switch: 8 threads each make 250 rounds of a list's two small read backs
    /// (the exposure metering's 5x1 and 1x1 float targets) and its reset, off then on, twice.
    /// D3D12 device calls only, no work on the card. Ignored; `--ignored --nocapture`, release.
    #[test]
    #[ignore]
    fn bench_read_back_buffers() {
        let Some(host) = crate::host::host() else {
            eprintln!("no D3D12 device");
            return;
        };
        for pool in [false, true, false, true] {
            victory_core::settings::set("VICTORY_READ_BACK_POOL", if pool { "1" } else { "" })
                .unwrap();
            let total = AtomicU64::new(0);
            let wall = std::time::Instant::now();
            std::thread::scope(|sc| {
                for _ in 0..8 {
                    sc.spawn(|| {
                        for _ in 0..250 {
                            let t = std::time::Instant::now();
                            let mut backs =
                                vec![buffer(host, 5 * 16 + 256).unwrap(), buffer(host, 256).unwrap()];
                            give(&mut backs, |b| b);
                            drop(backs);
                            total.fetch_add(t.elapsed().as_nanos() as u64, Ordering::Relaxed);
                        }
                    });
                }
            });
            println!(
                "read-back buffers, pool={pool}: {:.1} us a round of two (made and let go), wall {:.1} ms for 2000 rounds",
                total.into_inner() as f64 / 1e3 / 2000.0,
                wall.elapsed().as_secs_f64() * 1e3
            );
        }
        victory_core::settings::set("VICTORY_READ_BACK_POOL", "").unwrap();
    }
}
