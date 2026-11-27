//! (foxhound `9f8838b`, `crate::draw` `ambient`) The views the stand-in for
//! reflections (`VICTORY_AMBIENT_SPECULAR`, `crate::ambient::Role::Miss`)
//! needs after a dispatch of the title's miss program: the rays' image it
//! just wrote (R11G11B10, half the picture's size) and the picture's depth.

use super::*;
use crate::ambient::{MISS_DEPTH, MISS_LIGHT, Role, Views};
use crate::ambient_miss::{Note, note, takes};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_D24_UNORM_S8_UINT, DXGI_FORMAT_D32_FLOAT, DXGI_FORMAT_D32_FLOAT_S8X24_UINT,
    DXGI_FORMAT_R24G8_TYPELESS, DXGI_FORMAT_R32_FLOAT, DXGI_FORMAT_R32_TYPELESS,
    DXGI_FORMAT_R32G8X24_TYPELESS,
};

/// A depth-stencil texture's view of its stencil plane (formats
/// X32_TYPELESS_G8X24_UINT, X24_TYPELESS_G8_UINT, or a 2D view of plane 1)
/// is no depth: the estimate made of it puts every point far away with its
/// normal to the camera (flat floors). (The description's words are
/// D3D12_SHADER_RESOURCE_VIEW_DESC's: its union starts at word 4, so a 2D
/// view's plane is word 6.)
fn stencil_view(view: Option<View>) -> bool {
    let view = view.map(|v| crate::descriptor::host::placed_texture(&v).unwrap_or(v));
    match view {
        Some(View::Shader(_, Some(d))) => {
            matches!(crate::resource::pc_format(d[0]), 22 | 47) || (d[1] == 4 && d[6] == 1)
        }
        _ => false,
    }
}

/// A 2D texture of a depth format, or of the typeless format one is viewed through.
fn depth_like(d: &D3D12_RESOURCE_DESC) -> bool {
    d.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE2D
        && matches!(
            d.Format,
            DXGI_FORMAT_R32_TYPELESS
                | DXGI_FORMAT_D32_FLOAT
                | DXGI_FORMAT_R32G8X24_TYPELESS
                | DXGI_FORMAT_D32_FLOAT_S8X24_UINT
                | DXGI_FORMAT_R24G8_TYPELESS
                | DXGI_FORMAT_D24_UNORM_S8_UINT
        )
}

/// A 32-bit depth.
fn depth32(d: &D3D12_RESOURCE_DESC) -> bool {
    matches!(
        d.Format,
        DXGI_FORMAT_R32_TYPELESS
            | DXGI_FORMAT_D32_FLOAT
            | DXGI_FORMAT_R32G8X24_TYPELESS
            | DXGI_FORMAT_D32_FLOAT_S8X24_UINT
    )
}

/// Twice the rays' image `(w, h)` each way, give or take a texel: the picture's size.
fn twice(d: &D3D12_RESOURCE_DESC, (w, h): (u64, u32)) -> bool {
    d.Width.abs_diff(w * 2) <= 1 && u64::from(d.Height).abs_diff(u64::from(h) * 2) <= 1
}

/// The depth may be read for an image of `of`: 32-bit, at least its size
/// and at most four times its width.
fn sizes_fit(seen: &D3D12_RESOURCE_DESC, of: &D3D12_RESOURCE_DESC) -> bool {
    (depth32(seen) || seen.Format == DXGI_FORMAT_R32_FLOAT)
        && seen.Width >= of.Width
        && seen.Height >= of.Height
        && seen.Width <= of.Width * 4
}

impl State {
    /// After a dispatch of the miss program `p`: its reflections that found
    /// nothing get the estimate along the reflected direction
    /// (`crate::ambient::after`). False if nothing was recorded.
    pub(super) fn ambient_miss(&self, list: &HostList, p: &HostPipeline) -> bool {
        let Some(t) = p.stages.first().map(|s| s.translated()) else {
            return false;
        };
        // Which surfaces its rays are of (its first root constant): water's
        // reflections are left as they are (the depth here is of what lies
        // under it).
        let layer = t
            .used
            .iter()
            .find(|u| u.root == RootType::Constants)
            .and_then(|u| match self.roots_ref(true).get(usize::from(u.slot)) {
                Some(Root::Constants(c)) => c.first().copied(),
                _ => None,
            })
            .unwrap_or(u32::MAX);
        if !takes(layer, crate::ambient::specular_alpha()) {
            note(Note::Water);
            return false;
        }
        let Some(heap) = (self.heaps[0] != 0)
            .then(|| crate::descriptor::heap(self.heaps[0] as This).host())
            .flatten()
        else {
            note(Note::NotReady);
            return false;
        };
        let start_of = |slot: u8| match self.ambient_tables.iter().find(|m| m.0 == slot) {
            Some(m) => m.1,
            None => self.user_value(true, slot, RootType::Table, 0),
        };
        let entry = |kind: ResourceType, (slot, offset): (u8, u32)| {
            t.used.iter().find(|u| {
                u.kind == kind
                    && u.root == RootType::Table
                    && (u.slot, u.table_offset_dwords) == (slot, offset)
            })?;
            let Some(Root::Table(table)) = self.roots_ref(true).get(usize::from(slot)) else {
                return None;
            };
            // A view of a texture the title placed in its memory is a placed
            // record; its host texture is the one placed_texture names.
            let view = crate::descriptor::view(table + u64::from(offset) * 4)?;
            let view = crate::descriptor::host::placed_texture(&view).unwrap_or(view);
            let resource = match (kind, view) {
                (ResourceType::Srv, View::Shader(r, _))
                | (ResourceType::Uav, View::Unordered(r, ..)) => r,
                _ => return None,
            };
            let host = crate::resource::host_resource(resource as This)?;
            // SAFETY: a live resource.
            let desc = unsafe { host.GetDesc() };
            Some((desc, heap.gpu_handle((start_of(slot) + offset * 4) / 32)))
        };
        let stencil_at = |(slot, offset): (u8, u32)| {
            let Some(Root::Table(table)) = self.roots_ref(true).get(usize::from(slot)) else {
                return false;
            };
            stencil_view(crate::descriptor::view(table + u64::from(offset) * 4))
        };
        // Variants of the miss program bind the rays' image and the depth at
        // other places of their tables than the usual one: where the usual
        // place has none, the first table entry of that kind that fits.
        let scan = |kind: ResourceType, fits: &dyn Fn(&D3D12_RESOURCE_DESC) -> bool| {
            t.used
                .iter()
                .filter(|u| u.kind == kind && u.root == RootType::Table)
                .filter(|u| {
                    kind != ResourceType::Srv || !stencil_at((u.slot, u.table_offset_dwords))
                })
                .filter_map(|u| entry(kind, (u.slot, u.table_offset_dwords)))
                .find(|(desc, _)| fits(desc))
        };
        let image = entry(ResourceType::Uav, MISS_LIGHT).or_else(|| {
            scan(ResourceType::Uav, &|d| {
                d.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE2D && d.Format.0 == 26
            })
        });
        let half = image.as_ref().map(|(d, _)| (d.Width, d.Height));
        let depth = (!stencil_at(MISS_DEPTH))
            .then(|| entry(ResourceType::Srv, MISS_DEPTH))
            .flatten()
            .filter(|(d, _)| half.is_some_and(|h| twice(d, h)))
            .or_else(|| {
                scan(ResourceType::Srv, &|d| {
                    depth_like(d) && depth32(d) && half.is_some_and(|h| twice(d, h))
                })
            })
            .or_else(|| self.frame_depth(half?, &heap, start_of(0)));
        // (trustno1-coder/VIctory-NVIDIA#124) The rays' image without a
        // depth that fits: the light around, without a direction, if asked.
        let without_depth = |of: &D3D12_RESOURCE_DESC, image| {
            let ran = crate::ambient_reflect::bound()
                && of.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE2D
                && of.Format.0 == 26
                && crate::ambient::without_depth(
                    list,
                    &Views {
                        image,
                        depth: None,
                        size: [of.Width as u32, of.Height],
                        slices: 1,
                        depth_size: [of.Width as u32, of.Height],
                    },
                );
            note(if ran { Note::Bound(layer) } else { Note::NotReady });
            ran
        };
        let (Some((of, image)), Some((seen, depth))) = (image, depth) else {
            once!(
                "no stand-in for reflections: the miss program's image or depth not found (counted in the 10 s line)"
            );
            return image.is_some_and(|(of, image)| without_depth(&of, image));
        };
        let volume = of.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE3D;
        if of.Format.0 != 26 || volume || !sizes_fit(&seen, &of) {
            once!(
                format = of.Format.0,
                "no stand-in for reflections: another image"
            );
            return without_depth(&of, image);
        }
        let ran = crate::ambient::after(
            list,
            Role::Miss,
            &Views {
                image,
                depth: Some(depth),
                size: [of.Width as u32, of.Height],
                slices: u32::from(of.DepthOrArraySize),
                depth_size: [seen.Width as u32, seen.Height],
            },
        );
        if ran {
            note(Note::Ran(layer));
        }
        ran
    }

    /// In the opening's cutscenes the miss program's own places hold no
    /// depth of the picture (its usual place a 1x1 texture). The picture's
    /// depth is still in the frame's table, at a place this program does not
    /// read: every place of that table, a 24+8-bit-wide 32-bit depth (the
    /// depth-stencil target's depth plane) first, then any 32-bit depth of
    /// twice the rays' image's size.
    fn frame_depth(
        &self,
        half: (u64, u32),
        heap: &crate::descriptor::host::HostHeap,
        start: u32,
    ) -> Option<(D3D12_RESOURCE_DESC, D3D12_GPU_DESCRIPTOR_HANDLE)> {
        let Some(Root::Table(table)) = self.roots_ref(true).first() else {
            return None;
        };
        let mut plain = None;
        for offset in (0..1600u32).step_by(8) {
            let Some(view) = crate::descriptor::view(table + u64::from(offset) * 4) else {
                continue;
            };
            if stencil_view(Some(view.clone())) {
                continue;
            }
            let view = crate::descriptor::host::placed_texture(&view).unwrap_or(view);
            let View::Shader(resource, _) = view else {
                continue;
            };
            let Some(host) = crate::resource::host_resource(resource as This) else {
                continue;
            };
            // SAFETY: a live resource.
            let d = unsafe { host.GetDesc() };
            if d.Dimension != D3D12_RESOURCE_DIMENSION_TEXTURE2D || !twice(&d, half) {
                continue;
            }
            let found = (d, heap.gpu_handle((start + offset * 4) / 32));
            match d.Format {
                DXGI_FORMAT_R32G8X24_TYPELESS | DXGI_FORMAT_D32_FLOAT_S8X24_UINT => {
                    once!(
                        offset,
                        "reflections stand-in: the picture's depth taken from the frame's table"
                    );
                    return Some(found);
                }
                DXGI_FORMAT_R32_TYPELESS | DXGI_FORMAT_D32_FLOAT if plain.is_none() => {
                    plain = Some(found)
                }
                _ => {}
            }
        }
        plain
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texture(
        format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT,
        w: u64,
        h: u32,
    ) -> D3D12_RESOURCE_DESC {
        D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Width: w,
            Height: h,
            Format: format,
            ..Default::default()
        }
    }

    #[test]
    fn the_pictures_depth_is_twice_the_rays_image() {
        let depth = texture(DXGI_FORMAT_R32_TYPELESS, 3840, 2160);
        assert!(twice(&depth, (1920, 1080)));
        assert!(twice(
            &texture(DXGI_FORMAT_R32_TYPELESS, 3841, 2161),
            (1920, 1080)
        ));
        assert!(!twice(&depth, (1280, 720)));
        assert!(depth_like(&depth) && depth32(&depth));
        assert!(depth_like(&texture(DXGI_FORMAT_R24G8_TYPELESS, 8, 8)));
        assert!(!depth32(&texture(DXGI_FORMAT_R24G8_TYPELESS, 8, 8)));
        let image = texture(
            windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT(26),
            1920,
            1080,
        );
        assert!(sizes_fit(&depth, &image));
        assert!(!sizes_fit(
            &texture(DXGI_FORMAT_R32_TYPELESS, 960, 540),
            &image
        ));
        assert!(!sizes_fit(
            &texture(DXGI_FORMAT_R24G8_TYPELESS, 3840, 2160),
            &image
        ));
    }
}
