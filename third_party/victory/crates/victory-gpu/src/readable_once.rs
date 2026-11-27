//! A texture a list's draws read is made ready for them (`draw.rs`
//! `readable`: its host copy looked up, its upload registered, its state
//! made shader-readable) once while nothing that could change the answer
//! happened, not again for every table of every draw that names it
//! (`VICTORY_LIST_READABLE_ONCE=1`; off unless set).
//!
//! The title's culling and lighting dispatches bind tables of their own
//! that differ in one constant buffer beside the same views: every view of
//! every such table was made ready again, four locks a texture each (its
//! host side, its alias answer, the list's uploads, the list's states).
//!
//! A texture made ready is remembered, on the recording thread, with:
//! - the list and its count of changes (`HostList::changes`: every state
//!   change, title write recorded, twin copy and reset of the list), so
//!   anything recorded in the list since makes it ready again;
//! - its generation, bumped when it is made or freed, its host copy made
//!   or let go, its staging buffer made or let go, and its staging buffer
//!   detiled anew from title memory (the title wrote it, seen at a refresh);
//! - its alias answer's stamp (the targets that entered the table near its
//!   memory, `crate::resource::alias_stamp`).
//!
//! Never remembered: views of placed records, a texture read through a
//! colour twin, an alias copy, the depth target a draw tests, one whose
//! generation moved while it was made ready, and one the title fills that
//! has no staging buffer from the levels the view shows (its upload is
//! asked for again by the next read). Buffers are never remembered (only a
//! texture's host side is).
//!
//! Source: DoubleGhosty/cool_fox 4e77a8795b (its skip of `readable` within
//! a list's epoch), with the per-texture generation added here.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering::Relaxed};

/// The switch (`VICTORY_LIST_READABLE_ONCE`; on only when `1`).
pub(crate) fn on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| switch(std::env::var_os("VICTORY_LIST_READABLE_ONCE").as_deref()))
}

fn switch(value: Option<&std::ffi::OsStr>) -> bool {
    value.is_some_and(|v| v == "1")
}

/// Generations, by a hash of the resource: two resources that share one
/// are made ready again for each other's changes, never less.
const GENERATIONS: usize = 4096;
static GENERATION: [AtomicU32; GENERATIONS] = [const { AtomicU32::new(0) }; GENERATIONS];

fn bucket(resource: usize) -> usize {
    (resource >> 4 ^ resource >> 16) % GENERATIONS
}

/// Something that changes how title resource `resource` is made ready
/// happened: what threads remembered of it is forgotten.
pub(crate) fn bump(resource: usize) {
    if on() {
        bump_always(resource);
    }
}

fn bump_always(resource: usize) {
    GENERATION[bucket(resource)].fetch_add(1, Relaxed);
}

/// What a texture was made ready with.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Marks {
    generation: u32,
    alias: u32,
}

fn marks_of(resource: usize, alias: u32) -> Marks {
    Marks {
        generation: GENERATION[bucket(resource)].load(Relaxed),
        alias,
    }
}

/// Title texture `resource`'s marks now; `None` with the switch off.
pub(crate) fn marks(resource: usize) -> Option<Marks> {
    on().then(|| {
        marks_of(
            resource,
            crate::resource::alias_stamp(resource as crate::object::This),
        )
    })
}

/// Past this many textures remembered at one count of a list's changes,
/// they are forgotten.
const MOST: usize = 1 << 14;

/// The textures a thread made ready at one count of one list's changes.
#[derive(Default)]
struct Ready {
    /// The list and its count of changes.
    at: (usize, u64),
    made: HashMap<usize, Marks>,
}

impl Ready {
    fn move_to(&mut self, at: (usize, u64)) {
        if self.at != at {
            self.at = at;
            self.made.clear();
        }
    }

    /// Whether `resource` was made ready at `at` with the marks it has now
    /// (`marks`, asked only for one remembered) and is not `tested`.
    fn hit(
        &mut self,
        at: (usize, u64),
        resource: usize,
        tested: usize,
        marks: impl FnOnce() -> Marks,
    ) -> bool {
        self.move_to(at);
        resource != tested && self.made.get(&resource).is_some_and(|&m| m == marks())
    }

    /// `resource` was made ready, the list standing at `at` after it; its
    /// marks were `before` when that began and are `now`.
    fn note(&mut self, at: (usize, u64), resource: usize, before: Marks, now: Marks) {
        self.move_to(at);
        if before != now {
            return;
        }
        if self.made.len() >= MOST {
            self.made.clear();
        }
        self.made.insert(resource, before);
    }
}

thread_local! {
    static READY: std::cell::RefCell<Ready> = std::cell::RefCell::new(Ready::default());
}

fn at(list: &crate::host::HostList) -> (usize, u64) {
    (
        std::ptr::from_ref(list) as usize,
        list.changes.load(Relaxed),
    )
}

/// Whether title texture `resource`, read by a draw of `list`, is ready
/// as it was made by this thread (`tested`: the depth target the draw
/// tests, never skipped).
pub(crate) fn ready(list: &crate::host::HostList, resource: usize, tested: usize) -> bool {
    on() && READY.with_borrow_mut(|r| {
        r.hit(at(list), resource, tested, || {
            marks(resource).unwrap_or(Marks {
                generation: u32::MAX,
                alias: u32::MAX,
            })
        })
    })
}

/// Title texture `resource` was made ready for a draw of `list`; its marks
/// were `before` when that began.
pub(crate) fn made_ready(list: &crate::host::HostList, resource: usize, before: Marks) {
    let Some(now) = marks(resource) else {
        return;
    };
    READY.with_borrow_mut(|r| r.note(at(list), resource, before, now));
}

/// A recording thread begins another list or resets its state: nothing it
/// remembered stays.
pub(crate) fn forget() {
    if on() {
        READY.with_borrow_mut(|r| {
            r.at = (0, 0);
            r.made.clear();
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIST: usize = 0x1000;
    const TEX: usize = 0x7f00_0010_0000;

    fn m(generation: u32, alias: u32) -> Marks {
        Marks { generation, alias }
    }

    fn remembered(changes: u64) -> Ready {
        let mut r = Ready::default();
        r.note((LIST, changes), TEX, m(1, 0), m(1, 0));
        r
    }

    #[test]
    fn the_switch_is_off_unless_one() {
        assert!(!switch(None));
        assert!(!switch(Some(std::ffi::OsStr::new("0"))));
        assert!(switch(Some(std::ffi::OsStr::new("1"))));
    }

    /// Nothing changed: the second read is skipped.
    #[test]
    fn a_texture_made_ready_is_skipped_while_nothing_changed() {
        let mut r = remembered(5);
        assert!(r.hit((LIST, 5), TEX, 0, || m(1, 0)));
        assert!(r.hit((LIST, 5), TEX, 0, || m(1, 0)));
        // Never remembered: made ready.
        assert!(!r.hit((LIST, 5), TEX + 0x100, 0, || m(1, 0)));
    }

    /// Anything recorded in the list since (a state change, a title write,
    /// a twin copy, a reset: `HostList::changes`) makes it ready again, and
    /// what was remembered before goes.
    #[test]
    fn a_change_in_the_list_makes_it_ready_again() {
        let mut r = remembered(5);
        assert!(!r.hit((LIST, 6), TEX, 0, || m(1, 0)));
        assert!(!r.hit((LIST, 5), TEX, 0, || m(1, 0)));
    }

    /// Another list (or a list made anew at the same address after a
    /// forget): ready again.
    #[test]
    fn another_list_makes_it_ready_again() {
        let mut r = remembered(5);
        assert!(!r.hit((LIST + 8, 5), TEX, 0, || m(1, 0)));
        let mut r = remembered(5);
        r.at = (0, 0);
        r.made.clear();
        assert!(!r.hit((LIST, 5), TEX, 0, || m(1, 0)));
    }

    /// The texture's generation moved (made or freed, its host copy made
    /// or let go, its staging buffer made, let go or detiled anew): ready
    /// again.
    #[test]
    fn a_new_generation_makes_it_ready_again() {
        let mut r = remembered(5);
        assert!(!r.hit((LIST, 5), TEX, 0, || m(2, 0)));
    }

    /// The alias answer may have changed (a target entered near its
    /// memory): ready again.
    #[test]
    fn a_new_alias_stamp_makes_it_ready_again() {
        let mut r = remembered(5);
        assert!(!r.hit((LIST, 5), TEX, 0, || m(1, 1)));
    }

    /// The depth target the draw tests is always made ready (both read
    /// states, noted for the draw).
    #[test]
    fn the_tested_depth_target_is_always_made_ready() {
        let mut r = remembered(5);
        assert!(!r.hit((LIST, 5), TEX, TEX, || m(1, 0)));
        assert!(r.hit((LIST, 5), TEX, 0, || m(1, 0)));
    }

    /// Marks that moved while it was made ready: not remembered.
    #[test]
    fn a_change_while_made_ready_is_not_remembered() {
        let mut r = Ready::default();
        r.note((LIST, 5), TEX, m(1, 0), m(2, 0));
        assert!(!r.hit((LIST, 5), TEX, 0, || m(2, 0)));
        r.note((LIST, 5), TEX, m(2, 0), m(2, 3));
        assert!(!r.hit((LIST, 5), TEX, 0, || m(2, 3)));
    }

    /// Made ready at a later count (its own state change bumped it): what
    /// was remembered at the earlier count goes, the texture stays.
    #[test]
    fn a_note_at_a_later_count_forgets_the_earlier() {
        let mut r = remembered(5);
        r.note((LIST, 6), TEX + 0x100, m(1, 0), m(1, 0));
        assert!(!r.hit((LIST, 6), TEX, 0, || m(1, 0)));
        assert!(r.hit((LIST, 6), TEX + 0x100, 0, || m(1, 0)));
    }

    /// Past the most remembered, all of them go.
    #[test]
    fn past_the_most_all_are_forgotten() {
        let mut r = Ready::default();
        for i in 0..MOST {
            r.note((LIST, 5), 0x10_0000 + i * 0x100, m(0, 0), m(0, 0));
        }
        r.note((LIST, 5), TEX, m(1, 0), m(1, 0));
        assert_eq!(r.made.len(), 1);
        assert!(r.hit((LIST, 5), TEX, 0, || m(1, 0)));
    }

    /// A bump moves the generation of the resource bumped, and of no other
    /// that does not share its bucket.
    #[test]
    fn a_bump_moves_only_its_bucket() {
        let a = 0x7e00_0000_0040;
        let b = a + 0x40;
        assert_ne!(bucket(a), bucket(b));
        let (ga, gb) = (marks_of(a, 0), marks_of(b, 0));
        bump_always(a);
        assert_ne!(marks_of(a, 0), ga);
        assert_eq!(marks_of(b, 0), gb);
    }
}
