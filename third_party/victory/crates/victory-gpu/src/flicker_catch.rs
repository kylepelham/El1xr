//! (texflicker, issue #96) The flicker catcher (`VICTORY_FLICKER_CATCH=1`;
//! live, a diagnostic, off unless set): catches a frame that goes dark for
//! a frame or a few and says what that frame did otherwise than the frames
//! around it.
//!
//! - Every present, a compute pass in the present's own list draws the
//!   presented texture 640x360 into a buffer that is read back
//!   ([`pass`]); a thread of its own waits for it, makes 16x9 tiles' mean
//!   luminance of it and judges the frame ([`detect`]): a quarter of the
//!   lit tiles darker by two thirds in one frame and back within 3 frames
//!   is a flicker (`black` when the whole frame is black); a black run
//!   longer than that is a black stretch (`VICTORY_HOLD_LAST_FRAME`'s case).
//! - Meanwhile every frame (from one present to the next) keeps compact
//!   counts of what our code did ([`Kind`]) and, per title pipeline, how
//!   many of its draws and dispatches ran and were dropped, and the render
//!   targets bound; the last [`RING`] frames are kept.
//! - On a flicker: one line `flicker catch: frame N vs N-1 ...` with the
//!   pipelines missing, extra and fewer in the dark frame, every count that
//!   differs (dark frame / frame before / frame after), the settles, the
//!   render targets missing and extra, and the three pictures (before,
//!   dark, after) as PNGs in the snap folder (`VICTORY_SNAP_DIR`, default
//!   `snaps`), named `flicker-f<frame>-<role>.png`.
//!
//! Frames are cut at the present: what was recorded between two presents
//! counts for the second. A title that records the next frame's lists
//! while the last is presented has some of them counted a frame early.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering::Relaxed};

pub(crate) mod detect;
#[cfg(windows)]
pub(crate) mod pass;

/// Frames whose counts are kept: the read-back and the wait for a dark
/// frame's end come several frames after its present.
const RING: usize = 24;
/// Pipelines named per list in a line.
const NAMED: usize = 12;

/// Whether frames are caught (`VICTORY_FLICKER_CATCH=1`).
pub(crate) fn on() -> bool {
    static ON: victory_core::settings::Live =
        victory_core::settings::Live::flag("VICTORY_FLICKER_CATCH");
    ON.get_bool()
}

/// What a frame counts, in the order a line names them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    /// Draws, mesh and compute dispatches run on the host.
    Ran,
    /// Draws and dispatches dropped (no host pipeline, a refused input).
    Dropped,
    /// Title submissions.
    Submits,
    /// Title lists in them.
    Lists,
    /// Title lists with no host list (run nothing).
    NoHostList,
    /// Submissions a queue held (behind a fence or a memory value).
    HeldSubmits,
    /// Waits for the flights in the air to land.
    Settles,
    /// Host descriptor slots made with a view.
    SlotsMade,
    /// Made slots made null again (stale or remade).
    SlotsNulled,
    /// Made slots made null because their view is gone.
    ViewGone,
    /// Made slots made null while their new view waits for its texture.
    NulledWaiting,
    /// Texture views held back until their upload ran (`VICTORY_SHOW_AFTER`).
    ShowHeld,
    /// Held views written into their slots.
    ShowCommitted,
    /// Held views queued again (their list reset unsent).
    ShowRequeued,
    /// Host copies of textures uploaded from title memory.
    Uploads,
    /// Host textures filled with zeros.
    Zeroed,
    /// Stand-ins for the indirect light run after the title's program.
    AmbientRan,
    /// Programs of the stand-in's roles it recorded nothing after.
    AmbientSkipped,
    /// Render target clears.
    Clears,
    /// Of those, to black.
    ZeroClears,
    /// The flicker watch's counts (`descriptor::host::flicker::Count`), in
    /// its order.
    StaleNulled,
    StaleNulledInAir,
    StaleKept,
    KeptNulled,
    KeptRemade,
    EmptyViews,
    Unmade,
    KeptRead,
    /// Draws of passes over the screen dropped while their pipeline was
    /// being made (`crate::pipeline::make::screen_wait`).
    ScreenUnmade,
}

pub(crate) const KINDS: usize = 29;
const NAMES: [&str; KINDS] = [
    "ran",
    "dropped",
    "submits",
    "lists",
    "no_host_list",
    "held_submits",
    "settles",
    "slots_made",
    "slots_nulled",
    "view_gone",
    "nulled_waiting",
    "show_held",
    "show_committed",
    "show_requeued",
    "uploads",
    "zeroed",
    "ambient_ran",
    "ambient_skipped",
    "clears",
    "zero_clears",
    "stale_nulled",
    "stale_nulled_in_air",
    "stale_kept",
    "kept_nulled",
    "kept_remade",
    "empty_views",
    "unmade_views",
    "kept_read",
    "screen_unmade",
];

static NOW: [AtomicU32; KINDS] = [const { AtomicU32::new(0) }; KINDS];
/// Why flights were settled this frame: bit `why` (`host::WHY_*`).
static SETTLED: AtomicU32 = AtomicU32::new(0);
/// The stand-in for the indirect light (`crate::ambient`) this frame: bit
/// `role` if it ran after a program of that role, bit `16 + role` if it
/// recorded nothing after one.
static AMBIENT: AtomicU32 = AtomicU32::new(0);

/// Per title pipeline: draws and dispatches run, dropped; per render
/// target: binds.
#[derive(Default)]
struct Frame {
    programs: HashMap<usize, (u32, u32)>,
    targets: HashMap<usize, u32>,
}

static FRAME: Mutex<Option<Frame>> = Mutex::new(None);

/// Counts `kind` in this frame.
pub(crate) fn count(kind: Kind) {
    if on() {
        NOW[kind as usize].fetch_add(1, Relaxed);
    }
}

/// The flicker watch's counts, in its order.
const WATCHED: [Kind; 8] = [
    Kind::StaleNulled,
    Kind::StaleNulledInAir,
    Kind::StaleKept,
    Kind::KeptNulled,
    Kind::KeptRemade,
    Kind::EmptyViews,
    Kind::Unmade,
    Kind::KeptRead,
];

/// The flicker watch counted its count number `what`.
pub(crate) fn descriptor(what: usize) {
    if let Some(&kind) = WATCHED.get(what) {
        count(kind);
    }
}

fn program(pipeline: usize, ran: bool) {
    if !on() {
        return;
    }
    NOW[if ran { Kind::Ran } else { Kind::Dropped } as usize].fetch_add(1, Relaxed);
    let mut frame = FRAME.lock().unwrap_or_else(|e| e.into_inner());
    let n = frame
        .get_or_insert_with(Frame::default)
        .programs
        .entry(pipeline)
        .or_default();
    let first = *n == (0, 0);
    if ran {
        n.0 += 1;
    } else {
        n.1 += 1;
    }
    drop(frame);
    if first {
        // Named while a list has it bound (alive): a pipeline made later at
        // the same address keeps this name.
        let mut names = NAMED_AS.lock().unwrap_or_else(|e| e.into_inner());
        names
            .get_or_insert_with(HashMap::new)
            .entry(pipeline)
            .or_insert_with(|| name_of(pipeline));
    }
}

/// A draw or dispatch of title pipeline `pipeline` was recorded on the host.
pub(crate) fn ran(pipeline: usize) {
    program(pipeline, true);
}

/// A draw or dispatch of title pipeline `pipeline` (0: not known) was dropped.
pub(crate) fn dropped(pipeline: usize) {
    program(pipeline, false);
}

/// A draw bound title resource `resource` as a render target.
pub(crate) fn target(resource: usize) {
    if on() {
        let mut frame = FRAME.lock().unwrap_or_else(|e| e.into_inner());
        *frame
            .get_or_insert_with(Frame::default)
            .targets
            .entry(resource)
            .or_default() += 1;
    }
}

/// A render target was cleared (`zero`: to black).
pub(crate) fn cleared(zero: bool) {
    count(Kind::Clears);
    if zero {
        count(Kind::ZeroClears);
    }
}

/// The title submitted `lists` lists; `no_host`: how many of them have no
/// host list.
pub(crate) fn submitted(lists: usize, no_host: impl FnOnce() -> usize) {
    if on() {
        NOW[Kind::Submits as usize].fetch_add(1, Relaxed);
        NOW[Kind::Lists as usize].fetch_add(lists as u32, Relaxed);
        NOW[Kind::NoHostList as usize].fetch_add(no_host() as u32, Relaxed);
    }
}

/// After a program of `crate::ambient::Role` number `role`, the stand-in
/// for the indirect light `ran` (or recorded nothing): `ran`.
pub(crate) fn ambient(role: usize, ran: bool) -> bool {
    if on() {
        count(if ran { Kind::AmbientRan } else { Kind::AmbientSkipped });
        AMBIENT.fetch_or(1 << (role.min(15) + if ran { 0 } else { 16 }), Relaxed);
    }
    ran
}

/// The host waited for its flights to land, for reason `why`.
pub(crate) fn settled(why: usize) {
    if on() {
        NOW[Kind::Settles as usize].fetch_add(1, Relaxed);
        SETTLED.fetch_or(1 << why.min(31), Relaxed);
    }
}

/// What one frame did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Summary {
    pub(crate) frame: u64,
    pub(crate) counts: [u32; KINDS],
    pub(crate) settled: u32,
    /// The stand-in's roles run (low half) and not (high half).
    pub(crate) ambient: u32,
    /// (pipeline, ran, dropped), by pipeline.
    pub(crate) programs: Vec<(usize, u32, u32)>,
    /// (render target, binds), by target.
    pub(crate) targets: Vec<(usize, u32)>,
}

static RINGED: Mutex<VecDeque<Summary>> = Mutex::new(VecDeque::new());
/// Shader names of the pipelines seen, by pipeline.
static NAMED_AS: Mutex<Option<HashMap<usize, String>>> = Mutex::new(None);

/// Frame `frame` is presented: what was counted since the last present is
/// its summary.
pub(crate) fn frame_done(frame: u64) {
    let counts = NOW.each_ref().map(|c| c.swap(0, Relaxed));
    let settled = SETTLED.swap(0, Relaxed);
    let ambient = AMBIENT.swap(0, Relaxed);
    let taken = FRAME
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
        .unwrap_or_default();
    let mut programs: Vec<_> = taken
        .programs
        .into_iter()
        .map(|(p, (r, d))| (p, r, d))
        .collect();
    programs.sort_unstable();
    let mut targets: Vec<_> = taken.targets.into_iter().collect();
    targets.sort_unstable();
    let mut ring = RINGED.lock().unwrap_or_else(|e| e.into_inner());
    ring.push_back(Summary {
        frame,
        counts,
        settled,
        ambient,
        programs,
        targets,
    });
    while ring.len() > RING {
        ring.pop_front();
    }
}

#[cfg(all(windows, not(test)))]
fn name_of(pipeline: usize) -> String {
    crate::pipeline::shaders_of(pipeline as crate::object::This)
}

/// (Tests count made-up pipelines.)
#[cfg(any(not(windows), test))]
fn name_of(pipeline: usize) -> String {
    format!("{pipeline:#x}")
}

/// The kept summary of frame `frame`.
pub(crate) fn summary_of(frame: u64) -> Option<Summary> {
    RINGED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .find(|s| s.frame == frame)
        .cloned()
}

/// The name of pipeline `p` for a line.
fn named(p: usize) -> String {
    NAMED_AS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .and_then(|n| n.get(&p).cloned())
        .unwrap_or_else(|| format!("{p:#x}"))
}

/// Settle reasons by bit (`host::WHY_*`).
fn settle_names(mask: u32) -> String {
    const WHY: [&str; 11] = [
        "why0",
        "why1",
        "sync",
        "card_done",
        "value",
        "stale",
        "read",
        "run",
        "most",
        "writes",
        "fence",
    ];
    let names: Vec<String> = (0..32)
        .filter(|b| mask >> b & 1 != 0)
        .map(|b| {
            WHY.get(b)
                .map_or_else(|| format!("why{b}"), |w| (*w).to_owned())
        })
        .collect();
    format!("[{}]", names.join(" "))
}

/// The stand-in's roles in a frame's mask: `ran=[..] none=[..]`.
pub(crate) fn ambient_names(mask: u32) -> String {
    const ROLES: [&str; 6] = ["resolve", "radiance", "air", "miss", "reflect", "knee"];
    let names = |half: u32| -> String {
        let v: Vec<String> = (0..16)
            .filter(|b| half >> b & 1 != 0)
            .map(|b| ROLES.get(b).map_or_else(|| format!("role{b}"), |r| (*r).to_owned()))
            .collect();
        v.join(" ")
    };
    format!("ran=[{}] none=[{}]", names(mask & 0xffff), names(mask >> 16))
}

/// The pipelines of `dark` against `before` (and `after`, the frame the
/// picture was back): missing (ran before, not in the dark frame; `*`: ran
/// again after), extra (ran only in the dark frame), fewer (under half the
/// draws of before), dropped in the dark frame.
pub(crate) fn programs_diff(
    before: &Summary,
    dark: &Summary,
    after: Option<&Summary>,
    name: &dyn Fn(usize) -> String,
) -> String {
    let ran = |s: &Summary| -> HashMap<usize, (u32, u32)> {
        s.programs.iter().map(|&(p, r, d)| (p, (r, d))).collect()
    };
    let (b, d) = (ran(before), ran(dark));
    let a = after.map(ran);
    let ran_in = |m: &HashMap<usize, (u32, u32)>, p: usize| m.get(&p).is_some_and(|n| n.0 > 0);
    let mut missing: Vec<(usize, u32)> = b
        .iter()
        .filter(|&(&p, n)| n.0 > 0 && !ran_in(&d, p))
        .map(|(&p, n)| (p, n.0))
        .collect();
    let mut extra: Vec<(usize, u32)> = d
        .iter()
        .filter(|&(&p, n)| n.0 > 0 && !ran_in(&b, p))
        .map(|(&p, n)| (p, n.0))
        .collect();
    let mut fewer: Vec<(usize, u32, u32)> = b
        .iter()
        .filter_map(|(&p, n)| {
            let now = d.get(&p)?.0;
            (now > 0 && now * 2 < n.0).then_some((p, n.0, now))
        })
        .collect();
    let mut dropped: Vec<(usize, u32)> = d
        .iter()
        .filter(|&(_, n)| n.1 > 0)
        .map(|(&p, n)| (p, n.1))
        .collect();
    let most = |a: &(usize, u32), b: &(usize, u32)| b.1.cmp(&a.1).then(a.0.cmp(&b.0));
    missing.sort_unstable_by(most);
    extra.sort_unstable_by(most);
    dropped.sort_unstable_by(most);
    fewer.sort_unstable_by(|x, y| (y.1 - y.2).cmp(&(x.1 - x.2)).then(x.0.cmp(&y.0)));
    let again = |p: usize| a.as_ref().is_some_and(|a| ran_in(a, p));
    let list = |v: &[(usize, u32)], star: bool| -> String {
        let mut parts: Vec<String> = v
            .iter()
            .take(NAMED)
            .map(|&(p, n)| {
                format!(
                    "{}{} x{n}",
                    if star && again(p) { "*" } else { "" },
                    name(p)
                )
            })
            .collect();
        if v.len() > NAMED {
            parts.push(format!("+{} more", v.len() - NAMED));
        }
        format!("{}[{}]", v.len(), parts.join("; "))
    };
    let fewer_text: Vec<String> = fewer
        .iter()
        .take(NAMED)
        .map(|&(p, was, now)| format!("{} {was}->{now}", name(p)))
        .collect();
    format!(
        "programs missing={} extra={} fewer={}[{}] dropped={}",
        list(&missing, true),
        list(&extra, false),
        fewer.len(),
        fewer_text.join("; "),
        list(&dropped, false)
    )
}

/// The counts that differ between `dark` and `before`, as
/// `name=dark/before/after`.
pub(crate) fn counts_diff(before: &Summary, dark: &Summary, after: Option<&Summary>) -> String {
    let parts: Vec<String> = (0..KINDS)
        .filter(|&k| dark.counts[k] != before.counts[k])
        .map(|k| match after {
            Some(a) => format!(
                "{}={}/{}/{}",
                NAMES[k], dark.counts[k], before.counts[k], a.counts[k]
            ),
            None => format!("{}={}/{}/-", NAMES[k], dark.counts[k], before.counts[k]),
        })
        .collect();
    if parts.is_empty() {
        "counts (dark/before/after): all the same".to_owned()
    } else {
        format!("counts (dark/before/after): {}", parts.join(" "))
    }
}

/// Render targets bound before and not in the dark frame, and the other
/// way round.
pub(crate) fn targets_diff(before: &Summary, dark: &Summary) -> String {
    let has = |s: &Summary, t: usize| s.targets.binary_search_by_key(&t, |&(r, _)| r).is_ok();
    let missing: Vec<String> = before
        .targets
        .iter()
        .filter(|&&(t, _)| !has(dark, t))
        .map(|&(t, n)| format!("{t:#x} x{n}"))
        .collect();
    let extra: Vec<String> = dark
        .targets
        .iter()
        .filter(|&&(t, _)| !has(before, t))
        .map(|&(t, n)| format!("{t:#x} x{n}"))
        .collect();
    let shown = |v: &[String]| v.iter().take(NAMED).cloned().collect::<Vec<_>>().join(" ");
    format!(
        "targets {}/{} missing={}[{}] extra={}[{}]",
        dark.targets.len(),
        before.targets.len(),
        missing.len(),
        shown(&missing),
        extra.len(),
        shown(&extra)
    )
}

/// The line for a caught event, from the kept summaries.
pub(crate) fn report(event: &detect::Event) -> String {
    let (frame, back) = match *event {
        detect::Event::Flicker { frame, frames, .. } => (frame, frame + u64::from(frames)),
        detect::Event::Black { frame, frames } => (frame, frame + u64::from(frames)),
    };
    let (before, dark, after) = (summary_of(frame - 1), summary_of(frame), summary_of(back));
    let head = event.describe();
    let (Some(before), Some(dark)) = (before, dark) else {
        return format!(
            "flicker catch: frame {frame} vs {} ({head}): no summary kept of them",
            frame - 1
        );
    };
    format!(
        "flicker catch: frame {frame} vs {} ({head}): {}; {}; settled={}/{}; ambient {} / before {}; {}",
        frame - 1,
        programs_diff(&before, &dark, after.as_ref(), &named),
        counts_diff(&before, &dark, after.as_ref()),
        settle_names(dark.settled),
        settle_names(before.settled),
        ambient_names(dark.ambient),
        ambient_names(before.ambient),
        targets_diff(&before, &dark)
    )
}

#[cfg(test)]
mod tests;
