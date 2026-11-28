//! `VICTORY_HEAP_FIND` (`crate::heap_find`): the bindings of a bind of a
//! walked frame that hold the memory looked for, and (`bink`) the Bink
//! draws whose render target is followed. Reads descriptors only; nothing
//! recorded changes.

use victory_shader::{ResourceType, RootType};

use super::{HostList, Root, State};
use crate::object::This;
use crate::pipeline::host::HostPipeline;

impl State {
    /// (`VICTORY_HEAP_FIND=bink`) A draw of the Bink pixel program: its
    /// render target 0 is followed.
    pub(super) fn heap_find_follow(&self, p: &HostPipeline) {
        let Some(&rt0) = self.targets.first() else {
            return;
        };
        let Some(pixel) = p.stages.iter().find(|s| s.shader.stage == "pixel") else {
            return;
        };
        let name = crate::draw_dump::name(std::sync::Arc::as_ptr(&pixel.shader) as usize, || {
            pixel.shader.camera_name()
        });
        if !crate::heap_find::may_be_bink(&pixel.shader.hash, &name) {
            return;
        }
        let facts = crate::resource::find_facts::object_span(rt0 as This)
            .zip(crate::resource::find_facts::object_width(rt0 as This));
        let Some(((start, len), width)) = facts else {
            return;
        };
        crate::heap_find::bink_draw(
            &pixel.shader.hash,
            &name,
            start,
            len,
            width,
            crate::resource::find_facts::object_text(rt0 as This),
        );
    }

    pub(super) fn heap_find(&self, list: &HostList, p: &HostPipeline, compute: bool) {
        let spans = crate::heap_find::spans();
        let bind = crate::heap_find::next_bind();
        let by_index = p.stages.iter().any(|s| {
            s.translated()
                .used
                .iter()
                .any(|u| u.root == RootType::ResourceHeap)
        });
        if by_index {
            crate::heap_find::note_by_index();
        }
        let mut hits = Vec::new();
        let object = |name: String, t: This| {
            let (start, len) = crate::resource::find_facts::object_span(t)?;
            crate::heap_find::meets(&spans, start, len)
                .then(|| format!("{name} {}", crate::resource::find_facts::object_text(t)))
        };
        if !compute {
            hits.extend(
                self.targets
                    .iter()
                    .enumerate()
                    .filter_map(|(i, &t)| object(format!("rt{i}"), t as This)),
            );
            hits.extend(object("dsv".into(), self.depth as This));
        }
        let roots = if compute {
            &self.compute
        } else {
            &self.graphics
        };
        for (slot, root) in roots.iter().enumerate() {
            if let &Root::View(address) = root
                && crate::heap_find::meets(&spans, address, 1)
            {
                hits.push(format!("root{slot} view at {address:#x}"));
            }
        }
        for stage in &p.stages {
            for u in stage.translated().used.iter() {
                let prefix = match (u.kind, u.root) {
                    (ResourceType::Srv, RootType::Table) => 't',
                    (ResourceType::Uav, RootType::Table) => 'u',
                    _ => continue,
                };
                let Some(Root::Table(table)) = roots.get(usize::from(u.slot)) else {
                    continue;
                };
                let handle = table + u64::from(u.table_offset_dwords) * 4;
                let Some(view) = crate::descriptor::view(handle) else {
                    continue;
                };
                if crate::heap_find::covers_any(&view, &spans).is_none() {
                    continue;
                }
                let slot = crate::descriptor::shader_heap_slot(handle)
                    .map_or_else(|| "?".into(), |(heap, i)| format!("{heap:#x}[{i}]"));
                hits.push(format!(
                    "{} {prefix}{} slot {slot} {}",
                    stage.shader.stage,
                    u.lower_bound,
                    crate::heap_find::view_text(&view, &spans)
                ));
            }
        }
        if by_index && self.heaps[0] != 0 {
            let heap = crate::descriptor::heap(self.heaps[0] as This).start;
            let slots = crate::heap_find::slots_in(heap);
            if !slots.is_empty() {
                hits.push(format!(
                    "by_index from heap {heap:#x}, which holds found slots {slots:?}"
                ));
            }
        }
        if hits.is_empty() {
            return;
        }
        let mut head = format!(
            "list={:#x} {}",
            std::ptr::from_ref(list) as usize,
            if compute { "dispatch" } else { "draw" }
        );
        for s in &p.stages {
            let name = crate::draw_dump::name(std::sync::Arc::as_ptr(&s.shader) as usize, || {
                s.shader.camera_name()
            });
            head.push_str(&format!(" {}={} ({name})", s.shader.stage, s.shader.hash));
        }
        head.push_str(&format!(" heap_indexed={by_index}"));
        crate::heap_find::log_draw(bind, &head, &hits);
    }
}
