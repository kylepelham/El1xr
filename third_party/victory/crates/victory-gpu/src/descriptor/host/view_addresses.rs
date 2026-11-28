//! (`VICTORY_HEAP_VIEW_ADDRESS=1`) The title address of the view at each title slot of a view
//! heap, for shaders that load a descriptor from the heap at an offset they compute and read
//! its address as numbers (`victory_shader::emit::SLOT_VIEW_ADDRESSES`; the TraceShadowRays
//! programs take their ray-tracing structure's base that way). An upload buffer of
//! `VIEW_ADDRESSES_COUNT` pairs (low, high), written where the stride table is
//! ([`super::HostHeap::note_buffer_into`]): a buffer view's first byte, a structure view's
//! location, 0 for anything else.

use windows::Win32::Graphics::Direct3D12::ID3D12Resource;

use super::View;

/// The table for a new view heap (its buffer, GPU address and mapping), zeroed; none with the
/// switch off.
pub(super) fn make() -> Option<(ID3D12Resource, u64, usize)> {
    if !victory_shader::emit::heap_view_address() {
        return None;
    }
    let bytes = u64::from(victory_shader::emit::VIEW_ADDRESSES_COUNT) * 8;
    let (buffer, data) = crate::host::host()?.upload_buffer(bytes)?;
    // SAFETY: the buffer's whole mapping; a live buffer.
    let gpu = unsafe {
        std::ptr::write_bytes(data, 0, bytes as usize);
        buffer.GetGPUVirtualAddress()
    };
    crate::host::name(&buffer, || "view addresses by view heap slot".to_owned());
    Some((buffer, gpu, data as usize))
}

/// The title address shaders read for `view`: a ray-tracing structure's location, a buffer
/// view's first byte; 0 for anything else.
pub(crate) fn title_address(view: Option<&View>) -> u64 {
    view.and_then(|v| crate::rt_bind::structure_address(v).or_else(|| Some(super::buffer_range(v)?.0)))
        .unwrap_or(0)
}

/// Writes title slot `index`'s pair.
pub(super) fn write(table: &Option<(ID3D12Resource, u64, usize)>, index: u32, view: Option<&View>) {
    let Some((_, _, data)) = table else { return };
    if index >= victory_shader::emit::VIEW_ADDRESSES_COUNT {
        return;
    }
    let at = title_address(view);
    // SAFETY: inside the mapping; shaders read it while we write (each dword is one the slot
    // had or has).
    unsafe {
        let pair = (*data as *mut u32).add(2 * index as usize);
        pair.write_volatile(at as u32);
        pair.add(1).write_volatile((at >> 32) as u32);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_structure_view_gives_its_location_and_others_nothing() {
        let mut desc = [0u32; 10];
        desc[1] = 11; // a ray-tracing structure
        desc[4] = 0x5678_9a00;
        desc[5] = 0x12;
        assert_eq!(title_address(Some(&View::Shader(0, Some(desc)))), 0x12_5678_9a00);
        assert_eq!(title_address(Some(&View::Shader(0, None))), 0);
        assert_eq!(title_address(None), 0);
    }
}
