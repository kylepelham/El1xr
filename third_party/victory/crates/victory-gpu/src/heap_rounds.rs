//! Early heap-sync rounds (`VICTORY_HEAP_EARLY_ROUNDS=1`), adapted from
//! renderer port #7 (Belobug 34061929, `port_journal.rs` part 1) onto our
//! `descriptor::sync_heaps_start`/`sync_heaps_wait` tickets.
//!
//! Before: the submission asked `victory-heap-sync` for a round after its
//! settle and sweep, and when one of its lists wrote (or may have written) a
//! heap's title memory, or a settle copied back meanwhile, it ran a whole
//! `sync_heaps` again on the submitting thread after waiting for that round.
//!
//! With the switch: the round is asked for when the title's submission
//! enters `Host::submit`, before the settle, so it overlaps that too; and a
//! list that wrote a heap (or a settle) asks for a new round right then
//! ([`Rounds::wrote`]), which runs beside the rest of the preparing. The
//! submission waits for the last round it asked for. Each such round begins
//! after the writes that asked for it, so it sees them, as the synchronous
//! pass did. Shader writes landing in a heap's memory are not covered, as
//! before.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

pub(crate) fn on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_HEAP_EARLY_ROUNDS").is_none_or(|v| v != "0"))
}

/// Since the last line: microseconds waiting for a round, microseconds of
/// synchronous passes, synchronous passes, rounds asked mid-preparing,
/// submissions that waited.
static STATS: [AtomicU64; 5] = [const { AtomicU64::new(0) }; 5];
const WAIT_US: usize = 0;
const SYNC_US: usize = 1;
const SYNCS: usize = 2;
const MID_ROUNDS: usize = 3;
const WAITS: usize = 4;

/// The heap-sync round a submission being prepared waits for.
pub(crate) struct Rounds {
    ticket: Option<u64>,
    /// Something the submission did wrote, or may have written, a heap's
    /// title memory after its round was asked for, and no newer round was.
    stale: bool,
}

impl Rounds {
    /// No round asked yet (the switch is off, or the lists use no heaps).
    pub(crate) const NONE: Rounds = Rounds {
        ticket: None,
        stale: false,
    };

    pub(crate) fn start() -> Rounds {
        Rounds {
            ticket: Some(crate::descriptor::sync_heaps_start()),
            stale: false,
        }
    }

    /// [`Rounds::start`] for a submission of `runs`; with
    /// `VICTORY_HEAP_ROUND_SHARE=1` a round already done or asked for that
    /// began after every list of it was given to its queue (and after the
    /// last write we made into a heap) is its round ([`crate::round_share`]).
    pub(crate) fn start_for(runs: &[&crate::host::HostList]) -> Rounds {
        if !crate::round_share::on() {
            return Rounds::start();
        }
        match crate::round_share::start(runs.iter().map(|l| &l.heap_stamp)) {
            Some(ticket) => Rounds {
                ticket: Some(ticket),
                stale: false,
            },
            None => Rounds::start(),
        }
    }

    pub(crate) fn asked(&self) -> bool {
        self.ticket.is_some()
    }

    /// A list or a settle wrote, or may have written, a heap's title memory.
    /// Early: a new round is asked for now. Otherwise the wait ends with a
    /// synchronous pass, as before.
    pub(crate) fn wrote(&mut self) {
        if self.ticket.is_none() {
            return;
        }
        if on() {
            self.ticket = Some(crate::descriptor::sync_heaps_start());
            STATS[MID_ROUNDS].fetch_add(1, Relaxed);
        } else {
            self.stale = true;
        }
    }

    /// Waits for the last round asked for (and the synchronous pass, if one
    /// is due); whether there was one.
    pub(crate) fn wait(self) -> bool {
        let Some(ticket) = self.ticket else {
            return false;
        };
        let began = std::time::Instant::now();
        crate::descriptor::sync_heaps_wait(ticket);
        STATS[WAIT_US].fetch_add(began.elapsed().as_micros() as u64, Relaxed);
        STATS[WAITS].fetch_add(1, Relaxed);
        if self.stale {
            let began = std::time::Instant::now();
            crate::descriptor::sync_heaps();
            STATS[SYNC_US].fetch_add(began.elapsed().as_micros() as u64, Relaxed);
            STATS[SYNCS].fetch_add(1, Relaxed);
        }
        true
    }
}

pub(crate) fn line(frames: f64) {
    if frames <= 0.0 {
        return;
    }
    let [wait, sync, syncs, mid, waits] =
        STATS.each_ref().map(|n| n.swap(0, Relaxed) as f64 / frames);
    tracing::info!(
        "heap-sync rounds per frame (VICTORY_HEAP_EARLY_ROUNDS={}): {waits:.1} submissions waited {:.2} ms for their round; \
         {mid:.1} rounds asked mid-preparing; {syncs:.1} synchronous passes {:.2} ms",
        u8::from(on()),
        wait / 1000.0,
        sync / 1000.0,
    );
}
