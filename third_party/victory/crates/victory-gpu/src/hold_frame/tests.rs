use super::*;
use windows::Win32::Foundation::{CloseHandle, HANDLE, RECT, WAIT_OBJECT_0};
use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

#[test]
fn the_switch_is_off_unless_a_number() {
    assert_eq!(parse(None), 0);
    assert_eq!(parse(Some("0")), 0);
    assert_eq!(parse(Some("on")), 0);
    assert_eq!(parse(Some("1")), MOST_DEFAULT);
    assert_eq!(parse(Some(" 12 ")), 12);
}

#[test]
fn only_float_readable_formats_are_scanned() {
    assert_eq!(
        view_format(DXGI_FORMAT_R8G8B8A8_TYPELESS),
        Some(DXGI_FORMAT_R8G8B8A8_UNORM)
    );
    assert_eq!(
        view_format(DXGI_FORMAT_B8G8R8A8_TYPELESS),
        Some(DXGI_FORMAT_B8G8R8A8_UNORM)
    );
    assert_eq!(
        view_format(DXGI_FORMAT_R10G10B10A2_UNORM),
        Some(DXGI_FORMAT_R10G10B10A2_UNORM)
    );
    assert_eq!(
        view_format(DXGI_FORMAT_R16G16B16A16_TYPELESS),
        Some(DXGI_FORMAT_R16G16B16A16_FLOAT)
    );
    assert_eq!(view_format(DXGI_FORMAT_R8G8B8A8_UINT), None);
    assert_eq!(view_format(DXGI_FORMAT_R16G16B16A16_UINT), None);
}

const W: u32 = 40;
const H: u32 = 24;
const FORMAT: DXGI_FORMAT = DXGI_FORMAT_R8G8B8A8_UNORM;

struct Rig {
    device: ID3D12Device,
    queue: ID3D12CommandQueue,
    allocator: ID3D12CommandAllocator,
    list: ID3D12GraphicsCommandList,
    fence: ID3D12Fence,
    event: HANDLE,
    submitted: u64,
    targets: ID3D12DescriptorHeap,
    _serial: std::sync::MutexGuard<'static, ()>,
}

impl Drop for Rig {
    fn drop(&mut self) {
        // SAFETY: our own event.
        unsafe {
            let _ = CloseHandle(self.event);
        }
    }
}

impl Rig {
    fn new() -> Option<Rig> {
        // One WARP user at a time (#66).
        let serial = crate::indirect::tests::WARP_SERIAL
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // SAFETY: plain object creation on a device of our own.
        unsafe {
            let adapter = crate::host::warp_adapter().ok()?;
            let mut device: Option<ID3D12Device> = None;
            D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_12_0, &mut device).ok()?;
            let device = device?;
            let queue: ID3D12CommandQueue = device
                .CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                    Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
                    ..Default::default()
                })
                .ok()?;
            let allocator: ID3D12CommandAllocator = device
                .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
                .ok()?;
            let list: ID3D12GraphicsCommandList = device
                .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)
                .ok()?;
            let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE).ok()?;
            let event = CreateEventW(None, false, false, None).ok()?;
            let targets: ID3D12DescriptorHeap = device
                .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: D3D12_DESCRIPTOR_HEAP_TYPE_RTV,
                    NumDescriptors: 1,
                    Flags: D3D12_DESCRIPTOR_HEAP_FLAG_NONE,
                    NodeMask: 0,
                })
                .ok()?;
            Some(Rig {
                device,
                queue,
                allocator,
                list,
                fence,
                event,
                submitted: 0,
                targets,
                _serial: serial,
            })
        }
    }

    fn run(&mut self) {
        self.submitted += 1;
        // SAFETY: our list, queue and fence; the allocator is reset only
        // after the device finished.
        unsafe {
            self.list.Close().unwrap();
            let run: ID3D12CommandList = windows::core::Interface::cast(&self.list).unwrap();
            self.queue.ExecuteCommandLists(&[Some(run)]);
            self.queue.Signal(&self.fence, self.submitted).unwrap();
            self.fence
                .SetEventOnCompletion(self.submitted, self.event)
                .unwrap();
            assert_eq!(WaitForSingleObject(self.event, 60_000), WAIT_OBJECT_0);
            let removed = self.device.GetDeviceRemovedReason();
            assert!(removed.is_ok(), "device lost: {removed:?}");
            self.allocator.Reset().unwrap();
            self.list.Reset(&self.allocator, None).unwrap();
        }
    }

    /// A texture of the test's size, in [`READ`] if `shown` (else a copy
    /// destination): black, with `lit` filled with `colour`.
    fn texture(&mut self, shown: bool, lit: Option<(RECT, [f32; 4])>) -> ID3D12Resource {
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Width: u64::from(W),
            Height: H,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: FORMAT,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Flags: D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
            ..Default::default()
        };
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_DEFAULT,
            ..Default::default()
        };
        let mut texture: Option<ID3D12Resource> = None;
        // SAFETY: our device, heap and list.
        unsafe {
            self.device
                .CreateCommittedResource(
                    &heap,
                    D3D12_HEAP_FLAG_NONE,
                    &desc,
                    D3D12_RESOURCE_STATE_RENDER_TARGET,
                    None,
                    &mut texture,
                )
                .unwrap();
            let texture = texture.unwrap();
            let target = self.targets.GetCPUDescriptorHandleForHeapStart();
            self.device.CreateRenderTargetView(&texture, None, target);
            self.list
                .ClearRenderTargetView(target, &[0.0, 0.0, 0.0, 1.0], None);
            if let Some((rect, colour)) = lit {
                self.list
                    .ClearRenderTargetView(target, &colour, Some(&[rect]));
            }
            let after = if shown {
                READ
            } else {
                D3D12_RESOURCE_STATE_COPY_DEST
            };
            self.list.ResourceBarrier(&[transition(
                &texture,
                D3D12_RESOURCE_STATE_RENDER_TARGET,
                after,
            )]);
            self.run();
            texture
        }
    }

    /// The texels of `back` (a copy destination) at (0, 0) and at the far
    /// corner, as RGBA bytes.
    fn shown(&mut self, back: &ID3D12Resource) -> [[u8; 4]; 2] {
        // Each texel at its own 512-byte-aligned place.
        const STRIDE: u64 = 512;
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_READBACK,
            ..Default::default()
        };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: 2 * STRIDE,
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            ..Default::default()
        };
        let mut readback: Option<ID3D12Resource> = None;
        // SAFETY: our device, list and live resources.
        unsafe {
            self.device
                .CreateCommittedResource(
                    &heap,
                    D3D12_HEAP_FLAG_NONE,
                    &desc,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                    None,
                    &mut readback,
                )
                .unwrap();
            let readback = readback.unwrap();
            self.list.ResourceBarrier(&[transition(
                back,
                D3D12_RESOURCE_STATE_COPY_DEST,
                D3D12_RESOURCE_STATE_COPY_SOURCE,
            )]);
            for (i, (x, y)) in [(0, 0), (W - 1, H - 1)].into_iter().enumerate() {
                let to = D3D12_TEXTURE_COPY_LOCATION {
                    pResource: std::mem::transmute_copy(&readback),
                    Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                    Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                        PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                            Offset: i as u64 * STRIDE,
                            Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                                Format: FORMAT,
                                Width: 1,
                                Height: 1,
                                Depth: 1,
                                RowPitch: 256,
                            },
                        },
                    },
                };
                let from = D3D12_TEXTURE_COPY_LOCATION {
                    pResource: std::mem::transmute_copy(back),
                    Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                    Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: 0 },
                };
                let texel = D3D12_BOX {
                    left: x,
                    top: y,
                    front: 0,
                    right: x + 1,
                    bottom: y + 1,
                    back: 1,
                };
                self.list.CopyTextureRegion(&to, 0, 0, 0, &from, Some(&texel));
            }
            self.list.ResourceBarrier(&[transition(
                back,
                D3D12_RESOURCE_STATE_COPY_SOURCE,
                D3D12_RESOURCE_STATE_COPY_DEST,
            )]);
            self.run();
            let mut at = std::ptr::null_mut();
            readback.Map(0, None, Some(&mut at)).unwrap();
            let bytes = std::slice::from_raw_parts(at.cast::<u8>(), (2 * STRIDE) as usize);
            let texel = |i: usize| {
                let s = i * STRIDE as usize;
                [bytes[s], bytes[s + 1], bytes[s + 2], bytes[s + 3]]
            };
            let shown = [texel(0), texel(1)];
            readback.Unmap(0, None);
            shown
        }
    }

    /// Presents `texture` through `hold` into `back`; what `back` shows.
    fn present(&mut self, hold: &mut Hold, texture: &ID3D12Resource, back: &ID3D12Resource) -> [[u8; 4]; 2] {
        let device = self.device.clone();
        let list = self.list.clone();
        let slot = hold
            .record(&device, &list, texture, back, 0)
            .expect("the pass presents this texture");
        self.run();
        hold.sent(slot, 0);
        hold.harvest(0);
        self.shown(back)
    }
}

const BLACK: [u8; 4] = [0, 0, 0, 255];
const RED: [u8; 4] = [255, 0, 0, 255];
const GREEN: [u8; 4] = [0, 255, 0, 255];

/// On the software adapter: a black present before any good frame shows
/// black; after one it shows that frame, up to the most in a row, then
/// black; a frame with one lit texel in its far corner is not black (every
/// texel is read, not a grid) and is the one kept from then on; a frame
/// kept before a present the pass missed is not shown.
#[test]
fn a_black_present_shows_the_last_good_frame() {
    let Some(mut rig) = Rig::new() else {
        eprintln!("no WARP device: skipped");
        return;
    };
    let mut hold = Hold::new(&rig.device, 2).expect("the pass");
    let back = rig.texture(false, None);
    let black = rig.texture(true, None);
    let whole = RECT { left: 0, top: 0, right: W as i32, bottom: H as i32 };
    let red = rig.texture(true, Some((whole, [1.0, 0.0, 0.0, 1.0])));
    let corner = RECT {
        left: W as i32 - 1,
        top: H as i32 - 1,
        right: W as i32,
        bottom: H as i32,
    };
    let speck = rig.texture(true, Some((corner, [0.0, 1.0, 0.0, 1.0])));

    assert_eq!(rig.present(&mut hold, &black, &back), [BLACK, BLACK], "nothing kept yet");
    assert_eq!(rig.present(&mut hold, &red, &back), [RED, RED]);
    assert_eq!(rig.present(&mut hold, &black, &back), [RED, RED], "held once");
    assert_eq!(rig.present(&mut hold, &black, &back), [RED, RED], "held twice");
    assert_eq!(rig.present(&mut hold, &black, &back), [BLACK, BLACK], "past the most in a row");
    assert_eq!(rig.present(&mut hold, &speck, &back), [BLACK, GREEN], "one lit texel is not black");
    assert_eq!(rig.present(&mut hold, &black, &back), [BLACK, GREEN], "the speck is the frame kept");
    // A present went by without the pass: what was kept is not shown.
    hold.stale = true;
    assert_eq!(rig.present(&mut hold, &black, &back), [BLACK, BLACK], "the kept frame is forgotten");
    // Held 3, lit 2, black shown 3.
    assert_eq!(hold.counts(), [3, 2, 3]);
}
