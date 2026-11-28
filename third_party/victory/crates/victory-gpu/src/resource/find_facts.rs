//! (phonescreen, #115) What `crate::heap_find` says of the textures whose
//! memory holds the address it looks for: where each lies, its
//! description, and the texture the host writes it is linked to.

use super::{BUFFER, ResourceDesc, footprint, resource};
use crate::object::This;

fn desc_of(words: &[u32; 14]) -> ResourceDesc {
    // SAFETY: a resource description is 14 words (asserted at its type).
    unsafe { words.as_ptr().cast::<ResourceDesc>().read_unaligned() }
}

/// The bytes of title memory a resource of description `words` takes.
pub(crate) fn words_bytes(words: &[u32; 14]) -> Option<u64> {
    let d = desc_of(words);
    if d.dimension == BUFFER {
        return Some(d.width);
    }
    footprint(&d).map(|f| f.size)
}

/// Where title resource `this` lies and how many bytes it takes.
pub(crate) fn object_span(this: This) -> Option<(u64, u64)> {
    if this.is_null() {
        return None;
    }
    let r = resource(this);
    let size = if r.desc.dimension == BUFFER {
        r.desc.width
    } else {
        footprint(&r.desc)?.size
    };
    Some((r.data() as u64, size))
}

/// The width and height of title resource `this`.
pub(crate) fn object_size(this: This) -> Option<(u64, u64)> {
    (!this.is_null()).then(|| {
        let d = &resource(this).desc;
        (d.width, u64::from(d.height))
    })
}

/// (`VICTORY_FLICKER_CATCH_REGION`) Title resource `this`'s memory,
/// width, height, format and dimension.
pub(crate) fn object_brief(this: This) -> Option<(u64, u64, u32, u32, u32)> {
    (!this.is_null()).then(|| {
        let r = resource(this);
        let d = &r.desc;
        (r.data() as u64, d.width, d.height, d.format, d.dimension)
    })
}

/// (`VICTORY_FLICKER_CATCH_REGION`) The width, height, format and
/// dimension in resource description `words`.
pub(crate) fn words_brief(words: &[u32; 14]) -> (u64, u32, u32, u32) {
    let d = desc_of(words);
    (d.width, d.height, d.format, d.dimension)
}

/// The width of title resource `this`.
pub(crate) fn object_width(this: This) -> Option<u64> {
    (!this.is_null()).then(|| resource(this).desc.width)
}

fn desc_text(d: &ResourceDesc) -> String {
    format!(
        "dim {} {}x{}x{} lv {} fmt {} layout {:#x} flags {:#x}",
        d.dimension,
        d.width,
        d.height,
        d.depth_or_array_size,
        d.mip_levels,
        d.format,
        d.layout,
        d.flags
    )
}

/// Description `words` in a few words.
pub(crate) fn words_text(words: &[u32; 14]) -> String {
    desc_text(&desc_of(words))
}

/// Title resource `this` in a few words: where it lies and its description.
pub(crate) fn object_text(this: This) -> String {
    if this.is_null() {
        return "none".into();
    }
    let r = resource(this);
    format!("{:#x} {}", r.data() as u64, desc_text(&r.desc))
}

/// The texture the host writes (render target, depth, unordered access)
/// whose memory holds `address`, if any.
pub(crate) fn target_text(address: u64) -> String {
    let Some(start) = super::placed_inside::target_over(address, 1) else {
        return "none".into();
    };
    let targets = super::TARGETS_AT.lock().unwrap_or_else(|e| e.into_inner());
    targets
        .get(&start)
        .map_or_else(|| "none".into(), |&t| object_text(t as This))
}

/// What a draw reading title texture `this` gets copied from: the texture
/// the host writes it is linked to ([`super::alias_of`]), or `none` (it is
/// filled from title memory).
pub(crate) fn link_text(this: This) -> String {
    match super::alias_of(this) {
        Some(held) => {
            let source = held.get();
            let twin = resource(source).data() == resource(this).data();
            format!("{} at_start={twin}", object_text(source))
        }
        None => "none".into(),
    }
}
