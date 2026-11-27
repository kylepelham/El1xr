//! `VICTORY_HEAP_SYNC_BUSY_LATER=1`: the heap-sync mirroring does not wait
//! for a texture's lock.
//!
//! Mirroring a title record that views a texture object first asks whether
//! the texture still waits for its host copy ([`crate::resource::host_waits`]),
//! which takes the texture's lock. The texture prepare threads hold that lock
//! while they make the host copy (about 3 ms each in the world streaming
//! window, hundreds a second), and the title writes the views of the very
//! textures being made into its heap. The `victory-heap-sync` worker then
//! stood at the lock, holding the heap's records lock (the draws' binds wait
//! on that), while the submitting thread waited for its round: in batch 3
//! the pass's mirroring took 10-85 ms a frame in the streaming window against
//! 1-3 ms of reading, with no tie to the number of records mirrored.
//!
//! With the switch, a texture whose lock is held is taken as waiting: the
//! slot goes on as a slot of a texture without its host copy does (it is
//! noted in `WAITING`, and keeps the view it showed or is made null), and the
//! texture is put in [`BUSY`]. [`ready`], at the next `make_waiting` (on the
//! submitting thread, before its lists run), asks again without waiting: a
//! texture with its host copy by then has its slots made there; one still
//! without it waits as any other (its making calls `texture_made`); one still
//! held is put off to the next `make_waiting`.
#![cfg(windows)]

use super::View;
use crate::object::This;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

pub(super) fn on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_HEAP_SYNC_BUSY_LATER").is_some_and(|v| v == "1"))
}

/// Textures whose lock a mirroring found held, for [`ready`].
static BUSY: Mutex<Vec<usize>> = Mutex::new(Vec::new());

/// [0] records whose texture's lock was held when mirrored, [1] µs waited
/// for it (switch off), [2] textures [`ready`] found made, [3] found still
/// held and put off again.
static COUNTS: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

/// The texture object the view is of, if that texture has no host copy
/// yet (`crate::resource::host_waits`): a slot written with such a view is
/// left null until the texture is first used. And whether its lock was
/// held (with the switch on: not waited for, and taken as waiting).
pub(super) fn waiting_texture(view: &View) -> Option<(usize, bool)> {
    waiting_texture_in(view, on())
}

fn waiting_texture_in(view: &View, on: bool) -> Option<(usize, bool)> {
    let (View::Shader(r, _) | View::Unordered(r, _, _)) = *view else {
        return None;
    };
    match crate::resource::host_waits_now(r as This) {
        Some(waits) => waits.then_some((r, false)),
        None => {
            COUNTS[0].fetch_add(1, Relaxed);
            if on {
                return Some((r, true));
            }
            let began = std::time::Instant::now();
            let waits = crate::resource::host_waits(r as This);
            COUNTS[1].fetch_add(began.elapsed().as_micros() as u64, Relaxed);
            waits.then_some((r, false))
        }
    }
}

/// The slot at `handle` waits for texture `resource`'s host copy
/// (`busy`: only for all the mirroring knew, its lock being held).
pub(super) fn wait_for(resource: usize, handle: u64, busy: bool) {
    if !busy {
        super::wait_for(resource, handle);
        return;
    }
    {
        let mut waiting = super::WAITING.lock().unwrap_or_else(|e| e.into_inner());
        let slots = waiting.entry(resource).or_default();
        if !slots.contains(&handle) {
            slots.push(handle);
        }
    }
    BUSY.lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(resource);
}

/// Of the textures put in [`BUSY`], those with their host copy now, whose
/// waiting slots `make_waiting` makes. Never waits for a texture's lock.
pub(super) fn ready() -> Vec<usize> {
    if !on() {
        return Vec::new();
    }
    ready_now()
}

fn ready_now() -> Vec<usize> {
    let mut busy = std::mem::take(&mut *BUSY.lock().unwrap_or_else(|e| e.into_inner()));
    if busy.is_empty() {
        return busy;
    }
    busy.sort_unstable();
    busy.dedup();
    let (mut made, mut again) = (Vec::new(), Vec::new());
    {
        // Held while the textures are looked at: a texture the title
        // releases leaves `WAITING` first (`forget`), so one still in it
        // is alive.
        let waiting = super::WAITING.lock().unwrap_or_else(|e| e.into_inner());
        for resource in busy {
            // Not there: released, or made and its slots taken already.
            if !waiting.contains_key(&resource) {
                continue;
            }
            match crate::resource::host_waits_now(resource as This) {
                Some(false) => made.push(resource),
                // Waits as any texture without its host copy does.
                Some(true) => {}
                None => again.push(resource),
            }
        }
    }
    COUNTS[2].fetch_add(made.len() as u64, Relaxed);
    COUNTS[3].fetch_add(again.len() as u64, Relaxed);
    if !again.is_empty() {
        BUSY.lock().unwrap_or_else(|e| e.into_inner()).extend(again);
    }
    made
}

/// With the frame line.
pub(super) fn line(frames: f64) {
    let [held, waited, made, again] = COUNTS.each_ref().map(|n| n.swap(0, Relaxed));
    if held == 0 && made == 0 && again == 0 {
        return;
    }
    tracing::info!(
        "texture locks the heap-sync mirroring found held per frame (VICTORY_HEAP_SYNC_BUSY_LATER={}): {:.1} records, \
         {:.2} ms waited for them (off; on: not waited for, the slots wait for make_waiting); {:.1} of those textures \
         made by make_waiting, {:.1} still held there and put off again",
        u8::from(on()),
        held as f64 / frames,
        waited as f64 / frames / 1000.0,
        made as f64 / frames,
        again as f64 / frames,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tests share `BUSY` and `ready_now` takes all of it.
    static SERIAL: Mutex<()> = Mutex::new(());

    fn in_waiting(resource: usize, handle: u64) -> bool {
        super::super::WAITING
            .lock()
            .unwrap()
            .get(&resource)
            .is_some_and(|slots| slots.contains(&handle))
    }

    fn in_busy(resource: usize) -> bool {
        BUSY.lock().unwrap().contains(&resource)
    }

    /// A texture whose lock is held is not waited for with the switch on
    /// (and is, as before, with it off); its slot waits; `ready` puts it
    /// off while the lock is held, leaves it waiting as any texture once it
    /// is seen without its host copy, and skips one no longer in `WAITING`.
    /// None of it waits for the lock.
    #[test]
    fn a_held_texture_lock_is_not_waited_for() {
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let image = crate::resource::test_lazy_texture();
        let r = image as usize;
        let view = View::Shader(r, None);
        let handle = 0x7e57_0000_u64;
        assert_eq!(
            crate::resource::host_waits_now(image),
            Some(crate::resource::host_waits(image)),
            "free: as host_waits says"
        );
        assert!(crate::resource::host_waits(image), "lazy: no host copy yet");
        assert_eq!(waiting_texture_in(&view, true), Some((r, false)));
        let held = crate::resource::test_hold_texture(image);
        assert_eq!(crate::resource::host_waits_now(image), None);
        assert_eq!(waiting_texture_in(&view, true), Some((r, true)));
        wait_for(r, handle, true);
        assert!(in_waiting(r, handle), "the slot waits");
        assert!(in_busy(r));
        assert!(ready_now().is_empty(), "held: put off");
        assert!(in_busy(r), "and asked again next time");
        assert!(in_waiting(r, handle));
        drop(held);
        // Off: waited for, as `host_waits` says (free now).
        assert_eq!(waiting_texture_in(&view, false), Some((r, false)));
        // Free and still without its host copy: waits as any texture does.
        assert!(ready_now().is_empty());
        assert!(!in_busy(r));
        assert!(in_waiting(r, handle), "its making calls texture_made");
        // Not in `WAITING` any more (released, or taken by make_waiting).
        super::super::WAITING.lock().unwrap().remove(&r);
        BUSY.lock().unwrap().push(r);
        assert!(ready_now().is_empty());
        assert!(!in_busy(r));
        crate::resource::test_release_texture(image);
    }

    /// A texture that has its host copy once its lock is let go is handed
    /// to `make_waiting`, which makes its waiting slots.
    #[test]
    fn a_texture_made_meanwhile_is_ready() {
        if crate::host::host().is_none() {
            return; // no host GPU here
        }
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let image = crate::resource::test_lazy_texture();
        let r = image as usize;
        let handle = 0x7e58_0000_u64;
        let held = crate::resource::test_hold_texture(image);
        wait_for(r, handle, true);
        drop(held);
        assert!(crate::resource::host_copy(image).is_some(), "made");
        assert_eq!(crate::resource::host_waits_now(image), Some(false));
        let made = ready_now();
        assert!(made.contains(&r));
        assert!(!in_busy(r));
        super::super::WAITING.lock().unwrap().remove(&r);
        crate::resource::test_release_texture(image);
    }
}
