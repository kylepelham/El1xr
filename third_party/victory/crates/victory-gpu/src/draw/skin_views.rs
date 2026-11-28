//! (`crate::skin_fill`, LNX `0f9a071979`) The views a skin lighting draw reads that the
//! traced passes alone write: the screen probes' radiance atlas and the probes' layers. Both
//! are set to the ambient stand-in's light before the frame's first such draw.

use super::*;
use crate::skin_fill::{ATLAS, C, LAYERS, add};

impl State {
    /// Before a draw of `p`: when one of its stages is skin's lighting and reads both probe
    /// inputs, and its atlas has not been filled this frame, both are filled. Whether it
    /// recorded anything (the compute root signature, pipeline and heaps it set are then set
    /// again by the draw's bind).
    pub(super) fn skin_fill(&mut self, list: &HostList, p: &HostPipeline) -> bool {
        let Some(stage) = p.stages.iter().find(|s| s.shader.skin()) else {
            return false;
        };
        let used = &stage.translated().used;
        let reads = |(slot, offset): (u8, u32)| {
            used.iter().any(|u| {
                u.kind == ResourceType::Srv
                    && u.root == RootType::Table
                    && (u.slot, u.table_offset_dwords) == (slot, offset)
            })
        };
        // Screen-space skin passes read neither: left alone.
        if !reads(ATLAS) || !reads(LAYERS) {
            return false;
        }
        add(C::Draws, 1);
        let view = |(slot, offset): (u8, u32)| match self.roots_ref(false).get(usize::from(slot)) {
            Some(&Root::Table(table)) => {
                match crate::descriptor::view(table + u64::from(offset) * 4) {
                    Some(View::Shader(r, _)) if r != 0 => Some(r),
                    _ => None,
                }
            }
            _ => None,
        };
        let Some(atlas) = view(ATLAS) else {
            add(C::NoView, 1);
            return false;
        };
        if !crate::skin_fill::first_this_frame(atlas) {
            return false;
        }
        let Some(light) = crate::ambient::skin_light() else {
            add(C::NotReady, 1);
            return false;
        };
        let mut recorded = false;
        if let Some(first_only) = crate::skin_fill::layers() {
            match view(LAYERS) {
                Some(layers) => {
                    recorded |=
                        crate::skin_fill::fill(list, layers as This, true, first_only, &light);
                }
                None => add(C::NoView, 1),
            }
        }
        recorded |= crate::skin_fill::fill(list, atlas as This, false, false, &light);
        if recorded {
            self.host_pipeline = 0;
            self.host_roots = [0; 2];
            self.host_heaps = [usize::MAX; 2];
        }
        recorded
    }
}
