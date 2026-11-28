use super::*;

#[test]
fn the_wide_hash_is_the_plain_one() {
    let bytes: Vec<u8> = (0..65_568u32)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
        .collect();
    for n in [
        0usize,
        16,
        32,
        1008,
        1024,
        1040,
        5000 & !15,
        65_536,
        65_552,
        65_568,
    ] {
        assert_eq!(hash(&bytes[..n]), hash_plain(&bytes[..n]), "{n} bytes");
    }
    let mut other = bytes.clone();
    other[40_000] ^= 1;
    assert_ne!(hash(&bytes), hash(&other));
}

/// Off, the compare is compiled from the same source as before.
#[test]
fn without_the_check_the_compare_is_its_old_source() {
    let base = crate::host::CARD_COMPARE;
    assert_eq!(compare_source_for(base, false), base);
    let with = compare_source_for(base, true);
    assert!(
        with.starts_with("#define HASH_AT 1048576u\n"),
        "{}",
        &with[..40]
    );
    assert!(with.ends_with(base));
}

/// What is hashed of a piece starts where the compare keeps it, ends on a
/// 16-byte line, and fits a slot.
#[test]
fn a_piece_is_hashed_from_where_it_is_kept() {
    for skew in [0u32, 8, 0x1f08, 0xfff0, 0xffff] {
        for size in [1usize << 16, 5 << 16, 0x20_0000] {
            for piece in 0..(size >> 16) as u32 + 1 {
                let (lo, hi) = hashed(piece, skew, size);
                assert_eq!(
                    lo,
                    crate::host::snap_piece_from(piece, skew),
                    "skew {skew:#x} piece {piece}"
                );
                assert_eq!(hi % 16, 0);
                assert!(hi <= size.next_multiple_of(16));
                assert!(
                    hi - lo <= 65_568,
                    "skew {skew:#x} piece {piece}: {lo:#x}..{hi:#x}"
                );
            }
        }
    }
}

/// A model of one piece, after upstream's test. The card runs flights in
/// order; each writes the piece several times, and its compare flags it if
/// it ends otherwise than the flight before. Each landing fetches what is
/// wanted, and the fetch sees the piece as it is while flight `seen.0` is
/// after its step `seen.1` (a torn fetch when that is a later flight).
/// Returns what the CPU's side holds in the end, and the fetches.
fn landed(flights: &[&[u32]], seen: &[(usize, usize)], check: bool) -> (u32, u32) {
    let piece: Piece = (1, 0);
    let mut suspects = HashSet::new();
    let (mut held, mut ended, mut fetches) = (0, 0, 0);
    for (f, steps) in flights.iter().enumerate() {
        let before = ended;
        ended = *steps.last().expect("a step");
        let flagged = ended != before;
        let wanted = flagged || (check && suspects.contains(&piece));
        if !wanted {
            continue;
        }
        fetches += 1;
        let (at, step) = seen[f];
        assert!(at >= f);
        held = flights[at][step];
        checked(&mut suspects, piece, true, held == ended);
    }
    (held, fetches)
}

/// A counter each flight clears and counts up to the same: the first
/// flight's fetch sees it cleared by the second, whose compare flags
/// nothing. Without the check the CPU's side keeps the 0.
#[test]
fn a_piece_caught_in_between_is_fetched_again() {
    let count: &[u32] = &[0, 3, 5];
    assert_eq!(landed(&[count, count], &[(1, 0), (1, 2)], false), (0, 1));
    assert_eq!(landed(&[count, count], &[(1, 0), (1, 2)], true), (5, 2));
    // Torn twice; the fourth finds it whole.
    let four = [count; 4];
    assert_eq!(
        landed(&four, &[(1, 0), (2, 1), (3, 0), (3, 2)], true),
        (5, 4)
    );
    // Fetched whole every time: no more fetches than without the check.
    assert_eq!(
        landed(&four, &[(0, 2), (1, 2), (2, 2), (3, 2)], true),
        (5, 1)
    );
    assert_eq!(
        landed(&four, &[(0, 2), (1, 2), (2, 2), (3, 2)], false),
        (5, 1)
    );
}

/// A copy in the card's memory for [`widen`]: title memory from `start`,
/// a card buffer of `size` bytes.
struct Copy {
    start: usize,
    size: usize,
    id: u64,
}

impl Whole for Copy {
    fn hashed_at(&self, at: usize) -> Option<(u32, usize, usize)> {
        let piece = ((self.start + at) / 65536 - self.start / 65536) as u32;
        let (lo, hi) = hashed(piece, (self.start % 65536) as u32, self.size);
        Some((piece, lo, hi))
    }
    fn number(&self) -> u64 {
        self.id
    }
}

/// Merged parts of one piece come from one whole fetch of it; each merge
/// reads its bytes at its offset in that fetch; a kept piece is fetched
/// from the start of its slot; checks are made where a hash is known.
#[test]
fn pieces_are_fetched_whole_and_merged_from_their_place() {
    let copy = Arc::new(Copy {
        start: 0x10_1f08,
        size: 4 << 16,
        id: 7,
    });
    // As the copy cuts them: piece 1 is [0xe0f8, 0x1e0f8), piece 2 the next.
    let (p1, p2) = (hashed(1, 0x1f08, 4 << 16), hashed(2, 0x1f08, 4 << 16));
    let mut live = vec![
        (copy.clone(), p1.0 + 0x100, p1.0 + 0x900, 0),
        (copy.clone(), p1.0 + 0x4000, p1.0 + 0x5000, 0x800),
        (copy.clone(), p2.0 + 0x10, p2.0 + 0x20, 0x1800),
    ];
    let p0 = hashed(0, 0x1f08, 4 << 16);
    let slot = 65_536 + 3 * 65_568u64;
    let mut kept = vec![(copy.clone(), p0.0 + 0x40, p0.0 + 0x80, 0, slot + 0x40, 9)];
    let wide = widen(&mut live, &mut kept, |_, piece, kept| {
        (piece == 1 || kept).then_some(0xabc)
    });
    assert_eq!(wide.live.len(), 2);
    assert_eq!(
        (wide.live[0].1, wide.live[0].2, wide.live[0].3),
        (p1.0, p1.1, 0)
    );
    let second = (p1.1 - p1.0) as u64;
    assert_eq!(
        (wide.live[1].1, wide.live[1].2, wide.live[1].3),
        (p2.0, p2.1, second)
    );
    assert_eq!(wide.live_total, second + (p2.1 - p2.0) as u64);
    assert_eq!(
        [live[0].3, live[1].3, live[2].3],
        [0x100, 0x4000, second + 0x10]
    );
    assert_eq!(wide.kept.len(), 1);
    assert_eq!(
        (
            wide.kept[0].1,
            wide.kept[0].2,
            wide.kept[0].3,
            wide.kept[0].4,
            wide.kept[0].5
        ),
        (p0.0, p0.1, 0, slot, 9)
    );
    assert_eq!(kept[0].3, 0x40);
    let checks: Vec<(Piece, bool, u64, usize)> = wide
        .checks
        .iter()
        .map(|c| (c.piece, c.kept, c.at, c.len))
        .collect();
    assert_eq!(
        checks,
        [
            ((7, 1), false, 0, p1.1 - p1.0),
            ((7, 0), true, 0, p0.1 - p0.0)
        ]
    );
}
