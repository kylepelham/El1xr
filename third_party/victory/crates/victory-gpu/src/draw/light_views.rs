//! The views the stand-in for the indirect light (`crate::ambient`) raises
//! after a dispatch of the resolve, the radiance cache or the air: their
//! places from `crate::ambient_places` (by register with
//! `VICTORY_AMBIENT_VARIANTS=1`), and what came of each dispatch counted by
//! `crate::ambient_cover`.

use super::miss_views::{depth_like, stencil_view};
use super::*;
use crate::ambient::{Role, Views};
use crate::ambient_cover::Outcome;

/// `VICTORY_AMBIENT_DEPTH_CHECK=1` (off by default; TheSmurphilicious
/// 9f8294c6c1, a port of readmet3xt-lab/foxhound d226814 + bda6e59): the
/// resolve's depth is never a view of a stencil plane where another depth of
/// the image's size is in its tables. After the `_ReflectiveCaustics`
/// variant `34fb51b8a0312e82` the depth place names the scene depth's
/// stencil plane, and the estimate made of it lights the room as a flat
/// milky fill. With the check, the resolve's views also read a texture the
/// title placed in its memory as the resource made for it, as the miss
/// program's do.
fn depth_check() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_AMBIENT_DEPTH_CHECK").is_some_and(|v| v == "1"))
}

/// The depth the resolve reads: with the check on and the usual place a
/// stencil plane, the first other one `others` finds; `None` keeps the
/// usual place (as without the check when no other fits).
fn other_depth<T>(check: bool, stencil: bool, others: impl FnOnce() -> Option<T>) -> Option<T> {
    (check && stencil).then(others).flatten()
}

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
        let check = role == Role::Resolve && depth_check();
        let entry = |kind: ResourceType, (slot, offset): (u8, u32), not_set, no_view, no_copy| {
            let Some(Root::Table(table)) = self.roots_ref(true).get(usize::from(slot)) else {
                return Err(not_set);
            };
            let view = crate::descriptor::view(table + u64::from(offset) * 4).map(|v| {
                if check { crate::descriptor::host::placed_texture(&v).unwrap_or(v) } else { v }
            });
            let resource = match (kind, view) {
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
        let stencil_at = |(slot, offset): (u8, u32)| match self.roots_ref(true).get(usize::from(slot)) {
            Some(Root::Table(table)) => stencil_view(crate::descriptor::view(table + u64::from(offset) * 4)),
            _ => false,
        };
        let depth = match places.depth {
            Some(at) => {
                let stencil = check && stencil_at(at);
                let other = other_depth(check, stencil, || {
                    t.used
                        .iter()
                        .filter(|u| u.kind == ResourceType::Srv && u.root == RootType::Table)
                        .filter(|u| !stencil_at((u.slot, u.table_offset_dwords)))
                        .filter_map(|u| entry(ResourceType::Srv, (u.slot, u.table_offset_dwords), "", "", "").ok())
                        .find(|(d, _)| depth_like(d) && (d.Width, d.Height) == (of.Width, of.Height))
                });
                if stencil {
                    static SAID: std::sync::Once = std::sync::Once::new();
                    SAID.call_once(|| {
                        let taken = other.as_ref().map(|(d, _)| (d.Width, d.Height, d.Format.0));
                        tracing::info!(
                            place = ?at,
                            ?taken,
                            "stand-in: the resolve's depth place names a stencil plane; another depth view of its tables is taken \
                             (VICTORY_AMBIENT_DEPTH_CHECK=1, said once; none: the place as before)"
                        );
                    });
                }
                let (seen, depth) = match other {
                    Some(found) => found,
                    None => entry(
                        ResourceType::Srv,
                        at,
                        "the depth's table is not set",
                        "no depth SRV in the table",
                        "the depth has no host copy",
                    )?,
                };
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

#[cfg(test)]
mod tests {
    use super::other_depth;

    /// Only with the check on and the usual place a stencil plane is another
    /// depth taken; else (or when none fits) the usual place.
    #[test]
    fn another_depth_only_for_a_stencil_plane_with_the_check() {
        assert_eq!(other_depth(true, true, || Some(7)), Some(7));
        assert_eq!(other_depth(true, true, || None::<u32>), None);
        assert_eq!(other_depth(false, true, || Some(7)), None);
        assert_eq!(other_depth(true, false, || Some(7)), None);
    }
}
