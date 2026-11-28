//! (El1xr) The pass of `VICTORY_IDLE_EVICT_S` ([`crate::idle_evict`]): host copies of
//! textures the title fills that only table slots still show, idle, with their title memory
//! unchanged since their upload, let go with those slots.
//!
//! The title textures are gathered under the table's lock, which is let go for the pass over
//! the heaps' slots (it takes a lock a slot); each pick is then taken under the table's lock
//! again, only if it is still in it with the same host copy, still idle and held by no list.

use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::Ordering::Relaxed;

use super::{KINDS, Resource, fills, oldest_covering, textures};
use crate::idle_evict::{self, Plan};
use crate::object::This;

/// A copy that may go: the title texture, its last use, bytes, and its host texture.
struct Pick {
    at: usize,
    used: u64,
    bytes: u64,
    raw: usize,
}

/// Whether `r`'s host copy is one the pass may let go once nothing holds it: filled by the
/// title alone, all of it from title memory (not made level by level, nor whole from the
/// store), without a twin.
fn plain_copy(r: &Resource, host: &super::HostTexture, twin: bool) -> bool {
    host.filled
        && !twin
        && host.tiled.is_none()
        && host.prefilled.load(Relaxed) == 0
        && host
            .prefill
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_none()
        && fills(&r.desc)
}

/// One pass: see the module.
pub(crate) fn sweep(plan: Plan) {
    let started = std::time::Instant::now();
    let mut census = [0u64; 8];
    let mut picks = Vec::new();
    {
        let all = textures();
        for &at in all.iter() {
            // SAFETY: a texture leaves the table before it is freed, under the lock held here.
            let r = unsafe { &*(at as *const Resource) };
            if !fills(&r.desc) || crate::texture::host_wrote(at) {
                continue;
            }
            let made = r.host.made.lock().unwrap_or_else(|e| e.into_inner());
            let Some((host, twin)) = made.as_ref() else {
                continue;
            };
            let used = r.host.used.load(Relaxed);
            if !crate::host::long_ago(used, plan.idle_ms, plan.frames)
                || !plain_copy(r, host, twin.is_some())
            {
                continue;
            }
            if std::sync::Arc::strong_count(host) > 1 {
                census[idle_evict::BY_LIST] += host.bytes;
                continue;
            }
            if idle_evict::protected(at) {
                census[idle_evict::PROTECTED] += host.bytes;
                continue;
            }
            picks.push(Pick {
                at,
                used,
                bytes: host.bytes,
                raw: windows::core::Interface::as_raw(&host.texture) as usize,
            });
        }
    }
    let raws: HashMap<usize, usize> = picks.iter().map(|p| (p.raw, p.at)).collect();
    let (slots, blocked) =
        crate::descriptor::slots_by_class(&raws, crate::frame::current(), plan.wide);
    let mut copies = Vec::new();
    let mut raw_of = HashMap::new();
    for p in &picks {
        if !slots.contains_key(&p.at) {
            // No slot at all: the idle sweep's (a list let go of it since); else what blocks.
            match blocked.get(&p.at) {
                Some(&crate::descriptor::SlotClass::Earlier) => {
                    census[idle_evict::EARLIER] += p.bytes
                }
                Some(_) => census[idle_evict::NEVER] += p.bytes,
                None => {}
            }
            continue;
        }
        match crate::texture::unchanged_since_upload(p.at as This) {
            Some(true) => {
                census[idle_evict::LATELY] += p.bytes;
                copies.push((p.at, p.used, p.bytes));
                raw_of.insert(p.at, p.raw);
            }
            Some(false) => census[idle_evict::CHANGED] += p.bytes,
            None => census[idle_evict::UNKNOWN] += p.bytes,
        }
    }
    for at in oldest_covering(copies, plan.need) {
        let all = textures();
        if !all.contains(&at) {
            continue;
        }
        let _ = take(
            &all,
            at,
            raw_of[&at],
            &slots[&at],
            plan.idle_ms,
            plan.frames,
        );
    }
    idle_evict::passed();
    census[idle_evict::PASS_US] = started.elapsed().as_micros() as u64;
    for (to, v) in idle_evict::CENSUS.iter().zip(census) {
        to.store(v, Relaxed);
    }
}

/// Lets go of title texture `at`'s host copy (host texture `raw`) with its slots `slots`
/// (heap, index), if it is still that copy, idle for `idle_ms` and `frames` and held by no
/// list; else its slots are made again before the next submission. Its bytes if it went.
/// `all`: the table's lock, held.
fn take(
    all: &BTreeSet<usize>,
    at: usize,
    raw: usize,
    slots: &[(usize, u32)],
    idle_ms: u64,
    frames: u64,
) -> Option<u64> {
    debug_assert!(all.contains(&at));
    // SAFETY: in the table, whose lock the caller holds.
    let r = unsafe { &*(at as *const Resource) };
    let reset = crate::descriptor::evict_slots(at, slots);
    let gone = {
        let mut made = r.host.made.lock().unwrap_or_else(|e| e.into_inner());
        let idle = made.as_ref().is_some_and(|(host, _)| {
            windows::core::Interface::as_raw(&host.texture) as usize == raw
                && crate::host::long_ago(r.host.used.load(Relaxed), idle_ms, frames)
                && std::sync::Arc::strong_count(host) == 1
        });
        if idle {
            crate::table_null::unstaged(at, crate::table_null::Why::Evicted);
            crate::texture::unstage(at as This);
            let gone = made.take();
            super::bump_view_epoch();
            gone
        } else {
            None
        }
    };
    match gone {
        Some((host, _)) => {
            let bytes = host.bytes;
            r.host.gone.store(true, Relaxed);
            KINDS[2].fetch_add(1, Relaxed);
            KINDS[3].fetch_add(bytes, Relaxed);
            idle_evict::evicted(at, bytes, reset);
            crate::host::retire(host);
            Some(bytes)
        }
        None => {
            crate::descriptor::texture_made(at);
            None
        }
    }
}

/// For the tests: lets go of `this`'s host copy as a pass would (no slot shows it), its idle
/// time and frames taken as passed.
#[cfg(test)]
pub(crate) fn take_for_test(this: This) -> Option<u64> {
    let all = textures();
    let raw = {
        // SAFETY: a live test texture.
        let r = unsafe { &*(this as *const Resource) };
        let made = r.host.made.lock().unwrap_or_else(|e| e.into_inner());
        windows::core::Interface::as_raw(&made.as_ref()?.0.texture) as usize
    };
    take(&all, this as usize, raw, &[], 0, 0)
}

#[cfg(all(test, windows))]
mod tests {
    use windows::core::Interface;
    use windows::Win32::Graphics::Direct3D12::ID3D12Resource;

    use crate::object::{S_OK, This, guid, slot};

    const RGBA8_64: [u32; 14] = [3, 0, 0, 0, 64, 0, 64, 1 | 1 << 16, 28, 1, 0, 0, 0, 0];

    /// A 64x64 RGBA8 title texture at `address`.
    fn texture_at(address: u64) -> This {
        let i = ID3D12Resource::IID;
        let iid = guid(i.data1, i.data2, i.data3, i.data4);
        let mut out = std::ptr::null();
        let made = crate::resource::create_at_address(
            std::ptr::null(),
            address,
            RGBA8_64.as_ptr().cast(),
            0,
            std::ptr::null(),
            &iid,
            &mut out,
        );
        assert_eq!(made, S_OK);
        out
    }

    /// Title memory of `bytes`, 64 KiB aligned, holding a pattern; leaked, as title memory
    /// outlives a test.
    fn title_memory(bytes: usize) -> u64 {
        let layout = std::alloc::Layout::from_size_align(bytes, 1 << 16).unwrap();
        // SAFETY: a non-zero size.
        let at = unsafe { std::alloc::alloc_zeroed(layout) };
        for i in 0..bytes {
            // SAFETY: inside the allocation.
            unsafe { *at.add(i) = (i * 7 + 3) as u8 };
        }
        at as u64
    }

    fn release(t: This) {
        let _ = slot::<extern "system" fn(This) -> u32>(t, 0x10)(t);
    }

    /// A host copy let go by the pass is made again by the next read, never left without
    /// one (a table makes it at once), of the same shape, and uploads exactly the bytes the
    /// one let go uploaded.
    #[test]
    fn a_texture_let_go_comes_back_identical_and_never_null() {
        let Some(host) = crate::host::host() else {
            return; // no host GPU here
        };
        let t = texture_at(title_memory(1 << 16));
        let list = host.new_list().unwrap();
        let first = crate::resource::host_resource(t).unwrap();
        // SAFETY: a live resource.
        let shape = |r: &ID3D12Resource| {
            let d = unsafe { r.GetDesc() };
            (d.Width, d.Height, d.MipLevels, d.DepthOrArraySize, d.Format)
        };
        let before_shape = shape(first);
        crate::texture::upload(&list, t, first);
        let before = crate::texture::detiled_for_test(t).expect("staged by the upload");
        assert!(before.iter().any(|&b| b != 0), "the pattern was detiled");
        list.close();
        drop(list);
        assert!(super::take_for_test(t).is_some(), "let go");
        assert_eq!(
            crate::resource::host_copy_uploaded_from(t),
            None,
            "no host copy now"
        );
        assert!(
            crate::idle_evict::reads_now(t as usize),
            "a table makes it at once: never a null slot"
        );
        let list = host.new_list().unwrap();
        let again = crate::resource::host_resource(t).expect("made again by the read");
        assert_eq!(shape(again), before_shape);
        crate::texture::upload(&list, t, again);
        let after = crate::texture::detiled_for_test(t).expect("staged again");
        assert_eq!(before, after, "the copy made again holds what the one let go held");
        list.close();
        crate::idle_evict::forget(t as usize);
        release(t);
    }
}
