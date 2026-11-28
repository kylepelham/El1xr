//! `VICTORY_SWAP_WATCH=1` (off by default, live; a diagnostic): names, per
//! frame, the texture slots the heap pass or a draw made with another view
//! than they showed: a view of the same texture with more or fewer mips, of
//! another texture (moved: same size and format elsewhere; resized; another
//! format, dimension or channel mapping), or of memory that held another
//! texture (reused). A new view whose first mip starts with 64 zero bytes in
//! title memory (mapped and never written, or written later) is flagged
//! `zero_top`.
//!
//! For "lit, then dark for good after a texture streams in" (Kyle, the
//! prison window sill): the frame it goes dark is found in the 1 s snaps;
//! the lines at that frame name what the slots were made with then.
//! `VIEW_AGES=1` names the slots that wait (null or kept) instead.
//!
//! And a texture the card wrote (a copy or resolve into it) is never
//! uploaded from title memory again (`texture::upload_from`): `host_kept`
//! counts those whose title memory was written since their last upload (a
//! streamer that copies a texture's small mips on the card and writes its
//! large ones in memory would leave them as they were for good). Asked at
//! most once every [`KEPT_EVERY`] frames a texture.
//!
//! Logged: `swap watch: frame` (counts by kind, at most once a frame, for
//! the frame before the first event in a new one), up to [`EXAMPLES`]
//! `swap watch: slot` lines, the rarer kinds first, and up to
//! [`EXAMPLES`] `swap watch: host kept` lines.

use std::sync::Mutex;

use super::View;

/// Example lines a frame at most.
const EXAMPLES: usize = 8;
/// Frames between two asks of one card-written texture.
const KEPT_EVERY: u64 = 30;

/// The switch (`VICTORY_SWAP_WATCH`; on only when `1`).
pub(crate) fn on() -> bool {
    static ON: victory_core::settings::Live =
        victory_core::settings::Live::new("VICTORY_SWAP_WATCH", |v| u64::from(v == Some("1")));
    ON.get_bool()
}

/// What a slot's new view changed, the rarer first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Kind {
    /// Memory that held a texture of another description.
    Reused,
    /// Another format, dimension or channel mapping.
    Format,
    /// The same texture, fewer mips shown (blurrier).
    Fewer,
    /// Another texture of another size or mip count.
    Resized,
    /// The same texture, more mips shown (sharper).
    More,
    /// Another texture of the same size and format elsewhere.
    Moved,
    /// The same texture and mips, another view word (array slices, plane, ...).
    Other,
    /// A slot made for the first time, or after it was null (an example only with `zero_top`).
    New,
}

const KINDS: usize = 8;
const NAMES: [&str; KINDS] = [
    "reused", "format", "fewer", "resized", "more", "moved", "other", "new",
];

/// What changed from `old` to `new`, for views of textures; `None` when
/// neither is a placed texture view, or nothing did.
pub(crate) fn kind(old: &View, new: &View) -> Option<Kind> {
    match (old, new) {
        (View::Placed(false, r, d), View::Placed(false, nr, nd)) => {
            if r == nr && d == nd {
                return None;
            }
            let (at, new_at) = (address(d), address(nd));
            if at == new_at && r != nr {
                return Some(Kind::Reused);
            }
            if d[..3] != nd[..3] || r[8] != nr[8] {
                return Some(Kind::Format);
            }
            if at == new_at {
                // The same texture: the mips shown (first mip, then count).
                return Some(match nd[4].cmp(&d[4]).then(d[5].cmp(&nd[5])) {
                    std::cmp::Ordering::Less => Kind::More,
                    std::cmp::Ordering::Greater => Kind::Fewer,
                    std::cmp::Ordering::Equal => Kind::Other,
                });
            }
            Some(if r[4..8] == nr[4..8] {
                Kind::Moved
            } else {
                Kind::Resized
            })
        }
        (View::Shader(r, d), View::Shader(nr, nd)) if r != nr || d != nd => {
            Some(if r == nr { Kind::Other } else { Kind::Moved })
        }
        _ => None,
    }
}

fn address(d: &[u32; 16]) -> u64 {
    u64::from(d[10]) | u64::from(d[11]) << 32
}

/// Whether the new view's first 64 bytes of title memory are mapped and all zero
/// (a view of the texture from its first mip only: of a later mip the offset is
/// the layout's).
fn zero_top(view: &View) -> bool {
    let View::Placed(false, r, d) = view else {
        return false;
    };
    if r[0] == 1 || d[4] != 0 {
        return false;
    }
    let at = address(d) as usize;
    if at == 0 || !crate::texture::committed(at as *const u8, 64) {
        return false;
    }
    // SAFETY: 64 committed bytes of title memory, read as bytes.
    let bytes = unsafe { std::slice::from_raw_parts(at as *const u8, 64) };
    bytes.iter().all(|&b| b == 0)
}

#[derive(Default)]
struct Frame {
    frame: u64,
    counts: [u64; KINDS],
    zero: u64,
    examples: Vec<(Kind, bool, usize, u32, String, String)>,
    /// Card-written textures whose title memory changed since their last upload.
    kept: u64,
    kept_examples: Vec<String>,
}

static FRAME: Mutex<Option<Frame>> = Mutex::new(None);

/// Runs `note` on this frame's tally; says the last frame's when this is a new one.
fn with_frame(note: impl FnOnce(&mut Frame)) {
    let frame = crate::frame::current();
    let said = {
        let mut slot = FRAME.lock().unwrap_or_else(|e| e.into_inner());
        let f = slot.get_or_insert_with(|| Frame {
            frame,
            ..Frame::default()
        });
        let said = (f.frame != frame).then(|| {
            std::mem::replace(
                f,
                Frame {
                    frame,
                    ..Frame::default()
                },
            )
        });
        note(f);
        said
    };
    if let Some(f) = said {
        say(&f);
    }
}

/// `texture::upload_from` left out card-written texture `resource` (host
/// copy `texture`): counted when its title memory was written since its last upload.
#[cfg(windows)]
pub(crate) fn host_kept(
    resource: crate::object::This,
    texture: &windows::Win32::Graphics::Direct3D12::ID3D12Resource,
) {
    static ASKED: Mutex<Option<std::collections::HashMap<usize, u64>>> = Mutex::new(None);
    let frame = crate::frame::current();
    {
        let mut asked = ASKED.lock().unwrap_or_else(|e| e.into_inner());
        let asked = asked.get_or_insert_with(Default::default);
        if asked.len() > 1 << 16 {
            asked.clear();
        }
        let last = asked.entry(resource as usize).or_insert(0);
        if *last != 0 && frame < *last + KEPT_EVERY {
            return;
        }
        *last = frame.max(1);
    }
    if crate::texture::unchanged_since_upload(resource) != Some(false) {
        return;
    }
    // SAFETY: a live resource.
    let d = unsafe { texture.GetDesc() };
    let text = format!(
        "resource {:#x} {}x{}x{} mips {} fmt {}",
        resource as usize, d.Width, d.Height, d.DepthOrArraySize, d.MipLevels, d.Format.0
    );
    with_frame(|f| {
        f.kept += 1;
        if f.kept_examples.len() < EXAMPLES {
            f.kept_examples.push(text);
        }
    });
}

/// Slot `index` of heap `heap` was made with `new`, after showing `old`.
pub(crate) fn made(heap: usize, index: u32, old: Option<&View>, new: &View) {
    let kind = match old {
        Some(old) => kind(old, new),
        None => matches!(new, View::Placed(false, r, _) if r[0] != 1).then_some(Kind::New),
    };
    let Some(kind) = kind else {
        return;
    };
    let zero = zero_top(new);
    with_frame(|f| note(f, kind, zero, heap, index, old, new));
}

fn note(
    f: &mut Frame,
    kind: Kind,
    zero: bool,
    heap: usize,
    index: u32,
    old: Option<&View>,
    new: &View,
) {
    f.counts[kind as usize] += 1;
    f.zero += u64::from(zero);
    if kind == Kind::New && !zero {
        return;
    }
    // Kept: the rarer kinds, zero tops before others of their kind.
    let rank = |k: Kind, z: bool| (k, !z);
    if f.examples.len() == EXAMPLES {
        let worst = f
            .examples
            .iter()
            .map(|e| rank(e.0, e.1))
            .max()
            .expect("full");
        if rank(kind, zero) >= worst {
            return;
        }
        let at = f
            .examples
            .iter()
            .position(|e| rank(e.0, e.1) == worst)
            .expect("there");
        f.examples.swap_remove(at);
    }
    let text = super::view_ages::text;
    f.examples
        .push((kind, zero, heap, index, text(old), text(Some(new))));
}

fn say(f: &Frame) {
    let c = |k: usize| f.counts[k];
    tracing::info!(
        frame = f.frame,
        reused = c(0),
        format = c(1),
        fewer = c(2),
        resized = c(3),
        more = c(4),
        moved = c(5),
        other = c(6),
        new = c(7),
        zero_top = f.zero,
        host_kept = f.kept,
        "swap watch: frame (texture slots made with another view than they showed)"
    );
    let mut examples: Vec<_> = f.examples.iter().collect();
    examples.sort_by_key(|e| (e.0, !e.1, e.3));
    for (kind, zero, heap, index, old, new) in examples {
        tracing::info!(
            frame = f.frame,
            kind = NAMES[*kind as usize],
            zero_top = zero,
            heap = format_args!("{heap:#x}"),
            index,
            old = %old,
            new = %new,
            "swap watch: slot"
        );
    }
    for texture in &f.kept_examples {
        tracing::info!(
            frame = f.frame,
            texture = %texture,
            "swap watch: host kept (a texture the card wrote, its title memory written since its last upload, not uploaded)"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn placed(at: u64, w: u32, h: u32, mips: u32, fmt: u32, first: u32, count: u32) -> View {
        let mut r = [0u32; 14];
        r[0] = 4;
        r[4] = w;
        r[6] = h;
        r[7] = 1 | mips << 16;
        r[8] = fmt;
        let mut d = [0u32; 16];
        d[0] = fmt;
        d[1] = 4;
        d[2] = 0x1688;
        d[4] = first;
        d[5] = count;
        d[10] = at as u32;
        d[11] = (at >> 32) as u32;
        View::Placed(false, r, d)
    }

    #[test]
    fn the_switch_is_live() {
        victory_core::settings::check_live("VICTORY_SWAP_WATCH", "1", on, false, true);
    }

    #[test]
    fn kinds() {
        let a = placed(0x10000, 1024, 1024, 11, 71, 2, 9);
        assert_eq!(kind(&a, &a), None);
        assert_eq!(
            kind(&a, &placed(0x10000, 1024, 1024, 11, 71, 0, 11)),
            Some(Kind::More)
        );
        assert_eq!(
            kind(&a, &placed(0x10000, 1024, 1024, 11, 71, 4, 7)),
            Some(Kind::Fewer)
        );
        assert_eq!(
            kind(&a, &placed(0x90000, 1024, 1024, 11, 71, 2, 9)),
            Some(Kind::Moved)
        );
        assert_eq!(
            kind(&a, &placed(0x90000, 2048, 2048, 12, 71, 0, 12)),
            Some(Kind::Resized)
        );
        assert_eq!(
            kind(&a, &placed(0x10000, 512, 512, 10, 71, 0, 10)),
            Some(Kind::Reused)
        );
        assert_eq!(
            kind(&a, &placed(0x90000, 1024, 1024, 11, 77, 2, 9)),
            Some(Kind::Format)
        );
        assert_eq!(
            kind(&View::Shader(1, None), &View::Shader(2, None)),
            Some(Kind::Moved)
        );
        assert_eq!(kind(&View::Shader(1, None), &a), None);
    }

    #[test]
    fn a_frame_keeps_the_rarer_examples() {
        let mut f = Frame::default();
        let (a, b) = (
            placed(0x10000, 64, 64, 7, 71, 1, 6),
            placed(0x10000, 64, 64, 7, 71, 0, 7),
        );
        for i in 0..20 {
            note(&mut f, Kind::More, false, 1, i, Some(&a), &b);
        }
        note(&mut f, Kind::Reused, false, 1, 99, Some(&a), &b);
        note(&mut f, Kind::New, false, 1, 97, None, &b);
        note(&mut f, Kind::More, true, 1, 98, Some(&a), &b);
        assert_eq!(f.counts[Kind::More as usize], 21);
        assert_eq!(f.zero, 1);
        assert_eq!(f.examples.len(), EXAMPLES);
        assert!(f.examples.iter().any(|e| e.3 == 99));
        assert!(f.examples.iter().any(|e| e.3 == 98 && e.1));
        assert_eq!(f.counts[Kind::New as usize], 1);
        assert!(!f.examples.iter().any(|e| e.3 == 97), "a new slot without zeros: counted only");
    }

    #[test]
    fn zero_top_reads_committed_memory_only() {
        let mut page = vec![0u8; 4096];
        let at = page.as_mut_ptr() as u64;
        assert!(zero_top(&placed(at, 64, 64, 1, 71, 0, 1)));
        page[10] = 1;
        assert!(!zero_top(&placed(at, 64, 64, 1, 71, 0, 1)));
        assert!(
            !zero_top(&placed(at, 64, 64, 1, 71, 1, 1)),
            "a later mip: not asked"
        );
        assert!(!zero_top(&placed(0, 64, 64, 1, 71, 0, 1)));
    }
}
