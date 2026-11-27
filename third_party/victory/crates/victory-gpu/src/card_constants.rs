//! Constant buffers that read what a title buffer copy on the card wrote
//! earlier in the same list (`VICTORY_CARD_COPY_CONSTANTS=1`, off by default).
//!
//! A copy whose source shaders write runs on the card at its place in the
//! list (`crate::list::card_copy`, `VICTORY_CARD_COPIES`); its bytes reach
//! title memory only once the list has run and been copied back. A constant
//! buffer filled on the CPU at submission (`HostList::defer_constants`,
//! `HostList::constant_scratch`, `HostList::fill_root_cbv`) reads title
//! memory before the list runs, so a draw recorded after the copy saw the
//! constants from before it: one submission stale.
//!
//! With the switch each card copy's destination range is noted per list, and
//! a constant buffer that lies wholly inside the most recent noted write that
//! meets it is copied on the card, at its place in the list, from the
//! destination's host copy into a buffer of the list's own (256-byte slots),
//! and read from there. Anything else (a partial overlap, a CPU copy over the
//! range after the card copy) keeps the CPU fill as before.
//!
//! Source: trustno1-coder/VIctory-NVIDIA PR #34 (`af242dab13`, k0ma2k15),
//! "snapshot placed-copy constants on GPU", adapted from upstream's
//! `place.rs` (`Step::Placed`) to Neptune's card copies. GPL-3.0.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use windows::Win32::Graphics::Direct3D12::{
    D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_GENERIC_READ, ID3D12Resource,
};

use crate::host::HostList;
use crate::shadow::Shadow;

/// Constant-buffer views need 256-byte aligned addresses.
const SLOT_ALIGN: u64 = 256;
/// The least size of a list's snapshot buffer.
const SLAB_MIN: u64 = 64 << 10;
/// Noted writes kept per list; past it the oldest go (the CPU fill again).
const MOST_NOTED: usize = 256;

/// Whether the switch is on (`VICTORY_CARD_COPY_CONSTANTS=1`).
pub(crate) fn on() -> bool {
    #[cfg(test)]
    if tests::FORCED.load(Ordering::Relaxed) {
        return true;
    }
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| parse_switch(std::env::var("VICTORY_CARD_COPY_CONSTANTS").ok().as_deref()))
}

fn parse_switch(value: Option<&str>) -> bool {
    value.is_some_and(|v| v.trim() == "1")
}

/// Since the last frame line: card copies noted, constant buffers snapshotted
/// on the card, and those that met a noted write without lying inside it.
static STATS: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

/// What a list knows of its card copies, until it is reset.
#[derive(Default)]
pub(crate) struct PerList {
    /// Whether `inner.written` may hold something (no lock taken otherwise).
    any: AtomicBool,
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    /// Ranges `[dst, end)` card copies wrote, in recorded order, with the
    /// destination's host copy.
    written: Vec<(usize, usize, Arc<Shadow>)>,
    /// The buffer snapshots go into now: (buffer, capacity, used).
    slab: Option<(ID3D12Resource, u64, u64)>,
    /// Earlier, full buffers, kept until the list is reset.
    full: Vec<ID3D12Resource>,
}

impl PerList {
    fn inner(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A card copy wrote `[dst, dst + len)` of `shadow` at this place.
    pub(crate) fn wrote(&self, dst: usize, len: usize, shadow: &Arc<Shadow>) {
        if !on() || len == 0 {
            return;
        }
        let mut inner = self.inner();
        if inner.written.len() >= MOST_NOTED {
            inner.written.remove(0);
        }
        inner
            .written
            .push((dst, dst.saturating_add(len), shadow.clone()));
        self.any.store(true, Ordering::Relaxed);
        STATS[0].fetch_add(1, Ordering::Relaxed);
    }

    /// A copy on the CPU at submission writes `[dst, dst + len)`: what card
    /// copies wrote there is no longer the last word.
    pub(crate) fn overwritten(&self, dst: usize, len: usize) {
        if !self.any.load(Ordering::Relaxed) {
            return;
        }
        let end = dst.saturating_add(len);
        self.inner()
            .written
            .retain(|w| !meets((w.0, w.1), (dst, end)));
    }

    /// The list was reset: its commands have run.
    pub(crate) fn clear(&self) {
        if !self.any.swap(false, Ordering::Relaxed) {
            return;
        }
        *self.inner() = Inner::default();
    }
}

fn meets(a: (usize, usize), b: (usize, usize)) -> bool {
    a.0 < b.1 && b.0 < a.1
}

/// Of `written` (in recorded order), the most recent that meets `[from, to)`,
/// if it holds all of it; `Err(())` if it meets it only in part.
fn covering<T>(written: &[(usize, usize, T)], from: usize, to: usize) -> Result<Option<&T>, ()> {
    match written.iter().rev().find(|w| meets((w.0, w.1), (from, to))) {
        None => Ok(None),
        Some(w) if w.0 <= from && to <= w.1 => Ok(Some(&w.2)),
        Some(_) => Err(()),
    }
}

/// Where `alloc` bytes go in a buffer of `cap` bytes with `used` taken.
fn slot_in(cap: u64, used: u64, alloc: u64) -> Option<u64> {
    (used.checked_add(alloc)? <= cap).then_some(used)
}

/// For a constant buffer of `len` bytes at `address` read at this place of
/// `list`: a copy of it made on the card, as `(buffer, GPU address)` in the
/// generic read state, if a card copy earlier in the list wrote all of it.
pub(crate) fn snapshot(list: &HostList, address: u64, len: u64) -> Option<(ID3D12Resource, u64)> {
    let per = &list.card_constants;
    if len == 0 || !per.any.load(Ordering::Relaxed) || !on() {
        return None;
    }
    let from = usize::try_from(address).ok()?;
    let to = from.checked_add(usize::try_from(len).ok()?)?;
    let mut inner = per.inner();
    let shadow = match covering(&inner.written, from, to) {
        Ok(Some(shadow)) => shadow.clone(),
        Ok(None) => return None,
        Err(()) => {
            STATS[2].fetch_add(1, Ordering::Relaxed);
            return None;
        }
    };
    if shadow.in_place() || shadow.left(address) < len {
        return None;
    }
    let alloc = len.next_multiple_of(SLOT_ALIGN);
    let reused = inner.slab.as_mut().and_then(|(buffer, cap, used)| {
        let at = slot_in(*cap, *used, alloc)?;
        *used = at + alloc;
        Some((buffer.clone(), at))
    });
    let (buffer, at, fresh) = match reused {
        Some((buffer, at)) => (buffer, at, false),
        None => {
            let cap = alloc.max(SLAB_MIN);
            let buffer = crate::host::host()?.uav_buffer(cap)?;
            crate::host::name(&buffer, || {
                "constants copied on the card after a card copy".into()
            });
            if let Some((old, ..)) = inner.slab.replace((buffer.clone(), cap, alloc)) {
                inner.full.push(old);
            }
            (buffer, 0, true)
        }
    };
    drop(inner);
    // SAFETY: an open list; `buffer` is held by `list` until its reset and
    // `at + len` lies inside it; the host copy holds `[address, address +
    // len)` (`left`) and lives as long as the list's note of it.
    unsafe {
        let mut before = vec![crate::indirect::uav_barrier()];
        if !fresh {
            // A fresh buffer is in the common state and goes to the copy's
            // by itself; a used one was left readable.
            before.push(crate::host::transition(
                &buffer,
                D3D12_RESOURCE_STATE_GENERIC_READ,
                D3D12_RESOURCE_STATE_COPY_DEST,
            ));
        }
        list.list.ResourceBarrier(&before);
        list.list
            .CopyBufferRegion(&buffer, at, &shadow.buffer, shadow.offset(address), len);
        list.list.ResourceBarrier(&[crate::host::transition(
            &buffer,
            D3D12_RESOURCE_STATE_COPY_DEST,
            D3D12_RESOURCE_STATE_GENERIC_READ,
        )]);
        STATS[1].fetch_add(1, Ordering::Relaxed);
        Some((buffer.clone(), buffer.GetGPUVirtualAddress() + at))
    }
}

/// The frame line's, with the switch on.
pub(crate) fn line(frames: f64) {
    if !on() {
        return;
    }
    let [noted, taken, partial] = STATS
        .each_ref()
        .map(|a| a.swap(0, Ordering::Relaxed) as f64 / frames);
    tracing::info!(
        "constants after card copies per frame (VICTORY_CARD_COPY_CONSTANTS=1): {noted:.1} card copies noted, \
         {taken:.1} constant buffers copied on the card from what they wrote, {partial:.1} meeting one only in part \
         (filled on the CPU as before)"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The switch, on for the test that needs it.
    pub(super) static FORCED: AtomicBool = AtomicBool::new(false);

    /// A private allocation standing for a title buffer, registered as one.
    struct Title(usize, usize);

    impl Title {
        fn new(len: usize, key: usize) -> Self {
            use windows_sys::Win32::System::Memory::{
                MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc,
            };
            // SAFETY: a fresh private allocation.
            let at = unsafe {
                VirtualAlloc(
                    std::ptr::null(),
                    len,
                    MEM_COMMIT | MEM_RESERVE,
                    PAGE_READWRITE,
                )
            } as usize;
            assert_ne!(at, 0);
            crate::shadow::register(key, at as u64, len as u64);
            Self(at, key)
        }
    }

    impl Drop for Title {
        fn drop(&mut self) {
            use windows_sys::Win32::System::Memory::{MEM_RELEASE, VirtualFree};
            crate::shadow::unregister(self.1, self.0 as u64);
            // SAFETY: our own allocation, its lists run.
            unsafe { VirtualFree(self.0 as *mut _, 0, MEM_RELEASE) };
        }
    }

    /// A card copy as `crate::list::card_copy` records one, then constant
    /// buffers inside its destination read what it wrote; the CPU fill's
    /// title memory still holds zeros there.
    #[test]
    fn a_constant_buffer_after_a_card_copy_reads_what_it_wrote() {
        let Some(host) = crate::host::host() else {
            return;
        };
        FORCED.store(true, Ordering::Relaxed);
        const LEN: usize = 64 << 10;
        let (from, to) = (Title::new(LEN, 0x5eed_0001), Title::new(LEN, 0x5eed_0002));
        let (src, dst) = (from.0 + 4096, to.0 + 8192);
        // SAFETY: inside the committed allocation.
        unsafe {
            for i in 0..512 {
                ((src + i) as *mut u8).write((i.wrapping_mul(37) ^ 0xa5) as u8);
            }
        }
        let source = crate::shadow::find(src as u64, 512).expect("source copy");
        let target = crate::shadow::find(dst as u64, 512).expect("target copy");
        let list = host.new_list().unwrap();
        source.refresh(&list, src as u64, 512);
        target.refresh(&list, dst as u64, 512);
        // SAFETY: an open list; both host copies are held by its refreshes.
        unsafe {
            list.list.ResourceBarrier(&[crate::indirect::uav_barrier()]);
            list.list.CopyBufferRegion(
                &target.buffer,
                target.offset(dst as u64),
                &source.buffer,
                source.offset(src as u64),
                512,
            );
            list.list.ResourceBarrier(&[
                crate::indirect::uav_barrier(),
                crate::host::transition(
                    &target.buffer,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                    windows::Win32::Graphics::Direct3D12::D3D12_RESOURCE_STATE_COMMON,
                ),
            ]);
        }
        list.card_constants.wrote(dst, 512, &target);
        // In part, or elsewhere: the CPU fill as before.
        assert!(snapshot(&list, dst as u64 - 16, 64).is_none());
        assert!(snapshot(&list, dst as u64 + 480, 64).is_none());
        assert!(snapshot(&list, to.0 as u64 + 16384, 64).is_none());
        let (b1, g1) = snapshot(&list, dst as u64, 256).expect("first half");
        let (b2, g2) = snapshot(&list, dst as u64 + 256, 256).expect("second half");
        assert_eq!((g1 % 256, g2 % 256), (0, 0));
        let (out, out_ptr) = host.shared_buffer(512).unwrap();
        // SAFETY: an open list; the offsets lie inside the snapshot buffers.
        unsafe {
            let base = |b: &ID3D12Resource| b.GetGPUVirtualAddress();
            list.list
                .CopyBufferRegion(&out, 0, &b1, g1 - base(&b1), 256);
            list.list
                .CopyBufferRegion(&out, 256, &b2, g2 - base(&b2), 256);
        }
        list.close();
        host.execute(&[&list]);
        // SAFETY: a 512-byte shared buffer's mapping; `src` inside its allocation.
        let (got, want) = unsafe {
            (
                std::slice::from_raw_parts(out_ptr, 512),
                std::slice::from_raw_parts(src as *const u8, 512),
            )
        };
        assert_eq!(got, want);
        // A CPU copy over the bytes afterwards: no longer the card copy's.
        list.reset();
        assert!(snapshot(&list, dst as u64, 256).is_none());
        list.card_constants.wrote(dst, 512, &target);
        list.card_constants.overwritten(dst + 100, 4);
        assert!(snapshot(&list, dst as u64, 256).is_none());
        list.close();
        list.reset();
        FORCED.store(false, Ordering::Relaxed);
        drop((list, source, target));
    }

    #[test]
    fn the_switch_is_one_only() {
        assert!(parse_switch(Some("1")));
        assert!(parse_switch(Some(" 1 ")));
        assert!(!parse_switch(Some("0")));
        assert!(!parse_switch(Some("")));
        assert!(!parse_switch(None));
    }

    #[test]
    fn the_last_write_that_meets_a_constant_buffer_must_hold_it() {
        let written = [
            (0x1000, 0x1200, 'a'),
            (0x3000, 0x3100, 'b'),
            (0x1100, 0x1180, 'c'),
        ];
        // Inside the first, untouched by later ones.
        assert_eq!(covering(&written, 0x1000, 0x1100), Ok(Some(&'a')));
        // The later, smaller write is the last word on its bytes.
        assert_eq!(covering(&written, 0x1100, 0x1180), Ok(Some(&'c')));
        // Meets the later write in part: the CPU fill.
        assert_eq!(covering(&written, 0x1080, 0x1120), Err(()));
        // Reaches past the write.
        assert_eq!(covering(&written, 0x30f0, 0x3110), Err(()));
        // Touching is not meeting.
        assert_eq!(covering(&written, 0x1200, 0x1300), Ok(None));
        assert_eq!(covering(&written, 0x0f00, 0x1000), Ok(None));
        assert_eq!(covering::<char>(&[], 0, 16), Ok(None));
    }

    #[test]
    fn slots_fill_a_buffer_then_ask_for_another() {
        assert_eq!(slot_in(1024, 0, 256), Some(0));
        assert_eq!(slot_in(1024, 768, 256), Some(768));
        assert_eq!(slot_in(1024, 1024, 256), None);
        assert_eq!(slot_in(1024, 768, 512), None);
        assert_eq!(slot_in(u64::MAX, u64::MAX - 1, 256), None);
    }
}
