//! (`crate::hdr_scrub`, `VICTORY_HDR_SCRUB`) The 2D float images an HDR scene-colour program
//! of the title's wrote, handed to the NaN/infinity count after its dispatch.

use super::*;

impl State {
    /// After a dispatch of `p`: when its program is one of `crate::hdr_scrub::NAMES`, every
    /// image it writes through a table is counted (and with `2` scrubbed). Whether it recorded
    /// anything (the compute root signature and pipeline are then set again by the next bind).
    pub(super) fn hdr_scrub(&mut self, list: &HostList, p: &HostPipeline) -> bool {
        let Some(pass) = p.stages.first().and_then(|s| s.shader.hdr) else {
            return false;
        };
        let Some(t) = p.stages.first().map(|s| s.translated()) else {
            return false;
        };
        let Some(heap) = (self.heaps[0] != 0)
            .then(|| crate::descriptor::heap(self.heaps[0] as This).host())
            .flatten()
        else {
            return false;
        };
        let mut seen: Vec<(u8, u32)> = Vec::new();
        let mut recorded = false;
        for u in &t.used {
            if u.kind != ResourceType::Uav || u.root != RootType::Table {
                continue;
            }
            let at = (u.slot, u.table_offset_dwords);
            if seen.contains(&at) {
                continue;
            }
            seen.push(at);
            let Some(Root::Table(table)) = self.roots_ref(true).get(usize::from(u.slot)) else {
                continue;
            };
            let resource =
                match crate::descriptor::view(table + u64::from(u.table_offset_dwords) * 4) {
                    Some(View::Unordered(r, ..)) if r != 0 => r,
                    _ => {
                        crate::hdr_scrub::skipped(pass, -1);
                        continue;
                    }
                };
            let Some(host) = crate::resource::host_resource(resource as This) else {
                crate::hdr_scrub::skipped(pass, -1);
                continue;
            };
            // SAFETY: a live resource.
            let desc = unsafe { host.GetDesc() };
            if desc.Dimension != D3D12_RESOURCE_DIMENSION_TEXTURE2D
                || desc.DepthOrArraySize != 1
                || desc.SampleDesc.Count != 1
            {
                continue;
            }
            let Some(kind) = crate::hdr_scrub::kind_of(desc.Format.0) else {
                crate::hdr_scrub::skipped(pass, desc.Format.0);
                continue;
            };
            let start = match self.ambient_tables.iter().find(|m| m.0 == u.slot) {
                Some(m) => m.1,
                None => self.user_value(true, u.slot, RootType::Table, 0),
            };
            let image = heap.gpu_handle((start + u.table_offset_dwords * 4) / 32);
            recorded |=
                crate::hdr_scrub::run(list, pass, kind, image, [desc.Width as u32, desc.Height]);
        }
        if recorded {
            self.host_pipeline = 0;
            self.host_roots[1] = 0;
        }
        recorded
    }
}
