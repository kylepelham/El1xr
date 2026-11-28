//! (El1xr, #115) Textures the title places inside the memory of a texture
//! the host writes (a render target or unordered-access texture) other
//! than as its twin at its start: a level of it, a layer, a later part.
//! [`super::alias_of`] links only a view at the target's start with the
//! target's size, levels and layers, so such a view was filled from title
//! memory, which the host's writes never reach: black, or the bytes another
//! texture left there.
//!
//! `VICTORY_PLACED_INSIDE_DIAG=1` (live) says so, once per kind of pair,
//! with whether the view's levels and layers line up with the target's
//! (from trustno1's `placed_inside_target`, VIctory b3d49db; here where a
//! draw reads the view).
//!
//! `VICTORY_PLACED_INSIDE=1` links a view to the target when each of its
//! levels and layers has the bytes of one of the target's: the same place
//! in memory, the same size and the same console layout. A draw that reads
//! the view then gets those levels and layers of the target, copied on the
//! host as an alias copy is (`crate::texture::alias`). A view that does not
//! line up (a corner of an atlas, another layout) stays as it was.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, OnceLock};

use victory_formats::texture::{self, Dimension, Layout, Mip};

use super::{Resource, ResourceDesc};
use crate::object::This;

/// The most levels times layers a link copies (as `crate::texture::alias`).
const MOST_PARTS: u32 = 54;

/// `VICTORY_PLACED_INSIDE=1`: views inside a target are linked to it.
pub(crate) fn on() -> bool {
    #[cfg(test)]
    if let Some(on) = TEST_ON.get() {
        return on;
    }
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_PLACED_INSIDE").is_some_and(|v| v == "1"))
}

#[cfg(test)]
thread_local! {
    pub(crate) static TEST_ON: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

static DIAG: victory_core::settings::Live =
    victory_core::settings::Live::flag("VICTORY_PLACED_INSIDE_DIAG");

fn diag() -> bool {
    DIAG.get_bool()
}

/// For each subresource of `inner` (level + layer * levels), the subresource
/// of `outer` holding its bytes when `inner` starts `offset` bytes into
/// `outer`'s memory: at the same place, of the same size and laid out
/// alike. `None` if one has no such subresource.
pub(crate) fn subresources_inside(
    outer: &texture::Texture,
    inner: &texture::Texture,
    offset: u64,
) -> Option<Vec<u32>> {
    let ([o], [i]) = (&outer.planes[..], &inner.planes[..]) else {
        return None;
    };
    let (o, i) = (&o.layout, &i.layout);
    let (od, id) = (o.desc, i.desc);
    if od.dimension == Dimension::Tex3d
        || id.dimension == Dimension::Tex3d
        || od.samples != 1
        || id.samples != 1
        || (od.element_bytes, od.block, od.swizzle) != (id.element_bytes, id.block, id.swizzle)
        || o.slice_size == 0
    {
        return None;
    }
    let at = |l: &Layout, layer: u32, mip: usize| {
        let m = &l.mips[mip];
        u64::from(layer) * l.slice_size + m.offset + m.tail.map_or(0, |t| u64::from(t.offset))
    };
    let shape = |m: &Mip| {
        (
            m.width,
            m.height,
            m.pitch,
            m.rows,
            m.tail.map(|t| (t.x, t.y)),
        )
    };
    let mut parts = Vec::with_capacity(i.mips.len() * i.slices as usize);
    for layer in 0..i.slices {
        for mip in 0..i.mips.len() {
            let start = offset.checked_add(at(i, layer, mip))?;
            let k = u32::try_from(start / o.slice_size).ok()?;
            if k >= o.slices {
                return None;
            }
            let level = (0..o.mips.len())
                .find(|&m| at(o, k, m) == start && shape(&o.mips[m]) == shape(&i.mips[mip]))?;
            parts.push(u32::try_from(level + k as usize * o.mips.len()).ok()?);
        }
    }
    Some(parts)
}

/// [`subresources_inside`] for the title textures described by `target` and
/// `placed`, `offset` bytes in. A plain texture's layout left to the
/// library (0) is a guess here; the title names the target's mode when it
/// places a view in a target, so that mode is tried too.
fn parts_for(target: &ResourceDesc, placed: &ResourceDesc, offset: u64) -> Option<Arc<[u32]>> {
    const DEPTH: u32 = 0x2;
    const TEXTURE_2D: u32 = 3;
    let (o, r) = (target, placed);
    let fits = r.dimension == TEXTURE_2D
        && o.dimension == TEXTURE_2D
        && o.flags & DEPTH == 0
        && r.sample_count <= 1
        && o.sample_count <= 1
        && r.mip_levels >= 1
        && o.mip_levels >= 1
        && r.depth_or_array_size >= 1
        && o.depth_or_array_size >= 1
        && u32::from(r.mip_levels) * u32::from(r.depth_or_array_size) <= MOST_PARTS
        && super::target_format(o.format) == o.format
        && super::element(o.format)
            .is_some_and(|e| e.1 == 1 && Some(e) == super::element(r.format));
    if !fits {
        return None;
    }
    let outer = super::texture_layout(o)?;
    subresources_inside(&outer, &super::texture_layout(r)?, offset)
        .or_else(|| {
            if r.layout != 0 {
                return None;
            }
            let mut d = *r;
            d.layout = 0x100 | super::texture_desc(o)?.swizzle.0;
            subresources_inside(&outer, &super::texture_layout(&d)?, offset)
        })
        .map(Arc::from)
}

/// The view, the target, where the target starts, and what it is: the
/// key of an answer of [`parts_for`] kept in [`PARTS`].
type Key = (usize, usize, u64, [u32; 5]);

/// [`parts_for`]'s answers, kept: a draw asks for them at each read.
static PARTS: Mutex<Option<HashMap<Key, Option<Arc<[u32]>>>>> = Mutex::new(None);

/// [`parts_for`] of `placed` (`r`) inside `target` (`o`), kept.
fn kept_parts(placed: This, r: &Resource, target: This, o: &Resource) -> Option<Arc<[u32]>> {
    let start = o.data() as u64;
    let offset = (r.data() as u64).checked_sub(start)?;
    let d = &o.desc;
    let key = (
        placed as usize,
        target as usize,
        start,
        [
            d.width as u32,
            d.height,
            u32::from(d.mip_levels),
            u32::from(d.depth_or_array_size),
            d.format,
        ],
    );
    let mut map = PARTS.lock().unwrap_or_else(|e| e.into_inner());
    let map = map.get_or_insert_with(HashMap::new);
    if let Some(parts) = map.get(&key) {
        return parts.clone();
    }
    if map.len() >= 4096 {
        map.clear();
    }
    let parts = parts_for(d, &r.desc, offset);
    map.insert(key, parts.clone());
    parts
}

/// The target whose memory holds the view `this` (`r`), if any: the last
/// one starting at or before it, if it reaches it.
fn holder(targets: &BTreeMap<u64, usize>, r: &Resource) -> Option<This> {
    let address = r.data() as u64;
    let (&start, &other) = targets.range(..=address).next_back()?;
    let size = super::footprint(&super::resource(other as This).desc)?.size;
    (address < start.checked_add(size)?).then_some(other as This)
}

/// (#115, `crate::descriptor::host::live_watch`) The start of a texture
/// the host writes whose memory holds part of `[at, at + len)`: the last
/// one starting before its end, if it reaches it.
pub(crate) fn target_over(at: u64, len: u64) -> Option<u64> {
    let end = at.checked_add(len.max(1))?;
    let targets = super::TARGETS_AT.lock().unwrap_or_else(|e| e.into_inner());
    let (&start, &other) = targets.range(..end).next_back()?;
    let size = super::footprint(&super::resource(other as This).desc)?.size;
    (at < start.checked_add(size)?).then_some(start)
}

/// [`super::alias_of`]'s answer from the table of targets, held locked,
/// for a placed texture no target twins: with `VICTORY_PLACED_INSIDE`, the
/// target whose memory holds it, if its levels and layers line up. Says
/// what it finds with `VICTORY_PLACED_INSIDE_DIAG`.
pub(super) fn linked(targets: &BTreeMap<u64, usize>, this: This, r: &Resource) -> Option<This> {
    let (link, say) = (on(), diag());
    if !link && !say {
        return None;
    }
    let target = holder(targets, r)?;
    let parts = kept_parts(this, r, target, super::resource(target));
    if say {
        tell(this, r, target, parts.as_deref(), link);
    }
    (link && parts.is_some()).then_some(target)
}

/// For `crate::texture::alias`: with `VICTORY_PLACED_INSIDE`, the
/// subresource of `source` for each of `placed`'s, when `placed` lies in
/// `source`'s memory other than as its twin. `None` for a twin (the alias
/// copies every level and layer as they are) or a view that does not line
/// up.
pub(crate) fn inside_parts(placed: This, source: This) -> Option<Arc<[u32]>> {
    if !on() || placed.is_null() || source.is_null() {
        return None;
    }
    let (r, o) = (super::resource(placed), super::resource(source));
    let parts = kept_parts(placed, r, source, o)?;
    let all = u32::from(o.desc.mip_levels) * u32::from(o.desc.depth_or_array_size);
    let twin = r.data() == o.data()
        && parts.len() == all as usize
        && parts.iter().enumerate().all(|(n, &p)| p as usize == n);
    (!twin).then_some(parts)
}

/// With `VICTORY_PLACED_INSIDE` or `VICTORY_PLACED_INSIDE_DIAG`, a target
/// that enters the table at `start` changes the count of every 64 KiB block
/// it covers, not only its first: a placed texture further in keeps no
/// "no target" answer from before it (`Hosted::no_target`).
pub(super) fn cover(start: u64, desc: &ResourceDesc) {
    if !on() && !diag() {
        return;
    }
    let Some(size) = super::footprint(desc).map(|f| f.size) else {
        return;
    };
    let (first, last) = (start >> 16, start.saturating_add(size.max(1) - 1) >> 16);
    for block in (first + 1..=last).take(super::TARGET_BUCKETS - 1) {
        super::target_changes(block << 16).fetch_add(1, Ordering::Release);
    }
}

/// The diagnostic's line, once per kind of pair (at most 64).
fn tell(this: This, r: &Resource, target: This, parts: Option<&[u32]>, link: bool) {
    let o = super::resource(target);
    let (rd, od) = (&r.desc, &o.desc);
    let pair = [
        rd.width as u32,
        rd.height,
        rd.format,
        u32::from(rd.mip_levels),
        u32::from(rd.depth_or_array_size),
        od.width as u32,
        od.height,
        od.format,
        u32::from(od.mip_levels),
        u32::from(od.depth_or_array_size),
        u32::from(parts.is_some()),
    ];
    static SAID: Mutex<BTreeSet<[u32; 11]>> = Mutex::new(BTreeSet::new());
    let mut said = SAID.lock().unwrap_or_else(|e| e.into_inner());
    if said.len() < 64 && said.insert(pair) {
        tracing::warn!(
            view = ?this,
            texture = ?target,
            address = format_args!("{:#x}", r.data() as u64),
            target_address = format_args!("{:#x}", o.data() as u64),
            offset = format_args!("{:#x}", r.data() as u64 - o.data() as u64),
            target_flags = format_args!("{:#x}", od.flags),
            view_layout = format_args!("{:#x}", rd.layout),
            target_layout = format_args!("{:#x}", od.layout),
            lines_up = parts.is_some(),
            subresources = ?parts,
            linked = link && parts.is_some(),
            "placed inside: a placed texture lies in the memory of a texture the host writes, not as its twin at its start: filled from title memory, which the host's writes never reach unless linked (VICTORY_PLACED_INSIDE=1 links those whose levels and layers line up) (width, height, format, levels, layers of each; lines up): {pair:?}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use victory_formats::texture::{Swizzle, TextureDesc};

    fn tex(width: u32, height: u32, layers: u32, mips: u32, swizzle: Swizzle) -> texture::Texture {
        texture::Texture::new(&TextureDesc {
            dimension: Dimension::Tex2d,
            width,
            height,
            depth_or_array: layers,
            mips,
            format: 28,
            samples: 1,
            swizzle,
        })
        .unwrap()
    }

    fn level_at(t: &texture::Texture, layer: u32, mip: usize) -> u64 {
        let l = &t.planes[0].layout;
        u64::from(layer) * l.slice_size + l.mips[mip].offset
    }

    /// A view of the target's levels from the second on, placed where the
    /// second level lies, reads those levels; the second level alone too.
    #[test]
    fn a_view_of_a_targets_later_levels_lines_up() {
        let target = tex(256, 256, 1, 9, Swizzle::SW_64KB_R_X);
        let rest = tex(128, 128, 1, 8, Swizzle::SW_64KB_R_X);
        // The view's own first level lies where its chain puts it.
        let offset = level_at(&target, 0, 1) - level_at(&rest, 0, 0);
        assert_eq!(
            subresources_inside(&target, &rest, offset),
            Some((1..9).collect())
        );
        let one = tex(128, 128, 1, 1, Swizzle::SW_64KB_R_X);
        assert_eq!(
            subresources_inside(&target, &one, level_at(&target, 0, 1)),
            Some(vec![1])
        );
        // A level in the mip tail is laid out otherwise than a texture of
        // its size alone: it does not line up.
        let small = tex(64, 64, 1, 1, Swizzle::SW_64KB_R_X);
        assert!(target.planes[0].layout.mips[2].tail.is_some());
        assert_eq!(
            subresources_inside(&target, &small, level_at(&target, 0, 2)),
            None
        );
    }

    /// A view of one layer of an array target reads that layer; the twin
    /// at the start reads every subresource as it is.
    #[test]
    fn a_view_of_a_layer_lines_up_and_a_twin_is_the_identity() {
        let target = tex(64, 64, 6, 1, Swizzle::SW_64KB_R_X);
        let layer = tex(64, 64, 1, 1, Swizzle::SW_64KB_R_X);
        assert_eq!(
            subresources_inside(&target, &layer, level_at(&target, 3, 0)),
            Some(vec![3])
        );
        let mipped = tex(64, 64, 2, 7, Swizzle::SW_64KB_R_X);
        assert_eq!(
            subresources_inside(&mipped, &mipped, 0),
            Some((0..14).collect())
        );
    }

    /// A corner of the target, another layout, a view past its end, or a
    /// view that is not the size of the level it starts at: none lines up.
    #[test]
    fn what_does_not_line_up_is_refused() {
        let target = tex(256, 256, 1, 1, Swizzle::SW_64KB_R_X);
        let corner = tex(64, 64, 1, 1, Swizzle::SW_64KB_R_X);
        assert_eq!(subresources_inside(&target, &corner, 0x1_0000), None);
        let plain = tex(256, 256, 1, 1, Swizzle::SW_4KB_S);
        assert_eq!(subresources_inside(&target, &plain, 0), None);
        let mipped = tex(256, 256, 1, 9, Swizzle::SW_64KB_R_X);
        let wrong = tex(100, 100, 1, 1, Swizzle::SW_64KB_R_X);
        assert_eq!(
            subresources_inside(&mipped, &wrong, level_at(&mipped, 0, 1)),
            None
        );
        let layers = tex(64, 64, 2, 1, Swizzle::SW_64KB_R_X);
        let one = tex(64, 64, 1, 1, Swizzle::SW_64KB_R_X);
        assert_eq!(
            subresources_inside(&layers, &one, level_at(&layers, 2, 0)),
            None
        );
    }

    #[test]
    fn the_diagnostic_is_live() {
        victory_core::settings::check_live("VICTORY_PLACED_INSIDE_DIAG", "1", diag, false, true);
    }
}
