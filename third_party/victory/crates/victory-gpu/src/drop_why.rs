//! Why draws, mesh dispatches, dispatches and indirect executions were not
//! run on the host (`VICTORY_DROP_REASONS=1`): one log line per frame line, each
//! reason counted per frame by where it was dropped. The frame line's
//! "dropped ... without a host pipeline" and "... indirect" counts are these
//! drops summed (a release build names no reason otherwise).
//!
//! A failure point notes its reason on its thread ([`set`]); the drop site
//! counts it ([`count`]) and clears it. A drop with no reason noted is
//! counted as "unclassified". A second line names the pipelines (their
//! shaders' hashes) dropped most. An indirect execution's site names its
//! signature: `indirect <kind> [<argument types>] stride S max M count
//! yes|no`, the kind `draw`, `indexed` or `mesh` for geometry, `dispatch`
//! for compute.

use std::cell::Cell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

/// Reasons per frame line shown at most.
const SHOWN: usize = 32;

/// Whether drops are counted by reason (`VICTORY_DROP_REASONS=1`).
pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_DROP_REASONS").is_some_and(|v| v == "1"))
}

/// Pipelines per frame line named at most.
const NAMED: usize = 8;

thread_local! {
    static WHY: Cell<&'static str> = const { Cell::new("") };
    /// The title pipeline of the draw or dispatch being recorded.
    static PIPELINE: Cell<usize> = const { Cell::new(0) };
    /// The signature of the indirect execution being recorded.
    static SIGNATURE: Cell<Option<Signature>> = const { Cell::new(None) };
}

/// An indirect execution's signature, as the title made it and called it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct Signature {
    /// The argument types, the first four (`u32::MAX`: none).
    types: [u32; 4],
    /// Argument descriptions in all.
    arguments: u32,
    stride: u32,
    max: u32,
    counted: bool,
}

impl std::fmt::Display for Signature {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The last argument is what the record runs (D3D12: draw 0, indexed 1,
        // dispatch 2, mesh 10; the console's single-pass dispatch 105).
        let last = self.types[(self.arguments.clamp(1, 4) - 1) as usize];
        let kind = match last {
            0 => "draw",
            1 => "indexed",
            2 | 105 => "dispatch",
            10 => "mesh",
            _ => "other",
        };
        let types: Vec<String> = self
            .types
            .iter()
            .filter(|&&t| t != u32::MAX)
            .map(u32::to_string)
            .collect();
        let more = if self.arguments > 4 { ",.." } else { "" };
        write!(
            f,
            "{kind} [{}{more}] stride {} max {} count {}",
            types.join(","),
            self.stride,
            self.max,
            if self.counted { "yes" } else { "no" }
        )
    }
}

/// Where a drop was counted: a plain site, or an indirect execution with
/// its signature.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum Site {
    Plain(&'static str),
    Indirect(Option<Signature>),
}

impl std::fmt::Display for Site {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Site::Plain(site) => f.write_str(site),
            Site::Indirect(Some(sig)) => write!(f, "indirect {sig}"),
            Site::Indirect(None) => f.write_str("indirect"),
        }
    }
}

type Key = (Site, &'static str);

/// Since the last line: (site, reason) -> drops; (site, reason, shaders)
/// -> drops; and the shaders of each pipeline seen (named once).
#[derive(Default)]
struct Counts {
    by_reason: HashMap<Key, u64>,
    by_pipeline: HashMap<(Key, usize), u64>,
    names: HashMap<usize, String>,
}

static COUNTS: Mutex<Option<Counts>> = Mutex::new(None);
/// Frames presented at the last line.
static FRAMES_AT: AtomicU64 = AtomicU64::new(0);

/// Notes why the draw or dispatch being prepared on this thread fails.
pub(crate) fn set(why: &'static str) {
    if on() {
        WHY.set(why);
    }
}

/// Notes the signature of the indirect execution recorded now on this
/// thread: its arguments (type and fields), record stride, most records,
/// and whether a count buffer is given.
pub(crate) fn signature(arguments: &[[u32; 4]], stride: u32, max: u32, counted: bool) {
    if on() {
        SIGNATURE.set(Some(signature_of(arguments, stride, max, counted)));
    }
}

fn signature_of(arguments: &[[u32; 4]], stride: u32, max: u32, counted: bool) -> Signature {
    let mut types = [u32::MAX; 4];
    for (t, a) in types.iter_mut().zip(arguments) {
        *t = a[0];
    }
    Signature {
        types,
        arguments: arguments.len() as u32,
        stride,
        max,
        counted,
    }
}

/// Notes the title pipeline of the indirect execution recorded now on this
/// thread (its drop is counted where the pipeline is not at hand).
pub(crate) fn pipeline(this: usize) {
    if on() {
        PIPELINE.set(this);
    }
}

/// Counts a drop at `site` (draw, indexed, mesh, dispatch) of title
/// pipeline `this` for the reason last noted on this thread.
pub(crate) fn count_of(site: &'static str, this: usize) {
    if on() {
        PIPELINE.set(this);
        count(site);
    }
}

/// Counts a drop at `site` for the reason and pipeline last noted on this
/// thread.
pub(crate) fn count(site: &'static str) {
    if !on() {
        return;
    }
    let why = match WHY.replace("") {
        "" => "unclassified",
        why => why,
    };
    let this = PIPELINE.replace(0);
    let site = if site == "indirect" {
        Site::Indirect(SIGNATURE.take())
    } else {
        Site::Plain(site)
    };
    let mut counts = COUNTS.lock().unwrap_or_else(|e| e.into_inner());
    let counts = counts.get_or_insert_with(Counts::default);
    *counts.by_reason.entry((site, why)).or_default() += 1;
    *counts.by_pipeline.entry(((site, why), this)).or_default() += 1;
    // Named while it is bound (alive); a pipeline made later at the same
    // address before the next line keeps this name.
    counts
        .names
        .entry(this)
        .or_insert_with(|| crate::pipeline::shaders_of(this as crate::object::This));
}

/// Clears what was noted on this thread for an indirect execution that ran.
pub(crate) fn ran() {
    if on() {
        WHY.set("");
        PIPELINE.set(0);
        SIGNATURE.set(None);
    }
}

/// Logs the drops counted since the last call, per frame presented.
pub(crate) fn line(presented: u64) {
    if !on() {
        return;
    }
    let frames = presented.saturating_sub(FRAMES_AT.swap(presented, Ordering::Relaxed));
    let Some(counts) = COUNTS.lock().unwrap_or_else(|e| e.into_inner()).take() else {
        return;
    };
    if counts.by_reason.is_empty() {
        return;
    }
    tracing::info!(
        "drop reasons per frame (VICTORY_DROP_REASONS=1, over {frames} frames): {}",
        summary(counts.by_reason, frames)
    );
    tracing::info!(
        "drop reasons per frame, by pipeline (VICTORY_DROP_REASONS=1): {}",
        by_pipeline(counts.by_pipeline, &counts.names, frames)
    );
}

/// `site: reason [shaders] count`, most first, per frame, joined by `; `.
fn by_pipeline(counts: HashMap<(Key, usize), u64>, names: &HashMap<usize, String>, frames: u64) -> String {
    let mut all: Vec<_> = counts
        .into_iter()
        .map(|((key, this), n)| {
            let name = names.get(&this).map_or("?", String::as_str);
            ((key, name), n)
        })
        .collect();
    all.sort_unstable_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let frames = frames.max(1) as f64;
    all.iter()
        .take(NAMED)
        .map(|(((site, why), name), n)| format!("{site}: {why} {name} {:.1}", *n as f64 / frames))
        .collect::<Vec<_>>()
        .join("; ")
}

/// `site: reason count`, most first, per frame, joined by `; `.
fn summary(counts: HashMap<Key, u64>, frames: u64) -> String {
    let mut all: Vec<_> = counts.into_iter().collect();
    // Ties in a fixed order, so lines compare from run to run.
    all.sort_unstable_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let frames = frames.max(1) as f64;
    all.iter()
        .take(SHOWN)
        .map(|((site, why), n)| format!("{site}: {why} {:.1}", *n as f64 / frames))
        .collect::<Vec<_>>()
        .join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_is_per_frame_and_most_first() {
        let counts = HashMap::from([
            ((Site::Plain("draw"), "IndexBuffer"), 30),
            ((Site::Indirect(None), "ray-only skinning"), 300),
            ((Site::Plain("dispatch"), "no host pipeline"), 30),
        ]);
        assert_eq!(
            summary(counts, 10),
            "indirect: ray-only skinning 30.0; dispatch: no host pipeline 3.0; draw: IndexBuffer 3.0"
        );
    }

    #[test]
    fn pipelines_are_named_most_first() {
        let names = HashMap::from([(1, "[compute aa]".to_owned()), (2, "[mesh bb, pixel cc]".to_owned())]);
        let counts = HashMap::from([
            (((Site::Plain("dispatch"), "ray-tracing structure builder off"), 1), 50),
            (((Site::Plain("mesh"), "RootSrvSlot"), 2), 20),
            (((Site::Plain("draw"), "unclassified"), 3), 10),
        ]);
        assert_eq!(
            by_pipeline(counts, &names, 10),
            "dispatch: ray-tracing structure builder off [compute aa] 5.0; mesh: RootSrvSlot [mesh bb, pixel cc] 2.0; draw: unclassified ? 1.0"
        );
    }

    #[test]
    fn summary_survives_no_frames() {
        let counts = HashMap::from([((Site::Plain("mesh"), "unclassified"), 4)]);
        assert_eq!(summary(counts, 0), "mesh: unclassified 4.0");
    }

    #[test]
    fn indirect_sites_name_their_signature() {
        let dispatch = signature_of(&[[2, 13, 0, 0]], 12, 1, false);
        let mesh = signature_of(&[[4, 0, 0, 0], [10, 0, 0, 0]], 20, 4096, true);
        let many = signature_of(&[[5, 0, 0, 0]; 6], 64, 2, false);
        assert_eq!(Site::Indirect(Some(dispatch)).to_string(), "indirect dispatch [2] stride 12 max 1 count no");
        assert_eq!(Site::Indirect(Some(mesh)).to_string(), "indirect mesh [4,10] stride 20 max 4096 count yes");
        assert_eq!(Site::Indirect(Some(many)).to_string(), "indirect other [5,5,5,5,..] stride 64 max 2 count no");
        assert_eq!(signature_of(&[], 0, 0, false).to_string(), "other [] stride 0 max 0 count no");
        let counts = HashMap::from([
            ((Site::Indirect(Some(dispatch)), "ray-only pass left out"), 20),
            ((Site::Indirect(Some(mesh)), "IndexBuffer"), 10),
        ]);
        assert_eq!(
            summary(counts, 10),
            "indirect dispatch [2] stride 12 max 1 count no: ray-only pass left out 2.0; indirect mesh [4,10] stride 20 max 4096 count yes: IndexBuffer 1.0"
        );
    }
}
