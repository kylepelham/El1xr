//! `VICTORY_CAMERA_CAPTURE=1` (off by default): at the bind of the title's
//! velocity passes (`PS_VelocityFill`, `*_2GBufFixupAndClearTemp_Velocity`)
//! and of its UI composite (`PS_BlitUISDRToBackbuffer`), notes where the
//! camera constants and the UI texture are, for [`crate::camera`]. Reads
//! descriptors only; nothing recorded changes.

use std::collections::HashMap;
use std::sync::Mutex;

use victory_shader::{ResourceType, RootType, UsedResource};

use super::{Root, State};
use crate::descriptor::View;
use crate::pipeline::host::{HostPipeline, Stage};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Role {
    None,
    Velocity,
    Ui,
}

/// What a shader of this program name is to the camera.
pub(super) fn role_of(name: &str) -> Role {
    if name.starts_with("PS_VelocityFill") || name.contains("2GBufFixupAndClearTemp_Velocity") {
        Role::Velocity
    } else if name.starts_with("PS_BlitUISDRToBackbuffer") {
        Role::Ui
    } else {
        Role::None
    }
}

/// By shader object (the shader table keeps every one for the run).
fn role(stage: &Stage) -> Role {
    static ROLES: Mutex<Option<HashMap<usize, Role>>> = Mutex::new(None);
    let key = std::sync::Arc::as_ptr(&stage.shader) as usize;
    let mut roles = ROLES.lock().unwrap_or_else(|e| e.into_inner());
    *roles
        .get_or_insert_with(HashMap::new)
        .entry(key)
        .or_insert_with(|| role_of(stage.shader.program().unwrap_or("")))
}

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

/// The UI texture of the composite: its SRV at table dword 8.
fn is_ui_texture(u: &UsedResource) -> bool {
    const UI_DWORDS: u32 = 8;
    u.kind == ResourceType::Srv && u.root == RootType::Table && u.table_offset_dwords == UI_DWORDS
}

impl State {
    pub(super) fn camera_tap(&self, p: &HostPipeline, compute: bool) {
        for stage in &p.stages {
            match role(stage) {
                Role::Velocity => self.camera_velocity(stage, compute),
                Role::Ui => self.camera_ui(stage, compute),
                Role::None => {}
            }
        }
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

    fn camera_ui(&self, stage: &Stage, compute: bool) {
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
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_follow_the_program_names() {
        assert_eq!(role_of("PS_VelocityFill_4f86a26b_Wrapped"), Role::Velocity);
        assert_eq!(
            role_of("CS_2GBufFixupAndClearTemp_Velocity_2b5e7808_Wrapped"),
            Role::Velocity
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
    }
}
