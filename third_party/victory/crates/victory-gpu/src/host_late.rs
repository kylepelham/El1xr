//! Late lists: settling and waiting on the host fence, the helper and
//! relay threads, the late line, and landing what flights wrote
//! (read-backs, hand-backs, [`walls_landed`]).

use super::*;

pub(super) fn stale_under_flights(runs: &[&HostList]) -> bool {
    use crate::card_prof::P;
    use std::sync::Arc;
    let mut lap = crate::card_prof::Lap::new();
    crate::card_prof::add(P::StaleCalls, 1);
    // Per copy, the span of what is refreshed (a flush brings that much
    // up to date at most).
    let mut spans: Vec<(Arc<crate::shadow::Shadow>, usize, usize)> = Vec::new();
    let mut index: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();
    if step_index() {
        for list in runs {
            let deferred = list.deferred();
            for (shadow, address, len) in &deferred.rebar_refreshes {
                if shadow.rebar_stale(*address, *len) {
                    return true;
                }
            }
            for (shadow, ranges) in &deferred.gpu_writes {
                if shadow.writes_pending()
                    && ranges
                        .iter()
                        .any(|&(a, e)| shadow.has_lazy_resync_under_writes(a, e - a))
                {
                    return true;
                }
            }
            for (shadow, from, to) in &deferred.refresh_spans {
                if shadow.writes_pending() {
                    let key = Arc::as_ptr(shadow) as usize;
                    if let Some(&i) = index.get(&key) {
                        let s = &mut spans[i];
                        (s.1, s.2) = (s.1.min(*from), s.2.max(*to));
                    } else {
                        index.insert(key, spans.len());
                        spans.push((shadow.clone(), *from, *to));
                    }
                }
            }
        }
    } else {
        for list in runs {
            for step in &list.deferred().copies {
                if let Step::Refresh(shadow, address, len) = step
                    && shadow.rebar_stale(*address, *len)
                {
                    return true;
                }
                if let Step::GpuWrite(shadow, address, len) = step
                    && shadow.writes_pending()
                    && shadow.has_lazy_resync_under_writes(*address, *len)
                {
                    return true;
                }
                if let Step::Refresh(shadow, address, len) = step
                    && shadow.writes_pending()
                {
                    match spans.iter_mut().find(|s| Arc::ptr_eq(&s.0, shadow)) {
                        Some(s) => (s.1, s.2) = (s.1.min(*address), s.2.max(*address + *len)),
                        None => spans.push((shadow.clone(), *address, *address + *len)),
                    }
                }
            }
        }
    }
    lap.mark(P::StaleWalk);
    stale_spans(spans, &mut lap)
}

/// The tail of [`stale_under_flights`], after the walk of the lists: the
/// merges under writes of `spans` (per copy, the span of what is refreshed).
pub(super) fn stale_spans(
    mut spans: Vec<(std::sync::Arc<crate::shadow::Shadow>, usize, usize)>,
    lap: &mut crate::card_prof::Lap,
) -> bool {
    use crate::card_prof::P;
    use std::sync::Arc;
    if late_exact() {
        // (entryhitch) `VICTORY_STALE_QUICK`: only the copies whose merge can do anything.
        if crate::shadow::merge_due::on() {
            crate::shadow::merge_due::keep_due(&mut spans);
            lap.mark(P::StaleWalk);
        }
        crate::card_prof::add(P::StaleCopies, spans.len() as u64);
        crate::prep_cut::add(crate::prep_cut::F::StaleCopies, spans.len() as u64);
        // The title's bytes are put in beside the flights' writes; a
        // settle is still due where that could not be done. At most this
        // many changed runs go up to copies in the card's memory, each a
        // copy of its own there.
        // (Neptune) Each copy on a thread of the refreshes' (`wide_refresh`), with the budget of
        // runs to itself: a copy's merge reads title memory and writes that copy alone, and
        // one after the other on the submitting thread they were a sixth of its time.
        // (entryhitch) Below `VICTORY_STALE_WIDE_MIN` copies the caller merges
        // them itself, as the threads would: a budget each, every copy merged.
        if wide_refresh() && spans.len() >= 2 && spans.len() < crate::prep_cut::stale_wide_min() {
            use crate::prep_cut::F;
            crate::prep_cut::add(F::StaleSerial, spans.len() as u64);
            let mut stale = false;
            for (shadow, from, to) in &spans {
                // As a kept thread's job runs it (wide work inside stays here).
                stale |= crate::shadow::as_wide_job(|| merge_one(shadow, *from, *to));
            }
            lap.mark(P::StaleMerge);
            return stale;
        }
        if wide_refresh() && spans.len() >= 2 {
            let stale = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let jobs: Vec<Box<dyn FnOnce() + Send>> = spans
                .into_iter()
                .map(|(shadow, from, to)| {
                    let stale = stale.clone();
                    Box::new(move || {
                        if merge_one(&shadow, from, to) {
                            stale.store(true, Ordering::Relaxed);
                        }
                    }) as Box<dyn FnOnce() + Send>
                })
                .collect();
            crate::shadow::wide_run(jobs);
            lap.mark(P::StaleMerge);
            return stale.load(Ordering::Relaxed);
        }
        let mut runs_left = under_writes_runs();
        let stale = spans.iter().any(|(shadow, from, to)| {
            shadow.merge_under_writes(*from, *to - *from, &mut runs_left)
        });
        lap.mark(P::StaleMerge);
        return stale;
    }
    spans
        .iter()
        .any(|(shadow, from, to)| shadow.stale_under_writes(*from, *to - *from))
}

/// One copy's merge under writes in `stale_under_flights`, with a budget
/// of runs of its own: the job of a kept thread, or of the caller below
/// `VICTORY_STALE_WIDE_MIN`. Whether a settle is still due.
fn merge_one(shadow: &crate::shadow::Shadow, from: usize, to: usize) -> bool {
    let mut t = crate::prep_cut::now();
    let mut runs_left = under_writes_runs();
    let stale = shadow.merge_under_writes(from, to - from, &mut runs_left);
    crate::prep_cut::lap(crate::prep_cut::F::StaleWork, &mut t);
    stale
}

/// A fence signal on a queue ([`late`]): goes behind the flights in the
/// air, if there are any, and is set when they are settled. Whether it
/// did; if not the caller sets the fence now.
pub(crate) fn late_signal(fence: crate::object::This, value: u64) -> bool {
    if !late() {
        return false;
    }
    // (Neptune) `VICTORY_SIGNAL_NOW=1`, an experiment: fence signals are set when the title
    // asks, not behind the flights in the air; to see whether the title's frame waits for
    // them. What the title then reads may not be there yet.
    static NOW: OnceLock<bool> = OnceLock::new();
    if *NOW.get_or_init(|| std::env::var_os("VICTORY_SIGNAL_NOW").is_some_and(|v| v == "1")) {
        return false;
    }
    let mut line = line();
    if line.line.is_empty() && !line.settling {
        return false;
    }
    line.line
        .push(Late::Signal(crate::fence::FenceRef::retain(fence), value));
    line.pushed += 1;
    LATE_STATS[14].fetch_add(1, Ordering::Relaxed);
    drop(line);
    // A signal alone (behind a settle under way) has no flight to wake
    // the helper thread for it.
    LINE_MOVED.notify_one();
    RELAY_MOVED.notify_one();
    true
}

/// A queue stands at `wait`, which memory does not meet now ([`late`]):
/// whether it is met once the flights in the air have run. A flight's
/// bottom-of-pipe write of a value that meets it says yes without a wait
/// for the card: the part behind the wait goes to the card behind that
/// flight, which is the order the wait asks for. If the flights may write
/// the value in a way not known here (shaders, a timestamp, another
/// width), they are settled and memory is asked again.
pub(crate) fn late_met(wait: &crate::list::ValueWait) -> bool {
    let (address, bytes) = wait.place();
    {
        let line = line();
        if line.line.is_empty() && line.in_hand.is_empty() {
            return false;
        }
        let mut unsure = touches(&line.written, address, address + bytes);
        // (With the relay: first the writes of flights being landed.)
        let writes = line
            .in_hand
            .iter()
            .map(|(at, what, n)| (at, what, n))
            .chain(line.line.iter().filter_map(|item| match item {
                Late::Write(at, what, n) => Some((at, what, n)),
                _ => None,
            }));
        for (at, what, n) in writes {
            if *at >= address + bytes || address >= *at + *n {
                continue;
            }
            match what {
                crate::queue::Written::Value(v) if *at == address && *n >= bytes => {
                    if wait.holds_for(*v) {
                        LATE_STATS[13].fetch_add(1, Ordering::Relaxed);
                        return true;
                    }
                }
                _ => unsure = true,
            }
        }
        if !unsure {
            return false;
        }
    }
    settle_now(WHY_VALUE);
    wait.met()
}

/// Settles the flights in the air, if any ([`Host::settle`]), for reason
/// `why` (one of the `WHY_` places). Not from inside a submission.
pub(crate) fn settle_now(why: usize) {
    if !late() {
        return;
    }
    let Some(host) = host() else {
        return;
    };
    if relay() {
        // Wait for everything currently in flight (and any batch currently
        // being landed) on the relay thread without holding `submitted`, so
        // the submitting thread can keep preparing and submitting meanwhile.
        let want = {
            let line = line();
            if line.line.is_empty() && !line.settling {
                return;
            }
            line.pushed
        };
        relay_wait_inner(why, false, |line| line.landed_seq >= want);
        return;
    }
    let target = {
        let line = line();
        if line.line.is_empty() && !line.settling {
            return;
        }
        line.target
    };
    if target > 0 {
        host.wait_fence(target);
    }
    let mut submitted = host.submitted.lock().unwrap_or_else(|e| e.into_inner());
    host.settle(&mut submitted, why);
}

/// [`settle_now`] at a present, and for a fence the title blocks on.
pub(crate) fn settle_for_present() {
    settle_now(WHY_SYNC);
}
pub(crate) fn settle_for_fence() {
    settle_now(WHY_FENCE);
}

/// Resets the host lists of settled flights and keeps them for reuse, as
/// a queue does for the lists it ran (`crate::list::recycle`). The number of
/// lists the flights ran.
pub(crate) fn land() -> usize {
    let landed = std::mem::take(&mut line().landed);
    let mut lists_ran = 0;
    for (lists, order) in landed {
        lists_ran += order.len();
        crate::list::recycle(lists, &order);
    }
    lists_ran
}

fn fence_reached(completed: u64, target: u64) -> bool {
    completed != u64::MAX && completed >= target
}

/// The event is private to the waiting thread and the fence remains alive.
unsafe fn wait_host_fence(fence: &ID3D12Fence, target: u64, event: HANDLE, context: &str) {
    // SAFETY: caller owns the event and fence for the entire wait.
    if let Err(e) = unsafe { fence.SetEventOnCompletion(target, event) } {
        fail(format_args!("{context}: SetEventOnCompletion({target}) failed: {e}"));
    }
    let status = unsafe { WaitForSingleObject(event, INFINITE) };
    if status != windows::Win32::Foundation::WAIT_OBJECT_0 {
        fail(format_args!("{context}: fence wait failed: {status:?}"));
    }
    let completed = unsafe { fence.GetCompletedValue() };
    if !fence_reached(completed, target) {
        fail(format_args!("{context}: fence {completed} did not reach {target}"));
    }
}

#[cfg(test)]
#[test]
fn host_fence_completion_rejects_removed_device() {
    assert!(!fence_reached(u64::MAX, 1));
    assert!(!fence_reached(6, 7));
    assert!(fence_reached(7, 7));
    assert!(fence_reached(8, 7));
}

/// The thread that settles the flights once the card has run them all
/// ([`late`]): without it a bottom-of-pipe write would reach the title only
/// at the next call that settles, and a title thread that polls such a
/// value while the submitting thread waits for that thread would never
/// see it.
fn late_helper() {
    let Some(host) = host() else {
        return;
    };
    // SAFETY: a plain event of our own.
    let Ok(event) = (unsafe { CreateEventW(None, false, false, None) }) else {
        tracing::warn!("no event for the thread that settles submissions sent ahead");
        return;
    };
    loop {
        let target = {
            let mut line = line();
            loop {
                if line.poke {
                    line.poke = false;
                    break None;
                }
                // (With the relay the flights are another thread's.)
                if !relay() && !line.line.is_empty() && !line.settling {
                    break Some(line.target);
                }
                line = LINE_MOVED.wait(line).unwrap_or_else(|e| e.into_inner());
            }
        };
        let Some(target) = target else {
            land();
            crate::queue::release_held(false);
            continue;
        };
        // The card first, without the submission lock: the submitting
        // thread prepares and sends meanwhile.
        // SAFETY: our fence and event.
        unsafe {
            wait_host_fence(&host.fence, target, event, "late helper");
        }
        {
            let mut submitted = host.submitted.lock().unwrap_or_else(|e| e.into_inner());
            // Something newer went up meanwhile: the card is waited for
            // again, the submitting thread is not held up for it.
            if line().target != target {
                continue;
            }
            host.settle(&mut submitted, WHY_CARD_DONE);
        }
        land();
        crate::queue::release_held(false);
    }
}

/// Starts the threads of [`late`], once: the one that settles flights when
/// the card has run them and looks at what queues hold, and with [`relay`]
/// the one that lands flights.
pub(super) fn late_threads() {
    static HELPER: std::sync::Once = std::sync::Once::new();
    HELPER.call_once(|| {
        if let Err(e) = std::thread::Builder::new().name("victory-late-wait".into()).spawn(late_helper) {
            tracing::warn!(error = %e, "no thread to settle submissions sent ahead: they are settled by the calls that follow");
        }
        tracing::info!(
            most = late_most(),
            writes = late_writes(),
            "submissions are sent to the card without a wait for each (VICTORY_LATE_WAIT=1)"
        );
        if relay() {
            // Without it nothing would ever land.
            if let Err(e) = std::thread::Builder::new().name("victory-relay".into()).spawn(relay_worker) {
                fail(format_args!("no thread to land submissions sent ahead (VICTORY_RELAY=1): {e}"));
            }
            tracing::info!(
                exact = relay_exact(),
                "what shaders wrote is copied back by a thread of its own, beside the submitting thread (VICTORY_RELAY=1)"
            );
        }
    });
}

/// With [`relay`]: waits until `done` says so of the line, looked at each
/// time flights have landed. Counted as a settle for `why`, and its time
/// as waiting for the card. The caller may hold the submission lock: the
/// thread that lands flights never takes it.
pub(super) fn relay_wait(why: usize, done: impl Fn(&Line) -> bool) {
    relay_wait_inner(why, true, done);
}

fn relay_wait_inner(why: usize, holds_submitted: bool, done: impl Fn(&Line) -> bool) {
    let mut line = line();
    if done(&line) {
        return;
    }
    LATE_STATS[1].fetch_add(1, Ordering::Relaxed);
    LATE_STATS[why].fetch_add(1, Ordering::Relaxed);
    let _hitch = crate::hitch::span(crate::hitch::Phase::CardWait);
    let was_working = holds_submitted && working(false);
    let waiting = std::time::Instant::now();
    while !done(&line) {
        line = LINE_LANDED.wait(line).unwrap_or_else(|e| e.into_inner());
    }
    drop(line);
    if holds_submitted {
        stood(waiting);
        working(was_working);
    }
    let us = waiting.elapsed().as_micros() as u64;
    LATE_STATS[12].fetch_add(us, Ordering::Relaxed);
    LATE_WAITED[why.min(10)].fetch_add(us, Ordering::Relaxed);
    SUBMISSION_US[1].fetch_add(us, Ordering::Relaxed);
}

/// Waits on this thread's own event until `fence` reaches `value`; `false`
/// if there is no such event or the fence would not set it.
pub(super) fn wait_unlocked(fence: &ID3D12Fence, value: u64) -> bool {
    struct Event(HANDLE);
    impl Drop for Event {
        fn drop(&mut self) {
            // SAFETY: our own event, closed once at thread exit.
            let _ = unsafe { windows::Win32::Foundation::CloseHandle(self.0) };
        }
    }
    thread_local! {
        // Auto-reset, and every setting of it is waited for below, so none
        // is left over for the next wait.
        // SAFETY: a plain event creation.
        static EVENT: Option<Event> = unsafe { CreateEventW(None, false, false, None) }.ok().map(Event);
    }
    EVENT.with(|event| {
        let Some(event) = event else { return false };
        // SAFETY: the fence is free-threaded; the event is this thread's.
        unsafe {
            if let Err(e) = fence.SetEventOnCompletion(value, event.0) {
                tracing::warn!(error = %e, "host fence wait failed");
                return false;
            }
            WaitForSingleObject(event.0, INFINITE);
        }
        true
    })
}

/// The thread that lands flights ([`relay`]): as soon as the card has run
/// the first flights of the line it takes them, with what follows them,
/// and does what they left to do ([`Host::relay_land`]); else it waits for
/// the card, or for a flight. It takes neither the submission lock nor
/// anything a caller of [`Host::settle`] may hold: resetting the landed
/// lists and looking at what queues hold are left to [`late_helper`].
fn relay_worker() {
    crate::cores::keep_on_fast_cores();
    // (Neptune) This thread's jobs have kept threads of their own.
    crate::shadow::wide_for_back();
    let Some(host) = host() else {
        return;
    };
    // SAFETY: a plain event of our own.
    let Ok(event) = (unsafe { CreateEventW(None, false, false, None) }) else {
        fail(format_args!(
            "no event for the thread that lands submissions sent ahead (VICTORY_RELAY=1)"
        ));
    };
    let mut fetch: Option<RelayFetch> = None;
    loop {
        let next = {
            let mut line = line();
            loop {
                if line.line.is_empty() {
                    line = RELAY_MOVED.wait(line).unwrap_or_else(|e| e.into_inner());
                    continue;
                }
                let fences: Vec<Option<u64>> = line
                    .line
                    .iter()
                    .map(|l| match l {
                        Late::Run(flight) => Some(flight.fence),
                        _ => None,
                    })
                    .collect();
                // SAFETY: a plain query.
                let completed = unsafe { host.fence.GetCompletedValue() };
                if completed == u64::MAX {
                    drop(line);
                    fail(format_args!("relay fence reported device removal"));
                }
                let n = landable(&fences, completed);
                if n == 0 {
                    // The first thing is a flight the card has not run.
                    break Err(fences[0].unwrap_or(0));
                }
                line.settling = true;
                let items: Vec<Late> = line.line.drain(..n).collect();
                line.in_hand = items
                    .iter()
                    .filter_map(|l| match l {
                        Late::Write(at, what, bytes) => Some((*at, *what, *bytes)),
                        _ => None,
                    })
                    .collect();
                break Ok(items);
            }
        };
        match next {
            Ok(items) => host.relay_land(items, &mut fetch),
            Err(target) => {
                let waiting = std::time::Instant::now();
                // SAFETY: the host's fence and this thread's event.
                unsafe { wait_host_fence(&host.fence, target, event, "relay") };
                RELAY_STATS[2].fetch_add(waiting.elapsed().as_micros() as u64, Ordering::Relaxed);
            }
        }
    }
}

/// [`relay`]: what shaders wrote to the declared ranges `back` of copies
/// in the PC's memory goes into title memory. As [`hand_back`], each byte
/// once, the pieces that differ found on several threads; but the merge is
/// `Shadow::relay_merge`, which does not write the copy, and the ranges
/// are not done with here (each flight's are, afterwards).
pub(super) fn relay_hand_back(back: &[Declared]) {
    let mut work: Vec<(std::sync::Arc<crate::shadow::Shadow>, usize, usize)> = Vec::new();
    for (shadow, declared) in back {
        if shadow.carded() || (shadow.in_place() && crate::port_order::account_on()) {
            continue;
        }
        let mut spans: Vec<(usize, usize)> = Vec::new();
        join_ranges(&mut spans, declared.iter().copied());
        for &(from, to) in &spans {
            shadow.relay_compared(from, to - from);
        }
        work.extend(
            spans
                .into_iter()
                .map(|(from, to)| (shadow.clone(), from, to)),
        );
    }
    if work.is_empty() {
        return;
    }
    #[cfg(feature = "diagnostics")]
    let scanning = std::time::Instant::now();
    // Looked through without a lock: a piece that changes meanwhile (a
    // later flight writing, the title's bytes going in) is found now or by
    // that flight's landing, and the merge looks again under the lock.
    let found = crate::shadow::find_all(&work);
    #[cfg(feature = "diagnostics")]
    crate::copystat::add(
        crate::copystat::C::BackScanUs,
        scanning.elapsed().as_micros() as u64,
    );
    // Group filtered pieces by distinct shadow so parallel workers never
    // contend on the same `Shadow::before` lock inside `relay_merge`.
    let mut groups: Vec<(
        std::sync::Arc<crate::shadow::Shadow>,
        Vec<(usize, usize, usize)>,
    )> = Vec::new();
    let mut by_shadow: ByCopy<usize> = ByCopy::default();
    let mut total_pieces = 0usize;
    for (idx, ((shadow, _, _), found)) in work.iter().zip(found).enumerate() {
        let pieces = crate::shadow::lazy::filter_relay_found(shadow, found);
        if pieces.is_empty() {
            continue;
        }
        total_pieces += pieces.len();
        let key = std::sync::Arc::as_ptr(shadow) as usize;
        let g = *by_shadow.entry(key).or_insert_with(|| {
            groups.push((shadow.clone(), Vec::new()));
            groups.len() - 1
        });
        groups[g]
            .1
            .extend(pieces.into_iter().map(|(at, n)| (idx, at, n)));
    }
    if (relay_wide_merge() || wide_merge()) && total_pieces > 1 && groups.len() > 1 {
        let groups = std::sync::Arc::new(groups);
        #[cfg(feature = "diagnostics")]
        let per_work = std::sync::Arc::new(
            (0..work.len())
                .map(|_| std::sync::atomic::AtomicUsize::new(0))
                .collect::<Vec<_>>(),
        );
        #[cfg(feature = "diagnostics")]
        let per_work_w = per_work.clone();
        let total_groups = groups.len();
        crate::shadow::wide_chunks(total_groups, 1, move |start, end| {
            let (mut now, mut title) = (Vec::new(), Vec::new());
            let mut total = 0usize;
            for (shadow, pieces) in &groups[start..end] {
                for &(idx, at, n) in pieces {
                    let w = shadow.relay_merge(at, n, &mut now, &mut title);
                    total += w;
                    #[cfg(feature = "diagnostics")]
                    if w > 0 {
                        per_work_w[idx].fetch_add(w, Ordering::Relaxed);
                    }
                    #[cfg(not(feature = "diagnostics"))]
                    let _ = idx;
                }
            }
            if total > 0 {
                COPY_BACK[1].fetch_add(total as u64, Ordering::Relaxed);
            }
        });
        #[cfg(feature = "diagnostics")]
        for (i, &(_, from, to)) in work.iter().enumerate() {
            crate::copystat::compared(from, to - from, per_work[i].load(Ordering::Relaxed));
        }
        return;
    }
    let (mut now, mut title) = (Vec::new(), Vec::new());
    #[cfg(feature = "diagnostics")]
    let mut per_work = vec![0usize; work.len()];
    let mut total = 0usize;
    for (shadow, pieces) in &groups {
        for &(idx, at, n) in pieces {
            let w = shadow.relay_merge(at, n, &mut now, &mut title);
            total += w;
            #[cfg(feature = "diagnostics")]
            {
                per_work[idx] += w;
            }
            #[cfg(not(feature = "diagnostics"))]
            let _ = idx;
        }
    }
    if total > 0 {
        COPY_BACK[1].fetch_add(total as u64, Ordering::Relaxed);
    }
    #[cfg(feature = "diagnostics")]
    for (i, &(_, from, to)) in work.iter().enumerate() {
        crate::copystat::compared(from, to - from, per_work[i]);
    }
}

#[cfg(test)]
pub(crate) fn relay_hand_back_for_test(
    back: &[(std::sync::Arc<crate::shadow::Shadow>, Vec<(usize, usize)>)],
) {
    relay_hand_back(back);
}

/// For the frame-rate line, with [`late`]: what the line of flights did
/// per frame since the last one.
pub(super) fn late_line(frames: f64) {
    crate::watch::port_guard_line(frames);
    crate::heap_rounds::line(frames);
    crate::round_share::line(frames);
    if !late() {
        return;
    }
    let s: Vec<f64> = LATE_STATS
        .iter()
        .map(|s| s.swap(0, Ordering::Relaxed) as f64 / frames)
        .collect();
    tracing::info!(
        "submissions sent ahead per frame: {:.1} in {:.1} settles ({:.1} flights each time), {:.1} ms waiting for the card in them; \
         settled for: a synchronous submission or present {:.1}, the card had run them {:.1}, a wait for a memory value {:.1}, \
         title memory changed under shader writes {:.1}, title memory read {:.1} (by a buffer copy {:.1}, a fill of upload memory {:.1}, \
         a texture {:.1}), a step of ours {:.1}, {} lists in the air {:.1}, \
         bottom-of-pipe writes (VICTORY_LATE_WAIT_WRITES=0) {:.1}, a blocking fence wait {:.1}; \
         {:.1} waits for memory values met by a flight's bottom-of-pipe write, {:.1} fence signals set behind flights",
        s[0],
        s[1],
        if s[1] > 0.0 { s[11] / s[1] } else { 0.0 },
        s[12] / 1000.0,
        s[WHY_SYNC],
        s[WHY_CARD_DONE],
        s[WHY_VALUE],
        s[WHY_STALE],
        s[WHY_READ],
        s[15],
        s[16],
        s[17],
        s[WHY_RUN],
        late_most(),
        s[WHY_MOST],
        s[WHY_WRITES],
        s[WHY_FENCE],
        s[13],
        s[14],
    );
    {
        let w: Vec<f64> = LATE_WAITED
            .iter()
            .map(|w| w.swap(0, Ordering::Relaxed) as f64 / 1000.0 / frames)
            .collect();
        tracing::info!(
            "of the waiting for the card in settles, per frame, by what settled: a synchronous submission or present {:.1} ms, \
             the card had run them {:.1}, a wait for a memory value {:.1}, title memory changed under shader writes {:.1}, \
             title memory read {:.1}, a step of ours {:.1}, lists in the air {:.1}, bottom-of-pipe writes {:.1}, a blocking fence wait {:.1}",
            w[WHY_SYNC],
            w[WHY_CARD_DONE],
            w[WHY_VALUE],
            w[WHY_STALE],
            w[WHY_READ],
            w[WHY_RUN],
            w[WHY_MOST],
            w[WHY_WRITES],
            w[WHY_FENCE],
        );
    }
    if relay() {
        let r: Vec<f64> = RELAY_STATS
            .iter()
            .map(|r| r.swap(0, Ordering::Relaxed) as f64 / frames)
            .collect();
        tracing::info!(
            "the relay per frame (VICTORY_RELAY=1): {:.1} flights landed in {:.1} landings by the thread that copies back, \
             beside the submitting thread; that thread waited {:.1} ms for the card with nothing to land and {:.1} ms for \
             fetches of card copies' pieces, and was {:.1} ms landing in all (the frame line's \"copying back\" is its time; \
             \"settles\" above are waits of other threads for it)",
            r[1],
            r[0],
            r[2] / 1000.0,
            r[3] / 1000.0,
            r[4] / 1000.0,
        );
        if under_writes_masks() {
            let [blocks, ranges] =
                [0, 1].map(|i| CARD_MASKED[i].swap(0, Ordering::Relaxed) as f64 / frames);
            let passes = CARD_MASKED_PASSES.swap(0, Ordering::Relaxed) as f64 / frames;
            tracing::info!(
                "changed ranges sent up to the card by mask per frame (VICTORY_UNDER_WRITES_MASKS=1): {ranges:.0} ranges \
                 in {blocks:.0} blocks of 4 KiB, in {passes:.0} passes (one for a copy's blocks) and not a copy for every range"
            );
        }
        if relay_own_queue() {
            tracing::info!(
                "the relay's fetches per frame (VICTORY_RELAY_OWN_QUEUE=1): {:.0} pieces as the compare kept them, down {:.1} ms \
                 after they were asked for on a queue of their own; {:.0} pieces from the live buffers, behind the card's other \
                 work (no slot was free for them, or the card did not compare their range)",
                r[6],
                r[5] / 1000.0,
                r[7],
            );
        }
        crate::fetch_check::log(frames);
        if inflight_fetch() {
            let i: Vec<f64> = INFLIGHT_STATS
                .iter()
                .map(|i| i.swap(0, Ordering::Relaxed) as f64 / frames)
                .collect();
            tracing::info!(
                "card copies' pieces fetched inside their flights per frame (VICTORY_CARD_FETCH_INFLIGHT=1): {:.1} pieces \
                 ({:.2} MB) merged with no fetch of their own; {:.1} landings fetched the old way (the wait above), {:.1} of \
                 them because a flight's pieces overflowed the slots",
                i[0],
                i[1] / (1 << 20) as f64,
                i[3],
                i[2],
            );
        }
    }
    if late_exact() {
        let [pages, runs, bytes, unwatched, unsaid, many, unsnapped] =
            crate::shadow::merged_stats().map(|n| n as f64 / frames);
        let p: Vec<f64> = PREP_PARTS
            .iter()
            .map(|a| a.swap(0, Ordering::Relaxed) as f64 / (frames * 1000.0))
            .collect();
        let stale_aside = super::stale_aside::take(frames);
        tracing::info!(
            "preparing breakdown per frame: sync_heaps {:.2} ms, make_waiting {:.2} ms, ask_once {:.2} ms, stale_flights {:.2} ms,              run_flush_one {:.2} ms, run_other {:.2} ms, batch_flush {:.2} ms, snapshots {:.2} ms, card_before {:.2} ms, exec_submit {:.2} ms; \
             stale_flights aside {:.1} checks, {:.2} ms on victory-ask-aside, {:.2} ms of it waited for at the gate, \
             {:.1} made at the gate (VICTORY_STALE_ASIDE); heap-sync rounds asked ahead {:.1} (VICTORY_HEAP_ROUND_AHEAD)",
            p[0],
            p[1],
            p[2],
            p[3],
            p[4],
            p[5],
            p[6],
            p[7],
            p[8],
            p[9],
            stale_aside[0],
            stale_aside[1],
            stale_aside[2],
            stale_aside[3],
            crate::round_share::ahead_take(frames),
        );
        flush::log(frames);
        crate::card_prof::log(frames);
        crate::prep_cut::log(frames);
        tracing::info!(
            "title bytes put in under flights' shader writes per frame (VICTORY_LATE_WAIT=2): {pages:.0} pages looked at, \
             {runs:.0} changed runs, {:.2} MB; the flights were settled all the same {:.1} times: {unwatched:.1} for memory \
             not write-watched, {unsaid:.1} where the journal could not say, {many:.1} for more changed runs than a \
             submission sends up (VICTORY_UNDER_WRITES_RUNS, now {}), {unsnapped:.1} for a missing snapshot",
            bytes / (1 << 20) as f64,
            unwatched + unsaid + many + unsnapped,
            under_writes_runs(),
        );
    }
}

/// Whether a title buffer copy that continues the one recorded before it
/// in its list becomes one step with it (`VICTORY_JOIN_COPIES=1`; off by
/// default).
///
/// Each copy is done at submission through the kernel, which reports a
/// page that is not there instead of faulting (`copy_mapped`), tells the
/// write watch and the guards, and ends the refreshes gathered so far: a
/// call into the kernel for some 480 bytes, 14,042 times a frame in run 86
/// (6.8 MB; "moves 19 ms" with the refreshes' own reads). How many of them
/// continue the one before is counted either way and said on its own line
/// with the frame rate; the switch is worth that many kernel calls.
///
/// Joined, the same bytes move: [`continues`] joins only copies whose
/// ranges, taken together, do not overlap, and nothing is recorded between
/// two steps that follow each other. What differs: a page that is not
/// mapped now fails the joined copy as a whole (it then goes page by page,
/// zeros for what cannot be read, as a single copy does), where before
/// only the copy that held it did.
pub(super) fn join_copies() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_JOIN_COPIES").is_some_and(|v| v == "1"))
}

/// Since the last frame-rate line: title buffer copies recorded, and those
/// that continue the one before ([`join_copies`]).
pub(super) static COPIES_JOINED: [std::sync::atomic::AtomicU64; 2] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 2];

/// Whether the copy `next` (destination, source, bytes) goes on exactly
/// where `last` ended, source and destination both, and the two as one do
/// not overlap (source over destination): then one copy of both moves the
/// bytes the two would move in turn.
pub(super) fn continues(last: (usize, usize, usize), next: (usize, usize, usize)) -> bool {
    let ((d, s, n), (dst, src, len)) = (last, next);
    let Some(all) = n.checked_add(len) else {
        return false;
    };
    d.checked_add(n) == Some(dst)
        && s.checked_add(n) == Some(src)
        && (d.checked_add(all).is_some_and(|end| end <= s)
            || s.checked_add(all).is_some_and(|end| end <= d))
}

/// The ranges the shaders of `runs` may write, per host copy, as declared.
/// One list's lock at a time: a submission may name a list twice.
/// Per-phase submission preparation timers (microseconds since last frame line):
/// [0] sync_heaps, [1] make_waiting, [2] ask_once, [3] stale_flights,
/// [4] run_flush_one, [5] run_other, [6] batch_flush, [7] snapshots,
/// [8] card_before, [9] exec_submit.
pub(crate) static PREP_PARTS: [std::sync::atomic::AtomicU64; 10] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 10];

pub(super) fn declared_writes(runs: &[&HostList]) -> Vec<Declared> {
    let mut back: Vec<Declared> = Vec::new();
    let mut index: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();
    if step_index() {
        for list in runs {
            for (shadow, ranges) in &list.deferred().gpu_writes {
                let key = std::sync::Arc::as_ptr(shadow) as usize;
                if let Some(&i) = index.get(&key) {
                    let dst = &mut back[i].1;
                    for &range in ranges {
                        if !dst.contains(&range) {
                            dst.push(range);
                        }
                    }
                } else {
                    index.insert(key, back.len());
                    back.push((shadow.clone(), ranges.clone()));
                }
            }
        }
    } else {
        for list in runs {
            for step in &list.deferred().copies {
                if let Step::GpuWrite(shadow, address, len) = step {
                    let range = (*address, *address + *len);
                    match back
                        .iter_mut()
                        .find(|(s, _)| std::sync::Arc::ptr_eq(s, shadow))
                    {
                        Some((_, ranges)) => {
                            if !ranges.contains(&range) {
                                ranges.push(range);
                            }
                        }
                        None => back.push((shadow.clone(), vec![range])),
                    }
                }
            }
        }
    }
    back
}

/// The walls trace (`VICTORY_WALLS_TRACE=1` or `2`; off by default): log
/// lines that say what the title's CPU was given while it decided whether
/// the camera is in a room. The values of the small read-backs
/// ([`read_backs`]), the frame's main pass by scissor rectangle with its
/// culling dispatches (`draw.rs`, `walls_census`), the shader-written
/// buffers merged into title memory (`shadow.rs`, `walls_merged`), host
/// copies found stale after their refresh (`shadow.rs`; only with
/// `VICTORY_REFRESH_CHECK=1` as well: it costs about 100 ms a frame), and, in the
/// runtime, the read requests that fail (with 2: every request).
pub(crate) fn walls_trace() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var_os("VICTORY_WALLS_TRACE").is_some_and(|v| v == "1" || v == "2")
    })
}

/// After `list` has run: the texels its read-backs copied into host
/// buffers of ours go to the title's memory (`HostList::defer_read_back`).
///
/// A submission that is waited for did this itself. One sent ahead
/// ([`late`]) did not, at any stage: the flights' lists were settled
/// without it, so the title read a buffer nobody wrote, as before read-backs
/// were made (the exposure metering's two small float targets, every frame).
pub(super) fn read_backs(list: &HostList) {
    for work in &list.deferred().after {
        work();
    }
    for back in &list.deferred().texture_backs {
        crate::texback::land(back);
    }
    for (buffer, at, [row, rows, pitch], source, recorded) in &list.deferred().backs {
        let mut landed = false;
        let mut data = std::ptr::null_mut();
        // SAFETY: a read-back buffer of ours, written by the list that has
        // run; the title's destination holds every row (checked when the
        // copy was recorded).
        unsafe {
            if buffer.Map(0, None, Some(&mut data)).is_ok() && !data.is_null() {
                let from = data.cast::<u8>().cast_const();
                for line in 0..*rows {
                    std::ptr::copy_nonoverlapping(
                        from.add(line * row.next_multiple_of(256)),
                        (*at as *mut u8).add(line * pitch),
                        *row,
                    );
                }
                // The walls trace: what the title is handed (one row of at
                // most 24 values: its 5x1 and 1x1 float targets).
                if walls_trace() && *rows == 1 && *row <= 96 {
                    let mut values = [0f32; 24];
                    std::ptr::copy_nonoverlapping(
                        from,
                        values.as_mut_ptr().cast::<u8>(),
                        *row & !3,
                    );
                    tracing::info!(
                        frame = crate::frame::current(),
                        to = format_args!("{:#x}", *at),
                        bytes = *row,
                        values = ?&values[..*row / 4],
                        "walls trace: a read-back lands in title memory"
                    );
                }
                buffer.Unmap(0, None);
                landed = true;
            }
        }
        let bytes = row * rows;
        walls_landed(
            READ_BACK,
            *at,
            pitch * (rows - 1) + row,
            bytes,
            if landed { bytes } else { 0 },
            *source,
            Some(*recorded),
        );
        if landed {
            crate::round_share::touched([(*at, *at + pitch * (rows - 1) + row)]);
        }
    }
}

/// The walls trace's kinds of what reaches title memory from the card.
pub(crate) const READ_BACK: &str = "read-back";
pub(crate) const CARD_COPY: &str = "card copy";
pub(crate) const HOST_COPY: &str = "host copy";

/// The walls trace: one line a frame of what the card wrote that reached
/// title memory in it, each summed per kind and destination. It lists
/// every read-back, with the title texture it copies and the frame it was
/// recorded in, and every card copy merged back and host copy shaders
/// changed (those compared and found unchanged only counted). It shows
/// when the results of a pass the title's CPU reads, such as the depth
/// pyramid it tests occlusion against, reach the title, against the
/// frame its scissor rectangle first narrows. Logging only.
pub(crate) fn walls_landed(
    kind: &'static str,
    at: usize,
    len: usize,
    bytes: usize,
    written: usize,
    source: usize,
    recorded: Option<u64>,
) {
    if !walls_trace() {
        return;
    }
    static LANDINGS: Mutex<Option<Landings>> = Mutex::new(None);
    let line = LANDINGS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_or_insert_with(Landings::default)
        .add(
            crate::frame::current(),
            kind,
            at,
            len,
            bytes,
            written,
            source,
            recorded,
        );
    if let Some((frame, count, unchanged, landed)) = line {
        tracing::info!(
            frame,
            count,
            unchanged,
            landed = ?landed,
            "walls trace: what the card wrote reached title memory in this frame"
        );
    }
}

/// One frame's [`walls_landed`].
#[derive(Default)]
pub(super) struct Landings {
    frame: u64,
    entries: std::collections::BTreeMap<(&'static str, usize), Landing>,
    /// Copies compared and found unchanged.
    unchanged: u64,
}

#[derive(Clone, Copy)]
struct Landing {
    len: usize,
    count: u32,
    bytes: u64,
    written: u64,
    source: usize,
    /// The earliest frame one was recorded in, for read-backs.
    recorded: Option<u64>,
}

impl Landings {
    /// Notes one in `frame`; if it is the first of a new frame, the line of
    /// the frame before: its number, its entries, how many were unchanged,
    /// and the entries (at most 256).
    #[allow(clippy::too_many_arguments)]
    pub(super) fn add(
        &mut self,
        frame: u64,
        kind: &'static str,
        at: usize,
        len: usize,
        bytes: usize,
        written: usize,
        source: usize,
        recorded: Option<u64>,
    ) -> Option<(u64, usize, u64, Vec<String>)> {
        let line = if self.frame == frame {
            None
        } else {
            self.take()
        };
        self.frame = frame;
        if written == 0 && kind != READ_BACK {
            self.unchanged += 1;
            return line;
        }
        let e = self.entries.entry((kind, at)).or_insert(Landing {
            len,
            count: 0,
            bytes: 0,
            written: 0,
            source,
            recorded,
        });
        e.len = e.len.max(len);
        e.count += 1;
        e.bytes += bytes as u64;
        e.written += written as u64;
        e.recorded = match (e.recorded, recorded) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        line
    }

    /// The line of the frame noted so far, if anything was noted.
    pub(super) fn take(&mut self) -> Option<(u64, usize, u64, Vec<String>)> {
        if self.entries.is_empty() && self.unchanged == 0 {
            return None;
        }
        let landed = self
            .entries
            .iter()
            .take(256)
            .map(|((kind, at), l)| {
                let mut s = format!(
                    "{kind} {at:#x}+{:#x}: x{}, {} bytes, {} written",
                    l.len, l.count, l.bytes, l.written
                );
                if l.source != 0 {
                    s += &format!(", from {:#x}", l.source);
                }
                if let Some(r) = l.recorded {
                    s += &format!(", recorded in frame {r}");
                }
                s
            })
            .collect();
        let line = (self.frame, self.entries.len(), self.unchanged, landed);
        self.entries.clear();
        self.unchanged = 0;
        Some(line)
    }
}

/// `VICTORY_HEAP_ROUND_SHARE`: the ranges the flights of `items` wrote back
/// into title memory ([`crate::round_share::touched`]).
pub(super) fn heap_landed(items: &[Late]) {
    if !crate::round_share::on() {
        return;
    }
    crate::round_share::touched(
        items
            .iter()
            .filter_map(|i| match i {
                Late::Run(f) => Some(f),
                _ => None,
            })
            .flat_map(|f| {
                f.back
                    .iter()
                    .flat_map(|(_, r)| r.iter().copied())
                    .chain(f.tex_back.iter().copied())
            }),
    );
}

/// After the card ran what declared `back`: what shaders wrote there goes
/// into title memory, and the ranges are done with.
pub(super) fn hand_back(back: &[Declared]) {
    if crate::shadow::wide_back() {
        // Every range first (the pieces that differ, found on several
        // threads), then the merges, in order, as below.
        let mut work: Vec<(std::sync::Arc<crate::shadow::Shadow>, usize, usize)> = Vec::new();
        for (shadow, declared) in back {
            if shadow.carded() || (shadow.in_place() && crate::port_order::account_on()) {
                continue;
            }
            let mut sorted = declared.clone();
            sorted.sort_unstable();
            let mut spans: Vec<(usize, usize)> = Vec::new();
            for (from, to) in sorted {
                match spans.last_mut() {
                    Some(last) if from <= last.1 => last.1 = last.1.max(to),
                    _ => spans.push((from, to)),
                }
            }
            work.extend(
                spans
                    .into_iter()
                    .map(|(from, to)| (shadow.clone(), from, to)),
            );
        }
        #[cfg(feature = "diagnostics")]
        let scanning = std::time::Instant::now();
        let found = crate::shadow::find_all(&work);
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(
            crate::copystat::C::BackScanUs,
            scanning.elapsed().as_micros() as u64,
        );
        for ((shadow, from, to), found) in work.iter().zip(found) {
            let written = shadow.copy_back_with(*from, to - from, Some(found));
            walls_landed(HOST_COPY, *from, to - from, to - from, written, 0, None);
            COPY_BACK[1].fetch_add(written as u64, Ordering::Relaxed);
            #[cfg(feature = "diagnostics")]
            crate::copystat::compared(*from, to - from, written);
        }
        for (shadow, declared) in back {
            shadow.copy_back_done(declared);
        }
        return;
    }
    for (shadow, declared) in back {
        if shadow.carded() || (shadow.in_place() && crate::port_order::account_on()) {
            // Done before (`Host::card_back`), or nothing to do (in place).
            shadow.copy_back_done(declared);
            continue;
        }
        // Each byte compared once: the world declares dozens of views
        // of its 174 MB heap that overlap almost entirely, and comparing
        // every one whole was gigabytes a frame (0.3-0.5 s) for the few
        // MB shaders wrote. Only ranges that overlap or touch are
        // joined: what lies between two was not given to shaders (the
        // snapshot does not hold it).
        let mut sorted = declared.clone();
        sorted.sort_unstable();
        let mut spans: Vec<(usize, usize)> = Vec::new();
        for (from, to) in sorted {
            match spans.last_mut() {
                Some(last) if from <= last.1 => last.1 = last.1.max(to),
                _ => spans.push((from, to)),
            }
        }
        for (from, to) in spans {
            let written = shadow.copy_back(from, to - from);
            walls_landed(HOST_COPY, from, to - from, to - from, written, 0, None);
            COPY_BACK[1].fetch_add(written as u64, Ordering::Relaxed);
            #[cfg(feature = "diagnostics")]
            crate::copystat::compared(from, to - from, written);
        }
        shadow.copy_back_done(declared);
    }
}
