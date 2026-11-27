//! Lets go of the host textures a reset host list alone held without
//! dropping them on the resetting thread (`VICTORY_LIST_REAP=0`: all
//! dropped there, as before).
//!
//! A list holds every host texture (and colour twin) it names until it is
//! reset (`HostList::reset`), so a texture the title released while a list
//! still named it is freed by that reset: a D3D12 `Release()` that destroys
//! the allocation, behind the driver's lock. Lists are reset by
//! `crate::host::land` right after a submission, on the thread that
//! submits and presents (`victory-submit` with `VICTORY_ASYNC_SUBMIT=1`).
//! Sampled in the frames of 100 ms and more after world entry (hitch
//! stacks, 200 s bench), the drops in that reset were 32% of the presenting
//! thread's samples; those of the textures and twins about half of them.
//!
//! Only what no one else can see is handed to [`crate::host::retire`]
//! (dropped on the `victory-reaper` thread): a host texture the list holds
//! the last reference to (no table, slot or other list names it, and no
//! `Weak` to host textures exists) and that has no tiles of the shared
//! heaps (`crate::tiles::Tiled` gives its tiles back when dropped, and a
//! later give-back changes which tiles, with which old texels, the next
//! textures get). Its drop is then counters and the `Release()`, so a later
//! drop changes nothing that is drawn. The rest is dropped here, as before:
//! a reference others still hold is a count decrement, and a tiled texture
//! gives its tiles back at once. The list's deferred work is never handed
//! over: host copies of title memory are reachable through `Weak`s and
//! flush into title memory when dropped, and a level claim is given up by
//! its drop before the next list writes the level. Handing all of it over
//! (with the textures, tiled or not) showed Lucia's hoodie dark in the
//! diner (capture `submithitch/dev/cap_sh_def1`).

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

/// Parses `VICTORY_LIST_REAP`: on unless `"0"`.
fn parse_switch(value: Option<&str>) -> bool {
    value.is_none_or(|v| v.trim() != "0")
}

fn wanted() -> bool {
    static WANTED: OnceLock<bool> = OnceLock::new();
    *WANTED.get_or_init(|| {
        let on = parse_switch(std::env::var("VICTORY_LIST_REAP").ok().as_deref());
        tracing::info!(
            "host textures a reset host list alone held are dropped {} (VICTORY_LIST_REAP={})",
            if on {
                "beside the submitting thread"
            } else {
                "by the thread that resets it"
            },
            u8::from(on)
        );
        on
    })
}

/// Whether `held` is a host texture only this reference names, without
/// tiles of the shared heaps.
pub(crate) fn alone(held: &Arc<crate::resource::HostTexture>) -> bool {
    Arc::strong_count(held) == 1 && held.tiled.is_none()
}

/// Empties `held` (keeping its capacity): the values `unseen` says no one
/// else can see go to the reaper thread when [`wanted`]; the others, and
/// all of them otherwise, are dropped here.
pub(crate) fn let_go<K, V: 'static>(held: &mut HashMap<K, V>, unseen: impl Fn(&V) -> bool) {
    if held.is_empty() {
        return;
    }
    if !wanted() {
        held.clear();
        return;
    }
    let away: Vec<V> = held
        .drain()
        .map(|(_, v)| v)
        .filter_map(|v| if unseen(&v) { Some(v) } else { drop(v); None })
        .collect();
    if !away.is_empty() {
        crate::host::retire(away);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::time::Duration;

    use super::*;

    #[test]
    fn parse_switch_is_on_unless_zero() {
        assert!(parse_switch(None));
        assert!(parse_switch(Some("1")));
        assert!(parse_switch(Some("")));
        assert!(!parse_switch(Some("0")));
        assert!(!parse_switch(Some(" 0 ")));
    }

    struct Tell(u32, mpsc::Sender<(u32, std::thread::ThreadId)>);

    impl Drop for Tell {
        fn drop(&mut self) {
            let _ = self.1.send((self.0, std::thread::current().id()));
        }
    }

    #[test]
    fn only_unseen_values_are_dropped_off_the_resetting_thread() {
        let (send, dropped) = mpsc::channel();
        let mut held = HashMap::new();
        for at in 0..4u32 {
            held.insert(at, Tell(at, send.clone()));
        }
        drop(send);
        let capacity = held.capacity();
        let_go(&mut held, |t| t.0 % 2 == 0);
        assert!(held.is_empty());
        assert!(held.capacity() >= capacity);
        let me = std::thread::current().id();
        for _ in 0..4 {
            let (which, on) = dropped
                .recv_timeout(Duration::from_secs(5))
                .expect("every held value is dropped");
            if which % 2 == 0 {
                assert_ne!(on, me, "an unseen value was dropped on the resetting thread");
            } else {
                assert_eq!(on, me, "a value others see was handed to the reaper");
            }
        }
    }

    #[test]
    fn an_empty_map_hands_nothing_over() {
        let mut held: HashMap<u32, Tell> = HashMap::new();
        let_go(&mut held, |_| true);
        assert!(held.is_empty());
    }
}
