//! `VICTORY_CAMERA_CAPTURE=1` (off by default): at the bind of the title's
//! velocity passes (`PS_VelocityFill`, `*_2GBufFixupAndClearTemp_Velocity`)
//! and of its UI composite (`PS_BlitUISDRToBackbuffer`), notes where the
//! camera constants, depth, motion vectors and the UI texture are, for
//! [`crate::camera`]. Reads descriptors only; nothing recorded changes,
//! except for the copies `VICTORY_CAMERA_HUDLESS=1` (the HUD-less picture
//! before the UI blit, [`crate::camera_copy`]) and `VICTORY_CAMERA_MV_CHECK=1`
//! (64 pixels of depth and motion after the velocity pass,
//! [`crate::camera_mv`]) record.
//!
//! Programs are named by their `_Wrapped` identifier or known hash
//! ([`crate::camera_diag::program_name`]). On Series X 1.0.2.1 no
//! `PS_BlitUISDRToBackbuffer` runs: the scene reaches output size in
//! `PS_TAA2_SpatialUpscaleWithDithering` and the Scaleform UI draws onto it,
//! so the HUD-less picture is also taken at the frame's first Scaleform draw
//! onto that target (or one of its size). `VICTORY_CAMERA_DIAG=1` logs a
//! frame's candidate passes now and then ([`crate::camera_diag`]).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use victory_shader::{ResourceType, RootType, UsedResource};

use super::{HostList, Root, State};
use crate::camera_diag::{Entry, Shape};
use crate::descriptor::View;
use crate::pipeline::host::{HostPipeline, Stage};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Role {
    None,
    /// `PS_VelocityFill`: writes motion into its render target over the
    /// depth it tests.
    VelocityFill,
    /// `CS_2GBufFixupAndClearTemp_Velocity`: reads depth at table dword 8
    /// (t211), writes motion at table dword 56 (u19).
    VelocityCs,
    /// The other velocity shaders (the record only).
    Velocity,
    Ui,
    /// `PS_TAA2_SpatialUpscale*`: writes the scene at output size.
    SceneOut,
    /// A Scaleform UI draw (HUD, menus).
    UiDraw,
}

/// What a shader of this program name is to the camera.
pub(super) fn role_of(name: &str) -> Role {
    if name.starts_with("PS_VelocityFill") {
        Role::VelocityFill
    } else if name.contains("CS_2GBufFixupAndClearTemp_Velocity") {
        Role::VelocityCs
    } else if name.contains("2GBufFixupAndClearTemp_Velocity") {
        Role::Velocity
    } else if name.starts_with("PS_BlitUISDRToBackbuffer") {
        Role::Ui
    } else if name.starts_with("PS_TAA2_SpatialUpscale") {
        Role::SceneOut
    } else if crate::camera_diag::is_scaleform(name) {
        Role::UiDraw
    } else {
        Role::None
    }
}

/// What the camera knows of a shader.
struct Info {
    role: Role,
    name: String,
    tag: Option<&'static str>,
}

/// By shader object (the shader table keeps every one for the run).
fn info(stage: &Stage) -> Arc<Info> {
    static INFOS: Mutex<Option<HashMap<usize, Arc<Info>>>> = Mutex::new(None);
    let key = Arc::as_ptr(&stage.shader) as usize;
    let mut infos = INFOS.lock().unwrap_or_else(|e| e.into_inner());
    infos
        .get_or_insert_with(HashMap::new)
        .entry(key)
        .or_insert_with(|| {
            let name = stage.shader.camera_name();
            Arc::new(Info { role: role_of(&name), tag: crate::camera_diag::tag(&name), name })
        })
        .clone()
}

/// The frame (`camera::frame`) and title render target the scene reached
/// output size in.
static SCENE_OUT: Mutex<(u64, u64)> = Mutex::new((u64::MAX, 0));
/// The last frame the UI was found drawing over the scene.
static UI_FRAME: AtomicU64 = AtomicU64::new(u64::MAX);

/// The table SRV the velocity shaders read the per-view record from.
fn is_view_record(u: &UsedResource) -> bool {
    u.kind == ResourceType::Srv
        && u.root == RootType::Table
        && u.stride as usize == crate::camera::VIEW_STRIDE
}

/// The pass constants, `b0`.
fn is_pass_constants(u: &UsedResource) -> bool {
    u.kind == ResourceType::Cbv && u.root == RootType::Table && u.lower_bound == 0
}

/// The depth the fixup CS reads (t211, table dword 8).
fn is_fixup_depth(u: &UsedResource) -> bool {
    u.kind == ResourceType::Srv && u.root == RootType::Table && u.table_offset_dwords == 8
}

/// The motion vectors the fixup CS writes (u19, table dword 56).
fn is_fixup_motion(u: &UsedResource) -> bool {
    u.kind == ResourceType::Uav && u.root == RootType::Table && u.table_offset_dwords == 56
}

/// The UI texture of the composite: its SRV at table dword 8.
fn is_ui_texture(u: &UsedResource) -> bool {
    const UI_DWORDS: u32 = 8;
    u.kind == ResourceType::Srv && u.root == RootType::Table && u.table_offset_dwords == UI_DWORDS
}

impl State {
    pub(super) fn camera_tap(&self, list: &HostList, p: &HostPipeline, compute: bool) {
        crate::camera_mv::at_bind(me_of(self), list);
        let diag = crate::camera_diag::recording();
        let bind = if diag { crate::camera_diag::next_bind() } else { 0 };
        for stage in &p.stages {
            let info = info(stage);
            if diag {
                self.camera_diag(list, stage, compute, &info, bind);
            }
            match info.role {
                Role::None => {}
                Role::Ui => self.camera_ui(list, stage, compute),
                Role::SceneOut => self.camera_scene_out(compute),
                Role::UiDraw => self.camera_ui_draw(list, compute),
                r => {
                    self.camera_velocity(stage, compute);
                    self.camera_targets(list, stage, compute, r);
                }
            }
        }
    }

    /// Depth and motion of a velocity pass.
    fn camera_targets(&self, list: &HostList, stage: &Stage, compute: bool, r: Role) {
        let resource = |want: fn(&UsedResource) -> bool| {
            stage
                .translated()
                .used
                .iter()
                .find(|u| want(u))
                .and_then(|u| self.camera_view(compute, u))
                .and_then(|v| match v {
                    View::Shader(r, _) | View::Unordered(r, _, _) => Some(r as u64),
                    _ => None,
                })
        };
        let (depth, motion) = match r {
            Role::VelocityCs => (resource(is_fixup_depth), resource(is_fixup_motion)),
            Role::VelocityFill if !compute => (
                (self.depth != 0).then_some(self.depth as u64),
                self.targets.first().map(|&t| t as u64),
            ),
            _ => return,
        };
        let (d, m) = (
            depth.and_then(crate::camera_copy::surface_of),
            motion.and_then(crate::camera_copy::surface_of),
        );
        crate::camera::note_velocity_targets(d.as_ref().map(|s| s.0), m.as_ref().map(|s| s.0));
        crate::camera_copy::note_held(d.map(|s| s.1), m.map(|s| s.1), None);
        crate::camera_mv::after_velocity(me_of(self), std::ptr::from_ref(list) as usize, depth, motion);
    }

    fn camera_view(&self, compute: bool, u: &UsedResource) -> Option<View> {
        let Some(Root::Table(table)) = self.roots_ref(compute).get(usize::from(u.slot)) else {
            return None;
        };
        crate::descriptor::view(table + u64::from(u.table_offset_dwords) * 4)
    }

    fn camera_velocity(&self, stage: &Stage, compute: bool) {
        let t = stage.translated();
        let record = t
            .used
            .iter()
            .find(|u| is_view_record(u))
            .and_then(|u| self.camera_view(compute, u))
            .and_then(|v| crate::descriptor::host::buffer_range(&v));
        let constants = t
            .used
            .iter()
            .find(|u| is_pass_constants(u))
            .and_then(|u| self.camera_view(compute, u))
            .and_then(|v| match v {
                View::Constant(address, size) => Some((address, u64::from(size))),
                _ => None,
            });
        if let (Some(record), Some(constants)) = (record, constants) {
            crate::camera::note_view(record, constants);
        }
    }

    fn camera_ui(&self, list: &HostList, stage: &Stage, compute: bool) {
        let t = stage.translated();
        let texture = t
            .used
            .iter()
            .find(|u| is_ui_texture(u))
            .and_then(|u| self.camera_view(compute, u))
            .and_then(|v| match v {
                View::Shader(r, _) => Some(r as u64),
                _ => None,
            });
        let target = self
            .bound_targets
            .as_ref()
            .and_then(|b| b.rtvs[0].as_ref())
            .map_or(0, |r| r.1 as u64);
        if let Some(texture) = texture {
            crate::camera::note_ui(crate::camera::Ui { texture, target });
            if let Some((s, held)) = crate::camera_copy::surface_of(texture) {
                crate::camera::note_ui_mask(s);
                crate::camera_copy::note_held(None, None, Some(held));
            }
        }
        // DLSS-G without an upscale hook: the velocity pass is behind us.
        crate::dlssg::tag_from_camera(list);
        let current = self.targets.first().map_or(0, |&t| t as u64);
        if crate::camera_copy::hudless_on() && current != 0 && !compute {
            crate::camera_copy::copy_hudless(list, current);
        }
    }
}

impl State {
    fn camera_scene_out(&self, compute: bool) {
        if let (false, Some(&t)) = (compute, self.targets.first()) {
            *SCENE_OUT.lock().unwrap_or_else(|e| e.into_inner()) = (crate::camera::frame(), t as u64);
        }
    }

    /// The frame's first Scaleform draw onto the scene at output size (the
    /// same target, or one of its size): the HUD-less picture is the target
    /// as it is now.
    fn camera_ui_draw(&self, list: &HostList, compute: bool) {
        let frame = crate::camera::frame();
        if compute || UI_FRAME.load(Ordering::Relaxed) == frame {
            return;
        }
        let Some(current) = self.targets.first().map(|&t| t as u64) else {
            return;
        };
        let (scene_frame, scene) = *SCENE_OUT.lock().unwrap_or_else(|e| e.into_inner());
        if scene_frame != frame || scene == 0 {
            return;
        }
        if current != scene {
            let size = |t| crate::camera_copy::surface_of(t).map(|s| s.0.size);
            if size(current).is_none() || size(current) != size(scene) {
                return;
            }
        }
        UI_FRAME.store(frame, Ordering::Relaxed);
        crate::camera::note_ui(crate::camera::Ui { texture: 0, target: current });
        crate::dlssg::tag_from_camera(list);
        if crate::camera_copy::hudless_on() {
            crate::camera_copy::copy_hudless(list, current);
        }
    }

    /// `VICTORY_CAMERA_DIAG=1`: the bind's targets and views, kept for the
    /// frame's dump if it is a candidate.
    fn camera_diag(&self, list: &HostList, stage: &Stage, compute: bool, info: &Info, bind: u32) {
        if stage.shader.stage == "vertex" {
            return;
        }
        let shape = |slot: String, title: u64| {
            if title == 0 || !crate::resource::host_copy_made(title as crate::object::This) {
                return None;
            }
            crate::camera_copy::surface_of(title)
                .map(|(s, _)| Shape { slot, title, format: s.format, size: s.size })
        };
        let mut shapes = Vec::new();
        if !compute {
            shapes.extend(self.targets.iter().enumerate().filter_map(|(i, &t)| shape(format!("rt{i}"), t as u64)));
            shapes.extend(shape("dsv".into(), self.depth as u64));
        }
        let views = |kind: ResourceType, prefix: char| {
            stage
                .translated()
                .used
                .iter()
                .filter(move |u| u.kind == kind && u.root == RootType::Table)
                .filter_map(move |u| match self.camera_view(compute, u) {
                    Some(View::Shader(r, _) | View::Unordered(r, _, _)) => {
                        shape(format!("{prefix}{}", u.lower_bound), r as u64)
                    }
                    _ => None,
                })
        };
        shapes.extend(views(ResourceType::Uav, 'u').take(8));
        let written = shapes.iter().any(|s| s.format == crate::camera_diag::R16G16_FLOAT);
        if info.tag.is_none() && !written {
            return;
        }
        shapes.extend(views(ResourceType::Srv, 't').take(8));
        crate::camera_diag::note(Entry {
            bind,
            list: std::ptr::from_ref(list) as usize,
            stage: stage.shader.stage,
            hash: stage.shader.hash.clone(),
            name: info.name.clone(),
            tag: info.tag,
            shapes,
        });
    }
}

fn me_of(s: &State) -> usize {
    std::ptr::from_ref(s) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_follow_the_program_names() {
        assert_eq!(role_of("PS_VelocityFill_4f86a26b_Wrapped"), Role::VelocityFill);
        assert_eq!(
            role_of("deferred_to_gbuffer_fill:CS_2GBufFixupAndClearTemp_Velocity_2b5e7808_Wrapped"),
            Role::VelocityCs
        );
        assert_eq!(
            role_of("PS_2GBufFixupAndClearTemp_Velocity_ca13643b_Wrapped"),
            Role::Velocity
        );
        assert_eq!(
            role_of("PS_BlitUISDRToBackbuffer_8e9c2e8a_Wrapped"),
            Role::Ui
        );
        assert_eq!(role_of("VS_VelocityFill_4f86a26b_Wrapped"), Role::None);
        assert_eq!(role_of("PS_LateComposite_f0b98a13_Wrapped"), Role::None);
        assert_eq!(
            role_of("PS_TAA2_SpatialUpscaleWithDithering_08f754c8_Wrapped"),
            Role::SceneOut
        );
        assert_eq!(role_of("VS_TAA2_SpatialUpscale_08f754c8_Wrapped"), Role::None);
        assert_eq!(role_of("FVertexCxformEAlpha_bc8bd7de_Wrapped"), Role::UiDraw);
        assert_eq!(role_of("FTexTGCxformEAlpha_dbb12f08_Wrapped"), Role::UiDraw);
        // What the old name lookup gave a graphics container: no role.
        assert_eq!(role_of("SV_Position"), Role::None);
        assert_eq!(role_of(""), Role::None);
    }

    fn used(kind: ResourceType, dwords: u32, lower_bound: u32, stride: u32) -> UsedResource {
        UsedResource {
            kind,
            root: RootType::Table,
            table_offset_dwords: dwords,
            lower_bound,
            stride,
            dwords: 4,
            user_data: 9,
            slot: 1,
        }
    }

    #[test]
    fn the_velocity_and_ui_bindings_are_picked_as_the_shaders_declare_them() {
        // PS_VelocityFill: t13 (stride 288) at dword 0, b0 at dword 8.
        assert!(is_view_record(&used(ResourceType::Srv, 0, 13, 288)));
        assert!(!is_view_record(&used(ResourceType::Srv, 0, 13, 4)));
        assert!(is_pass_constants(&used(ResourceType::Cbv, 8, 0, 0)));
        assert!(!is_pass_constants(&used(ResourceType::Cbv, 16, 26, 0)));
        // PS_BlitUISDRToBackbuffer: t274 at dword 0, the UI t257 at dword 8.
        assert!(is_ui_texture(&used(ResourceType::Srv, 8, 257, 4)));
        assert!(!is_ui_texture(&used(ResourceType::Srv, 0, 274, 4)));
        // CS_2GBufFixupAndClearTemp_Velocity: depth t211 at dword 8, motion
        // u19 at dword 56 (the record t16 at dword 0, b0 at dword 64).
        assert!(is_fixup_depth(&used(ResourceType::Srv, 8, 211, 4)));
        assert!(!is_fixup_depth(&used(ResourceType::Srv, 16, 212, 4)));
        assert!(is_fixup_motion(&used(ResourceType::Uav, 56, 19, 4)));
        assert!(!is_fixup_motion(&used(ResourceType::Uav, 48, 17, 4)));
        assert!(is_view_record(&used(ResourceType::Srv, 0, 16, 288)));
    }
}
