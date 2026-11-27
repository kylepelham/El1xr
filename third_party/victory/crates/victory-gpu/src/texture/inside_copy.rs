//! (El1xr, #115) The copy into a view the title placed inside a texture the
//! host writes, at one of its levels or layers (`VICTORY_PLACED_INSIDE`,
//! `crate::resource::placed_inside`): each of the view's subresources from
//! the target's subresource with the same bytes, directly when the formats
//! are one (or `reinterpret::direct` with `VICTORY_ALIAS_DIRECT`), else
//! through a buffer as [`super::alias`] does.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use windows::Win32::Graphics::Direct3D12::*;

use super::{Through, reinterpret};
use crate::host::Host;
use crate::object::This;

/// The copies of [`through`], by the view's texture and the target.
static INSIDE: Mutex<Option<HashMap<(usize, usize), Arc<Through>>>> = Mutex::new(None);

/// How `placed` is copied from `source`, whose subresource `parts[n]` has
/// the bytes of `placed`'s subresource `n`: made once per pair. `None` if
/// the host's textures do not take the copy (another count of levels and
/// layers, or subresources of other sizes).
pub(super) fn through(
    host: &Host,
    placed: This,
    source: This,
    from: &ID3D12Resource,
    to: &ID3D12Resource,
    parts: &Arc<[u32]>,
) -> Option<Arc<Through>> {
    let key = (placed as usize, source as usize);
    let mut map = INSIDE.lock().unwrap_or_else(|e| e.into_inner());
    let map = map.get_or_insert_with(HashMap::new);
    if let Some(through) = map.get(&key)
        && through.from_parts.as_deref() == Some(&parts[..])
    {
        return Some(through.clone());
    }
    // SAFETY: live textures.
    let (of_from, of_to) = unsafe { (from.GetDesc(), to.GetDesc()) };
    let count = |d: &D3D12_RESOURCE_DESC| u32::from(d.MipLevels) * u32::from(d.DepthOrArraySize);
    let two_d = |d: &D3D12_RESOURCE_DESC| d.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE2D;
    if !two_d(&of_from)
        || !two_d(&of_to)
        || parts.len() != count(&of_to) as usize
        || parts.iter().any(|&p| p >= count(&of_from))
    {
        return None;
    }
    let mut rows = Vec::with_capacity(parts.len());
    let mut at = 0u64;
    for (n, &p) in parts.iter().enumerate() {
        let mut row = [D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); 2];
        let mut bytes = [0u64; 2];
        // SAFETY: plain queries of one subresource each; the out pointers
        // are ours.
        unsafe {
            host.device.GetCopyableFootprints(
                &of_from,
                p,
                1,
                at,
                Some(&mut row[0]),
                None,
                None,
                Some(&mut bytes[0]),
            );
            host.device.GetCopyableFootprints(
                &of_to,
                n as u32,
                1,
                at,
                Some(&mut row[1]),
                None,
                None,
                Some(&mut bytes[1]),
            );
        }
        let (a, b) = (row[0].Footprint, row[1].Footprint);
        if bytes[0] == 0
            || bytes[0] == u64::MAX
            || bytes[0] != bytes[1]
            || (a.Width, a.Height, a.Depth, a.RowPitch) != (b.Width, b.Height, b.Depth, b.RowPitch)
        {
            return None;
        }
        rows.push((row[0], row[1]));
        at = (at + bytes[0]).next_multiple_of(u64::from(D3D12_TEXTURE_DATA_PLACEMENT_ALIGNMENT));
    }
    let direct = of_from.Format == of_to.Format
        || (reinterpret::on() && reinterpret::direct(of_from.Format, of_to.Format));
    let buffer = if direct {
        None
    } else {
        let buffer = host.uav_buffer(at)?;
        crate::host::name(&buffer, || {
            format!(
                "placed inside copy {}x{} f{} to f{}",
                of_to.Width, of_to.Height, of_from.Format.0, of_to.Format.0
            )
        });
        Some(buffer)
    };
    tracing::info!(
        view = ?placed,
        texture = ?source,
        width = of_to.Width,
        height = of_to.Height,
        target_width = of_from.Width,
        target_height = of_from.Height,
        subresources = ?&parts[..],
        written_as = of_from.Format.0,
        read_as = of_to.Format.0,
        direct = buffer.is_none(),
        "placed inside: a view placed at a level or layer of a texture the host writes: read from those levels and layers of that texture by a copy on the host (VICTORY_PLACED_INSIDE)"
    );
    let through = Arc::new(Through {
        buffer,
        parts: rows,
        from_parts: Some(parts.clone()),
    });
    map.insert(key, through.clone());
    Some(through)
}

/// Records the direct copy of each of `placed`'s subresources (`to`) from
/// its subresource of the target (`from`), both in the copy states.
pub(super) fn record_direct(
    list: &ID3D12GraphicsCommandList,
    from: &ID3D12Resource,
    to: &ID3D12Resource,
    parts: &[u32],
) {
    let part = |texture: &ID3D12Resource, index: u32| D3D12_TEXTURE_COPY_LOCATION {
        // SAFETY: a borrowed pointer the location never releases.
        pResource: unsafe { std::mem::transmute_copy(texture) },
        Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
            SubresourceIndex: index,
        },
    };
    for (n, &p) in parts.iter().enumerate() {
        // SAFETY: an open list; live textures of the same format, whose
        // subresources `n` and `p` are of one size, in the copy states.
        unsafe { list.CopyTextureRegion(&part(to, n as u32), 0, 0, 0, &part(from, p), None) };
    }
}
