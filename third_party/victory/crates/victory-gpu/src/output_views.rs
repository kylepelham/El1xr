//! Per-thread CPU descriptor scratch for output-merger calls and clears.
//!
//! D3D12 copies CPU-only RTV/DSV descriptors during recording (`OMSetRenderTargets`,
//! `ClearRenderTargetView`, `ClearDepthStencilView`). Keeping a small private
//! scratch heap per recording thread eliminates the race where another worker
//! wraps a shared ring and overwrites a descriptor before the first thread
//! records its command.
//!
//! Ported from Peppacodes/playablegames-peppa (`0e6c0b9aef`, `b4aed925e3`,
//! `90f71c2022`), GPL-3.0.

use windows::Win32::Graphics::Direct3D12::*;
use windows::core::Interface;

use crate::host::fail;

struct OutputHeaps {
    device_key: usize,
    colors: OutputHeap,
    depth: OutputHeap,
}

impl OutputHeaps {
    fn new(device: &ID3D12Device) -> Self {
        Self {
            device_key: device.as_raw() as usize,
            colors: OutputHeap::new(
                device,
                D3D12_DESCRIPTOR_HEAP_TYPE_RTV,
                D3D12_SIMULTANEOUS_RENDER_TARGET_COUNT,
            ),
            depth: OutputHeap::new(device, D3D12_DESCRIPTOR_HEAP_TYPE_DSV, 1),
        }
    }
}

struct OutputHeap {
    _heap: ID3D12DescriptorHeap,
    start: usize,
    step: usize,
}

impl OutputHeap {
    fn new(device: &ID3D12Device, kind: D3D12_DESCRIPTOR_HEAP_TYPE, slots: u32) -> Self {
        // SAFETY: a CPU-only heap description and a live device.
        unsafe {
            let heap: ID3D12DescriptorHeap = device
                .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: kind,
                    NumDescriptors: slots,
                    ..Default::default()
                })
                .unwrap_or_else(|e| fail(format_args!("output descriptor heap: {e}")));
            Self {
                start: heap.GetCPUDescriptorHandleForHeapStart().ptr,
                step: device.GetDescriptorHandleIncrementSize(kind) as usize,
                _heap: heap,
            }
        }
    }

    fn slot(&self, index: u32) -> D3D12_CPU_DESCRIPTOR_HANDLE {
        D3D12_CPU_DESCRIPTOR_HANDLE {
            ptr: self.start + index as usize * self.step,
        }
    }
}

pub(crate) struct OutputViews<'a> {
    device: &'a ID3D12Device,
    heaps: &'a OutputHeaps,
    next_rtv: u32,
    has_dsv: bool,
}

impl OutputViews<'_> {
    /// A host render target view, valid within the recording scope.
    pub(crate) fn rtv(
        &mut self,
        resource: Option<&ID3D12Resource>,
        desc: Option<&D3D12_RENDER_TARGET_VIEW_DESC>,
    ) -> D3D12_CPU_DESCRIPTOR_HANDLE {
        assert!(self.next_rtv < D3D12_SIMULTANEOUS_RENDER_TARGET_COUNT);
        let slot = self.heaps.colors.slot(self.next_rtv);
        self.next_rtv += 1;
        // SAFETY: this thread owns the slot and the resource is live, or the
        // description supplies the type of a null view.
        unsafe {
            self.device
                .CreateRenderTargetView(resource, desc.map(std::ptr::from_ref), slot)
        };
        slot
    }

    /// A host depth view, valid within the recording scope.
    pub(crate) fn dsv(
        &mut self,
        resource: &ID3D12Resource,
        desc: Option<&D3D12_DEPTH_STENCIL_VIEW_DESC>,
    ) -> D3D12_CPU_DESCRIPTOR_HANDLE {
        assert!(!std::mem::replace(&mut self.has_dsv, true));
        let slot = self.heaps.depth.slot(0);
        // SAFETY: this thread owns the slot and the resource is live.
        unsafe {
            self.device
                .CreateDepthStencilView(resource, desc.map(std::ptr::from_ref), slot)
        };
        slot
    }
}

/// Prepare and record the CPU descriptors for one output-merger call or clear.
/// D3D12 copies these descriptors during recording. Each thread owns its
/// scratch heaps through the call, even if it is preempted before recording.
pub(crate) fn with_output_views(device: &ID3D12Device, record: impl FnOnce(&mut OutputViews<'_>)) {
    thread_local! {
        static HEAPS: std::cell::RefCell<Option<OutputHeaps>> = const { std::cell::RefCell::new(None) };
    }
    let key = device.as_raw() as usize;
    HEAPS.with(|heaps| {
        let mut heaps = heaps.borrow_mut();
        if heaps.as_ref().is_none_or(|h| h.device_key != key) {
            *heaps = Some(OutputHeaps::new(device));
        }
        let heaps = heaps.as_ref().expect("output heaps initialized");
        record(&mut OutputViews {
            device,
            heaps,
            next_rtv: 0,
            has_dsv: false,
        });
    });
}

#[cfg(test)]
mod tests {
    use windows::Win32::Graphics::Direct3D12::*;
    use windows::Win32::Graphics::Dxgi::Common::*;

    use crate::host::{host, transition};

    #[test]
    fn another_worker_cannot_retarget_a_pending_clear() {
        let Some(host) = host() else {
            eprintln!("no D3D12 device; pending output clear test skipped");
            return;
        };
        let make = |format, flags, state| {
            let heap = D3D12_HEAP_PROPERTIES {
                Type: D3D12_HEAP_TYPE_DEFAULT,
                ..Default::default()
            };
            let desc = D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
                Width: 1,
                Height: 1,
                DepthOrArraySize: 1,
                MipLevels: 1,
                Format: format,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Flags: flags,
                ..Default::default()
            };
            let mut texture = None;
            // SAFETY: a valid texture description and a live output pointer.
            unsafe {
                host.device.CreateCommittedResource(
                    &heap,
                    D3D12_HEAP_FLAG_NONE,
                    &desc,
                    state,
                    None,
                    &mut texture,
                )
            }
            .unwrap();
            texture.unwrap()
        };
        let color = || {
            make(
                DXGI_FORMAT_R8G8B8A8_UNORM,
                D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
                D3D12_RESOURCE_STATE_RENDER_TARGET,
            )
        };
        let depth = || {
            make(
                DXGI_FORMAT_D32_FLOAT,
                D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL,
                D3D12_RESOURCE_STATE_DEPTH_WRITE,
            )
        };
        let [first_color, second_color] = [color(), color()];
        let [first_depth, second_depth] = [depth(), depth()];
        let list = host.new_list().unwrap();
        for (color, depth) in [(&first_color, &first_depth), (&second_color, &second_depth)] {
            host.output_views(|views| {
                let rtv = views.rtv(Some(color), None);
                let dsv = views.dsv(depth, None);
                // SAFETY: the list is open and the textures are in their clear states.
                unsafe {
                    list.list.ClearRenderTargetView(rtv, &[0.0; 4], None);
                    list.list
                        .ClearDepthStencilView(dsv, D3D12_CLEAR_FLAG_DEPTH, 1.0, 0, None);
                }
            });
        }
        host.output_views(|views| {
            let rtv = views.rtv(Some(&first_color), None);
            let dsv = views.dsv(&first_depth, None);
            // Hold this worker between descriptor creation and recording while
            // another worker allocates more than an entire old process-wide ring.
            std::thread::scope(|scope| {
                scope
                    .spawn(|| {
                        for _ in 0..1024 {
                            host.output_views(|views| {
                                views.rtv(Some(&second_color), None);
                                views.dsv(&second_depth, None);
                            });
                        }
                    })
                    .join()
                    .unwrap();
            });
            // SAFETY: the list is open and both pairs of textures are still alive.
            unsafe {
                list.list
                    .ClearRenderTargetView(rtv, &[1.0, 0.0, 0.0, 0.0], None);
                list.list
                    .ClearDepthStencilView(dsv, D3D12_CLEAR_FLAG_DEPTH, 0.25, 0, None);
            }
        });

        let readback = host.read_back_buffer(2048).unwrap();
        let textures = [
            (&first_color, D3D12_RESOURCE_STATE_RENDER_TARGET),
            (&second_color, D3D12_RESOURCE_STATE_RENDER_TARGET),
            (&first_depth, D3D12_RESOURCE_STATE_DEPTH_WRITE),
            (&second_depth, D3D12_RESOURCE_STATE_DEPTH_WRITE),
        ];
        for (i, (texture, before)) in textures.into_iter().enumerate() {
            let mut footprint = D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default();
            // SAFETY: an open list, live textures, and disjoint readback footprints.
            unsafe {
                host.device.GetCopyableFootprints(
                    &texture.GetDesc(),
                    0,
                    1,
                    512 * i as u64,
                    Some(&mut footprint),
                    None,
                    None,
                    None,
                );
                let src = D3D12_TEXTURE_COPY_LOCATION {
                    pResource: std::mem::transmute_copy(texture),
                    Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                    Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                        SubresourceIndex: 0,
                    },
                };
                let dst = D3D12_TEXTURE_COPY_LOCATION {
                    pResource: std::mem::transmute_copy(&readback),
                    Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                    Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                        PlacedFootprint: footprint,
                    },
                };
                list.list.ResourceBarrier(&[transition(
                    texture,
                    before,
                    D3D12_RESOURCE_STATE_COPY_SOURCE,
                )]);
                list.list.CopyTextureRegion(&dst, 0, 0, 0, &src, None);
            }
        }
        list.close();
        host.execute(&[&list]);
        let mut mapped = std::ptr::null_mut();
        // SAFETY: the GPU finished writing the 2048-byte readback buffer.
        let pixels: [u32; 4] = unsafe {
            readback
                .Map(0, Some(&D3D12_RANGE { Begin: 0, End: 2048 }), Some(&mut mapped))
                .unwrap();
            let values = std::array::from_fn(|i| mapped.cast::<u32>().add(128 * i).read());
            readback.Unmap(0, Some(&D3D12_RANGE { Begin: 0, End: 0 }));
            values
        };
        assert_eq!(
            pixels,
            [255, 0, 0.25f32.to_bits(), 1.0f32.to_bits()],
            "pending clears reached the wrong textures"
        );
        eprintln!(
            "both pending clears reached their own textures after 1024 descriptors on another recording thread"
        );
    }

    #[test]
    fn output_views_scope_allocates_eight_distinct_rtv_slots_and_resets() {
        let Some(host) = host() else {
            return;
        };
        let null_desc = D3D12_RENDER_TARGET_VIEW_DESC {
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            ViewDimension: D3D12_RTV_DIMENSION_TEXTURE2D,
            ..Default::default()
        };
        let mut first_pass = Vec::new();
        host.output_views(|views| {
            for _ in 0..D3D12_SIMULTANEOUS_RENDER_TARGET_COUNT {
                first_pass.push(views.rtv(None, Some(&null_desc)).ptr);
            }
        });
        assert_eq!(first_pass.len(), D3D12_SIMULTANEOUS_RENDER_TARGET_COUNT as usize);
        for pair in first_pass.windows(2) {
            assert!(pair[1] > pair[0], "each RTV slot in a scope must be distinct");
        }
        let mut second_first = 0;
        host.output_views(|views| {
            second_first = views.rtv(None, Some(&null_desc)).ptr;
        });
        assert_eq!(second_first, first_pass[0], "next scope reuses slot 0 on same thread");
    }
}
