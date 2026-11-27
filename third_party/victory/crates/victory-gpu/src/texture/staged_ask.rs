//! (readmet3xt-lab, trustno1-coder/VIctory-NVIDIA `36c4c37`)
//! `VICTORY_STAGED_TITLE_ONLY=1`: whether a staged texture's memory is all
//! there ([`super::staged`]) is asked of Windows only for a texture made at
//! an address the title mapped ([`crate::resource::at_title_address`]); the
//! memory of a committed or heap resource is ours, allocated whole when it
//! was made, and is all there while it lives.
//!
//! The pass over textures picked by index asks it of every texture object it
//! uploads, tabled slots among them (a `QueryWorkingSetEx` per 256 pages and
//! a read of each page not in the working set: 18-111 us for 1-16 MB, and
//! milliseconds where the pages had been trimmed). Upstream counted 20,000
//! to 57,000 such questions per 5 s, 66 of them "not all there" in a whole
//! run, and the pass's unattributed time growing by about 13 us for each.
//! Unset: asked of every texture, as before (the answer for ours is the
//! same; only the time differs).

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// Questions skipped (the texture's memory is ours) since the start.
static SKIPPED: AtomicU64 = AtomicU64::new(0);

fn title_only() -> bool {
    static ON: victory_core::settings::Live = victory_core::settings::Live::flag("VICTORY_STAGED_TITLE_ONLY");
    ON.get_bool()
}

/// Whether Windows is asked whether staged texture `resource`'s memory is
/// all there.
pub(super) fn asks(resource: crate::object::This) -> bool {
    asks_as(title_only(), || crate::resource::at_title_address(resource))
}

fn asks_as(title_only: bool, at_title_address: impl FnOnce() -> bool) -> bool {
    if !title_only || at_title_address() {
        return true;
    }
    let n = SKIPPED.fetch_add(1, Relaxed) + 1;
    if n.is_power_of_two() && n >= 4096 {
        tracing::info!(
            "staged textures of our own memory not asked whether all there, since the start: {n} (VICTORY_STAGED_TITLE_ONLY=1)"
        );
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::{S_OK, guid, slot};
    use windows::Win32::Graphics::Direct3D12::ID3D12Resource;
    use windows::core::Interface;

    const RGBA8_64: [u32; 14] = [3, 0, 0, 0, 64, 0, 64, 1 | 1 << 16, 28, 1, 0, 0, 0, 0];

    #[test]
    fn the_switch_is_live() {
        victory_core::settings::check_live("VICTORY_STAGED_TITLE_ONLY", "1", title_only, false, true);
    }

    /// Unset, every staged texture is asked about and nothing is asked of
    /// the resource; with the switch, only one at a title address.
    #[test]
    fn only_textures_at_a_title_address_are_asked_with_the_switch() {
        assert!(asks_as(false, || panic!("not asked unless the switch is on")));
        assert!(asks_as(true, || true));
        let before = SKIPPED.load(Relaxed);
        assert!(!asks_as(true, || false));
        assert!(SKIPPED.load(Relaxed) > before);
    }

    /// Only a resource made at an address the title mapped can lose its
    /// memory while it lives.
    #[test]
    fn only_resources_at_a_title_address_may_lose_their_memory() {
        let i = ID3D12Resource::IID;
        let iid = guid(i.data1, i.data2, i.data3, i.data4);
        let layout = std::alloc::Layout::from_size_align(64 << 10, 1 << 16).unwrap();
        // SAFETY: a non-zero size; leaked, as title memory outlives a test.
        let title = unsafe { std::alloc::alloc_zeroed(layout) as u64 };
        let mut at = std::ptr::null();
        let made = crate::resource::create_at_address(
            std::ptr::null(),
            title,
            RGBA8_64.as_ptr().cast(),
            0,
            std::ptr::null(),
            &iid,
            &mut at,
        );
        assert_eq!(made, S_OK);
        // SAFETY: plain integer structs with matching `#[repr(C)]` layouts.
        let desc: crate::resource::ResourceDesc = unsafe { std::mem::transmute(RGBA8_64) };
        let props: crate::resource::HeapProperties = unsafe { std::mem::transmute([1u32, 0, 0, 1, 1]) };
        let mut committed = std::ptr::null();
        let made = crate::resource::create_committed(
            std::ptr::null(),
            &props,
            0,
            &desc,
            0,
            std::ptr::null(),
            &iid,
            &mut committed,
        );
        assert_eq!(made, S_OK);
        assert!(crate::resource::at_title_address(at));
        assert!(!crate::resource::at_title_address(committed));
        assert!(!crate::resource::at_title_address(std::ptr::null()));
        for r in [at, committed] {
            let _ = slot::<extern "system" fn(crate::object::This) -> u32>(r, 0x10)(r);
        }
    }
}
