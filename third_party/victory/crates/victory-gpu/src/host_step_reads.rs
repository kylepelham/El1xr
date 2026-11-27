//! Which of a list's submission-time steps read title memory, kept as they
//! are recorded (`VICTORY_BLOCKED_INDEX=1`, off by default), so that
//! [`blocked_by`] looks only at those instead of at every step.
//!
//! With flights in the air every submission asks whether preparing it reads
//! what their shaders may write (`super::late_blocked`). The walk looked at
//! every step of every list: the constant buffers' fills (some 10,000 a
//! frame), the refreshes and the shaders' declared writes among them, none
//! of which can block. What can: the title's buffer copies (source and
//! destination), fills of upload memory that wait for flights, textures'
//! memory, and steps of ours that run on title memory. Those are noted here
//! with their place in the list, and the answer is the one the walk gives:
//! the first such step, in order, that blocks.

use super::{Deferred, HostList, LATE_STATS, Step, WHY_READ, WHY_RUN};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

/// Whether the steps are noted and [`blocked_by`] uses the notes.
pub(super) fn on() -> bool {
    #[cfg(test)]
    if tests::FORCED.get() {
        return true;
    }
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_BLOCKED_INDEX").is_some_and(|v| v == "1"))
}

/// What reads title memory: a buffer copy, a fill, a texture (as the
/// walk's counters, `LATE_STATS[15 + kind]`).
const COPY: u8 = 0;
const FILL: u8 = 1;
const TEXTURE: u8 = 2;

/// A list's steps that may block, in recorded order.
#[derive(Default)]
pub(super) struct Reads {
    /// Ranges `[from, to)` read or written on the CPU: the step's place,
    /// what it is.
    ranges: Vec<(usize, usize, usize, u8)>,
    /// The place of the first step of ours that runs on title memory.
    first_run: Option<usize>,
}

impl Reads {
    fn read(&mut self, at: usize, kind: u8, from: usize, len: usize) {
        self.ranges.push((from, from.saturating_add(len), at, kind));
    }

    /// The first step that blocks against `written` (in order, joined):
    /// `Err(())` a step of ours, `Ok(kind)` a read.
    fn first(&self, written: &[(usize, usize)]) -> Option<Result<u8, ()>> {
        let read = self
            .ranges
            .iter()
            .filter(|r| touches(written, r.0, r.1))
            .min_by_key(|r| r.2)
            .map(|r| (r.2, r.3));
        match (read, self.first_run) {
            (Some((at, _)), Some(run)) if run < at => Some(Err(())),
            (Some((_, kind)), _) => Some(Ok(kind)),
            (None, Some(_)) => Some(Err(())),
            (None, None) => None,
        }
    }
}

/// As `super::touches`: whether `[from, to)` meets one of `ranges` (in
/// order, joined).
fn touches(ranges: &[(usize, usize)], from: usize, to: usize) -> bool {
    if from >= to {
        return false;
    }
    let at = ranges.partition_point(|r| r.1 <= from);
    ranges.get(at).is_some_and(|r| r.0 < to)
}

impl Deferred {
    /// Records `step`, noting it in [`Reads`] when it may block.
    pub(super) fn push_step(&mut self, step: Step) {
        if on() {
            let at = self.copies.len();
            match &step {
                &Step::Copy(dst, src, len) => {
                    self.reads.read(at, COPY, src, len);
                    self.reads.read(at, COPY, dst, len);
                }
                &Step::Fill(_, src, len, constants) if !constants => {
                    self.reads.read(at, FILL, src, len)
                }
                Step::Texture(upload) => {
                    let (from, len) = upload.staging.memory();
                    self.reads.read(at, TEXTURE, from, len);
                }
                Step::Run(_) => {
                    self.reads.first_run.get_or_insert(at);
                }
                _ => {}
            }
        }
        self.copies.push(step);
    }

    /// The last step, a buffer copy, goes on over `[dst, dst + len)` from
    /// `[src, src + len)` (`super::join_copies`).
    pub(super) fn copy_joined(&mut self, dst: usize, src: usize, len: usize) {
        if on() {
            let at = self.copies.len().saturating_sub(1);
            self.reads.read(at, COPY, src, len);
            self.reads.read(at, COPY, dst, len);
        }
    }
}

/// `super::blocked_by`: why `runs` cannot be prepared while flights whose
/// shaders may write `written` (in order, joined) are in the air, if so.
pub(super) fn blocked_by(
    written: &[(usize, usize)],
    runs: &[&HostList],
    count: bool,
) -> Option<usize> {
    if !count {
        return answer(written, runs, count);
    }
    let started = std::time::Instant::now();
    let why = answer(written, runs, count);
    ASKED[0].fetch_add(1, Ordering::Relaxed);
    ASKED[1].fetch_add(started.elapsed().as_nanos() as u64, Ordering::Relaxed);
    why
}

/// Since the last frame line: submissions asked about (`super::late_blocked`)
/// and the nanoseconds the answers took.
static ASKED: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

/// The frame line's.
pub(super) fn line(frames: f64) {
    let [asked, ns] = ASKED
        .each_ref()
        .map(|a| a.swap(0, Ordering::Relaxed) as f64 / frames);
    if asked > 0.0 {
        tracing::info!(
            "submissions asked whether they read what flights write, per frame: {asked:.1} in {:.3} ms \
             (VICTORY_BLOCKED_INDEX={}: {})",
            ns / 1e6,
            u8::from(on()),
            if on() {
                "the noted steps"
            } else {
                "every step walked"
            }
        );
    }
}

fn answer(written: &[(usize, usize)], runs: &[&HostList], count: bool) -> Option<usize> {
    // The card copies' steps (`VICTORY_CARD_READ_COPY=1`) block or not as `readcopy::decide`
    // sets them at submission: not noted here, so the walk answers.
    if !on() || super::readcopy::card_read_copy() {
        return super::blocked_by_walk(written, runs, count);
    }
    if written.is_empty() {
        return None;
    }
    for list in runs {
        match list.deferred().reads.first(written) {
            None => continue,
            Some(Err(())) => return Some(WHY_RUN),
            Some(Ok(kind)) => {
                if count {
                    LATE_STATS[15 + usize::from(kind)].fetch_add(1, Ordering::Relaxed);
                }
                return Some(WHY_READ);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    // The notes, on for the test that records real lists. Per thread: tests run side by side, and only this test's own calls may see it.
    thread_local! {
        pub(super) static FORCED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    /// Lists recorded with the host's own calls (copies, joined copies, fills
    /// of both kinds, steps of ours) answer as the walk does.
    #[test]
    fn recorded_lists_answer_as_the_walk() {
        let Some(host) = super::super::host() else {
            return;
        };
        FORCED.set(true);
        let memory = vec![0u8; 1 << 16];
        let base = memory.as_ptr() as usize;
        let mut rng = Rng(0x2545_f491_4f6c_dd1d);
        for _ in 0..150 {
            let lists: Vec<HostList> = (0..1 + rng.below(3))
                .map(|_| host.new_list().unwrap())
                .collect();
            for list in &lists {
                let mut last: Option<(usize, usize, usize)> = None;
                for _ in 0..rng.below(30) {
                    let (a, b, len) = (rng.below(60_000), rng.below(60_000), 1 + rng.below(256));
                    // Never run: the lists are not submitted.
                    let (to, from) = ((base + a) as *mut u8, (base + b) as *const u8);
                    match rng.below(9) {
                        0 => {
                            list.defer_copy(to, from, len);
                            last = Some((a, b, len));
                        }
                        1 => {
                            if let Some((d, s, n)) = last
                                .filter(|l| l.0 + l.2 + len < 60_000 && l.1 + l.2 + len < 60_000)
                            {
                                list.defer_copy(
                                    (base + d + n) as *mut u8,
                                    (base + s + n) as *const u8,
                                    len,
                                );
                                last = Some((d, s, n + len));
                            }
                        }
                        2 | 3 => list.defer_fill(to, from, len),
                        4 | 5 => list.defer_constants(to, from, len),
                        6 if rng.below(3) == 0 => list.defer(|| {}),
                        _ => list.defer_host(|| {}),
                    }
                }
            }
            let runs: Vec<&HostList> = lists.iter().collect();
            let mut written: Vec<(usize, usize)> = Vec::new();
            let mut at = base + rng.below(4096);
            for _ in 0..rng.below(8) {
                let len = 1 + rng.below(2048);
                written.push((at, at + len));
                at += len + 1 + rng.below(8192);
            }
            assert_eq!(
                blocked_by(&written, &runs, false),
                super::super::blocked_by_walk(&written, &runs, false)
            );
        }
        FORCED.set(false);
        drop(memory);
    }

    /// A small generator, the same sequence every run.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, n: u64) -> usize {
            (self.next() % n) as usize
        }
    }

    /// The walk, over a model of the steps: the first that blocks.
    #[derive(Clone, Copy)]
    enum Model {
        Copy(usize, usize, usize),
        Fill(usize, usize, bool),
        Run,
        Other,
    }

    fn walk(steps: &[Model], written: &[(usize, usize)]) -> Option<Result<u8, ()>> {
        for s in steps {
            match *s {
                Model::Copy(dst, src, len) => {
                    if touches(written, src, src + len) || touches(written, dst, dst + len) {
                        return Some(Ok(COPY));
                    }
                }
                Model::Fill(src, len, constants) => {
                    if !constants && touches(written, src, src + len) {
                        return Some(Ok(FILL));
                    }
                }
                Model::Run => return Some(Err(())),
                Model::Other => {}
            }
        }
        None
    }

    fn noted(steps: &[Model]) -> Reads {
        let mut reads = Reads::default();
        for (at, s) in steps.iter().enumerate() {
            match *s {
                Model::Copy(dst, src, len) => {
                    reads.read(at, COPY, src, len);
                    reads.read(at, COPY, dst, len);
                }
                Model::Fill(src, len, false) => reads.read(at, FILL, src, len),
                Model::Run => {
                    reads.first_run.get_or_insert(at);
                }
                _ => {}
            }
        }
        reads
    }

    #[test]
    fn the_notes_give_the_walks_answer() {
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        for _ in 0..4000 {
            let n = rng.below(40);
            let steps: Vec<Model> = (0..n)
                .map(|_| {
                    let (a, b, len) = (rng.below(4096), rng.below(4096), 1 + rng.below(64));
                    match rng.below(10) {
                        0 => Model::Copy(a, b, len),
                        1..=5 => Model::Fill(a, len, rng.below(2) == 0),
                        6 if rng.below(4) == 0 => Model::Run,
                        _ => Model::Other,
                    }
                })
                .collect();
            let mut written: Vec<(usize, usize)> = Vec::new();
            let mut at = rng.below(512);
            for _ in 0..rng.below(6) {
                let len = 1 + rng.below(256);
                written.push((at, at + len));
                at += len + 1 + rng.below(1024);
            }
            assert_eq!(
                noted(&steps).first(&written),
                walk(&steps, &written),
                "{written:?}"
            );
        }
    }

    #[test]
    fn a_step_of_ours_before_a_read_answers_first_and_after_it_not() {
        let written = [(100, 200)];
        let read = Model::Copy(150, 5000, 8);
        assert_eq!(noted(&[Model::Run, read]).first(&written), Some(Err(())));
        assert_eq!(noted(&[read, Model::Run]).first(&written), Some(Ok(COPY)));
        // Constant buffers' fills never block; others do.
        assert_eq!(noted(&[Model::Fill(120, 4, true)]).first(&written), None);
        assert_eq!(
            noted(&[Model::Fill(120, 4, false)]).first(&written),
            Some(Ok(FILL))
        );
        assert_eq!(noted(&[read]).first(&[]), None);
    }
}
