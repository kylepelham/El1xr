use super::super::detect::{Detector, Event, TILES, largest_group};
use super::*;

#[test]
fn a_footprint_is_the_viewport_cut_by_the_scissor() {
    assert_eq!(
        footprint(None, None, 1920, 1080),
        Some([0.0, 0.0, 1.0, 1.0])
    );
    assert_eq!(
        footprint(
            Some([0.0, 0.0, 1920.0, 1080.0]),
            Some([960, 540, 1920, 1080]),
            1920,
            1080
        ),
        Some([0.5, 0.5, 1.0, 1.0])
    );
    assert_eq!(
        footprint(None, Some([100, 100, 100, 200]), 1920, 1080),
        None
    );
    // The right half of the bottom half: the tiles x 8-15, y 4-8.
    let m = mask_of_share([0.5, 0.5, 1.0, 1.0]);
    let tiles: Vec<usize> = (0..TILES)
        .filter(|&t| m[t / 64] >> (t % 64) & 1 != 0)
        .collect();
    assert_eq!(tiles.len(), 8 * 5);
    assert!(tiles.iter().all(|&t| t % 16 >= 8 && t / 16 >= 4));
    assert_eq!(mask_of_share([0.0, 0.0, 1.0, 1.0]), ALL);
}

#[test]
fn only_targets_shaped_as_the_picture_count() {
    assert!(screen_shaped(3840, 2160));
    assert!(screen_shaped(960, 540));
    assert!(screen_shaped(3840, 2176));
    assert!(!screen_shaped(2048, 2048));
    assert!(!screen_shaped(736, 1280));
    assert!(!screen_shaped(160, 90));
}

#[test]
fn the_largest_group_joins_tiles_by_their_sides_only() {
    // A 2x3 block at x 11-12, y 3-5, a lone tile and a diagonal neighbour.
    let block = [59, 60, 75, 76, 91, 92];
    let mut dark = block.to_vec();
    dark.extend([0, 17]);
    assert_eq!(largest_group(&dark), block.to_vec());
    // Tile 15 (right edge) does not join tile 16 (next row's left edge).
    assert_eq!(largest_group(&[15, 16]).len(), 1);
}

fn frame(lit: f32, dark: &[usize], v: f32) -> Vec<f32> {
    let mut t = vec![lit; TILES];
    for &i in dark {
        t[i] = v;
    }
    t
}

#[test]
fn a_dark_region_back_the_next_frame_is_a_region_event_only_when_asked() {
    let wall = [59, 60, 75, 76, 91, 92];
    let frames = || {
        vec![
            frame(36.0, &[], 0.0),
            frame(36.0, &wall, 8.0),
            frame(36.0, &[], 0.0),
            frame(36.0, &[], 0.0),
        ]
    };
    let run = |region: usize| {
        let mut d = Detector::default();
        d.region = region;
        frames()
            .into_iter()
            .enumerate()
            .flat_map(|(i, t)| d.push(500 + i as u64, t))
            .collect::<Vec<_>>()
    };
    assert!(run(0).is_empty());
    assert!(run(7).is_empty());
    assert_eq!(
        run(6),
        vec![Event::Region {
            frame: 501,
            tiles: wall.to_vec(),
            before: 36.0,
            now: 8.0
        }]
    );
    let text = run(6)[0].describe();
    assert!(
        text.contains("6 adjacent tiles") && text.contains("x 11-12, y 3-5"),
        "{text}"
    );
    // Dark for two frames: not a region (back on the next frame only).
    let mut d = Detector::default();
    d.region = 6;
    let mut events = Vec::new();
    for (i, t) in [
        frame(36.0, &[], 0.0),
        frame(36.0, &wall, 8.0),
        frame(36.0, &wall, 8.0),
        frame(36.0, &[], 0.0),
    ]
    .into_iter()
    .enumerate()
    {
        events.extend(d.push(700 + i as u64, t));
    }
    assert!(events.is_empty(), "{events:?}");
}

fn texture(
    register: u16,
    memory: u64,
    host: &'static str,
    view: (u32, u32),
    first: u64,
) -> Texture {
    Texture {
        kind: b't',
        register,
        resource: 1,
        memory,
        width: 1024,
        height: 1024,
        format: 71,
        slot: Some(40),
        host,
        view,
        first,
    }
}

fn used(draws: u32, mask: Mask, textures: Vec<Texture>) -> Use {
    Use {
        draws,
        mask,
        target: (0xa000, 3840, 2160),
        textures,
        ..Use::default()
    }
}

#[test]
fn a_region_line_names_the_pairs_into_the_region_and_flags_their_textures() {
    let wall = [59, 60, 75, 76, 91, 92];
    let corner = mask_of_share([0.0, 0.0, 0.1, 0.1]);
    let mut before = Notes::new();
    // Drew into the region before, not in the dark frame.
    before.insert(
        (0x10, 0xa),
        used(4, ALL, vec![texture(0, 0x5000, "made", (7, 1), 10)]),
    );
    // Drew in both; its t1 remade in the dark frame.
    before.insert(
        (0x20, 0xa),
        used(9, ALL, vec![texture(1, 0x6000, "made", (8, 1), 10)]),
    );
    // Drew only into a corner: not named.
    before.insert((0x30, 0xa), used(2, corner, vec![]));
    let mut dark = Notes::new();
    dark.insert(
        (0x20, 0xa),
        used(9, ALL, vec![texture(1, 0x6000, "null", (8, 2), 10)]),
    );
    // New in the dark frame with a texture first seen then.
    dark.insert(
        (0x40, 0xa),
        used(1, ALL, vec![texture(2, 0x7000, "made", (9, 1), 101)]),
    );
    let name = |p: usize| format!("P{p:x}");
    let text = line_of(
        101,
        &wall,
        [Some(before), Some(dark), None],
        &[(100, 0x40)],
        [None, None, None],
        &name,
    );
    assert!(text.starts_with("region pairs 2/2/0 (dark/before/after) missing=1 fewer=0 extra=1 textures_flagged=2 first_bound=1[P40 f100]"), "{text}");
    let p10 = text.find("P10 rt").expect("P10");
    let p20 = text.find("P20 rt").expect("P20");
    let p40 = text.find("P40 rt").expect("P40");
    assert!(
        p10 < p40 && p40 < p20,
        "missing first, then extra, then the rest: {text}"
    );
    assert!(!text.contains("P30"), "{text}");
    assert!(
        text.contains("P10 rt 0xa000 3840x2160 draws 0/4/0"),
        "{text}"
    );
    assert!(
        text.contains("host=null view 8/2 first f10 remade not_made"),
        "{text}"
    );
    assert!(text.contains("first_bind=f100"), "{text}");
    assert!(text.contains("first f101 new"), "{text}");
}

#[test]
fn merged_notes_add_draws_and_keep_distinct_textures() {
    let mut a = used(
        2,
        mask_of_tiles(&[0]),
        vec![texture(0, 0x5000, "made", (1, 1), 3)],
    );
    merge(
        &mut a,
        used(
            3,
            mask_of_tiles(&[1]),
            vec![
                texture(0, 0x5000, "made", (1, 1), 3),
                texture(1, 0x6000, "made", (2, 1), 3),
            ],
        ),
    );
    assert_eq!(a.draws, 5);
    assert_eq!(a.mask, mask_of_tiles(&[0, 1]));
    assert_eq!(a.textures.len(), 2);
}

#[test]
fn the_switch_is_live_and_capped_at_the_grid() {
    assert_eq!(parse(Some("6")), 6);
    assert_eq!(parse(Some("1000")), TILES as u64);
    assert_eq!(parse(Some("x")), 0);
    assert_eq!(parse(None), 0);
    victory_core::settings::check_live("VICTORY_FLICKER_CATCH_REGION", "6", || SET.get(), 0, 6);
}
