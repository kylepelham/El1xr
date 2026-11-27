//! (Vice-Tay `VICTORY_AMBIENT_REFLECT`, CCS03 `VICTORY_REFLECT_KNEE`) The
//! image a pass of `crate::ambient_reflect` holds after a dispatch of the
//! title's reflection resolve or history pass: a 2D R32_UINT image (holding
//! R9G9B9E5) at one of the program's places.

use super::*;
use crate::ambient::{Role, Views};
use crate::ambient_reflect::{FORMAT, KNEE_IMAGE, REFLECTIONS};

impl State {
    /// After a dispatch of `p` in `role` ([`Role::holds_image`]): the image
    /// it wrote held to the light around (`crate::ambient::after`). False if
    /// nothing was recorded.
    pub(super) fn ambient_hold(&self, list: &HostList, p: &HostPipeline, role: Role) -> bool {
        let Some(t) = p.stages.first().map(|s| s.translated()) else {
            return false;
        };
        let Some(heap) = (self.heaps[0] != 0)
            .then(|| crate::descriptor::heap(self.heaps[0] as This).host())
            .flatten()
        else {
            return false;
        };
        let entry = |(slot, offset): (u8, u32)| {
            t.used.iter().find(|u| {
                u.kind == ResourceType::Uav
                    && u.root == RootType::Table
                    && (u.slot, u.table_offset_dwords) == (slot, offset)
            })?;
            let Some(Root::Table(table)) = self.roots_ref(true).get(usize::from(slot)) else {
                return None;
            };
            let view = crate::descriptor::view(table + u64::from(offset) * 4)?;
            let view = crate::descriptor::host::placed_texture(&view).unwrap_or(view);
            let View::Unordered(resource, ..) = view else {
                return None;
            };
            let host = crate::resource::host_resource(resource as This)?;
            // SAFETY: a live resource.
            let desc = unsafe { host.GetDesc() };
            let start = match self.ambient_tables.iter().find(|m| m.0 == slot) {
                Some(m) => m.1,
                None => self.user_value(true, slot, RootType::Table, 0),
            };
            Some((desc, heap.gpu_handle((start + offset * 4) / 32)))
        };
        let places: &[(u8, u32)] = match role {
            Role::Reflect => &REFLECTIONS,
            _ => &[KNEE_IMAGE],
        };
        let image = places
            .iter()
            .find_map(|&at| entry(at).filter(|(d, _)| fits(d)));
        let Some((of, image)) = image else {
            once!(
                ?role,
                "no hold of the reflections: no 2D R32_UINT image at the program's places"
            );
            return false;
        };
        static SAID: std::sync::Mutex<[bool; 2]> = std::sync::Mutex::new([false; 2]);
        {
            let mut said = SAID.lock().unwrap_or_else(|e| e.into_inner());
            let at = usize::from(role == Role::Knee);
            if !said[at] {
                said[at] = true;
                tracing::info!(?role, program = %p.name, width = of.Width, height = of.Height, "reflections held to the light around");
            }
        }
        crate::ambient::after(
            list,
            role,
            &Views {
                image,
                depth: None,
                size: [of.Width as u32, of.Height],
                slices: 1,
                depth_size: [of.Width as u32, of.Height],
            },
        )
    }
}

/// A 2D image of one layer in the format the holds read.
fn fits(d: &D3D12_RESOURCE_DESC) -> bool {
    d.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE2D
        && d.Format.0 as u32 == FORMAT
        && d.DepthOrArraySize == 1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_flat_r32_uint_image_is_held() {
        let texture = |format: i32, layers: u16| D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Width: 1920,
            Height: 1080,
            DepthOrArraySize: layers,
            Format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT(format),
            ..Default::default()
        };
        assert!(fits(&texture(42, 1)));
        assert!(!fits(&texture(26, 1)));
        assert!(!fits(&texture(42, 2)));
        let mut buffer = texture(42, 1);
        buffer.Dimension = D3D12_RESOURCE_DIMENSION_BUFFER;
        assert!(!fits(&buffer));
    }
}
