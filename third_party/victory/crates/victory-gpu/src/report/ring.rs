//! (El1xr) The last seconds of frame times, one entry per present, in a ring allocated once: a
//! push is a few stores, so keeping it costs nothing a frame would notice.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// How far back a report looks.
pub(crate) const WINDOW: Duration = Duration::from_secs(10);
/// Entries kept at most: ten seconds at 400 frames a second.
const CAP: usize = 4096;
/// A hitch: at least this long, and at least [`HITCH_X`] times the window's median.
pub(crate) const HITCH_MS: f32 = 50.0;
const HITCH_X: f32 = 2.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Entry {
    pub frame: u64,
    pub at: Instant,
    /// Milliseconds since the present before it.
    pub ms: f32,
}

pub(crate) struct Ring {
    entries: VecDeque<Entry>,
}

impl Ring {
    pub(crate) const fn new() -> Self {
        Ring {
            entries: VecDeque::new(),
        }
    }

    pub(crate) fn push(&mut self, e: Entry) {
        if self.entries.capacity() == 0 {
            self.entries.reserve_exact(CAP);
        }
        while self.entries.len() >= CAP
            || self
                .entries
                .front()
                .is_some_and(|f| e.at.duration_since(f.at) > WINDOW + WINDOW / 5)
        {
            self.entries.pop_front();
        }
        self.entries.push_back(e);
    }

    /// The entries of the `window` up to `until`, oldest first.
    pub(crate) fn since(&self, until: Instant, window: Duration) -> Vec<Entry> {
        self.entries
            .iter()
            .filter(|e| e.at <= until && until.duration_since(e.at) <= window)
            .copied()
            .collect()
    }

    /// The last `n` frame times, oldest first.
    pub(crate) fn last_ms(&self, n: usize) -> Vec<f32> {
        self.entries
            .iter()
            .skip(self.entries.len().saturating_sub(n))
            .map(|e| e.ms)
            .collect()
    }

    pub(crate) fn last_frame(&self) -> Option<u64> {
        self.entries.back().map(|e| e.frame)
    }
}

/// What a report says about its frames.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Summary {
    pub frames: usize,
    pub avg_ms: f32,
    pub p50_ms: f32,
    pub p99_ms: f32,
    pub worst_ms: f32,
    /// Over 33.3 ms (under 30 fps).
    pub over_33: usize,
    /// `(frame, ms)` of each hitch.
    pub hitches: Vec<(u64, f32)>,
}

/// The `p` quantile (0..1) of `sorted` (nearest rank).
fn quantile(sorted: &[f32], p: f32) -> f32 {
    if sorted.is_empty() {
        return 0.0;
    }
    let i = ((sorted.len() as f32 * p).ceil() as usize).clamp(1, sorted.len()) - 1;
    sorted[i]
}

/// The hitch threshold of a run of frame times: [`HITCH_MS`], or twice its median if longer.
pub(crate) fn hitch_threshold(ms: &[f32]) -> f32 {
    let mut sorted = ms.to_vec();
    sorted.sort_by(f32::total_cmp);
    (quantile(&sorted, 0.5) * HITCH_X).max(HITCH_MS)
}

pub(crate) fn summarise(entries: &[Entry]) -> Summary {
    let ms: Vec<f32> = entries.iter().map(|e| e.ms).collect();
    let mut sorted = ms.clone();
    sorted.sort_by(f32::total_cmp);
    let limit = hitch_threshold(&ms);
    Summary {
        frames: ms.len(),
        avg_ms: if ms.is_empty() {
            0.0
        } else {
            ms.iter().sum::<f32>() / ms.len() as f32
        },
        p50_ms: quantile(&sorted, 0.5),
        p99_ms: quantile(&sorted, 0.99),
        worst_ms: sorted.last().copied().unwrap_or(0.0),
        over_33: ms.iter().filter(|&&m| m > 1000.0 / 30.0).count(),
        hitches: entries
            .iter()
            .filter(|e| e.ms >= limit)
            .map(|e| (e.frame, e.ms))
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ring_of(ms: &[f32]) -> (Ring, Instant) {
        let t0 = Instant::now();
        let mut ring = Ring::new();
        let mut at = t0;
        for (i, &m) in ms.iter().enumerate() {
            at += Duration::from_secs_f32(m / 1000.0);
            ring.push(Entry {
                frame: i as u64 + 1,
                at,
                ms: m,
            });
        }
        (ring, at)
    }

    #[test]
    fn the_ring_keeps_about_ten_seconds_and_never_grows_past_its_cap() {
        let (ring, end) = ring_of(&[40.0; 600]);
        let kept = ring.since(end, WINDOW);
        assert_eq!(kept.len(), 251, "10 s of 40 ms frames, both ends included");
        assert_eq!(kept.last().unwrap().frame, 600);
        assert!(
            ring.entries.len() <= 301,
            "about 12 s kept: {}",
            ring.entries.len()
        );
        let (ring, _) = ring_of(&[1.0; CAP + 50]);
        assert_eq!(ring.entries.len(), CAP);
        assert_eq!(ring.entries.capacity(), CAP, "allocated once");
        assert_eq!(ring.last_frame(), Some(CAP as u64 + 50));
        assert_eq!(ring.last_ms(3), vec![1.0; 3]);
    }

    #[test]
    fn hitches_are_long_against_the_median_and_at_least_50_ms() {
        let mut ms = vec![40.0; 100];
        ms[10] = 85.0;
        ms[50] = 79.0;
        ms[70] = 60.0;
        let (ring, end) = ring_of(&ms);
        let s = summarise(&ring.since(end, WINDOW));
        assert_eq!(s.frames, 100);
        assert_eq!(
            s.hitches,
            vec![(11, 85.0)],
            "79 ms is under twice the 40 ms median"
        );
        assert_eq!(s.worst_ms, 85.0);
        assert_eq!(s.over_33, 100);
        assert_eq!(s.p50_ms, 40.0);
        let (ring, end) = ring_of(&[16.0, 16.0, 16.0, 45.0, 16.0, 55.0]);
        let s = summarise(&ring.since(end, WINDOW));
        assert_eq!(s.hitches, vec![(6, 55.0)], "a fast run still needs 50 ms");
        assert_eq!(s.over_33, 2);
        assert_eq!(summarise(&[]), Summary::default());
    }
}
