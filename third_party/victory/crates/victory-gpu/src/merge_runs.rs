//! Consecutive submissions of a queue sent as one host submission
//! (`VICTORY_MERGE_SUBMISSIONS=1`; off by default).
//!
//! The idea is Peppacodes/playablegames-peppa pull request 20 (from LooperSalty/AnyXboxX pull
//! request 337 by Skyfrye): the title makes about 41 submissions a frame, and each host
//! submission pays its own fixed costs on the submitting thread (the submission lock, the
//! write-watch asking, the batch flush's setting up, the card clock's wait and the
//! `ExecuteCommandLists` call, the landing of one flight). There 41 became 8.3 and the frame
//! 1.9-2.2 ms shorter. Their code lives in a submission line we do not have; this is the idea on
//! ours ([`crate::queue`]'s held pieces).
//!
//! When the thread that runs a queue's line takes a submission out of it, it takes along the
//! submissions right behind it in the same queue, as one host submission, in order:
//! - only past waits that are met already (a fence reached, a memory value there now), so the
//!   queue would have run straight on to them anyway;
//! - never past a signal (it is set behind what was sent before it, as before);
//! - only a submission whose preparing on the CPU does not read what the ones before it in the
//!   gathering write on the card (`crate::host::joinable`, the test a submission meets against
//!   flights in the air), and with no work of ours that may write title memory anywhere;
//! - at most [`MOST_LISTS`] lists in all.
//!
//! What the title can tell: its bottom-of-pipe writes of the earlier submissions are told when
//! the whole host submission has run, later than before, never earlier; nothing it waits for
//! is among them (a wait passed is met already).

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

/// `VICTORY_MERGE_SUBMISSIONS=1`.
pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        let on = std::env::var_os("VICTORY_MERGE_SUBMISSIONS").is_some_and(|v| v == "1");
        if on {
            tracing::info!("consecutive submissions of a queue go to the card as one (VICTORY_MERGE_SUBMISSIONS=1)");
        }
        on
    })
}

/// The most lists a gathering holds.
pub(crate) const MOST_LISTS: usize = 128;

/// What stands behind a submission in its queue's line, as far as gathering cares.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Next {
    /// A submission of this many lists that may join the gathering.
    Run(usize),
    /// A submission that may not join it.
    Refused,
    /// A wait that is met already.
    Met,
    /// Anything else: a wait not met, a signal.
    Stop,
}

/// How many entries at the front of `next` a gathering that holds `lists` lists takes along:
/// it ends after its last submission, never on a wait it passed.
pub(crate) fn take(lists: usize, next: impl IntoIterator<Item = Next>) -> usize {
    let (mut lists, mut seen, mut taken) = (lists, 0, 0);
    for n in next {
        seen += 1;
        match n {
            Next::Met => {}
            Next::Run(more) if lists + more <= MOST_LISTS => {
                lists += more;
                taken = seen;
            }
            Next::Run(_) | Next::Refused | Next::Stop => break,
        }
    }
    taken
}

/// Puts `more` (lists and the order they run in) behind `lists` and `order`.
pub(crate) fn join<T>(
    lists: &mut Vec<T>,
    order: &mut Vec<usize>,
    more: Vec<T>,
    more_order: Vec<usize>,
) {
    let base = lists.len();
    lists.extend(more);
    order.extend(more_order.into_iter().map(|i| i + base));
}

/// Since the last line: host submissions that gathered, title submissions they took along,
/// gatherings a submission refused.
static COUNTS: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

/// A gathering took `along` submissions behind its first; `refused` whether it ended at one
/// that may not join.
pub(crate) fn gathered(along: usize, refused: bool) {
    if along > 0 {
        COUNTS[0].fetch_add(1, Ordering::Relaxed);
        COUNTS[1].fetch_add(along as u64, Ordering::Relaxed);
    }
    if refused {
        COUNTS[2].fetch_add(1, Ordering::Relaxed);
    }
}

/// The line, for `frames` frames since the last one.
pub(crate) fn line(frames: f64) {
    if !on() || frames <= 0.0 {
        return;
    }
    let [joined, along, refused] =
        [0, 1, 2].map(|i| COUNTS[i].swap(0, Ordering::Relaxed) as f64 / frames);
    tracing::info!(
        "submissions merged per frame (VICTORY_MERGE_SUBMISSIONS=1): {joined:.1} host submissions took along \
         {along:.1} title submissions behind their first; {refused:.1} gatherings ended at one that reads what \
         those before it write"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn takes_runs_and_the_met_waits_between_them() {
        let next = [
            Next::Run(2),
            Next::Met,
            Next::Run(3),
            Next::Stop,
            Next::Run(1),
        ];
        assert_eq!(take(4, next), 3);
    }

    #[test]
    fn never_ends_on_a_passed_wait() {
        assert_eq!(take(4, [Next::Run(2), Next::Met, Next::Met, Next::Stop]), 1);
        assert_eq!(take(4, [Next::Met, Next::Stop]), 0);
        assert_eq!(take(4, [Next::Met]), 0);
    }

    #[test]
    fn stops_at_a_refused_one_and_at_the_most_lists() {
        assert_eq!(take(4, [Next::Refused, Next::Run(1)]), 0);
        assert_eq!(take(4, [Next::Run(1), Next::Refused, Next::Run(1)]), 1);
        assert_eq!(take(MOST_LISTS - 2, [Next::Run(2), Next::Run(1)]), 1);
        assert_eq!(take(MOST_LISTS, [Next::Run(1)]), 0);
    }

    #[test]
    fn join_keeps_each_order_behind_the_other() {
        let (mut lists, mut order) = (vec!['a', 'b'], vec![1, 0, 1]);
        join(&mut lists, &mut order, vec!['c', 'd', 'e'], vec![2, 0]);
        assert_eq!(lists, vec!['a', 'b', 'c', 'd', 'e']);
        assert_eq!(order, vec![1, 0, 1, 4, 2]);
        let run: String = order.iter().map(|&i| lists[i]).collect();
        assert_eq!(run, "babec");
    }
}
