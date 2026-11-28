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
    s.programs.sort_unstable();
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
        "programs missing=2[*p1 x10 (not issued); p4 x1 (not issued)] (not issued 2, dropped 0) extra=1[p5 x3] fewer=1[p2 40->10] dropped=1[p3 x2 (unclassified)]"
    );
}

#[test]
fn a_missing_program_says_whether_it_was_dropped_and_why() {
    let name = |p: usize| format!("p{p}");
    let before = summary(9, &[(1, 10, 0), (2, 4, 0), (3, 1, 0)], &[], &[]);
    let mut dark = summary(10, &[(1, 0, 10), (3, 0, 1)], &[], &[]);
    dark.why = vec![(1, "ray-only pass left out"), (3, "MissingHostPipeline")];
    assert_eq!(
        programs_diff(&before, &dark, None, &name),
        "programs missing=3[p1 x10 (dropped x10: ray-only pass left out); p2 x4 (not issued); p3 x1 (dropped x1: MissingHostPipeline)] (not issued 1, dropped 2) extra=0[] fewer=0[] dropped=2[p1 x10 (ray-only pass left out); p3 x1 (MissingHostPipeline)]"
    );
}

#[test]
fn watched_programs_are_listed_with_their_runs_drops_and_paths() {
    let name = |p: usize| match p {
        1 => "[compute aa CS_MainBlur_446b3af2_Wrapped]".to_owned(),
        2 => "[compute bb CS_Composite_12345678_Wrapped]".to_owned(),
        p => format!("[compute {p}]"),
    };
    let parts: Vec<String> = WATCHED_NAMES.iter().map(|&n| n.to_owned()).collect();
    let mut before = summary(9, &[(1, 1, 0), (2, 1, 0), (3, 5, 0)], &[], &[]);
    before.paths = vec![(1, Path::Clamped as u8), (2, Path::Clamped as u8)];
    let mut dark = summary(10, &[(1, 0, 1), (3, 5, 0)], &[], &[]);
    dark.why = vec![(1, "MissingHostPipeline")];
    let mut after = summary(11, &[(1, 1, 0), (2, 1, 0)], &[], &[]);
    after.paths = vec![(1, Path::Raw as u8 | Path::Clamped as u8), (2, Path::Clamped as u8)];
    assert_eq!(
        watched(&before, &dark, Some(&after), &name, &parts),
        "watched=2[[compute aa CS_MainBlur_446b3af2_Wrapped] ran 0/1/1 dropped 1/0/0 (MissingHostPipeline) paths -/clamped/raw+clamped; [compute bb CS_Composite_12345678_Wrapped] ran 0/1/1 dropped 0/0/0 paths -/clamped/clamped]"
    );
    assert_eq!(
        watched(&before, &dark, None, &name, &parts[..1]),
        "watched=0[]"
    );
    assert_eq!(path_names(0), "-");
    assert_eq!(path_names(Path::Ahead as u8 | Path::Zeroed as u8), "ahead+zeroed");
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
    assert_eq!(Kind::TableKeptSeen as usize, KINDS - 1);
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
    dropped(0x7123_4568, "");
    path(0x7123_4560, Path::Clamped);
    path(0x7123_4560, Path::Raw);
    // An indirect execution dropped with the drop reasons off: its pipeline
    // and reason, not "no pipeline".
    crate::drop_why::pipeline(0x7123_4578);
    crate::drop_why::set("ray-only pass left out");
    crate::drop_why::indirect_dropped();
    crate::drop_why::count("indirect");
    count(Kind::Zeroed);
    descriptor(1);
    settled(6);
    target(0x7123_4570);
    assert!(!ambient(2, false));
    periodic::ran(periodic::Task::TexturesGone);
    frame_done(7_000_001);
    victory_core::settings::set("VICTORY_FLICKER_CATCH", "").unwrap();
    let s = summary_of(7_000_001).unwrap();
    assert!(s.programs.contains(&(0x7123_4560, 2, 0)));
    assert!(s.programs.contains(&(0x7123_4568, 0, 1)));
    assert!(s.programs.contains(&(0x7123_4578, 0, 1)));
    assert_eq!(s.why_of(0x7123_4568), "unclassified");
    assert_eq!(s.why_of(0x7123_4578), "ray-only pass left out");
    assert_eq!(s.paths_of(0x7123_4560), Path::Raw as u8 | Path::Clamped as u8);
    assert!(s.counts[Kind::Ran as usize] >= 2);
    assert!(s.counts[Kind::Dropped as usize] >= 1);
    assert!(s.counts[Kind::Zeroed as usize] >= 1);
    assert!(s.counts[Kind::StaleNulledInAir as usize] >= 1);
    assert_ne!(s.settled & 1 << 6, 0);
    assert!(s.targets.contains(&(0x7123_4570, 1)));
    assert_ne!(s.ambient & 1 << 18, 0);
    assert_ne!(s.periodic & 1 << periodic::Task::TexturesGone as u32, 0);

    // (round 15) Cut where the title presents: what is recorded after its
    // present call and before the present runs is the next frame's; the
    // present's own counts stay with it. In the same test: the frames are
    // one global state.
    assert!(victory_core::settings::check_live(
        "VICTORY_FLICKER_CATCH_TITLE_CUT",
        "1",
        title_cut,
        false,
        true
    ));
    victory_core::settings::set("VICTORY_FLICKER_CATCH", "1").unwrap();
    victory_core::settings::set("VICTORY_FLICKER_CATCH_TITLE_CUT", "1").unwrap();
    ran(0x7123_4580);
    target(0x7123_4590);
    title_presents();
    ran(0x7123_4588);
    count_n(Kind::KeptAside, 7);
    frame_done(7_000_002);
    title_presents();
    frame_done(7_000_003);
    victory_core::settings::set("VICTORY_FLICKER_CATCH_TITLE_CUT", "").unwrap();
    victory_core::settings::set("VICTORY_FLICKER_CATCH", "").unwrap();
    let first = summary_of(7_000_002).unwrap();
    assert!(first.programs.contains(&(0x7123_4580, 1, 0)));
    assert_eq!(first.of(0x7123_4588), (0, 0), "recorded after the title's present call");
    assert!(first.targets.contains(&(0x7123_4590, 1)));
    assert!(first.counts[Kind::KeptAside as usize] >= 7, "the present's own count");
    let second = summary_of(7_000_003).unwrap();
    assert!(second.programs.contains(&(0x7123_4588, 1, 0)));
    assert_eq!(second.of(0x7123_4580), (0, 0));
}

#[test]
fn the_line_names_the_periodic_tasks_of_the_three_frames() {
    use periodic::Task;
    let bit = |t: Task| 1u32 << t as u32;
    assert_eq!(
        periodic::line(0, bit(Task::Sweep) | bit(Task::TexturesGone), Some(bit(Task::Snap))),
        "periodic: dark=[sweep+textures_gone] before=[-] after=[snap]"
    );
    assert_eq!(
        periodic::line(bit(Task::PrepPruned), bit(Task::SweepSettle), None),
        "periodic: dark=[sweep_settle] before=[prep_pruned] after=[?]"
    );
    assert_eq!(periodic::names(bit(Task::SnapWriting) | bit(Task::LevelsAsked)), "levels_asked+snap_writing");
}
