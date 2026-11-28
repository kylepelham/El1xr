//! (feedcard, #88) `VICTORY_COPIED_SHARED=1` (Live; off by default): the looks at a host
//! copy's page stamps that change nothing take the copy's lock shared.
//!
//! The stamps (`CopiedPages`) sit behind one lock per copy. The wide flush cuts a big copy into
//! pieces and refreshes them on up to 16 threads at once, and nearly every piece first asks
//! whether anything is new in it (`watch::nothing_new`: a read of its pages' stamps against the
//! journal's). Under a plain lock those looks stood in line with one another and with the stale
//! copy's plans of the other pieces: b5g's FLUSH_PROF line put the clean checks at 8,835 calls
//! and 17 ms a frame over all threads, and the plans' wait for the lock at 1 ms of their 2.5.
//!
//! The lock is now a reader-writer lock. Every site that changes the stamps takes it exclusive
//! ([`CopiedLock::lock`], as before). The looks ([`CopiedLock::look`]) take it shared with the
//! switch on and exclusive with it off, so off is the old lock exactly. A look under the shared
//! lock reads the same state as under the exclusive one (no writer runs beside it), so its answer
//! is the same: only looks run beside looks. Not in the cache key: no shader or pipeline output.

use std::ops::Deref;
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

use super::CopiedPages;

static ON: victory_core::settings::Live =
    victory_core::settings::Live::new("VICTORY_COPIED_SHARED", |v| u64::from(v == Some("1")));

/// Live: read at each look.
pub(crate) fn on() -> bool {
    ON.get_bool()
}

/// A copy's page stamps behind a reader-writer lock.
pub(super) struct CopiedLock(RwLock<CopiedPages>);

/// A look's hold on the stamps: shared with the switch, exclusive without it.
pub(super) enum Looked<'a> {
    Shared(RwLockReadGuard<'a, CopiedPages>),
    Alone(RwLockWriteGuard<'a, CopiedPages>),
}

impl Deref for Looked<'_> {
    type Target = CopiedPages;
    fn deref(&self) -> &CopiedPages {
        match self {
            Looked::Shared(g) => g,
            Looked::Alone(g) => g,
        }
    }
}

impl CopiedLock {
    pub(super) fn new(pages: CopiedPages) -> Self {
        Self(RwLock::new(pages))
    }

    /// The stamps to change: exclusive, as the plain lock was.
    pub(super) fn lock(&self) -> std::sync::LockResult<RwLockWriteGuard<'_, CopiedPages>> {
        self.0.write()
    }

    /// The stamps to read only, shared whatever the switch (`VICTORY_PLAN_SNAPSHOT`'s snapshot).
    pub(super) fn read(&self) -> RwLockReadGuard<'_, CopiedPages> {
        self.0.read().unwrap_or_else(|e| e.into_inner())
    }

    /// The stamps to read only.
    pub(super) fn look(&self) -> Looked<'_> {
        if on() {
            Looked::Shared(self.0.read().unwrap_or_else(|e| e.into_inner()))
        } else {
            Looked::Alone(self.0.write().unwrap_or_else(|e| e.into_inner()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_switch_is_live() {
        assert_eq!(
            on(),
            std::env::var_os("VICTORY_COPIED_SHARED").is_some_and(|v| v == "1")
        );
        if std::env::var_os("VICTORY_COPIED_SHARED").is_some() {
            return;
        }
        victory_core::settings::set("VICTORY_COPIED_SHARED", "1").unwrap();
        let now = on();
        victory_core::settings::set("VICTORY_COPIED_SHARED", "").unwrap();
        assert!(now, "a change takes effect while running");
        assert!(!on());
    }

    #[test]
    fn looks_see_what_the_exclusive_lock_wrote_and_share_with_the_switch() {
        let c = CopiedLock::new(CopiedPages::new(8));
        c.lock().unwrap().set(3, 7);
        let off = c.look();
        assert!(matches!(off, Looked::Alone(_)) || on());
        assert_eq!(off.get(3), Some(7));
        drop(off);
        // Two looks at once: shared, both see the stamp; a writer would have to wait.
        let (a, b) = (
            Looked::Shared(c.0.read().unwrap()),
            Looked::Shared(c.0.read().unwrap()),
        );
        assert_eq!((a.get(3), b.get(3)), (Some(7), Some(7)));
        assert!(c.0.try_write().is_err(), "no writer beside the looks");
    }

    /// (#88) Sizes the switch: 16 threads ask `nothing_new`-sized questions of 1 MiB pieces of
    /// one copy (16 stamps against 16 journal stamps read from a shared table), a fifth of them
    /// followed by a plan under the exclusive lock, with the looks exclusive (switch off) and
    /// shared (on). Ignored; `--ignored --nocapture`, release for real numbers.
    #[test]
    #[ignore]
    fn bench_looks_beside_plans() {
        use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
        const PAGES: usize = 4096;
        const THREADS: usize = 16;
        const ROUNDS: usize = 400;
        let journal: Vec<AtomicU64> = (0..PAGES).map(|_| AtomicU64::new(0)).collect();
        for shared in [false, true, false, true] {
            let c = CopiedLock::new(CopiedPages::new(PAGES));
            c.lock().unwrap().at.iter_mut().for_each(|s| *s = 1);
            let spent = AtomicU64::new(0);
            let wall = std::time::Instant::now();
            std::thread::scope(|sc| {
                for t in 0..THREADS {
                    let (c, journal, spent) = (&c, &journal, &spent);
                    sc.spawn(move || {
                        for round in 1..=ROUNDS as u64 {
                            for i in (t..PAGES / 16).step_by(THREADS) {
                                if (i as u64 + round) % 5 == 0 {
                                    journal[i * 16].store(round + 1, Relaxed);
                                }
                                let at = std::time::Instant::now();
                                let new = {
                                    let g = if shared {
                                        Looked::Shared(c.0.read().unwrap())
                                    } else {
                                        Looked::Alone(c.0.write().unwrap())
                                    };
                                    (i * 16..i * 16 + 16)
                                        .any(|p| journal[p].load(Relaxed) > g.at[p])
                                };
                                if new {
                                    let mut g = c.lock().unwrap();
                                    for p in i * 16..i * 16 + 16 {
                                        let s = journal[p].load(Relaxed);
                                        g.set(p, s.max(1));
                                    }
                                }
                                spent.fetch_add(at.elapsed().as_nanos() as u64, Relaxed);
                            }
                        }
                    });
                }
            });
            let looks = (PAGES / 16 * ROUNDS) as f64;
            println!(
                "looks on {THREADS} threads, shared={shared}: {:.3} us a look and its plan (all threads), \
                 wall {:.1} ms for {looks} looks",
                spent.into_inner() as f64 / 1e3 / looks,
                wall.elapsed().as_secs_f64() * 1e3
            );
        }
    }
}
