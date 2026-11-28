use super::*;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Bind {
    /// `PS_VelocityFill`: notes the depth and motion textures.
    Velocity,
    SceneOut,
    /// The first Scaleform draw after the scene (HUD on).
    Ui,
}

/// Runs the frames' binds (in translation order) through a tagger; per
/// frame, the age of the textures tagged (`None`: no tag, DLSS-G off).
fn run(every_frame: bool, frames: &[&[Bind]]) -> Vec<Option<u64>> {
    let mut t = Tagger::new(every_frame);
    let mut out = Vec::new();
    for (frame, binds) in frames.iter().enumerate() {
        let frame = frame as u64;
        let mut tagged = None;
        for b in *binds {
            let point = match b {
                Bind::Velocity => {
                    t.velocity(frame, ("depth", "motion"));
                    continue;
                }
                Bind::SceneOut => Point::SceneOut,
                Bind::Ui => Point::Ui,
            };
            if let Some((textures, age)) = t.tag(frame, point) {
                assert_eq!(textures, ("depth", "motion"));
                assert!(tagged.is_none(), "frame {frame} tagged twice");
                tagged = Some(age);
            }
        }
        out.push(tagged);
    }
    out
}

use Bind::{SceneOut, Ui, Velocity};

/// Batch 5 part C's settled world: the scene pass bound every frame (at
/// times before the velocity pass, lists translate in parallel), the HUD in
/// a few frames only, one velocity pass falling into the previous frame.
const SETTLED: &[&[Bind]] = &[
    &[Velocity, SceneOut],
    &[SceneOut, Velocity],
    &[Velocity, SceneOut, Ui],
    &[Velocity, SceneOut, Velocity],
    &[SceneOut],
    &[Velocity, SceneOut],
    &[Velocity, SceneOut],
    &[Velocity, SceneOut],
];

#[test]
fn every_frame_tags_each_rendered_frame_once() {
    let ages = run(true, SETTLED);
    assert_eq!(
        ages,
        vec![
            Some(0),
            Some(1),
            Some(0),
            Some(0),
            Some(1),
            Some(0),
            Some(0),
            Some(0)
        ]
    );
}

#[test]
fn round_3_tags_only_frames_with_a_ui_draw() {
    // What the batch measured: the camera tagged a few frames per window.
    let ages = run(false, SETTLED);
    assert_eq!(
        ages,
        vec![None, None, Some(0), None, None, None, None, None]
    );
    // No HUD at all: nothing, ever (the batch's last 80 s).
    let no_hud: Vec<&[Bind]> = vec![&[Velocity, SceneOut]; 6];
    assert!(run(false, &no_hud).iter().all(Option::is_none));
    assert!(run(true, &no_hud).iter().all(|a| *a == Some(0)));
}

#[test]
fn old_textures_are_not_tagged_past_the_hold() {
    let mut frames: Vec<&[Bind]> = vec![&[Velocity, SceneOut]];
    frames.extend(std::iter::repeat_n(
        &[SceneOut][..],
        HOLD_FRAMES as usize + 2,
    ));
    let ages = run(true, &frames);
    let tagged = ages.iter().filter(|a| a.is_some()).count();
    assert_eq!(tagged, HOLD_FRAMES as usize + 1);
    assert_eq!(ages.last(), Some(&None));
    // Nothing noted yet: no tag.
    assert_eq!(run(true, &[&[SceneOut], &[Ui]]), vec![None, None]);
}

#[test]
fn a_ui_point_after_the_scene_tag_does_not_tag_again() {
    let mut t = Tagger::new(true);
    t.velocity(7, 1);
    assert_eq!(t.tag(7, Point::SceneOut), Some((1, 0)));
    assert_eq!(t.tag(7, Point::Ui), None);
    assert_eq!(t.tag(8, Point::Ui), Some((1, 1)));
    // A frame counter that went back (never): no tag.
    assert_eq!(
        Tagger::new(true).tag(3, Point::Ui).map(|(v, _): (u8, _)| v),
        None
    );
    let mut back = Tagger::new(true);
    back.velocity(9, 1);
    assert_eq!(back.tag(8, Point::SceneOut), None);
}

fn camera(frame: u64, valid: bool) -> Arc<FrameCamera> {
    Arc::new(FrameCamera {
        frame,
        valid,
        why: if valid {
            String::new()
        } else {
            "no velocity pass this frame".into()
        },
        ..FrameCamera::default()
    })
}

#[test]
fn a_frame_without_a_velocity_pass_takes_the_last_valid_camera() {
    let mut h = CameraHold::default();
    assert_eq!(h.pick(None, HOLD_FRAMES).0.map(|c| c.frame), None);
    let (c, held) = h.pick(Some(camera(10, true)), HOLD_FRAMES);
    assert_eq!((c.map(|c| c.frame), held), (Some(10), false));
    let (c, held) = h.pick(Some(camera(11, false)), HOLD_FRAMES);
    assert_eq!((c.map(|c| c.frame), held), (Some(10), true));
    let (c, held) = h.pick(Some(camera(10 + HOLD_FRAMES, false)), HOLD_FRAMES);
    assert_eq!((c.map(|c| c.frame), held), (Some(10), true));
    // Past the hold: the invalid one, whose `why` turns DLSS-G off.
    let (c, held) = h.pick(Some(camera(11 + HOLD_FRAMES, false)), HOLD_FRAMES);
    let c = c.unwrap();
    assert_eq!((c.frame, c.valid, held), (11 + HOLD_FRAMES, false, false));
    // Hold 0 (switch off): round 3, never an older camera.
    let mut strict = CameraHold::default();
    strict.pick(Some(camera(1, true)), 0);
    let (c, held) = strict.pick(Some(camera(2, false)), 0);
    assert_eq!((c.unwrap().frame, held), (2, false));
}
