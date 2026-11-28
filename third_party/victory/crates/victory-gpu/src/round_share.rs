//! Queued submissions share heap-sync rounds (`VICTORY_HEAP_ROUND_SHARE=1`;
//! off by default).
//!
//! The idea of TaynanGT/Vice-Tay PR #323 (PetrSeifert, head 6a1cf20ad7,
//! "Share heap-sync rounds between queued submissions"), put onto our
//! `descriptor::sync_heaps_start`/`sync_heaps_wait` tickets and
//! `crate::heap_rounds`; not their diff. On an RTX 4070 their wait for the
//! heaps' records went from 2.86 to 1.10 ms a frame settled.
//!
//! Before (and with the switch off): every submission sent ahead asks the
//! `victory-heap-sync` worker for a round of its own when it enters
//! `Host::submit`, and waits for it at the end of its preparing (2.6-15 ms
//! a frame over batches 2 and 3, 6-29 ms at the 90th percentile).
//!
//! With the switch: the title's `ExecuteCommandLists` stamps each list with
//! the ticket the next round would get ([`stamp_lists`]). The worker takes
//! as its target the newest ticket asked when it begins a pass, so a round
//! for that ticket, or any later one, began after the stamp: after the
//! title recorded the list and wrote the records it reads. A submission
//! needs a round from the newest stamp of its lists on, and from [`FLOOR`]
//! on: the ticket after the last time we wrote a heap's title memory (a
//! list's copy into it, shaders' writes landing there, a read-back). If one
//! is done it waits for nothing; if one was asked it waits for that; else
//! it asks one, as before ([`decide`]).
//!
//! What keeps its round of its own, as before: a submission with a list
//! not stamped (ours, or one taken twice); one prepared while a landing is
//! under way ([`Landing`]); and everything a queue held behind a wait for a
//! fence or a memory value: the title may write records for it until the
//! value is there, so when a wait is passed what is behind it is stamped
//! again ([`raise`], from `queue.rs`). The rounds the preparing asks for
//! after its own heap writes and settles (`Rounds::wrote`) are not shared.
//!
//! (critpath, #88) `VICTORY_HEAP_ROUND_AHEAD=1` (Live, off by default; acts with
//! `VICTORY_HEAP_ROUND_SHARE=1`): the title's `ExecuteCommandLists` (and a wait passed, for the
//! parts behind it) also asks the round, right after stamping, when a list uses heaps
//! ([`ask_ahead`]). With `VICTORY_ASYNC_SUBMIT=1` the submission then sits queued while the one
//! before it is prepared, and its round runs meanwhile: when it enters `Host::submit` the round
//! is done or under way, where before it was asked only then and waited for at the end of its
//! preparing (sync_heaps 0.35-0.9 ms a frame settled, 1.8-7.8 ms while streaming, part S/T; the
//! pass itself 1.0-1.6 ms a frame over 24 rounds). The ticket asked is the stamp or later, so
//! its round begins after the title wrote the records; a heap write of ours after it raises the
//! floor past it, and the submission asks a round of its own, as before.

use std::sync::atomic::{AtomicU64, Ordering};

pub(crate) fn on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        crate::heap_rounds::on()
            && crate::descriptor::async_sync_on()
            && std::env::var_os("VICTORY_HEAP_ROUND_SHARE").is_some_and(|v| v != "0")
    })
}

/// How a submission had its round.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Decision {
    /// A round from the ticket needed on is done: nothing to wait for.
    Done,
    /// One was asked for already: wait for it.
    InFlight,
    /// None was: ask for one.
    Ask,
}

/// The round for ticket `need` or later, with the worker's tickets
/// `asked` and `done`. A round for ticket `t` begins after `t` was asked,
/// and the worker's pass for its target covers every ticket up to it.
pub(crate) fn decide(asked: u64, done: u64, need: u64) -> Decision {
    if done >= need {
        Decision::Done
    } else if asked >= need {
        Decision::InFlight
    } else {
        Decision::Ask
    }
}

/// The ticket after the last time we wrote a heap's title memory: a
/// submission prepared after it needs a round from it on.
static FLOOR: AtomicU64 = AtomicU64::new(0);

/// Landings under way ([`Landing`]).
static LANDING: AtomicU64 = AtomicU64::new(0);

/// Since the last line: rounds done already, rounds asked already, rounds
/// asked (a stamp past both), rounds asked because a list was not stamped,
/// because a landing was under way; times the floor rose; parts stamped
/// again behind a wait.
static STATS: [AtomicU64; 7] = [const { AtomicU64::new(0) }; 7];
const DONE: usize = 0;
const IN_FLIGHT: usize = 1;
const ASKED: usize = 2;
const UNSTAMPED: usize = 3;
const LANDING_ON: usize = 4;
const RAISED: usize = 5;
const BEHIND: usize = 6;

static AHEAD: victory_core::settings::Live =
    victory_core::settings::Live::flag("VICTORY_HEAP_ROUND_AHEAD");

/// Since the last line: rounds asked ahead ([`ask_ahead`]).
static AHEAD_ASKED: AtomicU64 = AtomicU64::new(0);

/// (`VICTORY_HEAP_ROUND_AHEAD`) Lists that use heaps were stamped (or raised) just now: their
/// round is asked now, not when their submission is prepared.
fn ask_ahead(uses_heaps: bool) {
    if uses_heaps && AHEAD.get_bool() {
        ask_now();
    }
}

fn ask_now() {
    if crate::descriptor::sync_heaps_start() != 0 {
        AHEAD_ASKED.fetch_add(1, Ordering::Relaxed);
    }
}

/// Rounds asked ahead since the last call, per frame over `frames`.
pub(crate) fn ahead_take(frames: f64) -> f64 {
    let n = AHEAD_ASKED.swap(0, Ordering::Relaxed) as f64;
    if frames > 0.0 { n / frames } else { 0.0 }
}

/// The stamp of a list given to a queue now (0: no worker; never shared).
pub(crate) fn stamp_now() -> u64 {
    crate::descriptor::heap_sync_next()
}

/// The title gave `lists` to a queue: each is stamped with now.
pub(crate) fn stamp_lists(lists: &[crate::object::This]) {
    if !on() {
        return;
    }
    let stamp = stamp_now();
    let mut uses_heaps = false;
    for &l in lists {
        if let Some(host) = crate::list::host_list(l) {
            host.heap_stamp.store(stamp, Ordering::Release);
            uses_heaps |= host.uses_heaps.load(Ordering::Relaxed);
        }
    }
    if stamp != 0 {
        ask_ahead(uses_heaps);
    }
}

/// A list stamped `stamp` stands behind a wait passed now (`now`, from
/// [`stamp_now`]): it needs a round from now on. A list not stamped stays so.
pub(crate) fn raise(stamp: &AtomicU64, now: u64) {
    let _ = stamp.fetch_update(Ordering::AcqRel, Ordering::Acquire, |s| {
        (s != 0 && s < now).then_some(now)
    });
}

/// [`raise`] for the lists of a part held behind a wait just passed.
pub(crate) fn raise_lists<'a>(
    lists: impl IntoIterator<Item = &'a crate::host::HostList>,
    now: u64,
) {
    let (mut any, mut uses_heaps) = (false, false);
    for l in lists {
        raise(&l.heap_stamp, now);
        any = true;
        uses_heaps |= l.uses_heaps.load(Ordering::Relaxed);
    }
    if any {
        STATS[BEHIND].fetch_add(1, Ordering::Relaxed);
        if now != 0 {
            ask_ahead(uses_heaps);
        }
    }
}

/// The ticket a submission whose lists hold `stamps` needs a round from,
/// with the floor `floor`; `None` if a list is not stamped. The stamps are
/// taken (a list sent again unstamped gets a round of its own).
pub(crate) fn need<'a>(stamps: impl IntoIterator<Item = &'a AtomicU64>, floor: u64) -> Option<u64> {
    let mut need = floor;
    let mut all = true;
    for s in stamps {
        match s.swap(0, Ordering::AcqRel) {
            0 => all = false,
            s => need = need.max(s),
        }
    }
    (all && need > 0).then_some(need)
}

/// The ticket the submission whose lists hold `stamps` waits for, shared
/// if it can be; `None`: it asks a round of its own ([`crate::heap_rounds::Rounds::start`]).
pub(crate) fn start<'a>(stamps: impl IntoIterator<Item = &'a AtomicU64>) -> Option<u64> {
    // The landing count first: a landing that ended before it raised the
    // floor before it ended.
    let landing = LANDING.load(Ordering::SeqCst) > 0;
    let floor = FLOOR.load(Ordering::SeqCst);
    let need = need(stamps, floor);
    if landing {
        STATS[LANDING_ON].fetch_add(1, Ordering::Relaxed);
        return None;
    }
    let Some(need) = need else {
        STATS[UNSTAMPED].fetch_add(1, Ordering::Relaxed);
        return None;
    };
    let (ticket, decision) = crate::descriptor::sync_heaps_share(need);
    STATS[match decision {
        Decision::Done => DONE,
        Decision::InFlight => IN_FLIGHT,
        Decision::Ask => ASKED,
    }]
    .fetch_add(1, Ordering::Relaxed);
    Some(ticket)
}

/// We wrote a heap's title memory just now (and are in no [`Landing`]):
/// the floor rises to the next ticket.
fn rise() {
    FLOOR.fetch_max(stamp_now(), Ordering::SeqCst);
    STATS[RAISED].fetch_add(1, Ordering::Relaxed);
}

thread_local! {
    /// [`Landing`]s open on this thread, and whether one of them touched a
    /// heap.
    static OPEN: std::cell::Cell<(u32, bool)> = const { std::cell::Cell::new((0, false)) };
}

/// A [`Landing`] opens on this thread.
fn open() {
    OPEN.with(|o| {
        let (n, t) = o.get();
        o.set((n + 1, t));
    });
}

/// One closes: whether the floor is to rise now (the last one closed, and
/// a write under them touched a heap).
fn close() -> bool {
    OPEN.with(|o| {
        let (n, t) = o.get();
        let n = n.saturating_sub(1);
        o.set((n, t && n > 0));
        t && n == 0
    })
}

/// A write into a heap on this thread: whether the floor is to rise now
/// (no [`Landing`] is open; else when the last one closes).
fn touch() -> bool {
    OPEN.with(|o| {
        let (n, _) = o.get();
        if n > 0 {
            o.set((n, true));
        }
        n == 0
    })
}

/// While it lives, what this thread writes into title memory from the
/// card (copy-backs, card copies' pieces, read-backs) is under way: a
/// submission prepared meanwhile asks a round of its own, and if a write
/// was into a heap ([`touched`]) the floor rises when it ends, after the
/// writes.
pub(crate) struct Landing(bool);

impl Landing {
    pub(crate) fn begin() -> Landing {
        if !on() {
            return Landing(false);
        }
        LANDING.fetch_add(1, Ordering::SeqCst);
        open();
        Landing(true)
    }
}

impl Drop for Landing {
    fn drop(&mut self) {
        if !self.0 {
            return;
        }
        if close() {
            rise();
        }
        LANDING.fetch_sub(1, Ordering::SeqCst);
    }
}

/// We wrote `ranges` (`[from, to)`) of title memory from the card: if one
/// is in a heap, the floor rises (when the open [`Landing`] ends, or now).
pub(crate) fn touched(ranges: impl IntoIterator<Item = (usize, usize)>) {
    if on() && crate::descriptor::any_in_shader_heap(ranges) && touch() {
        rise();
    }
}

/// A list wrote a heap's title memory while it was prepared: the floor
/// rises (later submissions need a round that sees it).
pub(crate) fn list_wrote() {
    if on() {
        rise();
    }
}

pub(crate) fn line(frames: f64) {
    if !on() || frames <= 0.0 {
        return;
    }
    let [done, flight, asked, unstamped, landing, raised, behind] = STATS
        .each_ref()
        .map(|n| n.swap(0, Ordering::Relaxed) as f64 / frames);
    tracing::info!(
        "heap-sync rounds shared per frame (VICTORY_HEAP_ROUND_SHARE=1): {:.1} submissions found theirs done, \
         {flight:.1} shared one asked already, {asked:.1} asked one (stamp past both), {:.1} asked one of their own \
         ({unstamped:.1} with a list not stamped, {landing:.1} during a landing); floor rose {raised:.1} times; \
         {behind:.1} parts stamped again behind a wait",
        done,
        asked + unstamped + landing,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decide_shares_only_rounds_from_the_ticket_needed_on() {
        assert_eq!(decide(5, 5, 5), Decision::Done);
        assert_eq!(decide(7, 6, 5), Decision::Done);
        assert_eq!(decide(6, 4, 5), Decision::InFlight);
        assert_eq!(decide(5, 4, 5), Decision::InFlight);
        assert_eq!(decide(4, 4, 5), Decision::Ask);
        assert_eq!(decide(4, 3, 5), Decision::Ask);
    }

    #[test]
    fn need_is_the_newest_stamp_or_floor_and_takes_the_stamps() {
        let s = [AtomicU64::new(3), AtomicU64::new(9), AtomicU64::new(4)];
        assert_eq!(need(&s, 2), Some(9));
        assert!(s.iter().all(|s| s.load(Ordering::Relaxed) == 0));
        let s = [AtomicU64::new(3)];
        assert_eq!(need(&s, 12), Some(12));
        // A list not stamped (or taken already): a round of its own.
        let s = [AtomicU64::new(3), AtomicU64::new(0)];
        assert_eq!(need(&s, 0), None);
        let s = [AtomicU64::new(3)];
        assert_eq!(need(&s, 0), Some(3));
        assert_eq!(need(&s, 0), None, "a stamp is had once");
    }

    #[test]
    fn behind_a_wait_the_stamp_rises_and_an_unstamped_list_stays_so() {
        let s = AtomicU64::new(4);
        raise(&s, 10);
        assert_eq!(s.load(Ordering::Relaxed), 10);
        raise(&s, 7);
        assert_eq!(s.load(Ordering::Relaxed), 10, "never lowered");
        let none = AtomicU64::new(0);
        raise(&none, 10);
        assert_eq!(none.load(Ordering::Relaxed), 0);
    }

    /// The worker as `descriptor::heap_sync_loop` runs it, the title, the
    /// queue and the landings, in random order. Every submission's round
    /// (the first pass to complete the ticket it waits for or a later one)
    /// must have begun after the title wrote the records its lists read,
    /// after what the title wrote before meeting a wait the submission
    /// stood behind, and after every heap write we made before it was
    /// prepared.
    #[test]
    fn no_submission_runs_on_records_older_than_it_needs() {
        let (shared, asked_own) = model(false);
        assert!(shared > 0 && asked_own > 0, "shared {shared}, asked {asked_own}");
    }

    /// (`VICTORY_HEAP_ROUND_AHEAD`) The title asks the round when it gives the lists (and when
    /// it meets a wait): no submission runs on older records, and more find theirs had already.
    #[test]
    fn rounds_asked_ahead_keep_every_submission_on_its_records() {
        let (shared_ahead, own_ahead) = model(true);
        let (shared, own) = model(false);
        assert!(shared_ahead > 0, "shared {shared_ahead}, asked {own_ahead}");
        assert!(
            shared_ahead * (shared + own) > shared * (shared_ahead + own_ahead),
            "ahead: {shared_ahead} shared of {}, before: {shared} of {}",
            shared_ahead + own_ahead,
            shared + own,
        );
    }

    /// The model below; `ahead`: the title asks a round right after each stamp and raise.
    /// Submissions that shared a round, and that asked one of their own.
    fn model(ahead: bool) -> (u64, u64) {
        struct Sub {
            stamp: u64,
            /// The records' version it must see at least.
            must_see: u64,
            behind_wait: bool,
        }
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut random = move |n: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % n
        };
        // The first pass that completed `ticket` or later saw `must` or newer.
        let check = |seen_by: &[(u64, u64)], ticket: u64, must: u64| {
            let &(_, saw) = seen_by
                .iter()
                .find(|&&(t, _)| t >= ticket)
                .expect("a pass completed it");
            assert!(
                saw >= must,
                "ticket {ticket}: the round saw version {saw}, the submission needs {must}"
            );
        };
        let (mut shared, mut asked_own) = (0, 0);
        for _ in 0..300 {
            let (mut asked, mut done) = (0u64, 0u64);
            // The pass running: its target, and the records' version when it began.
            let mut running: Option<(u64, u64)> = None;
            // Per pass completed in order: its target and the version it saw.
            let mut seen_by: Vec<(u64, u64)> = Vec::new();
            let mut version = 0u64;
            let mut floor = 0u64;
            // The version of the last landing in a heap.
            let mut landed = 0u64;
            let mut line: Vec<Sub> = Vec::new();
            // Submissions waiting for their round: the ticket, the version.
            let mut waiting: Vec<(u64, u64)> = Vec::new();
            for _ in 0..400 {
                match random(10) {
                    // The title writes records, then gives a list to its
                    // queue, now or behind a wait it meets later.
                    0 | 1 => {
                        version += 1;
                        line.push(Sub {
                            stamp: asked + 1,
                            must_see: version,
                            behind_wait: random(4) == 0,
                        });
                        if ahead {
                            asked += 1;
                        }
                    }
                    // It writes records for those behind the wait and meets
                    // it: their stamps rise (`queue::raise_behind`).
                    2 => {
                        version += 1;
                        for s in line.iter_mut().filter(|s| s.behind_wait) {
                            s.must_see = version;
                            let stamp = AtomicU64::new(s.stamp);
                            raise(&stamp, asked + 1);
                            s.stamp = stamp.into_inner();
                            s.behind_wait = false;
                        }
                        if ahead {
                            asked += 1;
                        }
                    }
                    // Shaders' writes land in a heap; the floor rises after.
                    3 => {
                        version += 1;
                        landed = version;
                        floor = floor.max(asked + 1);
                    }
                    // The worker begins a pass if one is asked.
                    4 | 5 if running.is_none() && asked > done => {
                        running = Some((asked, version));
                    }
                    6 | 7 if running.is_some() => {
                        let (target, saw) = running.take().unwrap();
                        done = done.max(target);
                        seen_by.push((target, saw));
                        waiting.retain(|&(ticket, must)| {
                            let over = ticket <= done;
                            if over {
                                check(&seen_by, ticket, must);
                            }
                            !over
                        });
                    }
                    // The queue prepares the first submission of its line,
                    // if no wait holds it.
                    8 | 9 => {
                        if line.first().is_none_or(|s| s.behind_wait) {
                            continue;
                        }
                        let sub = line.remove(0);
                        let must = sub.must_see.max(landed);
                        let stamps = [AtomicU64::new(sub.stamp)];
                        let need = need(&stamps, floor).expect("stamped");
                        match decide(asked, done, need) {
                            Decision::Done => {
                                shared += 1;
                                check(&seen_by, need, must);
                            }
                            Decision::InFlight => {
                                shared += 1;
                                waiting.push((need, must));
                            }
                            Decision::Ask => {
                                asked_own += 1;
                                asked += 1;
                                waiting.push((asked, must));
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        (shared, asked_own)
    }

    /// (`VICTORY_HEAP_ROUND_AHEAD`) A round asked right after the stamp is the lists' round:
    /// their submission shares it, done or under way, and asks none of its own.
    #[test]
    fn a_round_asked_ahead_is_had_by_the_stamped_submission() {
        let stamp = stamp_now();
        if stamp == 0 {
            return; // no worker thread
        }
        let before = AHEAD_ASKED.load(Ordering::Relaxed);
        ask_now();
        assert!(AHEAD_ASKED.load(Ordering::Relaxed) > before);
        let (wait_for, decision) = crate::descriptor::sync_heaps_share(stamp);
        assert_ne!(decision, Decision::Ask, "the round asked ahead was not had");
        assert!(wait_for >= stamp);
        crate::descriptor::sync_heaps_wait(wait_for);
    }

    /// A stamp taken before a round began is not met by that round, even
    /// when it is the newest done: the real worker's tickets.
    #[test]
    fn a_round_begun_before_the_stamp_is_not_shared() {
        let ticket = crate::descriptor::sync_heaps_start();
        crate::descriptor::sync_heaps_wait(ticket);
        let stamp = crate::descriptor::heap_sync_next();
        if stamp == 0 {
            return; // no worker thread
        }
        assert!(stamp > ticket);
        let (wait_for, decision) = crate::descriptor::sync_heaps_share(stamp);
        // (Done only if another test's round began after the stamp.)
        let _ = decision;
        assert!(wait_for >= stamp);
        crate::descriptor::sync_heaps_wait(wait_for);
        // Now one from the stamp on is done.
        let (_, again) = crate::descriptor::sync_heaps_share(stamp);
        assert_eq!(again, Decision::Done);
    }

    /// Under a landing a heap write raises the floor only when the last
    /// landing on the thread closes (after its writes); outside one, at once.
    #[test]
    fn a_heap_write_under_a_landing_raises_the_floor_when_it_ends() {
        assert!(touch(), "no landing open: at once");
        open();
        assert!(!touch());
        open();
        assert!(!close(), "an outer landing is still open");
        assert!(close(), "the last one closes: now");
        open();
        assert!(!close(), "nothing touched");
    }
}
