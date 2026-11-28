//! `VICTORY_EXPOSURE_WATCH` (`crate::exposure_watch`): an exposure or
//! luminance pass's target and the views its heap offsets name, as the
//! host shows them.

use victory_shader::{ResourceType, RootType};

use super::{HostList, Root, State};
use crate::descriptor::View;
use crate::object::This;
use crate::pipeline::host::HostPipeline;

impl State {
    pub(super) fn exposure_watch(&self, list: &HostList, p: &HostPipeline, compute: bool) {
        use crate::exposure_watch::{
            BASE_DWORDS, OFFSETS, first_this_frame, registers, slot, texel, watched,
        };
        let named: Vec<String> = p
            .stages
            .iter()
            .map(|s| {
                let name =
                    crate::draw_dump::name(std::sync::Arc::as_ptr(&s.shader) as usize, || {
                        s.shader.camera_name()
                    });
                format!("{}={} ({name})", s.shader.stage, s.shader.hash)
            })
            .collect();
        if !named.iter().any(|n| watched(n)) {
            return;
        }
        let frame = crate::frame::current();
        if !first_this_frame(frame, std::ptr::from_ref(p) as u64) {
            return;
        }
        let rt0 = if compute {
            None
        } else {
            self.targets.first().copied()
        };
        let mut rt_text = String::from("none");
        if let Some(t) = rt0 {
            let t = t as This;
            rt_text = crate::resource::find_facts::object_text(t);
            // A 1x1 target goes back to title memory when the copy-back
            // takes its kind (`VICTORY_TEXTURE_BACK=1x1f2`).
            list.texture_written(t);
            if let Some((start, _)) = crate::resource::find_facts::object_span(t)
                && let Some(v) = crate::heap_find::entry::read_words(start, 16)
                    .as_deref()
                    .and_then(texel)
            {
                rt_text.push_str(&format!(" rt0_texel={v:?}"));
            }
        }
        let roots = if compute {
            &self.compute
        } else {
            &self.graphics
        };
        let b0 = p.stages.iter().find_map(|s| {
            let u = s
                .translated()
                .used
                .iter()
                .find(|u| u.kind == ResourceType::Cbv && u.lower_bound == 0)?;
            match (u.root, roots.get(usize::from(u.slot))) {
                (RootType::Table, Some(&Root::Table(table))) => {
                    match crate::descriptor::view(table + u64::from(u.table_offset_dwords) * 4) {
                        Some(View::Constant(a, _)) => Some(a),
                        _ => None,
                    }
                }
                (RootType::Descriptor, Some(&Root::View(a))) => Some(a),
                _ => None,
            }
        });
        let words = b0.and_then(|a| crate::heap_find::entry::read_words(a, 48));
        let heap = self
            .heaps
            .iter()
            .filter(|&&h| h != 0)
            .map(|&h| crate::descriptor::heap(h as This))
            .find(|h| !h.samplers());
        let mut slots = Vec::new();
        if let (Some(words), Some(heap)) = (words.as_ref(), heap) {
            let host_count = heap.host().map_or(0, |h| h.count());
            for d in BASE_DWORDS {
                let base = words[d];
                if base == 0 {
                    continue;
                }
                for k in OFFSETS {
                    let s = slot(base, k);
                    let handle = heap.start + u64::from(s) * crate::heap_find::SLOT_BYTES;
                    if heap.index(handle) != Some(s) {
                        slots.push(format!("b0[{d}]+{k} slot {s} past the title heap"));
                        continue;
                    }
                    let state = crate::descriptor::host_slot_state(
                        heap.start,
                        s,
                        crate::descriptor::record_at(handle),
                    );
                    let past = if s >= host_count {
                        " past_host_count"
                    } else {
                        ""
                    };
                    let what = match crate::descriptor::view(handle) {
                        Some(View::Constant(a, size)) => {
                            let w = crate::heap_find::entry::read_words(
                                a,
                                u64::from(size).min(67 * 16),
                            )
                            .unwrap_or_default();
                            let (xs, zero) = registers(&w);
                            format!("cbv {a:#x}+{size:#x} r54-56,r63-66.x={xs:x?} all_zero={zero}")
                        }
                        Some(View::Shader(r, _)) => {
                            format!(
                                "srv {}",
                                crate::resource::find_facts::object_text(r as This)
                            )
                        }
                        Some(_) => "other view".into(),
                        None => "no view".into(),
                    };
                    slots.push(format!("b0[{d}]+{k} slot {s} host={state}{past} {what}"));
                }
            }
        }
                tracing::warn!(
            frame,
            list = format_args!("{:#x}", std::ptr::from_ref(list) as usize),
            shaders = named.join(" "),
            rt0 = rt_text,
            b0 = format_args!("{:x?}", words.as_deref().unwrap_or(&[])),
            slots = slots.join("; "),
            "exposure watch: pass"
        );
    }
}
