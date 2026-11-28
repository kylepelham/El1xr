use super::detect::{self, Detector, Event, TILES};
use super::*;

fn lit(v: f32) -> Vec<f32> {
    vec![v; TILES]
}

/// `lit(before)` with its first `n` tiles at `v`.
fn dimmed(before: f32, n: usize, v: f32) -> Vec<f32> {
    let mut t = lit(before);
    t[..n].fill(v);
    t
}

fn run(frames: Vec<Vec<f32>>) -> Vec<Event> {
    let mut d = Detector::default();
    frames
        .into_iter()
        .enumerate()
        .flat_map(|(i, t)| d.push(100 + i as u64, t))
        .collect()
}

#[test]
fn one_dark_frame_between_lit_ones_is_a_flicker() {
    let events = run(vec![
        lit(100.0),
        lit(100.0),
        dimmed(100.0, 80, 5.0),
        lit(100.0),
        lit(100.0),
    ]);
    assert_eq!(
        events,
        vec![Event::Flicker {
            frame: 102,
            frames: 1,
            dark: 80,
            before: 100.0,
            now: 5.0,
            black: false
        }]
    );
}

#[test]
fn a_whole_black_frame_is_a_black_flicker_and_a_long_run_a_black_stretch() {
    let events = run(vec![lit(80.0), lit(0.0), lit(0.0), lit(80.0)]);
    assert!(matches!(
        events[..],
        [Event::Flicker {
            frame: 101,
            frames: 2,
            black: true,
            ..
        }]
    ));
    let mut frames = vec![lit(80.0)];
    frames.extend(std::iter::repeat_n(lit(0.0), 10));
    frames.push(lit(80.0));
    assert_eq!(
        run(frames),
        vec![Event::Black {
            frame: 101,
            frames: 10
        }]
    );
}

#[test]
fn cuts_fades_small_patches_and_dark_scenes_are_no_flicker() {
    // A cut to a dark scene that stays.
    let mut frames = vec![lit(100.0)];
    frames.extend(std::iter::repeat_n(lit(10.0), 8));
    assert!(run(frames).is_empty());
    // A fade: darker a little each frame, then back.
    let fade: Vec<_> = [100.0, 80.0, 60.0, 45.0, 34.0, 26.0, 40.0, 100.0]
        .iter()
        .map(|&v| lit(v))
        .collect();
    assert!(run(fade).is_empty());
    // A few tiles (under a quarter) black for a frame.
    assert!(
        run(vec![
            lit(100.0),
            dimmed(100.0, TILES / 4 - 1, 0.0),
            lit(100.0)
        ])
        .is_empty()
    );
    // A dark scene flickering darker: nothing lit enough to go dark.
    assert!(run(vec![lit(10.0), lit(1.5), lit(10.0)]).is_empty());
    // Darker for longer than a flicker lasts.
    let long = vec![
        lit(100.0),
        dimmed(100.0, 80, 5.0),
        dimmed(100.0, 80, 5.0),
        dimmed(100.0, 80, 5.0),
        dimmed(100.0, 80, 5.0),
        lit(100.0),
    ];
    assert!(run(long).is_empty());
}

#[test]
fn a_frame_not_read_breaks_the_sequence() {
    let mut d = Detector::default();
    assert!(d.push(1, lit(100.0)).is_empty());
    assert!(d.push(2, dimmed(100.0, 80, 5.0)).is_empty());
    // Frame 3 missing: no flicker spans it.
    assert!(d.push(4, lit(100.0)).is_empty());
    assert!(d.push(5, dimmed(100.0, 80, 5.0)).is_empty());
    assert_eq!(d.push(6, lit(100.0)).len(), 1);
}

#[test]
fn tiles_are_the_mean_luminance_of_their_pixels() {
    let mut pixels = vec![0x00ff_ffffu32; detect::WIDE * detect::HIGH];
    // The top left tile (40x40) black.
    for y in 0..40 {
        pixels[y * detect::WIDE..y * detect::WIDE + 40].fill(0);
    }
    let tiles = detect::tiles_of(&pixels);
    assert_eq!(tiles.len(), TILES);
    assert!(tiles[0] < 0.01);
    assert!((tiles[1] - 255.0).abs() < 0.1);
    // Pure green weighs 0.7152.
    let green = detect::tiles_of(&vec![0x0000_ff00; detect::WIDE * detect::HIGH]);
    assert!((green[5] - 0.7152 * 255.0).abs() < 0.1);
    assert_eq!(detect::rgb_of(&[0x0003_0201]), vec![1, 2, 3]);
}

fn summary(
    frame: u64,
    programs: &[(usize, u32, u32)],
    counts: &[(Kind, u32)],
    targets: &[usize],
) -> Summary {
    let mut s = Summary {
        frame,
        programs: programs.to_vec(),
        targets: targets.iter().map(|&t| (t, 1)).collect(),
        ..Default::default()
    };
    for &(k, n) in counts {
        s.counts[k as usize] = n;
    }
    s
}

#[test]
fn the_diff_names_missing_extra_fewer_and_dropped_pipelines() {
    let name = |p: usize| format!("p{p}");
    let before = summary(9, &[(1, 10, 0), (2, 40, 0), (3, 8, 0), (4, 1, 0)], &[], &[]);
    let dark = summary(10, &[(2, 10, 0), (3, 8, 2), (5, 3, 0)], &[], &[]);
    let after = summary(11, &[(1, 10, 0), (2, 40, 0), (3, 8, 0)], &[], &[]);
    assert_eq!(
        programs_diff(&before, &dark, Some(&after), &name),
        "programs missing=2[*p1 x10; p4 x1] extra=1[p5 x3] fewer=1[p2 40->10] dropped=1[p3 x2]"
    );
}

#[test]
fn the_diff_names_counts_that_differ_and_targets() {
    let before = summary(
        9,
        &[],
        &[(Kind::Ran, 100), (Kind::SlotsNulled, 2)],
        &[0x10, 0x20],
    );
    let dark = summary(
        10,
        &[],
        &[
            (Kind::Ran, 100),
            (Kind::SlotsNulled, 40),
            (Kind::StaleNulledInAir, 7),
        ],
        &[0x10, 0x30],
    );
    let after = summary(11, &[], &[(Kind::SlotsNulled, 3)], &[]);
    assert_eq!(
        counts_diff(&before, &dark, Some(&after)),
        "counts (dark/before/after): slots_nulled=40/2/3 stale_nulled_in_air=7/0/0"
    );
    assert_eq!(
        counts_diff(&before, &before, None),
        "counts (dark/before/after): all the same"
    );
    assert_eq!(
        targets_diff(&before, &dark),
        "targets 2/2 missing=1[0x20 x1] extra=1[0x30 x1]"
    );
    assert_eq!(settle_names(1 << 6 | 1 << 2), "[sync read]");
    assert_eq!(ambient_names(1 | 1 << 1 | 1 << 16), "ran=[resolve radiance] none=[resolve]");
}

#[test]
fn every_kind_has_a_name_in_order() {
    assert_eq!(NAMES.len(), KINDS);
    assert_eq!(Kind::ScreenUnmade as usize, KINDS - 1);
    assert_eq!(NAMES[Kind::StaleNulled as usize], "stale_nulled");
    assert_eq!(NAMES[Kind::Zeroed as usize], "zeroed");
}

#[test]
fn the_switch_is_live_and_frames_are_summed_at_the_present() {
    if !victory_core::settings::check_live("VICTORY_FLICKER_CATCH", "1", on, false, true) {
        return;
    }
    // Other tests may count meanwhile while the switch is on: only what
    // this one alone counts is checked exactly.
    victory_core::settings::set("VICTORY_FLICKER_CATCH", "1").unwrap();
    ran(0x7123_4560);
    ran(0x7123_4560);
    dropped(0x7123_4568);
    count(Kind::Zeroed);
    descriptor(1);
    settled(6);
    target(0x7123_4570);
    assert!(!ambient(2, false));
    frame_done(7_000_001);
    victory_core::settings::set("VICTORY_FLICKER_CATCH", "").unwrap();
    let s = summary_of(7_000_001).unwrap();
    assert!(s.programs.contains(&(0x7123_4560, 2, 0)));
    assert!(s.programs.contains(&(0x7123_4568, 0, 1)));
    assert!(s.counts[Kind::Ran as usize] >= 2);
    assert!(s.counts[Kind::Dropped as usize] >= 1);
    assert!(s.counts[Kind::Zeroed as usize] >= 1);
    assert!(s.counts[Kind::StaleNulledInAir as usize] >= 1);
    assert_ne!(s.settled & 1 << 6, 0);
    assert!(s.targets.contains(&(0x7123_4570, 1)));
    assert_ne!(s.ambient & 1 << 18, 0);
}
