//! A view our fresh descriptor tables hold is made once per recording of a
//! list and copied after (`VICTORY_LIST_VIEWS=1`; off unless set).
//!
//! The title's culling and lighting dispatches each bind a table of their
//! own that differs in one constant buffer, beside the same views: a table
//! made afresh (`crate::draw::State::tables`) made all its views anew for
//! every dispatch. Here a thread keeps the descriptors it made for the list
//! it records, by the record they were made from, in a heap the card does
//! not see, and copies a descriptor into the table's slot when the same
//! record comes again. Everything else a table's view gets for its draw
//! (made ready, `readable`; noted for the texture queue) is done as before.
//!
//! Source: DoubleGhosty/cool_fox 4e77a8795b, the view cache alone (not its
//! skip of `readable` within a list's epoch). Their numbers: the recording
//! job the frame waits for last, 2,500 indirect dispatches, 24 of 65 ms
//! before; no after given.

use std::collections::HashMap;

use windows::Win32::Graphics::Direct3D12::*;

use super::{HostHeap, Kept, View, make};

/// The switch (`VICTORY_LIST_VIEWS`; on only when `1`).
pub(crate) fn on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| switch(std::env::var_os("VICTORY_LIST_VIEWS").as_deref()))
}

fn switch(value: Option<&std::ffi::OsStr>) -> bool {
    value.is_some_and(|v| v == "1")
}

/// The descriptors this thread made for one recording of a list, by the
/// record words they were made from.
struct ListViews {
    recording: u64,
    heap: ID3D12DescriptorHeap,
    step: usize,
    start: usize,
    made: Made,
}

/// By record words: the slot in [`ListViews`]'s heap and what it keeps alive.
type Made = HashMap<[u32; 3], (u32, Kept)>;

/// Descriptors a thread keeps for one recording; past this many, a new
/// record is made in the table's slot as before.
const SLOTS: u32 = 8192;

impl ListViews {
    fn new() -> Option<ListViews> {
        let host = crate::host::host()?;
        // SAFETY: plain object creation; a heap the card does not see.
        unsafe {
            let heap: ID3D12DescriptorHeap = host
                .device
                .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                    NumDescriptors: SLOTS,
                    ..Default::default()
                })
                .ok()?;
            let step = host
                .device
                .GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV)
                as usize;
            let start = heap.GetCPUDescriptorHandleForHeapStart().ptr;
            Some(ListViews {
                recording: 0,
                heap,
                step,
                start,
                made: HashMap::new(),
            })
        }
    }

    fn slot(&self, i: u32) -> D3D12_CPU_DESCRIPTOR_HANDLE {
        debug_assert!(i < SLOTS && !windows::core::Interface::as_raw(&self.heap).is_null());
        D3D12_CPU_DESCRIPTOR_HANDLE {
            ptr: self.start + i as usize * self.step,
        }
    }
}

thread_local! {
    static LIST_VIEWS: std::cell::RefCell<Option<ListViews>> = const { std::cell::RefCell::new(None) };
}

/// What to do with record `words` in a recording whose views are `made`:
/// copy the descriptor from slot `Some(at)`, make it in slot `at` first
/// (`Some` with `new` true), or make it in the table's slot as before
/// (`None`: no room, or a view not kept so).
fn place(made: &Made, words: &[u32; 3], keepable: bool) -> Option<(u32, bool)> {
    if let Some(&(at, _)) = made.get(words) {
        return Some((at, false));
    }
    let at = made.len() as u32;
    (keepable && at < SLOTS).then_some((at, true))
}

/// Whether a view is made once and copied: not a constant buffer's (its
/// bytes are copied for each draw), not a sampler.
fn keepable(view: &View) -> bool {
    !matches!(
        view,
        View::Constant(..) | View::Sampler(_) | View::ConsoleSampler(_)
    )
}

impl HostHeap {
    /// Makes slot `index` (a dynamic one) the view the title's record
    /// `words` names, from this thread's descriptor of it if recording
    /// `recording` (of a list) made one, else made there first. False: the
    /// slot is not made (no such view, not one kept so, no room); the
    /// caller makes it as before.
    pub(crate) fn make_listed(&self, recording: u64, words: [u32; 3], index: u32) -> bool {
        if self.sampler || recording == 0 || words[0] != super::VIEW_TAG {
            return false;
        }
        LIST_VIEWS.with(|views| {
            let mut views = views.borrow_mut();
            if views.is_none() {
                *views = ListViews::new();
            }
            let Some(views) = views.as_mut() else {
                return false;
            };
            if views.recording != recording {
                views.recording = recording;
                views.made.clear();
            }
            let at = match views.made.get(&words) {
                Some(&(at, _)) => at,
                None => {
                    let Some(view) = super::view_at(words[1], words[2]) else {
                        return false;
                    };
                    let Some((at, _)) = place(&views.made, &words, keepable(&view)) else {
                        return false;
                    };
                    let Some((kept, _)) = make(&view, views.slot(at)) else {
                        return false;
                    };
                    views.made.insert(words, (at, kept));
                    at
                }
            };
            let Some(host) = crate::host::host() else {
                return false;
            };
            // SAFETY: a slot of our heap, and one of this thread's heap.
            unsafe {
                host.device.CopyDescriptorsSimple(
                    1,
                    self.cpu(index),
                    views.slot(at),
                    D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                )
            };
            self.keep(index, views.made[&words].1.clone());
            true
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_switch_is_off_unless_one() {
        assert!(!switch(None));
        assert!(!switch(Some(std::ffi::OsStr::new("0"))));
        assert!(switch(Some(std::ffi::OsStr::new("1"))));
    }

    /// A record made before is copied from its slot; a new one gets the
    /// next slot while there is room; one not kept so never does.
    #[test]
    fn a_record_is_made_once_then_copied() {
        let mut made = Made::new();
        assert_eq!(place(&made, &[1, 2, 3], true), Some((0, true)));
        made.insert([1, 2, 3], (0, None));
        assert_eq!(place(&made, &[1, 2, 3], true), Some((0, false)));
        assert_eq!(place(&made, &[1, 2, 4], true), Some((1, true)));
        assert_eq!(place(&made, &[1, 2, 4], false), None);
        for i in 1..SLOTS {
            made.insert([9, i, 0], (i, None));
        }
        assert_eq!(place(&made, &[1, 2, 5], true), None);
        assert_eq!(place(&made, &[1, 2, 3], true), Some((0, false)));
    }

    /// Constant buffers and samplers are made for each table.
    #[test]
    fn constant_buffers_and_samplers_are_not_kept() {
        assert!(!keepable(&View::Constant(0x1000, 256)));
        assert!(!keepable(&View::Sampler([0; 13])));
        assert!(keepable(&View::Shader(1, None)));
        assert!(keepable(&View::Unordered(1, 0, None)));
    }

    /// Without a recording (0) or for a record that is not a view's,
    /// nothing is made.
    #[test]
    fn nothing_is_made_without_a_recording() {
        let Some(heap) = HostHeap::new(false, 4) else {
            return; // no host GPU
        };
        assert!(!heap.make_listed(0, [super::super::VIEW_TAG, 0, 0], 0));
        assert!(!heap.make_listed(1, [0, 0, 0], 0));
    }
}
