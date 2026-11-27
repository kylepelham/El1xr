//! (racefix) A texture slot the layer rewrites on its own waits until every
//! submission begun when the change was queued has finished
//! (`VICTORY_SLOT_WAIT_ALL=1`; off by default).
//!
//! Ported from Belobug/VIctory-Belobug-AMD pull request #636 (issue #545,
//! MamaLeVrai; confirmed in game by rpmaster2 on an RX 9070 XT, where
//! `VICTORY_LATE_WAIT=0` took away the missing near-LOD skin and the black
//! lines). Lists in the air read the shader-visible heap by index while
//! they run, and Direct3D 12 lets a descriptor they read change only until
//! they execute, not while they are on the card.
//!
//! Two of our own rewrites land under them:
//! - [`super::commit_show`] (`VICTORY_SHOW_AFTER`): the new view is written
//!   once the submission that recorded its upload has finished, while the
//!   submissions sent after it (up to `VICTORY_LATE_WAIT_LISTS` lists with
//!   the relay) still run;
//! - [`super::HostHeap::texture_views_remade`] (levels on demand): slots
//!   are made again at the texture pass whatever is in the air (b2 logs:
//!   3,500-5,100 a run while a submission was in flight).
//!
//! With the switch both are queued with `after` = the newest submission
//! begun (`crate::host::submissions_started`) and done once
//! `crate::host::submissions_finished` reaches it (flights land in order),
//! at the next texture pass or landing. With nothing in the air they are
//! done at once, as before. Each is checked again when it is done: a show
//! only if the title slot still holds the view, a remake only if the host
//! slot still shows it.
//!
//! Not covered (as in #636): the title's own descriptor writes mirrored
//! while lists are in the air, the null written when a slot's view waits
//! for its texture, and slots evicted with their texture
//! (`descriptor::evict_slots`).

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// `VICTORY_SLOT_WAIT_ALL=1`. Live: turned off, the changes still held
/// are done as their submissions finish ([`drain`] runs while any was ever
/// held).
static ON: victory_core::settings::Live =
    victory_core::settings::Live::new("VICTORY_SLOT_WAIT_ALL", |v| u64::from(parse(v)));
/// A change was held once: [`drain`] runs whatever the switch says now.
static HELD: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub(super) fn on() -> bool {
    static LOGGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    let on = ON.get_bool();
    if on && !LOGGED.load(Relaxed) && !LOGGED.swap(true, Relaxed) {
        tracing::info!(
            "slots the layer rewrites wait for every submission begun when the change was queued (VICTORY_SLOT_WAIT_ALL=1; Belobug #636)"
        );
    }
    on
}

fn parse(value: Option<&str>) -> bool {
    value.is_some_and(|v| v.trim() == "1")
}

/// A rewrite held back.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Change {
    /// [`super::commit_show`]: title heap (`super::super::Heap` address),
    /// slot, title descriptor handle, the view's key.
    Show {
        heap: usize,
        index: u32,
        handle: u64,
        key: u64,
    },
    /// A remake of slot `index` of the host heap at `host` showing `key`.
    Remake { host: usize, index: u32, key: u64 },
}

/// Rewrites waiting, in the order queued: (after, queued at, change).
#[derive(Default)]
pub(super) struct Line<T> {
    waiting: VecDeque<(u64, T, Change)>,
}

impl<T: Copy> Line<T> {
    /// Queues `change` behind submission `after`. A change of the same slot
    /// to the same thing already waiting stands for it (it waits for fewer
    /// submissions): false then.
    pub(super) fn push(&mut self, after: u64, at: T, change: Change) -> bool {
        if self.waiting.iter().any(|&(_, _, c)| c == change) {
            return false;
        }
        self.waiting.push_back((after, at, change));
        true
    }

    /// Takes the changes whose submissions have all finished (`finished`).
    pub(super) fn due(&mut self, finished: u64) -> Vec<(T, Change)> {
        let mut out = Vec::new();
        self.waiting.retain(|&(after, at, change)| {
            if after <= finished {
                out.push((at, change));
                false
            } else {
                true
            }
        });
        out
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.waiting.len()
    }
}

static LINE: Mutex<Line<std::time::Instant>> = Mutex::new(Line {
    waiting: VecDeque::new(),
});

/// Since the last line: changes that had to wait, microseconds they waited
/// in all, changes done after their wait, of those dropped (the slot moved
/// on meanwhile), the most waiting at once.
static STATS: [AtomicU64; 5] = [const { AtomicU64::new(0) }; 5];
/// Since the start: changes that waited.
static TOTAL: AtomicU64 = AtomicU64::new(0);

/// Queues `change` if a submission is in the air: true if the caller must
/// not do it now.
pub(super) fn hold(change: Change) -> bool {
    if !on() {
        return false;
    }
    let after = crate::host::submissions_started();
    if after <= crate::host::submissions_finished() {
        return false;
    }
    HELD.store(true, Relaxed);
    let mut line = LINE.lock().unwrap_or_else(|e| e.into_inner());
    if line.push(after, std::time::Instant::now(), change) {
        STATS[0].fetch_add(1, Relaxed);
        TOTAL.fetch_add(1, Relaxed);
        STATS[4].fetch_max(line.waiting.len() as u64, Relaxed);
    }
    true
}

/// Does the changes whose submissions have all finished. Called at every
/// texture pass and when a show is committed.
pub(super) fn drain() {
    if !on() && !HELD.load(Relaxed) {
        return;
    }
    let due = {
        let mut line = LINE.lock().unwrap_or_else(|e| e.into_inner());
        if line.waiting.is_empty() {
            Vec::new()
        } else {
            line.due(crate::host::submissions_finished())
        }
    };
    for (at, change) in due {
        STATS[1].fetch_add(at.elapsed().as_micros() as u64, Relaxed);
        STATS[2].fetch_add(1, Relaxed);
        let done = match change {
            Change::Show {
                heap,
                index,
                handle,
                key,
            } => super::commit_show_now(heap, index, handle, key),
            Change::Remake { host, index, key } => remake(host, index, key),
        };
        if !done {
            STATS[3].fetch_add(1, Relaxed);
        }
    }
    line_now();
}

/// Makes slot `index` of the host heap at `host` again from `key`, if that
/// heap is still registered and the slot still shows `key`.
fn remake(host: usize, index: u32, key: u64) -> bool {
    let heaps = super::super::SHADER_HEAPS
        .read()
        .unwrap_or_else(|e| e.into_inner());
    for &(_, _, at) in heaps.iter() {
        // SAFETY: registered heaps are live (removed when dropped, which takes `.write()`).
        let Some(Some(h)) = unsafe { &*(at as *const super::super::Heap) }.host.get() else {
            continue;
        };
        if std::ptr::from_ref(h) as usize == host {
            return h.remake_slot(index, key);
        }
    }
    false
}

/// The per-frame line, every 300 frames.
fn line_now() {
    static LAST: AtomicU64 = AtomicU64::new(0);
    let frame = crate::frame::current();
    let last = LAST.load(Relaxed);
    if frame < last + 300
        || LAST
            .compare_exchange(last, frame, Relaxed, Relaxed)
            .is_err()
    {
        return;
    }
    let frames = (frame - last).max(1) as f64;
    let [held, us, done, dropped, most] = STATS.each_ref().map(|s| s.swap(0, Relaxed));
    let waiting = LINE.lock().unwrap_or_else(|e| e.into_inner()).waiting.len();
    tracing::info!(
        frame,
        held_per_frame = format_args!("{:.1}", held as f64 / frames),
        waited_ms_per_frame = format_args!("{:.2}", us as f64 / 1000.0 / frames),
        mean_wait_ms = format_args!(
            "{:.1}",
            if done > 0 {
                us as f64 / 1000.0 / done as f64
            } else {
                0.0
            }
        ),
        done,
        dropped,
        most_waiting = most,
        waiting_now = waiting,
        held_since_start = TOTAL.load(Relaxed),
        "slot wait per frame: slot rewrites held until the submissions begun before them finished (VICTORY_SLOT_WAIT_ALL)"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_switch_is_live() {
        // Unchanged, the switch reads what the environment said, as before.
        assert_eq!(on(), parse(std::env::var("VICTORY_SLOT_WAIT_ALL").ok().as_deref()));
        if std::env::var_os("VICTORY_SLOT_WAIT_ALL").is_some() {
            return;
        }
        victory_core::settings::set("VICTORY_SLOT_WAIT_ALL", "1").unwrap();
        let now = on();
        victory_core::settings::set("VICTORY_SLOT_WAIT_ALL", "").unwrap();
        assert!(now, "a change takes effect while running");
        assert!(!on(), "and clears back to the launch value");
    }

    fn show(index: u32, key: u64) -> Change {
        Change::Show {
            heap: 1,
            index,
            handle: 2,
            key,
        }
    }

    #[test]
    fn the_switch_is_off_unless_one() {
        assert!(!parse(None));
        assert!(!parse(Some("0")));
        assert!(!parse(Some("2")));
        assert!(parse(Some("1")));
    }

    #[test]
    fn a_change_waits_for_the_submission_begun_when_it_was_queued() {
        let mut line = Line::default();
        assert!(line.push(7, 0u32, show(3, 9)));
        assert!(line.due(5).is_empty());
        assert!(line.due(6).is_empty());
        assert_eq!(line.due(7), vec![(0, show(3, 9))]);
        assert_eq!(line.len(), 0);
    }

    #[test]
    fn the_same_change_waits_once_and_others_keep_their_own_wait() {
        let mut line = Line::default();
        assert!(line.push(7, 0u32, show(3, 9)));
        assert!(!line.push(9, 1, show(3, 9)));
        assert!(line.push(9, 2, show(3, 10)));
        let remake = Change::Remake {
            host: 4,
            index: 3,
            key: 9,
        };
        assert!(line.push(8, 3, remake));
        assert_eq!(line.due(8), vec![(0, show(3, 9)), (3, remake)]);
        assert_eq!(line.due(9), vec![(2, show(3, 10))]);
    }
}
