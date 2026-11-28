//! When DLSS-G takes the camera's depth and motion vectors (no upscale hook),
//! and which camera its constants use.
//!
//! The title's command lists are translated on several threads at once, so
//! the order of binds between two presents is not the frame's pass order:
//! batch 5 (part C, the `dlssg` launch) saw `PS_TAA2_SpatialUpscale` bound
//! before passes that run ahead of it, the first Scaleform draw after it in
//! only 1 to 12 of about 100 frames per 5 s (none once the HUD was off), and
//! some presents with no velocity pass between them (it fell between the
//! neighbours). The title writes its depth and motion vectors into the same
//! two textures every frame, and a tag copies them where the tagging list
//! runs on the queue, after the velocity pass whatever thread translated it.
//!
//! `VICTORY_DLSSG_EVERY_FRAME=1` (off by default) therefore also tags at the
//! bind of `PS_TAA2_SpatialUpscale*` (once a frame, the scene's last pass;
//! no HUD needed), takes the velocity pass's textures noted up to
//! [`HOLD_FRAMES`] frames back, and gives the constants the last valid camera
//! up to [`HOLD_FRAMES`] frames back when the frame just finished had none.
//! Off: tags only at the first UI draw after the scene, with this frame's
//! velocity pass and camera (round 3).

use std::sync::{Arc, OnceLock};

use crate::camera::FrameCamera;

/// Frames a velocity pass's textures and a valid camera stay usable for.
pub const HOLD_FRAMES: u64 = 4;

/// `VICTORY_DLSSG_EVERY_FRAME=1`.
pub fn every_frame() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_DLSSG_EVERY_FRAME").is_some_and(|v| v == "1"))
}

/// Where a tag may be made.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Point {
    /// `PS_TAA2_SpatialUpscale*`: the scene at output size.
    SceneOut,
    /// The first UI draw over the scene, or the UI blit.
    Ui,
}

/// The velocity pass's depth and motion (`T`) by frame, and the frame last
/// tagged.
#[derive(Debug)]
pub struct Tagger<T> {
    every_frame: bool,
    hold: u64,
    velocity: Option<(u64, T)>,
    tagged: Option<u64>,
}

impl<T: Clone> Tagger<T> {
    /// `every_frame` off: round 3 (UI points, this frame's velocity only).
    pub const fn new(every_frame: bool) -> Self {
        Self {
            every_frame,
            hold: if every_frame { HOLD_FRAMES } else { 0 },
            velocity: None,
            tagged: None,
        }
    }

    /// A velocity pass of `frame` wrote `textures`.
    pub fn velocity(&mut self, frame: u64, textures: T) {
        self.velocity = Some((frame, textures));
    }

    /// At a tag point of `frame`: the textures to tag and how many frames
    /// back they were noted, once a frame.
    pub fn tag(&mut self, frame: u64, point: Point) -> Option<(T, u64)> {
        if (point == Point::SceneOut && !self.every_frame) || self.tagged == Some(frame) {
            return None;
        }
        let (at, textures) = self.velocity.as_ref()?;
        let age = frame.checked_sub(*at)?;
        if age > self.hold {
            return None;
        }
        self.tagged = Some(frame);
        Some((textures.clone(), age))
    }
}

/// The last valid camera, for presents whose frame had none.
#[derive(Debug, Default)]
pub struct CameraHold {
    valid: Option<Arc<FrameCamera>>,
}

impl CameraHold {
    /// The camera for the present after `latest`: itself when valid (or with
    /// `hold` 0), else the last valid one up to `hold` frames older, else
    /// `latest` (its `why` says what is missing). The flag: held.
    pub fn pick(
        &mut self,
        latest: Option<Arc<FrameCamera>>,
        hold: u64,
    ) -> (Option<Arc<FrameCamera>>, bool) {
        let Some(cam) = latest else {
            return (None, false);
        };
        if cam.valid {
            self.valid = Some(cam.clone());
            return (Some(cam), false);
        }
        match &self.valid {
            Some(v) if hold > 0 && cam.frame.saturating_sub(v.frame) <= hold => {
                (Some(v.clone()), true)
            }
            _ => (Some(cam), false),
        }
    }
}

#[cfg(test)]
#[path = "camera_tag_tests.rs"]
mod tests;
