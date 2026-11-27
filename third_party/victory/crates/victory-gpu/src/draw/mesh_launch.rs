//! A single indirect mesh record launched by the card (`VICTORY_MESH_LAUNCH=1`,
//! `crate::launch`): the draw binds the pipeline with the amplification
//! stage, which reads the record where the title's passes wrote it, bounds
//! it and charges the list's budget as the clamp does. No pass of ours runs
//! before the draw (no clamp, no after-writer sanitize).
//!
//! Source: fxceless/victory-fork `b1d777a` (`draw.rs`, the launch in
//! `execute_indirect`).

use std::sync::Arc;
use std::sync::atomic::Ordering;

use super::{DRAWN, State};
use crate::host::HostList;
use crate::launch::{
    LAUNCHED, Launch, Launching, NO_BUDGET, NO_STAGE, UNALIGNED, UNPREPARED, note,
};
use crate::pipeline::host::HostPipeline;
use crate::shadow::Shadow;

impl State {
    /// An indirect mesh call of `max` records (`counted`: with a count
    /// buffer) from `arguments` in `args`, by `p`: `Some(drawn)` if the
    /// launch took it, `None` to run it as before.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn launch_mesh(
        &mut self,
        list: &HostList,
        p: &HostPipeline,
        max: u32,
        counted: bool,
        args: &Arc<Shadow>,
        arguments: u64,
        len: u64,
    ) -> Option<bool> {
        if max != 1 || counted {
            crate::launch::OTHER_CALLS.fetch_add(1, Ordering::Relaxed);
            crate::launch::each_second();
            return None;
        }
        let Some(launch) = p.launch.as_ref() else {
            note(NO_STAGE);
            return None;
        };
        let offset = args.offset(arguments);
        if !offset.is_multiple_of(4) {
            note(UNALIGNED);
            return None;
        }
        if list.budget().is_none() {
            note(NO_BUDGET);
            return None;
        }
        self.window = None;
        args.refresh(list, arguments, len);
        let prepared = {
            let _launching = Launching::start();
            self.prepare_for(list, false, true)
        };
        if !prepared {
            note(UNPREPARED);
            return Some(false);
        }
        #[cfg(windows)]
        self.cost_title(list, "indirect");
        #[cfg(feature = "diagnostics")]
        list.note_op(|| {
            self.op_text(
                format!("indirect kind 10 max 1 at {arguments:#x} launched"),
                false,
            )
        });
        // SAFETY: a live buffer.
        let record = unsafe { args.buffer.GetGPUVirtualAddress() } + offset;
        list.keep(&args.buffer);
        self.mesh_scissor(list, true);
        let launched = crate::launch::record(list, launch.roots, Launch::Record(record));
        self.mesh_scissor(list, false);
        if !launched {
            note(NO_BUDGET);
            return Some(false);
        }
        #[cfg(feature = "diagnostics")]
        self.check_rtv_formats(Some(p), "DispatchMesh launched");
        note(LAUNCHED);
        DRAWN.fetch_add(1, Ordering::Relaxed);
        Some(true)
    }
}
