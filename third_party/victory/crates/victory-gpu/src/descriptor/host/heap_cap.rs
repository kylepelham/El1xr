//! (heapcap) `VICTORY_HEAP_CAP`: title view slots kept off the host's
//! per-draw slots on a view heap capped below the title's size.
//!
//! The RTX 5090 takes a shader-visible view heap of at most 1,000,000
//! descriptors (resource binding tier 3; `D3D12_OPTIONS19`
//! `MaxViewDescriptorHeapSize` = 1,000,000, and 1,000,001 is refused). The
//! title's heap holds 1,245,184. A capped host heap keeps the title's slots
//! 0..737,855, then the 262,144 dynamic slots (per-draw tables), then the
//! null slot 999,999 every view index at or past it reads. A view index the
//! title computes itself (a material word read from memory, `shl 5`,
//! `lshr 5`, `createHandleFromHeap`) in 737,855..999,998 read one of those
//! dynamic slots: some other draw's table entry, not the title's view.
//!
//! With the switch on, the first dynamic slot (737,855 here) is never given
//! out and stays null, and translated shaders clamp every view index the
//! title computes (root type `ResourceHeap`) to it
//! (`victory_shader::emit::title_view_limit`). Indices a table gives are
//! left alone: the host made the table, in title slots it holds or in
//! dynamic ones. A title slot past the host's still has no host slot, so it
//! reads null instead of the wrong view. Values:
//! - `0` (default): as before.
//! - `1`: the clamp and the reserved slot.
//! - `2`: as 1, and the log of title slots past the host heap
//!   (`VICTORY_HEAP_CAP_LOG=1` gives the log alone, with or without the
//!   switch): views the title makes there, pages it writes there, and every
//!   [`SCAN_EVERY`] frames the records it holds there and how many name a
//!   live view; its lines add the most dynamic slots taken at once.
//! - `3`: title slots past the host heap's read their own views, packed
//!   into host slots taken from the dynamic ones, through a table
//!   ([`super::heap_remap`]).

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering::Relaxed};
use std::sync::{Mutex, OnceLock};

/// Frames between scans of the title's records past the host heap.
pub(crate) const SCAN_EVERY: u64 = 300;

fn parse(value: Option<&str>) -> u8 {
    value
        .and_then(|v| v.trim().parse::<u8>().ok())
        .map_or(0, |n| n.min(3))
}

fn mode() -> u8 {
    static MODE: OnceLock<u8> = OnceLock::new();
    *MODE.get_or_init(|| parse(std::env::var("VICTORY_HEAP_CAP").ok().as_deref()))
}

/// Whether title view indices are clamped off the dynamic slots.
pub(crate) fn on() -> bool {
    mode() >= 1
}

/// Whether title slots past the host heap read their own views
/// (`VICTORY_HEAP_CAP=3`).
pub(crate) fn remap_on() -> bool {
    mode() == 3
}

/// Whether title slots past the host heap are logged.
pub(crate) fn log_on() -> bool {
    static LOG: OnceLock<bool> = OnceLock::new();
    *LOG.get_or_init(|| {
        mode() == 2 || std::env::var("VICTORY_HEAP_CAP_LOG").is_ok_and(|v| v.trim() == "1")
    })
}

/// Dynamic slots now free (`HostHeap::dynamic`) of `dynamic`: the most
/// taken at once since the last line and ever (the reserved ones left out).
pub(super) fn note_free(free: u32, dynamic: u32) {
    if !on() {
        return;
    }
    let taken = u64::from(dynamic.saturating_sub(free).saturating_sub(RESERVED.load(Relaxed)));
    super::heap_remap::PEAK_TAKEN[0].fetch_max(taken, Relaxed);
    super::heap_remap::PEAK_TAKEN[1].fetch_max(taken, Relaxed);
}

/// Dynamic slots [`reserve`] kept out.
static RESERVED: AtomicU32 = AtomicU32::new(0);

/// The slot past the title's on a host view heap whose last (null) slot is
/// `null`, with `dynamic` slots: `null - dynamic`, where a title heap larger
/// than the host's has its dynamic slots begin. `None` on a host whose view
/// heap is not capped (`null` is `uncapped_null`, or 0: no limit), where
/// every title slot has its own host slot.
pub(crate) fn title_null_of(null: u32, dynamic: u32, uncapped_null: u32) -> Option<u32> {
    (null != 0 && null < uncapped_null && dynamic < null).then(|| null - dynamic)
}

/// Tells the translator the slot title view indices are clamped to, from
/// the view heap's null slot: called wherever that is set
/// (`probe_view_limit`, `host_facts::assume_host_facts`).
pub(crate) fn tell_translator(null: u32) {
    let slot = if on() {
        title_null_of(null, super::dynamic_slots(null), super::UNCAPPED_VIEWS - 1)
    } else {
        None
    };
    victory_shader::emit::set_title_view_limit(slot.unwrap_or(0));
    let len = if slot.is_some() && remap_on() {
        victory_shader::emit::REMAP_LEN
    } else {
        0
    };
    victory_shader::emit::set_title_remap_len(len);
    if let Some(slot) = slot {
        tracing::info!(
            title_null = slot,
            null,
            "heap cap: title view indices at or past {slot} read the null slot {slot}; dynamic slots {} to {null} are the host's alone",
            slot + 1
        );
    }
}

/// The dynamic slot (index into the dynamic slots) to keep null: the one at
/// host slot `title_null`, if it is one of this heap's dynamic slots
/// (`first_dynamic`, `dynamic` of them). A smaller title heap's dynamic
/// slots end before it: its title indices past its own count are past the
/// heap the title made, and slot `title_null` is null already.
pub(crate) fn reserved_of(title_null: u32, first_dynamic: u32, dynamic: u32) -> Option<u32> {
    if title_null == 0 {
        return None;
    }
    let at = title_null.checked_sub(first_dynamic)?;
    (at < dynamic).then_some(at)
}

/// Keeps the dynamic slot at the translator's title null slot out of
/// `taken`'s free slots, and with `=3` the packed slots after it
/// ([`super::heap_remap::compact_of`]); how many it took.
pub(super) fn reserve(
    taken: &mut super::DynamicBits,
    sampler: bool,
    first_dynamic: u32,
    dynamic: u32,
) -> u32 {
    if sampler {
        return 0;
    }
    let title_null = victory_shader::emit::title_view_limit();
    let Some(at) = reserved_of(title_null, first_dynamic, dynamic) else {
        return 0;
    };
    let n = if remap_on() {
        1 + super::heap_remap::compact_of(super::heap_remap::compact_slots(), at, dynamic)
    } else {
        1
    };
    taken.set_taken(at as usize, n as usize);
    RESERVED.store(n, Relaxed);
    tracing::info!(
        title_null,
        first_dynamic,
        dynamic,
        reserved = n,
        "heap cap: host view slot {title_null} kept null; title slots {first_dynamic} and up have no host slot"
    );
    n
}

/// The root SRV of the `=3` remap table for a list drawing with `host`'s
/// heap (`draw.rs`, `victory_shader::emit::SLOT_REMAP`).
pub(crate) fn remap_root(list: &crate::host::HostList, host: &super::HostHeap) -> Option<u64> {
    super::heap_remap::root(list, host)
}

/// Counts of title slots past the host heap since the last line: views
/// made there, their lowest and highest slot.
static MADE: [AtomicU64; 1] = [AtomicU64::new(0)];
static MADE_RANGE: [AtomicU32; 2] = [AtomicU32::new(u32::MAX), AtomicU32::new(0)];
/// Pages written past the host heap, slots on them, lowest and highest slot.
static WRITTEN: [AtomicU64; 2] = [AtomicU64::new(0), AtomicU64::new(0)];
static WRITTEN_RANGE: [AtomicU32; 2] = [AtomicU32::new(u32::MAX), AtomicU32::new(0)];

/// A view the title made at slot `index`, past the host heap's `count`
/// title slots (`HostHeap::slot_in_bounds`).
pub(super) fn made_past(index: u32, count: u32, for_draw: bool) {
    if remap_on() {
        super::heap_remap::dirty(index, index + 1);
    }
    if !log_on() {
        return;
    }
    let n = MADE[0].fetch_add(1, Relaxed);
    MADE_RANGE[0].fetch_min(index, Relaxed);
    MADE_RANGE[1].fetch_max(index, Relaxed);
    // the first 32 each, then one in 4,096
    if n < 32 || n.is_multiple_of(4096) {
        tracing::warn!(
            index,
            count,
            for_draw,
            n = n + 1,
            "heap cap: title view made past the host heap"
        );
    }
}

/// The slots `[lo, hi)` of byte range `[from, to)` of a title heap at
/// `start` (`step` bytes a slot) that lie in `[count, title)`.
pub(crate) fn slots_past(
    (from, to): (usize, usize),
    start: usize,
    step: usize,
    count: u32,
    title: u32,
) -> Option<(u32, u32)> {
    if step == 0 || count >= title {
        return None;
    }
    let lo = (from.saturating_sub(start) / step).max(count as usize);
    let hi = to.saturating_sub(start).div_ceil(step).min(title as usize);
    (lo < hi).then_some((lo as u32, hi as u32))
}

/// Pages the title wrote in a placed view heap (`sync_heaps`), as byte
/// ranges: those past the host heap's `count` title slots (of `title`) are
/// counted; nothing mirrors them.
pub(crate) fn note_writes(
    ranges: &[(usize, usize)],
    start: u64,
    step: u64,
    count: u32,
    title: u32,
) {
    let (log, remap) = (log_on(), remap_on());
    if !(log || remap) || count >= title {
        return;
    }
    for &range in ranges {
        let Some((lo, hi)) = slots_past(range, start as usize, step as usize, count, title) else {
            continue;
        };
        if remap {
            super::heap_remap::dirty(lo, hi);
        }
        if !log {
            continue;
        }
        WRITTEN[0].fetch_add(
            range.1.saturating_sub(range.0).div_ceil(crate::watch::PAGE) as u64,
            Relaxed,
        );
        WRITTEN[1].fetch_add(u64::from(hi - lo), Relaxed);
        WRITTEN_RANGE[0].fetch_min(lo, Relaxed);
        WRITTEN_RANGE[1].fetch_max(hi - 1, Relaxed);
    }
}

/// What a scan of the title's records past the host heap found.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Scan {
    /// Records naming a view (tag set).
    pub records: u64,
    /// Of those, records naming a view that is alive now.
    pub live: u64,
    /// Live records whose slot a title index reached the dynamic slots by
    /// (below the heap's null slot) when the switch is off.
    pub live_in_dynamic: u64,
    pub lowest_live: Option<u32>,
    pub highest_live: Option<u32>,
    /// Up to 8 live slots, lowest first.
    pub sample: Vec<u32>,
    /// Live records by 65,536-slot block (block = slot >> 16).
    pub blocks: std::collections::BTreeMap<u32, u64>,
}

impl Scan {
    /// Adds the record at `slot`: `tagged` (it names a view), `live` (that
    /// view is alive). `null` is the host heap's null slot.
    pub(crate) fn add(&mut self, slot: u32, tagged: bool, live: bool, null: u32) {
        if !tagged {
            return;
        }
        self.records += 1;
        if !live {
            return;
        }
        self.live += 1;
        if slot < null {
            self.live_in_dynamic += 1;
        }
        self.lowest_live = Some(self.lowest_live.map_or(slot, |l| l.min(slot)));
        self.highest_live = Some(self.highest_live.map_or(slot, |h| h.max(slot)));
        if self.sample.len() < 8 {
            self.sample.push(slot);
        }
        *self.blocks.entry(slot >> 16).or_default() += 1;
    }
}

/// Once a frame from `sync_heaps`, for a title view heap at `start` of
/// `title` slots whose host heap holds `count` and has its null slot at
/// `null`: every [`SCAN_EVERY`] frames, the line of what was made and
/// written past the host heap since the last, and a scan of the records
/// there.
pub(crate) fn watch(start: u64, step: u64, host: &super::HostHeap, title: u32, null: u32) {
    let count = host.count;
    if remap_on() && count < title {
        super::heap_remap::refresh(start, step, host, title);
    }
    if !log_on() || count >= title || step != 32 {
        return;
    }
    static LAST: Mutex<Option<u64>> = Mutex::new(None);
    let frame = crate::frame::current();
    {
        let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
        if last.is_some_and(|l| frame < l + SCAN_EVERY) {
            return;
        }
        *last = Some(frame);
    }
    let started = std::time::Instant::now();
    let mut scan = Scan::default();
    const CHUNK: u32 = 1 << 14;
    let mut bytes = vec![0u8; (CHUNK * 32) as usize];
    let mut unread = 0u32;
    let mut at = count;
    while at < title {
        let n = CHUNK.min(title - at);
        let buf = &mut bytes[..(n * 32) as usize];
        let from = start + u64::from(at) * step;
        if !crate::host::read_into(from as *const u8, buf) {
            unread += n;
            at += n;
            continue;
        }
        for (k, record) in buf.chunks_exact(32).enumerate() {
            let word = |w: usize| {
                u32::from_le_bytes(record[w * 4..w * 4 + 4].try_into().unwrap_or_default())
            };
            let [tag, i, generation] = [word(0), word(1), word(2)];
            let tagged = tag == super::super::VIEW_TAG;
            let live = tagged && super::super::view_at(i, generation).is_some();
            scan.add(at + k as u32, tagged, live, null);
        }
        at += n;
    }
    let blocks: Vec<String> = scan
        .blocks
        .iter()
        .map(|(b, n)| format!("{}..{}:{n}", b << 16, (b + 1) << 16))
        .collect();
    let made = MADE[0].swap(0, Relaxed);
    let made_range = (
        MADE_RANGE[0].swap(u32::MAX, Relaxed),
        MADE_RANGE[1].swap(0, Relaxed),
    );
    let pages = WRITTEN[0].swap(0, Relaxed);
    let slots = WRITTEN[1].swap(0, Relaxed);
    let written_range = (
        WRITTEN_RANGE[0].swap(u32::MAX, Relaxed),
        WRITTEN_RANGE[1].swap(0, Relaxed),
    );
    tracing::warn!(
        frame,
        host_title_slots = count,
        title_slots = title,
        null,
        switch_on = on(),
        records = scan.records,
        live = scan.live,
        live_in_dynamic = scan.live_in_dynamic,
        lowest_live = ?scan.lowest_live,
        highest_live = ?scan.highest_live,
        sample = ?scan.sample,
        blocks = %blocks.join(" "),
        unread,
        made_since = made,
        made_range = ?(made != 0).then_some(made_range),
        written_pages_since = pages,
        written_slots_since = slots,
        written_range = ?(pages != 0).then_some(written_range),
        dynamic_peak = super::heap_remap::PEAK_TAKEN[0].swap(0, Relaxed),
        dynamic_peak_ever = super::heap_remap::PEAK_TAKEN[1].load(Relaxed),
        ms = started.elapsed().as_millis() as u64,
        "heap cap: title slots past the host heap"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The retail layout on the RTX 5090: the null slot 999,999, 2^18
    /// dynamic slots, the title's 1,245,184 views.
    const NULL: u32 = 999_999;
    const DYNAMIC: u32 = 1 << 18;
    const TITLE: u32 = 1_245_184;
    const UNCAPPED_NULL: u32 = (1 << 21) - 1;

    /// The host slot a shader reads for title view index `i` (as
    /// `lower.rs` emits it: `umin(i, title_null)` when set, then
    /// `heap_handle`'s `umin(.., null)`).
    fn host_slot(i: u32, title_null: Option<u32>) -> u32 {
        let i = title_null.map_or(i, |t| i.min(t));
        i.min(NULL)
    }

    /// The host heap's layout as `HostHeap::new` makes it for `count` title
    /// views: title slots kept, the first dynamic slot.
    fn layout(count: u32) -> (u32, u32) {
        let fitted = if count + DYNAMIC <= NULL {
            count
        } else {
            NULL - DYNAMIC
        };
        (fitted, fitted)
    }

    #[test]
    fn with_the_switch_no_title_index_reads_a_dynamic_slot_given_out() {
        let title_null = title_null_of(NULL, DYNAMIC, UNCAPPED_NULL);
        assert_eq!(title_null, Some(737_855));
        let (fitted, first_dynamic) = layout(TITLE);
        assert_eq!(fitted, 737_855);
        let reserved = reserved_of(title_null.unwrap(), first_dynamic, DYNAMIC);
        assert_eq!(reserved, Some(0), "the first dynamic slot is kept null");
        let given = |slot: u32| {
            (first_dynamic..first_dynamic + DYNAMIC).contains(&slot)
                && Some(slot - first_dynamic) != reserved
        };
        for i in 0..TITLE {
            let slot = host_slot(i, title_null);
            assert!(!given(slot), "title index {i} reads dynamic slot {slot}");
            if i < fitted {
                assert_eq!(slot, i, "a title slot the host holds reads itself");
            } else {
                assert_eq!(slot, 737_855, "past the host's: the kept null slot");
            }
        }
        // garbage indices too (27 bits)
        for i in [TITLE, NULL, NULL + 1, (1 << 27) - 1, u32::MAX] {
            assert!(!given(host_slot(i, title_null)), "{i}");
        }
    }

    #[test]
    fn without_the_switch_the_layout_and_reads_are_as_before() {
        // nothing told the translator: the reads of before, aliasing included
        let (fitted, first_dynamic) = layout(TITLE);
        for i in [0, 737_854, 898_339, 938_783, 999_998, 999_999, TITLE] {
            assert_eq!(host_slot(i, None), i.min(NULL), "{i}");
        }
        assert!((first_dynamic..first_dynamic + DYNAMIC).contains(&host_slot(898_339, None)));
        assert_eq!(fitted, 737_855);
        assert_eq!(reserved_of(0, first_dynamic, DYNAMIC), None, "no slot kept");
        assert_eq!(parse(None), 0);
        assert_eq!(parse(Some("")), 0);
        assert_eq!(parse(Some("1")), 1);
        assert_eq!(parse(Some(" 2 ")), 2);
        assert_eq!(parse(Some("3")), 3);
        assert_eq!(parse(Some("9")), 3);
    }

    #[test]
    fn an_uncapped_host_or_none_keeps_its_layout() {
        assert_eq!(title_null_of(UNCAPPED_NULL, DYNAMIC, UNCAPPED_NULL), None);
        assert_eq!(title_null_of(0, DYNAMIC, UNCAPPED_NULL), None);
        // VICTORY_DYNAMIC_SLOTS at its most (half the heap)
        assert_eq!(
            title_null_of(NULL, NULL / 2, UNCAPPED_NULL),
            Some(NULL - NULL / 2)
        );
    }

    #[test]
    fn a_small_title_heap_keeps_every_dynamic_slot() {
        // 8 title views: dynamic slots 8..8+2^18, before 737,855
        let (_, first_dynamic) = layout(8);
        assert_eq!(reserved_of(737_855, first_dynamic, DYNAMIC), None);
        // one whose dynamic slots reach it keeps that one
        let (_, first_dynamic) = layout(600_000);
        assert_eq!(reserved_of(737_855, first_dynamic, DYNAMIC), Some(137_855));
    }

    #[test]
    fn the_reserved_slot_is_never_given_out() {
        let mut taken = super::super::DynamicBits::new(1000);
        taken.set_taken(0, 1);
        assert_eq!(taken.find_run(0, 1), Some(1));
        assert_eq!(taken.find_run(0, 999), Some(1));
        assert_eq!(taken.find_run(0, 1000), None);
        assert_eq!(taken.find_run(500, 600), Some(1));
    }

    #[test]
    fn written_ranges_past_the_host_heap_give_their_slots() {
        let start = 0x1000_0000usize;
        let at = |slot: usize| start + slot * 32;
        // a page over the boundary: only the slots past 737,855
        assert_eq!(
            slots_past((at(737_800), at(737_928)), start, 32, 737_855, TITLE),
            Some((737_855, 737_928))
        );
        assert_eq!(
            slots_past((at(0), at(128)), start, 32, 737_855, TITLE),
            None
        );
        assert_eq!(
            slots_past((at(1_245_000), at(1_300_000)), start, 32, 737_855, TITLE),
            Some((1_245_000, TITLE))
        );
        assert_eq!(slots_past((at(0), at(9)), start, 32, TITLE, TITLE), None);
    }

    #[test]
    fn a_scan_counts_live_records_and_where_they_fall() {
        let mut scan = Scan::default();
        scan.add(737_900, false, false, NULL);
        scan.add(898_339, true, false, NULL);
        scan.add(938_783, true, true, NULL);
        scan.add(1_100_000, true, true, NULL);
        assert_eq!(scan.records, 2 + 1);
        assert_eq!(scan.live, 2);
        assert_eq!(scan.live_in_dynamic, 1);
        assert_eq!(scan.lowest_live, Some(938_783));
        assert_eq!(scan.highest_live, Some(1_100_000));
        assert_eq!(scan.sample, [938_783, 1_100_000]);
        assert_eq!(scan.blocks.get(&(938_783 >> 16)), Some(&1));
    }
}

#[cfg(test)]
mod probe {
    use windows::Win32::Graphics::Direct3D12::*;

    /// What the host GPU says of its view heaps, and the largest one it
    /// makes (run by hand: `-- --ignored --nocapture`). RTX 5090, driver
    /// of 2026-11: binding tier 3, `MaxViewDescriptorHeapSize` 1,000,000,
    /// largest 1,000,000.
    #[test]
    #[ignore]
    fn probe_host_view_heap_limit() {
        let Some(host) = crate::host::host() else {
            return;
        };
        let mut o = D3D12_FEATURE_DATA_D3D12_OPTIONS19::default();
        // SAFETY: the feature query writes the structure of the given size.
        let r = unsafe {
            host.device.CheckFeatureSupport(
                D3D12_FEATURE_D3D12_OPTIONS19,
                (&raw mut o).cast(),
                size_of_val(&o) as u32,
            )
        };
        eprintln!(
            "options19 ok={} max_view={} max_sampler={}",
            r.is_ok(),
            o.MaxViewDescriptorHeapSize,
            o.MaxSamplerDescriptorHeapSize
        );
        let mut b = D3D12_FEATURE_DATA_D3D12_OPTIONS::default();
        // SAFETY: as above.
        let _ = unsafe {
            host.device.CheckFeatureSupport(
                D3D12_FEATURE_D3D12_OPTIONS,
                (&raw mut b).cast(),
                size_of_val(&b) as u32,
            )
        };
        eprintln!("binding tier {}", b.ResourceBindingTier.0);
        let fits = |n: u32| -> bool {
            // SAFETY: plain heap creation; the heap is released at once.
            let made: windows::core::Result<ID3D12DescriptorHeap> = unsafe {
                host.device
                    .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                        Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                        NumDescriptors: n,
                        Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                        NodeMask: 0,
                    })
            };
            made.is_ok()
        };
        let (mut lo, mut hi) = (1_000_000u32, 1 << 22);
        if !fits(lo) {
            eprintln!("no view heap of 1,000,000");
            return;
        }
        while lo + 1 < hi {
            let mid = lo + (hi - lo) / 2;
            if fits(mid) { lo = mid } else { hi = mid }
        }
        eprintln!("largest view heap {lo}");
    }
}
