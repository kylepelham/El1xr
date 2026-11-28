//! `VICTORY_FLICKER_CATCH_REGION` (`crate::flicker_catch::region`): what a
//! draw or dispatch covers of the picture and the textures its tables
//! bind. Reads descriptors only; nothing recorded changes.

use victory_shader::{ResourceType, RootType};

use super::{Root, State};
use crate::descriptor::View;
use crate::flicker_catch::region::{self, Texture};
use crate::object::This;
use crate::pipeline::host::HostPipeline;

impl State {
    pub(super) fn flicker_region(&self, p: &HostPipeline, compute: bool) {
        use crate::resource::find_facts::object_brief;
        let rt = if compute {
            0
        } else {
            match self.targets.first() {
                Some(&t) => t,
                None => return,
            }
        };
        let mut target = (0, 0, 0);
        let mut mask = [0u64; 3];
        if !compute {
            let Some((memory, w, h, _, _)) = object_brief(rt as This) else {
                return;
            };
            if !region::screen_shaped(w, u64::from(h)) {
                return;
            }
            target = (memory, w, h);
            let viewport = self
                .viewports
                .first()
                .map(|v| [v.TopLeftX, v.TopLeftY, v.Width, v.Height]);
            let scissor = self
                .scissors
                .first()
                .map(|r| [r.left, r.top, r.right, r.bottom]);
            match region::footprint(viewport, scissor, w, u64::from(h)) {
                Some(share) => mask = region::mask_of_share(share),
                None => return,
            }
        }
        let roots = if compute {
            &self.compute
        } else {
            &self.graphics
        };
        let sig = roots.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, r| match r {
            Root::Table(t) => (h ^ t).wrapping_mul(0x0100_0000_01b3),
            _ => h,
        });
        if !region::note(self.pipeline, rt, sig, mask, compute) {
            return;
        }
        let frame = crate::frame::current();
        let heap = self
            .heaps
            .iter()
            .filter(|&&h| h != 0)
            .map(|&h| crate::descriptor::heap(h as This))
            .find(|h| !h.samplers());
        let mut textures = Vec::new();
        let mut wide = false;
        let mut heap_indexed = false;
        for stage in &p.stages {
            if stage.shader.stage == "vertex" {
                continue;
            }
            let used = &stage.translated().used;
            heap_indexed |= used.iter().any(|u| u.root == RootType::ResourceHeap);
            for u in used.iter().filter(|u| {
                matches!(u.kind, ResourceType::Srv | ResourceType::Uav) && u.root == RootType::Table
            }) {
                let Some(Root::Table(table)) = roots.get(usize::from(u.slot)) else {
                    continue;
                };
                let handle = table + u64::from(u.table_offset_dwords) * 4;
                let (resource, memory, width, height, format) =
                    match crate::descriptor::view(handle) {
                        Some(View::Shader(r, _) | View::Unordered(r, _, _)) => {
                            let Some((m, w, h, f, dim)) = object_brief(r as This) else {
                                continue;
                            };
                            // Buffers are not textures.
                            if dim == 1 {
                                continue;
                            }
                            (r, m, w, h, f)
                        }
                        Some(View::Placed(_, res, view)) => {
                            let (w, h, f, dim) =
                                crate::resource::find_facts::words_brief(&res);
                            if dim == 1 {
                                continue;
                            }
                            (0, u64::from(view[10]) | u64::from(view[11]) << 32, w, h, f)
                        }
                        _ => continue,
                    };
                let uav = u.kind == ResourceType::Uav;
                if compute && uav && region::screen_shaped(width, u64::from(height)) {
                    wide = true;
                }
                let record = crate::descriptor::record_at(handle);
                let slot = heap.and_then(|h| h.index(handle));
                let host = match (heap, slot) {
                    (Some(h), Some(s)) => crate::descriptor::host_slot_state(h.start, s, record),
                    _ => "not_in_heap",
                };
                let key = if resource != 0 {
                    resource as u64
                } else {
                    memory
                };
                textures.push(Texture {
                    kind: if uav { b'u' } else { b't' },
                    register: u.lower_bound as u16,
                    resource,
                    memory,
                    width,
                    height,
                    format,
                    slot,
                    host,
                    view: (record[1], record[2]),
                    first: region::first_seen(key, frame),
                });
            }
        }
        region::walked(self.pipeline, rt, sig, target, textures, wide, heap_indexed);
    }
}
