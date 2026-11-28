//! (`crate::zero_out`, voidopcode/vilocity `601d5db6d5`, `29111fec6f`) A dispatch of a program
//! `VICTORY_ZERO_OUT` names clears the views it writes through its tables instead of running.

use super::*;
use crate::zero_out::{INJECTED, Zero};

impl State {
    /// Before a dispatch of `p` (direct or indirect): when `VICTORY_ZERO_OUT` names its
    /// program, every view it writes through its tables is cleared to 0 and the dispatch does
    /// not run; the inject's radiance atlas takes the ambient stand-in's light instead of 0
    /// when the stand-in has one. The inject runs as the title's while its rays find
    /// something. Whether it took the dispatch.
    pub(super) fn zero_out(&mut self, list: &HostList, p: &HostPipeline) -> bool {
        let Some(stage) = p.stages.first() else {
            return false;
        };
        let Some(zero) = stage.shader.zeroed() else {
            return false;
        };
        if zero == Zero::Inject && !crate::ambient::probes_blind() {
            crate::zero_out::ran();
            return false;
        }
        let outputs: Vec<(u64, (u8, u32))> = stage
            .translated()
            .used
            .iter()
            .filter(|u| u.kind == ResourceType::Uav && u.root == RootType::Table)
            .filter_map(|u| match self.roots_ref(true).get(usize::from(u.slot)) {
                Some(&Root::Table(table)) => Some((
                    table + u64::from(u.table_offset_dwords) * 4,
                    (u.slot, u.table_offset_dwords),
                )),
                _ => None,
            })
            .collect();
        let light = (zero == Zero::Inject)
            .then(crate::ambient::skin_light)
            .flatten();
        let mut atlas = None;
        let (mut cleared, mut left) = (0, 0);
        let mut count = |done: bool| {
            if done {
                cleared += 1;
            } else {
                left += 1;
            }
        };
        let clear = |state: &mut Self, gpu: u64, view: &View, resource: Option<This>| {
            crate::list::uav_clear::record(list, state, gpu, view, resource, &[0; 4], &[], false)
        };
        for (gpu, at) in outputs {
            let Some(view) = crate::descriptor::view(gpu) else {
                count(false);
                continue;
            };
            let resource = match &view {
                View::Unordered(r, ..) if *r != 0 => Some(*r as This),
                _ => None,
            };
            // Filled after the clears, not cleared first: two writes would want a barrier.
            if light.is_some()
                && at == INJECTED
                && atlas.is_none()
                && let Some(r) = resource
            {
                atlas = Some((gpu, view, r));
                continue;
            }
            count(clear(self, gpu, &view, resource));
        }
        let mut lit = 0;
        if let Some((gpu, view, r)) = atlas
            && let Some(light) = &light
        {
            if crate::skin_fill::fill(list, r, false, false, light) {
                lit = 1;
                // The fill set its own root signature, pipeline and heap.
                self.host_pipeline = 0;
                self.host_roots = [0; 2];
                self.host_heaps = [usize::MAX; 2];
            } else {
                count(clear(self, gpu, &view, Some(r)));
            }
        }
        crate::zero_out::note(&stage.shader.hash, &p.name, cleared, left, lit);
        true
    }
}
