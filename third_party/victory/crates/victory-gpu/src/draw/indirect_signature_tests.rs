//! What an indirect execution binds once instead of on every call: the
//! host command signature, and the title's pipeline bound again after the
//! clamp's.

use super::*;

/// A command signature is made once: every thread, and the same thread
/// asking again after another, gets the same object.
#[test]
fn host_signatures_are_made_once() {
    if crate::host::host().is_none() {
        return; // no host GPU here
    }
    let dispatch = host_signature(2, 12).unwrap();
    let draw = host_signature(0, 16).unwrap();
    assert!(!std::ptr::eq(dispatch, draw));
    assert!(std::ptr::eq(host_signature(2, 12).unwrap(), dispatch));
    let other = std::thread::spawn(|| host_signature(2, 12).unwrap() as *const _ as usize)
        .join()
        .unwrap();
    assert_eq!(other, dispatch as *const _ as usize);
    assert_eq!(dispatch.as_raw(), host_signature(2, 12).unwrap().as_raw());
}

/// A list holds the pipeline it was last asked about until its reset: a
/// pipeline bound again after the indirect clamp's is kept once.
#[test]
fn a_list_holds_the_pipeline_it_last_kept_until_its_reset() {
    let Some(host) = crate::host::host() else {
        return; // no host GPU here
    };
    let list = host.new_list().unwrap();
    assert!(!list.holds_pipeline(1));
    assert!(list.holds_pipeline(1));
    assert!(!list.holds_pipeline(2));
    assert!(list.holds_pipeline(1));
    assert!(list.holds_pipeline(2));
    list.close();
    list.reset();
    assert!(!list.holds_pipeline(1));
}

#[test]
fn command_signature_cache_creates_outside_lock() {
    use std::sync::{Arc, Barrier, Mutex};
    static EXISTING: u32 = 100;
    let cache: Arc<Mutex<Vec<(u32, &'static u32)>>> = Arc::new(Mutex::new(vec![(1u32, &EXISTING)]));
    let in_create = Arc::new(Barrier::new(2));
    let proceed = Arc::new(Barrier::new(2));

    let cache_bg = Arc::clone(&cache);
    let in_create_bg = Arc::clone(&in_create);
    let proceed_bg = Arc::clone(&proceed);
    let handle = std::thread::spawn(move || {
        cached_command_signature(&cache_bg, 2, || {
            in_create_bg.wait();
            proceed_bg.wait();
            Some(200u32)
        })
    });

    // Wait until the worker is inside `create`, then verify another thread
    // can still read an existing key from `cache` without blocking.
    in_create.wait();
    assert_eq!(cached_command_signature(&cache, 1, || unreachable!()).copied(), Some(100));
    proceed.wait();

    assert_eq!(handle.join().unwrap().copied(), Some(200));
    assert_eq!(cached_command_signature(&cache, 2, || unreachable!()).copied(), Some(200));
}
