//! The views the stand-in for the indirect light (`crate::ambient`) raises
//! after a dispatch of the resolve, the radiance cache or the air: their
//! places from `crate::ambient_places` (by register with
//! `VICTORY_AMBIENT_VARIANTS=1`), and what came of each dispatch counted by
//! `crate::ambient_cover`.

use super::*;
use crate::ambient::{Role, Views};
use crate::ambient_cover::Outcome;

impl State {
    /// After a dispatch of `p` of `role` ([`Role::Resolve`],
    /// [`Role::Radiance`], [`Role::Air`]): whether it recorded anything.
    pub(super) fn ambient_light(&self, list: &HostList, p: &HostPipeline, role: Role) -> bool {
        let Some(stage) = p.stages.first() else {
            return false;
        };
        if !crate::ambient::light_asked(role) {
            return false;
        }
        let outcome = match self.light_views(list, p, role) {
            Ok(false) => Outcome::Applied,
            Ok(true) => Outcome::Elsewhere,
            Err(why) => Outcome::Skipped(why),
        };
        crate::ambient_cover::note(&stage.shader, role, outcome);
        !matches!(outcome, Outcome::Skipped(_))
    }

    /// Finds the views and records the stand-in: whether the image raised
    /// is not the one at the role's register, or why nothing was recorded.
    fn light_views(
        &self,
        list: &HostList,
        p: &HostPipeline,
        role: Role,
    ) -> Result<bool, &'static str> {
        let t = p.stages.first().ok_or("no stage")?.translated();
        let heap = (self.heaps[0] != 0)
            .then(|| crate::descriptor::heap(self.heaps[0] as This).host())
            .flatten()
            .ok_or("no host view heap is set")?;
        let places =
            crate::ambient_places::places(role, &t.used, crate::ambient_places::variants())?;
        let entry = |kind: ResourceType, (slot, offset): (u8, u32), not_set, no_view, no_copy| {
            let Some(Root::Table(table)) = self.roots_ref(true).get(usize::from(slot)) else {
                return Err(not_set);
            };
            let resource = match (kind, crate::descriptor::view(table + u64::from(offset) * 4)) {
                (ResourceType::Srv, Some(View::Shader(r, _)))
                | (ResourceType::Uav, Some(View::Unordered(r, ..))) => r,
                _ => return Err(no_view),
            };
            let start = match self.ambient_tables.iter().find(|m| m.0 == slot) {
                Some(m) => m.1,
                None => self.user_value(true, slot, RootType::Table, 0),
            };
            let host = crate::resource::host_resource(resource as This).ok_or(no_copy)?;
            // SAFETY: a live resource.
            let desc = unsafe { host.GetDesc() };
            Ok((desc, heap.gpu_handle((start + offset * 4) / 32)))
        };
        let (of, image) = entry(
            ResourceType::Uav,
            places.image,
            "the image's table is not set",
            "no image UAV in the table",
            "the image has no host copy",
        )?;
        let depth = match places.depth {
            Some(at) => {
                let (seen, depth) = entry(
                    ResourceType::Srv,
                    at,
                    "the depth's table is not set",
                    "no depth SRV in the table",
                    "the depth has no host copy",
                )?;
                if (seen.Width, seen.Height) != (of.Width, of.Height) {
                    return Err("depth dimensions differ");
                }
                Some(depth)
            }
            None => None,
        };
        // (R32_UINT holding R9G9B9E5; R11G11B10_FLOAT.)
        let format = if role == Role::Resolve { 42 } else { 26 };
        let volume = of.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE3D;
        if of.Format.0 != format || volume != (role == Role::Air) {
            return Err("another image (format or dimension)");
        }
        let views = Views {
            image,
            depth,
            size: [of.Width as u32, of.Height],
            slices: u32::from(of.DepthOrArraySize),
            depth_size: [of.Width as u32, of.Height],
        };
        if !crate::ambient::after(list, role, &views) {
            return Err(crate::ambient::not_recorded());
        }
        Ok(places.elsewhere)
    }
}
