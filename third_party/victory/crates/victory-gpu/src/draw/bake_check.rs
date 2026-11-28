//! `VICTORY_BAKE_CHECK` (`crate::bake_check`): a bake draw's inputs as they stand when it is
//! recorded (after every texture pass of its bind), and a few rows of its targets read back
//! right after it, looked at once its list has run.

use super::*;
use crate::bake_check::{Bake, C, Held, ROW_TEXELS, ROWS, Slot, TARGETS, Target, ViewKind, add};

/// Bytes of the read-back buffer per row: [`ROW_TEXELS`] of the widest format read, 512-aligned.
const ROW_BYTES: u64 = ROW_TEXELS as u64 * 16;
const COPY_SOURCE: u32 = 0x800;
const RENDER_TARGET: u32 = 0x4;

/// A bake draw bound and not drawn yet.
pub(super) struct Pending {
    id: u64,
    frame: u64,
    prog: &'static str,
    /// Its targets are read back.
    check: bool,
}

/// One target's read-back: what to tell of it, its buffer, and per row its offset and texels.
struct Read {
    target: Target,
    buffer: Option<ID3D12Resource>,
    rows: Vec<(u64, u32)>,
}

impl State {
    /// Bake draw `p` is being bound in `list`.
    pub(super) fn bake_check_inputs(&mut self, list: &HostList, p: &HostPipeline) {
        let frame = crate::frame::current();
        let prog = bake_pass::bake_hash(p);
        let (id, check) = crate::bake_check::begin(frame);
        crate::bake_check::bake_in_list(std::ptr::from_ref(list) as usize, id, frame, prog);
        self.bake_check = Some(Pending {
            id,
            frame,
            prog,
            check,
        });
    }

    /// What each input slot of bake draw `p` shows now.
    fn bake_check_slots(&self, p: &HostPipeline, frame: u64) -> Option<Vec<Slot>> {
        let inputs = self.bake_input_slots(p).and_then(|slots| {
            let (heap, host) = self.view_heap()?;
            let heap_id = std::ptr::from_ref(heap) as usize;
            Some(
                slots
                    .iter()
                    .map(|&slot| self.bake_check_slot(heap, host, heap_id, slot, frame))
                    .collect::<Vec<_>>(),
            )
        });
        match &inputs {
            Some(slots) => crate::bake_check::inputs_seen(slots),
            None => add(C::Unread, 1),
        }
        inputs
    }

    pub(super) fn bake_check_slot(
        &self,
        heap: &crate::descriptor::Heap,
        host: &crate::descriptor::host::HostHeap,
        heap_id: usize,
        slot: u32,
        frame: u64,
    ) -> Slot {
        let mut s = Slot {
            slot,
            ..Slot::default()
        };
        if let Some(handle) = heap.handle(slot) {
            s.title_key = crate::descriptor::host::title_key(handle);
            match crate::descriptor::view(handle) {
                Some(View::Placed(false, resource, desc)) => {
                    s.view = ViewKind::Placed;
                    if let Some((address, len, shape)) = bake_pass::placed(heap, slot) {
                        s.address = address;
                        s.len = len;
                        s.mapped = bake_pass::mapped(address, len);
                        s.shape = shape;
                    } else {
                        s.address = u64::from(desc[10]) | u64::from(desc[11]) << 32;
                        s.shape = crate::bake_why::shape(&resource);
                    }
                }
                Some(View::Shader(object, _)) => {
                    s.view = ViewKind::Object;
                    s.address = object as u64;
                }
                Some(_) => s.view = ViewKind::Other,
                None => {}
            }
        }
        (s.made, s.queued) = host.slot_check(slot, frame);
        s.held = if self
            .pending_shows
            .iter()
            .any(|&(h, i, _, _)| h == heap_id && i == slot)
        {
            Held::Here
        } else if crate::bake_publish::is_held(heap_id, slot) {
            Held::Other
        } else {
            Held::No
        };
        s
    }

    /// A draw was recorded (`ran`) or dropped: a bake draw's targets are read back; a draw of
    /// another program into a bake result writes over it.
    pub(super) fn bake_check_drawn(&mut self, list: &HostList, ran: bool) {
        let pending = self.bake_check.take();
        let p = crate::pipeline::host_pipeline(self.pipeline as This);
        if !p.is_some_and(|p| p.is_bake) {
            if ran {
                crate::bake_check::drawn(&self.targets);
            }
            return;
        }
        if !ran {
            let prog = p.map_or("?", bake_pass::bake_hash);
            let why = if pending.is_some() {
                "after_bind"
            } else {
                "before_bind"
            };
            crate::bake_check::dropped(crate::frame::current(), prog, why);
            return;
        }
        let (Some(Pending { id, frame, prog, check }), Some(p)) = (pending, p) else {
            return;
        };
        let bake = Bake {
            id,
            frame,
            prog,
            inputs: self.bake_check_slots(p, frame),
        };
        if !check {
            crate::bake_check::unchecked(&bake);
            return;
        }
        let reads = self.bake_check_record(list);
        if reads.iter().all(|r| r.buffer.is_none()) {
            crate::bake_check::unchecked(&bake);
            return;
        }
        add(C::Checked, 1);
        list.defer_after(move || {
            let targets: Vec<Target> = reads.iter().map(read_out).collect();
            crate::bake_check::landed(&bake, &targets);
        });
    }

    /// Records the copies of a few rows of each target, inside the viewport, into read-back
    /// buffers of ours, right after the draw.
    fn bake_check_record(&self, list: &HostList) -> Vec<Read> {
        let Some(host) = crate::host::host() else {
            return Vec::new();
        };
        let viewport = self.viewports.first().map_or([0; 4], |v| {
            [v.TopLeftX, v.TopLeftY, v.Width, v.Height].map(|f| f.max(0.0) as u32)
        });
        let mut reads = Vec::new();
        for (index, &resource) in self.targets.iter().enumerate().take(TARGETS) {
            let Some(copy) = crate::resource::host_copy(resource as This) else {
                continue;
            };
            // SAFETY: a live texture.
            let desc = unsafe { copy.texture.GetDesc() };
            if desc.Dimension != D3D12_RESOURCE_DIMENSION_TEXTURE2D || desc.SampleDesc.Count > 1 {
                continue;
            }
            let format = desc.Format.0 as u32;
            let size = [desc.Width as u32, desc.Height];
            let mut read = Read {
                target: Target {
                    index,
                    resource,
                    size,
                    format,
                    rect: crate::bake_check::clip(viewport, size),
                    out: None,
                },
                buffer: None,
                rows: Vec::new(),
            };
            let rows = crate::bake_check::rows(viewport, size);
            let (Some(texel), false) = (crate::bake_check::texel_bytes(format), rows.is_empty())
            else {
                reads.push(read);
                continue;
            };
            let Some(buffer) = read_back_buffer(&host.device, u64::from(ROWS) * ROW_BYTES) else {
                reads.push(read);
                continue;
            };
            crate::list::ensure_held_state(list, &copy, COPY_SOURCE);
            for (k, &(x, y, n)) in rows.iter().enumerate() {
                let offset = k as u64 * ROW_BYTES;
                let to = D3D12_TEXTURE_COPY_LOCATION {
                    // SAFETY: a borrowed pointer the location never releases.
                    pResource: unsafe { std::mem::transmute_copy(&buffer) },
                    Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                    Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                        PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                            Offset: offset,
                            Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                                Format: desc.Format,
                                Width: n,
                                Height: 1,
                                Depth: 1,
                                RowPitch: (n * texel).next_multiple_of(256),
                            },
                        },
                    },
                };
                let from = D3D12_TEXTURE_COPY_LOCATION {
                    // SAFETY: as above.
                    pResource: unsafe { std::mem::transmute_copy(&copy.texture) },
                    Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                    Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                        SubresourceIndex: 0,
                    },
                };
                let area = D3D12_BOX {
                    left: x,
                    top: y,
                    front: 0,
                    right: x + n,
                    bottom: y + 1,
                    back: 1,
                };
                // SAFETY: an open list, live resources, a box inside the texture.
                unsafe {
                    list.list
                        .CopyTextureRegion(&to, 0, 0, 0, &from, Some(&area))
                };
                read.rows.push((offset, n));
            }
            crate::list::ensure_held_state(list, &copy, RENDER_TARGET);
            read.buffer = Some(buffer);
            reads.push(read);
        }
        reads
    }
}

/// A committed read-back buffer of `bytes`.
fn read_back_buffer(device: &ID3D12Device, bytes: u64) -> Option<ID3D12Resource> {
    let heap = D3D12_HEAP_PROPERTIES {
        Type: D3D12_HEAP_TYPE_READBACK,
        ..Default::default()
    };
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: bytes,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        ..Default::default()
    };
    let mut buffer: Option<ID3D12Resource> = None;
    // SAFETY: a committed buffer.
    unsafe {
        device
            .CreateCommittedResource(
                &heap,
                D3D12_HEAP_FLAG_NONE,
                &desc,
                D3D12_RESOURCE_STATE_COPY_DEST,
                None,
                &mut buffer,
            )
            .ok()?;
    }
    buffer
}

/// What a read-back showed, once its list has run.
fn read_out(read: &Read) -> Target {
    let t = &read.target;
    let mut target = Target {
        index: t.index,
        resource: t.resource,
        size: t.size,
        format: t.format,
        rect: t.rect,
        out: None,
    };
    let (Some(buffer), Some(texel)) = (&read.buffer, crate::bake_check::texel_bytes(t.format))
    else {
        return target;
    };
    let mut data = std::ptr::null_mut();
    // SAFETY: a read-back buffer of ours the list's copies wrote, mapped whole while read.
    unsafe {
        if buffer.Map(0, None, Some(&mut data)).is_err() || data.is_null() {
            return target;
        }
        let bytes =
            std::slice::from_raw_parts(data.cast::<u8>(), (u64::from(ROWS) * ROW_BYTES) as usize);
        let texels: Vec<[u8; 4]> = read
            .rows
            .iter()
            .flat_map(|&(offset, n)| {
                (0..n as usize).filter_map(move |i| {
                    let at = offset as usize + i * texel as usize;
                    crate::bake_check::rgba(t.format, bytes.get(at..at + texel as usize)?)
                })
            })
            .collect();
        buffer.Unmap(0, None);
        target.out = Some(crate::bake_check::judge(&texels));
    }
    target
}
