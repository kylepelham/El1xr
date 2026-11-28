//! (texflicker, #96) `VICTORY_TABLE_NULL_ONCE=1` (live; off unless set): a
//! draw's table leaves a texture's slot null while the prepare threads make
//! it ready (`crate::table_defer`) only the first time a table meets that
//! texture. Every later time it is uploaded where the draw is recorded, as
//! without `VICTORY_TABLE_DEFER`.
//!
//! The diner's background went dark for one frame every ~2.06 s with prepare
//! threads on, and never without them (b5 parts N and Q; table deferral
//! needs the threads). In 4 runs of parts P and Q (two OFF, one
//! `VICTORY_PREPARE_PRUNE_SAFE=1`, one `VICTORY_GUARD_STAMP_FRESH=1`) nearly
//! every dark frame's flicker catch line says `table_nulled=1/0/0`: one
//! table slot left null in the dark frame, none in the frames before and
//! after. A texture already read by a table lost its staging buffer (the
//! texture pass lets it go after it uploads, the sweep after 2 s idle under
//! the upload cap, the host copy let go) or its prepared entry (the prune,
//! 2 s after it was made); the next table read handed it out anew and left
//! its slot null for that frame. One texture the far layer reads (all of
//! it, the lamps too) was enough.
//!
//! With `VICTORY_FLICKER_CATCH=1` each such null is named, at most
//! [`LINES_A_FRAME`] a frame (`table null:` line): the texture, whether a
//! table met it before, its host copy and staging, what the prepare threads
//! held for it, and the event of ours that came last before (`cause=`: the
//! pass's unstage, the sweep's, a host copy let go or evicted, the prune,
//! or `first_sight`) and how long before.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{LazyLock, Mutex};

pub(crate) fn on() -> bool {
    static ON: victory_core::settings::Live =
        victory_core::settings::Live::flag("VICTORY_TABLE_NULL_ONCE");
    ON.get_bool()
}

fn diag() -> bool {
    crate::flicker_catch::on()
}

/// Why a texture lost its staging buffer (it reads as never staged after).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Why {
    /// The texture pass recorded its upload and let it go (`crate::draw`).
    Pass,
    /// The sweep let it go, idle (`crate::texture::let_go_idle`).
    StagingIdle,
    /// Its host copy was let go, idle (`crate::resource::let_go_idle`).
    HostIdle,
    /// Its host copy was evicted for room.
    Evicted,
}

impl Why {
    fn name(self) -> &'static str {
        match self {
            Why::Pass => "pass_unstage",
            Why::StagingIdle => "staging_idle",
            Why::HostIdle => "host_idle",
            Why::Evicted => "evicted",
        }
    }
}

/// What is known of a texture (milliseconds of `crate::host::now_ms`; 0:
/// never).
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
struct Seen {
    /// Times a table left its slot null.
    nulls: u64,
    last_null: u64,
    unstaged: Option<(Why, u64)>,
    pruned: u64,
    made: u64,
}

/// Past this many textures the table is forgotten (as `crate::table_defer`'s
/// `HANDED`).
const MOST: usize = 1 << 17;

static SEEN: LazyLock<Mutex<HashMap<usize, Seen>>> = LazyLock::new(Default::default);

fn with(resource: usize, f: impl FnOnce(&mut Seen)) {
    let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    if seen.len() >= MOST && !seen.contains_key(&resource) {
        seen.clear();
    }
    f(seen.entry(resource).or_default());
}

/// `resource` lost its staging buffer for `why` (noted for the diag only).
pub(crate) fn unstaged(resource: usize, why: Why) {
    if diag() {
        let now = crate::host::now_ms();
        with(resource, |s| s.unstaged = Some((why, now)));
    }
}

/// The prune let go of what was made ready for `resource` (diag only).
pub(crate) fn pruned(resource: usize) {
    if diag() {
        let now = crate::host::now_ms();
        with(resource, |s| s.pruned = now);
    }
}

/// A prepare thread made `resource` ready (diag only).
pub(crate) fn made(resource: usize) {
    if diag() {
        let now = crate::host::now_ms();
        with(resource, |s| s.made = now);
    }
}

/// The title released `resource`.
pub(crate) fn forget(resource: usize) {
    let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    if !seen.is_empty() {
        seen.remove(&resource);
    }
}

/// Whether a table that would leave `resource`'s slot null reads it now
/// instead: the switch is on and a table left it null before.
pub(crate) fn keeps(resource: usize) -> bool {
    let kept = keeps_in(
        on(),
        SEEN.lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&resource)
            .copied(),
    );
    if kept {
        KEPT.fetch_add(1, Relaxed);
        crate::flicker_catch::count(crate::flicker_catch::Kind::TableKeptSeen);
    }
    kept
}

fn keeps_in(on: bool, seen: Option<Seen>) -> bool {
    on && seen.is_some_and(|s| s.nulls > 0)
}

/// Kept since the last `table null:` line (switch on).
static KEPT: AtomicU64 = AtomicU64::new(0);

/// At most this many `table null:` lines a frame: the dark frames have one
/// null, streaming frames hundreds.
pub(crate) const LINES_A_FRAME: u64 = 2;

/// The frame lines are counted for, and the lines in it.
static LINES: Mutex<(u64, u64)> = Mutex::new((0, 0));

/// What the table looked at, for the line.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Look {
    pub(crate) slot: u32,
    pub(crate) first: u32,
    pub(crate) host_copy: bool,
    /// The first level the host copy holds an upload of (`u32::MAX` none).
    pub(crate) uploaded_from: Option<u32>,
    pub(crate) staged_first: u32,
    pub(crate) held: &'static str,
    pub(crate) handed_before: bool,
}

/// A table left `resource`'s slot null now: counted, and named while the
/// frame has had fewer than [`LINES_A_FRAME`] lines.
pub(crate) fn nulled(resource: usize, look: Look) {
    let now = crate::host::now_ms();
    let before = {
        let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
        if seen.len() >= MOST && !seen.contains_key(&resource) {
            seen.clear();
        }
        let s = seen.entry(resource).or_default();
        let before = *s;
        s.nulls += 1;
        s.last_null = now;
        before
    };
    if before.nulls > 0 {
        crate::flicker_catch::count(crate::flicker_catch::Kind::TableNulledSeen);
    }
    if !diag() {
        return;
    }
    let frame = crate::frame::current();
    {
        let mut lines = LINES.lock().unwrap_or_else(|e| e.into_inner());
        if lines.0 != frame {
            *lines = (frame, 0);
        }
        if lines.1 >= LINES_A_FRAME {
            return;
        }
        lines.1 += 1;
    }
    tracing::info!(
        "{}",
        line(
            frame,
            resource,
            &look,
            &before,
            now,
            KEPT.swap(0, Relaxed),
            on()
        )
    );
}

/// The event of ours that came last before `now` (within 10 s), and how
/// many milliseconds before: `first_sight` if none.
fn cause(s: &Seen, now: u64) -> (&'static str, Option<u64>) {
    let mut events: Vec<(&'static str, u64)> = vec![("pruned", s.pruned), ("made_ready", s.made)];
    if let Some((why, at)) = s.unstaged {
        events.push((why.name(), at));
    }
    events
        .into_iter()
        .filter(|&(name, at)| at != 0 && name != "made_ready" && now.saturating_sub(at) <= 10_000)
        .max_by_key(|&(_, at)| at)
        .map_or(("first_sight", None), |(name, at)| {
            (name, Some(now.saturating_sub(at)))
        })
}

fn ago(at: u64, now: u64) -> String {
    if at == 0 {
        "never".to_owned()
    } else {
        format!("{} ms ago", now.saturating_sub(at))
    }
}

fn line(frame: u64, resource: usize, l: &Look, s: &Seen, now: u64, kept: u64, on: bool) -> String {
    let (cause, cause_ms) = cause(s, now);
    let uploaded = match l.uploaded_from {
        None => "none".to_owned(),
        Some(u32::MAX) => "nothing".to_owned(),
        Some(f) => format!("mip {f}"),
    };
    let staged = if l.staged_first == u32::MAX {
        "no".to_owned()
    } else {
        format!("from mip {}", l.staged_first)
    };
    format!(
        "table null: frame {frame} slot {} texture {resource:#x} from mip {}: cause={cause}{} \
         seen_before={} (last null {}); host copy={} uploaded={uploaded} staged={staged} \
         held={} handed_before={}; last unstaged {}, pruned {}, made ready {}; \
         kept since last line={kept} (VICTORY_TABLE_NULL_ONCE={})",
        l.slot,
        l.first,
        cause_ms.map_or(String::new(), |ms| format!(" {ms} ms before")),
        s.nulls,
        ago(s.last_null, now),
        if l.host_copy { "made" } else { "none" },
        l.held,
        l.handed_before,
        s.unstaged.map_or("never".to_owned(), |(why, at)| format!(
            "by {} {}",
            why.name(),
            ago(at, now)
        )),
        ago(s.pruned, now),
        ago(s.made, now),
        u8::from(on)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_switch_is_live_and_off_unless_set() {
        assert!(!on());
    }

    #[test]
    fn only_a_texture_a_table_left_null_before_is_kept() {
        let once = Seen {
            nulls: 1,
            ..Seen::default()
        };
        assert!(keeps_in(true, Some(once)));
        assert!(!keeps_in(false, Some(once)), "switch off: as before");
        assert!(!keeps_in(true, None), "first sight still waits");
        assert!(
            !keeps_in(
                true,
                Some(Seen {
                    made: 5,
                    ..Seen::default()
                })
            ),
            "made ready but never left null by a table: waits"
        );
    }

    #[test]
    fn a_null_marks_the_texture_seen_and_forget_clears_it() {
        let r = 0x7e57_0001;
        let look = Look {
            slot: 3,
            first: 0,
            host_copy: true,
            uploaded_from: Some(u32::MAX),
            staged_first: u32::MAX,
            held: "nothing",
            handed_before: false,
        };
        nulled(r, look);
        assert!(keeps_in(true, SEEN.lock().unwrap().get(&r).copied()));
        forget(r);
        assert!(!keeps_in(true, SEEN.lock().unwrap().get(&r).copied()));
    }

    #[test]
    fn the_cause_is_the_latest_event_of_ours_within_ten_seconds() {
        let s = Seen {
            unstaged: Some((Why::Pass, 9_000)),
            pruned: 9_500,
            made: 9_900,
            ..Seen::default()
        };
        assert_eq!(cause(&s, 10_000), ("pruned", Some(500)));
        let s = Seen {
            unstaged: Some((Why::StagingIdle, 9_800)),
            pruned: 9_500,
            ..Seen::default()
        };
        assert_eq!(cause(&s, 10_000), ("staging_idle", Some(200)));
        assert_eq!(cause(&s, 30_000), ("first_sight", None));
        assert_eq!(cause(&Seen::default(), 10_000), ("first_sight", None));
    }

    #[test]
    fn the_line_names_the_texture_and_the_cause() {
        let s = Seen {
            nulls: 2,
            last_null: 7_940,
            unstaged: Some((Why::Pass, 9_990)),
            pruned: 8_000,
            made: 7_950,
            ..Seen::default()
        };
        let l = Look {
            slot: 77,
            first: 1,
            host_copy: true,
            uploaded_from: Some(1),
            staged_first: u32::MAX,
            held: "nothing",
            handed_before: false,
        };
        assert_eq!(
            line(1200, 0xabc0, &l, &s, 10_000, 4, false),
            "table null: frame 1200 slot 77 texture 0xabc0 from mip 1: cause=pass_unstage 10 ms before \
             seen_before=2 (last null 2060 ms ago); host copy=made uploaded=mip 1 staged=no \
             held=nothing handed_before=false; last unstaged by pass_unstage 10 ms ago, pruned 2000 ms ago, \
             made ready 2050 ms ago; kept since last line=4 (VICTORY_TABLE_NULL_ONCE=0)"
        );
    }
}
