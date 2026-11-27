//! `VICTORY_RAW_ARGS_WATCH=<n>` (diagnostics, off by default; 1..=4096):
//! what the card read for the indirect records run as the title wrote them
//! (`VICTORY_INDIRECT_RAW=2`, and `VICTORY_SPLIT_RAW`; not those our pass
//! made safe in place).
//!
//! Before each such execution a copy of ours takes its first record's first
//! five words (and its count) into the list's own read-back ring of `n`
//! slots. When the list is reset (every submission of it has run) the ring
//! is read: a record over the limits of `crate::indirect::raw_check` is
//! logged (`raw args watch:`). The hang watch (`VICTORY_HANG_WATCH`) prints
//! the ring of every list of the submission the card owes: its largest
//! record and the last 16. The card copies in list order, so the records
//! it has not reached yet read 0.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use windows::Win32::Graphics::Direct3D12::*;

/// Bytes a slot takes: five record words, the count, two spare.
const SLOT: u64 = 32;
/// Record bytes copied (the first five words).
const WORDS: u64 = 20;

/// `VICTORY_RAW_ARGS_WATCH`: slots per list.
pub(crate) fn slots() -> Option<usize> {
    static N: OnceLock<Option<usize>> = OnceLock::new();
    *N.get_or_init(|| parse(std::env::var("VICTORY_RAW_ARGS_WATCH").ok().as_deref()))
}

fn parse(v: Option<&str>) -> Option<usize> {
    v?.trim()
        .parse::<usize>()
        .ok()
        .filter(|n| (1..=4096).contains(n))
}

/// One execution noted.
#[derive(Clone)]
struct Meta {
    kind: u32,
    max: u32,
    counted: bool,
    at: u64,
    how: &'static str,
    name: String,
}

struct Ring {
    buffer: ID3D12Resource,
    data: *mut u32,
    metas: Vec<Meta>,
    next: usize,
}

// SAFETY: D3D12 objects are free-threaded; `data` is the buffer's mapping,
// read and written under the map's lock.
unsafe impl Send for Ring {}

/// Rings by list number (`crate::dred::Ops::id`).
static RINGS: Mutex<Option<HashMap<u64, Ring>>> = Mutex::new(None);

/// Records over the limits logged so far (at most [`SHOWN`]).
static SAID: AtomicU64 = AtomicU64::new(0);
const SHOWN: u64 = 32;

/// Before a raw execution on `list`: copies the record at `args` (and the
/// count at `count`) into the list's ring. `at`: its title address; `how`:
/// raw, sanitized or split; `name`: its shaders.
pub(crate) fn note(
    list: &crate::host::HostList,
    (kind, stride, max): (u32, u32, u32),
    args: (&ID3D12Resource, u64),
    count: Option<(&ID3D12Resource, u64)>,
    at: u64,
    how: &'static str,
    name: impl FnOnce() -> String,
) {
    let Some(n) = slots() else {
        return;
    };
    let mut rings = RINGS.lock().unwrap_or_else(|e| e.into_inner());
    let rings = rings.get_or_insert_with(HashMap::new);
    let id = list.ops.id();
    if !rings.contains_key(&id) {
        let Some(ring) = make(n) else {
            return;
        };
        rings.insert(id, ring);
    }
    let Some(ring) = rings.get_mut(&id) else {
        return;
    };
    let slot = ring.next % n;
    let meta = Meta {
        kind,
        max,
        counted: count.is_some(),
        at,
        how,
        name: name(),
    };
    if slot < ring.metas.len() {
        ring.metas[slot] = meta;
    } else {
        ring.metas.push(meta);
    }
    ring.next += 1;
    let to = slot as u64 * SLOT;
    // SAFETY: an open list; both buffers outlive it (the ring is kept until
    // the list is reset, the title copies by the list). The title copies are
    // in a state copies read (promoted, as for the execution after).
    unsafe {
        list.list.CopyBufferRegion(
            &ring.buffer,
            to,
            args.0,
            args.1,
            WORDS.min(u64::from(stride)),
        );
        if let Some((c, at)) = count {
            list.list
                .CopyBufferRegion(&ring.buffer, to + WORDS, c, at, 4);
        }
    }
}

fn make(n: usize) -> Option<Ring> {
    let host = crate::host::host()?;
    let buffer = host.read_back_buffer(n as u64 * SLOT)?;
    let mut data = std::ptr::null_mut();
    // SAFETY: mapping a read-back buffer for good.
    unsafe { buffer.Map(0, None, Some(&mut data)) }.ok()?;
    let data = data.cast::<u32>();
    // SAFETY: inside the mapping.
    unsafe { std::ptr::write_bytes(data, 0, n * SLOT as usize / 4) };
    Some(Ring {
        buffer,
        data,
        metas: Vec::with_capacity(n),
        next: 0,
    })
}

/// A slot's words: five record words and the count.
fn words(ring: &Ring, slot: usize) -> [u32; 6] {
    let mut w = [0; 6];
    for (i, v) in w.iter_mut().enumerate() {
        // SAFETY: inside the mapping; the card may still be writing it.
        *v = unsafe { ring.data.add(slot * SLOT as usize / 4 + i).read_volatile() };
    }
    w
}

/// The work a record asks for (vertex instances, groups), for the largest.
fn work(kind: u32, w: &[u32; 6]) -> u64 {
    match kind {
        0 | 1 => u64::from(w[0]) * u64::from(w[1]),
        _ => u64::from(w[0]) * u64::from(w[1]) * u64::from(w[2]),
    }
}

fn text(meta: &Meta, w: &[u32; 6]) -> String {
    let what = match meta.kind {
        0 => format!("draw {} vertices x {} instances", w[0], w[1]),
        1 => format!(
            "indexed {} indices x {} instances (first {}, base {}, instance {})",
            w[0], w[1], w[2], w[3] as i32, w[4]
        ),
        2 => format!("dispatch {}x{}x{}", w[0], w[1], w[2]),
        10 => format!("mesh {}x{}x{}", w[0], w[1], w[2]),
        k => format!("kind {k} {} {} {}", w[0], w[1], w[2]),
    };
    let records = if meta.counted {
        format!(", count {} of at most {}", w[5], meta.max)
    } else if meta.max > 1 {
        format!(", {} records (the first)", meta.max)
    } else {
        String::new()
    };
    format!(
        "{} {what}{records} at {:#x} [{}]",
        meta.how, meta.at, meta.name
    )
}

/// The slots noted, oldest first.
fn order(ring: &Ring, n: usize) -> impl Iterator<Item = usize> {
    let kept = ring.next.min(n);
    (ring.next - kept..ring.next).map(move |k| k % n)
}

/// `list` (by number) is reset: every submission of it has run. Logs its
/// records over the limits and starts its ring again.
pub(crate) fn reset(id: u64) {
    let Some(n) = slots() else {
        return;
    };
    let mut rings = RINGS.lock().unwrap_or_else(|e| e.into_inner());
    let Some(ring) = rings.as_mut().and_then(|r| r.get_mut(&id)) else {
        return;
    };
    for slot in order(ring, n) {
        let w = words(ring, slot);
        let meta = &ring.metas[slot];
        let over = crate::indirect::raw_check::over(meta.kind, [w[0], w[1], w[2]])
            || (meta.counted && w[5] > meta.max);
        if over && SAID.fetch_add(1, Ordering::Relaxed) < SHOWN {
            tracing::warn!(
                "raw args watch: victory list #{id} ran a record over the limits as the card read it: {}",
                text(meta, &w)
            );
        }
    }
    // SAFETY: inside the mapping; the list has run.
    unsafe { std::ptr::write_bytes(ring.data, 0, n * SLOT as usize / 4) };
    ring.next = 0;
}

/// For the hang watch: the records of list `id` copied so far, newest last
/// (at most `most`), and its largest; empty without the switch or when the
/// rings are held.
pub(crate) fn lines(id: u64, most: usize) -> Vec<String> {
    let Some(n) = slots() else {
        return Vec::new();
    };
    let Ok(rings) = RINGS.try_lock() else {
        return vec![format!(
            "  raw records of victory list #{id}: the rings are held by another thread"
        )];
    };
    let Some(ring) = rings.as_ref().and_then(|r| r.get(&id)) else {
        return Vec::new();
    };
    let all: Vec<(usize, [u32; 6])> = order(ring, n).map(|s| (s, words(ring, s))).collect();
    let mut lines = vec![format!(
        "  raw records of victory list #{id} as the card read them ({} noted, a record not yet copied reads 0):",
        ring.next
    )];
    if let Some((s, w)) = all.iter().max_by_key(|(s, w)| work(ring.metas[*s].kind, w)) {
        lines.push(format!("    the largest: {}", text(&ring.metas[*s], w)));
    }
    let from = all.len().saturating_sub(most);
    for (s, w) in &all[from..] {
        lines.push(format!("    {}", text(&ring.metas[*s], w)));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_slots() {
        assert_eq!(parse(Some("64")), Some(64));
        assert_eq!(parse(Some("0")), None);
        assert_eq!(parse(Some("5000")), None);
        assert_eq!(parse(None), None);
    }

    #[test]
    fn says_records_and_counts() {
        let meta = Meta {
            kind: 0,
            max: 1,
            counted: false,
            at: 0x1000,
            how: "raw",
            name: "[vertex 81749e9de9a48b7b]".into(),
        };
        assert_eq!(
            text(&meta, &[u32::MAX, 1, 0, 0, 0, 0]),
            "raw draw 4294967295 vertices x 1 instances at 0x1000 [[vertex 81749e9de9a48b7b]]"
        );
        let meta = Meta {
            kind: 10,
            max: 8,
            counted: true,
            at: 0x20,
            how: "raw",
            name: String::new(),
        };
        assert_eq!(
            text(&meta, &[2, 1, 1, 0, 0, 9]),
            "raw mesh 2x1x1, count 9 of at most 8 at 0x20 []"
        );
        assert_eq!(work(0, &[1 << 20, 8, 0, 0, 0, 0]), 1 << 23);
    }

    /// The copy into the ring runs on the card and the reset reads it back.
    #[test]
    fn the_ring_holds_what_the_card_copied() {
        let Some(host) = crate::host::host() else {
            return; // no host GPU here
        };
        let Some(ring) = make(4) else {
            return;
        };
        let (src, data) = host.shared_buffer(64).unwrap();
        // SAFETY: inside the mapping: a garbage draw record.
        unsafe { (data as *mut [u32; 5]).write([0xffff_fff0, 3, 0, 0, 0]) };
        let list = host.new_list().unwrap();
        // SAFETY: an open list; both buffers live past the wait below.
        unsafe {
            list.list
                .CopyBufferRegion(&ring.buffer, SLOT, &src, 0, WORDS)
        };
        list.close();
        host.execute(&[&list]);
        let w = words(&ring, 1);
        assert_eq!(&w[..2], &[0xffff_fff0, 3]);
        assert!(crate::indirect::raw_check::over(0, [w[0], w[1], w[2]]));
    }
}
