//! `VICTORY_HEAP_FIND` (`crate::heap_find`): the bindings of a bind of a
//! walked frame that hold the memory looked for, and (`bink`) the Bink
//! draws whose render target is followed. Reads descriptors only; nothing
//! recorded changes.

use victory_shader::{ResourceType, RootType};

use super::{HostList, Root, State};
use crate::descriptor::View;
use crate::object::This;
use crate::pipeline::host::HostPipeline;

/// The title memory a constant or buffer view shows: start and bytes.
fn buffer_memory(view: &View) -> Option<(u64, u64)> {
    const BUFFER: u32 = 1;
    match view {
        View::Constant(address, size) => Some((*address, u64::from(*size))),
        View::Shader(r, Some(d)) if d[1] == BUFFER => {
            let (start, len) = crate::resource::find_facts::object_span(*r as This)?;
            // The buffer union from word 4: first element (two words),
            // elements, stride, flags (raw: 4-byte elements).
            let first = u64::from(d[4]) | u64::from(d[5]) << 32;
            let each = u64::from(if d[7] != 0 { d[7] } else { 4 });
            let skip = first.saturating_mul(each).min(len);
            Some((start + skip, (u64::from(d[6]) * each).min(len - skip)))
        }
        View::Placed(_, resource, desc) if resource[0] == BUFFER => Some((
            u64::from(desc[10]) | u64::from(desc[11]) << 32,
            crate::resource::find_facts::words_bytes(resource)?,
        )),
        _ => None,
    }
}

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

    /// (`VICTORY_HEAP_FIND_REFS=1`) The bind's tables that reach a found
    /// slot, and the words of its root constants, root views and buffer
    /// views that name one (`crate::heap_find::wanted`).
    fn heap_find_refs(&self, p: &HostPipeline, roots: &[Root], hits: &mut Vec<String>) {
        use crate::heap_find::{MOST_SCAN, near, search, search_memory, slots_in, wanted};
        let Some(heap) = self
            .heaps
            .iter()
            .filter(|&&h| h != 0)
            .map(|&h| crate::descriptor::heap(h as This))
            .find(|h| !h.samplers())
        else {
            return;
        };
        let slots = slots_in(heap.start);
        if slots.is_empty() {
            return;
        }
        let tables: Vec<(u8, u32)> = roots
            .iter()
            .enumerate()
            .filter_map(|(i, r)| match *r {
                Root::Table(t) => heap.index(t).map(|x| (i as u8, x)),
                _ => None,
            })
            .collect();
        for (root, t, s) in near(&slots, &tables) {
            hits.push(format!("table{root} at slot {t} reaches found slot {s} at +{}", s - t));
        }
        let wanted = wanted(heap.start, &slots, &tables);
        let key = tables.iter().fold(0u64, |k, &(r, t)| {
            k.rotate_left(7) ^ (u64::from(r) << 32 | u64::from(t))
        });
        for (i, r) in roots.iter().enumerate() {
            match r {
                Root::Constants(c) => {
                    if let Some((at, v, what)) = search(c, &wanted) {
                        hits.push(format!("ref root{i} constants dword {at} = {v:#x} ({what})"));
                    }
                }
                &Root::View(a) => {
                    if let Some((at, v, what)) = search_memory(a, MOST_SCAN, key, &wanted) {
                        hits.push(format!("ref root{i} view at {a:#x} dword {at} = {v:#x} ({what})"));
                    }
                }
                _ => {}
            }
        }
        for stage in &p.stages {
            for u in stage.translated().used.iter() {
                if u.root != RootType::Table || !matches!(u.kind, ResourceType::Cbv | ResourceType::Srv) {
                    continue;
                }
                let Some(&Root::Table(table)) = roots.get(usize::from(u.slot)) else {
                    continue;
                };
                let handle = table + u64::from(u.table_offset_dwords) * 4;
                let Some((start, len)) = crate::descriptor::view(handle).as_ref().and_then(buffer_memory)
                else {
                    continue;
                };
                if let Some((at, v, what)) = search_memory(start, len, key, &wanted) {
                    hits.push(format!(
                        "ref {} {:?} s{}+{} buffer {start:#x}+{len:#x} dword {at} = {v:#x} ({what})",
                        stage.shader.stage, u.kind, u.slot, u.table_offset_dwords
                    ));
                }
            }
        }
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
        if crate::heap_find::refs() {
            self.heap_find_refs(p, roots, &mut hits);
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
