use super::*;

fn h(n: u64) -> blake3::Hash {
    blake3::hash(&n.to_le_bytes())
}

#[test]
fn the_switch_is_live() {
    victory_core::settings::check_live("VICTORY_TEXTURE_HASH_BEHIND", "1", on, false, true);
}

#[test]
fn off_is_the_check_as_it_was() {
    for bits in 0..32u32 {
        let [unwritten, turn, exempt, can, hashed] = std::array::from_fn(|i| bits >> i & 1 == 1);
        let mut behind = Behind::Idle;
        let got = look(
            &mut behind,
            false,
            unwritten,
            turn,
            exempt,
            can,
            hashed.then(|| h(1)),
        );
        let skipped_before = unwritten && !turn && !exempt;
        assert_eq!(
            got,
            if skipped_before {
                Look::Skip
            } else {
                Look::Read
            },
            "{bits:05b}"
        );
        assert_eq!(behind, Behind::Idle);
    }
}

#[test]
fn on_hands_only_the_turns_look_behind() {
    let mut behind = Behind::Idle;
    // Not the turn: skipped, as before.
    assert_eq!(
        look(&mut behind, true, true, false, false, true, Some(h(1))),
        Look::Skip
    );
    // A seen write, a texture in its window: read now.
    assert_eq!(
        look(&mut behind, true, false, true, false, true, Some(h(1))),
        Look::Read
    );
    assert_eq!(
        look(&mut behind, true, true, true, true, true, Some(h(1))),
        Look::Read
    );
    // Sampled or through the kernel, or never hashed: read now.
    assert_eq!(
        look(&mut behind, true, true, true, false, false, Some(h(1))),
        Look::Read
    );
    assert_eq!(
        look(&mut behind, true, true, true, false, true, None),
        Look::Read
    );
    assert_eq!(behind, Behind::Idle);
    // The turn's look goes behind, once; the next turn while it runs is it.
    assert_eq!(
        look(&mut behind, true, true, true, false, true, Some(h(1))),
        Look::Behind(h(1))
    );
    assert_eq!(behind, Behind::Pending(h(1)));
    assert_eq!(
        look(&mut behind, true, true, true, false, true, Some(h(1))),
        Look::Skip
    );
    // A seen write meanwhile is still read at once.
    assert_eq!(
        look(&mut behind, true, false, false, false, true, Some(h(1))),
        Look::Read
    );
    assert_eq!(behind, Behind::Pending(h(1)));
}

#[test]
fn a_look_lands_found_unread_same_or_dropped() {
    let mut behind = Behind::Pending(h(1));
    assert!(landed(&mut behind, Some(h(1)), h(1), Seen::Same));
    assert_eq!(behind, Behind::Idle);

    let mut behind = Behind::Pending(h(1));
    assert!(landed(&mut behind, Some(h(1)), h(1), Seen::Changed));
    assert_eq!(behind, Behind::Found);
    // The next check reads, turn or not, switch or not, and only once.
    assert_eq!(
        look(&mut behind, false, true, false, false, true, Some(h(1))),
        Look::Read
    );
    assert_eq!(behind, Behind::Idle);
    assert_eq!(
        look(&mut behind, false, true, false, false, true, Some(h(1))),
        Look::Skip
    );

    let mut behind = Behind::Pending(h(1));
    assert!(landed(&mut behind, Some(h(1)), h(1), Seen::Unread));
    assert_eq!(behind, Behind::Found);

    // Read since the look began (a seen write): the newer read stands.
    let mut behind = Behind::Pending(h(1));
    assert!(!landed(&mut behind, Some(h(2)), h(1), Seen::Changed));
    assert_eq!(behind, Behind::Idle);
    // Not this look's (switch turned off and on): left alone.
    let mut behind = Behind::Found;
    assert!(!landed(&mut behind, Some(h(1)), h(1), Seen::Same));
    assert_eq!(behind, Behind::Found);
}

/// One texture checked once a frame for 80 frames, looked at for an unseen
/// write every 8th: the frames its staging buffer is detiled anew (an
/// upload), as `refresh_timed` does with [`look`] and [`landed`]. Title
/// writes come at the start of a frame, seen by the guards or not; a look
/// behind reads the memory of its frame and lands before the next check.
fn uploads(on: bool, seen: &[u64], unseen: &[u64]) -> Vec<u64> {
    let (mut memory, mut detiled) = (0u64, 0u64);
    let (mut behind, mut unguarded) = (Behind::Idle, 0u64);
    let mut out = Vec::new();
    for frame in 1..80u64 {
        let written = seen.contains(&frame);
        if written || unseen.contains(&frame) {
            memory += 1;
        }
        let unwritten = !written;
        let mut job = None;
        match look(
            &mut behind,
            on,
            unwritten,
            frame % 8 == 0,
            frame < unguarded,
            true,
            Some(h(detiled)),
        ) {
            Look::Skip => {}
            Look::Behind(expect) => job = Some((expect, h(memory))),
            Look::Read => {
                if memory != detiled {
                    if unwritten {
                        unguarded = frame + 16;
                    }
                    detiled = memory;
                    out.push(frame);
                }
            }
        }
        if let Some((expect, now)) = job {
            let seen = if now == expect {
                Seen::Same
            } else {
                Seen::Changed
            };
            landed(&mut behind, Some(h(detiled)), expect, seen);
        }
    }
    out
}

#[test]
fn the_same_uploads_whenever_the_guards_saw_every_write() {
    for seen in [&[][..], &[3], &[8, 9], &[5, 16, 17, 40, 63]] {
        assert_eq!(
            uploads(true, seen, &[]),
            uploads(false, seen, &[]),
            "{seen:?}"
        );
        assert_eq!(uploads(false, seen, &[]), seen.to_vec());
    }
}

#[test]
fn an_unseen_write_shows_one_check_later_and_opens_the_same_window() {
    // Off: found by the look at frame 16; on: by the look behind at 16,
    // read at the check of frame 17. Both then read at every check, so a
    // second unseen write inside the window shows at once either way.
    assert_eq!(uploads(false, &[], &[13, 20]), vec![16, 20]);
    assert_eq!(uploads(true, &[], &[13, 20]), vec![17, 20]);
    // A seen write the check after the look: one read covers both.
    assert_eq!(uploads(true, &[17], &[13]), vec![17]);
    // Outside any window: the next turn's look again.
    assert_eq!(uploads(true, &[], &[13, 50]), vec![17, 57]);
    assert_eq!(uploads(false, &[], &[13, 50]), vec![16, 56]);
}

#[test]
fn hashes_in_place_against_the_detiled_hash() {
    let bytes = vec![7u8; 4096];
    assert_eq!(hash_against(&bytes, blake3::hash(&bytes)), Seen::Same);
    assert_eq!(
        hash_against(&bytes, blake3::hash(&[0u8; 4096])),
        Seen::Changed
    );
}

#[test]
fn looks_run_on_their_own_threads() {
    let (tx, rx) = std::sync::mpsc::channel();
    for _ in 0..8 {
        let tx = tx.clone();
        queue(move || {
            let name = std::thread::current().name().map(str::to_owned);
            tx.send(name).unwrap();
        });
    }
    wait_idle();
    let names: Vec<_> = rx.try_iter().collect();
    assert_eq!(names.len(), 8);
    assert!(
        names.iter().all(|n| n
            .as_deref()
            .is_some_and(|n| n.starts_with("victory-hash-behind-"))),
        "{names:?}"
    );
}

#[test]
fn line_is_per_frame_and_silent_without_looks() {
    assert_eq!(line([0; SLOTS], 10.0, true), None);
    let text = line([480, 470, 168 << 20, 32_000_000, 1, 2, 3], 10.0, true).unwrap();
    assert!(
        text.contains("HASH_BEHIND=1): 48.0 queued, 47.0 hashed, 16.8 MB, 3.20 ms"),
        "{text}"
    );
    assert!(text.contains("1 found changed, 2 not readable"), "{text}");
    assert!(text.contains("3 dropped"), "{text}");
}
