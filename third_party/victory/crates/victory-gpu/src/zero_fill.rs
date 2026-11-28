//! (El1xr) Zeros written into a buffer of the card's memory that a pass reads
//! before it writes (a count, a running mean, a flag), once, when it is made.
//!
//! Direct3D 12 says committed resources and heaps are only "almost always"
//! zeroed when made: the memory manager may hand a process memory it freed
//! itself without clearing it. The software adapter did (the card block
//! reuse zeroer flake, `texpool::reuse`); a probe of this PC's card found
//! none in 192 tries (`texpool::zero_probe`). A pass that reads such a
//! buffer first then starts from whatever it holds: the compare's kept-piece
//! count (`host_card`), the ambient stand-in's slow mean, flags and points
//! (`crate::ambient`). Where the memory was zeros, nothing changes.

use windows::Win32::Graphics::Direct3D12::*;
use windows::core::Interface;

/// The most bytes of zeros copied at once.
const CHUNK: u64 = 4 << 20;

/// Writes zeros over the first `bytes` of `buffer` (in the common state,
/// named by no list in flight) on a copy queue of its own, and waits for
/// them: the buffer is zeros for any list sent after this returns.
pub(crate) fn zero_buffer(
    device: &ID3D12Device,
    buffer: &ID3D12Resource,
    bytes: u64,
) -> windows::core::Result<()> {
    if bytes == 0 {
        return Ok(());
    }
    let chunk = bytes.min(CHUNK);
    // SAFETY: plain object creation; the upload buffer is mapped whole and
    // written before the copies are recorded; the list has run when the
    // fence is passed, and the objects live until then.
    unsafe {
        let queue: ID3D12CommandQueue = device.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
            Type: D3D12_COMMAND_LIST_TYPE_COPY,
            ..Default::default()
        })?;
        let allocator: ID3D12CommandAllocator =
            device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_COPY)?;
        let list: ID3D12GraphicsCommandList =
            device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_COPY, &allocator, None)?;
        let mut zeros: Option<ID3D12Resource> = None;
        device.CreateCommittedResource(
            &D3D12_HEAP_PROPERTIES {
                Type: D3D12_HEAP_TYPE_UPLOAD,
                ..Default::default()
            },
            D3D12_HEAP_FLAG_NONE,
            &crate::texpool::buffer_desc(chunk, D3D12_RESOURCE_FLAG_NONE),
            D3D12_RESOURCE_STATE_GENERIC_READ,
            None,
            &mut zeros,
        )?;
        let zeros = zeros.ok_or_else(windows::core::Error::empty)?;
        let mut data = std::ptr::null_mut();
        zeros.Map(0, None, Some(&mut data))?;
        std::ptr::write_bytes(data.cast::<u8>(), 0, chunk as usize);
        zeros.Unmap(0, None);
        let mut at = 0;
        while at < bytes {
            let len = (bytes - at).min(chunk);
            list.CopyBufferRegion(buffer, at, &zeros, 0, len);
            at += len;
        }
        list.Close()?;
        let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE)?;
        queue.ExecuteCommandLists(&[Some(list.cast()?)]);
        queue.Signal(&fence, 1)?;
        while fence.GetCompletedValue() < 1 {
            device.GetDeviceRemovedReason()?;
            std::thread::yield_now();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;

    /// On the software adapter, over a buffer of the card's memory filled
    /// with 0xCD: zeros over the bytes asked for (more than one chunk, an
    /// odd length), the bytes after them left as they were.
    #[test]
    fn zeros_go_over_the_bytes_asked_for_only() {
        let Ok(adapter) = crate::host::warp_adapter() else {
            eprintln!("no WARP adapter: skipped");
            return;
        };
        let mut device: Option<ID3D12Device> = None;
        // SAFETY: the software adapter.
        if unsafe { D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_12_0, &mut device) }.is_err() {
            eprintln!("no WARP device: skipped");
            return;
        }
        let device = device.unwrap();
        const SIZE: u64 = CHUNK + (1 << 20);
        const ZEROED: u64 = CHUNK + 4099;
        let buffer = |kind: D3D12_HEAP_TYPE, state| {
            let mut made: Option<ID3D12Resource> = None;
            // SAFETY: a plain buffer description.
            unsafe {
                device
                    .CreateCommittedResource(
                        &D3D12_HEAP_PROPERTIES {
                            Type: kind,
                            ..Default::default()
                        },
                        D3D12_HEAP_FLAG_NONE,
                        &crate::texpool::buffer_desc(SIZE, D3D12_RESOURCE_FLAG_NONE),
                        state,
                        None,
                        &mut made,
                    )
                    .unwrap();
            }
            made.unwrap()
        };
        let card = buffer(D3D12_HEAP_TYPE_DEFAULT, D3D12_RESOURCE_STATE_COMMON);
        let up = buffer(D3D12_HEAP_TYPE_UPLOAD, D3D12_RESOURCE_STATE_GENERIC_READ);
        let back = buffer(D3D12_HEAP_TYPE_READBACK, D3D12_RESOURCE_STATE_COPY_DEST);
        // SAFETY: mappings of live buffers of SIZE bytes; each list has run
        // when its fence is passed.
        unsafe {
            let mut data = std::ptr::null_mut();
            up.Map(0, None, Some(&mut data)).unwrap();
            std::ptr::write_bytes(data.cast::<u8>(), 0xCD, SIZE as usize);
            let queue: ID3D12CommandQueue = device
                .CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                    Type: D3D12_COMMAND_LIST_TYPE_COPY,
                    ..Default::default()
                })
                .unwrap();
            let allocator: ID3D12CommandAllocator = device
                .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_COPY)
                .unwrap();
            let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap();
            let run = |value: u64, to: &ID3D12Resource, from: &ID3D12Resource| {
                let list: ID3D12GraphicsCommandList = device
                    .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_COPY, &allocator, None)
                    .unwrap();
                list.CopyBufferRegion(to, 0, from, 0, SIZE);
                list.Close().unwrap();
                queue.ExecuteCommandLists(&[Some(list.cast().unwrap())]);
                queue.Signal(&fence, value).unwrap();
                while fence.GetCompletedValue() < value {
                    std::thread::yield_now();
                }
                allocator.Reset().unwrap();
            };
            run(1, &card, &up);
            zero_buffer(&device, &card, ZEROED).unwrap();
            run(2, &back, &card);
            let mut read = std::ptr::null_mut();
            back.Map(0, None, Some(&mut read)).unwrap();
            let bytes = std::slice::from_raw_parts(read.cast::<u8>(), SIZE as usize);
            assert_eq!(bytes[..ZEROED as usize].iter().position(|&b| b != 0), None);
            assert!(bytes[ZEROED as usize..].iter().all(|&b| b == 0xCD));
        }
    }
}
