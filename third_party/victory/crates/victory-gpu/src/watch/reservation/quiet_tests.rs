use super::*;
use windows_sys::Win32::System::Memory::{
    MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree,
};

/// Write-watched, committed memory of `pages` 64 KiB pages.
struct Memory(usize);

impl Memory {
    fn new(pages: usize) -> Self {
        // SAFETY: a fresh write-watched allocation, freed on drop.
        let at = unsafe {
            VirtualAlloc(
                std::ptr::null(),
                pages * PAGE,
                MEM_RESERVE | MEM_COMMIT | 0x20_0000,
                PAGE_READWRITE,
            )
        } as usize;
        assert_ne!(at, 0);
        Self(at)
    }

    fn write(&self, at: usize) {
        // SAFETY: a committed page of ours.
        unsafe { ((self.0 + at) as *mut u8).write_volatile(1) };
    }
}

impl Drop for Memory {
    fn drop(&mut self) {
        // SAFETY: our allocation.
        unsafe { VirtualFree(self.0 as *mut _, 0, MEM_RELEASE) };
    }
}

fn round(at: u64, need: u64, every: u64, frame_first: u64) -> Round {
    Round {
        at,
        need,
        every,
        frame_first,
    }
}

/// The asking of `[first, last)` as `run` and `outside::ask` make it: `None` when answered
/// from the journal, else the 4 KiB pages the kernel reported.
fn ask(round: &Round, first: usize, last: usize) -> Option<Vec<usize>> {
    if round.skipping(first, last) {
        return None;
    }
    let found = kernel_written(first, last).expect("write-watched");
    round.judge(first, last, &found);
    Some(found)
}

#[test]
fn a_page_is_quiet_only_when_clean_long_enough_walked_lately_and_this_frame() {
    let r = round(100, 3, 4, 90);
    assert!(r.quiet(99, 3));
    assert!(r.quiet(97, 200));
    assert!(!r.quiet(99, 2), "not clean long enough");
    assert!(!r.quiet(96, 3), "walked too long ago");
    assert!(!r.quiet(0, 9), "never walked");
    assert!(
        !round(100, 3, 4, 100).quiet(99, 3),
        "walked in an earlier frame"
    );
    assert!(!round(100, 3, 1, 0).quiet(99, 3), "EVERY=1 never skips");
    assert!(!Round::off().skipping(1, 2));
}

/// Two ranges of one allocation, one written often and one seldom, asked at every
/// submission over many frames with random writes between: every write is reported, none
/// later than `every - 1` submissions after it was made nor after the first asking of the
/// next frame; the seldom written range is skipped most of the time; and an asking with the
/// switch off afterwards finds nothing the quiet askings left behind.
#[test]
fn no_write_is_missed_and_none_is_found_later_than_the_bound() {
    const PAGES: usize = 8;
    const ROUNDS: u64 = 600;
    const PER_FRAME: u64 = 9;
    let (need, every) = (2, 4);
    let m = Memory::new(PAGES);
    let base = m.0 / PAGE;
    let ranges = [(base, base + 4), (base + 4, base + PAGES)];
    let mut seed = 0x2545_f491_4f6c_dd1du64;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    // 4 KiB page address -> the round before whose asking it was first written, unfound.
    let mut pending: std::collections::HashMap<usize, u64> = Default::default();
    let mut written = std::collections::BTreeSet::new();
    let mut found_all = std::collections::BTreeSet::new();
    let (mut skipped, mut walked) = ([0u32; 2], [0u32; 2]);
    let first_at = 1_000_000u64;
    let mut frame_first = first_at;
    for r in 0..ROUNDS {
        let at = first_at + r;
        let new_frame = r % PER_FRAME == 0;
        if new_frame {
            frame_first = at;
        }
        // Range 0 written at about half the submissions, range 1 at one in twenty.
        for (i, chance) in [(0usize, 2u64), (1, 20)] {
            if next() % chance == 0 {
                let page = (next() as usize) % (4 * PAGE / KERNEL_PAGE);
                let offset = (ranges[i].0 - base) * PAGE + page * KERNEL_PAGE;
                m.write(offset);
                written.insert(m.0 + offset);
                pending.entry(m.0 + offset).or_insert(r);
            }
        }
        let q = round(at, need, every, frame_first);
        for (i, &(first, last)) in ranges.iter().enumerate() {
            match ask(&q, first, last) {
                None => skipped[i] += 1,
                Some(found) => {
                    walked[i] += 1;
                    for page in found {
                        assert!(written.contains(&page), "reported a page never written");
                        pending.remove(&page);
                        found_all.insert(page);
                    }
                }
            }
        }
        for (&page, &since) in &pending {
            assert!(
                r < since + every - 1,
                "page {page:#x} written before round {since} still unfound at round {r}"
            );
        }
        if new_frame {
            assert!(
                pending.is_empty(),
                "the first asking of a frame left writes unfound"
            );
        }
    }
    assert_eq!(written, found_all, "a write was never reported");
    for &(first, last) in &ranges {
        assert_eq!(ask(&Round::off(), first, last), Some(Vec::new()));
    }
    assert!(
        skipped[1] > walked[1],
        "the seldom written range is mostly skipped: {skipped:?} {walked:?}"
    );
    assert!(
        walked[0] > skipped[0],
        "the often written range is mostly walked: {skipped:?} {walked:?}"
    );
}

/// A quiet range is skipped; once its memory is remapped, freed or written outside
/// ([`forget`]) it is walked at the next asking, and the write made meanwhile is reported.
#[test]
fn forgotten_pages_are_walked_at_once() {
    let m = Memory::new(2);
    let (first, last) = (m.0 / PAGE, m.0 / PAGE + 2);
    let (need, every) = (2, 8);
    let mut at = 5_000_000u64;
    let mut next = || {
        at += 1;
        round(at, need, every, 1)
    };
    for _ in 0..need {
        assert!(ask(&next(), first, last).is_some());
    }
    assert_eq!(ask(&next(), first, last), None);
    m.write(PAGE + 3 * KERNEL_PAGE);
    forget(m.0 + PAGE, KERNEL_PAGE);
    assert_eq!(
        ask(&next(), first, last),
        Some(vec![m.0 + PAGE + 3 * KERNEL_PAGE])
    );
    // Found written: walked until clean `need` times again.
    assert!(ask(&next(), first, last).is_some());
    assert!(ask(&next(), first, last).is_some());
    assert_eq!(ask(&next(), first, last), None);
}

/// Pages walked in part by one asking are not counted for the rest of the range: a range
/// is skipped only when each of its pages qualifies.
#[test]
fn a_range_is_skipped_only_when_every_page_qualifies() {
    let m = Memory::new(20);
    let first = m.0 / PAGE;
    let mut at = 7_000_000u64;
    let mut next = || {
        at += 1;
        round(at, 1, 8, 1)
    };
    // Pages 0-17 (across a 1 MiB block edge) walked clean once, 18-19 never.
    assert!(ask(&next(), first, first + 18).is_some());
    assert_eq!(ask(&next(), first + 2, first + 18), None);
    assert!(ask(&next(), first, first + 20).is_some());
    assert_eq!(ask(&next(), first, first + 20), None);
}

#[test]
fn the_switches_are_live() {
    victory_core::settings::check_live("VICTORY_ASK_QUIET_SKIP", "3", need, 0, 3);
    victory_core::settings::check_live("VICTORY_ASK_QUIET_EVERY", "2", every, 4, 2);
}
