//! (worldstream) `VICTORY_LOOK_SKIP=1` (live; off by default): a submission
//! whose lists the title all closed before the last write-watch asking of
//! this frame began, and whose ranges lie inside what that asking was given,
//! takes that asking again instead of asking the kernel: the journal's plan
//! of it is kept as the submission's own (`ask_once`,
//! `VICTORY_WATCH_RESERVATION=1`).
//!
//! The asking began after the lists closed, so it took the title's writes up
//! to then; what the lists read was written by their close. Taken back only
//! while nothing else was noted in the journal since that asking ended (the
//! asking count, [`super::ASKED`], has not moved: no kernel asking, page
//! guard, or copy-back of ours meanwhile), and only in the frame it was
//! made: every frame asks the kernel at least once. Taken back with the
//! asking's own number, so a copy refreshed meanwhile is not told anything
//! newer than it holds.
//!
//! The risk (as upstream says): the title may write what a closed list reads
//! until it submits it, and a closed list may be submitted again. A write
//! made between the asking and the submission is then taken by the next
//! asking, a submission late.
//!
//! Ported from xcl-dev/xcl #2768 (Decodere, `XCL_LOOK_SKIP`): there one look
//! covers all the hot memory at once; here the asking covers the ranges of
//! one submission, so the ranges must lie inside the last asking's too.

use super::*;
use std::cell::Cell;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering::{Relaxed, SeqCst};
use victory_core::settings::Live;

/// Whether a submission covered by this frame's last asking takes it again.
pub(crate) fn on() -> bool {
    static ON: Live = Live::flag("VICTORY_LOOK_SKIP");
    ON.get_bool()
}

/// Microseconds since the first call, never 0.
fn now_us() -> u64 {
    static T0: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    T0.get_or_init(std::time::Instant::now)
        .elapsed()
        .as_micros() as u64
        + 1
}

/// Host list (its address) → when the title closed it ([`now_us`]).
static CLOSED: Mutex<Option<std::collections::HashMap<usize, u64>>> = Mutex::new(None);
/// More lists than this and the times are all forgotten (only a shortcut).
const MOST_LISTS: usize = 1 << 16;

/// The title closed the host list at `list`.
pub(crate) fn closed(list: usize) {
    if !on() {
        return;
    }
    let at = now_us();
    let mut held = CLOSED.lock().unwrap_or_else(|e| e.into_inner());
    let map = held.get_or_insert_with(Default::default);
    if map.len() >= MOST_LISTS && !map.contains_key(&list) {
        map.clear();
    }
    map.insert(list, at);
}

/// The title reset the host list at `list`: it is open again.
pub(crate) fn forget(list: usize) {
    if on() {
        forget_now(list);
    }
}

fn forget_now(list: usize) {
    let mut held = CLOSED.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(map) = held.as_mut() {
        map.remove(&list);
    }
}

thread_local! {
    /// The submission being prepared on this thread: when its youngest list
    /// was closed (`None`: one was not seen closing). Taken by its asking.
    static SUBMISSION: Cell<Option<u64>> = const { Cell::new(None) };
}

/// The submission about to ask is made of the host lists at `lists`.
pub(crate) fn submission(lists: impl IntoIterator<Item = usize>) {
    if !on() {
        return;
    }
    let held = CLOSED.lock().unwrap_or_else(|e| e.into_inner());
    let youngest = held.as_ref().and_then(|map| {
        let mut youngest = 0u64;
        let mut any = false;
        for list in lists {
            youngest = youngest.max(*map.get(&list)?);
            any = true;
        }
        any.then_some(youngest)
    });
    SUBMISSION.with(|s| s.set(youngest));
}

/// (critpath #88, `VICTORY_ASK_ASIDE`) The submission noted on this thread, taken to be handed to
/// the thread that asks for it ([`handed`]).
pub(crate) fn taken() -> Option<u64> {
    SUBMISSION.with(Cell::take)
}

/// The submission [`taken`] on the submitting thread, noted on this one.
pub(crate) fn handed(youngest: Option<u64>) {
    SUBMISSION.with(|s| s.set(youngest));
}

/// The last kernel asking of a submission, as its end left it.
struct Saved {
    /// When it began ([`now_us`]) and the frame it began in.
    began: u64,
    frame: u64,
    /// What it was given: whole pages, sorted, merged.
    given: Vec<(usize, usize)>,
    /// The journal's plan of it at its end.
    ahead: Vec<(usize, usize)>,
    ordered: bool,
    ahead_asked: u64,
    /// [`ASKED`] at its end.
    count: u64,
}

/// A kernel asking under way: began, frame, what it was given.
static BEGAN: Mutex<Option<(u64, u64, Vec<(usize, usize)>)>> = Mutex::new(None);
static SAVED: Mutex<Option<Saved>> = Mutex::new(None);

/// Since the last line: submissions that took the last asking again, and
/// those that asked because of: another frame, a list not closed before it,
/// the journal moved, ranges outside it.
static TOLD: [AtomicU64; 5] = [const { AtomicU64::new(0) }; 5];

/// Whole pages of `ranges`, sorted and merged.
fn given(ranges: &[(usize, usize)]) -> Vec<(usize, usize)> {
    let mut pages: Vec<(usize, usize)> = ranges
        .iter()
        .filter(|r| r.0 < r.1)
        .map(|&(a, b)| (a / PAGE, b.div_ceil(PAGE)))
        .collect();
    pages.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(pages.len());
    for (a, b) in pages {
        match merged.last_mut() {
            Some((_, last)) if a <= *last => *last = (*last).max(b),
            _ => merged.push((a, b)),
        }
    }
    merged
}

/// Whether every interval of `inner` lies inside one of `outer` (both
/// sorted and merged).
fn inside(inner: &[(usize, usize)], outer: &[(usize, usize)]) -> bool {
    let mut k = 0;
    for &(a, b) in inner {
        while k < outer.len() && outer[k].1 <= a {
            k += 1;
        }
        match outer.get(k) {
            Some(&(oa, ob)) if oa <= a && b <= ob => {}
            _ => return false,
        }
    }
    true
}

/// Why a submission asks, or `None` if it may take the last asking again.
fn refusal(
    saved: &Saved,
    youngest_closed: Option<u64>,
    frame: u64,
    count: u64,
    ranges: &[(usize, usize)],
) -> Option<usize> {
    if saved.frame != frame {
        return Some(1);
    }
    if !youngest_closed.is_some_and(|t| t < saved.began) {
        return Some(2);
    }
    if count != saved.count {
        return Some(3);
    }
    if !inside(&given(ranges), &saved.given) {
        return Some(4);
    }
    None
}

/// Before the submission's asking (the last one ended): whether it took the
/// last asking again instead. If not, a kernel asking begins.
pub(super) fn take_again(ranges: &[(usize, usize)]) -> bool {
    if !on() {
        return false;
    }
    let youngest = SUBMISSION.with(Cell::take);
    let frame = crate::queue::presented();
    // `SAVED` is let go before `JOURNAL` is taken: an end holds `JOURNAL`
    // and takes `SAVED` ([`ended`]).
    let plan = {
        let saved = SAVED.lock().unwrap_or_else(|e| e.into_inner());
        saved.as_ref().map(|saved| {
            match refusal(saved, youngest, frame, ASKED.load(SeqCst), ranges) {
                None => Ok((saved.ahead.clone(), saved.ordered, saved.ahead_asked, saved.count)),
                Some(why) => Err(why),
            }
        })
    };
    match plan {
        Some(Ok((ahead, ordered, ahead_asked, count))) => {
            let mut j = JOURNAL.write().unwrap_or_else(|e| e.into_inner());
            if !j.kept && ASKED.load(SeqCst) == count {
                j.ahead = ahead;
                j.ordered = ordered;
                j.ahead_asked = ahead_asked;
                j.kept = true;
                KEPT.store(true, std::sync::atomic::Ordering::Release);
                TOLD[0].fetch_add(1, Relaxed);
                return true;
            }
            TOLD[3].fetch_add(1, Relaxed);
        }
        Some(Err(why)) => {
            TOLD[why].fetch_add(1, Relaxed);
        }
        None => {}
    }
    *BEGAN.lock().unwrap_or_else(|e| e.into_inner()) = Some((now_us(), frame, given(ranges)));
    false
}

/// The submission's asking ends with the journal `j` (an asking stands).
pub(super) fn ended(j: &Journal) {
    if !on() {
        return;
    }
    let mut saved = SAVED.lock().unwrap_or_else(|e| e.into_inner());
    let began = BEGAN.lock().unwrap_or_else(|e| e.into_inner()).take();
    let (began, frame, given) = match (began, saved.take()) {
        (Some(began), _) => began,
        // Taken again: still the same asking.
        (None, Some(last)) => (last.began, last.frame, last.given),
        (None, None) => return,
    };
    *saved = Some(Saved {
        began,
        frame,
        given,
        ahead: j.ahead.clone(),
        ordered: j.ordered,
        ahead_asked: j.ahead_asked,
        count: ASKED.load(SeqCst),
    });
}

/// One line on the frame-meter cadence while the switch is on.
pub(super) fn line(frames: f64) {
    if !on() || frames <= 0.0 {
        return;
    }
    let [again, frame, lists, moved, outside] =
        TOLD.each_ref().map(|n| n.swap(0, Relaxed) as f64 / frames);
    tracing::info!(
        "write-watch askings taken again per frame (VICTORY_LOOK_SKIP=1): {again:.1}; asked: {frame:.1} first of the frame, {lists:.1} lists not closed before, {moved:.1} journal moved, {outside:.1} ranges outside"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn saved(began: u64, frame: u64, count: u64, given: Vec<(usize, usize)>) -> Saved {
        Saved {
            began,
            frame,
            given,
            ahead: Vec::new(),
            ordered: true,
            ahead_asked: 1,
            count,
        }
    }

    #[test]
    fn ranges_must_lie_inside_what_the_asking_was_given() {
        let outer = given(&[(0, 3 * PAGE), (2 * PAGE, 5 * PAGE), (10 * PAGE, 11 * PAGE)]);
        assert_eq!(outer, vec![(0, 5), (10, 11)]);
        assert!(inside(
            &given(&[(PAGE, 2 * PAGE + 1), (10 * PAGE + 8, 10 * PAGE + 9)]),
            &outer
        ));
        assert!(!inside(&given(&[(4 * PAGE, 6 * PAGE)]), &outer));
        assert!(!inside(&given(&[(7 * PAGE, 8 * PAGE)]), &outer));
        assert!(inside(&[], &outer));
    }

    /// Taken again only in the same frame, with every list closed before the
    /// asking began, nothing noted since, and the ranges inside.
    #[test]
    fn a_submission_takes_the_asking_again_only_when_covered() {
        let s = saved(100, 7, 42, vec![(0, 8)]);
        let r = [(PAGE, 2 * PAGE)];
        assert_eq!(refusal(&s, Some(99), 7, 42, &r), None);
        assert_eq!(
            refusal(&s, Some(99), 8, 42, &r),
            Some(1),
            "a new frame asks"
        );
        assert_eq!(
            refusal(&s, Some(100), 7, 42, &r),
            Some(2),
            "closed as it began"
        );
        assert_eq!(
            refusal(&s, None, 7, 42, &r),
            Some(2),
            "a list not seen closing"
        );
        assert_eq!(
            refusal(&s, Some(99), 7, 43, &r),
            Some(3),
            "the journal moved"
        );
        assert_eq!(refusal(&s, Some(99), 7, 42, &[(0, 9 * PAGE)]), Some(4));
    }

    /// A list's close time goes with its reset; a submission with a list
    /// never seen closing has none.
    #[test]
    fn close_times_follow_the_lists() {
        // The switch is read live; tests run without it, so note directly.
        let (a, b) = (0x7000_0000_0010usize, 0x7000_0000_0020usize);
        {
            let mut held = CLOSED.lock().unwrap_or_else(|e| e.into_inner());
            let map = held.get_or_insert_with(Default::default);
            map.insert(a, 5);
            map.insert(b, 9);
        }
        let youngest = |lists: &[usize]| {
            let held = CLOSED.lock().unwrap_or_else(|e| e.into_inner());
            held.as_ref().and_then(|map| {
                lists
                    .iter()
                    .map(|l| map.get(l).copied())
                    .try_fold(0u64, |y, t| Some(y.max(t?)))
            })
        };
        assert_eq!(youngest(&[a, b]), Some(9));
        forget_now(b);
        assert_eq!(youngest(&[a, b]), None);
        forget_now(a);
    }
}
