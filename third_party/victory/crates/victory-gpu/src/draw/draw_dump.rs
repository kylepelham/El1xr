//! `VICTORY_DRAW_DUMP` (`crate::draw_dump`): what a bind of a dumped frame
//! draws with. Reads descriptors only; nothing recorded changes.

use victory_shader::{ResourceType, RootType};

use super::{HostList, Root, State};
use crate::descriptor::View;
use crate::draw_dump::{Bound, texture};
use crate::object::This;
use crate::pipeline::host::HostPipeline;

/// Views of one kind logged per stage at most.
const MOST_VIEWS: usize = 12;

impl State {
    pub(super) fn draw_dump(&self, list: &HostList, p: &HostPipeline, compute: bool) {
        let stages: Vec<(&str, &str, String)> = p
            .stages
            .iter()
            .map(|s| {
                (
                    s.shader.stage,
                    s.shader.hash.as_str(),
                    crate::draw_dump::name(std::sync::Arc::as_ptr(&s.shader) as usize, || {
                        s.shader.camera_name()
                    }),
                )
            })
            .collect();
        let heap = p.stages.iter().any(|s| {
            s.translated()
                .used
                .iter()
                .any(|u| u.root == RootType::ResourceHeap)
        });
        let mut bound = Vec::new();
        if !compute {
            bound.extend(
                self.targets
                    .iter()
                    .enumerate()
                    .filter_map(|(i, &t)| texture(format!("rt{i}"), t as This)),
            );
            bound.extend(texture("dsv".into(), self.depth as This));
        }
        let roots = if compute {
            &self.compute
        } else {
            &self.graphics
        };
        for stage in &p.stages {
            if stage.shader.stage == "vertex" {
                continue;
            }
            for (kind, prefix) in [(ResourceType::Srv, 't'), (ResourceType::Uav, 'u')] {
                let views = stage
                    .translated()
                    .used
                    .iter()
                    .filter(|u| u.kind == kind && u.root == RootType::Table)
                    .filter_map(|u| {
                        let Some(Root::Table(table)) = roots.get(usize::from(u.slot)) else {
                            return None;
                        };
                        let view =
                            crate::descriptor::view(table + u64::from(u.table_offset_dwords) * 4)?;
                        let slot = format!("{prefix}{}", u.lower_bound);
                        match view {
                            View::Shader(r, _) | View::Unordered(r, _, _) => {
                                texture(slot, r as This)
                            }
                            View::Placed(_, _, desc) => Some(Bound {
                                slot,
                                memory: u64::from(desc[10]) | u64::from(desc[11]) << 32,
                                shape: None,
                            }),
                            _ => None,
                        }
                    })
                    .take(MOST_VIEWS);
                bound.extend(views);
            }
        }
        let kind = if compute { "dispatch" } else { "draw" };
        crate::draw_dump::log(
            kind,
            &stages,
            heap,
            std::ptr::from_ref(list) as usize,
            &bound,
        );
    }
}
