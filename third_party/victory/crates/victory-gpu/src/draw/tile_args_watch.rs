//! `VICTORY_TILE_ARGS_WATCH` (`crate::tile_args_watch`): the first indirect record each skin
//! lighting draw reads, copied on the card right before its `ExecuteIndirect`, and what the draw
//! is bound with.

use super::*;
use std::sync::Arc;

const COPY_SOURCE: D3D12_RESOURCE_STATES = D3D12_RESOURCE_STATE_COPY_SOURCE;
const ARGUMENT: D3D12_RESOURCE_STATES = D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT;

/// Where a draw's record is read from.
pub(super) struct Read<'a> {
    /// `raw`, `sanitized`, `ahead` or `clamped`.
    pub(super) path: &'static str,
    /// The buffer the `ExecuteIndirect` reads and the byte in it.
    pub(super) buffer: (&'a ID3D12Resource, u64),
    /// Our scratch or ahead buffer, left in the indirect-argument state (moved to the copy state
    /// for the copy and back). Otherwise it is the title's host copy, read as is (as
    /// `crate::raw_watch` does).
    pub(super) ours: bool,
}

impl State {
    /// A skin lighting `ExecuteIndirect` of `kind` about to be recorded (pipeline `p`), reading
    /// its records as `read` says, from the title's host copy `args` at title address `arguments`
    /// (and the count in `count`): its first record copied into a read-back buffer of ours,
    /// logged once the list has run (`crate::tile_args_watch::landed`).
    #[allow(clippy::too_many_arguments)]
    pub(super) fn tile_args_watch(
        &self,
        list: &HostList,
        p: &HostPipeline,
        read: Read<'_>,
        args: &Arc<crate::shadow::Shadow>,
        count: Option<(&ID3D12Resource, u64)>,
        (kind, stride, max): (u32, u32, u32),
        arguments: u64,
    ) {
        const DRAW: u32 = 0;
        const INDEXED: u32 = 1;
        if !matches!(kind, DRAW | INDEXED) || !p.stages.iter().any(|s| s.shader.skin()) {
            return;
        }
        let frame = crate::frame::current();
        if !crate::tile_args_watch::this_frame(frame) {
            return;
        }
        let Some(host) = crate::host::host() else {
            return;
        };
        let Some(back) = host.read_back_buffer(crate::tile_args_watch::BYTES) else {
            return;
        };
        let bytes = u64::from(stride.min(20));
        let title_read = !read.ours && std::ptr::eq(read.buffer.0, &args.buffer);
        // SAFETY: an open list; live buffers the list keeps (the read-back buffer by the closure
        // below); our buffer is in the indirect-argument state the clamp left it in, and goes back.
        unsafe {
            if read.ours {
                list.list.ResourceBarrier(&[crate::host::transition(
                    read.buffer.0,
                    ARGUMENT,
                    COPY_SOURCE,
                )]);
            }
            list.list
                .CopyBufferRegion(&back, 0, read.buffer.0, read.buffer.1, bytes);
            if read.ours {
                list.list.ResourceBarrier(&[crate::host::transition(
                    read.buffer.0,
                    COPY_SOURCE,
                    ARGUMENT,
                )]);
            }
            if !title_read {
                list.list.CopyBufferRegion(
                    &back,
                    crate::tile_args_watch::TITLE_AT as u64,
                    &args.buffer,
                    args.offset(arguments),
                    bytes,
                );
            }
            if let Some((c, at)) = count {
                list.list.CopyBufferRegion(
                    &back,
                    crate::tile_args_watch::COUNT_AT as u64,
                    c,
                    at,
                    4,
                );
            }
        }
        let mut cpu = [0u32; 5];
        if args.offset_in(arguments).is_some() {
            for (i, w) in cpu.iter_mut().enumerate().take(bytes as usize / 4) {
                // SAFETY: inside the title buffer the copy covers (a record of `stride` bytes).
                *w = unsafe { ((arguments + 4 * i as u64) as *const u32).read_unaligned() };
            }
        }
        let id = list.ops.id();
        let draw = crate::tile_args_watch::Draw {
            frame,
            list: id,
            index: crate::tile_args_watch::next_index(frame),
            pixel: p
                .stages
                .iter()
                .find(|s| s.shader.stage == "pixel")
                .map_or_else(String::new, |s| s.shader.hash.clone()),
            path: read.path,
            kind,
            max,
            at: arguments,
            cpu,
            rest: self.tile_args_rest(list, args, arguments, bytes, p, frame),
        };
        let counted = count.is_some();
        list.defer_after(move || {
            let mut data = std::ptr::null_mut();
            // SAFETY: a read-back buffer of ours the list's copies wrote, mapped while read.
            let bytes = unsafe {
                if back.Map(0, None, Some(&mut data)).is_err() || data.is_null() {
                    return;
                }
                let b = std::slice::from_raw_parts(
                    data.cast::<u8>(),
                    crate::tile_args_watch::BYTES as usize,
                )
                .to_vec();
                back.Unmap(0, None);
                b
            };
            let w = crate::tile_args_watch::words_of(&bytes, !title_read, counted);
            crate::tile_args_watch::landed(&draw, &w);
        });
    }

    /// The line's tail: the record's host copy (number, whether shaders ever wrote it, in place,
    /// whether shaders of this list or a recent submission wrote the record), the class plane of
    /// the bound depth, the producer passes, the viewport and its depth range, the depth bias.
    fn tile_args_rest(
        &self,
        list: &HostList,
        args: &Arc<crate::shadow::Shadow>,
        arguments: u64,
        bytes: u64,
        p: &HostPipeline,
        frame: u64,
    ) -> String {
        let id = list.ops.id();
        let (at, len) = (arguments as usize, bytes as usize);
        let twin = crate::resource::host_side(self.depth as This)
            .and_then(|(_, twin)| twin)
            .filter(|t| t.plane.is_some())
            .map(|t| Arc::as_ptr(&t) as usize);
        let view = self.viewports.first().map_or_else(
            || "view=none".to_owned(),
            |v| {
                format!(
                    "view={}x{} z={}..{}",
                    v.Width, v.Height, v.MinDepth, v.MaxDepth
                )
            },
        );
        format!(
            "copy=#{} shaders_wrote={} in_place={} written_here={} written_lately={} {} {} {view} {}",
            args.id(),
            args.shaders_wrote(),
            args.in_place(),
            list.gpu_written(at, len),
            crate::host::shader_written(at, len),
            crate::tile_args_watch::plane_at(twin, id, frame),
            crate::tile_args_watch::producers_at(id, frame),
            crate::tile_args_watch::bias_of(&p.state),
        )
    }

    /// After a dispatch that ran: noted when it is `CS_GenerateMaterialMasks` or
    /// `CS_SetMaterialTileIndirectArgs`.
    pub(super) fn tile_args_producer(&self, list: &HostList) {
        if let Some(kind) = self.skin_watch_kind(true) {
            crate::tile_args_watch::producer(kind, list.ops.id());
        }
    }
}
