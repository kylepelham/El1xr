use super::*;

fn ok(slot: u32) -> Tex {
    Tex {
        slot,
        text: format!("{slot}:ok"),
        fresh: "ok",
        zero: Some(false),
        ..Tex::default()
    }
}

#[test]
fn one_names_the_character_shaders_and_a_list_names_its_hashes() {
    assert_eq!(hashes("1"), vec!["a402e650d0acbc92", "b15ba4896d650511"]);
    assert_eq!(
        hashes("2DEEC721A63FE8A5, 43e24028d4a1faef;bad xyz"),
        vec!["2deec721a63fe8a5", "43e24028d4a1faef"]
    );
    assert!(hashes("0").is_empty());
}

#[test]
fn known_layouts_keep_their_fields_and_others_read_nine_words() {
    let listed = hashes("a402e650d0acbc92,2deec721a63fe8a5");
    assert_eq!(
        fields_for("a402e650d0acbc92", &listed),
        Some(&[0u32, 4, 8, 12, 16, 20, 24][..])
    );
    assert_eq!(
        fields_for("2deec721a63fe8a5", &listed).map(<[u32]>::len),
        Some(9)
    );
    assert_eq!(fields_for("b15ba4896d650511", &listed), None);
}

#[test]
fn reasons_put_what_reads_zeros_first() {
    let mut t = ok(5);
    assert_eq!(reason(&t), None);
    t.zero = Some(true);
    t.flags = crate::bake_check::STALE;
    assert_eq!(reason(&t), Some("zero_bytes"));
    t.flags |= crate::bake_check::NULL;
    assert_eq!(reason(&t), Some("null"));
    let mut a = ok(6);
    a.in_bake = true;
    assert_eq!(
        reason(&a),
        Some("bake_alias"),
        "memory of a bake result read through another texture"
    );
    a.baked = Some("d126bcae5d5d8d26@699 black".into());
    assert_eq!(reason(&a), None, "the bake result itself");
}

#[test]
fn a_slot_is_told_when_it_turns_bad_and_when_it_turns_good() {
    let mut b = Book::default();
    let mut zero = ok(7);
    zero.zero = Some(true);
    assert!(b.turn(100).is_none(), "no line before any draw");
    let first = b.draw(100, "a402e650d0acbc92", 0x40, 0x80, &[ok(3), zero.clone()]);
    assert_eq!(first.len(), 1);
    assert!(
        first[0].contains("reads badly") && first[0].contains("7:ok why=zero_bytes"),
        "{first:?}"
    );
    // Same state again: nothing new.
    assert!(
        b.draw(100, "a402e650d0acbc92", 0x40, 0x80, &[ok(3), zero.clone()])
            .is_empty()
    );
    let line = b.turn(101).expect("frame line");
    assert_eq!(
        line,
        "face watch frame frame=100 draws=2 unread=0 slots: zero_bytes=2 ok=2"
    );
    let good = b.draw(400, "a402e650d0acbc92", 0x40, 0x80, &[ok(3), ok(7)]);
    assert_eq!(good.len(), 1);
    assert!(
        good[0].contains("slot 7 good after 300 frames (was zero_bytes)"),
        "{good:?}"
    );
}

#[test]
fn bake_result_memory_is_found_by_overlap() {
    let r = [(0x1000u64, 0x100u64, 1usize, false)];
    assert_eq!(overlaps(&r, 0x10f0, 0x20), Some(false));
    assert_eq!(overlaps(&r, 0x0f00, 0x101), Some(false));
    assert_eq!(overlaps(&r, 0x1100, 0x10), None);
    assert_eq!(overlaps(&r, 0x0f00, 0x100), None);
}

/// Memory a released bake result held is told apart from a bake result still the title's
/// (batch 5 part O: `in_bake_memory` could not say which), and from a target over it now.
#[test]
fn released_bake_memory_and_a_target_over_it_are_named() {
    let r = [
        (0x1000u64, 0x100u64, 1usize, true),
        (0x1080, 0x100, 2, false),
    ];
    assert_eq!(overlaps(&r[..1], 0x1010, 0x10), Some(true));
    assert_eq!(overlaps(&r, 0x1090, 0x10), Some(false), "one still the title's");
    let mut t = ok(5);
    t.note_bake(overlaps(&r[..1], 0x1010, 0x10));
    t.over_target = Some(0x1000);
    assert_eq!(reason(&t), Some("bake_alias"));
    let text = tex_text(&t);
    assert!(text.contains(" in_released_bake_memory over_target=0x1000"), "{text}");
    let mut live = ok(5);
    live.note_bake(overlaps(&r, 0x1090, 0x10));
    assert!(tex_text(&live).ends_with(" in_bake_memory"));
    let mut none = ok(5);
    none.note_bake(None);
    assert!(!none.in_bake);
}
