//! What became of the device calls that make pipelines from loose pieces,
//! for the operator's log.
//!
//! Method 0x1c8 with `VICTORY_PIPELINE_PIECES=1` (`pipeline::pieces`): each
//! reason a pipeline was refused is logged once, and every 1,024 calls a
//! line counts the pipelines made and the refusals by reason, most first
//! (ported from RipcuBAN/victory-fork d4bc52f).
//!
//! Method 0x1b8, `CreateDerivedPipelineStateX(base, count, patches, iid,
//! out)` by palmetto's notes (slot 55): a pipeline made from another with
//! some state patched. It is not implemented and answers `E_NOTIMPL` as the
//! table's stub did; palmetto saw 41,545 such calls once 0x1c8 made
//! pipelines, so each call is counted and logged at powers of two, with the
//! first call's arguments.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::object::{E_NOTIMPL, HRESULT, Iid, This};

/// Reasons kept; more are counted under the last line's total only.
const REASONS: usize = 64;
/// 0x1c8 calls between two lines of the table.
const EVERY: u32 = 1024;

#[derive(Default)]
struct Outcomes {
    made: u64,
    refused: u64,
    reasons: Vec<(&'static str, u64)>,
}

impl Outcomes {
    /// Counts a refusal; true the first time its reason is seen.
    fn refuse(&mut self, why: &'static str) -> bool {
        self.refused += 1;
        if let Some(r) = self.reasons.iter_mut().find(|r| r.0 == why) {
            r.1 += 1;
            false
        } else if self.reasons.len() < REASONS {
            self.reasons.push((why, 1));
            true
        } else {
            false
        }
    }

    fn line(&self, calls: u32) -> String {
        let mut reasons = self.reasons.clone();
        reasons.sort_by(|a, b| b.1.cmp(&a.1));
        let top: Vec<String> = reasons
            .iter()
            .take(12)
            .map(|(why, n)| format!("{n}x {why}"))
            .collect();
        format!(
            "0x1c8 pieces (VICTORY_PIPELINE_PIECES=1): calls={calls} made={} refused={}: {}",
            self.made,
            self.refused,
            top.join(" | ")
        )
    }
}

static OUTCOMES: Mutex<Outcomes> = Mutex::new(Outcomes {
    made: 0,
    refused: 0,
    reasons: Vec::new(),
});

/// The outcome of 0x1c8 call number `calls` with the switch on: `Ok` a
/// pipeline made, `Err` the reason it was not.
pub(crate) fn pieces_outcome(calls: u32, outcome: Result<(), &'static str>) {
    let mut o = OUTCOMES.lock().unwrap_or_else(|e| e.into_inner());
    match outcome {
        Ok(()) => o.made += 1,
        Err(why) => {
            if o.refuse(why) {
                tracing::warn!(
                    slot = "0x1c8",
                    calls,
                    "a pipeline is not made from its pieces: {why} (once a reason)"
                );
            }
        }
    }
    if calls.is_multiple_of(EVERY) {
        tracing::info!("{}", o.line(calls));
    }
}

/// 0x1b8 calls since the start.
static DERIVED: AtomicU64 = AtomicU64::new(0);

/// Device method 0x1b8 (`CreateDerivedPipelineStateX`): counted and logged,
/// not implemented. The arguments are taken as words, since only the first
/// call's are logged and their types are palmetto's reading.
pub(crate) extern "system" fn derived(
    _: This,
    base: usize,
    count: usize,
    patches: usize,
    _iid: *const Iid,
    _out: *mut This,
) -> HRESULT {
    let calls = DERIVED.fetch_add(1, Ordering::Relaxed) + 1;
    if calls == 1 {
        tracing::warn!(
            slot = "0x1b8",
            base = format_args!("{base:#x}"),
            count = count as u32,
            patches = format_args!("{patches:#x}"),
            first_patch_words = format_args!("{:x?}", first_words(patches)),
            "CreateDerivedPipelineStateX is not implemented: the title goes without this pipeline"
        );
    }
    if calls.is_power_of_two() {
        tracing::info!(
            "0x1b8 CreateDerivedPipelineStateX: calls={calls} (not implemented, E_NOTIMPL)"
        );
    }
    E_NOTIMPL
}

/// The first four words at `at`, read through the kernel (any address).
#[cfg(windows)]
fn first_words(at: usize) -> Option<[u32; 4]> {
    let mut bytes = [0u8; 16];
    (at >= 0x1_0000 && crate::host::read_into(at as *const u8, &mut bytes)).then(|| {
        let mut w = [0u32; 4];
        for (to, c) in w.iter_mut().zip(bytes.as_chunks::<4>().0) {
            *to = u32::from_le_bytes(*c);
        }
        w
    })
}

#[cfg(not(windows))]
fn first_words(_: usize) -> Option<[u32; 4]> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refusals_are_counted_by_reason_most_first() {
        let mut o = Outcomes::default();
        assert!(o.refuse("b"));
        assert!(!o.refuse("b"));
        assert!(o.refuse("a"));
        assert!(!o.refuse("b"));
        o.made = 5;
        assert_eq!(
            o.line(1024),
            "0x1c8 pieces (VICTORY_PIPELINE_PIECES=1): calls=1024 made=5 refused=4: 3x b | 1x a"
        );
        // reasons past the cap count in the total only
        let mut full = Outcomes::default();
        for why in (0..REASONS + 3).map(|n| -> &'static str { format!("r{n}").leak() }) {
            full.refuse(why);
        }
        assert_eq!(
            (full.reasons.len(), full.refused),
            (REASONS, REASONS as u64 + 3)
        );
    }

    #[test]
    fn derived_pipelines_are_counted_and_not_made() {
        use crate::object::slot;
        type Derived =
            extern "system" fn(This, usize, usize, usize, *const Iid, *mut This) -> HRESULT;
        let device = crate::device::device();
        let before = DERIVED.load(Ordering::Relaxed);
        let mut out = std::ptr::null();
        let method = slot::<Derived>(device, 0x1b8);
        assert_eq!(
            method(device, 0, 1, 0, std::ptr::null(), &mut out),
            E_NOTIMPL
        );
        assert!(out.is_null());
        assert!(DERIVED.load(Ordering::Relaxed) > before);
    }
}
