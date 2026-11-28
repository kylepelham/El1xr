//! (feedcard) The CPU's side of title buffer copies done on the card
//! (`VICTORY_CARD_READ_COPY=1`; off by default).
//!
//! Ported from fxceless/victory-fork pull request 1468 (brunofgmag), onto Neptune's card copies
//! (`crate::list` `card_copy`, `VICTORY_CARD_COPIES`): a title copy whose source shaders write
//! is recorded on the card, between the two host copies, and its destination declared written,
//! so its bytes reach title memory once the list has run. Nothing did the copy on the CPU, so a
//! step of the same submission that reads the destination on the CPU at submission (a fill of
//! upload memory such as a constant buffer, a copy onward, a texture's staging buffer) read
//! title memory from before the copy: the previous frame's bytes, where the console's card reads
//! the new ones.
//!
//! With the switch the card copy also becomes a step of the list ([`Step::CardCopy`]), and before
//! a submission is prepared each one is placed ([`places`], their rule): it is left to the card
//! alone when flights in the air write its source and nothing else of the submission needs its
//! bytes in title memory first; otherwise the CPU copies it too at submission, as a plain title
//! copy (settling the flights that write its source first, as such a copy does). A copy whose
//! source no flight writes at submission costs the CPU a copy and no wait, so it is done there.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use super::{HostList, Step, line, touches};

/// `VICTORY_CARD_READ_COPY=1`.
pub(crate) fn card_read_copy() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_CARD_READ_COPY").is_some_and(|v| v == "1"))
}

/// Since the last line: card copies left to the card, kept on the CPU because no flight writes
/// their source, kept for a later or earlier step ([`Pin`]).
static COUNT: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

/// What a step of a submission does with title memory on the CPU, as far as a card copy's
/// place cares.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Access {
    /// A copy by the CPU: destination, source, bytes.
    Copy(usize, usize, usize),
    /// A [`Step::CardCopy`]: destination, source, bytes.
    Card(usize, usize, usize),
    /// A fill of upload memory from title memory (address, bytes).
    Fill(usize, usize),
    /// A texture's staging buffer read from title memory (address, bytes).
    Texture(usize, usize),
    /// Work that may read and write title memory anywhere.
    Anywhere,
    Other,
}

impl Access {
    fn of(step: &Step) -> Access {
        match step {
            &Step::Copy(dst, src, len) => Access::Copy(dst, src, len),
            &Step::CardCopy(dst, src, len, _) => Access::Card(dst, src, len),
            &Step::Fill(_, src, len, _) => Access::Fill(src, len),
            Step::Texture(upload) => {
                let (at, len) = upload.staging.memory();
                Access::Texture(at, len)
            }
            Step::Run(_) => Access::Anywhere,
            Step::Refresh(..) | Step::GpuWrite(..) | Step::Host(..) => Access::Other,
        }
    }
}

/// Why a card copy is done on the CPU too.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Pin {
    /// No flight in the air writes the source: the copy waits for nothing.
    Settled,
    /// Work of ours that may go anywhere, before or after.
    Anywhere,
    /// An earlier step writes the source on the CPU.
    SourceWritten,
    /// A later copy reads or writes the destination.
    LaterCopy,
    /// A later fill of upload memory reads the destination.
    LaterFill,
    /// A later texture reads the destination.
    LaterTexture,
    /// A later card copy kept on the CPU reads or writes the destination.
    LaterCard,
}

/// Where a step of a submission is done.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Place {
    /// On the CPU, as the step it is: not a card copy.
    Cpu,
    /// A card copy left to the card.
    Card,
    /// A card copy done on the CPU too, for this reason, because of the step at this index.
    Kept(Pin, usize),
}

impl Place {
    fn on_cpu(self) -> bool {
        !matches!(self, Place::Card)
    }
}

/// A range of title memory a step reads or writes on the CPU, and what that makes of a card
/// copy it meets.
struct Reach {
    from: usize,
    to: usize,
    by: usize,
    pin: Pin,
}

impl Reach {
    fn new(at: usize, len: usize, by: usize, pin: Pin) -> Reach {
        Reach {
            from: at,
            to: at + len,
            by,
            pin,
        }
    }
}

/// The first step, in order, whose reach meets `[from, to)`, with its pin.
fn nearest(reaches: &[Reach], from: usize, to: usize) -> Option<(Pin, usize)> {
    reaches
        .iter()
        .filter(|r| r.from < to && from < r.to)
        .min_by_key(|r| r.by)
        .map(|r| (r.pin, r.by))
}

/// For each step of a submission, in order: where it is done. A card copy is left to the card
/// when flights in `written` (in order, joined) write its source and nothing else of the
/// submission depends on title memory holding the copy or its source before the card has run:
/// no later step reads or writes the destination on the CPU, and no earlier step writes the
/// source on the CPU or goes anywhere.
pub(super) fn places(steps: &[Access], written: &[(usize, usize)]) -> Vec<Place> {
    let early = early_hazards(steps);
    let mut places = vec![Place::Cpu; steps.len()];
    let mut later: Vec<Reach> = Vec::new();
    let mut later_anywhere: Option<usize> = None;
    for (i, step) in steps.iter().enumerate().rev() {
        match *step {
            Access::Copy(dst, src, len) => {
                later.push(Reach::new(dst, len, i, Pin::LaterCopy));
                later.push(Reach::new(src, len, i, Pin::LaterCopy));
            }
            Access::Card(dst, src, len) => {
                let pinned = later_anywhere
                    .map(|by| (Pin::Anywhere, by))
                    .or(early[i])
                    .or_else(|| nearest(&later, dst, dst + len));
                places[i] = match pinned {
                    _ if !touches(written, src, src + len) => Place::Kept(Pin::Settled, i),
                    Some((pin, by)) => Place::Kept(pin, by),
                    None => Place::Card,
                };
                if places[i].on_cpu() {
                    later.push(Reach::new(dst, len, i, Pin::LaterCard));
                    later.push(Reach::new(src, len, i, Pin::LaterCard));
                }
            }
            Access::Fill(at, len) => later.push(Reach::new(at, len, i, Pin::LaterFill)),
            Access::Texture(at, len) => later.push(Reach::new(at, len, i, Pin::LaterTexture)),
            Access::Anywhere => later_anywhere = Some(i),
            Access::Other => {}
        }
    }
    places
}

/// For each step: what, before a card copy, writes its source on the CPU or goes anywhere, and
/// the index of that step.
fn early_hazards(steps: &[Access]) -> Vec<Option<(Pin, usize)>> {
    let mut early = vec![None; steps.len()];
    let mut wrote: Vec<Reach> = Vec::new();
    let mut anywhere: Option<usize> = None;
    for (i, step) in steps.iter().enumerate() {
        match *step {
            Access::Copy(dst, _, len) => wrote.push(Reach::new(dst, len, i, Pin::SourceWritten)),
            Access::Card(_, src, len) => {
                early[i] = anywhere
                    .map(|by| (Pin::Anywhere, by))
                    .or_else(|| nearest(&wrote, src, src + len));
            }
            Access::Anywhere => {
                anywhere.get_or_insert(i);
            }
            Access::Fill(..) | Access::Texture(..) | Access::Other => {}
        }
    }
    early
}

/// Before a submission sent ahead is prepared: sets each of its card copies' flag from what
/// flights in the air write now.
pub(super) fn decide(runs: &[&HostList]) {
    if !card_read_copy() {
        return;
    }
    let any = runs.iter().any(|l| {
        l.deferred()
            .copies
            .iter()
            .any(|s| matches!(s, Step::CardCopy(..)))
    });
    if !any {
        return;
    }
    let written = line().written.clone();
    let steps: Vec<Access> = runs
        .iter()
        .flat_map(|l| {
            l.deferred()
                .copies
                .iter()
                .map(Access::of)
                .collect::<Vec<_>>()
        })
        .collect();
    let places = places(&steps, &written);
    let mut at = 0;
    for list in runs {
        for step in &list.deferred().copies {
            if let Step::CardCopy(.., cpu) = step {
                let place = places[at];
                cpu.store(place.on_cpu(), Relaxed);
                let which = match place {
                    Place::Card => 0,
                    Place::Kept(Pin::Settled, _) => 1,
                    _ => 2,
                };
                COUNT[which].fetch_add(1, Relaxed);
            }
            at += 1;
        }
    }
}

impl HostList {
    /// A title buffer copy of `len` bytes from `src` to `dst` was recorded on the card
    /// (`crate::list` `card_copy`): with `VICTORY_CARD_READ_COPY=1` it is also a step of the
    /// list, done by the CPU at submission unless [`decide`] leaves it to the card.
    pub(crate) fn card_copied(&self, dst: usize, src: usize, len: usize) {
        if !card_read_copy() {
            return;
        }
        // As `defer_copy`: title memory may change at this place in the list.
        self.cbv_epoch.fetch_add(1, Relaxed);
        let mut deferred = self.deferred();
        deferred.title_write();
        deferred.copies.push(Step::CardCopy(
            dst,
            src,
            len,
            std::sync::atomic::AtomicBool::new(true),
        ));
    }
}

/// With the frame line.
pub(super) fn frame_line(frames: f64) {
    if !card_read_copy() || frames <= 0.0 {
        return;
    }
    let [card, settled, pinned] = [0, 1, 2].map(|i| COUNT[i].swap(0, Relaxed) as f64 / frames);
    tracing::info!(
        "title buffer copies done on the card, per frame (VICTORY_CARD_READ_COPY=1): {card:.1} left to the card, \
         {settled:.1} done by the CPU too (no flight writes the source), {pinned:.1} done by the CPU too because \
         another step of the submission reads the bytes on the CPU"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: &[(usize, usize)] = &[(0x1000, 0x2000)];

    #[test]
    fn a_copy_from_what_flights_write_with_no_reader_goes_to_the_card() {
        let steps = [
            Access::Other,
            Access::Card(0x8000, 0x1000, 0x100),
            Access::Fill(0x9000, 0x10),
        ];
        assert_eq!(places(&steps, W)[1], Place::Card);
    }

    #[test]
    fn a_source_no_flight_writes_is_copied_by_the_cpu_too() {
        let steps = [Access::Card(0x8000, 0x4000, 0x100)];
        assert_eq!(places(&steps, W)[0], Place::Kept(Pin::Settled, 0));
    }

    #[test]
    fn a_later_fill_of_the_destination_keeps_it_on_the_cpu() {
        let steps = [
            Access::Card(0x8000, 0x1000, 0x100),
            Access::Other,
            Access::Fill(0x8040, 0x10),
        ];
        assert_eq!(places(&steps, W)[0], Place::Kept(Pin::LaterFill, 2));
    }

    #[test]
    fn later_copies_textures_and_work_anywhere_keep_it_on_the_cpu() {
        let card = Access::Card(0x8000, 0x1000, 0x100);
        assert_eq!(
            places(&[card, Access::Copy(0xa000, 0x8000, 4)], W)[0],
            Place::Kept(Pin::LaterCopy, 1)
        );
        assert_eq!(
            places(&[card, Access::Texture(0x7f00, 0x200)], W)[0],
            Place::Kept(Pin::LaterTexture, 1)
        );
        assert_eq!(
            places(&[card, Access::Anywhere], W)[0],
            Place::Kept(Pin::Anywhere, 1)
        );
        assert_eq!(
            places(&[Access::Anywhere, card], W)[1],
            Place::Kept(Pin::Anywhere, 0)
        );
    }

    #[test]
    fn an_earlier_write_of_the_source_keeps_it_on_the_cpu() {
        let steps = [
            Access::Copy(0x1080, 0x6000, 0x10),
            Access::Card(0x8000, 0x1000, 0x100),
        ];
        assert_eq!(places(&steps, W)[1], Place::Kept(Pin::SourceWritten, 0));
    }

    #[test]
    fn a_card_copy_kept_on_the_cpu_pins_an_earlier_one_into_its_source() {
        // The second copy reads the first's destination and its own source no flight writes:
        // it stays on the CPU, so the first's bytes must be in title memory before it.
        let steps = [
            Access::Card(0x8000, 0x1000, 0x100),
            Access::Card(0x9000, 0x8000, 0x100),
        ];
        let p = places(&steps, W);
        assert_eq!(p[1], Place::Kept(Pin::Settled, 1));
        assert_eq!(p[0], Place::Kept(Pin::LaterCard, 1));
    }

    #[test]
    fn readers_elsewhere_do_not_pin() {
        let steps = [
            Access::Fill(0x8000, 0x10),
            Access::Card(0x8000, 0x1000, 0x100),
            Access::Fill(0xc000, 0x10),
            Access::Copy(0xd000, 0xe000, 0x10),
        ];
        assert_eq!(places(&steps, W)[1], Place::Card);
    }
}
