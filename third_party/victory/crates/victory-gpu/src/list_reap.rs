//! Lets go of what a reset host list held without dropping it on the
//! resetting thread (`VICTORY_LIST_REAP=0`: dropped there, as before).
//!
//! A list holds every host texture (and colour twin) it names until it is
//! reset (`HostList::reset`), and its deferred work holds host copies of
//! title memory, upload and read-back buffers; a texture or buffer the
//! title let go while a list still named it is freed by that reset: a
//! D3D12 `Release()` that destroys the allocation, behind the driver's
//! lock. Lists are reset by `crate::host::land` right after a submission,
//! on the thread that submits and presents (`victory-submit` with
//! `VICTORY_ASYNC_SUBMIT=1`). Sampled in the frames of 100 ms and more
//! after world entry (hitch stacks, 200 s bench), those drops were 32% of
//! the presenting thread's samples and about two thirds of its busy time
//! in `submit`.
//!
//! The reset list has run, so nothing on the card reads what it held any
//! more: the held objects go to [`crate::host::retire`], which drops them on
//! the `victory-reaper` thread (`VICTORY_REAPER`, on by default), as it
//! already does for the host copies sweeps let go. Only the thread differs,
//! and the drop may come a little later; nothing recorded or submitted
//! changes, so the frame is rendered the same.

use std::collections::HashMap;
use std::sync::OnceLock;

/// Parses `VICTORY_LIST_REAP`: on unless `"0"`.
fn parse_switch(value: Option<&str>) -> bool {
    value.is_none_or(|v| v.trim() != "0")
}

fn wanted() -> bool {
    static WANTED: OnceLock<bool> = OnceLock::new();
    *WANTED.get_or_init(|| {
        let on = parse_switch(std::env::var("VICTORY_LIST_REAP").ok().as_deref());
        tracing::info!(
            "what a reset host list held is dropped {} (VICTORY_LIST_REAP={})",
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

/// Empties `held` (keeping its capacity) and lets go of its values: on the
/// reaper thread when [`wanted`], here otherwise.
pub(crate) fn let_go<K, V: 'static>(held: &mut HashMap<K, V>) {
    if held.is_empty() {
        return;
    }
    if !wanted() {
        held.clear();
        return;
    }
    let values: Vec<V> = held.drain().map(|(_, v)| v).collect();
    crate::host::retire(values);
}

/// Puts a fresh `T` in `held` and lets go of the old one as [`let_go`] does.
pub(crate) fn let_go_all<T: Default + 'static>(held: &mut T) {
    let old = std::mem::take(held);
    if wanted() {
        crate::host::retire(old);
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

    struct Tell(mpsc::Sender<std::thread::ThreadId>);

    impl Drop for Tell {
        fn drop(&mut self) {
            let _ = self.0.send(std::thread::current().id());
        }
    }

    #[test]
    fn held_values_are_dropped_off_the_resetting_thread() {
        let (send, dropped) = mpsc::channel();
        let mut held = HashMap::new();
        for at in 0..4usize {
            held.insert(at, Tell(send.clone()));
        }
        drop(send);
        let capacity = held.capacity();
        let_go(&mut held);
        assert!(held.is_empty());
        assert!(held.capacity() >= capacity);
        let me = std::thread::current().id();
        for _ in 0..4 {
            let on = dropped
                .recv_timeout(Duration::from_secs(5))
                .expect("every held value is dropped");
            assert_ne!(on, me, "dropped on the resetting thread");
        }
    }

    #[test]
    fn a_whole_value_is_replaced_and_dropped_off_the_resetting_thread() {
        let (send, dropped) = mpsc::channel();
        let mut held = vec![Tell(send.clone()), Tell(send)];
        let_go_all(&mut held);
        assert!(held.is_empty());
        let me = std::thread::current().id();
        for _ in 0..2 {
            let on = dropped
                .recv_timeout(Duration::from_secs(5))
                .expect("every held value is dropped");
            assert_ne!(on, me, "dropped on the resetting thread");
        }
    }

    #[test]
    fn an_empty_map_hands_nothing_over() {
        let mut held: HashMap<usize, Tell> = HashMap::new();
        let_go(&mut held);
        assert!(held.is_empty());
    }
}
