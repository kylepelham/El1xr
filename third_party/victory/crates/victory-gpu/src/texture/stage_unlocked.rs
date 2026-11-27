//! A staging buffer is made outside the lock of the table of staging
//! buffers (`VICTORY_STAGING_UNLOCKED=1`; off unless set). Making one is an
//! upload buffer (zeroed, or new from the driver) and the footprints, and
//! every recording thread asks the table for every texture it reads, so
//! all of them waited behind each one made. The table is looked at under
//! the lock, the staging made (or taken from the prepare threads) outside
//! it, and put in under the lock again: one another thread put in
//! meanwhile that serves as well wins, and ours goes (its buffer back to
//! the pool). It also no longer asks a texture's host side while the table
//! is locked (the sweep takes them the other way round).

use std::collections::HashMap;
use std::sync::Arc;

use windows::Win32::Graphics::Direct3D12::ID3D12Resource;

use super::{STAGING, Staging, build_staging, prepare};
use crate::object::This;

/// The switch (`VICTORY_STAGING_UNLOCKED`; on only when `1`).
pub(super) fn on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| switch(std::env::var_os("VICTORY_STAGING_UNLOCKED").as_deref()))
}

fn switch(value: Option<&std::ffi::OsStr>) -> bool {
    value.is_some_and(|v| v == "1")
}

/// `super::staging` with the switch on (`first` already 0 for a texture
/// made level by level).
pub(super) fn staging(
    resource: This,
    texture: &ID3D12Resource,
    first: u32,
    on_demand: bool,
) -> Option<Arc<Staging>> {
    {
        let map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(s) = map.as_ref().and_then(|m| m.get(&(resource as usize)))
            && s.first <= first
        {
            return Some(s.clone());
        }
    }
    let made = match prepare::take_ready(resource, first, on_demand) {
        Some(s) => s,
        None => build_staging(resource, texture, first, on_demand)?,
    };
    let kept = {
        let mut map = STAGING.lock().unwrap_or_else(|e| e.into_inner());
        let map = map.get_or_insert_with(HashMap::new);
        let kept = settle(map, resource as usize, made.clone(), |s| s.first);
        if Arc::ptr_eq(&kept, &made) {
            crate::resource::set_staged_first(resource, kept.first);
        }
        kept
    };
    // Ours lost (dropped outside the lock): its buffer goes back to the pool.
    drop(made);
    Some(kept)
}

/// Puts `made` in `map` under `key` unless what is there serves from the
/// same mip or a larger one (`first_of`) already; returns the one kept.
fn settle<T>(
    map: &mut HashMap<usize, Arc<T>>,
    key: usize,
    made: Arc<T>,
    first_of: impl Fn(&T) -> u32,
) -> Arc<T> {
    if let Some(other) = map.get(&key)
        && first_of(other) <= first_of(&made)
    {
        return other.clone();
    }
    map.insert(key, made.clone());
    made
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

    /// The first one in wins a race from the same mip; a later one that
    /// holds more mips replaces one that holds fewer.
    #[test]
    fn the_one_in_the_table_wins_unless_ours_holds_more() {
        let mut map = HashMap::new();
        let a = Arc::new(2u32);
        assert!(Arc::ptr_eq(&settle(&mut map, 1, a.clone(), |f| *f), &a));
        let b = Arc::new(2u32);
        assert!(Arc::ptr_eq(&settle(&mut map, 1, b, |f| *f), &a));
        let c = Arc::new(3u32);
        assert!(Arc::ptr_eq(&settle(&mut map, 1, c, |f| *f), &a));
        let d = Arc::new(0u32);
        assert!(Arc::ptr_eq(&settle(&mut map, 1, d.clone(), |f| *f), &d));
        assert!(Arc::ptr_eq(&map[&1], &d));
    }

    /// Two threads make one for the same texture at once: both get the
    /// same one, and only one stays.
    #[test]
    fn two_threads_get_the_same_one() {
        let map = std::sync::Mutex::new(HashMap::new());
        let got: Vec<Arc<u32>> = std::thread::scope(|s| {
            let made = [0, 1].map(|_| {
                s.spawn(|| {
                    let made = Arc::new(0u32);
                    settle(&mut map.lock().unwrap(), 5, made, |f| *f)
                })
            });
            made.map(|h| h.join().unwrap()).into()
        });
        assert!(Arc::ptr_eq(&got[0], &got[1]));
        assert_eq!(map.lock().unwrap().len(), 1);
    }
}
