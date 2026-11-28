//! `VICTORY_HEAP_FIND` (`crate::heap_find`): the bindings of a bind of a
//! walked frame that hold the memory looked for, and (`bink`) the Bink
//! draws whose render target is followed. Reads descriptors only; nothing
//! recorded changes.

use victory_shader::{ResourceType, RootType};

use super::{HostList, Root, State};
use crate::descriptor::View;
use crate::object::This;
use crate::pipeline::host::HostPipeline;

/// (round 10, `VICTORY_HEAP_FIND_ENTRY=1`) A buffer word found naming
/// slot `v` (`what`) becomes a watched entry; the text for its line: the
/// raw word when its flag bits differ from the slot's number.
fn entry_found(start: u64, len: u64, at: usize, v: u32, what: &str) -> String {
    use crate::heap_find::entry;
    if !entry::on() {
        return String::new();
    }
    let Some(slot) = what
        .strip_prefix("slot ")
        .and_then(|w| w.split(' ').next())
        .and_then(|n| n.parse::<u32>().ok())
    else {
        return String::new();
    };
    entry::found(start, len, at, slot, what);
    match entry::read_words(start + 4 * at as u64, 4).and_then(|w| w.first().copied()) {
        Some(raw) if raw != v => format!(" raw {raw:#x} (flags in bits 24-31 the shader masks off)"),
        _ => String::new(),
    }
}

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
    /// `p`: the host pipeline (`None` for a dropped draw: no buffer views
    /// are known then).
    fn heap_find_refs(&self, p: Option<&HostPipeline>, roots: &[Root], hits: &mut Vec<String>) {
        use crate::heap_find::{MOST_SCAN, filter, near, search, search_memory, slots_in, wanted};
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
        let filter = filter(heap.start, &slots);
        for (i, r) in roots.iter().enumerate() {
            match r {
                Root::Constants(c) => {
                    if let Some((at, v, what)) = search(c, &wanted) {
                        hits.push(format!("ref root{i} constants dword {at} = {v:#x} ({what})"));
                    }
                }
                &Root::View(a) => {
                    if let Some((at, v, what)) = search_memory(a, MOST_SCAN, &filter, &wanted) {
                        hits.push(format!(
                            "ref root{i} view at {a:#x} dword {at} = {v:#x} ({what}){}",
                            entry_found(a, MOST_SCAN, at, v, &what)
                        ));
                    }
                }
                _ => {}
            }
        }
        if let Some(p) = p
            && crate::heap_find::entry::on()
        {
            for (label, start, len) in self.heap_bound_buffers(p, roots, &heap) {
                if let Some((at, v, what)) = search_memory(start, len, &filter, &wanted) {
                    hits.push(format!(
                        "ref global {label} buffer {start:#x}+{len:#x} dword {at} = {v:#x} ({what}){}",
                        entry_found(start, len, at, v, &what)
                    ));
                }
            }
        }
        for stage in p.iter().flat_map(|p| p.stages.iter()) {
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
                if let Some((at, v, what)) = search_memory(start, len, &filter, &wanted) {
                    hits.push(format!(
                        "ref {} {:?} s{}+{} buffer {start:#x}+{len:#x} dword {at} = {v:#x} ({what}){}",
                        stage.shader.stage,
                        u.kind,
                        u.slot,
                        u.table_offset_dwords,
                        entry_found(start, len, at, v, &what)
                    ));
                }
            }
        }
    }

    /// (round 11, `VICTORY_HEAP_FIND_ENTRY=1`) The buffers a forward
    /// material shader takes from the heap (`crate::heap_find::entry`):
    /// for each heap byte offset in its `b0` dwords 8 and 9 first seen
    /// this frame, the buffer views of the slots from it (label, start,
    /// bytes).
    fn heap_bound_buffers(
        &self,
        p: &HostPipeline,
        roots: &[Root],
        heap: &crate::descriptor::Heap,
    ) -> Vec<(String, u64, u64)> {
        use crate::heap_find::entry::{BASE_DWORDS, base_slots, first_base, read_words};
        let frame = crate::frame::current();
        let mut out = Vec::new();
        for stage in &p.stages {
            for u in stage.translated().used.iter() {
                if u.kind != ResourceType::Cbv || u.lower_bound != 0 {
                    continue;
                }
                let at = match (u.root, roots.get(usize::from(u.slot))) {
                    (RootType::Table, Some(&Root::Table(table))) => {
                        match crate::descriptor::view(table + u64::from(u.table_offset_dwords) * 4) {
                            Some(View::Constant(a, _)) => a,
                            _ => continue,
                        }
                    }
                    (RootType::Descriptor, Some(&Root::View(a))) => a,
                    _ => continue,
                };
                let Some(words) = read_words(at, 4 * (BASE_DWORDS[1] as u64 + 1)) else {
                    continue;
                };
                for d in BASE_DWORDS {
                    let base = words[d];
                    if base == 0 || !first_base(frame, base) {
                        continue;
                    }
                    for slot in base_slots(base, u32::MAX) {
                        let handle = heap.start + u64::from(slot) * crate::heap_find::SLOT_BYTES;
                        if heap.index(handle) != Some(slot) {
                            break;
                        }
                        if let Some((start, len)) = crate::descriptor::view(handle).as_ref().and_then(buffer_memory) {
                            let k = u64::from(slot) * crate::heap_find::SLOT_BYTES - u64::from(base);
                            out.push((format!("b0 dword {d} base {base:#x} +{k} slot {slot}"), start, len));
                        }
                    }
                }
                return out;
            }
        }
        out
    }

    /// (round 10, `VICTORY_HEAP_FIND_ENTRY=1`) A bind whose buffer views
    /// hold a watched entry is a reader; one whose root constants, root
    /// views or constant buffers hold a number just before the entry (the
    /// start of the record holding it) is named: `entry reader`.
    fn heap_find_entry(&self, p: &HostPipeline, roots: &[Root], hits: &mut Vec<String>) {
        let entries = crate::heap_find::entry::entries();
        if entries.is_empty() {
            return;
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
                let Some(e) = entries.iter().find(|e| e.address >= start && e.address < start + len) else {
                    continue;
                };
                let off = e.address - start;
                let near = self.entry_near(p, roots, off);
                crate::heap_find::entry::reader(near.is_some());
                if let Some(near) = near {
                    hits.push(format!(
                        "entry reader {} {:?} s{}+{} buffer {start:#x}+{len:#x} entry {:#x} (+{off:#x}): {near}",
                        stage.shader.stage, u.kind, u.slot, u.table_offset_dwords, e.address
                    ));
                }
                return;
            }
        }
    }

    /// The number closest before an entry `off` bytes into its buffer in
    /// the bind's root constants, root views and constant buffers
    /// (`crate::heap_find::entry::nearest`), as text.
    fn entry_near(&self, p: &HostPipeline, roots: &[Root], off: u64) -> Option<String> {
        use crate::heap_find::entry::{MOST_NEAR_SCAN, nearest, read_words};
        let mut best: Option<(u64, String)> = None;
        let mut take = |words: &[u32], place: String| {
            if let Some((at, v, how, d)) = nearest(words, off)
                && best.as_ref().is_none_or(|b| d < b.0)
            {
                best = Some((d, format!("{place} dword {at} = {v:#x} ({how}, {d} bytes before the entry)")));
            }
        };
        for (i, r) in roots.iter().enumerate() {
            match r {
                Root::Constants(c) => take(c, format!("root{i} constants")),
                &Root::View(a) => {
                    if let Some(w) = read_words(a, MOST_NEAR_SCAN) {
                        take(&w, format!("root{i} view at {a:#x}"));
                    }
                }
                _ => {}
            }
        }
        for stage in &p.stages {
            for u in stage.translated().used.iter() {
                if u.root != RootType::Table || u.kind != ResourceType::Cbv {
                    continue;
                }
                let Some(&Root::Table(table)) = roots.get(usize::from(u.slot)) else {
                    continue;
                };
                let handle = table + u64::from(u.table_offset_dwords) * 4;
                if let Some(View::Constant(a, size)) = crate::descriptor::view(handle)
                    && let Some(w) = read_words(a, u64::from(size).min(MOST_NEAR_SCAN))
                {
                    take(&w, format!("{} cbv s{}+{}", stage.shader.stage, u.slot, u.table_offset_dwords));
                }
            }
        }
        best.map(|b| b.1)
    }

    /// (round 9) Whether the bind is searched in the frames after a Bink
    /// write: a dispatch, or a draw whose rt0 is not the target followed
    /// and not tiny (`crate::heap_find::searched_target`).
    fn heap_find_searched(&self, compute: bool, spans: &[(u64, u64)]) -> bool {
        use crate::resource::find_facts::{object_size, object_span};
        if !crate::heap_find::after_write() {
            return false;
        }
        if compute {
            return true;
        }
        let Some(&rt0) = self.targets.first() else {
            return false;
        };
        let rt0 = rt0 as This;
        let Some((start, len)) = object_span(rt0) else {
            return false;
        };
        !crate::heap_find::meets(spans, start, len)
            && object_size(rt0).is_some_and(|(w, h)| crate::heap_find::searched_target(w, h))
    }

    /// (round 8, `VICTORY_HEAP_FIND_REFS`) A draw or dispatch that was not
    /// run (no host pipeline, or dropped later): its tables that reach a
    /// found slot and its root constants and root views that name one, in
    /// the frames after a Bink write. Over a thousand draws a frame are
    /// dropped without a host pipeline and never reach [`Self::heap_find`].
    pub(super) fn heap_find_dropped(&self, compute: bool) {
        if !crate::heap_find::recording()
            || !crate::heap_find::refs()
            || !(crate::heap_find::refs_all() || crate::heap_find::after_write())
        {
            return;
        }
        let roots = if compute {
            &self.compute
        } else {
            &self.graphics
        };
        crate::heap_find::refs_dropped();
        let mut hits = Vec::new();
        self.heap_find_refs(None, roots, &mut hits);
        if hits.is_empty() {
            return;
        }
        let head = format!(
            "dropped {} pipeline={:#x} {} made={}",
            if compute { "dispatch" } else { "draw" },
            self.pipeline,
            crate::pipeline::named_shaders_of(self.pipeline as This),
            crate::pipeline::host_pipeline(self.pipeline as This).is_some()
        );
        crate::heap_find::log_draw(crate::heap_find::next_bind(), &head, &hits);
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
        self.heap_find_entry(p, roots, &mut hits);
        if crate::heap_find::refs() {
            if crate::heap_find::refs_all() || self.heap_find_searched(compute, &spans) {
                crate::heap_find::refs_bind();
                self.heap_find_refs(Some(p), roots, &mut hits);
            } else if crate::heap_find::after_write() {
                crate::heap_find::refs_passed_over();
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
